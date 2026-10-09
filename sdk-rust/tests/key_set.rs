//! A key set: a lease signed by a key the build does not carry, the ids of
//! the keys reported at each check-in; and `has` and `require`, read offline
//! from the stored lease.

mod common;

use common::*;
use keygrant::{
    ActivateError, License, LicenseError, LicenseReason, LicenseState, LicenseStatus,
    MissingEntitlement,
};
use serde_json::{json, Value};

/// The test claims (this product and key, activation `act_1`, 3 seats,
/// major 1, signed at `NOW` for 30 days) granting `ent`.
fn claims(ent: Value) -> Value {
    let iat = NOW / 1000;
    json!({
        "iss": "keygrant", "sub": KEY, "product": PRODUCT, "aid": "act_1", "model": "perpetual",
        "lim": 3, "major": 1, "ent": ent, "iat": iat, "exp": iat + 30 * 86_400,
    })
}

fn harness() -> Harness {
    Harness::new(None, Opts::default())
}

#[test]
fn answers_a_stored_lease_signed_by_a_key_the_build_does_not_carry_as_unknown_key_with_the_key() {
    let h = harness();
    let next = TestKey::new(77);
    h.set_state(licensed(
        KEY,
        &next.sign_named(&claims(json!({})), &next.kid()),
    ));
    h.http.fail(true);
    let mut want = LicenseStatus::new(LicenseState::Invalid);
    want.reason = Some(LicenseReason::UnknownKey);
    want.key = Some(KEY.to_owned());
    assert_eq!(h.license.status(false), want);
}

#[test]
fn verifies_a_stored_lease_naming_the_key_it_carries() {
    let h = harness();
    h.set_state(licensed(
        KEY,
        &h.key.sign_named(&claims(json!({})), &h.key.kid()),
    ));
    assert_eq!(h.license.status(false).state, LicenseState::Licensed);
}

#[test]
fn refuses_an_activation_whose_lease_names_a_key_the_build_does_not_carry() {
    let h = harness();
    let next = TestKey::new(78);
    h.http.answer(
        "activate",
        200,
        json!({ "lease": next.sign_named(&claims(json!({})), &next.kid()), "activationId": "act_1" }),
    );
    assert!(matches!(
        h.license.activate(KEY),
        Err(ActivateError::License(LicenseError::InvalidLease))
    ));
    assert!(h.storage.get().and_then(|s| s.lease).is_none());
}

#[test]
fn activates_on_a_lease_the_second_key_of_its_set_signed_and_reports_both_ids() {
    let h = harness();
    let next = TestKey::new(79);
    let mut config = h.config.clone();
    config.public_jwk = format!(
        r#"{{"keys":[{},{}]}}"#,
        h.key.public_jwk(),
        next.public_jwk()
    );
    let both = License::new(config).expect("a key set of two");
    h.http.answer(
        "activate",
        200,
        json!({ "lease": next.sign_named(&claims(json!({})), &next.kid()), "activationId": "act_1" }),
    );
    both.activate(KEY).expect("activated on the second key");
    assert_eq!(
        h.http.body_of("activate")["kids"],
        json!([h.key.kid(), next.kid()])
    );
    assert_eq!(both.status(false).state, LicenseState::Licensed);
}

#[test]
fn sends_the_ids_of_the_keys_it_carries_with_each_renewal() {
    let h = harness();
    h.set_state(licensed(KEY, &h.key.sign_payload(&claims(json!({})))));
    h.validate_with(&h.key.sign_payload(&claims(json!({}))));
    h.license.refresh();
    assert_eq!(h.http.body_of("validate")["kids"], json!([h.key.kid()]));
}

fn licensed_with(ent: Value) -> Harness {
    let h = harness();
    h.set_state(licensed(KEY, &h.key.sign_payload(&claims(ent))));
    h
}

fn ent() -> Value {
    json!({ "cloud": true, "beta": false, "seats": 0, "devices": 5, "edition": "pro", "blank": "" })
}

#[test]
fn has_answers_from_the_stored_lease_a_flag_on_a_number_not_zero_a_text_not_empty() {
    let h = licensed_with(ent());
    assert!(h.license.has("cloud"));
    assert!(!h.license.has("beta"));
    assert!(!h.license.has("seats"));
    assert!(h.license.has("devices"));
    assert!(h.license.has("edition"));
    assert!(!h.license.has("blank"));
    assert!(!h.license.has("export"));
    assert!(!h.license.has("Cloud"));
}

#[test]
fn has_never_goes_online_and_never_writes() {
    let h = licensed_with(ent());
    // A lease near its end, which status() would renew.
    h.clock.set(NOW + 29 * DAY);
    let writes = h.storage.writes();
    assert!(h.license.has("cloud"));
    assert!(h.http.calls().is_empty());
    assert_eq!(h.storage.writes(), writes);
}

#[test]
fn has_grants_nothing_on_a_lease_that_does_not_license_this_build_nor_unreadable_storage() {
    let h = licensed_with(ent());
    h.clock.set(NOW + 31 * DAY);
    assert!(!h.license.has("cloud"));
    let unreadable = licensed_with(ent());
    unreadable.storage.fail_reads(true);
    assert!(!unreadable.license.has("cloud"));
}

#[test]
fn has_grants_nothing_over_a_stored_refusal_though_the_lease_it_kept_still_verifies() {
    let h = harness();
    let lease = h.key.sign_payload(&claims(ent()));
    // The server refused this build's major: the lease is kept for builds it covers.
    h.set_state(state(json!({
        "key": KEY, "activationId": "act_1", "lease": lease, "checkedMajor": 1, "lastSeen": NOW,
        "refused": "upgrade", "refusedMajor": 1,
    })));
    assert!(!h.license.has("cloud"));
    // Revoked: the refusal stands over any lease still held.
    h.set_state(state(json!({
        "key": KEY, "activationId": "act_1", "lease": lease, "checkedMajor": 1, "lastSeen": NOW,
        "refused": "revoked",
    })));
    assert!(!h.license.has("cloud"));
}

#[test]
fn has_grants_during_a_running_trial_what_its_trial_lease_signs_and_nothing_with_no_licence() {
    let h = harness();
    let lease = h.key.lease_with(
        NOW,
        5 * 86_400,
        json!({ "sub": "trial:fp-test", "model": "trial", "lim": 1, "major": 1_000_000, "ent": { "cloud": true } }),
    );
    h.set_state(state(json!({
        "trialStart": NOW - 2 * DAY, "trialEndsAt": NOW + 5 * DAY, "trialLease": lease, "lastSeen": NOW,
    })));
    assert!(h.license.has("cloud"));
    assert!(!h.license.has("export"));
    assert!(h.http.calls().is_empty());
    h.storage.set(None);
    assert!(!h.license.has("cloud"));
}

#[test]
fn require_is_ok_for_a_granted_code_and_names_the_code_otherwise() {
    let h = licensed_with(ent());
    assert_eq!(h.license.require("cloud"), Ok(()));
    assert_eq!(
        h.license.require("seats"),
        Err(MissingEntitlement {
            entitlement: "seats".to_owned()
        })
    );
}
