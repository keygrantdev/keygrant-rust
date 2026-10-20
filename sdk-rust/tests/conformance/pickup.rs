//! The key-pickup groups, which close `conformance/vectors.json`: what a pickup
//! call sends for what a person typed, the error each refusal of the server is,
//! the keys a confirm hands back as read, and the key a restore activates,
//! answered from each vector's own JSON through the functions the SDK itself
//! decides with, with the conventions of the harness around this module.

use keygrant::pickup::{chosen_key, picked_keys_of, pickup_ask, pickup_refusal, PickedKey};
use serde_json::{json, Map, Value};

use super::{doubles, each, group, int, label, vectors};

/// Every group this module answers, in the file's order.
pub(super) const GROUPS: [&str; 4] = [
    "pickupAsks",
    "pickupRefusals",
    "pickedKeys",
    "pickupChoices",
];

/// How many vectors the groups hold (25, 24, 33 and 18).
const VECTORS: usize = 100;

#[test]
fn the_harness_answers_every_pickup_group_in_the_vectors() {
    let v = vectors();
    let total: usize = GROUPS.iter().map(|name| group(&v, name).len()).sum();
    assert_eq!(total, VECTORS, "the vectors the harness answers");
}

#[test]
fn what_a_person_typed_is_sent_trimmed_or_refused_before_anything_is_asked() {
    each(&vectors(), "pickupAsks", |vector| {
        // The code is null for the ask for a code.
        let email = vector["email"].as_str().expect("the text typed");
        match pickup_ask(email, vector["code"].as_str()) {
            Ok(ask) => {
                let mut send = Map::new();
                send.insert("email".into(), json!(ask.email));
                if let Some(code) = ask.code {
                    send.insert("code".into(), json!(code));
                }
                json!({ "send": Value::Object(send) })
            }
            Err(error) => json!({ "refused": error.as_str() }),
        }
    });
}

#[test]
fn a_refused_call_is_the_error_of_the_servers_word_with_a_wait_only_where_one_helps() {
    each(&vectors(), "pickupRefusals", |vector| {
        let refusal = pickup_refusal(Some(&vector["json"]));
        let mut answer = Map::new();
        answer.insert("error".into(), json!(refusal.error.as_str()));
        if let Some(seconds) = refusal.retry_after {
            answer.insert("retryAfter".into(), json!(seconds));
        }
        Value::Object(answer)
    });
}

#[test]
fn a_confirms_keys_are_read_leniently_and_a_body_that_is_no_answer_is_none() {
    let v = vectors();
    for vector in group(&v, "pickedKeys") {
        // The keys as the SDK serialises them: the entitlements' numbers are doubles.
        let read = picked_keys_of(Some(&vector["json"])).map_or(Value::Null, |keys| {
            serde_json::to_value(keys).expect("keys")
        });
        assert_eq!(
            doubles(&read),
            doubles(&vector["expect"]),
            "pickedKeys: {}",
            label(vector)
        );
    }
}

#[test]
fn the_key_a_restore_activates_is_the_one_held_then_one_with_a_seat_then_the_first() {
    each(&vectors(), "pickupChoices", |vector| {
        let keys: Vec<PickedKey> =
            picked_keys_of(Some(&json!({ "keys": vector["keys"] }))).expect("keys as read");
        // The vectors give the keys as read: reading them again changes none.
        assert_eq!(
            doubles(&serde_json::to_value(&keys).expect("keys")),
            doubles(&vector["keys"]),
            "{}",
            label(vector)
        );
        match chosen_key(&keys, int(&vector["major"]), int(&vector["now"])) {
            Ok(index) => json!({ "index": index }),
            Err(refused) => json!({ "refused": refused.as_str() }),
        }
    });
}
