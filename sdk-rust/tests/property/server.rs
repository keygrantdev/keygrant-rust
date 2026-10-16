//! The server a property test of `use_meter` runs the real `License` against:
//! what the KeyGrant API does with a request that carries a spend or a report of
//! unsynced ones, as the contract says: every report lands once by its id, an
//! entry that grew adds what it grew by, a void takes an online spend or a report
//! back and keeps its id from counting later, and an answer names the entries it
//! holds. Test support only.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{json, Map, Value};

use super::case::MeterSpec;
use crate::common::TestKey;

/// What the server did with one request.
pub struct Served {
    pub status: u16,
    pub json: Value,
    /// The id of a spend of its own that this request recorded: a `use` that landed.
    pub spent: Option<String>,
}

/// What it holds by spend id: an online spend or a report, at the count it has.
pub struct Held {
    pub meter: String,
    pub n: u64,
}

pub struct PropServer {
    pub meters: BTreeMap<String, MeterSpec>,
    key: TestKey,
    /// What it holds by spend id.
    pub recorded: HashMap<String, Held>,
    /// The ids a void took back, or was the first to name.
    pub voided: HashSet<String>,
    /// The server's time (ms): it moves on by a millisecond with each request, so no two answers carry one time.
    pub now: i64,
    activations: u32,
}

impl PropServer {
    pub fn new(meters: BTreeMap<String, MeterSpec>, key: TestKey, now: i64) -> PropServer {
        PropServer {
            meters,
            key,
            recorded: HashMap::new(),
            voided: HashSet::new(),
            now,
            activations: 0,
        }
    }

    /// What a meter has used: its recorded spends, whatever the allowance.
    pub fn used(&self, code: &str) -> u64 {
        self.recorded
            .values()
            .filter(|held| held.meter == code)
            .map(|held| held.n)
            .sum()
    }

    pub fn left(&self, code: &str) -> u64 {
        self.meters
            .get(code)
            .map_or(0, |spec| spec.total.saturating_sub(self.used(code)))
    }

    fn figures(&self) -> Value {
        let figures: Map<String, Value> = self
            .meters
            .iter()
            .map(|(code, m)| {
                let mut figure =
                    json!({ "left": self.left(code), "total": m.total, "offline": m.offline });
                if m.resets {
                    figure["resets"] = json!(true);
                }
                (code.clone(), figure)
            })
            .collect();
        Value::Object(figures)
    }

    /// The lease the server signs now for an activation: the key's meters as they stand, valid for 60 days.
    pub fn lease(&self, aid: &str) -> String {
        self.key.lease_with(
            self.now,
            60 * 86_400,
            json!({ "aid": aid, "model": "subscription", "major": 3, "met": self.figures() }),
        )
    }

    /// Take a request's reports, and name the entries held after (always accepted).
    fn report(&mut self, spends: &Value) -> Vec<Value> {
        let mut confirmed = Vec::new();
        for e in spends.as_array().into_iter().flatten() {
            let id = e["id"].as_str().unwrap_or_default().to_owned();
            let is_void = e.get("void").is_some();
            let n = e["n"].as_u64().unwrap_or(0);
            if self.voided.contains(&id) {
                confirmed.push(if is_void {
                    json!({ "id": id, "void": true })
                } else {
                    json!({ "id": id, "n": n })
                });
                continue;
            }
            if is_void {
                self.recorded.remove(&id);
                self.voided.insert(id.clone());
                confirmed.push(json!({ "id": id, "void": true }));
                continue;
            }
            match self.recorded.get_mut(&id) {
                None => {
                    let meter = e["meter"].as_str().unwrap_or_default().to_owned();
                    self.recorded.insert(id.clone(), Held { meter, n });
                }
                Some(held) if n > held.n => held.n = n,
                Some(_) => {}
            }
            confirmed.push(json!({ "id": id, "n": n }));
        }
        confirmed
    }

    /// What the server answers a request of `action`, and what it recorded.
    pub fn handle(&mut self, action: &str, body: &Value) -> Served {
        self.now += 1;
        let confirmed = self.report(&body["spends"]);
        let named = |mut json: Value| {
            if !confirmed.is_empty() {
                json["spends"] = Value::Array(confirmed.clone());
            }
            json
        };
        match action {
            "use" => self.use_meter(body, &named),
            "validate" => {
                let aid = body["activationId"].as_str().unwrap_or_default();
                Served {
                    status: 200,
                    json: named(json!({ "lease": self.lease(aid) })),
                    spent: None,
                }
            }
            "deactivate" => Served {
                status: 200,
                json: named(json!({ "released": true })),
                spent: None,
            },
            "activate" => {
                self.activations += 1;
                let aid = format!("act_{}", self.activations + 1);
                Served {
                    status: 200,
                    json: json!({ "lease": self.lease(&aid), "activationId": aid }),
                    spent: None,
                }
            }
            _ => Served {
                status: 404,
                json: json!({ "error": "unknown" }),
                spent: None,
            },
        }
    }

    fn use_meter(&mut self, body: &Value, named: &dyn Fn(Value) -> Value) -> Served {
        let meter = body["meter"].as_str().unwrap_or_default().to_owned();
        let n = body["n"].as_u64().unwrap_or(0);
        let id = body["id"].as_str().unwrap_or_default().to_owned();
        if !self.meters.contains_key(&meter) {
            return Served {
                status: 404,
                json: named(json!({ "error": "unknown-meter" })),
                spent: None,
            };
        }
        if self.recorded.contains_key(&id) {
            let json = named(json!({ "spent": true, "meters": self.figures(), "at": self.now }));
            return Served {
                status: 200,
                json,
                spent: None,
            };
        }
        if n > self.left(&meter) {
            let json = named(
                json!({ "error": "used-up", "left": self.left(&meter), "meters": self.figures(), "at": self.now }),
            );
            return Served {
                status: 409,
                json,
                spent: None,
            };
        }
        self.recorded.insert(id.clone(), Held { meter, n });
        let json = named(json!({ "spent": true, "meters": self.figures(), "at": self.now }));
        Served {
            status: 200,
            json,
            spent: Some(id),
        }
    }
}
