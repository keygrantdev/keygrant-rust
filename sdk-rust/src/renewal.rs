//! The online validate (a renewal), decided without I/O: when `status()` asks
//! the server while it holds a lease that licenses this build
//! ([`asks_on_valid_lease`], `License::status`), and what an ask's answer saves
//! and says ([`renewal_outcome`], `Engine::renew`). The engine makes the
//! request, judges the lease a 200 carried, and saves the patch as the answer
//! to its ask.
//!
//! Licensing fails in favour of the paying customer: only an explicit verdict
//! (a lease, or a refusal `refusal_of` names) gives a status. No answer at all,
//! a 5xx, a 429, any other error, and a 200 without a lease that can be used
//! give none, and the caller's own answer (a valid lease, a stored refusal, the
//! lease's own end) stands.

use crate::adapters::HttpResponse;
use crate::lease::{LeaseClaims, OPEN_MAJOR};
use crate::offline::{
    ask_due, kept_with, keyed_by, lease_status, leased, refusal_patch, renews_early, LeaseCheck,
    LeaseFailure, ASK_AGAIN_MS, RETRY_UNANSWERED_MS,
};
use crate::state::{Patch, StoredState};
use crate::status::{DeviceKind, LicenseState, LicenseStatus};
use crate::wire::{error_of, no_answer, parse_lease_body, refusal_of};

/// What an ask's answer is judged from.
#[derive(Debug, Clone, Copy)]
pub struct RenewalInput<'a> {
    /// The answer; none when there was none at all (the request failed or timed out).
    pub response: Option<&'a HttpResponse>,
    /// The offline check (`check_lease`) of the lease the 200 carried, made as
    /// of `now` on this device; none when it carried none.
    pub check: Option<&'a LeaseCheck>,
    /// When the ask was made, by the device clock (ms): where the wait before
    /// asking again starts.
    pub asked_at: i64,
    /// The time the lease was judged at (ms): the guarded clock, reconciled to
    /// the server's (`Engine::server_now`).
    pub now: i64,
    /// This build's major, sent with the ask.
    pub major: i64,
    /// The stored key, which a status names.
    pub key: Option<&'a str>,
    /// The stored kept major (`kept_with`).
    pub kept_major: Option<i64>,
    /// What the ask read this device as: a machine id keys the install (`keyed_by`).
    pub device_kind: Option<DeviceKind>,
    /// The claim of this device's own fingerprint, which a device refusal is
    /// held to (`refusal_patch`).
    pub own_claim: Option<&'a str>,
}

/// What an ask's answer does.
#[derive(Debug, Clone, PartialEq)]
pub struct RenewalOutcome {
    /// Whether the server answered at all: false for no answer, a 5xx or a 429.
    pub answered: bool,
    /// The status the answer gives; none when it gives none, and the caller's
    /// own answer stands.
    pub status: Option<LicenseStatus>,
    /// What is saved, as the answer to the ask (`merged`, basis `answer`).
    pub patch: Patch,
}

/// What a validate's answer saves and says. Every patch carries
/// `deviceKind: machine` when this run read a machine id (`keyed_by`): the seat
/// is re-keyed by it whatever the answer, so the install is keyed by it from
/// now on.
///
///  - no answer at all: wait `RETRY_UNANSWERED_MS`, no status;
///  - a 5xx or a 429: wait `ASK_AGAIN_MS`, no status;
///  - a 200 with a lease that licenses this build: the lease, the major heard
///    (`checkedMajor`), the kept major (`kept_with`, cleared when it gives
///    none), any refusal and wait cleared (`leased`): licensed, with the
///    check's `test` flag and the lease's entitlements (`lease_status`);
///  - a 200 with a lease the server stands behind that does not license this
///    build or this device (`upgrade`, `outdated`, `device`): kept as above,
///    with no kept major, so every later status says the same offline:
///    invalid, with no entitlements (`lease_status`);
///  - any other 200 (no lease, or one expired or not this licence's): the
///    major heard and a wait of `ASK_AGAIN_MS`, no status; never a lockout on
///    a bad body;
///  - a refusal (`refusal_of` of the body's error word, on any status but a
///    200, a 5xx or a 429): the major heard, the wait, and the refusal
///    (`refusal_patch`): invalid with it;
///  - any other answer (a proxy's 4xx, an unknown product, a body that is not
///    an object): the major heard and the wait, no status.
pub fn renewal_outcome(input: &RenewalInput<'_>) -> RenewalOutcome {
    let keyed = keyed_by(input.device_kind);
    let wait = |ms: i64| {
        Patch {
            backoff_since: Some(Some(input.asked_at)),
            backoff_ms: Some(Some(ms)),
            ..Patch::default()
        }
        .then(keyed.clone())
    };
    let Some(response) = input.response else {
        return RenewalOutcome {
            answered: false,
            status: None,
            patch: wait(RETRY_UNANSWERED_MS),
        };
    };
    let backoff = wait(ASK_AGAIN_MS);
    if no_answer(response.status) {
        return RenewalOutcome {
            answered: false,
            status: None,
            patch: backoff,
        };
    }
    let heard = Patch {
        checked_major: Some(Some(input.major)),
        ..Patch::default()
    };
    if response.status == 200 {
        return lease_outcome(input, response, heard, backoff);
    }
    // Lock out ONLY on an explicit definitive verdict. Anything else (a
    // proxy's or WAF's 4xx, an unknown product, a malformed body, any answer
    // not recognised) is treated like a network failure, so a still-valid
    // lease is never revoked by an ambiguous answer.
    let refused = refusal_of(error_of(response.json.as_ref()));
    // On a verdict, the next status() must not license offline what the
    // server has just refused (`refusal_patch`: what is dropped, what is kept).
    let verdict = refused
        .map(|refused| refusal_patch(refused, input.major, input.own_claim))
        .unwrap_or_default();
    RenewalOutcome {
        answered: true,
        status: refused.map(|refused| invalid(refused.reason(), input.key)),
        patch: heard.then(backoff).then(verdict),
    }
}

/// A 200's answer ([`renewal_outcome`]): by the lease it carried, as checked here.
fn lease_outcome(
    input: &RenewalInput<'_>,
    response: &HttpResponse,
    heard: Patch,
    backoff: Patch,
) -> RenewalOutcome {
    let (lease, _) = parse_lease_body(response.json.as_ref());
    if let (Some(lease), Some(check)) = (lease.filter(|lease| !lease.is_empty()), input.check) {
        let with_lease = Patch {
            lease: Some(Some(lease)),
            ..heard.clone()
        };
        match check {
            // Licensed, with the check's `test` flag and the lease's entitlements
            // (offline.rs `lease_status`).
            LeaseCheck::Ok { claims, .. } => {
                let kept = kept_with(
                    input.kept_major,
                    claims.upgrade_until,
                    input.now,
                    input.major,
                );
                let patch = Patch {
                    kept_major: Some(kept),
                    ..with_lease
                }
                .then(leased())
                .then(keyed_by(input.device_kind));
                return RenewalOutcome {
                    answered: true,
                    status: Some(lease_status(check, input.key)),
                    patch,
                };
            }
            // A lease the server stands behind that does not license this
            // build, or this device: kept, so every later status() says the
            // same offline.
            LeaseCheck::Fail(
                LeaseFailure::Upgrade | LeaseFailure::Outdated | LeaseFailure::Device,
            ) => {
                let patch = with_lease.then(leased()).then(keyed_by(input.device_kind));
                return RenewalOutcome {
                    answered: true,
                    status: Some(lease_status(check, input.key)),
                    patch,
                };
            }
            LeaseCheck::Fail(_) => {}
        }
    }
    // A 200 but no usable lease: nothing to heal with; whatever the caller has
    // is kept (fail in favour of the customer) rather than locked out on a bad body.
    RenewalOutcome {
        answered: true,
        status: None,
        patch: heard.then(backoff),
    }
}

fn invalid(reason: crate::status::LicenseReason, key: Option<&str>) -> LicenseStatus {
    LicenseStatus::new(LicenseState::Invalid)
        .with_reason(reason)
        .with_key(key)
}

/// What the decision to ask on a valid lease reads ([`asks_on_valid_lease`]).
#[derive(Debug, Clone, Copy)]
pub struct ValidLeaseAsk<'a> {
    /// The lease's claims: its `major`, and its `iat` and `exp` (seconds).
    pub claims: &'a LeaseClaims,
    /// The stored state: the major the server last heard (`checkedMajor`), and
    /// the wait (`backoffSince`, `backoffMs`).
    pub state: &'a StoredState,
    /// This build's major.
    pub major: i64,
    /// The guarded clock (ms), which the lease's end is judged by.
    pub now: i64,
    /// The device clock as it is (ms), which the wait is judged by (`ask_due`).
    pub clock_now: i64,
}

/// Whether `status()`, holding a lease that licenses this build, asks the
/// server now (unforced; `refresh()` always asks): when the server should
/// hear from this device before the lease runs out (a newer major than it
/// last heard, on an open lease; or a lease nearing its end, by the guarded
/// clock), and only once the wait after the last ask is over (by the device
/// clock). The ask is bounded, and the valid lease is the answer when it gets
/// no verdict.
pub fn asks_on_valid_lease(ask: &ValidLeaseAsk<'_>) -> bool {
    let new_major =
        ask.claims.major >= OPEN_MAJOR && ask.major > ask.state.checked_major.unwrap_or(0);
    let due = new_major || renews_early(ask.claims, ask.now);
    due && ask_due(ask.state, ask.clock_now, false)
}
