//! User-locked keys: what `activate_with` and `import_license_file_with` send
//! for the address and code a person typed, how each word the server refuses
//! a new device with comes back, and everything that never carries them. What
//! the typing is prepared as, and the word-to-error mapping, are held to the
//! conformance vectors (`typedFor`, `userLockErrors`); these are the calls.

mod common;

use common::*;
use keygrant::wire::MAX_EMAIL;
use keygrant::{
    ActivateError, ActivateOptions, ImportError, LicenseError, LicenseFileError, LicenseFileKind,
    LicenseState,
};
use serde_json::{json, Value};

const WORDS: [(&str, LicenseError); 7] = [
    ("user-email-required", LicenseError::UserEmailRequired),
    ("user-email-mismatch", LicenseError::UserEmailMismatch),
    ("user-code-required", LicenseError::UserCodeRequired),
    ("user-code-invalid", LicenseError::UserCodeInvalid),
    ("too-many-codes", LicenseError::TooManyCodes),
    ("too-many-attempts", LicenseError::TooManyAttempts),
    ("try-again", LicenseError::TryAgain),
];

fn error(result: Result<(), ActivateError>) -> Option<LicenseError> {
    result.err().and_then(|e| e.license_error())
}

fn activated(h: &Harness) {
    h.http.answer(
        "activate",
        200,
        json!({ "lease": h.lease(KEY, "perpetual", 30 * 86_400, NOW), "activationId": "act_1" }),
    );
}

fn refused(h: &Harness, status: u16, word: &str) {
    h.http.answer("activate", status, json!({ "error": word }));
}

fn has(body: &Value, field: &str) -> bool {
    body.get(field).is_some()
}

// --- activate_with --------------------------------------------------------------------

#[test]
fn activate_with_sends_what_the_person_typed_trimmed_beside_the_key_and_activate_sends_nothing() {
    let h = Harness::empty();
    activated(&h);
    h.license.activate(KEY).expect("activated");
    let body = h.http.body_of("activate");
    assert!(!has(&body, "email") && !has(&body, "code"), "{body}");

    let options = ActivateOptions::new()
        .email("  Mara@Studio.example\n")
        .code(" 123 456 ");
    h.license.activate_with(KEY, &options).expect("activated");
    let body = h.http.last_body_of("activate");
    assert_eq!(body["key"], json!(KEY));
    assert_eq!(body["email"], json!("Mara@Studio.example"));
    assert_eq!(body["code"], json!("123456"));
}

#[test]
fn a_blank_address_or_code_is_none_and_the_one_there_is_is_sent_alone() {
    let h = Harness::empty();
    refused(&h, 403, "user-email-required");
    let blank = ActivateOptions::new().email("   ").code("");
    h.license.activate_with(KEY, &blank).expect_err("refused");
    let body = h.http.last_body_of("activate");
    assert!(!has(&body, "email") && !has(&body, "code"), "{body}");

    let address = ActivateOptions::new().email("mara@studio.example");
    h.license.activate_with(KEY, &address).expect_err("refused");
    let body = h.http.last_body_of("activate");
    assert_eq!(body["email"], json!("mara@studio.example"));
    assert!(!has(&body, "code"), "{body}");

    let code = ActivateOptions::new().code("123456");
    h.license.activate_with(KEY, &code).expect_err("refused");
    let body = h.http.last_body_of("activate");
    assert_eq!(body["code"], json!("123456"));
    assert!(!has(&body, "email"), "{body}");
}

#[test]
fn each_word_of_the_server_is_its_own_error_whatever_the_status() {
    for (word, expected) in WORDS {
        for status in [403, 429, 503] {
            let h = Harness::empty();
            refused(&h, status, word);
            let options = ActivateOptions::new().email("mara@studio.example");
            assert_eq!(
                error(h.license.activate_with(KEY, &options)),
                Some(expected),
                "{status} {word}"
            );
        }
        // A word is its own error's word, for an app that sends the error to its front end.
        assert_eq!(expected.as_str(), word);
        assert_eq!(
            serde_json::to_value(expected).expect("serialises"),
            json!(word)
        );
    }
}

#[test]
fn activate_alone_answers_a_locked_keys_first_ask_as_the_error_that_asks_for_the_address() {
    let h = Harness::empty();
    refused(&h, 403, "user-email-required");
    assert_eq!(
        error(h.license.activate(KEY)),
        Some(LicenseError::UserEmailRequired)
    );
    // The word reaches an app whole through the error's own code.
    let err = h.license.activate(KEY).expect_err("refused");
    assert_eq!(err.code(), "user-email-required");
}

#[test]
fn a_rate_limit_or_an_outage_that_is_not_a_locks_word_is_network_as_it_always_was() {
    for (status, word) in [
        (429, "rate-limited"),
        (503, "unavailable"),
        (500, "internal"),
        (403, "user-something-new"),
    ] {
        let h = Harness::empty();
        refused(&h, status, word);
        let options = ActivateOptions::new().email("mara@studio.example");
        assert_eq!(
            error(h.license.activate_with(KEY, &options)),
            Some(LicenseError::Network),
            "{status} {word}"
        );
    }
    let h = Harness::empty();
    h.http.fail(true);
    let options = ActivateOptions::new().email("mara@studio.example");
    assert_eq!(
        error(h.license.activate_with(KEY, &options)),
        Some(LicenseError::Network)
    );
}

#[test]
fn nothing_about_the_address_the_code_or_a_refusal_is_stored() {
    let h = Harness::empty();
    refused(&h, 403, "user-code-required");
    let options = ActivateOptions::new()
        .email("mara@studio.example")
        .code("123456");
    assert_eq!(
        error(h.license.activate_with(KEY, &options)),
        Some(LicenseError::UserCodeRequired)
    );
    assert!(h.storage.get().is_none());
    assert_eq!(h.storage.writes(), 0);

    // Once the lease comes, the activation is stored as ever and nothing of what was typed.
    activated(&h);
    h.license.activate_with(KEY, &options).expect("activated");
    let stored = h.state().to_json().to_string();
    assert!(stored.contains(KEY), "{stored}");
    assert!(
        !stored.contains("studio") && !stored.contains("123456"),
        "{stored}"
    );
    assert_eq!(h.license.status(false).state, LicenseState::Licensed);
}

#[test]
fn an_address_too_long_and_a_code_that_is_not_six_digits_are_refused_before_the_server_is_asked() {
    let h = Harness::empty();
    refused(&h, 403, "user-email-required");
    let long = format!("{}@b.example", "a".repeat(MAX_EMAIL));
    let with = |email: &str, code: &str| {
        error(
            h.license
                .activate_with(KEY, &ActivateOptions::new().email(email).code(code)),
        )
    };
    assert_eq!(with(&long, ""), Some(LicenseError::UserEmailMismatch));
    assert_eq!(
        with("a@b.example", "12345"),
        Some(LicenseError::UserCodeInvalid)
    );
    assert_eq!(h.http.count("activate"), 0);
    // The address is judged first, and the key before both.
    assert_eq!(
        with(&"x".repeat(300), "nope"),
        Some(LicenseError::UserEmailMismatch)
    );
    assert_eq!(
        error(
            h.license
                .activate_with("", &ActivateOptions::new().email("x".repeat(300)))
        ),
        Some(LicenseError::InvalidKey)
    );
    assert_eq!(h.http.count("activate"), 0);
    // A blank one next to a code that is no code is still the code's fault.
    assert_eq!(with("", "12"), Some(LicenseError::UserCodeInvalid));
}

// --- what never carries them -----------------------------------------------------------

#[test]
fn validate_deactivate_and_a_trial_send_neither_even_after_an_activation_that_carried_them() {
    let h = Harness::empty();
    activated(&h);
    h.validate_with(&h.lease(KEY, "perpetual", 30 * 86_400, NOW));
    h.http.answer("deactivate", 200, json!({}));
    let options = ActivateOptions::new()
        .email("mara@studio.example")
        .code("123456");
    h.license.activate_with(KEY, &options).expect("activated");
    h.license.refresh();
    h.license.deactivate().expect("deactivated");
    h.license.start_trial();
    for (action, body) in h.http.calls() {
        if action != "activate" {
            assert!(
                !has(&body, "email") && !has(&body, "code"),
                "{action}: {body}"
            );
        }
    }
    assert_eq!(h.http.count("validate"), 1);
    assert_eq!(h.http.count("deactivate"), 1);
    assert!(h.http.count("trial") > 0);
}

// --- a key file ------------------------------------------------------------------------

#[test]
fn a_key_files_key_is_asked_with_the_same_address_and_code_and_its_answers_come_back_as_activates()
{
    let h = Harness::empty();
    refused(&h, 403, "user-code-required");
    let license = file_app(&h, &[]);
    let file = license_file_text(&h.key.key_file(json!({})));
    let options = ActivateOptions::new()
        .email(" Ada@Example.com ")
        .code("123 456");
    let result = license.import_license_file_with(&file, &options);
    assert!(
        matches!(
            result,
            Err(ImportError::License(LicenseError::UserCodeRequired))
        ),
        "{result:?}"
    );
    let body = h.http.body_of("activate");
    assert_eq!(body["key"], json!(KEY));
    assert_eq!(body["email"], json!("Ada@Example.com"));
    assert_eq!(body["code"], json!("123456"));
    // With no options it sends neither, as before.
    let result = license.import_license_file(&file);
    assert!(
        matches!(
            result,
            Err(ImportError::License(LicenseError::UserCodeRequired))
        ),
        "{result:?}"
    );
    let body = h.http.last_body_of("activate");
    assert!(!has(&body, "email") && !has(&body, "code"), "{body}");
    // Nothing of it is stored.
    assert!(h.storage.get().is_none());
}

#[test]
fn a_key_file_whose_key_activates_takes_the_options_sent_normalised_and_stores_neither() {
    let h = Harness::empty();
    h.http.answer(
        "activate",
        200,
        json!({ "lease": lease_to_3(&h), "activationId": "act_1" }),
    );
    let options = ActivateOptions::new()
        .email("  Mara@Studio.example ")
        .code(" 123 456 ");
    let taken = file_app(&h, &[])
        .import_license_file_with(license_file_text(&h.key.key_file(json!({}))), &options)
        .expect("taken");
    assert_eq!(taken.kind, LicenseFileKind::Key);
    let body = h.http.body_of("activate");
    assert_eq!(body["key"], json!(KEY));
    assert_eq!(body["email"], json!("Mara@Studio.example"));
    assert_eq!(body["code"], json!("123456"));
    // Activated as ever, and nothing of what was typed (nor the file's own address) is stored.
    let stored = h.state().to_json().to_string();
    assert!(stored.contains(KEY), "{stored}");
    // Whatever the case, the address as typed (its local part and its domain) and the code both as
    // normalised and as typed.
    let lowered = stored.to_lowercase();
    for typed in [
        "mara@",
        "studio.example",
        "123456",
        "123 456",
        "example.com",
    ] {
        assert!(!lowered.contains(typed), "{typed} in {stored}");
    }
}

#[test]
fn a_key_files_key_is_judged_before_the_address_and_the_address_before_the_code() {
    let h = Harness::empty();
    refused(&h, 403, "user-email-required");
    let license = file_app(&h, &[]);
    let options = ActivateOptions::new().email("x".repeat(300)).code("nope");
    let result =
        license.import_license_file_with(license_file_text(&h.key.key_file(json!({}))), &options);
    assert!(
        matches!(
            result,
            Err(ImportError::License(LicenseError::UserEmailMismatch))
        ),
        "{result:?}"
    );
    // The key a file carries, when it is none, is judged first.
    let blank = h.key.key_file(json!({ "sub": " " }));
    let result = license.import_license_file_with(license_file_text(&blank), &options);
    assert!(
        matches!(result, Err(ImportError::License(LicenseError::InvalidKey))),
        "{result:?}"
    );
    assert_eq!(h.http.count("activate"), 0);
}

#[test]
fn a_file_that_is_no_licence_file_is_refused_before_any_key_is_asked_for_whatever_comes_with_it() {
    let h = Harness::empty();
    let options = ActivateOptions::new().email("mara@studio.example");
    let result = h.license.import_license_file_with("not a file", &options);
    assert!(
        matches!(result, Err(ImportError::File(LicenseFileError::Malformed))),
        "{result:?}"
    );
    assert_eq!(h.http.count("activate"), 0);
}

#[test]
fn an_offline_file_takes_no_address_or_code_and_asks_nothing() {
    let h = Harness::empty();
    let license = file_app(&h, &[]);
    let options = ActivateOptions::new().email("x".repeat(300)).code("nope");
    license
        .import_license_file_with(h.key.offline_file(json!({})), &options)
        .expect("taken");
    assert!(h.http.calls().is_empty());
}
