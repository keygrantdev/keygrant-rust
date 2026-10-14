//! How a `License` is configured.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::adapters::{Adapters, Clock, Fingerprinter, HttpAdapter, RegistryStash, StorageAdapter};
use crate::device::IDENTIFY_GRACE;
use crate::http::DEFAULT_HTTP_TIMEOUT;
use crate::machine_id::{ACTIVATE_MACHINE_ID_TIMEOUT, MACHINE_ID_TIMEOUT};
use crate::wire::STATUS_CHECK_TIMEOUT;

/// How long past its HTTP bound a call may hold the next one waiting: room
/// for the storage reads and writes around its one request.
pub const LOCK_GRACE: Duration = Duration::from_millis(15_000);

/// The SDK's fixed time bounds. Not configuration: their defaults are the
/// reference's, and only tests change them (to run in milliseconds).
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timings {
    /// How long past its HTTP bound a call may hold the next one waiting.
    pub lock_grace: Duration,
    /// How long a check `status()` makes on a valid lease may take.
    pub status_check: Duration,
    /// How long `status()` may spend reading the machine id.
    pub machine_id: Duration,
    /// How long `activate()` and `start_trial()` may spend reading it.
    pub activate_machine_id: Duration,
    /// How long past the time it is given a fingerprinter's `identify` may
    /// take before the device is "cannot tell".
    pub identify_grace: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            lock_grace: LOCK_GRACE,
            status_check: STATUS_CHECK_TIMEOUT,
            machine_id: MACHINE_ID_TIMEOUT,
            activate_machine_id: ACTIVATE_MACHINE_ID_TIMEOUT,
            identify_grace: IDENTIFY_GRACE,
        }
    }
}

/// How a [`License`](crate::License) is configured.
///
/// ```
/// use keygrant::LicenseConfig;
///
/// let config = LicenseConfig::new(
///     "sluice",
///     "https://api.keygrant.dev",
///     r#"{"kty":"OKP","crv":"Ed25519","x":"Ohg4h8SrYQ0m1su_zVacJLLwf_6rNsvOtV5oWY6CzJs"}"#,
/// )
/// .major(2)
/// .device_name("Rae's MacBook Pro");
/// ```
#[derive(Clone)]
#[non_exhaustive]
pub struct LicenseConfig {
    /// The product slug, matching the KeyGrant product.
    pub product: String,
    /// The KeyGrant API base, e.g. `https://api.keygrant.dev`.
    pub api_base_url: String,
    /// The product's public keys, embedded at build time for offline
    /// verification: its key set (`{"keys":[...]}`, the `keys` of
    /// `GET https://api.keygrant.dev/v1/products/<product>/pubkey`: the key
    /// that signs its leases, and the next one once the product has one
    /// staged), a JSON list of JWKs, or one key, a set of one: its JWK
    /// (`{"kty":"OKP","crv":"Ed25519","x":"..."}`) or the JWK's `x` alone
    /// ([`KeySet::parse`](crate::lease::KeySet::parse)).
    pub public_jwk: String,
    /// The running app's major version (default 1). Set it: the server binds
    /// an unbound key to the major its first activation runs. Read as the
    /// server reads a version: 0 (a 0.x app) runs as major 1.
    pub major: u32,
    /// A human device name reported to the server (cut to 120 UTF-16 units).
    pub device_name: Option<String>,
    /// Whether a licence is held to the device it was activated on (default
    /// true). False for a product deployed on non-persistent, pooled VDI
    /// with roaming profiles, where every session may be another machine:
    /// the SDK then reports no kind for its fingerprint, so its seats are
    /// never held, judges no lease by its device, and sends the server no
    /// fingerprints to check. The device limit still counts seats.
    pub device_hold: bool,
    /// How long a request may take before the SDK abandons it, for every call
    /// that names no shorter bound of its own (default 15 s). Kept by the SDK
    /// whatever the HTTP adapter does with it.
    pub http_timeout: Duration,
    /// The ids of licence files this build refuses (a file's `fid`): how an
    /// app update stops an offline licence file that leaked, since nothing
    /// else can. Matched exactly. A stored file listed here reads as
    /// `Invalid` / `RevokedFile`; importing one is refused.
    pub revoked_license_files: Vec<String>,
    /// Adapter overrides; any left `None` uses the default.
    pub adapters: Adapters,
    #[doc(hidden)]
    pub timings: Timings,
}

impl fmt::Debug for LicenseConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LicenseConfig")
            .field("product", &self.product)
            .field("api_base_url", &self.api_base_url)
            .field("major", &self.major)
            .field("device_name", &self.device_name)
            .field("device_hold", &self.device_hold)
            .field("http_timeout", &self.http_timeout)
            .field("revoked_license_files", &self.revoked_license_files)
            .field("adapters", &self.adapters)
            .finish_non_exhaustive()
    }
}

impl LicenseConfig {
    /// A configuration with the defaults: major 1, held to the device, a
    /// 15-second HTTP bound and the default adapters.
    pub fn new(
        product: impl Into<String>,
        api_base_url: impl Into<String>,
        public_jwk: impl Into<String>,
    ) -> Self {
        LicenseConfig {
            product: product.into(),
            api_base_url: api_base_url.into(),
            public_jwk: public_jwk.into(),
            major: 1,
            device_name: None,
            device_hold: true,
            http_timeout: DEFAULT_HTTP_TIMEOUT,
            revoked_license_files: Vec::new(),
            adapters: Adapters::default(),
            timings: Timings::default(),
        }
    }

    /// The running app's major version. Read as the server reads a version:
    /// 0 (a 0.x app) runs as major 1.
    pub fn major(mut self, major: u32) -> Self {
        self.major = major;
        self
    }

    /// The device name reported to the server.
    pub fn device_name(mut self, name: impl Into<String>) -> Self {
        self.device_name = Some(name.into());
        self
    }

    /// Whether licences are held to their device (see [`device_hold`](Self::device_hold)).
    pub fn device_hold(mut self, hold: bool) -> Self {
        self.device_hold = hold;
        self
    }

    /// The HTTP bound.
    pub fn http_timeout(mut self, timeout: Duration) -> Self {
        self.http_timeout = timeout;
        self
    }

    /// The ids of licence files this build refuses (see
    /// [`revoked_license_files`](Self::revoked_license_files)).
    pub fn revoked_license_files<I, S>(mut self, ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.revoked_license_files = ids.into_iter().map(Into::into).collect();
        self
    }

    /// Where the state is kept.
    pub fn storage(mut self, storage: Arc<dyn StorageAdapter>) -> Self {
        self.adapters.storage = Some(storage);
        self
    }

    /// The uninstall-surviving trial stash.
    pub fn registry(mut self, registry: Arc<dyn RegistryStash>) -> Self {
        self.adapters.registry = Some(registry);
        self
    }

    /// The clock.
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.adapters.clock = Some(clock);
        self
    }

    /// The device fingerprint.
    pub fn fingerprinter(mut self, fingerprinter: Arc<dyn Fingerprinter>) -> Self {
        self.adapters.fingerprint = Some(fingerprinter);
        self
    }

    /// The HTTP client.
    pub fn http(mut self, http: Arc<dyn HttpAdapter>) -> Self {
        self.adapters.http = Some(http);
        self
    }

    /// All adapter overrides at once.
    pub fn adapters(mut self, adapters: Adapters) -> Self {
        self.adapters = adapters;
        self
    }

    #[doc(hidden)]
    pub fn timings(mut self, timings: Timings) -> Self {
        self.timings = timings;
        self
    }
}
