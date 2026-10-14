//! Licence-file test support: Ada's offline file and key file signed as the
//! server signs them, a file's text, the statuses a file gives, and the
//! harness's app rebuilt with a denylist.

use ed25519_dalek::Signer;
use keygrant::{
    License, LicenseReason, LicenseSource, LicenseState, LicenseStatus, LicensedTo, StoredState,
};
use serde_json::{json, Value};

use super::{b64url, Harness, TestKey, KEY, NOW, PRODUCT};

/// Ada, who the test licence files are licensed to.
pub const ADA_NAME: &str = "Ada Lovelace";
pub const ADA_EMAIL: &str = "ada@example.com";

impl TestKey {
    /// A licence file's token as the server signs one: the file header naming
    /// this key, and Ada's OFFLINE file's claims (`lf_1`, this product's
    /// `KEY`, majors 1 to 3, issued at NOW) with `over` laid over them
    /// (`null` removes a claim).
    pub fn offline_file(&self, over: Value) -> String {
        let claims = json!({
            "iss": "keygrant", "kind": "offline", "fid": "lf_1", "sub": KEY, "product": PRODUCT,
            "name": ADA_NAME, "email": ADA_EMAIL, "major": 3, "ent": {}, "iat": NOW / 1000,
        });
        self.license_file_with(claims, over)
    }

    /// Ada's KEY file (`lf_2`, this product's `KEY` in lower case, her email
    /// alone), with `over` laid over it.
    pub fn key_file(&self, over: Value) -> String {
        let claims = json!({
            "iss": "keygrant", "kind": "key", "fid": "lf_2", "sub": KEY.to_lowercase(),
            "product": PRODUCT, "email": ADA_EMAIL, "iat": NOW / 1000,
        });
        self.license_file_with(claims, over)
    }

    fn license_file_with(&self, mut claims: Value, over: Value) -> String {
        let map = claims.as_object_mut().expect("an object");
        if let Value::Object(over) = over {
            for (name, value) in over {
                if value.is_null() {
                    map.remove(&name);
                } else {
                    map.insert(name, value);
                }
            }
        }
        let header = format!(
            r#"{{"alg":"EdDSA","typ":"kglicense+jwt","kid":"{}"}}"#,
            self.kid()
        );
        let input = format!(
            "{}.{}",
            b64url(header.as_bytes()),
            b64url(claims.to_string().as_bytes())
        );
        let signature = self.signing.sign(input.as_bytes());
        format!("{input}.{}", b64url(&signature.to_bytes()))
    }
}

/// A licence file's text as the server writes one: notes for a person, then
/// the token on a line of its own, CRLF as a mail gateway may leave it.
pub fn license_file_text(token: &str) -> String {
    format!("# KeyGrant license file\r\n# Product: {PRODUCT}\r\n\r\n{token}\r\n")
}

/// `token` with its payload swapped for `other`'s, under its own header and
/// signature: a file edited after it was signed.
pub fn payload_swapped(token: &str, other: &str) -> String {
    let parts: Vec<&str> = token.split('.').collect();
    let payload = other.split('.').nth(1).expect("a payload");
    format!("{}.{payload}.{}", parts[0], parts[2])
}

/// Ada, named and addressed, as her offline file signs her.
pub fn ada() -> LicensedTo {
    LicensedTo {
        name: Some(ADA_NAME.into()),
        email: Some(ADA_EMAIL.into()),
    }
}

/// What `status()` gives for Ada's offline file.
pub fn file_licensed() -> LicenseStatus {
    let mut status = LicenseStatus::new(LicenseState::Licensed);
    status.key = Some(KEY.into());
    status.source = Some(LicenseSource::File);
    status.licensed_to = Some(ada());
    status
}

/// A stored file's status that does not license this build, with whose it is.
pub fn file_invalid(reason: LicenseReason) -> LicenseStatus {
    let mut status = file_licensed();
    status.state = LicenseState::Invalid;
    status.reason = Some(reason);
    status
}

/// A stored file's status with nothing of the file read.
pub fn file_unread(reason: LicenseReason) -> LicenseStatus {
    let mut status = LicenseStatus::new(LicenseState::Invalid);
    status.reason = Some(reason);
    status.source = Some(LicenseSource::File);
    status
}

/// KEY licensed by its lease, as `status()` gives it.
pub fn key_licensed() -> LicenseStatus {
    let mut status = LicenseStatus::new(LicenseState::Licensed);
    status.key = Some(KEY.into());
    status
}

/// The harness's app as major 2, its storage, clock and server kept, with
/// `revoked` as its denylist.
pub fn file_app(h: &Harness, revoked: &[&str]) -> License {
    let config = h
        .config
        .clone()
        .major(2)
        .revoked_license_files(revoked.iter().copied());
    License::new(config).expect("a valid configuration")
}

/// A licence lease for KEY on act_1, covering majors 1 to 3, for thirty days.
pub fn lease_to_3(h: &Harness) -> String {
    h.key.lease_with(NOW, 30 * 86_400, json!({ "major": 3 }))
}

/// A state holding `token` as its licence file, and nothing else.
pub fn stored_file(token: &str) -> StoredState {
    StoredState {
        license_file: Some(token.into()),
        ..StoredState::default()
    }
}
