//! Licence files on a `License`: taking one an app was handed, removing it,
//! and the status a stored one gives. Never online: a key file's key is
//! activated as a typed key is.

use serde::{Deserialize, Serialize};

use super::{ActivateOptions, License};
use crate::error::{Error, ImportError};
use crate::license_file::{
    license_file_status, license_file_token, licensed_to_of, LicenseFileCheck, LicenseFileError,
    LicenseFileKind,
};
use crate::offline::Basis;
use crate::state::{Patch, StoredState};
use crate::status::{LicenseStatus, LicensedTo};

/// What [`License::import_license_file`] took.
///
/// Serialises as `{"kind": "offline", "licensedTo": {...}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedLicenseFile {
    /// An offline file, now stored, or a key file, whose key is now activated.
    pub kind: LicenseFileKind,
    /// Who the file is licensed to, as it signs it.
    pub licensed_to: LicensedTo,
}

impl License {
    /// Take a licence file the customer was sent (`.kglicense`): its content,
    /// as text or bytes (`&str`, `String`, `&[u8]`, `Vec<u8>`), as a
    /// drag-and-drop handler or an open dialog reads it; never its path.
    ///
    /// Verified offline against the build's key set, for this product and,
    /// for an offline file, this build's major; refused when
    /// [`revoked_license_files`](crate::LicenseConfig::revoked_license_files)
    /// lists it ([`ImportError::File`], nothing stored). An OFFLINE file is
    /// then stored, replacing any stored before, and licenses the app with no
    /// network, ever: `status()` answers it with [`LicenseSource::File`](crate::LicenseSource::File).
    /// A KEY file's key is activated online exactly as
    /// [`activate`](License::activate) would, and its errors are
    /// `activate()`'s ([`ImportError::License`], [`ImportError::Storage`]).
    ///
    /// Fails with [`ImportError::Busy`] when a call before it has not finished,
    /// and with [`ImportError::Storage`] when an offline file cannot be saved.
    pub fn import_license_file(
        &self,
        content: impl AsRef<[u8]>,
    ) -> Result<ImportedLicenseFile, ImportError> {
        self.import_license_file_with(content, &ActivateOptions::default())
    }

    /// [`import_license_file`](License::import_license_file) with what a
    /// person typed for a user-locked key ([`ActivateOptions`]), passed on to
    /// a key file's activation exactly as
    /// [`activate_with`](License::activate_with) takes it. An offline file
    /// ignores it, and a file that is no licence file is refused before any
    /// key is asked for.
    pub fn import_license_file_with(
        &self,
        content: impl AsRef<[u8]>,
        options: &ActivateOptions,
    ) -> Result<ImportedLicenseFile, ImportError> {
        let content = content.as_ref();
        self.engine.serial(|read_only| {
            if read_only {
                return Err(ImportError::Busy);
            }
            self.import_now(content, options)
        })
    }

    /// Drop the stored offline licence file, if there is one; a key this
    /// device holds keeps working. Fails when a call before it has not
    /// finished ([`Error::Busy`]), or when that cannot be saved
    /// ([`Error::Storage`]). [`deactivate`](License::deactivate) leaves a
    /// stored file alone: it releases the key's seat, and a file holds none.
    pub fn remove_license_file(&self) -> Result<(), Error> {
        self.engine.serial(|read_only| {
            if read_only {
                return Err(Error::Busy);
            }
            self.save_license_file(None).map_err(Error::Storage)
        })
    }

    fn import_now(
        &self,
        content: &[u8],
        options: &ActivateOptions,
    ) -> Result<ImportedLicenseFile, ImportError> {
        let token =
            license_file_token(content).ok_or(ImportError::File(LicenseFileError::Malformed))?;
        let claims = match self.engine.check_file(&token) {
            LicenseFileCheck::Taken(claims) => claims,
            LicenseFileCheck::Refused(reason, _) => return Err(ImportError::File(reason)),
        };
        let taken = ImportedLicenseFile {
            kind: claims.kind(),
            licensed_to: licensed_to_of(&claims),
        };
        if taken.kind == LicenseFileKind::Key {
            self.activate_now(&claims.sub, options)?;
        } else {
            self.save_license_file(Some(token))
                .map_err(ImportError::Storage)?;
        }
        Ok(taken)
    }

    /// Store (or, with none, drop) the offline licence file. Based on
    /// nothing: it is about no activation, so it lands as it is on whatever is
    /// stored.
    fn save_license_file(&self, token: Option<String>) -> std::io::Result<()> {
        let patch = Patch {
            license_file: Some(token),
            ..Patch::default()
        };
        self.engine.save(&patch, None, &Basis::default())
    }

    /// The status the stored offline licence file gives, verified again now
    /// (`license_file_status`), or none when none is stored.
    pub(super) fn file_status(&self, state: &StoredState) -> Option<LicenseStatus> {
        let token = state.license_file.as_deref()?;
        Some(license_file_status(&self.engine.check_file(token)))
    }
}
