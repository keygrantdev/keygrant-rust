//! The journal of unsynced spends is reported with the next validate, `use`
//! and `deactivate`, dropped only when the server confirms it, and never costs
//! a customer a renewal or a release; and `status().meters` shows each meter of
//! the lease that licenses the device, net of the journal.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::*;
use keygrant::meters::MAX_PENDING_SPENDS;
use keygrant::offline::{ASK_AGAIN_MS, RETRY_UNANSWERED_MS};
use keygrant::{
    HttpResponse, LicenseState, MeterFigure, MeterStatus, Refusal, SpendEntry, SpendKind, UseError,
    UseResult,
};
use serde_json::{json, Value};

/// The `spends` the `nth` validate request carried.
fn reported(h: &Harness, nth: usize) -> Option<Value> {
    h.http
        .calls()
        .into_iter()
        .filter(|(action, _)| action == "validate")
        .nth(nth)
        .expect("a validate")
        .1
        .get("spends")
        .cloned()
}

/// What a server does with a validate, as a test sets it.
type Answer = Box<dyn Fn(&Harness)>;

fn id(letter: &str) -> String {
    letter.repeat(22)
}

// --- what a validate reports ------------------------------------------------------------------

#[test]
fn carries_the_journal_oldest_first_a_void_marked_and_no_spends_at_all_when_there_is_none() {
    let h = holding_journal(vec![
        entry_of(&id("A"), SpendKind::Offline, 3, NOW - 3000),
        entry_of(&id("B"), SpendKind::Unanswered, 1, NOW - 2000),
        entry_of(&id("C"), SpendKind::Void, 2, NOW - 1000),
    ]);
    renewing(&h);
    h.license.refresh();
    assert_eq!(
        reported(&h, 0),
        Some(json!([
            { "id": id("A"), "meter": "exports", "n": 3, "at": NOW - 3000 },
            { "id": id("B"), "meter": "exports", "n": 1, "at": NOW - 2000 },
            { "id": id("C"), "meter": "exports", "n": 2, "at": NOW - 1000, "void": true },
        ]))
    );
    let bare = holding();
    renewing(&bare);
    bare.license.refresh();
    assert!(bare.http.body_of("validate").get("spends").is_none());
}

fn renewing(h: &Harness) {
    h.validate_with(&metered_lease(&h.key, meters()));
}

#[test]
fn reports_only_what_is_valid_of_a_stored_journal_and_renews_all_the_same() {
    let h = Harness::empty();
    let stored = json!({
        "key": KEY, "activationId": "act_1", "lastSeen": NOW, "lease": metered_lease(&h.key, meters()),
        "spends": [
            { "id": "AAAAAAAAAAAAAAAAAAAAAA", "meter": "exports", "n": 2, "at": NOW, "kind": "offline" },
            { "id": 4 }, null, "x",
            { "id": id("Z"), "meter": "exports", "n": 0, "at": NOW, "kind": "offline" },
        ],
    });
    h.set_state(state(stored.clone()));
    renewing(&h);
    assert_eq!(h.license.refresh().state, LicenseState::Licensed);
    assert_eq!(
        reported(&h, 0).and_then(|r| r.as_array().map(Vec::len)),
        Some(1)
    );

    let junk = Harness::empty();
    let mut raw = stored;
    raw["lease"] = json!(metered_lease(&junk.key, meters()));
    raw["spends"] = json!({ "not": "a list" });
    junk.set_state(state(raw));
    renewing(&junk);
    assert_eq!(junk.license.refresh().state, LicenseState::Licensed);
    assert!(junk.http.body_of("validate").get("spends").is_none());
}

#[test]
fn renews_with_a_journal_that_is_full() {
    let full: Vec<SpendEntry> = (0..MAX_PENDING_SPENDS)
        .map(|i| SpendEntry {
            id: format!("{i:q>22}"),
            meter: format!("m{i}"),
            n: 1,
            at: LONG_AGO,
            kind: SpendKind::Unanswered,
        })
        .collect();
    let h = holding_journal(full);
    renewing(&h);
    assert_eq!(h.license.refresh().state, LicenseState::Licensed);
    assert_eq!(
        reported(&h, 0).and_then(|r| r.as_array().map(Vec::len)),
        Some(MAX_PENDING_SPENDS)
    );
}

#[test]
fn reports_the_first_64_of_a_longer_journal() {
    // 70 entries is a journal the cap let a told spend past; a report carries the first 64.
    let long: Vec<SpendEntry> = (0..70)
        .map(|i| SpendEntry {
            id: format!("{i:q>22}"),
            meter: format!("m{i}"),
            n: 1,
            at: LONG_AGO,
            kind: SpendKind::Unanswered,
        })
        .collect();
    let h = holding_journal(long);
    renewing(&h);
    h.license.refresh();
    assert_eq!(
        reported(&h, 0).and_then(|r| r.as_array().map(Vec::len)),
        Some(64)
    );
}

#[test]
fn drops_exactly_the_entries_the_answer_names_as_recorded() {
    let entries = vec![
        entry_of(&id("A"), SpendKind::Unanswered, 3, LONG_AGO),
        SpendEntry {
            meter: "renders".into(),
            ..entry_of(&id("B"), SpendKind::Offline, 5, LONG_AGO)
        },
        entry_of(&id("C"), SpendKind::Void, 1, LONG_AGO),
        SpendEntry {
            meter: "renders".into(),
            ..entry_of(&id("D"), SpendKind::Void, 1, LONG_AGO)
        },
    ];
    let h = holding_journal(entries);
    h.http.on("validate", |_| {
        reply(
            200,
            json!({
                "lease": "not a lease",
                "spends": [
                    { "id": id("A"), "n": 3 },
                    // More recorded than ever held: dropped. Less than held: kept.
                    { "id": id("B"), "n": 4 },
                    // A void is named void; a count for a void entry does not name it.
                    { "id": id("C"), "void": true },
                    { "id": id("D"), "n": 1 },
                    // Another shape confirms nothing, and an id the journal has not is nothing.
                    { "id": id("E"), "n": 1 }, { "n": 2 }, "x", null,
                ],
            }),
        )
    });
    h.license.refresh();
    let ids: Vec<String> = journal_of(&h).into_iter().map(|e| e.id).collect();
    assert_eq!(ids, [id("B"), id("D")]);
}

#[test]
fn keeps_an_offline_entry_another_process_grew_meanwhile_with_its_count() {
    let h = holding_journal(vec![entry_of(&id("A"), SpendKind::Offline, 3, LONG_AGO)]);
    let storage = h.storage.clone();
    h.http.on("validate", move |_| {
        // Another process spent two more while this request was out.
        let mut state = storage.get().expect("a state");
        state.spends = Some(vec![entry_of(&id("A"), SpendKind::Offline, 5, LONG_AGO)]);
        storage.set(Some(state));
        reply(
            200,
            json!({ "lease": "no lease", "spends": [{ "id": id("A"), "n": 3 }] }),
        )
    });
    h.license.refresh();
    assert_eq!(
        journal_of(&h),
        [entry_of(&id("A"), SpendKind::Offline, 5, LONG_AGO)]
    );
}

#[test]
fn drops_nothing_when_the_answer_names_nothing_and_waits_an_hour_before_status_offers_it_again() {
    let h = holding_journal(vec![entry(SpendKind::Offline, 1, LONG_AGO)]);
    renewing(&h);
    h.license.refresh();
    assert_eq!(journal_of(&h).len(), 1);
    assert_eq!(h.state().backoff_ms, Some(ASK_AGAIN_MS));
    // A valid, far lease with a journal asks only once the wait is over.
    h.license.status(false);
    assert_eq!(h.http.count("validate"), 1);
    h.set_clock(NOW + HOUR);
    h.license.status(false);
    assert_eq!(h.http.count("validate"), 2);
}

#[test]
fn drops_nothing_on_a_5xx_or_429_whatever_their_body_says_and_nothing_on_no_answer() {
    let named = json!({ "spends": [{ "id": id("A"), "n": 3 }] });
    for answer in [Some((503, named.clone())), Some((429, named)), None] {
        let h = holding_journal(vec![entry_of(&id("A"), SpendKind::Unanswered, 3, LONG_AGO)]);
        match &answer {
            None => h.http.fail(true),
            Some((status, body)) => h.http.answer("validate", *status, body.clone()),
        }
        h.license.refresh();
        assert_eq!(journal_of(&h).len(), 1, "{answer:?}");
    }
}

#[test]
fn drops_what_a_refusal_confirms_whatever_its_status() {
    let held = || vec![entry_of(&id("A"), SpendKind::Unanswered, 3, LONG_AGO)];
    let h = holding_journal(held());
    h.http.answer(
        "validate",
        403,
        json!({ "error": "revoked", "spends": [{ "id": id("A"), "n": 3 }] }),
    );
    let status = h.license.refresh();
    assert_eq!(status.state, LicenseState::Invalid);
    assert_eq!(status.reason, Some(Refusal::Revoked.reason()));
    assert!(journal_of(&h).is_empty());
    // A 200 without a lease is an answer too.
    let g = holding_journal(held());
    g.http.answer(
        "validate",
        200,
        json!({ "spends": [{ "id": id("A"), "n": 3 }] }),
    );
    g.license.refresh();
    assert!(journal_of(&g).is_empty());
}

#[test]
fn keeps_the_confirmation_in_memory_when_it_cannot_be_saved_so_the_entry_is_not_reported_again() {
    let h = holding_journal(vec![entry_of(&id("A"), SpendKind::Unanswered, 3, LONG_AGO)]);
    h.http.on("validate", |_| renewal_for(&id("A"), 3));
    h.storage.fail_writes(true);
    h.license.refresh();
    assert_eq!(journal_of(&h).len(), 1);
    assert_eq!(h.license.status(false).meters["exports"].unsynced, 0);
    h.license.refresh();
    assert_eq!(reported(&h, 1), None);
    h.storage.fail_writes(false);
    h.license.status(false);
    assert!(journal_of(&h).is_empty());
}

fn renewal_for(entry_id: &str, n: u64) -> HttpResponse {
    let key = TestKey::new(7);
    reply(
        200,
        json!({ "lease": metered_lease(&key, meters()), "spends": [{ "id": entry_id, "n": n }] }),
    )
}

#[test]
fn reports_to_a_valid_lease_at_the_next_status_once_its_own_wait_is_not_running_and_not_before() {
    let h = holding_journal(vec![entry_of(
        &id("A"),
        SpendKind::Unanswered,
        1,
        NOW - 30_000,
    )]);
    h.http.on("validate", |_| renewal_for(&id("A"), 1));
    // The journal's wait: a minute after one unanswered spend.
    h.license.status(false);
    assert_eq!(h.http.count("validate"), 0);
    h.set_clock(NOW - 30_000 + MINUTE);
    h.license.status(false);
    assert_eq!(h.http.count("validate"), 1);
    assert!(journal_of(&h).is_empty());
    // Nothing left to report: a valid, far lease asks nothing.
    h.license.status(false);
    assert_eq!(h.http.count("validate"), 1);
}

#[test]
fn reports_an_offline_only_journal_at_once_and_waits_for_the_asks_own_wait_after_no_answer() {
    let h = holding_journal(vec![entry(SpendKind::Offline, 2, NOW)]);
    h.http.fail(true);
    h.license.status(false);
    assert_eq!(h.http.count("validate"), 1);
    assert_eq!(h.state().backoff_ms, Some(RETRY_UNANSWERED_MS));
    h.license.status(false);
    assert_eq!(h.http.count("validate"), 1);
    h.http.fail(false);
    h.http
        .on("validate", |_| renewal_for("AAAAAAAAAAAAAAAAAAAAAA", 2));
    h.set_clock(NOW + MINUTE);
    h.license.status(false);
    assert!(journal_of(&h).is_empty());
}

#[test]
fn asks_of_a_lease_past_its_end_to_heal_it_with_the_journal() {
    let h = holding_journal(vec![entry(SpendKind::Offline, 1, NOW - 40 * 86_400_000)]);
    h.set_clock(NOW + 40 * 86_400_000);
    h.http.answer("validate", 200, json!({}));
    h.license.status(false);
    assert_eq!(
        reported(&h, 0).and_then(|r| r.as_array().map(Vec::len)),
        Some(1)
    );
}

#[test]
fn rides_a_use_request_too_and_is_dropped_by_what_that_request_confirms() {
    let h = holding_journal(vec![entry_of(&id("A"), SpendKind::Unanswered, 2, LONG_AGO)]);
    h.http.on("use", |_| {
        let mut answer = spent(87, NOW);
        answer.json.as_mut().expect("a body")["spends"] = json!([{ "id": id("A"), "n": 2 }]);
        answer
    });
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::spent(87, false)
    );
    assert_eq!(
        use_body(&h, 0)["spends"],
        json!([{ "id": id("A"), "meter": "exports", "n": 2, "at": LONG_AGO }])
    );
    assert!(journal_of(&h).is_empty());
}

// --- what a deactivate reports -------------------------------------------------------------------

#[test]
fn a_deactivate_carries_the_journal_with_the_devices_fingerprints_then_clears_it_with_the_licence()
{
    let h = holding_journal(vec![
        entry(SpendKind::Offline, 2, NOW),
        entry_of(&id("B"), SpendKind::Void, 1, NOW),
    ]);
    h.http.answer("deactivate", 200, json!({ "ok": true }));
    assert!(h.license.deactivate().expect("released").released);
    let body = h.http.body_of("deactivate");
    assert_eq!(
        (body["key"].as_str(), body["activationId"].as_str()),
        (Some(KEY), Some("act_1"))
    );
    assert_eq!(body["spends"].as_array().map(Vec::len), Some(2));
    assert_eq!(body["fingerprints"].as_array().map(Vec::len), Some(1));
    assert!(body.get("fingerprintKind").is_none());
    let stored = h.state();
    assert_eq!(stored.spends, None);
    assert_eq!(stored.meter_figures, None);
    assert_eq!((stored.key, stored.activation_id), (None, None));
}

#[test]
fn a_deactivate_sends_exactly_the_key_and_the_activation_when_there_is_nothing_to_report() {
    let figures: BTreeMap<String, MeterFigure> = BTreeMap::from([(
        "exports".to_owned(),
        MeterFigure {
            left: 5,
            total: 100,
            offline: 10,
            resets: false,
            at: NOW,
        },
    )]);
    let h = holding_with(meters(), Opts::default(), |state| {
        state.meter_figures = Some(figures)
    });
    h.http.answer("deactivate", 200, json!({}));
    h.license.deactivate().expect("released");
    assert_eq!(
        h.http.body_of("deactivate"),
        json!({ "key": KEY, "activationId": "act_1" })
    );
    assert_eq!(h.state().meter_figures, None);
}

#[test]
fn a_deactivate_names_no_fingerprints_when_it_cannot_tell_who_the_device_is() {
    struct Unknown;
    impl keygrant::Fingerprinter for Unknown {
        fn get(&self) -> Result<String, keygrant::AdapterError> {
            Err("no id".into())
        }
    }
    let h = holding_with(
        meters(),
        Opts {
            fingerprint: Some(std::sync::Arc::new(Unknown)),
            ..Opts::default()
        },
        |state| state.spends = Some(vec![entry(SpendKind::Offline, 1, NOW)]),
    );
    h.http.answer("deactivate", 200, json!({}));
    h.license.deactivate().expect("released");
    let body = h.http.body_of("deactivate");
    assert_eq!(body["spends"].as_array().map(Vec::len), Some(1));
    assert!(body.get("fingerprints").is_none());
}

#[test]
fn a_deactivate_clears_the_journal_locally_and_says_the_seat_may_be_held_when_the_release_got_no_200(
) {
    for fail in [true, false] {
        let h = holding_journal(vec![entry(SpendKind::Offline, 1, NOW)]);
        if fail {
            h.http.fail(true);
        } else {
            h.http.answer("deactivate", 503, json!({}));
        }
        assert!(!h.license.deactivate().expect("dropped").released);
        assert_eq!(h.state().spends, None);
    }
}

#[test]
fn a_deactivate_reports_what_this_process_could_not_save_and_forgets_it_with_the_rest() {
    let h = holding();
    h.http.fail(true);
    h.storage.fail_writes(true);
    h.license.use_meter("exports", 1);
    h.storage.fail_writes(false);
    h.http.fail(false);
    h.http.answer("deactivate", 200, json!({}));
    h.license.deactivate().expect("released");
    assert_eq!(
        h.http.body_of("deactivate")["spends"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    // Nothing of it comes back with the next licence.
    let lease = metered_lease(&h.key, meters());
    h.http.answer(
        "activate",
        200,
        json!({ "lease": lease, "activationId": "act_1" }),
    );
    h.license.activate(KEY).expect("activated");
    assert_eq!(h.license.status(false).meters["exports"].unsynced, 0);
    assert!(journal_of(&h).is_empty());
    assert!(h.license.held_changes().is_empty());
}

#[test]
fn a_deactivate_whose_state_cannot_be_saved_fails_keeping_the_key_and_reports_the_journal_again() {
    let h = holding_journal(vec![entry(SpendKind::Offline, 2, NOW)]);
    h.http.answer("deactivate", 200, json!({}));
    h.storage.fail_writes(true);
    assert!(h.license.deactivate().is_err());
    assert_eq!(h.state().key.as_deref(), Some(KEY));
    // The release is idempotent: the retry reports the journal still stored and drops the key.
    h.storage.fail_writes(false);
    assert!(h.license.deactivate().expect("released").released);
    assert_eq!(h.http.count("deactivate"), 2);
    assert_eq!(
        h.http.last_body_of("deactivate")["spends"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(h.state().key, None);
}

#[test]
fn a_deactivate_reports_the_first_64_of_a_longer_journal_and_clears_every_entry_with_the_licence() {
    let long: Vec<SpendEntry> = (0..MAX_PENDING_SPENDS + 6)
        .map(|i| SpendEntry {
            id: format!("{i:q>22}"),
            meter: format!("m{i}"),
            n: 1,
            at: LONG_AGO,
            kind: SpendKind::Unanswered,
        })
        .collect();
    let h = holding_journal(long);
    h.http.answer("deactivate", 200, json!({}));
    assert!(h.license.deactivate().expect("released").released);
    let sent = h.http.body_of("deactivate");
    let meters: Vec<&str> = sent["spends"]
        .as_array()
        .expect("a report")
        .iter()
        .map(|e| e["meter"].as_str().expect("a meter"))
        .collect();
    assert_eq!(meters.len(), MAX_PENDING_SPENDS);
    assert_eq!((meters[0], meters[MAX_PENDING_SPENDS - 1]), ("m0", "m63"));
    assert_eq!(h.state().spends, None);
}

#[test]
fn a_deactivate_that_cannot_land_what_it_holds_goes_with_what_is_stored_and_the_call_then_fails_with_the_key_kept(
) {
    let h = holding_journal(vec![entry_of(&id("S"), SpendKind::Offline, 4, LONG_AGO)]);
    h.http.fail(true);
    // A spend in doubt, a void and, while the journal waits, a spend made offline, held in memory: the disk
    // refuses.
    h.storage.fail_writes(true);
    assert!(h.license.use_meter("exports", 2).offline);
    assert_eq!(
        h.license.use_meter("renders", 1),
        UseResult::refused(UseError::OfflineLimit, Some(5))
    );
    assert!(h.license.use_meter("exports", 3).offline);
    assert_eq!(h.license.held_changes().len(), 3);
    assert_eq!(h.http.count("use"), 2);
    h.http.fail(false);
    h.http.answer("deactivate", 200, json!({}));
    assert!(matches!(
        h.license.deactivate(),
        Err(keygrant::Error::Storage(_))
    ));
    // The release went out with the stored journal and the held spend in doubt and void, and the key is kept.
    // The spend made offline joins the entry it was made beside when it lands, so it is not reported under an id
    // of its own.
    let sent = h.http.body_of("deactivate");
    let report: Vec<(&str, u64, bool)> = sent["spends"]
        .as_array()
        .expect("a report")
        .iter()
        .map(|e| {
            (
                e["meter"].as_str().expect("a meter"),
                e["n"].as_u64().expect("a count"),
                e.get("void").is_some(),
            )
        })
        .collect();
    assert_eq!(
        report,
        [
            ("exports", 4, false),
            ("exports", 2, false),
            ("renders", 1, true)
        ]
    );
    // And those two are the ids the server was asked about.
    let ids: Vec<&str> = sent["spends"]
        .as_array()
        .expect("a report")
        .iter()
        .map(|e| e["id"].as_str().expect("an id"))
        .collect();
    assert_eq!(ids[1], use_body(&h, 0)["id"].as_str().expect("an id"));
    assert_eq!(ids[2], use_body(&h, 1)["id"].as_str().expect("an id"));
    assert_eq!(h.state().key.as_deref(), Some(KEY));
}

#[test]
fn reports_nothing_for_a_seat_that_is_another_devices_the_release_is_not_its_to_make() {
    let h = holding_journal(vec![entry(SpendKind::Offline, 1, NOW)]);
    // Its lease is held to a machine this one is not: a state file copied from another PC.
    let lease = metered_lease_at(
        &h.key,
        meters(),
        NOW,
        json!({ "fp": "f".repeat(64), "fpk": "machine" }),
    );
    let mut stored = h.state();
    stored.lease = Some(lease);
    h.set_state(stored);
    // A device that can tell who it is, and is not that machine.
    let machine: std::sync::Arc<dyn keygrant::Fingerprinter> =
        std::sync::Arc::new(MachineOf("fp-machine"));
    let licence = h.license_with(machine);
    assert!(licence.deactivate().expect("dropped").released);
    assert_eq!(h.http.count("deactivate"), 0);
}

struct MachineOf(&'static str);

impl keygrant::Fingerprinter for MachineOf {
    fn get(&self) -> Result<String, keygrant::AdapterError> {
        Ok(self.0.to_owned())
    }
    fn identify(&self, _timeout: Duration) -> Option<keygrant::DeviceIdentity> {
        Some(keygrant::DeviceIdentity {
            fingerprint: Some(self.0.to_owned()),
            kind: Some(keygrant::DeviceKind::Machine),
            alternates: Vec::new(),
        })
    }
}

// --- status().meters ---------------------------------------------------------------------------------

fn shown(left: u64, total: u64, offline: u64, unsynced: u64, resets: bool) -> MeterStatus {
    MeterStatus {
        left,
        total,
        offline,
        unsynced,
        resets,
    }
}

#[test]
fn shows_each_meter_of_the_lease_with_its_figures_and_nothing_when_the_lease_has_none() {
    let h = holding();
    let status = h.license.status(false);
    assert_eq!(status.state, LicenseState::Licensed);
    assert_eq!(status.key.as_deref(), Some(KEY));
    assert_eq!(
        status.meters,
        BTreeMap::from([
            ("exports".to_owned(), shown(90, 100, 10, 0, false)),
            ("renders".to_owned(), shown(5, 5, 0, 0, true)),
        ])
    );
    assert_eq!(h.http.count("validate"), 0);
    // As a JSON status, the Electron SDK's.
    assert_eq!(
        serde_json::to_value(&status).expect("serialisable")["meters"]["exports"],
        json!({ "left": 90, "total": 100, "offline": 10, "unsynced": 0, "resets": false })
    );
    let bare = holding_with(Value::Null, Opts::default(), |_| {});
    let status = bare.license.status(false);
    assert!(status.meters.is_empty());
    assert_eq!(
        serde_json::to_value(&status).expect("serialisable"),
        json!({ "state": "licensed", "key": KEY })
    );
}

#[test]
fn is_net_of_the_unsynced_spends_of_each_kind_never_below_0_and_counts_no_void() {
    let h = holding_with(meters(), Opts::default(), |state| {
        state.spends = Some(vec![
            entry(SpendKind::Offline, 4, NOW),
            entry_of(&id("B"), SpendKind::Unanswered, 3, NOW),
            entry_of(&id("C"), SpendKind::Void, 50, NOW),
            SpendEntry {
                meter: "renders".into(),
                ..entry_of(&id("D"), SpendKind::Offline, 9, NOW)
            },
        ]);
        state.backoff_since = Some(NOW);
        state.backoff_ms = Some(RETRY_UNANSWERED_MS);
    });
    let meters = h.license.status(false).meters;
    assert_eq!(meters["exports"], shown(83, 100, 10, 7, false));
    assert_eq!(meters["renders"], shown(0, 5, 0, 9, true));
}

#[test]
fn works_behind_a_call_that_has_not_finished_saying_so() {
    let h = holding_with(meters(), behind_opts(), |state| {
        state.spends = Some(vec![entry(SpendKind::Offline, 2, NOW)])
    });
    h.http
        .answer("validate", 403, json!({ "error": "revoked" }));
    let gate = h.storage.hold_next_write();
    let later = std::thread::scope(|scope| {
        let refreshing = scope.spawn(|| h.license.refresh());
        let _releasing = gate.releasing();
        gate.await_reached();
        let later = h.license.status(false);
        gate.release();
        refreshing.join().expect("the refresh");
        later
    });
    assert_eq!(later.state, LicenseState::Licensed);
    assert!(later.stalled);
    assert_eq!(
        (
            later.meters["exports"].left,
            later.meters["exports"].unsynced
        ),
        (88, 2)
    );
}

#[test]
fn spends_behind_a_call_that_has_not_finished_asks_saves_nothing_of_the_answer_and_keeps_the_spend()
{
    let h = holding_with(meters(), behind_opts(), |_| {});
    h.http
        .answer("validate", 403, json!({ "error": "revoked" }));
    let gate = h.storage.hold_next_write();
    h.http.on("use", |_| spent(89, NOW));
    std::thread::scope(|scope| {
        let refreshing = scope.spawn(|| h.license.refresh());
        let _releasing = gate.releasing();
        gate.await_reached();
        let writes = h.storage.writes();
        let first = h.license.use_meter("exports", 1);
        assert_eq!(first, UseResult::spent(89, false));
        assert_eq!(h.http.count("use"), 1);
        // Offline now: the spend is kept in memory and counts.
        h.http.fail(true);
        let second = h.license.use_meter("exports", 2);
        // The answer the first call was given was not saved either, so the lease's own figure is the base.
        assert_eq!(second, UseResult::spent(88, true));
        assert_eq!(h.storage.writes(), writes);
        gate.release();
        // The call that was stuck finishes with its verdict, and the spend kept in memory lands beside it.
        refreshing.join().expect("the refresh");
    });
    assert_eq!(h.state().refused, Some(Refusal::Revoked));
    h.license.status(false);
    // The second call was asked (no wait ran) and got no answer: its spend is in doubt, and kept.
    assert_eq!(
        kinds_and_counts(&journal_of(&h)),
        [(SpendKind::Unanswered, 2)]
    );
}

#[test]
fn is_absent_on_every_status_but_a_keys_licensed_one() {
    let expired = holding();
    expired.set_clock(NOW + 40 * 86_400_000);
    expired.http.fail(true);
    let status = expired.license.status(false);
    assert_eq!(status.state, LicenseState::Expired);
    assert!(status.meters.is_empty());
    let revoked = holding_with(meters(), Opts::default(), |state| {
        state.refused = Some(Refusal::Revoked)
    });
    let status = revoked.license.status(false);
    assert_eq!(status.state, LicenseState::Invalid);
    assert!(status.meters.is_empty());
    assert!(Harness::empty().license.status(false).meters.is_empty());
}

#[test]
fn is_absent_on_a_trial_whose_lease_carries_meters_and_on_an_offline_licence_file_that_does() {
    let trial = Harness::empty();
    let claim = keygrant::lease::fingerprint_claim("fp-test");
    let lease = trial.key.lease_with(
        NOW,
        5 * 86_400,
        json!({ "sub": format!("trial:{claim}"), "aid": claim, "model": "trial", "met": meters() }),
    );
    trial.http.answer(
        "trial",
        200,
        json!({ "trialStart": NOW, "trialEndsAt": NOW + 5 * 86_400_000, "lease": lease }),
    );
    let started = trial.license.start_trial();
    assert_eq!(started.state, LicenseState::Trial);
    assert!(started.meters.is_empty());
    assert!(trial.license.status(false).meters.is_empty());

    let file = holding();
    let token = file.key.offline_file(json!({ "met": meters() }));
    file.license
        .import_license_file(license_file_text(&token))
        .expect("imported");
    // The file answers before the key beside it, and a file has no meters, so neither has the status or a spend.
    assert_eq!(file.license.status(false), file_licensed());
    assert_eq!(
        file.license.use_meter("exports", 1),
        UseResult::refused(UseError::NotLicensed, None)
    );
}

#[test]
fn keeps_a_copy_of_its_own_in_each_status() {
    let h = holding();
    let mut a = h.license.status(false);
    let b = h.license.status(false);
    a.meters.clear();
    assert_eq!(b.meters.len(), 2);
    assert_eq!(h.license.status(false).meters.len(), 2);
}

#[test]
fn shows_a_meter_named_proto_or_like_an_objects_member_as_a_meter_like_any_other() {
    let h = Harness::empty();
    let lease = raw_lease(
        &h.key,
        r#"{"__proto__":{"left":1,"total":2,"offline":0},"constructor":{"left":3,"total":3,"offline":1}}"#,
    );
    h.set_state(stored_with(&lease));
    let meters = h.license.status(false).meters;
    assert_eq!(
        meters.keys().map(String::as_str).collect::<Vec<_>>(),
        ["__proto__", "constructor"]
    );
    assert_eq!(meters["__proto__"], shown(1, 2, 0, 0, false));
}

#[test]
fn takes_a_figure_a_use_answer_gave_in_place_of_the_leases_until_the_lease_is_renewed() {
    let with_figure = |at: i64| {
        let figure = MeterFigure {
            left: 40,
            total: 100,
            offline: 10,
            resets: false,
            at,
        };
        holding_with(meters(), Opts::default(), |state| {
            state.meter_figures = Some(BTreeMap::from([("exports".to_owned(), figure)]));
        })
    };
    assert_eq!(
        with_figure(NOW + 1000).license.status(false).meters["exports"].left,
        40
    );
    // A figure from before the lease's own second is older than it; from the same second, newer.
    assert_eq!(
        with_figure(NOW - 1000).license.status(false).meters["exports"].left,
        90
    );
    assert_eq!(
        with_figure(NOW).license.status(false).meters["exports"].left,
        40
    );
}

#[test]
fn shows_a_renewals_meters_net_of_the_journal_after_what_the_answer_confirmed() {
    let h = holding_journal(vec![
        entry_of(&id("A"), SpendKind::Offline, 2, LONG_AGO),
        entry_of(&id("B"), SpendKind::Void, 1, LONG_AGO),
    ]);
    let lease = metered_lease_at(
        &h.key,
        json!({ "exports": { "left": 70, "total": 100, "offline": 10 } }),
        NOW + 5000,
        Value::Null,
    );
    h.http.answer(
        "validate",
        200,
        json!({ "lease": lease, "spends": [{ "id": id("A"), "n": 2 }] }),
    );
    h.set_clock(NOW + 5000);
    assert_eq!(
        h.license.refresh().meters,
        BTreeMap::from([("exports".to_owned(), shown(70, 100, 10, 0, false))])
    );
}

#[test]
fn stops_showing_a_meter_the_newest_lease_no_longer_names() {
    let figure = MeterFigure {
        left: 1,
        total: 5,
        offline: 0,
        resets: true,
        at: NOW - 5000,
    };
    let h = holding_with(meters(), Opts::default(), |state| {
        state.meter_figures = Some(BTreeMap::from([("renders".to_owned(), figure)]));
    });
    let lease = metered_lease(
        &h.key,
        json!({ "exports": { "left": 70, "total": 100, "offline": 10 } }),
    );
    let mut stored = h.state();
    stored.lease = Some(lease);
    h.set_state(stored);
    let names: Vec<String> = h.license.status(false).meters.into_keys().collect();
    assert_eq!(names, ["exports"]);
}

#[test]
fn spends_on_an_open_ended_lease_as_on_any_other() {
    let h = Harness::empty();
    let lease = metered_lease_at(
        &h.key,
        meters(),
        NOW,
        json!({ "model": "perpetual", "major": keygrant::lease::OPEN_MAJOR }),
    );
    h.set_state(stored_with(&lease));
    h.http.on("use", |_| spent(89, NOW));
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::spent(89, false)
    );
}

// --- an entry the server did not confirm ------------------------------------------------------------------------

#[test]
fn an_entry_the_server_did_not_confirm_is_reported_again_with_the_same_id_meter_count_and_time() {
    let held = entry_of(&id("A"), SpendKind::Offline, 3, LONG_AGO);
    let report = json!({ "id": held.id, "meter": held.meter, "n": held.n, "at": held.at });
    let answers: [(&str, Answer); 5] = [
        ("a lease naming nothing", Box::new(renewing)),
        (
            "a 503",
            Box::new(|h| h.http.answer("validate", 503, json!({ "error": "down" }))),
        ),
        (
            "a 429",
            Box::new(|h| {
                h.http
                    .answer("validate", 429, json!({ "error": "rate-limited" }))
            }),
        ),
        ("no answer at all", Box::new(|h| h.http.fail(true))),
        (
            "a count lower than the entry holds",
            Box::new(|h| {
                let lease = metered_lease(&h.key, meters());
                h.http.answer(
                    "validate",
                    200,
                    json!({ "lease": lease, "spends": [{ "id": id("A"), "n": 2 }] }),
                );
            }),
        ),
    ];
    for (name, answer) in answers {
        let h = holding_journal(vec![held.clone()]);
        answer(&h);
        h.license.refresh();
        assert_eq!(
            journal_of(&h).as_slice(),
            std::slice::from_ref(&held),
            "{name}"
        );
        // Past every wait, the next validate and the next use carry it as it was.
        h.http.fail(false);
        h.set_clock(NOW + 2 * HOUR);
        renewing(&h);
        h.license.refresh();
        let last = h.http.count("validate") - 1;
        assert_eq!(reported(&h, last), Some(json!([report.clone()])), "{name}");
        h.http.on("use", |_| spent(89, NOW));
        h.license.use_meter("exports", 1);
        assert_eq!(use_body(&h, 0)["spends"], json!([report.clone()]), "{name}");
    }
}

#[test]
fn carries_the_id_a_failed_use_sent_into_the_next_requests_report() {
    let h = holding();
    h.http.fail(true);
    h.license.use_meter("exports", 2);
    let sent = use_body(&h, 0)["id"].as_str().expect("an id").to_owned();
    h.http.fail(false);
    h.set_clock(NOW + 2 * MINUTE);
    renewing(&h);
    h.license.refresh();
    assert_eq!(
        reported(&h, 0),
        Some(json!([{ "id": sent, "meter": "exports", "n": 2, "at": NOW }]))
    );
    h.http.on("use", |_| spent(89, NOW));
    h.license.use_meter("exports", 1);
    assert_eq!(use_body(&h, 1)["spends"][0]["id"], sent.as_str());
}

// --- a request that carries no report -------------------------------------------------------------------------------

#[test]
fn sends_no_spends_with_an_activation_or_a_trial_though_the_journal_holds_some() {
    let h = holding_journal(vec![entry(SpendKind::Offline, 1, LONG_AGO)]);
    let lease = metered_lease(&h.key, meters());
    h.http.answer(
        "activate",
        200,
        json!({ "lease": lease, "activationId": "act_1" }),
    );
    h.license.activate(KEY).expect("activated");
    h.license.start_trial();
    assert_eq!(h.http.count("trial"), 1);
    assert!(h.http.body_of("activate").get("spends").is_none());
    assert!(h.http.body_of("trial").get("spends").is_none());
}

// --- a full journal -------------------------------------------------------------------------------------------------

#[test]
fn a_full_journal_refuses_a_spend_the_allowance_covers_though_the_request_was_sent_and_appends_no_void(
) {
    let full: Vec<SpendEntry> = (0..MAX_PENDING_SPENDS)
        .map(|i| SpendEntry {
            id: format!("{i:q>22}"),
            meter: format!("m{i}"),
            n: 1,
            at: LONG_AGO,
            kind: SpendKind::Unanswered,
        })
        .collect();
    let h = holding_journal(full);
    h.http.fail(true);
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::refused(UseError::OfflineLimit, Some(90))
    );
    assert_eq!(h.http.count("use"), 1);
    let journal = journal_of(&h);
    assert_eq!(journal.len(), MAX_PENDING_SPENDS);
    assert!(journal.iter().all(|e| e.kind != SpendKind::Void));
}

// --- the bodies the reference's own worker parses -----------------------------------------------------------------

#[test]
fn the_requests_that_carry_a_report_are_the_bodies_the_reference_sends() {
    let Some(captured) = wire_bodies() else {
        return;
    };
    let h = Harness::new(
        None,
        Opts {
            major: Some(3),
            ..Opts::default()
        },
    );
    let met = meters();
    let lease = h.key.lease_with(
        NOW,
        30 * 86_400,
        json!({ "model": "subscription", "major": 3, "met": met.clone() }),
    );
    let mut stored = stored_with(&lease);
    stored.spends = Some(vec![
        entry_of(
            "offlineoffline0000001",
            SpendKind::Offline,
            3,
            NOW - 2 * HOUR,
        ),
        SpendEntry {
            meter: "renders".into(),
            ..entry_of(
                "unansweredunanswered2",
                SpendKind::Unanswered,
                1,
                NOW - 2 * HOUR + 1,
            )
        },
        entry_of(
            "voidvoidvoidvoid00003",
            SpendKind::Void,
            2,
            NOW - 2 * HOUR + 2,
        ),
    ]);
    h.set_state(stored);
    // Answers that confirm nothing, so the journal is still there for the next request.
    h.http.on("use", move |_| {
        reply(
            200,
            json!({ "spent": true, "meters": met.clone(), "at": NOW }),
        )
    });
    h.http.answer("validate", 200, json!({}));
    h.http.answer("deactivate", 200, json!({}));
    h.license.use_meter("exports", 2);
    h.license.refresh();
    h.license.deactivate().expect("released");

    // The spend's id is random, and the key ids are this suite's own key's.
    let mut sent = h.http.body_of("use");
    let id = sent["id"].as_str().expect("an id").to_owned();
    assert_eq!(id.len(), 22);
    sent["id"] = captured["use"]["id"].clone();
    for (name, mut body) in [("use", sent), ("validate", h.http.body_of("validate"))] {
        assert_eq!(body["kids"], json!([h.key.kid()]), "{name}");
        body.as_object_mut().expect("an object").remove("kids");
        let mut expected = captured[name].clone();
        expected.as_object_mut().expect("an object").remove("kids");
        assert_eq!(body, expected, "{name}");
    }
    assert_eq!(h.http.body_of("deactivate"), captured["deactivate"]);
}

/// The request bodies the reference captured (`wire-bodies.json`, beside the
/// reference's generator in the repository that makes them): none outside it,
/// where the test is skipped; inside it a missing file fails the test.
fn wire_bodies() -> Option<Value> {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let find = |relative: &[&str]| {
        manifest.ancestors().find_map(|dir| {
            let mut plain = dir.to_path_buf();
            let mut under_sdk = dir.join("sdk");
            for part in relative {
                plain.push(part);
                under_sdk.push(part);
            }
            [plain, under_sdk]
                .into_iter()
                .find(|candidate| candidate.is_file())
        })
    };
    match find(&["conformance", "wire-bodies.json"]) {
        Some(path) => {
            let text = std::fs::read_to_string(path).expect("the captured bodies");
            Some(serde_json::from_str(&text).expect("JSON"))
        }
        None if find(&["scripts", "export-sdk.mjs"]).is_some() => {
            panic!("conformance/wire-bodies.json was not found above the crate: this repository makes it")
        }
        None => {
            eprintln!("skipped: the captured request bodies are not in this checkout");
            None
        }
    }
}
