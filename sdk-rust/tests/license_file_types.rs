//! The licence-file types an app handles: their words, their JSON, the
//! errors' messages and sources, the format's constants, and the stored
//! field's name in the state file every SDK shares.

mod common;

use std::error::Error as _;
use std::io;

use common::*;
use keygrant::{
    ImportError, ImportedLicenseFile, LicenseError, LicenseFileError, LicenseFileKind, LicensedTo,
    StoredState, LICENSE_FILE_EXTENSION, LICENSE_FILE_MEDIA_TYPE,
};
use serde_json::json;

const FILE_ERRORS: [(LicenseFileError, &str); 8] = [
    (LicenseFileError::Malformed, "malformed"),
    (LicenseFileError::NotALicenseFile, "not-a-license-file"),
    (LicenseFileError::BadSignature, "bad-signature"),
    (LicenseFileError::UnknownKey, "unknown-key"),
    (LicenseFileError::WrongProduct, "wrong-product"),
    (LicenseFileError::RevokedFile, "revoked-file"),
    (LicenseFileError::Upgrade, "upgrade"),
    (LicenseFileError::Outdated, "outdated"),
];

#[test]
fn every_file_error_has_the_references_word_as_its_word_its_json_and_its_message() {
    for (error, word) in FILE_ERRORS {
        assert_eq!(error.as_str(), word);
        assert_eq!(error.to_string(), word);
        assert_eq!(serde_json::to_value(error).expect("JSON"), json!(word));
        assert_eq!(
            serde_json::from_value::<LicenseFileError>(json!(word)).expect("read back"),
            error
        );
    }
}

#[test]
fn an_import_error_is_its_word_in_code_and_json_with_a_message_and_its_cause() {
    let disk = || io::Error::new(io::ErrorKind::ResourceBusy, "EBUSY");
    let cases = [
        (
            ImportError::File(LicenseFileError::RevokedFile),
            "revoked-file",
            true,
        ),
        (ImportError::License(LicenseError::Limit), "limit", true),
        (ImportError::Storage(disk()), "storage", true),
        (ImportError::Busy, "busy", false),
    ];
    for (error, word, has_cause) in cases {
        assert_eq!(error.code(), word);
        assert_eq!(serde_json::to_value(&error).expect("JSON"), json!(word));
        assert_eq!(error.source().is_some(), has_cause, "{word}");
    }
    assert_eq!(
        ImportError::Busy.to_string(),
        "a previous licence call has not finished; try again"
    );
    let cause = ImportError::File(LicenseFileError::UnknownKey);
    assert_eq!(
        cause.source().map(ToString::to_string).as_deref(),
        Some("unknown-key")
    );
}

#[test]
fn what_an_import_took_is_its_kind_and_licensee_in_json_and_reads_back() {
    let cases = [
        (
            ImportedLicenseFile {
                kind: LicenseFileKind::Key,
                licensed_to: LicensedTo {
                    name: None,
                    email: Some(ADA_EMAIL.into()),
                },
            },
            json!({ "kind": "key", "licensedTo": { "email": ADA_EMAIL } }),
        ),
        (
            ImportedLicenseFile {
                kind: LicenseFileKind::Offline,
                licensed_to: ada(),
            },
            json!({ "kind": "offline", "licensedTo": { "name": ADA_NAME, "email": ADA_EMAIL } }),
        ),
    ];
    for (taken, expected) in cases {
        assert_eq!(serde_json::to_value(&taken).expect("JSON"), expected);
        assert_eq!(
            serde_json::from_value::<ImportedLicenseFile>(expected).expect("read back"),
            taken
        );
    }
}

#[test]
fn the_extension_and_media_type_are_the_contracts() {
    assert_eq!(LICENSE_FILE_EXTENSION, ".kglicense");
    assert_eq!(LICENSE_FILE_MEDIA_TYPE, "application/vnd.keygrant.license");
}

#[test]
fn the_stored_file_is_licensefile_in_the_state_file_read_and_written_by_that_name() {
    let token = TestKey::new(7).offline_file(json!({}));
    let read =
        StoredState::from_json(&json!({ "licenseFile": token, "key": KEY })).expect("a state");
    assert_eq!(read.license_file.as_deref(), Some(token.as_str()));
    assert!(read.extra.is_empty());
    assert_eq!(read.to_json(), json!({ "key": KEY, "licenseFile": token }));
}
