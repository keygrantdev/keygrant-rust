//! What of a save lands on a state another process changed meanwhile.

use crate::pickup::{about_stored, PurchaseOf, PURCHASE_FIELDS};
use crate::state::{Patch, StoredState};
use crate::wire::earliest;

/// When something was asked of the server (guarded device clock, ms), and the
/// stored stamp of that kind the asker saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stamp {
    /// When it was asked.
    pub at: i64,
    /// The stamp stored when the asker read the state.
    pub seen: Option<i64>,
}

/// An ask about an activation, as a save of its answer sees it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Asked {
    /// When it was asked.
    pub at: i64,
    /// The activation it asked about.
    pub activation_id: Option<String>,
    /// The `verdictAt` stored when the asker read the state.
    pub seen: Option<i64>,
}

/// A save about an activation that is not an answer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct About {
    /// The activation it is about.
    pub activation_id: Option<String>,
}

/// What a save is based on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Basis {
    /// The answer to an ask about an activation: stamped `verdictAt`.
    pub answer: Option<Asked>,
    /// An activation's answer, about the activation it names: stamped `verdictAt`.
    pub activation: Option<Asked>,
    /// A save about an activation that is not an answer.
    pub about: Option<About>,
    /// The server's answer to a trial ask: stamped `trialAt`.
    pub trial: Option<Stamp>,
    /// A save about a purchase secret (its drop, or the wait after a claim
    /// ask): its purchase fields land only while that secret is the one
    /// stored, so one another process made meanwhile is neither dropped nor
    /// stamped by it; a wait, only while its link is as handed out when it
    /// was asked ([`PurchaseOf::at`]).
    pub purchase: Option<PurchaseOf>,
}

/// What the answer to an ask writes: what the server said, and the wait.
const VERDICT_FIELDS: &[&str] = &[
    "lease",
    "refused",
    "refusedMajor",
    "refusedFor",
    "backoffSince",
    "backoffMs",
    "checkedMajor",
    "verdictAt",
];

/// A trial answer, taken whole from the newest one.
const TRIAL_ANSWER_FIELDS: &[&str] = &["trialEndsAt", "trialLease", "trialAt"];

/// The trial evidence, which belongs to the device, not to any activation.
const TRIAL_FIELDS: &[&str] = &["trialStart", "trialEndsAt", "trialLease", "trialAt"];

/// Whether a stored stamp is a NEWER answer than `stamp`'s: saved since that
/// asker read the state, and asked for later than it.
pub fn outdated(stored: Option<i64>, stamp: &Stamp) -> bool {
    stored != stamp.seen && stored.is_some_and(|stored| stored > stamp.at)
}

/// How out of date an ask's answer is by the time it is saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Superseded {
    /// The state names another activation, or none: only trial evidence may land.
    Activation,
    /// A newer answer about the same activation was saved: its verdict may not land.
    Answer,
}

/// Whether an ask's answer is out of date by the time it is saved.
pub fn superseded(latest: &StoredState, asked: &Asked) -> Option<Superseded> {
    if latest.activation_id != asked.activation_id {
        return Some(Superseded::Activation);
    }
    outdated(
        latest.verdict_at,
        &Stamp {
            at: asked.at,
            seen: asked.seen,
        },
    )
    .then_some(Superseded::Answer)
}

/// A patch without the verdict fields: what of an out-of-date answer is still saved.
pub fn without_verdict(patch: Patch) -> Patch {
    patch.retain(|field| !VERDICT_FIELDS.contains(&field))
}

/// Only the trial evidence of a patch: what lands on another activation.
pub fn trial_only(patch: Patch) -> Patch {
    patch.retain(|field| TRIAL_FIELDS.contains(&field))
}

fn out_of_date(latest: &StoredState, basis: &Basis) -> Option<Superseded> {
    if let Some(answer) = &basis.answer {
        return superseded(latest, answer);
    }
    if let Some(activation) = &basis.activation {
        let same = latest.activation_id == activation.activation_id;
        let stamp = Stamp {
            at: activation.at,
            seen: activation.seen,
        };
        return (same && outdated(latest.verdict_at, &stamp)).then_some(Superseded::Answer);
    }
    match &basis.about {
        Some(about) if latest.activation_id != about.activation_id => Some(Superseded::Activation),
        _ => None,
    }
}

/// A patch merged onto the state as stored now, by what it is based on: an
/// answer or activation stamped `verdictAt`, a trial answer `trialAt`; a save
/// about an activation no longer stored keeps only its trial evidence; an
/// answer older than the stored one loses its verdict fields; an older trial
/// answer loses its end and lease; a save about a purchase secret no longer
/// stored (or, for a wait, one whose link was handed out again since) loses
/// its purchase fields; the trial start only ever moves earlier; the kept
/// major of the stored activation only ever up.
pub fn merged(latest: &StoredState, patch: &Patch, basis: &Basis) -> StoredState {
    let stamp = basis.answer.as_ref().or(basis.activation.as_ref());
    let mut applied = patch.clone();
    if let Some(stamp) = stamp {
        applied.verdict_at = Some(Some(stamp.at));
    }
    if let Some(trial) = &basis.trial {
        applied.trial_at = Some(Some(trial.at));
    }
    match out_of_date(latest, basis) {
        Some(Superseded::Activation) => applied = trial_only(applied),
        Some(Superseded::Answer) => applied = without_verdict(applied),
        None => {}
    }
    if let Some(trial) = &basis.trial {
        if outdated(latest.trial_at, trial) {
            applied = applied.retain(|field| !TRIAL_ANSWER_FIELDS.contains(&field));
        }
    }
    if let Some(purchase) = &basis.purchase {
        if !about_stored(latest, purchase) {
            applied = applied.retain(|field| !PURCHASE_FIELDS.contains(&field));
        }
    }
    let trial_start = earliest(&[latest.trial_start, applied.trial_start.flatten()]);
    let mut result = applied.applied_to(latest.clone());
    if trial_start.is_some() {
        result.trial_start = trial_start;
    }
    let about = stamp
        .map(|asked| &asked.activation_id)
        .or(basis.about.as_ref().map(|about| &about.activation_id));
    if about.is_some_and(|about| latest.activation_id == *about) {
        let kept = [latest.kept_major, applied.kept_major.flatten()]
            .into_iter()
            .flatten()
            .max();
        if kept.is_some() {
            result.kept_major = kept;
        }
    }
    result
}

/// What a new lease from the server clears: any refusal, and the wait before asking again.
pub fn leased() -> Patch {
    Patch {
        refused: Some(None),
        refused_major: Some(None),
        refused_for: Some(None),
        backoff_since: Some(None),
        backoff_ms: Some(None),
        ..Patch::default()
    }
}

/// What a deactivated device keeps of its licence: nothing but its trial evidence and clock guard.
pub fn unkeyed() -> Patch {
    Patch {
        key: Some(None),
        activation_id: Some(None),
        lease: Some(None),
        checked_major: Some(None),
        kept_major: Some(None),
        device_kind: Some(None),
        ..Patch::default()
    }
    .then(leased())
}
