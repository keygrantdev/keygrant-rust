//! What a request reports of the journal, the reports an answer confirms, and
//! what a `use` answer saves and says.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::read::{count_of, figure_of, meters_of};
use super::{MeterFigure, SpendEntry, SpendKind, MAX_PENDING_SPENDS};
use crate::adapters::HttpResponse;
use crate::offline::{keyed_by, refusal_patch, ASK_AGAIN_MS, RETRY_UNANSWERED_MS};
use crate::state::Patch;
use crate::status::{DeviceKind, Refusal};
use crate::wire::{error_of, no_answer, refusal_of};

// --- reports ----------------------------------------------------------------

/// The report a body carries: the journal's entries, oldest first, at most
/// [`MAX_PENDING_SPENDS`], a void marked; none for an empty journal (the member
/// is absent, never an empty list).
pub fn spends_reported(spends: &[SpendEntry]) -> Option<Value> {
    let report: Vec<Value> = spends
        .iter()
        .take(MAX_PENDING_SPENDS)
        .map(|entry| {
            let mut out = Map::new();
            out.insert("id".into(), Value::from(entry.id.clone()));
            out.insert("meter".into(), Value::from(entry.meter.clone()));
            out.insert("n".into(), Value::from(entry.n));
            out.insert("at".into(), Value::from(entry.at));
            if entry.kind == SpendKind::Void {
                out.insert("void".into(), Value::Bool(true));
            }
            Value::Object(out)
        })
        .collect();
    (!report.is_empty()).then_some(Value::Array(report))
}

/// A report the server names as recorded: how many it holds of a spend, or that
/// it took a void.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Confirmed {
    /// The server holds this many of the spend with this id.
    Count {
        /// The spend's id.
        id: String,
        /// How many of it the server holds.
        n: u64,
    },
    /// The server took the spend with this id back (a void).
    Void {
        /// The void's id.
        id: String,
    },
}

impl Confirmed {
    /// The spend's id.
    pub fn id(&self) -> &str {
        match self {
            Confirmed::Count { id, .. } | Confirmed::Void { id } => id,
        }
    }
}

/// The reports an answer names as recorded (`spends`), read leniently: a member
/// of another shape confirms nothing. A void names itself (its `void` is JSON
/// `true`; its `n` is then not read); any other names how many the server holds
/// (a whole number from 1 to 2^53 - 1). No answer at all, a 5xx and a 429
/// confirm nothing, whatever their body says.
pub fn spends_confirmed(response: Option<&HttpResponse>) -> Vec<Confirmed> {
    let Some(response) = response.filter(|response| !no_answer(response.status)) else {
        return Vec::new();
    };
    let listed = response
        .json
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|body| body.get("spends"))
        .and_then(Value::as_array);
    listed
        .into_iter()
        .flatten()
        .filter_map(|member| {
            let member = member.as_object()?;
            let id = member.get("id")?.as_str()?.to_owned();
            if member.get("void") == Some(&Value::Bool(true)) {
                return Some(Confirmed::Void { id });
            }
            Some(Confirmed::Count {
                id,
                n: count_of(member.get("n")?)?,
            })
        })
        .collect()
}

// --- what an answer does -----------------------------------------------------

/// What a `use` answer comes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UseOutcomeKind {
    /// A 200 with `spent: true`.
    Spent,
    /// Fewer left than asked.
    UsedUp,
    /// The server knows no such meter.
    NoMeter,
    /// The server's verdict on the key, this build or this device.
    Refused,
    /// No usable answer: the spend comes from the offline allowance, in doubt
    /// when it was sent.
    Unanswered,
}

impl UseOutcomeKind {
    /// The word the conformance vectors write it as.
    pub fn as_str(self) -> &'static str {
        match self {
            UseOutcomeKind::Spent => "spent",
            UseOutcomeKind::UsedUp => "used-up",
            UseOutcomeKind::NoMeter => "no-meter",
            UseOutcomeKind::Refused => "refused",
            UseOutcomeKind::Unanswered => "unanswered",
        }
    }
}

/// What a `use` answer is judged from.
#[derive(Debug, Clone, Copy)]
pub struct UseInput<'a> {
    /// The answer; none when there was none at all (the request failed or
    /// timed out).
    pub response: Option<&'a HttpResponse>,
    /// When the request was made, by the device clock (ms): where the wait
    /// before asking again starts.
    pub asked_at: i64,
    /// This build's major, sent with the request.
    pub major: i64,
    /// What the request read this device as: a machine id keys the install.
    pub device_kind: Option<DeviceKind>,
    /// The claim of this device's own fingerprint, which a device refusal is
    /// held to.
    pub own_claim: Option<&'a str>,
}

/// What a `use` answer does.
#[derive(Debug, Clone, PartialEq)]
pub struct UseOutcome {
    /// What the answer comes to.
    pub kind: UseOutcomeKind,
    /// The refusal, when the server's verdict was one.
    pub refusal: Option<Refusal>,
    /// What the answer says is left, for `UsedUp`.
    pub left: Option<u64>,
    /// What is saved as the answer to the ask.
    pub patch: Patch,
    /// The figures saved (the newer by `at` is kept).
    pub figures: BTreeMap<String, MeterFigure>,
    /// The reports the answer confirms.
    pub confirmed: Vec<Confirmed>,
}

impl UseOutcome {
    /// An answer that saves and confirms nothing.
    fn nothing(kind: UseOutcomeKind, patch: Patch) -> UseOutcome {
        UseOutcome {
            kind,
            refusal: None,
            left: None,
            patch,
            figures: BTreeMap::new(),
            confirmed: Vec::new(),
        }
    }
}

/// The figures a `use` answer gives, each stamped with the server's time
/// (`at`); none for an answer without a whole `at`.
fn answered_figures(body: &Map<String, Value>) -> BTreeMap<String, MeterFigure> {
    let Some(at) = body
        .get("at")
        .and_then(figure_of)
        .and_then(|at| i64::try_from(at).ok())
    else {
        return BTreeMap::new();
    };
    body.get("meters")
        .map(meters_of)
        .unwrap_or_default()
        .into_iter()
        .map(|(code, meter)| {
            let figure = MeterFigure {
                left: meter.left,
                total: meter.total,
                offline: meter.offline,
                resets: meter.resets,
                at,
            };
            (code, figure)
        })
        .collect()
}

/// What a `use` answer saves and says. No answer at all: the wait
/// (`RETRY_UNANSWERED_MS`) and the device's kind. A 5xx or a 429, and any answer
/// that is neither a spend nor a recognised word: nothing, the offline path. A
/// 200 is a spend only with `spent: true`. On any other status, `used-up`,
/// `unknown-meter` and a validate's verdicts (`refusal_of`, stored as a
/// validate's are) are read; each saves the reports it confirms, and a verdict
/// its refusal.
pub fn use_outcome(input: &UseInput<'_>) -> UseOutcome {
    let keyed = keyed_by(input.device_kind);
    let Some(response) = input.response else {
        let wait = Patch {
            backoff_since: Some(Some(input.asked_at)),
            backoff_ms: Some(Some(RETRY_UNANSWERED_MS)),
            ..Patch::default()
        };
        return UseOutcome::nothing(UseOutcomeKind::Unanswered, wait.then(keyed));
    };
    if no_answer(response.status) {
        return UseOutcome::nothing(UseOutcomeKind::Unanswered, Patch::default());
    }
    let empty = Map::new();
    let body = response.json.as_ref().and_then(Value::as_object);
    let confirmed = spends_confirmed(Some(response));
    if response.status == 200 {
        if body.and_then(|body| body.get("spent")) != Some(&Value::Bool(true)) {
            return UseOutcome::nothing(UseOutcomeKind::Unanswered, Patch::default());
        }
        return UseOutcome {
            figures: answered_figures(body.unwrap_or(&empty)),
            confirmed,
            ..UseOutcome::nothing(UseOutcomeKind::Spent, Patch::default())
        };
    }
    let error = error_of(response.json.as_ref());
    if error == Some("used-up") {
        let body = body.unwrap_or(&empty);
        return UseOutcome {
            left: body.get("left").and_then(figure_of),
            figures: answered_figures(body),
            confirmed,
            ..UseOutcome::nothing(UseOutcomeKind::UsedUp, Patch::default())
        };
    }
    if error == Some("unknown-meter") {
        return UseOutcome {
            confirmed,
            ..UseOutcome::nothing(UseOutcomeKind::NoMeter, Patch::default())
        };
    }
    let Some(refused) = refusal_of(error) else {
        return UseOutcome::nothing(UseOutcomeKind::Unanswered, Patch::default());
    };
    let verdict = Patch {
        checked_major: Some(Some(input.major)),
        backoff_since: Some(Some(input.asked_at)),
        backoff_ms: Some(Some(ASK_AGAIN_MS)),
        ..Patch::default()
    }
    .then(keyed)
    .then(refusal_patch(refused, input.major, input.own_claim));
    UseOutcome {
        refusal: Some(refused),
        confirmed,
        ..UseOutcome::nothing(UseOutcomeKind::Refused, verdict)
    }
}
