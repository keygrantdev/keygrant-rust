//! The user-lock groups: what an activation sends for the address and code a
//! person typed (or the error that refuses them first), and the error each
//! word of a refused activation gives, answered from each vector's own JSON
//! with the conventions of the harness around this module.

use keygrant::wire::{map_server_error, typed_for};
use serde_json::{json, Map, Value};

use super::{each, vectors};

#[test]
fn typed_for_sends_what_was_typed_or_refuses_it() {
    each(&vectors(), "typedFor", |vector| {
        // An option that was not given is `null` in the file.
        match typed_for(vector["email"].as_str(), vector["code"].as_str()) {
            Ok(typed) => {
                let mut send = Map::new();
                if let Some(email) = typed.email {
                    send.insert("email".into(), json!(email));
                }
                if let Some(code) = typed.code {
                    send.insert("code".into(), json!(code));
                }
                json!({ "send": Value::Object(send) })
            }
            Err(error) => json!({ "refused": error.as_str() }),
        }
    });
}

#[test]
fn the_words_of_a_user_lock_are_their_own_errors_and_any_other_is_network() {
    // Only a string error word can match: `null` and `42` are not one.
    each(&vectors(), "userLockErrors", |vector| {
        json!(map_server_error(vector["error"].as_str()).as_str())
    });
}
