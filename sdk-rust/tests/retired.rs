//! What an earlier version stored to buy from the app with no key typed (a
//! purchase secret and the two stamps beside it), through the whole engine
//! over the default file storage: never sent, and gone from the file once
//! the SDK saves.

mod common;

use std::path::Path;
use std::sync::Arc;

use common::*;
use keygrant::storage::FileStorage;
use keygrant::{License, LicenseConfig, LicenseState, LicenseStatus};
use serde_json::{json, Map, Value};

const TOKEN: &str = "q1w2e3r4t5y6u7i8o9p0a1s2d3f4g5h6j7k8l9z0x1c";
const RETIRED: [&str; 3] = ["purchaseToken", "purchaseTokenAt", "claimAskedAt"];

/// The three fields as an earlier version wrote them.
fn retired() -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert("purchaseToken".into(), json!(TOKEN));
    fields.insert("purchaseTokenAt".into(), json!(NOW - DAY));
    fields.insert("claimAskedAt".into(), json!(NOW - 60_000));
    fields
}

/// A licence over the default file storage at `path`, every other adapter the harness's.
fn over_file(h: &Harness, path: &Path) -> License {
    let config: LicenseConfig = h.config.clone().storage(Arc::new(FileStorage::new(path)));
    License::new(config).expect("a valid configuration")
}

fn write(path: &Path, mut state: Map<String, Value>) {
    state.extend(retired());
    std::fs::write(path, Value::Object(state).to_string()).expect("an earlier version's file");
}

fn read(path: &Path) -> Map<String, Value> {
    match serde_json::from_str(&std::fs::read_to_string(path).expect("the state")).expect("JSON") {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

fn assert_left_out(path: &Path) {
    let saved = read(path);
    for field in RETIRED {
        assert!(!saved.contains_key(field), "{field} is still in the file");
    }
}

fn assert_never_sent(h: &Harness) {
    assert!(!h.http.called("claim"));
    for (action, body) in h.http.calls() {
        assert!(
            !body.to_string().contains(TOKEN),
            "{action} sent the secret"
        );
    }
}

#[test]
fn a_running_trial_never_sends_it_and_its_first_save_leaves_it_out() {
    let h = Harness::empty();
    let dir = tempfile::tempdir().expect("a temp folder");
    let path = dir.path().join("sluice.json");
    let trial_lease = h.lease("trial:fp-test", "trial", 5 * 86_400, NOW);
    let mut state = Map::new();
    state.insert("trialStart".into(), json!(NOW - 2 * DAY));
    state.insert("trialEndsAt".into(), json!(NOW + 5 * DAY));
    state.insert("trialLease".into(), json!(trial_lease));
    state.insert("lastSeen".into(), json!(NOW));
    write(&path, state);
    h.http.answer(
        "trial",
        200,
        json!({ "trialStart": NOW - 2 * DAY, "trialEndsAt": NOW + 5 * DAY, "lease": trial_lease }),
    );
    let license = over_file(&h, &path);
    assert_eq!(license.status(false).state, LicenseState::Trial);
    license.launch_report_settled();
    // The launch report's stamp is the first save, and it carries none of them.
    assert_eq!(read(&path).get("trialReportedAt"), Some(&json!(NOW)));
    assert_left_out(&path);
    assert_eq!(license.refresh().state, LicenseState::Trial);
    assert_eq!(license.start_trial().state, LicenseState::Trial);
    assert_left_out(&path);
    assert_never_sent(&h);
}

#[test]
fn an_activation_leaves_it_out_and_a_later_deactivation_activates_nothing() {
    let h = Harness::empty();
    let dir = tempfile::tempdir().expect("a temp folder");
    let path = dir.path().join("sluice.json");
    let mut state = Map::new();
    state.insert("lastSeen".into(), json!(NOW));
    write(&path, state);
    let lease = h.lease("KEY-1", "perpetual", 30 * 86_400, NOW);
    h.http.answer(
        "activate",
        200,
        json!({ "lease": lease, "activationId": "act_1" }),
    );
    h.http.answer("deactivate", 200, json!({}));
    let license = over_file(&h, &path);
    license.activate("KEY-1").expect("activated");
    assert_left_out(&path);
    assert!(license.deactivate().expect("deactivated").released);
    assert_left_out(&path);
    assert_eq!(
        license.status(false),
        LicenseStatus::new(LicenseState::Unlicensed)
    );
    assert_never_sent(&h);
}

#[test]
fn a_deactivation_alone_leaves_it_out() {
    let h = Harness::empty();
    let dir = tempfile::tempdir().expect("a temp folder");
    let path = dir.path().join("sluice.json");
    let lease = h.lease("KEY-1", "perpetual", 30 * 86_400, NOW);
    let mut state = Map::new();
    state.insert("key".into(), json!("KEY-1"));
    state.insert("activationId".into(), json!("act_1"));
    state.insert("lease".into(), json!(lease));
    state.insert("checkedMajor".into(), json!(1));
    state.insert("lastSeen".into(), json!(NOW));
    write(&path, state);
    h.http.answer("deactivate", 200, json!({}));
    let license = over_file(&h, &path);
    assert_eq!(license.status(false).state, LicenseState::Licensed);
    assert!(license.deactivate().expect("deactivated").released);
    assert_left_out(&path);
    assert_never_sent(&h);
}
