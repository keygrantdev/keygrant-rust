//! The types of metered uses: what a spend answers, what a status shows of a
//! meter, and what the stored state keeps of them.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::status::Refusal;

/// One meter of a key as [`LicenseStatus::meters`](crate::LicenseStatus::meters)
/// shows it. Every figure is a whole number; the server signs none past
/// 1,000,000,000.
///
/// Serialises as the Electron SDK's `MeterStatus`: camelCase, `resets` always there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeterStatus {
    /// What is left: the newest figure (the lease's, or the last answer to a
    /// spend), less what this device spent and has not reported. Never below 0.
    pub left: u64,
    /// The uses the key holds, for good or for this billing period.
    pub total: u64,
    /// How many uses this device may spend while KeyGrant cannot be reached.
    pub offline: u64,
    /// Uses this device spent that KeyGrant has not heard of yet.
    pub unsynced: u64,
    /// True for a meter that comes back to its full uses each billing period.
    pub resets: bool,
}

/// Why [`License::use_meter`](crate::License::use_meter) did not spend. A
/// refusal is an answer, never a panic or an `Err`.
///
/// Serialises as its word (`"used-up"`, `"offline-limit"`, ...), so a Tauri
/// command can return it to the front end as is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UseError {
    /// `n` is not a whole number from 1 to 1,000,000,000: a bug in the app.
    /// Nothing was asked.
    InvalidAmount,
    /// The key has no meter of that code: the server's word; or, with no answer,
    /// no figure on this device names it; or a code no meter can have (empty,
    /// or longer than 32 UTF-16 units).
    NoMeter,
    /// Online: fewer left than asked. [`UseResult::left`] says how many, when
    /// the server did.
    UsedUp,
    /// KeyGrant could not be reached, and the spend is past this device's
    /// offline allowance for the meter, or past what it knows is left (every
    /// spend, for an allowance of 0). [`UseResult::left`] says how many.
    OfflineLimit,
    /// No key's lease licenses this build on this device (a device trial and an
    /// offline licence file have no meters). Nothing was asked: read `status()`.
    NotLicensed,
    /// The state could not be read just now, or an adapter panicked where there
    /// is no word for it: try again shortly.
    Storage,
    /// The server's verdict on the key, stored as a renewal's is.
    Revoked,
    /// The server's verdict on this build: the key does not cover it.
    Upgrade,
    /// The server's verdict on this build: the key's lowest version is newer.
    Outdated,
    /// The server's verdict on this device: the seat is another device's.
    Device,
}

impl UseError {
    /// The word the Electron SDK uses for it (`invalid-amount`, `used-up`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            UseError::InvalidAmount => "invalid-amount",
            UseError::NoMeter => "no-meter",
            UseError::UsedUp => "used-up",
            UseError::OfflineLimit => "offline-limit",
            UseError::NotLicensed => "not-licensed",
            UseError::Storage => "storage",
            UseError::Revoked => "revoked",
            UseError::Upgrade => "upgrade",
            UseError::Outdated => "outdated",
            UseError::Device => "device",
        }
    }

    /// The error a server verdict is answered as.
    pub fn of_refusal(refusal: Refusal) -> UseError {
        match refusal {
            Refusal::Revoked => UseError::Revoked,
            Refusal::Upgrade => UseError::Upgrade,
            Refusal::Outdated => UseError::Outdated,
            Refusal::Device => UseError::Device,
        }
    }
}

impl fmt::Display for UseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for UseError {}

fn is_false(value: &bool) -> bool {
    !*value
}

/// What [`License::use_meter`](crate::License::use_meter) did: spent, or why not.
///
/// Serialises exactly as the Electron SDK's `UseResult`: `{"ok":true,"left":89}`,
/// `{"ok":true,"left":88,"offline":true}` or `{"ok":false,"error":"used-up","left":1}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UseResult {
    /// Whether the uses were spent: do the work they count.
    pub ok: bool,
    /// What is left of the meter now, net of this device's unsynced spends.
    /// Given when `ok`, and on a refusal for [`UseError::UsedUp`] (the server's
    /// word, when it gave one) and [`UseError::OfflineLimit`]; none otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left: Option<u64>,
    /// True when KeyGrant could not be asked and the spend came from this
    /// device's offline allowance (it is reported with the next request that
    /// can carry it). Present only when true.
    #[serde(default, skip_serializing_if = "is_false")]
    pub offline: bool,
    /// Why not, when `ok` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<UseError>,
}

impl UseResult {
    /// The uses were spent, `left` are left; `offline` when from the allowance.
    pub fn spent(left: u64, offline: bool) -> UseResult {
        UseResult {
            ok: true,
            left: Some(left),
            offline,
            error: None,
        }
    }

    /// The uses were not spent, for `error`; `left` where the error gives one.
    pub fn refused(error: UseError, left: Option<u64>) -> UseResult {
        UseResult {
            ok: false,
            left,
            offline: false,
            error: Some(error),
        }
    }
}

/// Which kind of entry a [`SpendEntry`] is in the journal of unsynced spends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpendKind {
    /// Spends made with no request sent: one entry for each meter, its count
    /// growing with each later such spend.
    Offline,
    /// One spend whose own request got no answer, made from the allowance. Its
    /// count never changes: the server may hold it.
    Unanswered,
    /// One spend whose own request got no answer and that was refused offline,
    /// reported so that the server takes it back if it recorded it.
    Void,
}

impl SpendKind {
    /// The word stored for it.
    pub fn as_str(self) -> &'static str {
        match self {
            SpendKind::Offline => "offline",
            SpendKind::Unanswered => "unanswered",
            SpendKind::Void => "void",
        }
    }

    /// The kind a stored word names.
    pub fn parse(word: &str) -> Option<SpendKind> {
        match word {
            "offline" => Some(SpendKind::Offline),
            "unanswered" => Some(SpendKind::Unanswered),
            "void" => Some(SpendKind::Void),
            _ => None,
        }
    }
}

/// A spend this device made that KeyGrant has not confirmed (the journal,
/// [`StoredState::spends`](crate::StoredState::spends)): kept in the stored
/// state, reported with the next request, and dropped only when the server
/// says it recorded it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SpendEntry {
    /// The spend's id: 16 random bytes in base64url (22 characters), never reused.
    pub id: String,
    /// The meter's code.
    pub meter: String,
    /// How many uses.
    pub n: u64,
    /// When (the guarded device clock, ms since the epoch) the entry was opened.
    pub at: i64,
    /// Which kind of entry it is.
    pub kind: SpendKind,
}

/// A meter's figures as the newest `use` answer gave them
/// ([`StoredState::meter_figures`](crate::StoredState::meter_figures)): newer
/// than the stored lease's own until the lease is renewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MeterFigure {
    /// What the key had left when the server answered, every device's reported
    /// spends counted.
    pub left: u64,
    /// The uses the key holds.
    pub total: u64,
    /// The allowance each of its devices may spend with no answer.
    pub offline: u64,
    /// Whether the meter comes back each billing period.
    pub resets: bool,
    /// The server's time (ms since the epoch).
    pub at: i64,
}

/// A meter's figures as a lease signs them or an answer gives them, without
/// the time of the answer: the base of what a status shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BaseFigure {
    /// What the key had left when it was signed.
    pub left: u64,
    /// The uses the key holds.
    pub total: u64,
    /// The allowance each of its devices may spend with no answer.
    pub offline: u64,
    /// Whether the meter comes back each billing period.
    pub resets: bool,
}

impl BaseFigure {
    /// The figure without its time.
    pub fn of(figure: &MeterFigure) -> BaseFigure {
        BaseFigure {
            left: figure.left,
            total: figure.total,
            offline: figure.offline,
            resets: figure.resets,
        }
    }
}
