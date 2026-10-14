//! Licence files: an offline file imported, stored and answered with no
//! network; every refusal; a stored file that does not license this build;
//! a key file activating its key; and the calls behind one that has not
//! finished.

mod common;

use std::collections::BTreeMap;
use std::thread;
use std::time::Duration;

use common::*;
use keygrant::{
    Entitlement, Error, ImportError, ImportedLicenseFile, License, LicenseError, LicenseFileError,
    LicenseFileKind, LicenseReason, LicenseState, LicenseStatus, LicensedTo,
};
use serde_json::json;

fn offline_taken(licensed_to: LicensedTo) -> ImportedLicenseFile {
    ImportedLicenseFile {
        kind: LicenseFileKind::Offline,
        licensed_to,
    }
}

fn refused_with(result: Result<ImportedLicenseFile, ImportError>) -> LicenseFileError {
    match result {
        Err(ImportError::File(error)) => error,
        other => panic!("not refused: {other:?}"),
    }
}

#[test]
fn an_offline_file_is_taken_from_its_text_with_no_network_stored_and_licenses_as_the_file() {
    let h = Harness::empty();
    let token = h.key.offline_file(json!({}));
    let license = file_app(&h, &[]);
    let taken = license
        .import_license_file(license_file_text(&token))
        .expect("taken");
    assert_eq!(taken, offline_taken(ada()));
    assert_eq!(h.state().license_file.as_deref(), Some(token.as_str()));
    assert_eq!(license.status(false), file_licensed());
    assert!(h.http.calls().is_empty());
}

#[test]
fn an_offline_file_is_taken_as_bytes_as_a_drop_handler_reads_them() {
    let h = Harness::empty();
    let token = h.key.offline_file(json!({}));
    let bytes = license_file_text(&token).into_bytes();
    let taken = file_app(&h, &[]).import_license_file(bytes);
    assert_eq!(taken.ok(), Some(offline_taken(ada())));
    assert_eq!(h.state().license_file, Some(token));
}

#[test]
fn a_stored_offline_file_never_goes_online_refresh_included_though_the_key_beside_it_has_run_out() {
    let h = Harness::empty();
    // Without the file, this run-out lease would be asked about, and the wait after the ask saved.
    let expired = h.key.lease_with(NOW - 2 * DAY, 3600, json!({ "major": 3 }));
    let mut stored = licensed(KEY, &expired);
    stored.license_file = Some(h.key.offline_file(json!({})));
    h.set_state(stored);
    let license = file_app(&h, &[]);
    assert_eq!(license.status(false), file_licensed());
    assert_eq!(license.refresh(), file_licensed());
    assert!(h.http.calls().is_empty());
    assert_eq!(h.storage.writes(), 0);
}

#[test]
fn with_no_file_the_same_run_out_key_is_asked_about_and_the_answer_saved() {
    let h = Harness::empty();
    let expired = h.key.lease_with(NOW - 2 * DAY, 3600, json!({ "major": 3 }));
    h.set_state(licensed(KEY, &expired));
    file_app(&h, &[]).status(false);
    assert_eq!(h.validates(), 1);
    assert!(h.storage.writes() > 0);
}

#[test]
fn a_file_says_when_it_is_a_test_licence_and_carries_its_entitlements_which_has_reads() {
    let h = Harness::empty();
    let ent = json!({ "edition": "pro", "cloud": true });
    h.set_state(stored_file(
        &h.key.offline_file(json!({ "test": true, "ent": ent })),
    ));
    let license = file_app(&h, &[]);
    let mut expected = file_licensed();
    expected.test = true;
    expected.entitlements = BTreeMap::from([
        ("edition".to_owned(), Entitlement::Text("pro".into())),
        ("cloud".to_owned(), Entitlement::Bool(true)),
    ]);
    assert_eq!(license.status(false), expected);
    assert!(license.has("cloud"));
    assert_eq!(
        license.require("export").map_err(|e| e.entitlement),
        Err("export".to_owned())
    );
}

#[test]
fn removing_no_file_is_no_error_and_leaves_unlicensed_as_it_was() {
    let h = Harness::empty();
    let license = file_app(&h, &[]);
    license.remove_license_file().expect("removed");
    assert_eq!(
        license.status(false),
        LicenseStatus::new(LicenseState::Unlicensed)
    );
}

#[test]
fn a_file_replaces_the_one_stored_before_and_a_file_refused_leaves_it() {
    let h = Harness::empty();
    let license = file_app(&h, &[]);
    license
        .import_license_file(h.key.offline_file(json!({})))
        .expect("the first");
    let second = h
        .key
        .offline_file(json!({ "fid": "lf_3", "name": "Grace Hopper", "email": null }));
    let grace = LicensedTo {
        name: Some("Grace Hopper".into()),
        email: None,
    };
    assert_eq!(
        license.import_license_file(&second).ok(),
        Some(offline_taken(grace))
    );
    let other = h.key.offline_file(json!({ "product": "maindeck" }));
    assert_eq!(
        refused_with(license.import_license_file(other)),
        LicenseFileError::WrongProduct
    );
    assert_eq!(h.state().license_file, Some(second));
}

#[test]
fn a_file_refused_names_each_reason_and_stores_nothing() {
    let h = Harness::empty();
    let token = h.key.offline_file(json!({}));
    let swapped = payload_swapped(&token, &h.key.offline_file(json!({ "major": 9 })));
    let by_other = TestKey::new(9).offline_file(json!({}));
    let license = file_app(&h, &["lf_9"]);
    let cases: Vec<(Vec<u8>, LicenseFileError)> = vec![
        (Vec::new(), LicenseFileError::Malformed),
        (
            format!("{token}\n{token}").into(),
            LicenseFileError::Malformed,
        ),
        (vec![0xff, 0xfe, 0x00], LicenseFileError::Malformed),
        (
            h.lease(KEY, "perpetual", 86_400, NOW).into(),
            LicenseFileError::NotALicenseFile,
        ),
        (swapped.into(), LicenseFileError::BadSignature),
        (by_other.into(), LicenseFileError::UnknownKey),
        (
            h.key.offline_file(json!({ "product": "maindeck" })).into(),
            LicenseFileError::WrongProduct,
        ),
        (
            h.key.offline_file(json!({ "fid": "lf_9" })).into(),
            LicenseFileError::RevokedFile,
        ),
        (
            h.key.offline_file(json!({ "major": 1 })).into(),
            LicenseFileError::Upgrade,
        ),
        (
            h.key.offline_file(json!({ "minMajor": 3 })).into(),
            LicenseFileError::Outdated,
        ),
    ];
    for (content, error) in cases {
        assert_eq!(refused_with(license.import_license_file(&content)), error);
    }
    assert_eq!(h.storage.get().and_then(|s| s.license_file), None);
    assert!(h.http.calls().is_empty());
}

#[test]
fn an_import_or_a_removal_that_cannot_be_saved_fails_and_leaves_the_stored_file() {
    let h = Harness::empty();
    let stored = h.key.offline_file(json!({}));
    h.set_state(stored_file(&stored));
    let license = file_app(&h, &[]);
    h.storage.fail_writes(true);
    let imported = license.import_license_file(h.key.offline_file(json!({ "fid": "lf_3" })));
    assert!(
        matches!(imported, Err(ImportError::Storage(_))),
        "{imported:?}"
    );
    let removed = license.remove_license_file();
    assert!(matches!(removed, Err(Error::Storage(_))), "{removed:?}");
    assert_eq!(h.state().license_file, Some(stored));
}

#[test]
fn a_stored_file_is_revoked_file_once_an_update_lists_it_with_whose_it_is_and_no_entitlements() {
    let h = Harness::empty();
    h.set_state(stored_file(
        &h.key.offline_file(json!({ "ent": { "cloud": true } })),
    ));
    let license = file_app(&h, &["lf_0", "lf_1"]);
    assert_eq!(
        license.status(false),
        file_invalid(LicenseReason::RevokedFile)
    );
    assert!(!license.has("cloud"));
}

#[test]
fn a_stored_file_is_upgrade_on_a_build_newer_than_it_covers_and_outdated_on_one_older() {
    let h = Harness::empty();
    h.set_state(stored_file(
        &h.key.offline_file(json!({ "minMajor": 2, "major": 3 })),
    ));
    assert_eq!(
        h.license_for(4).status(false),
        file_invalid(LicenseReason::Upgrade)
    );
    assert_eq!(
        h.license_for(1).status(false),
        file_invalid(LicenseReason::Outdated)
    );
}

#[test]
fn a_major_past_the_64_bit_range_is_a_whole_number_like_any_other_never_refused_for_its_size() {
    // The reference compares majors as doubles; this port holds one past
    // i64 at its end, which judges every build the same way.
    let h = Harness::empty();
    let license = file_app(&h, &[]);
    h.set_state(stored_file(&h.key.offline_file(json!({ "major": 1e20 }))));
    assert_eq!(license.status(false), file_licensed());
    h.set_state(stored_file(
        &h.key
            .offline_file(json!({ "major": 1e20, "minMajor": 1e19 })),
    ));
    assert_eq!(license.status(false), file_invalid(LicenseReason::Outdated));
}

#[test]
fn a_stored_value_that_is_not_text_is_no_file_as_every_field_of_the_wrong_type_is() {
    // The reference reads one as tampered; this port reads a stored field of
    // the wrong type as absent, every field alike. Either way it licenses nothing.
    let h = Harness::empty();
    let license = file_app(&h, &[]);
    for value in [json!(42), json!(null), json!({ "token": "x" })] {
        h.set_state(state(json!({ "licenseFile": value })));
        assert_eq!(
            license.status(false),
            LicenseStatus::new(LicenseState::Unlicensed),
            "{value}"
        );
    }
    // The control: the same field holding text is the stored file.
    let token = h.key.offline_file(json!({}));
    h.set_state(state(json!({ "licenseFile": token })));
    assert_eq!(license.status(false), file_licensed());
}

#[test]
fn a_stored_token_edited_or_a_stored_key_file_or_none_at_all_is_tampered() {
    let h = Harness::empty();
    let token = h.key.offline_file(json!({}));
    let edited = payload_swapped(&token, &h.key.offline_file(json!({ "major": 9 })));
    let license = file_app(&h, &[]);
    for stored in [edited, h.key.key_file(json!({})), String::new()] {
        h.set_state(stored_file(&stored));
        assert_eq!(
            license.status(false),
            file_unread(LicenseReason::Tampered),
            "{stored:?}"
        );
    }
}

#[test]
fn a_stored_file_survives_a_rotation_while_the_build_carries_its_key_and_is_unknown_key_after() {
    let h = Harness::empty();
    let next = TestKey::new(11);
    h.set_state(stored_file(&h.key.offline_file(json!({}))));
    let set = |keys: &[&TestKey]| {
        let jwks: Vec<serde_json::Value> = keys
            .iter()
            .map(|k| serde_json::from_str(&k.public_jwk()).expect("a JWK"))
            .collect();
        let mut config = h.config.clone().major(2);
        config.public_jwk = json!({ "keys": jwks }).to_string();
        License::new(config).expect("a valid configuration")
    };
    // A build after the switch: the new key signs, the old one is kept because it signed files.
    assert_eq!(set(&[&next, &h.key]).status(false), file_licensed());
    // A build that dropped it (a leaked key): the file must be signed again.
    assert_eq!(
        set(&[&next]).status(false),
        file_unread(LicenseReason::UnknownKey)
    );
}

#[test]
fn a_key_file_activates_its_key_online_as_a_typed_key_judging_no_version_and_stores_no_file() {
    let h = Harness::empty();
    let lease = lease_to_3(&h);
    h.http.answer(
        "activate",
        200,
        json!({ "lease": lease, "activationId": "act_1" }),
    );
    let license = file_app(&h, &[]);
    // Versions on a key file are not read: this build is outside them.
    let file = h.key.key_file(json!({ "major": 1, "minMajor": 1 }));
    let taken = license
        .import_license_file(license_file_text(&file))
        .expect("taken");
    let email_only = LicensedTo {
        name: None,
        email: Some(ADA_EMAIL.into()),
    };
    assert_eq!(
        taken,
        ImportedLicenseFile {
            kind: LicenseFileKind::Key,
            licensed_to: email_only
        }
    );
    // The key as a typed one is sent: normalised.
    let body = h.http.body_of("activate");
    assert_eq!((&body["key"], &body["major"]), (&json!(KEY), &json!(2)));
    let stored = h.state();
    assert_eq!(stored.key.as_deref(), Some(KEY));
    assert_eq!(stored.lease, Some(lease));
    assert_eq!(stored.license_file, None);
    assert_eq!(license.status(false), key_licensed());
}

#[test]
fn a_key_file_answers_with_activates_error_when_the_key_does_not_activate() {
    let h = Harness::empty();
    h.http.answer("activate", 403, json!({ "error": "limit" }));
    let result = file_app(&h, &[]).import_license_file(h.key.key_file(json!({})));
    assert!(
        matches!(result, Err(ImportError::License(LicenseError::Limit))),
        "{result:?}"
    );
    h.http.fail(true);
    let result = file_app(&h, &[]).import_license_file(h.key.key_file(json!({})));
    assert!(
        matches!(result, Err(ImportError::License(LicenseError::Network))),
        "{result:?}"
    );
}

#[test]
fn a_key_file_fails_where_activate_does_the_state_unreadable_asking_nothing() {
    let h = Harness::empty();
    let license = file_app(&h, &[]);
    h.storage.fail_read(1);
    let result = license.import_license_file(h.key.key_file(json!({})));
    assert!(matches!(result, Err(ImportError::Storage(_))), "{result:?}");
    assert!(h.http.calls().is_empty());
}

#[test]
fn a_key_file_fails_where_activate_does_the_activation_not_saved() {
    let h = Harness::empty();
    h.http.answer(
        "activate",
        200,
        json!({ "lease": lease_to_3(&h), "activationId": "act_1" }),
    );
    let license = file_app(&h, &[]);
    h.storage.fail_writes(true);
    let result = license.import_license_file(h.key.key_file(json!({})));
    assert!(matches!(result, Err(ImportError::Storage(_))), "{result:?}");
    // The server answered, and the activation's save was tried: it is that save that failed.
    assert_eq!(h.http.count("activate"), 1);
    assert!(h.storage.writes() >= 1);
}

#[test]
fn a_key_file_not_for_this_app_is_refused_before_any_request() {
    let h = Harness::empty();
    let license = file_app(&h, &["lf_2"]);
    let other = h.key.key_file(json!({ "product": "maindeck" }));
    assert_eq!(
        refused_with(license.import_license_file(other)),
        LicenseFileError::WrongProduct
    );
    assert_eq!(
        refused_with(license.import_license_file(h.key.key_file(json!({})))),
        LicenseFileError::RevokedFile
    );
    assert!(h.http.calls().is_empty());
}

#[test]
fn behind_a_call_that_has_not_finished_import_and_remove_fail_busy_and_status_answers_the_file() {
    let h = Harness::new(
        None,
        Opts {
            major: Some(2),
            http_timeout: Some(Duration::from_millis(200)),
            timings: Some(fast_timings()),
            ..Opts::default()
        },
    );
    let token = h.key.offline_file(json!({}));
    h.set_state(stored_file(&token));
    let gate = h.storage.hang_next_read();
    let (imported, removed, answered) = thread::scope(|scope| {
        let _stuck = scope.spawn(|| h.license.status(false));
        gate.await_reached();
        let answers = (
            h.license.import_license_file(&token),
            h.license.remove_license_file(),
            h.license.status(false),
        );
        gate.release();
        answers
    });
    assert!(matches!(imported, Err(ImportError::Busy)), "{imported:?}");
    assert!(matches!(removed, Err(Error::Busy)), "{removed:?}");
    let mut stalled = file_licensed();
    stalled.stalled = true;
    assert_eq!(answered, stalled);
    assert_eq!(h.storage.writes(), 0);
}
