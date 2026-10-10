//! Adapters that panic: an app's own storage, stash, HTTP client or
//! fingerprinter must never take a licence away, nor `status()` down with it.
//! (Caught only where panics unwind: under `panic = "abort"` a panic ends the
//! process wherever it happens.)

mod common;

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use keygrant::offline::RETRY_UNANSWERED_MS;
use keygrant::{
    ActivateError, AdapterError, DeviceIdentity, DeviceKind, Error, Fingerprinter, HttpAdapter,
    HttpResponse, License, LicenseConfig, LicenseReason, LicenseState, LicenseStatus,
    RegistryStash, StorageAdapter, StoredState,
};
use serde_json::{json, Value};

fn status(state: LicenseState, reason: Option<LicenseReason>, key: Option<&str>) -> LicenseStatus {
    let mut status = LicenseStatus::new(state);
    status.reason = reason;
    status.key = key.map(str::to_owned);
    status
}

/// A storage adapter that panics on read, on write, or on both.
struct PanickingStorage {
    state: MemoryStorage,
    read: bool,
    write: bool,
}

impl StorageAdapter for PanickingStorage {
    fn read(&self, read_only: bool) -> io::Result<Option<StoredState>> {
        if self.read {
            panic!("the storage adapter panicked on read");
        }
        self.state.read(read_only)
    }
    fn write(&self, state: &StoredState) -> io::Result<()> {
        if self.write {
            panic!("the storage adapter panicked on write");
        }
        self.state.write(state)
    }
}

/// A stash that panics on read, on write, or on both.
#[derive(Default)]
struct PanickingStash {
    read: bool,
    write: bool,
}

impl RegistryStash for PanickingStash {
    fn read_trial_start(&self) -> Option<i64> {
        if self.read {
            panic!("the stash panicked on read");
        }
        None
    }
    fn write_trial_start(&self, _ms: i64) {
        if self.write {
            panic!("the stash panicked on write");
        }
    }
}

/// An HTTP adapter that records each call, then panics.
#[derive(Default)]
struct PanickingHttp {
    calls: AtomicUsize,
}

impl HttpAdapter for PanickingHttp {
    fn post(
        &self,
        _url: &str,
        _body: &Value,
        _timeout: Duration,
    ) -> Result<HttpResponse, AdapterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("the HTTP adapter panicked");
    }
}

/// A fingerprinter that panics in `get`, and in `identify` when it has one.
struct PanickingFingerprinter {
    identifies: bool,
}

impl Fingerprinter for PanickingFingerprinter {
    fn get(&self) -> Result<String, AdapterError> {
        panic!("the fingerprinter panicked in get");
    }
    fn identify(&self, _timeout: Duration) -> Option<DeviceIdentity> {
        if self.identifies {
            panic!("the fingerprinter panicked in identify");
        }
        None
    }
}

fn config(key: &TestKey) -> LicenseConfig {
    LicenseConfig::new(PRODUCT, "https://api.test", key.public_jwk())
        .clock(FakeClock::at(NOW))
        .fingerprinter(Arc::new(FixedFingerprinter("fp-test")))
        .timings(patient_timings())
}

fn over_storage(storage: PanickingStorage, http: Arc<FakeHttp>) -> License {
    License::new(
        config(&TestKey::new(7))
            .storage(Arc::new(storage))
            .http(http),
    )
    .expect("a valid configuration")
}

// --- storage and stash (status paths answer, activate and deactivate fail) ---------------

#[test]
fn a_storage_adapter_that_panics_on_read_is_a_state_that_cannot_be_read() {
    let http = Arc::new(FakeHttp::default());
    let storage = PanickingStorage {
        state: MemoryStorage::default(),
        read: true,
        write: false,
    };
    let license = over_storage(storage, Arc::clone(&http));
    let unknown = status(LicenseState::Unknown, Some(LicenseReason::Storage), None);
    assert_eq!(license.status(false), unknown);
    assert_eq!(license.refresh(), unknown);
    assert_eq!(license.start_trial(), unknown);
    assert!(matches!(
        license.activate(KEY),
        Err(ActivateError::Storage(_))
    ));
    assert!(matches!(license.deactivate(), Err(Error::Storage(_))));
    assert!(matches!(
        license.upgrade_url("https://buy.test/x"),
        Err(Error::Storage(_))
    ));
    assert!(http.calls().is_empty());
}

#[test]
fn a_storage_adapter_that_panics_on_write_leaves_the_servers_answer_standing() {
    let key = TestKey::new(7);
    let http = Arc::new(FakeHttp::default());
    let state = MemoryStorage::default();
    state.set(Some(licensed(
        KEY,
        &key.lease(KEY, "perpetual", 30 * 86_400, NOW),
    )));
    let storage = PanickingStorage {
        state,
        read: false,
        write: true,
    };
    let license = over_storage(storage, Arc::clone(&http));
    // A refusal the disk cannot keep is still the answer.
    http.answer("validate", 403, json!({ "error": "revoked" }));
    let revoked = status(
        LicenseState::Invalid,
        Some(LicenseReason::Revoked),
        Some(KEY),
    );
    assert_eq!(license.refresh(), revoked);
    // A trial the disk cannot keep is still the answer.
    let trial = key.lease("trial:fp-test", "trial", 5 * 86_400, NOW);
    let body = json!({ "trialStart": NOW - 2 * DAY, "trialEndsAt": NOW + 5 * DAY, "lease": trial });
    http.answer("trial", 200, body);
    let started = license.start_trial();
    assert_eq!(
        (started.state, started.trial_days_left),
        (LicenseState::Trial, Some(5))
    );
    // What the customer asked for fails, to be tried again.
    http.answer(
        "activate",
        200,
        json!({ "lease": key.lease(KEY, "perpetual", 30 * 86_400, NOW), "activationId": "act_1" }),
    );
    assert!(matches!(
        license.activate(KEY),
        Err(ActivateError::Storage(_))
    ));
    assert!(matches!(license.deactivate(), Err(Error::Storage(_))));
}

fn trial_harness(stash: PanickingStash) -> Harness {
    let h = Harness::empty();
    let trial = h.lease("trial:fp-test", "trial", 5 * 86_400, NOW);
    let body = json!({ "trialStart": NOW - 2 * DAY, "trialEndsAt": NOW + 5 * DAY, "lease": trial });
    h.http.answer("trial", 200, body);
    let config = h.config.clone().registry(Arc::new(stash));
    Harness {
        license: License::new(config).expect("a valid configuration"),
        ..h
    }
}

#[test]
fn a_stash_that_panics_on_read_is_no_evidence_and_the_trial_still_starts() {
    let h = trial_harness(PanickingStash {
        read: true,
        write: false,
    });
    let started = h.license.start_trial();
    assert_eq!(
        (started.state, started.trial_days_left),
        (LicenseState::Trial, Some(5))
    );
    assert_eq!(h.state().trial_start, Some(NOW - 2 * DAY));
    // No evidence from it, so no client start is sent.
    assert!(h.http.body_of("trial").get("clientStart").is_none());
}

#[test]
fn a_stash_that_panics_on_write_loses_no_trial() {
    let h = trial_harness(PanickingStash {
        read: false,
        write: true,
    });
    let started = h.license.start_trial();
    assert_eq!(
        (started.state, started.trial_days_left),
        (LicenseState::Trial, Some(5))
    );
    assert_has(
        &h.state(),
        json!({ "trialStart": NOW - 2 * DAY, "trialEndsAt": NOW + 5 * DAY, "trialAt": NOW }),
    );
}

// --- the HTTP adapter and the fingerprinter (no answer, and cannot tell) ------------------

fn harness_with(fingerprint: Option<Arc<dyn Fingerprinter>>, http: Arc<PanickingHttp>) -> Harness {
    let h = Harness::new(
        None,
        Opts {
            fingerprint,
            timings: Some(fast_timings()),
            ..Opts::default()
        },
    );
    let config = h.config.clone().http(http);
    Harness {
        license: License::new(config).expect("a valid configuration"),
        ..h
    }
}

/// A lease held to another machine (`fp` of a stranger, keyed by a machine id).
fn elsewhere(h: &Harness, ttl: i64, at: i64) -> String {
    let claim = keygrant::lease::fingerprint_claim("fp-another-machine");
    h.key.lease_with(
        at,
        ttl,
        json!({ "sub": KEY, "fp": claim, "fpk": "machine" }),
    )
}

#[test]
fn an_http_adapter_that_panics_is_no_answer() {
    let http = Arc::new(PanickingHttp::default());
    let h = harness_with(None, Arc::clone(&http));
    h.set_state(licensed(
        KEY,
        &h.lease(KEY, "perpetual", 3600, NOW - 2 * DAY),
    ));
    let expired = status(
        LicenseState::Expired,
        Some(LicenseReason::Offline),
        Some(KEY),
    );
    assert_eq!(h.license.status(false), expired);
    assert_eq!(http.calls.load(Ordering::SeqCst), 1);
    // No answer at all: the short wait, not the hour.
    assert_has(
        &h.state(),
        json!({ "backoffSince": NOW, "backoffMs": RETRY_UNANSWERED_MS }),
    );
    // A valid lease stays licensed through it.
    h.set_state(licensed(KEY, &h.lease(KEY, "perpetual", 30 * 86_400, NOW)));
    assert_eq!(
        h.license.refresh(),
        status(LicenseState::Licensed, None, Some(KEY))
    );
}

#[test]
fn a_fingerprinter_that_panics_cannot_tell_and_the_device_is_not_judged() {
    for identifies in [false, true] {
        let http = Arc::new(PanickingHttp::default());
        let fingerprint: Arc<dyn Fingerprinter> = Arc::new(PanickingFingerprinter { identifies });
        let h = harness_with(Some(fingerprint), Arc::clone(&http));
        // A valid lease of another machine: not judged on a run that cannot tell.
        h.set_state(licensed(KEY, &elsewhere(&h, 30 * 86_400, NOW)));
        assert_eq!(
            h.license.status(false),
            status(LicenseState::Licensed, None, Some(KEY))
        );
        // An expired one is expired, not another device's, and waits the minute.
        h.set_state(licensed(KEY, &elsewhere(&h, 3600, NOW - 2 * DAY)));
        let expired = status(
            LicenseState::Expired,
            Some(LicenseReason::Offline),
            Some(KEY),
        );
        assert_eq!(h.license.status(false), expired, "identify: {identifies}");
        assert_has(&h.state(), json!({ "backoffMs": RETRY_UNANSWERED_MS }));
        // And nothing can be activated under a device that cannot be told.
        let refused = h
            .license
            .activate(KEY)
            .err()
            .and_then(|e| e.license_error());
        assert_eq!(refused, Some(keygrant::LicenseError::Network));
    }
}

/// A fingerprinter whose first read panics (in `get`, or in `identify` when
/// it has one); every read after it answers.
struct PanicsOnce {
    identifies: bool,
    reads: AtomicUsize,
}

impl PanicsOnce {
    fn read(&self, what: &str) {
        if self.reads.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("the fingerprinter panicked in {what}");
        }
    }
}

impl Fingerprinter for PanicsOnce {
    fn get(&self) -> Result<String, AdapterError> {
        self.read("get");
        Ok("fp-test".into())
    }
    fn identify(&self, _timeout: Duration) -> Option<DeviceIdentity> {
        if !self.identifies {
            return None;
        }
        self.read("identify");
        Some(DeviceIdentity {
            fingerprint: Some("fp-test".into()),
            kind: Some(DeviceKind::Machine),
            alternates: Vec::new(),
        })
    }
}

#[test]
fn a_fingerprinter_that_panicked_is_read_again_and_its_panic_ends_the_read_at_once() {
    for identifies in [false, true] {
        let fingerprinter = Arc::new(PanicsOnce {
            identifies,
            reads: AtomicUsize::new(0),
        });
        let fingerprint: Arc<dyn Fingerprinter> = fingerprinter.clone();
        let h = Harness::new(
            None,
            Opts {
                fingerprint: Some(fingerprint),
                // Long bounds, so that a read the panic left unsettled shows.
                timings: Some(patient_timings()),
                ..Opts::default()
            },
        );
        h.http.fail(true);
        h.set_state(licensed(KEY, &elsewhere(&h, 30 * 86_400, NOW)));
        // The panic ends the read there and then: this run cannot tell, and
        // another machine's lease is not judged.
        let started = Instant::now();
        assert_eq!(
            h.license.status(false),
            status(LicenseState::Licensed, None, Some(KEY))
        );
        assert!(
            started.elapsed() < PATIENT / 3,
            "identify: {identifies}: {:?}",
            started.elapsed()
        );
        // Not kept: the next call reads again, can tell, and judges.
        assert_eq!(
            h.license.status(false),
            status(
                LicenseState::Invalid,
                Some(LicenseReason::Device),
                Some(KEY)
            )
        );
        assert_eq!(
            fingerprinter.reads.load(Ordering::SeqCst),
            2,
            "identify: {identifies}"
        );
    }
}
