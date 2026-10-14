//! A stored offline licence file beside a key and beside a trial: one that
//! licenses this build answers first, with no ask, no write and no launch
//! report; one that does not answers only when nothing else does.

mod common;

use common::*;
use keygrant::{
    DeactivateResult, LicenseFileKind, LicenseReason, LicenseState, LicenseStatus, Refusal,
    StoredState,
};
use serde_json::json;

const DAY_S: i64 = 86_400;

/// KEY held on act_1 with `lease`, heard on major 2, seen at NOW.
fn keyed(lease: &str, file: Option<String>) -> StoredState {
    StoredState {
        key: Some(KEY.into()),
        activation_id: Some("act_1".into()),
        lease: Some(lease.into()),
        checked_major: Some(2),
        last_seen: Some(NOW),
        license_file: file,
        ..StoredState::default()
    }
}

#[test]
fn a_file_wins_over_a_held_key_which_keeps_working_once_the_file_is_removed() {
    let h = Harness::empty();
    let lease = lease_to_3(&h);
    h.set_state(keyed(&lease, Some(h.key.offline_file(json!({})))));
    let license = file_app(&h, &[]);
    assert_eq!(license.status(false), file_licensed());
    license.remove_license_file().expect("removed");
    let stored = h.state();
    assert_eq!(
        (stored.key.as_deref(), stored.lease),
        (Some(KEY), Some(lease))
    );
    assert_eq!(stored.license_file, None);
    assert_eq!(license.status(false), key_licensed());
}

#[test]
fn a_file_imported_over_a_held_key_leaves_its_seat() {
    let h = Harness::empty();
    let lease = lease_to_3(&h);
    h.set_state(keyed(&lease, None));
    let license = file_app(&h, &[]);
    let taken = license
        .import_license_file(h.key.offline_file(json!({})))
        .expect("taken");
    assert_eq!(taken.kind, LicenseFileKind::Offline);
    // Offline: the seat is neither asked about nor touched.
    assert!(h.http.calls().is_empty());
    let stored = h.state();
    assert_eq!(
        (
            stored.key.as_deref(),
            stored.activation_id.as_deref(),
            stored.lease
        ),
        (Some(KEY), Some("act_1"), Some(lease))
    );
    license.remove_license_file().expect("removed");
    assert_eq!(license.status(false), key_licensed());
}

#[test]
fn a_file_is_kept_by_an_activation_typed_or_from_a_key_file_and_still_answers() {
    let h = Harness::empty();
    let file = h.key.offline_file(json!({}));
    h.set_state(StoredState {
        license_file: Some(file.clone()),
        ..StoredState::default()
    });
    h.http.answer(
        "activate",
        200,
        json!({ "lease": lease_to_3(&h), "activationId": "act_1" }),
    );
    let license = file_app(&h, &[]);
    license.activate(KEY).expect("activated");
    assert_eq!(
        (h.state().key.as_deref(), h.state().license_file),
        (Some(KEY), Some(file.clone()))
    );
    let taken = license
        .import_license_file(h.key.key_file(json!({})))
        .expect("taken");
    assert_eq!(taken.kind, LicenseFileKind::Key);
    assert_eq!(
        (h.state().key.as_deref(), h.state().license_file),
        (Some(KEY), Some(file))
    );
    assert_eq!(license.status(false), file_licensed());
}

#[test]
fn deactivate_leaves_a_file_alone_releasing_only_the_key() {
    let h = Harness::empty();
    let file = h.key.offline_file(json!({}));
    h.set_state(keyed(&lease_to_3(&h), Some(file.clone())));
    h.http.answer("deactivate", 200, json!({}));
    let license = file_app(&h, &[]);
    assert_eq!(
        license.deactivate().expect("deactivated"),
        DeactivateResult { released: true }
    );
    assert_eq!((h.state().key, h.state().license_file), (None, Some(file)));
    assert_eq!(license.status(false), file_licensed());
}

/// KEY held with a second under a day left of its two-day lease (less than
/// half its life, so due to renew early), inside an open upgrade window: an
/// unforced status asks about it and notes this major.
fn key_due(h: &Harness, file: Option<String>) {
    let over = json!({ "major": 3, "upgradeUntil": NOW / 1000 + 30 * DAY_S });
    let lease = h.key.lease_with(NOW - DAY - 1000, 2 * DAY_S, over);
    h.set_state(keyed(&lease, file));
}

#[test]
fn a_file_that_licenses_leaves_a_held_key_unrenewed_and_unnoted_forced_or_not() {
    let h = Harness::empty();
    key_due(&h, Some(h.key.offline_file(json!({}))));
    let license = file_app(&h, &[]);
    assert_eq!(license.status(false), file_licensed());
    assert_eq!(license.refresh(), file_licensed());
    assert!(h.http.calls().is_empty());
    assert_eq!(h.state().kept_major, None);
}

#[test]
fn with_no_file_the_same_key_is_asked_about_and_noted_unforced_so_the_test_above_can_see_it() {
    let h = Harness::empty();
    key_due(&h, None);
    file_app(&h, &[]).status(false);
    assert_eq!(h.validates(), 1);
    assert_eq!(h.state().kept_major, Some(2));
}

#[test]
fn a_file_that_does_not_license_gives_way_to_the_key() {
    let h = Harness::empty();
    h.set_state(keyed(&lease_to_3(&h), Some(h.key.offline_file(json!({})))));
    assert_eq!(file_app(&h, &["lf_1"]).status(false), key_licensed());
}

#[test]
fn a_file_that_does_not_license_gives_way_to_the_keys_stored_refusal() {
    let h = Harness::empty();
    h.set_state(StoredState {
        key: Some(KEY.into()),
        activation_id: Some("act_1".into()),
        refused: Some(Refusal::Revoked),
        license_file: Some(h.key.offline_file(json!({}))),
        ..StoredState::default()
    });
    h.http.fail(true);
    let mut revoked = LicenseStatus::new(LicenseState::Invalid);
    revoked.reason = Some(LicenseReason::Revoked);
    revoked.key = Some(KEY.into());
    assert_eq!(file_app(&h, &["lf_1"]).status(false), revoked);
    // The refusal is asked about, which a file that licenses spares (below).
    assert_eq!(h.validates(), 1);
}

/// What a trial started at NOW, five days long, answers while it runs.
fn running_trial() -> LicenseStatus {
    let mut running = LicenseStatus::new(LicenseState::Trial);
    running.trial_ends_at = Some(NOW + 5 * DAY);
    running.trial_days_left = Some(5);
    running
}

/// A trial this device started, with five days of its lease left from `now`.
fn trial(h: &Harness, start: i64, file: Option<String>) -> StoredState {
    StoredState {
        trial_start: Some(start),
        trial_ends_at: Some(start + 5 * DAY),
        trial_lease: Some(h.lease("trial:fp-test", "trial", 5 * DAY_S, start)),
        license_file: file,
        ..StoredState::default()
    }
}

#[test]
fn a_file_that_licenses_answers_before_a_running_trial_whose_launch_is_then_not_reported() {
    let h = Harness::empty();
    // No stamp of a report or a trial answer: a running trial would report its launch now.
    h.set_state(trial(&h, NOW, Some(h.key.offline_file(json!({})))));
    let license = file_app(&h, &[]);
    let writes = h.storage.writes();
    assert_eq!(license.status(false), file_licensed());
    license.launch_report_settled();
    assert!(h.http.calls().is_empty());
    assert_eq!(h.storage.writes(), writes);
}

#[test]
fn with_no_file_the_same_trial_reports_its_launch_so_the_test_above_can_see_it() {
    let h = Harness::empty();
    h.set_state(trial(&h, NOW, None));
    let license = file_app(&h, &[]);
    assert_eq!(license.status(false), running_trial());
    license.launch_report_settled();
    assert_eq!(h.http.count("trial"), 1);
    assert_eq!(h.state().trial_reported_at, Some(NOW));
}

#[test]
fn a_file_that_licenses_answers_over_a_keys_stored_refusal_asking_nothing() {
    let h = Harness::empty();
    h.set_state(StoredState {
        key: Some(KEY.into()),
        activation_id: Some("act_1".into()),
        refused: Some(Refusal::Revoked),
        license_file: Some(h.key.offline_file(json!({}))),
        ..StoredState::default()
    });
    let writes = h.storage.writes();
    assert_eq!(file_app(&h, &[]).status(true), file_licensed());
    assert!(h.http.calls().is_empty());
    assert_eq!(h.storage.writes(), writes);
}

#[test]
fn a_file_that_does_not_license_gives_way_to_a_running_trial() {
    let h = Harness::empty();
    h.set_state(StoredState {
        trial_at: Some(NOW),
        ..trial(&h, NOW, Some(h.key.offline_file(json!({ "major": 1 }))))
    });
    assert_eq!(file_app(&h, &[]).status(false), running_trial());
}

#[test]
fn a_file_that_does_not_license_answers_in_place_of_an_ended_trial() {
    let h = Harness::empty();
    h.set_state(StoredState {
        trial_at: Some(NOW),
        ..trial(
            &h,
            NOW - 10 * DAY,
            Some(h.key.offline_file(json!({ "major": 1 }))),
        )
    });
    assert_eq!(
        file_app(&h, &[]).status(false),
        file_invalid(LicenseReason::Upgrade)
    );
    assert!(h.http.calls().is_empty());
}
