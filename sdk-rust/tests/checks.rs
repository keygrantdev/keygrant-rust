//! When the SDK asks the server, what each answer does to the stored state,
//! the waits, early renewal, the clock guard, and saves that fail.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use keygrant::lease::OPEN_MAJOR;
use keygrant::offline::{ASK_AGAIN_MS, CLOCK_TOLERANCE_MS, RENEW_WITHIN_MS, RETRY_UNANSWERED_MS};
use keygrant::{AdapterError, LicenseReason, LicenseState, LicenseStatus, Refusal, StoredState};
use serde_json::json;

fn with(state: LicenseState, reason: Option<LicenseReason>, key: Option<&str>) -> LicenseStatus {
    let mut status = LicenseStatus::new(state);
    status.reason = reason;
    status.key = key.map(str::to_owned);
    status
}

fn licensed_as(key: &str) -> LicenseStatus {
    with(LicenseState::Licensed, None, Some(key))
}

fn invalid(reason: LicenseReason) -> LicenseStatus {
    with(LicenseState::Invalid, Some(reason), Some(KEY))
}

/// An open lease (every version), carrying its upgrade window's end when given (ms).
fn open_lease(h: &Harness, window_ends: Option<i64>, model: &str, ttl: i64) -> String {
    let mut over = json!({ "sub": KEY, "major": OPEN_MAJOR, "model": model });
    if let Some(end) = window_ends {
        over["upgradeUntil"] = json!(end / 1000);
    }
    h.key.lease_with(NOW, ttl, over)
}

/// A device on 3 holding a valid open lease, last heard by the server on 2.
fn on_new_major() -> Harness {
    let h = Harness::new(
        None,
        Opts {
            major: Some(3),
            ..Opts::default()
        },
    );
    let mut stored = licensed(KEY, &open_lease(&h, None, "perpetual", 30 * 86_400));
    stored.checked_major = Some(2);
    h.set_state(stored);
    h
}

// --- upgrade protection: telling the server what this device runs ------------------------

#[test]
fn reports_a_newer_major_on_an_open_lease_at_once_and_notes_it_was_heard() {
    let h = Harness::new(
        None,
        Opts {
            major: Some(3),
            ..Opts::default()
        },
    );
    let mut stored = licensed(
        KEY,
        &open_lease(&h, Some(NOW + 10 * DAY), "perpetual", 30 * 86_400),
    );
    stored.checked_major = Some(2);
    h.set_state(stored);
    let fresh = open_lease(&h, Some(NOW + 10 * DAY), "perpetual", 30 * 86_400);
    h.validate_with(&fresh);
    assert_eq!(h.license.status(false), licensed_as(KEY));
    let body = h.http.body_of("validate");
    assert_eq!(
        (
            body["key"].clone(),
            body["major"].clone(),
            body["activationId"].clone()
        ),
        (json!(KEY), json!(3), json!("act_1"))
    );
    assert_has(
        &h.state(),
        json!({ "lease": fresh, "checkedMajor": 3, "keptMajor": 3 }),
    );
}

#[test]
fn keeps_the_open_lease_and_the_app_running_when_that_check_cannot_reach_the_server() {
    let h = on_new_major();
    let lease = h.state().lease;
    h.http.fail(true);
    assert_eq!(h.license.status(false), licensed_as(KEY));
    assert!(h.http.called("validate"));
    assert_eq!(h.state().lease, lease);
    assert_eq!(h.state().checked_major, Some(2));
}

#[test]
fn notes_the_run_even_when_it_cannot_be_saved_and_sends_it_with_the_check() {
    let h = Harness::new(
        None,
        Opts {
            major: Some(3),
            ..Opts::default()
        },
    );
    let lease = open_lease(&h, Some(NOW + 10 * DAY), "perpetual", 30 * 86_400);
    let mut stored = licensed(KEY, &lease);
    stored.checked_major = Some(2);
    h.set_state(stored);
    h.validate_with(&lease);
    h.storage.fail_writes(true);
    assert_eq!(h.license.status(false), licensed_as(KEY));
    assert_eq!(h.http.body_of("validate")["keptMajor"], json!(3));
}

#[test]
fn does_not_ask_when_the_server_has_heard_this_major() {
    let h = Harness::new(
        None,
        Opts {
            major: Some(3),
            ..Opts::default()
        },
    );
    let mut stored = licensed(
        KEY,
        &open_lease(&h, Some(NOW + 10 * DAY), "perpetual", 30 * 86_400),
    );
    stored.checked_major = Some(3);
    h.set_state(stored);
    h.license.status(false);
    assert!(h.http.calls().is_empty());
    // It noted the major it runs in the window, and sends it on the next validate.
    assert_eq!(h.state().kept_major, Some(3));
    h.http
        .answer("validate", 503, json!({ "error": "unavailable" }));
    h.license.refresh();
    let body = h.http.body_of("validate");
    assert_eq!(
        (body["major"].clone(), body["keptMajor"].clone()),
        (json!(3), json!(3))
    );
}

#[test]
fn stops_noting_at_the_windows_end_counting_its_last_second_as_closed() {
    let end = NOW + 10 * DAY;
    for (clock, kept) in [(end + DAY, 3), (end, 3), (end - 1000, 4)] {
        let h = Harness::new(
            None,
            Opts {
                major: Some(4),
                ..Opts::default()
            },
        );
        let mut stored = licensed(KEY, &open_lease(&h, Some(end), "perpetual", 30 * 86_400));
        stored.checked_major = Some(4);
        stored.kept_major = Some(3);
        h.set_state(stored);
        h.set_clock(clock);
        assert_eq!(h.license.status(false).state, LicenseState::Licensed);
        assert_eq!(h.state().kept_major, Some(kept), "at end {:+}", clock - end);
    }
}

#[test]
fn notes_nothing_on_a_lease_with_no_window() {
    let h = Harness::new(
        None,
        Opts {
            major: Some(3),
            ..Opts::default()
        },
    );
    let mut stored = licensed(KEY, &open_lease(&h, None, "perpetual", 30 * 86_400));
    stored.checked_major = Some(3);
    h.set_state(stored);
    h.license.status(false);
    assert_eq!(h.state().kept_major, None);
}

#[test]
fn starts_a_keys_notes_afresh_on_activate_and_drops_them_on_deactivate() {
    let h = Harness::new(
        Some(StoredState {
            key: Some("OLD-KEY".into()),
            activation_id: Some("act_old".into()),
            kept_major: Some(7),
            checked_major: Some(7),
            ..StoredState::default()
        }),
        Opts {
            major: Some(2),
            ..Opts::default()
        },
    );
    let lease = open_lease(&h, Some(NOW + 10 * DAY), "perpetual", 30 * 86_400);
    h.http.answer(
        "activate",
        200,
        json!({ "lease": lease, "activationId": "act_1" }),
    );
    h.license.activate(KEY).expect("activated");
    assert_has(
        &h.state(),
        json!({ "key": KEY, "checkedMajor": 2, "keptMajor": 2 }),
    );
    h.http.answer("deactivate", 200, json!({ "ok": true }));
    h.license.deactivate().expect("deactivated");
    assert_eq!(
        (h.state().kept_major, h.state().checked_major),
        (None, None)
    );
}

/// Something a fingerprinter does once, on its next read.
type Once = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

#[test]
fn never_lands_a_kept_major_noted_for_one_activation_on_another_saved_meanwhile() {
    // Another instance activates another key while this one judges its lease
    // (reading who the device is): the major this run noted (`keptMajor`) is
    // about the activation it judged, and lands on no other.
    let on_read: Once = Arc::default();
    let pending = Arc::clone(&on_read);
    let fingerprint = Arc::new(FnFingerprinter(move || -> Result<String, AdapterError> {
        let swap = lock(&pending).take();
        if let Some(swap) = swap {
            swap();
        }
        Ok("fp-test".into())
    }));
    let h = Harness::new(
        None,
        Opts {
            major: Some(5),
            fingerprint: Some(fingerprint),
            ..Opts::default()
        },
    );
    let mut stored = licensed(
        KEY,
        &open_lease(&h, Some(NOW + 10 * DAY), "perpetual", 30 * 86_400),
    );
    stored.checked_major = Some(5);
    h.set_state(stored);
    let other = StoredState {
        key: Some("SLUICE-OTHR-OTHR-OTHR-OTHR".into()),
        activation_id: Some("act_2".into()),
        lease: Some("theirs".into()),
        last_seen: Some(NOW),
        ..StoredState::default()
    };
    let storage = Arc::clone(&h.storage);
    *lock(&on_read) = Some(Box::new(move || storage.set(Some(other))));
    h.http.fail(true);
    assert_eq!(h.license.status(false), licensed_as(KEY));
    let saved = h.state();
    assert_eq!(
        (saved.activation_id.as_deref(), saved.kept_major),
        (Some("act_2"), None)
    );
}

// --- refusals: once, kept, and healed ----------------------------------------------------

#[test]
fn records_each_refusal_with_the_major_heard_and_does_not_ask_again() {
    for (word, refusal, reason) in [
        ("revoked", Refusal::Revoked, LicenseReason::Revoked),
        ("expired", Refusal::Revoked, LicenseReason::Revoked),
        ("deactivated", Refusal::Revoked, LicenseReason::Revoked),
        ("upgrade-required", Refusal::Upgrade, LicenseReason::Upgrade),
        (
            "update-required",
            Refusal::Outdated,
            LicenseReason::Outdated,
        ),
    ] {
        let h = on_new_major();
        h.http.answer("validate", 403, json!({ "error": word }));
        assert_eq!(h.license.status(false), invalid(reason), "{word}");
        let stored = h.state();
        assert_eq!(
            (stored.checked_major, stored.refused),
            (Some(3), Some(refusal)),
            "{word}"
        );
        assert_eq!(h.license.status(false), invalid(reason), "{word}");
        assert_eq!(h.validates(), 1, "{word}");
        if refusal == Refusal::Revoked {
            // About the key: the lease is dropped.
            assert_eq!((stored.lease, stored.refused_major), (None, None), "{word}");
        } else {
            // About this build: kept, with the major refused.
            assert!(stored.lease.is_some(), "{word}");
            assert_eq!(stored.refused_major, Some(3), "{word}");
        }
    }
}

#[test]
fn standing_refusal_by_build_lets_a_rollback_run_offline_and_holds_newer_builds() {
    let h = on_new_major();
    h.http
        .answer("validate", 403, json!({ "error": "upgrade-required" }));
    assert_eq!(
        h.license_for(4).status(false),
        invalid(LicenseReason::Upgrade)
    );
    h.http.fail(true);
    assert_eq!(h.license_for(3).status(false).state, LicenseState::Licensed);
    assert_eq!(
        h.license_for(4).status(false),
        invalid(LicenseReason::Upgrade)
    );
    assert_eq!(
        h.license_for(5).status(false),
        invalid(LicenseReason::Upgrade)
    );
    assert_eq!(h.validates(), 1);

    let h = on_new_major();
    h.http
        .answer("validate", 403, json!({ "error": "update-required" }));
    assert_eq!(h.license.status(false), invalid(LicenseReason::Outdated));
    h.http.fail(true);
    assert_eq!(
        h.license_for(3).status(false),
        invalid(LicenseReason::Outdated)
    );
    assert_eq!(h.license_for(4).status(false).state, LicenseState::Licensed);
    assert_eq!(
        h.license_for(2).status(false),
        invalid(LicenseReason::Outdated)
    );
}

#[test]
fn keeps_a_cancelled_subscriber_refused_offline_too() {
    let h = Harness::new(
        None,
        Opts {
            major: Some(3),
            ..Opts::default()
        },
    );
    let mut stored = licensed(KEY, &open_lease(&h, None, "subscription", 7 * 86_400));
    stored.checked_major = Some(2);
    h.set_state(stored);
    h.http
        .answer("validate", 403, json!({ "error": "expired" }));
    for _ in 0..2 {
        assert_eq!(h.license.status(false), invalid(LicenseReason::Revoked));
    }
    h.http.fail(true);
    assert_eq!(h.license.status(false), invalid(LicenseReason::Revoked));
    assert_eq!(h.license.refresh(), invalid(LicenseReason::Revoked));
}

#[test]
fn heals_a_refused_key_when_the_server_licenses_it_again() {
    let h = on_new_major();
    h.http
        .answer("validate", 403, json!({ "error": "expired" }));
    assert_eq!(h.license.status(false).state, LicenseState::Invalid);
    h.set_clock(NOW + ASK_AGAIN_MS);
    h.validate_with(&open_lease(&h, None, "perpetual", 30 * 86_400));
    assert_eq!(h.license.status(false).state, LicenseState::Licensed);
    assert_eq!(h.state().refused, None);
    h.http.fail(true);
    assert_eq!(h.license.status(false).state, LicenseState::Licensed);
}

#[test]
fn starts_afresh_on_a_new_activation_after_a_refusal_and_on_a_deactivate() {
    let h = on_new_major();
    h.http
        .answer("validate", 403, json!({ "error": "deactivated" }));
    assert_eq!(h.license.status(false).state, LicenseState::Invalid);
    let lease = h.key.lease_with(
        NOW,
        30 * 86_400,
        json!({ "sub": KEY, "aid": "act_2", "major": OPEN_MAJOR }),
    );
    h.http.answer(
        "activate",
        200,
        json!({ "lease": lease, "activationId": "act_2" }),
    );
    h.license.activate(KEY).expect("activated");
    assert_eq!(h.state().refused, None);
    h.http.fail(true);
    assert_eq!(h.license.status(false).state, LicenseState::Licensed);
    let mut refused = h.state();
    refused.refused = Some(Refusal::Revoked);
    h.set_state(refused);
    h.license.deactivate().expect("deactivated");
    assert_eq!(h.state().refused, None);
    assert_eq!(
        h.license.status(false),
        LicenseStatus::new(LicenseState::Unlicensed)
    );
}

#[test]
fn records_the_major_heard_on_an_answer_it_cannot_use_and_does_not_ask_again() {
    let h = on_new_major();
    h.http
        .answer("validate", 200, json!({ "lease": "not.a.lease" }));
    assert_eq!(h.license.status(false).state, LicenseState::Licensed);
    h.license.status(false);
    assert_eq!(h.validates(), 1);
    assert_eq!(h.state().checked_major, Some(3));
}

// --- the waits ----------------------------------------------------------------------------

#[test]
fn asks_a_refused_key_again_only_once_the_wait_is_over_and_at_once_on_a_refresh() {
    let h = on_new_major();
    h.http
        .answer("validate", 403, json!({ "error": "revoked" }));
    h.license.status(false);
    assert_eq!(h.validates(), 1);
    h.set_clock(NOW + ASK_AGAIN_MS - 1);
    assert_eq!(h.license.status(false), invalid(LicenseReason::Revoked));
    assert_eq!(h.validates(), 1);
    assert_eq!(h.license.refresh().state, LicenseState::Invalid);
    assert_eq!(h.validates(), 2);
    // The refresh's refusal starts the wait afresh.
    h.set_clock(NOW + 2 * ASK_AGAIN_MS - 2);
    h.license.status(false);
    assert_eq!(h.validates(), 2);
    h.set_clock(NOW + 2 * ASK_AGAIN_MS - 1);
    h.license.status(false);
    assert_eq!(h.validates(), 3);
}

#[test]
fn asks_for_an_expired_lease_again_after_the_short_wait_and_a_lease_ends_the_wait() {
    let h = Harness::empty();
    h.set_state(licensed(
        KEY,
        &h.lease(KEY, "perpetual", 3600, NOW - 2 * DAY),
    ));
    h.http.fail(true);
    assert_eq!(
        h.license.status(false),
        with(
            LicenseState::Expired,
            Some(LicenseReason::Offline),
            Some(KEY)
        )
    );
    assert_has(
        &h.state(),
        json!({ "backoffSince": NOW, "backoffMs": RETRY_UNANSWERED_MS }),
    );
    h.set_clock(NOW + RETRY_UNANSWERED_MS - 1);
    assert_eq!(h.license.status(false).state, LicenseState::Expired);
    assert_eq!(h.validates(), 1);
    h.set_clock(NOW + RETRY_UNANSWERED_MS);
    h.license.status(false);
    assert_eq!(h.validates(), 2);
    h.license.refresh();
    assert_eq!(h.validates(), 3);
    h.http.fail(false);
    h.validate_with(&h.lease(KEY, "perpetual", 30 * 86_400, NOW + RETRY_UNANSWERED_MS));
    assert_eq!(h.license.refresh().state, LicenseState::Licensed);
    assert_eq!(
        (h.state().backoff_since, h.state().backoff_ms),
        (None, None)
    );
}

#[test]
fn backs_off_a_minute_after_no_answer_and_an_hour_after_a_5xx_or_429() {
    let h = on_new_major();
    h.http.fail(true);
    assert_eq!(h.license.status(false).state, LicenseState::Licensed);
    assert_has(
        &h.state(),
        json!({ "checkedMajor": 2, "backoffSince": NOW, "backoffMs": RETRY_UNANSWERED_MS }),
    );
    h.set_clock(NOW + RETRY_UNANSWERED_MS - 1);
    h.license.status(false);
    assert_eq!(h.validates(), 1);
    h.set_clock(NOW + RETRY_UNANSWERED_MS);
    h.license.status(false);
    assert_eq!(h.validates(), 2);

    for code in [503, 429] {
        let h = on_new_major();
        h.http.answer("validate", code, json!({ "error": "busy" }));
        assert_eq!(h.license.status(false).state, LicenseState::Licensed);
        assert_has(
            &h.state(),
            json!({ "checkedMajor": 2, "backoffSince": NOW, "backoffMs": ASK_AGAIN_MS }),
        );
        h.set_clock(NOW + RETRY_UNANSWERED_MS);
        h.license.status(false);
        assert_eq!(h.validates(), 1, "{code}");
        h.set_clock(NOW + ASK_AGAIN_MS);
        h.license.status(false);
        assert_eq!(h.validates(), 2, "{code}");
    }
}

#[test]
fn asks_at_once_when_the_clock_was_set_back_since_the_ask() {
    let h = on_new_major();
    h.http.answer("validate", 503, json!({}));
    h.license.status(false);
    h.set_clock(NOW - 1);
    h.license.status(false);
    assert_eq!(h.validates(), 2);
}

/// A stored state a case builds, with leases from the harness's key.
type Stored = fn(&Harness) -> StoredState;

#[test]
fn times_the_wait_before_asking_again_by_the_device_clock_not_the_guard() {
    // The guard (lastSeen) is a day ahead of the device clock, and the last
    // ask was half an hour ago by the device clock: its hour's wait is not
    // over, though by the guard it would be.
    let cases: [(&str, Stored); 3] = [
        ("a stored refusal", |_| StoredState {
            key: Some(KEY.into()),
            activation_id: Some("act_1".into()),
            refused: Some(Refusal::Revoked),
            ..StoredState::default()
        }),
        ("a lease past its end", |h| {
            licensed(KEY, &h.lease(KEY, "perpetual", 3600, NOW - 2 * DAY))
        }),
        ("a valid lease near its end", |h| {
            licensed(KEY, &h.lease(KEY, "perpetual", 30 * 86_400, NOW - 26 * DAY))
        }),
    ];
    for (case, stored) in cases {
        let h = Harness::empty();
        h.set_state(StoredState {
            backoff_since: Some(NOW - 30 * 60_000),
            backoff_ms: Some(ASK_AGAIN_MS),
            last_seen: Some(NOW + DAY),
            ..stored(&h)
        });
        h.http.fail(true);
        h.license.status(false);
        assert_eq!(h.validates(), 0, "{case}");
    }
}

#[test]
fn starts_the_wait_after_a_renewal_by_the_device_clock_not_the_guard() {
    let h = Harness::empty();
    let mut stored = licensed(KEY, &h.lease(KEY, "perpetual", 3600, NOW - 2 * DAY));
    stored.last_seen = Some(NOW + DAY);
    h.set_state(stored);
    h.http.fail(true);
    assert_eq!(
        h.license.status(false),
        with(
            LicenseState::Expired,
            Some(LicenseReason::Offline),
            Some(KEY)
        )
    );
    assert_has(
        &h.state(),
        json!({ "backoffSince": NOW, "backoffMs": RETRY_UNANSWERED_MS }),
    );
}

#[test]
fn licenses_from_a_renewal_whose_check_of_the_servers_time_cannot_read_the_state() {
    // The renewal reads the state again to bring the clock guard to the
    // server's time (`Engine::server_now`): a read that fails there costs the
    // correction, never the renewal, and status() still answers.
    let h = Harness::empty();
    h.set_state(licensed(
        KEY,
        &h.lease(KEY, "perpetual", 3600, NOW - 2 * DAY),
    ));
    let fresh = h.lease(KEY, "perpetual", 30 * 86_400, NOW);
    h.validate_with(&fresh);
    // The status's own read is the first; the renewal's check of the time, the second.
    h.storage.fail_read(2);
    assert_eq!(h.license.status(false), licensed_as(KEY));
    assert_eq!(h.state().lease.as_deref(), Some(fresh.as_str()));
}

#[test]
fn heals_a_lease_that_ran_out_offline_once_the_network_is_back_and_still_waits_the_hour_after_a_refusal(
) {
    let h = Harness::empty();
    h.set_state(licensed(
        KEY,
        &h.lease(KEY, "perpetual", 3600, NOW - 2 * DAY),
    ));
    h.http.fail(true);
    h.license.status(false);
    h.http.fail(false);
    h.validate_with(&h.lease(KEY, "perpetual", 30 * 86_400, NOW));
    h.set_clock(NOW + RETRY_UNANSWERED_MS - 1);
    assert_eq!(h.license.status(false).state, LicenseState::Expired);
    h.set_clock(NOW + RETRY_UNANSWERED_MS);
    assert_eq!(h.license.status(false), licensed_as(KEY));

    let h = Harness::empty();
    h.set_state(licensed(
        KEY,
        &h.lease(KEY, "perpetual", 3600, NOW - 2 * DAY),
    ));
    h.http
        .answer("validate", 403, json!({ "error": "revoked" }));
    assert_eq!(h.license.status(false).reason, Some(LicenseReason::Revoked));
    h.set_clock(NOW + RETRY_UNANSWERED_MS);
    h.license.status(false);
    assert_eq!(h.validates(), 1);
    h.set_clock(NOW + ASK_AGAIN_MS);
    h.license.status(false);
    assert_eq!(h.validates(), 2);
}

// --- early renewal ------------------------------------------------------------------------

#[test]
fn renews_early_once_within_renew_within_ms_of_its_end_and_not_before() {
    let h = Harness::empty();
    h.set_state(licensed(KEY, &h.lease(KEY, "perpetual", 30 * 86_400, NOW)));
    let renewed = h.lease(
        KEY,
        "perpetual",
        30 * 86_400,
        NOW + 30 * DAY - RENEW_WITHIN_MS,
    );
    h.validate_with(&renewed);
    h.set_clock(NOW + 30 * DAY - RENEW_WITHIN_MS - 1);
    assert_eq!(h.license.status(false).state, LicenseState::Licensed);
    assert_eq!(h.validates(), 0);
    h.set_clock(NOW + 30 * DAY - RENEW_WITHIN_MS + 1);
    assert_eq!(h.license.status(false).state, LicenseState::Licensed);
    assert_eq!(h.validates(), 1);
    assert_eq!(h.state().lease.as_deref(), Some(renewed.as_str()));
    h.license.status(false);
    assert_eq!(h.validates(), 1);
}

#[test]
fn renews_a_short_lease_at_half_its_life_not_on_every_status() {
    let h = Harness::empty();
    h.set_state(licensed(
        KEY,
        &h.lease(KEY, "subscription", 7 * 86_400, NOW),
    ));
    h.http.fail(true);
    h.set_clock(NOW + 7 * DAY / 2 - 1);
    h.license.status(false);
    assert_eq!(h.validates(), 0);
    h.set_clock(NOW + 7 * DAY / 2 + 1);
    h.license.status(false);
    assert_eq!(h.validates(), 1);
}

#[test]
fn keeps_the_valid_lease_when_an_early_renewal_gets_nothing_and_waits() {
    let h = Harness::empty();
    let lease = h.lease(KEY, "perpetual", 30 * 86_400, NOW);
    h.set_state(licensed(KEY, &lease));
    h.http.fail(true);
    let at = NOW + 25 * DAY;
    h.set_clock(at);
    assert_eq!(h.license.status(false), licensed_as(KEY));
    assert_has(
        &h.state(),
        json!({ "lease": lease, "backoffSince": at, "backoffMs": RETRY_UNANSWERED_MS }),
    );
    h.license.status(false);
    assert_eq!(h.validates(), 1);
}

#[test]
fn answers_a_refusal_to_an_early_renewal() {
    let h = Harness::empty();
    h.set_state(licensed(
        KEY,
        &h.lease(KEY, "subscription", 7 * 86_400, NOW),
    ));
    h.http
        .answer("validate", 403, json!({ "error": "expired" }));
    h.set_clock(NOW + 5 * DAY);
    assert_eq!(h.license.status(false), invalid(LicenseReason::Revoked));
}

// --- the clock guard ------------------------------------------------------------------------

#[test]
fn licenses_again_online_after_a_clock_that_jumped_ahead_is_put_right() {
    let h = Harness::empty();
    let mut stored = licensed(KEY, &h.lease(KEY, "perpetual", 30 * 86_400, NOW - DAY));
    stored.last_seen = Some(NOW + 60 * DAY);
    h.set_state(stored);
    h.validate_with(&h.lease(KEY, "perpetual", 30 * 86_400, NOW));
    assert_eq!(h.license.status(false), licensed_as(KEY));
    assert_eq!(h.state().last_seen, Some(NOW));
    h.http.fail(true);
    assert_eq!(h.license.status(false), licensed_as(KEY));
}

#[test]
fn licenses_from_the_servers_lease_when_the_guard_it_puts_right_cannot_be_saved() {
    let h = Harness::empty();
    let mut stored = licensed(KEY, &h.lease(KEY, "perpetual", 30 * 86_400, NOW - DAY));
    stored.last_seen = Some(NOW + 60 * DAY);
    h.set_state(stored);
    h.validate_with(&h.lease(KEY, "perpetual", 30 * 86_400, NOW));
    h.storage.fail_writes(true);
    assert_eq!(h.license.status(false), licensed_as(KEY));
    assert_eq!(h.state().last_seen, Some(NOW + 60 * DAY));
}

/// The guard after a refresh answered with a fresh lease signed at NOW, the clock at `clock`.
fn guard_after_refresh(guard: i64, clock: i64) -> Option<i64> {
    let h = Harness::empty();
    let lease = h.lease(KEY, "perpetual", 30 * 86_400, NOW);
    let mut stored = licensed(KEY, &lease);
    stored.last_seen = Some(guard);
    h.set_state(stored);
    h.set_clock(clock);
    h.validate_with(&lease);
    assert_eq!(h.license.refresh().state, LicenseState::Licensed);
    h.state().last_seen
}

#[test]
fn the_guard_comes_back_only_past_the_tolerance_and_never_below_the_signing_time() {
    assert_eq!(
        guard_after_refresh(NOW + 30 * 60_000, NOW),
        Some(NOW + 30 * 60_000)
    );
    assert_eq!(
        guard_after_refresh(NOW + CLOCK_TOLERANCE_MS, NOW),
        Some(NOW + CLOCK_TOLERANCE_MS)
    );
    assert_eq!(
        guard_after_refresh(NOW + CLOCK_TOLERANCE_MS + 1, NOW),
        Some(NOW)
    );
    assert_eq!(
        guard_after_refresh(NOW + 60 * DAY, NOW - 2 * 3_600_000),
        Some(NOW)
    );
}

#[test]
fn the_guard_moves_for_this_key_in_any_case_and_not_for_another_key_activation_or_an_older_lease() {
    let h = Harness::empty();
    let mut stored = licensed(KEY, &h.lease(KEY, "perpetual", 30 * 86_400, NOW));
    stored.last_seen = Some(NOW + 60 * DAY);
    h.set_state(stored.clone());
    h.validate_with(&h.lease("Sluice-aaaa-BBBB-CCCC-DDDD", "perpetual", 30 * 86_400, NOW));
    h.license.refresh();
    assert_eq!(h.state().last_seen, Some(NOW));

    for (sub, aid, at) in [
        ("SLUICE-OTHR-OTHR-OTHR-OTHR", "act_1", NOW),
        (KEY, "act_2", NOW),
        (KEY, "act_1", NOW - DAY),
    ] {
        let h = Harness::empty();
        let mut stored = licensed(KEY, &h.lease(KEY, "perpetual", 30 * 86_400, NOW));
        stored.last_seen = Some(NOW + 60 * DAY);
        h.set_state(stored);
        let answer = h
            .key
            .lease_with(at, 30 * 86_400, json!({ "sub": sub, "aid": aid }));
        h.http.answer(
            "validate",
            200,
            json!({ "lease": answer, "activationId": aid }),
        );
        h.license.refresh();
        assert_eq!(
            h.state().last_seen,
            Some(NOW + 60 * DAY),
            "{sub} {aid} {at}"
        );
    }
}

#[test]
fn stamps_answers_by_the_guarded_clock_and_saves_its_own_after_the_clock_is_put_back() {
    let h = Harness::with(StoredState {
        last_seen: Some(NOW + 5 * 60_000),
        ..StoredState::default()
    });
    h.http.answer(
        "activate",
        200,
        json!({ "lease": h.lease(KEY, "perpetual", 30 * 86_400, NOW), "activationId": "act_1" }),
    );
    h.http.answer("deactivate", 200, json!({ "ok": true }));
    let trial = h.lease("trial:fp-test", "trial", 5 * 86_400, NOW);
    h.http.answer(
        "trial",
        200,
        json!({ "trialStart": NOW, "trialEndsAt": NOW + 5 * DAY, "lease": trial }),
    );
    h.license.activate(KEY).expect("activated");
    assert_eq!(h.state().verdict_at, Some(NOW + 5 * 60_000));
    h.set_clock(NOW - DAY);
    h.license.deactivate().expect("deactivated");
    assert_eq!(h.state().verdict_at, Some(NOW + 5 * 60_000));
    h.license.start_trial();
    assert_eq!(h.state().trial_at, Some(NOW + 5 * 60_000));

    // The last answer was stamped by a clock 60 days ahead, since put right.
    let h = Harness::empty();
    let mut stored = licensed(KEY, &h.lease(KEY, "perpetual", 30 * 86_400, NOW - DAY));
    stored.verdict_at = Some(NOW + 60 * DAY);
    h.set_state(stored);
    h.http
        .answer("validate", 403, json!({ "error": "revoked" }));
    assert_eq!(h.license.refresh(), invalid(LicenseReason::Revoked));
    assert_has(
        &h.state(),
        json!({ "refused": "revoked", "verdictAt": NOW }),
    );
    assert_eq!(h.state().lease, None);
}

// --- a save that fails once the server has answered ------------------------------------------

#[test]
fn the_servers_answer_stands_when_it_cannot_be_saved() {
    let h = Harness::empty();
    let lease = h.lease(KEY, "perpetual", 30 * 86_400, NOW);
    h.set_state(licensed(KEY, &lease));
    h.http
        .answer("validate", 403, json!({ "error": "revoked" }));
    h.storage.fail_writes(true);
    assert_eq!(h.license.refresh(), invalid(LicenseReason::Revoked));
    assert!(h.storage.writes() > 0);
    assert_eq!(h.state().lease.as_deref(), Some(lease.as_str()));

    let h = Harness::empty();
    let old = h.lease(KEY, "perpetual", 3600, NOW - 2 * DAY);
    h.set_state(licensed(KEY, &old));
    let fresh = h.lease(KEY, "perpetual", 30 * 86_400, NOW);
    h.validate_with(&fresh);
    h.storage.fail_writes(true);
    assert_eq!(h.license.status(false), licensed_as(KEY));
    assert_eq!(h.state().lease.as_deref(), Some(old.as_str()));
    h.storage.fail_writes(false);
    assert_eq!(h.license.refresh(), licensed_as(KEY));
    assert_eq!(h.state().lease.as_deref(), Some(fresh.as_str()));

    let h = Harness::empty();
    h.set_state(licensed(
        KEY,
        &h.lease(KEY, "perpetual", 3600, NOW - 2 * DAY),
    ));
    h.http.fail(true);
    h.storage.fail_writes(true);
    assert_eq!(
        h.license.status(false),
        with(
            LicenseState::Expired,
            Some(LicenseReason::Offline),
            Some(KEY)
        )
    );
}

/// The server's trial: started two days ago, ending in five, its lease signed
/// to that end.
fn trial_answer(h: &Harness) {
    let lease = h.lease("trial:fp-test", "trial", 5 * 86_400, NOW);
    h.http.answer(
        "trial",
        200,
        json!({ "trialStart": NOW - 2 * DAY, "trialEndsAt": NOW + 5 * DAY, "lease": lease }),
    );
}

#[test]
fn keeps_a_trial_it_could_not_save_answers_it_and_saves_it_once_it_can() {
    let h = Harness::empty();
    trial_answer(&h);
    h.storage.fail_writes(true);
    let started = h.license.start_trial();
    assert_eq!(
        (started.state, started.trial_days_left),
        (LicenseState::Trial, Some(5))
    );
    assert!(h.storage.get().is_none());
    let status = h.license.status(false);
    assert_eq!(
        (status.state, status.trial_days_left),
        (LicenseState::Trial, Some(5))
    );
    assert!(h.storage.get().is_none());
    h.storage.fail_writes(false);
    assert_eq!(h.license.status(false).trial_days_left, Some(5));
    assert_has(
        &h.state(),
        json!({ "trialStart": NOW - 2 * DAY, "trialEndsAt": NOW + 5 * DAY, "trialAt": NOW }),
    );
    // Saved once: not written again by every status() after it.
    let writes = h.storage.writes();
    h.license.status(false);
    assert_eq!(h.storage.writes(), writes);
}

#[test]
fn never_moves_a_trial_start_later_when_it_saves_the_trial_it_kept() {
    let h = Harness::empty();
    trial_answer(&h);
    h.storage.fail_writes(true);
    h.license.start_trial();
    h.set_state(StoredState {
        trial_start: Some(NOW - 9 * DAY),
        ..StoredState::default()
    });
    h.storage.fail_writes(false);
    h.license.status(false);
    assert_has(
        &h.state(),
        json!({ "trialStart": NOW - 9 * DAY, "trialEndsAt": NOW + 5 * DAY }),
    );
}

/// startTrial() asked at NOW, its answer kept unsaved; then another instance saves `theirs`.
fn kept_then(theirs: impl FnOnce(&Harness) -> StoredState, still_failing: bool) -> Harness {
    let h = Harness::empty();
    trial_answer(&h);
    h.storage.fail_writes(true);
    assert_eq!(h.license.start_trial().trial_days_left, Some(5));
    let theirs = theirs(&h);
    h.set_state(theirs);
    h.storage.fail_writes(still_failing);
    h
}

#[test]
fn a_kept_trial_never_outlives_a_newer_answer_and_takes_an_extension() {
    let shorter = || StoredState {
        trial_start: Some(NOW - 9 * DAY),
        trial_ends_at: Some(NOW - 2 * DAY),
        trial_at: Some(NOW + 1),
        ..StoredState::default()
    };
    let expired = with(LicenseState::Expired, Some(LicenseReason::Trial), None);
    let h = kept_then(|_| shorter(), false);
    assert_eq!(h.license.status(false), expired);
    assert_has(
        &h.state(),
        json!({ "trialStart": NOW - 9 * DAY, "trialEndsAt": NOW - 2 * DAY, "trialAt": NOW + 1 }),
    );
    let h = kept_then(|_| shorter(), true);
    assert_eq!(h.license.status(false), expired);
    // The answer the other instance had came with a lease signed to the new end.
    let h = kept_then(
        |h| StoredState {
            trial_ends_at: Some(NOW + 20 * DAY),
            trial_lease: Some(h.lease("trial:fp-test", "trial", 20 * 86_400, NOW)),
            ..shorter()
        },
        false,
    );
    assert_eq!(h.license.status(false).trial_days_left, Some(20));
    assert_eq!(h.state().trial_ends_at, Some(NOW + 20 * DAY));
    // A stored trial with no stamp loses to it.
    let h = kept_then(
        |_| StoredState {
            trial_at: None,
            ..shorter()
        },
        false,
    );
    assert_eq!(h.license.status(false).trial_days_left, Some(5));
    assert_has(
        &h.state(),
        json!({ "trialStart": NOW - 9 * DAY, "trialEndsAt": NOW + 5 * DAY, "trialAt": NOW }),
    );
}

#[test]
fn a_kept_trial_is_saved_after_a_deactivate_or_another_keys_activation_leaving_the_licence_as_it_was(
) {
    let theirs = [
        StoredState {
            trial_start: Some(NOW - 9 * DAY),
            ..StoredState::default()
        },
        StoredState {
            key: Some("SLUICE-BBBB-BBBB-BBBB-BBBB".into()),
            activation_id: Some("act_2".into()),
            lease: Some("LB".into()),
            checked_major: Some(1),
            ..StoredState::default()
        },
    ];
    for theirs in theirs {
        let h = kept_then(|_| theirs.clone(), false);
        h.license.status(false);
        let stored = h.state();
        assert_eq!(stored.key, theirs.key);
        assert_eq!(stored.lease, theirs.lease);
        assert_eq!(
            stored.trial_start,
            Some(theirs.trial_start.unwrap_or(NOW - 2 * DAY))
        );
        assert_eq!(
            (stored.trial_ends_at, stored.trial_at),
            (Some(NOW + 5 * DAY), Some(NOW))
        );
    }
}
