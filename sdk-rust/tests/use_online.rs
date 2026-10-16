//! `use_meter(meter, n)` when the server answers, what a call asks for, and
//! when it is not licensed: exact online, a verdict stored as a validate's is,
//! and a licence that stays licensed whatever a meter says.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use keygrant::{
    AdapterError, HttpAdapter, HttpResponse, License, LicenseState, MeterFigure, Refusal,
    SpendKind, StoredState, Timings, UseError, UseResult,
};
use serde_json::{json, Value};

// --- online ------------------------------------------------------------------------

#[test]
fn spends_exactly_sending_the_key_the_seat_the_meter_the_amount_and_a_fresh_id() {
    let h = holding();
    h.http.on("use", |_| spent(89, NOW + 1));
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::spent(89, false)
    );
    let body = use_body(&h, 0);
    assert_eq!(body["key"], KEY);
    assert_eq!(body["activationId"], "act_1");
    assert_eq!(body["meter"], "exports");
    assert_eq!(body["n"], 1);
    assert_eq!(body["major"], 1);
    let id = body["id"].as_str().expect("an id");
    assert_eq!(id.len(), 22);
    assert!(id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    assert_eq!(body["kids"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["fingerprints"].as_array().map(Vec::len), Some(1));
    assert!(body.get("spends").is_none());
    assert!(body.get("keptMajor").is_none());
    let stored = h.state();
    assert_eq!(
        stored.meter_figures.expect("figures").get("exports"),
        Some(&MeterFigure {
            left: 89,
            total: 100,
            offline: 10,
            resets: false,
            at: NOW + 1
        })
    );
    assert!(journal_of(&h).is_empty());
}

#[test]
fn spends_a_batch_whole_in_one_request() {
    let h = holding();
    h.http.on("use", |_| spent(50, NOW));
    assert_eq!(
        h.license.use_meter("exports", 40),
        UseResult::spent(50, false)
    );
    assert_eq!(use_body(&h, 0)["n"], 40);
    assert_eq!(h.http.count("use"), 1);
}

#[test]
fn makes_a_new_id_for_each_call_two_calls_are_two_spends() {
    let h = holding();
    h.http.on("use", |_| spent(89, NOW));
    h.license.use_meter("exports", 1);
    h.license.use_meter("exports", 1);
    assert_ne!(use_body(&h, 0)["id"], use_body(&h, 1)["id"]);
}

#[test]
fn answers_from_the_figure_the_server_gave_net_of_what_this_device_has_not_reported() {
    let h = holding_journal(vec![entry(SpendKind::Offline, 3, LONG_AGO)]);
    h.http.on("use", |_| spent(80, NOW));
    // The server's 80 does not count the 3 unreported, so 77 is what the customer has.
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::spent(77, false)
    );
}

#[test]
fn answers_from_the_base_figures_as_they_were_when_the_server_gave_none_for_the_meter() {
    let h = holding();
    h.http.answer(
        "use",
        200,
        json!({ "spent": true, "meters": {}, "at": NOW }),
    );
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::spent(90, false)
    );
    h.http.answer("use", 200, json!({ "spent": true }));
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::spent(90, false)
    );
}

#[test]
fn keeps_a_figure_the_lease_is_newer_than_only_until_the_lease_is_renewed() {
    let h = holding();
    h.http.on("use", |_| spent(70, NOW + 5_000));
    h.license.use_meter("exports", 1);
    assert_eq!(h.license.status(false).meters["exports"].left, 70);
    // A renewal brings a lease signed after that figure, whose own figures win.
    let renewed = metered_lease_at(
        &h.key,
        json!({ "exports": { "left": 60, "total": 100, "offline": 10 } }),
        NOW + 10_000,
        Value::Null,
    );
    h.validate_with(&renewed);
    h.set_clock(NOW + 10_000);
    assert_eq!(h.license.refresh().meters["exports"].left, 60);
    assert_eq!(h.license.status(false).meters["exports"].left, 60);
}

#[test]
fn answers_used_up_with_what_the_server_says_is_left_and_spends_nothing_offline_after() {
    let spends = vec![entry_of(
        "BBBBBBBBBBBBBBBBBBBBBB",
        SpendKind::Unanswered,
        2,
        LONG_AGO,
    )];
    let h = holding_journal(spends);
    h.http.answer(
        "use",
        409,
        json!({
            "error": "used-up", "left": 1, "at": NOW,
            "meters": { "exports": { "left": 1, "total": 100, "offline": 10 } },
            "spends": [{ "id": "BBBBBBBBBBBBBBBBBBBBBB", "n": 2 }],
        }),
    );
    assert_eq!(
        h.license.use_meter("exports", 5),
        UseResult::refused(UseError::UsedUp, Some(1))
    );
    // The reports it confirmed and the figures it gave are saved.
    assert!(journal_of(&h).is_empty());
    assert_eq!(h.state().meter_figures.expect("figures")["exports"].left, 1);
    // The server's word stands offline too: one is left, and a spend of two is past it.
    h.http.fail(true);
    h.set_clock(NOW + 2 * MINUTE);
    assert_eq!(
        h.license.use_meter("exports", 2),
        UseResult::refused(UseError::OfflineLimit, Some(1))
    );
}

#[test]
fn answers_used_up_with_no_left_when_the_answer_gave_none() {
    let h = holding();
    h.http.answer("use", 409, json!({ "error": "used-up" }));
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::refused(UseError::UsedUp, None)
    );
}

#[test]
fn reads_the_servers_words_on_any_status_but_a_200_a_5xx_and_a_429() {
    let h = holding();
    h.http
        .answer("use", 400, json!({ "error": "used-up", "left": 0 }));
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::refused(UseError::UsedUp, Some(0))
    );
    // A 200 is a spend only with spent: true; anything else is no answer.
    h.http
        .answer("use", 200, json!({ "error": "used-up", "left": 0 }));
    let offline = h.license.use_meter("exports", 1);
    assert!(offline.ok && offline.offline);
}

#[test]
fn answers_no_meter_for_a_meter_the_server_does_not_know_and_confirms_the_reports_it_carried() {
    let h = holding_journal(vec![entry_of(
        "CCCCCCCCCCCCCCCCCCCCCC",
        SpendKind::Unanswered,
        1,
        LONG_AGO,
    )]);
    h.http.answer(
        "use",
        404,
        json!({ "error": "unknown-meter", "spends": [{ "id": "CCCCCCCCCCCCCCCCCCCCCC", "n": 1 }] }),
    );
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::refused(UseError::NoMeter, None)
    );
    assert!(journal_of(&h).is_empty());
}

#[test]
fn stores_a_verdict_as_a_validates_is_and_answers_it_as_status_will() {
    let cases = [
        ("revoked", UseError::Revoked, Refusal::Revoked),
        ("expired", UseError::Revoked, Refusal::Revoked),
        ("deactivated", UseError::Revoked, Refusal::Revoked),
        ("upgrade-required", UseError::Upgrade, Refusal::Upgrade),
        ("update-required", UseError::Outdated, Refusal::Outdated),
        ("device", UseError::Device, Refusal::Device),
    ];
    for (word, error, refusal) in cases {
        let h = holding();
        h.http.answer("use", 403, json!({ "error": word }));
        assert_eq!(
            h.license.use_meter("exports", 1),
            UseResult::refused(error, None),
            "{word}"
        );
        assert_eq!(h.state().refused, Some(refusal), "{word}");
        let status = h.license.status(false);
        assert_eq!(status.state, LicenseState::Invalid, "{word}");
        assert_eq!(status.reason, Some(refusal.reason()), "{word}");
        // The refusal stands: the next spend is not licensed, and asks nothing.
        assert_eq!(
            h.license.use_meter("exports", 1),
            UseResult::refused(UseError::NotLicensed, None),
            "{word}"
        );
        assert_eq!(h.http.count("use"), 1, "{word}");
    }
}

#[test]
fn records_the_reports_a_verdict_confirms() {
    let h = holding_journal(vec![entry_of(
        "DDDDDDDDDDDDDDDDDDDDDD",
        SpendKind::Void,
        1,
        LONG_AGO,
    )]);
    h.http.answer(
        "use",
        403,
        json!({ "error": "revoked", "spends": [{ "id": "DDDDDDDDDDDDDDDDDDDDDD", "void": true }] }),
    );
    h.license.use_meter("exports", 1);
    assert!(journal_of(&h).is_empty());
}

#[test]
fn a_used_up_meter_refuses_spends_never_the_licence() {
    let h = holding();
    h.http
        .answer("use", 409, json!({ "error": "used-up", "left": 0 }));
    h.license.use_meter("exports", 1);
    let status = h.license.status(false);
    assert_eq!(status.state, LicenseState::Licensed);
    assert!(status.permits_use());
}

// --- the bound of the request -------------------------------------------------------------

/// An HTTP adapter that notes the bound each request is given, and answers as
/// the harness's fake server does.
struct Bounds {
    inner: Arc<FakeHttp>,
    seen: Mutex<Vec<(String, Duration)>>,
}

impl HttpAdapter for Bounds {
    fn post(
        &self,
        url: &str,
        body: &Value,
        timeout: Duration,
    ) -> Result<HttpResponse, AdapterError> {
        let action = url.rsplit('/').next().unwrap_or_default().to_owned();
        lock(&self.seen).push((action, timeout));
        self.inner.post(url, body, timeout)
    }
}

/// A licence over the harness's state whose requests are noted, with the SDK's
/// own bounds (`Timings::default()`) and the HTTP bound given.
fn bounded(h: &Harness, http_timeout: Duration) -> (License, Arc<Bounds>) {
    let bounds = Arc::new(Bounds {
        inner: Arc::clone(&h.http),
        seen: Mutex::new(Vec::new()),
    });
    let config = h
        .config
        .clone()
        .http(bounds.clone())
        .http_timeout(http_timeout)
        .timings(Timings::default());
    (License::new(config).expect("a licence"), bounds)
}

#[test]
fn a_use_request_waits_five_seconds_for_the_server() {
    let h = holding();
    h.http.on("use", |_| spent(89, NOW));
    let (license, bounds) = bounded(&h, Duration::from_secs(15));
    license.use_meter("exports", 1);
    assert_eq!(
        *lock(&bounds.seen),
        [("use".to_owned(), Duration::from_secs(5))]
    );
}

#[test]
fn a_use_request_waits_for_the_http_bound_instead_when_that_is_shorter() {
    let h = holding();
    h.http.on("use", |_| spent(89, NOW));
    let (license, bounds) = bounded(&h, Duration::from_secs(1));
    license.use_meter("exports", 1);
    assert_eq!(
        *lock(&bounds.seen),
        [("use".to_owned(), Duration::from_secs(1))]
    );
}

#[test]
fn spends_from_the_allowance_when_nothing_answers_within_the_bound() {
    let h = holding_with(
        meters(),
        Opts {
            timings: Some(fast_timings()),
            ..Opts::default()
        },
        |_| {},
    );
    h.http.hang(true);
    let began = Instant::now();
    let spent = h.license.use_meter("exports", 1);
    assert_eq!(spent, UseResult::spent(89, true));
    // The status check's bound (300 ms here) is what it waited, not the 15 s HTTP bound.
    let waited = began.elapsed();
    assert!(waited >= Duration::from_millis(250), "{waited:?}");
    assert!(waited < Duration::from_secs(10), "{waited:?}");
}

// --- what is asked ------------------------------------------------------------------------------

#[test]
fn refuses_an_amount_that_is_not_a_whole_number_from_1_to_the_bound_asking_nothing() {
    let h = holding();
    for n in [0, 1_000_000_001, u64::MAX] {
        assert_eq!(
            h.license.use_meter("exports", n),
            UseResult::refused(UseError::InvalidAmount, None),
            "{n}"
        );
    }
    assert_eq!(h.http.count("use"), 0);
    assert!(journal_of(&h).is_empty());
}

#[test]
fn takes_the_bound_itself() {
    let h = holding_with(
        json!({ "exports": { "left": 2_000_000_000u64, "total": 2_000_000_000u64, "offline": 0 } }),
        Opts::default(),
        |_| {},
    );
    h.http.on("use", |_| spent(1_000_000_000, NOW));
    assert!(h.license.use_meter("exports", 1_000_000_000).ok);
    assert_eq!(use_body(&h, 0)["n"], 1_000_000_000u64);
}

#[test]
fn judges_the_amount_before_the_meter_and_refuses_a_code_of_no_meter_asking_nothing() {
    let h = holding();
    assert_eq!(
        h.license.use_meter("", 0),
        UseResult::refused(UseError::InvalidAmount, None)
    );
    for code in ["", &"x".repeat(33), &"\u{1F600}".repeat(17)] {
        assert_eq!(
            h.license.use_meter(code, 1),
            UseResult::refused(UseError::NoMeter, None),
            "{code:?}"
        );
    }
    assert_eq!(h.http.count("use"), 0);
}

#[test]
fn takes_a_code_of_32_utf16_units_a_surrogate_pair_at_the_edge_included_and_matches_it_exactly() {
    let code = format!("{}\u{1F600}", "x".repeat(30));
    assert_eq!(code.encode_utf16().count(), 32);
    let h = holding_with(
        json!({
            code.clone(): { "left": 9, "total": 9, "offline": 0 },
            "exports": { "left": 9, "total": 9, "offline": 0 },
        }),
        Opts::default(),
        |_| {},
    );
    h.http.on("use", |_| reply(200, json!({ "spent": true })));
    assert!(h.license.use_meter(&code, 1).ok);
    assert_eq!(use_body(&h, 0)["meter"], code.as_str());
    // Matched exactly, case included: the key has "exports", and "EXPORTS" is no meter of it.
    h.http.fail(true);
    h.set_clock(NOW + 5 * MINUTE);
    assert_eq!(
        h.license.use_meter("EXPORTS", 1),
        UseResult::refused(UseError::NoMeter, None)
    );
}

#[test]
fn spends_a_meter_named_like_an_objects_member_only_when_the_key_has_it() {
    let h = holding_with(
        json!({ "constructor": { "left": 9, "total": 9, "offline": 3 } }),
        Opts::default(),
        |_| {},
    );
    h.http.fail(true);
    assert!(h.license.use_meter("constructor", 1).ok);
    assert_eq!(
        h.license.use_meter("toString", 1),
        UseResult::refused(UseError::NoMeter, None)
    );
    assert_eq!(
        h.license.use_meter("__proto__", 1),
        UseResult::refused(UseError::NoMeter, None)
    );
}

#[test]
fn makes_an_id_of_22_base64url_characters_a_new_one_each_time() {
    let ids: std::collections::BTreeSet<String> = (0..200)
        .map(|_| keygrant::meters::new_spend_id().expect("a random source"))
        .collect();
    assert_eq!(ids.len(), 200);
    for id in ids {
        assert_eq!(id.len(), 22);
        assert!(id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    }
}

// --- when it is not licensed ---------------------------------------------------------------------

fn not_licensed(h: &Harness) {
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::refused(UseError::NotLicensed, None)
    );
    assert_eq!(h.http.count("use"), 0, "nothing is asked");
}

#[test]
fn is_not_licensed_with_no_licence_an_expired_or_tampered_lease_and_a_trial_asking_nothing() {
    // No licence at all.
    not_licensed(&Harness::empty());
    // An expired lease.
    let h = holding();
    h.set_clock(NOW + 40 * 86_400_000);
    h.http.fail(true);
    not_licensed(&h);
    // A lease for another activation.
    let h = Harness::empty();
    let lease = metered_lease_at(&h.key, meters(), NOW, json!({ "aid": "act_other" }));
    h.set_state(stored_with(&lease));
    not_licensed(&h);
    // A running trial.
    let h = Harness::empty();
    let trial = h.key.lease_with(
        NOW,
        7 * 86_400,
        json!({ "sub": "trial:abc", "model": "trial", "aid": "abc", "met": meters() }),
    );
    h.set_state(StoredState {
        trial_start: Some(NOW),
        trial_ends_at: Some(NOW + 7 * 86_400_000),
        trial_lease: Some(trial),
        last_seen: Some(NOW),
        ..StoredState::default()
    });
    assert_eq!(h.license.status(false).state, LicenseState::Trial);
    not_licensed(&h);
}

#[test]
fn is_not_licensed_with_a_stored_refusal_that_stands_and_for_a_build_the_lease_does_not_cover() {
    let h = holding_with(meters(), Opts::default(), |state| {
        state.refused = Some(Refusal::Revoked);
    });
    not_licensed(&h);
    // The lease covers majors 1 to 3: a build at major 4 is not licensed by it.
    let h = holding_with(
        meters(),
        Opts {
            major: Some(4),
            ..Opts::default()
        },
        |_| {},
    );
    h.http.fail(true);
    not_licensed(&h);
}

#[test]
fn is_not_licensed_with_an_offline_licence_file_which_has_no_meters() {
    // Even one that carries a `met` claim, which the signer refuses and a verifier ignores.
    let h = Harness::empty();
    let token = h.key.offline_file(json!({ "met": meters() }));
    h.license
        .import_license_file(license_file_text(&token))
        .expect("imported");
    let status = h.license.status(false);
    assert_eq!(status.state, LicenseState::Licensed);
    assert!(status.meters.is_empty());
    not_licensed(&h);
}

#[test]
fn answers_storage_when_the_state_cannot_be_read_just_now_asking_nothing() {
    let h = holding();
    h.storage.fail_reads(true);
    assert_eq!(
        h.license.use_meter("exports", 1),
        UseResult::refused(UseError::Storage, None)
    );
    assert_eq!(h.http.count("use"), 0);
}

#[test]
fn a_panicking_storage_adapter_is_a_state_that_cannot_be_read() {
    struct Panics;
    impl keygrant::StorageAdapter for Panics {
        fn read(&self, _read_only: bool) -> std::io::Result<Option<StoredState>> {
            panic!("the storage adapter panicked");
        }
        fn write(&self, _state: &StoredState) -> std::io::Result<()> {
            panic!("the storage adapter panicked");
        }
    }
    let h = holding();
    let license = License::new(h.config.clone().storage(Arc::new(Panics))).expect("a licence");
    assert_eq!(
        license.use_meter("exports", 1),
        UseResult::refused(UseError::Storage, None)
    );
}

#[test]
fn a_panicking_clock_is_storage_and_never_a_panic_and_the_next_call_runs() {
    // A clock that panics on its first reading only.
    struct PanicsOnce(std::sync::atomic::AtomicBool);
    impl keygrant::Clock for PanicsOnce {
        fn now(&self) -> i64 {
            assert!(
                self.0.swap(true, std::sync::atomic::Ordering::SeqCst),
                "the clock panicked"
            );
            NOW
        }
    }
    let h = holding();
    h.http.on("use", |_| spent(89, NOW));
    let clock = Arc::new(PanicsOnce(std::sync::atomic::AtomicBool::new(false)));
    let license = License::new(h.config.clone().clock(clock)).expect("a licence");
    assert_eq!(
        license.use_meter("exports", 1),
        UseResult::refused(UseError::Storage, None)
    );
    // The call that panicked left no lock held: the next call, on the same licence, runs.
    assert_eq!(license.use_meter("exports", 1), UseResult::spent(89, false));
}

#[test]
fn a_request_that_panics_is_no_answer() {
    struct Panics;
    impl HttpAdapter for Panics {
        fn post(&self, _: &str, _: &Value, _: Duration) -> Result<HttpResponse, AdapterError> {
            panic!("the HTTP adapter panicked");
        }
    }
    let h = holding();
    let license = License::new(h.config.clone().http(Arc::new(Panics))).expect("a licence");
    assert_eq!(license.use_meter("exports", 1), UseResult::spent(89, true));
}

// --- the device --------------------------------------------------------------------------------

#[test]
fn names_the_device_as_a_validate_does_its_fingerprints() {
    let h = holding();
    h.http.on("use", |_| spent(89, NOW));
    h.license.use_meter("exports", 1);
    let use_request = use_body(&h, 0);
    h.license.refresh();
    let validate = h.http.last_body_of("validate");
    for name in ["key", "activationId", "major", "fingerprints"] {
        assert_eq!(use_request[name], validate[name], "{name}");
    }
}

#[test]
fn sends_no_fingerprints_when_devices_are_not_held() {
    let h = holding_with(
        meters(),
        Opts {
            device_hold: Some(false),
            ..Opts::default()
        },
        |_| {},
    );
    h.http.on("use", |_| spent(89, NOW));
    h.license.use_meter("exports", 1);
    let body = use_body(&h, 0);
    assert!(body.get("fingerprints").is_none());
    assert!(body.get("fingerprintKind").is_none());
}
