//! The machinery under `License`: the adapters, one call at a time, every
//! request bounded, the stored state and its clock guard, and the online
//! validate with what its answer does to the stored state. `License` decides
//! when to go online and what to answer.

use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};

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
use crate::lease::{verify_lease, PublicKey, VerifyResult};
use crate::offline::{
    check_lease, clock_correction, guarded_now, major_of, merged, saved_last_seen, trial_lease_of,
    Asked, Basis, LeaseCheck, LeaseHolder, SignedAt, TrialLease,
};
use crate::pickup::RandomSource;
use crate::renewal::{renewal_outcome, RenewalInput};
use crate::serial::Serial;
use crate::state::{present, Patch, StoredState};
use crate::status::{DeviceKind, LicenseStatus};
use crate::storage::{default_storage_path, legacy_storage_path, FileStorage, FileStorageOptions};
use crate::wire::{js_trim, parse_lease_body};

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
    key: PublicKey,
    pub storage: Arc<dyn StorageAdapter>,
    pub registry: Arc<dyn RegistryStash>,
    pub clock: Arc<dyn Clock>,
    http: Arc<dyn HttpAdapter>,
    device_reader: DeviceIdentityReader,
    http_timeout: Duration,
    device_hold: bool,
    pub timings: Timings,
    /// Where a purchase secret's bytes come from.
    pub(crate) random: RandomSource,
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
        let key = PublicKey::parse(&config.public_jwk)
            .map_err(|e| ConfigError::PublicKey(e.to_string()))?;
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
            key,
            storage,
            registry: adapters.registry.unwrap_or_else(|| Arc::new(NoopRegistry)),
            clock: adapters.clock.unwrap_or_else(|| Arc::new(SystemClock)),
            http: adapters.http.unwrap_or_else(|| Arc::new(UreqHttp::new())),
            device_reader: DeviceIdentityReader::new(fingerprint, config.timings.identify_grace),
            http_timeout: config.http_timeout,
            device_hold: config.device_hold,
            timings: config.timings,
            random: config.random,
            serial: Serial::new(),
        })
    }

    /// Run `work` once every call before it on this instance has finished
    /// (see `serial.rs`); `work` is told whether it must write nothing. The
    /// calls behind it wait for it up to the HTTP bound and the lock grace.
    pub fn serial<T>(&self, work: impl FnOnce(bool) -> T) -> T {
        self.serial_longer(Duration::ZERO, work)
    }

    /// [`serial`](Self::serial), for a call that may make a second request
    /// bounded by `extra` (`start_trial()`: a claim, then its trial ask): the
    /// calls behind it wait that much longer.
    pub fn serial_longer<T>(&self, extra: Duration, work: impl FnOnce(bool) -> T) -> T {
        let bound = self
            .http_timeout
            .saturating_add(self.timings.lock_grace)
            .saturating_add(extra);
        self.serial.run(bound, work)
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
        verify_lease(lease, &self.key, now)
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

    /// `now` for judging a lease the server has just signed, with the clock
    /// guard reconciled to it: when the guard is more than the tolerance past
    /// the lease's signing time, it comes back to the device clock, never
    /// below `iat`. Only a lease signed FOR THIS DEVICE (this key and
    /// activation, no older than the one held) can move it.
    pub fn server_now(
        &self,
        lease: &str,
        key: &str,
        activation_id: &str,
        save: GuardSave,
    ) -> io::Result<i64> {
        let stored = self.load(false)?;
        let now = self.effective_now(&stored);
        let verified = self.verify(lease, now);
        let held_iat = present(&stored.lease)
            .and_then(|held| self.verify(held, now).claims().map(|claims| claims.iat));
        let corrected = clock_correction(
            self.clock.now(),
            stored.last_seen,
            verified.claims().map(SignedAt::of),
            held_iat,
            key,
            activation_id,
        );
        let Some(corrected) = corrected else {
            return Ok(now);
        };
        match save {
            GuardSave::Strict => {
                self.save(&Patch::default(), Some(corrected), &Basis::default())?
            }
            GuardSave::BestEffort => {
                self.save_best_effort(&Patch::default(), Some(corrected), &Basis::default());
            }
        }
        Ok(corrected)
    }

    /// An online validate; a status on a definitive answer, else none.
    pub fn try_renew(
        &self,
        state: &StoredState,
        timeout: Option<Duration>,
    ) -> Option<LicenseStatus> {
        match self.renew(state, timeout) {
            Renewal::Answered(status) => status,
            Renewal::Unanswered => None,
        }
    }

    /// An online validate, and whether the server gave a verdict. What its
    /// answer saves and says is renewal.rs `renewal_outcome`: any verdict (a
    /// lease, a refusal, a body it could not use) records the major it was
    /// sent as heard, and an ask that came back without a lease starts the
    /// wait before the SDK asks again on its own.
    pub fn renew(&self, state: &StoredState, timeout: Option<Duration>) -> Renewal {
        // The wait runs on the device clock, as `ask_due` reads it; the answer
        // is stamped by the guarded one, which a clock put back does not move
        // back, so an answer asked for later still reads as later to another process.
        let asked_at = self.clock.now();
        let asked = Asked {
            at: self.effective_now(state),
            activation_id: state.activation_id.clone(),
            seen: state.verdict_at,
        };
        // A machine id read here keys this install from now on (`deviceKind`):
        // written with whatever this ask writes, as the seat is re-keyed by it.
        let device = self.device(self.timings.machine_id, state.device_kind);
        let response = self
            .post("validate", self.validate_body(state, &device), timeout)
            .ok();
        let (check, now) = self.judge_fresh(state, response.as_ref());
        let outcome = renewal_outcome(&RenewalInput {
            response: response.as_ref(),
            check: check.as_ref(),
            asked_at,
            now,
            major: self.major,
            key: state.key.as_deref(),
            kept_major: state.kept_major,
            device_kind: device.kind,
            own_claim: device.own_claim.as_deref(),
        });
        // Saved as the answer to its ask; a save that fails leaves the answer
        // standing (`save_best_effort`).
        let basis = Basis {
            answer: Some(asked),
            ..Basis::default()
        };
        self.save_best_effort(&outcome.patch, None, &basis);
        if outcome.answered {
            Renewal::Answered(outcome.status)
        } else {
            Renewal::Unanswered
        }
    }

    /// The lease a 200 carried, checked here as of the server's time (the
    /// clock guard reconciled to it first, `server_now`); none, and the
    /// guarded clock, when there was no such lease.
    fn judge_fresh(
        &self,
        state: &StoredState,
        response: Option<&HttpResponse>,
    ) -> (Option<LeaseCheck>, i64) {
        let lease = response
            .filter(|response| response.status == 200)
            .and_then(|response| parse_lease_body(response.json.as_ref()).0)
            .filter(|lease| !lease.is_empty());
        let Some(lease) = lease else {
            return (None, self.effective_now(state));
        };
        let (check, now) = self.judged(&lease, &Holder::of(state), self.effective_now(state));
        (Some(check), now)
    }

    /// A lease the server just answered, checked for `holder` on this device
    /// (`evaluate`) as of the server's time (`server_now`, the clock guard
    /// brought back best-effort; `fallback_now` when the state cannot be read
    /// or saved just now). With the time it was judged at.
    pub fn judged(&self, lease: &str, holder: &Holder<'_>, fallback_now: i64) -> (LeaseCheck, i64) {
        let key = holder.key.unwrap_or_default();
        let activation_id = holder.activation_id.unwrap_or_default();
        let now = self
            .server_now(lease, key, activation_id, GuardSave::BestEffort)
            .unwrap_or(fallback_now);
        (self.evaluate(lease, now, holder), now)
    }

    /// A validate's body: with every fingerprint this device answers to when
    /// it can tell, and none when it cannot.
    fn validate_body(&self, state: &StoredState, device: &Device) -> Value {
        let mut body = Map::new();
        if let Some(key) = &state.key {
            body.insert("key".into(), json!(key));
        }
        if let Some(activation_id) = &state.activation_id {
            body.insert("activationId".into(), json!(activation_id));
        }
        body.insert("major".into(), json!(self.major));
        if let Some(kept) = state.kept_major {
            body.insert("keptMajor".into(), json!(kept));
        }
        if device.can_tell {
            body.insert("fingerprints".into(), json!(device.fingerprints));
            if let Some(kind) = device.kind {
                body.insert("fingerprintKind".into(), json!(kind.as_str()));
            }
        }
        Value::Object(body)
    }
}
