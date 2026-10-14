//! Licence files: a signed token a buyer is sent as a file (`.kglicense`),
//! which the app takes by drag-and-drop or an open dialog and verifies
//! offline. One format, two kinds ([`LicenseFileKind`]):
//!
//! - a KEY file carries the licence key, and the app activates online with it
//!   exactly as with a typed key (seats, revocation and refunds as usual);
//! - an OFFLINE file is a lease that never expires and is bound to no device,
//!   for machines that never go online. It has no seat limit and cannot be
//!   revoked: a leaked file works until an app update lists its id as revoked
//!   ([`LicenseConfig::revoked_license_files`](crate::LicenseConfig::revoked_license_files)).
//!
//! The token is a compact JWS signed with the product's Ed25519 key, as a
//! lease is, but it is never taken for one, either way: its header is exactly
//! `{"alg":"EdDSA","typ":"kglicense+jwt","kid":"<kid>"}`, which is part of what
//! is signed, and its claims carry none of the `aid`, `lim`, `exp` and `model`
//! every lease reader requires.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::lease::{b64url_decode_canonical, decode_utf8, named_kid, KeySet};
use crate::status::{LicenseReason, LicenseSource, LicenseState, LicenseStatus, LicensedTo};

mod claims;
mod container;

use claims::parse_file_claims;
pub use claims::{LicenseFileClaims, LicenseFileKind, OfflineTerms};
pub use container::{license_file_token, MAX_LICENSE_FILE_BYTES};

/// A licence file's extension, for an open dialog's filter or a drop handler.
pub const LICENSE_FILE_EXTENSION: &str = ".kglicense";

/// The media type of a licence file attached to an email: a vendor type, so a
/// mail client hands it over as a download, byte for byte.
pub const LICENSE_FILE_MEDIA_TYPE: &str = "application/vnd.keygrant.license";

/// A licence file's header, up to its key id, as the server writes it.
const FILE_HEADER_START: &[u8] = br#"{"alg":"EdDSA","typ":"kglicense+jwt","kid":""#;

/// Why a licence file's token did not verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileVerifyFailure {
    /// Not three parts, or a signature that is not canonical base64url.
    Malformed,
    /// Its header is not a licence file's (a lease, or any other token).
    NotALicenseFile,
    /// It names a key (`kid`) the key set does not hold.
    UnknownKey,
    /// Not signed by the key it names.
    BadSignature,
    /// Signed, but its claims are not a licence file's.
    BadClaims,
}

impl FileVerifyFailure {
    /// The reference's word for it.
    pub fn as_str(self) -> &'static str {
        match self {
            FileVerifyFailure::Malformed => "malformed",
            FileVerifyFailure::NotALicenseFile => "not-a-license-file",
            FileVerifyFailure::UnknownKey => "unknown-key",
            FileVerifyFailure::BadSignature => "bad-signature",
            FileVerifyFailure::BadClaims => "bad-claims",
        }
    }
}

/// What verifying a licence file's token gave.
#[derive(Debug, Clone, PartialEq)]
pub enum FileVerifyResult {
    /// Signed by the key it names, and well formed.
    Valid(Box<LicenseFileClaims>),
    /// Not valid.
    Invalid(FileVerifyFailure),
}

impl FileVerifyResult {
    /// What verifying said, as the conformance vectors write it: `{valid, reason?, claims?}`.
    pub fn to_json(&self) -> Value {
        match self {
            FileVerifyResult::Valid(claims) => {
                serde_json::json!({ "valid": true, "claims": claims.to_json() })
            }
            FileVerifyResult::Invalid(reason) => {
                serde_json::json!({ "valid": false, "reason": reason.as_str() })
            }
        }
    }
}

/// Verify a licence file's token offline against a key set, in this order:
/// three parts (`Malformed`); a licence file's header, matched as bytes
/// (`NotALicenseFile`); the signature canonical base64url (`Malformed`); the
/// key the header names in the set (`UnknownKey`); a signature of 64 bytes
/// verified by THAT key alone (`BadSignature`: no other key of the set is
/// tried); the payload canonical base64url of JSON matching the claims
/// (`BadClaims`). It never expires, so there is no time to judge it at.
pub fn verify_license_file(token: &str, keys: &KeySet) -> FileVerifyResult {
    let invalid = FileVerifyResult::Invalid;
    let parts: Vec<&str> = token.split('.').collect();
    let [header, payload, signature] = parts.as_slice() else {
        return invalid(FileVerifyFailure::Malformed);
    };
    let Some(kid) = named_kid(header, FILE_HEADER_START) else {
        return invalid(FileVerifyFailure::NotALicenseFile);
    };
    let Some(signature) = b64url_decode_canonical(signature) else {
        return invalid(FileVerifyFailure::Malformed);
    };
    let Some(key) = keys.named(&kid) else {
        return invalid(FileVerifyFailure::UnknownKey);
    };
    if !key.verifies(format!("{header}.{payload}").as_bytes(), &signature) {
        return invalid(FileVerifyFailure::BadSignature);
    }
    b64url_decode_canonical(payload)
        .map(|bytes| decode_utf8(&bytes))
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| parse_file_claims(&value))
        .map_or(invalid(FileVerifyFailure::BadClaims), |claims| {
            FileVerifyResult::Valid(Box::new(claims))
        })
}

/// Why a licence file was not taken
/// ([`License::import_license_file`](crate::License::import_license_file)),
/// or why a stored one does not license this build.
///
/// Serialises as its word (`"malformed"`, `"not-a-license-file"`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LicenseFileError {
    /// Not a licence file's text, or its token or claims are broken.
    Malformed,
    /// A signed token, but not a licence file (a lease, say).
    NotALicenseFile,
    /// Not signed by the key it names.
    BadSignature,
    /// Signed by a key this build does not carry: the app's update carries it.
    UnknownKey,
    /// Another product's file.
    WrongProduct,
    /// Its id is one this build lists as revoked.
    RevokedFile,
    /// An offline file for older versions than this build.
    Upgrade,
    /// An offline file for later versions than this build: update the app.
    Outdated,
}

impl LicenseFileError {
    /// The word for it (`malformed`, `not-a-license-file`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            LicenseFileError::Malformed => "malformed",
            LicenseFileError::NotALicenseFile => "not-a-license-file",
            LicenseFileError::BadSignature => "bad-signature",
            LicenseFileError::UnknownKey => "unknown-key",
            LicenseFileError::WrongProduct => "wrong-product",
            LicenseFileError::RevokedFile => "revoked-file",
            LicenseFileError::Upgrade => "upgrade",
            LicenseFileError::Outdated => "outdated",
        }
    }
}

impl fmt::Display for LicenseFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for LicenseFileError {}

/// What a licence file says about this app, offline.
#[derive(Debug, Clone, PartialEq)]
pub enum LicenseFileCheck {
    /// It may be taken: an offline file that covers this build, or a key file.
    Taken(Box<LicenseFileClaims>),
    /// Refused; the claims when the signature held, so a status can still say
    /// whose file it is.
    Refused(LicenseFileError, Option<Box<LicenseFileClaims>>),
}

/// A verified licence file held to this product, this build's major and the
/// app's denylist, in this order: not valid (`UnknownKey`, `BadSignature` and
/// `NotALicenseFile` as they are; bad claims and `Malformed` as `Malformed`);
/// another product (`WrongProduct`); its id in `revoked`, matched exactly
/// (`RevokedFile`); for an offline file, the major below `min_major` (none:
/// 1, `Outdated`) or above `major` (`Upgrade`). A key file covers what its key
/// does, which only the server can say, so no version is judged.
pub fn check_license_file(
    verified: &FileVerifyResult,
    product: &str,
    major: i64,
    revoked: &[String],
) -> LicenseFileCheck {
    let claims = match verified {
        FileVerifyResult::Valid(claims) => claims.clone(),
        FileVerifyResult::Invalid(failure) => {
            let reason = match failure {
                FileVerifyFailure::UnknownKey => LicenseFileError::UnknownKey,
                FileVerifyFailure::BadSignature => LicenseFileError::BadSignature,
                FileVerifyFailure::NotALicenseFile => LicenseFileError::NotALicenseFile,
                FileVerifyFailure::BadClaims | FileVerifyFailure::Malformed => {
                    LicenseFileError::Malformed
                }
            };
            return LicenseFileCheck::Refused(reason, None);
        }
    };
    let refused = |reason| LicenseFileCheck::Refused(reason, Some(claims.clone()));
    if claims.product != product {
        return refused(LicenseFileError::WrongProduct);
    }
    if revoked.contains(&claims.fid) {
        return refused(LicenseFileError::RevokedFile);
    }
    if let Some(terms) = &claims.offline {
        if major < terms.min_major.unwrap_or(1) {
            return refused(LicenseFileError::Outdated);
        }
        if major > terms.major {
            return refused(LicenseFileError::Upgrade);
        }
    }
    LicenseFileCheck::Taken(claims)
}

/// Who a file is licensed to, as it signs it.
pub fn licensed_to_of(claims: &LicenseFileClaims) -> LicensedTo {
    LicensedTo {
        name: claims.name.clone(),
        email: claims.email.clone(),
    }
}

/// The status a STORED offline licence file gives, every one with
/// [`LicenseSource::File`]: `Licensed`, with its key, who it is licensed to,
/// `test` when it says so and its entitlements; `Invalid` with `RevokedFile`,
/// `Upgrade` or `Outdated`, its key and licensee, but no entitlements;
/// `Invalid` / `UnknownKey` when the build does not carry its key (nothing of
/// it read); and `Invalid` / `Tampered` for anything else, a key file included
/// (one is never stored: the state was edited).
pub fn license_file_status(check: &LicenseFileCheck) -> LicenseStatus {
    let from_file = |state| {
        let mut status = LicenseStatus::new(state);
        status.source = Some(LicenseSource::File);
        status
    };
    let of = |state, claims: &LicenseFileClaims| {
        let mut status = from_file(state).with_key(Some(&claims.sub));
        status.licensed_to = Some(licensed_to_of(claims));
        status
    };
    match check {
        LicenseFileCheck::Taken(claims) => match &claims.offline {
            Some(terms) => {
                let mut status = of(LicenseState::Licensed, claims);
                status.test = claims.test == Some(true);
                status.entitlements = terms.ent.clone();
                status
            }
            None => from_file(LicenseState::Invalid).with_reason(LicenseReason::Tampered),
        },
        LicenseFileCheck::Refused(
            reason @ (LicenseFileError::RevokedFile
            | LicenseFileError::Upgrade
            | LicenseFileError::Outdated),
            Some(claims),
        ) => of(LicenseState::Invalid, claims).with_reason(match reason {
            LicenseFileError::RevokedFile => LicenseReason::RevokedFile,
            LicenseFileError::Upgrade => LicenseReason::Upgrade,
            _ => LicenseReason::Outdated,
        }),
        LicenseFileCheck::Refused(LicenseFileError::UnknownKey, _) => {
            from_file(LicenseState::Invalid).with_reason(LicenseReason::UnknownKey)
        }
        LicenseFileCheck::Refused(..) => {
            from_file(LicenseState::Invalid).with_reason(LicenseReason::Tampered)
        }
    }
}

impl LicenseFileCheck {
    /// As the conformance vectors write it: `{ok: true, kind, test}` or `{ok: false, reason}`.
    pub fn to_json(&self) -> Value {
        match self {
            LicenseFileCheck::Taken(claims) => serde_json::json!({
                "ok": true,
                "kind": claims.kind().as_str(),
                "test": claims.test == Some(true),
            }),
            LicenseFileCheck::Refused(reason, _) => {
                serde_json::json!({ "ok": false, "reason": reason.as_str() })
            }
        }
    }
}
