//! Metered values read leniently from JSON, as the reference reads them: a
//! lease's `met`, the figures a `use` answer gives, and the journal and
//! figures a state file holds. A value of another shape is left out, never
//! the lease, the answer or the state it rides in.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::{BaseFigure, MeterFigure, SpendEntry, SpendKind};

/// The largest whole number a double holds exactly (2^53 - 1): every KeyGrant
/// SDK draws the line here, so no two read a lease differently.
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// A figure of a meter: a JSON number that is a non-negative whole number a
/// double holds exactly, however it is written (`4.0` is 4, `1e1` is 10, `-0`
/// is 0). A number out of a double's range, a fraction, a negative, or one
/// past 2^53 - 1 is none.
pub fn figure_of(value: &Value) -> Option<u64> {
    let number = value.as_number()?.as_f64()?;
    let whole = number.is_finite() && number >= 0.0 && number.fract() == 0.0;
    // 2^53 - 1 is exact as a double; `as` is exact for every double below 2^64.
    (whole && number <= MAX_SAFE_INTEGER as f64).then_some(number as u64)
}

/// A whole number from 1 to 2^53 - 1, however written: the count of a spend
/// or of a confirmation.
pub fn count_of(value: &Value) -> Option<u64> {
    figure_of(value).filter(|n| *n >= 1)
}

/// A meter as a lease or an answer gives it: an object whose `left`, `total`
/// and `offline` are each a figure. `resets` is true only for JSON `true`.
fn meter_of(value: &Value) -> Option<BaseFigure> {
    let meter = value.as_object()?;
    let figure = |name: &str| meter.get(name).and_then(figure_of);
    Some(BaseFigure {
        left: figure("left")?,
        total: figure("total")?,
        offline: figure("offline")?,
        resets: meter.get("resets") == Some(&Value::Bool(true)),
    })
}

/// A lease's `met` (or the `meters` of a `use` answer) read leniently: every
/// member that is an object whose `left`, `total` and `offline` are each a
/// figure; any other member is dropped, never the lease and never the meters
/// beside it. Anything but an object (a list, a text, a number, a flag, `null`)
/// is no meters. Names are kept as they are, `__proto__` included, and a
/// meter's other members are ignored. The server's caps (how many, how large,
/// `left` at most `total`) are not enforced: the signature is the server's
/// word, and a later server may raise them without locking out a build made
/// before.
pub fn meters_of(value: &Value) -> BTreeMap<String, BaseFigure> {
    let Some(members) = value.as_object() else {
        return BTreeMap::new();
    };
    members
        .iter()
        .filter_map(|(code, member)| Some((code.clone(), meter_of(member)?)))
        .collect()
}

/// A stored journal entry: an object with a text `id` and `meter`, an `n` that
/// is a count, an `at` that is a figure, and one of the three kinds. Any other
/// is left out, and an entry's unknown members are not kept.
fn spend_of(value: &Value) -> Option<SpendEntry> {
    let entry = value.as_object()?;
    Some(SpendEntry {
        id: entry.get("id")?.as_str()?.to_owned(),
        meter: entry.get("meter")?.as_str()?.to_owned(),
        n: count_of(entry.get("n")?)?,
        at: i64::try_from(figure_of(entry.get("at")?)?).ok()?,
        kind: SpendKind::parse(entry.get("kind")?.as_str()?)?,
    })
}

/// The journal a stored `spends` is: its entries in order; anything that is
/// not a list is an empty journal, and an entry of another shape is left out.
/// The state is the device's own, never the server's word.
pub fn spends_of(stored: &Value) -> Vec<SpendEntry> {
    stored
        .as_array()
        .map(|entries| entries.iter().filter_map(spend_of).collect())
        .unwrap_or_default()
}

/// The figures a stored `meterFigures` holds, by code: a member that is not an
/// object of four figures and a time is left out.
pub fn stored_figures(stored: &Value) -> BTreeMap<String, MeterFigure> {
    let Some(members) = stored.as_object() else {
        return BTreeMap::new();
    };
    members
        .iter()
        .filter_map(|(code, member)| Some((code.clone(), stored_figure_of(member)?)))
        .collect()
}

fn stored_figure_of(value: &Value) -> Option<MeterFigure> {
    let figure = value.as_object()?;
    let number = |name: &str| figure.get(name).and_then(figure_of);
    Some(MeterFigure {
        left: number("left")?,
        total: number("total")?,
        offline: number("offline")?,
        resets: figure.get("resets") == Some(&Value::Bool(true)),
        at: i64::try_from(number("at")?).ok()?,
    })
}

/// A figure as the state file writes it, `at` included.
pub fn stored_figure_json(figure: &MeterFigure) -> Value {
    let mut out = Map::new();
    out.insert("left".into(), Value::from(figure.left));
    out.insert("total".into(), Value::from(figure.total));
    out.insert("offline".into(), Value::from(figure.offline));
    out.insert("resets".into(), Value::from(figure.resets));
    out.insert("at".into(), Value::from(figure.at));
    Value::Object(out)
}

/// A journal entry as the state file writes it.
pub fn spend_json(entry: &SpendEntry) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), Value::from(entry.id.clone()));
    out.insert("meter".into(), Value::from(entry.meter.clone()));
    out.insert("n".into(), Value::from(entry.n));
    out.insert("at".into(), Value::from(entry.at));
    out.insert("kind".into(), Value::from(entry.kind.as_str()));
    Value::Object(out)
}

/// A lease's meters as the conformance vectors write them: `resets` always
/// there.
pub fn meters_json(meters: &BTreeMap<String, BaseFigure>) -> Value {
    Value::Object(
        meters
            .iter()
            .map(|(code, figure)| {
                let mut out = Map::new();
                out.insert("left".into(), Value::from(figure.left));
                out.insert("total".into(), Value::from(figure.total));
                out.insert("offline".into(), Value::from(figure.offline));
                out.insert("resets".into(), Value::from(figure.resets));
                (code.clone(), Value::Object(out))
            })
            .collect(),
    )
}
