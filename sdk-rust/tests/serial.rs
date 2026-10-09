//! One call at a time, every request bounded, and calls behind a stuck one
//! going ahead read-only (real threads). A bound a test waits out is short;
//! one a call must not reach (an ask that answers, a call that finishes) is
//! patient, so that the outcome holds on a loaded machine.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use common::*;
use keygrant::lease::OPEN_MAJOR;
use keygrant::offline::RETRY_UNANSWERED_MS;
use keygrant::{
    Error, LicenseError, LicenseReason, LicenseState, LicenseStatus, StoredState, Timings,
};
use serde_json::json;

fn licensed_as(key: &str) -> LicenseStatus {
    let mut status = LicenseStatus::new(LicenseState::Licensed);
    status.key = Some(key.to_owned());
    status
}

fn stalled(mut status: LicenseStatus) -> LicenseStatus {
    status.stalled = true;
    status
}

fn invalid(reason: LicenseReason) -> LicenseStatus {
    let mut status = LicenseStatus::new(LicenseState::Invalid);
    status.reason = Some(reason);
    status.key = Some(KEY.to_owned());
    status
}

/// A harness whose bounds a test can wait out: requests `http_ms`, the grace 300 ms.
fn quick(initial: Option<StoredState>, http_ms: u64) -> Harness {
    with_bounds(initial, Duration::from_millis(http_ms), fast_timings())
}

fn with_bounds(initial: Option<StoredState>, http: Duration, timings: Timings) -> Harness {
    Harness::new(
        initial,
        Opts {
            http_timeout: Some(http),
            timings: Some(timings),
            ..Opts::default()
        },
    )
}

fn valid(h: &Harness) -> StoredState {
    licensed(KEY, &h.lease(KEY, "perpetual", 30 * 86_400, NOW))
}

#[test]
fn overlapping_status_calls_ask_once_and_the_refusal_survives() {
    // Nothing here is waited out: the ask answers once let, and the second
    // call waits for the first.
    let h = Harness::new(
        None,
        Opts {
            major: Some(3),
            http_timeout: Some(PATIENT),
            ..Opts::default()
        },
    );
    let mut stored = licensed(
        KEY,
        &h.key
            .lease_with(NOW, 30 * 86_400, json!({ "sub": KEY, "major": OPEN_MAJOR })),
    );
    stored.checked_major = Some(2);
    h.set_state(stored);
    let gate = Gate::new();
    let asked = Arc::new(AtomicUsize::new(0));
    {
        let (gate, asked) = (gate.clone(), Arc::clone(&asked));
        h.http.on("validate", move |_| {
            if asked.fetch_add(1, Ordering::SeqCst) > 0 {
                return reply(503, json!({ "error": "busy" }));
            }
            gate.wait();
            reply(403, json!({ "error": "revoked" }))
        });
    }
    thread::scope(|scope| {
        let first = scope.spawn(|| h.license.status(false));
        gate.await_reached();
        let second = scope.spawn(|| h.license.status(false));
        thread::sleep(Duration::from_millis(100));
        gate.release();
        assert_eq!(
            first.join().expect("first"),
            invalid(LicenseReason::Revoked)
        );
        assert_eq!(
            second.join().expect("second"),
            invalid(LicenseReason::Revoked)
        );
    });
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    assert_has(
        &h.state(),
        json!({ "refused": "revoked", "checkedMajor": 3 }),
    );
    assert_eq!(h.state().lease, None);
}

#[test]
fn a_check_the_network_never_answers_gives_the_valid_lease_after_its_bound() {
    let h = Harness::new(
        None,
        Opts {
            major: Some(3),
            timings: Some(fast_timings()),
            ..Opts::default()
        },
    );
    let mut stored = licensed(
        KEY,
        &h.key
            .lease_with(NOW, 30 * 86_400, json!({ "sub": KEY, "major": OPEN_MAJOR })),
    );
    stored.checked_major = Some(2);
    h.set_state(stored);
    h.http.hang(true);
    let started = Instant::now();
    assert_eq!(h.license.status(false), licensed_as(KEY));
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(290),
        "answered before the bound: {took:?}"
    );
    // Not the 15 s every other request may take.
    assert!(
        took < Duration::from_secs(10),
        "held past the bound: {took:?}"
    );
    assert_has(
        &h.state(),
        json!({ "backoffSince": NOW, "backoffMs": RETRY_UNANSWERED_MS }),
    );
}

#[test]
fn every_request_is_bounded_by_the_configured_timeout() {
    let h = with_bounds(None, Duration::from_millis(300), patient_timings());
    h.http.hang(true);
    let started = Instant::now();
    let result = h.license.activate(KEY);
    let took = started.elapsed();
    assert_eq!(
        result.err().and_then(|e| e.license_error()),
        Some(LicenseError::Network)
    );
    // Asked, and given up at the bound, not when the server would answer.
    assert!(h.http.called("activate"));
    assert!(
        took >= Duration::from_millis(290) && took < Duration::from_secs(10),
        "{took:?}"
    );
}

#[test]
fn a_hung_request_does_not_hold_the_calls_behind_it() {
    // The request's bound is waited out; the call behind it waits for it to
    // finish however long that takes, and so goes ahead with writes.
    let h = with_bounds(None, Duration::from_millis(300), patient_timings());
    h.set_state(valid(&h));
    h.http.hang(true);
    thread::scope(|scope| {
        let refreshed = scope.spawn(|| h.license.refresh());
        wait_until(|| h.http.called("validate"));
        let later = h.license.status(false);
        assert_eq!(refreshed.join().expect("refresh"), licensed_as(KEY));
        assert_eq!(later, licensed_as(KEY));
    });
}

#[test]
fn a_call_stuck_on_storage_lets_the_next_go_ahead_read_only_after_the_grace() {
    let h = quick(None, 200);
    h.set_state(valid(&h));
    let gate = h.storage.hang_next_read();
    thread::scope(|scope| {
        let stuck = scope.spawn(|| h.license.status(false));
        gate.await_reached();
        let started = Instant::now();
        let later = h.license.status(false);
        let took = started.elapsed();
        // Answered from storage behind the stuck call, and saying so.
        assert_eq!(later, stalled(licensed_as(KEY)));
        assert!(
            took >= Duration::from_millis(400) && took < Duration::from_secs(10),
            "{took:?}"
        );
        assert_eq!(h.storage.writes(), 0);
        gate.release();
        assert_eq!(stuck.join().expect("the stuck call"), licensed_as(KEY));
    });
}

#[test]
fn no_call_behind_a_stuck_write_asks_or_writes_and_the_next_heals() {
    // The asks answer, and land within their 5 s; the call behind the stuck
    // one goes ahead read-only 5.3 s after it began.
    let h = with_bounds(
        None,
        Duration::from_secs(5),
        Timings {
            lock_grace: Duration::from_millis(300),
            ..patient_timings()
        },
    );
    let lease = h.lease(KEY, "perpetual", 30 * 86_400, NOW);
    h.set_state(licensed(KEY, &lease));
    h.http
        .answer("validate", 403, json!({ "error": "revoked" }));
    let gate = h.storage.hold_next_write();
    thread::scope(|scope| {
        let a = scope.spawn(|| h.license.refresh());
        gate.await_reached();
        wait_until(|| h.validates() == 1);
        h.validate_with(&h.lease(KEY, "perpetual", 30 * 86_400, NOW));
        // Read-only: the lease still stored, licensed; not asked, nothing written.
        assert_eq!(h.license.refresh(), stalled(licensed_as(KEY)));
        assert_eq!(h.validates(), 1);
        assert_eq!(h.storage.writes(), 1);
        assert_eq!(h.state().lease.as_deref(), Some(lease.as_str()));
        gate.release();
        assert_eq!(a.join().expect("a"), invalid(LicenseReason::Revoked));
    });
    assert_eq!(h.state().refused, Some(keygrant::Refusal::Revoked));
    // A finished: the next call writes again, and heals.
    assert_eq!(h.license.refresh(), licensed_as(KEY));
    assert_eq!(h.validates(), 2);
}

#[test]
fn the_grace_runs_from_when_the_stuck_call_started_not_from_when_the_next_was_queued() {
    // The bound is 6 s from when the stuck call began, and the next call is
    // queued 3 s on: from its queueing, it would answer 9 s on, not 6 s.
    let timings = Timings {
        lock_grace: Duration::from_millis(5_900),
        ..fast_timings()
    };
    let h = with_bounds(None, Duration::from_millis(100), timings);
    h.set_state(valid(&h));
    h.http
        .answer("validate", 403, json!({ "error": "revoked" }));
    let gate = h.storage.hold_next_write();
    thread::scope(|scope| {
        let began = Instant::now();
        let _stuck = scope.spawn(|| h.license.refresh());
        gate.await_reached();
        thread::sleep(Duration::from_secs(3).saturating_sub(began.elapsed()));
        let later = h.license.status(false);
        let answered = began.elapsed();
        assert_eq!(later, stalled(licensed_as(KEY)));
        assert!(answered >= Duration::from_millis(5_900), "{answered:?}");
        assert!(answered < Duration::from_millis(8_500), "{answered:?}");
        gate.release();
    });
}

/// A refresh whose refusal write is held inside the adapter, and time past the
/// grace: what is called now runs read-only behind it. Returns the gate.
fn behind_a_stuck_call(h: &Harness) -> Gate {
    h.http
        .answer("validate", 403, json!({ "error": "revoked" }));
    h.storage.hold_next_write()
}

#[test]
fn behind_a_stuck_call_activate_is_network_deactivate_busy_and_start_trial_the_stored_trial() {
    let h = quick(None, 100);
    let mut stored = valid(&h);
    stored.trial_start = Some(NOW);
    stored.trial_ends_at = Some(NOW + 5 * DAY);
    stored.trial_lease = Some(h.lease("trial:fp-test", "trial", 5 * 86_400, NOW));
    h.set_state(stored);
    let gate = behind_a_stuck_call(&h);
    thread::scope(|scope| {
        let _stuck = scope.spawn(|| h.license.refresh());
        gate.await_reached();
        let result = h.license.activate(KEY);
        assert_eq!(
            result.err().and_then(|e| e.license_error()),
            Some(LicenseError::Network)
        );
        assert!(matches!(h.license.deactivate(), Err(Error::Busy)));
        let mut trial = LicenseStatus::new(LicenseState::Trial);
        trial.trial_ends_at = Some(NOW + 5 * DAY);
        trial.trial_days_left = Some(5);
        assert_eq!(h.license.start_trial(), stalled(trial));
        assert_eq!(
            (
                h.http.count("activate"),
                h.http.count("deactivate"),
                h.http.count("trial")
            ),
            (0, 0, 0)
        );
        assert_eq!(h.storage.writes(), 1);
        assert_eq!(h.state().key.as_deref(), Some(KEY));
        gate.release();
    });
}

#[test]
fn behind_a_stuck_call_a_stored_refusal_or_expired_lease_is_answered_without_asking() {
    let h = quick(None, 100);
    h.set_state(StoredState {
        key: Some(KEY.into()),
        activation_id: Some("act_1".into()),
        refused: Some(keygrant::Refusal::Revoked),
        lease: Some(h.lease(KEY, "perpetual", 30 * 86_400, NOW)),
        last_seen: Some(NOW),
        ..StoredState::default()
    });
    h.http
        .answer("validate", 403, json!({ "error": "revoked" }));
    let gate = h.storage.hold_next_write();
    thread::scope(|scope| {
        let _stuck = scope.spawn(|| h.license.refresh());
        gate.await_reached();
        // Its ask is made on a thread of its own: counted by now, or soon.
        wait_until(|| h.validates() == 1);
        assert_eq!(
            h.license.status(false),
            stalled(invalid(LicenseReason::Revoked))
        );
        assert_eq!(h.validates(), 1);
        gate.release();
    });

    let h = quick(None, 100);
    h.set_state(licensed(
        KEY,
        &h.lease(KEY, "perpetual", 3600, NOW - 2 * DAY),
    ));
    let gate = behind_a_stuck_call(&h);
    thread::scope(|scope| {
        let _stuck = scope.spawn(|| h.license.refresh());
        gate.await_reached();
        wait_until(|| h.validates() == 1);
        let mut expired = LicenseStatus::new(LicenseState::Expired);
        expired.reason = Some(LicenseReason::Offline);
        expired.key = Some(KEY.to_owned());
        assert_eq!(h.license.status(false), stalled(expired));
        assert_eq!(h.validates(), 1);
        gate.release();
    });
}

#[test]
fn behind_a_stuck_call_a_trial_whose_lease_does_not_verify_is_not_asked_for() {
    let h = quick(None, 100);
    h.set_state(StoredState {
        trial_start: Some(NOW - 2 * DAY),
        trial_ends_at: Some(NOW + 5 * DAY),
        trial_lease: Some(TestKey::new(8).lease("trial:fp-test", "trial", 5 * 86_400, NOW)),
        last_seen: Some(NOW),
        ..StoredState::default()
    });
    let gate = h.storage.hang_next_read();
    thread::scope(|scope| {
        let _stuck = scope.spawn(|| h.license.status(false));
        gate.await_reached();
        let mut over = LicenseStatus::new(LicenseState::Expired);
        over.reason = Some(LicenseReason::Trial);
        assert_eq!(h.license.status(false), stalled(over));
        assert!(!h.http.called("trial"));
        assert_eq!(h.storage.writes(), 0);
        gate.release();
    });
}

#[test]
fn behind_a_stuck_call_an_unreadable_state_is_unknown_and_stalled() {
    let h = quick(None, 100);
    h.set_state(valid(&h));
    let gate = h.storage.hang_next_read();
    thread::scope(|scope| {
        let _stuck = scope.spawn(|| h.license.status(false));
        gate.await_reached();
        h.storage.fail_reads(true);
        let mut unknown = LicenseStatus::new(LicenseState::Unknown);
        unknown.reason = Some(LicenseReason::Storage);
        assert_eq!(h.license.status(false), stalled(unknown));
        assert_eq!(h.storage.writes(), 0);
        gate.release();
    });
}

#[test]
fn three_calls_on_a_slow_network_run_one_after_another_each_asking() {
    // Each answer takes 400 ms, and lands: each call waits for the one
    // before it, and asks in turn.
    let h = with_bounds(None, PATIENT, patient_timings());
    let lease = h.lease(KEY, "perpetual", 30 * 86_400, NOW);
    h.set_state(licensed(KEY, &lease));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let most = Arc::new(AtomicUsize::new(0));
    {
        let (in_flight, most, lease) = (Arc::clone(&in_flight), Arc::clone(&most), lease.clone());
        h.http.on("validate", move |_| {
            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            most.fetch_max(now, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(400));
            in_flight.fetch_sub(1, Ordering::SeqCst);
            reply(200, json!({ "lease": lease, "activationId": "act_1" }))
        });
    }
    thread::scope(|scope| {
        let calls: Vec<_> = (0..3)
            .map(|_| scope.spawn(|| h.license.refresh()))
            .collect();
        for call in calls {
            assert_eq!(call.join().expect("a call"), licensed_as(KEY));
        }
    });
    assert_eq!(h.validates(), 3);
    assert_eq!(most.load(Ordering::SeqCst), 1);
}
