//! The status from what is stored, and when it asks the server first.

use super::{invalid, License};
use crate::device::Device;
use crate::engine::Holder;
use crate::offline::{
    ask_due, kept_with, lease_status, standing_refusal, About, Basis, DeviceView, LeaseCheck,
};
use crate::renewal::{asks_on_valid_lease, ValidLeaseAsk};
use crate::state::{present, Patch, StoredState};
use crate::status::{LicenseState, LicenseStatus, Refusal};

impl License {
    /// The status, asking the server when it should. `read_only`: from what
    /// is stored only, with no ask and no write.
    pub(super) fn status_now(
        &self,
        state: &StoredState,
        force_online: bool,
        read_only: bool,
    ) -> LicenseStatus {
        let now = self.engine.effective_now(state);
        // A device refusal stands only for the device it was given for: who this is.
        let device: Option<Device> = (state.refused == Some(Refusal::Device)).then(|| {
            self.engine
                .device(self.engine.timings.machine_id, state.device_kind)
        });
        let view = device.as_ref().map(|device| DeviceView {
            own_claim: device.own_claim.as_deref(),
            can_tell: device.can_tell,
        });
        let key = present(&state.key);
        let refused = key.and_then(|_| standing_refusal(state, self.engine.major, view));
        if let Some(refused) = refused {
            if read_only {
                return invalid(refused.reason(), key);
            }
            return self.refused_status(state, refused, force_online);
        }
        if let (Some(_), Some(lease)) = (key, present(&state.lease)) {
            return self.license_status(state, lease, now, force_online, read_only);
        }
        if state.trial_start.is_some() {
            return self.trial_status_asking(state, now, force_online, read_only);
        }
        LicenseStatus::new(LicenseState::Unlicensed)
    }

    /// A refusal that stands for this build: answered as stored, and asked
    /// again once the wait after the last ask is over, or when forced.
    pub(super) fn refused_status(
        &self,
        state: &StoredState,
        refused: Refusal,
        force: bool,
    ) -> LicenseStatus {
        let stored = invalid(refused.reason(), state.key.as_deref());
        if !ask_due(state, self.engine.clock.now(), force) {
            return stored;
        }
        self.engine.try_renew(state, None).unwrap_or(stored)
    }

    pub(super) fn license_status(
        &self,
        state: &StoredState,
        lease: &str,
        now: i64,
        force_online: bool,
        read_only: bool,
    ) -> LicenseStatus {
        let check = self.engine.evaluate(lease, now, &Holder::of(state));
        // What the lease says offline, entitlements included: the answer
        // whenever the server gives no verdict (offline.rs `lease_status`).
        let offline = lease_status(&check, state.key.as_deref());
        let LeaseCheck::Ok { claims, .. } = check else {
            // Locally invalid (expired, tampered, upgrade, outdated, device):
            // give the server a chance to heal it before locking the customer
            // out, once the wait is over, or when forced.
            if !read_only && ask_due(state, self.engine.clock.now(), force_online) {
                if let Some(renewed) = self.engine.try_renew(state, None) {
                    return renewed;
                }
            }
            return offline;
        };
        if read_only {
            return offline;
        }
        let held = self.note_kept(state, claims.upgrade_until, now);
        if force_online {
            return self.engine.try_renew(&held, None).unwrap_or(offline);
        }
        // Asked, bounded in time, when the server should hear from this
        // device before the lease runs out (a newer major on an open lease, or
        // a lease nearing its end) and the wait allows (renewal.rs
        // `asks_on_valid_lease`). The valid lease is the answer when the ask
        // gets no verdict.
        let ask = ValidLeaseAsk {
            claims: &claims,
            state: &held,
            major: self.engine.major,
            now,
            clock_now: self.engine.clock.now(),
        };
        if !asks_on_valid_lease(&ask) {
            return offline;
        }
        self.engine
            .try_renew(&held, Some(self.engine.timings.status_check))
            .unwrap_or(offline)
    }

    /// The state with this run noted in `kept_major`, persisted when that changed.
    pub(super) fn note_kept(
        &self,
        state: &StoredState,
        upgrade_until: Option<i64>,
        now: i64,
    ) -> StoredState {
        let kept = kept_with(state.kept_major, upgrade_until, now, self.engine.major);
        if kept == state.kept_major {
            return state.clone();
        }
        // About this activation: not saved onto another one stored meanwhile.
        let basis = Basis {
            about: Some(About {
                activation_id: state.activation_id.clone(),
            }),
            ..Basis::default()
        };
        self.engine.save_best_effort(
            &Patch {
                kept_major: Some(kept),
                ..Patch::default()
            },
            None,
            &basis,
        );
        StoredState {
            kept_major: kept,
            ..state.clone()
        }
    }
}
