//! The clock guard: the time a run judges by, and how a server's answer moves it.

use super::{same_key, CLOCK_TOLERANCE_MS};
use crate::lease::LeaseClaims;

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
