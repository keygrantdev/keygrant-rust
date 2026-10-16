//! The figures a device holds of each meter, what a status shows of them, and
//! what a spend with no answer may be.

use std::collections::{BTreeMap, BTreeSet};

use super::{BaseFigure, MeterFigure, MeterStatus, SpendEntry, SpendKind};

/// Uses this device spent that KeyGrant has not heard of: the `n` of a meter's
/// `Offline` and `Unanswered` entries, never a `Void`.
pub fn unsynced_of(spends: &[SpendEntry], meter: &str) -> u64 {
    spends
        .iter()
        .filter(|entry| entry.kind != SpendKind::Void && entry.meter == meter)
        .fold(0u64, |sum, entry| sum.saturating_add(entry.n))
}

/// The base figures of each meter: its stored figure (a `use` answer's) when
/// that figure is at or after the lease's own second (`iat`, seconds), else the
/// lease's `met` member, else none, and the meter is unknown on this device. A
/// meter the newest lease no longer names, and no newer answer names, is gone.
/// A stored figure replaces the lease's whole member: its total, its allowance
/// and its `resets` too.
pub fn figures_of(
    met: Option<&BTreeMap<String, BaseFigure>>,
    iat: i64,
    stored: Option<&BTreeMap<String, MeterFigure>>,
) -> BTreeMap<String, BaseFigure> {
    // From the lease's own second counts as newer: at worst a second's spends behind.
    let newer = |figure: &MeterFigure| i128::from(figure.at) >= i128::from(iat) * 1000;
    let leased = met.into_iter().flatten().map(|(code, _)| code);
    let answered = stored.into_iter().flatten().map(|(code, _)| code);
    let codes: BTreeSet<&String> = leased.chain(answered).collect();
    codes
        .into_iter()
        .filter_map(|code| {
            let figure = match stored.and_then(|stored| stored.get(code)) {
                Some(answered) if newer(answered) => BaseFigure::of(answered),
                _ => *met?.get(code)?,
            };
            Some((code.clone(), figure))
        })
        .collect()
}

/// `status().meters`: each meter with base figures, its `left` net of this
/// device's unsynced spends and never below 0.
pub fn meter_status_of(
    figures: &BTreeMap<String, BaseFigure>,
    spends: &[SpendEntry],
) -> BTreeMap<String, MeterStatus> {
    figures
        .iter()
        .map(|(code, figure)| {
            let unsynced = unsynced_of(spends, code);
            let status = MeterStatus {
                left: figure.left.saturating_sub(unsynced),
                total: figure.total,
                offline: figure.offline,
                unsynced,
                resets: figure.resets,
            };
            (code.clone(), status)
        })
        .collect()
}

/// What a spend with no answer comes to ([`offline_spend`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfflineVerdict {
    /// The allowance covers it. `left` is what is left after it; `limit` the
    /// most the meter's unsynced spends may come to with it (the smaller of the
    /// allowance and what the figure says is left), which the journal judges
    /// again where the spend lands.
    Allowed {
        /// What is left after the spend.
        left: u64,
        /// The most the meter's unsynced spends may come to with it.
        limit: u64,
    },
    /// No base figure names the meter on this device.
    NoMeter,
    /// Past the allowance, or past what is left. `left` is what is left now,
    /// never below 0.
    OfflineLimit {
        /// What is left now.
        left: u64,
    },
}

/// Whether a spend with no answer is allowed, from the meter's base figures and
/// the journal: its unsynced spends with this one at most the figure's
/// `offline` allowance and at most its `left`. A meter with no base figure is
/// [`OfflineVerdict::NoMeter`]; an allowance of 0 refuses every spend.
pub fn offline_spend(
    figures: &BTreeMap<String, BaseFigure>,
    spends: &[SpendEntry],
    meter: &str,
    n: u64,
) -> OfflineVerdict {
    let Some(figure) = figures.get(meter) else {
        return OfflineVerdict::NoMeter;
    };
    let unsynced = unsynced_of(spends, meter);
    let left = figure.left.saturating_sub(unsynced);
    if unsynced.saturating_add(n) <= figure.offline && n <= left {
        return OfflineVerdict::Allowed {
            left: left - n,
            limit: figure.offline.min(figure.left),
        };
    }
    OfflineVerdict::OfflineLimit { left }
}
