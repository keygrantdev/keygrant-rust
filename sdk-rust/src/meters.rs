//! Metered uses, decided without I/O: what [`License::use_meter`](crate::License::use_meter)
//! may ask for, the figures a device holds, what it may spend while it gets no
//! answer, when a spend asks the server, what an answer does, and what a
//! request reports of the journal and an answer confirms of it. How the journal
//! lands on the stored state is `journal.rs`; the machinery that makes the
//! request is `license/meters.rs`.
//!
//! A meter never takes a licence away: nothing here is read to decide whether
//! the app is licensed.

use std::time::Duration;

mod answers;
mod asks;
mod figures;
mod read;
mod types;

pub use answers::{
    spends_confirmed, spends_reported, use_outcome, Confirmed, UseInput, UseOutcome, UseOutcomeKind,
};
pub use asks::{journal_asks, journal_waiting, use_asks_now, void_waiting};
pub use figures::{figures_of, meter_status_of, offline_spend, unsynced_of, OfflineVerdict};
pub use read::{
    count_of, figure_of, meters_json, meters_of, spend_json, spends_of, stored_figure_json,
    stored_figures, MAX_SAFE_INTEGER,
};
pub use types::{BaseFigure, MeterFigure, MeterStatus, SpendEntry, SpendKind, UseError, UseResult};

use std::collections::BTreeMap;

use crate::lease::LeaseClaims;
use crate::state::StoredState;
use crate::status::{LicenseState, LicenseStatus};
use crate::wire::{utf16_len, STATUS_CHECK_TIMEOUT};

/// The most uses one spend may ask for, and the most a figure of a meter is
/// when the server signs it.
pub const MAX_METER_USES: u64 = 1_000_000_000;

/// The most entries the journal holds, and a report carries (the server reads
/// no more).
pub const MAX_PENDING_SPENDS: usize = 64;

/// The most `Void` entries the journal holds: a void is the take-back of a
/// spend that was refused after it got no answer, and a device out of reach
/// that keeps trying a spend its allowance cannot cover would otherwise fill
/// the journal, leaving no room for the spends it can make. A new void past
/// them takes the place of the oldest.
pub const MAX_PENDING_VOIDS: usize = 16;

/// How long an entry is kept unreported (ms): twice the longest lease. The
/// tenant loses a report, never the customer a use.
pub const PENDING_MAX_MS: i64 = 180 * 86_400_000;

/// How long a `use` request may take: a person waits on it, so a status
/// check's bound (the HTTP bound when that is shorter). The engine reads it
/// from the status check's bound in its timings, which defaults to this value
/// and which only tests shorten.
pub const USE_TIMEOUT: Duration = STATUS_CHECK_TIMEOUT;

/// The longest meter code a call may name, in UTF-16 units: an entitlement's name.
pub const MAX_METER_CODE: usize = 32;

/// How many uses a call spends: a whole number from 1 to [`MAX_METER_USES`] is
/// itself; anything else (0, past the bound) is none
/// ([`UseError::InvalidAmount`]).
pub fn use_amount(n: u64) -> Option<u64> {
    (1..=MAX_METER_USES).contains(&n).then_some(n)
}

/// A meter's code as a call names it: 1 to 32 UTF-16 units, matched exactly;
/// anything else is none ([`UseError::NoMeter`]).
pub fn meter_code(code: &str) -> Option<&str> {
    (1..=MAX_METER_CODE)
        .contains(&utf16_len(code))
        .then_some(code)
}

/// What `use_meter(meter, n)` asks for, or why it asks nothing: the amount is
/// judged before the code.
pub fn asked_for(meter: &str, n: u64) -> Result<(&str, u64), UseError> {
    let amount = use_amount(n).ok_or(UseError::InvalidAmount)?;
    let code = meter_code(meter).ok_or(UseError::NoMeter)?;
    Ok((code, amount))
}

/// A spend's id: 16 bytes from the platform's secure random source, base64url
/// with no padding (22 characters), never reused. `None` only when the
/// platform has no random source to give.
pub fn new_spend_id() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).ok()?;
    Some(crate::lease::b64url_encode(&bytes))
}

/// The base figures of the lease that licenses this device and the answers
/// stored beside it.
pub fn figures_for(claims: &LeaseClaims, state: &StoredState) -> BTreeMap<String, BaseFigure> {
    figures_of(
        claims.met.as_ref(),
        claims.iat,
        state.meter_figures.as_ref(),
    )
}

/// What is left of `meter` by `state`: its base figure less the unsynced
/// spends, never below 0; 0 for a meter with no figure.
pub(crate) fn left_of(claims: &LeaseClaims, state: &StoredState, meter: &str) -> u64 {
    let spends = state.spends.as_deref().unwrap_or_default();
    figures_for(claims, state).get(meter).map_or(0, |figure| {
        figure.left.saturating_sub(unsynced_of(spends, meter))
    })
}

/// A `Licensed` status from a key's lease with its meters
/// ([`meter_status_of`]), each status holding a copy of its own; any other
/// status, and a lease with no meter, unchanged.
pub(crate) fn metered_status(
    mut status: LicenseStatus,
    claims: &LeaseClaims,
    state: &StoredState,
) -> LicenseStatus {
    if status.state == LicenseState::Licensed {
        let spends = state.spends.as_deref().unwrap_or_default();
        status.meters = meter_status_of(&figures_for(claims, state), spends);
    }
    status
}
