//! A licence file's claims as a verifier reads them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::lease::{entitlements_of, optional, string, Entitlement, ISSUER};

/// The kind of a licence file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LicenseFileKind {
    /// A key file: it carries the licence key, which the app activates online
    /// exactly as a typed key. Only the delivery differs.
    Key,
    /// An offline licence file: a lease that never expires and is bound to no
    /// device, for machines that never go online.
    Offline,
}

impl LicenseFileKind {
    /// The claim's word for it (`key`, `offline`).
    pub fn as_str(self) -> &'static str {
        match self {
            LicenseFileKind::Key => "key",
            LicenseFileKind::Offline => "offline",
        }
    }
}

/// What an offline licence file covers: the versions and the entitlements.
/// A key file carries none: its key's are the server's to say.
#[derive(Debug, Clone, PartialEq)]
pub struct OfflineTerms {
    /// The highest major it covers ([`OPEN_MAJOR`](crate::lease::OPEN_MAJOR)
    /// for no upper bound).
    pub major: i64,
    /// The lowest major it covers; none is 1.
    pub min_major: Option<i64>,
    /// What it grants beyond the base product, read leniently as a lease's
    /// `ent` is.
    pub ent: BTreeMap<String, Entitlement>,
}

/// A licence file's claims, as the reference's schema reads them: unknown
/// claims ignored, an optional claim absent or of its type (`null` is not
/// absent, but for `ent`), and an integer any whole JSON number (`4.0` is 4).
/// Times are SECONDS since the epoch.
#[derive(Debug, Clone, PartialEq)]
pub struct LicenseFileClaims {
    /// Always `keygrant`.
    pub iss: String,
    /// The file's id: what a build's `revoked_license_files` names it by.
    pub fid: String,
    /// The licence key: the one a key file activates with; the one an
    /// offline file records.
    pub sub: String,
    /// The product slug.
    pub product: String,
    /// Who it is licensed to, by name.
    pub name: Option<String>,
    /// Who it is licensed to, by email address. A file names at least one.
    pub email: Option<String>,
    /// When it was signed (seconds). It never expires.
    pub iat: i64,
    /// True on a test licence's file.
    pub test: Option<bool>,
    /// What an offline file covers; none for a key file, whose versions and
    /// entitlements are not read even when present.
    pub offline: Option<OfflineTerms>,
}

impl LicenseFileClaims {
    /// The file's kind.
    pub fn kind(&self) -> LicenseFileKind {
        if self.offline.is_some() {
            LicenseFileKind::Offline
        } else {
            LicenseFileKind::Key
        }
    }

    /// The claims as JSON, as the reference's schema gives them back: absent
    /// claims left out, an offline file's `ent` always there, a key file's
    /// versions and entitlements never.
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        let mut put = |name: &str, value: Value| {
            map.insert(name.to_owned(), value);
        };
        put("iss", Value::from(self.iss.clone()));
        put("kind", Value::from(self.kind().as_str()));
        put("fid", Value::from(self.fid.clone()));
        put("sub", Value::from(self.sub.clone()));
        put("product", Value::from(self.product.clone()));
        if let Some(name) = &self.name {
            put("name", Value::from(name.clone()));
        }
        if let Some(email) = &self.email {
            put("email", Value::from(email.clone()));
        }
        if let Some(terms) = &self.offline {
            put("major", Value::from(terms.major));
            if let Some(min) = terms.min_major {
                put("minMajor", Value::from(min));
            }
            let ent = terms
                .ent
                .iter()
                .map(|(name, value)| (name.clone(), value.to_json()))
                .collect();
            put("ent", Value::Object(ent));
        }
        put("iat", Value::from(self.iat));
        if let Some(test) = self.test {
            put("test", Value::from(test));
        }
        Value::Object(map)
    }
}

/// The claims as the reference's schema reads them, or none when they are not
/// a licence file's.
pub(super) fn parse_file_claims(value: &Value) -> Option<LicenseFileClaims> {
    let map = value.as_object()?;
    let offline = match map.get("kind")?.as_str()? {
        "key" => None,
        "offline" => Some(OfflineTerms {
            major: whole(map.get("major")?).filter(|n| *n > 0)?,
            min_major: optional(map, "minMajor", |v| whole(v).filter(|n| *n > 0))?,
            // Absent, or anything but an object, is none; never bad claims.
            ent: map.get("ent").map(entitlements_of).unwrap_or_default(),
        }),
        _ => return None,
    };
    let iss = string(map, "iss")?;
    if iss != ISSUER {
        return None;
    }
    let name = optional(map, "name", non_empty)?;
    let email = optional(map, "email", non_empty)?;
    if name.is_none() && email.is_none() {
        return None;
    }
    Some(LicenseFileClaims {
        iss,
        fid: string(map, "fid")?,
        sub: string(map, "sub")?,
        product: string(map, "product")?,
        name,
        email,
        iat: whole(map.get("iat")?).filter(|n| *n >= 0)?,
        test: optional(map, "test", Value::as_bool)?,
        offline,
    })
}

/// A non-empty string.
fn non_empty(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// A JSON number with no fractional value, however it is written (`4.0` is
/// 4). One past the range of an `i64` is held at its end: majors and times are
/// compared as the reference compares its doubles, so no whole number is
/// refused for its size.
fn whole(value: &Value) -> Option<i64> {
    let number = value.as_number()?;
    if let Some(whole) = number.as_i64() {
        return Some(whole);
    }
    let float = number.as_f64()?;
    // `as` saturates at the ends of the range.
    (float.is_finite() && float.fract() == 0.0).then_some(float as i64)
}
