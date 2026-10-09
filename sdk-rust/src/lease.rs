//! The lease: a standard JWT (JWS, EdDSA / Ed25519) the server signs and the
//! app verifies OFFLINE against the product's public key, trusted until its
//! `exp`. The lease's lifetime is the offline grace window.
//!
//! Verification checks the signature first, then the claims' shape, then the
//! expiry, exactly as the server's own verifier does.

use std::collections::BTreeMap;
use std::fmt;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::status::DeviceKind;
use crate::wire::utf16_len;

/// The issuer every lease names.
pub const ISSUER: &str = "keygrant";

/// The highest real major version anything accepts.
pub const MAX_MAJOR: i64 = 99_999;

/// The lease's `major` claim for a key with no upper bound: above every real
/// major, so every future version runs on it.
pub const OPEN_MAJOR: i64 = 1_000_000;

/// A product's public key, as embedded in the app to verify its leases offline.
#[derive(Clone, PartialEq, Eq)]
pub struct PublicKey(VerifyingKey);

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PublicKey")
            .field(&b64url_encode(self.0.as_bytes()))
            .finish()
    }
}

/// Why a public key could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyError(String);

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for KeyError {}

impl PublicKey {
    /// The key from the product's public JWK (`{"kty":"OKP","crv":"Ed25519","x":"..."}`,
    /// the `publicJwk` of `GET https://api.keygrant.dev/v1/products/<product>/pubkey`),
    /// or from its `x` member alone (base64url, 32 bytes).
    pub fn parse(jwk_or_x: &str) -> Result<PublicKey, KeyError> {
        let text = jwk_or_x.trim();
        if text.starts_with('{') {
            let value: Value = serde_json::from_str(text)
                .map_err(|e| KeyError(format!("the public JWK is not JSON: {e}")))?;
            return PublicKey::from_jwk(&value);
        }
        PublicKey::from_x(text)
    }

    /// The key from a parsed public JWK.
    pub fn from_jwk(jwk: &Value) -> Result<PublicKey, KeyError> {
        let map = jwk
            .as_object()
            .ok_or_else(|| KeyError("the public JWK is not an object".into()))?;
        if map.get("kty").and_then(Value::as_str) != Some("OKP") {
            return Err(KeyError("the public JWK's kty is not \"OKP\"".into()));
        }
        if map.get("crv").and_then(Value::as_str) != Some("Ed25519") {
            return Err(KeyError("the public JWK's crv is not \"Ed25519\"".into()));
        }
        // A private key in an app is every lease forgeable: refused, as
        // WebCrypto refuses to import one for `verify`.
        if map.contains_key("d") {
            return Err(KeyError(
                "this JWK holds the PRIVATE key (`d`): embed the public JWK only".into(),
            ));
        }
        let x = map
            .get("x")
            .and_then(Value::as_str)
            .ok_or_else(|| KeyError("the public JWK has no x".into()))?;
        PublicKey::from_x(x)
    }

    /// The key from its 32 bytes, base64url (a JWK's `x`).
    pub fn from_x(x: &str) -> Result<PublicKey, KeyError> {
        let bytes =
            b64url_decode(x).ok_or_else(|| KeyError("the public key is not base64url".into()))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| KeyError("the public key is not 32 bytes of Ed25519".into()))?;
        VerifyingKey::from_bytes(&bytes)
            .map(PublicKey)
            .map_err(|_| KeyError("the public key is not an Ed25519 point".into()))
    }

    /// The key from its 32 raw bytes.
    pub fn from_bytes(bytes: &[u8; 32]) -> Result<PublicKey, KeyError> {
        VerifyingKey::from_bytes(bytes)
            .map(PublicKey)
            .map_err(|_| KeyError("the public key is not an Ed25519 point".into()))
    }
}

/// A licence model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LicenseModel {
    /// Bought once.
    Perpetual,
    /// Paid while it runs.
    Subscription,
    /// A trial.
    Trial,
}

/// One entitlement's value: what a licence grants beyond the base product,
/// under one name of its lease's `ent` claim (`{"edition": "pro", "seats": 5}`),
/// as [`LicenseStatus::entitlements`](crate::LicenseStatus::entitlements)
/// hands it to the app.
///
/// A flag, a number or a text, as JSON has them. A number is a double, as
/// JSON and JavaScript read one, correctly rounded (the nearest double; the
/// crate reads JSON with serde_json's `float_roundtrip`): `5` and `5.0` are
/// the same entitlement.
///
/// Serialises as the bare JSON value (`true`, `5`, `0.5`, `"pro"`), a whole
/// number without a fraction as JavaScript writes it, so a status reaches a
/// front end exactly as the Electron SDK's does; and reads back from one.
///
/// Equality is by content, numbers compared as numbers (`5` equals `5.0`,
/// and `0` equals `-0`), with one departure from IEEE 754: a NaN equals a
/// NaN. That keeps equality an equivalence relation (reflexive, symmetric and
/// transitive: IEEE equality is all three on every value but NaN, and the
/// NaNs here form one class of their own), so `Eq` is sound, and so
/// [`LicenseStatus`](crate::LicenseStatus) can implement `Eq` too. No lease
/// can carry a NaN (JSON has none); only a value an app builds itself could.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Entitlement {
    /// A flag.
    Bool(bool),
    /// A number (a double).
    Number(f64),
    /// A text.
    Text(String),
}

impl Entitlement {
    /// The flag, when this is one.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Entitlement::Bool(flag) => Some(*flag),
            _ => None,
        }
    }

    /// The number, when this is one.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Entitlement::Number(number) => Some(*number),
            _ => None,
        }
    }

    /// The text, when this is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Entitlement::Text(text) => Some(text),
            _ => None,
        }
    }

    /// The value as JSON, as the reference writes it: a whole number without a fraction.
    fn to_json(&self) -> Value {
        match self {
            Entitlement::Bool(flag) => Value::from(*flag),
            Entitlement::Number(number) => js_number(*number),
            Entitlement::Text(text) => Value::from(text.clone()),
        }
    }
}

impl PartialEq for Entitlement {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Entitlement::Bool(a), Entitlement::Bool(b)) => a == b,
            // As numbers, and a NaN as itself: see the type's documentation.
            (Entitlement::Number(a), Entitlement::Number(b)) => {
                a == b || (a.is_nan() && b.is_nan())
            }
            (Entitlement::Text(a), Entitlement::Text(b)) => a == b,
            _ => false,
        }
    }
}

/// Sound: [`PartialEq`] for `Entitlement` is an equivalence relation (see the type).
impl Eq for Entitlement {}

impl Serialize for Entitlement {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_json().serialize(serializer)
    }
}

/// A lease's claims, read as the server's schema reads them: unknown claims
/// ignored, `major` defaulting to 0, and `ent` read leniently (empty when
/// absent or not an object, a malformed member dropped). Times are SECONDS
/// since the epoch.
#[derive(Debug, Clone, PartialEq)]
pub struct LeaseClaims {
    /// Always `keygrant`.
    pub iss: String,
    /// The licence key.
    pub sub: String,
    /// The product slug the lease is for.
    pub product: String,
    /// The activation (device seat) it was issued to; on a trial lease, the
    /// device's fingerprint claim.
    pub aid: String,
    /// The hash of the device's fingerprint (`fingerprint_claim`), when signed with one.
    pub fp: Option<String>,
    /// What `fp` hashes, when known.
    pub fpk: Option<DeviceKind>,
    /// The licence model.
    pub model: LicenseModel,
    /// The licence's device limit.
    pub lim: i64,
    /// The highest major it covers ([`OPEN_MAJOR`] for no upper bound); 0 when absent.
    pub major: i64,
    /// The lowest major it covers, when above 1.
    pub min_major: Option<i64>,
    /// When the key's upgrade window ends (seconds), while it is open.
    pub upgrade_until: Option<i64>,
    /// What the licence grants beyond the base product, by name; empty for the
    /// base product. Read leniently: a member that is not a flag, a number or a
    /// text is dropped, and an `ent` that is not an object is empty, never bad
    /// claims.
    pub ent: BTreeMap<String, Entitlement>,
    /// Issued at (seconds).
    pub iat: i64,
    /// Expires at (seconds): the grace boundary.
    pub exp: i64,
    /// True inside the grace window the licence's terms grant.
    pub grace: Option<bool>,
    /// True on a TEST licence.
    pub test: Option<bool>,
}

impl LicenseModel {
    /// The claim's word for it.
    pub fn as_str(self) -> &'static str {
        match self {
            LicenseModel::Perpetual => "perpetual",
            LicenseModel::Subscription => "subscription",
            LicenseModel::Trial => "trial",
        }
    }
}

/// A number as JavaScript writes it: a whole one without a fraction.
fn js_number(value: f64) -> Value {
    let whole = value.fract() == 0.0 && (-9.0e15..=9.0e15).contains(&value);
    if whole {
        Value::from(value as i64)
    } else {
        serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}

impl LeaseClaims {
    /// The claims as JSON, as the reference's schema gives them back: its
    /// defaults applied (`major` 0, `ent` {}), absent claims left out.
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        let mut put = |name: &str, value: Value| {
            map.insert(name.to_owned(), value);
        };
        put("iss", Value::from(self.iss.clone()));
        put("sub", Value::from(self.sub.clone()));
        put("product", Value::from(self.product.clone()));
        put("aid", Value::from(self.aid.clone()));
        if let Some(fp) = &self.fp {
            put("fp", Value::from(fp.clone()));
        }
        if let Some(fpk) = self.fpk {
            put("fpk", Value::from(fpk.as_str()));
        }
        put("model", Value::from(self.model.as_str()));
        put("lim", Value::from(self.lim));
        put("major", Value::from(self.major));
        if let Some(min) = self.min_major {
            put("minMajor", Value::from(min));
        }
        if let Some(until) = self.upgrade_until {
            put("upgradeUntil", Value::from(until));
        }
        let ent = self
            .ent
            .iter()
            .map(|(name, value)| (name.clone(), value.to_json()))
            .collect();
        put("ent", Value::Object(ent));
        put("iat", Value::from(self.iat));
        put("exp", Value::from(self.exp));
        if let Some(grace) = self.grace {
            put("grace", Value::from(grace));
        }
        if let Some(test) = self.test {
            put("test", Value::from(test));
        }
        Value::Object(map)
    }

    /// Claims as a verified lease's payload is read (the claims schema, its
    /// defaults applied), or none when they are not a lease's claims.
    pub fn from_json(value: &Value) -> Option<LeaseClaims> {
        parse_claims(value)
    }
}

/// Why a lease did not verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VerifyFailure {
    /// Not three parts, or a signature that is not base64.
    Malformed,
    /// Not signed by this key.
    BadSignature,
    /// Signed, but its claims are not a lease's.
    BadClaims,
    /// Past its `exp`.
    Expired,
}

impl VerifyFailure {
    /// The reference's word for it (`malformed`, `bad-signature`, `bad-claims`, `expired`).
    pub fn as_str(self) -> &'static str {
        match self {
            VerifyFailure::Malformed => "malformed",
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

/// Verify a lease offline at `now_ms` (ms since the epoch): the signature
/// first, then the claims' shape, then the expiry (`exp * 1000 < now` is
/// expired: a lease is still valid during its `exp` second).
///
/// The header is not read: the algorithm and the key are the verifier's,
/// never the token's. The signature and the payload must be CANONICAL
/// base64url, as the server writes them: a signature that is not is
/// `Malformed`, a payload that is not is `BadClaims`.
pub fn verify_lease(token: &str, key: &PublicKey, now_ms: i64) -> VerifyResult {
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
    let Ok(signature) = Signature::from_slice(&signature) else {
        return invalid(VerifyFailure::BadSignature);
    };
    let signed = format!("{header}.{payload}");
    if key.0.verify(signed.as_bytes(), &signature).is_err() {
        return invalid(VerifyFailure::BadSignature);
    }
    let claims = b64url_decode_canonical(payload)
        .map(|bytes| decode_utf8(&bytes))
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
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

/// Bytes as text the way JavaScript's `TextDecoder` (UTF-8, the default)
/// reads them: one leading byte-order mark dropped, and anything that is not
/// UTF-8 replaced with U+FFFD.
fn decode_utf8(bytes: &[u8]) -> String {
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

/// The claims as the reference's schema reads them, or none when they are not a lease's.
fn parse_claims(value: &Value) -> Option<LeaseClaims> {
    let map = value.as_object()?;
    let iss = string(map, "iss")?;
    if iss != ISSUER {
        return None;
    }
    let fp = optional(map, "fp", |v| {
        v.as_str()
            .filter(|s| (1..=128).contains(&utf16_len(s)))
            .map(str::to_owned)
    })?;
    Some(LeaseClaims {
        iss,
        sub: string(map, "sub")?,
        product: string(map, "product")?,
        aid: string(map, "aid")?,
        fp,
        fpk: optional(map, "fpk", |v| v.as_str().and_then(DeviceKind::parse))?,
        model: match map.get("model")?.as_str()? {
            "perpetual" => LicenseModel::Perpetual,
            "subscription" => LicenseModel::Subscription,
            "trial" => LicenseModel::Trial,
            _ => return None,
        },
        lim: integer(map.get("lim")?).filter(|n| *n > 0)?,
        major: optional(map, "major", |v| integer(v).filter(|n| *n >= 0))?.unwrap_or(0),
        min_major: optional(map, "minMajor", |v| integer(v).filter(|n| *n > 0))?,
        upgrade_until: optional(map, "upgradeUntil", |v| integer(v).filter(|n| *n >= 0))?,
        ent: map.get("ent").map(entitlements_of).unwrap_or_default(),
        iat: integer(map.get("iat")?).filter(|n| *n >= 0)?,
        exp: integer(map.get("exp")?).filter(|n| *n >= 0)?,
        grace: optional(map, "grace", Value::as_bool)?,
        test: optional(map, "test", Value::as_bool)?,
    })
}

/// A required non-empty string.
fn string(map: &Map<String, Value>, name: &str) -> Option<String> {
    map.get(name)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// An optional claim: `Some(None)` when absent, `None` (bad claims) when
/// present and not what `read` accepts (`null` included).
fn optional<T>(
    map: &Map<String, Value>,
    name: &str,
    read: impl Fn(&Value) -> Option<T>,
) -> Option<Option<T>> {
    match map.get(name) {
        None => Some(None),
        Some(value) => read(value).map(Some),
    }
}

/// A JSON number that is a whole number (`Number.isInteger`), within i64.
fn integer(value: &Value) -> Option<i64> {
    let number = value.as_number()?;
    if let Some(whole) = number.as_i64() {
        return Some(whole);
    }
    let float = number.as_f64()?;
    let whole = float.is_finite()
        && float.fract() == 0.0
        && (-9.223_372_036_854_775e18..9.223_372_036_854_775e18).contains(&float);
    whole.then_some(float as i64)
}

/// `ent` as a verifier reads it, from the JSON a lease payload is read into: an
/// object's flag, number and text members, kept as they are, names included;
/// every other member (an object, a list, `null`) dropped; and nothing at all
/// from anything but an object (`null`, a list, a text, a number, a flag; an
/// absent `ent` reads as none too). Never bad claims: a lease that verifies is
/// the server's word, and one malformed entitlement must not take away the
/// licence it rides on, or the entitlements beside it. None of the server's
/// caps (how many, how long) is enforced: the signature is what is trusted. A
/// number is the double the JSON reader gave it, which `verify_lease` reads
/// correctly rounded.
pub fn entitlements_of(value: &Value) -> BTreeMap<String, Entitlement> {
    let Some(map) = value.as_object() else {
        return BTreeMap::new();
    };
    map.iter()
        .filter_map(|(name, item)| {
            let entitlement = match item {
                Value::Bool(flag) => Entitlement::Bool(*flag),
                Value::Number(number) => Entitlement::Number(number.as_f64()?),
                Value::String(text) => Entitlement::Text(text.clone()),
                _ => return None,
            };
            Some((name.clone(), entitlement))
        })
        .collect()
}

/// Base64url as the reference decodes it: `-` and `_` read as `+` and `/`,
/// padded to a multiple of four, then decoded as `atob` does (the WHATWG
/// "forgiving-base64 decode": ASCII whitespace ignored, up to two trailing
/// `=`, leftover bits discarded). None where `atob` throws.
pub(crate) fn b64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut text: String = input
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    let length = utf16_len(&text);
    let pad = if length % 4 == 0 { 0 } else { 4 - length % 4 };
    text.extend(std::iter::repeat_n('=', pad));
    forgiving_base64_decode(&text)
}

/// CANONICAL base64url, as the server's verifier takes it for a token's parts:
/// the URL-safe alphabet only, no padding, and nothing the encoder would not
/// have written (encoding the result gives the input back, so no stray bits in
/// the last character). None for anything else: one signature must have one
/// spelling, and every lease the server signs is canonical.
pub(crate) fn b64url_decode_canonical(input: &str) -> Option<Vec<u8>> {
    let alphabet = input
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !alphabet || input.len() % 4 == 1 {
        return None;
    }
    let bytes = b64url_decode(input)?;
    (b64url_encode(&bytes) == input).then_some(bytes)
}

fn forgiving_base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut data: Vec<char> = input
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\x0c' | '\r' | ' '))
        .collect();
    if data.len() % 4 == 0 {
        if data.ends_with(&['=', '=']) {
            data.truncate(data.len() - 2);
        } else if data.ends_with(&['=']) {
            data.truncate(data.len() - 1);
        }
    }
    if data.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for c in data {
        let sextet = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | sextet;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// Base64url without padding.
pub(crate) fn b64url_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_as_atob_does() {
        assert_eq!(b64url_decode("QQ"), Some(b"A".to_vec()));
        assert_eq!(b64url_decode("QQ=="), Some(b"A".to_vec()));
        assert_eq!(b64url_decode("QQ="), Some(b"A".to_vec()));
        // Leftover bits are discarded, not refused.
        assert_eq!(b64url_decode("QR"), Some(b"A".to_vec()));
        assert_eq!(b64url_decode("Q"), None);
        assert_eq!(b64url_decode("Q="), None);
        assert_eq!(b64url_decode("!!!"), None);
        assert_eq!(b64url_decode("Q=Q="), None);
        assert_eq!(b64url_decode("-_-_"), Some(vec![0xfb, 0xff, 0xbf]));
        assert_eq!(b64url_decode("+/+/"), Some(vec![0xfb, 0xff, 0xbf]));
        assert_eq!(b64url_decode(""), Some(vec![]));
    }

    #[test]
    fn decodes_a_tokens_parts_only_when_canonical() {
        assert_eq!(b64url_decode_canonical("QQ"), Some(b"A".to_vec()));
        assert_eq!(
            b64url_decode_canonical("-_-_"),
            Some(vec![0xfb, 0xff, 0xbf])
        );
        assert_eq!(b64url_decode_canonical(""), Some(vec![]));
        // Padding, the standard alphabet, white space and stray bits each give
        // a second spelling of the same bytes: refused.
        for other in ["QQ==", "QQ=", "+/+/", "Q Q", "QR", "Q", "!!!"] {
            assert_eq!(b64url_decode_canonical(other), None, "{other:?}");
        }
    }

    #[test]
    fn a_signature_with_stray_bits_is_malformed_and_a_payload_bad_claims() {
        use ed25519_dalek::{Signer, SigningKey};
        let signing = SigningKey::from_bytes(&[3; 32]);
        let key = PublicKey::from_bytes(signing.verifying_key().as_bytes()).expect("a key");
        let header = b64url_encode(br#"{"alg":"EdDSA","typ":"JWT"}"#);
        let payload = b64url_encode(
            br#"{"iss":"keygrant","sub":"K","product":"p","aid":"a","model":"trial","lim":1,"iat":0,"exp":9999999999}"#,
        );
        let input = format!("{header}.{payload}");
        let signature = b64url_encode(&signing.sign(input.as_bytes()).to_bytes());
        assert!(verify_lease(&format!("{input}.{signature}"), &key, 0).is_valid());
        // The last of 86 characters holds 2 bits and 4 that must be zero, so a
        // canonical one is A, Q, g or w: `B` has stray bits (malformed), and
        // another of the four is canonical but not the signature.
        let (body, last) = signature.split_at(signature.len() - 1);
        assert!(["A", "Q", "g", "w"].contains(&last), "{last}");
        let other = if last == "A" { "Q" } else { "A" };
        let with_last = |c: &str| verify_lease(&format!("{input}.{body}{c}"), &key, 0).failure();
        assert_eq!(with_last("B"), Some(VerifyFailure::Malformed));
        assert_eq!(with_last(other), Some(VerifyFailure::BadSignature));
        assert_eq!(
            verify_lease(&format!("{input}.{signature}="), &key, 0).failure(),
            Some(VerifyFailure::Malformed)
        );
        // A payload written with padding was not signed as the server writes it.
        let padded = format!("{header}.{payload}=");
        let padded_signature = b64url_encode(&signing.sign(padded.as_bytes()).to_bytes());
        assert_eq!(
            verify_lease(&format!("{padded}.{padded_signature}"), &key, 0).failure(),
            Some(VerifyFailure::BadClaims)
        );
    }

    #[test]
    fn encodes_round_trip() {
        for len in 0..40u8 {
            let bytes: Vec<u8> = (0..len)
                .map(|i| i.wrapping_mul(37).wrapping_add(11))
                .collect();
            assert_eq!(b64url_decode(&b64url_encode(&bytes)), Some(bytes));
        }
    }

    #[test]
    fn a_payload_with_a_byte_order_mark_reads_as_the_reference_reads_it() {
        use ed25519_dalek::{Signer, SigningKey};
        let signing = SigningKey::from_bytes(&[5; 32]);
        let key = PublicKey::from_bytes(signing.verifying_key().as_bytes()).expect("a key");
        let claims = r#"{"iss":"keygrant","sub":"K","product":"p","aid":"a","model":"perpetual","lim":1,"iat":0,"exp":9999999999}"#;
        let token = |payload: &str| {
            let input = format!(
                "{}.{}",
                b64url_encode(b"{}"),
                b64url_encode(payload.as_bytes())
            );
            let signature = b64url_encode(&signing.sign(input.as_bytes()).to_bytes());
            format!("{input}.{signature}")
        };
        // TextDecoder drops one leading byte-order mark: valid, as in the reference.
        assert!(verify_lease(&token(&format!("\u{FEFF}{claims}")), &key, 0).is_valid());
        // A second is text JSON.parse refuses.
        assert_eq!(
            verify_lease(&token(&format!("\u{FEFF}\u{FEFF}{claims}")), &key, 0).failure(),
            Some(VerifyFailure::BadClaims)
        );
    }
}
