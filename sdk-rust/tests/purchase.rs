//! Buying from the trial with no key typed: `purchase_url` keeps this
//! install's purchase secret and hands out the reference to it, and
//! `status()` and `start_trial()` pick up the key bought with it.

mod common;

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use common::*;
use keygrant::lease::{verify_lease, KeySet, VerifyFailure, VerifyResult};
use keygrant::machine_id::{ACTIVATE_MACHINE_ID_TIMEOUT, MACHINE_ID_TIMEOUT};
use keygrant::pickup::{purchase_reference, CLAIM_EAGER_MS, CLAIM_WAIT_MS, PURCHASE_TOKEN_MS};
use keygrant::{
    DeviceIdentity, DeviceKind, Error, License, LicenseState, LicenseStatus, RegistryStash,
    StorageAdapter, StoredState, Timings,
};
use serde_json::{json, Value};

const HOUR: i64 = 3_600_000;

const LINK: &str = "https://buy.stripe.com/test_abc";
/// A purchase secret: the base64url of 32 zero bytes.
const TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const BOUGHT_KEY: &str = "SLUICE-BUY1-BUY1-BUY1-BUY1";

/// The purchase secret and its stamps, by their stored names.
const PURCHASE_FIELDS: [&str; 3] = ["purchaseToken", "purchaseTokenAt", "claimAskedAt"];

/// The link `purchase_url` answers for the held `secret`: its reference, never the secret.
fn link_for(secret: &str) -> String {
    format!("{LINK}?client_reference_id={}", purchase_reference(secret))
}

/// Whether `text` is base64url with no padding, `len` characters long.
fn base64url_of(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A purchase secret held, its link handed out `ago` ms before NOW, over `state`.
fn held(ago: i64, state: StoredState) -> StoredState {
    StoredState {
        purchase_token: Some(TOKEN.into()),
        purchase_token_at: Some(NOW - ago),
        ..state
    }
}

/// A running trial, synced within the day (so no launch report is due), over `state`.
fn trial_state(h: &Harness, state: StoredState) -> StoredState {
    StoredState {
        trial_start: Some(NOW - 2 * DAY),
        trial_ends_at: Some(NOW + 5 * DAY),
        trial_lease: Some(h.lease("trial:fp-test", "trial", 5 * 86_400, NOW)),
        trial_at: Some(NOW - DAY / 2),
        last_seen: Some(NOW),
        ..state
    }
}

/// The server's answer when the key bought with the token is this device's: as an activation's.
fn bought(h: &Harness, model: &str) -> Value {
    let lease = h.lease(BOUGHT_KEY, model, 30 * 86_400, NOW);
    json!({ "key": BOUGHT_KEY, "activationId": "act_1", "lease": lease, "inGrace": false, "validUntil": null })
}

fn trial_status() -> LicenseStatus {
    let mut status = LicenseStatus::new(LicenseState::Trial);
    status.trial_ends_at = Some(NOW + 5 * DAY);
    status.trial_days_left = Some(5);
    status
}

fn licensed_as(key: &str) -> LicenseStatus {
    let mut status = LicenseStatus::new(LicenseState::Licensed);
    status.key = Some(key.to_owned());
    status
}

fn unlicensed() -> LicenseStatus {
    LicenseStatus::new(LicenseState::Unlicensed)
}

fn stalled(mut status: LicenseStatus) -> LicenseStatus {
    status.stalled = true;
    status
}

/// Whether the stored state holds `field`.
fn holds(h: &Harness, field: &str) -> bool {
    json_of(&h.state()).contains_key(field)
}

fn holds_no_purchase(h: &Harness) -> bool {
    PURCHASE_FIELDS.iter().all(|field| !holds(h, field))
}

/// A harness whose HTTP bound and lock grace a test can wait out (100 ms and 300 ms).
fn quick(initial: Option<StoredState>) -> Harness {
    Harness::new(
        initial,
        Opts {
            http_timeout: Some(Duration::from_millis(100)),
            timings: Some(fast_timings()),
            ..Opts::default()
        },
    )
}

// --- purchase_url ------------------------------------------------------------------

#[test]
fn keeps_a_new_secret_of_256_random_bits_and_hands_out_its_kgp_reference_never_the_secret() {
    let h = Harness::empty();
    let url = h
        .license
        .purchase_url(LINK)
        .expect("a link")
        .expect("no key held");
    let secret = h.state().purchase_token.expect("a secret kept");
    assert!(base64url_of(&secret, 43), "{secret}");
    assert_eq!(h.state().purchase_token_at, Some(NOW));
    let reference = url
        .strip_prefix(&format!("{LINK}?client_reference_id="))
        .expect("the reference as client_reference_id");
    let hash = reference.strip_prefix("kgp_").expect("kgp_ first");
    assert!(base64url_of(hash, 43), "{reference}");
    assert_eq!(reference, purchase_reference(&secret));
    assert!(!url.contains(&secret));
    assert!(h.http.calls().is_empty());
}

#[test]
fn hands_the_held_token_out_again_its_bound_counted_from_this_call_and_clears_the_claims_wait() {
    let h = Harness::with(held(
        10 * DAY,
        StoredState {
            claim_asked_at: Some(NOW - 1000),
            ..StoredState::default()
        },
    ));
    h.set_clock(NOW + 1000);
    assert_eq!(
        h.license.purchase_url(LINK).expect("a link"),
        Some(link_for(TOKEN))
    );
    assert_has(
        &h.state(),
        json!({ "purchaseToken": TOKEN, "purchaseTokenAt": NOW + 1000 }),
    );
    assert!(!holds(&h, "claimAskedAt"));
}

#[test]
fn makes_a_new_token_once_the_held_one_is_past_its_bound() {
    let h = Harness::with(held(PURCHASE_TOKEN_MS, StoredState::default()));
    let url = h
        .license
        .purchase_url(LINK)
        .expect("a link")
        .expect("a link");
    assert_ne!(url, link_for(TOKEN));
    assert_ne!(h.state().purchase_token.as_deref(), Some(TOKEN));
}

#[test]
fn keeps_the_links_own_parameters_and_fragment_and_replaces_a_client_reference_id_on_it() {
    let h = Harness::with(held(0, StoredState::default()));
    let url = h.license.purchase_url(
        "https://buy.stripe.com/x?prefilled_email=jo%40example.com&client_reference_id=old#pay",
    );
    assert_eq!(
        url.expect("a link"),
        Some(format!(
            "https://buy.stripe.com/x?prefilled_email=jo%40example.com&client_reference_id={}#pay",
            purchase_reference(TOKEN)
        ))
    );
}

#[test]
fn is_none_while_any_key_is_held_and_saves_nothing() {
    for state in [
        licensed("KEY-1", "header.payload.sig"),
        StoredState {
            key: Some("KEY-1".into()),
            refused: Some(keygrant::Refusal::Revoked),
            ..StoredState::default()
        },
        StoredState {
            key: Some("KEY-1".into()),
            ..StoredState::default()
        },
    ] {
        let h = Harness::with(held(0, state));
        assert_eq!(h.license.purchase_url(LINK).expect("no failure"), None);
        assert_eq!(h.storage.writes(), 0);
    }
}

#[test]
fn refuses_a_link_that_is_not_an_absolute_url_before_reading_anything() {
    let h = Harness::empty();
    for link in [
        "buy.stripe.com/test_abc",
        "/buy",
        "",
        "mailto:jo@example.com",
    ] {
        assert!(
            matches!(h.license.purchase_url(link), Err(Error::InvalidUrl)),
            "{link}"
        );
    }
    assert_eq!(h.storage.reads.load(Ordering::SeqCst), 0);
}

#[test]
fn fails_when_the_state_cannot_be_read_or_the_token_cannot_be_saved() {
    let unreadable = Harness::empty();
    unreadable.storage.fail_read(1);
    assert!(matches!(
        unreadable.license.purchase_url(LINK),
        Err(Error::Storage(_))
    ));
    let unsaved = Harness::empty();
    unsaved.storage.fail_writes(true);
    assert!(matches!(
        unsaved.license.purchase_url(LINK),
        Err(Error::Storage(_))
    ));
}

#[test]
fn behind_a_call_that_has_not_finished_answers_the_held_token_as_it_is_and_none_held_is_busy() {
    let h = quick(Some(held(DAY, StoredState::default())));
    let gate = h.storage.hang_next_read();
    thread::scope(|scope| {
        let _stuck = scope.spawn(|| h.license.status(false));
        gate.await_reached();
        assert_eq!(
            h.license.purchase_url(LINK).expect("the held token"),
            Some(link_for(TOKEN))
        );
        assert_eq!(h.storage.writes(), 0);
        gate.release();
    });

    let none = quick(None);
    let gate = none.storage.hang_next_read();
    thread::scope(|scope| {
        let _stuck = scope.spawn(|| none.license.status(false));
        gate.await_reached();
        assert!(matches!(none.license.purchase_url(LINK), Err(Error::Busy)));
        assert_eq!(none.storage.writes(), 0);
        gate.release();
    });
}

// --- the pickup: status() asks for the key bought with the token ---------------------

#[test]
fn saves_a_bought_key_exactly_as_activate_does_clears_the_token_and_answers_licensed() {
    let h = Harness::new(
        None,
        Opts {
            device_name: Some("Rae's laptop".into()),
            ..Opts::default()
        },
    );
    h.set_state(trial_state(&h, held(0, StoredState::default())));
    let answer = bought(&h, "perpetual");
    h.http.answer("claim", 200, answer);
    assert_eq!(h.license.status(false), licensed_as(BOUGHT_KEY));
    assert_eq!(
        h.http.body_of("claim"),
        json!({
            "token": TOKEN,
            "fingerprint": "fp-test",
            "fingerprints": ["fp-test"],
            "name": "Rae's laptop",
            "major": 1,
            "kids": [h.key.kid()],
        })
    );
    assert_has(
        &h.state(),
        json!({ "key": BOUGHT_KEY, "activationId": "act_1", "checkedMajor": 1, "verdictAt": NOW }),
    );
    assert!(holds_no_purchase(&h));
    // Licensed from then on, offline, and never asked again.
    h.http.fail(true);
    assert_eq!(h.license.status(false), licensed_as(BOUGHT_KEY));
    assert_eq!(h.http.count("claim"), 1);
    assert!(!h.http.called("trial"));
}

#[test]
fn on_not_bought_yet_answers_the_trial_keeps_the_token_and_asks_again_only_after_the_wait() {
    let h = Harness::empty();
    h.set_state(trial_state(&h, held(0, StoredState::default())));
    h.http.answer("claim", 200, json!({ "pending": true }));
    assert_eq!(h.license.status(false), trial_status());
    assert_has(
        &h.state(),
        json!({ "purchaseToken": TOKEN, "claimAskedAt": NOW }),
    );
    h.set_clock(NOW + CLAIM_WAIT_MS - 1);
    h.license.status(false);
    assert_eq!(h.http.count("claim"), 1);
    h.set_clock(NOW + CLAIM_WAIT_MS);
    h.license.status(false);
    assert_eq!(h.http.count("claim"), 2);
}

#[test]
fn asks_at_once_on_refresh_and_when_the_clock_was_set_back_before_the_last_ask() {
    let h = Harness::empty();
    h.set_state(trial_state(
        &h,
        held(
            0,
            StoredState {
                claim_asked_at: Some(NOW - 1000),
                ..StoredState::default()
            },
        ),
    ));
    h.http.answer("claim", 200, json!({ "pending": true }));
    h.license.refresh();
    assert_eq!(h.http.count("claim"), 1);
    h.set_state(StoredState {
        claim_asked_at: Some(NOW + 60_000),
        ..h.state()
    });
    h.license.status(false);
    assert_eq!(h.http.count("claim"), 2);
}

#[test]
fn keeps_the_token_when_nothing_answers_or_the_answer_carries_no_error_word() {
    for answer in [
        None,
        Some((503, json!({ "error": "internal" }))),
        Some((404, json!("<html>"))),
        Some((200, json!("portal"))),
    ] {
        let h = Harness::with(held(0, StoredState::default()));
        match answer {
            None => h.http.fail(true),
            Some((status, body)) => h.http.answer("claim", status, body),
        }
        assert_eq!(h.license.status(false), unlicensed());
        assert_has(
            &h.state(),
            json!({ "purchaseToken": TOKEN, "claimAskedAt": NOW }),
        );
    }
}

#[test]
fn drops_the_token_on_an_error_word_and_on_a_key_whose_lease_does_not_license_this_build() {
    let h = Harness::empty();
    let elsewhere = h.key.lease_with(
        NOW,
        86_400,
        json!({ "sub": BOUGHT_KEY, "product": "another-product" }),
    );
    let answers = [
        (409, json!({ "error": "spent" })),
        (403, json!({ "error": "pickup-off" })),
        (403, json!({ "error": "limit" })),
        (
            200,
            json!({ "key": BOUGHT_KEY, "activationId": "act_1", "lease": elsewhere }),
        ),
    ];
    for (status, body) in &answers {
        h.set_state(trial_state(&h, held(0, StoredState::default())));
        h.http.answer("claim", *status, body.clone());
        assert_eq!(h.license.status(false), trial_status());
        assert!(holds_no_purchase(&h));
        assert!(!holds(&h, "key"));
    }
    h.license.refresh();
    assert_eq!(h.http.count("claim"), answers.len());
}

#[test]
fn drops_a_token_past_its_bound_without_sending_it() {
    let h = Harness::with(held(PURCHASE_TOKEN_MS, StoredState::default()));
    assert_eq!(h.license.refresh(), unlicensed());
    assert!(!h.http.called("claim"));
    assert!(!holds(&h, "purchaseToken"));
}

#[test]
fn saves_a_drop_about_its_token_so_a_token_another_process_made_meanwhile_stays() {
    let h = Harness::with(held(0, StoredState::default()));
    let storage = h.storage.clone();
    h.http.on("claim", move |_| {
        storage.set(Some(StoredState {
            purchase_token: Some("kgp_another-process-made-this-one-meanwhile".into()),
            purchase_token_at: Some(NOW),
            ..StoredState::default()
        }));
        reply(409, json!({ "error": "spent" }))
    });
    h.license.status(false);
    assert_has(
        &h.state(),
        json!({ "purchaseToken": "kgp_another-process-made-this-one-meanwhile", "purchaseTokenAt": NOW }),
    );
}

#[test]
fn bounds_an_unforced_claim_as_a_status_check_is_keeping_the_token() {
    // The status check's bound is 300 ms here; the HTTP bound is patient.
    let h = Harness::new(
        Some(held(0, StoredState::default())),
        Opts {
            http_timeout: Some(PATIENT),
            timings: Some(Timings {
                status_check: Duration::from_millis(300),
                ..patient_timings()
            }),
            ..Opts::default()
        },
    );
    h.http.hang(true);
    let started = Instant::now();
    assert_eq!(h.license.status(false), unlicensed());
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(300) && took < Duration::from_secs(10),
        "{took:?}"
    );
    assert_has(
        &h.state(),
        json!({ "purchaseToken": TOKEN, "claimAskedAt": NOW }),
    );
}

#[test]
fn answers_a_bought_key_even_when_the_disk_refuses_it() {
    let h = Harness::with(held(0, StoredState::default()));
    let answer = bought(&h, "perpetual");
    h.http.answer("claim", 200, answer);
    h.storage.fail_writes(true);
    assert_eq!(h.license.status(false), licensed_as(BOUGHT_KEY));
    // Nothing was saved: the next status() claims again, with the same body,
    // and saves the key.
    h.storage.fail_writes(false);
    assert_eq!(h.license.status(false), licensed_as(BOUGHT_KEY));
    let bodies: Vec<Value> = h
        .http
        .calls()
        .into_iter()
        .filter(|(action, _)| action == "claim")
        .map(|(_, body)| body)
        .collect();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0], bodies[1]);
    assert_eq!(h.state().key.as_deref(), Some(BOUGHT_KEY));
}

#[test]
fn asks_nothing_while_a_key_is_held_or_behind_a_call_that_has_not_finished() {
    let keyed = Harness::with(held(
        0,
        StoredState {
            key: Some("KEY-1".into()),
            ..StoredState::default()
        },
    ));
    keyed.license.refresh();
    assert!(!keyed.http.called("claim"));

    let h = quick(Some(held(0, StoredState::default())));
    let gate = h.storage.hang_next_read();
    thread::scope(|scope| {
        let _stuck = scope.spawn(|| h.license.status(false));
        gate.await_reached();
        assert_eq!(h.license.status(true), stalled(unlicensed()));
        assert!(!h.http.called("claim"));
        gate.release();
    });
}

#[test]
fn an_unforced_status_asks_only_for_a_day_after_the_link_refresh_until_the_bound() {
    let h = Harness::with(held(CLAIM_EAGER_MS, StoredState::default()));
    h.http.answer("claim", 200, json!({ "pending": true }));
    assert_eq!(h.license.status(false), unlicensed());
    assert!(!h.http.called("claim"));
    assert_eq!(h.license.refresh(), unlicensed());
    assert_eq!(h.http.count("claim"), 1);
    // A day less a millisecond: asked.
    let fresh = Harness::with(held(CLAIM_EAGER_MS - 1, StoredState::default()));
    fresh.http.answer("claim", 200, json!({ "pending": true }));
    fresh.license.status(false);
    assert_eq!(fresh.http.count("claim"), 1);
}

/// A device whose machine id does not read: no own fingerprint, its host
/// name's as the alternate.
fn host_name_only() -> Opts {
    Opts {
        fingerprint: Some(std::sync::Arc::new(IdentifyingFingerprinter(|| {
            DeviceIdentity {
                fingerprint: None,
                kind: None,
                alternates: vec!["fp-host".into()],
            }
        }))),
        ..Opts::default()
    }
}

#[test]
fn status_never_claims_under_the_host_name_and_starts_no_wait_but_start_trial_does_claim() {
    let h = Harness::new(Some(held(0, StoredState::default())), host_name_only());
    h.http.answer("claim", 200, json!({ "pending": true }));
    assert_eq!(h.license.status(false), unlicensed());
    assert_eq!(h.license.refresh(), unlicensed());
    assert!(!h.http.called("claim"));
    assert!(!holds(&h, "claimAskedAt"));
    h.license.start_trial();
    assert_eq!(
        h.http.body_of("claim"),
        json!({
            "token": TOKEN,
            "fingerprint": "fp-host",
            "fingerprintKind": "hostname",
            "fingerprints": ["fp-host"],
            "major": 1,
            "kids": [h.key.kid()],
        })
    );
}

#[test]
fn sends_every_fingerprint_the_run_read_when_the_device_is_not_held_too() {
    let h = Harness::new(
        Some(held(0, StoredState::default())),
        Opts {
            device_hold: Some(false),
            fingerprint: Some(Arc::new(IdentifyingFingerprinter(|| DeviceIdentity {
                fingerprint: Some("fp-own".into()),
                kind: Some(DeviceKind::Machine),
                alternates: vec!["fp-host".into()],
            }))),
            ..Opts::default()
        },
    );
    h.http.answer("claim", 200, json!({ "pending": true }));
    h.license.status(false);
    // Not held: no kind, and both fingerprints all the same.
    assert_eq!(
        h.http.body_of("claim"),
        json!({ "token": TOKEN, "fingerprint": "fp-own", "fingerprints": ["fp-own", "fp-host"], "major": 1, "kids": [h.key.kid()] })
    );
}

#[test]
fn sends_at_most_4_fingerprints_own_first_each_of_1_to_200_characters_never_cut() {
    let alternates = vec![
        "fp-a1".to_owned(),
        "x".repeat(201),
        "fp-a2".to_owned(),
        String::new(),
        "y".repeat(200),
        "fp-a4".to_owned(),
    ];
    let h = Harness::new(
        Some(held(0, StoredState::default())),
        Opts {
            fingerprint: Some(std::sync::Arc::new(IdentifyingFingerprinter(move || {
                DeviceIdentity {
                    fingerprint: Some("fp-own".into()),
                    kind: Some(keygrant::DeviceKind::Machine),
                    alternates: alternates.clone(),
                }
            }))),
            ..Opts::default()
        },
    );
    h.http.answer("claim", 200, json!({ "pending": true }));
    h.license.status(false);
    let body = h.http.body_of("claim");
    assert_eq!(body["fingerprint"], json!("fp-own"));
    assert_eq!(
        body["fingerprints"],
        json!(["fp-own", "fp-a1", "fp-a2", "y".repeat(200)])
    );
}

#[test]
fn bounds_refreshs_claim_by_the_http_bound_not_the_status_check() {
    // The status check's bound is 300 ms here, the HTTP bound patient, and the
    // claim answers a bought key after a second: refresh() waits for it.
    let h = Harness::new(
        Some(held(0, StoredState::default())),
        Opts {
            http_timeout: Some(PATIENT),
            timings: Some(Timings {
                status_check: Duration::from_millis(300),
                ..patient_timings()
            }),
            ..Opts::default()
        },
    );
    let answer = bought(&h, "perpetual");
    h.http.on("claim", move |_| {
        thread::sleep(Duration::from_secs(1));
        reply(200, answer.clone())
    });
    assert_eq!(h.license.refresh(), licensed_as(BOUGHT_KEY));
}

#[test]
fn reads_the_device_2_s_for_a_statuss_claim_and_once_for_10_s_at_a_trial_start() {
    let given = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let h = Harness::new(Some(held(0, StoredState::default())), recording(&given));
    h.http.answer("claim", 200, json!({ "pending": true }));
    h.license.status(false);
    assert_eq!(std::mem::take(&mut *lock(&given)), [MACHINE_ID_TIMEOUT]);
    h.license.start_trial();
    assert_eq!(*lock(&given), [ACTIVATE_MACHINE_ID_TIMEOUT]);
    assert_eq!(h.http.actions(), ["claim"]);
}

#[test]
fn start_trial_reads_the_device_once_for_its_claim_and_its_trial_ask() {
    let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = std::sync::Arc::clone(&reads);
    let h = Harness::new(
        Some(held(0, StoredState::default())),
        Opts {
            fingerprint: Some(std::sync::Arc::new(IdentifyingFingerprinter(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                // Nothing to keep: every read of the device asks the fingerprinter.
                DeviceIdentity {
                    fingerprint: None,
                    kind: None,
                    alternates: vec!["fp-host".into()],
                }
            }))),
            ..Opts::default()
        },
    );
    h.http.answer("claim", 200, json!({ "pending": true }));
    h.license.start_trial();
    assert_eq!(h.http.actions(), ["claim"]);
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

#[test]
fn start_trial_bounds_its_claim_as_a_status_check_is_then_asks_for_the_trial() {
    // The status check's bound is 300 ms here; the HTTP bound is patient.
    let h = Harness::new(
        Some(held(0, StoredState::default())),
        Opts {
            http_timeout: Some(PATIENT),
            timings: Some(Timings {
                status_check: Duration::from_millis(300),
                ..patient_timings()
            }),
            ..Opts::default()
        },
    );
    h.http.on("claim", |_| {
        thread::sleep(Duration::from_secs(20));
        reply(200, json!({ "pending": true }))
    });
    let lease = h.lease("trial:fp-test", "trial", 5 * 86_400, NOW);
    h.http.answer(
        "trial",
        200,
        json!({ "trialStart": NOW, "trialEndsAt": NOW + 5 * DAY, "lease": lease }),
    );
    let started = Instant::now();
    assert_eq!(h.license.start_trial(), trial_status());
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(300) && took < Duration::from_secs(10),
        "{took:?}"
    );
    assert_eq!(h.http.actions(), ["claim", "trial"]);
}

#[test]
fn a_late_answer_does_not_start_the_wait_again_after_a_link_handed_out_since() {
    // Another process hands the link out again while this claim is in
    // flight: its link cleared the wait, so the next status() asks at once.
    let h = Harness::with(held(DAY / 2, StoredState::default()));
    let storage = h.storage.clone();
    h.http.on("claim", move |_| {
        let mut state = storage.get().expect("a state");
        state.purchase_token_at = Some(NOW);
        state.claim_asked_at = None;
        storage.set(Some(state));
        reply(200, json!({ "pending": true }))
    });
    h.license.status(false);
    assert_has(
        &h.state(),
        json!({ "purchaseToken": TOKEN, "purchaseTokenAt": NOW }),
    );
    assert!(!holds(&h, "claimAskedAt"));
}

#[test]
fn a_call_behind_a_stuck_start_trial_waits_for_its_second_request_too() {
    // Requests 100 ms, the lock grace 300 ms, an activation's device read
    // 600 ms, the status check 1500 ms: a call behind a stuck start_trial()
    // waits 1900 ms before it goes ahead read-only (an extra of the lock
    // grace or of the device read would be shorter, a longer one longer),
    // counted from when start_trial() started (so from before it was spawned
    // here, which is no later).
    let h = Harness::new(
        Some(held(0, StoredState::default())),
        Opts {
            http_timeout: Some(Duration::from_millis(100)),
            timings: Some(Timings {
                lock_grace: Duration::from_millis(300),
                status_check: Duration::from_millis(1500),
                machine_id: Duration::from_millis(200),
                activate_machine_id: Duration::from_millis(600),
                identify_grace: Duration::from_secs(1),
            }),
            ..Opts::default()
        },
    );
    let gate = h.storage.hang_next_read();
    thread::scope(|scope| {
        let started = Instant::now();
        let _stuck = scope.spawn(|| h.license.start_trial());
        gate.await_reached();
        assert_eq!(h.license.status(false), stalled(unlicensed()));
        let took = started.elapsed();
        let bound = Duration::from_millis(1900);
        assert!(
            took >= bound && took < bound + Duration::from_secs(1),
            "{took:?}"
        );
        gate.release();
    });
}

// --- the pickup at start_trial() --------------------------------------------------------

#[test]
fn start_trial_asks_the_claim_first_whatever_the_wait_says_and_a_bought_key_is_the_answer() {
    let h = Harness::with(held(
        0,
        StoredState {
            claim_asked_at: Some(NOW - 1000),
            ..StoredState::default()
        },
    ));
    let answer = bought(&h, "perpetual");
    h.http.answer("claim", 200, answer);
    assert_eq!(h.license.start_trial(), licensed_as(BOUGHT_KEY));
    assert_eq!(h.http.actions(), ["claim"]);
}

#[test]
fn start_trial_asks_for_the_trial_after_a_claim_that_brought_no_key() {
    let h = Harness::with(held(0, StoredState::default()));
    let lease = h.lease("trial:fp-test", "trial", 5 * 86_400, NOW);
    h.http.answer("claim", 200, json!({ "pending": true }));
    h.http.answer(
        "trial",
        200,
        json!({ "trialStart": NOW, "trialEndsAt": NOW + 5 * DAY, "lease": lease }),
    );
    assert_eq!(h.license.start_trial(), trial_status());
    assert_eq!(h.http.actions(), ["claim", "trial"]);
    assert_has(
        &h.state(),
        json!({ "purchaseToken": TOKEN, "claimAskedAt": NOW }),
    );
}

// --- an activation and a deactivation clear the purchase token ---------------------------

#[test]
fn a_typed_key_clears_the_token_so_a_later_deactivate_leaves_no_pickup_pending() {
    let h = Harness::with(held(0, StoredState::default()));
    let lease = h.lease("KEY-1", "perpetual", 30 * 86_400, NOW);
    h.http.answer(
        "activate",
        200,
        json!({ "lease": lease, "activationId": "act_1" }),
    );
    h.license.activate("KEY-1").expect("activated");
    assert!(holds_no_purchase(&h));
}

#[test]
fn deactivate_clears_the_token() {
    let h = Harness::with(held(
        0,
        StoredState {
            key: Some("KEY-1".into()),
            activation_id: Some("act_1".into()),
            ..StoredState::default()
        },
    ));
    h.http.answer("deactivate", 200, json!({}));
    h.license.deactivate().expect("deactivated");
    assert!(holds_no_purchase(&h));
}

// --- a key bought from a trial model's own price (model trial) ---------------------------

#[test]
fn a_bought_licence_lease_whose_model_is_trial_licenses_and_is_never_read_as_a_trial() {
    let h = Harness::with(held(0, StoredState::default()));
    let answer = bought(&h, "trial");
    h.http.answer("claim", 200, answer);
    assert_eq!(h.license.status(false), licensed_as(BOUGHT_KEY));
    h.http.fail(true);
    assert_eq!(h.license.status(false), licensed_as(BOUGHT_KEY));
    assert!(!holds(&h, "trialLease"));
}

#[test]
fn a_typed_licence_lease_whose_model_is_trial_licenses_offline() {
    let h = Harness::empty();
    let lease = h.lease("KEY-1", "trial", 30 * 86_400, NOW);
    h.set_state(licensed("KEY-1", &lease));
    h.http.fail(true);
    assert_eq!(h.license.status(false), licensed_as("KEY-1"));
}

// --- every answer parser ignores members it does not know ---------------------------------

#[test]
fn every_answer_parser_ignores_members_it_does_not_know() {
    let extra = json!({ "future": { "nested": [1, 2] }, "note": "added by a later server" });
    let with_extra = |answer: Value| {
        let mut answer = answer;
        for (name, value) in extra.as_object().expect("an object") {
            answer[name] = value.clone();
        }
        answer
    };
    let h = Harness::with(held(0, StoredState::default()));
    let lease = h.lease("KEY-1", "perpetual", 30 * 86_400, NOW);
    h.http.answer(
        "claim",
        200,
        with_extra(json!({ "key": "KEY-1", "activationId": "act_1", "lease": lease })),
    );
    h.http
        .answer("validate", 200, with_extra(json!({ "lease": lease })));
    h.http.answer(
        "activate",
        200,
        with_extra(json!({ "lease": lease, "activationId": "act_1" })),
    );
    assert_eq!(h.license.status(false), licensed_as("KEY-1"));
    assert_eq!(h.license.refresh(), licensed_as("KEY-1"));
    h.license.activate("KEY-1").expect("activated");

    let trial = Harness::empty();
    let trial_lease = trial.lease("trial:fp-test", "trial", 5 * 86_400, NOW);
    trial.http.answer(
        "trial",
        200,
        with_extra(
            json!({ "trialStart": NOW, "trialEndsAt": NOW + 5 * DAY, "lease": trial_lease }),
        ),
    );
    assert_eq!(trial.license.start_trial(), trial_status());
}

// --- a bought key's lease checked before it is saved ----------------------------------------

/// Base64url (no padding) decoded.
fn b64url_decode(text: &str) -> Vec<u8> {
    let sextet = |c: u8| -> u32 {
        match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'-' => 62,
            b'_' => 63,
            _ => panic!("not base64url: {c}"),
        }
    };
    let (mut out, mut buffer, mut bits) = (Vec::new(), 0u32, 0);
    for c in text.bytes() {
        buffer = (buffer << 6) | sextet(c);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xff).expect("a byte"));
        }
    }
    out
}

/// `lease` with one claim changed (`lim` 3 to 99) and its payload encoded
/// again, under the header and signature it was signed with.
fn tampered(lease: &str) -> String {
    let parts: Vec<&str> = lease.split('.').collect();
    let mut claims: Value = serde_json::from_slice(&b64url_decode(parts[1])).expect("claims");
    claims["lim"] = json!(99);
    let payload = b64url(claims.to_string().as_bytes());
    format!("{}.{payload}.{}", parts[0], parts[2])
}

#[test]
fn refuses_a_bought_key_whose_lease_another_key_signed_or_someone_changed_dropping_the_secret() {
    let h = Harness::empty();
    let foreign = TestKey::new(9).lease(BOUGHT_KEY, "perpetual", 30 * 86_400, NOW);
    let changed = tampered(&h.lease(BOUGHT_KEY, "perpetual", 30 * 86_400, NOW));
    // Both refused by their signature, and nothing else: their claims are a lease's.
    let key = KeySet::parse(&h.key.public_jwk()).expect("the product's key");
    for lease in [&foreign, &changed] {
        assert!(matches!(
            verify_lease(lease, &key, NOW),
            VerifyResult::Invalid {
                reason: VerifyFailure::BadSignature,
                ..
            }
        ));
    }
    let trial_lease = h.lease("trial:fp-test", "trial", 5 * 86_400, NOW);
    for lease in [foreign, changed] {
        let answer = json!({ "key": BOUGHT_KEY, "activationId": "act_1", "lease": lease });
        // status(): answered as with no secret, the secret dropped, no key stored.
        h.set_state(trial_state(&h, held(0, StoredState::default())));
        h.http.answer("claim", 200, answer.clone());
        assert_eq!(h.license.status(false), trial_status());
        assert!(holds_no_purchase(&h));
        assert!(!holds(&h, "key") && !holds(&h, "lease"));
        // start_trial(): the same, and the trial then asked.
        h.set_state(held(0, StoredState::default()));
        h.http.answer(
            "trial",
            200,
            json!({ "trialStart": NOW, "trialEndsAt": NOW + 5 * DAY, "lease": trial_lease }),
        );
        assert_eq!(h.license.start_trial(), trial_status());
        assert!(holds_no_purchase(&h));
        assert!(!holds(&h, "key") && !holds(&h, "lease"));
    }
    assert_eq!(
        h.http.actions(),
        ["claim", "claim", "trial", "claim", "claim", "trial"]
    );
}

#[test]
fn judges_a_bought_keys_lease_at_the_servers_time_bringing_the_clock_guard_back() {
    // The guard is 60 days ahead of the device clock (a clock that once
    // jumped): judged by it, the 30-day lease signed now has run out.
    let h = Harness::with(held(
        -60 * DAY,
        StoredState {
            last_seen: Some(NOW + 60 * DAY),
            ..StoredState::default()
        },
    ));
    let answer = bought(&h, "perpetual");
    h.http.answer("claim", 200, answer);
    assert_eq!(h.license.refresh(), licensed_as(BOUGHT_KEY));
    assert_eq!(h.state().key.as_deref(), Some(BOUGHT_KEY));
    assert_eq!(h.state().last_seen, Some(NOW));
}

#[test]
fn claims_under_a_machine_id_with_its_kind_and_keys_the_install_by_it() {
    let h = Harness::new(
        Some(held(0, StoredState::default())),
        Opts {
            fingerprint: Some(Arc::new(IdentifyingFingerprinter(|| DeviceIdentity {
                fingerprint: Some("fp-machine".into()),
                kind: Some(DeviceKind::Machine),
                alternates: Vec::new(),
            }))),
            ..Opts::default()
        },
    );
    let answer = bought(&h, "perpetual");
    h.http.answer("claim", 200, answer);
    assert_eq!(h.license.status(false), licensed_as(BOUGHT_KEY));
    assert_eq!(
        h.http.body_of("claim"),
        json!({
            "token": TOKEN,
            "fingerprint": "fp-machine",
            "fingerprintKind": "machine",
            "fingerprints": ["fp-machine"],
            "major": 1,
            "kids": [h.key.kid()],
        })
    );
    assert_eq!(h.state().device_kind, Some(DeviceKind::Machine));
}

// --- each stamp and check on its own clock --------------------------------------------------

/// `state` with the clock guard (`last_seen`) a day ahead of the device clock (NOW).
fn guard_a_day_ahead(state: StoredState) -> StoredState {
    StoredState {
        last_seen: Some(NOW + DAY),
        ..state
    }
}

#[test]
fn judges_the_links_day_by_the_guarded_clock_and_the_wait_by_the_device_clock() {
    // A link an hour old by the device clock is stale by the guarded one.
    let stale = Harness::with(held(HOUR, guard_a_day_ahead(StoredState::default())));
    stale.http.answer("claim", 200, json!({ "pending": true }));
    stale.license.status(false);
    assert!(!stale.http.called("claim"));
    // Fresh by the guarded clock, but asked a second ago by the device clock: the wait still runs.
    let waiting = Harness::with(held(
        -(DAY - HOUR),
        guard_a_day_ahead(StoredState {
            claim_asked_at: Some(NOW - 1000),
            ..StoredState::default()
        }),
    ));
    waiting.license.status(false);
    assert!(!waiting.http.called("claim"));
    // Neither was dropped: refresh() asks.
    stale.license.refresh();
    assert_eq!(stale.http.count("claim"), 1);
}

#[test]
fn stamps_the_link_by_the_guarded_clock_and_the_wait_by_the_device_clock() {
    let h = Harness::with(guard_a_day_ahead(StoredState::default()));
    h.license.purchase_url(LINK).expect("a link");
    assert_eq!(h.state().purchase_token_at, Some(NOW + DAY));
    h.http.answer("claim", 200, json!({ "pending": true }));
    h.license.status(false);
    assert_eq!(h.state().claim_asked_at, Some(NOW));
}

#[test]
fn drops_a_secret_past_its_bound_by_the_guarded_clock() {
    // 29 days by the device clock, 30 by the guarded one.
    let h = Harness::with(held(
        PURCHASE_TOKEN_MS - DAY,
        guard_a_day_ahead(StoredState::default()),
    ));
    h.http.answer("claim", 200, json!({ "pending": true }));
    assert_eq!(h.license.refresh(), unlicensed());
    assert!(!h.http.called("claim"));
    assert!(!holds(&h, "purchaseToken"));
}

// --- calls behind a stuck one, and start_trial() with nothing to claim under ----------------

#[test]
fn behind_a_call_that_has_not_finished_a_secret_past_its_bound_is_busy() {
    let h = quick(Some(held(PURCHASE_TOKEN_MS, StoredState::default())));
    let gate = h.storage.hang_next_read();
    thread::scope(|scope| {
        let _stuck = scope.spawn(|| h.license.status(false));
        gate.await_reached();
        assert!(matches!(h.license.purchase_url(LINK), Err(Error::Busy)));
        assert_eq!(h.storage.writes(), 0);
        gate.release();
    });
}

#[test]
fn start_trial_behind_a_call_that_has_not_finished_claims_nothing() {
    let h = quick(Some(held(0, StoredState::default())));
    let gate = h.storage.hang_next_read();
    thread::scope(|scope| {
        let _stuck = scope.spawn(|| h.license.status(false));
        gate.await_reached();
        assert_eq!(h.license.start_trial(), stalled(unlicensed()));
        assert!(!h.http.called("claim"));
        assert_eq!(h.storage.writes(), 0);
        gate.release();
    });
}

/// A trial stash that has nothing, and counts its reads.
#[derive(Default)]
struct CountingStash(AtomicUsize);

impl RegistryStash for CountingStash {
    fn read_trial_start(&self) -> Option<i64> {
        self.0.fetch_add(1, Ordering::SeqCst);
        None
    }
    fn write_trial_start(&self, _ms: i64) {}
}

#[test]
fn start_trial_with_no_fingerprint_at_all_sends_nothing_starts_the_wait_and_goes_on_to_the_trial() {
    let h = Harness::with(held(0, StoredState::default()));
    let stash = Arc::new(CountingStash::default());
    let config = h
        .config
        .clone()
        .registry(stash.clone())
        .fingerprinter(Arc::new(IdentifyingFingerprinter(DeviceIdentity::default)));
    let license = License::new(config).expect("a licence");
    assert_eq!(license.start_trial(), unlicensed());
    // No claim, as no answer: the secret kept and the wait started.
    assert!(h.http.calls().is_empty());
    assert_has(
        &h.state(),
        json!({ "purchaseToken": TOKEN, "claimAskedAt": NOW }),
    );
    // Then the trial ask, which reads the stash (and, as it cannot tell who
    // the device is, sends nothing).
    assert_eq!(stash.0.load(Ordering::SeqCst), 1);
}

// --- the secret goes nowhere but the claim --------------------------------------------------

#[test]
fn sends_the_secret_in_no_request_but_the_claim() {
    let h = Harness::empty();
    let trial_lease = h.lease("trial:fp-test", "trial", 5 * 86_400, NOW);
    h.set_state(held(
        0,
        StoredState {
            trial_start: Some(NOW - 2 * DAY),
            trial_ends_at: Some(NOW + 5 * DAY),
            trial_lease: Some(trial_lease.clone()),
            last_seen: Some(NOW),
            ..StoredState::default()
        },
    ));
    let lease = h.lease("KEY-1", "perpetual", 30 * 86_400, NOW);
    h.http.answer("claim", 200, json!({ "pending": true }));
    h.http.answer(
        "trial",
        200,
        json!({ "trialStart": NOW - 2 * DAY, "trialEndsAt": NOW + 5 * DAY, "lease": trial_lease }),
    );
    h.http.answer(
        "activate",
        200,
        json!({ "lease": lease, "activationId": "act_1" }),
    );
    h.http.answer("validate", 200, json!({ "lease": lease }));
    h.license.status(false); // a claim and the launch report
    h.license.launch_report_settled();
    h.license.start_trial(); // a claim and the trial ask
    h.license.activate("KEY-1").expect("activated"); // the secret still held when its body is made
                                                     // A key and a secret stored together (purchase_url's save is unbased: it
                                                     // can land on a key another process activated meanwhile): a validate and a
                                                     // deactivate made while the secret is held.
    h.set_state(held(0, licensed("KEY-1", &lease)));
    h.http.answer("deactivate", 200, json!({}));
    h.license.refresh();
    h.license.deactivate().expect("deactivated");
    assert_eq!(
        h.http.actions(),
        [
            "claim",
            "trial",
            "claim",
            "trial",
            "activate",
            "validate",
            "deactivate"
        ]
    );
    for (action, body) in h.http.calls() {
        assert_eq!(
            body.to_string().contains(TOKEN),
            action == "claim",
            "{action}"
        );
    }
}

// --- the claim saves other than a bought key's ----------------------------------------------

/// State in memory whose second read finds `other` stored: another process
/// saved it between a call's read and its save.
struct SwapOnSecondRead {
    state: std::sync::Mutex<Option<StoredState>>,
    reads: AtomicUsize,
    other: StoredState,
}

impl StorageAdapter for SwapOnSecondRead {
    fn read(&self, _read_only: bool) -> io::Result<Option<StoredState>> {
        if self.reads.fetch_add(1, Ordering::SeqCst) + 1 == 2 {
            *lock(&self.state) = Some(self.other.clone());
        }
        Ok(lock(&self.state).clone())
    }
    fn write(&self, state: &StoredState) -> io::Result<()> {
        *lock(&self.state) = Some(state.clone());
        Ok(())
    }
}

#[test]
fn saves_a_drop_past_the_bound_about_its_secret_so_another_processs_stays() {
    let h = Harness::empty();
    let other = StoredState {
        purchase_token: Some("another-process-made-this-one-meanwhile".into()),
        purchase_token_at: Some(NOW),
        ..StoredState::default()
    };
    let storage = Arc::new(SwapOnSecondRead {
        state: std::sync::Mutex::new(Some(held(PURCHASE_TOKEN_MS, StoredState::default()))),
        reads: AtomicUsize::new(0),
        other: other.clone(),
    });
    let license = License::new(h.config.clone().storage(storage.clone())).expect("a licence");
    assert_eq!(license.refresh(), unlicensed());
    assert!(!h.http.called("claim"));
    let saved = lock(&storage.state).clone().expect("a state");
    assert_eq!(saved.purchase_token, other.purchase_token);
    assert_eq!(saved.purchase_token_at, Some(NOW));
}

#[test]
fn a_drop_lands_after_the_same_secrets_link_was_handed_out_again() {
    // A spent secret is dead however recently its link was handed out.
    let h = Harness::with(held(DAY / 2, StoredState::default()));
    let storage = h.storage.clone();
    h.http.on("claim", move |_| {
        let mut state = storage.get().expect("a state");
        state.purchase_token_at = Some(NOW + 1000);
        state.claim_asked_at = None;
        storage.set(Some(state));
        reply(409, json!({ "error": "spent" }))
    });
    h.license.status(false);
    assert!(holds_no_purchase(&h));
}

#[test]
fn a_wait_or_a_drop_the_disk_refuses_leaves_the_answer_standing() {
    for answer in [None, Some((409, json!({ "error": "spent" })))] {
        let h = Harness::with(held(0, StoredState::default()));
        match answer {
            None => h.http.fail(true),
            Some((status, body)) => h.http.answer("claim", status, body),
        }
        h.storage.fail_writes(true);
        assert_eq!(h.license.status(false), unlicensed());
        assert_eq!(h.http.count("claim"), 1);
    }
}

// --- a drop the disk refuses, and the secret's own clock ------------------------------------

#[test]
fn a_past_bound_drop_the_disk_refuses_leaves_the_answer_standing() {
    let h = Harness::empty();
    h.set_state(trial_state(
        &h,
        held(PURCHASE_TOKEN_MS, StoredState::default()),
    ));
    h.storage.fail_writes(true);
    assert_eq!(h.license.status(false), trial_status());
    assert!(!h.http.called("claim"));
}

#[test]
fn purchase_url_keeps_a_secret_by_the_guarded_clock() {
    // 29 days old by the device clock, 30 by the guarded one: past its bound,
    // so a new secret, its link stamped with the guarded time.
    let h = Harness::with(held(
        PURCHASE_TOKEN_MS - DAY,
        guard_a_day_ahead(StoredState::default()),
    ));
    let url = h
        .license
        .purchase_url(LINK)
        .expect("a link")
        .expect("a link");
    assert_ne!(url, link_for(TOKEN));
    assert_ne!(h.state().purchase_token.as_deref(), Some(TOKEN));
    assert_eq!(h.state().purchase_token_at, Some(NOW + DAY));
}
