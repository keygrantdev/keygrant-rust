//! Shared test support: a signing key, fake adapters, a harness around a
//! `License`, and an in-memory file system for the default storage.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};
use keygrant::storage::{StorageFile, StorageFs};
use keygrant::{
    AdapterError, Clock, DeviceIdentity, Fingerprinter, HttpAdapter, HttpResponse, License,
    LicenseConfig, RegistryStash, StorageAdapter, StoredState, Timings,
};
use serde_json::{json, Map, Value};

pub const NOW: i64 = 1_757_000_000_000;
pub const DAY: i64 = 86_400_000;
pub const PRODUCT: &str = "sluice";
pub const KEY: &str = "SLUICE-AAAA-BBBB-CCCC-DDDD";

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// --- signing ----------------------------------------------------------------

pub fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..=chunk.len() {
            out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize]));
        }
    }
    out
}

/// A product's signing key, for tests only.
#[derive(Clone)]
pub struct TestKey {
    signing: SigningKey,
}

impl TestKey {
    pub fn new(seed: u8) -> Self {
        TestKey {
            signing: SigningKey::from_bytes(&[seed; 32]),
        }
    }

    pub fn public_jwk(&self) -> String {
        json!({ "kty": "OKP", "crv": "Ed25519", "x": b64url(self.signing.verifying_key().as_bytes()) }).to_string()
    }

    /// Any payload, signed as the server signs a lease.
    pub fn sign_payload(&self, payload: &Value) -> String {
        self.sign_text(&payload.to_string())
    }

    /// Any payload TEXT, signed as the server signs a lease: for a payload
    /// whose numbers must be written exactly as given, not as serde_json
    /// would write them again.
    pub fn sign_text(&self, payload: &str) -> String {
        let header = b64url(
            json!({ "alg": "EdDSA", "typ": "JWT" })
                .to_string()
                .as_bytes(),
        );
        let body = b64url(payload.as_bytes());
        let input = format!("{header}.{body}");
        let signature = self.signing.sign(input.as_bytes());
        format!("{input}.{}", b64url(&signature.to_bytes()))
    }

    /// A lease as the test harness signs one: this product, activation `act_1`,
    /// 3 seats, major 1, issued at `now` (ms).
    pub fn lease(&self, sub: &str, model: &str, ttl_secs: i64, now: i64) -> String {
        self.lease_with(now, ttl_secs, json!({ "sub": sub, "model": model }))
    }

    /// A lease issued at `now` (ms) living `ttl_secs`, with `over` laid over
    /// the defaults (`null` removes a claim).
    pub fn lease_with(&self, now: i64, ttl_secs: i64, over: Value) -> String {
        let iat = now.div_euclid(1000);
        let mut claims = json!({
            "iss": "keygrant", "sub": KEY, "product": PRODUCT, "aid": "act_1", "model": "perpetual",
            "lim": 3, "major": 1, "ent": {}, "iat": iat, "exp": iat + ttl_secs,
        });
        let map = claims.as_object_mut().expect("an object");
        if let Value::Object(over) = over {
            for (name, value) in over {
                if value.is_null() {
                    map.remove(&name);
                } else {
                    map.insert(name, value);
                }
            }
        }
        self.sign_payload(&claims)
    }
}

// --- a gate a fake adapter waits at ---------------------------------------------

/// Something a fake adapter waits at until the test releases it.
#[derive(Clone, Default)]
pub struct Gate {
    inner: Arc<(Mutex<(bool, bool)>, Condvar)>,
}

impl Gate {
    pub fn new() -> Self {
        Gate::default()
    }
    /// Wait here until released, or for 30 s at most: a test that fails while
    /// a call waits here then ends, rather than hang on that call for ever.
    pub fn wait(&self) {
        let (state, changed) = &*self.inner;
        let until = std::time::Instant::now() + Duration::from_secs(30);
        let mut guard = lock(state);
        guard.0 = true;
        changed.notify_all();
        while !guard.1 {
            let left = until.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return;
            }
            guard = changed
                .wait_timeout(guard, left)
                .map(|(guard, _)| guard)
                .unwrap_or_else(|p| p.into_inner().0);
        }
    }
    pub fn release(&self) {
        let (state, changed) = &*self.inner;
        lock(state).1 = true;
        changed.notify_all();
    }
    pub fn reached(&self) -> bool {
        lock(&self.inner.0).0
    }
    /// Wait (up to 10 s) until something waits here.
    pub fn await_reached(&self) {
        for _ in 0..2000 {
            if self.reached() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("nothing reached the gate");
    }
}

// --- fake adapters ------------------------------------------------------------

#[derive(Default)]
pub struct FakeClock(AtomicI64);

impl FakeClock {
    pub fn at(now: i64) -> Arc<Self> {
        Arc::new(FakeClock(AtomicI64::new(now)))
    }
    pub fn set(&self, now: i64) {
        self.0.store(now, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// State in memory, with reads and writes that can be made to fail or wait.
#[derive(Default)]
pub struct MemoryStorage {
    state: Mutex<Option<StoredState>>,
    pub writes: AtomicUsize,
    pub reads: AtomicUsize,
    fail_writes: AtomicBool,
    fail_reads: AtomicBool,
    /// The read, counted from the first, that fails (`fail_read`); 0 for none.
    fail_read_at: AtomicUsize,
    hang_next_read: Mutex<Option<Gate>>,
    hold_next_write: Mutex<Option<Gate>>,
}

impl MemoryStorage {
    pub fn get(&self) -> Option<StoredState> {
        lock(&self.state).clone()
    }
    pub fn set(&self, state: Option<StoredState>) {
        *lock(&self.state) = state;
    }
    pub fn writes(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }
    pub fn fail_writes(&self, on: bool) {
        self.fail_writes.store(on, Ordering::SeqCst);
    }
    pub fn fail_reads(&self, on: bool) {
        self.fail_reads.store(on, Ordering::SeqCst);
    }
    /// The `nth` read from now fails (1 the next), as a file an antivirus
    /// scan holds just then does; the reads around it answer.
    pub fn fail_read(&self, nth: usize) {
        let at = self.reads.load(Ordering::SeqCst) + nth;
        self.fail_read_at.store(at, Ordering::SeqCst);
    }
    /// The next read waits at the gate returned.
    pub fn hang_next_read(&self) -> Gate {
        let gate = Gate::new();
        *lock(&self.hang_next_read) = Some(gate.clone());
        gate
    }
    /// The next write waits at the gate returned, holding the whole state it was handed.
    pub fn hold_next_write(&self) -> Gate {
        let gate = Gate::new();
        *lock(&self.hold_next_write) = Some(gate.clone());
        gate
    }
}

impl StorageAdapter for MemoryStorage {
    fn read(&self, _read_only: bool) -> io::Result<Option<StoredState>> {
        let read = self.reads.fetch_add(1, Ordering::SeqCst) + 1;
        let hang = lock(&self.hang_next_read).take();
        if let Some(gate) = hang {
            gate.wait();
        }
        if self.fail_reads.load(Ordering::SeqCst)
            || read == self.fail_read_at.load(Ordering::SeqCst)
        {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                "EBUSY: the disk said no",
            ));
        }
        Ok(self.get())
    }

    fn write(&self, state: &StoredState) -> io::Result<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                "EBUSY: the disk said no",
            ));
        }
        let hold = lock(&self.hold_next_write).take();
        if let Some(gate) = hold {
            gate.wait();
        }
        self.set(Some(state.clone()));
        Ok(())
    }
}

#[derive(Default)]
pub struct MemoryRegistry(Mutex<Option<i64>>);

impl MemoryRegistry {
    pub fn get(&self) -> Option<i64> {
        *lock(&self.0)
    }
    pub fn set(&self, ms: Option<i64>) {
        *lock(&self.0) = ms;
    }
}

impl RegistryStash for MemoryRegistry {
    fn read_trial_start(&self) -> Option<i64> {
        self.get()
    }
    fn write_trial_start(&self, ms: i64) {
        self.set(Some(ms));
    }
}

type Handler = Arc<dyn Fn(&Value) -> HttpResponse + Send + Sync>;

/// A server whose answers each test scripts, recording every call.
#[derive(Default)]
pub struct FakeHttp {
    handlers: Mutex<HashMap<String, Handler>>,
    calls: Mutex<Vec<(String, Value)>>,
    urls: Mutex<Vec<String>>,
    fail: AtomicBool,
    hang: AtomicBool,
}

pub fn reply(status: u16, json: Value) -> HttpResponse {
    HttpResponse {
        status,
        json: Some(json),
    }
}

impl FakeHttp {
    pub fn on(
        &self,
        action: &str,
        handler: impl Fn(&Value) -> HttpResponse + Send + Sync + 'static,
    ) {
        lock(&self.handlers).insert(action.to_owned(), Arc::new(handler));
    }
    /// Answer `action` with `status` and `json`, every time.
    pub fn answer(&self, action: &str, status: u16, json: Value) {
        self.on(action, move |_| reply(status, json.clone()));
    }
    pub fn fail(&self, on: bool) {
        self.fail.store(on, Ordering::SeqCst);
    }
    pub fn hang(&self, on: bool) {
        self.hang.store(on, Ordering::SeqCst);
    }
    pub fn urls(&self) -> Vec<String> {
        lock(&self.urls).clone()
    }
    pub fn calls(&self) -> Vec<(String, Value)> {
        lock(&self.calls).clone()
    }
    pub fn actions(&self) -> Vec<String> {
        self.calls().into_iter().map(|(action, _)| action).collect()
    }
    pub fn count(&self, action: &str) -> usize {
        self.calls().iter().filter(|(a, _)| a == action).count()
    }
    pub fn called(&self, action: &str) -> bool {
        self.count(action) > 0
    }
    /// The body of the first `action` call.
    pub fn body_of(&self, action: &str) -> Value {
        self.calls()
            .into_iter()
            .find(|(a, _)| a == action)
            .map(|(_, body)| body)
            .expect("a call")
    }
    /// The body of the last `action` call.
    pub fn last_body_of(&self, action: &str) -> Value {
        self.calls()
            .into_iter()
            .rev()
            .find(|(a, _)| a == action)
            .map(|(_, body)| body)
            .expect("a call")
    }
}

impl HttpAdapter for FakeHttp {
    fn post(
        &self,
        url: &str,
        body: &Value,
        _timeout: Duration,
    ) -> Result<HttpResponse, AdapterError> {
        let action = url.rsplit('/').next().unwrap_or_default().to_owned();
        lock(&self.calls).push((action.clone(), body.clone()));
        lock(&self.urls).push(url.to_owned());
        if self.fail.load(Ordering::SeqCst) {
            return Err("network down".into());
        }
        if self.hang.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_secs(120));
            return Err("never answered".into());
        }
        let handler = lock(&self.handlers).get(&action).cloned();
        Ok(match handler {
            Some(handler) => handler(body),
            None => reply(404, json!({ "error": "unknown" })),
        })
    }
}

/// A fingerprinter with only `get()`: a fixed fingerprint of unknown kind.
pub struct FixedFingerprinter(pub &'static str);

impl Fingerprinter for FixedFingerprinter {
    fn get(&self) -> Result<String, AdapterError> {
        Ok(self.0.to_owned())
    }
}

/// A fingerprinter that answers what its closure says.
pub struct FnFingerprinter<F>(pub F);

impl<F: Fn() -> Result<String, AdapterError> + Send + Sync> Fingerprinter for FnFingerprinter<F> {
    fn get(&self) -> Result<String, AdapterError> {
        (self.0)()
    }
}

/// A fingerprinter with `identify`.
pub struct IdentifyingFingerprinter<F>(pub F);

impl<F: Fn() -> DeviceIdentity + Send + Sync> Fingerprinter for IdentifyingFingerprinter<F> {
    fn get(&self) -> Result<String, AdapterError> {
        (self.0)().fingerprint.ok_or_else(|| "cannot tell".into())
    }
    fn identify(&self, _timeout: Duration) -> Option<DeviceIdentity> {
        Some((self.0)())
    }
}

/// A fingerprinter that records the time each read of it is given, and
/// tells nothing but the host name (`fp-host`), so that no read is kept and
/// every read of the device reaches it.
pub struct TimedFingerprinter {
    pub given: Arc<Mutex<Vec<Duration>>>,
}

impl Fingerprinter for TimedFingerprinter {
    fn get(&self) -> Result<String, AdapterError> {
        Err("cannot tell".into())
    }
    fn identify(&self, timeout: Duration) -> Option<DeviceIdentity> {
        lock(&self.given).push(timeout);
        Some(DeviceIdentity {
            fingerprint: None,
            kind: None,
            alternates: vec!["fp-host".into()],
        })
    }
}

/// Options for a harness over a [`TimedFingerprinter`] recording into
/// `given`, with the SDK's own device-read bounds (2 s and 10 s) and every
/// other bound patient.
pub fn recording(given: &Arc<Mutex<Vec<Duration>>>) -> Opts {
    Opts {
        fingerprint: Some(Arc::new(TimedFingerprinter {
            given: Arc::clone(given),
        })),
        timings: Some(Timings {
            machine_id: keygrant::machine_id::MACHINE_ID_TIMEOUT,
            activate_machine_id: keygrant::machine_id::ACTIVATE_MACHINE_ID_TIMEOUT,
            ..patient_timings()
        }),
        ..Opts::default()
    }
}

// --- the harness ----------------------------------------------------------------

#[derive(Clone, Default)]
pub struct Opts {
    pub major: Option<u32>,
    pub device_name: Option<String>,
    pub http_timeout: Option<Duration>,
    pub fingerprint: Option<Arc<dyn Fingerprinter>>,
    pub device_hold: Option<bool>,
    /// The SDK's bounds: `patient_timings()` when none.
    pub timings: Option<Timings>,
}

/// Bounds short enough for a test to wait them out: for a test that waits on
/// something that never answers. Load only makes such a wait longer, never
/// its outcome another.
pub fn fast_timings() -> Timings {
    Timings {
        lock_grace: Duration::from_millis(300),
        status_check: Duration::from_millis(300),
        machine_id: Duration::from_millis(200),
        activate_machine_id: Duration::from_millis(600),
        identify_grace: Duration::from_secs(1),
    }
}

/// A bound no call reaches when what it waits on answers, however loaded
/// the machine running the tests.
pub const PATIENT: Duration = Duration::from_secs(30);

/// Bounds no call reaches when what it waits on answers (`PATIENT`): for a
/// test that needs a read or an ask to land, and waits on nothing that never
/// answers. Every harness has them unless a test says otherwise.
pub fn patient_timings() -> Timings {
    Timings {
        lock_grace: PATIENT,
        status_check: PATIENT,
        machine_id: PATIENT,
        activate_machine_id: PATIENT,
        identify_grace: PATIENT,
    }
}

pub struct Harness {
    pub license: License,
    pub key: TestKey,
    pub storage: Arc<MemoryStorage>,
    pub clock: Arc<FakeClock>,
    pub http: Arc<FakeHttp>,
    pub registry: Arc<MemoryRegistry>,
    pub fingerprint: Arc<dyn Fingerprinter>,
    pub config: LicenseConfig,
}

impl Harness {
    pub fn new(initial: Option<StoredState>, opts: Opts) -> Harness {
        let key = TestKey::new(7);
        let storage = Arc::new(MemoryStorage::default());
        storage.set(initial);
        let clock = FakeClock::at(NOW);
        let http = Arc::new(FakeHttp::default());
        let registry = Arc::new(MemoryRegistry::default());
        let fingerprint: Arc<dyn Fingerprinter> = opts
            .fingerprint
            .clone()
            .unwrap_or_else(|| Arc::new(FixedFingerprinter("fp-test")));
        let mut config = LicenseConfig::new(PRODUCT, "https://api.test/", key.public_jwk())
            .storage(storage.clone())
            .clock(clock.clone())
            .http(http.clone())
            .registry(registry.clone())
            .fingerprinter(fingerprint.clone());
        if let Some(major) = opts.major {
            config = config.major(major);
        }
        if let Some(name) = &opts.device_name {
            config = config.device_name(name.clone());
        }
        if let Some(timeout) = opts.http_timeout {
            config = config.http_timeout(timeout);
        }
        if let Some(hold) = opts.device_hold {
            config = config.device_hold(hold);
        }
        config = config.timings(opts.timings.unwrap_or_else(patient_timings));
        let license = License::new(config.clone()).expect("a valid configuration");
        Harness {
            license,
            key,
            storage,
            clock,
            http,
            registry,
            fingerprint,
            config,
        }
    }

    pub fn empty() -> Harness {
        Harness::new(None, Opts::default())
    }

    pub fn with(initial: StoredState) -> Harness {
        Harness::new(Some(initial), Opts::default())
    }

    /// Another build of the app on this device: the same storage and server, another major.
    pub fn license_for(&self, major: u32) -> License {
        License::new(self.config.clone().major(major)).expect("a valid configuration")
    }

    /// Another instance of the app with another fingerprinter.
    pub fn license_with(&self, fingerprint: Arc<dyn Fingerprinter>) -> License {
        License::new(self.config.clone().fingerprinter(fingerprint)).expect("a valid configuration")
    }

    pub fn state(&self) -> StoredState {
        self.storage.get().expect("a stored state")
    }

    pub fn set_state(&self, state: StoredState) {
        self.storage.set(Some(state));
    }

    pub fn set_clock(&self, now: i64) {
        self.clock.set(now);
    }

    /// A lease as the test harness signs one.
    pub fn lease(&self, sub: &str, model: &str, ttl_secs: i64, now: i64) -> String {
        self.key.lease(sub, model, ttl_secs, now)
    }

    /// Answer every validate with this lease.
    pub fn validate_with(&self, lease: &str) {
        self.http.answer(
            "validate",
            200,
            json!({ "lease": lease, "activationId": "act_1" }),
        );
    }

    pub fn validates(&self) -> usize {
        self.http.count("validate")
    }
}

/// A stored state: the licence `key` on `act_1` holding `lease`, seen at NOW, heard on major 1.
pub fn licensed(key: &str, lease: &str) -> StoredState {
    StoredState {
        key: Some(key.into()),
        activation_id: Some("act_1".into()),
        lease: Some(lease.into()),
        checked_major: Some(1),
        last_seen: Some(NOW),
        ..StoredState::default()
    }
}

/// A state from JSON.
pub fn state(json: Value) -> StoredState {
    StoredState::from_json(&json).expect("a state")
}

/// The state as JSON, to compare fields.
pub fn json_of(state: &StoredState) -> Map<String, Value> {
    match state.to_json() {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// Whether `state` holds every field of `expected` as given.
pub fn assert_has(state: &StoredState, expected: Value) {
    let actual = json_of(state);
    for (name, value) in expected.as_object().expect("an object") {
        assert_eq!(actual.get(name), Some(value), "field {name} of {actual:?}");
    }
}

/// Run `work` on a thread of its own.
pub fn spawn<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> std::thread::JoinHandle<T> {
    std::thread::spawn(work)
}

/// Wait (up to 10 s) until `done` says so.
pub fn wait_until(done: impl Fn() -> bool) {
    for _ in 0..2000 {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the condition never came about");
}

// --- an in-memory file system ------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Op {
    ReadFile,
    Create,
    Write,
    Sync,
    Close,
    Rename,
    Rm,
    Readdir,
    MtimeMs,
    Mkdir,
    SyncDir,
}

pub fn errno(kind: io::ErrorKind, code: &str) -> io::Error {
    io::Error::new(kind, format!("{code}: the file system said no"))
}

type Hook = Box<dyn Fn(&mut HashMap<PathBuf, String>) + Send>;

/// How a queued operation fails: with an error of a kind, or a raw OS error code.
#[derive(Clone, Copy, Debug)]
enum Failure {
    Kind(io::ErrorKind),
    Os(i32),
}

impl Failure {
    fn error(self) -> io::Error {
        match self {
            Failure::Kind(kind) => errno(kind, &format!("{kind:?}")),
            Failure::Os(code) => io::Error::from_raw_os_error(code),
        }
    }
}

#[derive(Default)]
struct MemoryFsState {
    files: HashMap<PathBuf, String>,
    mtimes: HashMap<PathBuf, f64>,
    queued: HashMap<Op, Vec<Failure>>,
    always: HashMap<Op, io::ErrorKind>,
    holds: HashMap<Op, Vec<Gate>>,
    before: HashMap<Op, Hook>,
    ops: Vec<String>,
    changes: Vec<String>,
}

/// Files in memory, as one or more processes see them, with failures queued
/// per operation or given every time, an operation held until released,
/// something another process does just before an operation, each file's
/// mtime, every operation in order, and every one that changes a file.
#[derive(Clone, Default)]
pub struct MemoryFs {
    inner: Arc<Mutex<MemoryFsState>>,
}

impl MemoryFs {
    pub fn new(files: &[(&Path, &str)]) -> MemoryFs {
        let fs = MemoryFs::default();
        for (path, text) in files {
            fs.put(path, text);
        }
        fs
    }
    pub fn put(&self, path: &Path, text: &str) {
        lock(&self.inner)
            .files
            .insert(path.to_path_buf(), text.to_owned());
    }
    pub fn file(&self, path: &Path) -> Option<String> {
        lock(&self.inner).files.get(path).cloned()
    }
    pub fn names(&self) -> Vec<PathBuf> {
        let mut names: Vec<PathBuf> = lock(&self.inner).files.keys().cloned().collect();
        names.sort();
        names
    }
    pub fn set_mtime(&self, path: &Path, ms: f64) {
        lock(&self.inner).mtimes.insert(path.to_path_buf(), ms);
    }
    pub fn fail_next(&self, op: Op, kinds: &[io::ErrorKind]) {
        lock(&self.inner)
            .queued
            .entry(op)
            .or_default()
            .extend(kinds.iter().copied().map(Failure::Kind));
    }
    /// The next operations fail with these raw OS error codes (Windows' 32, a sharing violation, say).
    pub fn fail_next_os(&self, op: Op, codes: &[i32]) {
        lock(&self.inner)
            .queued
            .entry(op)
            .or_default()
            .extend(codes.iter().copied().map(Failure::Os));
    }
    pub fn fail_always(&self, op: Op, kind: io::ErrorKind) {
        lock(&self.inner).always.insert(op, kind);
    }
    pub fn hold_next(&self, op: Op) -> Gate {
        let gate = Gate::new();
        lock(&self.inner)
            .holds
            .entry(op)
            .or_default()
            .push(gate.clone());
        gate
    }
    pub fn before(&self, op: Op, hook: impl Fn(&mut HashMap<PathBuf, String>) + Send + 'static) {
        lock(&self.inner).before.insert(op, Box::new(hook));
    }
    pub fn ops(&self) -> Vec<String> {
        lock(&self.inner).ops.clone()
    }
    pub fn changes(&self) -> Vec<String> {
        lock(&self.inner).changes.clone()
    }
    pub fn count(&self, op: Op) -> usize {
        let name = format!("{op:?} ");
        self.ops()
            .iter()
            .filter(|line| line.starts_with(&name))
            .count()
    }

    fn enter(&self, op: Op, path: &str, changing: bool) -> io::Result<()> {
        let gate = {
            let mut inner = lock(&self.inner);
            let MemoryFsState { before, files, .. } = &mut *inner;
            if let Some(hook) = before.get(&op) {
                hook(files);
            }
            inner
                .holds
                .get_mut(&op)
                .and_then(|holds| (!holds.is_empty()).then(|| holds.remove(0)))
        };
        if let Some(gate) = gate {
            gate.wait();
        }
        let mut inner = lock(&self.inner);
        let queued = inner
            .queued
            .get_mut(&op)
            .and_then(|queue| (!queue.is_empty()).then(|| queue.remove(0)));
        let failure = queued.or_else(|| inner.always.get(&op).copied().map(Failure::Kind));
        if let Some(failure) = failure {
            return Err(failure.error());
        }
        inner.ops.push(format!("{op:?} {path}"));
        if changing {
            inner.changes.push(format!("{op:?} {path}"));
        }
        Ok(())
    }
}

struct MemoryFile {
    fs: MemoryFs,
    path: PathBuf,
}

impl StorageFile for MemoryFile {
    fn write(&mut self, data: &str) -> io::Result<()> {
        self.fs
            .enter(Op::Write, &self.path.display().to_string(), true)?;
        lock(&self.fs.inner)
            .files
            .insert(self.path.clone(), data.to_owned());
        Ok(())
    }
    fn sync(&mut self) -> io::Result<()> {
        self.fs
            .enter(Op::Sync, &self.path.display().to_string(), false)
    }
    fn close(self: Box<Self>) -> io::Result<()> {
        self.fs
            .enter(Op::Close, &self.path.display().to_string(), false)
    }
}

impl StorageFs for MemoryFs {
    fn read_file(&self, path: &Path) -> io::Result<String> {
        self.enter(Op::ReadFile, &path.display().to_string(), false)?;
        self.file(path)
            .ok_or_else(|| errno(io::ErrorKind::NotFound, "ENOENT"))
    }
    fn create(&self, path: &Path) -> io::Result<Box<dyn StorageFile>> {
        self.enter(Op::Create, &path.display().to_string(), true)?;
        let mut inner = lock(&self.inner);
        if inner.files.contains_key(path) {
            return Err(errno(io::ErrorKind::AlreadyExists, "EEXIST"));
        }
        inner.files.insert(path.to_path_buf(), String::new());
        Ok(Box::new(MemoryFile {
            fs: self.clone(),
            path: path.to_path_buf(),
        }))
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.enter(
            Op::Rename,
            &format!("{} {}", from.display(), to.display()),
            true,
        )?;
        let mut inner = lock(&self.inner);
        let text = inner
            .files
            .remove(from)
            .ok_or_else(|| errno(io::ErrorKind::NotFound, "ENOENT"))?;
        inner.files.insert(to.to_path_buf(), text);
        Ok(())
    }
    fn rm(&self, path: &Path) -> io::Result<()> {
        self.enter(Op::Rm, &path.display().to_string(), true)?;
        lock(&self.inner).files.remove(path);
        Ok(())
    }
    fn readdir(&self, dir: &Path) -> io::Result<Vec<String>> {
        self.enter(Op::Readdir, &dir.display().to_string(), false)?;
        let inner = lock(&self.inner);
        Ok(inner
            .files
            .keys()
            .filter(|path| path.parent() == Some(dir))
            .filter_map(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .collect())
    }
    fn mtime_ms(&self, path: &Path) -> io::Result<f64> {
        self.enter(Op::MtimeMs, &path.display().to_string(), false)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0);
        Ok(lock(&self.inner).mtimes.get(path).copied().unwrap_or(now))
    }
    fn mkdir(&self, dir: &Path) -> io::Result<()> {
        self.enter(Op::Mkdir, &dir.display().to_string(), false)
    }
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.enter(Op::SyncDir, &dir.display().to_string(), false)
    }
}
