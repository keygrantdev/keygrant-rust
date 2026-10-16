//! The lease: a standard JWT (JWS, EdDSA / Ed25519) the server signs and the
//! app verifies OFFLINE against the product's public key, trusted until its
//! `exp`. The lease's lifetime is the offline grace window.
//!
//! Verification checks the signature first, then the claims' shape, then the
//! expiry, exactly as the server's own verifier does.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// The issuer every lease names.
pub const ISSUER: &str = "keygrant";

/// The highest real major version anything accepts.
pub const MAX_MAJOR: i64 = 99_999;

/// The lease's `major` claim for a key with no upper bound: above every real
/// major, so every future version runs on it.
pub const OPEN_MAJOR: i64 = 1_000_000;

mod base64;
mod claims;
mod keys;
#[cfg(test)]
mod tests;

#[cfg(test)]
use base64::b64url_decode;
pub(crate) use base64::{b64url_decode_canonical, b64url_encode};
use claims::parse_claims;
pub use claims::{entitlements_of, grants, Entitlement, LeaseClaims, LicenseModel};
pub(crate) use claims::{optional, string};
use keys::kid_of;
pub(crate) use keys::named_kid;
pub use keys::{KeyError, KeySet, PublicKey};

/// Why a lease did not verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VerifyFailure {
    /// Not three parts, or a signature that is not base64.
    Malformed,
    /// It names a key (`kid`) the key set does not hold.
    UnknownKey,
    /// Not signed by the key it names, or by any key of the set.
    BadSignature,
    /// Signed, but its claims are not a lease's.
    BadClaims,
    /// Past its `exp`.
    Expired,
}

impl VerifyFailure {
    /// The reference's word for it (`malformed`, `unknown-key`, `bad-signature`, `bad-claims`, `expired`).
    pub fn as_str(self) -> &'static str {
        match self {
            VerifyFailure::Malformed => "malformed",
            VerifyFailure::UnknownKey => "unknown-key",
            VerifyFailure::BadSignature => "bad-signature",
            VerifyFailure::BadClaims => "bad-claims",
            VerifyFailure::Expired => "expired",
        }
    }
}

/// What verifying a lease gave.
#[derive(Debug, Clone, PartialEq)]
pub enum VerifyResult {
    /// Signed by the key, well formed, and not past its end.
    Valid(LeaseClaims),
    /// Not valid; the claims are there when the lease only expired.
    Invalid {
        /// Why.
        reason: VerifyFailure,
        /// The claims of an expired lease.
        claims: Option<LeaseClaims>,
    },
}

impl VerifyResult {
    /// Whether it verified.
    pub fn is_valid(&self) -> bool {
        matches!(self, VerifyResult::Valid(_))
    }

    /// The claims, when the lease verified or only expired.
    pub fn claims(&self) -> Option<&LeaseClaims> {
        match self {
            VerifyResult::Valid(claims) => Some(claims),
            VerifyResult::Invalid { claims, .. } => claims.as_ref(),
        }
    }

    /// What verifying said, as the reference's conformance vectors write it:
    /// `{valid, reason?, claims?}`, the claims there for a valid or merely
    /// expired lease.
    pub fn to_json(&self) -> Value {
        match self {
            VerifyResult::Valid(claims) => {
                serde_json::json!({ "valid": true, "claims": claims.to_json() })
            }
            VerifyResult::Invalid { reason, claims } => {
                let mut out = serde_json::json!({ "valid": false, "reason": reason.as_str() });
                if let Some(claims) = claims {
                    out["claims"] = claims.to_json();
                }
                out
            }
        }
    }

    /// Why it did not verify.
    pub fn failure(&self) -> Option<VerifyFailure> {
        match self {
            VerifyResult::Valid(_) => None,
            VerifyResult::Invalid { reason, .. } => Some(*reason),
        }
    }
}

/// Verify a lease offline at `now_ms` (ms since the epoch) against a key
/// set: the signature first, then the claims' shape, then the expiry
/// (`exp * 1000 < now` is expired: a lease is still valid during its `exp`
/// second).
///
/// The header is read for one thing, the id of the key that signed the lease
/// (`kid`): a lease naming a key is verified by that key only, and is
/// `UnknownKey` when the set holds none of that id; a lease naming none is
/// tried against each key in turn. The algorithm is the verifier's, never the
/// token's. The signature and the payload must be CANONICAL base64url, as the
/// server writes them: a signature that is not is `Malformed`, a payload that
/// is not is `BadClaims`.
pub fn verify_lease(token: &str, keys: &KeySet, now_ms: i64) -> VerifyResult {
    let invalid = |reason| VerifyResult::Invalid {
        reason,
        claims: None,
    };
    let parts: Vec<&str> = token.split('.').collect();
    let [header, payload, signature] = parts.as_slice() else {
        return invalid(VerifyFailure::Malformed);
    };
    let Some(signature) = b64url_decode_canonical(signature) else {
        return invalid(VerifyFailure::Malformed);
    };
    let kid = kid_of(header);
    let candidates: Vec<&PublicKey> = keys
        .0
        .iter()
        .filter(|(held, _)| kid.as_ref().is_none_or(|kid| held == kid))
        .map(|(_, key)| key)
        .collect();
    if candidates.is_empty() {
        return invalid(VerifyFailure::UnknownKey);
    }
    let signed = format!("{header}.{payload}");
    if !candidates
        .iter()
        .any(|key| key.verifies(signed.as_bytes(), &signature))
    {
        return invalid(VerifyFailure::BadSignature);
    }
    let claims = b64url_decode_canonical(payload)
        .map(|bytes| decode_utf8(&bytes))
        .and_then(|text| parse_json(&text))
        .and_then(|value| parse_claims(&value));
    let Some(claims) = claims else {
        return invalid(VerifyFailure::BadClaims);
    };
    if i128::from(claims.exp) * 1000 < i128::from(now_ms) {
        return VerifyResult::Invalid {
            reason: VerifyFailure::Expired,
            claims: Some(claims),
        };
    }
    VerifyResult::Valid(claims)
}

/// JSON text as JavaScript's `JSON.parse` reads it, in the one way that matters
/// to a lease: a number out of a double's range (`1e400`) reads as infinity
/// there and fails the parse here. It fails nothing in a lease, which a figure
/// the SDK reads leniently (a meter's) must not cost: such a number is read as
/// the largest double of its sign, which no figure, count or time of a lease
/// holds either, so every figure, count and time that rejects infinity rejects it
/// too. An entitlement keeps it, as JavaScript keeps `Infinity` there. The HTTP
/// adapter reads every answer's body through it. None when the text is not JSON.
pub fn parse_json(text: &str) -> Option<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return Some(value);
    }
    serde_json::from_str(&clamp_infinite_numbers(text)?).ok()
}

/// `text` with every number token outside a string that reads as infinity
/// replaced by the largest double of its sign; none when there is no such token.
fn clamp_infinite_numbers(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let (mut out, mut copied, mut at) = (String::new(), 0, 0);
    while at < bytes.len() {
        match bytes[at] {
            b'"' => {
                // Past the string, escapes included.
                at += 1;
                while at < bytes.len() && bytes[at] != b'"' {
                    at += if bytes[at] == b'\\' { 2 } else { 1 };
                }
                at += 1;
            }
            b'-' | b'0'..=b'9' => {
                let start = at;
                at += 1;
                while at < bytes.len()
                    && matches!(bytes[at], b'0'..=b'9' | b'+' | b'-' | b'.' | b'e' | b'E')
                {
                    at += 1;
                }
                let token = &text[start..at];
                if token.parse::<f64>().is_ok_and(f64::is_infinite) {
                    out.push_str(&text[copied..start]);
                    out.push_str(if token.starts_with('-') {
                        "-1.7976931348623157e308"
                    } else {
                        "1.7976931348623157e308"
                    });
                    copied = at;
                }
            }
            _ => at += 1,
        }
    }
    (copied > 0).then(|| {
        out.push_str(&text[copied..]);
        out
    })
}

/// Bytes as text the way JavaScript's `TextDecoder` (UTF-8, the default)
/// reads them: one leading byte-order mark dropped, and anything that is not
/// UTF-8 replaced with U+FFFD.
pub(crate) fn decode_utf8(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.strip_prefix('\u{FEFF}').unwrap_or(&text).to_owned()
}

/// The `fp` claim for a device fingerprint: hex SHA-256 of `keygrant-fp|<fingerprint>`.
pub fn fingerprint_claim(fingerprint: &str) -> String {
    hex_sha256(&format!("keygrant-fp|{fingerprint}"))
}

pub(crate) fn hex_sha256(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push(char::from(HEX[usize::from(byte >> 4)]));
        hex.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    hex
}

const HEX: &[u8; 16] = b"0123456789abcdef";
