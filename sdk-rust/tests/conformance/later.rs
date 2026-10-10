//! The groups after the first ones (the major a build runs as, the link
//! `upgrade_url` builds, the launch report), answered from each vector's own
//! JSON through the functions the SDK itself decides with, with the
//! conventions of the harness around this module.

use keygrant::launch::trial_report_due;
use keygrant::offline::major_of;
use keygrant::url::referenced_link;
use keygrant::LicenseConfig;
use serde_json::json;

use super::{each, group, int, label, or_null, state, text, vectors};

/// The major a build runs as. A `u32` major holds no fraction (the vectors
/// marked `jsOnly`) and no negative (which this unsigned major skips, as the
/// file says), so those are skipped, and the harness checks that it skips
/// only those; a major left unset is the configuration's default.
#[test]
fn majors() {
    let v = vectors();
    let (mut fractions, mut negatives) = (0, 0);
    for vector in group(&v, "majors") {
        let configured = &vector["configured"];
        if vector["jsOnly"] == json!(true) {
            assert!(
                configured.as_i64().is_none() && configured.is_number(),
                "majors: {} is marked jsOnly but is whole",
                label(vector)
            );
            fractions += 1;
            continue;
        }
        if configured.as_i64().is_some_and(|n| n < 0) {
            negatives += 1;
            continue;
        }
        let major = if configured.is_null() {
            LicenseConfig::new("sluice", "https://api.test", "unused").major
        } else {
            u32::try_from(int(configured)).expect("a major a u32 holds")
        };
        assert_eq!(
            json!(major_of(major)),
            vector["expect"],
            "majors: {}",
            label(vector)
        );
    }
    assert_eq!((fractions, negatives), (4, 1), "the majors skipped");
}

#[test]
fn referenced_links() {
    each(&vectors(), "referencedLinks", |vector| {
        or_null(referenced_link(
            text(&vector["link"], "link"),
            text(&vector["reference"], "reference"),
        ))
    });
}

#[test]
fn launch_reports() {
    each(&vectors(), "trialReportDue", |vector| {
        json!(trial_report_due(
            &state(&vector["state"]),
            int(&vector["now"])
        ))
    });
}
