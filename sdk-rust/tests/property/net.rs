//! The machine a property test of `use_meter` runs the real `License` on: two
//! processes of the app over one state file and one device clock, a KeyGrant
//! server whose every answer a step may lose, delay or refuse, a disk that may
//! refuse its writes, and a device clock that jumps forward and back.
//!
//! Concurrency is deterministic. A request of a process is PARKED at the world,
//! which decides when and how it is answered, and a call is started only once
//! every call running before it is parked at a request or done, so one thread
//! moves at a time and a case replays the same way on any machine. The races that
//! matter (a process's whole call between another's request and its answer) are
//! the order the world answers its parked requests in. A request nobody answers
//! is timed out one at a time, as an adapter's is at its bound: no answer, to the
//! SDK; no clock is raced against a timer.

use std::collections::{HashMap, HashSet};
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use keygrant::{
    ActivateResult, AdapterError, Clock, DeactivateResult, Error, HttpAdapter, HttpResponse,
    License, LicenseConfig, SpendKind, StorageAdapter, StoredState, UseResult,
};
use serde_json::{json, Value};

use super::server::{PropServer, Served};
use crate::common::{patient_timings, FixedFingerprinter, TestKey, PRODUCT};

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A property that did not hold: which, and where.
#[derive(Debug, Clone)]
pub struct PropertyFailure {
    pub invariant: String,
    pub message: String,
}

impl PropertyFailure {
    pub fn new(invariant: &str, message: impl Into<String>) -> PropertyFailure {
        PropertyFailure {
            invariant: invariant.to_owned(),
            message: format!("{invariant}: {}", message.into()),
        }
    }
}

pub type Checked<T> = Result<T, PropertyFailure>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Net {
    Good,
    Down,
    Flaky,
}

impl Net {
    pub fn parse(word: &str) -> Net {
        match word {
            "good" => Net::Good,
            "down" => Net::Down,
            _ => Net::Flaky,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fate {
    Ok,
    Lost,
    LostAfter,
    Hang,
    HangAfter,
    Fail,
    FailAfter,
    Limited,
}

pub fn fate_of(net: Net, r: f64) -> Fate {
    let thresholds: &[(f64, Fate)] = match net {
        Net::Good => return Fate::Ok,
        Net::Down => &[
            (0.45, Fate::Lost),
            (0.75, Fate::LostAfter),
            (0.88, Fate::Hang),
        ],
        Net::Flaky => &[
            (0.35, Fate::Ok),
            (0.45, Fate::Lost),
            (0.55, Fate::LostAfter),
            (0.65, Fate::Fail),
            (0.75, Fate::FailAfter),
            (0.82, Fate::Limited),
            (0.9, Fate::Hang),
        ],
    };
    thresholds
        .iter()
        .find(|(below, _)| r < *below)
        .map_or(Fate::HangAfter, |(_, fate)| *fate)
}

/// A request a process made for one of its `use_meter` calls.
#[derive(Debug, Clone)]
pub struct Req {
    pub meter: String,
    pub n: u64,
    pub id: String,
    /// The server recorded the spend.
    pub applied: bool,
    /// The SDK got a usable answer.
    pub answered: bool,
}

/// What a parked request is given.
pub enum Delivery {
    Response(HttpResponse),
    Fail,
}

/// A request parked at the world: it is answered, lost or left to time out when
/// the world says.
pub struct Parked {
    pub index: usize,
    pub action: String,
    pub body: Value,
    pub fate: Fate,
    pub hung: bool,
    delivery: Sender<Delivery>,
}

/// What a call came to.
pub enum CallValue {
    Use(UseResult),
    Status,
    Deactivate(Result<DeactivateResult, Error>),
    Activate(ActivateResult),
}

/// A call under way: the process it is made on, and whether it is done.
pub struct Running {
    pub p: usize,
    done: AtomicBool,
    value: Mutex<Option<Result<CallValue, String>>>,
}

impl Running {
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }

    /// What the call came to; an error text when it panicked.
    pub fn take(&self) -> Result<CallValue, String> {
        lock(&self.value)
            .take()
            .unwrap_or_else(|| Err("no result".into()))
    }
}

/// Everything the adapters and the world share.
pub struct Inner {
    pub state: StoredState,
    pub server: PropServer,
    pub net: Net,
    pub rs: Vec<f64>,
    pub rk: usize,
    pub parked: [Option<Parked>; 2],
    pub active: Vec<Arc<Running>>,
    pub procs: Vec<Arc<License>>,
    pub use_reqs: [Vec<Req>; 2],
    /// The releases a step made: whether the server recorded each, and the entries it carried.
    pub releases: Vec<(bool, HashSet<String>)>,
    /// The ids of spends a request reported as plain spends (not as voids): spends the customer was told were made.
    pub reported_plain: HashSet<String>,
    /// The voids the contract let be lost before the server heard them.
    pub lost_voids: HashSet<String>,
    /// The voids held in memory when a save left the journal at its size: one that did not land may have found no room.
    pub refused_voids: HashSet<String>,
    /// A save that dropped a void with nothing to replace it, found inside the SDK's save and reported by the next check.
    pub violation: Option<String>,
    pub tracing: bool,
    pub trace: Vec<String>,
    pub labels: HashMap<String, String>,
    pub pre_ids: HashSet<String>,
}

pub struct Shared {
    pub inner: Mutex<Inner>,
    changed: Condvar,
    pub key: TestKey,
    pub true_now: AtomicI64,
    pub offset: AtomicI64,
    pub disk_failing: AtomicBool,
}

const ENTRIES: usize = 64;
const VOIDS: usize = 16;

impl Shared {
    pub fn new(state: StoredState, server: PropServer, key: TestKey, now: i64) -> Arc<Shared> {
        let inner = Inner {
            state,
            server,
            net: Net::Good,
            rs: vec![0.5],
            rk: 0,
            parked: [None, None],
            active: Vec::new(),
            procs: Vec::new(),
            use_reqs: [Vec::new(), Vec::new()],
            releases: Vec::new(),
            reported_plain: HashSet::new(),
            lost_voids: HashSet::new(),
            refused_voids: HashSet::new(),
            violation: None,
            tracing: false,
            trace: Vec::new(),
            labels: HashMap::new(),
            pre_ids: HashSet::new(),
        };
        Arc::new(Shared {
            inner: Mutex::new(inner),
            changed: Condvar::new(),
            key,
            true_now: AtomicI64::new(now),
            offset: AtomicI64::new(0),
            disk_failing: AtomicBool::new(false),
        })
    }

    pub fn lock(&self) -> MutexGuard<'_, Inner> {
        lock(&self.inner)
    }

    /// The device clock.
    pub fn clock_now(&self) -> i64 {
        self.true_now.load(Ordering::SeqCst) + self.offset.load(Ordering::SeqCst)
    }

    /// Something changed that the world may be waiting for. The lock is taken
    /// first, so a waiter that has checked and not yet slept cannot miss it.
    fn signal(&self) {
        drop(self.lock());
        self.changed.notify_all();
    }

    /// Wait until `condition` holds of the shared state.
    pub fn until(&self, what: &str, condition: impl Fn(&Inner) -> bool) -> Checked<()> {
        let began = Instant::now();
        let mut guard = self.lock();
        loop {
            if condition(&guard) {
                return Ok(());
            }
            if began.elapsed() > Duration::from_secs(30) {
                return Err(PropertyFailure::new(
                    "stuck",
                    format!("a call never settled ({what}): {}", whereabouts(&guard)),
                ));
            }
            guard = self
                .changed
                .wait_timeout(guard, Duration::from_millis(50))
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }

    /// A process of the app: the real `License`, over the world's adapters.
    pub fn process(self: &Arc<Self>, index: usize) -> Arc<License> {
        let config = LicenseConfig::new(PRODUCT, "https://api.test/", self.key.public_jwk())
            .storage(Arc::new(WorldStorage {
                shared: Arc::clone(self),
            }))
            .clock(Arc::new(WorldClock {
                shared: Arc::clone(self),
            }))
            .http(Arc::new(WorldHttp {
                shared: Arc::clone(self),
                index,
            }))
            .fingerprinter(Arc::new(FixedFingerprinter("fp-test")))
            .timings(patient_timings());
        Arc::new(License::new(config).expect("a licence"))
    }

    /// Start a call on process `p` on a thread of its own.
    pub fn launch(
        self: &Arc<Self>,
        p: usize,
        work: impl FnOnce(&License) -> CallValue + Send + 'static,
    ) -> Arc<Running> {
        let running = Arc::new(Running {
            p,
            done: AtomicBool::new(false),
            value: Mutex::new(None),
        });
        let license = {
            let mut inner = self.lock();
            inner.active.push(Arc::clone(&running));
            Arc::clone(&inner.procs[p])
        };
        let (shared, slot) = (Arc::clone(self), Arc::clone(&running));
        std::thread::spawn(move || {
            let value = catch_unwind(AssertUnwindSafe(|| work(&license))).map_err(|panic| {
                panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
                    .unwrap_or_else(|| "a panic".into())
            });
            *lock(&slot.value) = Some(value);
            slot.done.store(true, Ordering::SeqCst);
            shared.signal();
        });
        running
    }

    /// Every call running is parked at a request or done: nothing moves until the world moves it.
    fn quiescent(inner: &Inner) -> bool {
        (0..2).all(|p| {
            // The first call of a process not done runs (the others wait their turn); it is moving unless it is parked.
            let running = inner.active.iter().any(|a| a.p == p && !a.is_done());
            !running || inner.parked[p].is_some()
        })
    }

    /// Wait for a call just started to park at a request or be done.
    pub fn settle(&self, step: usize) -> Checked<()> {
        self.until(&format!("step {step}"), Shared::quiescent)
    }

    /// Answer the requests parked, one at a time in an order the case fixes, until
    /// every call started is done. Requests left to time out (hung) are timed out last.
    pub fn drive(&self, step: usize) -> Checked<()> {
        for rounds in 0.. {
            self.settle(step)?;
            let mut inner = self.lock();
            if inner.active.iter().all(|a| a.is_done()) {
                break;
            }
            if rounds > 400 {
                return Err(PropertyFailure::new(
                    "stuck",
                    format!("a call never settled (step {step})"),
                ));
            }
            let last = inner.rs.last().copied().unwrap_or(0.5);
            // Two requests parked: the case says whose is answered first.
            let waiting: Vec<usize> = (0..2)
                .filter(|i| inner.parked[*i].as_ref().is_some_and(|p| !p.hung))
                .collect();
            if !waiting.is_empty() {
                let pick = if waiting.len() == 1 || last < 0.5 {
                    waiting[0]
                } else {
                    waiting[waiting.len() - 1]
                };
                let req = inner.parked[pick].take().expect("a parked request");
                release(&mut inner, req);
                continue;
            }
            // Two left that nobody answers: one at a time, in the order the case fixes, so a case replays the same way.
            let hung: Vec<usize> = (0..2).filter(|i| inner.parked[*i].is_some()).collect();
            if hung.is_empty() {
                return Err(PropertyFailure::new(
                    "stuck",
                    format!(
                        "a call never settled (step {step}): {}",
                        whereabouts(&inner)
                    ),
                ));
            }
            let pick = if hung.len() == 1 || last < 0.5 {
                hung[0]
            } else {
                hung[hung.len() - 1]
            };
            // A request nobody answers times out, as an adapter's does at its bound (no answer, to the SDK).
            let silent = inner.parked[pick].take().expect("a parked request");
            let _ = silent.delivery.send(Delivery::Fail);
        }
        self.lock().active.clear();
        Ok(())
    }
}

/// Where every call is, for a failure that says a call never settled.
fn whereabouts(inner: &Inner) -> String {
    let calls: Vec<String> = inner
        .active
        .iter()
        .enumerate()
        .map(|(i, a)| {
            format!(
                "call {i} on p{}: {}",
                a.p,
                if a.is_done() { "done" } else { "running" }
            )
        })
        .collect();
    let parked: Vec<String> = inner
        .parked
        .iter()
        .enumerate()
        .map(|(i, p)| match p {
            None => format!("p{i}: nothing parked"),
            Some(p) => format!(
                "p{i}: {} {:?}{}",
                p.action,
                p.fate,
                if p.hung { " hung" } else { "" }
            ),
        })
        .collect();
    format!("{}; {}", calls.join("; "), parked.join("; "))
}

/// An id as the trace shows it: the order it was first seen in, so a case reads the same every time.
pub fn label(inner: &mut Inner, id: &str) -> String {
    if id.is_empty() {
        return String::new();
    }
    let next = inner.labels.len();
    let pre = inner.pre_ids.contains(id);
    inner
        .labels
        .entry(id.to_owned())
        .or_insert_with(|| {
            if pre {
                id[..6].to_owned()
            } else {
                format!("s{next}")
            }
        })
        .clone()
}

/// What a request comes to over a network that may lose it, or its answer, or both.
fn release(inner: &mut Inner, mut req: Parked) {
    let index = req.index;
    let respond = |req: &Parked, response: HttpResponse| {
        let _ = req.delivery.send(Delivery::Response(response));
    };
    let answer = |inner: &mut Inner, req: &Parked, answered: bool| -> Served {
        let served = inner.server.handle(&req.action, &req.body);
        noted(inner, req, Some(&served), answered);
        served
    };
    match req.fate {
        Fate::Ok => {
            let served = answer(inner, &req, true);
            respond(
                &req,
                HttpResponse {
                    status: served.status,
                    json: Some(served.json),
                },
            );
        }
        Fate::LostAfter => {
            answer(inner, &req, false);
            let _ = req.delivery.send(Delivery::Fail);
        }
        Fate::HangAfter => {
            answer(inner, &req, false);
            req.hung = true;
            inner.parked[index] = Some(req);
        }
        Fate::FailAfter => {
            let served = answer(inner, &req, false);
            respond(
                &req,
                HttpResponse {
                    status: 500,
                    json: Some(served.json),
                },
            );
        }
        Fate::Lost => {
            noted(inner, &req, None, false);
            let _ = req.delivery.send(Delivery::Fail);
        }
        Fate::Hang => {
            noted(inner, &req, None, false);
            req.hung = true;
            inner.parked[index] = Some(req);
        }
        Fate::Fail => {
            noted(inner, &req, None, false);
            respond(
                &req,
                HttpResponse {
                    status: 503,
                    json: Some(json!({ "error": "unavailable" })),
                },
            );
        }
        Fate::Limited => {
            noted(inner, &req, None, false);
            respond(
                &req,
                HttpResponse {
                    status: 429,
                    json: Some(json!({ "error": "rate-limited" })),
                },
            );
        }
    }
}

fn noted(inner: &mut Inner, req: &Parked, served: Option<&Served>, answered: bool) {
    let body = &req.body;
    if inner.tracing {
        let reports: Vec<String> = body["spends"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| {
                let who = label(inner, e["id"].as_str().unwrap_or_default());
                let void = if e.get("void").is_some() { "v" } else { "" };
                format!(
                    "{who}:{}:{}{void}",
                    e["meter"].as_str().unwrap_or_default(),
                    e["n"]
                )
            })
            .collect();
        let id = label(inner, body["id"].as_str().unwrap_or_default());
        let line = format!(
            "  p{} {} {{meter:{},n:{},id:{id},spends:[{}]}} -> {} {}",
            req.index,
            req.action,
            body["meter"].as_str().unwrap_or_default(),
            body["n"],
            reports.join(","),
            served.map_or("lost".to_owned(), |s| s.status.to_string()),
            if answered { "answered" } else { "unanswered" },
        );
        inner.trace.push(line);
    }
    for e in body["spends"].as_array().into_iter().flatten() {
        if e.get("void").is_none() {
            if let Some(plain) = e["id"].as_str() {
                inner.reported_plain.insert(plain.to_owned());
            }
        }
    }
    if req.action == "deactivate" {
        let ids = body["spends"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e["id"].as_str().map(str::to_owned))
            .collect();
        inner.releases.push((served.is_some(), ids));
    }
    if req.action == "use" {
        inner.use_reqs[req.index].push(Req {
            meter: body["meter"].as_str().unwrap_or_default().to_owned(),
            n: body["n"].as_u64().unwrap_or(0),
            id: body["id"].as_str().unwrap_or_default().to_owned(),
            applied: served.is_some_and(|s| s.spent.is_some()),
            answered,
        });
    }
}

// --- the adapters -----------------------------------------------------------------------

struct WorldClock {
    shared: Arc<Shared>,
}

impl Clock for WorldClock {
    fn now(&self) -> i64 {
        self.shared.clock_now()
    }
}

struct WorldHttp {
    shared: Arc<Shared>,
    index: usize,
}

impl HttpAdapter for WorldHttp {
    fn post(
        &self,
        url: &str,
        body: &Value,
        _timeout: Duration,
    ) -> Result<HttpResponse, AdapterError> {
        let action = url.rsplit('/').next().unwrap_or_default().to_owned();
        let (sender, receiver) = channel();
        {
            let mut inner = self.shared.lock();
            let r = inner
                .rs
                .get(inner.rk % inner.rs.len().max(1))
                .copied()
                .unwrap_or(0.5);
            inner.rk += 1;
            let fate = fate_of(inner.net, r);
            inner.parked[self.index] = Some(Parked {
                index: self.index,
                action,
                body: body.clone(),
                fate,
                hung: false,
                delivery: sender,
            });
        }
        self.shared.changed.notify_all();
        match receiver.recv() {
            Ok(Delivery::Response(response)) => Ok(response),
            Ok(Delivery::Fail) => Err("network down".into()),
            Err(_) => Err("the world is gone".into()),
        }
    }
}

struct WorldStorage {
    shared: Arc<Shared>,
}

fn busy() -> io::Error {
    io::Error::new(io::ErrorKind::ResourceBusy, "EBUSY: the disk said no")
}

impl StorageAdapter for WorldStorage {
    fn read(&self, _read_only: bool) -> io::Result<Option<StoredState>> {
        Ok(Some(self.shared.lock().state.clone()))
    }

    fn write(&self, state: &StoredState) -> io::Result<()> {
        let mut inner = self.shared.lock();
        if self.shared.disk_failing.load(Ordering::SeqCst) {
            return Err(busy());
        }
        landed(&mut inner, state);
        inner.state = state.clone();
        Ok(())
    }

    fn update(&self, merge: &dyn Fn(StoredState) -> StoredState) -> Option<io::Result<()>> {
        let mut inner = self.shared.lock();
        let next = merge(inner.state.clone());
        if self.shared.disk_failing.load(Ordering::SeqCst) {
            return Some(Err(busy()));
        }
        landed(&mut inner, &next);
        inner.state = next;
        Some(Ok(()))
    }
}

/// A save that landed. A void that left the journal and that the server did not confirm was replaced, and a
/// replacement happens only when a void lands with sixteen standing: for each void that lands (one now in the journal
/// that was not, or one held that is in neither, whether refused or confirmed in the same save) the oldest may go. A
/// void dropped otherwise is a fault, kept for the next check to report. A seat taken or dropped is the release's and
/// the restart's to account for.
fn landed(inner: &mut Inner, after: &StoredState) {
    let before = &inner.state;
    if before.activation_id != after.activation_id || after.key.is_none() {
        return;
    }
    let was = before.spends.clone().unwrap_or_default();
    let now = after.spends.clone().unwrap_or_default();
    let kept: HashSet<&str> = now.iter().map(|e| e.id.as_str()).collect();
    let had: HashSet<&str> = was.iter().map(|e| e.id.as_str()).collect();
    let dropped: Vec<&str> = was
        .iter()
        .filter(|e| {
            e.kind == SpendKind::Void
                && !kept.contains(e.id.as_str())
                && !inner.server.voided.contains(&e.id)
        })
        .map(|e| e.id.as_str())
        .collect();
    let held_voids: Vec<String> = inner
        .procs
        .iter()
        .flat_map(|p| p.held_changes())
        .filter_map(|op| match op {
            keygrant::journal::JournalOp::Spend(s) if s.kind == SpendKind::Void => Some(s.id),
            _ => None,
        })
        .collect();
    let landing: HashSet<&str> = now
        .iter()
        .filter(|e| e.kind == SpendKind::Void && !had.contains(e.id.as_str()))
        .map(|e| e.id.as_str())
        .chain(
            held_voids
                .iter()
                .map(String::as_str)
                .filter(|id| !had.contains(id) && !kept.contains(id)),
        )
        .collect();
    let standing = was.iter().filter(|e| e.kind == SpendKind::Void).count();
    let (dropped_ids, landing_count) = (
        dropped
            .iter()
            .map(|id| (*id).to_owned())
            .collect::<Vec<_>>(),
        landing.len(),
    );
    let now_voids = now.iter().filter(|e| e.kind == SpendKind::Void).count();
    let full = now.len() - usize::from(now_voids >= VOIDS) >= ENTRIES;
    let unplaced: Vec<String> = held_voids
        .iter()
        .filter(|id| !kept.contains(id.as_str()))
        .cloned()
        .collect();
    if !dropped_ids.is_empty() {
        if dropped_ids.len() <= landing_count && standing + landing_count > VOIDS {
            inner.lost_voids.extend(dropped_ids);
        } else if inner.violation.is_none() {
            inner.violation = Some(format!(
                "{} void(s) left the journal of {standing} voids with {landing_count} landing, and nothing was to replace them",
                dropped_ids.len()
            ));
        }
    }
    // Left at its size, with sixteen voids' oldest not yet replaced, the journal took no more: a void that was held and did not land found no room.
    if full {
        inner.refused_voids.extend(unplaced);
    }
}
