//! `use_meter(meter, n)` when nothing answers: the allowance, the journal and
//! its waits, what is stored of a spend, and the state a licence leaves behind.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use keygrant::meters::{MAX_PENDING_SPENDS, PENDING_MAX_MS};
use keygrant::offline::{ASK_AGAIN_MS, RETRY_UNANSWERED_MS};
use keygrant::{
    AdapterError, DeviceIdentity, DeviceKind, Fingerprinter, HttpResponse, LicenseState,
    MeterFigure, SpendEntry, SpendKind, StorageAdapter, StoredState, UseError, UseResult,
    MAX_METER_USES,
};
use serde_json::{json, Value};

fn offline_limit(left: u64) -> UseResult {
    UseResult::refused(UseError::OfflineLimit, Some(left))
}

fn no_meter() -> UseResult {
    UseResult::refused(UseError::NoMeter, None)
}

fn limited(offline: u64) -> Harness {
    holding_with(
        json!({ "exports": { "left": 90, "total": 100, "offline": offline } }),
        Opts::default(),
        |_| {},
    )
}

// --- with no answer -------------------------------------------------------------------

#[test]
fn spends_from_the_allowance_journals_the_spend_under_the_id_it_sent_and_waits_a_minute() {
    let h = holding();
    h.http.fail(true);
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::spent(89, true)
    );
    let sent = use_body(&h, 0)["id"].as_str().expect("an id").to_owned();
    assert_eq!(
        journal_of(&h),
        vec![SpendEntry {
            id: sent,
            meter: "exports".into(),
            n: 1,
            at: NOW,
            kind: SpendKind::Unanswered
        }]
    );
    let stored = h.state();
    assert_eq!(stored.backoff_since, Some(NOW));
    assert_eq!(stored.backoff_ms, Some(RETRY_UNANSWERED_MS));
    // Inside the wait it asks nothing: the spend is made with no request, and grows one entry.
    assert_eq!(
        h.license.use_meter("exports", 2),
        UseResult::spent(87, true)
    );
    assert_eq!(h.http.count("use"), 1);
    assert_eq!(
        kinds_and_counts(&journal_of(&h)),
        [(SpendKind::Unanswered, 1), (SpendKind::Offline, 2)]
    );
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::spent(86, true)
    );
    assert_eq!(
        kinds_and_counts(&journal_of(&h)),
        [(SpendKind::Unanswered, 1), (SpendKind::Offline, 3)]
    );
}

#[test]
fn refuses_a_spend_past_the_allowance_and_past_what_is_left() {
    let h = limited(3);
    h.http.fail(true);
    assert_eq!(
        h.license.use_meter("exports", 2),
        UseResult::spent(88, true)
    );
    assert_eq!(h.license.use_meter("exports", 2), offline_limit(88));
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::spent(87, true)
    );
    assert_eq!(h.license.use_meter("exports", 1), offline_limit(87));
    let low = holding_with(
        json!({ "exports": { "left": 2, "total": 100, "offline": 50 } }),
        Opts::default(),
        |_| {},
    );
    low.http.fail(true);
    assert_eq!(low.license.use_meter("exports", 3), offline_limit(2));
    assert_eq!(
        low.license.use_meter("exports", 2),
        UseResult::spent(0, true)
    );
    assert_eq!(low.license.use_meter("exports", 1), offline_limit(0));
}

#[test]
fn refuses_every_spend_with_no_answer_on_a_meter_whose_allowance_is_0() {
    let h = holding();
    h.http.fail(true);
    assert_eq!(h.license.use_meter("renders", 1), offline_limit(5));
    // Online it spends as any other.
    h.http.fail(false);
    h.set_clock(NOW + 2 * MINUTE);
    h.http.answer(
        "use",
        200,
        json!({
            "spent": true, "at": NOW + 2 * MINUTE,
            "meters": { "renders": { "left": 4, "total": 5, "offline": 0, "resets": true } },
        }),
    );
    assert_eq!(
        h.license.use_meter("renders", 1),
        UseResult::spent(4, false)
    );
}

#[test]
fn journals_a_refusal_of_a_spend_that_was_sent_as_a_void_for_the_server_to_take_back() {
    let h = limited(1);
    h.http.fail(true);
    h.license.use_meter("exports", 1);
    h.set_clock(NOW + 2 * MINUTE);
    assert_eq!(h.license.use_meter("exports", 1), offline_limit(89));
    let journal = journal_of(&h);
    assert_eq!((journal[0].kind, journal[0].n), (SpendKind::Unanswered, 1));
    assert_eq!(
        journal[1],
        SpendEntry {
            id: use_body(&h, 1)["id"].as_str().expect("an id").to_owned(),
            meter: "exports".into(),
            n: 1,
            at: NOW + 2 * MINUTE,
            kind: SpendKind::Void
        }
    );
}

#[test]
fn asks_again_for_a_spend_the_allowance_refuses_though_the_minute_has_not_passed() {
    let h = limited(1);
    h.http.fail(true);
    h.license.use_meter("exports", 1);
    // Inside the wait, but the allowance cannot cover it: asked all the same, so an online customer is not refused.
    assert_eq!(h.license.use_meter("exports", 1), offline_limit(89));
    assert_eq!(h.http.count("use"), 2);
    let kinds: Vec<SpendKind> = journal_of(&h).iter().map(|e| e.kind).collect();
    assert_eq!(kinds, [SpendKind::Unanswered, SpendKind::Void]);
}

#[test]
fn serves_a_customer_who_is_back_online_at_once_though_the_last_outage_left_a_wait() {
    let h = holding();
    h.http.fail(true);
    // The outage: a spend the allowance covers is made with no answer, and the minute's wait begins.
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::spent(89, true)
    );
    h.http.fail(false);
    h.http.answer(
        "use",
        200,
        json!({ "spent": true, "at": NOW, "meters": { "renders": { "left": 4, "total": 5, "offline": 0, "resets": true } } }),
    );
    assert_eq!(
        h.license.use_meter("renders", 1),
        UseResult::spent(4, false)
    );
    h.http.on("use", |_| spent(70, NOW));
    // The server's 70 less the one spend not yet reported.
    assert_eq!(
        h.license.use_meter("exports", 50),
        UseResult::spent(69, false)
    );
    h.http.answer(
        "use",
        200,
        json!({ "spent": true, "at": NOW, "meters": { "credits": { "left": 9, "total": 10, "offline": 0 } } }),
    );
    // A meter the lease lacks, which the server knows.
    assert_eq!(
        h.license.use_meter("credits", 1),
        UseResult::spent(9, false)
    );
    // A spend the allowance covers still waits out the minute.
    let asked = h.http.count("use");
    assert!(h.license.use_meter("exports", 1).offline);
    assert_eq!(h.http.count("use"), asked);
}

#[test]
fn counts_a_5xx_and_a_429_as_no_answer_saves_nothing_of_them_and_journals_the_spend_in_doubt() {
    for (status, body) in [
        (500, json!({ "error": "boom" })),
        (503, Value::Null),
        (429, json!({ "error": "rate-limited" })),
    ] {
        let h = holding();
        h.http.on("use", move |_| HttpResponse {
            status,
            json: Some(body.clone()).filter(|json| !json.is_null()),
        });
        assert_eq!(
            h.license.use_meter("exports", 1),
            UseResult::spent(89, true),
            "{status}"
        );
        let first = &journal_of(&h)[0];
        assert_eq!(
            (first.kind, first.n),
            (SpendKind::Unanswered, 1),
            "{status}"
        );
        assert_eq!(h.state().backoff_since, None, "{status}");
    }
}

#[test]
fn counts_any_answer_it_cannot_use_as_no_answer_too_and_saves_nothing_of_it() {
    let answers = [
        (200, json!({ "spent": false })),
        (200, json!("ok")),
        (200, Value::Null),
        (400, json!({ "error": "invalid-request" })),
        (404, json!({ "error": "invalid-key" })),
        (403, json!({ "error": "something-new" })),
        (401, json!("denied")),
    ];
    for (status, body) in answers {
        let h = holding();
        let label = format!("{status} {body}");
        h.http.on("use", move |_| HttpResponse {
            status,
            json: Some(body.clone()).filter(|json| !json.is_null()),
        });
        assert_eq!(
            h.license.use_meter("exports", 1),
            UseResult::spent(89, true),
            "{label}"
        );
        assert_eq!(journal_of(&h)[0].kind, SpendKind::Unanswered, "{label}");
        let stored = h.state();
        assert_eq!(stored.backoff_since, None, "{label}");
        assert_eq!(stored.refused, None, "{label}");
    }
}

#[test]
fn does_not_take_the_confirmations_of_an_answer_it_cannot_use() {
    let held = entry_of("EEEEEEEEEEEEEEEEEEEEEE", SpendKind::Unanswered, 1, NOW);
    let h = holding_journal(vec![held]);
    h.http.answer(
        "use",
        500,
        json!({ "spends": [{ "id": "EEEEEEEEEEEEEEEEEEEEEE", "n": 1 }] }),
    );
    h.set_clock(NOW + 2 * MINUTE);
    h.license.use_meter("exports", 1);
    // It asked, and the entry the answer named is still there.
    assert_eq!(h.http.count("use"), 1);
    assert!(journal_of(&h)
        .iter()
        .any(|e| e.id == "EEEEEEEEEEEEEEEEEEEEEE"));
}

#[test]
fn asks_again_once_the_minute_is_over_and_keeps_the_journals_wait_doubling_to_an_hour() {
    let h = holding_with(
        json!({ "exports": { "left": 1_000_000, "total": 1_000_000, "offline": 1_000_000 } }),
        Opts::default(),
        |_| {},
    );
    h.http.fail(true);
    let mut at = NOW;
    for (i, wait) in [1, 2, 4, 8, 16, 32, 60, 60].into_iter().enumerate() {
        h.set_clock(at);
        h.license.use_meter("exports", 1);
        assert_eq!(h.http.count("use"), i + 1, "ask {}", i + 1);
        // One millisecond short of the wait: not asked.
        h.set_clock(at + wait * MINUTE - 1);
        h.license.use_meter("exports", 1);
        assert_eq!(h.http.count("use"), i + 1, "short of wait {}", i + 1);
        at += wait * MINUTE;
    }
}

#[test]
fn keeps_the_journals_wait_when_the_device_clock_is_set_back() {
    let h = holding();
    h.http.fail(true);
    h.set_clock(NOW + 10 * MINUTE);
    h.license.use_meter("exports", 1);
    h.set_clock(NOW);
    h.license.use_meter("exports", 1);
    assert_eq!(h.http.count("use"), 1);
}

#[test]
fn asks_at_once_when_a_stored_no_answer_wait_begins_after_the_device_clock() {
    let h = holding_with(meters(), Opts::default(), |state| {
        state.backoff_since = Some(NOW + 10 * MINUTE);
        state.backoff_ms = Some(RETRY_UNANSWERED_MS);
    });
    h.http.on("use", |_| spent(89, NOW));
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::spent(89, false)
    );
}

#[test]
fn asks_when_the_newest_unanswered_spend_was_opened_after_the_guarded_clock() {
    // It jumped ahead and was corrected: asked, not stopped for days.
    let ahead = holding_journal(vec![entry(SpendKind::Unanswered, 1, NOW + 5 * 86_400_000)]);
    ahead.http.on("use", |_| spent(89, NOW));
    assert_eq!(
        ahead.license.use_meter("exports", 1),
        UseResult::spent(88, false)
    );
    assert_eq!(ahead.http.count("use"), 1);
    // One opened at the guarded clock waits as it would.
    let now = holding_journal(vec![entry(SpendKind::Unanswered, 1, NOW)]);
    now.http.on("use", |_| spent(89, NOW));
    now.license.use_meter("exports", 1);
    assert_eq!(now.http.count("use"), 0);
}

#[test]
fn does_not_wait_on_a_longer_wait_a_validate_set_the_use_endpoint_is_asked_all_the_same() {
    let h = holding_with(meters(), Opts::default(), |state| {
        state.backoff_since = Some(NOW);
        state.backoff_ms = Some(ASK_AGAIN_MS);
    });
    h.http.on("use", |_| spent(89, NOW));
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::spent(89, false)
    );
}

#[test]
fn refuses_a_meter_the_key_has_no_figure_for_as_no_meter_and_journals_a_void_when_in_doubt() {
    let h = holding();
    h.http.fail(true);
    assert_eq!(h.license.use_meter("credits", 1), no_meter());
    let first = &journal_of(&h)[0];
    assert_eq!(
        (first.kind, first.meter.as_str(), first.n),
        (SpendKind::Void, "credits", 1)
    );
    // A wait running does not stop the ask: the server may know a meter the lease does not name.
    let w = holding_with(meters(), Opts::default(), |state| {
        state.backoff_since = Some(NOW);
        state.backoff_ms = Some(RETRY_UNANSWERED_MS);
    });
    w.http.fail(true);
    assert_eq!(w.license.use_meter("credits", 1), no_meter());
    assert_eq!(w.http.count("use"), 1);
    assert_eq!(journal_of(&w)[0].kind, SpendKind::Void);
}

fn full_journal() -> Vec<SpendEntry> {
    (0..MAX_PENDING_SPENDS)
        .map(|i| SpendEntry {
            id: format!("{i:x>22}"),
            meter: format!("m{i}"),
            n: 1,
            at: NOW - 1,
            kind: SpendKind::Unanswered,
        })
        .collect()
}

#[test]
fn fills_the_journal_to_64_and_refuses_the_65th_spend_as_past_the_allowance() {
    let h = holding_with(meters(), Opts::default(), |state| {
        state.spends = Some(full_journal());
        state.backoff_since = Some(NOW);
        state.backoff_ms = Some(RETRY_UNANSWERED_MS);
    });
    // A new entry has no room: the spend is refused, though the allowance had it.
    assert_eq!(h.license.use_meter("exports", 1), offline_limit(90));
    assert_eq!(journal_of(&h).len(), MAX_PENDING_SPENDS);
}

#[test]
fn lets_an_offline_entry_that_is_already_open_grow_when_the_journal_is_full() {
    let mut full = full_journal();
    full.truncate(MAX_PENDING_SPENDS - 1);
    full.insert(0, entry_of(&"x".repeat(22), SpendKind::Offline, 2, NOW));
    let h = holding_with(meters(), Opts::default(), |state| {
        state.spends = Some(full);
        state.backoff_since = Some(NOW);
        state.backoff_ms = Some(RETRY_UNANSWERED_MS);
    });
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::spent(87, true)
    );
    let journal = journal_of(&h);
    assert_eq!(journal.len(), MAX_PENDING_SPENDS);
    assert_eq!(journal[0].n, 3);
}

#[test]
fn waits_the_hour_after_a_journal_of_64_unanswered_entries_with_nothing_overflowing() {
    // 64 entries: 2^63 minutes would overflow; the wait is the hour, computed with its cap first.
    let opened = NOW - 10 * MINUTE;
    let spends: Vec<SpendEntry> = (0..64)
        .map(|i| SpendEntry {
            id: format!("{i:u>22}"),
            meter: "credits".into(),
            n: 1,
            at: opened + i,
            kind: SpendKind::Unanswered,
        })
        .collect();
    let newest = opened + 63;
    let h = holding_with(
        json!({ "exports": { "left": 90, "total": 100, "offline": 10 } }),
        Opts::default(),
        |state| state.spends = Some(spends),
    );
    h.http.fail(true);
    h.set_clock(newest + 60 * MINUTE - 1);
    h.license.use_meter("exports", 1);
    assert_eq!(h.http.count("use"), 0, "an hour has not passed");
    h.set_clock(newest + 60 * MINUTE);
    h.license.use_meter("exports", 1);
    assert_eq!(h.http.count("use"), 1, "an hour has passed");
}

// --- the journal ------------------------------------------------------------------------------------

#[test]
fn survives_a_save_that_fails_it_counts_toward_the_allowance_and_the_status_and_lands_with_the_next_save(
) {
    let h = limited(2);
    h.http.fail(true);
    h.storage.fail_writes(true);
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::spent(89, true)
    );
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::spent(88, true)
    );
    assert!(journal_of(&h).is_empty());
    // It counts: the allowance is spent, and the status shows it.
    assert_eq!(h.license.use_meter("exports", 1), offline_limit(88));
    let shown = h.license.status(false).meters["exports"];
    assert_eq!(
        (
            shown.left,
            shown.total,
            shown.offline,
            shown.unsynced,
            shown.resets
        ),
        (88, 100, 2, 2, false)
    );
    h.storage.fail_writes(false);
    h.license.status(false);
    // The third call was asked, and in doubt: the server is told it is taken back.
    let journal = journal_of(&h);
    let unsynced: u64 = journal
        .iter()
        .filter(|e| e.kind != SpendKind::Void)
        .map(|e| e.n)
        .sum();
    assert_eq!(unsynced, 2);
    assert_eq!(
        journal.iter().filter(|e| e.kind == SpendKind::Void).count(),
        1
    );
    // Landed once: a failing disk again changes nothing the view shows.
    h.storage.fail_writes(true);
    assert_eq!(h.license.status(false).meters["exports"].unsynced, 2);
}

#[test]
fn lands_with_the_next_use_that_can_save_in_order() {
    let h = holding();
    h.http.fail(true);
    h.storage.fail_writes(true);
    h.license.use_meter("exports", 1);
    h.storage.fail_writes(false);
    h.http.fail(false);
    // The server records the report the request carries, and says so.
    h.http.on("use", |body| {
        let confirmed: Vec<Value> = body["spends"]
            .as_array()
            .map(|spends| {
                spends
                    .iter()
                    .map(|s| json!({ "id": s["id"], "n": s["n"] }))
                    .collect()
            })
            .unwrap_or_default();
        let mut answer = spent(88, NOW);
        answer.json.as_mut().expect("a body")["spends"] = Value::Array(confirmed);
        answer
    });
    h.set_clock(NOW + 2 * MINUTE);
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::spent(88, false)
    );
    // The earlier spend rode this request as a report, and its confirmation dropped it.
    assert_eq!(use_body(&h, 1)["spends"].as_array().map(Vec::len), Some(1));
    assert!(journal_of(&h).is_empty());
}

#[test]
fn lands_on_the_state_as_stored_now_so_a_second_processs_spend_is_never_lost() {
    // Through the storage's own merge (`update`): the other process saves between read and save.
    struct Updating {
        inner: Arc<MemoryStorage>,
        injected: Mutex<bool>,
    }
    impl StorageAdapter for Updating {
        fn read(&self, read_only: bool) -> std::io::Result<Option<StoredState>> {
            self.inner.read(read_only)
        }
        fn write(&self, state: &StoredState) -> std::io::Result<()> {
            self.inner.write(state)
        }
        fn update(
            &self,
            merge: &dyn Fn(StoredState) -> StoredState,
        ) -> Option<std::io::Result<()>> {
            let mut latest = self.inner.get().unwrap_or_default();
            let mut injected = lock(&self.injected);
            if !*injected {
                *injected = true;
                latest.spends = Some(vec![entry_of(
                    "OOOOOOOOOOOOOOOOOOOOOO",
                    SpendKind::Offline,
                    4,
                    NOW - 5,
                )]);
            }
            Some(self.inner.write(&merge(latest)))
        }
    }
    let h = limited(10);
    h.http.fail(true);
    let storage = Arc::new(Updating {
        inner: Arc::clone(&h.storage),
        injected: Mutex::new(false),
    });
    let app = keygrant::License::new(h.config.clone().storage(storage)).expect("a licence");
    // The first no-answer spend is journaled beside theirs; the second (in its wait) grows the offline entry that is there.
    assert!(app.use_meter("exports", 1).ok);
    assert!(app.use_meter("exports", 2).ok);
    let journal = journal_of(&h);
    let shown: Vec<(&str, u64)> = journal
        .iter()
        .map(|e| {
            let who = if e.id == "OOOOOOOOOOOOOOOOOOOOOO" {
                "theirs"
            } else {
                e.kind.as_str()
            };
            (who, e.n)
        })
        .collect();
    assert_eq!(shown, [("theirs", 6), ("unanswered", 1)]);
    // And the write-through path (a storage with no merge of its own) reads the file as it is at the save, not
    // as the call found it: the other process saves while this one's request is in flight.
    let h = limited(10);
    let file = Arc::clone(&h.storage);
    h.http.on("use", move |_| {
        let mut state = file.get().expect("a state");
        state.spends = Some(vec![entry_of(
            "OOOOOOOOOOOOOOOOOOOOOO",
            SpendKind::Offline,
            4,
            NOW - 5,
        )]);
        file.set(Some(state));
        reply(503, json!({}))
    });
    assert!(h.license.use_meter("exports", 1).ok);
    // Theirs is there as they left it, and this spend, in doubt, beside it.
    let journal = journal_of(&h);
    assert_eq!(
        kinds_and_counts(&journal),
        [(SpendKind::Offline, 4), (SpendKind::Unanswered, 1)]
    );
    assert_eq!(journal[0].id, "OOOOOOOOOOOOOOOOOOOOOO");
}

#[test]
fn keeps_an_entry_in_the_journal_of_the_activation_it_is_about_and_nothing_of_it_on_another_seat() {
    let figures = [(
        "exports".to_owned(),
        MeterFigure {
            left: 50,
            total: 100,
            offline: 10,
            resets: false,
            at: NOW,
        },
    )];
    let h = holding_with(meters(), Opts::default(), |state| {
        state.spends = Some(vec![entry(SpendKind::Offline, 2, NOW)]);
        state.meter_figures = Some(figures.into_iter().collect());
    });
    // The same seat activated again keeps it.
    let lease = metered_lease(&h.key, meters());
    h.http.answer(
        "activate",
        200,
        json!({ "lease": lease, "activationId": "act_1" }),
    );
    h.license.activate(KEY).expect("activated");
    assert_eq!(journal_of(&h).len(), 1);
    assert!(h.state().meter_figures.is_some());
    // Another seat of the same key starts afresh.
    let other = metered_lease_at(&h.key, meters(), NOW, json!({ "aid": "act_9" }));
    h.http.answer(
        "activate",
        200,
        json!({ "lease": other, "activationId": "act_9" }),
    );
    h.license.activate(KEY).expect("activated");
    let stored = h.state();
    assert_eq!(stored.activation_id.as_deref(), Some("act_9"));
    assert_eq!(stored.spends, None);
    assert_eq!(stored.meter_figures, None);
}

#[test]
fn drops_the_journal_of_an_old_activation_when_another_key_is_activated_unreported() {
    let h = holding_with(meters(), Opts::default(), |state| {
        state.spends = Some(vec![entry(SpendKind::Offline, 2, NOW)]);
    });
    let another = "SLUICE-OTHER-AAAA-BBBB-CCCC";
    let lease = metered_lease_at(
        &h.key,
        meters(),
        NOW,
        json!({ "sub": another, "aid": "act_2" }),
    );
    h.http.answer(
        "activate",
        200,
        json!({ "lease": lease, "activationId": "act_2" }),
    );
    h.license.activate(another).expect("activated");
    assert_eq!(h.http.count("validate"), 0);
    let stored = h.state();
    assert_eq!(stored.key.as_deref(), Some(another));
    assert_eq!(stored.spends, None);
}

#[test]
fn drops_entries_unreported_for_more_than_180_days_at_the_next_save_and_keeps_the_ones_inside() {
    let day = 86_400_000;
    let old = "o".repeat(22);
    let recent = "n".repeat(22);
    let h = holding_journal(vec![
        entry_of(&old, SpendKind::Unanswered, 1, NOW - 181 * day),
        entry_of(&recent, SpendKind::Unanswered, 1, NOW - 179 * day),
    ]);
    h.http.fail(true);
    h.license.use_meter("exports", 1);
    let ids: Vec<String> = journal_of(&h).into_iter().map(|e| e.id).collect();
    assert!(!ids.contains(&old));
    assert!(ids.contains(&recent));
    assert_eq!(PENDING_MAX_MS, 180 * day);
}

#[test]
fn keeps_an_entry_opened_exactly_180_days_ago() {
    let boundary = "b".repeat(22);
    let h = holding_journal(vec![entry_of(
        &boundary,
        SpendKind::Unanswered,
        1,
        NOW - PENDING_MAX_MS,
    )]);
    h.http.fail(true);
    h.license.use_meter("exports", 1);
    assert!(journal_of(&h).iter().any(|e| e.id == boundary));
    h.set_clock(NOW + 1);
    h.license.use_meter("exports", 1);
    assert!(journal_of(&h).iter().all(|e| e.id != boundary));
}

#[test]
fn reads_a_stored_journal_that_is_not_one_as_an_empty_journal_and_an_entry_of_another_shape_as_none(
) {
    let mut raw = json!({ "key": KEY, "activationId": "act_1", "lastSeen": NOW, "spends": "junk" });
    let h = Harness::empty();
    raw["lease"] = json!(metered_lease(&h.key, meters()));
    h.set_state(state(raw.clone()));
    h.http.fail(true);
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::spent(89, true)
    );

    raw["spends"] = json!([
        { "id": 3 }, null, "x",
        { "id": "AAAAAAAAAAAAAAAAAAAAAA", "meter": "exports", "n": 2, "at": NOW, "kind": "offline", "extra": true },
        { "id": "x", "meter": "exports", "n": 0, "at": 1, "kind": "offline" },
        { "id": "y", "meter": "exports", "n": 1.5, "at": 1, "kind": "offline" },
        { "id": "z", "meter": "exports", "n": 1, "at": -1, "kind": "offline" },
        { "id": "w", "meter": "exports", "n": 1, "at": 1, "kind": "later" },
    ]);
    let g = Harness::empty();
    g.set_state(state(raw));
    g.http.fail(true);
    assert_eq!(g.license.status(false).meters["exports"].unsynced, 2);
    // The next save that lands the journal writes it without them: the status above
    // asked and got no answer, so the minute's wait is running and the spend grows the entry.
    assert_eq!(
        g.license.use_meter("exports", 1),
        UseResult::spent(87, true)
    );
    assert_eq!(kinds_and_counts(&journal_of(&g)), [(SpendKind::Offline, 3)]);
    let stored = g.state().to_json();
    assert!(stored["spends"][0].get("extra").is_none());
}

#[test]
fn reads_stored_figures_of_another_shape_as_none() {
    let h = Harness::empty();
    let mut raw = json!({ "key": KEY, "activationId": "act_1", "lastSeen": NOW });
    raw["lease"] = json!(metered_lease(&h.key, meters()));
    raw["meterFigures"] = json!({
        "exports": { "left": 1, "total": 100, "offline": 10, "at": NOW + 1, "extra": 1 },
        "renders": { "left": -1, "total": 5, "offline": 0, "at": NOW + 1 },
        "credits": 7,
    });
    h.set_state(state(raw));
    let shown = h.license.status(false).meters;
    assert_eq!(shown["exports"].left, 1);
    // A figure of another shape is left out; the lease's own stands for that meter.
    assert_eq!(shown["renders"].left, 5);
    assert!(!shown.contains_key("credits"));
}

// --- the device ---------------------------------------------------------------------------------------

struct Machine(Vec<String>);

impl Fingerprinter for Machine {
    fn get(&self) -> Result<String, AdapterError> {
        Ok("fp-machine".into())
    }
    fn identify(&self, _timeout: std::time::Duration) -> Option<DeviceIdentity> {
        Some(DeviceIdentity {
            fingerprint: Some("fp-machine".into()),
            kind: Some(DeviceKind::Machine),
            alternates: self.0.clone(),
        })
    }
}

#[test]
fn names_the_device_as_a_validate_does_its_fingerprints_and_what_they_hash() {
    let machine: Arc<dyn Fingerprinter> = Arc::new(Machine(vec!["fp-host".into()]));
    let h = holding_with(
        meters(),
        Opts {
            fingerprint: Some(machine),
            ..Opts::default()
        },
        |_| {},
    );
    h.http.on("use", |_| spent(89, NOW));
    h.license.use_meter("exports", 1);
    let body = use_body(&h, 0);
    assert_eq!(body["fingerprintKind"], "machine");
    assert_eq!(body["fingerprints"].as_array().map(Vec::len), Some(2));
}

#[test]
fn keys_the_install_by_the_machine_id_it_read_when_the_request_gets_no_answer() {
    let machine: Arc<dyn Fingerprinter> = Arc::new(Machine(Vec::new()));
    let h = holding_with(
        meters(),
        Opts {
            fingerprint: Some(machine),
            ..Opts::default()
        },
        |_| {},
    );
    h.http.fail(true);
    h.license.use_meter("exports", 1);
    assert_eq!(h.state().device_kind, Some(DeviceKind::Machine));
}

#[test]
fn sends_no_fingerprints_when_the_device_cannot_be_told() {
    struct Unknown;
    impl Fingerprinter for Unknown {
        fn get(&self) -> Result<String, AdapterError> {
            Err("no id".into())
        }
    }
    let unknown: Arc<dyn Fingerprinter> = Arc::new(Unknown);
    let h = holding_with(
        meters(),
        Opts {
            fingerprint: Some(unknown),
            ..Opts::default()
        },
        |_| {},
    );
    h.http.on("use", |_| spent(89, NOW));
    h.license.use_meter("exports", 1);
    assert!(use_body(&h, 0).get("fingerprints").is_none());
}

// --- a meter never takes the licence away ---------------------------------------------------------------

#[test]
fn a_licence_stays_licensed_when_a_meter_is_used_up_absent_or_past_its_offline_limit() {
    let h = limited(1);
    h.http.fail(true);
    assert!(h.license.use_meter("exports", 1).ok);
    assert_eq!(h.license.use_meter("exports", 1), offline_limit(89));
    assert_eq!(h.license.use_meter("credits", 1), no_meter());
    h.http.fail(false);
    h.http
        .answer("use", 409, json!({ "error": "used-up", "left": 0 }));
    h.set_clock(NOW + HOUR);
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::refused(UseError::UsedUp, Some(0))
    );
    let status = h.license.status(false);
    assert_eq!(status.state, LicenseState::Licensed);
    assert!(status.permits_use());
    assert_eq!(
        h.license.use_meter("exports", MAX_METER_USES + 1).error,
        Some(UseError::InvalidAmount)
    );
}
