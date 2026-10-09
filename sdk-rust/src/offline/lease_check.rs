//! Whether a lease licenses this build on this device, offline, and what a trial lease runs.

use std::collections::BTreeMap;

use crate::lease::{Entitlement, LeaseClaims, LicenseModel, VerifyFailure, VerifyResult};
use crate::status::{DeviceKind, LicenseReason, LicenseState, LicenseStatus};

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
    /// Signed by a key the build does not carry.
    UnknownKey,
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
            LeaseFailure::UnknownKey => "unknown-key",
            LeaseFailure::Device => "device",
        }
    }

    pub(crate) fn reason(self) -> LicenseReason {
        match self {
            LeaseFailure::Expired => LicenseReason::Offline,
            LeaseFailure::Upgrade => LicenseReason::Upgrade,
            LeaseFailure::Outdated => LicenseReason::Outdated,
            LeaseFailure::Tampered => LicenseReason::Tampered,
            LeaseFailure::UnknownKey => LicenseReason::UnknownKey,
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
        VerifyResult::Invalid {
            reason: VerifyFailure::UnknownKey,
            ..
        } => return LeaseCheck::Fail(LeaseFailure::UnknownKey),
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
/// the reason, and the key, when it is not for this build or this device, or
/// is signed by a key the build does not carry (`UnknownKey`);
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
            failure @ (LeaseFailure::Upgrade
            | LeaseFailure::Outdated
            | LeaseFailure::Device
            | LeaseFailure::UnknownKey),
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
