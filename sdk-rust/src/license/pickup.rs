//! Key pickup under `License` (`pickup.rs` holds the pure pieces): a code
//! mailed to the address a key was sold to (`pick_up`), that address's keys of
//! the product for the code (`pick_up_with_code`), and a restore that confirms
//! and activates the key that fits this build (`restore_by_email`), so an App
//! Store build licenses itself with no key typed.
//!
//! The two `pick_up` calls store nothing and run beside any call; a restore
//! runs in the one-at-a-time order as `activate` does, and what it activates is
//! stored as any activation is. Nothing here takes a licence away.

use std::sync::{Mutex, MutexGuard};

use serde_json::{json, Map, Value};

use super::{ActivateOptions, License};
use crate::error::ActivateError;
use crate::pickup::{
    chosen_key, picked_keys_of, pickup_ask, pickup_refusal, KeptKeys, PickedKey, PickupAsk,
    PickupError, PickupRefusal, RestoreError,
};
use crate::status::LicenseError;

/// A lock that a panicking holder does not poison for good.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl License {
    /// Ask KeyGrant to mail a code to `email`, the address a key was sold to.
    /// For a build that may not take a typed key (an App Store build): the
    /// customer types the code, then
    /// [`pick_up_with_code`](License::pick_up_with_code) hands back that
    /// address's keys, or [`restore_by_email`](License::restore_by_email)
    /// activates the one that fits. `Ok` means the code is on its way when the
    /// address holds a key: the answer is the same when it holds none.
    ///
    /// The product's Key pickup must be on ([`PickupError::PickupDisabled`]
    /// otherwise). A blank address, or one longer than any, is
    /// [`PickupError::InvalidEmail`], asking nothing. Stores nothing, runs beside
    /// any call and never panics for an outcome; no answer at all is
    /// [`PickupError::Network`].
    pub fn pick_up(&self, email: &str) -> Result<(), PickupRefusal> {
        let ask = pickup_ask(email, None)?;
        let body = json!({ "email": ask.email });
        match self.engine.post("pickup", body, None) {
            Ok(response) if response.status == 200 => Ok(()),
            Ok(response) => Err(pickup_refusal(response.json.as_ref())),
            Err(()) => Err(PickupRefusal::of(PickupError::Network)),
        }
    }

    /// The keys of the product that `email` was sold, for the `code` mailed to
    /// it: each with what the app may show (its [`model_name`](PickedKey::model_name))
    /// and `held` when this device already holds a seat on it (the fingerprints
    /// a validate sends go with the code, when the device can tell who it is).
    /// An address with no key answers an empty list. Activate one with
    /// `ActivateOptions::new().email(address)`, adding `.code(c.clone())` when
    /// `picked.activation_code` is `Some(c)`, passed to `activate_with(&picked.key, ...)`, or let
    /// [`restore_by_email`](License::restore_by_email) choose.
    ///
    /// The address is judged first ([`PickupError::InvalidEmail`]), then the
    /// code: the ends trimmed, the spaces dropped, exactly six ASCII digits or
    /// [`PickupError::WrongCode`], at once and asking nothing, so the address
    /// loses none of its wrong tries. The first confirm spends the code, so the
    /// keys it answered are kept in memory ([`PICKUP_KEYS_KEEP_MS`](crate::pickup::PICKUP_KEYS_KEEP_MS),
    /// 10 minutes) by the address and code as sent: the same pair answers from them
    /// and asks nothing. Stores nothing (not the address, the code, the keys or a
    /// refusal), runs beside any call, and a 200 that holds no list of keys is
    /// [`PickupError::Network`].
    pub fn pick_up_with_code(
        &self,
        email: &str,
        code: &str,
    ) -> Result<Vec<PickedKey>, PickupRefusal> {
        let ask = pickup_ask(email, Some(code))?;
        if let Some(kept) = self.kept_keys(&ask) {
            return Ok(kept);
        }
        // Read-only: this runs outside the one-at-a-time order, beside any call.
        let stored = self.engine.load(true).unwrap_or_default();
        let device = self.engine.device(
            self.engine.timings.machine_id,
            self.engine.install_kind(stored.device_kind),
        );
        let mut body = Map::new();
        body.insert("email".into(), Value::from(ask.email.clone()));
        body.insert(
            "code".into(),
            Value::from(ask.code.clone().unwrap_or_default()),
        );
        if device.can_tell {
            body.insert("fingerprints".into(), json!(device.fingerprints));
        }
        let Ok(response) = self
            .engine
            .post("pickup/confirm", Value::Object(body), None)
        else {
            return Err(PickupRefusal::of(PickupError::Network));
        };
        if response.status != 200 {
            return Err(pickup_refusal(response.json.as_ref()));
        }
        let keys = picked_keys_of(response.json.as_ref())
            .ok_or_else(|| PickupRefusal::of(PickupError::Network))?;
        *lock(&self.pickup_keys) = Some(KeptKeys::new(&ask, &keys, self.engine.clock.now()));
        Ok(keys)
    }

    /// The keys a confirm with this address and code answered, while they are kept.
    fn kept_keys(&self, ask: &PickupAsk) -> Option<Vec<PickedKey>> {
        let kept = lock(&self.pickup_keys);
        kept.as_ref()?.for_ask(ask, self.engine.clock.now())
    }

    /// Confirm a pickup's code and activate, on this device, the key of the
    /// address that fits this build: the one this device already holds a seat
    /// on, else one with a free seat, else the first. It activates as
    /// `activate_with(key, email, code)` would, with the address as the pickup
    /// sent it and, for a code-locked key, the code the pickup handed back, so
    /// nothing more is asked of the customer. The key is stored as an
    /// activation's is and never handed to the app; nothing else is stored.
    ///
    /// Answers the confirm's errors ([`RestoreError::Pickup`]); then
    /// [`RestoreError::NoKey`] for an address with no key, and
    /// `Upgrade` or `Outdated` ([`RestoreError::License`]) when none of its keys
    /// covers this build (`Upgrade`: they are for an older major, buy an upgrade;
    /// `Outdated`: update the app), activating nothing; then the activation's
    /// own errors, as they come. Behind a call that has not finished it
    /// answers `Network`, to retry, as `activate` does.
    pub fn restore_by_email(&self, email: &str, code: &str) -> Result<(), RestoreError> {
        self.engine.serial(|read_only| {
            if read_only {
                return Err(RestoreError::License(LicenseError::Network));
            }
            self.restore_now(email, code)
        })
    }

    /// The confirm, the choice, and the activation of the key chosen.
    fn restore_now(&self, email: &str, code: &str) -> Result<(), RestoreError> {
        let keys = self.pick_up_with_code(email, code)?;
        let index = chosen_key(&keys, self.engine.major, self.engine.clock.now())
            .map_err(RestoreError::from)?;
        let picked = &keys[index];
        // The address as the pickup sent it (the confirm just accepted it).
        let address = pickup_ask(email, None).map_or_else(|_| email.to_owned(), |ask| ask.email);
        let mut options = ActivateOptions::new().email(address);
        if let Some(code) = &picked.activation_code {
            options = options.code(code.clone());
        }
        self.activate_now(&picked.key, &options)
            .map_err(|error: ActivateError| RestoreError::from(error))?;
        // Activated: the keys (and a lock's single-use code among them) are done with.
        *lock(&self.pickup_keys) = None;
        Ok(())
    }
}
