//! What the SDK sends and reads on the wire, and the small pieces of it that
//! need no engine state: the server's error words, the bounds the endpoints
//! hold strings to, and the shapes of their answers.

use std::time::Duration;

use serde_json::Value;

use crate::state::lenient_i64;
use crate::status::{LicenseError, Refusal};

/// Map a server error word to the SDK's [`LicenseError`].
pub fn map_server_error(error: Option<&str>) -> LicenseError {
    match error {
        Some("invalid-key") => LicenseError::InvalidKey,
        Some("limit") => LicenseError::Limit,
        Some("revoked") | Some("expired") => LicenseError::Revoked,
        Some("upgrade-required") => LicenseError::Upgrade,
        Some("update-required") => LicenseError::Outdated,
        // A user-locked key's new device: each word is its own error.
        Some("user-email-required") => LicenseError::UserEmailRequired,
        Some("user-email-mismatch") => LicenseError::UserEmailMismatch,
        Some("user-code-required") => LicenseError::UserCodeRequired,
        Some("user-code-invalid") => LicenseError::UserCodeInvalid,
        Some("too-many-codes") => LicenseError::TooManyCodes,
        Some("too-many-attempts") => LicenseError::TooManyAttempts,
        Some("try-again") => LicenseError::TryAgain,
        _ => LicenseError::Network,
    }
}

/// The definitive refusal a validate's error is, or none for any other error,
/// which is never taken as a verdict (a proxy's 4xx, an unknown product, a
/// body the server could not read).
pub fn refusal_of(error: Option<&str>) -> Option<Refusal> {
    match error {
        Some("upgrade-required") => Some(Refusal::Upgrade),
        Some("update-required") => Some(Refusal::Outdated),
        Some("revoked") | Some("expired") | Some("deactivated") => Some(Refusal::Revoked),
        Some("device") => Some(Refusal::Device),
        _ => None,
    }
}

/// What the activate endpoint accepts for a device name; longer is cut, not refused.
pub const MAX_DEVICE_NAME: usize = 120;
/// What it accepts for a licence key; a longer one is not a key it could hold.
pub const MAX_KEY: usize = 120;

/// How long a check `status()` makes on a valid lease (a newer major, or an
/// early renewal) may hold it up.
pub const STATUS_CHECK_TIMEOUT: Duration = Duration::from_millis(5_000);

/// The key a person entered, NORMALISED (trimmed as JavaScript trims, upper
/// case), which is what is sent and stored; or none when it cannot be a key
/// (nothing, or longer than [`MAX_KEY`] UTF-16 units).
pub fn entered_key(key: &str) -> Option<String> {
    let entered = js_trim(key).to_uppercase();
    (!entered.is_empty() && utf16_len(&entered) <= MAX_KEY).then_some(entered)
}

/// The longest address the endpoint takes; a longer one is no key's address.
pub const MAX_EMAIL: usize = 254;
/// How many digits a code has.
const CODE_DIGITS: usize = 6;

/// What an activation sends for what a person typed for a user-locked key:
/// the address and the code, each only when there is one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Typed {
    /// The address, trimmed, as typed otherwise.
    pub email: Option<String>,
    /// The code, its spaces dropped: exactly six ASCII digits.
    pub code: Option<String>,
}

/// What an activation sends for the address and code a person typed, or the
/// error that refuses them before anything is sent (an address longer than
/// any is `UserEmailMismatch`, a code that is not six digits
/// `UserCodeInvalid`; the address is judged first).
pub fn typed_for(email: Option<&str>, code: Option<&str>) -> Result<Typed, LicenseError> {
    let email = entered_email(email)?;
    let code = entered_code(code)?;
    Ok(Typed { email, code })
}

/// The address a person entered, trimmed as a key is: none when it is blank
/// (and it is not sent). The server compares it without regard to case, so
/// it is sent as typed. One longer than [`MAX_EMAIL`] UTF-16 units after the
/// trim can match no key's address.
fn entered_email(email: Option<&str>) -> Result<Option<String>, LicenseError> {
    let entered = js_trim(email.unwrap_or_default());
    if entered.is_empty() {
        return Ok(None);
    }
    if utf16_len(entered) <= MAX_EMAIL {
        Ok(Some(entered.to_owned()))
    } else {
        Err(LicenseError::UserEmailMismatch)
    }
}

/// The code a person entered: its ends trimmed as a key's are, then its
/// spaces (U+0020) dropped, since a code read off an email is typed as
/// `123 456` as often as `123456`. None when it is blank; else exactly six
/// ASCII digits, or `UserCodeInvalid`.
fn entered_code(code: Option<&str>) -> Result<Option<String>, LicenseError> {
    let entered: String = js_trim(code.unwrap_or_default())
        .chars()
        .filter(|c| *c != ' ')
        .collect();
    if entered.is_empty() {
        return Ok(None);
    }
    if entered.len() == CODE_DIGITS && entered.bytes().all(|b| b.is_ascii_digit()) {
        Ok(Some(entered))
    } else {
        Err(LicenseError::UserCodeInvalid)
    }
}

/// `value` cut to `max` UTF-16 units without splitting a character: a cut
/// that would leave half a surrogate pair drops the half.
pub fn trim_to_length(value: &str, max: usize) -> String {
    let units: Vec<u16> = value.encode_utf16().collect();
    if units.len() <= max {
        return value.to_owned();
    }
    let mut cut = &units[..max];
    if let Some(last) = cut.last() {
        if (0xd800..=0xdbff).contains(last) {
            cut = &cut[..cut.len() - 1];
        }
    }
    String::from_utf16_lossy(cut)
}

/// A string's length as JavaScript counts it (UTF-16 units).
pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Whether JavaScript's `String.prototype.trim` removes `c`.
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// `text` trimmed as JavaScript's `trim()` trims it.
pub fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_whitespace)
}

/// The `error` word of a server body, when it has one that is a string.
pub fn error_of(json: Option<&Value>) -> Option<&str> {
    json?.as_object()?.get("error")?.as_str()
}

/// The `lease` and `activationId` of a server body, each kept only when it is
/// a string (an empty one included: whether it is usable is the caller's call).
pub fn parse_lease_body(json: Option<&Value>) -> (Option<String>, Option<String>) {
    let Some(map) = json.and_then(Value::as_object) else {
        return (None, None);
    };
    let text = |name: &str| map.get(name).and_then(Value::as_str).map(str::to_owned);
    (text("lease"), text("activationId"))
}

/// The (unsigned) trial answer: its start and end (ms), and its lease when it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrialBody {
    /// The server's trial start (ms).
    pub trial_start: i64,
    /// The server's trial end (ms).
    pub trial_ends_at: i64,
    /// The signed trial lease.
    pub lease: Option<String>,
}

/// The trial answer, or none when it is not one (its start and end must be numbers).
pub fn parse_trial_body(json: Option<&Value>) -> Option<TrialBody> {
    let map = json?.as_object()?;
    Some(TrialBody {
        trial_start: lenient_i64(map.get("trialStart")?)?,
        trial_ends_at: lenient_i64(map.get("trialEndsAt")?)?,
        lease: map.get("lease").and_then(Value::as_str).map(str::to_owned),
    })
}

/// The smallest of the times given, or none when none is.
pub fn earliest(values: &[Option<i64>]) -> Option<i64> {
    values.iter().flatten().copied().min()
}

/// Whether a response is no answer at all: an outage (5xx) or a refusal to serve now (429).
pub fn no_answer(status: u16) -> bool {
    status >= 500 || status == 429
}
