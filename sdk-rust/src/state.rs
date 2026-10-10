//! The state the SDK keeps on the device, in the Electron SDK's file format,
//! and the patches its saves are made of.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::status::{DeviceKind, Refusal};

/// A stored field's value, read leniently: a value of the wrong type is read
/// as absent, never as a file that does not parse (which would cost the
/// customer their licence).
pub(crate) trait Field: Sized + Clone {
    fn from_json(value: &Value) -> Option<Self>;
    fn to_json(&self) -> Value;
}

impl Field for String {
    fn from_json(value: &Value) -> Option<Self> {
        value.as_str().map(str::to_owned)
    }
    fn to_json(&self) -> Value {
        Value::String(self.clone())
    }
}

impl Field for i64 {
    fn from_json(value: &Value) -> Option<Self> {
        lenient_i64(value)
    }
    fn to_json(&self) -> Value {
        Value::from(*self)
    }
}

impl Field for Vec<String> {
    fn from_json(value: &Value) -> Option<Self> {
        let items = value.as_array()?;
        Some(
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect(),
        )
    }
    fn to_json(&self) -> Value {
        Value::Array(self.iter().cloned().map(Value::String).collect())
    }
}

impl Field for Refusal {
    fn from_json(value: &Value) -> Option<Self> {
        value.as_str().and_then(Refusal::parse)
    }
    fn to_json(&self) -> Value {
        Value::String(self.as_str().to_owned())
    }
}

impl Field for DeviceKind {
    fn from_json(value: &Value) -> Option<Self> {
        value.as_str().and_then(DeviceKind::parse)
    }
    fn to_json(&self) -> Value {
        Value::String(self.as_str().to_owned())
    }
}

/// A JSON number as whole milliseconds (or a whole major): an integer as it
/// is, a fraction cut toward zero, anything else none.
pub(crate) fn lenient_i64(value: &Value) -> Option<i64> {
    let number = value.as_number()?;
    if let Some(whole) = number.as_i64() {
        return Some(whole);
    }
    let float = number.as_f64()?;
    // `as` cuts toward zero and saturates at the ends of the range.
    float.is_finite().then_some(float as i64)
}

macro_rules! state_fields {
    ($( $(#[doc = $doc:literal])* $field:ident : $ty:ty = $json:literal; )*) => {
        /// The state the SDK keeps on the device (the Electron SDK's
        /// `license.json`, field for field: the same file is read and written
        /// by both).
        ///
        /// Serialised with the reference's camelCase names, absent fields left
        /// out. Fields this SDK does not know are kept in [`extra`](Self::extra)
        /// and written back as they were, but for the few an earlier version
        /// stored and this one retired, which are neither read nor written.
        #[derive(Debug, Clone, Default, PartialEq)]
        pub struct StoredState {
            $( $(#[doc = $doc])* pub $field: Option<$ty>, )*
            /// Fields this SDK does not know (a newer SDK's), kept as they were.
            /// A retired field's name here is never written.
            pub extra: Map<String, Value>,
        }

        /// A change to a [`StoredState`]: per field, not touched (`None`),
        /// cleared (`Some(None)`) or set (`Some(Some(value))`).
        #[derive(Debug, Clone, Default, PartialEq)]
        pub struct Patch {
            $( #[doc = concat!("`", $json, "`.")] pub $field: Option<Option<$ty>>, )*
        }

        /// Every field's stored name, in order.
        pub(crate) const FIELD_NAMES: &[&str] = &[$($json),*];

        /// What an earlier version stored to buy from the app with no key
        /// typed (its purchase secret and the two stamps beside it). Retired:
        /// not read, not even into [`extra`](StoredState::extra), and never
        /// written, whatever `extra` holds, so the next save clears them.
        pub(crate) const RETIRED_FIELDS: &[&str] = &["purchaseToken", "purchaseTokenAt", "claimAskedAt"];

        impl StoredState {
            /// The state a JSON value holds: none for `null`, an empty state
            /// for anything that is not an object, and an object read field by
            /// field (a field of the wrong type read as absent).
            pub fn from_json(value: &Value) -> Option<StoredState> {
                let map = match value {
                    Value::Null => return None,
                    Value::Object(map) => map,
                    _ => return Some(StoredState::default()),
                };
                let mut state = StoredState::default();
                $( state.$field = map.get($json).and_then(<$ty as Field>::from_json); )*
                for (name, item) in map {
                    if !FIELD_NAMES.contains(&name.as_str()) && !RETIRED_FIELDS.contains(&name.as_str()) {
                        state.extra.insert(name.clone(), item.clone());
                    }
                }
                Some(state)
            }

            /// The state as the JSON object stored for it.
            pub fn to_json(&self) -> Value {
                let mut map = Map::new();
                $( if let Some(value) = &self.$field { map.insert($json.to_owned(), Field::to_json(value)); } )*
                for (name, item) in &self.extra {
                    if !FIELD_NAMES.contains(&name.as_str()) && !RETIRED_FIELDS.contains(&name.as_str()) {
                        map.insert(name.clone(), item.clone());
                    }
                }
                Value::Object(map)
            }
        }

        impl Patch {
            /// This patch with `later`'s fields over it, as `{ ...this, ...later }`.
            pub fn then(mut self, later: Patch) -> Patch {
                $( if later.$field.is_some() { self.$field = later.$field; } )*
                self
            }

            /// `state` with this patch's fields set or cleared on it.
            pub fn applied_to(&self, mut state: StoredState) -> StoredState {
                $( if let Some(value) = &self.$field { state.$field = value.clone(); } )*
                state
            }

            /// This patch with only the fields `keep` names.
            pub fn retain(mut self, keep: impl Fn(&str) -> bool) -> Patch {
                $( if !keep($json) { self.$field = None; } )*
                self
            }

            /// The fields it sets, as JSON.
            pub fn set_fields(&self) -> Map<String, Value> {
                let mut map = Map::new();
                $( if let Some(Some(value)) = &self.$field { map.insert($json.to_owned(), Field::to_json(value)); } )*
                map
            }

            /// The names of the fields it clears, in field order.
            pub fn cleared_fields(&self) -> Vec<&'static str> {
                let mut names = Vec::new();
                $( if let Some(None) = &self.$field { names.push($json); } )*
                names
            }

            /// The patch as JSON carries it: a field set is its value, a
            /// field cleared is `null`, a field left alone is absent.
            pub fn encoded(&self) -> Map<String, Value> {
                let mut map = Map::new();
                $(
                    match &self.$field {
                        Some(Some(value)) => { map.insert($json.to_owned(), Field::to_json(value)); }
                        Some(None) => { map.insert($json.to_owned(), Value::Null); }
                        None => {}
                    }
                )*
                map
            }

            /// A patch from a JSON object: each known field set to its value
            /// (`null` clears it); unknown fields and values of the wrong type
            /// are left out.
            pub fn from_json(map: &Map<String, Value>) -> Patch {
                let mut patch = Patch::default();
                $(
                    if let Some(value) = map.get($json) {
                        patch.$field = if value.is_null() {
                            Some(None)
                        } else {
                            <$ty as Field>::from_json(value).map(Some)
                        };
                    }
                )*
                patch
            }
        }
    };
}

state_fields! {
    /// The licence key, normalised (trimmed, upper case).
    key: String = "key";
    /// The activation (device seat) this device holds.
    activation_id: String = "activationId";
    /// The signed JWT lease for a paid licence.
    lease: String = "lease";
    /// The earliest trial start ever seen (ms). Only ever moves earlier.
    trial_start: i64 = "trialStart";
    /// The server-authoritative trial end (ms).
    trial_ends_at: i64 = "trialEndsAt";
    /// The signed JWT lease for a trial.
    trial_lease: String = "trialLease";
    /// When (guarded device clock, ms) the stored trial answer was asked for.
    trial_at: i64 = "trialAt";
    /// The monotonic high-water clock (ms): guards against a clock put back.
    last_seen: i64 = "lastSeen";
    /// The app major sent with the last check-in the server answered.
    checked_major: i64 = "checkedMajor";
    /// The newest app major run while holding a lease signed inside the key's
    /// upgrade window, before that window's end.
    kept_major: i64 = "keptMajor";
    /// When (device clock, ms) the last ask of the server came back without a lease.
    backoff_since: i64 = "backoffSince";
    /// How long to wait after `backoff_since` before asking again on its own (ms).
    backoff_ms: i64 = "backoffMs";
    /// The server's definitive refusal of this key on this device.
    refused: Refusal = "refused";
    /// The build major an `upgrade` or `outdated` refusal was given for.
    refused_major: i64 = "refusedMajor";
    /// The device (its `fp` claims) a `device` refusal was given for.
    refused_for: Vec<String> = "refusedFor";
    /// What this install is keyed by: `machine` once a machine id was read here.
    device_kind: DeviceKind = "deviceKind";
    /// When (guarded device clock, ms) the stored server answer was asked for.
    verdict_at: i64 = "verdictAt";
    /// When (guarded device clock, ms) a running trial last reported its
    /// launch: at most once a day, a trial answer (`trialAt`) counting as one.
    trial_reported_at: i64 = "trialReportedAt";
    /// The default storage's revision of this state, one more on every save.
    rev: i64 = "rev";
}

impl Serialize for StoredState {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_json().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for StoredState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Ok(StoredState::from_json(&value).unwrap_or_default())
    }
}

/// Whether a stored string is there and not empty (JavaScript truthiness).
pub(crate) fn present(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|text| !text.is_empty())
}
