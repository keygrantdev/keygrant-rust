//! What a person typed for a user-locked key.

/// What a person typed for a user-locked key, for
/// [`License::activate_with`](crate::License::activate_with) and
/// [`License::import_license_file_with`](crate::License::import_license_file_with):
/// the address the key was sold to and, for a key that mails a code to it,
/// the code that came. Each is optional, and neither is ever stored.
///
/// A licence model can let only the person a key was sold to activate a NEW
/// device on it. The first activation of such a key answers
/// [`UserEmailRequired`](crate::LicenseError::UserEmailRequired); with the
/// address, [`UserCodeRequired`](crate::LicenseError::UserCodeRequired) (a
/// code was just mailed) or a lease; with the code, a lease. A key that is
/// not locked ignores both once they reach the server, but an address longer
/// than any, or a code that is not six digits, is refused here for any key
/// ([`UserEmailMismatch`](crate::LicenseError::UserEmailMismatch),
/// [`UserCodeInvalid`](crate::LicenseError::UserCodeInvalid)) before anything
/// is asked of the server.
///
/// ```
/// use keygrant::ActivateOptions;
///
/// let options = ActivateOptions::new().email("mara@studio.example").code("123 456");
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ActivateOptions {
    /// The address the key was sold to, as the person typed it. The ends are
    /// trimmed and a blank is none; the server compares it without regard to
    /// case.
    pub email: Option<String>,
    /// The code mailed to that address, as the person typed it. The ends are
    /// trimmed and the spaces inside dropped (`123 456` is `123456`); a blank
    /// is none.
    pub code: Option<String>,
}

impl ActivateOptions {
    /// No address and no code: what a key that is not locked needs.
    pub fn new() -> Self {
        ActivateOptions::default()
    }

    /// The address the key was sold to.
    pub fn email(mut self, email: impl Into<String>) -> Self {
        self.email = Some(email.into());
        self
    }

    /// The code mailed to that address.
    pub fn code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
}
