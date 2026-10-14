//! A lease's claims as the server's schema reads them, and its entitlements.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};

use super::ISSUER;
use crate::status::DeviceKind;
use crate::wire::utf16_len;

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
    pub(crate) fn to_json(&self) -> Value {
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
    /// The highest major it covers ([`OPEN_MAJOR`](super::OPEN_MAJOR) for no upper bound); 0 when absent.
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

/// Whether entitlements grant `code`: a flag that is on, a number that is not
/// zero, or a text that is not empty. Absent, off, zero or empty is not
/// granted. The code is matched exactly, case included.
pub fn grants(entitlements: &BTreeMap<String, Entitlement>, code: &str) -> bool {
    match entitlements.get(code) {
        Some(Entitlement::Bool(flag)) => *flag,
        Some(Entitlement::Number(number)) => *number != 0.0,
        Some(Entitlement::Text(text)) => !text.is_empty(),
        None => false,
    }
}

/// The claims as the reference's schema reads them, or none when they are not a lease's.
pub(super) fn parse_claims(value: &Value) -> Option<LeaseClaims> {
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
pub(crate) fn string(map: &Map<String, Value>, name: &str) -> Option<String> {
    map.get(name)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// An optional claim: `Some(None)` when absent, `None` (bad claims) when
/// present and not what `read` accepts (`null` included).
pub(crate) fn optional<T>(
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
