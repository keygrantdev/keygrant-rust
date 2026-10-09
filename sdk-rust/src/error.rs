//! The errors the public API returns.

use std::fmt;
use std::io;

use serde::{Serialize, Serializer};

use crate::status::LicenseError;

/// Why `activate()` did not activate.
///
/// Serialises as the error's word (`"invalid-key"`, `"limit"`, ...,
/// `"storage"`), so a Tauri command can return it to the front end as is.
#[derive(Debug)]
#[non_exhaustive]
pub enum ActivateError {
    /// The server refused it, or could not be reached, or the SDK refused the
    /// key before asking (see [`LicenseError`]).
    License(LicenseError),
    /// The licence state could not be read or saved: nothing was changed on
    /// this device; try again. (When the server had already activated the
    /// seat, activating again re-uses it.)
    Storage(io::Error),
}

impl ActivateError {
    /// The licence error, unless this is a storage failure.
    pub fn license_error(&self) -> Option<LicenseError> {
        match self {
            ActivateError::License(error) => Some(*error),
            ActivateError::Storage(_) => None,
        }
    }

    /// The error's word: a [`LicenseError`]'s, or `storage`.
    pub fn code(&self) -> &'static str {
        match self {
            ActivateError::License(error) => error.as_str(),
            ActivateError::Storage(_) => "storage",
        }
    }
}

impl From<LicenseError> for ActivateError {
    fn from(error: LicenseError) -> Self {
        ActivateError::License(error)
    }
}

impl fmt::Display for ActivateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActivateError::License(error) => write!(f, "activation failed: {error}"),
            ActivateError::Storage(error) => {
                write!(f, "the licence state could not be saved: {error}")
            }
        }
    }
}

impl std::error::Error for ActivateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ActivateError::License(error) => Some(error),
            ActivateError::Storage(error) => Some(error),
        }
    }
}

impl Serialize for ActivateError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.code())
    }
}

/// What `activate()` answers: `Ok(())` once this device holds a verified lease.
pub type ActivateResult = Result<(), ActivateError>;

/// Why `deactivate()`, `upgrade_url()` or `purchase_url()` failed.
///
/// Serialises as its word (`"storage"`, `"busy"`, `"invalid-url"`, `"random"`).
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The licence state could not be read or saved.
    Storage(io::Error),
    /// A previous licence call on this `License` has not finished, and this
    /// call cannot write safely until it has (`deactivate()` drops the local
    /// licence; `purchase_url()` with no secret held saves a new one). Try again.
    Busy,
    /// The payment link is not an absolute `http` or `https` URL.
    InvalidUrl,
    /// The platform's random source failed, so `purchase_url()` could make
    /// no purchase secret.
    Random(io::Error),
}

impl Error {
    /// The error's word.
    pub fn code(&self) -> &'static str {
        match self {
            Error::Storage(_) => "storage",
            Error::Busy => "busy",
            Error::InvalidUrl => "invalid-url",
            Error::Random(_) => "random",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Storage(error) => {
                write!(f, "the licence state could not be read or saved: {error}")
            }
            Error::Busy => f.write_str("a previous licence call has not finished; try again"),
            Error::InvalidUrl => f.write_str("the payment link is not an absolute http(s) URL"),
            Error::Random(error) => {
                write!(f, "no purchase secret could be made: {error}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Storage(error) | Error::Random(error) => Some(error),
            _ => None,
        }
    }
}

impl Serialize for Error {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.code())
    }
}

/// Why a [`LicenseConfig`](crate::LicenseConfig) cannot make a `License`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigError {
    /// The product slug is empty, or holds a character a slug cannot (a path
    /// separator, a URL delimiter, white space or a control character).
    Product(String),
    /// The API base URL is not an absolute `http` or `https` URL.
    ApiBaseUrl(String),
    /// The public key cannot be read.
    PublicKey(String),
    /// No storage was given and there is no home folder to keep the state in.
    StorageLocation,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Product(product) => write!(f, "{product:?} is not a product slug"),
            ConfigError::ApiBaseUrl(url) => write!(f, "{url:?} is not an absolute http(s) URL"),
            ConfigError::PublicKey(why) => {
                write!(f, "the product's public key cannot be read: {why}")
            }
            ConfigError::StorageLocation => f.write_str(
                "there is no home folder to keep the licence state in; give a storage adapter",
            ),
        }
    }
}

impl std::error::Error for ConfigError {}
