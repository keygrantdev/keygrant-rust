//! The body a claim sends, within the server's bounds on its fingerprints.
//! Re-exported from `pickup.rs`, beside what a claim's answer does.

use serde_json::{json, Map, Value};

use crate::device::{activation_identity, Device};
use crate::status::DeviceKind;
use crate::wire::utf16_len;

/// At most this many fingerprints in a claim's `fingerprints`: the server's bound.
pub const MAX_CLAIM_FINGERPRINTS: usize = 4;

/// At most this many characters (UTF-16 units) in each of a claim's
/// `fingerprints`: the server's bound.
pub const MAX_CLAIM_FINGERPRINT_LENGTH: usize = 200;

/// What a claim is asked with ([`claim_request`]).
#[derive(Debug, Clone, Copy)]
pub struct ClaimAsk<'a> {
    /// The purchase secret.
    pub secret: &'a str,
    /// The device, as this run read it.
    pub device: &'a Device,
    /// The device name, already cut to what the server takes; none when the
    /// app gave none.
    pub name: Option<&'a str>,
    /// This build's major (`major_of`).
    pub major: i64,
    /// From `start_trial()` (a host-name fallback allowed), not `status()`.
    pub start: bool,
}

/// What a claim sends ([`claim_request`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimRequest {
    /// The purchase secret.
    pub token: String,
    /// The fingerprint the claim is made under.
    pub fingerprint: String,
    /// What it hashes, when that is known.
    pub kind: Option<DeviceKind>,
    /// Every fingerprint the run read, within the server's bounds.
    pub fingerprints: Vec<String>,
    /// The device name.
    pub name: Option<String>,
    /// This build's major.
    pub major: i64,
}

impl ClaimRequest {
    /// The body as JSON: `fingerprintKind` only with a kind, `fingerprints`
    /// only when one is left, `name` only when given.
    pub fn to_json(&self) -> Value {
        let mut body = Map::new();
        body.insert("token".into(), json!(self.token));
        body.insert("fingerprint".into(), json!(self.fingerprint));
        if let Some(kind) = self.kind {
            body.insert("fingerprintKind".into(), json!(kind.as_str()));
        }
        if !self.fingerprints.is_empty() {
            body.insert("fingerprints".into(), json!(self.fingerprints));
        }
        if let Some(name) = &self.name {
            body.insert("name".into(), json!(name));
        }
        body.insert("major".into(), json!(self.major));
        Value::Object(body)
    }
}

/// The claim's body, or none when none is sent. The fingerprint is a first
/// activation's (`activation_identity`: the device's own, else the host
/// name), and from `status()` and `refresh()` (`start` false) only the
/// device's own: a claim under the host-name fallback could take the seat
/// under a fingerprint the next ask does not send, so with no own fingerprint
/// nothing is sent (and nothing saved). `fingerprints` is every fingerprint
/// the run read, its own first, then the host name and any other alternates,
/// whether or not the device is held (they only match this claim to an
/// earlier one, and judge no seat), within the server's bounds: those of 1 to
/// [`MAX_CLAIM_FINGERPRINT_LENGTH`] UTF-16 units, never cut (a cut one matches
/// nothing), the first [`MAX_CLAIM_FINGERPRINTS`] of them; left out when none
/// is. A body with too many, or one too long, is refused with an error word,
/// which would drop the secret.
pub fn claim_request(ask: &ClaimAsk<'_>) -> Option<ClaimRequest> {
    if !ask.start && ask.device.own.is_none() {
        return None;
    }
    let (fingerprint, kind) = activation_identity(ask.device, None)?;
    let fingerprints = ask
        .device
        .fingerprints
        .iter()
        .filter(|fingerprint| (1..=MAX_CLAIM_FINGERPRINT_LENGTH).contains(&utf16_len(fingerprint)))
        .take(MAX_CLAIM_FINGERPRINTS)
        .cloned()
        .collect();
    Some(ClaimRequest {
        token: ask.secret.to_owned(),
        fingerprint,
        kind,
        fingerprints,
        name: ask.name.map(str::to_owned),
        major: ask.major,
    })
}
