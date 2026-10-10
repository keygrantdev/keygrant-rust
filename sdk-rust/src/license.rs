//! The public API.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

mod activation;
mod status_flow;

use crate::config::LicenseConfig;
use crate::engine::Engine;
use crate::error::{ActivateError, ActivateResult, ConfigError, Error, MissingEntitlement};
use crate::lease::grants;
use crate::offline::Basis;
use crate::state::{present, Patch};
use crate::status::{DeactivateResult, LicenseError, LicenseReason, LicenseState, LicenseStatus};
use crate::url::referenced_link;
use crate::wire::{trim_to_length, MAX_DEVICE_NAME};

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
///    trial is asked for again, bounded to 5 seconds.
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

    /// Whether the licence this device holds grants the entitlement `code`: a
    /// flag that is on, a number that is not zero, or a text that is not
    /// empty ([`grants`](crate::grants)). Offline, from the stored lease only:
    /// the entitlements of the status `status()` gives from what is stored,
    /// with no ask and no write, so a licence that does not license this
    /// build, and none at all, grant nothing. A running trial grants what its
    /// trial lease does. A state that cannot be read grants nothing just now.
    pub fn has(&self, code: &str) -> bool {
        let status = match self.load_for_status(true) {
            Some(state) => self.status_now(&state, false, true),
            None => unreadable(),
        };
        grants(&status.entitlements, code)
    }

    /// `Ok` when [`has`](License::has) is true; otherwise the
    /// [`MissingEntitlement`] naming `code`: for work that may run only when
    /// the licence grants that entitlement.
    pub fn require(&self, code: &str) -> Result<(), MissingEntitlement> {
        if self.has(code) {
            Ok(())
        } else {
            Err(MissingEntitlement {
                entitlement: code.to_owned(),
            })
        }
    }

    /// Begin (or re-sync) a trial for this device. Like `status()` for
    /// storage: `Unknown` when the state cannot be read, and the server's
    /// trial when it cannot be saved.
    pub fn start_trial(&self) -> LicenseStatus {
        self.engine.serial(|read_only| {
            let Some(state) = self.load_for_status(read_only) else {
                return if read_only {
                    stalled(unreadable())
                } else {
                    unreadable()
                };
            };
            if !read_only {
                // The device read given an activation's time.
                let device = self
                    .engine
                    .device(self.engine.timings.activate_machine_id, state.device_kind);
                return self.start_trial_now(&state, &device);
            }
            stalled(self.trial_status(&state, self.engine.effective_now(&state)))
        })
    }

    // --- internals -------------------------------------------------------
}
