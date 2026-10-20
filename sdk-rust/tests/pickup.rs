//! Key pickup: what `pick_up` and `pick_up_with_code` send and answer, and the
//! key `restore_by_email` activates. The rules each piece follows are held to the
//! conformance vectors (`pickupAsks`, `pickupRefusals`, `pickedKeys`,
//! `pickupChoices`); these are the calls around them.

mod common;

use std::thread;
use std::time::Duration;

use common::*;
use keygrant::pickup::{
    chosen_key, covers, picked_keys_of, pickup_ask, pickup_refusal, PICKUP_KEYS_KEEP_MS,
};
use keygrant::{
    LicenseError, LicenseState, PickedKey, PickedVersions, PickupError, PickupRefusal, RestoreError,
};
use serde_json::{json, Value};

const MONTH: i64 = 30 * 86_400;
const ADDRESS: &str = "Mara@Studio.example";

/// A key as a confirm's answer gives it, with what a test leaves out filled in.
fn picked(over: Value) -> Value {
    let mut key = json!({
        "key": KEY, "model": "perpetual", "modelName": "Single", "state": "active",
        "versions": { "minMajor": 1, "maxMajor": 1, "upgradeUntil": null },
        "deviceLimit": 1, "activeDevices": 0, "held": false,
    });
    for (name, value) in over.as_object().expect("an object") {
        key[name] = value.clone();
    }
    key
}

fn sent(h: &Harness) {
    h.http.answer("pickup", 200, json!({ "ok": true }));
}

fn keys(h: &Harness, keys: Vec<Value>) {
    h.http.answer("confirm", 200, json!({ "keys": keys }));
}

fn activated(h: &Harness) {
    h.http.answer(
        "activate",
        200,
        json!({ "lease": h.lease(KEY, "perpetual", MONTH, NOW), "activationId": "act_1" }),
    );
}

fn refusal(error: PickupError) -> Result<(), PickupRefusal> {
    Err(PickupRefusal::of(error))
}

fn word(result: Result<(), RestoreError>) -> Option<&'static str> {
    result.err().map(|e| e.code())
}

// --- pick_up(email) ---------------------------------------------------------------------

#[test]
fn pick_up_asks_for_a_code_with_the_address_trimmed_and_answers_sent() {
    let h = Harness::empty();
    sent(&h);
    assert_eq!(h.license.pick_up("  Mara@Studio.example\n"), Ok(()));
    assert_eq!(h.http.body_of("pickup"), json!({ "email": ADDRESS }));
    assert!(h.http.urls()[0].ends_with("/v1/products/sluice/pickup"));
}

#[test]
fn pick_up_refuses_a_blank_address_before_it_asks_and_answers_each_refusal_as_its_error() {
    let h = Harness::empty();
    assert_eq!(h.license.pick_up("   "), refusal(PickupError::InvalidEmail));
    assert_eq!(
        h.license.pick_up(&format!("{}@b.example", "a".repeat(254))),
        refusal(PickupError::InvalidEmail)
    );
    assert_eq!(h.http.count("pickup"), 0);

    h.http
        .answer("pickup", 403, json!({ "error": "pickup-disabled" }));
    assert_eq!(
        h.license.pick_up(ADDRESS),
        refusal(PickupError::PickupDisabled)
    );
    h.http.answer(
        "pickup",
        429,
        json!({ "error": "rate-limited", "retryAfter": 120 }),
    );
    assert_eq!(
        h.license.pick_up(ADDRESS),
        Err(PickupRefusal {
            error: PickupError::TooManyAttempts,
            retry_after: Some(120)
        })
    );
    h.http.fail(true);
    assert_eq!(h.license.pick_up(ADDRESS), refusal(PickupError::Network));
}

#[test]
fn pick_up_reads_the_wait_from_the_body_never_a_header_or_the_status() {
    let h = Harness::empty();
    h.http.answer(
        "pickup",
        503,
        json!({ "error": "unavailable", "retryAfter": 60 }),
    );
    assert_eq!(
        h.license.pick_up(ADDRESS),
        Err(PickupRefusal {
            error: PickupError::TryAgain,
            retry_after: Some(60)
        })
    );
    // The word decides, never the status.
    h.http
        .answer("pickup", 502, json!({ "error": "wrong-code" }));
    assert_eq!(h.license.pick_up(ADDRESS), refusal(PickupError::WrongCode));
    h.http
        .answer("pickup", 404, json!({ "error": "unknown-product" }));
    assert_eq!(h.license.pick_up(ADDRESS), refusal(PickupError::Network));
}

#[test]
fn pick_up_writes_nothing() {
    let h = Harness::empty();
    sent(&h);
    h.license.pick_up(ADDRESS).expect("sent");
    assert_eq!(h.storage.writes(), 0);
    assert!(h.storage.get().is_none());
}

// --- pick_up_with_code ------------------------------------------------------------------

#[test]
fn pick_up_with_code_sends_the_address_the_code_and_this_devices_fingerprints_and_answers_the_keys_as_read(
) {
    let h = Harness::empty();
    keys(
        &h,
        vec![
            json!({ "key": KEY, "model": "perpetual", "modelName": "Single", "state": "active", "held": true }),
        ],
    );
    let got = h
        .license
        .pick_up_with_code(ADDRESS, " 123 456 ")
        .expect("keys");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].key, KEY);
    assert_eq!(got[0].model_name.as_deref(), Some("Single"));
    assert!(got[0].held);
    let body = h.http.body_of("confirm");
    assert_eq!(body["email"], json!(ADDRESS));
    assert_eq!(body["code"], json!("123456"));
    assert_eq!(body["fingerprints"], json!(["fp-test"]));
    assert!(h.http.urls()[0].ends_with("/v1/products/sluice/pickup/confirm"));
}

#[test]
fn pick_up_with_code_sends_no_fingerprints_for_a_product_whose_devices_are_not_held() {
    let h = Harness::new(
        None,
        Opts {
            device_hold: Some(false),
            ..Opts::default()
        },
    );
    keys(&h, vec![]);
    assert_eq!(h.license.pick_up_with_code(ADDRESS, "123456"), Ok(vec![]));
    assert!(h.http.body_of("confirm").get("fingerprints").is_none());
}

#[test]
fn pick_up_with_code_refuses_a_code_that_is_not_six_digits_before_it_asks_costing_the_address_no_try(
) {
    let h = Harness::empty();
    for code in ["12345", "", "   ", "12345a", "１２３４５６"] {
        assert_eq!(
            h.license.pick_up_with_code(ADDRESS, code),
            Err(PickupRefusal::of(PickupError::WrongCode)),
            "{code:?}"
        );
    }
    // The address is judged first.
    assert_eq!(
        h.license.pick_up_with_code("", "12"),
        Err(PickupRefusal::of(PickupError::InvalidEmail))
    );
    assert_eq!(h.http.count("confirm"), 0);
}

#[test]
fn pick_up_with_code_answers_the_servers_words_and_a_200_that_is_no_pickup_answer_as_network() {
    let h = Harness::empty();
    for (status, word, error) in [
        (403, "wrong-code", PickupError::WrongCode),
        (403, "code-expired", PickupError::CodeExpired),
        (403, "pickup-disabled", PickupError::PickupDisabled),
        (503, "try-again", PickupError::TryAgain),
        (400, "invalid-request", PickupError::InvalidEmail),
    ] {
        h.http.answer("confirm", status, json!({ "error": word }));
        assert_eq!(
            h.license.pick_up_with_code(ADDRESS, "123456"),
            Err(PickupRefusal::of(error)),
            "{word}"
        );
    }
    h.http.answer(
        "confirm",
        429,
        json!({ "error": "too-many-attempts", "retryAfter": 3000 }),
    );
    assert_eq!(
        h.license.pick_up_with_code(ADDRESS, "123456"),
        Err(PickupRefusal {
            error: PickupError::TooManyAttempts,
            retry_after: Some(3000)
        })
    );
    for body in [json!({ "ok": true }), json!({ "keys": "no" }), json!([])] {
        h.http.answer("confirm", 200, body);
        assert_eq!(
            h.license.pick_up_with_code(ADDRESS, "123456"),
            Err(PickupRefusal::of(PickupError::Network))
        );
    }
    h.http.fail(true);
    assert_eq!(
        h.license.pick_up_with_code(ADDRESS, "123456"),
        Err(PickupRefusal::of(PickupError::Network))
    );
}

#[test]
fn pick_up_with_code_writes_nothing() {
    let h = Harness::empty();
    keys(&h, vec![picked(json!({}))]);
    h.license
        .pick_up_with_code(ADDRESS, "123456")
        .expect("keys");
    assert_eq!(h.storage.writes(), 0);
    assert!(h.storage.get().is_none());
}

#[test]
fn behind_a_stuck_call_the_pickups_still_answer_and_a_restore_is_network_asking_nothing() {
    let h = Harness::new(
        None,
        Opts {
            http_timeout: Some(Duration::from_millis(100)),
            timings: Some(fast_timings()),
            ..Opts::default()
        },
    );
    h.set_state(licensed(KEY, &h.lease(KEY, "perpetual", MONTH, NOW)));
    h.http
        .answer("validate", 403, json!({ "error": "revoked" }));
    sent(&h);
    keys(&h, vec![]);
    let gate = h.storage.hold_next_write();
    thread::scope(|scope| {
        let _stuck = scope.spawn(|| h.license.refresh());
        gate.await_reached();
        assert_eq!(h.license.pick_up(ADDRESS), Ok(()));
        assert_eq!(h.license.pick_up_with_code(ADDRESS, "123456"), Ok(vec![]));
        assert_eq!(
            word(h.license.restore_by_email(ADDRESS, "123456")),
            Some("network")
        );
        // The restore, behind the stuck call, asked nothing.
        assert_eq!(h.http.count("confirm"), 1);
        assert_eq!(h.storage.writes(), 1);
        gate.release();
    });
}

// --- restore_by_email -------------------------------------------------------------------

#[test]
fn restore_confirms_then_activates_the_key_that_fits_with_the_address_and_stores_the_activation_never_the_code(
) {
    let h = Harness::empty();
    keys(&h, vec![picked(json!({}))]);
    activated(&h);
    h.license
        .restore_by_email(ADDRESS, "123456")
        .expect("restored");
    let body = h.http.body_of("activate");
    assert_eq!(body["key"], json!(KEY));
    assert_eq!(body["email"], json!(ADDRESS));
    assert!(body.get("code").is_none(), "{body}");
    let stored = h.state().to_json().to_string();
    assert!(stored.contains(KEY), "{stored}");
    let lowered = stored.to_lowercase();
    assert!(
        !lowered.contains("studio") && !lowered.contains("123456"),
        "{stored}"
    );
    assert_eq!(h.license.status(false).state, LicenseState::Licensed);
    // The confirm went first, then the activation.
    assert_eq!(h.http.actions(), ["confirm", "activate"]);
}

#[test]
fn restore_activates_the_address_as_the_pickup_sent_it_trimmed() {
    let h = Harness::empty();
    keys(&h, vec![picked(json!({}))]);
    activated(&h);
    h.license
        .restore_by_email("  Mara@Studio.example\n", " 123 456 ")
        .expect("restored");
    assert_eq!(h.http.body_of("confirm")["email"], json!(ADDRESS));
    assert_eq!(h.http.body_of("confirm")["code"], json!("123456"));
    assert_eq!(h.http.body_of("activate")["email"], json!(ADDRESS));
}

#[test]
fn restore_activates_a_code_locked_key_with_the_code_the_pickup_handed_back() {
    let h = Harness::empty();
    keys(&h, vec![picked(json!({ "activationCode": "654321" }))]);
    activated(&h);
    h.license
        .restore_by_email(ADDRESS, "123456")
        .expect("restored");
    let body = h.http.body_of("activate");
    assert_eq!(body["key"], json!(KEY));
    assert_eq!(body["email"], json!(ADDRESS));
    assert_eq!(body["code"], json!("654321"));
    // The pickup's own code is not the lock's, and neither is stored.
    let stored = h.state().to_json().to_string();
    assert!(
        !stored.contains("654321") && !stored.contains("123456"),
        "{stored}"
    );
}

#[test]
fn restore_chooses_the_key_this_device_holds_then_one_with_a_seat() {
    let h = Harness::empty();
    keys(
        &h,
        vec![
            picked(json!({ "key": "SLUICE-AAAA-AAAA-AAAA-AAAA", "activeDevices": 1 })),
            picked(json!({ "key": "SLUICE-BBBB-BBBB-BBBB-BBBB" })),
            picked(
                json!({ "key": "SLUICE-CCCC-CCCC-CCCC-CCCC", "activeDevices": 1, "held": true }),
            ),
        ],
    );
    h.http.answer("activate", 409, json!({ "error": "limit" }));
    let _ = h.license.restore_by_email(ADDRESS, "123456");
    assert_eq!(
        h.http.body_of("activate")["key"],
        json!("SLUICE-CCCC-CCCC-CCCC-CCCC")
    );
    keys(
        &h,
        vec![
            picked(json!({ "key": "SLUICE-AAAA-AAAA-AAAA-AAAA", "activeDevices": 1 })),
            picked(json!({ "key": "SLUICE-BBBB-BBBB-BBBB-BBBB" })),
        ],
    );
    let _ = h.license.restore_by_email(ADDRESS, "654321");
    assert_eq!(
        h.http.last_body_of("activate")["key"],
        json!("SLUICE-BBBB-BBBB-BBBB-BBBB")
    );
}

#[test]
fn restore_answers_no_key_for_an_address_with_none_and_upgrade_or_outdated_activating_nothing() {
    let h = Harness::new(
        None,
        Opts {
            major: Some(2),
            ..Opts::default()
        },
    );
    keys(&h, vec![]);
    assert_eq!(
        word(h.license.restore_by_email(ADDRESS, "123456")),
        Some("no-key")
    );
    let err = h
        .license
        .restore_by_email(ADDRESS, "123456")
        .expect_err("no key");
    assert!(matches!(err, RestoreError::NoKey));
    // Keys for an older major: buy an upgrade.
    keys(&h, vec![picked(json!({}))]);
    let err = h
        .license
        .restore_by_email(ADDRESS, "222222")
        .expect_err("upgrade");
    assert!(matches!(err, RestoreError::License(LicenseError::Upgrade)));
    // Keys that start at a major after this build: update the app.
    keys(
        &h,
        vec![picked(
            json!({ "versions": { "minMajor": 3, "maxMajor": null, "upgradeUntil": null } }),
        )],
    );
    assert_eq!(
        word(h.license.restore_by_email(ADDRESS, "333333")),
        Some("outdated")
    );
    assert_eq!(h.http.count("activate"), 0);
    assert!(h.storage.get().is_none());
}

#[test]
fn restore_keeps_the_confirmed_keys_in_memory_so_a_retry_or_a_restore_after_pick_up_asks_once() {
    let h = Harness::empty();
    keys(&h, vec![picked(json!({}))]);
    h.http
        .answer("activate", 503, json!({ "error": "unavailable" }));
    assert_eq!(
        word(h.license.restore_by_email(ADDRESS, "123456")),
        Some("network")
    );
    activated(&h);
    h.license
        .restore_by_email(ADDRESS, " 123 456 ")
        .expect("restored");
    assert_eq!(h.http.count("confirm"), 1);
    // Done with once activated: the same pair asks again.
    h.license
        .pick_up_with_code(ADDRESS, "123456")
        .expect("keys");
    assert_eq!(h.http.count("confirm"), 2);
    // Kept in memory only: nothing of them is stored.
    let stored = h.state().to_json().to_string();
    assert!(!stored.contains("123456"), "{stored}");
}

#[test]
fn pick_up_with_code_asks_again_with_another_code_and_once_the_keys_are_ten_minutes_old() {
    let h = Harness::empty();
    keys(&h, vec![]);
    h.license
        .pick_up_with_code(ADDRESS, "123456")
        .expect("keys");
    h.license
        .pick_up_with_code(ADDRESS, "654321")
        .expect("keys");
    assert_eq!(h.http.count("confirm"), 2);
    h.license
        .pick_up_with_code(ADDRESS, "654321")
        .expect("keys");
    assert_eq!(h.http.count("confirm"), 2);
    h.set_clock(NOW + PICKUP_KEYS_KEEP_MS);
    h.license
        .pick_up_with_code(ADDRESS, "654321")
        .expect("keys");
    assert_eq!(h.http.count("confirm"), 3);
    // Another address asks again too.
    h.license
        .pick_up_with_code("other@studio.example", "654321")
        .expect("keys");
    assert_eq!(h.http.count("confirm"), 4);
}

#[test]
fn restore_leaves_the_licence_this_device_holds_as_it_was_when_it_fails() {
    let h = Harness::empty();
    activated(&h);
    h.license.activate(KEY).expect("activated");
    let before = h.state().to_json().to_string();
    keys(
        &h,
        vec![picked(json!({ "key": "SLUICE-OTHR-BBBB-CCCC-DDDD" }))],
    );
    h.http.answer("activate", 409, json!({ "error": "limit" }));
    assert_eq!(
        word(h.license.restore_by_email(ADDRESS, "123456")),
        Some("limit")
    );
    h.http.fail(true);
    assert_eq!(
        word(h.license.restore_by_email(ADDRESS, "654321")),
        Some("network")
    );
    assert_eq!(h.state().to_json().to_string(), before);
}

#[test]
fn restore_answers_a_pickup_refusal_and_an_activations_as_they_come() {
    let h = Harness::empty();
    h.http
        .answer("confirm", 403, json!({ "error": "code-expired" }));
    assert_eq!(
        word(h.license.restore_by_email(ADDRESS, "123456")),
        Some("code-expired")
    );
    h.http.answer(
        "confirm",
        429,
        json!({ "error": "too-many-attempts", "retryAfter": 3600 }),
    );
    let err = h
        .license
        .restore_by_email(ADDRESS, "123456")
        .expect_err("refused");
    assert_eq!(err.code(), "too-many-attempts");
    assert_eq!(err.retry_after(), Some(3600));
    assert_eq!(
        serde_json::to_value(&err).expect("serialises"),
        json!({ "error": "too-many-attempts", "retryAfter": 3600 })
    );
    // The typed refusals ask nothing.
    assert_eq!(
        word(h.license.restore_by_email("", "123456")),
        Some("invalid-email")
    );
    assert_eq!(
        word(h.license.restore_by_email(ADDRESS, "12")),
        Some("wrong-code")
    );
    assert_eq!(h.http.count("confirm"), 2);

    keys(&h, vec![picked(json!({}))]);
    h.http.answer("activate", 409, json!({ "error": "limit" }));
    assert_eq!(
        word(h.license.restore_by_email(ADDRESS, "123456")),
        Some("limit")
    );
    h.http
        .answer("activate", 403, json!({ "error": "user-code-invalid" }));
    assert_eq!(
        word(h.license.restore_by_email(ADDRESS, "123456")),
        Some("user-code-invalid")
    );
    h.http.fail(true);
    assert_eq!(
        word(h.license.restore_by_email(ADDRESS, "123456")),
        Some("network")
    );
}

#[test]
fn restore_never_takes_a_licence_away() {
    let h = Harness::with(licensed(KEY, &h_lease()));
    let before = h.storage.get();
    h.http
        .answer("confirm", 403, json!({ "error": "wrong-code" }));
    assert_eq!(
        word(h.license.restore_by_email(ADDRESS, "123456")),
        Some("wrong-code")
    );
    keys(&h, vec![]);
    assert_eq!(
        word(h.license.restore_by_email(ADDRESS, "123456")),
        Some("no-key")
    );
    assert_eq!(h.storage.get(), before);
    assert_eq!(h.license.status(false).state, LicenseState::Licensed);
}

fn h_lease() -> String {
    TestKey::new(7).lease(KEY, "perpetual", MONTH, NOW)
}

// --- the pieces -------------------------------------------------------------------------

#[test]
fn the_pieces_ask_for_a_code_with_an_address_alone_and_confirm_with_a_code() {
    let ask = pickup_ask(ADDRESS, None).expect("ask");
    assert_eq!((ask.email.as_str(), ask.code), (ADDRESS, None));
    let ask = pickup_ask(ADDRESS, Some("123456")).expect("ask");
    assert_eq!(ask.code.as_deref(), Some("123456"));
    assert_eq!(
        pickup_ask("", Some("123456")),
        Err(PickupError::InvalidEmail)
    );
}

#[test]
fn a_refusal_is_read_leniently() {
    assert_eq!(
        pickup_refusal(None),
        PickupRefusal::of(PickupError::Network)
    );
    assert_eq!(
        pickup_refusal(Some(&json!({ "error": "unavailable", "retryAfter": 60 }))),
        PickupRefusal {
            error: PickupError::TryAgain,
            retry_after: Some(60)
        }
    );
    assert_eq!(
        pickup_refusal(Some(&json!({ "error": "wrong-code", "retryAfter": 60 }))),
        PickupRefusal::of(PickupError::WrongCode)
    );
    assert_eq!(
        pickup_refusal(Some(
            &json!({ "error": "too-many-attempts", "retryAfter": 1.5 })
        )),
        PickupRefusal::of(PickupError::TooManyAttempts)
    );
    // Serialises as the reference's result.
    assert_eq!(
        serde_json::to_value(pickup_refusal(Some(
            &json!({ "error": "try-again", "retryAfter": 5 })
        )))
        .expect("serialises"),
        json!({ "error": "try-again", "retryAfter": 5 })
    );
}

#[test]
fn a_confirms_keys_are_read_leniently_and_none_from_what_is_no_answer() {
    assert_eq!(picked_keys_of(Some(&json!({ "keys": "no" }))), None);
    assert_eq!(picked_keys_of(Some(&json!([]))), None);
    assert_eq!(picked_keys_of(None), None);
    let read = picked_keys_of(Some(&json!({
        "keys": [{ "key": "" }, 7, { "key": KEY, "held": "yes", "activationCode": "12345" }]
    })))
    .expect("keys");
    assert_eq!(read.len(), 1);
    assert_eq!(
        serde_json::to_value(&read[0]).expect("serialises"),
        json!({
            "key": KEY, "model": "", "modelName": null, "state": "", "entitlements": {},
            "expiresAt": null, "versions": null, "deviceLimit": 0, "activeDevices": 0, "held": false
        })
    );
}

#[test]
fn a_keys_versions_are_judged_as_the_server_does() {
    let v = |min, max, until| PickedVersions {
        min_major: min,
        max_major: max,
        upgrade_until: until,
    };
    assert!(covers(None, 9, NOW));
    assert!(!covers(Some(&v(2, Some(3), None)), 1, NOW));
    assert!(!covers(Some(&v(2, Some(3), None)), 4, NOW));
    assert!(covers(Some(&v(2, Some(3), Some(NOW + 1))), 4, NOW));
    assert!(!covers(Some(&v(2, Some(3), Some(NOW))), 4, NOW));
    assert!(covers(Some(&v(2, None, None)), 40, NOW));
}

#[test]
fn the_pieces_choose_the_key_held_then_one_with_a_free_seat_then_the_first() {
    let read = |over: Value| -> PickedKey {
        picked_keys_of(Some(&json!({ "keys": [picked(over)] })))
            .expect("keys")
            .remove(0)
    };
    let full = read(json!({ "key": "A", "activeDevices": 1 }));
    let free = read(json!({ "key": "B" }));
    let held = read(json!({ "key": "C", "activeDevices": 1, "held": true }));
    assert_eq!(
        chosen_key(&[full.clone(), free.clone(), held], 1, NOW),
        Ok(2)
    );
    assert_eq!(chosen_key(&[full.clone(), free], 1, NOW), Ok(1));
    let also_full = read(json!({ "key": "D", "activeDevices": 1 }));
    assert_eq!(chosen_key(&[full, also_full], 1, NOW), Ok(0));
    let later =
        read(json!({ "versions": { "minMajor": 3, "maxMajor": null, "upgradeUntil": null } }));
    assert_eq!(
        chosen_key(&[later], 1, NOW).map_err(|e| e.as_str()),
        Err("outdated")
    );
}
