//! A lease held to its key, its activation and its device; and who the
//! device is when it cannot be told.

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use keygrant::lease::{fingerprint_claim, OPEN_MAJOR};
use keygrant::machine_id::{MachineIdSource, ReadFailure};
use keygrant::{
    hash_fingerprint, hostname_fingerprint, AdapterError, DeactivateResult, DefaultFingerprinter,
    DeviceIdentity, DeviceKind, Fingerprinter, LicenseError, LicenseReason, LicenseState,
    LicenseStatus, Refusal, StoredState, Timings,
};
use serde_json::{json, Value};

const GUID: &str = "3f2504e0-4f89-11d3-9a0c-0305e82c3301";

fn machine() -> String {
    hash_fingerprint(&format!("machine-id|{GUID}"))
}

fn licensed_as(key: &str) -> LicenseStatus {
    let mut status = LicenseStatus::new(LicenseState::Licensed);
    status.key = Some(key.to_owned());
    status
}

fn refused(reason: LicenseReason, key: Option<&str>) -> LicenseStatus {
    let mut status = LicenseStatus::new(LicenseState::Invalid);
    status.reason = Some(reason);
    status.key = key.map(str::to_owned);
    status
}

/// A Windows machine whose registry read does not finish the first `hangs`
/// times, then answers; or one with no id to read (`blocked`).
struct WinSource {
    hangs: AtomicUsize,
    blocked: bool,
}

impl WinSource {
    fn slow(hangs: usize) -> Arc<dyn MachineIdSource> {
        Arc::new(WinSource {
            hangs: AtomicUsize::new(hangs),
            blocked: false,
        })
    }
    fn blocked() -> Arc<dyn MachineIdSource> {
        Arc::new(WinSource {
            hangs: AtomicUsize::new(0),
            blocked: true,
        })
    }
}

impl MachineIdSource for WinSource {
    fn platform(&self) -> String {
        "win32".into()
    }
    fn windows_machine_guid(&self) -> Result<String, ReadFailure> {
        if self.blocked {
            return Err(ReadFailure::Unavailable);
        }
        let hangs = self.hangs.load(Ordering::SeqCst);
        if hangs > 0 {
            self.hangs.store(hangs - 1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_secs(60));
        }
        Ok(GUID.into())
    }
    fn run(
        &self,
        _command: &str,
        _args: &[&str],
        _timeout: Duration,
    ) -> Result<String, ReadFailure> {
        Err(ReadFailure::Unavailable)
    }
    fn read_file(&self, _path: &str) -> Result<String, ReadFailure> {
        Err(ReadFailure::Unavailable)
    }
}

fn over(source: Arc<dyn MachineIdSource>) -> Option<Arc<dyn Fingerprinter>> {
    Some(Arc::new(DefaultFingerprinter::with_source(source)))
}

/// A harness over the default fingerprinter reading `source`, whose reads
/// that answer always land (`patient_timings`).
fn harness_over(source: Arc<dyn MachineIdSource>) -> Harness {
    harness_over_with(source, patient_timings())
}

fn harness_over_with(source: Arc<dyn MachineIdSource>, timings: Timings) -> Harness {
    Harness::new(
        None,
        Opts {
            fingerprint: over(source),
            timings: Some(timings),
            ..Opts::default()
        },
    )
}

/// A licence lease for this key and activation, its seat keyed (`kind`) by the fingerprint `device`.
fn lease_for(h: &Harness, device: &str, kind: &str) -> String {
    h.key.lease_with(
        NOW,
        30 * 86_400,
        json!({ "sub": KEY, "fp": fingerprint_claim(device), "fpk": kind }),
    )
}

fn answer_lease(h: &Harness, action: &str, lease: &str) {
    h.http.answer(
        action,
        200,
        json!({ "lease": lease, "activationId": "act_1" }),
    );
}

fn failing() -> Arc<dyn Fingerprinter> {
    Arc::new(FnFingerprinter(|| -> Result<String, AdapterError> {
        Err("fingerprint adapter failed".into())
    }))
}

// --- a lease held to its key, its activation and its device ----------------------------------

#[test]
fn licenses_this_device_on_its_own_lease_or_one_with_no_device_claim_offline() {
    let h = Harness::empty();
    h.set_state(licensed(KEY, &lease_for(&h, "fp-test", "machine")));
    h.http.fail(true);
    assert_eq!(h.license.status(false), licensed_as(KEY));
    assert_eq!(h.validates(), 0);
    let h = Harness::empty();
    h.set_state(licensed(KEY, &h.lease(KEY, "perpetual", 30 * 86_400, NOW)));
    h.http.fail(true);
    assert_eq!(h.license.status(false), licensed_as(KEY));
    // The key matched without regard to case, as the server matches it.
    let h = Harness::empty();
    let lease = h.key.lease_with(
        NOW,
        30 * 86_400,
        json!({ "sub": "Sluice-aaaa-BBBB-CCCC-DDDD" }),
    );
    h.set_state(licensed(KEY, &lease));
    h.http.fail(true);
    assert_eq!(h.license.status(false), licensed_as(KEY));
}

#[test]
fn refuses_offline_a_lease_for_another_key_activation_or_device_and_asks_under_the_waits() {
    let cases: [(Value, LicenseStatus); 3] = [
        (
            json!({ "sub": "SLUICE-OTHR-OTHR-OTHR-OTHR" }),
            refused(LicenseReason::Tampered, None),
        ),
        (
            json!({ "aid": "act_2" }),
            refused(LicenseReason::Tampered, None),
        ),
        (
            json!({ "fp": fingerprint_claim("fp-another-machine"), "fpk": "machine" }),
            refused(LicenseReason::Device, Some(KEY)),
        ),
    ];
    for (over, expected) in cases {
        let h = Harness::empty();
        let mut claims = json!({ "sub": KEY });
        for (name, value) in over.as_object().expect("an object") {
            claims[name] = value.clone();
        }
        h.set_state(licensed(KEY, &h.key.lease_with(NOW, 30 * 86_400, claims)));
        h.http.fail(true);
        assert_eq!(h.license.status(false), expected);
        assert_eq!(h.validates(), 1);
        assert_eq!(h.license.status(false), expected);
        assert_eq!(h.validates(), 1);
    }
}

#[test]
fn stays_refused_for_another_device_online_the_server_renewing_the_others_lease() {
    let h = Harness::empty();
    let theirs = lease_for(&h, "fp-another-machine", "machine");
    h.set_state(licensed(KEY, &theirs));
    answer_lease(&h, "validate", &theirs);
    assert_eq!(
        h.license.status(false),
        refused(LicenseReason::Device, Some(KEY))
    );
    assert_eq!(
        h.license.refresh(),
        refused(LicenseReason::Device, Some(KEY))
    );
    // A lease with no device claim, renewed with another device's: device,
    // offline too.
    let h = Harness::empty();
    h.set_state(licensed(KEY, &h.lease(KEY, "perpetual", 30 * 86_400, NOW)));
    answer_lease(
        &h,
        "validate",
        &lease_for(&h, "fp-another-machine", "machine"),
    );
    assert_eq!(
        h.license.refresh(),
        refused(LicenseReason::Device, Some(KEY))
    );
    h.http.fail(true);
    assert_eq!(
        h.license.status(false),
        refused(LicenseReason::Device, Some(KEY))
    );
}

#[test]
fn activation_takes_this_devices_lease_and_refuses_another_devices() {
    let h = Harness::empty();
    answer_lease(&h, "activate", &lease_for(&h, "fp-test", "machine"));
    h.license.activate(KEY).expect("activated");
    h.http.fail(true);
    assert_eq!(h.license.status(false), licensed_as(KEY));
    let other = Harness::empty();
    answer_lease(
        &other,
        "activate",
        &lease_for(&other, "fp-another-machine", "machine"),
    );
    assert_eq!(
        other
            .license
            .activate(KEY)
            .err()
            .and_then(|e| e.license_error()),
        Some(LicenseError::InvalidLease)
    );
}

// --- who this device is, when it cannot be told -------------------------------------------------

#[test]
fn a_slow_first_read_is_not_another_device_and_the_next_reads_the_id() {
    let source = WinSource::slow(1);
    let first = harness_over_with(Arc::clone(&source), fast_timings());
    first.set_state(licensed(KEY, &lease_for(&first, &machine(), "machine")));
    first.http.fail(true);
    // The read outlives its bound: this run cannot tell, and does not judge.
    assert_eq!(first.license.status(false), licensed_as(KEY));
    let second = harness_over(source);
    second.set_state(licensed(KEY, &lease_for(&second, &machine(), "machine")));
    second.http.fail(true);
    assert_eq!(second.license.status(false), licensed_as(KEY));
}

#[test]
fn activates_under_the_host_name_after_the_full_activation_bound_and_moves_to_the_id_once_it_reads()
{
    // The activation's bound is waited out (600 ms here); the reads that
    // answer, the activation's own giving up and the renewal's, always land.
    let timings = Timings {
        activate_machine_id: Duration::from_millis(600),
        ..patient_timings()
    };
    let h = harness_over_with(WinSource::slow(1), timings);
    answer_lease(
        &h,
        "activate",
        &lease_for(&h, &hostname_fingerprint(), "hostname"),
    );
    let started = Instant::now();
    h.license.activate(KEY).expect("activated");
    // The activation's bound, not a status's.
    assert!(
        started.elapsed() >= Duration::from_millis(590),
        "{:?}",
        started.elapsed()
    );
    let body = h.http.body_of("activate");
    assert_eq!(
        (body["fingerprint"].clone(), body["fingerprintKind"].clone()),
        (json!(hostname_fingerprint()), json!("hostname"))
    );
    assert_eq!(h.state().device_kind, Some(DeviceKind::Hostname));
    // The id reads now: the renewal sends it first, as a machine id, and the install is keyed by it.
    answer_lease(&h, "validate", &lease_for(&h, &machine(), "machine"));
    assert_eq!(h.license.refresh(), licensed_as(KEY));
    let body = h.http.body_of("validate");
    assert_eq!(
        body["fingerprints"],
        json!([machine(), hostname_fingerprint()])
    );
    assert_eq!(body["fingerprintKind"], json!("machine"));
    assert_eq!(h.state().device_kind, Some(DeviceKind::Machine));
}

#[test]
fn never_reactivates_a_machine_keyed_seat_under_the_host_name_whatever_case_its_key_was_stored_in()
{
    for (source, stored_key) in [
        (WinSource::slow(1), KEY.to_owned()),
        (WinSource::blocked(), KEY.to_lowercase()),
    ] {
        let h = harness_over_with(source, fast_timings());
        h.set_state(StoredState {
            key: Some(stored_key),
            activation_id: Some("act_1".into()),
            device_kind: Some(DeviceKind::Machine),
            last_seen: Some(NOW),
            ..StoredState::default()
        });
        assert_eq!(
            h.license
                .activate(KEY)
                .err()
                .and_then(|e| e.license_error()),
            Some(LicenseError::Network)
        );
        assert!(!h.http.called("activate"));
    }
}

#[test]
fn takes_a_lease_from_an_activation_under_the_host_name_and_renews_it_telling_both() {
    let h = harness_over(WinSource::slow(0));
    let legacy = lease_for(&h, &hostname_fingerprint(), "hostname");
    h.set_state(licensed(KEY, &legacy));
    h.http.fail(true);
    assert_eq!(h.license.status(false), licensed_as(KEY));
    h.http.fail(false);
    answer_lease(&h, "validate", &legacy);
    assert_eq!(h.license.refresh(), licensed_as(KEY));
    assert_eq!(
        h.http.body_of("validate")["fingerprints"],
        json!([machine(), hostname_fingerprint()])
    );
}

#[test]
fn does_not_judge_the_device_nor_tell_the_server_who_it_is_on_a_run_that_cannot_tell() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let fingerprint: Arc<dyn Fingerprinter> =
        Arc::new(FnFingerprinter(move || -> Result<String, AdapterError> {
            counted.fetch_add(1, Ordering::SeqCst);
            Err("fingerprint adapter failed".into())
        }));
    let h = Harness::new(
        None,
        Opts {
            fingerprint: Some(fingerprint),
            ..Opts::default()
        },
    );
    let elsewhere = lease_for(&h, "fp-another-machine", "machine");
    h.set_state(licensed(KEY, &elsewhere));
    assert_eq!(h.license.status(false), licensed_as(KEY));
    answer_lease(&h, "validate", &elsewhere);
    assert_eq!(h.license.refresh(), licensed_as(KEY));
    assert!(h.http.body_of("validate").get("fingerprints").is_none());
    // Not kept: every call asks the adapter again.
    assert!(calls.load(Ordering::SeqCst) > 1);
}

#[test]
fn reads_again_after_a_failed_read_and_judges_the_device_once_it_can_tell() {
    let failing = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&failing);
    let fingerprint: Arc<dyn Fingerprinter> =
        Arc::new(FnFingerprinter(move || -> Result<String, AdapterError> {
            if flag.load(Ordering::SeqCst) {
                Err("not yet".into())
            } else {
                Ok("fp-test".into())
            }
        }));
    let h = Harness::new(
        None,
        Opts {
            fingerprint: Some(fingerprint),
            ..Opts::default()
        },
    );
    h.set_state(licensed(
        KEY,
        &lease_for(&h, "fp-another-machine", "machine"),
    ));
    h.http.fail(true);
    assert_eq!(h.license.status(false).state, LicenseState::Licensed);
    failing.store(false, Ordering::SeqCst);
    assert_eq!(
        h.license.status(false),
        refused(LicenseReason::Device, Some(KEY))
    );
}

#[test]
fn bounds_a_fingerprinter_that_never_answers_and_does_not_keep_that() {
    // The first read fails only once the test lets it, long past a status's
    // bound (3 s here); every read after it answers at once.
    let gate = Gate::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let first_ended = Arc::new(AtomicBool::new(false));
    let (counted, ended, held) = (Arc::clone(&calls), Arc::clone(&first_ended), gate.clone());
    let fingerprint: Arc<dyn Fingerprinter> =
        Arc::new(FnFingerprinter(move || -> Result<String, AdapterError> {
            if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                held.wait();
                ended.store(true, Ordering::SeqCst);
                return Err("the first read failed".into());
            }
            Ok("fp-test".into())
        }));
    let bound = Duration::from_secs(3);
    let h = Harness::new(
        None,
        Opts {
            fingerprint: Some(fingerprint),
            timings: Some(Timings {
                machine_id: bound,
                ..patient_timings()
            }),
            ..Opts::default()
        },
    );
    h.set_state(licensed(
        KEY,
        &lease_for(&h, "fp-another-machine", "machine"),
    ));
    h.http.fail(true);
    // Bounded: the device cannot be told, so it is not judged.
    let started = Instant::now();
    assert_eq!(h.license.status(false), licensed_as(KEY));
    let took = started.elapsed();
    assert!(took >= bound && took < bound * 4, "{took:?}");
    // That read still runs, past its bound: the next call does not wait on
    // it but reads again, and judges.
    assert!(!first_ended.load(Ordering::SeqCst));
    assert_eq!(
        h.license.status(false),
        refused(LicenseReason::Device, Some(KEY))
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    // The first read failing at last changes nothing: what was read is kept.
    gate.release();
    wait_until(|| first_ended.load(Ordering::SeqCst));
    assert_eq!(
        h.license.status(false),
        refused(LicenseReason::Device, Some(KEY))
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

/// A machine whose id is read only by a read given at least 500 ms; a read
/// given less gives up, and late: 1.4 s on, past a status's bound. It notes
/// the timeout each read was given.
struct SlowId {
    timeouts: Arc<Mutex<Vec<Duration>>>,
}

impl Fingerprinter for SlowId {
    fn get(&self) -> Result<String, AdapterError> {
        Err("identify answers".into())
    }
    fn identify(&self, timeout: Duration) -> Option<DeviceIdentity> {
        self.timeouts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(timeout);
        let read = timeout >= Duration::from_millis(500);
        if !read {
            std::thread::sleep(Duration::from_millis(1_400));
        }
        Some(DeviceIdentity {
            fingerprint: read.then(machine),
            kind: read.then_some(DeviceKind::Machine),
            alternates: vec![hostname_fingerprint()],
        })
    }
}

#[test]
fn an_activation_and_a_trial_start_read_for_themselves_not_through_a_status_read_given_less_time() {
    // A status's bound (200 ms) is waited out; an activation's is patient,
    // and its read, which answers at once, always lands.
    let timings = Timings {
        machine_id: Duration::from_millis(200),
        identify_grace: Duration::from_secs(1),
        ..patient_timings()
    };
    let timeouts = Arc::new(Mutex::new(Vec::new()));
    let harness = || {
        Harness::new(
            None,
            Opts {
                fingerprint: Some(Arc::new(SlowId {
                    timeouts: Arc::clone(&timeouts),
                })),
                timings: Some(timings),
                ..Opts::default()
            },
        )
    };
    let taken = || {
        std::mem::take(
            &mut *timeouts
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    };
    let (status_bound, activation_bound) = (timings.machine_id, timings.activate_machine_id);

    // A status's read gives up after the status has: it cannot tell, and the
    // read runs on. The activation after it reads the id in its own bound and
    // re-activates the machine's seat under it (a read given 200 ms would have
    // left a machine-keyed install no way to activate: `network`).
    let h = harness();
    let mut stored = licensed(KEY, &lease_for(&h, &machine(), "machine"));
    stored.device_kind = Some(DeviceKind::Machine);
    h.set_state(stored);
    assert_eq!(h.license.status(false), licensed_as(KEY));
    answer_lease(&h, "activate", &lease_for(&h, &machine(), "machine"));
    h.license.activate(KEY).expect("activated");
    assert_eq!(h.http.body_of("activate")["fingerprint"], json!(machine()));
    assert_eq!(taken(), [status_bound, activation_bound]);

    // So does a trial start: it is started under the id, not refused for a
    // device that cannot be told.
    let h = harness();
    h.set_state(trial_state(NOW, trial_lease(&h, &machine(), "machine")));
    assert_eq!(h.license.status(false).state, LicenseState::Trial);
    h.http.answer("trial", 503, json!({}));
    h.license.start_trial();
    let body = h.http.body_of("trial");
    assert_eq!(
        (body["fingerprint"].clone(), body["fingerprintKind"].clone()),
        (json!(machine()), json!("machine"))
    );
    assert_eq!(taken(), [status_bound, activation_bound]);
}

#[test]
fn takes_the_servers_device_refusal_for_this_device_only_and_licenses_again_when_it_flips_back() {
    let h = Harness::empty();
    h.set_state(licensed(KEY, &lease_for(&h, "fp-test", "machine")));
    h.http.answer("validate", 403, json!({ "error": "device" }));
    assert_eq!(
        h.license.refresh(),
        refused(LicenseReason::Device, Some(KEY))
    );
    let body = h.http.body_of("validate");
    assert_eq!(body["fingerprints"], json!(["fp-test"]));
    assert!(body.get("fingerprintKind").is_none());
    assert_has(
        &h.state(),
        json!({ "refused": "device", "refusedFor": [fingerprint_claim("fp-test")] }),
    );
    assert!(h.state().lease.is_some());
    h.http.fail(true);
    assert_eq!(
        h.license.status(false),
        refused(LicenseReason::Device, Some(KEY))
    );

    // The refusal was given to a device reading `fp-flipped`; the next run reads it as it was.
    let h = Harness::new(
        None,
        Opts {
            fingerprint: Some(Arc::new(FixedFingerprinter("fp-flipped"))),
            ..Opts::default()
        },
    );
    h.set_state(licensed(KEY, &lease_for(&h, "fp-test", "machine")));
    h.http.answer("validate", 403, json!({ "error": "device" }));
    assert_eq!(h.license.refresh().reason, Some(LicenseReason::Device));
    let next = h.license_with(Arc::new(FixedFingerprinter("fp-test")));
    h.http.fail(true);
    assert_eq!(next.status(false), licensed_as(KEY));
}

#[test]
fn does_not_stand_a_device_refusal_on_a_later_run_that_only_shares_its_host_name_alternate() {
    let identify = |own: &'static str| -> Arc<dyn Fingerprinter> {
        Arc::new(IdentifyingFingerprinter(move || DeviceIdentity {
            fingerprint: Some(own.into()),
            kind: Some(DeviceKind::Machine),
            alternates: vec!["fp-same-host".into()],
        }))
    };
    let h = Harness::new(
        None,
        Opts {
            fingerprint: Some(identify("fp-boot-1")),
            ..Opts::default()
        },
    );
    h.set_state(licensed(KEY, &h.lease(KEY, "perpetual", 30 * 86_400, NOW)));
    h.http.answer("validate", 403, json!({ "error": "device" }));
    assert_eq!(h.license.refresh().reason, Some(LicenseReason::Device));
    assert_eq!(
        h.state().refused_for,
        Some(vec![fingerprint_claim("fp-boot-1")])
    );
    let next = h.license_with(identify("fp-boot-2"));
    h.http.fail(true);
    assert_eq!(next.status(false), licensed_as(KEY));
}

#[test]
fn does_not_refuse_a_machine_keyed_install_whose_id_stops_reading() {
    let h = harness_over(WinSource::blocked());
    let lease = lease_for(&h, &machine(), "machine");
    let mut stored = licensed(KEY, &lease);
    stored.device_kind = Some(DeviceKind::Machine);
    h.set_state(stored);
    h.http.fail(true);
    assert_eq!(h.license.status(false), licensed_as(KEY));
    h.http.fail(false);
    answer_lease(&h, "validate", &lease);
    assert_eq!(h.license.refresh(), licensed_as(KEY));
    assert!(h.http.body_of("validate").get("fingerprints").is_none());
    assert_eq!(h.state().lease.as_deref(), Some(lease.as_str()));
    assert_eq!(h.state().device_kind, Some(DeviceKind::Machine));
}

#[test]
fn does_not_refuse_a_host_name_keyed_seat_or_one_of_unknown_kind() {
    for source in [WinSource::slow(0), WinSource::blocked()] {
        let h = harness_over(source);
        h.set_state(licensed(
            KEY,
            &lease_for(&h, &hash_fingerprint("old-name|darwin|x64"), "hostname"),
        ));
        h.http.fail(true);
        assert_eq!(h.license.status(false), licensed_as(KEY));
    }
    let h = harness_over(WinSource::slow(0));
    let claim = fingerprint_claim(&hash_fingerprint("old-name|darwin|x64"));
    h.set_state(licensed(
        KEY,
        &h.key
            .lease_with(NOW, 30 * 86_400, json!({ "sub": KEY, "fp": claim })),
    ));
    h.http.fail(true);
    assert_eq!(h.license.status(false), licensed_as(KEY));
}

#[test]
fn device_hold_false_holds_nothing_to_its_device() {
    let h = Harness::new(
        None,
        Opts {
            fingerprint: over(WinSource::slow(0)),
            device_hold: Some(false),
            timings: Some(patient_timings()),
            ..Opts::default()
        },
    );
    answer_lease(&h, "activate", &lease_for(&h, &machine(), "machine"));
    h.license.activate(KEY).expect("activated");
    let body = h.http.body_of("activate");
    assert_eq!(body["fingerprint"], json!(machine()));
    assert!(body.get("fingerprintKind").is_none());
    assert_eq!(h.state().device_kind, None);
    // A machine-keyed lease for another machine: not judged.
    let elsewhere = lease_for(&h, "fp-another-machine", "machine");
    h.set_state(licensed(KEY, &elsewhere));
    h.http.fail(true);
    assert_eq!(h.license.status(false), licensed_as(KEY));
    // And the server is told no fingerprints to check.
    h.http.fail(false);
    answer_lease(&h, "validate", &elsewhere);
    assert_eq!(h.license.refresh(), licensed_as(KEY));
    let body = h.http.body_of("validate");
    assert!(body.get("fingerprints").is_none() && body.get("fingerprintKind").is_none());
}

#[test]
fn device_hold_false_activates_under_the_host_name_on_an_install_a_machine_id_once_keyed() {
    let h = Harness::new(
        None,
        Opts {
            fingerprint: over(WinSource::blocked()),
            device_hold: Some(false),
            timings: Some(patient_timings()),
            ..Opts::default()
        },
    );
    h.set_state(StoredState {
        key: Some(KEY.into()),
        activation_id: Some("act_1".into()),
        device_kind: Some(DeviceKind::Machine),
        last_seen: Some(NOW),
        ..StoredState::default()
    });
    answer_lease(&h, "activate", &lease_for(&h, &machine(), "machine"));
    h.license.activate(KEY).expect("activated");
    let body = h.http.body_of("activate");
    assert_eq!(body["fingerprint"], json!(hostname_fingerprint()));
    assert!(body.get("fingerprintKind").is_none());
    assert_eq!(h.state().device_kind, None);
    h.http.fail(true);
    assert_eq!(h.license.status(false), licensed_as(KEY));
}

#[test]
fn clears_a_stored_machine_kind_when_a_fingerprint_of_unknown_kind_activates_the_seat() {
    let h = Harness::with(StoredState {
        key: Some(KEY.into()),
        activation_id: Some("act_1".into()),
        device_kind: Some(DeviceKind::Machine),
        last_seen: Some(NOW),
        ..StoredState::default()
    });
    answer_lease(&h, "activate", &h.lease(KEY, "perpetual", 30 * 86_400, NOW));
    h.license.activate(KEY).expect("activated");
    assert_eq!(h.state().device_kind, None);
}

#[test]
fn activates_under_the_host_name_once_a_deactivate_frees_the_install_or_for_another_key() {
    let h = harness_over(WinSource::blocked());
    let mut stored = licensed(KEY, &lease_for(&h, &machine(), "machine"));
    stored.device_kind = Some(DeviceKind::Machine);
    h.set_state(stored);
    h.http.answer("deactivate", 200, json!({ "ok": true }));
    h.license.deactivate().expect("deactivated");
    assert_eq!(h.state().device_kind, None);
    answer_lease(
        &h,
        "activate",
        &lease_for(&h, &hostname_fingerprint(), "hostname"),
    );
    h.license.activate(KEY).expect("activated");
    let body = h.http.body_of("activate");
    assert_eq!(
        (body["fingerprint"].clone(), body["fingerprintKind"].clone()),
        (json!(hostname_fingerprint()), json!("hostname"))
    );
    assert_eq!(h.state().device_kind, Some(DeviceKind::Hostname));

    let h = harness_over(WinSource::blocked());
    h.set_state(StoredState {
        key: Some("SLUICE-OLDK-OLDK-OLDK-OLDK".into()),
        activation_id: Some("act_1".into()),
        device_kind: Some(DeviceKind::Machine),
        last_seen: Some(NOW),
        ..StoredState::default()
    });
    answer_lease(
        &h,
        "activate",
        &lease_for(&h, &hostname_fingerprint(), "hostname"),
    );
    h.license.activate(KEY).expect("activated");
    assert_eq!(
        h.http.body_of("activate")["fingerprintKind"],
        json!("hostname")
    );
}

// --- deactivate, when the activation stored is another device's -------------------------------

// A state file copied or restored from another PC carries that PC's
// activation: releasing it would lock that PC out at its next check-in.

const RELEASED: DeactivateResult = DeactivateResult { released: true };

fn deactivate_answers(h: &Harness) {
    h.http.answer("deactivate", 200, json!({ "ok": true }));
}

#[test]
fn deactivate_drops_another_devices_activation_here_without_releasing_its_seat() {
    let h = Harness::empty();
    h.set_state(licensed(KEY, &lease_for(&h, "fp-other-pc", "machine")));
    deactivate_answers(&h);
    assert_eq!(h.license.deactivate().expect("deactivated"), RELEASED);
    assert!(!h.http.called("deactivate"));
    let left = h.state();
    assert!(left.key.is_none() && left.activation_id.is_none() && left.lease.is_none());
    assert_eq!(left.verdict_at, Some(NOW));
}

#[test]
fn deactivate_reads_whose_seat_it_is_from_a_lease_that_has_run_out_too() {
    let h = Harness::empty();
    let expired = h.key.lease_with(
        NOW - 40 * DAY,
        30 * 86_400,
        json!({ "sub": KEY, "fp": fingerprint_claim("fp-other-pc"), "fpk": "machine" }),
    );
    h.set_state(licensed(KEY, &expired));
    deactivate_answers(&h);
    assert_eq!(h.license.deactivate().expect("deactivated"), RELEASED);
    assert!(!h.http.called("deactivate"));
    assert!(h.state().key.is_none());
}

#[test]
fn deactivate_takes_a_device_refusal_that_stands_for_this_device_as_the_same() {
    let h = Harness::empty();
    let mut stored = licensed(KEY, &lease_for(&h, "fp-test", "machine"));
    stored.refused = Some(Refusal::Device);
    stored.refused_for = Some(vec![fingerprint_claim("fp-test")]);
    h.set_state(stored);
    deactivate_answers(&h);
    assert_eq!(h.license.deactivate().expect("deactivated"), RELEASED);
    assert!(!h.http.called("deactivate"));
    assert!(h.state().key.is_none());
}

#[test]
fn deactivate_still_releases_this_devices_own_seat_one_held_to_no_machine_or_one_it_cannot_judge() {
    type LeaseOf = fn(&Harness) -> String;
    let leases: [(&str, LeaseOf); 3] = [
        ("its own", |h| lease_for(h, "fp-test", "machine")),
        ("held to no machine", |h| {
            h.lease(KEY, "perpetual", 30 * 86_400, NOW)
        }),
        ("held to a host name", |h| {
            lease_for(h, "fp-other-pc", "hostname")
        }),
    ];
    for (which, lease) in leases {
        let h = Harness::empty();
        h.set_state(licensed(KEY, &lease(&h)));
        deactivate_answers(&h);
        assert_eq!(
            h.license.deactivate().expect("deactivated"),
            RELEASED,
            "{which}"
        );
        assert!(h.http.called("deactivate"), "{which}");
        assert!(h.state().key.is_none(), "{which}");
    }
    // A run that cannot tell who the device is cannot tell that the seat is
    // another's: it is released as any seat is.
    let h = Harness::new(
        None,
        Opts {
            fingerprint: Some(failing()),
            ..Opts::default()
        },
    );
    h.set_state(licensed(KEY, &lease_for(&h, "fp-other-pc", "machine")));
    deactivate_answers(&h);
    assert_eq!(h.license.deactivate().expect("deactivated"), RELEASED);
    assert!(h.http.called("deactivate"));
}

// --- trials and the device ------------------------------------------------------------------

fn trial_lease(h: &Harness, device: &str, kind: &str) -> String {
    let claim = fingerprint_claim(device);
    h.key.lease_with(
        NOW,
        7 * 86_400,
        json!({ "sub": format!("trial:{claim}"), "aid": claim, "fpk": kind, "model": "trial", "lim": 1, "major": OPEN_MAJOR }),
    )
}

fn trial_state(start: i64, lease: String) -> StoredState {
    StoredState {
        trial_start: Some(start),
        trial_lease: Some(lease),
        // Synced within the day, so no launch report reads the device beside
        // the reads these tests count (launch.rs `report_launch`).
        trial_at: Some(NOW - DAY / 2),
        last_seen: Some(NOW),
        ..StoredState::default()
    }
}

#[test]
fn a_trial_lease_is_held_to_its_device_only_under_a_machine_id_and_only_when_the_run_can_tell() {
    let h = Harness::empty();
    h.set_state(trial_state(NOW, trial_lease(&h, "fp-test", "machine")));
    assert_eq!(h.license.status(false).state, LicenseState::Trial);
    let h = Harness::empty();
    h.set_state(trial_state(
        NOW - 30 * DAY,
        trial_lease(&h, "fp-another-machine", "machine"),
    ));
    let mut expired = LicenseStatus::new(LicenseState::Expired);
    expired.reason = Some(LicenseReason::Trial);
    assert_eq!(h.license.status(false), expired);
    let h = Harness::empty();
    h.set_state(trial_state(
        NOW - 30 * DAY,
        trial_lease(&h, "fp-another-machine", "hostname"),
    ));
    assert_eq!(h.license.status(false).state, LicenseState::Trial);
    let h = Harness::new(
        None,
        Opts {
            fingerprint: Some(failing()),
            ..Opts::default()
        },
    );
    h.set_state(trial_state(
        NOW,
        trial_lease(&h, "fp-another-machine", "machine"),
    ));
    assert_eq!(h.license.status(false).state, LicenseState::Trial);
}

#[test]
fn takes_a_trial_lease_started_under_the_host_name_this_device_answers_to() {
    let h = harness_over(WinSource::slow(0));
    h.set_state(trial_state(
        NOW - 2 * DAY,
        trial_lease(&h, &hostname_fingerprint(), "machine"),
    ));
    assert_eq!(h.license.status(false).state, LicenseState::Trial);
}

#[test]
fn starts_no_trial_on_a_run_that_cannot_tell_who_the_device_is_and_sends_the_kind_when_it_can() {
    let h = Harness::new(
        None,
        Opts {
            fingerprint: Some(failing()),
            ..Opts::default()
        },
    );
    h.license.start_trial();
    assert!(!h.http.called("trial"));
    let h = harness_over(WinSource::slow(0));
    h.http.answer("trial", 503, json!({}));
    h.license.start_trial();
    assert_eq!(
        h.http.body_of("trial"),
        json!({ "fingerprint": machine(), "fingerprintKind": "machine" })
    );
}

#[test]
fn a_device_refusal_stored_without_a_device_stands_for_every_device() {
    let h = Harness::empty();
    let mut stored = licensed(KEY, &h.lease(KEY, "perpetual", 30 * 86_400, NOW));
    stored.refused = Some(Refusal::Device);
    stored.backoff_since = Some(NOW);
    h.set_state(stored);
    assert_eq!(
        h.license.status(false),
        refused(LicenseReason::Device, Some(KEY))
    );
}

/// A fingerprinter that ignores its timeout: each read waits at `gate` until
/// released, counting the reads begun (in `get`, or in `identify` when it has one).
struct Stuck {
    gate: Gate,
    reads: Arc<AtomicUsize>,
    identifies: bool,
}

impl Fingerprinter for Stuck {
    fn get(&self) -> Result<String, AdapterError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.gate.wait();
        Ok("fp-test".into())
    }
    fn identify(&self, _timeout: Duration) -> Option<DeviceIdentity> {
        if !self.identifies {
            return None;
        }
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.gate.wait();
        Some(DeviceIdentity {
            fingerprint: Some("fp-test".into()),
            kind: Some(DeviceKind::Machine),
            alternates: Vec::new(),
        })
    }
}

#[test]
fn a_fingerprinter_that_ignores_its_timeout_is_not_waited_on_past_its_bound() {
    for identifies in [false, true] {
        let gate = Gate::new();
        let reads = Arc::new(AtomicUsize::new(0));
        let stuck = Stuck {
            gate: gate.clone(),
            reads: Arc::clone(&reads),
            identifies,
        };
        let h = Harness::new(
            None,
            Opts {
                fingerprint: Some(Arc::new(stuck)),
                timings: Some(fast_timings()),
                ..Opts::default()
            },
        );
        h.set_state(licensed(
            KEY,
            &lease_for(&h, "fp-another-machine", "machine"),
        ));
        h.http.fail(true);
        // Each call waits its own bound, cannot tell, and does not judge the
        // device. A read past its bound is not waited on again: the next call
        // reads anew, as a failed read is not kept.
        for call in 1..=3 {
            let started = Instant::now();
            assert_eq!(h.license.status(false), licensed_as(KEY));
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "{:?}",
                started.elapsed()
            );
            // Its own read: begun by now, or about to be on a loaded machine.
            wait_until(|| reads.load(Ordering::SeqCst) >= call);
            assert_eq!(reads.load(Ordering::SeqCst), call, "identify: {identifies}");
        }
        // Once they answer, the next call reads what they say: another device.
        gate.release();
        wait_until(|| h.license.status(false) == refused(LicenseReason::Device, Some(KEY)));
    }
}
