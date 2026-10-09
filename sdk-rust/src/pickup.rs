//! Buying from the app with no key typed, decided without I/O: the purchase
//! secret `purchase_url` keeps (`new_purchase_secret_from`), the reference to
//! it a link carries ([`purchase_reference`], [`purchase_link`], built as
//! `upgrade_url`'s link is: [`referenced_link`]), how long a secret is held
//! ([`purchase_token_kept`]), when `status()` asks for the key bought with it
//! ([`claim_due`]), and what the claim's answer saves and says
//! ([`claim_outcome`]). `purchases.rs` makes the requests and the saves.

use std::io;

use sha2::{Digest, Sha256};

use crate::adapters::HttpResponse;
use crate::lease::{b64url_encode, LeaseClaims};
use crate::offline::{kept_with, lease_status, leased, LeaseCheck, DAY_MS};
use crate::state::{Patch, StoredState};
use crate::status::{DeviceKind, LicenseStatus};
use crate::wire::{no_answer, parse_claim_body};

pub use crate::claim_body::{
    claim_request, ClaimAsk, ClaimRequest, MAX_CLAIM_FINGERPRINTS, MAX_CLAIM_FINGERPRINT_LENGTH,
};
pub use crate::url::referenced_link;

/// How long after a claim ask that brought no key back (`claimAskedAt`)
/// before `status()` asks again on its own (`claim_due`, ms): a customer back
/// from the checkout finds the app licensed within minutes, and a device with
/// a token held does not ask on every `status()`. `refresh()` and
/// `start_trial()` ask at once.
pub const CLAIM_WAIT_MS: i64 = 5 * 60_000;

/// How long after `purchase_url` last handed a link out (`purchaseTokenAt`)
/// an unforced `status()` still asks the claim (`claim_due`, ms): a customer
/// who buys does so soon after opening the link. After it, only `refresh()`
/// and `start_trial()` ask, until [`PURCHASE_TOKEN_MS`], so an abandoned
/// checkout does not put the network in front of every launch.
pub const CLAIM_EAGER_MS: i64 = DAY_MS;

/// How long a purchase secret is held after `purchase_url` last handed out
/// its link (`purchase_token_kept`, by the guarded clock, ms): a checkout left
/// that long is not coming back, and a secret past it is dropped, never sent.
/// A drop is the device's alone: it revokes nothing on the server.
pub const PURCHASE_TOKEN_MS: i64 = 30 * DAY_MS;

/// What starts every purchase reference, so the server tells one apart from
/// an activation id or a key.
pub const PURCHASE_REFERENCE_PREFIX: &str = "kgp_";

/// Where a purchase secret's bytes come from: [`os_random`], the platform's
/// CSPRNG, but in the crate's own tests.
pub(crate) type RandomSource = fn(&mut [u8]) -> io::Result<()>;

/// `bytes` filled from the platform's CSPRNG (`getrandom`); an error only
/// when it fails.
pub(crate) fn os_random(bytes: &mut [u8]) -> io::Result<()> {
    getrandom::getrandom(bytes).map_err(|error| io::Error::other(error.to_string()))
}

/// A new purchase secret: the base64url (no padding) of 32 bytes drawn from
/// `random` (the platform's CSPRNG, [`os_random`]), 43 characters. It stays
/// on the device (`purchaseToken`) and is sent only in the claim: a link
/// carries the reference to it ([`purchase_reference`]), so a link that leaks
/// (browser history, the checkout's own records) claims nothing. Random, and
/// never the fingerprint: a fingerprint is no secret. An error only when the
/// random source fails.
pub(crate) fn new_purchase_secret_from(random: RandomSource) -> io::Result<String> {
    let mut bytes = [0u8; 32];
    random(&mut bytes)?;
    Ok(b64url_encode(&bytes))
}

/// The reference a payment link carries for `secret`: `kgp_` and the
/// base64url (no padding) of the SHA-256 of the secret's UTF-8 bytes, 47
/// characters. The checkout keeps it on the key it mints; the server derives
/// it from the secret a claim sends, to find that key.
pub fn purchase_reference(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    format!("{PURCHASE_REFERENCE_PREFIX}{}", b64url_encode(&digest))
}

/// The link `purchase_url` answers for a held `secret`: the link with its
/// reference ([`referenced_link`]); none when the link is not an absolute
/// `http`/`https` URL.
pub fn purchase_link(link: &str, secret: &str) -> Option<String> {
    referenced_link(link, &purchase_reference(secret))
}

/// Whether the stored purchase secret is still held at `now` (the guarded
/// clock): there is one, stamped with when `purchase_url` last handed out its
/// link (`purchaseTokenAt`), and less than [`PURCHASE_TOKEN_MS`] has passed
/// since. A stamp ahead of `now` holds it too: no time has passed that can be
/// measured. A secret with no stamp is not held.
pub fn purchase_token_kept(state: &StoredState, now: i64) -> bool {
    match (&state.purchase_token, state.purchase_token_at) {
        (Some(_), Some(at)) => now.saturating_sub(at) < PURCHASE_TOKEN_MS,
        _ => false,
    }
}

/// Whether `status()` asks the claim now, unforced (`refresh()` and
/// `start_trial()` always ask). Only while the link is fresh: less than
/// [`CLAIM_EAGER_MS`] since `purchase_url` last handed it out
/// (`purchaseTokenAt`, judged at `now`, the guarded clock; a stamp ahead of
/// `now` is fresh), so an abandoned checkout does not put the network in
/// front of every launch for a month. And only once the wait is over
/// (`claimAskedAt`, judged at `clock_now`, the device clock, as `ask_due`:
/// the wait is courtesy, not protection): not asked since the link, the
/// device clock set back before the last ask, or [`CLAIM_WAIT_MS`] since it.
pub fn claim_due(state: &StoredState, clock_now: i64, now: i64) -> bool {
    let fresh = state
        .purchase_token_at
        .is_some_and(|at| now.saturating_sub(at) < CLAIM_EAGER_MS);
    if !fresh {
        return false;
    }
    match state.claim_asked_at {
        None => true,
        Some(asked) => clock_now < asked || clock_now.saturating_sub(asked) >= CLAIM_WAIT_MS,
    }
}

/// The purchase secret a save is about.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PurchaseOf {
    /// The secret (`purchaseToken`).
    pub token: String,
    /// For a wait, the `purchaseTokenAt` its asker read: it lands only while
    /// that is stored too. A link handed out again since (another process's
    /// `purchase_url`) cleared the wait so that the next `status()` asks at
    /// once, and a late answer must not start it again. A drop carries none:
    /// a spent secret is dead however recently its link was handed out.
    pub at: Option<i64>,
}

/// Whether a save about a purchase secret is about the one stored, as
/// handed out when its asker read it (`Basis::purchase`, offline.rs `merged`).
pub(crate) fn about_stored(latest: &StoredState, purchase: &PurchaseOf) -> bool {
    latest.purchase_token.as_deref() == Some(purchase.token.as_str())
        && purchase
            .at
            .is_none_or(|at| latest.purchase_token_at == Some(at))
}

/// The purchase secret and its stamps, which a save about a secret writes
/// only while it is stored (`Basis::purchase`, offline.rs `merged`).
pub(crate) const PURCHASE_FIELDS: &[&str] = &["purchaseToken", "purchaseTokenAt", "claimAskedAt"];

/// What an activation (typed, or a bought key picked up) and a deactivation
/// clear: the purchase secret and its stamps. A secret left on a keyed install
/// would, after a later deactivation, activate the device on a key the
/// customer may have meant for another machine.
pub fn no_purchase() -> Patch {
    Patch {
        purchase_token: Some(None),
        purchase_token_at: Some(None),
        claim_asked_at: Some(None),
        ..Patch::default()
    }
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

/// What an activation saves, a typed key's and a bought key's claim alike,
/// stamped as an activation: the key, the activation, the lease, the major
/// heard, the kept major (`kept_with`), any refusal and wait cleared
/// (`leased`), what the install is keyed by (cleared for a fingerprint of
/// unknown kind), and the purchase secret and its stamps cleared (`no_purchase`).
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
    .then(no_purchase())
}

/// What a claim's answer is judged from ([`claim_outcome`]).
#[derive(Debug, Clone, Copy)]
pub struct ClaimInput<'a> {
    /// The answer; none when there was none (it failed or timed out, or no
    /// fingerprint could be sent).
    pub response: Option<&'a HttpResponse>,
    /// The offline check (`check_lease`, as of `now`, for the key and
    /// activation it came with, on this device) of the lease a 200 carried
    /// with a key and an activation id; none otherwise.
    pub check: Option<&'a LeaseCheck>,
    /// The device clock when the claim was asked: where the wait before
    /// asking again starts.
    pub clock_now: i64,
    /// The time the lease was judged at: the guarded clock, reconciled to the server's.
    pub now: i64,
    /// This build's major.
    pub major: i64,
    /// What the claim's fingerprint hashes.
    pub kind: Option<DeviceKind>,
}

/// What a claim's answer does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimResult {
    /// A bought key licenses this device.
    Bought,
    /// The secret is kept, and the wait before asking again started.
    Kept,
    /// The secret is let go.
    Dropped,
}

impl ClaimResult {
    /// The reference's word for it.
    pub fn as_str(self) -> &'static str {
        match self {
            ClaimResult::Bought => "bought",
            ClaimResult::Kept => "kept",
            ClaimResult::Dropped => "dropped",
        }
    }
}

/// What a claim's answer saves and says.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaimOutcome {
    /// What the answer does.
    pub result: ClaimResult,
    /// The status a bought key gives; none otherwise.
    pub status: Option<LicenseStatus>,
    /// What is saved: a bought key as an activation, a kept or dropped
    /// secret as a save about that secret.
    pub patch: Patch,
}

/// What a claim's answer saves and says:
///  - a 200 with a key, an activation id and a lease (strings) whose lease
///    licenses this build here (`check`): saved exactly as `activate(key)`
///    saves ([`activation_patch`]), and licensed (`lease_status`), even when
///    it also says pending or carries an error word;
///  - a 200 with all three whose lease does not license this build here: the
///    secret dropped (the customer has the key by email, and typing it gives
///    the proper error);
///  - a 200 `{pending: true}` ("not bought yet"), no answer, a 5xx or a 429,
///    and any other answer with no error word (a captive portal's page, a
///    404 page from a server with no claim endpoint): the secret kept, and
///    the wait started (`claimAskedAt`);
///  - any other answer with an error word (`spent`, `pickup-off`, a
///    refusal): the secret dropped.
///
/// Only a bought key gives a status: otherwise the caller answers as it
/// would with no secret held.
pub fn claim_outcome(input: &ClaimInput<'_>) -> ClaimOutcome {
    let kept = ClaimOutcome {
        result: ClaimResult::Kept,
        status: None,
        patch: Patch {
            claim_asked_at: Some(Some(input.clock_now)),
            ..Patch::default()
        },
    };
    let dropped = ClaimOutcome {
        result: ClaimResult::Dropped,
        status: None,
        patch: no_purchase(),
    };
    let Some(response) = input
        .response
        .filter(|response| !no_answer(response.status))
    else {
        return kept;
    };
    let body = parse_claim_body(response.json.as_ref());
    let ok = response.status == 200;
    if let (true, Some(key), Some(activation_id), Some(lease)) =
        (ok, &body.key, &body.activation_id, &body.lease)
    {
        let Some(check @ LeaseCheck::Ok { claims, .. }) = input.check else {
            return dropped;
        };
        let patch = activation_patch(&Activation {
            key,
            activation_id,
            lease,
            claims,
            now: input.now,
            major: input.major,
            kind: input.kind,
        });
        return ClaimOutcome {
            result: ClaimResult::Bought,
            status: Some(lease_status(check, Some(key))),
            patch,
        };
    }
    if (ok && body.pending) || body.error.is_none() {
        kept
    } else {
        dropped
    }
}
