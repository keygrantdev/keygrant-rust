//! The licence-file groups: a file's container read for its token, a file's
//! token verified, judged and given as a status, and a file given where a
//! lease goes, answered from each vector's own JSON with the conventions of
//! the harness around this module.

use keygrant::lease::verify_lease;
use keygrant::license_file::{
    check_license_file, license_file_status, license_file_token, verify_license_file,
};
use keygrant::offline::trial_lease_valid;
use serde_json::json;

use super::keys::key_set;
use super::{checked, doubles, each, group, int, key, label, status_json, strings, text, vectors};

/// A vector's bytes: canonical base64url, no padding, as the file writes them.
fn bytes_of(b64: &str) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = Vec::new();
    let (mut bits, mut held) = (0u32, 0u32);
    for c in b64.bytes() {
        let index = ALPHABET
            .iter()
            .position(|a| *a == c)
            .unwrap_or_else(|| panic!("bytes: {c} is not base64url"));
        bits = (bits << 6) | u32::try_from(index).expect("a 6-bit index");
        held += 6;
        if held >= 8 {
            held -= 8;
            out.push(u8::try_from((bits >> held) & 0xff).expect("a byte"));
        }
    }
    out
}

#[test]
fn license_file_containers() {
    each(&vectors(), "licenseFileContainers", |vector| {
        json!(license_file_token(&bytes_of(text(
            &vector["bytes"],
            "bytes"
        ))))
    });
}

#[test]
fn license_files() {
    for vector in group(&vectors(), "licenseFiles") {
        let name = label(vector);
        let verified =
            verify_license_file(text(&vector["token"], "token"), &key_set(&vector["keys"]));
        assert_eq!(
            doubles(&verified.to_json()),
            doubles(&vector["verify"]),
            "{name}: verify"
        );
        let check = check_license_file(
            &verified,
            text(&vector["product"], "product"),
            int(&vector["major"]),
            &strings(&vector["revoked"]),
        );
        assert_eq!(check.to_json(), vector["expect"], "{name}: expect");
        assert_eq!(
            doubles(&status_json(&license_file_status(&check))),
            doubles(&vector["status"]),
            "{name}: status"
        );
    }
}

#[test]
fn files_as_leases() {
    // A file given where a lease goes: refused by the lease verifier as it
    // is, and never a trial lease.
    let v = vectors();
    let key = key(&v);
    for vector in group(&v, "filesAsLeases") {
        let name = label(vector);
        let verified = verify_lease(text(&vector["token"], "token"), &key, int(&vector["now"]));
        assert_eq!(verified.to_json(), vector["verify"], "{name}: verify");
        let expect = match checked(vector, &verified) {
            keygrant::offline::LeaseCheck::Ok { test, .. } => json!({ "ok": true, "test": test }),
            keygrant::offline::LeaseCheck::Fail(failure) => {
                json!({ "ok": false, "reason": failure.as_str() })
            }
        };
        assert_eq!(expect, vector["expect"], "{name}: expect");
        let holder = &vector["holder"];
        let trial = trial_lease_valid(
            &verified,
            text(&vector["product"], "product"),
            &strings(&holder["claims"]),
            holder["canTell"].as_bool().expect("holder.canTell"),
        );
        assert_eq!(json!(trial), vector["trial"], "{name}: trial");
    }
}
