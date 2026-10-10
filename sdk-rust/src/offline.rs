//! What the engine decides from what the device holds, without asking the
//! server: whether a lease licenses this build, what a trial shows, which
//! refusal still stands, when to ask the server again, and what of a save
//! lands on a state another process changed meanwhile.

use std::collections::BTreeMap;

use crate::lease::{Entitlement, LeaseClaims};
use crate::state::{Patch, StoredState};
use crate::status::{DeviceKind, LicenseReason, LicenseState, LicenseStatus, Refusal};

pub(crate) const DAY_MS: i64 = 86_400_000;

mod clock;
mod lease_check;
mod merge;

pub use clock::{clock_correction, guarded_now, saved_last_seen, SignedAt};
pub use lease_check::{
    check_lease, held_elsewhere, kept_with, lease_status, same_key, trial_lease_of,
    trial_lease_valid, LeaseCheck, LeaseFailure, LeaseHolder, TrialLease,
};
pub use merge::{
    activation_patch, leased, merged, outdated, superseded, trial_only, unkeyed, without_verdict,
    About, Activation, Asked, Basis, Stamp, Superseded,
};

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
/// is major 1 everywhere the SDK uses the major (sent with `activate` and
/// `validate`, and judged by `check_lease`, `kept_with` and
/// `standing_refusal`), rather than refused offline as outdated by every lease.
/// The reference also cuts a fraction and reads a value that is not a finite
/// number as 1; a `u32` holds neither.
pub fn major_of(configured: u32) -> i64 {
    i64::from(configured.max(1))
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
