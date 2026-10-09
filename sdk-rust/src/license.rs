//! The public API.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use serde_json::{Map, Value};

use crate::config::LicenseConfig;
use crate::device::{activation_identity, Device};
use crate::engine::{Engine, GuardSave, Holder};
use crate::error::{ActivateError, ActivateResult, ConfigError, Error};
use crate::offline::{
    ask_due, held_elsewhere, kept_with, lease_status, same_key, standing_refusal, unkeyed, About,
    Asked, Basis, DeviceView, LeaseCheck,
};
use crate::pickup::{activation_patch, no_purchase, Activation};
use crate::purchases::{Pickup, PickupAsk};
use crate::renewal::{asks_on_valid_lease, ValidLeaseAsk};
use crate::state::{present, Patch, StoredState};
use crate::status::DeviceKind;
use crate::status::{
    DeactivateResult, LicenseError, LicenseReason, LicenseState, LicenseStatus, Refusal,
};
use crate::url::{is_absolute_link, referenced_link};
use crate::wire::{
    entered_key, error_of, map_server_error, parse_lease_body, trim_to_length, MAX_DEVICE_NAME,
};

/// Client-side licensing for a desktop app.
///
/// Verifies signed JWT leases OFFLINE and only contacts the server to
/// activate, to renew a lease nearing its end or past it, or to tell it of a
/// newer major, so a transient outage never locks out a paying customer while
/// the lease is live (the grace window is the lease's lifetime). Trial start
/// is anti-reset (it only moves earlier) and a monotonic clock guard stops a
/// clock put back from resurrecting an expired lease.
///
/// Every method blocks (the network included, always bounded). Calls on one
/// `License` run one at a time; `License` is `Send + Sync`, so share one
/// (`Arc<License>`, or Tauri's managed state) and call it from any thread. In
/// a Tauri command, run it with `tauri::async_runtime::spawn_blocking`.
///
/// When `status()` goes online (each at most once per wait; `refresh()` always does):
///  - a valid lease: when this build is a newer major than the server last
///    heard, on an open lease, or when the lease is nearing its end; bounded
///    to 5 seconds, and the valid lease is the answer when that gets no verdict;
///  - a lease past its end or otherwise invalid here, and a stored refusal
///    that stands for this build: to give the server the chance to heal it;
///  - a trial whose lease does not verify here while its stored end is still
///    ahead (a lease signed before the product's key was rotated, say): the
///    trial is asked for again, bounded to 5 seconds;
///  - no key held and a purchase secret held (`purchase_url`): the key bought
///    with it is asked for (`claim`) for a day after the link was handed
///    out, once 5 minutes have passed since the last ask, and only under
///    this device's own fingerprint, bounded to 5 seconds.
///
/// And a running trial reports its launch, at most once a day, on a thread
/// of its own that no call waits on.
pub struct License {
    pub(crate) engine: Arc<Engine>,
    pub(crate) device_name: Option<String>,
    /// The server's trial when it could not be saved, kept for this process:
    /// merged into every state a status reads, and saved by the next call that can.
    pub(crate) unsaved_trial: Mutex<Option<(Patch, Basis)>>,
    /// The launch report in flight, if any (`launch.rs`), which no call waits on.
    pub(crate) launch_report: Mutex<Option<JoinHandle<()>>>,
}

impl fmt::Debug for License {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("License")
            .field("product", &self.engine.product)
            .field("major", &self.engine.major)
            .finish()
    }
}

/// What `status()` answers when the stored state cannot be read just now.
fn unreadable() -> LicenseStatus {
    LicenseStatus::new(LicenseState::Unknown).with_reason(LicenseReason::Storage)
}

fn stalled(mut status: LicenseStatus) -> LicenseStatus {
    status.stalled = true;
    status
}

fn invalid(reason: LicenseReason, key: Option<&str>) -> LicenseStatus {
    LicenseStatus::new(LicenseState::Invalid)
        .with_reason(reason)
        .with_key(key)
}

impl License {
    /// A licence for the product `config` names. Fails only on configuration
    /// a developer must fix (a product slug, the API base, the public key, or
    /// no home folder for the default storage); nothing is read or asked yet.
    pub fn new(config: LicenseConfig) -> Result<License, ConfigError> {
        let engine = Engine::new(&config)?;
        // CUT, not refused: the activate endpoint bounds every string it
        // takes, and a device name is a label in a device list.
        let device_name = config
            .device_name
            .as_deref()
            .map(|name| trim_to_length(name, MAX_DEVICE_NAME));
        Ok(License {
            engine: Arc::new(engine),
            device_name,
            unsaved_trial: Mutex::new(None),
            launch_report: Mutex::new(None),
        })
    }

    /// Activate a licence key on this device.
    ///
    /// The key is normalised first (trimmed, upper case): that is the key
    /// sent and stored. Behind a call that has not finished, nothing may be
    /// written: the activation answers `Network`, to retry. A state that
    /// cannot be read or saved is [`ActivateError::Storage`].
    pub fn activate(&self, key: &str) -> ActivateResult {
        self.engine.serial(|read_only| {
            if read_only {
                return Err(ActivateError::License(LicenseError::Network));
            }
            self.activate_now(key)
        })
    }

    /// `payment_link_url` with this device's ACTIVATION id added as
    /// `client_reference_id`, or `None` when the device holds no activated
    /// licence. Send a customer there to buy an upgrade for the key they hold:
    /// a checkout for an upgrade offer finds their key through the activation
    /// and extends it rather than minting a second one. The activation id,
    /// never the licence key: a URL lands in browser history and logs, and the
    /// activation id grants nothing on its own. Read-only: runs beside any call.
    pub fn upgrade_url(&self, payment_link_url: &str) -> Result<Option<String>, Error> {
        let state = self.engine.load(true).map_err(Error::Storage)?;
        let (Some(_), Some(activation_id)) = (present(&state.key), present(&state.activation_id))
        else {
            return Ok(None);
        };
        referenced_link(payment_link_url, activation_id)
            .map(Some)
            .ok_or(Error::InvalidUrl)
    }

    /// `payment_link_url` with a PURCHASE REFERENCE as its
    /// `client_reference_id`, or `None` while the device holds a key (any
    /// key: use [`upgrade_url`](Self::upgrade_url), or the plain link). Send a
    /// customer with no key there to buy: the checkout keeps the reference on
    /// the key it mints, and once they are back the SDK asks for that key (at
    /// once on `refresh()` or `start_trial()`, and from `status()` for a day
    /// after the link), activates this device on it and answers `Licensed`,
    /// with nothing typed.
    ///
    /// The reference is a hash of a one-time secret (256 bits from the
    /// platform's CSPRNG, never the fingerprint) that stays on the device and
    /// is sent only in that ask, so a link that leaks claims nothing. The
    /// secret is made the first time, kept, and its link handed out again by
    /// every later call; it is dropped 30 days after the last one, or once a
    /// key bought with it is picked up. Fails with [`Error::InvalidUrl`] when
    /// the link is not an absolute `http`/`https` URL (before anything is
    /// read), [`Error::Storage`] when the state cannot be read or the secret
    /// cannot be saved (a link whose secret is not kept could never be picked
    /// up), and [`Error::Random`] when no secret can be made. Runs one call
    /// at a time like the calls that write: behind a call that has not
    /// finished, a held secret's link is answered as it is, and with none
    /// held it fails with [`Error::Busy`].
    pub fn purchase_url(&self, payment_link_url: &str) -> Result<Option<String>, Error> {
        if !is_absolute_link(payment_link_url) {
            return Err(Error::InvalidUrl);
        }
        self.engine
            .serial(|read_only| self.purchase_url_now(payment_link_url, read_only))
    }

    /// Release this device's activation and drop the local licence, saying
    /// whether the seat is now free ([`DeactivateResult::released`]): true
    /// when KeyGrant answered the release with a 200, or when there was no
    /// activation to release; false when it could not be asked or answered
    /// anything else (a 5xx, a 429, a proxy's page), the local licence dropped
    /// all the same.
    ///
    /// An activation that is another device's (a state file copied or
    /// restored from another PC) is never released: it is dropped here, and
    /// the answer is `released: true`, as this device held no seat. Released,
    /// it would lock the other PC out at its next check-in.
    ///
    /// Fails, releasing nothing, when a call before it has not finished
    /// ([`Error::Busy`]) or the state cannot be read ([`Error::Storage`]).
    /// When the state cannot be saved ([`Error::Storage`] too), the release
    /// has already been asked for, and the seat may already be free while
    /// this device still holds the key: try again. KeyGrant's release is
    /// idempotent, so the retry answers `released: true` and drops the key.
    pub fn deactivate(&self) -> Result<DeactivateResult, Error> {
        self.engine.serial(|read_only| {
            if read_only {
                Err(Error::Busy)
            } else {
                self.deactivate_now()
            }
        })
    }

    /// Re-check licensing: renews the lease online, whatever the waits say.
    pub fn refresh(&self) -> LicenseStatus {
        self.status(true)
    }

    /// The current licensing status. Offline-first; only calls out when
    /// forced or needed. Never fails for storage: a state that cannot be read
    /// just now is `Unknown` (reason `Storage`), with no ask and no write, and
    /// a save that fails leaves the answer standing.
    pub fn status(&self, force_online: bool) -> LicenseStatus {
        self.engine.serial(|read_only| {
            if !read_only {
                self.save_unsaved_trial();
            }
            let status = match self.load_for_status(read_only) {
                Some(state) => self.status_now(&state, force_online, read_only),
                None => unreadable(),
            };
            if read_only {
                stalled(status)
            } else {
                status
            }
        })
    }

    /// Begin (or re-sync) a trial for this device. Like `status()` for
    /// storage: `Unknown` when the state cannot be read, and the server's
    /// trial when it cannot be saved.
    pub fn start_trial(&self) -> LicenseStatus {
        // It may make two requests (a claim bounded by the status check, then
        // the trial ask): the calls behind it wait that much longer.
        let extra = self.engine.timings.status_check;
        self.engine.serial_longer(extra, |read_only| {
            let Some(state) = self.load_for_status(read_only) else {
                return if read_only {
                    stalled(unreadable())
                } else {
                    unreadable()
                };
            };
            if !read_only {
                // The device read once, given an activation's time, for both asks.
                let device = self
                    .engine
                    .device(self.engine.timings.activate_machine_id, state.device_kind);
                // A key bought through `purchase_url` is asked for first;
                // bought, it is the answer, and no trial is asked.
                let ask = PickupAsk {
                    start: true,
                    force: true,
                    device: Some(&device),
                };
                return match self.pick_up(&state, ask) {
                    Pickup::Bought(status) => status,
                    Pickup::State(state) => self.start_trial_now(&state, &device),
                };
            }
            stalled(self.trial_status(&state, self.engine.effective_now(&state)))
        })
    }

    // --- internals -------------------------------------------------------

    fn activate_now(&self, key: &str) -> ActivateResult {
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
    fn activate_body(&self, key: &str, fingerprint: String, kind: Option<DeviceKind>) -> Value {
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
        Value::Object(body)
    }

    fn deactivate_now(&self) -> Result<DeactivateResult, Error> {
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
    fn seat_elsewhere(&self, state: &StoredState) -> bool {
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

    /// The status, asking the server when it should. `read_only`: from what
    /// is stored only, with no ask and no write.
    fn status_now(
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
        // No licence held: a key bought through `purchase_url` is picked up
        // first (purchases.rs `pick_up`), never read-only.
        let picked = if read_only {
            Pickup::State(Box::new(state.clone()))
        } else {
            let ask = PickupAsk {
                start: false,
                force: force_online,
                device: None,
            };
            self.pick_up(state, ask)
        };
        let state = match picked {
            Pickup::Bought(status) => return status,
            Pickup::State(state) => state,
        };
        if state.trial_start.is_some() {
            return self.trial_status_asking(&state, now, force_online, read_only);
        }
        LicenseStatus::new(LicenseState::Unlicensed)
    }

    /// A refusal that stands for this build: answered as stored, and asked
    /// again once the wait after the last ask is over, or when forced.
    fn refused_status(&self, state: &StoredState, refused: Refusal, force: bool) -> LicenseStatus {
        let stored = invalid(refused.reason(), state.key.as_deref());
        if !ask_due(state, self.engine.clock.now(), force) {
            return stored;
        }
        self.engine.try_renew(state, None).unwrap_or(stored)
    }

    fn license_status(
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
    fn note_kept(&self, state: &StoredState, upgrade_until: Option<i64>, now: i64) -> StoredState {
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
