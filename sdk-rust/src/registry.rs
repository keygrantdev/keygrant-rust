//! An uninstall-surviving trial stash in the Windows registry (opt-in).

#[cfg(windows)]
use winreg::enums::{RegType, HKEY_CURRENT_USER, KEY_READ};
#[cfg(windows)]
use winreg::{RegKey, RegValue};

#[cfg(windows)]
use crate::adapters::RegistryStash;

/// The earliest trial start, stashed under
/// `HKEY_CURRENT_USER\Software\KeyGrant\<product>` (value `TrialStart`),
/// which an uninstall leaves behind: a reinstall cannot reset a trial.
/// Opt-in: pass it as the `registry` adapter. Best-effort both ways.
///
/// The format every KeyGrant SDK keeps, so a trial one of them stashed is seen
/// by another for the same product: the start in ms since the epoch, written
/// as a `REG_QWORD`. It is read back from a `REG_QWORD`, or from a `REG_SZ` of
/// decimal digits (what a script or a hand writes). Anything else is no
/// evidence: another type (a `REG_DWORD` read as a time is 1970), a value of 0
/// or less, or one of 2^53 or more; and a start of that kind is not written
/// over one that is evidence.
#[cfg(windows)]
#[derive(Debug, Clone)]
pub struct WindowsRegistryStash {
    subkey: String,
}

#[cfg(windows)]
impl WindowsRegistryStash {
    /// The stash for `product`, under `HKCU\Software\KeyGrant\<product>`.
    pub fn new(product: &str) -> Self {
        WindowsRegistryStash {
            subkey: format!(r"Software\KeyGrant\{product}"),
        }
    }

    /// The stash under another `HKCU` subkey (`Software\MyCompany\MyApp`, say).
    pub fn at(subkey: &str) -> Self {
        WindowsRegistryStash {
            subkey: subkey.to_owned(),
        }
    }
}

#[cfg(windows)]
const VALUE: &str = "TrialStart";

/// The first start that is no evidence: 2^53 ms, past what a JavaScript
/// number holds exactly (and some 285,000 years from now).
#[cfg(windows)]
const PAST_EVIDENCE: u64 = 1 << 53;

/// `ms` when it is a start a stash may hold: more than 0, less than 2^53.
#[cfg(windows)]
fn evidence(ms: u64) -> Option<i64> {
    if (1..PAST_EVIDENCE).contains(&ms) {
        i64::try_from(ms).ok()
    } else {
        None
    }
}

/// The start a stashed value holds ([`WindowsRegistryStash`]), or none.
#[cfg(windows)]
fn trial_start_of(value: &RegValue) -> Option<i64> {
    let ms = match value.vtype {
        RegType::REG_QWORD => u64::from_le_bytes(value.bytes.as_slice().try_into().ok()?),
        RegType::REG_SZ => decimal(&value.bytes)?,
        _ => return None,
    };
    evidence(ms)
}

/// A `REG_SZ`'s text (UTF-16, its terminating NULs dropped) as a number,
/// when it is decimal digits and nothing else; none past `u64`.
#[cfg(windows)]
fn decimal(bytes: &[u8]) -> Option<u64> {
    let pairs = bytes.chunks_exact(2);
    if !pairs.remainder().is_empty() {
        return None;
    }
    let units: Vec<u16> = pairs
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let end = units
        .iter()
        .rposition(|unit| *unit != 0)
        .map_or(0, |last| last + 1);
    let digits = &units[..end];
    if digits.is_empty() {
        return None;
    }
    digits.iter().try_fold(0u64, |number, unit| {
        let digit = char::from_u32(u32::from(*unit))?.to_digit(10)?;
        number.checked_mul(10)?.checked_add(u64::from(digit))
    })
}

#[cfg(windows)]
impl RegistryStash for WindowsRegistryStash {
    fn read_trial_start(&self) -> Option<i64> {
        let key = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(&self.subkey, KEY_READ)
            .ok()?;
        trial_start_of(&key.get_raw_value(VALUE).ok()?)
    }

    fn write_trial_start(&self, ms: i64) {
        let Some(ms) = u64::try_from(ms).ok().filter(|ms| evidence(*ms).is_some()) else {
            return;
        };
        if let Ok((key, _)) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(&self.subkey) {
            // A u64 is written as a REG_QWORD.
            let _ = key.set_value(VALUE, &ms);
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use winreg::enums::KEY_WRITE;
    use winreg::types::ToRegValue;

    /// A subkey of this test's own under `HKCU\Software`, removed when dropped.
    struct Scratch(String);

    impl Scratch {
        fn new() -> Scratch {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or_default();
            Scratch(format!(
                r"Software\KeyGrant-test-{}-{nanos}",
                std::process::id()
            ))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = RegKey::predef(HKEY_CURRENT_USER).delete_subkey_all(&self.0);
        }
    }

    /// Text as a value of type `vtype`, terminated as Windows terminates it.
    fn text(text: &str, vtype: RegType) -> RegValue {
        let bytes = text
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect();
        RegValue { bytes, vtype }
    }

    const START: i64 = 1_790_000_000_000;
    const LAST: u64 = (1 << 53) - 1;

    #[test]
    fn reads_a_qword_or_decimal_text_and_nothing_else_as_a_start() {
        let qword = |ms: u64| ms.to_reg_value();
        assert_eq!(trial_start_of(&qword(START as u64)), Some(START));
        assert_eq!(trial_start_of(&qword(1)), Some(1));
        assert_eq!(trial_start_of(&qword(LAST)), Some(LAST as i64));
        assert_eq!(trial_start_of(&qword(0)), None);
        assert_eq!(trial_start_of(&qword(1 << 53)), None);
        // A negative start, written as a QWORD, reads as one past 2^63.
        assert_eq!(trial_start_of(&qword(-START as u64)), None);
        assert_eq!(trial_start_of(&qword(u64::MAX)), None);
        // A QWORD of another size is no QWORD.
        let short = RegValue {
            bytes: vec![1, 2, 3, 4],
            vtype: RegType::REG_QWORD,
        };
        assert_eq!(trial_start_of(&short), None);

        let sz = |value: &str| text(value, RegType::REG_SZ);
        assert_eq!(trial_start_of(&sz("1790000000000")), Some(START));
        assert_eq!(trial_start_of(&sz("0001790000000000")), Some(START));
        assert_eq!(trial_start_of(&sz("9007199254740991")), Some(LAST as i64));
        // Unterminated, as a program may write it.
        let bare = RegValue {
            bytes: "42".encode_utf16().flat_map(u16::to_le_bytes).collect(),
            vtype: RegType::REG_SZ,
        };
        assert_eq!(trial_start_of(&bare), Some(42));
        for not_a_start in [
            "0",
            "-1790000000000",
            "+1790000000000",
            "9007199254740992",
            "99999999999999999999999",
            "not a time",
            "1790000000000 ",
            " 1790000000000",
            "1790000000000.0",
            "1.79e12",
            "0x1A0C",
            "١٧٩",
            "",
        ] {
            assert_eq!(trial_start_of(&sz(not_a_start)), None, "{not_a_start:?}");
        }
        let odd = RegValue {
            bytes: vec![b'4', 0, b'2'],
            vtype: RegType::REG_SZ,
        };
        assert_eq!(trial_start_of(&odd), None);

        // Any other type is no evidence, a DWORD (a time in 1970) first.
        assert_eq!(trial_start_of(&42u32.to_reg_value()), None);
        assert_eq!(
            trial_start_of(&text("1790000000000", RegType::REG_EXPAND_SZ)),
            None
        );
        assert_eq!(
            trial_start_of(&text("1790000000000", RegType::REG_MULTI_SZ)),
            None
        );
        let binary = RegValue {
            bytes: (START as u64).to_le_bytes().to_vec(),
            vtype: RegType::REG_BINARY,
        };
        assert_eq!(trial_start_of(&binary), None);
    }

    #[test]
    fn stashes_the_trial_start_as_a_qword_and_reads_back_either_type() {
        let scratch = Scratch::new();
        let stash = WindowsRegistryStash::at(&scratch.0);
        assert_eq!(stash.read_trial_start(), None);
        stash.write_trial_start(START);
        assert_eq!(stash.read_trial_start(), Some(START));
        let key = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(&scratch.0, KEY_READ | KEY_WRITE)
            .expect("the scratch key");
        let raw = key.get_raw_value(VALUE).expect("written");
        assert_eq!(raw.vtype, RegType::REG_QWORD);
        assert_eq!(raw.bytes, (START as u64).to_le_bytes());
        stash.write_trial_start(START - 1);
        assert_eq!(stash.read_trial_start(), Some(START - 1));
        // A start no reader takes is not written over one it does.
        for not_a_start in [0, -START, 1 << 53, i64::MAX] {
            stash.write_trial_start(not_a_start);
            assert_eq!(stash.read_trial_start(), Some(START - 1), "{not_a_start}");
        }
        // Another SDK's QWORD, and a start a script or a hand wrote as text.
        key.set_value(VALUE, &1_780_000_000_000u64)
            .expect("written");
        assert_eq!(stash.read_trial_start(), Some(1_780_000_000_000));
        key.set_value(VALUE, &"1770000000000").expect("written");
        assert_eq!(
            key.get_raw_value(VALUE).expect("written").vtype,
            RegType::REG_SZ
        );
        assert_eq!(stash.read_trial_start(), Some(1_770_000_000_000));
        // What is not a start reads as none: no evidence, never a guess.
        key.set_value(VALUE, &"not a time").expect("written");
        assert_eq!(stash.read_trial_start(), None);
        key.set_value(VALUE, &"0").expect("written");
        assert_eq!(stash.read_trial_start(), None);
        key.set_value(VALUE, &"-1770000000000").expect("written");
        assert_eq!(stash.read_trial_start(), None);
        key.set_value(VALUE, &(1u64 << 53)).expect("written");
        assert_eq!(stash.read_trial_start(), None);
        key.set_value(VALUE, &0u64).expect("written");
        assert_eq!(stash.read_trial_start(), None);
        key.set_value(VALUE, &1_770_000u32).expect("written");
        assert_eq!(stash.read_trial_start(), None);
        // And the stash writes over any of them.
        stash.write_trial_start(START);
        assert_eq!(stash.read_trial_start(), Some(START));
    }

    #[test]
    fn keeps_each_product_under_its_own_key() {
        let stash = WindowsRegistryStash::new("sluice");
        assert_eq!(stash.subkey, r"Software\KeyGrant\sluice");
    }
}
