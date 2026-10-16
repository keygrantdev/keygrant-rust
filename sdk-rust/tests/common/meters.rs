//! What the metered-uses suites share: a key's lease carrying meters, a
//! harness holding it, and the server's answers to a `use`.

use keygrant::{HttpResponse, SpendEntry, SpendKind, StoredState};
use serde_json::{json, Value};

use super::{reply, Harness, Opts, TestKey, KEY, NOW, PRODUCT};

pub const MONTH: i64 = 30 * 24 * 3600;
pub const MINUTE: i64 = 60_000;
pub const HOUR: i64 = 3_600_000;

/// Long enough ago that the journal's wait for an entry is over.
pub const LONG_AGO: i64 = NOW - 2 * HOUR;

/// Bounds for a test whose call waits behind one that has not finished: it waits the HTTP bound and the
/// grace, then goes on without the lock. The bound is also what its own request may take, so it is long
/// beside an answer in memory, and the grace is short.
pub fn behind_opts() -> Opts {
    Opts {
        timings: Some(keygrant::Timings {
            lock_grace: std::time::Duration::from_millis(50),
            ..super::patient_timings()
        }),
        http_timeout: Some(std::time::Duration::from_millis(1500)),
        ..Opts::default()
    }
}

/// A key with 100 exports, 90 of them left, a device allowance of 10, and 5
/// renders a billing period.
pub fn meters() -> Value {
    json!({
        "exports": { "left": 90, "total": 100, "offline": 10 },
        "renders": { "left": 5, "total": 5, "offline": 0, "resets": true },
    })
}

/// A licence lease for `KEY` on `act_1`, covering majors 1 to 3, with `met`
/// (none for `Value::Null`), signed at `now` (ms) for a month, `over` laid over
/// its claims.
pub fn metered_lease_at(key: &TestKey, met: Value, now: i64, over: Value) -> String {
    let iat = now.div_euclid(1000);
    let mut claims = json!({
        "iss": "keygrant", "sub": KEY, "product": PRODUCT, "aid": "act_1", "model": "subscription",
        "lim": 3, "major": 3, "ent": {}, "iat": iat, "exp": iat + MONTH,
    });
    let map = claims.as_object_mut().expect("an object");
    if !met.is_null() {
        map.insert("met".into(), met);
    }
    if let Value::Object(over) = over {
        for (name, value) in over {
            if value.is_null() {
                map.remove(&name);
            } else {
                map.insert(name, value);
            }
        }
    }
    key.sign_payload(&claims)
}

/// [`metered_lease_at`] signed now.
pub fn metered_lease(key: &TestKey, met: Value) -> String {
    metered_lease_at(key, met, NOW, Value::Null)
}

/// A lease whose `met` is the JSON text given, written exactly as given (a
/// figure `4.0`, `1e1` or `1e400` stays as it is), signed as the server signs.
pub fn raw_lease(key: &TestKey, met_text: &str) -> String {
    let iat = NOW.div_euclid(1000);
    let text = format!(
        r#"{{"iss":"keygrant","sub":"{KEY}","product":"{PRODUCT}","aid":"act_1","model":"subscription","lim":3,"major":3,"ent":{{}},"met":{met_text},"iat":{iat},"exp":{}}}"#,
        iat + MONTH
    );
    key.sign_text(&text)
}

/// A stored state holding `KEY` activated on `act_1` with `lease`, seen at NOW.
pub fn stored_with(lease: &str) -> StoredState {
    StoredState {
        key: Some(KEY.into()),
        activation_id: Some("act_1".into()),
        lease: Some(lease.into()),
        last_seen: Some(NOW),
        ..StoredState::default()
    }
}

/// A harness whose storage holds `KEY` activated on `act_1` with a lease
/// carrying `met`, and `state` laid beside it.
pub fn holding_with(met: Value, opts: Opts, state: impl FnOnce(&mut StoredState)) -> Harness {
    let h = Harness::new(None, opts);
    let mut stored = stored_with(&metered_lease(&h.key, met));
    state(&mut stored);
    h.set_state(stored);
    h
}

/// [`holding_with`] the usual meters and nothing beside the lease.
pub fn holding() -> Harness {
    holding_with(meters(), Opts::default(), |_| {})
}

/// A harness holding the usual meters and `spends` in its journal.
pub fn holding_journal(spends: Vec<SpendEntry>) -> Harness {
    holding_with(meters(), Opts::default(), |state| {
        state.spends = Some(spends);
    })
}

/// The server's answer to a spend that landed: what is left of `exports` now,
/// at `at`.
pub fn spent(left: u64, at: i64) -> HttpResponse {
    reply(
        200,
        json!({ "spent": true, "meters": { "exports": { "left": left, "total": 100, "offline": 10 } }, "at": at }),
    )
}

/// The journal's entry for a spend of `exports`.
pub fn entry(kind: SpendKind, n: u64, at: i64) -> SpendEntry {
    SpendEntry {
        id: "AAAAAAAAAAAAAAAAAAAAAA".into(),
        meter: "exports".into(),
        n,
        at,
        kind,
    }
}

/// [`entry`] under an id of its own.
pub fn entry_of(id: &str, kind: SpendKind, n: u64, at: i64) -> SpendEntry {
    SpendEntry {
        id: id.into(),
        ..entry(kind, n, at)
    }
}

/// The body of the `nth` (from 0) `use` request.
pub fn use_body(h: &Harness, nth: usize) -> Value {
    h.http
        .calls()
        .into_iter()
        .filter(|(action, _)| action == "use")
        .nth(nth)
        .map(|(_, body)| body)
        .unwrap_or_else(|| panic!("expected use request {nth}"))
}

/// The journal as stored now.
pub fn journal_of(h: &Harness) -> Vec<SpendEntry> {
    h.storage
        .get()
        .and_then(|state| state.spends)
        .unwrap_or_default()
}

/// The journal as `[(kind, n)]`.
pub fn kinds_and_counts(journal: &[SpendEntry]) -> Vec<(SpendKind, u64)> {
    journal.iter().map(|e| (e.kind, e.n)).collect()
}
