//! The trial machinery under `License`: asking the server for this device's
//! trial and saving its answer (`ask_trial`), even when the disk refuses it for
//! now (the unsaved trial); a trial's status from what the device holds
//! (`trial_status`); and asking again for a trial whose lease does not verify
//! while its stored end is still ahead (`heal_trial`). `License` decides when.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::MutexGuard;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::device::Device;
use crate::license::License;
use crate::offline::{
    ask_due, merged, trial_status_of, Basis, Stamp, TrialLease, ASK_AGAIN_MS, RETRY_UNANSWERED_MS,
};
use crate::state::{present, Patch, StoredState};
use crate::status::{LicenseState, LicenseStatus};
use crate::wire::{earliest, parse_trial_body};

impl License {
    pub(crate) fn unsaved(&self) -> MutexGuard<'_, Option<(Patch, Basis)>> {
        self.unsaved_trial
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The stored state for a status, with a trial this process could not
    /// save; or none when it cannot be read just now.
    pub(crate) fn load_for_status(&self, read_only: bool) -> Option<StoredState> {
        let state = self.engine.load(read_only).ok()?;
        match self.unsaved().as_ref() {
            Some((patch, basis)) => Some(merged(&state, patch, basis)),
            None => Some(state),
        }
    }

    /// Save the trial this process could not, and forget it once saved.
    pub(crate) fn save_unsaved_trial(&self) {
        let Some((patch, basis)) = self.unsaved().clone() else {
            return;
        };
        if self.engine.save_best_effort(&patch, None, &basis) {
            *self.unsaved() = None;
        }
    }

    /// `start_trial()`'s trial ask, under the device it read for its asks.
    pub(crate) fn start_trial_now(&self, state: &StoredState, device: &Device) -> LicenseStatus {
        let (_, latest) = self.ask_trial(state, device, None);
        // The server's trial even when it could not be saved: kept for this
        // process until a later call saves it.
        self.trial_status(&latest, self.engine.effective_now(state))
    }

    /// Ask the server for this device's trial (`trial`, with the earliest
    /// start the device holds as `clientStart`) and save its answer: whether
    /// the server answered at all, and the state with its trial merged in.
    /// Asked under `device`, and bounded by `timeout` (the configured bound
    /// when none).
    pub(crate) fn ask_trial(
        &self,
        state: &StoredState,
        device: &Device,
        timeout: Option<Duration>,
    ) -> (bool, StoredState) {
        // The stash is the app's own adapter: one that panics is no evidence,
        // and the trial is still asked for (as one that fails already is).
        let registry = &self.engine.registry;
        let registry_start = catch_unwind(AssertUnwindSafe(|| registry.read_trial_start()))
            .ok()
            .flatten();
        let client_start = earliest(&[state.trial_start, registry_start]);
        let mut latest = state.clone();
        let mut reached = false;
        // A device that cannot be told this time starts no trial: as offline.
        if let Some(own) = &device.own {
            let asked = Stamp {
                at: self.engine.effective_now(state),
                seen: state.trial_at,
            };
            let mut body = Map::new();
            body.insert("fingerprint".into(), Value::from(own.clone()));
            if let Some(kind) = device.kind {
                body.insert("fingerprintKind".into(), Value::from(kind.as_str()));
            }
            if let Some(start) = client_start {
                body.insert("clientStart".into(), Value::from(start));
            }
            let answer = self.engine.post("trial", Value::Object(body), timeout).ok();
            reached = answer.is_some();
            let trial = answer
                .filter(|response| response.status == 200)
                .and_then(|r| parse_trial_body(r.json.as_ref()));
            if let Some(trial) = trial {
                // The earliest start across all evidence: never moved forward.
                let trial_start =
                    earliest(&[Some(trial.trial_start), state.trial_start, registry_start])
                        .unwrap_or(trial.trial_start);
                // Mirrored to the stash first; a stash that panics loses no trial.
                let _ = catch_unwind(AssertUnwindSafe(|| registry.write_trial_start(trial_start)));
                // A trial lease this build cannot verify (signed with a key the
                // product rotated to ahead of this build, say) never replaces
                // one it can: a trial runs only on a valid lease
                // (`trial_status_of`), so taking it would end a running trial.
                // As a renewal keeps the lease it has when the answer's will
                // not verify. An answer with no lease (the trial is over) is
                // taken as it is.
                let trial_lease = if self.keeps_its_trial_lease(state, trial.lease.as_deref()) {
                    state.trial_lease.clone()
                } else {
                    trial.lease
                };
                let patch = Patch {
                    trial_start: Some(Some(trial_start)),
                    trial_ends_at: Some(Some(trial.trial_ends_at)),
                    trial_lease: Some(trial_lease),
                    ..Patch::default()
                };
                let basis = Basis {
                    trial: Some(asked),
                    ..Basis::default()
                };
                latest = merged(state, &patch, &basis);
                // The server's trial even when it could not be saved: kept
                // for this process until a later call saves it.
                let saved = self.engine.save_best_effort(&patch, None, &basis);
                *self.unsaved() = if saved { None } else { Some((patch, basis)) };
            }
        }
        (reached, latest)
    }

    /// Whether a trial answer's lease fails here while the stored one
    /// verifies ([`ask_trial`](Self::ask_trial)): only when the answer has a
    /// lease and one is stored.
    pub(crate) fn keeps_its_trial_lease(
        &self,
        state: &StoredState,
        answered: Option<&str>,
    ) -> bool {
        let (Some(answered), Some(stored)) = (answered, state.trial_lease.as_deref()) else {
            return false;
        };
        let now = self.engine.effective_now(state);
        if self
            .engine
            .trial_lease(answered, now, state.device_kind)
            .valid
        {
            return false;
        }
        self.engine
            .trial_lease(stored, now, state.device_kind)
            .valid
    }

    /// The trial's status: live only on a valid trial lease, to its end, with
    /// what that lease grants.
    pub(crate) fn trial_status(&self, state: &StoredState, now: i64) -> LicenseStatus {
        let lease = self.trial_lease(state, now);
        trial_status_of(state, now, lease.valid, lease.ends_at, &lease.entitlements)
    }

    /// Whether the stored trial lease is valid here, and when it is, the end
    /// it was signed to and its entitlements (offline.rs `trial_lease_of`).
    pub(crate) fn trial_lease(&self, state: &StoredState, now: i64) -> TrialLease {
        present(&state.trial_lease).map_or_else(TrialLease::default, |lease| {
            self.engine.trial_lease(lease, now, state.device_kind)
        })
    }

    /// The trial's status as `status()` answers it: a trial whose lease does
    /// not verify here while its stored end is still ahead is asked for again
    /// first ([`heal_trial`](Self::heal_trial)), and a running one reports
    /// its launch when due (launch.rs `report_launch`); neither read-only.
    pub(crate) fn trial_status_asking(
        &self,
        state: &StoredState,
        now: i64,
        force_online: bool,
        read_only: bool,
    ) -> LicenseStatus {
        let lease = self.trial_lease(state, now);
        let status = trial_status_of(state, now, lease.valid, lease.ends_at, &lease.entitlements);
        if read_only {
            return status;
        }
        if status.state == LicenseState::Trial {
            self.report_launch(state, now);
            return status;
        }
        let stored_end_ahead = state.trial_ends_at.is_some_and(|end| now < end);
        if lease.valid || !stored_end_ahead {
            return status;
        }
        self.heal_trial(state, force_online).unwrap_or(status)
    }

    /// A trial whose lease does not verify here while its stored end is still
    /// ahead: a lease signed with the product's key before it was rotated (on a
    /// build that ships the new one), one signed to end short of the trial's
    /// end, or one never saved whole. The stored end is unsigned, so it runs no
    /// trial on its own (`trial_status_of`): the server is asked for the trial
    /// again instead, under the usual waits (`ask_due`) and bounded as a status
    /// check is, and its answer stands.
    ///
    /// An ask that brings back no trial this build can verify starts the wait
    /// (`RETRY_UNANSWERED_MS` when nothing answered, `ASK_AGAIN_MS` when the
    /// server did), so a device that is offline, or that carries a key the
    /// product no longer signs with, is not asked on every `status()`. None
    /// when it did not ask, or nothing answered: the local answer stands.
    pub(crate) fn heal_trial(
        &self,
        state: &StoredState,
        force_online: bool,
    ) -> Option<LicenseStatus> {
        let asked_at = self.engine.clock.now();
        if !ask_due(state, asked_at, force_online) {
            return None;
        }
        let timings = &self.engine.timings;
        let device = self.engine.device(timings.machine_id, state.device_kind);
        let (reached, latest) = self.ask_trial(state, &device, Some(timings.status_check));
        let after = reached.then(|| self.trial_status(&latest, self.engine.effective_now(&latest)));
        if after
            .as_ref()
            .is_some_and(|status| status.state == LicenseState::Trial)
        {
            return after;
        }
        let wait = Patch {
            backoff_since: Some(Some(asked_at)),
            backoff_ms: Some(Some(if reached {
                ASK_AGAIN_MS
            } else {
                RETRY_UNANSWERED_MS
            })),
            ..Patch::default()
        };
        self.engine.save_best_effort(&wait, None, &Basis::default());
        after
    }
}
