//! What of a save lands on a state another process changed meanwhile.

use super::kept_with;
use crate::lease::LeaseClaims;
use crate::state::{Patch, StoredState};
use crate::status::DeviceKind;
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
/// answer loses its end and lease; the trial start only ever moves earlier;
/// the kept major of the stored activation only ever up.
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

/// An activation the server answered with a lease that licenses this build
/// ([`activation_patch`]).
#[derive(Debug, Clone, Copy)]
pub struct Activation<'a> {
    /// The key.
    pub key: &'a str,
    /// This device's activation on it.
    pub activation_id: &'a str,
    /// The lease.
    pub lease: &'a str,
    /// The lease's claims.
    pub claims: &'a LeaseClaims,
    /// The time the lease was judged at.
    pub now: i64,
    /// This build's major.
    pub major: i64,
    /// What the activation's fingerprint hashes: none for one of unknown kind.
    pub kind: Option<DeviceKind>,
}

/// What an activation saves, stamped as an activation: the key, the
/// activation, the lease, the major heard, the kept major (`kept_with`), any
/// refusal and wait cleared (`leased`), and what the install is keyed by
/// (cleared for a fingerprint of unknown kind).
pub fn activation_patch(a: &Activation<'_>) -> Patch {
    Patch {
        key: Some(Some(a.key.to_owned())),
        activation_id: Some(Some(a.activation_id.to_owned())),
        lease: Some(Some(a.lease.to_owned())),
        checked_major: Some(Some(a.major)),
        kept_major: Some(kept_with(None, a.claims.upgrade_until, a.now, a.major)),
        ..Patch::default()
    }
    .then(leased())
    .then(Patch {
        device_kind: Some(a.kind),
        ..Patch::default()
    })
}
