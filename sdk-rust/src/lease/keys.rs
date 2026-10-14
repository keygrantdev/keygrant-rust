//! A product's public keys: one key, its id (`kid`), the key set an app
//! carries, and the key a lease's header names.

use std::fmt;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::base64::{b64url_decode_canonical, b64url_encode};

/// A product's public key, as embedded in the app to verify its leases offline:
/// its 32 bytes. Bytes that are not a point on the curve are a key all the
/// same, as every KeyGrant SDK reads them, and verify no lease.
#[derive(Clone, PartialEq, Eq)]
pub struct PublicKey(pub(super) [u8; 32]);

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PublicKey")
            .field(&b64url_encode(&self.0))
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
        if map
            .get("use")
            .is_some_and(|used| used.as_str() != Some("sig"))
        {
            return Err(KeyError("the public JWK's use is not \"sig\"".into()));
        }
        if let Some(ops) = map.get("key_ops") {
            let verifies = ops
                .as_array()
                .is_some_and(|ops| ops.iter().any(|op| op.as_str() == Some("verify")));
            if !verifies {
                return Err(KeyError(
                    "the public JWK's key_ops do not include \"verify\"".into(),
                ));
            }
        }
        if map
            .get("alg")
            .is_some_and(|alg| !matches!(alg.as_str(), Some("EdDSA" | "Ed25519")))
        {
            return Err(KeyError("the public JWK's alg is not \"EdDSA\"".into()));
        }
        let x = map
            .get("x")
            .and_then(Value::as_str)
            .ok_or_else(|| KeyError("the public JWK has no x".into()))?;
        PublicKey::from_x(x)
    }

    /// The key from its 32 bytes, base64url (a JWK's `x`): canonical once its
    /// trailing `=` are dropped, as a key pasted with padding has them.
    pub fn from_x(x: &str) -> Result<PublicKey, KeyError> {
        let bytes = b64url_decode_canonical(x.trim_end_matches('='))
            .ok_or_else(|| KeyError("the public key is not base64url".into()))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| KeyError("the public key is not 32 bytes of Ed25519".into()))?;
        Ok(PublicKey(bytes))
    }

    /// The key from its 32 raw bytes. Never an error: any 32 bytes are a key.
    pub fn from_bytes(bytes: &[u8; 32]) -> Result<PublicKey, KeyError> {
        Ok(PublicKey(*bytes))
    }

    /// The key's id, the `kid` a lease's header names it by: its RFC 7638
    /// thumbprint, the base64url (no padding) of the SHA-256 of
    /// `{"crv":"Ed25519","kty":"OKP","x":"<x>"}`, with `x` the key's 32
    /// bytes written canonically. Computed from the key itself, so a JWK with
    /// no `kid` member has one all the same.
    pub fn kid(&self) -> String {
        let members = format!(
            r#"{{"crv":"Ed25519","kty":"OKP","x":"{}"}}"#,
            b64url_encode(&self.0)
        );
        b64url_encode(&Sha256::digest(members.as_bytes()))
    }

    /// Whether `signature` is this key's Ed25519 signature over `message`:
    /// 64 bytes, and verified. Bytes that are not a point on the curve verify
    /// nothing.
    pub(crate) fn verifies(&self, message: &[u8], signature: &[u8]) -> bool {
        let Ok(signature) = Signature::from_slice(signature) else {
            return false;
        };
        VerifyingKey::from_bytes(&self.0).is_ok_and(|key| key.verify(message, &signature).is_ok())
    }
}

/// A product's key set, as embedded in the app: the key that signs its
/// leases, and the next one once the product has one staged. A lease naming
/// its key (`kid`) is verified by that key; one naming none by each in turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySet(pub(super) Vec<(String, PublicKey)>);

impl KeySet {
    /// The set from text: a JWK set (`{"keys":[...]}`, as
    /// `GET https://api.keygrant.dev/v1/products/<product>/pubkey` answers
    /// it, and the dashboard copies it), a JSON list of JWKs, or one key as
    /// [`PublicKey::parse`] reads it (a JWK, or its `x` alone): a set of one.
    pub fn parse(text: &str) -> Result<KeySet, KeyError> {
        let trimmed = text.trim();
        if !trimmed.starts_with('{') && !trimmed.starts_with('[') {
            return Ok(KeySet::from(PublicKey::from_x(trimmed)?));
        }
        let value: Value = serde_json::from_str(trimmed)
            .map_err(|e| KeyError(format!("the public key set is not JSON: {e}")))?;
        let list = match &value {
            Value::Array(list) => list,
            Value::Object(map) => match map.get("keys") {
                Some(Value::Array(list)) => list,
                Some(_) => return Err(KeyError("the public key set's keys are not a list".into())),
                None => return Ok(KeySet::from(PublicKey::from_jwk(&value)?)),
            },
            _ => return Err(KeyError("the public key set is not a JWK set".into())),
        };
        let keys = list
            .iter()
            .map(PublicKey::from_jwk)
            .collect::<Result<Vec<_>, _>>()?;
        KeySet::from_keys(keys)
    }

    /// The set of `keys`, in their order, a key given twice kept once. An
    /// empty set is refused: it verifies nothing.
    pub fn from_keys(keys: impl IntoIterator<Item = PublicKey>) -> Result<KeySet, KeyError> {
        let mut set: Vec<(String, PublicKey)> = Vec::new();
        for key in keys {
            let kid = key.kid();
            if !set.iter().any(|(held, _)| *held == kid) {
                set.push((kid, key));
            }
        }
        if set.is_empty() {
            return Err(KeyError("the public key set holds no key".into()));
        }
        Ok(KeySet(set))
    }

    /// The ids of its keys, in its order: what a check-in reports as `kids`.
    pub fn kids(&self) -> Vec<String> {
        self.0.iter().map(|(kid, _)| kid.clone()).collect()
    }

    /// The key of the set whose id is `kid` (matched exactly), if any.
    pub(crate) fn named(&self, kid: &str) -> Option<&PublicKey> {
        self.0
            .iter()
            .find(|(held, _)| held == kid)
            .map(|(_, key)| key)
    }
}

impl From<PublicKey> for KeySet {
    fn from(key: PublicKey) -> KeySet {
        KeySet(vec![(key.kid(), key)])
    }
}

/// The key a lease's header names (`kid`), or none. A header names a key
/// only when it is, byte for byte, the header the server writes for one:
/// canonical base64url of `{"alg":"EdDSA","typ":"JWT","kid":"<kid>"}`,
/// `<kid>` 1 to 64 characters of the base64url alphabet. Any other header
/// names no key. It is matched as bytes, never parsed.
pub(super) fn kid_of(header: &str) -> Option<String> {
    named_kid(header, br#"{"alg":"EdDSA","typ":"JWT","kid":""#)
}

/// The key id a header names when it is, byte for byte, canonical base64url
/// of `<start><kid>"}`, `<kid>` 1 to 64 characters of the base64url
/// alphabet; none for any other header. Matched as bytes, never parsed, so
/// no two JSON readers can disagree about what a header names. A lease's
/// header starts `{"alg":"EdDSA","typ":"JWT","kid":"`, a licence file's
/// `{"alg":"EdDSA","typ":"kglicense+jwt","kid":"`.
pub(crate) fn named_kid(header: &str, start: &[u8]) -> Option<String> {
    let bytes = b64url_decode_canonical(header)?;
    let kid = bytes
        .strip_prefix(start)?
        .strip_suffix(br#""}"#.as_slice())?;
    let base64url = |b: &u8| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_';
    ((1..=64).contains(&kid.len()) && kid.iter().all(base64url))
        .then(|| String::from_utf8_lossy(kid).into_owned())
}
