//! Buying from the app with no key typed, under `License`: the purchase link
//! ([`License::purchase_url_now`], which keeps this install's one-time secret
//! and hands out the reference to it) and the pickup ([`License::pick_up`]):
//! while no key is held and a secret is, asking the server for the key bought
//! with it (`claim`) and saving it exactly as `activate(key)` saves one. What
//! each answer does is `pickup.rs`; `License` decides when to ask.

use std::time::Duration;

use crate::adapters::HttpResponse;
use crate::device::Device;
use crate::engine::Holder;
use crate::error::Error;
use crate::license::License;
use crate::offline::{merged, Asked, Basis, LeaseCheck};
use crate::pickup::{
    claim_due, claim_outcome, claim_request, new_purchase_secret_from, no_purchase, purchase_link,
    purchase_token_kept, ClaimAsk, ClaimInput, ClaimOutcome, ClaimRequest, ClaimResult, PurchaseOf,
};
use crate::state::{present, Patch, StoredState};
use crate::status::{DeviceKind, LicenseStatus};
use crate::wire::parse_claim_body;

/// How a pickup asks: for `start_trial()` (`start`: always, under the device
/// `start_trial()` read for both its asks, `device`, bounded as a status
/// check is, a host-name fallback allowed), or for `status()` (when `force`d,
/// under the HTTP bound, or when `claim_due`, bounded as a status check is),
/// reading the device as a status does and asking only under its own
/// fingerprint.
pub(crate) struct PickupAsk<'a> {
    pub start: bool,
    pub force: bool,
    pub device: Option<&'a Device>,
}

/// What a pickup did: the licensed status when a bought key licensed this
/// device, else the state to answer from.
pub(crate) enum Pickup {
    Bought(LicenseStatus),
    State(Box<StoredState>),
}

/// A save about the purchase secret `token` (`Basis::purchase`): for a
/// wait, as its link was handed out when asked (`at`); for a drop, none.
fn about_secret(token: &str, at: Option<i64>) -> Basis {
    Basis {
        purchase: Some(PurchaseOf {
            token: token.to_owned(),
            at,
        }),
        ..Basis::default()
    }
}

impl License {
    /// `purchase_url`'s answer: none while a key is held; else the link
    /// carrying the reference to the held secret, or to a new one, saved with
    /// when its link was handed out (the claim's wait cleared, so the next
    /// `status()` asks). Read-only (behind a call that has not finished), a
    /// held secret's link is answered as it is and none held is `Busy`: a
    /// secret that is not saved could never be picked up.
    pub(crate) fn purchase_url_now(
        &self,
        link: &str,
        read_only: bool,
    ) -> Result<Option<String>, Error> {
        let state = self.engine.load(read_only).map_err(Error::Storage)?;
        if present(&state.key).is_some() {
            return Ok(None);
        }
        let now = self.engine.effective_now(&state);
        let held = purchase_token_kept(&state, now)
            .then(|| state.purchase_token.clone())
            .flatten();
        let secret = match (held, read_only) {
            (Some(secret), true) => secret,
            (None, true) => return Err(Error::Busy),
            (held, false) => {
                let secret = match held {
                    Some(secret) => secret,
                    None => new_purchase_secret_from(self.engine.random).map_err(Error::Random)?,
                };
                let patch = Patch {
                    purchase_token: Some(Some(secret.clone())),
                    purchase_token_at: Some(Some(now)),
                    claim_asked_at: Some(None),
                    ..Patch::default()
                };
                self.engine
                    .save(&patch, None, &Basis::default())
                    .map_err(Error::Storage)?;
                secret
            }
        };
        purchase_link(link, &secret)
            .map(Some)
            .ok_or(Error::InvalidUrl)
    }

    /// The key bought through `purchase_url`, picked up: with no key held
    /// and a secret held, the secret is dropped once past its bound
    /// (`purchase_token_kept`), and otherwise the claim is asked as `ask`
    /// says. Never asked behind a call that has not finished: the caller
    /// skips it.
    pub(crate) fn pick_up(&self, state: &StoredState, ask: PickupAsk<'_>) -> Pickup {
        let Some(secret) = &state.purchase_token else {
            return Pickup::State(Box::new(state.clone()));
        };
        if present(&state.key).is_some() {
            return Pickup::State(Box::new(state.clone()));
        }
        let now = self.engine.effective_now(state);
        if !purchase_token_kept(state, now) {
            return Pickup::State(Box::new(self.drop_secret(state, secret)));
        }
        if !ask.start && !ask.force && !claim_due(state, self.engine.clock.now(), now) {
            return Pickup::State(Box::new(state.clone()));
        }
        self.claim(state, secret, ask)
    }

    /// The secret dropped (saved about it, best-effort): the state without it.
    fn drop_secret(&self, state: &StoredState, secret: &str) -> StoredState {
        let basis = about_secret(secret, None);
        let patch = no_purchase();
        self.engine.save_best_effort(&patch, None, &basis);
        merged(state, &patch, &basis)
    }

    /// Ask for the key bought with `secret` (`claim`) and save what the
    /// answer does (pickup.rs `claim_outcome`). The body is pickup.rs
    /// `claim_request`'s: from `status()`, a device whose own fingerprint did
    /// not read is not asked at all, and nothing is saved, so the next status
    /// asks again; at `start_trial()`, a device with no fingerprint at all is
    /// no answer.
    fn claim(&self, state: &StoredState, secret: &str, ask: PickupAsk<'_>) -> Pickup {
        let clock_now = self.engine.clock.now();
        let asked_at = self.engine.effective_now(state);
        let timings = &self.engine.timings;
        let read;
        let device = match ask.device {
            Some(device) => device,
            None => {
                read = self.engine.device(timings.machine_id, state.device_kind);
                &read
            }
        };
        let body = claim_request(&ClaimAsk {
            secret,
            device,
            name: self.device_name.as_deref(),
            major: self.engine.major,
            start: ask.start,
        });
        if body.is_none() && !ask.start {
            return Pickup::State(Box::new(state.clone()));
        }
        let kind = body.as_ref().and_then(|body| body.kind);
        let timeout = (ask.start || !ask.force).then_some(timings.status_check);
        let response = body.and_then(|body| self.post_claim(&body, timeout));
        let (check, now) = self.judge_claim(response.as_ref(), kind, asked_at);
        let outcome = claim_outcome(&ClaimInput {
            response: response.as_ref(),
            check: check.as_ref(),
            clock_now,
            now,
            major: self.engine.major,
            kind,
        });
        self.save_claim(state, secret, asked_at, outcome)
    }

    /// What a claim's answer does, saved best-effort: a bought key as an
    /// activation, answered all the same when the disk refuses it (the next
    /// claim answers it again: the server knows this device by any of the
    /// fingerprints it sends); a wait about the secret as its link was handed
    /// out when asked, a drop about the secret alone (offline.rs `merged`).
    fn save_claim(
        &self,
        state: &StoredState,
        secret: &str,
        asked_at: i64,
        outcome: ClaimOutcome,
    ) -> Pickup {
        if let (ClaimResult::Bought, Some(status)) = (outcome.result, outcome.status) {
            let basis = Basis {
                activation: Some(Asked {
                    at: asked_at,
                    activation_id: outcome.patch.activation_id.clone().flatten(),
                    seen: state.verdict_at,
                }),
                ..Basis::default()
            };
            self.engine.save_best_effort(&outcome.patch, None, &basis);
            return Pickup::Bought(status);
        }
        let at = (outcome.result == ClaimResult::Kept)
            .then_some(state.purchase_token_at)
            .flatten();
        let basis = about_secret(secret, at);
        self.engine.save_best_effort(&outcome.patch, None, &basis);
        Pickup::State(Box::new(merged(state, &outcome.patch, &basis)))
    }

    /// POST `claim` with its body (pickup.rs `claim_request`); none when no
    /// answer came.
    fn post_claim(&self, body: &ClaimRequest, timeout: Option<Duration>) -> Option<HttpResponse> {
        self.engine.post("claim", body.to_json(), timeout).ok()
    }

    /// The lease a claim's 200 carried with a key and an activation id,
    /// checked for them on this device as of the server's time
    /// (`Engine::judged`); no check for any other answer. With the time it
    /// was judged at.
    fn judge_claim(
        &self,
        response: Option<&HttpResponse>,
        kind: Option<DeviceKind>,
        asked_at: i64,
    ) -> (Option<LeaseCheck>, i64) {
        let body = response
            .filter(|response| response.status == 200)
            .map(|response| parse_claim_body(response.json.as_ref()))
            .unwrap_or_default();
        let (Some(key), Some(activation_id), Some(lease)) =
            (&body.key, &body.activation_id, &body.lease)
        else {
            return (None, asked_at);
        };
        let holder = Holder {
            key: Some(key),
            activation_id: Some(activation_id),
            device_kind: kind,
        };
        let (check, now) = self.engine.judged(lease, &holder, asked_at);
        (Some(check), now)
    }
}

/// The purchase secret's random source, which no app can reach: tested here,
/// inside the crate, over an in-memory state and a fixed clock.
#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use crate::adapters::{Clock, StorageAdapter};
    use crate::pickup::{purchase_reference, RandomSource};
    use crate::{Error, License, LicenseConfig, StoredState};

    const NOW: i64 = 1_757_000_000_000;
    const LINK: &str = "https://buy.stripe.com/test_abc";
    const PUBLIC_JWK: &str =
        r#"{"kty":"OKP","crv":"Ed25519","x":"Ohg4h8SrYQ0m1su_zVacJLLwf_6rNsvOtV5oWY6CzJs"}"#;

    /// The state in memory, its writes counted.
    #[derive(Default)]
    struct Memory {
        state: Mutex<Option<StoredState>>,
        writes: Mutex<usize>,
    }

    impl StorageAdapter for Memory {
        fn read(&self, _read_only: bool) -> io::Result<Option<StoredState>> {
            Ok(self.state.lock().expect("the state").clone())
        }
        fn write(&self, state: &StoredState) -> io::Result<()> {
            *self.writes.lock().expect("the count") += 1;
            *self.state.lock().expect("the state") = Some(state.clone());
            Ok(())
        }
    }

    struct At(i64);

    impl Clock for At {
        fn now(&self) -> i64 {
            self.0
        }
    }

    fn license(storage: &Arc<Memory>, random: RandomSource) -> License {
        let config = LicenseConfig::new("sluice", "https://api.test", PUBLIC_JWK)
            .storage(storage.clone())
            .clock(Arc::new(At(NOW)))
            .random(random);
        License::new(config).expect("a licence")
    }

    /// A random source giving the bytes 0 to 31, and checking that 32 are drawn.
    fn bytes_0_to_31(bytes: &mut [u8]) -> io::Result<()> {
        assert_eq!(bytes.len(), 32);
        for (at, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::try_from(at).expect("a byte");
        }
        Ok(())
    }

    /// A random source that fails.
    fn no_entropy(_: &mut [u8]) -> io::Result<()> {
        Err(io::Error::other("no entropy"))
    }

    #[test]
    fn draws_the_secret_as_32_bytes_from_the_random_source() {
        let storage = Arc::new(Memory::default());
        let url = license(&storage, bytes_0_to_31)
            .purchase_url(LINK)
            .expect("a link");
        let secret = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
        let stored = storage.state.lock().expect("the state").clone();
        assert_eq!(
            stored.and_then(|state| state.purchase_token).as_deref(),
            Some(secret)
        );
        assert_eq!(
            url,
            Some(format!(
                "{LINK}?client_reference_id={}",
                purchase_reference(secret)
            ))
        );
    }

    #[test]
    fn the_default_random_source_draws_anew_every_time() {
        let random = LicenseConfig::new("sluice", "https://api.test", PUBLIC_JWK).random;
        let (mut first, mut second) = ([0u8; 32], [0u8; 32]);
        random(&mut first).expect("32 bytes");
        random(&mut second).expect("32 bytes");
        assert_ne!(first, second);
    }

    #[test]
    fn fails_with_error_random_and_saves_nothing_when_the_random_source_fails() {
        let storage = Arc::new(Memory::default());
        let answer = license(&storage, no_entropy).purchase_url(LINK);
        assert!(matches!(answer, Err(Error::Random(_))));
        assert_eq!(*storage.writes.lock().expect("the count"), 0);
    }
}
