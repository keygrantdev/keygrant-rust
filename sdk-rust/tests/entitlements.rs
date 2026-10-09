//! Entitlements: what a licence grants beyond the base product, read from its
//! lease's `ent` claim leniently (a malformed member dropped, never the lease),
//! and carried by a status only when the lease it rests on licenses this build
//! or runs a trial. Through the public API against fake adapters, and straight
//! through the functions the engine decides with (`lease_status`,
//! `trial_lease_of`, `trial_status_of`) and the lease verifier.

mod common;

use std::collections::BTreeMap;
use std::thread;
use std::time::Duration;

use common::*;
use keygrant::lease::{verify_lease, KeySet, LeaseClaims, VerifyFailure, VerifyResult};
use keygrant::offline::{
    lease_status, trial_lease_of, trial_status_of, LeaseCheck, LeaseFailure, TrialLease,
};
use keygrant::{Entitlement, LicenseReason, LicenseState, LicenseStatus, StoredState};
use serde_json::{json, Map, Value};

/// What the leases here grant: a text, a whole number and a flag.
fn pro() -> BTreeMap<String, Entitlement> {
    BTreeMap::from([
        ("edition".to_owned(), Entitlement::Text("pro".into())),
        ("seats".to_owned(), Entitlement::Number(5.0)),
        ("beta".to_owned(), Entitlement::Bool(false)),
    ])
}

/// [`pro`] as a lease signs it.
fn pro_json() -> Value {
    json!({ "edition": "pro", "seats": 5, "beta": false })
}

fn text(value: &str) -> Entitlement {
    Entitlement::Text(value.to_owned())
}

/// A lease from the harness's key over the test claims (this product and key,
/// activation `act_1`, 3 seats, major 1, signed at `now` for 30 days), with
/// `over` laid over them AS GIVEN: an `ent` that is `null`, a list or malformed
/// included, which the server never signs and an SDK must still read.
fn signed_with(key: &TestKey, now: i64, over: Value) -> String {
    let iat = now.div_euclid(1000);
    let mut claims = json!({
        "iss": "keygrant", "sub": KEY, "product": PRODUCT, "aid": "act_1", "model": "perpetual",
        "lim": 3, "major": 1, "iat": iat, "exp": iat + 30 * 86_400,
    });
    let map = claims.as_object_mut().expect("an object");
    if let Value::Object(over) = over {
        map.extend(over);
    }
    key.sign_payload(&claims)
}

fn licensed_with(entitlements: BTreeMap<String, Entitlement>) -> LicenseStatus {
    let mut status = LicenseStatus::new(LicenseState::Licensed);
    status.key = Some(KEY.to_owned());
    status.entitlements = entitlements;
    status
}

fn refused(state: LicenseState, reason: LicenseReason, key: Option<&str>) -> LicenseStatus {
    let mut status = LicenseStatus::new(state);
    status.reason = Some(reason);
    status.key = key.map(str::to_owned);
    status
}

fn activated(h: &Harness, lease: &str) {
    h.http.answer(
        "activate",
        200,
        json!({ "lease": lease, "activationId": "act_1" }),
    );
    h.license.activate(KEY).expect("activated");
}

// --- through the public API --------------------------------------------------------

#[test]
fn hands_the_app_what_the_lease_grants_once_activated() {
    let h = Harness::empty();
    activated(&h, &signed_with(&h.key, NOW, json!({ "ent": pro_json() })));
    let status = h.license.status(false);
    assert_eq!(status, licensed_with(pro()));
    assert_eq!(
        status
            .entitlements
            .get("edition")
            .and_then(Entitlement::as_str),
        Some("pro")
    );
    assert_eq!(
        status
            .entitlements
            .get("seats")
            .and_then(Entitlement::as_f64),
        Some(5.0)
    );
}

#[test]
fn says_nothing_of_them_when_the_lease_grants_none_and_the_status_is_as_it_was() {
    // As `test`: an app comparing whole statuses, or their JSON, sees no change.
    let h = Harness::empty();
    activated(&h, &signed_with(&h.key, NOW, json!({ "ent": {} })));
    let status = h.license.status(false);
    assert_eq!(status, licensed_with(BTreeMap::new()));
    assert!(status.entitlements.is_empty());
    assert_eq!(
        serde_json::to_value(&status).expect("serialisable"),
        json!({ "state": "licensed", "key": KEY })
    );
}

#[test]
fn keeps_the_good_members_of_a_malformed_ent_and_the_licence_it_rides_on() {
    let h = Harness::empty();
    let ent = json!({ "edition": "pro", "nested": { "tier": "gold" }, "none": null, "list": [1], "seats": 2 });
    h.set_state(licensed(
        KEY,
        &signed_with(&h.key, NOW, json!({ "ent": ent })),
    ));
    let kept = BTreeMap::from([
        ("edition".to_owned(), text("pro")),
        ("seats".to_owned(), Entitlement::Number(2.0)),
    ]);
    assert_eq!(h.license.status(false), licensed_with(kept));
    assert!(h.http.calls().is_empty());
}

#[test]
fn reads_an_ent_that_is_not_an_object_as_none_never_as_a_refusal() {
    for ent in [
        json!(["pro"]),
        json!(null),
        json!("pro"),
        json!(42),
        json!(true),
    ] {
        let h = Harness::empty();
        h.set_state(licensed(
            KEY,
            &signed_with(&h.key, NOW, json!({ "ent": ent })),
        ));
        assert_eq!(
            h.license.status(false),
            licensed_with(BTreeMap::new()),
            "{ent}"
        );
    }
}

#[test]
fn takes_them_from_the_renewed_lease_not_the_one_it_replaced() {
    // From an EXPIRED lease, so the status can only come from the renewal.
    let h = Harness::empty();
    let basic = json!({ "ent": { "edition": "basic" }, "exp": NOW / 1000 + 60 });
    activated(&h, &signed_with(&h.key, NOW, basic));
    let later = NOW + 2 * 3_600_000;
    h.set_clock(later);
    h.validate_with(&signed_with(&h.key, later, json!({ "ent": pro_json() })));
    assert_eq!(h.license.status(false), licensed_with(pro()));
    assert!(h.http.called("validate"));
}

#[test]
fn refresh_takes_the_new_leases_when_the_server_answers_and_the_held_leases_when_it_cannot() {
    let h = Harness::empty();
    activated(
        &h,
        &signed_with(&h.key, NOW, json!({ "ent": { "edition": "basic" } })),
    );
    h.validate_with(&signed_with(&h.key, NOW, json!({ "ent": pro_json() })));
    assert_eq!(h.license.refresh(), licensed_with(pro()));
    h.http.fail(true);
    assert_eq!(h.license.refresh(), licensed_with(pro()));
    assert_eq!(h.http.actions(), ["activate", "validate", "validate"]);
}

#[test]
fn carries_a_test_licences_beside_its_test_flag() {
    let h = Harness::empty();
    activated(
        &h,
        &signed_with(&h.key, NOW, json!({ "ent": pro_json(), "test": true })),
    );
    let mut expected = licensed_with(pro());
    expected.test = true;
    assert_eq!(h.license.status(false), expected);
}

#[test]
fn carries_none_on_a_status_that_does_not_license_though_the_lease_grants_them() {
    let h = Harness::new(
        None,
        Opts {
            major: Some(2),
            ..Opts::default()
        },
    );
    h.http.fail(true);
    // A newer build than the lease covers.
    h.set_state(licensed(
        KEY,
        &signed_with(&h.key, NOW, json!({ "ent": pro_json() })),
    ));
    assert_eq!(
        h.license.status(false),
        refused(LicenseState::Invalid, LicenseReason::Upgrade, Some(KEY))
    );
    // A lease past its end, the server out of reach.
    let ended = json!({ "ent": pro_json(), "major": 2, "exp": NOW / 1000 - 1 });
    h.set_state(licensed(KEY, &signed_with(&h.key, NOW - DAY, ended)));
    assert_eq!(
        h.license.status(false),
        refused(LicenseState::Expired, LicenseReason::Offline, Some(KEY))
    );
    // A lease for another key.
    let another = json!({ "ent": pro_json(), "major": 2, "sub": "SLUICE-ZZZZ-ZZZZ-ZZZZ-ZZZZ" });
    h.set_state(licensed(KEY, &signed_with(&h.key, NOW, another)));
    assert_eq!(
        h.license.status(false),
        refused(LicenseState::Invalid, LicenseReason::Tampered, None)
    );
}

#[test]
fn carries_none_on_a_refusal_the_server_gave_though_the_lease_it_keeps_grants_them() {
    // An upgrade refusal keeps the lease (a rollback runs offline on it), so
    // the entitlements are still on the device: the refusal must not carry them.
    let h = Harness::empty();
    activated(&h, &signed_with(&h.key, NOW, json!({ "ent": pro_json() })));
    h.http
        .answer("validate", 403, json!({ "error": "upgrade-required" }));
    let upgrade = refused(LicenseState::Invalid, LicenseReason::Upgrade, Some(KEY));
    assert_eq!(h.license.refresh(), upgrade);
    assert!(h.state().lease.is_some());
    // The refusal as stored, the server out of reach.
    h.http.fail(true);
    assert_eq!(h.license.status(false), upgrade);
}

#[test]
fn carries_none_on_a_stored_refusal_answered_behind_a_call_that_has_not_finished() {
    let h = Harness::new(
        None,
        Opts {
            http_timeout: Some(Duration::from_millis(200)),
            timings: Some(fast_timings()),
            ..Opts::default()
        },
    );
    h.set_state(StoredState {
        refused: Some(keygrant::Refusal::Upgrade),
        refused_major: Some(1),
        ..licensed(KEY, &signed_with(&h.key, NOW, json!({ "ent": pro_json() })))
    });
    let gate = h.storage.hang_next_read();
    let behind = thread::scope(|scope| {
        let _stuck = scope.spawn(|| h.license.status(false));
        gate.await_reached();
        let behind = h.license.status(false);
        gate.release();
        behind
    });
    let mut expected = refused(LicenseState::Invalid, LicenseReason::Upgrade, Some(KEY));
    expected.stalled = true;
    assert_eq!(behind, expected);
}

#[test]
fn keeps_the_held_leases_when_an_early_renewal_gets_no_answer() {
    // Five days left of thirty: due to renew early, and the server is out of reach.
    let h = Harness::empty();
    let over = json!({
        "ent": pro_json(),
        "iat": NOW / 1000 - 25 * 86_400,
        "exp": NOW / 1000 + 5 * 86_400,
    });
    h.set_state(licensed(KEY, &signed_with(&h.key, NOW, over)));
    h.http.fail(true);
    assert_eq!(h.license.status(false), licensed_with(pro()));
    assert_eq!(h.http.actions(), ["validate"]);
}

/// The trial a trial's status says, with what its lease grants.
fn trial_until(end: i64, entitlements: BTreeMap<String, Entitlement>) -> LicenseStatus {
    let mut trial = LicenseStatus::new(LicenseState::Trial);
    trial.trial_ends_at = Some(end);
    trial.trial_days_left = Some((end - NOW + DAY - 1) / DAY);
    trial.entitlements = entitlements;
    trial
}

/// A trial lease (a trial's, held to no machine) granting `pro()`, signed at NOW to `end_secs`.
fn trial_lease(h: &Harness, end_secs: i64) -> String {
    let over =
        json!({ "sub": "trial:fp-test", "model": "trial", "ent": pro_json(), "exp": end_secs });
    signed_with(&h.key, NOW, over)
}

#[test]
fn hands_a_trial_the_trial_leases_from_start_trial_and_every_status_after() {
    let h = Harness::empty();
    let lease = trial_lease(&h, NOW / 1000 + 5 * 86_400);
    h.http.answer(
        "trial",
        200,
        json!({ "trialStart": NOW - 2 * DAY, "trialEndsAt": NOW + 5 * DAY, "lease": lease }),
    );
    let trial = trial_until(NOW + 5 * DAY, pro());
    assert_eq!(h.license.start_trial(), trial);
    h.http.fail(true);
    assert_eq!(h.license.status(false), trial);
}

#[test]
fn gives_a_trial_none_once_its_lease_no_longer_runs_it() {
    let h = Harness::empty();
    h.set_state(StoredState {
        trial_start: Some(NOW - 6 * DAY),
        trial_ends_at: Some(NOW + DAY),
        trial_lease: Some(trial_lease(&h, NOW / 1000 + 86_400)),
        last_seen: Some(NOW),
        ..StoredState::default()
    });
    h.set_clock(NOW + 2 * DAY);
    h.http.fail(true);
    assert_eq!(
        h.license.status(false),
        refused(LicenseState::Expired, LicenseReason::Trial, None)
    );
}

#[test]
fn still_carries_them_behind_a_call_that_has_not_finished_from_the_lease_held() {
    let h = Harness::new(
        None,
        Opts {
            http_timeout: Some(Duration::from_millis(200)),
            timings: Some(fast_timings()),
            ..Opts::default()
        },
    );
    h.set_state(licensed(
        KEY,
        &signed_with(&h.key, NOW, json!({ "ent": pro_json() })),
    ));
    let gate = h.storage.hang_next_read();
    let behind = thread::scope(|scope| {
        let _stuck = scope.spawn(|| h.license.status(false));
        gate.await_reached();
        let behind = h.license.status(false);
        gate.release();
        behind
    });
    let mut expected = licensed_with(pro());
    expected.stalled = true;
    assert_eq!(behind, expected);
}

#[test]
fn cannot_be_changed_through_a_status_which_leaves_the_next_status_as_the_lease_says() {
    // A status owns a copy: whatever the app does to it reaches nothing else.
    let h = Harness::empty();
    h.set_state(licensed(
        KEY,
        &signed_with(&h.key, NOW, json!({ "ent": pro_json() })),
    ));
    let mut first = h.license.status(false);
    first
        .entitlements
        .insert("edition".to_owned(), text("studio"));
    first
        .entitlements
        .insert("seats".to_owned(), Entitlement::Number(500.0));
    assert_eq!(h.license.status(false).entitlements, pro());
}

#[test]
fn a_status_serialises_its_entitlements_as_the_reference_does_and_reads_them_back() {
    // A whole number without a fraction, as JavaScript writes it, so a front
    // end reads the same JSON from this SDK as from the Electron one.
    let mut status = licensed_with(pro());
    status
        .entitlements
        .insert("ratio".to_owned(), Entitlement::Number(0.5));
    let value = serde_json::to_value(&status).expect("serialisable");
    assert_eq!(
        value,
        json!({ "state": "licensed", "key": KEY, "entitlements": { "edition": "pro", "seats": 5, "beta": false, "ratio": 0.5 } })
    );
    let written = serde_json::to_string(&status).expect("serialisable");
    assert!(
        written.contains(r#""seats":5}"#) || written.contains(r#""seats":5,"#),
        "{written}"
    );
    assert_eq!(
        serde_json::from_value::<LicenseStatus>(value).expect("a status"),
        status
    );
}

// --- the value type ---------------------------------------------------------------------

#[test]
fn an_entitlement_reads_as_its_own_kind_and_nothing_else() {
    let flag = Entitlement::Bool(true);
    let number = Entitlement::Number(-2.0);
    let label = text("");
    assert_eq!(
        (flag.as_bool(), flag.as_f64(), flag.as_str()),
        (Some(true), None, None)
    );
    assert_eq!(
        (number.as_bool(), number.as_f64(), number.as_str()),
        (None, Some(-2.0), None)
    );
    assert_eq!(
        (label.as_bool(), label.as_f64(), label.as_str()),
        (None, None, Some(""))
    );
}

#[test]
fn entitlements_compare_by_content_numbers_as_numbers() {
    // 5 and 5.0 are one double; 0 and -0 are equal numbers.
    assert_eq!(Entitlement::Number(5.0), Entitlement::Number(5f64));
    assert_eq!(Entitlement::Number(0.0), Entitlement::Number(-0.0));
    assert_ne!(Entitlement::Number(5.0), Entitlement::Number(5.5));
    // A kind is part of the content: a flag is not the text or number it reads like.
    assert_ne!(Entitlement::Bool(true), text("true"));
    assert_ne!(Entitlement::Bool(true), Entitlement::Number(1.0));
    assert_ne!(text("5"), Entitlement::Number(5.0));
    // Reflexive even for a NaN, which no lease carries, so `Eq` holds.
    assert_eq!(Entitlement::Number(f64::NAN), Entitlement::Number(f64::NAN));
    // Two statuses: equal by their entitlements' content, not by where they came from.
    let mut other = licensed_with(BTreeMap::new());
    other
        .entitlements
        .insert("seats".to_owned(), Entitlement::Number(5.0));
    other.entitlements.insert("edition".to_owned(), text("pro"));
    other
        .entitlements
        .insert("beta".to_owned(), Entitlement::Bool(false));
    assert_eq!(other, licensed_with(pro()));
    other
        .entitlements
        .insert("beta".to_owned(), Entitlement::Bool(true));
    assert_ne!(other, licensed_with(pro()));
}

#[test]
fn an_entitlement_serialises_as_a_bare_json_value_and_reads_back_from_one() {
    let cases = [
        (Entitlement::Bool(false), json!(false)),
        (Entitlement::Number(5.0), json!(5)),
        (Entitlement::Number(-2.0), json!(-2)),
        (Entitlement::Number(0.5), json!(0.5)),
        (text("pro"), json!("pro")),
    ];
    for (entitlement, value) in cases {
        assert_eq!(
            serde_json::to_value(&entitlement).expect("serialisable"),
            value
        );
        assert_eq!(
            serde_json::from_value::<Entitlement>(value).expect("an entitlement"),
            entitlement
        );
    }
    assert_eq!(
        serde_json::from_value::<Entitlement>(json!(5.0)).expect("an entitlement"),
        Entitlement::Number(5.0)
    );
}

// --- the functions the engine decides with ----------------------------------------------

/// A licence lease's claims, granting `ent`.
fn licence_claims(ent: Value) -> LeaseClaims {
    LeaseClaims::from_json(&json!({
        "iss": "keygrant", "sub": KEY, "product": PRODUCT, "aid": "act_1", "model": "perpetual",
        "lim": 3, "major": 1, "ent": ent, "iat": NOW / 1000, "exp": NOW / 1000 + 30 * 86_400,
    }))
    .expect("a lease's claims")
}

#[test]
fn lease_status_licensed_carries_the_leases_entitlements_and_the_checks_test_flag() {
    let check = LeaseCheck::Ok {
        test: false,
        claims: Box::new(licence_claims(pro_json())),
    };
    assert_eq!(lease_status(&check, Some(KEY)), licensed_with(pro()));
    let mut test_claims = licence_claims(pro_json());
    test_claims.test = Some(true);
    let check = LeaseCheck::Ok {
        test: true,
        claims: Box::new(test_claims.clone()),
    };
    let mut expected = licensed_with(pro());
    expected.test = true;
    assert_eq!(lease_status(&check, Some(KEY)), expected);
    // The check is the flag's one source: claims that say otherwise beside it
    // change nothing (`check_lease` never makes such a pair).
    let check = LeaseCheck::Ok {
        test: true,
        claims: Box::new(licence_claims(pro_json())),
    };
    assert_eq!(lease_status(&check, Some(KEY)), expected);
    let check = LeaseCheck::Ok {
        test: false,
        claims: Box::new(test_claims),
    };
    assert_eq!(lease_status(&check, Some(KEY)), licensed_with(pro()));
}

#[test]
fn lease_status_licensed_by_a_lease_granting_none_is_the_status_it_always_was() {
    let check = LeaseCheck::Ok {
        test: false,
        claims: Box::new(licence_claims(json!({}))),
    };
    let status = lease_status(&check, Some(KEY));
    assert_eq!(status, licensed_with(BTreeMap::new()));
    assert_eq!(
        serde_json::to_value(&status).expect("serialisable"),
        json!({ "state": "licensed", "key": KEY })
    );
}

#[test]
fn lease_status_not_licensed_carries_no_entitlements() {
    let cases = [
        (
            LeaseFailure::Expired,
            refused(LicenseState::Expired, LicenseReason::Offline, Some(KEY)),
        ),
        (
            LeaseFailure::Upgrade,
            refused(LicenseState::Invalid, LicenseReason::Upgrade, Some(KEY)),
        ),
        (
            LeaseFailure::Outdated,
            refused(LicenseState::Invalid, LicenseReason::Outdated, Some(KEY)),
        ),
        (
            LeaseFailure::Device,
            refused(LicenseState::Invalid, LicenseReason::Device, Some(KEY)),
        ),
        (
            LeaseFailure::Tampered,
            refused(LicenseState::Invalid, LicenseReason::Tampered, None),
        ),
    ];
    for (failure, expected) in cases {
        assert_eq!(
            lease_status(&LeaseCheck::Fail(failure), Some(KEY)),
            expected,
            "{failure:?}"
        );
    }
}

/// A trial lease's claims for the device `claim-own`, held to its machine, granting `pro()`.
fn trial_claims() -> LeaseClaims {
    let mut trial = licence_claims(pro_json());
    trial.model = keygrant::lease::LicenseModel::Trial;
    trial.aid = "claim-own".to_owned();
    trial.fpk = Some(keygrant::DeviceKind::Machine);
    trial
}

#[test]
fn trial_lease_of_a_valid_trial_lease_is_its_end_and_its_entitlements() {
    let trial = trial_claims();
    let own = ["claim-own".to_owned()];
    assert_eq!(
        trial_lease_of(&VerifyResult::Valid(trial.clone()), PRODUCT, &own, true),
        TrialLease {
            valid: true,
            ends_at: Some(trial.exp * 1000),
            entitlements: pro(),
        }
    );
}

#[test]
fn trial_lease_of_a_lease_not_valid_here_says_nothing_of_its_end_or_entitlements() {
    let own = ["claim-own".to_owned()];
    let other = ["claim-other".to_owned()];
    let expired = VerifyResult::Invalid {
        reason: VerifyFailure::Expired,
        claims: Some(trial_claims()),
    };
    let mut licence = trial_claims();
    licence.model = keygrant::lease::LicenseModel::Perpetual;
    let valid = VerifyResult::Valid(trial_claims());
    let cases: [(&str, &VerifyResult, &str, &[String]); 4] = [
        ("expired", &expired, PRODUCT, &own),
        ("another product's", &valid, "maindeck", &own),
        ("another device's", &valid, PRODUCT, &other),
        ("a licence's", &VerifyResult::Valid(licence), PRODUCT, &own),
    ];
    for (case, verified, product, claims) in cases {
        assert_eq!(
            trial_lease_of(verified, product, claims, true),
            TrialLease::default(),
            "{case}"
        );
    }
}

#[test]
fn trial_status_of_a_running_trial_carries_the_entitlements_given_and_no_other_answer_does() {
    let state = StoredState {
        trial_start: Some(NOW),
        trial_lease: Some("stored".into()),
        ..StoredState::default()
    };
    let end = NOW + 3 * DAY;
    assert_eq!(
        trial_status_of(&state, NOW, true, Some(end), &pro()),
        trial_until(end, pro())
    );
    assert_eq!(
        trial_status_of(&state, NOW, true, Some(end), &BTreeMap::new()),
        trial_until(end, BTreeMap::new())
    );
    assert_eq!(
        trial_status_of(&state, NOW, false, None, &pro()),
        refused(LicenseState::Expired, LicenseReason::Trial, None)
    );
    assert_eq!(
        trial_status_of(&StoredState::default(), NOW, true, Some(end), &pro()),
        LicenseStatus::new(LicenseState::Unlicensed)
    );
}

// --- reading `ent` (the parse rule) ------------------------------------------------------

/// A licence lease's claims as the server would sign them, with `ent` as
/// given (any JSON) or left out (`None`), verified at NOW: valid, and its
/// entitlements as read.
fn verified_ent(ent: Option<Value>) -> BTreeMap<String, Entitlement> {
    verified_ent_text(ent.map(|ent| ent.to_string()).as_deref())
}

/// [`verified_ent`], with `ent` given as the TEXT the payload carries, so
/// its numbers are signed exactly as written.
fn verified_ent_text(ent: Option<&str>) -> BTreeMap<String, Entitlement> {
    let key = TestKey::new(9);
    let (iat, exp) = (NOW / 1000, NOW / 1000 + 3600);
    let ent = ent
        .map(|ent| format!(r#","ent":{ent}"#))
        .unwrap_or_default();
    let payload = format!(
        r#"{{"iss":"keygrant","sub":"{KEY}","product":"{PRODUCT}","aid":"act_1","model":"perpetual","lim":3,"iat":{iat},"exp":{exp}{ent}}}"#
    );
    let token = key.sign_text(&payload);
    let public = KeySet::parse(&key.public_jwk()).expect("a key");
    match verify_lease(&token, &public, NOW) {
        VerifyResult::Valid(claims) => claims.ent,
        other => panic!("expected a valid lease, got {other:?}"),
    }
}

#[test]
fn reading_keeps_every_kind_of_entitlement_as_signed() {
    let ent = json!({ "edition": "pro", "seats": 5, "ratio": 0.5, "discount": -2, "beta": false, "cloud": true, "label": "" });
    let expected = BTreeMap::from([
        ("edition".to_owned(), text("pro")),
        ("seats".to_owned(), Entitlement::Number(5.0)),
        ("ratio".to_owned(), Entitlement::Number(0.5)),
        ("discount".to_owned(), Entitlement::Number(-2.0)),
        ("beta".to_owned(), Entitlement::Bool(false)),
        ("cloud".to_owned(), Entitlement::Bool(true)),
        ("label".to_owned(), text("")),
    ]);
    assert_eq!(verified_ent(Some(ent)), expected);
}

#[test]
fn reading_drops_a_malformed_member_and_keeps_the_rest_and_the_lease() {
    // A signed lease is the server's word: one bad member must not take the
    // licence away, nor the good members beside it.
    let ent = json!({ "nested": { "tier": "gold" }, "edition": "pro", "none": null, "list": ["a"], "seats": 2 });
    let expected = BTreeMap::from([
        ("edition".to_owned(), text("pro")),
        ("seats".to_owned(), Entitlement::Number(2.0)),
    ]);
    assert_eq!(verified_ent(Some(ent)), expected);
    assert!(verified_ent(Some(json!({ "nested": {}, "none": null, "list": [] }))).is_empty());
}

#[test]
fn reading_takes_anything_but_an_object_as_none_never_as_bad_claims() {
    let shapes = [
        None,
        Some(json!(null)),
        Some(json!([])),
        Some(json!(["pro", true])),
        Some(json!("pro")),
        Some(json!(42)),
        Some(json!(true)),
    ];
    for ent in shapes {
        assert!(verified_ent(ent.clone()).is_empty(), "{ent:?}");
    }
}

#[test]
fn reading_keeps_names_as_signed_beyond_ascii_and_proto_included() {
    // Past U+FFFF too: a character two UTF-16 units long, in a name and a text.
    let ent =
        json!({ "édition": "Pro Studio ✓", "版本": "专业版", "__proto__": "pro", "🎵": "Pro 𝄞" });
    let expected = BTreeMap::from([
        ("édition".to_owned(), text("Pro Studio ✓")),
        ("版本".to_owned(), text("专业版")),
        ("__proto__".to_owned(), text("pro")),
        ("🎵".to_owned(), text("Pro 𝄞")),
    ]);
    assert_eq!(verified_ent(Some(ent)), expected);
}

#[test]
fn reading_enforces_none_of_the_caps_the_server_holds_entitlements_to() {
    // More than 16 entries, a name past 32 characters, a text past 64: the
    // signature is what is trusted.
    let mut ent = Map::new();
    let mut expected = BTreeMap::new();
    for i in 0..20 {
        ent.insert(format!("feature{i}"), json!(i));
        expected.insert(format!("feature{i}"), Entitlement::Number(f64::from(i)));
    }
    ent.insert("n".repeat(40), json!(true));
    expected.insert("n".repeat(40), Entitlement::Bool(true));
    ent.insert("notes".to_owned(), json!("t".repeat(100)));
    expected.insert("notes".to_owned(), text(&"t".repeat(100)));
    assert_eq!(verified_ent(Some(Value::Object(ent))), expected);
}

#[test]
fn reading_takes_each_number_as_the_nearest_double_as_javascript_does() {
    // Numbers are read correctly rounded. serde_json's default reader lands one
    // double off on each of these, which is why the crate reads with its
    // `float_roundtrip` feature. Rust's own literals and `str::parse` are
    // correctly rounded, so they say what each must be. Signed as text, the
    // last in the integer form the reference writes.
    let ent = verified_ent_text(Some(
        r#"{"ratio":221.62324973788606,"share":975.1952520418383,"total":123456789012345680000}"#,
    ));
    let read = |name: &str| {
        ent.get(name)
            .and_then(Entitlement::as_f64)
            .map(f64::to_bits)
    };
    assert_eq!(read("ratio"), Some(221.623_249_737_886_06_f64.to_bits()));
    assert_eq!(read("share"), Some(975.195_252_041_838_3_f64.to_bits()));
    let total: f64 = "123456789012345680000".parse().expect("a double");
    assert_eq!(read("total"), Some(total.to_bits()));
    // And not the double next to it, which a reader rounding otherwise gives.
    assert_ne!(read("ratio"), Some(221.623_249_737_886_08_f64.to_bits()));
}
