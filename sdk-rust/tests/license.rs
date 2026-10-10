//! The public API against fake adapters: activate, status, refresh,
//! deactivate, trials, test licences, version ranges and the upgrade link.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use keygrant::lease::OPEN_MAJOR;
use keygrant::offline::{ASK_AGAIN_MS, RETRY_UNANSWERED_MS};
use keygrant::{
    ActivateError, DeactivateResult, Entitlement, Error, HttpResponse, LicenseError, LicenseReason,
    LicenseState, LicenseStatus, StoredState,
};
use serde_json::{json, Value};

fn status(state: LicenseState) -> LicenseStatus {
    LicenseStatus::new(state)
}

fn with(state: LicenseState, reason: Option<LicenseReason>, key: Option<&str>) -> LicenseStatus {
    let mut status = LicenseStatus::new(state);
    status.reason = reason;
    status.key = key.map(str::to_owned);
    status
}

fn licensed_as(key: &str) -> LicenseStatus {
    with(LicenseState::Licensed, None, Some(key))
}

fn activate_answer(h: &Harness, lease: &str) {
    let body = json!({ "lease": lease, "activationId": "act_1" });
    h.http.answer("activate", 200, body);
}

fn error(result: Result<(), ActivateError>) -> Option<LicenseError> {
    result.err().and_then(|e| e.license_error())
}

// --- activate ------------------------------------------------------------------

#[test]
fn activate_cuts_a_device_name_the_server_would_refuse() {
    let h = Harness::new(
        None,
        Opts {
            device_name: Some("D".repeat(500)),
            ..Opts::default()
        },
    );
    activate_answer(&h, &h.lease(KEY, "perpetual", 30 * 86_400, NOW));
    h.license.activate(KEY).expect("activated");
    assert_eq!(
        h.http.body_of("activate")["name"]
            .as_str()
            .map(|n| n.encode_utf16().count()),
        Some(120)
    );
}

#[test]
fn activate_does_not_cut_a_character_in_half() {
    let h = Harness::new(
        None,
        Opts {
            device_name: Some(format!("{}😀", "D".repeat(119))),
            ..Opts::default()
        },
    );
    activate_answer(&h, &h.lease(KEY, "perpetual", 30 * 86_400, NOW));
    h.license.activate(KEY).expect("activated");
    assert_eq!(h.http.body_of("activate")["name"], json!("D".repeat(119)));
}

#[test]
fn activate_leaves_a_name_of_exactly_the_limit_alone_and_one_that_fits_unchanged() {
    for name in ["D".repeat(120), "Rae’s MacBook".to_owned()] {
        let h = Harness::new(
            None,
            Opts {
                device_name: Some(name.clone()),
                ..Opts::default()
            },
        );
        activate_answer(&h, &h.lease(KEY, "perpetual", 30 * 86_400, NOW));
        h.license.activate(KEY).expect("activated");
        assert_eq!(h.http.body_of("activate")["name"], json!(name));
    }
}

#[test]
fn activate_sends_no_name_when_none_is_configured() {
    let h = Harness::empty();
    activate_answer(&h, &h.lease(KEY, "perpetual", 30 * 86_400, NOW));
    h.license.activate(KEY).expect("activated");
    let body = h.http.body_of("activate");
    assert!(body.get("name").is_none());
    assert_eq!(body["fingerprint"], json!("fp-test"));
    assert_eq!(body["major"], json!(1));
    // A custom get() says nothing of what it hashes: no kind is claimed for it.
    assert!(body.get("fingerprintKind").is_none());
}

#[test]
fn activate_refuses_a_key_longer_than_one_could_be_or_empty_without_asking() {
    let h = Harness::empty();
    assert_eq!(
        error(h.license.activate(&"S".repeat(200))),
        Some(LicenseError::InvalidKey)
    );
    assert_eq!(
        error(h.license.activate("   ")),
        Some(LicenseError::InvalidKey)
    );
    assert!(!h.http.called("activate"));
}

#[test]
fn activate_sends_a_key_of_exactly_the_limit_and_does_not_count_the_whitespace_around_it() {
    let key = format!("SLUICE-{}", "A".repeat(113));
    assert_eq!(key.len(), 120);
    let h = Harness::empty();
    activate_answer(&h, &h.lease(&key, "perpetual", 30 * 86_400, NOW));
    h.license.activate(&format!("{key}\n")).expect("activated");
    assert_eq!(h.http.body_of("activate")["key"], json!(key));
}

#[test]
fn activate_normalises_a_pasted_or_lower_case_key_and_stores_the_normalised_one() {
    for entered in [
        "  SLUICE-AAAA-BBBB-CCCC-DDDD\n",
        "sluice-aaaa-bbbb-cccc-dddd",
    ] {
        let h = Harness::empty();
        activate_answer(&h, &h.lease(KEY, "perpetual", 30 * 86_400, NOW));
        h.license.activate(entered).expect("activated");
        assert_eq!(h.http.body_of("activate")["key"], json!(KEY));
        assert_eq!(h.state().key.as_deref(), Some(KEY));
    }
}

#[test]
fn activate_stores_a_verified_lease_and_reports_licensed() {
    let h = Harness::empty();
    activate_answer(&h, &h.lease(KEY, "perpetual", 30 * 86_400, NOW));
    h.license.activate(KEY).expect("activated");
    let stored = h.state();
    assert_eq!(stored.key.as_deref(), Some(KEY));
    assert_eq!(stored.activation_id.as_deref(), Some("act_1"));
    assert_eq!(stored.checked_major, Some(1));
    assert_eq!(stored.verdict_at, Some(NOW));
    assert_eq!(h.license.status(false), licensed_as(KEY));
}

#[test]
fn a_0x_build_runs_as_major_1_as_the_server_reads_it() {
    // Configured as major 0: sent as 1, and a lease for major 1 (its lowest
    // major 1 by default) licenses it rather than refusing it as outdated.
    let h = Harness::new(
        None,
        Opts {
            major: Some(0),
            ..Opts::default()
        },
    );
    activate_answer(&h, &h.lease(KEY, "perpetual", 30 * 86_400, NOW));
    h.license.activate(KEY).expect("activated");
    assert_eq!(h.http.body_of("activate")["major"], json!(1));
    assert_eq!(h.state().checked_major, Some(1));
    h.http.fail(true);
    assert_eq!(h.license.status(false), licensed_as(KEY));
    h.http.fail(false);
    h.validate_with(&h.lease(KEY, "perpetual", 30 * 86_400, NOW));
    assert_eq!(h.license.refresh(), licensed_as(KEY));
    assert_eq!(h.http.body_of("validate")["major"], json!(1));
}

#[test]
fn activate_maps_the_servers_errors() {
    for (status, word, expected) in [
        (404, "invalid-key", LicenseError::InvalidKey),
        (409, "limit", LicenseError::Limit),
        (403, "revoked", LicenseError::Revoked),
        (403, "expired", LicenseError::Revoked),
        (403, "upgrade-required", LicenseError::Upgrade),
        (403, "update-required", LicenseError::Outdated),
        (400, "weird", LicenseError::Network),
        (403, "device", LicenseError::Network),
    ] {
        let h = Harness::empty();
        h.http.answer("activate", status, json!({ "error": word }));
        assert_eq!(
            error(h.license.activate("KEY")),
            Some(expected),
            "{status} {word}"
        );
        assert!(h.storage.get().is_none(), "nothing stored for {word}");
    }
}

#[test]
fn activate_is_network_on_a_transport_failure_or_an_answer_without_a_lease() {
    let h = Harness::empty();
    h.http.fail(true);
    assert_eq!(
        error(h.license.activate("KEY")),
        Some(LicenseError::Network)
    );
    let h = Harness::empty();
    h.http
        .answer("activate", 200, json!({ "activationId": "act_1" }));
    assert_eq!(
        error(h.license.activate("KEY")),
        Some(LicenseError::Network)
    );
    let h = Harness::empty();
    h.http.answer(
        "activate",
        201,
        json!({ "lease": "x", "activationId": "act_1" }),
    );
    assert_eq!(
        error(h.license.activate("KEY")),
        Some(LicenseError::Network)
    );
}

#[test]
fn activate_is_invalid_lease_when_the_lease_fails_offline_verification() {
    let h = Harness::empty();
    h.http.answer(
        "activate",
        200,
        json!({ "lease": "not.a.jwt", "activationId": "act_1" }),
    );
    assert_eq!(
        error(h.license.activate("KEY")),
        Some(LicenseError::InvalidLease)
    );
    // Signed by another key (the embedded public key is not the product's).
    let other = TestKey::new(99);
    let lease = other.lease("KEY", "perpetual", 30 * 86_400, NOW);
    h.http.answer(
        "activate",
        200,
        json!({ "lease": lease, "activationId": "act_1" }),
    );
    assert_eq!(
        error(h.license.activate("KEY")),
        Some(LicenseError::InvalidLease)
    );
    assert!(h.storage.get().is_none());
}

#[test]
fn activate_fails_with_the_storage_error_when_the_state_cannot_be_saved() {
    let h = Harness::empty();
    activate_answer(&h, &h.lease(KEY, "perpetual", 30 * 86_400, NOW));
    h.storage.fail_writes(true);
    match h.license.activate(KEY) {
        Err(ActivateError::Storage(e)) => assert_eq!(e.kind(), std::io::ErrorKind::ResourceBusy),
        other => panic!("expected a storage error, got {other:?}"),
    }
    // Nor when it cannot be read: nothing asked.
    let h = Harness::empty();
    h.storage.fail_reads(true);
    assert!(matches!(
        h.license.activate(KEY),
        Err(ActivateError::Storage(_))
    ));
    assert!(!h.http.called("activate"));
}

#[test]
fn activate_errors_serialise_as_their_words() {
    let error = ActivateError::from(LicenseError::InvalidKey);
    assert_eq!(
        serde_json::to_value(&error).expect("serialisable"),
        json!("invalid-key")
    );
    let storage = ActivateError::Storage(std::io::Error::other("x"));
    assert_eq!(
        serde_json::to_value(&storage).expect("serialisable"),
        json!("storage")
    );
    assert_eq!(
        serde_json::to_value(Error::Busy).expect("serialisable"),
        json!("busy")
    );
}

// --- status (offline-first) ------------------------------------------------------

#[test]
fn status_is_unlicensed_with_no_state() {
    assert_eq!(
        Harness::empty().license.status(false),
        status(LicenseState::Unlicensed)
    );
}

#[test]
fn status_is_licensed_from_a_valid_lease_without_contacting_the_server() {
    let h = Harness::empty();
    h.set_state(licensed(
        "KEY",
        &h.lease("KEY", "perpetual", 30 * 86_400, NOW),
    ));
    assert_eq!(h.license.status(false), licensed_as("KEY"));
    assert!(h.http.calls().is_empty());
}

#[test]
fn status_licenses_a_typed_key_whose_lease_is_a_trial_models_offline_never_as_a_device_trial() {
    let h = Harness::empty();
    let lease = h.lease("KEY-1", "trial", 30 * 86_400, NOW);
    h.set_state(licensed("KEY-1", &lease));
    h.http.fail(true);
    assert_eq!(h.license.status(false), licensed_as("KEY-1"));
    h.license.launch_report_settled();
    assert!(h.http.calls().is_empty());
    assert!(h.state().trial_lease.is_none());
}

#[test]
fn every_answer_parser_ignores_members_it_does_not_know() {
    let extra = json!({ "future": { "nested": [1, 2] }, "note": "added by a later server" });
    let with_extra = |answer: Value| {
        let mut answer = answer;
        for (name, value) in extra.as_object().expect("an object") {
            answer[name] = value.clone();
        }
        answer
    };
    let h = Harness::empty();
    let lease = h.lease("KEY-1", "perpetual", 30 * 86_400, NOW);
    h.http.answer(
        "activate",
        200,
        with_extra(json!({ "lease": lease, "activationId": "act_1" })),
    );
    h.http
        .answer("validate", 200, with_extra(json!({ "lease": lease })));
    h.license.activate("KEY-1").expect("activated");
    assert_eq!(h.license.refresh(), licensed_as("KEY-1"));

    let trial = Harness::empty();
    let trial_lease = trial.lease("trial:fp-test", "trial", 5 * 86_400, NOW);
    trial.http.answer(
        "trial",
        200,
        with_extra(
            json!({ "trialStart": NOW, "trialEndsAt": NOW + 5 * DAY, "lease": trial_lease }),
        ),
    );
    assert_eq!(trial.license.start_trial().state, LicenseState::Trial);
}

#[test]
fn status_renews_online_when_the_lease_has_expired() {
    let h = Harness::empty();
    h.set_state(licensed(
        "KEY",
        &h.lease("KEY", "perpetual", 3600, NOW - 2 * DAY),
    ));
    let fresh = h.lease("KEY", "perpetual", 30 * 86_400, NOW);
    h.validate_with(&fresh);
    assert_eq!(h.license.status(false), licensed_as("KEY"));
    assert_eq!(h.state().lease.as_deref(), Some(fresh.as_str()));
}

#[test]
fn status_stays_expired_offline_when_an_expired_lease_cannot_be_renewed() {
    let h = Harness::empty();
    h.set_state(licensed(
        "KEY",
        &h.lease("KEY", "perpetual", 3600, NOW - 2 * DAY),
    ));
    h.http.fail(true);
    assert_eq!(
        h.license.status(false),
        with(
            LicenseState::Expired,
            Some(LicenseReason::Offline),
            Some("KEY")
        )
    );
}

#[test]
fn status_locks_out_only_on_a_definitive_key_verdict() {
    for word in ["revoked", "expired", "deactivated"] {
        let h = Harness::empty();
        h.set_state(licensed(
            "KEY",
            &h.lease("KEY", "perpetual", 3600, NOW - 2 * DAY),
        ));
        h.http.answer("validate", 403, json!({ "error": word }));
        assert_eq!(
            h.license.status(false),
            with(
                LicenseState::Invalid,
                Some(LicenseReason::Revoked),
                Some("KEY")
            ),
            "{word}"
        );
    }
}

#[test]
fn status_reports_upgrade_and_outdated_from_the_renewals_verdict() {
    for (word, reason) in [
        ("upgrade-required", LicenseReason::Upgrade),
        ("update-required", LicenseReason::Outdated),
    ] {
        let h = Harness::empty();
        h.set_state(licensed(
            "KEY",
            &h.lease("KEY", "perpetual", 3600, NOW - 2 * DAY),
        ));
        h.http.answer("validate", 403, json!({ "error": word }));
        assert_eq!(
            h.license.status(false),
            with(LicenseState::Invalid, Some(reason), Some("KEY"))
        );
    }
}

#[test]
fn status_never_locks_out_on_an_answer_it_cannot_use_a_5xx_a_429_or_an_odd_4xx() {
    let answers = [
        (200, json!({ "lease": "garbage.lease.value" })),
        (200, json!({})),
        (200, json!("not an object")),
        (503, json!({ "error": "unavailable" })),
        (500, json!({ "error": "revoked" })),
        (429, json!({ "error": "rate-limited" })),
        (404, json!({ "error": "unknown-product" })),
        (400, json!({ "error": "bad-request" })),
        (401, json!({ "error": "limit" })),
        (403, json!({ "error": "invalid-key" })),
    ];
    for (code, body) in answers {
        // An expired lease stays expired-offline, never revoked.
        let h = Harness::empty();
        h.set_state(licensed(
            "KEY",
            &h.lease("KEY", "perpetual", 3600, NOW - 2 * DAY),
        ));
        h.http.answer("validate", code, body.clone());
        assert_eq!(
            h.license.status(false),
            with(
                LicenseState::Expired,
                Some(LicenseReason::Offline),
                Some("KEY")
            ),
            "{code} {body}"
        );
        assert!(h.state().refused.is_none());
        // And a valid lease stays licensed, forced or not.
        let h = Harness::empty();
        let lease = h.lease("KEY", "perpetual", 30 * 86_400, NOW);
        h.set_state(licensed("KEY", &lease));
        h.http.answer("validate", code, body.clone());
        assert_eq!(h.license.refresh(), licensed_as("KEY"), "{code} {body}");
        assert_eq!(h.state().lease.as_deref(), Some(lease.as_str()));
        assert!(h.state().refused.is_none());
    }
}

#[test]
fn status_keeps_a_renewed_lease_that_covers_a_lower_major_and_reports_upgrade() {
    let h = Harness::new(
        None,
        Opts {
            major: Some(2),
            ..Opts::default()
        },
    );
    h.set_state(licensed(
        "KEY",
        &h.lease("KEY", "perpetual", 3600, NOW - 2 * DAY),
    ));
    let fresh = h.lease("KEY", "perpetual", 30 * 86_400, NOW);
    h.validate_with(&fresh);
    assert_eq!(
        h.license.status(false),
        with(
            LicenseState::Invalid,
            Some(LicenseReason::Upgrade),
            Some("KEY")
        )
    );
    assert_eq!(h.state().lease.as_deref(), Some(fresh.as_str()));
}

#[test]
fn status_reports_tampered_for_a_lease_the_product_did_not_sign_and_heals_when_the_server_does() {
    let h = Harness::empty();
    let foreign = TestKey::new(42).lease("KEY", "perpetual", 30 * 86_400, NOW);
    h.set_state(licensed("KEY", &foreign));
    h.http.fail(true);
    assert_eq!(
        h.license.status(false),
        with(LicenseState::Invalid, Some(LicenseReason::Tampered), None)
    );
    let h = Harness::empty();
    h.set_state(licensed("KEY", &foreign));
    let fresh = h.lease("KEY", "perpetual", 30 * 86_400, NOW);
    h.validate_with(&fresh);
    assert_eq!(h.license.status(false), licensed_as("KEY"));
    assert_eq!(h.state().lease.as_deref(), Some(fresh.as_str()));
}

#[test]
fn status_rejects_a_validly_signed_lease_for_another_product() {
    let h = Harness::empty();
    let lease = h.key.lease_with(
        NOW,
        30 * 86_400,
        json!({ "sub": "KEY", "product": "other-app" }),
    );
    h.set_state(licensed("KEY", &lease));
    h.http.fail(true);
    assert_eq!(
        h.license.status(false),
        with(LicenseState::Invalid, Some(LicenseReason::Tampered), None)
    );
}

#[test]
fn status_reports_upgrade_offline_when_the_app_major_exceeds_the_lease() {
    let h = Harness::new(
        None,
        Opts {
            major: Some(2),
            ..Opts::default()
        },
    );
    h.set_state(licensed(
        "KEY",
        &h.lease("KEY", "perpetual", 30 * 86_400, NOW),
    ));
    h.http.fail(true);
    assert_eq!(
        h.license.status(false),
        with(
            LicenseState::Invalid,
            Some(LicenseReason::Upgrade),
            Some("KEY")
        )
    );
}

#[test]
fn status_ignores_a_rolled_back_clock() {
    let h = Harness::empty();
    let mut stored = licensed("KEY", &h.lease("KEY", "perpetual", 3600, NOW - 2 * DAY));
    stored.last_seen = Some(NOW + 10 * DAY);
    h.set_state(stored);
    h.set_clock(NOW - 5 * DAY);
    h.http.fail(true);
    assert_eq!(
        h.license.status(false),
        with(
            LicenseState::Expired,
            Some(LicenseReason::Offline),
            Some("KEY")
        )
    );
}

#[test]
fn status_is_unknown_never_a_panic_when_the_state_cannot_be_read() {
    let h = Harness::empty();
    h.storage.fail_reads(true);
    let unknown = with(LicenseState::Unknown, Some(LicenseReason::Storage), None);
    assert_eq!(h.license.status(false), unknown);
    assert_eq!(h.license.refresh(), unknown);
    assert_eq!(h.license.start_trial(), unknown);
    assert!(unknown.permits_use() && !unknown.is_entitled());
    assert!(matches!(h.license.deactivate(), Err(Error::Storage(_))));
    assert!(matches!(
        h.license.upgrade_url("https://buy.test/x"),
        Err(Error::Storage(_))
    ));
    assert!(h.http.calls().is_empty());
    assert_eq!(h.storage.writes(), 0);
}

#[test]
fn status_serialises_as_the_reference_does() {
    let h = Harness::empty();
    h.set_state(licensed(
        "KEY",
        &h.lease("KEY", "perpetual", 3600, NOW - 2 * DAY),
    ));
    h.http.fail(true);
    let value = serde_json::to_value(h.license.status(false)).expect("serialisable");
    assert_eq!(
        value,
        json!({ "state": "expired", "reason": "offline", "key": "KEY" })
    );
}

// --- refresh -----------------------------------------------------------------------

#[test]
fn refresh_renews_a_still_valid_lease() {
    let h = Harness::empty();
    h.set_state(licensed(
        "KEY",
        &h.lease("KEY", "perpetual", 30 * 86_400, NOW),
    ));
    let fresh = h.lease("KEY", "perpetual", 30 * 86_400, NOW + 1000);
    h.validate_with(&fresh);
    assert_eq!(h.license.refresh(), licensed_as("KEY"));
    assert_eq!(h.state().lease.as_deref(), Some(fresh.as_str()));
}

#[test]
fn refresh_keeps_a_valid_lease_licensed_when_the_server_cannot_be_reached() {
    let h = Harness::empty();
    h.set_state(licensed(
        "KEY",
        &h.lease("KEY", "perpetual", 30 * 86_400, NOW),
    ));
    h.http.fail(true);
    assert_eq!(h.license.refresh(), licensed_as("KEY"));
    assert!(h.http.called("validate"));
}

// --- deactivate ------------------------------------------------------------------

const RELEASED: DeactivateResult = DeactivateResult { released: true };
const NOT_RELEASED: DeactivateResult = DeactivateResult { released: false };

#[test]
fn deactivate_releases_the_activation_and_clears_all_but_the_trial_evidence() {
    let h = Harness::empty();
    let mut stored = licensed("KEY", &h.lease("KEY", "perpetual", 30 * 86_400, NOW));
    stored.trial_start = Some(NOW - 9 * DAY);
    stored.trial_ends_at = Some(NOW - 2 * DAY);
    stored.trial_lease = Some("trial-lease".into());
    stored.trial_at = Some(NOW - 9 * DAY);
    stored.kept_major = Some(3);
    stored.device_kind = Some(keygrant::DeviceKind::Machine);
    stored.refused = Some(keygrant::Refusal::Revoked);
    stored.backoff_since = Some(NOW);
    h.set_state(stored);
    h.http.answer("deactivate", 200, json!({ "ok": true }));
    let answer = h.license.deactivate().expect("deactivated");
    assert_eq!(answer, RELEASED);
    // As the Electron SDK's `DeactivateResult`.
    assert_eq!(
        serde_json::to_value(answer).expect("serialisable"),
        json!({ "released": true })
    );
    assert_eq!(
        h.http.body_of("deactivate"),
        json!({ "key": "KEY", "activationId": "act_1" })
    );
    let left = h.state();
    assert_eq!(
        left,
        StoredState {
            trial_start: Some(NOW - 9 * DAY),
            trial_ends_at: Some(NOW - 2 * DAY),
            trial_lease: Some("trial-lease".into()),
            trial_at: Some(NOW - 9 * DAY),
            last_seen: Some(NOW),
            verdict_at: Some(NOW),
            ..StoredState::default()
        }
    );
    assert_eq!(
        h.license.status(false),
        with(LicenseState::Expired, Some(LicenseReason::Trial), None)
    );
}

#[test]
fn deactivate_clears_local_state_even_when_the_server_is_unreachable_and_says_the_seat_may_be_held()
{
    let h = Harness::empty();
    h.set_state(licensed(
        "KEY",
        &h.lease("KEY", "perpetual", 30 * 86_400, NOW),
    ));
    h.http.fail(true);
    assert_eq!(h.license.deactivate().expect("deactivated"), NOT_RELEASED);
    assert!(h.http.called("deactivate"));
    assert!(h.state().key.is_none());
    assert_eq!(h.license.status(false), status(LicenseState::Unlicensed));
}

#[test]
fn deactivate_is_released_only_on_a_200_and_clears_local_state_whatever_the_answer() {
    // A 5xx, a 429, a proxy's page or a refusal is an answer, but not
    // KeyGrant saying the seat is free.
    let answers: [(u16, Option<Value>); 7] = [
        (503, Some(json!({ "error": "unavailable" }))),
        (500, None),
        (429, Some(json!({ "error": "rate-limited" }))),
        (404, None),
        (403, Some(json!({ "error": "revoked" }))),
        (401, Some(json!({ "ok": true }))),
        (204, None),
    ];
    for (code, body) in answers {
        let h = Harness::empty();
        h.set_state(licensed(KEY, &h.lease(KEY, "perpetual", 30 * 86_400, NOW)));
        h.http.on("deactivate", move |_| HttpResponse {
            status: code,
            json: body.clone(),
        });
        assert_eq!(
            h.license.deactivate().expect("deactivated"),
            NOT_RELEASED,
            "{code}"
        );
        let left = h.state();
        assert!(left.key.is_none() && left.activation_id.is_none() && left.lease.is_none());
        assert_eq!(h.license.status(false), status(LicenseState::Unlicensed));
    }
    // A 200 is the release, whatever its body (none at all included).
    let h = Harness::empty();
    h.set_state(licensed(KEY, &h.lease(KEY, "perpetual", 30 * 86_400, NOW)));
    h.http.on("deactivate", |_| HttpResponse {
        status: 200,
        json: None,
    });
    assert_eq!(h.license.deactivate().expect("deactivated"), RELEASED);
    assert!(h.state().key.is_none());
}

#[test]
fn deactivate_asks_nothing_and_is_released_when_there_is_no_activation_to_release() {
    let h = Harness::empty();
    assert_eq!(h.license.deactivate().expect("deactivated"), RELEASED);
    assert!(!h.http.called("deactivate"));
    assert_eq!(h.license.status(false), status(LicenseState::Unlicensed));
    // A key with no activation (an empty one included) holds no seat either.
    for activation_id in [None, Some(String::new())] {
        let h = Harness::empty();
        h.set_state(StoredState {
            key: Some(KEY.into()),
            activation_id,
            last_seen: Some(NOW),
            ..StoredState::default()
        });
        assert_eq!(h.license.deactivate().expect("deactivated"), RELEASED);
        assert!(!h.http.called("deactivate"));
        assert!(h.state().key.is_none());
    }
}

#[test]
fn deactivate_fails_and_keeps_the_key_when_the_state_cannot_be_saved() {
    let h = Harness::empty();
    let lease = h.lease(KEY, "perpetual", 30 * 86_400, NOW);
    h.set_state(licensed(KEY, &lease));
    h.http.answer("deactivate", 200, json!({ "ok": true }));
    h.storage.fail_writes(true);
    assert!(matches!(h.license.deactivate(), Err(Error::Storage(_))));
    // The release was asked for, and answered: the seat may be free already,
    // while this device still holds the key.
    assert_eq!(h.http.count("deactivate"), 1);
    assert_eq!(h.state().key.as_deref(), Some(KEY));
    assert_eq!(h.state().lease.as_deref(), Some(lease.as_str()));
    // Tried again, the release (idempotent at KeyGrant) is answered again,
    // and the key dropped.
    h.storage.fail_writes(false);
    assert_eq!(h.license.deactivate().expect("deactivated"), RELEASED);
    assert_eq!(h.http.count("deactivate"), 2);
    assert!(h.state().key.is_none());
}

// --- trials ------------------------------------------------------------------------

/// The server's trial, its lease signed to the trial's end as KeyGrant signs it.
fn trial_answer(h: &Harness, start: i64, end: i64) {
    let lease = h.lease("trial:fp-test", "trial", (end - NOW) / 1000, NOW);
    h.http.answer(
        "trial",
        200,
        json!({ "trialStart": start, "trialEndsAt": end, "lease": lease }),
    );
}

#[test]
fn start_trial_starts_a_trial_and_stashes_its_start() {
    let h = Harness::empty();
    trial_answer(&h, NOW - 2 * DAY, NOW + 5 * DAY);
    let started = h.license.start_trial();
    assert_eq!(started.state, LicenseState::Trial);
    assert_eq!(started.trial_days_left, Some(5));
    assert_eq!(started.trial_ends_at, Some(NOW + 5 * DAY));
    assert_eq!(h.state().trial_start, Some(NOW - 2 * DAY));
    assert_eq!(h.state().trial_at, Some(NOW));
    assert_eq!(h.registry.get(), Some(NOW - 2 * DAY));
    assert_eq!(h.http.body_of("trial"), json!({ "fingerprint": "fp-test" }));
}

#[test]
fn start_trial_never_moves_the_start_forward_and_sends_the_earliest_it_knows() {
    let h = Harness::with(StoredState {
        trial_start: Some(NOW - 10 * DAY),
        ..StoredState::default()
    });
    h.registry.set(Some(NOW - 8 * DAY));
    trial_answer(&h, NOW - 2 * DAY, NOW + 4 * DAY);
    h.license.start_trial();
    assert_eq!(h.state().trial_start, Some(NOW - 10 * DAY));
    assert_eq!(
        h.http.body_of("trial")["clientStart"],
        json!(NOW - 10 * DAY)
    );
    // The registry's evidence wins when local storage was wiped.
    let h = Harness::empty();
    h.registry.set(Some(NOW - 10 * DAY));
    trial_answer(&h, NOW - 2 * DAY, NOW + 5 * DAY);
    h.license.start_trial();
    assert_eq!(h.state().trial_start, Some(NOW - 10 * DAY));
}

#[test]
fn start_trial_falls_back_to_local_evidence_offline_or_on_any_other_answer() {
    for (fail, code, body) in [
        (true, 200, json!({})),
        (false, 400, json!({ "error": "bad" })),
        (false, 200, json!({ "trialStart": "oops", "lease": "x" })),
    ] {
        let h = Harness::empty();
        let lease = h.lease("trial:fp-test", "trial", 7 * 86_400, NOW);
        h.set_state(StoredState {
            trial_start: Some(NOW - DAY),
            trial_ends_at: Some(NOW + 6 * DAY),
            trial_lease: Some(lease),
            last_seen: Some(NOW),
            ..StoredState::default()
        });
        h.http.fail(fail);
        h.http.answer("trial", code, body);
        assert_eq!(h.license.start_trial().state, LicenseState::Trial);
    }
    // With nothing local and a malformed answer: nothing written.
    let h = Harness::empty();
    h.http
        .answer("trial", 200, json!({ "trialStart": "oops", "lease": "x" }));
    assert_eq!(h.license.start_trial(), status(LicenseState::Unlicensed));
    assert!(h.storage.get().is_none());
}

/// A trial lease for this device (`fp-test`), signed at `iat` (ms) to `ttl` seconds on.
fn trial_lease_from(h: &Harness, iat: i64, ttl: i64) -> String {
    let claim = keygrant::lease::fingerprint_claim("fp-test");
    h.key.lease_with(
        iat,
        ttl,
        json!({ "sub": format!("trial:{claim}"), "aid": claim, "model": "trial", "lim": 1, "major": OPEN_MAJOR }),
    )
}

fn trial_until(end: i64) -> LicenseStatus {
    let mut trial = status(LicenseState::Trial);
    trial.trial_ends_at = Some(end);
    trial.trial_days_left = Some((end - NOW + DAY - 1) / DAY);
    trial
}

#[test]
fn a_trial_runs_only_on_a_valid_trial_lease_to_the_end_it_was_signed_to() {
    let expired = with(LicenseState::Expired, Some(LicenseReason::Trial), None);
    // The stored end is unsigned: edited a year out, it runs no trial without
    // a valid lease, nor beside one that has run out.
    let h = Harness::with(StoredState {
        trial_start: Some(NOW - 2 * DAY),
        trial_ends_at: Some(NOW + 365 * DAY),
        ..StoredState::default()
    });
    assert_eq!(h.license.status(false), expired);
    let h = Harness::empty();
    h.set_state(StoredState {
        trial_start: Some(NOW - 20 * DAY),
        trial_ends_at: Some(NOW + 365 * DAY),
        trial_lease: Some(trial_lease_from(&h, NOW - 20 * DAY, 14 * 86_400)),
        last_seen: Some(NOW),
        ..StoredState::default()
    });
    assert_eq!(h.license.status(false), expired);
    // With a valid lease, the trial ends when the lease says, whatever the stored end.
    let h = Harness::empty();
    h.set_state(StoredState {
        trial_start: Some(NOW - 2 * DAY),
        trial_ends_at: Some(NOW + 365 * DAY),
        trial_lease: Some(trial_lease_from(&h, NOW - 2 * DAY, 7 * 86_400)),
        last_seen: Some(NOW),
        ..StoredState::default()
    });
    assert_eq!(h.license.status(false), trial_until(NOW + 5 * DAY));
    // And with no stored end at all.
    let h = Harness::empty();
    h.set_state(StoredState {
        trial_start: Some(NOW - 2 * DAY),
        trial_lease: Some(trial_lease_from(&h, NOW, 7 * 86_400)),
        last_seen: Some(NOW),
        ..StoredState::default()
    });
    assert_eq!(h.license.status(false), trial_until(NOW + 7 * DAY));
    // A stored end that has passed, and no lease: over.
    let h = Harness::with(StoredState {
        trial_start: Some(NOW - 10 * DAY),
        trial_ends_at: Some(NOW - DAY),
        ..StoredState::default()
    });
    assert_eq!(h.license.status(false), expired);
}

/// A running trial, its lease (`ours`) valid here: started two days ago, ending in five.
fn running_trial(h: &Harness, ours: &str) {
    h.set_state(StoredState {
        trial_start: Some(NOW - 2 * DAY),
        trial_ends_at: Some(NOW + 5 * DAY),
        trial_lease: Some(ours.to_owned()),
        last_seen: Some(NOW),
        ..StoredState::default()
    });
}

#[test]
fn start_trial_keeps_the_trial_lease_it_can_verify_when_the_server_answers_with_one_it_cannot() {
    // The product's key rotated ahead of this build: every answer is signed
    // with a key the build does not carry. Taking it would end the running
    // trial at the next sync.
    let h = Harness::empty();
    let ours = h.lease("trial:fp-test", "trial", 5 * 86_400, NOW);
    running_trial(&h, &ours);
    let rotated = TestKey::new(8).lease("trial:fp-test", "trial", 5 * 86_400, NOW);
    h.http.answer(
        "trial",
        200,
        json!({ "trialStart": NOW - 2 * DAY, "trialEndsAt": NOW + 5 * DAY, "lease": rotated }),
    );
    assert_eq!(h.license.start_trial(), trial_until(NOW + 5 * DAY));
    assert_eq!(h.state().trial_lease.as_deref(), Some(ours.as_str()));
}

#[test]
fn start_trial_takes_a_valid_trial_lease_the_server_answers_with_over_the_valid_one_stored() {
    // An extension, or a lease carrying new entitlements: the server's word
    // wins whenever its lease verifies here.
    let h = Harness::empty();
    let ours = h.lease("trial:fp-test", "trial", 3 * 86_400, NOW);
    h.set_state(StoredState {
        trial_start: Some(NOW - 4 * DAY),
        trial_ends_at: Some(NOW + 3 * DAY),
        trial_lease: Some(ours),
        last_seen: Some(NOW),
        ..StoredState::default()
    });
    let extended = h.key.lease_with(
        NOW,
        6 * 86_400,
        json!({ "sub": "trial:fp-test", "model": "trial", "lim": 1, "ent": { "edition": "pro" } }),
    );
    h.http.answer(
        "trial",
        200,
        json!({ "trialStart": NOW - 4 * DAY, "trialEndsAt": NOW + 6 * DAY, "lease": extended }),
    );
    let mut expected = trial_until(NOW + 6 * DAY);
    expected.entitlements =
        BTreeMap::from([("edition".to_owned(), Entitlement::Text("pro".into()))]);
    assert_eq!(h.license.start_trial(), expected);
    assert_eq!(h.state().trial_lease.as_deref(), Some(extended.as_str()));
}

#[test]
fn start_trial_takes_the_servers_word_when_it_says_the_trial_is_over_whatever_lease_is_held() {
    let h = Harness::empty();
    running_trial(&h, &h.lease("trial:fp-test", "trial", 5 * 86_400, NOW));
    h.http.answer(
        "trial",
        200,
        json!({ "trialStart": NOW - 9 * DAY, "trialEndsAt": NOW - 2 * DAY }),
    );
    assert_eq!(
        h.license.start_trial(),
        with(LicenseState::Expired, Some(LicenseReason::Trial), None)
    );
    assert_eq!(h.state().trial_lease, None);
}

#[test]
fn start_trial_waits_the_activation_bound_for_the_machine_id_not_the_status_bound() {
    // A machine id that does not read in time is waited on for an
    // activation's bound (1.5 s here), not the 100 ms a status() gets. This
    // device could not be told: no trial is asked for, and none is held.
    let gate = Gate::new();
    let bound = Duration::from_millis(1_500);
    let h = Harness::new(
        None,
        Opts {
            fingerprint: Some(hanging(&gate)),
            timings: Some(keygrant::Timings {
                machine_id: Duration::from_millis(100),
                activate_machine_id: bound,
                ..patient_timings()
            }),
            ..Opts::default()
        },
    );
    let started = Instant::now();
    let answered = h.license.start_trial();
    let took = started.elapsed();
    gate.release();
    assert_eq!(answered, status(LicenseState::Unlicensed));
    assert!(took >= bound && took < Duration::from_secs(20), "{took:?}");
    assert!(!h.http.called("trial"));
}

#[test]
fn status_answers_unlicensed_for_a_trial_lease_stored_without_a_start_asking_nothing() {
    // Only a stored start is a trial this device began; no save leaves a lease
    // or an end without one.
    let h = Harness::empty();
    h.set_state(StoredState {
        trial_ends_at: Some(NOW + 5 * DAY),
        trial_lease: Some(h.lease("trial:fp-test", "trial", 5 * 86_400, NOW)),
        last_seen: Some(NOW),
        ..StoredState::default()
    });
    assert_eq!(h.license.status(false), status(LicenseState::Unlicensed));
    assert!(!h.http.called("trial"));
}

// --- a trial whose lease does not verify while its stored end is ahead --------------

/// A trial stored with a lease signed by another key: one the product signed
/// before its key was rotated. `over` changes the state stored.
fn rotated(over: impl FnOnce(StoredState) -> StoredState) -> Harness {
    rotated_with(Opts::default(), over)
}

/// [`rotated`], on a harness made with `opts`.
fn rotated_with(opts: Opts, over: impl FnOnce(StoredState) -> StoredState) -> Harness {
    let h = Harness::new(None, opts);
    let stale = TestKey::new(8).lease("trial:fp-test", "trial", 5 * 86_400, NOW);
    h.set_state(over(StoredState {
        trial_start: Some(NOW - 2 * DAY),
        trial_ends_at: Some(NOW + 5 * DAY),
        trial_lease: Some(stale),
        last_seen: Some(NOW),
        ..StoredState::default()
    }));
    h
}

/// The server's trial: started two days ago, ending in five, with `lease`.
fn answers_trial(h: &Harness, lease: &str) {
    h.http.answer(
        "trial",
        200,
        json!({ "trialStart": NOW - 2 * DAY, "trialEndsAt": NOW + 5 * DAY, "lease": lease }),
    );
}

fn trial_over() -> LicenseStatus {
    with(LicenseState::Expired, Some(LicenseReason::Trial), None)
}

#[test]
fn a_trial_whose_lease_was_signed_before_a_key_rotation_is_asked_for_again_and_the_fresh_lease_runs_it(
) {
    let h = rotated(|state| state);
    let fresh = h.lease("trial:fp-test", "trial", 5 * 86_400, NOW);
    answers_trial(&h, &fresh);
    assert_eq!(h.license.status(false), trial_until(NOW + 5 * DAY));
    // Asked as start_trial() asks, and its answer saved as it saves one.
    assert_eq!(h.http.count("trial"), 1);
    assert_eq!(
        h.http.body_of("trial"),
        json!({ "fingerprint": "fp-test", "clientStart": NOW - 2 * DAY })
    );
    assert_eq!(h.state().trial_lease.as_deref(), Some(fresh.as_str()));
    assert_eq!(h.state().trial_at, Some(NOW));
    // Healed: the next status() needs no ask.
    assert_eq!(h.license.status(false), trial_until(NOW + 5 * DAY));
    assert_eq!(h.http.count("trial"), 1);
}

#[test]
fn a_seven_day_lease_on_a_fourteen_day_trial_is_healed_on_day_eight() {
    let h = Harness::empty();
    // Seven days on a fourteen-day trial: a lease signed short of the trial's
    // end.
    let capped = h.lease("trial:fp-test", "trial", 7 * 86_400, NOW);
    h.set_state(StoredState {
        trial_start: Some(NOW),
        trial_ends_at: Some(NOW + 14 * DAY),
        trial_lease: Some(capped),
        last_seen: Some(NOW),
        ..StoredState::default()
    });
    h.set_clock(NOW + 8 * DAY);
    let fresh = h.lease("trial:fp-test", "trial", 6 * 86_400, NOW + 8 * DAY);
    h.http.answer(
        "trial",
        200,
        json!({ "trialStart": NOW, "trialEndsAt": NOW + 14 * DAY, "lease": fresh }),
    );
    let healed = h.license.status(false);
    assert_eq!(
        (healed.state, healed.trial_ends_at, healed.trial_days_left),
        (LicenseState::Trial, Some(NOW + 14 * DAY), Some(6))
    );
}

#[test]
fn a_trial_that_cannot_be_asked_for_is_over_for_now_and_asked_again_after_a_minute() {
    let h = rotated(|state| state);
    h.http.fail(true);
    assert_eq!(h.license.status(false), trial_over());
    assert_has(
        &h.state(),
        json!({ "backoffSince": NOW, "backoffMs": RETRY_UNANSWERED_MS }),
    );
    // Within the wait, no ask: a status() offline does not pay a timeout each time.
    assert_eq!(h.license.status(false), trial_over());
    assert_eq!(h.http.count("trial"), 1);
    h.set_clock(NOW + RETRY_UNANSWERED_MS);
    assert_eq!(h.license.status(false), trial_over());
    assert_eq!(h.http.count("trial"), 2);
    // refresh() asks whatever the wait says, and heals once the server answers.
    h.http.fail(false);
    answers_trial(&h, &h.lease("trial:fp-test", "trial", 5 * 86_400, NOW));
    assert_eq!(h.license.refresh().state, LicenseState::Trial);
    assert_eq!(h.http.count("trial"), 3);
}

#[test]
fn a_healed_trial_records_no_wait_so_a_later_heal_is_not_held_off() {
    let h = rotated(|state| state);
    answers_trial(&h, &h.lease("trial:fp-test", "trial", 5 * 86_400, NOW));
    assert_eq!(h.license.status(false), trial_until(NOW + 5 * DAY));
    let healed = h.state();
    assert_eq!((healed.backoff_since, healed.backoff_ms), (None, None));
    // The product's key rotated again a minute later: healed at once, not
    // an hour on.
    h.set_state(StoredState {
        trial_lease: Some(TestKey::new(9).lease("trial:fp-test", "trial", 5 * 86_400, NOW)),
        ..healed
    });
    h.set_clock(NOW + 60_000);
    assert_eq!(h.license.status(false).state, LicenseState::Trial);
    assert_eq!(h.http.count("trial"), 2);
}

#[test]
fn a_trials_wait_is_timed_by_the_device_clock_not_the_guard() {
    // The guard (lastSeen) is a day ahead of this device's clock. A wait
    // stamped by the guard would read as asked after a clock set back, and
    // every status() would pay the ask.
    let h = rotated(|state| StoredState {
        last_seen: Some(NOW + DAY),
        ..state
    });
    h.http.fail(true);
    assert_eq!(h.license.status(false), trial_over());
    assert_has(
        &h.state(),
        json!({ "backoffSince": NOW, "backoffMs": RETRY_UNANSWERED_MS }),
    );
    assert_eq!(h.license.status(false), trial_over());
    assert_eq!(h.http.count("trial"), 1);
}

#[test]
fn a_trial_asked_for_again_is_bounded_as_a_status_check_is() {
    // A server that never answers: given up on at the status check's bound
    // (300 ms here), not the minute every other request may take; and, as
    // nothing answered, asked again a minute on.
    let bound = Duration::from_millis(300);
    let opts = Opts {
        timings: Some(keygrant::Timings {
            status_check: bound,
            ..patient_timings()
        }),
        http_timeout: Some(Duration::from_secs(60)),
        ..Opts::default()
    };
    let h = rotated_with(opts, |state| state);
    h.http.hang(true);
    let started = Instant::now();
    assert_eq!(h.license.status(false), trial_over());
    let took = started.elapsed();
    assert!(took >= bound && took < Duration::from_secs(20), "{took:?}");
    assert_has(
        &h.state(),
        json!({ "backoffSince": NOW, "backoffMs": RETRY_UNANSWERED_MS }),
    );
}

/// A fingerprinter whose every read waits at `gate`: a machine id that does
/// not read in time, for a test to wait the bounds out.
fn hanging(gate: &Gate) -> Arc<dyn keygrant::Fingerprinter> {
    let gate = gate.clone();
    Arc::new(FnFingerprinter(
        move || -> Result<String, keygrant::AdapterError> {
            gate.wait();
            Err("released".into())
        },
    ))
}

#[test]
fn a_trial_asked_for_again_reads_the_machine_id_with_a_status_bound_not_the_activation_bound() {
    // The stored lease's check reads the device first, then the ask reads it
    // again: each waited out at a status's bound (200 ms here), not the
    // activation's (30 s). A device that could not be told asks for no trial,
    // and waits a minute.
    let gate = Gate::new();
    let bound = Duration::from_millis(200);
    let opts = Opts {
        fingerprint: Some(hanging(&gate)),
        timings: Some(keygrant::Timings {
            machine_id: bound,
            ..patient_timings()
        }),
        ..Opts::default()
    };
    let h = rotated_with(opts, |state| state);
    let started = Instant::now();
    let answered = h.license.status(false);
    let took = started.elapsed();
    gate.release();
    assert_eq!(answered, trial_over());
    assert!(
        took >= bound * 2 && took < Duration::from_secs(10),
        "{took:?}"
    );
    assert!(!h.http.called("trial"));
    assert_has(
        &h.state(),
        json!({ "backoffSince": NOW, "backoffMs": RETRY_UNANSWERED_MS }),
    );
}

#[test]
fn a_trial_is_not_asked_for_when_its_stored_end_is_exactly_now() {
    let h = rotated(|state| StoredState {
        trial_ends_at: Some(NOW),
        ..state
    });
    assert_eq!(h.license.status(false), trial_over());
    assert!(!h.http.called("trial"));
}

#[test]
fn a_trial_the_server_answers_with_nothing_this_build_can_verify_waits_an_hour() {
    // A build carrying a key the product no longer signs with: the answer
    // cannot verify either (signed with another key this build does not
    // carry, so it is not the stored lease again). Any other answer, a 5xx
    // included, is the same.
    let still_stale = TestKey::new(10).lease("trial:fp-test", "trial", 5 * 86_400, NOW);
    for answer in [Some(still_stale), None] {
        let h = rotated(|state| state);
        let stored = h.state().trial_lease;
        match &answer {
            Some(lease) => answers_trial(&h, lease),
            None => h.http.answer("trial", 503, json!({})),
        }
        assert_eq!(h.license.status(false), trial_over());
        assert_has(
            &h.state(),
            json!({ "backoffSince": NOW, "backoffMs": ASK_AGAIN_MS }),
        );
        if let Some(lease) = &answer {
            // Neither verifies, so nothing is kept over the server's: its
            // lease is the one stored.
            assert_ne!(stored.as_deref(), Some(lease.as_str()));
            assert_eq!(h.state().trial_lease.as_deref(), Some(lease.as_str()));
        }
        h.set_clock(NOW + ASK_AGAIN_MS - 1);
        assert_eq!(h.license.status(false), trial_over());
        assert_eq!(h.http.count("trial"), 1);
    }
}

#[test]
fn a_trial_the_server_says_is_over_is_over_and_its_end_saved() {
    let h = rotated(|state| state);
    h.http.answer(
        "trial",
        200,
        json!({ "trialStart": NOW - 9 * DAY, "trialEndsAt": NOW - 2 * DAY }),
    );
    assert_eq!(h.license.status(false), trial_over());
    assert_has(
        &h.state(),
        json!({ "trialStart": NOW - 9 * DAY, "trialEndsAt": NOW - 2 * DAY }),
    );
    // Over: not asked again, whatever the wait.
    h.set_clock(NOW + 2 * ASK_AGAIN_MS);
    assert_eq!(h.license.status(false), trial_over());
    assert_eq!(h.http.count("trial"), 1);
}

#[test]
fn a_trial_is_not_asked_for_once_its_stored_end_has_passed_or_when_its_lease_verifies() {
    let h = rotated(|state| StoredState {
        trial_start: Some(NOW - 9 * DAY),
        trial_ends_at: Some(NOW - 1),
        ..state
    });
    assert_eq!(h.license.status(false), trial_over());
    assert_eq!(h.license.refresh(), trial_over());
    assert!(!h.http.called("trial"));

    let valid = Harness::empty();
    valid.set_state(StoredState {
        trial_start: Some(NOW - 2 * DAY),
        trial_ends_at: Some(NOW + 5 * DAY),
        trial_lease: Some(valid.lease("trial:fp-test", "trial", 5 * 86_400, NOW)),
        // Synced within the day, so no launch report is due either (launch.rs `report_launch`).
        trial_at: Some(NOW - DAY / 2),
        last_seen: Some(NOW),
        ..StoredState::default()
    });
    assert_eq!(valid.license.status(false).state, LicenseState::Trial);
    assert_eq!(valid.license.refresh().state, LicenseState::Trial);
    assert!(!valid.http.called("trial"));
}

#[test]
fn start_trial_answers_its_own_ask_and_asks_no_more() {
    // Its answer is the server's trial as this build reads it: a lease it
    // cannot verify is a trial over, with no second ask and no wait.
    let h = Harness::empty();
    answers_trial(
        &h,
        &TestKey::new(8).lease("trial:fp-test", "trial", 5 * 86_400, NOW),
    );
    assert_eq!(h.license.start_trial(), trial_over());
    assert_eq!(h.http.count("trial"), 1);
    assert_eq!(h.state().backoff_ms, None);
}

#[test]
fn a_trial_lease_signed_to_the_trials_end_keeps_a_14_day_trial_live_on_day_10_offline() {
    let h = Harness::empty();
    h.set_state(StoredState {
        trial_start: Some(NOW - 10 * DAY),
        trial_ends_at: Some(NOW + 4 * DAY),
        trial_lease: Some(trial_lease_from(&h, NOW - 10 * DAY, 14 * 86_400)),
        trial_at: Some(NOW - 10 * DAY),
        last_seen: Some(NOW - DAY),
        ..StoredState::default()
    });
    h.http.fail(true);
    assert_eq!(h.license.status(false), trial_until(NOW + 4 * DAY));
    // No renewal needed, nor asked for: only the day's launch report was
    // tried, and failed offline, in the background.
    h.license.launch_report_settled();
    assert_eq!(h.http.actions(), ["trial"]);
}

// --- test licences -----------------------------------------------------------------

fn test_lease(h: &Harness, ttl: i64, now: i64) -> String {
    h.key
        .lease_with(now, ttl, json!({ "sub": KEY, "test": true }))
}

#[test]
fn a_test_licence_licenses_and_says_so_even_after_a_renewal_or_offline() {
    let h = Harness::empty();
    activate_answer(&h, &test_lease(&h, 60, NOW));
    h.license.activate(KEY).expect("activated");
    let mut expected = licensed_as(KEY);
    expected.test = true;
    // From an expired lease, so the status can only come from the renewed one.
    let later = NOW + 2 * 3_600_000;
    h.set_clock(later);
    h.validate_with(&test_lease(&h, 30 * 86_400, later));
    assert_eq!(h.license.status(false), expected);
    assert!(h.http.called("validate"));
    h.http.fail(true);
    assert_eq!(h.license.refresh(), expected);
    assert_eq!(
        serde_json::to_value(&expected).expect("serialisable")["test"],
        json!(true)
    );
}

#[test]
fn a_real_licence_says_nothing_about_tests() {
    let h = Harness::empty();
    activate_answer(&h, &h.lease(KEY, "perpetual", 30 * 86_400, NOW));
    h.license.activate(KEY).expect("activated");
    let status = h.license.status(false);
    assert_eq!(status, licensed_as(KEY));
    assert!(serde_json::to_value(&status)
        .expect("serialisable")
        .get("test")
        .is_none());
}

// --- a key for a range of versions -----------------------------------------------------

fn range_lease(h: &Harness, major: i64, min_major: Option<i64>) -> String {
    let mut over = json!({ "sub": "KEY", "major": major });
    if let Some(min) = min_major {
        over["minMajor"] = json!(min);
    }
    h.key.lease_with(NOW, 30 * 86_400, over)
}

#[test]
fn a_range_licenses_its_builds_and_refuses_older_ones_as_outdated() {
    let h = Harness::new(
        None,
        Opts {
            major: Some(1),
            ..Opts::default()
        },
    );
    h.set_state(licensed("KEY", &range_lease(&h, 3, Some(2))));
    h.http.fail(true);
    assert_eq!(
        h.license.status(false),
        with(
            LicenseState::Invalid,
            Some(LicenseReason::Outdated),
            Some("KEY")
        )
    );
    for major in [2, 3] {
        let h = Harness::new(
            None,
            Opts {
                major: Some(major),
                ..Opts::default()
            },
        );
        h.set_state(licensed("KEY", &range_lease(&h, 3, Some(2))));
        assert_eq!(
            h.license.status(false).state,
            LicenseState::Licensed,
            "{major}"
        );
    }
    let h = Harness::new(
        None,
        Opts {
            major: Some(41),
            ..Opts::default()
        },
    );
    h.set_state(licensed("KEY", &range_lease(&h, OPEN_MAJOR, Some(2))));
    assert_eq!(h.license.status(false).state, LicenseState::Licensed);
}

#[test]
fn a_renewed_lease_that_starts_above_this_build_is_kept_and_reported_outdated() {
    let h = Harness::new(
        None,
        Opts {
            major: Some(1),
            ..Opts::default()
        },
    );
    h.set_state(licensed(
        "KEY",
        &h.lease("KEY", "perpetual", 3600, NOW - 2 * DAY),
    ));
    let fresh = range_lease(&h, 2, Some(2));
    h.validate_with(&fresh);
    assert_eq!(
        h.license.status(false),
        with(
            LicenseState::Invalid,
            Some(LicenseReason::Outdated),
            Some("KEY")
        )
    );
    assert_eq!(h.state().lease.as_deref(), Some(fresh.as_str()));
}

// --- upgradeUrl ------------------------------------------------------------------------

#[test]
fn upgrade_url_names_the_activation_never_the_key() {
    let h = Harness::with(StoredState {
        key: Some(KEY.into()),
        activation_id: Some("act_device1".into()),
        ..StoredState::default()
    });
    let url = h
        .license
        .upgrade_url("https://buy.stripe.com/test_abc")
        .expect("a link");
    assert_eq!(
        url.as_deref(),
        Some("https://buy.stripe.com/test_abc?client_reference_id=act_device1")
    );
    let replaced = h
        .license
        .upgrade_url("https://buy.stripe.com/test_abc?client_reference_id=x")
        .expect("a link");
    assert!(!replaced.unwrap_or_default().contains("SLUICE"));
    assert!(matches!(
        h.license.upgrade_url("not a url"),
        Err(Error::InvalidUrl)
    ));
}

#[test]
fn upgrade_url_is_none_without_an_activated_licence() {
    for stored in [
        StoredState {
            trial_start: Some(NOW),
            ..StoredState::default()
        },
        StoredState {
            key: Some("KEY-1".into()),
            ..StoredState::default()
        },
    ] {
        let h = Harness::with(stored);
        assert_eq!(
            h.license
                .upgrade_url("https://buy.stripe.com/test_abc")
                .expect("no failure"),
            None
        );
        assert!(h.http.calls().is_empty());
    }
}

#[test]
fn license_is_send_and_sync() {
    fn shared<T: Send + Sync>() {}
    shared::<keygrant::License>();
    let _shared: Arc<keygrant::License> = Arc::new(Harness::empty().license);
}

// --- configuration ---------------------------------------------------------------------

#[test]
fn configuration_a_developer_must_fix_is_refused_at_construction() {
    use keygrant::{ConfigError, LicenseConfig};
    let key = TestKey::new(7);
    let storage = Arc::new(MemoryStorage::default());
    let make = |product: &str, base: &str, jwk: String| {
        keygrant::License::new(LicenseConfig::new(product, base, jwk).storage(storage.clone()))
    };
    // The JWK, or its `x` alone, with or without a trailing slash on the base.
    assert!(make("sluice", "https://api.test/", key.public_jwk()).is_ok());
    let x = serde_json::from_str::<serde_json::Value>(&key.public_jwk()).expect("JSON")["x"]
        .as_str()
        .expect("x")
        .to_owned();
    assert!(make("sluice", "http://localhost:8787", x.clone()).is_ok());
    for product in ["", "a/b", "a b", "..", "x?y"] {
        assert!(
            matches!(
                make(product, "https://api.test", x.clone()),
                Err(ConfigError::Product(_))
            ),
            "{product:?}"
        );
    }
    for base in ["", "api.keygrant.dev", "ftp://api.test", "https://"] {
        assert!(
            matches!(
                make("sluice", base, x.clone()),
                Err(ConfigError::ApiBaseUrl(_))
            ),
            "{base:?}"
        );
    }
    let private =
        serde_json::json!({ "kty": "OKP", "crv": "Ed25519", "x": x, "d": "secret" }).to_string();
    let other_curve = serde_json::json!({ "kty": "OKP", "crv": "Ed448", "x": x }).to_string();
    for jwk in [
        private,
        other_curve,
        "not a key".to_owned(),
        "{".to_owned(),
        "AAAA".to_owned(),
    ] {
        assert!(
            matches!(
                make("sluice", "https://api.test", jwk.clone()),
                Err(ConfigError::PublicKey(_))
            ),
            "{jwk}"
        );
    }
}

#[test]
fn requests_go_to_the_products_endpoints_under_the_api_base() {
    let h = Harness::empty();
    activate_answer(&h, &h.lease(KEY, "perpetual", 30 * 86_400, NOW));
    h.license.activate(KEY).expect("activated");
    h.license.refresh();
    h.license.deactivate().expect("deactivated");
    h.license.start_trial();
    assert_eq!(
        h.http.actions(),
        ["activate", "validate", "deactivate", "trial"]
    );
    // The base's trailing slash taken off once, as the reference does.
    assert_eq!(
        h.http.urls(),
        [
            "https://api.test/v1/products/sluice/activate",
            "https://api.test/v1/products/sluice/validate",
            "https://api.test/v1/products/sluice/deactivate",
            "https://api.test/v1/products/sluice/trial",
        ]
    );
}

#[test]
fn a_licence_lease_in_the_trials_place_is_not_a_trial() {
    // The server only ever signs a trial lease as `model: trial`.
    let h = Harness::empty();
    let claim = keygrant::lease::fingerprint_claim("fp-test");
    let licence = h.key.lease_with(
        NOW,
        30 * 86_400,
        json!({ "sub": format!("trial:{claim}"), "aid": claim, "model": "perpetual", "major": OPEN_MAJOR }),
    );
    h.set_state(StoredState {
        trial_start: Some(NOW - 30 * DAY),
        trial_lease: Some(licence),
        last_seen: Some(NOW),
        ..StoredState::default()
    });
    assert_eq!(
        h.license.status(false),
        with(LicenseState::Expired, Some(LicenseReason::Trial), None)
    );
}

#[test]
fn an_api_base_read_with_white_space_around_it_is_trimmed() {
    // Read from a file or the environment, a base often ends in a newline. It
    // is trimmed as JavaScript trims: a byte-order mark too (a file saved by
    // Notepad starts with one).
    for base in [
        " https://api.test/\n",
        "\u{FEFF}https://api.test/\u{FEFF}\r\n",
    ] {
        let h = Harness::empty();
        let mut config = h.config.clone();
        config.api_base_url = base.into();
        let license = keygrant::License::new(config).expect("a valid configuration");
        activate_answer(&h, &h.lease(KEY, "perpetual", 30 * 86_400, NOW));
        license.activate(KEY).expect("activated");
        assert_eq!(
            h.http.urls(),
            ["https://api.test/v1/products/sluice/activate"],
            "{base:?}"
        );
    }
    // White space inside it is no URL at all; nor is a next-line (U+0085) at
    // its end, which JavaScript does not trim and no URL holds.
    let h = Harness::empty();
    for base in ["https://api .test", "https://api.test/\u{85}"] {
        let mut config = h.config.clone();
        config.api_base_url = base.into();
        assert!(
            matches!(
                keygrant::License::new(config),
                Err(keygrant::ConfigError::ApiBaseUrl(_))
            ),
            "{base:?}"
        );
    }
}
