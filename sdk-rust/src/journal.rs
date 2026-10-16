//! How the journal of unsynced spends ([`StoredState::spends`]) and the figures
//! beside it ([`StoredState::meter_figures`]) land on the stored state: on the
//! state as stored at that moment, as `offline::merged` lands the rest, so that
//! two processes of one app never lose each other's spends. Both fields are
//! about the stored activation: a save made for another activation, or on a
//! state with none, writes neither.

use std::collections::BTreeMap;

use crate::meters::{
    unsynced_of, Confirmed, MeterFigure, SpendEntry, SpendKind, MAX_PENDING_SPENDS,
    MAX_PENDING_VOIDS, PENDING_MAX_MS,
};
use crate::state::StoredState;

/// A spend made with no request sent (`Offline`), or whose own request got no
/// answer (`Unanswered`), or one refused after that (`Void`).
///
/// A spend carries the most its meter's unsynced spends may come to with it
/// (`limit`, the smaller of the device's allowance and what is left), judged
/// again where it lands. `told`: the customer was told the spend was made while
/// a failed save kept it in memory, so it lands whatever the limit and the cap
/// say: a spend is never taken back. Not read on a void, which is the take-back
/// of a spend the customer was refused, never one told: a void is always held
/// to the void cap and to the journal's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpendOp {
    /// Which kind of entry it opens or grows.
    pub kind: SpendKind,
    /// The activation it is about.
    pub about: String,
    /// The spend's id.
    pub id: String,
    /// The meter's code.
    pub meter: String,
    /// How many uses.
    pub n: u64,
    /// When (the guarded clock) the spend was made.
    pub at: i64,
    /// The most the meter's unsynced spends may come to with it.
    pub limit: Option<u64>,
    /// Whether the customer was told it was made while only memory could hold it.
    pub told: bool,
}

/// One change to the journal or its figures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalOp {
    /// A spend: opens or grows an entry.
    Spend(SpendOp),
    /// An answer's `spends`: the entries the server recorded.
    Confirmed {
        /// The activation it is about.
        about: String,
        /// What the server named.
        entries: Vec<Confirmed>,
    },
    /// The figures a `use` answer gave, by code.
    Figures {
        /// The activation it is about.
        about: String,
        /// The figures.
        figures: BTreeMap<String, MeterFigure>,
    },
    /// An activation, saved: another seat's journal is dropped, the same seat's
    /// is kept.
    Activated {
        /// The activation saved.
        about: String,
    },
    /// A deactivation: the journal goes with the rest of the licence.
    Deactivated,
}

/// The fields after an op; `refused` is true when it was a spend the full
/// journal could not take, or whose limit forbids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Journaled {
    /// The journal.
    pub spends: Vec<SpendEntry>,
    /// The figures.
    pub meter_figures: BTreeMap<String, MeterFigure>,
    /// Whether an append was refused.
    pub refused: bool,
}

/// Whether an answer's confirmation names a stored entry as recorded. A void is
/// named by `void`, any other by how many the server holds.
fn confirms(entry: &SpendEntry, confirmed: &[Confirmed]) -> bool {
    confirmed.iter().any(|c| match c {
        Confirmed::Void { id } => *id == entry.id && entry.kind == SpendKind::Void,
        Confirmed::Count { id, n } => {
            *id == entry.id && entry.kind != SpendKind::Void && entry.n <= *n
        }
    })
}

/// `stored` with the incoming figures, for each meter the newer by `at` kept
/// (the stored one on a tie).
fn newest(
    stored: &BTreeMap<String, MeterFigure>,
    incoming: &BTreeMap<String, MeterFigure>,
) -> BTreeMap<String, MeterFigure> {
    let mut merged = stored.clone();
    for (code, figure) in incoming {
        if merged.get(code).is_none_or(|held| figure.at > held.at) {
            merged.insert(code.clone(), *figure);
        }
    }
    merged
}

/// Whether a spend was told to the customer: so it lands whatever the limit
/// and the cap say. A void never is.
fn told_of(op: &SpendOp) -> bool {
    op.kind != SpendKind::Void && op.told
}

/// The journal an entry is opened in: for a void, without the oldest void when
/// [`MAX_PENDING_VOIDS`] are held (the new one takes its place), so the attempts
/// of a device out of reach never fill the journal the spends it can make need.
/// Whether it has room: a new entry past the cap is refused, unless it was told.
fn opened(mut journal: Vec<SpendEntry>, op: &SpendOp) -> (Vec<SpendEntry>, bool) {
    let replaced = if op.kind == SpendKind::Void {
        void_to_replace(&journal)
    } else {
        None
    };
    if journal.len() - usize::from(replaced.is_some()) >= MAX_PENDING_SPENDS && !told_of(op) {
        return (journal, true);
    }
    if let Some(index) = replaced {
        journal.remove(index);
    }
    journal.push(SpendEntry {
        id: op.id.clone(),
        meter: op.meter.clone(),
        n: op.n,
        at: op.at,
        kind: op.kind,
    });
    (journal, false)
}

/// The index of the void a new void takes the place of: the oldest, when the
/// journal already holds [`MAX_PENDING_VOIDS`].
fn void_to_replace(journal: &[SpendEntry]) -> Option<usize> {
    let mut voids = journal
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.kind == SpendKind::Void)
        .map(|(index, _)| index);
    let oldest = voids.next()?;
    (1 + voids.count() >= MAX_PENDING_VOIDS).then_some(oldest)
}

/// The journal with an append applied: an `Offline` entry grows or is opened,
/// the others are opened once by id; none that would take the meter's unsynced
/// spends past the op's limit (a void is no spend, and a told one lands whatever
/// it says), nor a new entry past the cap ([`opened`]). An offline spend that
/// joins an entry loses its id, which is why no request reports a spend still
/// held in memory.
fn appended(journal: Vec<SpendEntry>, op: &SpendOp) -> (Vec<SpendEntry>, bool) {
    // A spend whose id the journal holds is the one it is: left as it is,
    // whatever the limit now says.
    if op.kind != SpendKind::Offline && journal.iter().any(|entry| entry.id == op.id) {
        return (journal, false);
    }
    let limit = (op.kind != SpendKind::Void && !told_of(op))
        .then_some(op.limit)
        .flatten();
    if limit.is_some_and(|most| unsynced_of(&journal, &op.meter).saturating_add(op.n) > most) {
        return (journal, true);
    }
    let open = (op.kind == SpendKind::Offline)
        .then(|| {
            journal
                .iter()
                .position(|e| e.kind == SpendKind::Offline && e.meter == op.meter)
        })
        .flatten();
    match open {
        Some(index) => {
            let mut grown = journal;
            grown[index].n = grown[index].n.saturating_add(op.n);
            (grown, false)
        }
        None => opened(journal, op),
    }
}

/// The journal and the figures after `op`, landed on `latest` at `now` (the
/// guarded clock). An op about another activation than the one stored, or about
/// none, changes nothing; a deactivation clears both, and an activation of
/// another seat does (the same seat activated again keeps them). Every op that
/// lands drops the entries opened more than [`PENDING_MAX_MS`] before `now`,
/// unreported. An append the journal has no room for is `refused`.
pub fn journaled(latest: &StoredState, op: &JournalOp, now: i64) -> Journaled {
    let about = match op {
        JournalOp::Deactivated => return Journaled::cleared(),
        JournalOp::Spend(spend) => &spend.about,
        JournalOp::Confirmed { about, .. }
        | JournalOp::Figures { about, .. }
        | JournalOp::Activated { about } => about,
    };
    let spends = latest.spends.clone().unwrap_or_default();
    let figures = latest.meter_figures.clone().unwrap_or_default();
    if latest.activation_id.as_ref() != Some(about) {
        return if matches!(op, JournalOp::Activated { .. }) {
            Journaled::cleared()
        } else {
            Journaled {
                spends,
                meter_figures: figures,
                refused: false,
            }
        };
    }
    let live: Vec<SpendEntry> = spends
        .into_iter()
        .filter(|entry| i128::from(now) - i128::from(entry.at) <= i128::from(PENDING_MAX_MS))
        .collect();
    match op {
        JournalOp::Activated { .. } => Journaled {
            spends: live,
            meter_figures: figures,
            refused: false,
        },
        JournalOp::Confirmed { entries, .. } => Journaled {
            spends: live
                .into_iter()
                .filter(|entry| !confirms(entry, entries))
                .collect(),
            meter_figures: figures,
            refused: false,
        },
        JournalOp::Figures { figures: given, .. } => Journaled {
            spends: live,
            meter_figures: newest(&figures, given),
            refused: false,
        },
        JournalOp::Spend(spend) => {
            let (spends, refused) = appended(live, spend);
            Journaled {
                spends,
                meter_figures: figures,
                refused,
            }
        }
        JournalOp::Deactivated => Journaled::cleared(),
    }
}

impl Journaled {
    fn cleared() -> Journaled {
        Journaled {
            spends: Vec::new(),
            meter_figures: BTreeMap::new(),
            refused: false,
        }
    }
}

/// Confirmations with each id once as a count and once as a void, the larger
/// count kept: the entries any of them would have dropped.
fn folded_confirmations(entries: impl IntoIterator<Item = Confirmed>) -> Vec<Confirmed> {
    let mut folded: Vec<Confirmed> = Vec::new();
    for entry in entries {
        let held = folded.iter_mut().find(|held| match (&entry, &**held) {
            (Confirmed::Void { id }, Confirmed::Void { id: other }) => id == other,
            (Confirmed::Count { id, .. }, Confirmed::Count { id: other, .. }) => id == other,
            _ => false,
        });
        match (held, &entry) {
            (None, _) => folded.push(entry),
            (Some(Confirmed::Count { n: held, .. }), Confirmed::Count { n, .. }) if *n > *held => {
                *held = *n;
            }
            _ => {}
        }
    }
    folded
}

/// `ops` with neighbours folded together where landing them one after the other
/// lands what landing the fold does: offline spends of one meter (their counts
/// added, the first's id and time kept), figures (the newer of each meter) and
/// confirmations (each id once, the larger count). A disk that keeps refusing
/// the journal then costs the process one change for a run of spends of one
/// meter, not one for each; spends of meters that take turns are not folded
/// across one another.
pub fn compacted(ops: &[JournalOp]) -> Vec<JournalOp> {
    let mut out: Vec<JournalOp> = Vec::new();
    for op in ops {
        match (out.last_mut(), op) {
            (Some(JournalOp::Spend(last)), JournalOp::Spend(next))
                if last.kind == SpendKind::Offline
                    && next.kind == SpendKind::Offline
                    && last.about == next.about
                    && last.meter == next.meter =>
            {
                last.n = last.n.saturating_add(next.n);
            }
            (
                Some(JournalOp::Figures { about, figures }),
                JournalOp::Figures {
                    about: next_about,
                    figures: next,
                },
            ) if about == next_about => {
                *figures = newest(figures, next);
            }
            (
                Some(JournalOp::Confirmed { about, entries }),
                JournalOp::Confirmed {
                    about: next_about,
                    entries: next,
                },
            ) if about == next_about => {
                *entries = folded_confirmations(entries.iter().chain(next).cloned());
            }
            _ => out.push(op.clone()),
        }
    }
    out
}

/// `ops` with all but the newest [`MAX_PENDING_VOIDS`] void ops dropped: a
/// process that cannot save keeps no more voids than the journal would.
pub fn bounded_voids(ops: Vec<JournalOp>) -> Vec<JournalOp> {
    let is_void = |op: &JournalOp| matches!(op, JournalOp::Spend(s) if s.kind == SpendKind::Void);
    let mut surplus = ops
        .iter()
        .filter(|op| is_void(op))
        .count()
        .saturating_sub(MAX_PENDING_VOIDS);
    ops.into_iter()
        .filter(|op| {
            if surplus > 0 && is_void(op) {
                surplus -= 1;
                return false;
            }
            true
        })
        .collect()
}

/// `state` with the journal fields `journaled` gives: an empty journal or no
/// figures is the field absent.
fn set_journal(state: &mut StoredState, journaled: Journaled) {
    state.spends = Some(journaled.spends).filter(|spends| !spends.is_empty());
    state.meter_figures = Some(journaled.meter_figures).filter(|figures| !figures.is_empty());
}

/// What landing ops on a state came to ([`land_journal`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Landed {
    /// The state after every op.
    pub state: StoredState,
    /// Whether each op was refused.
    pub refused: Vec<bool>,
}

/// `ops` landed in order on `state` at `now`, and whether each was refused; the
/// state itself when there are none.
pub fn land_journal(state: &StoredState, ops: &[JournalOp], now: i64) -> Landed {
    let mut current = state.clone();
    let mut refused = Vec::with_capacity(ops.len());
    for op in ops {
        let landed = journaled(&current, op, now);
        refused.push(landed.refused);
        set_journal(&mut current, landed);
    }
    Landed {
        state: current,
        refused,
    }
}
