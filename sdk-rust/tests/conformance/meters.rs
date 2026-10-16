//! The metered-uses groups, which close `conformance/vectors.json`: what every
//! KeyGrant SDK must answer for the same inputs, answered from each vector's own
//! JSON through the functions the SDK itself decides with, with the conventions
//! of the harness around this module.

use std::collections::BTreeSet;

use keygrant::journal::{journaled, JournalOp, SpendOp};
use keygrant::lease::{parse_json, verify_lease, KeySet, LeaseClaims, PublicKey};
use keygrant::meters::{
    asked_for, figures_of, meter_status_of, meters_json, meters_of, offline_spend, spend_json,
    spends_confirmed, spends_of, spends_reported, stored_figure_json, stored_figures, use_asks_now,
    use_outcome, Confirmed, OfflineVerdict, UseInput, UseOutcomeKind, MAX_METER_USES,
    MAX_PENDING_SPENDS, MAX_PENDING_VOIDS, PENDING_MAX_MS, USE_TIMEOUT,
};
use keygrant::offline::{check_lease, LeaseCheck, LeaseHolder};
use keygrant::renewal::{asks_on_valid_lease, ValidLeaseAsk};
use keygrant::SpendKind;
use serde_json::{json, Map, Value};

use super::{
    base_claims, doubles, each, encoded, int, kind, label, or_null, response, state, strings, text,
    vectors,
};

/// Every group this module answers, in the file's order.
pub(super) const GROUPS: [&str; 12] = [
    "meterClaims",
    "meterLeases",
    "useAmounts",
    "meterFigures",
    "meterStatuses",
    "offlineSpends",
    "useAsks",
    "syncAsks",
    "useOutcomes",
    "journals",
    "spendReports",
    "spendConfirmations",
];

/// How many vectors the groups hold, and how many of them no Rust call can
/// hold (two are JavaScript's only; two give an amount below zero, which a
/// `u64` cannot be).
const VECTORS: usize = 412;
const UNHOLDABLE: usize = 4;

#[test]
fn the_harness_answers_every_metered_group_in_the_vectors() {
    let v = vectors();
    for group in GROUPS {
        assert!(
            v.get(group).is_some_and(Value::is_array),
            "{group}: no such group"
        );
    }
    let total: usize = GROUPS
        .iter()
        .map(|g| v[*g].as_array().map_or(0, Vec::len))
        .sum();
    assert_eq!(total, VECTORS, "the vectors the harness answers");
}

/// The constants the metered-uses vectors state, as this crate holds them.
pub(super) fn constants() -> Value {
    let ms = |ms: u128| json!(u64::try_from(ms).expect("a time"));
    json!({
        "MAX_METER_USES": MAX_METER_USES,
        "MAX_PENDING_SPENDS": MAX_PENDING_SPENDS,
        "MAX_PENDING_VOIDS": MAX_PENDING_VOIDS,
        "PENDING_MAX_MS": PENDING_MAX_MS,
        "USE_TIMEOUT_MS": ms(USE_TIMEOUT.as_millis()),
    })
}

// --- the lease's met --------------------------------------------------------------------

#[test]
fn meters_are_read_from_a_leases_claim_as_the_reference_reads_them() {
    each(&vectors(), "meterClaims", |vector| {
        // The claim as the JSON text a lease carries it in; null is a lease with none.
        let read = vector["json"]
            .as_str()
            .map_or_else(Default::default, |claim| {
                meters_of(&parse_json(claim).expect("the claim is JSON"))
            });
        meters_json(&read)
    });
}

#[test]
fn leases_that_carry_meters_verify_and_check_as_the_reference() {
    let v = vectors();
    let key = KeySet::from(PublicKey::from_jwk(&v["publicJwk"]).expect("the vectors' key"));
    for vector in v["meterLeases"].as_array().expect("a group") {
        let name = label(vector);
        let verified = verify_lease(text(&vector["token"], "token"), &key, int(&vector["now"]));
        assert_eq!(
            doubles(&verified.to_json()),
            doubles(&vector["verify"]),
            "meterLeases: {name}: verify"
        );
        let holder = &vector["holder"];
        let claims = strings(&holder["claims"]);
        let check = check_lease(
            &verified,
            text(&vector["product"], "product"),
            int(&vector["major"]),
            &LeaseHolder {
                key: text(&holder["key"], "key"),
                activation_id: text(&holder["activationId"], "activationId"),
                claims: &claims,
                can_tell: holder["canTell"].as_bool().expect("canTell"),
            },
        );
        let answer = match check {
            LeaseCheck::Ok { test, .. } => json!({ "ok": true, "test": test }),
            LeaseCheck::Fail(failure) => json!({ "ok": false, "reason": failure.as_str() }),
        };
        assert_eq!(answer, vector["expect"], "meterLeases: {name}: expect");
    }
}

// --- what a call asks for, and the figures -------------------------------------------------

#[test]
fn what_a_call_asks_for_is_what_the_reference_reads() {
    let v = vectors();
    let (mut javascript, mut below_zero) = (0, 0);
    for vector in v["useAmounts"].as_array().expect("a group") {
        let name = label(vector);
        if vector["jsOnly"] == json!(true) {
            // A fraction or a text: no Rust call holds one.
            assert!(
                !vector["n"].is_u64() && !vector["n"].is_i64(),
                "useAmounts: {name} is marked jsOnly but is whole"
            );
            javascript += 1;
            continue;
        }
        // A call that gave no amount is a spend of 1.
        let n = if vector["n"].is_null() {
            Some(1)
        } else {
            u64::try_from(int(&vector["n"])).ok()
        };
        let Some(n) = n else {
            // An amount below zero: a `u64` cannot be one, so no call can ask it.
            assert_eq!(
                vector["expect"],
                json!({ "refused": "invalid-amount" }),
                "useAmounts: {name}"
            );
            below_zero += 1;
            continue;
        };
        let answer = match asked_for(text(&vector["meter"], "meter"), n) {
            Ok((meter, n)) => json!({ "meter": meter, "n": n }),
            Err(error) => json!({ "refused": error.as_str() }),
        };
        assert_eq!(answer, vector["expect"], "useAmounts: {name}");
    }
    assert_eq!(
        (javascript, below_zero),
        (UNHOLDABLE - 2, 2),
        "the useAmounts left out"
    );
}

#[test]
fn base_figures_are_the_references() {
    each(&vectors(), "meterFigures", |vector| {
        let stored = stored_figures(&vector["figures"]);
        let figures = figures_of(
            Some(&meters_of(&vector["met"])),
            int(&vector["iat"]),
            Some(&stored),
        );
        meters_json(&figures)
    });
}

#[test]
fn statuses_show_the_meters_as_the_reference_does() {
    each(&vectors(), "meterStatuses", |vector| {
        let shown = meter_status_of(
            &meters_of(&vector["figures"]),
            &spends_of(&vector["spends"]),
        );
        serde_json::to_value(shown).expect("serialisable")
    });
}

#[test]
fn a_spend_with_no_answer_is_allowed_as_the_reference_allows_it() {
    each(&vectors(), "offlineSpends", |vector| {
        let verdict = offline_spend(
            &meters_of(&vector["figures"]),
            &spends_of(&vector["spends"]),
            text(&vector["meter"], "meter"),
            u64::try_from(int(&vector["n"])).expect("an amount"),
        );
        match verdict {
            OfflineVerdict::Allowed { left, limit } => {
                json!({ "ok": true, "left": left, "limit": limit })
            }
            OfflineVerdict::NoMeter => json!({ "ok": false, "error": "no-meter" }),
            OfflineVerdict::OfflineLimit { left } => {
                json!({ "ok": false, "error": "offline-limit", "left": left })
            }
        }
    });
}

// --- when to ask -----------------------------------------------------------------------------

#[test]
fn a_call_asks_the_server_when_the_reference_does() {
    each(&vectors(), "useAsks", |vector| {
        json!(use_asks_now(
            &state(&vector["state"]),
            int(&vector["clockNow"]),
            int(&vector["now"]),
            vector["covered"].as_bool().expect("covered"),
        ))
    });
}

#[test]
fn a_valid_lease_asks_to_report_a_journal_when_the_reference_does() {
    each(&vectors(), "syncAsks", |vector| {
        let given = &vector["claims"];
        let claims = LeaseClaims {
            major: int(&given["major"]),
            iat: int(&given["iat"]),
            exp: int(&given["exp"]),
            ..base_claims()
        };
        json!(asks_on_valid_lease(&ValidLeaseAsk {
            claims: &claims,
            state: &state(&vector["state"]),
            major: int(&vector["major"]),
            now: int(&vector["now"]),
            clock_now: int(&vector["clockNow"]),
        }))
    });
}

// --- what an answer does -----------------------------------------------------------------------

fn confirmed_json(entries: &[Confirmed]) -> Value {
    Value::Array(
        entries
            .iter()
            .map(|entry| match entry {
                Confirmed::Count { id, n } => json!({ "id": id, "n": n }),
                Confirmed::Void { id } => json!({ "id": id, "void": true }),
            })
            .collect(),
    )
}

#[test]
fn a_use_answer_does_what_the_reference_does() {
    each(&vectors(), "useOutcomes", |vector| {
        let response = response(&vector["response"]);
        let device = &vector["device"];
        let outcome = use_outcome(&UseInput {
            response: response.as_ref(),
            asked_at: int(&vector["askedAt"]),
            major: int(&vector["major"]),
            device_kind: kind(&device["kind"]),
            own_claim: device["ownClaim"].as_str(),
        });
        let mut out = Map::new();
        out.insert("outcome".into(), json!(outcome.kind.as_str()));
        if let Some(refusal) = outcome.refusal {
            out.insert("refusal".into(), json!(refusal.as_str()));
        }
        if let Some(left) = outcome.left {
            out.insert("left".into(), json!(left));
        }
        out.insert("patch".into(), encoded(&outcome.patch));
        let figures = outcome
            .figures
            .iter()
            .map(|(code, figure)| (code.clone(), stored_figure_json(figure)))
            .collect();
        out.insert("figures".into(), Value::Object(figures));
        out.insert("confirmed".into(), confirmed_json(&outcome.confirmed));
        Value::Object(out)
    });
}

#[test]
fn every_kind_of_answer_has_a_vector() {
    let v = vectors();
    let mut seen = BTreeSet::new();
    for vector in v["useOutcomes"].as_array().expect("a group") {
        seen.insert(text(&vector["expect"]["outcome"], "outcome").to_owned());
    }
    let all: BTreeSet<String> = [
        UseOutcomeKind::Spent,
        UseOutcomeKind::UsedUp,
        UseOutcomeKind::NoMeter,
        UseOutcomeKind::Refused,
        UseOutcomeKind::Unanswered,
    ]
    .iter()
    .map(|kind| kind.as_str().to_owned())
    .collect();
    assert_eq!(seen, all);
}

// --- the journal ---------------------------------------------------------------------------------

fn op_of(op: &Value) -> JournalOp {
    let about = || text(&op["about"], "about").to_owned();
    match text(&op["kind"], "kind") {
        word @ ("offline" | "unanswered" | "void") => JournalOp::Spend(SpendOp {
            kind: SpendKind::parse(word).expect("a kind"),
            about: about(),
            id: text(&op["id"], "id").to_owned(),
            meter: text(&op["meter"], "meter").to_owned(),
            n: u64::try_from(int(&op["n"])).expect("a count"),
            at: int(&op["at"]),
            limit: op["limit"].as_u64(),
            told: op["told"] == json!(true),
        }),
        "confirmed" => JournalOp::Confirmed {
            about: about(),
            entries: op["entries"]
                .as_array()
                .expect("entries")
                .iter()
                .map(|entry| {
                    let id = text(&entry["id"], "id").to_owned();
                    if entry["void"] == json!(true) {
                        Confirmed::Void { id }
                    } else {
                        Confirmed::Count {
                            id,
                            n: entry["n"].as_u64().expect("a count"),
                        }
                    }
                })
                .collect(),
        },
        "figures" => JournalOp::Figures {
            about: about(),
            figures: stored_figures(&op["figures"]),
        },
        "activated" => JournalOp::Activated { about: about() },
        "deactivated" => JournalOp::Deactivated,
        other => panic!("unknown op {other:?}"),
    }
}

#[test]
fn the_journal_lands_as_the_reference_lands_it() {
    each(&vectors(), "journals", |vector| {
        let landed = journaled(
            &state(&vector["latest"]),
            &op_of(&vector["op"]),
            int(&vector["now"]),
        );
        let figures = landed
            .meter_figures
            .iter()
            .map(|(code, figure)| (code.clone(), stored_figure_json(figure)))
            .collect();
        json!({
            "spends": landed.spends.iter().map(spend_json).collect::<Vec<_>>(),
            "meterFigures": Value::Object(figures),
            "refused": landed.refused,
        })
    });
}

#[test]
fn a_report_carries_the_journal_as_the_reference_reports_it() {
    each(&vectors(), "spendReports", |vector| {
        or_null(spends_reported(&spends_of(&vector["spends"])))
    });
}

#[test]
fn an_answer_confirms_the_reports_the_reference_says_it_does() {
    each(&vectors(), "spendConfirmations", |vector| {
        confirmed_json(&spends_confirmed(response(&vector["response"]).as_ref()))
    });
}
