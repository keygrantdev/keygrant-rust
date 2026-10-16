//! `use_meter` beside a disk that refuses, a second process of the app, and a
//! call that has not finished: a spend the customer was told was made is kept
//! in memory and counted everywhere, a request reports the journal as stored,
//! and the allowance is judged again where a spend lands.

mod common;

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use keygrant::journal::JournalOp;
use keygrant::meters::{MAX_PENDING_SPENDS, MAX_PENDING_VOIDS};
use keygrant::offline::RETRY_UNANSWERED_MS;
use keygrant::storage::{FileStorage, FileStorageOptions};
use keygrant::{
    HttpResponse, License, MeterFigure, SpendEntry, SpendKind, StorageAdapter, StoredState,
    UseError, UseResult,
};
use serde_json::{json, Value};

fn offline_limit(left: u64) -> UseResult {
    UseResult::refused(UseError::OfflineLimit, Some(left))
}

fn waiting(state: &mut StoredState) {
    state.backoff_since = Some(NOW);
    state.backoff_ms = Some(RETRY_UNANSWERED_MS);
}

/// A storage that lets a second process save before this one's merge, and
/// misbehaves as it is told.
#[derive(Default)]
struct Racing {
    inner: Arc<MemoryStorage>,
    /// Entries another process saved just before this one's first merge.
    theirs: Mutex<Option<Vec<SpendEntry>>>,
    /// The other process's state has no figures.
    drop_figures: bool,
    /// Reads fail once a save has landed.
    fail_reads_after: bool,
    saved: AtomicBool,
}

impl Racing {
    fn over(h: &Harness, theirs: Vec<SpendEntry>) -> Racing {
        Racing {
            inner: Arc::clone(&h.storage),
            theirs: Mutex::new(Some(theirs)),
            ..Racing::default()
        }
    }

    fn app(self, h: &Harness) -> License {
        License::new(h.config.clone().storage(Arc::new(self))).expect("a licence")
    }
}

impl StorageAdapter for Racing {
    fn read(&self, read_only: bool) -> io::Result<Option<StoredState>> {
        if self.fail_reads_after && self.saved.load(Ordering::SeqCst) {
            return Err(io::Error::other("the disk is gone"));
        }
        self.inner.read(read_only)
    }
    fn write(&self, state: &StoredState) -> io::Result<()> {
        self.inner.write(state)
    }
    fn update(&self, merge: &dyn Fn(StoredState) -> StoredState) -> Option<io::Result<()>> {
        let mut latest = self.inner.get().unwrap_or_default();
        if let Some(theirs) = lock(&self.theirs).take() {
            latest.spends = Some(theirs);
            latest.rev = Some(9);
            if self.drop_figures {
                latest.meter_figures = None;
            }
        }
        let written = self.inner.write(&merge(latest));
        self.saved.store(true, Ordering::SeqCst);
        Some(written)
    }
}

// --- a spend held in memory -----------------------------------------------------------------

fn renewing(h: &Harness) {
    let lease = metered_lease(&h.key, meters());
    h.validate_with(&lease);
}

fn report_of(h: &Harness, nth: usize) -> Option<Value> {
    let call = h
        .http
        .calls()
        .into_iter()
        .filter(|(action, _)| action == "validate")
        .nth(nth)
        .expect("a validate");
    call.1.get("spends").cloned()
}

fn waiting_harness(met: Value) -> Harness {
    holding_with(met, Opts::default(), waiting)
}

#[test]
fn is_not_reported_until_it_has_landed_so_the_entry_it_joins_is_reported_with_it_once() {
    let h = waiting_harness(json!({ "exports": { "left": 90, "total": 100, "offline": 10 } }));
    renewing(&h);
    h.storage.fail_writes(true);
    assert_eq!(
        h.license.use_meter("exports", 2),
        UseResult::spent(88, true)
    );
    // Held in memory, it counts toward the status but no request carries it: it has no place in the journal yet.
    let shown = h.license.refresh().meters["exports"];
    assert_eq!((shown.left, shown.unsynced), (88, 2));
    assert_eq!(report_of(&h, 0), None);
    // Another process lands an entry, and the disk is back: the spend lands in it, and the entry is what is reported.
    let mut stored = h.state();
    stored.spends = Some(vec![entry_of(
        &"Y".repeat(22),
        SpendKind::Offline,
        1,
        NOW - 5,
    )]);
    h.set_state(stored);
    h.storage.fail_writes(false);
    h.license.refresh();
    assert_eq!(
        report_of(&h, 1),
        Some(json!([{ "id": "Y".repeat(22), "meter": "exports", "n": 3, "at": NOW - 5 }]))
    );
}

#[test]
fn is_reported_with_a_release_once_it_has_landed_as_everything_the_journal_holds() {
    let h = waiting_harness(json!({ "exports": { "left": 90, "total": 100, "offline": 10 } }));
    h.http.answer("deactivate", 200, json!({}));
    h.storage.fail_writes(true);
    h.license.use_meter("exports", 2);
    h.storage.fail_writes(false);
    h.license.deactivate().expect("released");
    let body = h.http.body_of("deactivate");
    assert_eq!(body["spends"][0]["meter"], "exports");
    assert_eq!(body["spends"][0]["n"], 2);
}

#[test]
fn reports_an_unanswered_spend_and_a_void_which_open_once_by_their_ids_and_withholds_only_the_offline_spends(
) {
    let h = holding();
    renewing(&h);
    h.storage.fail_writes(true);
    h.http.fail(true);
    // In doubt, then refused after asking: an unanswered spend, and a void. A third, made while the journal waits, never left the device.
    assert_eq!(
        h.license.use_meter("exports", 2),
        UseResult::spent(88, true)
    );
    assert_eq!(
        h.license.use_meter("renders", 1).error,
        Some(UseError::OfflineLimit)
    );
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::spent(87, true)
    );
    h.http.fail(false);
    h.license.refresh();
    let report = report_of(&h, 0).expect("a report");
    let shown: Vec<(String, u64, bool)> = report
        .as_array()
        .expect("a list")
        .iter()
        .map(|e| {
            (
                e["meter"].as_str().unwrap_or_default().to_owned(),
                e["n"].as_u64().unwrap_or(0),
                e["void"] == json!(true),
            )
        })
        .collect();
    assert_eq!(
        shown,
        [
            ("exports".to_owned(), 2, false),
            ("renders".to_owned(), 1, true)
        ]
    );
}

/// A storage that fails its saves as the test says: all of them, or the first of
/// a count, and lets a second process's entry in before one.
struct Flaky {
    inner: Arc<MemoryStorage>,
    failing: AtomicBool,
    saves: AtomicUsize,
    /// The save, counted from the first once the disk is back, that finds another process's entry.
    inject_at: usize,
}

impl StorageAdapter for Flaky {
    fn read(&self, read_only: bool) -> io::Result<Option<StoredState>> {
        self.inner.read(read_only)
    }
    fn write(&self, state: &StoredState) -> io::Result<()> {
        self.inner.write(state)
    }
    fn update(&self, merge: &dyn Fn(StoredState) -> StoredState) -> Option<io::Result<()>> {
        if self.failing.load(Ordering::SeqCst) {
            return Some(Err(io::Error::other("EBUSY: the disk said no")));
        }
        let save = self.saves.fetch_add(1, Ordering::SeqCst) + 1;
        // The first save of the next call fails; before the second another process lands an entry the held spend joins.
        if save == 1 {
            return Some(Err(io::Error::other("EBUSY: the disk said no")));
        }
        let mut latest = self.inner.get().unwrap_or_default();
        if save == self.inject_at {
            latest.spends = Some(vec![entry_of(
                &"Y".repeat(22),
                SpendKind::Offline,
                1,
                NOW - 5,
            )]);
        }
        Some(self.inner.write(&merge(latest)))
    }
}

#[test]
fn judges_a_view_that_carried_held_spends_by_what_is_stored_though_a_save_in_the_same_call_landed_them(
) {
    let h = Harness::empty();
    let lease = metered_lease_at(
        &h.key,
        json!({ "exports": { "left": 90, "total": 100, "offline": 10 } }),
        NOW,
        json!({ "model": "perpetual", "major": 1_000_000, "upgradeUntil": (NOW + 10 * 86_400_000) / 1000 }),
    );
    let mut stored = stored_with(&lease);
    waiting(&mut stored);
    h.set_state(stored);
    let storage = Arc::new(Flaky {
        inner: Arc::clone(&h.storage),
        failing: AtomicBool::new(true),
        saves: AtomicUsize::new(0),
        inject_at: 2,
    });
    let app = License::new(h.config.clone().storage(storage.clone())).expect("a licence");
    h.validate_with(&lease);
    assert_eq!(app.use_meter("exports", 2), UseResult::spent(88, true));
    storage.failing.store(false, Ordering::SeqCst);
    app.refresh();
    // The spend landed in that entry before the request was made; the request reports the entry, not the spend under an id of its own.
    assert_eq!(
        h.http.body_of("validate")["spends"],
        json!([{ "id": "Y".repeat(22), "meter": "exports", "n": 3, "at": NOW - 5 }])
    );
}

#[test]
fn does_not_make_status_ask_at_every_call_for_spends_held_in_memory_that_no_request_carries() {
    let h = waiting_harness(json!({ "exports": { "left": 90, "total": 100, "offline": 10 } }));
    renewing(&h);
    h.storage.fail_writes(true);
    h.license.use_meter("exports", 2);
    h.set_clock(NOW + 2 * MINUTE);
    for _ in 0..3 {
        h.license.status(false);
    }
    assert_eq!(h.http.count("validate"), 0);
    // Landed, the journal asks as it does, once.
    h.storage.fail_writes(false);
    h.license.status(false);
    assert_eq!(h.http.count("validate"), 1);
}

#[test]
fn is_not_reported_with_a_use_either_whose_own_spend_is_the_report_is_what_is_stored() {
    let h = holding_with(meters(), Opts::default(), waiting);
    h.storage.fail_writes(true);
    h.license.use_meter("exports", 2);
    h.http.answer(
        "use",
        200,
        json!({ "spent": true, "at": NOW, "meters": { "renders": { "left": 4, "total": 5, "offline": 0, "resets": true } } }),
    );
    assert_eq!(
        h.license.use_meter("renders", 1),
        UseResult::spent(4, false)
    );
    let body = use_body(&h, 0);
    assert_eq!(
        (body["meter"].as_str(), body["n"].as_u64()),
        (Some("renders"), Some(1))
    );
    assert!(body.get("spends").is_none());
}

#[test]
fn starts_the_hours_wait_after_an_answer_only_when_the_request_carried_a_report() {
    let h = waiting_harness(json!({ "exports": { "left": 90, "total": 100, "offline": 10 } }));
    renewing(&h);
    let storage = Arc::new(Flaky {
        inner: Arc::clone(&h.storage),
        failing: AtomicBool::new(true),
        saves: AtomicUsize::new(0),
        inject_at: 0,
    });
    let app = License::new(h.config.clone().storage(storage.clone())).expect("a licence");
    assert_eq!(app.use_meter("exports", 2), UseResult::spent(88, true));
    // The next call's first save fails and its answer's save lands the spend: it read a journal, and sent none of it.
    storage.failing.store(false, Ordering::SeqCst);
    app.refresh();
    assert_eq!(report_of(&h, 0), None);
    assert_eq!(journal_of(&h).len(), 1);
    assert_ne!(h.state().backoff_ms, Some(keygrant::offline::ASK_AGAIN_MS));
    // So the journal that has landed is offered at the next status() once the minute's wait is over, not an hour on.
    h.set_clock(NOW + 2 * MINUTE);
    app.status(false);
    assert_eq!(h.http.count("validate"), 2);
    assert_eq!(report_of(&h, 1).expect("a report")[0]["n"], 2);
}

// --- calls one at a time -----------------------------------------------------------------------

#[test]
fn runs_two_calls_made_together_one_after_the_other_each_a_spend_of_its_own() {
    let h = holding();
    h.http.fail(true);
    let (a, b) = std::thread::scope(|scope| {
        let a = scope.spawn(|| h.license.use_meter("exports", 3));
        let b = scope.spawn(|| h.license.use_meter("exports", 4));
        (a.join().expect("a call"), b.join().expect("a call"))
    });
    assert!(a.offline && b.offline && a.ok && b.ok);
    // Whichever ran first, the second saw what it left.
    let mut lefts = [a.left.expect("left"), b.left.expect("left")];
    lefts.sort_unstable();
    assert!(lefts == [83, 86] || lefts == [83, 87], "{lefts:?}");
    let spent: u64 = journal_of(&h).iter().map(|e| e.n).sum();
    assert_eq!(spent, 7);
}

#[test]
fn waits_behind_a_status_already_running_and_sees_what_it_saved() {
    let h = holding();
    let renewed = metered_lease_at(
        &h.key,
        json!({ "exports": { "left": 60, "total": 100, "offline": 10 } }),
        NOW + 1000,
        Value::Null,
    );
    h.set_clock(NOW + 1000);
    let gate = Gate::new();
    let held = gate.clone();
    let validated = Arc::new(AtomicBool::new(false));
    let done = Arc::clone(&validated);
    h.http.on("validate", move |_| {
        held.wait();
        done.store(true, Ordering::SeqCst);
        reply(200, json!({ "lease": renewed }))
    });
    // The spend's answer gives no figure: what is left can only come from the lease the status saved.
    let asked = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&asked);
    let saw = Arc::clone(&validated);
    h.http.on("use", move |_| {
        seen.lock()
            .expect("a lock")
            .push(saw.load(Ordering::SeqCst));
        reply(200, json!({ "spent": true }))
    });
    let spent_after = std::thread::scope(|scope| {
        let refreshing = scope.spawn(|| h.license.refresh());
        let _releasing = gate.releasing();
        gate.await_reached();
        let spending = scope.spawn(|| h.license.use_meter("exports", 1));
        // The spend is queued behind the status: it has asked nothing yet.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(h.http.count("use"), 0);
        gate.release();
        refreshing.join().expect("the status");
        spending.join().expect("the spend")
    });
    assert_eq!(spent_after, UseResult::spent(60, false));
    // It asked once, and after the status had its answer.
    assert_eq!(*asked.lock().expect("a lock"), [true]);
}

// --- a disk that keeps refusing --------------------------------------------------------------------

#[test]
fn costs_the_process_a_short_list_of_changes_however_many_spends_of_one_meter_it_holds_and_none_is_lost(
) {
    let h = holding_with(
        json!({ "exports": { "left": 1_000_000, "total": 1_000_000, "offline": 1_000_000 } }),
        Opts::default(),
        |_| {},
    );
    h.http.fail(true);
    h.storage.fail_writes(true);
    for _ in 0..400 {
        assert!(h.license.use_meter("exports", 1).ok);
    }
    let shown = h.license.status(false).meters["exports"];
    assert_eq!((shown.left, shown.unsynced), (1_000_000 - 400, 400));
    // However many spends of one meter it holds, a run of them is folded into one change.
    assert!(
        h.license.held_changes().len() < 10,
        "{}",
        h.license.held_changes().len()
    );
    h.storage.fail_writes(false);
    h.license.status(false);
    let spent: u64 = journal_of(&h).iter().map(|e| e.n).sum();
    assert_eq!(spent, 400);
}

#[test]
fn keeps_every_spend_of_two_meters_that_take_turns_and_lands_each_meters_total_when_the_disk_is_back(
) {
    // Only a run of one meter is folded into one change (a spend of the other between two is not
    // folded across), so this list grows with the spends; what must hold is that none is lost or counted twice.
    let h = holding_with(
        json!({
            "exports": { "left": 1_000, "total": 1_000, "offline": 1_000 },
            "renders": { "left": 1_000, "total": 1_000, "offline": 1_000 },
        }),
        Opts::default(),
        |_| {},
    );
    h.http.fail(true);
    h.storage.fail_writes(true);
    for turn in 0..100 {
        assert!(h.license.use_meter("exports", 1).ok, "turn {turn}");
        assert!(h.license.use_meter("renders", 2).ok, "turn {turn}");
    }
    let shown = h.license.status(false).meters;
    assert_eq!(shown["exports"].unsynced, 100);
    assert_eq!(shown["renders"].unsynced, 200);
    h.storage.fail_writes(false);
    h.license.status(false);
    let mut totals: BTreeMap<String, u64> = BTreeMap::new();
    for entry in journal_of(&h) {
        *totals.entry(entry.meter).or_default() += entry.n;
    }
    assert_eq!(
        totals,
        BTreeMap::from([("exports".to_owned(), 100), ("renders".to_owned(), 200)])
    );
    assert!(h.license.held_changes().is_empty());
}

#[test]
fn keeps_the_newest_voids_in_memory_none_of_them_told_and_refuses_no_covered_spend_for_them() {
    let h = holding();
    h.http.fail(true);
    h.storage.fail_writes(true);
    for minute in 0..20 {
        h.set_clock(NOW + minute * MINUTE);
        assert_eq!(
            h.license.use_meter("renders", 1),
            offline_limit(5),
            "minute {minute}"
        );
    }
    let held = h.license.held_changes();
    let voids: Vec<_> = held
        .iter()
        .filter_map(|op| match op {
            JournalOp::Spend(spend) if spend.kind == SpendKind::Void => Some(spend),
            _ => None,
        })
        .collect();
    assert_eq!(voids.len(), MAX_PENDING_VOIDS);
    assert!(
        voids.iter().all(|spend| !spend.told),
        "a void is no spend told"
    );
    // A spend the allowance covers is made, whatever the uncovered ones did to what this process holds.
    assert_eq!(
        h.license.use_meter("exports", 3),
        UseResult::spent(87, true)
    );
    h.storage.fail_writes(false);
    h.license.status(false);
    let stored = journal_of(&h);
    // A void is no spend told: it lands under the journal's rules, the newest sixteen of the twenty.
    let at: Vec<i64> = stored
        .iter()
        .filter(|e| e.kind == SpendKind::Void)
        .map(|e| e.at)
        .collect();
    let expected: Vec<i64> = (0..MAX_PENDING_VOIDS as i64)
        .map(|i| NOW + (4 + i) * MINUTE)
        .collect();
    assert_eq!(at, expected);
    let rest: Vec<(&str, u64)> = stored
        .iter()
        .filter(|e| e.kind != SpendKind::Void)
        .map(|e| (e.meter.as_str(), e.n))
        .collect();
    assert_eq!(rest, [("exports", 3)]);
}

// --- two instances of the app over one state file ---------------------------------------------------------

fn state_path() -> std::path::PathBuf {
    std::env::temp_dir()
        .join("keygrant-memory")
        .join("sluice.json")
}

fn over_file(memory: &MemoryFs) -> FileStorage {
    FileStorage::with_options(
        state_path(),
        FileStorageOptions {
            fs: Some(Arc::new(memory.clone())),
            sleep: Some(Arc::new(|_| {})),
            ..FileStorageOptions::default()
        },
    )
}

#[test]
fn a_deactivate_releases_all_the_same_over_a_file_whose_journal_is_not_one_and_writes_none_of_it_back(
) {
    let key = TestKey::new(7);
    let stored = json!({
        "key": KEY, "activationId": "act_1", "lastSeen": NOW,
        "lease": metered_lease(&key, meters()),
        "spends": "junk", "meterFigures": 7,
    });
    let memory = MemoryFs::new(&[(&state_path(), &stored.to_string())]);
    let http = Arc::new(FakeHttp::default());
    http.answer("deactivate", 200, json!({}));
    let config = keygrant::LicenseConfig::new(PRODUCT, "https://api.test", key.public_jwk())
        .storage(Arc::new(over_file(&memory)))
        .http(http.clone())
        .clock(FakeClock::at(NOW))
        .fingerprinter(Arc::new(FixedFingerprinter("fp-test")))
        .timings(patient_timings());
    let licence = License::new(config).expect("a licence");
    assert!(licence.deactivate().expect("released").released);
    assert_eq!(
        http.body_of("deactivate"),
        json!({ "key": KEY, "activationId": "act_1" })
    );
    // The junk went with the licence: the file written has none of it.
    let text = memory.file(&state_path()).expect("a state file");
    let saved: Value = serde_json::from_str(&text).expect("JSON");
    assert!(saved.get("key").is_none(), "{saved}");
    assert!(saved.get("spends").is_none(), "{saved}");
    assert!(saved.get("meterFigures").is_none(), "{saved}");
}

#[test]
fn two_instances_never_lose_each_others_spends_a_save_that_finds_the_file_changed_merges_onto_it() {
    let key = TestKey::new(7);
    let lease = metered_lease(
        &key,
        json!({ "exports": { "left": 1_000, "total": 1_000, "offline": 1_000 } }),
    );
    let stored = json!({ "key": KEY, "activationId": "act_1", "lease": lease, "lastSeen": NOW });
    let memory = MemoryFs::new(&[(&state_path(), &stored.to_string())]);
    let instance = || {
        let http = Arc::new(FakeHttp::default());
        http.fail(true);
        let config = keygrant::LicenseConfig::new(PRODUCT, "https://api.test", key.public_jwk())
            .storage(Arc::new(over_file(&memory)))
            .http(http)
            .clock(FakeClock::at(NOW))
            .fingerprinter(Arc::new(FixedFingerprinter("fp-test")))
            .timings(patient_timings());
        License::new(config).expect("a licence")
    };
    let (a, b) = (instance(), instance());
    // A's first save has read the file and is making its temp file: there, the other instance spends and saves,
    // so A's revision check finds the file changed and merges onto it.
    let gate = memory.hold_next(Op::Create);
    let first = std::thread::scope(|scope| {
        let running = scope.spawn(|| a.use_meter("exports", 5));
        let _releasing = gate.releasing();
        gate.await_reached();
        assert!(b.use_meter("exports", 7).offline);
        gate.release();
        running.join().expect("the first spend")
    });
    assert!(first.ok && first.offline);
    // A's first save was made twice (merged again), then its spend was saved.
    let text = memory.file(&state_path()).expect("a state file");
    let saved: Value = serde_json::from_str(&text).expect("JSON");
    let mut spends: Vec<(String, u64)> = saved["spends"]
        .as_array()
        .expect("a journal")
        .iter()
        .map(|e| {
            (
                e["kind"].as_str().unwrap_or_default().to_owned(),
                e["n"].as_u64().unwrap_or(0),
            )
        })
        .collect();
    spends.sort();
    assert_eq!(
        spends,
        [("unanswered".to_owned(), 5), ("unanswered".to_owned(), 7)]
    );
    assert_eq!(a.status(false).meters["exports"].unsynced, 12);
    assert_eq!(b.status(false).meters["exports"].unsynced, 12);
}

// --- the allowance, judged where the spend lands -----------------------------------------------------------

#[test]
fn refuses_a_spend_that_another_process_took_the_allowance_from_meanwhile_and_keeps_theirs() {
    let h = waiting_harness(json!({ "exports": { "left": 90, "total": 100, "offline": 10 } }));
    let app = Racing::over(
        &h,
        vec![entry_of(&"T".repeat(22), SpendKind::Offline, 8, NOW - 5)],
    )
    .app(&h);
    // Read with nothing unsynced, so the allowance covers 5; by the time it lands, 8 are there: 13 would pass 10.
    // What is left is as the landing found it: the 90, less the 8 the other process had spent by then.
    assert_eq!(app.use_meter("exports", 5), offline_limit(82));
    assert_eq!(kinds_and_counts(&journal_of(&h)), [(SpendKind::Offline, 8)]);
    // What the allowance still covers lands.
    assert!(app.use_meter("exports", 2).offline);
    assert_eq!(journal_of(&h).iter().map(|e| e.n).collect::<Vec<_>>(), [10]);
}

#[test]
fn holds_a_spend_to_what_is_left_when_that_is_below_the_allowance() {
    let met = json!({ "exports": { "left": 2, "total": 100, "offline": 50 } });
    let h = waiting_harness(met.clone());
    let app = Racing::over(
        &h,
        vec![entry_of(&"T".repeat(22), SpendKind::Offline, 2, NOW - 5)],
    )
    .app(&h);
    // The allowance of 50 would take it; the 2 that are left, and the other process has spent them.
    assert_eq!(app.use_meter("exports", 2), offline_limit(0));
    assert_eq!(kinds_and_counts(&journal_of(&h)), [(SpendKind::Offline, 2)]);
    let alone = waiting_harness(met);
    assert_eq!(
        alone.license.use_meter("exports", 2),
        UseResult::spent(0, true)
    );
    assert_eq!(alone.license.use_meter("exports", 1), offline_limit(0));
}

#[test]
fn journals_a_void_for_a_spend_that_was_in_doubt_and_is_then_refused_at_landing() {
    let h = holding();
    h.http.fail(true);
    let app = Racing::over(
        &h,
        vec![entry_of(&"T".repeat(22), SpendKind::Offline, 8, NOW - 5)],
    )
    .app(&h);
    assert_eq!(app.use_meter("exports", 5), offline_limit(82));
    let sent = use_body(&h, 0)["id"].as_str().expect("an id").to_owned();
    let journal = journal_of(&h);
    assert_eq!(
        kinds_and_counts(&journal),
        [(SpendKind::Offline, 8), (SpendKind::Void, 5)]
    );
    assert_eq!(journal[1].id, sent);
}

#[test]
fn never_takes_back_a_spend_the_customer_was_told_was_made_it_lands_past_the_allowance() {
    let h = waiting_harness(json!({ "exports": { "left": 90, "total": 100, "offline": 10 } }));
    h.storage.fail_writes(true);
    let told = h.license.use_meter("exports", 6);
    assert_eq!(told, UseResult::spent(84, true));
    h.storage.fail_writes(false);
    // Meanwhile another process of the app spent 8, and the journal is full.
    let mut crowd: Vec<SpendEntry> = (0..MAX_PENDING_SPENDS - 1)
        .map(|i| SpendEntry {
            id: format!("{i:c>22}"),
            meter: format!("m{i}"),
            n: 1,
            at: NOW - 100,
            kind: SpendKind::Unanswered,
        })
        .collect();
    crowd.insert(0, entry_of(&"T".repeat(22), SpendKind::Offline, 8, NOW));
    let mut stored = h.state();
    stored.spends = Some(crowd);
    h.set_state(stored);
    h.license.status(false);
    let journal = journal_of(&h);
    assert_eq!(journal.len(), MAX_PENDING_SPENDS);
    assert_eq!(
        (journal[0].id.as_str(), journal[0].n),
        ("T".repeat(22).as_str(), 14)
    );
    let shown = h.license.status(false).meters["exports"];
    assert_eq!((shown.unsynced, shown.left), (14, 76));
}

#[test]
fn lands_a_spend_the_customer_was_told_was_made_as_a_65th_entry_when_it_opens_an_entry_of_its_own()
{
    let h = waiting_harness(json!({ "exports": { "left": 90, "total": 100, "offline": 10 } }));
    h.storage.fail_writes(true);
    assert!(h.license.use_meter("exports", 2).ok);
    h.storage.fail_writes(false);
    let crowd: Vec<SpendEntry> = (0..MAX_PENDING_SPENDS)
        .map(|i| SpendEntry {
            id: format!("{i:c>22}"),
            meter: format!("m{i}"),
            n: 1,
            at: NOW - 100,
            kind: SpendKind::Unanswered,
        })
        .collect();
    let mut stored = h.state();
    stored.spends = Some(crowd);
    h.set_state(stored);
    h.license.status(false);
    let journal = journal_of(&h);
    assert_eq!(journal.len(), MAX_PENDING_SPENDS + 1);
    let last = journal.last().expect("an entry");
    assert_eq!(
        (last.meter.as_str(), last.n, last.kind),
        ("exports", 2, SpendKind::Offline)
    );
}

// --- what is left when a landing refuses a spend and the state cannot say ----------------------------------

fn credits_figure() -> BTreeMap<String, MeterFigure> {
    BTreeMap::from([(
        "credits".to_owned(),
        MeterFigure {
            left: 20,
            total: 20,
            offline: 10,
            resets: false,
            at: NOW,
        },
    )])
}

#[test]
fn answers_0_left_when_the_state_cannot_be_read_again_just_after_the_refusal() {
    let h = waiting_harness(json!({ "exports": { "left": 90, "total": 100, "offline": 10 } }));
    let racing = Racing {
        fail_reads_after: true,
        ..Racing::over(
            &h,
            vec![entry_of(&"T".repeat(22), SpendKind::Offline, 8, NOW - 5)],
        )
    };
    let app = racing.app(&h);
    assert_eq!(app.use_meter("exports", 5), offline_limit(0));
    // The refusal stands, and their spend is kept: only the figure was lost to the customer.
    assert_eq!(kinds_and_counts(&journal_of(&h)), [(SpendKind::Offline, 8)]);
}

#[test]
fn answers_0_left_when_the_state_as_the_landing_found_it_has_no_figure_for_the_meter_any_more() {
    // The lease names no credits; the figure is a use answer's, stored beside it.
    let with_figure = || {
        holding_with(meters(), Opts::default(), |state| {
            waiting(state);
            state.meter_figures = Some(credits_figure());
        })
    };
    let h = with_figure();
    let shown = h.license.status(false).meters["credits"];
    assert_eq!((shown.left, shown.offline), (20, 10));
    let theirs = vec![SpendEntry {
        meter: "credits".into(),
        ..entry_of(&"T".repeat(22), SpendKind::Offline, 8, NOW - 5)
    }];
    let racing = Racing {
        drop_figures: true,
        ..Racing::over(&h, theirs.clone())
    };
    assert_eq!(racing.app(&h).use_meter("credits", 5), offline_limit(0));
    assert_eq!(kinds_and_counts(&journal_of(&h)), [(SpendKind::Offline, 8)]);
    // With the figure kept, the same refusal says what is left: 20 less their 8.
    let kept = with_figure();
    assert_eq!(
        Racing::over(&kept, theirs)
            .app(&kept)
            .use_meter("credits", 5),
        offline_limit(12)
    );
}

// --- what an answer stamps ------------------------------------------------------------------------------------

fn unconfirmed(answer: Option<HttpResponse>) -> Harness {
    let h = holding_with(meters(), Opts::default(), |state| {
        state.verdict_at = Some(111)
    });
    match answer {
        None => h.http.fail(true),
        Some(answer) => h.http.on("use", move |_| answer.clone()),
    }
    h.license.use_meter("exports", 1);
    h
}

#[test]
fn leaves_verdict_at_alone_for_a_spend_used_up_and_an_unknown_meter() {
    let used_up = reply(
        409,
        json!({ "error": "used-up", "left": 0, "meters": meters(), "at": NOW }),
    );
    let unknown = reply(404, json!({ "error": "unknown-meter" }));
    for answer in [spent(89, NOW + 5), used_up, unknown] {
        assert_eq!(
            unconfirmed(Some(answer.clone())).state().verdict_at,
            Some(111),
            "{answer:?}"
        );
    }
}

#[test]
fn sets_it_for_a_verdict_and_for_a_wait_as_a_validates_answer_does() {
    let revoked = reply(403, json!({ "error": "revoked" }));
    assert_eq!(unconfirmed(Some(revoked)).state().verdict_at, Some(NOW));
    assert_eq!(unconfirmed(None).state().verdict_at, Some(NOW));
}

#[test]
fn lets_a_validate_that_was_in_flight_across_a_spend_keep_its_lease() {
    let h = holding();
    let other = h.license_for(1);
    let renewed = metered_lease_at(&h.key, meters(), NOW + 1_000, Value::Null);
    let gate = Gate::new();
    let held = gate.clone();
    let answer = renewed.clone();
    h.http.on("validate", move |_| {
        held.wait();
        reply(200, json!({ "lease": answer }))
    });
    h.http.on("use", |_| spent(89, NOW + 2_000));
    std::thread::scope(|scope| {
        let validating = scope.spawn(|| h.license.refresh());
        let _releasing = gate.releasing();
        gate.await_reached();
        // Another process spends, asked later and answered first.
        h.clock.set(NOW + 2_000);
        assert_eq!(other.use_meter("exports", 1), UseResult::spent(89, false));
        assert_eq!(h.state().verdict_at, None);
        gate.release();
        validating.join().expect("the validate");
    });
    assert_eq!(h.state().lease.as_deref(), Some(renewed.as_str()));
}

// --- a spend kept while a save is in flight ----------------------------------------------------------------------

#[test]
fn a_spend_kept_while_a_save_is_in_flight_is_counted_once_beside_the_one_that_save_carries() {
    let h = holding_with(
        json!({ "exports": { "left": 90, "total": 100, "offline": 50 } }),
        behind_opts(),
        waiting,
    );
    // A: kept in memory because the disk refused it.
    h.storage.fail_writes(true);
    assert!(h.license.use_meter("exports", 2).offline);
    h.storage.fail_writes(false);
    // A save in flight carries A and is held inside the storage.
    h.http.fail(true);
    let release = h.storage.hold_next_write();
    std::thread::scope(|scope| {
        let refreshing = scope.spawn(|| h.license.refresh());
        let _releasing = release.releasing();
        release.await_reached();
        // B, of the same meter, is made behind it: kept in memory, beside A.
        let second = h.license.use_meter("exports", 3);
        assert!(second.ok && second.offline);
        release.release();
        refreshing.join().expect("the refresh");
    });
    h.license.status(false);
    let spent: u64 = journal_of(&h).iter().map(|e| e.n).sum();
    assert_eq!(spent, 5);
    assert_eq!(h.license.status(false).meters["exports"].unsynced, 5);
}

// --- a spend the allowance cannot cover, while nothing answers ------------------------------------------------------

#[test]
fn asks_once_a_minute_keeps_at_most_the_cap_of_voids_and_leaves_the_journal_its_room() {
    let h = holding();
    h.http.fail(true);
    for minute in 0..30 {
        h.set_clock(NOW + minute * MINUTE);
        assert_eq!(h.license.use_meter("renders", 1), offline_limit(5));
        // The same minute: the void is a wait of its own, and nothing is asked.
        assert_eq!(h.license.use_meter("renders", 1), offline_limit(5));
    }
    assert_eq!(h.http.count("use"), 30);
    let voids: Vec<i64> = journal_of(&h)
        .iter()
        .filter(|e| e.kind == SpendKind::Void)
        .map(|e| e.at)
        .collect();
    // The newest sixteen stay: the first void to go was the oldest.
    let expected: Vec<i64> = (0..MAX_PENDING_VOIDS as i64)
        .map(|i| NOW + (14 + i) * MINUTE)
        .collect();
    assert_eq!(voids, expected);
    // A spend on another meter that its allowance covers is made, whatever the uncovered ones did to the journal.
    assert_eq!(
        h.license.use_meter("exports", 3),
        UseResult::spent(87, true)
    );
    assert_eq!(journal_of(&h).len(), MAX_PENDING_VOIDS + 1);
}

#[test]
fn asks_again_when_the_device_clock_is_set_back_below_the_void() {
    let h = holding();
    h.http.fail(true);
    h.set_clock(NOW + 10 * MINUTE);
    assert_eq!(
        h.license.use_meter("renders", 1).error,
        Some(UseError::OfflineLimit)
    );
    // Inside the void's minute by both clocks: nothing is asked.
    h.set_clock(NOW + 10 * MINUTE + 1000);
    assert_eq!(
        h.license.use_meter("renders", 1).error,
        Some(UseError::OfflineLimit)
    );
    assert_eq!(h.http.count("use"), 1);
    // The device clock is set back to before the void while the guard stands at its minute: the wait is none, and a customer who is online is served.
    h.http.fail(false);
    h.http.answer(
        "use",
        200,
        json!({ "spent": true, "at": NOW, "meters": { "renders": { "left": 4, "total": 5, "offline": 0, "resets": true } } }),
    );
    h.set_clock(NOW);
    assert_eq!(
        h.license.use_meter("renders", 1),
        UseResult::spent(4, false)
    );
    assert_eq!(h.http.count("use"), 2);
}

#[test]
fn is_served_at_once_when_the_server_is_back_and_its_minute_is_over_not_before() {
    let h = holding();
    h.http.fail(true);
    assert_eq!(
        h.license.use_meter("renders", 1).error,
        Some(UseError::OfflineLimit)
    );
    h.http.fail(false);
    h.http.answer(
        "use",
        200,
        json!({ "spent": true, "at": NOW + MINUTE, "meters": { "renders": { "left": 4, "total": 5, "offline": 0, "resets": true } } }),
    );
    // Inside the void's minute: refused without asking.
    h.set_clock(NOW + MINUTE - 1);
    assert_eq!(
        h.license.use_meter("renders", 1).error,
        Some(UseError::OfflineLimit)
    );
    assert_eq!(h.http.count("use"), 1);
    h.set_clock(NOW + MINUTE);
    assert_eq!(
        h.license.use_meter("renders", 1),
        UseResult::spent(4, false)
    );
}

#[test]
fn asks_for_it_whatever_an_unanswered_spend_of_a_covered_meter_waits_for_and_a_void_ahead_of_the_clock(
) {
    let h = holding_journal(vec![
        entry_of(&"U".repeat(22), SpendKind::Unanswered, 1, NOW - 1_000),
        entry_of(&"V".repeat(22), SpendKind::Void, 1, NOW + 86_400_000),
    ]);
    h.http.answer(
        "use",
        200,
        json!({ "spent": true, "at": NOW, "meters": { "renders": { "left": 4, "total": 5, "offline": 0, "resets": true } } }),
    );
    assert_eq!(
        h.license.use_meter("renders", 1),
        UseResult::spent(4, false)
    );
}

#[test]
fn a_wait_that_cannot_be_measured_leaves_out_only_the_entries_ahead_of_the_guarded_clock() {
    let ahead = entry_of(
        &"Z".repeat(22),
        SpendKind::Unanswered,
        1,
        NOW + 5 * 86_400_000,
    );
    let waiting = holding_journal(vec![
        entry_of(&"A".repeat(22), SpendKind::Unanswered, 1, NOW - 30_000),
        ahead.clone(),
    ]);
    waiting.http.on("use", |_| spent(89, NOW));
    waiting.license.use_meter("exports", 1);
    assert_eq!(waiting.http.count("use"), 0);
    let over = holding_journal(vec![
        entry_of(&"A".repeat(22), SpendKind::Unanswered, 1, NOW - 2 * MINUTE),
        ahead,
    ]);
    over.http.on("use", |_| spent(89, NOW));
    over.license.use_meter("exports", 1);
    assert_eq!(over.http.count("use"), 1);
}
