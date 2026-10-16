//! Activating and deactivating this device: the asks, and what each answer saves.

use serde_json::{Map, Value};

use super::{ActivateOptions, License};
use crate::device::{activation_identity, Device};
use crate::engine::{GuardSave, Holder, View};
use crate::error::{ActivateError, ActivateResult, Error};
use crate::journal::JournalOp;
use crate::meters::spends_reported;
use crate::offline::{
    activation_patch, held_elsewhere, same_key, standing_refusal, unkeyed, Activation, Asked,
    Basis, DeviceView, LeaseCheck,
};
use crate::state::{present, Patch, StoredState};
use crate::status::{DeactivateResult, DeviceKind, LicenseError, Refusal};
use crate::wire::{entered_key, error_of, map_server_error, parse_lease_body, typed_for, Typed};

impl License {
    pub(super) fn activate_now(&self, key: &str, options: &ActivateOptions) -> ActivateResult {
        let Some(entered) = entered_key(key) else {
            return Err(LicenseError::InvalidKey.into());
        };
        // What a person typed for a user-locked key: one that can be no
        // address, or no code, is refused here, before anything is asked of
        // the server or its wrong tries are spent.
        let typed = typed_for(options.email.as_deref(), options.code.as_deref())?;
        // Install memory protects the seat this install holds: re-activating
        // that key keeps to it; a first activation, or another key's, starts afresh.
        let stored = self.engine.load(false).map_err(ActivateError::Storage)?;
        let same_seat = stored
            .key
            .as_deref()
            .is_some_and(|stored| same_key(stored, &entered));
        let install_kind = if same_seat {
            self.engine.install_kind(stored.device_kind)
        } else {
            None
        };
        let device = self
            .engine
            .device(self.engine.timings.activate_machine_id, install_kind);
        let Some((fingerprint, kind)) = activation_identity(&device, install_kind) else {
            return Err(LicenseError::Network.into());
        };
        // Stamped with when it was asked for: an older ask's answer, from
        // another process, is not saved over it.
        let asked_at = self.engine.effective_now(&stored);
        let body = self.activate_body(&entered, fingerprint, kind, &typed);
        let Ok(response) = self.engine.post("activate", body, None) else {
            return Err(LicenseError::Network.into());
        };
        if response.status != 200 {
            return Err(map_server_error(error_of(response.json.as_ref())).into());
        }
        let (lease, activation_id) = parse_lease_body(response.json.as_ref());
        let usable = |text: Option<String>| text.filter(|text| !text.is_empty());
        let (Some(lease), Some(activation_id)) = (usable(lease), usable(activation_id)) else {
            return Err(LicenseError::Network.into());
        };
        let asked = Asked {
            at: asked_at,
            activation_id: Some(activation_id),
            seen: stored.verdict_at,
        };
        self.take_seat(&entered, &lease, kind, asked)
    }

    /// The seat the server just gave: its lease judged here as of the server's
    /// time, and saved with the key and the activation as an activation's
    /// answer (`asked` names the activation). Activating another seat drops
    /// the journal of the old one, unreported; the same seat keeps it.
    fn take_seat(
        &self,
        entered: &str,
        lease: &str,
        kind: Option<DeviceKind>,
        asked: Asked,
    ) -> ActivateResult {
        let activation_id = asked.activation_id.clone().unwrap_or_default();
        let now = self
            .engine
            .server_now(lease, entered, &activation_id, GuardSave::Strict)
            .map_err(ActivateError::Storage)?;
        let holder = Holder {
            key: Some(entered),
            activation_id: Some(&activation_id),
            device_kind: kind,
        };
        let LeaseCheck::Ok { claims, .. } = self.engine.evaluate(lease, now, &holder) else {
            return Err(LicenseError::InvalidLease.into());
        };
        let patch = activation_patch(&Activation {
            key: entered,
            activation_id: &activation_id,
            lease,
            claims: &claims,
            now,
            major: self.engine.major,
            kind,
        });
        let seat = JournalOp::Activated {
            about: activation_id,
        };
        let basis = Basis {
            activation: Some(asked),
            ..Basis::default()
        };
        self.engine
            .save_with(&patch, None, &basis, &[seat])
            .map(|_| ())
            .map_err(ActivateError::Storage)
    }

    /// An activation's body: the key, the fingerprint it is made under (and
    /// its kind), the device name when the app gave one, the major, and what
    /// a person typed for a user-locked key, each only when there is one.
    pub(super) fn activate_body(
        &self,
        key: &str,
        fingerprint: String,
        kind: Option<DeviceKind>,
        typed: &Typed,
    ) -> Value {
        let mut body = Map::new();
        body.insert("key".into(), Value::from(key));
        body.insert("fingerprint".into(), Value::from(fingerprint));
        if let Some(kind) = kind {
            body.insert("fingerprintKind".into(), Value::from(kind.as_str()));
        }
        if let Some(name) = &self.device_name {
            body.insert("name".into(), Value::from(name.clone()));
        }
        body.insert("major".into(), Value::from(self.engine.major));
        body.insert("kids".into(), self.engine.kids());
        if let Some(email) = &typed.email {
            body.insert("email".into(), Value::from(email.clone()));
        }
        if let Some(code) = &typed.code {
            body.insert("code".into(), Value::from(code.clone()));
        }
        Value::Object(body)
    }

    pub(super) fn deactivate_now(&self) -> Result<DeactivateResult, Error> {
        // What this process could not save lands first, so the release reports
        // it: the journal goes with the seat.
        self.engine.save_unsaved_journal();
        let view = self.engine.view(false).map_err(Error::Storage)?;
        let state: &StoredState = &view;
        // Nothing held is nothing to release: the seat is as free as it gets.
        // Nor is a seat another device holds this one's to release.
        let mut released = true;
        if let (Some(key), Some(activation_id)) =
            (present(&state.key), present(&state.activation_id))
        {
            let device = self
                .engine
                .device(self.engine.timings.machine_id, state.device_kind);
            if !self.seat_elsewhere(state, &device) {
                released = self.release(&view, key, activation_id, &device);
            }
        }
        // Stamped, so that an answer to an older ask is not saved over it. The
        // journal goes with the rest of the licence: what the release carried is
        // all the server will hear of it.
        let patch = unkeyed().then(Patch {
            verdict_at: Some(Some(self.engine.effective_now(state))),
            ..Patch::default()
        });
        self.engine
            .save_with(&patch, None, &Basis::default(), &[JournalOp::Deactivated])
            .map_err(Error::Storage)?;
        Ok(DeactivateResult { released })
    }

    /// The release request, carrying the journal of unsynced spends and, with
    /// it, the device's fingerprints as a validate sends them (so the server
    /// holds the reports to the seat's device; none when it cannot tell). Only a
    /// 200 is KeyGrant saying the seat is free: a 5xx, a 429 or a proxy's page
    /// is an answer, but not that one, and no answer is none; the local licence
    /// is dropped by the caller either way.
    fn release(&self, view: &View, key: &str, activation_id: &str, device: &Device) -> bool {
        let mut body = Map::new();
        body.insert("key".into(), Value::from(key));
        body.insert("activationId".into(), Value::from(activation_id));
        if let Some(reports) = spends_reported(&self.engine.reportable(view)) {
            body.insert("spends".into(), reports);
            if device.can_tell {
                body.insert(
                    "fingerprints".into(),
                    Value::from(device.fingerprints.clone()),
                );
            }
        }
        self.engine
            .post("deactivate", Value::Object(body), None)
            .is_ok_and(|response| response.status == 200)
    }

    /// Whether the stored activation is another device's: a device refusal
    /// that stands for this one, or a lease held to a machine id this device
    /// does not have, expired or not (`held_elsewhere`). A state file copied
    /// or restored from another PC carries that PC's activation; releasing it
    /// would lock that PC out at its next check-in.
    pub(super) fn seat_elsewhere(&self, state: &StoredState, device: &Device) -> bool {
        let view = DeviceView {
            own_claim: device.own_claim.as_deref(),
            can_tell: device.can_tell,
        };
        if standing_refusal(state, self.engine.major, Some(view)) == Some(Refusal::Device) {
            return true;
        }
        present(&state.lease).is_some_and(|lease| {
            let verified = self.engine.verify(lease, self.engine.effective_now(state));
            held_elsewhere(&verified, &device.claims, device.can_tell)
        })
    }
}
