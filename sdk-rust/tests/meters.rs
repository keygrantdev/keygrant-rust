//! The decisions under `use_meter` and `status().meters`, by the rule each
//! states, straight through the functions the engine decides with. The
//! conformance vectors pin the same decisions for every port; these hold this
//! port to the contract itself, and cover what vectors cannot say (a lease
//! carrying a figure out of a double's range, the words and the JSON a result
//! has, what the stored state is read as).

mod common;

use std::collections::BTreeMap;

use common::*;
use keygrant::journal::{bounded_voids, compacted, journaled, land_journal, JournalOp, SpendOp};
use keygrant::lease::{parse_json, verify_lease, KeySet, PublicKey, VerifyFailure};
use keygrant::meters::{
    asked_for, figures_of, journal_asks, journal_waiting, meter_status_of, meters_of,
    offline_spend, spends_confirmed, spends_reported, use_asks_now, use_outcome, void_waiting,
    BaseFigure, Confirmed, OfflineVerdict, UseInput, UseOutcomeKind, MAX_METER_USES,
    MAX_PENDING_SPENDS, MAX_PENDING_VOIDS, PENDING_MAX_MS,
};
use keygrant::offline::{ASK_AGAIN_MS, RETRY_UNANSWERED_MS};
use keygrant::{
    DeviceKind, HttpResponse, MeterFigure, MeterStatus, Refusal, SpendEntry, SpendKind,
    StoredState, UseError, UseResult,
};
use serde_json::{json, Value};

const A: &str = "act_1";

fn figure(left: u64, total: u64, offline: u64, resets: bool) -> BaseFigure {
    BaseFigure {
        left,
        total,
        offline,
        resets,
    }
}

fn usual() -> BTreeMap<String, BaseFigure> {
    BTreeMap::from([("exports".to_owned(), figure(90, 100, 10, false))])
}

fn id(letter: &str) -> String {
    letter.repeat(22)
}

fn journal(spends: Vec<SpendEntry>) -> StoredState {
    StoredState {
        activation_id: Some(A.into()),
        spends: Some(spends).filter(|spends| !spends.is_empty()),
        ..StoredState::default()
    }
}

fn spend(kind: SpendKind, who: &str, n: u64) -> SpendOp {
    SpendOp {
        kind,
        about: A.into(),
        id: who.into(),
        meter: "exports".into(),
        n,
        at: NOW,
        limit: None,
        told: false,
    }
}

fn offline_op(n: u64) -> SpendOp {
    spend(SpendKind::Offline, &id("N"), n)
}

fn op(spend: SpendOp) -> JournalOp {
    JournalOp::Spend(spend)
}

fn response(status: u16, json: Value) -> HttpResponse {
    HttpResponse {
        status,
        json: Some(json).filter(|json| !json.is_null()),
    }
}

// --- what is asked ----------------------------------------------------------------------

#[test]
fn takes_a_whole_number_from_1_to_the_bound_and_judges_the_amount_before_the_code() {
    assert_eq!(asked_for("exports", 1), Ok(("exports", 1)));
    assert_eq!(
        asked_for("exports", MAX_METER_USES),
        Ok(("exports", MAX_METER_USES))
    );
    for n in [0, MAX_METER_USES + 1, u64::MAX, 1 << 53] {
        assert_eq!(asked_for("exports", n), Err(UseError::InvalidAmount), "{n}");
    }
    assert_eq!(asked_for("", 0), Err(UseError::InvalidAmount));
    assert_eq!(asked_for("", 1), Err(UseError::NoMeter));
}

#[test]
fn counts_a_code_in_utf16_units_32_are_a_code_33_are_not_a_surrogate_pair_is_two() {
    assert!(asked_for(&"x".repeat(32), 1).is_ok());
    assert_eq!(asked_for(&"x".repeat(33), 1), Err(UseError::NoMeter));
    assert!(asked_for(&format!("{}\u{1F600}", "x".repeat(30)), 1).is_ok());
    assert_eq!(
        asked_for(&format!("{}\u{1F600}", "x".repeat(31)), 1),
        Err(UseError::NoMeter)
    );
    // Characters beyond the BMP count two each.
    assert!(asked_for(&"\u{1F600}".repeat(16), 1).is_ok());
    assert_eq!(
        asked_for(&"\u{1F600}".repeat(17), 1),
        Err(UseError::NoMeter)
    );
}

// --- the base figures --------------------------------------------------------------------------

#[test]
fn base_figures_are_the_leases_when_nothing_is_stored_and_a_stored_figure_from_the_leases_own_second_on(
) {
    let met = BTreeMap::from([
        ("exports".to_owned(), figure(90, 100, 10, false)),
        ("renders".to_owned(), figure(5, 5, 0, true)),
    ]);
    let iat = 1_790_000_000;
    let at = iat * 1000;
    assert_eq!(figures_of(Some(&met), iat, None), met);
    let stored = |when: i64| {
        BTreeMap::from([(
            "exports".to_owned(),
            MeterFigure {
                left: 40,
                total: 100,
                offline: 10,
                resets: false,
                at: when,
            },
        )])
    };
    for (when, left) in [(at, 40), (at + 999, 40), (at - 1, 90)] {
        let figures = figures_of(Some(&met), iat, Some(&stored(when)));
        assert_eq!(figures["exports"].left, left, "{when}");
    }
}

#[test]
fn a_stored_figure_replaces_the_leases_whole_member() {
    let met = BTreeMap::from([("exports".to_owned(), figure(90, 100, 10, false))]);
    let stored = BTreeMap::from([(
        "exports".to_owned(),
        MeterFigure {
            left: 7,
            total: 50,
            offline: 3,
            resets: true,
            at: 2_000_000_000_000,
        },
    )]);
    assert_eq!(
        figures_of(Some(&met), 1, Some(&stored))["exports"],
        figure(7, 50, 3, true)
    );
}

#[test]
fn names_a_meter_only_a_newer_stored_figure_names_and_drops_one_the_newest_lease_no_longer_names() {
    let met = BTreeMap::from([("exports".to_owned(), figure(90, 100, 10, false))]);
    let iat = 1_790_000_000;
    let mut stored = BTreeMap::new();
    stored.insert(
        "added".to_owned(),
        MeterFigure {
            left: 7,
            total: 7,
            offline: 1,
            resets: false,
            at: iat * 1000 + 1,
        },
    );
    stored.insert(
        "gone".to_owned(),
        MeterFigure {
            left: 7,
            total: 7,
            offline: 1,
            resets: false,
            at: iat * 1000 - 1,
        },
    );
    let names: Vec<String> = figures_of(Some(&met), iat, Some(&stored))
        .into_keys()
        .collect();
    assert_eq!(names, ["added", "exports"]);
    assert!(figures_of(None, iat, None).is_empty());
}

// --- the status's meters -----------------------------------------------------------------------------

#[test]
fn the_status_takes_the_unsynced_spends_off_left_counts_no_void_and_never_goes_below_0() {
    let figures = BTreeMap::from([
        ("exports".to_owned(), figure(90, 100, 10, false)),
        ("renders".to_owned(), figure(5, 5, 0, true)),
    ]);
    let spends = vec![
        entry(SpendKind::Offline, 4, NOW),
        entry_of(&id("B"), SpendKind::Unanswered, 3, NOW),
        entry_of(&id("C"), SpendKind::Void, 50, NOW),
        SpendEntry {
            meter: "renders".into(),
            ..entry_of(&id("D"), SpendKind::Offline, 9, NOW)
        },
    ];
    assert_eq!(
        meter_status_of(&figures, &spends),
        BTreeMap::from([
            (
                "exports".to_owned(),
                MeterStatus {
                    left: 83,
                    total: 100,
                    offline: 10,
                    unsynced: 7,
                    resets: false
                }
            ),
            (
                "renders".to_owned(),
                MeterStatus {
                    left: 0,
                    total: 5,
                    offline: 0,
                    unsynced: 9,
                    resets: true
                }
            ),
        ])
    );
}

// --- a spend with no answer --------------------------------------------------------------------------------

#[test]
fn a_spend_is_allowed_up_to_the_allowance_and_up_to_what_is_left_and_answers_what_is_left_after_it()
{
    let allowed = |left, limit| OfflineVerdict::Allowed { left, limit };
    assert_eq!(offline_spend(&usual(), &[], "exports", 10), allowed(80, 10));
    assert_eq!(
        offline_spend(&usual(), &[entry(SpendKind::Offline, 4, NOW)], "exports", 6),
        allowed(80, 10)
    );
    let four = BTreeMap::from([("exports".to_owned(), figure(4, 100, 10, false))]);
    assert_eq!(offline_spend(&four, &[], "exports", 4), allowed(0, 4));
}

#[test]
fn a_spend_is_refused_past_either_with_what_is_left_and_always_for_an_allowance_of_0() {
    let refused = |left| OfflineVerdict::OfflineLimit { left };
    assert_eq!(offline_spend(&usual(), &[], "exports", 11), refused(90));
    let four = BTreeMap::from([("exports".to_owned(), figure(4, 100, 10, false))]);
    assert_eq!(offline_spend(&four, &[], "exports", 5), refused(4));
    let none = BTreeMap::from([("exports".to_owned(), figure(90, 100, 0, false))]);
    assert_eq!(offline_spend(&none, &[], "exports", 1), refused(90));
    let low = BTreeMap::from([("exports".to_owned(), figure(5, 100, 50, false))]);
    assert_eq!(
        offline_spend(&low, &[entry(SpendKind::Offline, 12, NOW)], "exports", 1),
        refused(0)
    );
}

#[test]
fn counts_the_unsynced_of_its_own_meter_and_of_both_kinds_never_a_void_and_knows_no_meter_without_a_figure(
) {
    let spends = vec![
        entry(SpendKind::Offline, 4, NOW),
        entry_of(&id("B"), SpendKind::Unanswered, 3, NOW),
        entry_of(&id("C"), SpendKind::Void, 50, NOW),
        SpendEntry {
            meter: "renders".into(),
            ..entry_of(&id("D"), SpendKind::Offline, 9, NOW)
        },
    ];
    assert_eq!(
        offline_spend(&usual(), &spends, "exports", 3),
        OfflineVerdict::Allowed {
            left: 80,
            limit: 10
        }
    );
    assert_eq!(
        offline_spend(&usual(), &spends, "exports", 4),
        OfflineVerdict::OfflineLimit { left: 83 }
    );
    assert_eq!(
        offline_spend(&usual(), &[], "credits", 1),
        OfflineVerdict::NoMeter
    );
    assert_eq!(
        offline_spend(&usual(), &[], "constructor", 1),
        OfflineVerdict::NoMeter
    );
}

// --- when use_meter asks ----------------------------------------------------------------------------------------

fn unanswered(k: usize, ago: i64) -> Vec<SpendEntry> {
    (0..k)
        .map(|i| {
            let at = NOW - if i == k - 1 { ago } else { ago + HOUR };
            entry_of(&format!("{i:k>22}"), SpendKind::Unanswered, 1, at)
        })
        .collect()
}

fn asks(state: StoredState, clock_now: i64, now: i64, covered: bool) -> bool {
    use_asks_now(&state, clock_now, now, covered)
}

fn waiting_state(since: i64, ms: i64) -> StoredState {
    StoredState {
        backoff_since: Some(since),
        backoff_ms: Some(ms),
        ..StoredState::default()
    }
}

#[test]
fn asks_every_time_but_while_a_no_answer_wait_runs_on_the_device_clock() {
    assert!(asks(StoredState::default(), NOW, NOW, true));
    assert!(!asks(
        waiting_state(NOW - 1000, RETRY_UNANSWERED_MS),
        NOW,
        NOW,
        true
    ));
    assert!(asks(
        waiting_state(NOW - RETRY_UNANSWERED_MS, RETRY_UNANSWERED_MS),
        NOW,
        NOW,
        true
    ));
    assert!(asks(
        waiting_state(NOW + 1000, RETRY_UNANSWERED_MS),
        NOW,
        NOW,
        true
    ));
    assert!(asks(
        waiting_state(NOW - 1000, ASK_AGAIN_MS),
        NOW,
        NOW,
        true
    ));
}

#[test]
fn waits_a_minute_after_one_unanswered_spend_doubling_with_each_to_an_hour_from_the_seventh_on() {
    for (i, minutes) in [1, 2, 4, 8, 16, 32, 60, 60, 60].into_iter().enumerate() {
        let k = i + 1;
        assert!(
            journal_waiting(&unanswered(k, minutes * MINUTE - 1), NOW),
            "{k} unanswered"
        );
        assert!(
            !journal_waiting(&unanswered(k, minutes * MINUTE), NOW),
            "{k} unanswered, over"
        );
    }
    assert!(!journal_waiting(&unanswered(64, HOUR), NOW));
    // The cap is applied first: a journal of any length waits the hour and overflows nothing.
    assert!(journal_waiting(
        &unanswered(MAX_PENDING_SPENDS, HOUR - 1),
        NOW
    ));
    let many: Vec<SpendEntry> = (0..10_000)
        .map(|i| {
            entry_of(
                &format!("{i:k>22}"),
                SpendKind::Unanswered,
                1,
                NOW - HOUR + 1,
            )
        })
        .collect();
    assert!(journal_waiting(&many, NOW));
}

#[test]
fn counts_a_void_toward_the_wait_an_offline_spend_not_and_goes_by_the_newest_entry_whatever_the_order(
) {
    assert!(journal_waiting(
        &[entry(SpendKind::Void, 1, NOW - 1000)],
        NOW
    ));
    assert!(!journal_waiting(&[entry(SpendKind::Offline, 1, NOW)], NOW));
    let older_last = [
        entry_of(&id("a"), SpendKind::Unanswered, 1, NOW - 1000),
        entry_of(&id("b"), SpendKind::Unanswered, 1, NOW - 3 * HOUR),
    ];
    assert!(journal_waiting(&older_last, NOW));
}

#[test]
fn judges_the_journals_wait_by_the_guarded_clock_and_the_other_by_the_device_clock() {
    let ten_days = 10 * 86_400_000;
    let spends = unanswered(1, 2 * MINUTE);
    let with_spends = StoredState {
        spends: Some(spends.clone()),
        ..StoredState::default()
    };
    assert!(asks(with_spends, NOW - ten_days, NOW, true));
    let both = StoredState {
        spends: Some(spends),
        backoff_since: Some(NOW - ten_days - 1000),
        backoff_ms: Some(RETRY_UNANSWERED_MS),
        ..StoredState::default()
    };
    assert!(!asks(both, NOW - ten_days, NOW, true));
}

#[test]
fn asks_whatever_the_waits_a_spend_the_allowance_would_refuse_and_one_it_covers_only_when_no_wait_runs(
) {
    let waiting = StoredState {
        backoff_since: Some(NOW - 1000),
        backoff_ms: Some(RETRY_UNANSWERED_MS),
        spends: Some(unanswered(1, 1000)),
        ..StoredState::default()
    };
    assert!(!asks(waiting.clone(), NOW, NOW, true));
    assert!(asks(waiting, NOW, NOW, false));
    assert!(asks(StoredState::default(), NOW, NOW, false));
}

#[test]
fn gives_the_uncovered_spend_a_wait_of_its_own_a_void_opened_less_than_a_minute_ago_none_ahead_of_either_clock(
) {
    let voided = |at| vec![entry(SpendKind::Void, 1, at)];
    assert!(void_waiting(
        &voided(NOW - RETRY_UNANSWERED_MS + 1),
        NOW,
        NOW
    ));
    assert!(!void_waiting(&voided(NOW - RETRY_UNANSWERED_MS), NOW, NOW));
    assert!(!void_waiting(&voided(NOW + 1), NOW, NOW));
    let others = [
        entry(SpendKind::Unanswered, 1, NOW),
        entry(SpendKind::Offline, 1, NOW),
    ];
    assert!(!void_waiting(&others, NOW, NOW));
    let waiting = StoredState {
        spends: Some(voided(NOW - 1000)),
        ..waiting_state(NOW - 1000, RETRY_UNANSWERED_MS)
    };
    assert!(!asks(waiting, NOW, NOW, false));
    let unanswered_only = StoredState {
        spends: Some(unanswered(1, 1000)),
        ..StoredState::default()
    };
    assert!(asks(unanswered_only, NOW, NOW, false));
}

#[test]
fn needs_the_device_clock_at_or_after_the_void_too() {
    let voided = [entry(SpendKind::Void, 1, NOW - 1000)];
    assert!(void_waiting(&voided, NOW, NOW - 1000));
    assert!(!void_waiting(&voided, NOW, NOW - 1001));
    assert!(!void_waiting(&voided, NOW, NOW - 10 * 86_400_000));
    // A device clock far ahead does not stretch the wait: the guarded clock measures the minute.
    assert!(void_waiting(&voided, NOW, NOW + 10 * 86_400_000));
    assert!(!void_waiting(
        &voided,
        NOW + RETRY_UNANSWERED_MS,
        NOW + 10 * 86_400_000
    ));
    // One void at the device clock is enough, whatever a newer void ahead of it says.
    let two = [
        entry_of(&id("a"), SpendKind::Void, 1, NOW - 30_000),
        entry_of(&id("b"), SpendKind::Void, 1, NOW - 1000),
    ];
    assert!(void_waiting(&two, NOW, NOW - 20_000));
    assert!(!void_waiting(&two, NOW, NOW - 40_000));
}

#[test]
fn leaves_out_of_a_wait_only_the_entries_ahead_of_the_clock_not_their_count_not_their_time() {
    let behind = |who: &str, at| entry_of(&id(who), SpendKind::Unanswered, 1, at);
    let ahead = behind("z", NOW + 86_400_000);
    assert!(journal_waiting(
        &[behind("a", NOW - 30_000), ahead.clone()],
        NOW
    ));
    assert!(!journal_waiting(
        &[behind("a", NOW - 2 * MINUTE), ahead.clone()],
        NOW
    ));
    // Two behind wait two minutes; counted with the one ahead they would wait four.
    assert!(!journal_waiting(
        &[
            behind("a", NOW - 2 * MINUTE),
            behind("b", NOW - 3 * HOUR),
            ahead.clone()
        ],
        NOW
    ));
    assert!(journal_waiting(
        &[
            behind("a", NOW - 2 * MINUTE + 1),
            behind("b", NOW - 3 * HOUR),
            ahead
        ],
        NOW
    ));
}

#[test]
fn has_no_wait_to_measure_from_an_entry_opened_after_the_guarded_clock_and_waits_from_one_opened_at_it(
) {
    assert!(journal_waiting(
        &[entry(SpendKind::Unanswered, 1, NOW)],
        NOW
    ));
    assert!(!journal_waiting(
        &[entry(SpendKind::Unanswered, 1, NOW + 1)],
        NOW
    ));
    assert!(!journal_waiting(
        &[entry(SpendKind::Void, 1, NOW + 10 * 86_400_000)],
        NOW
    ));
    assert!(journal_asks(
        &[entry(SpendKind::Unanswered, 1, NOW + 1)],
        NOW
    ));
}

#[test]
fn has_status_offer_a_journal_that_holds_an_entry_while_its_wait_is_not_running() {
    assert!(!journal_asks(&[], NOW));
    assert!(journal_asks(&[entry(SpendKind::Offline, 1, NOW)], NOW));
    assert!(!journal_asks(&unanswered(1, 30_000), NOW));
    assert!(journal_asks(&unanswered(1, MINUTE), NOW));
}

// --- what a use answer does ----------------------------------------------------------------------------------------

fn outcome_of(response: Option<&HttpResponse>) -> keygrant::meters::UseOutcome {
    use_outcome(&UseInput {
        response,
        asked_at: NOW,
        major: 3,
        device_kind: Some(DeviceKind::Machine),
        own_claim: Some("c"),
    })
}

#[test]
fn a_200_with_spent_true_is_a_spend_with_the_figures_stamped_by_the_servers_time() {
    let meters = json!({ "exports": { "left": 89, "total": 100, "offline": 10 } });
    let answer = response(
        200,
        json!({ "spent": true, "meters": meters, "at": NOW + 1, "spends": [{ "id": id("A"), "n": 2 }] }),
    );
    let outcome = outcome_of(Some(&answer));
    assert_eq!(outcome.kind, UseOutcomeKind::Spent);
    assert!(outcome.patch.is_empty());
    assert_eq!(
        outcome.figures,
        BTreeMap::from([(
            "exports".to_owned(),
            MeterFigure {
                left: 89,
                total: 100,
                offline: 10,
                resets: false,
                at: NOW + 1
            }
        )])
    );
    assert_eq!(outcome.confirmed, [Confirmed::Count { id: id("A"), n: 2 }]);
    let fractional = response(200, json!({ "spent": true, "meters": meters, "at": 1.5 }));
    assert!(outcome_of(Some(&fractional)).figures.is_empty());
    let text = response(200, json!({ "spent": "true", "meters": meters, "at": NOW }));
    assert_eq!(outcome_of(Some(&text)).kind, UseOutcomeKind::Unanswered);
}

#[test]
fn used_up_carries_what_the_answer_says_is_left_on_any_status_but_a_200_a_5xx_and_a_429() {
    let used_up = |status, left: Value| {
        outcome_of(Some(&response(
            status,
            json!({ "error": "used-up", "left": left }),
        )))
    };
    assert_eq!(
        (used_up(409, json!(3)).kind, used_up(409, json!(3)).left),
        (UseOutcomeKind::UsedUp, Some(3))
    );
    assert_eq!(used_up(400, json!(0)).left, Some(0));
    assert_eq!(used_up(409, json!(-1)).left, None);
    assert_eq!(used_up(409, json!(1.5)).left, None);
    assert_eq!(used_up(503, json!(3)).kind, UseOutcomeKind::Unanswered);
    assert_eq!(used_up(429, json!(3)).kind, UseOutcomeKind::Unanswered);
}

#[test]
fn stores_a_verdict_as_a_validate_does_and_a_missing_answer_as_the_wait_of_an_unanswered_request() {
    let revoked = response(
        403,
        json!({ "error": "revoked", "spends": [{ "id": id("A"), "n": 1 }] }),
    );
    let outcome = outcome_of(Some(&revoked));
    assert_eq!(
        (outcome.kind, outcome.refusal),
        (UseOutcomeKind::Refused, Some(Refusal::Revoked))
    );
    assert_eq!(outcome.confirmed, [Confirmed::Count { id: id("A"), n: 1 }]);
    let patch = outcome.patch.encoded();
    assert_eq!(patch["checkedMajor"], 3);
    assert_eq!(
        (patch["backoffSince"].clone(), patch["backoffMs"].clone()),
        (json!(NOW), json!(ASK_AGAIN_MS))
    );
    assert_eq!(
        (patch["deviceKind"].clone(), patch["refused"].clone()),
        (json!("machine"), json!("revoked"))
    );
    let none = outcome_of(None);
    assert_eq!(
        Value::Object(none.patch.encoded()),
        json!({ "backoffSince": NOW, "backoffMs": RETRY_UNANSWERED_MS, "deviceKind": "machine" })
    );
    assert!(outcome_of(Some(&response(500, Value::Null)))
        .patch
        .is_empty());
}

// --- the reports ---------------------------------------------------------------------------------------------------

#[test]
fn reports_are_the_journal_oldest_first_with_a_void_marked_none_for_none_and_at_most_64() {
    assert_eq!(spends_reported(&[]), None);
    let report = spends_reported(&[
        entry(SpendKind::Void, 1, NOW),
        entry_of(&id("B"), SpendKind::Unanswered, 1, NOW),
    ]);
    assert_eq!(
        report,
        Some(json!([
            { "id": "AAAAAAAAAAAAAAAAAAAAAA", "meter": "exports", "n": 1, "at": NOW, "void": true },
            { "id": id("B"), "meter": "exports", "n": 1, "at": NOW },
        ]))
    );
    let many: Vec<SpendEntry> = (0..MAX_PENDING_SPENDS + 3)
        .map(|i| entry_of(&format!("{i:k>22}"), SpendKind::Offline, 1, NOW))
        .collect();
    assert_eq!(
        spends_reported(&many).and_then(|r| r.as_array().map(Vec::len)),
        Some(MAX_PENDING_SPENDS)
    );
}

#[test]
fn reports_are_confirmed_by_an_answer_that_is_not_a_5xx_a_429_or_none_read_leniently() {
    let body = json!({ "spends": [
        { "id": "a", "n": 2 }, { "id": "b", "void": true }, { "id": "c", "n": 0 }, { "n": 1 }, "x",
        { "id": "d", "n": 1.5 }, { "id": "e", "void": "true" }, { "id": "f", "n": 9007199254740992u64 },
    ] });
    let named = [
        Confirmed::Count {
            id: "a".into(),
            n: 2,
        },
        Confirmed::Void { id: "b".into() },
    ];
    assert_eq!(spends_confirmed(Some(&response(200, body.clone()))), named);
    assert_eq!(
        spends_confirmed(Some(&response(403, body.clone()))).len(),
        2
    );
    for status in [500, 503, 429] {
        assert!(spends_confirmed(Some(&response(status, body.clone()))).is_empty());
    }
    assert!(spends_confirmed(None).is_empty());
    assert!(spends_confirmed(Some(&response(200, json!({ "spends": "x" })))).is_empty());
}

// --- the journal's landing -----------------------------------------------------------------------------------------

#[test]
fn grows_one_offline_entry_for_each_meter_and_opens_the_others_once_by_id() {
    let grown = journaled(
        &journal(vec![entry(SpendKind::Offline, 3, NOW)]),
        &op(spend(SpendKind::Offline, &id("X"), 2)),
        NOW,
    );
    assert_eq!(grown.spends, [entry(SpendKind::Offline, 5, NOW)]);
    let held = journal(vec![entry(SpendKind::Unanswered, 1, NOW)]);
    let again = journaled(
        &held,
        &op(spend(SpendKind::Unanswered, "AAAAAAAAAAAAAAAAAAAAAA", 9)),
        NOW,
    );
    assert_eq!(again.spends, [entry(SpendKind::Unanswered, 1, NOW)]);
    // An offline entry is the one entry of its meter; another meter opens its own.
    let other = SpendOp {
        meter: "renders".into(),
        ..offline_op(1)
    };
    let two = journaled(
        &journal(vec![entry(SpendKind::Offline, 3, NOW)]),
        &op(other),
        NOW,
    );
    assert_eq!(two.spends.len(), 2);
}

#[test]
fn an_id_already_in_the_journal_is_left_as_it_is_an_offline_entrys_included() {
    let held = journal(vec![entry_of(&id("B"), SpendKind::Offline, 4, NOW)]);
    for kind in [SpendKind::Unanswered, SpendKind::Void] {
        let landed = journaled(&held, &op(spend(kind, &id("B"), 9)), NOW);
        assert_eq!(
            (landed.refused, landed.spends.clone()),
            (false, held.spends.clone().expect("a journal")),
            "{kind:?}"
        );
    }
    // Before its limit is judged.
    let limited = SpendOp {
        limit: Some(1),
        ..spend(SpendKind::Unanswered, &id("B"), 9)
    };
    let landed = journaled(&held, &op(limited), NOW);
    assert_eq!(
        (landed.refused, landed.spends),
        (false, held.spends.expect("a journal"))
    );
}

#[test]
fn refuses_a_spend_that_would_take_its_meters_unsynced_past_its_limit_and_holds_a_void_to_none() {
    let held = journal(vec![
        entry(SpendKind::Offline, 3, NOW),
        entry_of(&id("B"), SpendKind::Unanswered, 2, NOW),
        entry_of(&id("C"), SpendKind::Void, 50, NOW),
        SpendEntry {
            meter: "renders".into(),
            ..entry_of(&id("D"), SpendKind::Offline, 9, NOW)
        },
    ]);
    let limited = |n| SpendOp {
        limit: Some(10),
        ..offline_op(n)
    };
    assert!(!journaled(&held, &op(limited(5)), NOW).refused);
    let refused = journaled(&held, &op(limited(6)), NOW);
    assert_eq!(
        (refused.refused, refused.spends),
        (true, held.spends.clone().expect("a journal"))
    );
    let void = SpendOp {
        limit: Some(0),
        ..spend(SpendKind::Void, &id("V"), 6)
    };
    assert!(!journaled(&held, &op(void), NOW).refused);
}

#[test]
fn lands_a_spend_the_customer_was_told_was_made_past_its_limit_and_the_cap_and_never_refuses_it() {
    let full: Vec<SpendEntry> = (0..MAX_PENDING_SPENDS)
        .map(|i| SpendEntry {
            meter: format!("m{i}"),
            ..entry_of(&format!("{i:k>22}"), SpendKind::Unanswered, 1, NOW)
        })
        .collect();
    let told = SpendOp {
        told: true,
        ..offline_op(1)
    };
    let landed = journaled(&journal(full), &op(told), NOW);
    assert!(!landed.refused);
    assert_eq!(landed.spends.len(), MAX_PENDING_SPENDS + 1);
    let past_limit = SpendOp {
        limit: Some(5),
        told: true,
        ..offline_op(9)
    };
    assert_eq!(
        journaled(
            &journal(vec![entry(SpendKind::Offline, 3, NOW)]),
            &op(past_limit),
            NOW
        )
        .spends[0]
            .n,
        12
    );
}

#[test]
fn keeps_the_cap_of_voids_a_new_one_taking_the_place_of_the_oldest_and_never_refuses_for_it() {
    let voids: Vec<SpendEntry> = (0..MAX_PENDING_VOIDS as i64)
        .map(|i| entry_of(&format!("{i:v>22}"), SpendKind::Void, 1, NOW - 1000 + i))
        .collect();
    let next = spend(SpendKind::Void, &id("N"), 1);
    let fifteen = journaled(&journal(voids[1..].to_vec()), &op(next.clone()), NOW);
    assert_eq!(fifteen.spends.len(), MAX_PENDING_VOIDS);
    let replaced = journaled(&journal(voids.clone()), &op(next.clone()), NOW);
    assert!(!replaced.refused);
    let mut expected: Vec<String> = voids[1..].iter().map(|e| e.id.clone()).collect();
    expected.push(id("N"));
    assert_eq!(
        replaced
            .spends
            .iter()
            .map(|e| e.id.clone())
            .collect::<Vec<_>>(),
        expected
    );
    // A void in a full journal that holds fewer voids than that is refused, as any spend is.
    let crowd: Vec<SpendEntry> = (0..MAX_PENDING_SPENDS - 1)
        .map(|i| SpendEntry {
            meter: format!("m{i}"),
            ..entry_of(&format!("{i:k>22}"), SpendKind::Unanswered, 1, NOW)
        })
        .collect();
    let mut crowded = vec![entry_of(&id("V"), SpendKind::Void, 1, NOW)];
    crowded.extend(crowd);
    assert!(journaled(&journal(crowded.clone()), &op(next.clone()), NOW).refused);
    // A void marked told is held to all of it (a void is never told); an unanswered spend is held to no void cap.
    let told = SpendOp {
        told: true,
        ..next.clone()
    };
    assert_eq!(
        journaled(&journal(voids.clone()), &op(told.clone()), NOW).spends,
        replaced.spends
    );
    assert!(journaled(&journal(crowded), &op(told), NOW).refused);
    let unanswered = SpendOp {
        kind: SpendKind::Unanswered,
        ..next
    };
    assert_eq!(
        journaled(&journal(voids), &op(unanswered), NOW)
            .spends
            .len(),
        MAX_PENDING_VOIDS + 1
    );
}

#[test]
fn refuses_an_append_past_64_entries_and_lets_an_open_entry_grow() {
    let full: Vec<SpendEntry> = (0..MAX_PENDING_SPENDS)
        .map(|i| SpendEntry {
            meter: format!("m{i}"),
            ..entry_of(&format!("{i:k>22}"), SpendKind::Unanswered, 1, NOW)
        })
        .collect();
    assert!(journaled(&journal(full.clone()), &op(offline_op(1)), NOW).refused);
    let mut open = vec![entry(SpendKind::Offline, 3, NOW)];
    open.extend(full[1..].iter().cloned());
    let grown = journaled(&journal(open), &op(offline_op(1)), NOW);
    assert!(!grown.refused);
    assert_eq!(grown.spends[0].n, 4);
}

fn confirm(spends: Vec<SpendEntry>, entries: Vec<Confirmed>) -> Vec<SpendEntry> {
    let confirmed = JournalOp::Confirmed {
        about: A.into(),
        entries,
    };
    journaled(&journal(spends), &confirmed, NOW).spends
}

#[test]
fn drops_an_entry_by_the_count_the_server_recorded_a_void_by_a_void_and_keeps_what_grew_meanwhile()
{
    let held = entry(SpendKind::Offline, 3, NOW);
    let who = held.id.clone();
    let count = |n| vec![Confirmed::Count { id: who.clone(), n }];
    assert!(confirm(vec![held.clone()], count(3)).is_empty());
    assert!(confirm(vec![held.clone()], count(9)).is_empty());
    let grown = entry(SpendKind::Offline, 5, NOW);
    assert_eq!(confirm(vec![grown.clone()], count(3)), [grown]);
    let void = entry(SpendKind::Void, 3, NOW);
    assert!(confirm(
        vec![void.clone()],
        vec![Confirmed::Void { id: who.clone() }]
    )
    .is_empty());
    assert_eq!(confirm(vec![void.clone()], count(1)), [void]);
    let unanswered = entry(SpendKind::Unanswered, 3, NOW);
    assert_eq!(
        confirm(vec![unanswered.clone()], vec![Confirmed::Void { id: who }]),
        [unanswered]
    );
}

#[test]
fn keeps_the_newer_figure_of_each_meter_and_nothing_about_another_activation() {
    let stored = BTreeMap::from([(
        "exports".to_owned(),
        MeterFigure {
            left: 70,
            total: 100,
            offline: 10,
            resets: false,
            at: NOW,
        },
    )]);
    let given = |at| {
        let figures = BTreeMap::from([(
            "exports".to_owned(),
            MeterFigure {
                left: 50,
                total: 100,
                offline: 10,
                resets: false,
                at,
            },
        )]);
        JournalOp::Figures {
            about: A.into(),
            figures,
        }
    };
    let held = |activation: Option<&str>| StoredState {
        activation_id: activation.map(str::to_owned),
        meter_figures: Some(stored.clone()),
        ..StoredState::default()
    };
    let left =
        |state: &StoredState, at| journaled(state, &given(at), NOW).meter_figures["exports"].left;
    assert_eq!(left(&held(Some(A)), NOW + 1), 50);
    assert_eq!(left(&held(Some(A)), NOW), 70);
    assert_eq!(left(&held(Some(A)), NOW - 1), 70);
    assert_eq!(left(&held(Some("act_2")), NOW + 1), 70);
    assert_eq!(left(&held(None), NOW + 1), 70);
}

#[test]
fn clears_both_for_another_seat_or_a_deactivation_and_keeps_them_for_the_same_seat() {
    let figures = BTreeMap::from([(
        "exports".to_owned(),
        MeterFigure {
            left: 70,
            total: 100,
            offline: 10,
            resets: false,
            at: NOW,
        },
    )]);
    let held = StoredState {
        spends: Some(vec![entry(SpendKind::Offline, 1, NOW)]),
        meter_figures: Some(figures),
        ..journal(vec![entry(SpendKind::Offline, 1, NOW)])
    };
    let other = journaled(
        &held,
        &JournalOp::Activated {
            about: "act_2".into(),
        },
        NOW,
    );
    assert!(other.spends.is_empty() && other.meter_figures.is_empty());
    let same = journaled(&held, &JournalOp::Activated { about: A.into() }, NOW);
    assert_eq!((same.spends.len(), same.meter_figures.len()), (1, 1));
    let none = journaled(
        &StoredState::default(),
        &JournalOp::Activated { about: A.into() },
        NOW,
    );
    assert!(none.spends.is_empty());
    let gone = journaled(&held, &JournalOp::Deactivated, NOW);
    assert!(gone.spends.is_empty() && gone.meter_figures.is_empty());
}

#[test]
fn drops_entries_opened_more_than_180_days_ago_on_any_op_that_lands_and_keeps_those_exactly_that_old(
) {
    let old = entry_of(&id("O"), SpendKind::Unanswered, 1, NOW - PENDING_MAX_MS - 1);
    let edge = entry_of(&id("E"), SpendKind::Unanswered, 1, NOW - PENDING_MAX_MS);
    let landed = journaled(&journal(vec![old.clone(), edge]), &op(offline_op(1)), NOW);
    assert_eq!(
        landed
            .spends
            .iter()
            .map(|e| e.id.clone())
            .collect::<Vec<_>>(),
        [id("E"), id("N")]
    );
    let confirmed = JournalOp::Confirmed {
        about: A.into(),
        entries: Vec::new(),
    };
    assert!(journaled(&journal(vec![old.clone()]), &confirmed, NOW)
        .spends
        .is_empty());
    // An op about another activation, or none, changes nothing: the horizon included.
    let about_other = SpendOp {
        about: "act_2".into(),
        ..offline_op(1)
    };
    assert_eq!(
        journaled(&journal(vec![old.clone()]), &op(about_other), NOW).spends,
        [old]
    );
}

#[test]
fn lands_ops_in_order_on_a_state_and_is_that_state_itself_with_none() {
    let state = journal(Vec::new());
    assert_eq!(land_journal(&state, &[], NOW).state, state);
    let confirm_all = JournalOp::Confirmed {
        about: A.into(),
        entries: vec![Confirmed::Count { id: id("N"), n: 2 }],
    };
    let landed = land_journal(&state, &[op(offline_op(2)), confirm_all], NOW);
    assert_eq!(landed.state.spends, None);
    assert_eq!(landed.refused, [false, false]);
}

// --- the changes a process keeps -----------------------------------------------------------------------------------

#[test]
fn folds_offline_spends_of_one_meter_figures_and_confirmations_and_nothing_across_another_kind_of_change(
) {
    // Each spend is made a second after the one before, so the entry a fold keeps can be told by its time.
    let offline_of = |n: u64, meter: &str| {
        op(SpendOp {
            meter: meter.into(),
            id: format!("{n:q>22}"),
            at: NOW + 1000 * n as i64,
            ..offline_op(n)
        })
    };
    let folded = compacted(&[
        offline_of(1, "exports"),
        offline_of(2, "exports"),
        offline_of(3, "exports"),
    ]);
    assert_eq!(
        folded,
        [op(SpendOp {
            id: format!("{:q>22}", 1),
            at: NOW + 1000,
            ..offline_op(6)
        })]
    );
    assert_eq!(
        compacted(&[
            offline_of(1, "exports"),
            offline_of(2, "renders"),
            offline_of(3, "exports")
        ])
        .len(),
        3
    );
    let figure_op = |left, at| {
        let figures = BTreeMap::from([(
            "exports".to_owned(),
            MeterFigure {
                left,
                total: 100,
                offline: 10,
                resets: false,
                at,
            },
        )]);
        JournalOp::Figures {
            about: A.into(),
            figures,
        }
    };
    assert_eq!(
        compacted(&[figure_op(70, NOW), figure_op(50, NOW + 1)]),
        [figure_op(50, NOW + 1)]
    );
    assert_eq!(
        compacted(&[figure_op(50, NOW + 1), figure_op(70, NOW)]),
        [figure_op(50, NOW + 1)]
    );
    let confirmed = |n| JournalOp::Confirmed {
        about: A.into(),
        entries: vec![Confirmed::Count { id: id("A"), n }],
    };
    assert_eq!(compacted(&[confirmed(2), confirmed(3)]), [confirmed(3)]);
    assert_eq!(compacted(&[confirmed(3), confirmed(2)]), [confirmed(3)]);
    // Never across another kind of change, another activation or a deactivation.
    assert_eq!(
        compacted(&[
            offline_of(1, "exports"),
            JournalOp::Deactivated,
            offline_of(2, "exports")
        ])
        .len(),
        3
    );
    let other_seat = op(SpendOp {
        about: "act_2".into(),
        ..offline_op(2)
    });
    assert_eq!(compacted(&[offline_of(1, "exports"), other_seat]).len(), 2);
    assert_eq!(
        compacted(&[
            offline_of(1, "exports"),
            JournalOp::Activated { about: A.into() },
            offline_of(2, "exports")
        ])
        .len(),
        3
    );
    let void = op(spend(SpendKind::Void, &id("V"), 1));
    assert_eq!(
        compacted(&[offline_of(1, "exports"), void, offline_of(2, "exports")]).len(),
        3
    );
    // A void and a count of one id are two confirmations.
    let both = JournalOp::Confirmed {
        about: A.into(),
        entries: vec![Confirmed::Void { id: id("A") }],
    };
    assert_eq!(
        compacted(&[confirmed(1), both]),
        [JournalOp::Confirmed {
            about: A.into(),
            entries: vec![
                Confirmed::Count { id: id("A"), n: 1 },
                Confirmed::Void { id: id("A") }
            ]
        }]
    );
}

#[test]
fn what_is_folded_lands_as_the_ops_it_was_folded_from_would() {
    let base = journal(Vec::new());
    let ops = [
        op(SpendOp {
            id: id("a"),
            at: NOW - 3000,
            ..offline_op(1)
        }),
        op(SpendOp {
            id: id("b"),
            at: NOW - 2000,
            ..offline_op(2)
        }),
        op(SpendOp {
            id: id("c"),
            at: NOW - 1000,
            ..offline_op(4)
        }),
    ];
    let landed = land_journal(&base, &compacted(&ops), NOW).state;
    assert_eq!(landed, land_journal(&base, &ops, NOW).state);
    // One entry, with the first spend's id and time.
    assert_eq!(
        landed.spends,
        Some(vec![entry_of(&id("a"), SpendKind::Offline, 7, NOW - 3000)])
    );
}

#[test]
fn keeps_only_the_newest_voids_in_memory_and_every_other_change() {
    let void = |i: usize| op(spend(SpendKind::Void, &format!("{i:v>22}"), 1));
    let mut ops: Vec<JournalOp> = (0..20).map(void).collect();
    ops.insert(5, op(offline_op(1)));
    ops.push(JournalOp::Deactivated);
    let kept = bounded_voids(ops.clone());
    let voids: Vec<&JournalOp> = kept
        .iter()
        .filter(|o| matches!(o, JournalOp::Spend(s) if s.kind == SpendKind::Void))
        .collect();
    assert_eq!(voids.len(), MAX_PENDING_VOIDS);
    assert_eq!(*voids[0], void(4), "the oldest four went");
    assert!(kept.contains(&op(offline_op(1))) && kept.contains(&JournalOp::Deactivated));
    assert_eq!(bounded_voids(ops[16..].to_vec()).len(), ops[16..].len());
}

// --- a lease's met, read leniently ---------------------------------------------------------------------------------------

fn met_of(text: &str) -> BTreeMap<String, BaseFigure> {
    meters_of(&parse_json(text).expect("JSON"))
}

#[test]
fn reads_a_met_that_is_not_an_object_as_no_meters() {
    for text in [
        "null",
        "[]",
        r#"[{"left":1,"total":1,"offline":1}]"#,
        r#""exports""#,
        "7",
        "true",
    ] {
        assert!(met_of(text).is_empty(), "{text}");
    }
}

#[test]
fn drops_a_member_whose_figures_are_not_non_negative_whole_numbers_of_at_most_2_to_the_53_minus_1()
{
    let meter = |left: &str| format!(r#"{{"exports":{{"left":{left},"total":10,"offline":1}}}}"#);
    for kept in [
        "4",
        "4.0",
        "4e0",
        "0",
        "-0",
        "9007199254740991",
        "9.007199254740991e15",
    ] {
        assert_eq!(met_of(&meter(kept)).len(), 1, "{kept}");
    }
    for dropped in [
        "-1",
        "1.5",
        "0.5",
        r#""4""#,
        "null",
        "true",
        "[4]",
        "{}",
        "9007199254740992",
        "9007199254740993",
        "1e400",
        "-1e400",
        "1e19",
        "18446744073709551615",
        "123456789012345678901234567890",
    ] {
        assert!(met_of(&meter(dropped)).is_empty(), "{dropped}");
    }
    // Only the member that is not a meter goes, never the meters beside it.
    let both = r#"{"a":{"left":4,"total":10,"offline":1},"b":{"left":-1,"total":10,"offline":1},"c":{"left":4,"total":10,"offline":1}}"#;
    assert_eq!(
        met_of(both).keys().map(String::as_str).collect::<Vec<_>>(),
        ["a", "c"]
    );
}

#[test]
fn reads_resets_as_true_only_for_json_true() {
    let resets = |value: &str| {
        met_of(&format!(
            r#"{{"m":{{"left":1,"total":1,"offline":0,"resets":{value}}}}}"#
        ))["m"]
            .resets
    };
    assert!(resets("true"));
    for other in ["false", "null", "1", r#""true""#, "[true]", "{}", "0"] {
        assert!(!resets(other), "{other}");
    }
    assert!(!met_of(r#"{"m":{"left":1,"total":1,"offline":0}}"#)["m"].resets);
}

#[test]
fn keeps_names_as_they_are_the_servers_caps_unenforced_and_a_meters_other_members_ignored() {
    let nine: String = (0..9)
        .map(|i| {
            format!(r#""m{i}":{{"left":20,"total":10,"offline":1,"overage":3,"extra":{{"a":1}}}}"#)
        })
        .collect::<Vec<_>>()
        .join(",");
    let read = met_of(&format!("{{{nine}}}"));
    assert_eq!(read.len(), 9, "more than 8 meters, and left above total");
    let named = met_of(
        r#"{"__proto__":{"left":4,"total":10,"offline":1},"":{"left":4,"total":10,"offline":1},"éxports":{"left":4,"total":10,"offline":1}}"#,
    );
    assert_eq!(
        named.keys().map(String::as_str).collect::<Vec<_>>(),
        ["", "__proto__", "éxports"]
    );
    // The last of a repeated name is the one read, as JSON.parse reads it.
    let repeated =
        met_of(r#"{"m":{"left":1,"total":9,"offline":0},"m":{"left":2,"total":9,"offline":0}}"#);
    assert_eq!(repeated["m"].left, 2);
}

#[test]
fn a_figure_out_of_a_doubles_range_drops_its_member_and_never_the_lease() {
    let h = Harness::empty();
    let key = KeySet::from(PublicKey::parse(&h.key.public_jwk()).expect("a key"));
    let lease = raw_lease(
        &h.key,
        r#"{"huge":{"left":1e400,"total":10,"offline":1},"kept":{"left":1,"total":2,"offline":0}}"#,
    );
    let verified = verify_lease(&lease, &key, NOW);
    assert!(verified.is_valid(), "{:?}", verified.failure());
    let met = verified
        .claims()
        .and_then(|claims| claims.met.clone())
        .expect("a met claim");
    assert_eq!(met.keys().map(String::as_str).collect::<Vec<_>>(), ["kept"]);
    // And through the SDK: the lease licenses, with the meter that is whole.
    h.set_state(stored_with(&lease));
    let status = h.license.status(false);
    assert_eq!(status.state, keygrant::LicenseState::Licensed);
    assert_eq!(
        status.meters.keys().map(String::as_str).collect::<Vec<_>>(),
        ["kept"]
    );
}

#[test]
fn a_met_claim_is_absent_null_or_any_json_and_a_lease_with_none_is_unchanged() {
    let h = Harness::empty();
    let key = KeySet::from(PublicKey::parse(&h.key.public_jwk()).expect("a key"));
    let claims = |text: &str| {
        verify_lease(&raw_lease(&h.key, text), &key, NOW)
            .claims()
            .cloned()
            .expect("claims")
    };
    assert_eq!(claims("null").met, Some(BTreeMap::new()));
    assert_eq!(claims("[]").met, Some(BTreeMap::new()));
    assert_eq!(claims("7").met, Some(BTreeMap::new()));
    let without = h.key.lease_with(NOW, 86_400, json!({}));
    let verified = verify_lease(&without, &key, NOW);
    assert_eq!(
        verified.claims().and_then(|claims| claims.met.clone()),
        None
    );
    assert!(verified.to_json()["claims"].get("met").is_none());
    let bad = verify_lease(&without.replace('.', "#"), &key, NOW);
    assert_eq!(bad.failure(), Some(VerifyFailure::Malformed));
}

#[test]
fn reads_json_as_a_lease_payload_is_read_a_number_out_of_range_clamped_and_text_inside_a_string_left_alone(
) {
    let read =
        parse_json(r#"{"a":1e400,"b":-1e999,"s":"1e400 \" 1e500","n":[2e308,3]}"#).expect("JSON");
    assert_eq!(read["s"], r#"1e400 " 1e500"#);
    assert!(read["a"].as_f64().is_some_and(|a| a > 1e308));
    assert!(read["b"].as_f64().is_some_and(|b| b < -1e308));
    assert_eq!(read["n"][1], 3);
    assert_eq!(parse_json("not json"), None);
    assert_eq!(parse_json(r#"{"a":1e400"#), None);
    assert_eq!(
        parse_json("[1e5,1.0e-400]").map(|v| v[0].as_f64()),
        Some(Some(100_000.0))
    );
}

// --- the stored state ---------------------------------------------------------------------------------------------------

#[test]
fn the_stored_state_reads_the_journal_leniently_and_writes_it_back_without_what_it_dropped() {
    let raw = json!({
        "key": KEY,
        "spends": [
            { "id": "A", "meter": "m", "n": 2, "at": 5, "kind": "offline", "extra": 1 },
            { "id": "B", "meter": "m", "n": 0, "at": 5, "kind": "offline" },
            { "id": "C", "meter": "m", "n": 1, "at": -5, "kind": "offline" },
            { "id": "D", "meter": "m", "n": 1, "at": 5, "kind": "other" },
            { "id": "E", "meter": 3, "n": 1, "at": 5, "kind": "offline" },
            { "id": "F", "meter": "m", "n": 1, "at": 5.5, "kind": "void" },
            17,
        ],
        "meterFigures": {
            "m": { "left": 1, "total": 2, "offline": 0, "resets": true, "at": 9, "extra": true },
            "bad": { "left": 1, "total": 2, "offline": 0, "at": -9 },
            "worse": "x",
        },
    });
    let read = StoredState::from_json(&raw).expect("a state");
    assert_eq!(
        read.spends,
        Some(vec![SpendEntry {
            id: "A".into(),
            meter: "m".into(),
            n: 2,
            at: 5,
            kind: SpendKind::Offline
        }])
    );
    assert_eq!(read.meter_figures.as_ref().map(BTreeMap::len), Some(1));
    let written = read.to_json();
    assert_eq!(
        written["spends"],
        json!([{ "id": "A", "meter": "m", "n": 2, "at": 5, "kind": "offline" }])
    );
    assert_eq!(
        written["meterFigures"],
        json!({ "m": { "left": 1, "total": 2, "offline": 0, "resets": true, "at": 9 } })
    );
    // Neither field is an unknown field of a newer SDK, kept as it was.
    assert!(read.extra.is_empty());
    // A journal that is not a list, and one with no valid entry, is the field absent.
    for spends in [
        json!("junk"),
        json!({}),
        json!(7),
        json!([null, 1]),
        json!([]),
    ] {
        let state = StoredState::from_json(&json!({ "spends": spends })).expect("a state");
        assert_eq!(state.spends, None);
        assert!(state.to_json().get("spends").is_none());
    }
}

// --- the words and the JSON ----------------------------------------------------------------------------------------------

#[test]
fn a_result_serialises_as_the_electron_sdks_and_every_error_is_its_word() {
    assert_eq!(
        serde_json::to_value(UseResult::spent(89, false)).expect("serialisable"),
        json!({ "ok": true, "left": 89 })
    );
    assert_eq!(
        serde_json::to_value(UseResult::spent(88, true)).expect("serialisable"),
        json!({ "ok": true, "left": 88, "offline": true })
    );
    assert_eq!(
        serde_json::to_value(UseResult::refused(UseError::UsedUp, Some(1))).expect("serialisable"),
        json!({ "ok": false, "left": 1, "error": "used-up" })
    );
    assert_eq!(
        serde_json::to_value(UseResult::refused(UseError::Storage, None)).expect("serialisable"),
        json!({ "ok": false, "error": "storage" })
    );
    let words = [
        (UseError::InvalidAmount, "invalid-amount"),
        (UseError::NoMeter, "no-meter"),
        (UseError::UsedUp, "used-up"),
        (UseError::OfflineLimit, "offline-limit"),
        (UseError::NotLicensed, "not-licensed"),
        (UseError::Storage, "storage"),
        (UseError::Revoked, "revoked"),
        (UseError::Upgrade, "upgrade"),
        (UseError::Outdated, "outdated"),
        (UseError::Device, "device"),
    ];
    for (error, word) in words {
        assert_eq!(error.as_str(), word);
        assert_eq!(error.to_string(), word);
        assert_eq!(
            serde_json::to_value(error).expect("serialisable"),
            json!(word)
        );
        assert_eq!(
            serde_json::from_value::<UseError>(json!(word)).expect("a word"),
            error
        );
    }
    assert_eq!(UseError::of_refusal(Refusal::Device), UseError::Device);
    assert_eq!(UseError::of_refusal(Refusal::Revoked), UseError::Revoked);
}

#[test]
fn a_status_counts_its_meters_in_its_equality_and_reads_back_from_its_json() {
    let plain = keygrant::LicenseStatus::new(keygrant::LicenseState::Licensed);
    let mut metered = plain.clone();
    assert_eq!(metered, plain);
    metered.meters.insert(
        "exports".into(),
        MeterStatus {
            left: 9,
            total: 10,
            offline: 1,
            unsynced: 1,
            resets: true,
        },
    );
    assert_ne!(metered, plain);
    let mut other = metered.clone();
    other.meters.get_mut("exports").expect("a meter").left = 8;
    assert_ne!(metered, other);
    // Each status holds a copy of its own: changing one changes no other.
    assert_eq!(metered.clone().meters, metered.meters);
    let read: keygrant::LicenseStatus =
        serde_json::from_value(serde_json::to_value(&metered).expect("serialisable"))
            .expect("read back");
    assert_eq!(read, metered);
    // And a status without meters has no `meters` in its JSON.
    assert!(serde_json::to_value(&plain)
        .expect("serialisable")
        .get("meters")
        .is_none());
}
