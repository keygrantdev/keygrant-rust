//! Key pickup, decided without I/O: what a pickup call sends for what a person
//! typed, the error each refusal of the server is, the keys a confirm hands
//! back (read leniently), and the key a restore activates. A build that may not
//! take a typed key (an App Store build) restores a web buyer's licence by the
//! address the key was sold to: [`License::pick_up`](crate::License::pick_up)
//! asks for a code, [`License::pick_up_with_code`](crate::License::pick_up_with_code)
//! trades it for the address's keys, and
//! [`License::restore_by_email`](crate::License::restore_by_email) activates the
//! one that fits. The machinery that makes the requests is `license/pickup.rs`.
//!
//! A pickup never takes a licence away and stores nothing.

use std::collections::BTreeMap;
use std::fmt;
use std::io;

use serde::{Serialize, Serializer};
use serde_json::{Map, Value};

use crate::error::ActivateError;
use crate::lease::{entitlements_of, Entitlement};
use crate::meters::figure_of;
use crate::status::LicenseError;
use crate::wire::{entered_code, entered_email, utf16_len, MAX_KEY};

/// Why a pickup call did not do what it asked.
///
/// Serialises as the error's word (`"wrong-code"`, `"too-many-attempts"`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PickupError {
    /// The address is blank or longer than any address: nothing was asked.
    InvalidEmail,
    /// The code is not six digits (nothing was asked, so the address lost none
    /// of its wrong tries), or it is not the address's live code.
    WrongCode,
    /// The address has no live code: it expired (10 minutes), was used, was
    /// burnt by its fifth wrong try, or was never asked for. Ask for a new one with `pick_up`.
    CodeExpired,
    /// Thirty wrong codes for the address this hour, or too many asks or confirms
    /// from this network. [`PickupRefusal::retry_after`] says when to try again.
    TooManyAttempts,
    /// The product's Key pickup is off: offer another way (support).
    PickupDisabled,
    /// A count could not be made just now: retry. [`PickupRefusal::retry_after`]
    /// says when, when the server did.
    TryAgain,
    /// No answer, or an answer that is none of a pickup's (a proxy's page, an
    /// unknown product, a 200 that holds no keys).
    Network,
}

impl PickupError {
    /// The word the Electron SDK uses for it (`invalid-email`, `wrong-code`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            PickupError::InvalidEmail => "invalid-email",
            PickupError::WrongCode => "wrong-code",
            PickupError::CodeExpired => "code-expired",
            PickupError::TooManyAttempts => "too-many-attempts",
            PickupError::PickupDisabled => "pickup-disabled",
            PickupError::TryAgain => "try-again",
            PickupError::Network => "network",
        }
    }
}

impl fmt::Display for PickupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for PickupError {}

/// A pickup call that did not do what it asked: the error and, for
/// [`TooManyAttempts`](PickupError::TooManyAttempts) and
/// [`TryAgain`](PickupError::TryAgain), the seconds the server said to wait,
/// when it said (read from its body, never a header).
///
/// Serialises as the Electron SDK's result: `{"error":"too-many-attempts","retryAfter":3600}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PickupRefusal {
    /// What went wrong.
    pub error: PickupError,
    /// The seconds to wait before asking again, when the server gave them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<u64>,
}

impl PickupRefusal {
    /// A refusal of `error` that names no wait.
    pub fn of(error: PickupError) -> Self {
        PickupRefusal {
            error,
            retry_after: None,
        }
    }
}

impl From<PickupError> for PickupRefusal {
    fn from(error: PickupError) -> Self {
        PickupRefusal::of(error)
    }
}

impl fmt::Display for PickupRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.retry_after {
            Some(seconds) => write!(f, "key pickup refused: {} (wait {seconds} s)", self.error),
            None => write!(f, "key pickup refused: {}", self.error),
        }
    }
}

impl std::error::Error for PickupRefusal {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// How long (ms) the keys a confirm answered are kept in memory, never stored,
/// for a confirm or a restore with the same address and code: a code's own life.
/// The first confirm spends the code, so a restore whose activation got no
/// answer, or a restore after `pick_up_with_code`, needs no new one.
pub const PICKUP_KEYS_KEEP_MS: i64 = 10 * 60_000;

/// The keys the last confirm answered, with the address and code as sent and
/// when (the clock adapter, ms), kept in memory by a `License`.
#[derive(Debug, Clone)]
pub struct KeptKeys {
    email: String,
    code: String,
    keys: Vec<PickedKey>,
    at: i64,
}

impl KeptKeys {
    /// Keep `keys`, answered for `ask` at `at`.
    pub fn new(ask: &PickupAsk, keys: &[PickedKey], at: i64) -> Self {
        KeptKeys {
            email: ask.email.clone(),
            code: ask.code.clone().unwrap_or_default(),
            keys: keys.to_vec(),
            at,
        }
    }

    /// The keys, when they were answered for `ask` and are younger than
    /// [`PICKUP_KEYS_KEEP_MS`] at `now`.
    pub fn for_ask(&self, ask: &PickupAsk, now: i64) -> Option<Vec<PickedKey>> {
        let age = now.saturating_sub(self.at);
        let same = self.email == ask.email && Some(self.code.as_str()) == ask.code.as_deref();
        (same && (0..PICKUP_KEYS_KEEP_MS).contains(&age)).then(|| self.keys.clone())
    }
}

/// What a pickup call sends for what a person typed: the address (trimmed) and,
/// for the confirm, the code (spaces dropped, six ASCII digits).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickupAsk {
    /// The address, trimmed.
    pub email: String,
    /// The code; none for the ask for a code.
    pub code: Option<String>,
}

/// What a pickup call sends for what a person typed, or the error that refuses
/// it before anything is sent. `code` none is the ask for a code
/// ([`pick_up`](crate::License::pick_up)); a code, even a blank one, is a
/// confirm ([`pick_up_with_code`](crate::License::pick_up_with_code)).
///
/// The address is entered as an activation's is: trimmed, and a blank one, or
/// one longer than any address, is [`InvalidEmail`](PickupError::InvalidEmail).
/// The code is entered as an activation's is: ends trimmed, spaces dropped, and
/// a blank one, or one that is not six ASCII digits, is
/// [`WrongCode`](PickupError::WrongCode), refused here without costing the
/// address one of its wrong tries. The address is judged first.
pub fn pickup_ask(email: &str, code: Option<&str>) -> Result<PickupAsk, PickupError> {
    let Ok(Some(email)) = entered_email(Some(email)) else {
        return Err(PickupError::InvalidEmail);
    };
    let Some(code) = code else {
        return Ok(PickupAsk { email, code: None });
    };
    let Ok(Some(code)) = entered_code(Some(code)) else {
        return Err(PickupError::WrongCode);
    };
    Ok(PickupAsk {
        email,
        code: Some(code),
    })
}

/// The error a refused pickup call is, from the server's body (`json`, as
/// parsed; none for no body): each pickup word as itself; the per-network limit
/// (`rate-limited`) as `TooManyAttempts`; a count that could not be made
/// (`unavailable`) as `TryAgain`; a body the server refused to read
/// (`invalid-request`) as `InvalidEmail`, since what a person typed is all a
/// pickup sends; and anything else, a proxy's page or an unknown product
/// included, as `Network`. The status is never read. `retry_after` is kept, on
/// the two errors that wait, when the body gives it as a whole number of seconds.
pub fn pickup_refusal(json: Option<&Value>) -> PickupRefusal {
    let body = json.and_then(Value::as_object);
    let error = match body
        .and_then(|body| body.get("error"))
        .and_then(Value::as_str)
    {
        Some("pickup-disabled") => PickupError::PickupDisabled,
        Some("wrong-code") => PickupError::WrongCode,
        Some("code-expired") => PickupError::CodeExpired,
        Some("too-many-attempts") | Some("rate-limited") => PickupError::TooManyAttempts,
        Some("try-again") | Some("unavailable") => PickupError::TryAgain,
        Some("invalid-request") => PickupError::InvalidEmail,
        _ => PickupError::Network,
    };
    let waits = matches!(error, PickupError::TooManyAttempts | PickupError::TryAgain);
    let retry_after = body
        .and_then(|body| body.get("retryAfter"))
        .and_then(figure_of)
        .filter(|_| waits);
    PickupRefusal { error, retry_after }
}

/// The versions a picked key covers: from `min_major` up to `max_major` (none:
/// no top), and every major from `min_major` up while `upgrade_until` (ms) is
/// ahead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PickedVersions {
    /// The lowest major the key covers.
    pub min_major: i64,
    /// The highest major it covers, none for no top.
    pub max_major: Option<i64>,
    /// When (ms) its upgrade window ends, none for no window.
    pub upgrade_until: Option<i64>,
}

/// The sale a picked key came with.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct PickedSale {
    /// The sale's id.
    pub id: String,
    /// This key's place in the sale, from 1.
    pub index: u64,
    /// How many keys the sale made, from 1.
    pub size: u64,
}

/// One key a pickup handed back: enough for the app to let the customer pick
/// one (by [`model_name`](PickedKey::model_name)) or to let
/// [`restore_by_email`](crate::License::restore_by_email) choose and activate
/// one. Never show the key, nor ask the customer to type anything but the
/// address and the code.
///
/// Serialises as the Electron SDK's `PickedKey`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PickedKey {
    /// The licence key. Activate it with `activate_with(&key, &options)`, where
    /// `options` is `ActivateOptions::new().email(address)` with `.code(c.clone())`
    /// added when `activation_code` is `Some(c)`.
    pub key: String,
    /// The licence model's kind (`perpetual`, `subscription`), empty when the
    /// server did not say.
    pub model: String,
    /// The licence model's name, to show.
    pub model_name: Option<String>,
    /// The key's state (`active`, ...), empty when the server did not say.
    pub state: String,
    /// What the key grants, read as a lease's `ent` is.
    pub entitlements: BTreeMap<String, Entitlement>,
    /// When (ms) the key ends, none for a key that does not.
    pub expires_at: Option<i64>,
    /// The versions the key covers. None is a key sold without a version and
    /// not yet bound: its first activation binds it to the build's major, so it
    /// covers any build.
    pub versions: Option<PickedVersions>,
    /// How many devices the key allows.
    pub device_limit: u64,
    /// How many devices hold a seat now.
    pub active_devices: u64,
    /// True when this device already holds a seat on the key (a reinstall takes
    /// it back).
    pub held: bool,
    /// The sale the key came with, when it came with one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sale: Option<PickedSale>,
    /// The code a user-locked key's activation takes, already proven by the
    /// pickup (the address's code is all a lock's code proves).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation_code: Option<String>,
}

/// The keys a confirm's 200 answers, read leniently, in the order given; none
/// when the body is not a pickup's answer at all (not an object, or `keys` not
/// a list), which the SDK takes as no answer ([`PickupError::Network`]). A
/// member that is not an object with a `key` (text, 1 to
/// [`MAX_KEY`] UTF-16 units) is left out; every other field falls back when it
/// is not what it should be.
pub fn picked_keys_of(json: Option<&Value>) -> Option<Vec<PickedKey>> {
    let keys = json?.as_object()?.get("keys")?.as_array()?;
    Some(keys.iter().filter_map(picked_key).collect())
}

/// One key of a confirm's answer, or none when it has no key.
fn picked_key(raw: &Value) -> Option<PickedKey> {
    let map = raw.as_object()?;
    let key = map.get("key")?.as_str()?;
    if !(1..=MAX_KEY).contains(&utf16_len(key)) {
        return None;
    }
    let text = |name: &str| map.get(name).and_then(Value::as_str).map(str::to_owned);
    let whole = |name: &str| map.get(name).and_then(figure_of);
    Some(PickedKey {
        key: key.to_owned(),
        model: text("model").unwrap_or_default(),
        model_name: text("modelName"),
        state: text("state").unwrap_or_default(),
        entitlements: map
            .get("entitlements")
            .map_or_else(BTreeMap::new, entitlements_of),
        expires_at: whole("expiresAt").map(as_i64),
        versions: map.get("versions").and_then(versions_of),
        device_limit: whole("deviceLimit").unwrap_or(0),
        active_devices: whole("activeDevices").unwrap_or(0),
        held: map.get("held") == Some(&Value::Bool(true)),
        sale: map.get("sale").and_then(sale_of),
        activation_code: text("activationCode").filter(|code| is_code(code)),
    })
}

/// A whole number the reader keeps (at most 2^53 - 1) as a time or a major.
fn as_i64(whole: u64) -> i64 {
    i64::try_from(whole).unwrap_or(i64::MAX)
}

/// Exactly six ASCII digits.
fn is_code(code: &str) -> bool {
    code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit())
}

/// A key's versions as read: none for a key not yet bound and for anything that
/// is not an object with a whole `minMajor`; else `minMajor`, and `maxMajor`
/// and `upgradeUntil` each a whole number or none.
fn versions_of(raw: &Value) -> Option<PickedVersions> {
    let map = raw.as_object()?;
    let whole = |name: &str| map.get(name).and_then(figure_of).map(as_i64);
    Some(PickedVersions {
        min_major: whole("minMajor")?,
        max_major: whole("maxMajor"),
        upgrade_until: whole("upgradeUntil"),
    })
}

/// A key's sale, kept only with a text `id` and whole `index` and `size` of at
/// least 1.
fn sale_of(raw: &Value) -> Option<PickedSale> {
    let map = raw.as_object()?;
    let at_least_one = |name: &str| map.get(name).and_then(figure_of).filter(|n| *n >= 1);
    Some(PickedSale {
        id: map.get("id")?.as_str()?.to_owned(),
        index: at_least_one("index")?,
        size: at_least_one("size")?,
    })
}

/// Whether a key covers the build's `major` at `now` (ms): a key not yet bound
/// always does; else from `min_major` up to `max_major` (none: no top), and
/// every major from `min_major` up while `upgrade_until` is after `now`.
pub fn covers(versions: Option<&PickedVersions>, major: i64, now: i64) -> bool {
    let Some(versions) = versions else {
        return true;
    };
    if major < versions.min_major {
        return false;
    }
    versions.max_major.is_none_or(|max| major <= max)
        || versions.upgrade_until.is_some_and(|until| until > now)
}

/// Why no picked key is activated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Unchosen {
    /// The address holds no key of the product.
    NoKey,
    /// The keys are for an older major than the build: buy an upgrade.
    Upgrade,
    /// The keys' lowest version is newer than the build: update the app.
    Outdated,
}

impl Unchosen {
    /// The word for it (`no-key`, `upgrade`, `outdated`).
    pub fn as_str(self) -> &'static str {
        match self {
            Unchosen::NoKey => "no-key",
            Unchosen::Upgrade => "upgrade",
            Unchosen::Outdated => "outdated",
        }
    }
}

/// The key [`restore_by_email`](crate::License::restore_by_email) activates, by
/// its place in `keys` (the server's order: first sold first), or why none is.
/// Of the keys that cover the build's `major`: the first this device already
/// holds a seat on (`held`, a reinstall takes that seat back), else the first
/// with a free seat, else the first (its activation answers `limit`). No key at
/// all is [`NoKey`](Unchosen::NoKey); none that covers the build is
/// [`Upgrade`](Unchosen::Upgrade) when one of them is for an older major than the
/// build (buy an upgrade), else [`Outdated`](Unchosen::Outdated) (update the app).
pub fn chosen_key(keys: &[PickedKey], major: i64, now: i64) -> Result<usize, Unchosen> {
    if keys.is_empty() {
        return Err(Unchosen::NoKey);
    }
    let covering = || {
        keys.iter()
            .enumerate()
            .filter(|(_, key)| covers(key.versions.as_ref(), major, now))
    };
    let pick = covering()
        .find(|(_, key)| key.held)
        .or_else(|| covering().find(|(_, key)| key.active_devices < key.device_limit))
        .or_else(|| covering().next());
    if let Some((index, _)) = pick {
        return Ok(index);
    }
    let older = keys
        .iter()
        .any(|key| key.versions.is_some_and(|v| v.min_major <= major));
    Err(if older {
        Unchosen::Upgrade
    } else {
        Unchosen::Outdated
    })
}

/// Why [`restore_by_email`](crate::License::restore_by_email) did not activate
/// a key: the confirm's error, no key to activate, or what `activate` answers.
///
/// Serialises as the Electron SDK's result: `{"error":"no-key"}`, with
/// `retryAfter` when a pickup refusal carried one.
#[derive(Debug)]
#[non_exhaustive]
pub enum RestoreError {
    /// The code was not taken (see [`PickupError`]).
    Pickup(PickupRefusal),
    /// The address holds no key of the product.
    NoKey,
    /// What activating the key answers (see [`LicenseError`]), or the keys' own
    /// `Upgrade` (they are for an older major than this build: buy an upgrade)
    /// and `Outdated` (their lowest version is newer: update the app), with
    /// nothing activated for those two.
    License(LicenseError),
    /// The licence state could not be read or saved: nothing was changed on
    /// this device; try again.
    Storage(io::Error),
}

impl RestoreError {
    /// The error's word: a [`PickupError`]'s, a [`LicenseError`]'s, `no-key` or `storage`.
    pub fn code(&self) -> &'static str {
        match self {
            RestoreError::Pickup(refusal) => refusal.error.as_str(),
            RestoreError::NoKey => "no-key",
            RestoreError::License(error) => error.as_str(),
            RestoreError::Storage(_) => "storage",
        }
    }

    /// The seconds to wait before asking again, when a pickup refusal said.
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            RestoreError::Pickup(refusal) => refusal.retry_after,
            _ => None,
        }
    }
}

impl From<PickupRefusal> for RestoreError {
    fn from(refusal: PickupRefusal) -> Self {
        RestoreError::Pickup(refusal)
    }
}

impl From<ActivateError> for RestoreError {
    fn from(error: ActivateError) -> Self {
        match error {
            ActivateError::License(error) => RestoreError::License(error),
            ActivateError::Storage(error) => RestoreError::Storage(error),
        }
    }
}

impl From<Unchosen> for RestoreError {
    fn from(unchosen: Unchosen) -> Self {
        match unchosen {
            Unchosen::NoKey => RestoreError::NoKey,
            Unchosen::Upgrade => RestoreError::License(LicenseError::Upgrade),
            Unchosen::Outdated => RestoreError::License(LicenseError::Outdated),
        }
    }
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RestoreError::Pickup(refusal) => refusal.fmt(f),
            RestoreError::NoKey => f.write_str("the address holds no key of this product"),
            RestoreError::License(error) => write!(f, "activation failed: {error}"),
            RestoreError::Storage(error) => {
                write!(f, "the licence state could not be read or saved: {error}")
            }
        }
    }
}

impl std::error::Error for RestoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RestoreError::Pickup(refusal) => Some(refusal),
            RestoreError::License(error) => Some(error),
            RestoreError::Storage(error) => Some(error),
            RestoreError::NoKey => None,
        }
    }
}

impl Serialize for RestoreError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut result = Map::new();
        result.insert("error".into(), Value::from(self.code()));
        if let Some(seconds) = self.retry_after() {
            result.insert("retryAfter".into(), Value::from(seconds));
        }
        result.serialize(serializer)
    }
}
