//! The key-set groups: a key's id, a lease verified against a key set, and
//! whether entitlements grant a code, answered from each vector's own JSON
//! with the conventions of the harness around this module.

use keygrant::grants;
use keygrant::lease::{entitlements_of, verify_lease, KeySet, PublicKey};
use serde_json::{json, Value};

use super::{each, int, text, vectors};

/// A vector's keys, as a key set.
fn key_set(keys: &Value) -> KeySet {
    let keys = keys
        .as_array()
        .expect("keys")
        .iter()
        .map(|jwk| PublicKey::from_jwk(jwk).expect("a public JWK"));
    KeySet::from_keys(keys).expect("a key set")
}

#[test]
fn key_ids() {
    each(&vectors(), "keyIds", |vector| {
        // null for a JWK the SDK refuses.
        json!(PublicKey::from_jwk(&vector["jwk"])
            .ok()
            .map(|key| key.kid()))
    });
}

#[test]
fn key_sets() {
    for vector in super::group(&vectors(), "keySets") {
        let name = super::label(vector);
        let set = key_set(&vector["keys"]);
        assert_eq!(json!(set.kids()), vector["kids"], "{name}: kids");
        let verified = verify_lease(text(&vector["token"], "token"), &set, int(&vector["now"]));
        assert_eq!(
            super::doubles(&verified.to_json()),
            super::doubles(&vector["verify"]),
            "{name}: verify"
        );
    }
}

#[test]
fn grants_a_code() {
    // As a lease's `ent` is read: leniently (`entitlements_of`).
    each(&vectors(), "grants", |vector| {
        let ent = entitlements_of(&vector["ent"]);
        json!(grants(&ent, text(&vector["code"], "code")))
    });
}

#[test]
fn a_key_set_reads_a_jwk_set_a_list_one_jwk_or_an_x() {
    let v = vectors();
    let one = &v["publicJwk"];
    let x = one["x"].as_str().expect("x");
    let kid = PublicKey::from_jwk(one).expect("the key").kid();
    for text in [
        json!({ "keys": [one] }).to_string(),
        json!([one, one]).to_string(),
        one.to_string(),
        x.to_owned(),
    ] {
        assert_eq!(
            KeySet::parse(&text).expect("a key set").kids(),
            std::slice::from_ref(&kid),
            "{text}"
        );
    }
    assert!(KeySet::parse(r#"{"keys":[]}"#).is_err());
    assert!(KeySet::parse("[]").is_err());
    assert!(KeySet::parse("{\"keys\":[{\"kty\":\"EC\"}]}").is_err());
    // Two keys, in their order.
    let other = &v["keySets"][0]["keys"][1];
    let other_kid = PublicKey::from_jwk(other).expect("the second key").kid();
    assert_ne!(other_kid, kid);
    assert_eq!(
        KeySet::parse(&json!({ "keys": [one, other] }).to_string())
            .expect("a key set of two")
            .kids(),
        vec![kid.clone(), other_kid]
    );
    // A JWK set whose keys are not a list is refused, never read as one JWK.
    let mut odd = one.clone();
    odd["keys"] = json!(one.to_string());
    assert!(KeySet::parse(&odd.to_string()).is_err());
    // A bare x is read as a JWK's: canonical once its padding is dropped.
    assert!(KeySet::parse(&format!("{x}=")).is_ok());
    assert!(KeySet::parse(&x.replace('_', "/")).is_err());
}
