//! What the engine decides from what the device holds, without asking the
//! server: whether a lease licenses this build, what a trial shows, which
//! refusal still stands, when to ask the server again, and what of a save
//! lands on a state another process changed meanwhile.

use std::collections::BTreeMap;

use crate::lease::{Entitlement, LeaseClaims, LicenseModel, VerifyFailure, VerifyResult};
use crate::pickup::{about_stored, PurchaseOf, PURCHASE_FIELDS};
use crate::state::{Patch, StoredState};
use crate::status::{DeviceKind, LicenseReason, LicenseState, LicenseStatus, Refusal};
use crate::wire::earliest;

pub(crate) const DAY_MS: i64 = 86_400_000;

/// How long after the server answered without a lease (a refusal, a 5xx, a
/// 429, an answer the SDK could not use) before the SDK asks again on its own (ms).
pub const ASK_AGAIN_MS: i64 = 60 * 60_000;

/// How long after an ask that got no answer at all (no network, a timeout)
/// before the SDK asks again on its own (ms).
pub const RETRY_UNANSWERED_MS: i64 = 60_000;

/// A valid lease is renewed early once less than this is left of it, or less
/// than half its own life if that is shorter (ms).
pub const RENEW_WITHIN_MS: i64 = 7 * 24 * 60 * 60_000;

/// How far the clock guard may run ahead of the time the server signed a
/// fresh lease before it is brought back (ms).
pub const CLOCK_TOLERANCE_MS: i64 = 60 * 60_000;

/// The major a build runs as: the configured one, read as the server reads a
/// version (0.x as 1, and majors from 1 up). So a build configured as major 0
/// is major 1 everywhere the SDK uses the major (sent with `activate`,
/// `validate` and `claim`, and judged by `check_lease`, `kept_with` and
/// `standing_refusal`), rather than refused offline as outdated by every lease.
/// The reference also cuts a fraction and reads a value that is not a finite
/// number as 1; a `u32` holds neither.
pub fn major_of(configured: u32) -> i64 {
    i64::from(configured.max(1))
}

/// Why a lease does not license this build, offline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LeaseFailure {
    /// Past its end.
    Expired,
    /// This build is newer than it covers.
    Upgrade,
    /// This build is older than it covers.
    Outdated,
    /// Not a lease for this licence.
    Tampered,
    /// A lease for another device.
    Device,
}

impl LeaseFailure {
    /// The reference's word for it.
    pub fn as_str(self) -> &'static str {
        match self {
            LeaseFailure::Expired => "expired",
            LeaseFailure::Upgrade => "upgrade",
            LeaseFailure::Outdated => "outdated",
            LeaseFailure::Tampered => "tampered",
            LeaseFailure::Device => "device",
        }
    }

    pub(crate) fn reason(self) -> LicenseReason {
        match self {
            LeaseFailure::Expired => LicenseReason::Offline,
            LeaseFailure::Upgrade => LicenseReason::Upgrade,
            LeaseFailure::Outdated => LicenseReason::Outdated,
            LeaseFailure::Tampered => LicenseReason::Tampered,
            LeaseFailure::Device => LicenseReason::Device,
        }
    }
}

/// What a lease says about this build, offline.
#[derive(Debug, Clone, PartialEq)]
pub enum LeaseCheck {
    /// It licenses this build.
    Ok {
        /// A test licence.
        test: bool,
        /// Its claims.
        claims: Box<LeaseClaims>,
    },
    /// It does not.
    Fail(LeaseFailure),
}

/// Whose a licence lease must be: the key and activation stored, and this device.
#[derive(Debug, Clone, Copy)]
pub struct LeaseHolder<'a> {
    /// The stored key.
    pub key: &'a str,
    /// The stored activation.
    pub activation_id: &'a str,
    /// Every `fp` claim this device answers to.
    pub claims: &'a [String],
    /// Whether this run could tell who the device is.
    pub can_tell: bool,
}

/// Whether two licence keys are the same key: the server matches them without regard to case.
pub fn same_key(a: &str, b: &str) -> bool {
    a.to_uppercase() == b.to_uppercase()
}

/// A verified licence lease held to this product, this holder and this
/// build's major. Another product, key or activation is `Tampered`; another
/// device is `Device`, only for a seat keyed by a machine id (`fpk`), and
/// only when this run can tell who the device is; a build below the key's
/// lowest major (`minMajor`, absent meaning 1) is `Outdated`; one above
/// `major` is `Upgrade`.
pub fn check_lease(
    v: &VerifyResult,
    product: &str,
    major: i64,
    holder: &LeaseHolder<'_>,
) -> LeaseCheck {
    let claims = match v {
        VerifyResult::Valid(claims) => claims,
        VerifyResult::Invalid {
            reason: VerifyFailure::Expired,
            ..
        } => return LeaseCheck::Fail(LeaseFailure::Expired),
        VerifyResult::Invalid { .. } => return LeaseCheck::Fail(LeaseFailure::Tampered),
    };
    if claims.product != product {
        return LeaseCheck::Fail(LeaseFailure::Tampered);
    }
    if !same_key(&claims.sub, holder.key) || claims.aid != holder.activation_id {
        return LeaseCheck::Fail(LeaseFailure::Tampered);
    }
    let held = claims.fp.is_some() && claims.fpk == Some(DeviceKind::Machine);
    let fp = claims.fp.as_deref().unwrap_or_default();
    if held && holder.can_tell && !holder.claims.iter().any(|claim| claim == fp) {
        return LeaseCheck::Fail(LeaseFailure::Device);
    }
    if major < claims.min_major.unwrap_or(1) {
        return LeaseCheck::Fail(LeaseFailure::Outdated);
    }
    if major > claims.major {
        return LeaseCheck::Fail(LeaseFailure::Upgrade);
    }
    LeaseCheck::Ok {
        test: claims.test == Some(true),
        claims: Box::new(claims.clone()),
    }
}

/// The status a stored licence lease gives, judged offline ([`check_lease`]),
/// when the server gives no verdict on it: licensed, with `test` when the
/// check says so and the lease's entitlements (empty when it grants none);
/// `Expired` / `Offline`, with the key, once it has run out; `Invalid` with
/// the reason, and the key, when it is not for this build or this device;
/// `Invalid` / `Tampered`, with no key, when it is not this licence's at all.
/// Only a lease that licenses carries entitlements: one that does not license
/// this build grants it nothing.
///
/// The test flag is the check's (`test`), its one source, so a check and the
/// claims beside it can never give two answers. Test licences verify like any
/// other (signed with the product's real key); the flag is how an app tells
/// them apart, to refuse them in a release build, say.
pub fn lease_status(check: &LeaseCheck, key: Option<&str>) -> LicenseStatus {
    match check {
        LeaseCheck::Ok { test, claims } => {
            let mut status = LicenseStatus::new(LicenseState::Licensed).with_key(key);
            status.test = *test;
            status.entitlements = claims.ent.clone();
            status
        }
        LeaseCheck::Fail(LeaseFailure::Expired) => LicenseStatus::new(LicenseState::Expired)
            .with_reason(LicenseReason::Offline)
            .with_key(key),
        LeaseCheck::Fail(LeaseFailure::Tampered) => {
            LicenseStatus::new(LicenseState::Invalid).with_reason(LicenseReason::Tampered)
        }
        LeaseCheck::Fail(
            failure @ (LeaseFailure::Upgrade | LeaseFailure::Outdated | LeaseFailure::Device),
        ) => LicenseStatus::new(LicenseState::Invalid)
            .with_reason(failure.reason())
            .with_key(key),
    }
}

/// Whether a licence lease is held to a machine that is not this device: its
/// seat keyed by a machine id (`fp`, `fpk` machine) that this run can tell
/// (`can_tell`) is not one of its own (`claims`, this device's fingerprint
/// claims). Read from the claims of a lease whose signature holds, expired or
/// not: a seat does not change hands when its lease runs out. A lease held to
/// no machine (no `fp`, or `fpk` hostname) is never another device's. A
/// seat that is another device's is not this device's to release
/// (`License::deactivate`).
pub fn held_elsewhere(v: &VerifyResult, claims: &[String], can_tell: bool) -> bool {
    let lease = match v {
        VerifyResult::Valid(lease) => lease,
        VerifyResult::Invalid {
            reason: VerifyFailure::Expired,
            claims: Some(lease),
        } => lease,
        VerifyResult::Invalid { .. } => return false,
    };
    let (Some(fp), Some(DeviceKind::Machine)) = (&lease.fp, lease.fpk) else {
        return false;
    };
    can_tell && !claims.contains(fp)
}

/// `kept` after running `major` on a lease whose upgrade window ends at
/// `upgrade_until` (seconds) at `now` (ms): raised to it while the window is
/// open, left as it was once it has closed.
pub fn kept_with(
    kept: Option<i64>,
    upgrade_until: Option<i64>,
    now: i64,
    major: i64,
) -> Option<i64> {
    match upgrade_until {
        Some(until) if i128::from(now) < i128::from(until) * 1000 => {
            Some(kept.unwrap_or(0).max(major))
        }
        _ => kept,
    }
}

/// A trial lease, verified: a trial's (`model`), for this product, not past
/// its end, and for this device (its `aid`, a fingerprint claim), judged only
/// when the trial was started under a machine id and this run can tell who
/// the device is. A licence's lease in the trial's place is not a trial: the
/// server only ever signs a trial lease as `model: trial`.
pub fn trial_lease_valid(
    v: &VerifyResult,
    product: &str,
    claims: &[String],
    can_tell: bool,
) -> bool {
    let VerifyResult::Valid(lease) = v else {
        return false;
    };
    if lease.model != LicenseModel::Trial || lease.product != product {
        return false;
    }
    lease.fpk != Some(DeviceKind::Machine) || !can_tell || claims.contains(&lease.aid)
}

/// What a trial's status reads of its trial lease ([`trial_lease_of`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrialLease {
    /// Whether it is valid here ([`trial_lease_valid`]).
    pub valid: bool,
    /// The end it was signed to (its `exp`, ms), when it is valid.
    pub ends_at: Option<i64>,
    /// What it grants (its `ent`), when it is valid; empty otherwise.
    pub entitlements: BTreeMap<String, Entitlement>,
}

/// A trial lease, verified, as a trial's status reads it: whether it is valid
/// here ([`trial_lease_valid`]), and, when it is, the end it was signed to
/// and its entitlements. A lease that is not valid here says nothing of either.
pub fn trial_lease_of(
    v: &VerifyResult,
    product: &str,
    claims: &[String],
    can_tell: bool,
) -> TrialLease {
    let VerifyResult::Valid(lease) = v else {
        return TrialLease::default();
    };
    if !trial_lease_valid(v, product, claims, can_tell) {
        return TrialLease::default();
    }
    TrialLease {
        valid: true,
        ends_at: Some(lease.exp.saturating_mul(1000)),
        entitlements: lease.ent.clone(),
    }
}

/// Now, by the clock guard: never earlier than the latest time the device
/// has seen (`last_seen`), so a clock put back cannot bring an expired lease back.
pub fn guarded_now(clock_now: i64, last_seen: Option<i64>) -> i64 {
    clock_now.max(last_seen.unwrap_or(0))
}

/// The latest time seen (`lastSeen`) a save writes onto the state as stored
/// when it saves (`stored`): the guard brought back on the server's evidence
/// (`guard`, a [`clock_correction`]) exactly as given when the save carries
/// one; otherwise never earlier than what is stored, nor than the device clock
/// now, so the guard only ever moves forward on its own.
pub fn saved_last_seen(stored: Option<i64>, clock_now: i64, guard: Option<i64>) -> i64 {
    guard.unwrap_or_else(|| stored.unwrap_or(0).max(clock_now))
}

/// A fresh lease's claims as the clock guard reads them ([`clock_correction`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignedAt<'a> {
    /// The key it names.
    pub sub: &'a str,
    /// The activation it names.
    pub aid: &'a str,
    /// When it was signed (seconds).
    pub iat: i64,
}

impl<'a> SignedAt<'a> {
    /// What the clock guard reads of a lease's claims.
    pub fn of(claims: &'a LeaseClaims) -> Self {
        SignedAt {
            sub: &claims.sub,
            aid: &claims.aid,
            iat: claims.iat,
        }
    }
}

/// Where the clock guard should be brought back to on a lease the server has
/// just signed, or none to leave it.
///
/// The guard only moves forward, so a clock that once jumped ahead and was
/// put right left it there, and every lease read as expired even online. The
/// server's signing time (`iat`) is better evidence: when the guard is MORE
/// than [`CLOCK_TOLERANCE_MS`] past it, the guard comes back to the device
/// clock, never below `iat`. Only a lease signed for THIS key (without regard
/// to case) and activation, and no older than the lease the device holds
/// (`held_iat`), moves it. `claims` is the fresh lease's, verified (valid or
/// merely expired); none when it did not verify, which moves nothing.
pub fn clock_correction(
    clock_now: i64,
    last_seen: Option<i64>,
    claims: Option<SignedAt<'_>>,
    held_iat: Option<i64>,
    key: &str,
    activation_id: &str,
) -> Option<i64> {
    let claims = claims?;
    if !same_key(claims.sub, key) || claims.aid != activation_id {
        return None;
    }
    let signed = i128::from(claims.iat) * 1000;
    if i128::from(last_seen.unwrap_or(0)) - signed <= i128::from(CLOCK_TOLERANCE_MS) {
        return None;
    }
    if held_iat.is_some_and(|held| claims.iat < held) {
        return None;
    }
    Some(clock_now.max(i64::try_from(signed).unwrap_or(i64::MAX)))
}

/// A trial's status from what the device holds: `lease_valid` is its trial
/// lease, checked ([`trial_lease_valid`]), `lease_ends_at` the end that lease
/// was signed to (ms; none when not known), and `lease_entitlements` what it
/// grants (its `ent`), which a running trial carries and no other answer does.
///
/// Live ONLY while the trial lease is valid, to the lease's end (the stored
/// `trialEndsAt` only when the lease's is not known). The stored end is the
/// server's word but unsigned, so it never runs a trial on its own: trusted,
/// an edit to the state file was a trial without end. KeyGrant signs a trial
/// lease to the trial's own end, so an honest trial holds a valid lease for
/// every day it runs, offline included.
pub fn trial_status_of(
    state: &StoredState,
    now: i64,
    lease_valid: bool,
    lease_ends_at: Option<i64>,
    lease_entitlements: &BTreeMap<String, Entitlement>,
) -> LicenseStatus {
    if state.trial_start.is_none() && state.trial_ends_at.is_none() && state.trial_lease.is_none() {
        return LicenseStatus::new(LicenseState::Unlicensed);
    }
    if !lease_valid {
        return LicenseStatus::new(LicenseState::Expired).with_reason(LicenseReason::Trial);
    }
    let ends_at = lease_ends_at.or(state.trial_ends_at);
    let mut status = LicenseStatus::new(LicenseState::Trial);
    status.trial_ends_at = ends_at;
    status.trial_days_left = ends_at.map(|end| {
        let left = i128::from(end) - i128::from(now);
        if left <= 0 {
            0
        } else {
            i64::try_from((left + i128::from(DAY_MS) - 1) / i128::from(DAY_MS)).unwrap_or(i64::MAX)
        }
    });
    status.entitlements = lease_entitlements.clone();
    status
}

/// What storing the server's refusal changes: a verdict on the KEY drops the
/// lease; one on this BUILD keeps it with the major refused; one on this
/// DEVICE keeps it with the device's OWN claim (none when it has none).
pub fn refusal_patch(refused: Refusal, major: i64, own_claim: Option<&str>) -> Patch {
    let mut patch = Patch {
        refused: Some(Some(refused)),
        ..Patch::default()
    };
    match refused {
        Refusal::Revoked => {
            patch.lease = Some(None);
            patch.refused_major = Some(None);
            patch.refused_for = Some(None);
        }
        Refusal::Device => {
            patch.refused_major = Some(None);
            patch.refused_for = Some(Some(
                own_claim
                    .map(|claim| vec![claim.to_owned()])
                    .unwrap_or_default(),
            ));
        }
        Refusal::Upgrade | Refusal::Outdated => {
            patch.refused_major = Some(Some(major));
            patch.refused_for = Some(None);
        }
    }
    patch
}

/// What a check-in with this device writes of the install's key: a machine id read here keys it.
pub fn keyed_by(kind: Option<DeviceKind>) -> Patch {
    match kind {
        Some(DeviceKind::Machine) => Patch {
            device_kind: Some(Some(DeviceKind::Machine)),
            ..Patch::default()
        },
        _ => Patch::default(),
    }
}

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

/// Who this device is, as far as a standing `device` refusal needs to know.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeviceView<'a> {
    /// The claim of this run's own fingerprint.
    pub own_claim: Option<&'a str>,
    /// Whether this run could tell who the device is.
    pub can_tell: bool,
}

/// The stored refusal that still stands for a build running `major`, or none.
/// A key-level one always does; `upgrade` for the refused major and newer;
/// `outdated` for it and older; one stored with no major for every build; a
/// `device` one only while this run can tell it is still the device refused.
pub fn standing_refusal(
    state: &StoredState,
    major: i64,
    device: Option<DeviceView<'_>>,
) -> Option<Refusal> {
    let refused = state.refused?;
    if refused == Refusal::Device {
        if let Some(refused_for) = &state.refused_for {
            let own = device
                .filter(|device| device.can_tell)
                .and_then(|device| device.own_claim);
            return own
                .is_some_and(|own| refused_for.iter().any(|claim| claim == own))
                .then_some(refused);
        }
    }
    let Some(refused_major) = state.refused_major else {
        return Some(refused);
    };
    match refused {
        Refusal::Revoked | Refusal::Device => Some(refused),
        Refusal::Upgrade => (major >= refused_major).then_some(refused),
        Refusal::Outdated => (major <= refused_major).then_some(refused),
    }
}

/// Whether to ask the server now: when forced, when no ask has come back
/// without a lease since the last lease, or once that ask's wait is over.
/// `clock_now` is the device clock as it is: a clock set back since the ask
/// asks at once.
pub fn ask_due(state: &StoredState, clock_now: i64, force: bool) -> bool {
    let Some(since) = state.backoff_since else {
        return true;
    };
    if force || clock_now < since {
        return true;
    }
    clock_now.saturating_sub(since) >= state.backoff_ms.unwrap_or(ASK_AGAIN_MS)
}

/// Whether a valid lease is near enough its end at `now` (ms) to renew it
/// early: less than [`RENEW_WITHIN_MS`] left, or less than half its life if
/// that is shorter.
pub fn renews_early(claims: &LeaseClaims, now: i64) -> bool {
    let left = i128::from(claims.exp) * 1000 - i128::from(now);
    let life = (i128::from(claims.exp) - i128::from(claims.iat)) * 1000;
    left < i128::from(RENEW_WITHIN_MS) && left * 2 < life
}
