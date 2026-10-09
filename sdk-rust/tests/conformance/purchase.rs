//! The purchase groups, answered from each vector's own JSON through the
//! functions the SDK itself decides with, with the conventions of the
//! harness around this module.

use keygrant::device::device_from;
use keygrant::launch::trial_report_due;
use keygrant::lease::fingerprint_claim;
use keygrant::offline::major_of;
use keygrant::pickup::{
    claim_due, claim_outcome, claim_request, purchase_link, purchase_reference,
    purchase_token_kept, ClaimAsk, ClaimInput,
};
use keygrant::wire::parse_claim_body;
use keygrant::LicenseConfig;
use serde_json::{json, Map, Value};

use super::{
    each, each_status, encoded, group, identity, int, kind, label, lease_check, or_null, response,
    state, status_json, text, vectors,
};

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
fn purchase_references_and_links() {
    let v = vectors();
    each(&v, "purchaseReferences", |vector| {
        json!(purchase_reference(text(&vector["secret"], "secret")))
    });
    each(&v, "purchaseUrls", |vector| {
        or_null(purchase_link(
            text(&vector["link"], "link"),
            text(&vector["secret"], "secret"),
        ))
    });
}

#[test]
fn the_token_and_its_waits() {
    let v = vectors();
    each(&v, "purchaseTokenKept", |vector| {
        json!(purchase_token_kept(
            &state(&vector["state"]),
            int(&vector["now"])
        ))
    });
    each(&v, "claimDue", |vector| {
        json!(claim_due(
            &state(&vector["state"]),
            int(&vector["clockNow"]),
            int(&vector["now"]),
        ))
    });
    each(&v, "trialReportDue", |vector| {
        json!(trial_report_due(
            &state(&vector["state"]),
            int(&vector["now"])
        ))
    });
}

#[test]
fn parse_claim_bodies() {
    each(&vectors(), "parseClaimBody", |vector| {
        let body = parse_claim_body(Some(&vector["body"]));
        let mut out = Map::new();
        for (name, value) in [
            ("key", body.key),
            ("activationId", body.activation_id),
            ("lease", body.lease),
        ] {
            if let Some(value) = value {
                out.insert(name.into(), json!(value));
            }
        }
        if body.pending {
            out.insert("pending".into(), json!(true));
        }
        if let Some(error) = body.error {
            out.insert("error".into(), json!(error));
        }
        Value::Object(out)
    });
}

#[test]
fn claim_outcomes() {
    // A bought key's status carries the entitlements of the lease's claims.
    each_status(&vectors(), "claimOutcomes", |vector| {
        let response = response(&vector["response"]);
        let check = lease_check(&vector["check"]);
        let outcome = claim_outcome(&ClaimInput {
            response: response.as_ref(),
            check: check.as_ref(),
            clock_now: int(&vector["clockNow"]),
            now: int(&vector["now"]),
            major: int(&vector["major"]),
            kind: kind(&vector["kind"]),
        });
        json!({
            "result": outcome.result.as_str(),
            "status": or_null(outcome.status.as_ref().map(status_json)),
            "patch": encoded(&outcome.patch),
        })
    });
}

/// The body a claim sends: the device read as the `devices` group reads one,
/// with no install kind (no key is held), the name already cut and the major
/// already read.
#[test]
fn claim_bodies() {
    each(&vectors(), "claimBodies", |vector| {
        let device = device_from(
            &identity(&vector["identity"]),
            None,
            vector["deviceHold"].as_bool().expect("deviceHold"),
            fingerprint_claim,
        );
        let body = claim_request(&ClaimAsk {
            secret: text(&vector["secret"], "secret"),
            device: &device,
            name: vector["deviceName"].as_str(),
            major: int(&vector["major"]),
            start: vector["start"].as_bool().expect("start"),
        });
        or_null(body.map(|body| body.to_json()))
    });
}
