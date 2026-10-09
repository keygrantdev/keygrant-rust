//! `conformance/vectors.json` (generated from the Electron SDK, the reference)
//! holds what every KeyGrant SDK must answer for the same inputs. Each group
//! here is answered from the vector's own JSON, and every answer must match.
//! The answers come from the functions the SDK itself decides with (the
//! renewal, the device, the identity, the clock guard), not from copies of
//! them.
//!
//! The file's conventions: times are milliseconds (lease claims seconds); a
//! state has absent fields unset; in a patch, `null` clears a field and an
//! absent field is left alone; an answer of nothing is `null`; a status
//! carries `entitlements` only when there are any, and numbers in them are
//! doubles, compared as numbers (`5` and `5.0` alike).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use keygrant::device::device_from;
use keygrant::launch::TRIAL_REPORT_MS;
use keygrant::lease::{
    entitlements_of, fingerprint_claim, verify_lease, LeaseClaims, LicenseModel, PublicKey,
    VerifyResult, OPEN_MAJOR,
};
use keygrant::machine_id::{
    machine_id_filesystem, read_machine_id, MachineIdReading, MachineIdSource, ReadFailure,
    MACHINE_ID_TIMEOUT,
};
use keygrant::offline::{
    self, ask_due, check_lease, clock_correction, guarded_now, kept_with, keyed_by, lease_status,
    leased, merged, refusal_patch, renews_early, saved_last_seen, standing_refusal, trial_lease_of,
    trial_lease_valid, trial_status_of, unkeyed, About, Asked, Basis, DeviceView, LeaseCheck,
    LeaseFailure, LeaseHolder, SignedAt, Stamp,
};
use keygrant::pickup::{
    no_purchase, PurchaseOf, CLAIM_EAGER_MS, CLAIM_WAIT_MS, MAX_CLAIM_FINGERPRINTS,
    MAX_CLAIM_FINGERPRINT_LENGTH, PURCHASE_REFERENCE_PREFIX, PURCHASE_TOKEN_MS,
};
use keygrant::renewal::{asks_on_valid_lease, renewal_outcome, RenewalInput, ValidLeaseAsk};
use keygrant::storage::{KEEP_CORRUPT, STALE_TEMP_MS, UPDATE_ATTEMPTS};
use keygrant::wire::{
    self, entered_key, map_server_error, no_answer, parse_lease_body, parse_trial_body, refusal_of,
    trim_to_length,
};
use keygrant::{
    hash_fingerprint, host_fingerprint, identity_of, machine_fingerprint, DeviceIdentity,
    DeviceKind, Entitlement, HostNames, HttpResponse, LicenseStatus, Patch, Refusal, StoredState,
    UNKNOWN_HOST,
};
use serde_json::{json, Map, Value};

// --- the file ------------------------------------------------------------------

/// The vectors' text: the repo's, or others named by KEYGRANT_VECTORS (a set
/// just regenerated from the reference, say).
fn vectors_text() -> String {
    let path = std::env::var("KEYGRANT_VECTORS").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../conformance/vectors.json").to_owned()
    });
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("the vectors at {path}: {e}"))
}

fn vectors() -> Value {
    serde_json::from_str(&vectors_text()).expect("the vectors are JSON")
}

/// Every number in a JSON text, as written: each token outside a string that
/// starts with a digit or a minus.
fn number_tokens(json: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut chars = json.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        match c {
            '"' => {
                // Past the string, escapes included.
                while let Some((_, c)) = chars.next() {
                    match c {
                        '\\' => {
                            chars.next();
                        }
                        '"' => break,
                        _ => {}
                    }
                }
            }
            '-' | '0'..='9' => {
                let mut end = start + c.len_utf8();
                while let Some(&(at, c)) = chars.peek() {
                    if !matches!(c, '0'..='9' | '+' | '-' | '.' | 'e' | 'E') {
                        break;
                    }
                    end = at + c.len_utf8();
                    chars.next();
                }
                tokens.push(&json[start..end]);
            }
            _ => {}
        }
    }
    tokens
}

#[test]
fn every_number_in_the_file_is_read_as_the_nearest_double() {
    // The harness compares doubles exactly, so it must read each number as the
    // reference does (correctly rounded); a reader that rounds otherwise would
    // misread the vectors and the leases alike, and agree with itself. Rust's
    // `str::parse` is correctly rounded: serde_json (with `float_roundtrip`, as
    // the crate reads leases) must say the same of every number in the file,
    // the entitlements past 2^53 and in exponent form included.
    let text = vectors_text();
    let tokens = number_tokens(&text);
    assert!(tokens.len() > 1000, "only {} numbers", tokens.len());
    for token in tokens {
        let exact: f64 = token
            .parse()
            .unwrap_or_else(|e| panic!("{token}: not a number ({e})"));
        let read: f64 = serde_json::from_str(token).expect("a JSON number");
        assert_eq!(read.to_bits(), exact.to_bits(), "{token} read as {read}");
    }
}

/// The purchase groups, after every other one, answered in
/// `conformance/purchase.rs`.
#[path = "conformance/purchase.rs"]
mod purchase;

/// Every group this harness answers, in the file's order.
const GROUPS: [&str; 43] = [
    "leases",
    "trials",
    "fingerprintClaims",
    "fingerprints",
    "machineFingerprints",
    "hostFingerprints",
    "kept",
    "renew",
    "asks",
    "standing",
    "trialStatuses",
    "refusalPatches",
    "leasePatches",
    "keyedBy",
    "merges",
    "guardedNow",
    "clockCorrections",
    "mapServerError",
    "refusalOf",
    "enteredKey",
    "trimDeviceName",
    "noAnswer",
    "parseLeaseBody",
    "parseTrialBody",
    "mountinfo",
    "machineIds",
    "renewals",
    "validLeaseAsks",
    "devices",
    "identities",
    "savedLastSeen",
    "leaseStatuses",
    "trialLeaseStatuses",
    "entitlementNumbers",
    "majors",
    "purchaseUrls",
    "purchaseTokenKept",
    "claimDue",
    "parseClaimBody",
    "claimOutcomes",
    "trialReportDue",
    "purchaseReferences",
    "claimBodies",
];

fn group<'a>(vectors: &'a Value, name: &str) -> &'a Vec<Value> {
    let items = vectors[name]
        .as_array()
        .unwrap_or_else(|| panic!("a `{name}` group"));
    assert!(!items.is_empty(), "the `{name}` group is empty");
    items
}

/// A vector's name, for messages (the input itself when it has none).
fn label(vector: &Value) -> String {
    vector["name"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| vector.to_string())
}

fn int(value: &Value) -> i64 {
    value
        .as_i64()
        .unwrap_or_else(|| panic!("an integer, not {value}"))
}

/// An optional input: `null` (or absent) is none.
fn opt_int(value: &Value) -> Option<i64> {
    value.as_i64()
}

fn text<'a>(value: &'a Value, what: &str) -> &'a str {
    value
        .as_str()
        .unwrap_or_else(|| panic!("{what}: a string, not {value}"))
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn state(value: &Value) -> StoredState {
    StoredState::from_json(value).unwrap_or_default()
}

/// An answer of nothing is `null`.
fn or_null<T: Into<Value>>(value: Option<T>) -> Value {
    value.map_or(Value::Null, Into::into)
}

fn refusal(word: &str) -> Refusal {
    match word {
        "revoked" => Refusal::Revoked,
        "upgrade" => Refusal::Upgrade,
        "outdated" => Refusal::Outdated,
        "device" => Refusal::Device,
        other => panic!("unknown refusal {other:?}"),
    }
}

fn kind(value: &Value) -> Option<DeviceKind> {
    match value.as_str() {
        Some("machine") => Some(DeviceKind::Machine),
        Some("hostname") => Some(DeviceKind::Hostname),
        None => None,
        Some(other) => panic!("unknown kind {other:?}"),
    }
}

/// Answer every vector of `name` with `answer`, and compare it with `expect`.
fn each(vectors: &Value, name: &str, answer: impl Fn(&Value) -> Value) {
    for vector in group(vectors, name) {
        assert_eq!(
            answer(vector),
            vector["expect"],
            "{name}: {}",
            label(vector)
        );
    }
}

/// `value` with every number in it a double: how the file says to compare
/// the numbers of an entitlement (`5` and `5.0` are the same one), which
/// JSON writes either way. Applied to both sides of a comparison, so only
/// the numbers' values count.
fn doubles(value: &Value) -> Value {
    match value {
        Value::Number(number) => number
            .as_f64()
            .and_then(serde_json::Number::from_f64)
            .map_or(Value::Null, Value::Number),
        Value::Array(items) => Value::Array(items.iter().map(doubles).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(name, item)| (name.clone(), doubles(item)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// [`each`], for a group whose answer holds a status: its numbers compared as numbers.
fn each_status(vectors: &Value, name: &str, answer: impl Fn(&Value) -> Value) {
    for vector in group(vectors, name) {
        assert_eq!(
            doubles(&answer(vector)),
            doubles(&vector["expect"]),
            "{name}: {}",
            label(vector)
        );
    }
}

#[test]
fn the_harness_answers_every_group_in_the_file() {
    let v = vectors();
    let in_file: BTreeSet<&str> = v
        .as_object()
        .expect("an object")
        .iter()
        .filter(|(_, item)| item.is_array())
        .map(|(k, _)| k.as_str())
        .collect();
    let answered: BTreeSet<&str> = GROUPS.iter().copied().chain(["conventions"]).collect();
    assert_eq!(in_file, answered);
}

#[test]
fn constants_are_the_references() {
    let v = vectors();
    let c = &v["constants"];
    let ms = |d: Duration| i64::try_from(d.as_millis()).unwrap_or(i64::MAX);
    let expected = json!({
        "OPEN_MAJOR": OPEN_MAJOR,
        "ASK_AGAIN_MS": offline::ASK_AGAIN_MS,
        "RETRY_UNANSWERED_MS": offline::RETRY_UNANSWERED_MS,
        "RENEW_WITHIN_MS": offline::RENEW_WITHIN_MS,
        "CLOCK_TOLERANCE_MS": offline::CLOCK_TOLERANCE_MS,
        "STATUS_CHECK_TIMEOUT_MS": ms(wire::STATUS_CHECK_TIMEOUT),
        "DEFAULT_HTTP_TIMEOUT_MS": ms(keygrant::DEFAULT_HTTP_TIMEOUT),
        "LOCK_GRACE_MS": ms(keygrant::LOCK_GRACE),
        "MACHINE_ID_TIMEOUT_MS": ms(MACHINE_ID_TIMEOUT),
        "ACTIVATE_MACHINE_ID_TIMEOUT_MS": ms(keygrant::machine_id::ACTIVATE_MACHINE_ID_TIMEOUT),
        "BOOT_SLACK_MS": keygrant::machine_id::BOOT_SLACK_MS as i64,
        "MAX_DEVICE_NAME": wire::MAX_DEVICE_NAME,
        "MAX_KEY": wire::MAX_KEY,
        "KEEP_CORRUPT": KEEP_CORRUPT,
        "STALE_TEMP_MS": STALE_TEMP_MS as i64,
        "UPDATE_ATTEMPTS": UPDATE_ATTEMPTS,
        "UNKNOWN_HOST": UNKNOWN_HOST,
        "CLAIM_WAIT_MS": CLAIM_WAIT_MS,
        "PURCHASE_TOKEN_MS": PURCHASE_TOKEN_MS,
        "TRIAL_REPORT_MS": TRIAL_REPORT_MS,
        "PURCHASE_REFERENCE_PREFIX": PURCHASE_REFERENCE_PREFIX,
        "CLAIM_EAGER_MS": CLAIM_EAGER_MS,
        "MAX_CLAIM_FINGERPRINTS": MAX_CLAIM_FINGERPRINTS,
        "MAX_CLAIM_FINGERPRINT_LENGTH": MAX_CLAIM_FINGERPRINT_LENGTH,
    });
    assert_eq!(*c, expected);
}

// --- leases and trials --------------------------------------------------------------

fn key(vectors: &Value) -> PublicKey {
    PublicKey::from_jwk(&vectors["publicJwk"]).expect("the vectors' public key")
}

#[test]
fn leases_verify_and_check() {
    let v = vectors();
    let key = key(&v);
    let mut verdicts = BTreeSet::new();
    for vector in group(&v, "leases") {
        let name = label(vector);
        let verified = verify_lease(text(&vector["token"], "token"), &key, int(&vector["now"]));
        // What verifying says, the claims as parsed included (`ent` read
        // leniently, its numbers compared as numbers).
        assert_eq!(
            doubles(&verified.to_json()),
            doubles(&vector["verify"]),
            "{name}: verify"
        );
        let answer = match checked(vector, &verified) {
            LeaseCheck::Ok { test, .. } => {
                verdicts.insert("ok");
                json!({ "ok": true, "test": test })
            }
            LeaseCheck::Fail(failure) => {
                verdicts.insert(failure.as_str());
                json!({ "ok": false, "reason": failure.as_str() })
            }
        };
        assert_eq!(answer, vector["expect"], "{name}: expect");
    }
    // Every kind of verdict the offline check can give is covered.
    let all = ["device", "expired", "ok", "outdated", "tampered", "upgrade"];
    assert_eq!(verdicts.into_iter().collect::<Vec<_>>(), all);
}

/// A `leases`-shaped vector's lease, verified, judged as `check_lease` judges
/// it: for its product, its build's major and its holder.
fn checked(vector: &Value, verified: &VerifyResult) -> LeaseCheck {
    let holder = &vector["holder"];
    let claims = strings(&holder["claims"]);
    let bound = LeaseHolder {
        key: text(&holder["key"], "holder.key"),
        activation_id: text(&holder["activationId"], "holder.activationId"),
        claims: &claims,
        can_tell: holder["canTell"].as_bool().expect("holder.canTell"),
    };
    check_lease(
        verified,
        text(&vector["product"], "product"),
        int(&vector["major"]),
        &bound,
    )
}

/// What a status serialises as, which the vectors hold.
fn status_json(status: &LicenseStatus) -> Value {
    serde_json::to_value(status).expect("serialisable")
}

#[test]
fn lease_statuses() {
    // The status a stored licence lease gives with no verdict from the
    // server (`lease_status` of `check_lease`), entitlements included.
    let v = vectors();
    let key = key(&v);
    each_status(&v, "leaseStatuses", |vector| {
        let verified = verify_lease(text(&vector["token"], "token"), &key, int(&vector["now"]));
        let holder_key = text(&vector["holder"]["key"], "holder.key");
        status_json(&lease_status(&checked(vector, &verified), Some(holder_key)))
    });
}

#[test]
fn trial_lease_statuses() {
    // A trial's status from its start and its trial lease (`trial_status_of`
    // of `trial_lease_of`): the stored state is the two, so its end and its
    // entitlements are the lease's.
    let v = vectors();
    let key = key(&v);
    each_status(&v, "trialLeaseStatuses", |vector| {
        let token = text(&vector["token"], "token");
        let now = int(&vector["now"]);
        let device = &vector["device"];
        let lease = trial_lease_of(
            &verify_lease(token, &key, now),
            text(&vector["product"], "product"),
            &strings(&device["claims"]),
            device["canTell"].as_bool().expect("device.canTell"),
        );
        let stored = StoredState {
            trial_start: Some(int(&vector["trialStart"])),
            trial_lease: Some(token.to_owned()),
            ..StoredState::default()
        };
        status_json(&trial_status_of(
            &stored,
            now,
            lease.valid,
            lease.ends_at,
            &lease.entitlements,
        ))
    });
}

#[test]
fn entitlement_numbers() {
    // The bits of the double an `ent` member reads from a number as the
    // server writes it: what no JSON reader can round, so a reader that
    // rounds wrongly fails here though it reads the file and a lease alike.
    // `{"n": <json>}` is read as a lease payload is (`serde_json::from_str`
    // into a `Value`, as `verify_lease` reads one), then as the engine reads
    // `ent` (`entitlements_of`).
    each(&vectors(), "entitlementNumbers", |vector| {
        let json = text(&vector["json"], "json");
        let object: Value =
            serde_json::from_str(&format!(r#"{{"n":{json}}}"#)).expect("a JSON object");
        let read = entitlements_of(&object)
            .get("n")
            .and_then(Entitlement::as_f64)
            .unwrap_or_else(|| panic!("{json} is not a number"));
        json!(format!("{:016x}", read.to_bits()))
    });
}

#[test]
fn trial_leases() {
    let v = vectors();
    let key = key(&v);
    each(&v, "trials", |vector| {
        let verified = verify_lease(text(&vector["token"], "token"), &key, int(&vector["now"]));
        let device = &vector["device"];
        json!(trial_lease_valid(
            &verified,
            text(&vector["product"], "product"),
            &strings(&device["claims"]),
            device["canTell"].as_bool().expect("device.canTell"),
        ))
    });
}

// --- fingerprints ---------------------------------------------------------------------

#[test]
fn fingerprints() {
    let v = vectors();
    for vector in group(&v, "fingerprintClaims") {
        let input = text(&vector["input"], "input");
        assert_eq!(
            json!(fingerprint_claim(input)),
            vector["claim"],
            "fingerprintClaims: {input:?}"
        );
    }
    for vector in group(&v, "fingerprints") {
        let raw = text(&vector["raw"], "raw");
        assert_eq!(
            json!(hash_fingerprint(raw)),
            vector["fingerprint"],
            "fingerprints: {raw:?}"
        );
    }
    for vector in group(&v, "machineFingerprints") {
        let id = text(&vector["id"], "id");
        assert_eq!(
            json!(machine_fingerprint(id)),
            vector["fingerprint"],
            "machineFingerprints: {id:?}"
        );
    }
    for vector in group(&v, "hostFingerprints") {
        let answer = host_fingerprint(
            text(&vector["host"], "host"),
            text(&vector["platform"], "platform"),
            text(&vector["arch"], "arch"),
        );
        assert_eq!(
            json!(answer),
            vector["fingerprint"],
            "hostFingerprints: {vector}"
        );
    }
}

// --- the pure decisions ------------------------------------------------------------------

/// A valid licence lease's claims (the vectors' base claims): the decisions
/// read one or two of them; the rest only make it a whole claim set.
fn base_claims() -> LeaseClaims {
    LeaseClaims {
        iss: "keygrant".into(),
        sub: "SLUICE-AAAA-BBBB-CCCC-DDDD".into(),
        product: "sluice".into(),
        aid: "act_1".into(),
        fp: None,
        fpk: None,
        model: LicenseModel::Perpetual,
        lim: 3,
        major: 3,
        min_major: None,
        upgrade_until: None,
        ent: BTreeMap::new(),
        iat: 1_789_913_600,
        exp: 1_792_505_600,
        grace: None,
        test: None,
    }
}

#[test]
fn kept_majors() {
    each(&vectors(), "kept", |vector| {
        or_null(kept_with(
            opt_int(&vector["kept"]),
            opt_int(&vector["upgradeUntil"]),
            int(&vector["now"]),
            int(&vector["major"]),
        ))
    });
}

#[test]
fn early_renewals() {
    each(&vectors(), "renew", |vector| {
        let claims = LeaseClaims {
            iat: int(&vector["iat"]),
            exp: int(&vector["exp"]),
            ..base_claims()
        };
        json!(renews_early(&claims, int(&vector["now"])))
    });
}

#[test]
fn asks() {
    each(&vectors(), "asks", |vector| {
        json!(ask_due(
            &state(&vector["state"]),
            int(&vector["clockNow"]),
            vector["force"].as_bool().expect("force")
        ))
    });
}

#[test]
fn standing_refusals() {
    each(&vectors(), "standing", |vector| {
        let device = &vector["device"];
        let view = device.is_object().then(|| DeviceView {
            own_claim: device["ownClaim"].as_str(),
            can_tell: device["canTell"].as_bool().unwrap_or(true),
        });
        or_null(
            standing_refusal(&state(&vector["state"]), int(&vector["major"]), view)
                .map(Refusal::as_str),
        )
    });
}

#[test]
fn trial_statuses() {
    each(&vectors(), "trialStatuses", |vector| {
        // These vectors name no lease's entitlements: none.
        let status = trial_status_of(
            &state(&vector["state"]),
            int(&vector["now"]),
            vector["leaseValid"].as_bool().expect("leaseValid"),
            opt_int(&vector["leaseEndsAt"]),
            &BTreeMap::new(),
        );
        status_json(&status)
    });
}

fn encoded(patch: &Patch) -> Value {
    Value::Object(patch.encoded())
}

#[test]
fn patches() {
    let v = vectors();
    each(&v, "refusalPatches", |vector| {
        let own_claim = vector["device"]["ownClaim"].as_str();
        encoded(&refusal_patch(
            refusal(text(&vector["refused"], "refused")),
            int(&vector["major"]),
            own_claim,
        ))
    });
    each(&v, "leasePatches", |vector| {
        match text(&vector["patch"], "patch") {
            "LEASED" => encoded(&leased()),
            "UNKEYED" => encoded(&unkeyed()),
            "NO_PURCHASE" => encoded(&no_purchase()),
            other => panic!("unknown patch {other:?}"),
        }
    });
    each(&v, "keyedBy", |vector| {
        encoded(&keyed_by(kind(&vector["kind"])))
    });
}

fn asked(value: &Value) -> Asked {
    Asked {
        at: int(&value["at"]),
        activation_id: value["activationId"].as_str().map(str::to_owned),
        seen: opt_int(&value["seen"]),
    }
}

/// A basis from JSON: `null` is none.
fn basis(value: &Value) -> Basis {
    let part = |name: &str| value.get(name).filter(|part| part.is_object());
    Basis {
        answer: part("answer").map(asked),
        activation: part("activation").map(asked),
        about: part("about").map(|about| About {
            activation_id: about["activationId"].as_str().map(str::to_owned),
        }),
        trial: part("trial").map(|trial| Stamp {
            at: int(&trial["at"]),
            seen: opt_int(&trial["seen"]),
        }),
        purchase: part("purchase").map(|purchase| PurchaseOf {
            token: text(&purchase["token"], "basis.purchase.token").to_owned(),
            at: opt_int(&purchase["at"]),
        }),
    }
}

#[test]
fn merges() {
    each(&vectors(), "merges", |vector| {
        let patch = Patch::from_json(vector["patch"].as_object().expect("a patch"));
        merged(&state(&vector["latest"]), &patch, &basis(&vector["basis"])).to_json()
    });
}

#[test]
fn the_clock_guard() {
    let v = vectors();
    each(&v, "guardedNow", |vector| {
        json!(guarded_now(
            int(&vector["clockNow"]),
            opt_int(&vector["lastSeen"])
        ))
    });
    each(&v, "clockCorrections", |vector| {
        let claims = &vector["claims"];
        let signed = claims.is_object().then(|| SignedAt {
            sub: text(&claims["sub"], "claims.sub"),
            aid: text(&claims["aid"], "claims.aid"),
            iat: int(&claims["iat"]),
        });
        or_null(clock_correction(
            int(&vector["clockNow"]),
            opt_int(&vector["lastSeen"]),
            signed,
            opt_int(&vector["heldIat"]),
            text(&vector["key"], "key"),
            text(&vector["activationId"], "activationId"),
        ))
    });
    // What a save writes as the last time seen (a guard of 0 is a guard).
    each(&v, "savedLastSeen", |vector| {
        json!(saved_last_seen(
            opt_int(&vector["stored"]),
            int(&vector["clockNow"]),
            opt_int(&vector["guard"]),
        ))
    });
}

// --- the renewal ------------------------------------------------------------------------------

/// A vector's answer to the ask: `null` when there was none at all (the
/// request threw or timed out); a `null` body is no body.
fn response(value: &Value) -> Option<HttpResponse> {
    value.is_object().then(|| HttpResponse {
        status: u16::try_from(int(&value["status"])).expect("an HTTP status"),
        json: Some(value["json"].clone()).filter(|json| !json.is_null()),
    })
}

fn lease_failure(word: &str) -> LeaseFailure {
    match word {
        "expired" => LeaseFailure::Expired,
        "upgrade" => LeaseFailure::Upgrade,
        "outdated" => LeaseFailure::Outdated,
        "tampered" => LeaseFailure::Tampered,
        "device" => LeaseFailure::Device,
        other => panic!("unknown lease failure {other:?}"),
    }
}

/// A vector's offline check of the lease a 200 carried (`null`: none). The
/// lease itself is not verified: it stands for one (`header.payload.signature`).
fn lease_check(value: &Value) -> Option<LeaseCheck> {
    if value.is_null() {
        return None;
    }
    let check = if value["ok"].as_bool().expect("check.ok") {
        LeaseCheck::Ok {
            test: value["test"].as_bool().expect("check.test"),
            claims: Box::new(
                LeaseClaims::from_json(&value["claims"]).expect("check.claims: a lease's claims"),
            ),
        }
    } else {
        LeaseCheck::Fail(lease_failure(text(&value["reason"], "check.reason")))
    };
    Some(check)
}

#[test]
fn renewals() {
    // A licensed status carries the entitlements of the lease's claims.
    each_status(&vectors(), "renewals", |vector| {
        let response = response(&vector["response"]);
        let check = lease_check(&vector["check"]);
        let device = &vector["device"];
        let outcome = renewal_outcome(&RenewalInput {
            response: response.as_ref(),
            check: check.as_ref(),
            asked_at: int(&vector["askedAt"]),
            now: int(&vector["now"]),
            major: int(&vector["major"]),
            key: vector["key"].as_str(),
            kept_major: opt_int(&vector["keptMajor"]),
            device_kind: kind(&device["kind"]),
            own_claim: device["ownClaim"].as_str(),
        });
        let status = outcome.status.as_ref().map(status_json);
        json!({
            "answered": outcome.answered,
            "status": or_null(status),
            "patch": encoded(&outcome.patch),
        })
    });
}

#[test]
fn valid_lease_asks() {
    each(&vectors(), "validLeaseAsks", |vector| {
        let given = &vector["claims"];
        let claims = LeaseClaims {
            major: int(&given["major"]),
            iat: int(&given["iat"]),
            exp: int(&given["exp"]),
            ..base_claims()
        };
        json!(asks_on_valid_lease(&ValidLeaseAsk {
            claims: &claims,
            state: &state(&vector["state"]),
            major: int(&vector["major"]),
            now: int(&vector["now"]),
            clock_now: int(&vector["clockNow"]),
        }))
    });
}

// --- the wire ------------------------------------------------------------------------------

#[test]
fn the_wire() {
    let v = vectors();
    // Only a string error word can match: `null` and `42` are not one.
    each(&v, "mapServerError", |vector| {
        json!(map_server_error(vector["error"].as_str()).as_str())
    });
    each(&v, "refusalOf", |vector| {
        or_null(refusal_of(vector["error"].as_str()).map(Refusal::as_str))
    });
    each(&v, "enteredKey", |vector| {
        or_null(entered_key(text(&vector["input"], "input")))
    });
    each(&v, "trimDeviceName", |vector| {
        let max = usize::try_from(int(&vector["max"])).expect("a length");
        json!(trim_to_length(text(&vector["input"], "input"), max))
    });
    each(&v, "noAnswer", |vector| {
        json!(no_answer(
            u16::try_from(int(&vector["status"])).expect("an HTTP status")
        ))
    });
    each(&v, "parseLeaseBody", |vector| {
        let (lease, activation_id) = parse_lease_body(Some(&vector["body"]));
        let mut out = Map::new();
        if let Some(lease) = lease {
            out.insert("lease".into(), json!(lease));
        }
        if let Some(activation_id) = activation_id {
            out.insert("activationId".into(), json!(activation_id));
        }
        Value::Object(out)
    });
    each(&v, "parseTrialBody", |vector| {
        match parse_trial_body(Some(&vector["body"])) {
            None => Value::Null,
            Some(body) => {
                let mut out =
                    json!({ "trialStart": body.trial_start, "trialEndsAt": body.trial_ends_at });
                if let Some(lease) = body.lease {
                    out["lease"] = json!(lease);
                }
                out
            }
        }
    });
}

// --- the machine id ----------------------------------------------------------------------------

#[test]
fn the_filesystem_holding_the_machine_id() {
    each(&vectors(), "mountinfo", |vector| {
        or_null(machine_id_filesystem(text(
            &vector["mountinfo"],
            "mountinfo",
        )))
    });
}

/// A `MachineIdSource` that answers from a vector's script: a file's content or
/// error (unlisted: ENOENT) and when it was written; a command's output or
/// error (unlisted: ENOENT).
struct Scripted {
    platform: String,
    system_root: Option<String>,
    files: Map<String, Value>,
    commands: Map<String, Value>,
}

impl Scripted {
    fn of(source: &Value) -> Scripted {
        let map = |name: &str| source[name].as_object().cloned().unwrap_or_default();
        Scripted {
            platform: text(&source["platform"], "platform").to_owned(),
            system_root: source["systemRoot"].as_str().map(str::to_owned),
            files: map("files"),
            commands: map("commands"),
        }
    }

    /// A scripted command's output, or what its failure means.
    fn command(&self, command: &str) -> Result<String, ReadFailure> {
        let answer = self.commands.get(command).ok_or(ReadFailure::Unavailable)?;
        if let Some(error) = answer.get("error") {
            return Err(failure_of(error));
        }
        Ok(answer["output"].as_str().unwrap_or_default().to_owned())
    }
}

/// What a scripted error means: killed or ended by a signal is this time only;
/// an exit code is for good; an errno is for good when it says there is nothing
/// to read, else this time only.
fn failure_of(error: &Value) -> ReadFailure {
    if error["killed"].as_bool() == Some(true) || error["signal"].is_string() {
        return ReadFailure::Transient;
    }
    match &error["code"] {
        Value::Number(_) => ReadFailure::Unavailable,
        Value::String(code)
            if ["ENOENT", "EACCES", "EPERM", "ENOTDIR", "EISDIR"].contains(&code.as_str()) =>
        {
            ReadFailure::Unavailable
        }
        _ => ReadFailure::Transient,
    }
}

const STANDARD_REG: &str = r"C:\Windows\System32\reg.exe";

impl MachineIdSource for Scripted {
    fn platform(&self) -> String {
        self.platform.clone()
    }

    /// This SDK reads `MachineGuid` through the registry API, not `reg.exe`.
    /// The vectors script `reg.exe`, so this maps the script onto what the
    /// registry would hold: the value printed on the `MachineGuid REG_SZ`
    /// line (the standard reg.exe's, else `%SystemRoot%`'s when the standard
    /// one is not there), none when no such line, the failure otherwise. The
    /// SDK's own step, the id from the value, is what this pins.
    fn windows_machine_guid(&self) -> Result<String, ReadFailure> {
        let output = match self.command(STANDARD_REG) {
            Err(ReadFailure::Unavailable) if !self.commands.contains_key(STANDARD_REG) => {
                let elsewhere = self
                    .system_root
                    .as_ref()
                    .map(|root| format!(r"{root}\System32\reg.exe"));
                match elsewhere.filter(|path| path != STANDARD_REG) {
                    Some(path) => self.command(&path)?,
                    None => return Err(ReadFailure::Unavailable),
                }
            }
            other => other?,
        };
        output
            .lines()
            .find_map(|line| {
                let after = line.trim_start().strip_prefix("MachineGuid")?;
                let value = after.trim_start().strip_prefix("REG_SZ")?;
                Some(value.trim().to_owned())
            })
            .ok_or(ReadFailure::Unavailable)
    }

    fn run(
        &self,
        command: &str,
        _args: &[&str],
        _timeout: Duration,
    ) -> Result<String, ReadFailure> {
        self.command(command)
    }

    fn read_file(&self, path: &str) -> Result<String, ReadFailure> {
        let file = self.files.get(path).ok_or(ReadFailure::Unavailable)?;
        if let Some(error) = file.get("error") {
            return Err(failure_of(error));
        }
        Ok(file["content"].as_str().unwrap_or_default().to_owned())
    }

    fn mtime_ms(&self, path: &str) -> Result<f64, ReadFailure> {
        self.files
            .get(path)
            .and_then(|file| file["mtimeMs"].as_f64())
            .ok_or(ReadFailure::Unavailable)
    }
}

#[test]
fn machine_ids() {
    each(&vectors(), "machineIds", |vector| {
        match read_machine_id(
            Arc::new(Scripted::of(&vector["source"])),
            MACHINE_ID_TIMEOUT,
        ) {
            MachineIdReading::Id(id) => json!({ "kind": "id", "id": id }),
            MachineIdReading::Unavailable => json!({ "kind": "unavailable" }),
            MachineIdReading::Transient => json!({ "kind": "transient" }),
        }
    });
}

// --- who this device is ---------------------------------------------------------------------------

fn reading(value: &Value) -> MachineIdReading {
    match text(&value["kind"], "reading.kind") {
        "id" => MachineIdReading::Id(text(&value["id"], "reading.id").to_owned()),
        "unavailable" => MachineIdReading::Unavailable,
        "transient" => MachineIdReading::Transient,
        other => panic!("unknown reading {other:?}"),
    }
}

/// A vector's host names (`null`: they could not be read).
fn host_names(value: &Value) -> Option<HostNames> {
    value.is_object().then(|| HostNames {
        host: text(&value["host"], "host.host").to_owned(),
        platform: text(&value["platform"], "host.platform").to_owned(),
        arch: text(&value["arch"], "host.arch").to_owned(),
    })
}

#[test]
fn identities() {
    each(&vectors(), "identities", |vector| {
        let identity = identity_of(
            &reading(&vector["reading"]),
            host_names(&vector["host"]).as_ref(),
        );
        let mut out = json!({
            "fingerprint": identity.fingerprint,
            "alternates": identity.alternates,
        });
        if let Some(kind) = identity.kind {
            out["kind"] = json!(kind.as_str());
        }
        out
    });
}

/// A vector's identity, as a fingerprinter answers it.
fn identity(value: &Value) -> DeviceIdentity {
    DeviceIdentity {
        fingerprint: value["fingerprint"].as_str().map(str::to_owned),
        kind: kind(&value["kind"]),
        alternates: strings(&value["alternates"]),
    }
}

#[test]
fn devices() {
    each(&vectors(), "devices", |vector| {
        device_from(
            &identity(&vector["identity"]),
            kind(&vector["installKind"]),
            vector["deviceHold"].as_bool().expect("deviceHold"),
            fingerprint_claim,
        )
        .to_json()
    });
}
