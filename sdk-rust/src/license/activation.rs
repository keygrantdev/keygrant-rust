//! Activating and deactivating this device: the asks, and what each answer saves.

use serde_json::{Map, Value};

use super::License;
use crate::device::activation_identity;
use crate::engine::{GuardSave, Holder};
use crate::error::{ActivateError, ActivateResult, Error};
use crate::offline::{
    held_elsewhere, same_key, standing_refusal, unkeyed, Asked, Basis, DeviceView, LeaseCheck,
};
use crate::pickup::{activation_patch, no_purchase, Activation};
use crate::state::{present, Patch, StoredState};
use crate::status::{DeactivateResult, DeviceKind, LicenseError, Refusal};
use crate::wire::{entered_key, error_of, map_server_error, parse_lease_body};

impl License {
    pub(super) fn activate_now(&self, key: &str) -> ActivateResult {
        let Some(entered) = entered_key(key) else {
            return Err(LicenseError::InvalidKey.into());
        };
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
        let body = self.activate_body(&entered, fingerprint, kind);
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
        let now = self
            .engine
            .server_now(&lease, &entered, &activation_id, GuardSave::Strict)
            .map_err(ActivateError::Storage)?;
        let holder = Holder {
            key: Some(&entered),
            activation_id: Some(&activation_id),
            device_kind: kind,
        };
        let LeaseCheck::Ok { claims, .. } = self.engine.evaluate(&lease, now, &holder) else {
            return Err(LicenseError::InvalidLease.into());
        };
        // As a bought key's claim saves one (pickup.rs `activation_patch`),
        // the purchase secret cleared too: else a later deactivate would let a
        // pending pickup activate the device again.
        let patch = activation_patch(&Activation {
            key: &entered,
            activation_id: &activation_id,
            lease: &lease,
            claims: &claims,
            now,
            major: self.engine.major,
            kind,
        });
        let basis = Basis {
            activation: Some(Asked {
                at: asked_at,
                activation_id: Some(activation_id),
                seen: stored.verdict_at,
            }),
            ..Basis::default()
        };
        self.engine
            .save(&patch, None, &basis)
            .map_err(ActivateError::Storage)
    }

    /// An activation's body: the key, the fingerprint it is made under (and
    /// its kind), the device name when the app gave one, and the major.
    pub(super) fn activate_body(
        &self,
        key: &str,
        fingerprint: String,
        kind: Option<DeviceKind>,
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
        Value::Object(body)
    }

    pub(super) fn deactivate_now(&self) -> Result<DeactivateResult, Error> {
        let state = self.engine.load(false).map_err(Error::Storage)?;
        // Nothing held is nothing to release: the seat is as free as it gets.
        // Nor is a seat another device holds this one's to release.
        let mut released = true;
        let held = match (present(&state.key), present(&state.activation_id)) {
            (Some(key), Some(activation_id)) if !self.seat_elsewhere(&state) => {
                Some((key, activation_id))
            }
            _ => None,
        };
        if let Some((key, activation_id)) = held {
            // Only a 200 is KeyGrant saying the seat is free. A 5xx, a 429 or a
            // proxy's page is an answer, but not that one, and no answer is
            // none; the local licence is dropped below either way.
            let body = serde_json::json!({ "key": key, "activationId": activation_id });
            released = self
                .engine
                .post("deactivate", body, None)
                .is_ok_and(|response| response.status == 200);
        }
        // Stamped, so that an answer to an older ask is not saved over it.
        let patch = unkeyed().then(no_purchase()).then(Patch {
            verdict_at: Some(Some(self.engine.effective_now(&state))),
            ..Patch::default()
        });
        self.engine
            .save(&patch, None, &Basis::default())
            .map_err(Error::Storage)?;
        Ok(DeactivateResult { released })
    }

    /// Whether the stored activation is another device's: a device refusal
    /// that stands for this one, or a lease held to a machine id this device
    /// does not have, expired or not (`held_elsewhere`). A state file copied
    /// or restored from another PC carries that PC's activation; releasing it
    /// would lock that PC out at its next check-in.
    pub(super) fn seat_elsewhere(&self, state: &StoredState) -> bool {
        let device = self
            .engine
            .device(self.engine.timings.machine_id, state.device_kind);
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
