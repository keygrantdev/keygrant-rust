//! Everything the SDK does to the platform goes through these adapters, so an
//! app (or a test) can replace any of them. Each has a default; see
//! [`LicenseConfig`](crate::LicenseConfig).

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::state::StoredState;
use crate::status::DeviceKind;

/// An adapter's error, whatever the adapter is.
pub type AdapterError = Box<dyn std::error::Error + Send + Sync>;

/// Where the licence state is kept.
///
/// The default is [`FileStorage`](crate::storage::FileStorage) at
/// [`default_storage_path`](crate::storage::default_storage_path).
pub trait StorageAdapter: Send + Sync {
    /// The stored state, or `None` for none. Only a state that is MISSING is
    /// `None`: any other failure to read must be an error, or the SDK would
    /// write what it read over the real state. `read_only` (a call behind one
    /// that has not finished): read and write nothing, not even to repair or
    /// migrate.
    fn read(&self, read_only: bool) -> std::io::Result<Option<StoredState>>;

    /// Save the whole state, atomically.
    fn write(&self, state: &StoredState) -> std::io::Result<()>;

    /// Optional: save `merge` of the state as stored at the moment of saving,
    /// merged again onto a newer one when another process saved in between.
    /// Used for every save when it returns `Some`; the default (`None`) has
    /// the SDK read, merge and write.
    fn update(&self, merge: &dyn Fn(StoredState) -> StoredState) -> Option<std::io::Result<()>> {
        let _ = merge;
        None
    }
}

/// Survives an app uninstall (the Windows registry, say): the earliest trial
/// start this device has had, so a reinstall does not reset a trial.
///
/// Both methods are best-effort: a stash that cannot be read reads as none,
/// and one that cannot be written is left as it was.
pub trait RegistryStash: Send + Sync {
    /// The trial start stashed (ms), if any.
    fn read_trial_start(&self) -> Option<i64>;
    /// Stash the trial start (ms).
    fn write_trial_start(&self, ms: i64);
}

/// The device clock.
pub trait Clock: Send + Sync {
    /// Milliseconds since the epoch.
    fn now(&self) -> i64;
}

/// Who this device is, as far as can be told this time ([`Fingerprinter::identify`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceIdentity {
    /// What a normal run gives, or `None` when it cannot be told this time.
    pub fingerprint: Option<String>,
    /// What `fingerprint` hashes. Only a seat keyed by a `Machine` id is held
    /// to its device; `None` means unknown, and such a seat never is.
    pub kind: Option<DeviceKind>,
    /// Other fingerprints this device answers to (such as the host-name
    /// fingerprint).
    pub alternates: Vec<String>,
}

/// This device's fingerprint.
///
/// The default ([`DefaultFingerprinter`](crate::DefaultFingerprinter)) hashes
/// the operating system's machine id.
pub trait Fingerprinter: Send + Sync {
    /// This device's fingerprint. An error, or no answer in time, is read as
    /// "cannot tell this time". A fingerprinter with only `get()` says nothing
    /// of what its fingerprint hashes, so its seats are NEVER held to their
    /// device: implement `identify` and report `DeviceKind::Machine` for a
    /// stable machine id that should be.
    fn get(&self) -> Result<String, AdapterError>;

    /// Optional, and what the SDK uses when it returns `Some`: this device's
    /// fingerprint (`None` when it cannot be read this time), what it hashes,
    /// and any other fingerprints this device has been activated under. The
    /// default (`None`) has the SDK use `get()`. `timeout` is how long the
    /// read may take; the SDK abandons it a second after that.
    fn identify(&self, timeout: Duration) -> Option<DeviceIdentity> {
        let _ = timeout;
        None
    }
}

/// A server's answer: its status and its body, parsed as JSON (`None` when it
/// is not JSON).
#[derive(Debug, Clone, PartialEq)]
pub struct HttpResponse {
    /// The HTTP status.
    pub status: u16,
    /// The body as JSON, when it is JSON.
    pub json: Option<Value>,
}

/// The HTTP the SDK speaks: one JSON POST.
///
/// The default is [`UreqHttp`](crate::UreqHttp).
pub trait HttpAdapter: Send + Sync {
    /// POST `body` as JSON to `url`, taking at most `timeout`. Any HTTP status
    /// is an `Ok` answer (a 4xx or 5xx included); only a request that got no
    /// answer at all is an error. The SDK holds the call to `timeout` whatever
    /// the adapter does with it.
    fn post(
        &self,
        url: &str,
        body: &Value,
        timeout: Duration,
    ) -> Result<HttpResponse, AdapterError>;
}

/// Adapter overrides: any left `None` uses the default.
#[derive(Clone, Default)]
pub struct Adapters {
    /// Where the state is kept.
    pub storage: Option<Arc<dyn StorageAdapter>>,
    /// The uninstall-surviving trial stash.
    pub registry: Option<Arc<dyn RegistryStash>>,
    /// The clock.
    pub clock: Option<Arc<dyn Clock>>,
    /// The device fingerprint.
    pub fingerprint: Option<Arc<dyn Fingerprinter>>,
    /// The HTTP client.
    pub http: Option<Arc<dyn HttpAdapter>>,
}

impl fmt::Debug for Adapters {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let given = |set: bool| if set { "custom" } else { "default" };
        f.debug_struct("Adapters")
            .field("storage", &given(self.storage.is_some()))
            .field("registry", &given(self.registry.is_some()))
            .field("clock", &given(self.clock.is_some()))
            .field("fingerprint", &given(self.fingerprint.is_some()))
            .field("http", &given(self.http.is_some()))
            .finish()
    }
}

/// The default registry stash: stashes nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopRegistry;

impl RegistryStash for NoopRegistry {
    fn read_trial_start(&self) -> Option<i64> {
        None
    }
    fn write_trial_start(&self, _ms: i64) {}
}

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        system_now_ms()
    }
}

/// The system clock in ms since the epoch (negative before it).
pub(crate) fn system_now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_millis()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_millis()).map_or(i64::MIN, |ms| -ms),
    }
}
