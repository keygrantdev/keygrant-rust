//! # KeyGrant licensing for Tauri and native Rust desktop apps
//!
//! A licence is proven to the app by a signed JWT lease (EdDSA / Ed25519)
//! that the app verifies OFFLINE against the product's public key. The app
//! only phones home to activate, to renew a lease near or past its end, to
//! report a newer major version, or to report a running trial's launch (at
//! most once a day).
//!
//! **Licensing failure fails in favour of the paying customer.** An outage, no
//! network, a 5xx, a 429, an ambiguous 4xx or an unusable body never locks
//! out a valid lease; only the server's explicit verdicts do.
//!
//! This crate is a port of the Electron SDK (`@keygrant/electron`), the
//! reference, and behaves the same: the same wire protocol, the same state
//! file (format and default location), the same fingerprint for the same
//! machine, and the same answers to the shared conformance vectors.
//!
//! ```no_run
//! use keygrant::{License, LicenseConfig};
//!
//! const PUBLIC_JWK: &str = r#"{"kty":"OKP","crv":"Ed25519","x":"Ohg4h8SrYQ0m1su_zVacJLLwf_6rNsvOtV5oWY6CzJs"}"#;
//!
//! let license = License::new(LicenseConfig::new("sluice", "https://api.keygrant.dev", PUBLIC_JWK).major(1))?;
//! let status = license.status(false); // offline-first
//! if !status.permits_use() {
//!     // show the activation screen; then:
//!     license.activate("SLUICE-AAAA-BBBB-CCCC-DDDD")?;
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod adapters;
mod bounded;
mod config;
#[doc(hidden)]
pub mod device;
mod engine;
mod error;
mod fingerprint;
mod http;
#[doc(hidden)]
pub mod launch;
pub mod lease;
mod license;
pub mod machine_id;
#[doc(hidden)]
pub mod offline;
mod registry;
#[doc(hidden)]
pub mod renewal;
mod serial;
mod state;
mod status;
pub mod storage;
mod trials;
#[doc(hidden)]
pub mod url;
#[doc(hidden)]
pub mod wire;

pub use adapters::{
    AdapterError, Adapters, Clock, DeviceIdentity, Fingerprinter, HttpAdapter, HttpResponse,
    NoopRegistry, RegistryStash, StorageAdapter, SystemClock,
};
#[doc(hidden)]
pub use config::Timings;
pub use config::{LicenseConfig, LOCK_GRACE};
pub use error::{ActivateError, ActivateResult, ConfigError, Error, MissingEntitlement};
pub use fingerprint::{
    hash_fingerprint, host_fingerprint, host_name, host_names, hostname_fingerprint, identity_of,
    machine_fingerprint, DefaultFingerprinter, HostNames, UNKNOWN_HOST,
};
pub use http::{UreqHttp, DEFAULT_HTTP_TIMEOUT};
pub use lease::{grants, Entitlement, KeySet};
pub use license::License;
#[cfg(windows)]
pub use registry::WindowsRegistryStash;
#[doc(hidden)]
pub use state::Patch;
pub use state::StoredState;
pub use status::{
    DeactivateResult, DeviceKind, LicenseError, LicenseReason, LicenseState, LicenseStatus, Refusal,
};
pub use storage::{default_storage_path, FileStorage};
