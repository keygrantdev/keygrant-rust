//! What the SDK says of a licence: its state, why, and the words the server
//! and the SDK use for a refusal. Every type here serialises to the same JSON
//! as the Electron SDK's, so a Tauri command can hand it to the front end as is.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::lease::Entitlement;

/// What [`License::status`](crate::License::status) says of the licence.
///
/// `Unknown` (reason [`LicenseReason::Storage`]): the state file could not
/// be read just now (an antivirus scan holding it, say). Nothing is known
/// either way: keep running, and ask again shortly. Never show the activation
/// screen for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LicenseState {
    /// A valid lease licenses this build on this device.
    Licensed,
    /// A trial is running.
    Trial,
    /// The lease ran out and could not be renewed (reason `offline`), or the
    /// trial ended (reason `trial`).
    Expired,
    /// The licence does not license this build or this device (see the reason).
    Invalid,
    /// No licence and no trial.
    Unlicensed,
    /// The state file could not be read just now (reason `storage`).
    Unknown,
}

/// Why a licence does not license this build.
///
/// `Upgrade`: the build is NEWER than the key covers (buy an upgrade).
/// `Outdated`: the build is OLDER than the key's lowest version (update the
/// app; the key is for a later version). `Device`: the licence was activated
/// on another device (this machine changed, or the state file was copied
/// from another one); activate again on this one. `Tampered`: the lease is not
/// for this licence at all (another key or activation, another product, a bad
/// signature), or the stored licence file is not one at all (the state file was
/// edited). `UnknownKey`: the lease is signed by a key this build does not
/// carry; asked online, the server signs one with the key it uses now. A
/// stored licence file signed by such a key needs the app's update that
/// carries it, or the same licence's file signed again. On a status from a
/// licence file (`source` [`LicenseSource::File`]), `Upgrade` and `Outdated`
/// are the file's range of versions, which no renewal changes.
/// `Storage`: the state file could not be read just now (state `Unknown`).
/// `RevokedFile`: the stored offline licence file is one this build lists as
/// revoked ([`LicenseConfig::revoked_license_files`](crate::LicenseConfig::revoked_license_files)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LicenseReason {
    /// The key was revoked or expired, or this device was released.
    Revoked,
    /// This build is newer than the key covers.
    Upgrade,
    /// This build is older than the key's lowest version.
    Outdated,
    /// The lease ran out while the server could not be reached.
    Offline,
    /// The lease is not for this licence (another key, activation or product, or a bad signature),
    /// or the stored licence file is not one (the state file was edited).
    Tampered,
    /// The licence was activated on another device.
    Device,
    /// The trial has ended.
    Trial,
    /// The state file could not be read just now.
    Storage,
    /// The lease, or the stored licence file, is signed by a key this build does not carry.
    #[serde(rename = "unknown-key")]
    UnknownKey,
    /// The stored offline licence file is one this build lists as revoked.
    #[serde(rename = "revoked-file")]
    RevokedFile,
}

/// What a status rests on, when it is not a key's lease or a trial: a stored
/// offline licence file ([`License::import_license_file`](crate::License::import_license_file)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum LicenseSource {
    /// A stored offline licence file. It holds no seat: offer "Remove licence
    /// file" ([`License::remove_license_file`](crate::License::remove_license_file))
    /// rather than deactivating.
    File,
}

/// Who a licence file is licensed to, as it signs it: a name, an email
/// address, or both. Show it: "Licensed to Ada Lovelace".
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LicensedTo {
    /// The name, when the file names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The email address, when the file names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

/// The licence's status, as `status()`, `refresh()` and `start_trial()` answer it.
///
/// Serialises exactly as the Electron SDK's `LicenseStatus`: camelCase, absent
/// fields left out, `test` / `stalled` present only when true,
/// `entitlements` only when there are any, and `source` / `licensedTo` only
/// on a status from a stored licence file.
///
/// Two statuses are equal when every field is, the entitlements compared by
/// content (names and values, numbers as numbers). That equality is still a
/// sound `Eq` with a double among the entitlements: [`Entitlement`]'s
/// equality treats a NaN as equal to itself, so it stays an equivalence
/// relation, and a lease never carries a NaN anyway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LicenseStatus {
    /// What the licence is.
    pub state: LicenseState,
    /// Why, when the state needs one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<LicenseReason>,
    /// The licence key this device holds, when it holds one; on a status from a
    /// licence file that is `Licensed`, or `Invalid` with `RevokedFile`,
    /// `Upgrade` or `Outdated`, the key the file records; none on any other
    /// status from a file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Whole days left of a running trial, when its end is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trial_days_left: Option<i64>,
    /// When a running trial ends (ms since the epoch), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trial_ends_at: Option<i64>,
    /// True when the licence is a TEST licence: one the developer made to
    /// exercise their app, which lapses on its own within 30 days (an offline
    /// licence file made for one never does: it is a file). It licenses the app
    /// like any other; check this to refuse test licences in a release build.
    #[serde(default, skip_serializing_if = "is_false")]
    pub test: bool,
    /// What the licence grants beyond the base product, by name, as its lease
    /// signs them (the `ent` claim):
    /// `status.entitlements.get("edition").and_then(Entitlement::as_str) == Some("pro")`.
    ///
    /// On a `Licensed` status, from the licence's lease, or the stored offline
    /// licence file's `ent`; on a `Trial` status, from the trial's. Empty on
    /// every other status (a stored refusal
    /// included, whatever the lease held grants), and when the lease grants
    /// none. A member of the claim that is not a flag, a number or a text is
    /// left out, and never costs the licence. A copy: changing it changes
    /// nothing the next status says.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub entitlements: BTreeMap<String, Entitlement>,
    /// [`LicenseSource::File`] when the status rests on a stored offline
    /// licence file, licensed or not; none for a key's lease, a trial and no
    /// licence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<LicenseSource>,
    /// Who the stored offline licence file is licensed to, on a file status
    /// that is `Licensed`, or `Invalid` with `RevokedFile`, `Upgrade` or
    /// `Outdated`; none otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub licensed_to: Option<LicensedTo>,
    /// True when an earlier call on this `License` has not finished (an
    /// adapter stuck on storage, say), so this answer comes from what is
    /// stored, with no ask and no write. A restart clears it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stalled: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl LicenseStatus {
    /// A status in `state`, with nothing else said.
    pub fn new(state: LicenseState) -> Self {
        LicenseStatus {
            state,
            reason: None,
            key: None,
            trial_days_left: None,
            trial_ends_at: None,
            test: false,
            entitlements: BTreeMap::new(),
            source: None,
            licensed_to: None,
            stalled: false,
        }
    }

    pub(crate) fn with_reason(mut self, reason: LicenseReason) -> Self {
        self.reason = Some(reason);
        self
    }

    pub(crate) fn with_key(mut self, key: Option<&str>) -> Self {
        self.key = key.map(str::to_owned);
        self
    }

    /// Whether the app should run as a paid or trial app: `Licensed` or
    /// `Trial`. A licensed TEST licence counts; check [`test`](Self::test) to
    /// refuse those in a release build.
    pub fn is_entitled(&self) -> bool {
        matches!(self.state, LicenseState::Licensed | LicenseState::Trial)
    }

    /// Whether to keep the app running rather than show the activation screen:
    /// entitled, or `Unknown` (the state file could not be read just now,
    /// which says nothing either way; ask again shortly).
    pub fn permits_use(&self) -> bool {
        self.is_entitled() || self.state == LicenseState::Unknown
    }
}

/// What [`License::deactivate`](crate::License::deactivate) answers once it
/// has dropped the local licence: whether the seat is now free.
///
/// Serialises as the Electron SDK's `DeactivateResult`: `{"released": true}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeactivateResult {
    /// True when KeyGrant confirmed the release (a 200), or when this device
    /// held no activation to release. False when it could not be asked, or
    /// answered anything else (a 5xx, a 429, a proxy's page): the seat may
    /// still be held, and the app should say so (the developer's dashboard, or
    /// support, can release it), not tell the customer it is free.
    pub released: bool,
}

/// Why an activation did not succeed: the Electron SDK's `LicenseError` words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LicenseError {
    /// The key is not one the server knows (or not a key at all).
    InvalidKey,
    /// The key is on as many devices as it allows.
    Limit,
    /// The key was revoked or has expired.
    Revoked,
    /// This build is newer than the key covers.
    Upgrade,
    /// The key is for a later version than this build: update the app.
    Outdated,
    /// The server could not be reached, or did not answer in a way the SDK
    /// could use: try again.
    Network,
    /// The server's lease failed offline verification (an integrity or
    /// configuration error: the embedded public key is not the product's).
    InvalidLease,
}

impl LicenseError {
    /// The word the Electron SDK uses for it (`invalid-key`, `limit`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            LicenseError::InvalidKey => "invalid-key",
            LicenseError::Limit => "limit",
            LicenseError::Revoked => "revoked",
            LicenseError::Upgrade => "upgrade",
            LicenseError::Outdated => "outdated",
            LicenseError::Network => "network",
            LicenseError::InvalidLease => "invalid-lease",
        }
    }
}

impl fmt::Display for LicenseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for LicenseError {}

/// A definitive refusal from the server, as [`StoredState::refused`](crate::StoredState::refused) keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Refusal {
    /// About the key: revoked, expired, or this device released.
    Revoked,
    /// About this build: newer than the key covers.
    Upgrade,
    /// About this build: older than the key's lowest version.
    Outdated,
    /// About this device: the seat is another machine's.
    Device,
}

impl Refusal {
    /// The word stored for it.
    pub fn as_str(self) -> &'static str {
        match self {
            Refusal::Revoked => "revoked",
            Refusal::Upgrade => "upgrade",
            Refusal::Outdated => "outdated",
            Refusal::Device => "device",
        }
    }

    pub(crate) fn parse(word: &str) -> Option<Refusal> {
        match word {
            "revoked" => Some(Refusal::Revoked),
            "upgrade" => Some(Refusal::Upgrade),
            "outdated" => Some(Refusal::Outdated),
            "device" => Some(Refusal::Device),
            _ => None,
        }
    }

    /// The reason a status gives for it.
    pub fn reason(self) -> LicenseReason {
        match self {
            Refusal::Revoked => LicenseReason::Revoked,
            Refusal::Upgrade => LicenseReason::Upgrade,
            Refusal::Outdated => LicenseReason::Outdated,
            Refusal::Device => LicenseReason::Device,
        }
    }
}

/// What a fingerprint hashes: the operating system's machine id, or the host name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    /// The operating system's own id for this machine.
    Machine,
    /// The host name, platform and architecture.
    Hostname,
}

impl DeviceKind {
    /// The word stored and sent for it.
    pub fn as_str(self) -> &'static str {
        match self {
            DeviceKind::Machine => "machine",
            DeviceKind::Hostname => "hostname",
        }
    }

    pub(crate) fn parse(word: &str) -> Option<DeviceKind> {
        match word {
            "machine" => Some(DeviceKind::Machine),
            "hostname" => Some(DeviceKind::Hostname),
            _ => None,
        }
    }
}
