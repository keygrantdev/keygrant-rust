//! A running trial reports its launch from `status()`, at most once a day, on
//! a thread of its own that no call waits on.

mod common;

use std::thread;
use std::time::{Duration, Instant};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::*;
use keygrant::launch::TRIAL_REPORT_MS;
use keygrant::machine_id::MACHINE_ID_TIMEOUT;
use keygrant::{DeviceIdentity, DeviceKind, LicenseState, LicenseStatus, StoredState, Timings};
use serde_json::json;

const HOUR: i64 = 3_600_000;

fn trial_status() -> LicenseStatus {
    let mut status = LicenseStatus::new(LicenseState::Trial);
    status.trial_ends_at = Some(NOW + 5 * DAY);
    status.trial_days_left = Some(5);
    status
}

/// A running trial (its lease valid), over `state`: no trial answer or report
/// stamped unless given.
fn running(h: &Harness, state: StoredState) -> StoredState {
    StoredState {
        trial_start: Some(NOW - 2 * DAY),
        trial_ends_at: Some(NOW + 5 * DAY),
        trial_lease: Some(h.lease("trial:fp-test", "trial", 5 * 86_400, NOW)),
        last_seen: Some(NOW),
        ..state
    }
}

#[test]
fn status_stamps_it_first_then_posts_it_in_the_background_as_start_trial_would_its_answer_not_read()
{
    let h = Harness::empty();
    h.set_state(running(&h, StoredState::default()));
    h.http.answer(
        "trial",
        200,
        json!({ "trialStart": NOW - 9 * DAY, "trialEndsAt": NOW + 30 * DAY }),
    );
    assert_eq!(h.license.status(false), trial_status());
    assert_has(&h.state(), json!({ "trialReportedAt": NOW }));
    h.license.launch_report_settled();
    assert_eq!(
        h.http.calls(),
        [(
            "trial".to_owned(),
            json!({ "fingerprint": "fp-test", "clientStart": NOW - 2 * DAY })
        )]
    );
    // The answer is not read: the stored trial is as it was.
    assert_has(
        &h.state(),
        json!({ "trialStart": NOW - 2 * DAY, "trialEndsAt": NOW + 5 * DAY }),
    );
}

#[test]
fn it_is_made_at_most_once_a_day_a_start_trial_answer_counting_as_one() {
    let h = Harness::empty();
    h.set_state(running(&h, StoredState::default()));
    h.license.status(false);
    h.license.launch_report_settled();
    h.set_clock(NOW + TRIAL_REPORT_MS - 1);
    h.license.status(false);
    h.license.launch_report_settled();
    assert_eq!(h.http.count("trial"), 1);
    h.set_clock(NOW + TRIAL_REPORT_MS);
    h.license.status(false);
    h.license.launch_report_settled();
    assert_eq!(h.http.count("trial"), 2);

    let synced = Harness::empty();
    synced.set_state(running(
        &synced,
        StoredState {
            trial_at: Some(NOW - DAY / 2),
            ..StoredState::default()
        },
    ));
    synced.license.status(false);
    synced.license.launch_report_settled();
    assert!(!synced.http.called("trial"));
}

#[test]
fn it_never_holds_up_the_launch_status_answers_while_the_report_hangs() {
    let h = Harness::empty();
    h.set_state(running(&h, StoredState::default()));
    h.http.hang(true);
    let started = Instant::now();
    assert_eq!(h.license.status(false), trial_status());
    // The report's request hangs for two minutes; status() did not wait for it.
    assert!(started.elapsed() < Duration::from_secs(20));
    // Made on another thread than the caller's: the caller is back while
    // the request is in flight.
    wait_until(|| h.http.called("trial"));
}

#[test]
fn it_is_bounded_as_a_status_check_is() {
    // The status check's bound is 300 ms here; the HTTP bound is patient, and
    // the request never answers.
    let h = Harness::new(
        None,
        Opts {
            http_timeout: Some(PATIENT),
            timings: Some(Timings {
                status_check: Duration::from_millis(300),
                ..patient_timings()
            }),
            ..Opts::default()
        },
    );
    h.set_state(running(&h, StoredState::default()));
    h.http.hang(true);
    h.license.status(false);
    wait_until(|| h.http.called("trial"));
    let started = Instant::now();
    h.license.launch_report_settled();
    let took = started.elapsed();
    assert!(took < Duration::from_secs(10), "{took:?}");
}

#[test]
fn it_never_fails_offline() {
    let h = Harness::empty();
    h.set_state(running(&h, StoredState::default()));
    h.http.fail(true);
    assert_eq!(h.license.status(false), trial_status());
    h.license.launch_report_settled();
    assert_eq!(h.http.count("trial"), 1);
    assert_eq!(h.license.status(false), trial_status());
}

#[test]
fn it_sends_nothing_when_its_stamp_cannot_be_saved_so_it_is_never_sent_twice_in_a_day() {
    let h = Harness::empty();
    h.set_state(running(&h, StoredState::default()));
    h.storage.fail_writes(true);
    assert_eq!(h.license.status(false), trial_status());
    h.license.launch_report_settled();
    assert!(!h.http.called("trial"));
}

#[test]
fn it_is_not_made_by_an_expired_trial_with_a_key_held_or_behind_a_call_that_has_not_finished() {
    let expired = Harness::empty();
    expired.set_state(StoredState {
        trial_lease: None,
        trial_ends_at: Some(NOW - 1),
        ..running(&expired, StoredState::default())
    });
    expired.license.status(false);
    expired.license.launch_report_settled();
    assert!(!expired.http.called("trial"));

    let keyed = Harness::empty();
    keyed.set_state(running(
        &keyed,
        StoredState {
            key: Some("KEY-1".into()),
            ..StoredState::default()
        },
    ));
    keyed.license.status(false);
    keyed.license.launch_report_settled();
    assert!(!keyed.http.called("trial"));

    let stalled = Harness::new(
        None,
        Opts {
            http_timeout: Some(Duration::from_millis(100)),
            timings: Some(fast_timings()),
            ..Opts::default()
        },
    );
    stalled.set_state(running(&stalled, StoredState::default()));
    let gate = stalled.storage.hang_next_read();
    thread::scope(|scope| {
        let _stuck = scope.spawn(|| stalled.license.status(false));
        gate.await_reached();
        let mut trial = trial_status();
        trial.stalled = true;
        assert_eq!(stalled.license.status(false), trial);
        stalled.license.launch_report_settled();
        assert!(!stalled.http.called("trial"));
        assert_eq!(stalled.storage.writes(), 0);
        gate.release();
    });
}

#[test]
fn it_reads_the_device_for_a_statuss_time_and_sends_nothing_from_a_device_with_no_own_fingerprint()
{
    let given = Arc::new(Mutex::new(Vec::new()));
    let h = Harness::new(None, recording(&given));
    h.set_state(running(&h, StoredState::default()));
    assert_eq!(h.license.status(false), trial_status());
    h.license.launch_report_settled();
    // Every read (the status's, the report's) given a status's 2 s, never an activation's.
    let given = lock(&given).clone();
    assert!(!given.is_empty());
    assert!(
        given.iter().all(|bound| *bound == MACHINE_ID_TIMEOUT),
        "{given:?}"
    );
    // Stamped, then nothing sent: the host name is no fingerprint to report under.
    assert_eq!(h.state().trial_reported_at, Some(NOW));
    assert!(!h.http.called("trial"));
}

#[test]
fn it_reads_the_device_off_the_callers_thread() {
    // The first read (the status's own) cannot tell at once; every later one
    // (the report's) waits at the gate. A status that read the device for the
    // report itself would wait there too.
    let gate = Gate::new();
    let waits = gate.clone();
    let reads = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&reads);
    let h = Harness::new(
        None,
        Opts {
            fingerprint: Some(Arc::new(IdentifyingFingerprinter(move || {
                if counted.fetch_add(1, Ordering::SeqCst) > 0 {
                    waits.wait();
                }
                DeviceIdentity::default()
            }))),
            ..Opts::default()
        },
    );
    h.set_state(running(&h, StoredState::default()));
    let started = Instant::now();
    assert_eq!(h.license.status(false), trial_status());
    let took = started.elapsed();
    gate.release();
    h.license.launch_report_settled();
    assert!(took < Duration::from_secs(10), "{took:?}");
}

#[test]
fn it_sends_the_kind_and_the_stored_start_reading_no_stash() {
    let h = Harness::new(
        None,
        Opts {
            fingerprint: Some(Arc::new(IdentifyingFingerprinter(|| DeviceIdentity {
                fingerprint: Some("fp-machine".into()),
                kind: Some(DeviceKind::Machine),
                alternates: Vec::new(),
            }))),
            ..Opts::default()
        },
    );
    h.registry.set(Some(NOW - 9 * DAY));
    h.set_state(running(&h, StoredState::default()));
    h.license.status(false);
    h.license.launch_report_settled();
    assert_eq!(
        h.http.body_of("trial"),
        json!({ "fingerprint": "fp-machine", "fingerprintKind": "machine", "clientStart": NOW - 2 * DAY })
    );
}

#[test]
fn it_is_stamped_and_judged_by_the_guarded_clock() {
    // The clock guard a day ahead of the device clock: reported an hour ago
    // by the device clock is a day and an hour ago by the guarded one, so due.
    let h = Harness::empty();
    h.set_state(StoredState {
        trial_reported_at: Some(NOW - HOUR),
        last_seen: Some(NOW + DAY),
        ..running(&h, StoredState::default())
    });
    assert_eq!(h.license.status(false).state, LicenseState::Trial);
    h.license.launch_report_settled();
    assert_eq!(h.http.count("trial"), 1);
    assert_eq!(h.state().trial_reported_at, Some(NOW + DAY));
}
