//! The machinery under `License`: the adapters, one call at a time, every
//! request bounded, the stored state and its clock guard, and the online
//! validate with what its answer does to the stored state. `License` decides
//! when to go online and what to answer.

mod renewal_ask;

use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::adapters::{
    Clock, Fingerprinter, HttpAdapter, HttpResponse, NoopRegistry, RegistryStash, StorageAdapter,
    SystemClock,
};
use crate::bounded::bounded;
use crate::config::{LicenseConfig, Timings};
use crate::device::{Device, DeviceIdentityReader};
use crate::error::ConfigError;
use crate::fingerprint::DefaultFingerprinter;
use crate::http::UreqHttp;
use crate::lease::{verify_lease, KeySet, VerifyResult};
use crate::offline::{
    check_lease, guarded_now, major_of, merged, saved_last_seen, trial_lease_of, Basis, LeaseCheck,
    LeaseHolder, TrialLease,
};
use crate::serial::Serial;
use crate::state::{Patch, StoredState};
use crate::status::{DeviceKind, LicenseStatus};
use crate::storage::{default_storage_path, legacy_storage_path, FileStorage, FileStorageOptions};
use crate::wire::js_trim;

/// The outcome of an online validate: the server's verdict, or no answer at all.
pub(crate) enum Renewal {
    Answered(Option<LicenseStatus>),
    Unanswered,
}

/// Whose a lease must be: the key, activation and install kind stored.
pub(crate) struct Holder<'a> {
    pub key: Option<&'a str>,
    pub activation_id: Option<&'a str>,
    pub device_kind: Option<DeviceKind>,
}

impl<'a> Holder<'a> {
    pub fn of(state: &'a StoredState) -> Self {
        Holder {
            key: state.key.as_deref(),
            activation_id: state.activation_id.as_deref(),
            device_kind: state.device_kind,
        }
    }
}

/// How the clock guard brought back by `server_now` is saved.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuardSave {
    /// A failed save fails the call (activate).
    Strict,
    /// A failed save fails nothing (a status's renewal).
    BestEffort,
}

pub(crate) struct Engine {
    pub product: String,
    pub major: i64,
    base: String,
    keys: KeySet,
    pub storage: Arc<dyn StorageAdapter>,
    pub registry: Arc<dyn RegistryStash>,
    pub clock: Arc<dyn Clock>,
    http: Arc<dyn HttpAdapter>,
    device_reader: DeviceIdentityReader,
    http_timeout: Duration,
    device_hold: bool,
    pub timings: Timings,
    serial: Serial,
}

/// Whether a product slug can name a file and a URL path segment.
fn valid_product(product: &str) -> bool {
    !product.is_empty()
        && product != "."
        && product != ".."
        && !product
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || "/\\?#%".contains(c))
}

/// The API base, trimmed and with one trailing slash removed, when it is an
/// absolute http(s) URL. Trimmed: a base read from a file or the environment
/// often ends in a newline, which would make every request fail.
fn api_base(url: &str) -> Option<String> {
    let url = js_trim(url);
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))?;
    if rest.is_empty() || rest.starts_with('/') || rest.contains(char::is_whitespace) {
        return None;
    }
    Some(url.strip_suffix('/').unwrap_or(url).to_owned())
}

/// What a storage adapter that panicked is: a read or save that failed.
fn panicked(what: &str) -> io::Error {
    io::Error::other(format!("the storage adapter panicked on {what}"))
}

impl Engine {
    pub fn new(config: &LicenseConfig) -> Result<Engine, ConfigError> {
        if !valid_product(&config.product) {
            return Err(ConfigError::Product(config.product.clone()));
        }
        let base = api_base(&config.api_base_url)
            .ok_or_else(|| ConfigError::ApiBaseUrl(config.api_base_url.clone()))?;
        let keys =
            KeySet::parse(&config.public_jwk).map_err(|e| ConfigError::PublicKey(e.to_string()))?;
        let adapters = config.adapters.clone();
        let storage: Arc<dyn StorageAdapter> = match adapters.storage {
            Some(storage) => storage,
            None => {
                let path =
                    default_storage_path(&config.product).ok_or(ConfigError::StorageLocation)?;
                let options = FileStorageOptions {
                    migrate_from: legacy_storage_path(&config.product),
                    ..FileStorageOptions::default()
                };
                Arc::new(FileStorage::with_options(path, options))
            }
        };
        let fingerprint: Arc<dyn Fingerprinter> = adapters
            .fingerprint
            .unwrap_or_else(|| Arc::new(DefaultFingerprinter::new()));
        Ok(Engine {
            product: config.product.clone(),
            // As the server reads it (`major_of`): one reading, used wherever the major is.
            major: major_of(config.major),
            base,
            keys,
            storage,
            registry: adapters.registry.unwrap_or_else(|| Arc::new(NoopRegistry)),
            clock: adapters.clock.unwrap_or_else(|| Arc::new(SystemClock)),
            http: adapters.http.unwrap_or_else(|| Arc::new(UreqHttp::new())),
            device_reader: DeviceIdentityReader::new(fingerprint, config.timings.identify_grace),
            http_timeout: config.http_timeout,
            device_hold: config.device_hold,
            timings: config.timings,
            serial: Serial::new(),
        })
    }

    /// Run `work` once every call before it on this instance has finished
    /// (see `serial.rs`); `work` is told whether it must write nothing. The
    /// calls behind it wait for it up to the HTTP bound and the lock grace.
    pub fn serial<T>(&self, work: impl FnOnce(bool) -> T) -> T {
        self.serial.run(
            self.http_timeout.saturating_add(self.timings.lock_grace),
            work,
        )
    }

    /// POST to the product's endpoint, ALWAYS bounded: by `timeout`, else by
    /// the configured HTTP bound, which is passed to the adapter and also
    /// kept by the SDK itself. `Err` when no answer came.
    pub fn post(
        &self,
        action: &str,
        body: Value,
        timeout: Option<Duration>,
    ) -> Result<HttpResponse, ()> {
        let url = format!("{}/v1/products/{}/{}", self.base, self.product, action);
        let bound = timeout.unwrap_or(self.http_timeout);
        let http = Arc::clone(&self.http);
        match bounded(bound, move || http.post(&url, &body, bound)) {
            Some(Ok(response)) => Ok(response),
            _ => Err(()),
        }
    }

    pub fn verify(&self, lease: &str, now: i64) -> VerifyResult {
        verify_lease(lease, &self.keys, now)
    }

    /// A licence lease verified offline and held to this product and build,
    /// and to the key and activation it is for, on this device.
    pub fn evaluate(&self, lease: &str, now: i64, holder: &Holder<'_>) -> LeaseCheck {
        let device = self.device(self.timings.machine_id, holder.device_kind);
        let bound = LeaseHolder {
            key: holder.key.unwrap_or_default(),
            activation_id: holder.activation_id.unwrap_or_default(),
            claims: &device.claims,
            can_tell: device.can_tell,
        };
        check_lease(&self.verify(lease, now), &self.product, self.major, &bound)
    }

    /// A trial lease verified offline, for this product and this device, with
    /// the end it was signed to (ms) and its entitlements when it is valid
    /// (offline.rs `trial_lease_of`).
    pub fn trial_lease(
        &self,
        lease: &str,
        now: i64,
        install_kind: Option<DeviceKind>,
    ) -> TrialLease {
        let device = self.device(self.timings.machine_id, install_kind);
        trial_lease_of(
            &self.verify(lease, now),
            &self.product,
            &device.claims,
            device.can_tell,
        )
    }

    /// Who this device is, read for at most about `timeout`, on an install
    /// keyed as `install_kind`, held to its device as configured
    /// (`device_hold`): device.rs `device_from`.
    pub fn device(&self, timeout: Duration, install_kind: Option<DeviceKind>) -> Device {
        self.device_reader
            .read(timeout, install_kind, self.device_hold)
    }

    /// The install's kind as it bears on this device: none when nothing is held.
    pub fn install_kind(&self, stored: Option<DeviceKind>) -> Option<DeviceKind> {
        if self.device_hold {
            stored
        } else {
            None
        }
    }

    /// The stored state; `read_only` behind a call that has not finished.
    ///
    /// A storage adapter that panics is a state that cannot be read: an error,
    /// which `status()` and `start_trial()` answer as `Unknown`.
    pub fn load(&self, read_only: bool) -> io::Result<StoredState> {
        let storage = &self.storage;
        let read = catch_unwind(AssertUnwindSafe(|| storage.read(read_only)))
            .unwrap_or_else(|_| Err(panicked("read")));
        Ok(read?.unwrap_or_default())
    }

    /// Persist a patch onto the state as it is stored NOW, merged by what it
    /// is based on, and advance the clock guard (or set it to `guard`).
    pub fn save(&self, patch: &Patch, guard: Option<i64>, basis: &Basis) -> io::Result<()> {
        let clock = Arc::clone(&self.clock);
        let merge = |latest: StoredState| {
            let mut next = merged(&latest, patch, basis);
            next.last_seen = Some(saved_last_seen(latest.last_seen, clock.now(), guard));
            next
        };
        // A storage adapter that panics is a save that failed.
        catch_unwind(AssertUnwindSafe(|| {
            if let Some(result) = self.storage.update(&merge) {
                return result;
            }
            let latest = self.load(false)?;
            self.storage.write(&merge(latest))
        }))
        .unwrap_or_else(|_| Err(panicked("save")))
    }

    /// A save whose failure fails nothing: the answer stands. Whether it was saved.
    pub fn save_best_effort(&self, patch: &Patch, guard: Option<i64>, basis: &Basis) -> bool {
        self.save(patch, guard, basis).is_ok()
    }

    /// Never earlier than the highest wall-clock ever seen (anti clock-rollback).
    pub fn effective_now(&self, state: &StoredState) -> i64 {
        guarded_now(self.clock.now(), state.last_seen)
    }
}
