//! Asking the server to renew: the validate, its body, and the clock it brings back.

use std::io;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::adapters::HttpResponse;
use crate::device::Device;
use crate::offline::{clock_correction, Asked, Basis, LeaseCheck, SignedAt};
use crate::renewal::{renewal_outcome, RenewalInput};
use crate::state::{present, Patch, StoredState};
use crate::status::LicenseStatus;
use crate::wire::parse_lease_body;

use super::{Engine, GuardSave, Holder, Renewal};

impl Engine {
    /// `now` for judging a lease the server has just signed, with the clock
    /// guard reconciled to it: when the guard is more than the tolerance past
    /// the lease's signing time, it comes back to the device clock, never
    /// below `iat`. Only a lease signed FOR THIS DEVICE (this key and
    /// activation, no older than the one held) can move it.
    pub fn server_now(
        &self,
        lease: &str,
        key: &str,
        activation_id: &str,
        save: GuardSave,
    ) -> io::Result<i64> {
        let stored = self.load(false)?;
        let now = self.effective_now(&stored);
        let verified = self.verify(lease, now);
        let held_iat = present(&stored.lease)
            .and_then(|held| self.verify(held, now).claims().map(|claims| claims.iat));
        let corrected = clock_correction(
            self.clock.now(),
            stored.last_seen,
            verified.claims().map(SignedAt::of),
            held_iat,
            key,
            activation_id,
        );
        let Some(corrected) = corrected else {
            return Ok(now);
        };
        match save {
            GuardSave::Strict => {
                self.save(&Patch::default(), Some(corrected), &Basis::default())?
            }
            GuardSave::BestEffort => {
                self.save_best_effort(&Patch::default(), Some(corrected), &Basis::default());
            }
        }
        Ok(corrected)
    }

    /// An online validate; a status on a definitive answer, else none.
    pub fn try_renew(
        &self,
        state: &StoredState,
        timeout: Option<Duration>,
    ) -> Option<LicenseStatus> {
        match self.renew(state, timeout) {
            Renewal::Answered(status) => status,
            Renewal::Unanswered => None,
        }
    }

    /// An online validate, and whether the server gave a verdict. What its
    /// answer saves and says is renewal.rs `renewal_outcome`: any verdict (a
    /// lease, a refusal, a body it could not use) records the major it was
    /// sent as heard, and an ask that came back without a lease starts the
    /// wait before the SDK asks again on its own.
    pub fn renew(&self, state: &StoredState, timeout: Option<Duration>) -> Renewal {
        // The wait runs on the device clock, as `ask_due` reads it; the answer
        // is stamped by the guarded one, which a clock put back does not move
        // back, so an answer asked for later still reads as later to another process.
        let asked_at = self.clock.now();
        let asked = Asked {
            at: self.effective_now(state),
            activation_id: state.activation_id.clone(),
            seen: state.verdict_at,
        };
        // A machine id read here keys this install from now on (`deviceKind`):
        // written with whatever this ask writes, as the seat is re-keyed by it.
        let device = self.device(self.timings.machine_id, state.device_kind);
        let response = self
            .post("validate", self.validate_body(state, &device), timeout)
            .ok();
        let (check, now) = self.judge_fresh(state, response.as_ref());
        let outcome = renewal_outcome(&RenewalInput {
            response: response.as_ref(),
            check: check.as_ref(),
            asked_at,
            now,
            major: self.major,
            key: state.key.as_deref(),
            kept_major: state.kept_major,
            device_kind: device.kind,
            own_claim: device.own_claim.as_deref(),
        });
        // Saved as the answer to its ask; a save that fails leaves the answer
        // standing (`save_best_effort`).
        let basis = Basis {
            answer: Some(asked),
            ..Basis::default()
        };
        self.save_best_effort(&outcome.patch, None, &basis);
        if outcome.answered {
            Renewal::Answered(outcome.status)
        } else {
            Renewal::Unanswered
        }
    }

    /// The lease a 200 carried, checked here as of the server's time (the
    /// clock guard reconciled to it first, `server_now`); none, and the
    /// guarded clock, when there was no such lease.
    pub(super) fn judge_fresh(
        &self,
        state: &StoredState,
        response: Option<&HttpResponse>,
    ) -> (Option<LeaseCheck>, i64) {
        let lease = response
            .filter(|response| response.status == 200)
            .and_then(|response| parse_lease_body(response.json.as_ref()).0)
            .filter(|lease| !lease.is_empty());
        let Some(lease) = lease else {
            return (None, self.effective_now(state));
        };
        let (check, now) = self.judged(&lease, &Holder::of(state), self.effective_now(state));
        (Some(check), now)
    }

    /// A lease the server just answered, checked for `holder` on this device
    /// (`evaluate`) as of the server's time (`server_now`, the clock guard
    /// brought back best-effort; `fallback_now` when the state cannot be read
    /// or saved just now). With the time it was judged at.
    pub fn judged(&self, lease: &str, holder: &Holder<'_>, fallback_now: i64) -> (LeaseCheck, i64) {
        let key = holder.key.unwrap_or_default();
        let activation_id = holder.activation_id.unwrap_or_default();
        let now = self
            .server_now(lease, key, activation_id, GuardSave::BestEffort)
            .unwrap_or(fallback_now);
        (self.evaluate(lease, now, holder), now)
    }

    /// A validate's body: with every fingerprint this device answers to when
    /// it can tell, and none when it cannot.
    pub(super) fn validate_body(&self, state: &StoredState, device: &Device) -> Value {
        let mut body = Map::new();
        if let Some(key) = &state.key {
            body.insert("key".into(), json!(key));
        }
        if let Some(activation_id) = &state.activation_id {
            body.insert("activationId".into(), json!(activation_id));
        }
        body.insert("major".into(), json!(self.major));
        if let Some(kept) = state.kept_major {
            body.insert("keptMajor".into(), json!(kept));
        }
        if device.can_tell {
            body.insert("fingerprints".into(), json!(device.fingerprints));
            if let Some(kind) = device.kind {
                body.insert("fingerprintKind".into(), json!(kind.as_str()));
            }
        }
        body.insert("kids".into(), self.kids());
        Value::Object(body)
    }

    /// The ids of the keys this build carries (`kid`, each key's thumbprint),
    /// in the order configured: sent with each activation and validate, so
    /// the product's dashboard can count the devices whose builds do not
    /// carry a key yet.
    pub fn kids(&self) -> Value {
        json!(self.keys.kids())
    }
}
