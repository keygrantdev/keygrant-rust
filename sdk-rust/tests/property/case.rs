//! A case of the property test of `use_meter`: plain data, so a failing one can
//! be shrunk and kept as a named test. The generator is the reference's, line for
//! line (the same seeded generator and the draws in the same order), so a seed
//! means the same case in every KeyGrant SDK.

use std::collections::BTreeMap;

use keygrant::SpendKind;
use serde_json::{json, Map, Value};

const MINUTE: i64 = 60_000;
const HOUR: i64 = 3_600_000;

/// A meter the key holds.
#[derive(Debug, Clone, PartialEq)]
pub struct MeterSpec {
    pub total: u64,
    pub offline: u64,
    pub resets: bool,
}

/// One `use_meter` of a process.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub p: usize,
    pub meter: String,
    pub n: u64,
}

/// An entry the journal already holds when the case begins. `recorded`: the
/// server holds the spend (or the report) already.
#[derive(Debug, Clone, PartialEq)]
pub struct Preloaded {
    pub meter: String,
    pub n: u64,
    pub kind: SpendKind,
    pub ago_ms: i64,
    pub recorded: bool,
}

/// What a step does; `r` are the random numbers its requests draw their fates
/// from, so a case replays whole.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Use {
        calls: Vec<Call>,
        status: Option<(usize, bool)>,
        r: Vec<f64>,
    },
    Status {
        p: usize,
        force: bool,
        r: Vec<f64>,
    },
    Repeat {
        call: Call,
        times: u32,
        gap_ms: i64,
        r: Vec<f64>,
    },
    Deactivate {
        p: usize,
        r: Vec<f64>,
    },
    Restart {
        p: usize,
    },
    Wait {
        ms: i64,
    },
    Jump {
        ms: i64,
    },
    Net {
        mode: String,
    },
    Disk {
        failing: bool,
    },
}

/// A case: the meters, the journal it begins with, and the steps.
#[derive(Debug, Clone, PartialEq)]
pub struct Case {
    pub seed: i64,
    pub meters: BTreeMap<String, MeterSpec>,
    pub preload: Vec<Preloaded>,
    pub steps: Vec<Step>,
}

fn call_json(call: &Call) -> Value {
    json!({ "p": call.p, "meter": call.meter, "n": call.n })
}

fn call_of(value: &Value) -> Call {
    Call {
        p: value["p"].as_u64().expect("p") as usize,
        meter: value["meter"].as_str().expect("meter").to_owned(),
        n: value["n"].as_u64().expect("n"),
    }
}

fn draws_of(value: &Value) -> Vec<f64> {
    value["r"]
        .as_array()
        .map(|r| r.iter().map(|d| d.as_f64().expect("a draw")).collect())
        .unwrap_or_default()
}

impl Step {
    /// The step's kind, as the file writes it.
    pub fn kind(&self) -> &'static str {
        match self {
            Step::Use { .. } => "use",
            Step::Status { .. } => "status",
            Step::Repeat { .. } => "repeat",
            Step::Deactivate { .. } => "deactivate",
            Step::Restart { .. } => "restart",
            Step::Wait { .. } => "wait",
            Step::Jump { .. } => "jump",
            Step::Net { .. } => "net",
            Step::Disk { .. } => "disk",
        }
    }

    /// The draws the step carries, if it has any.
    pub fn draws(&self) -> &[f64] {
        match self {
            Step::Use { r, .. }
            | Step::Status { r, .. }
            | Step::Repeat { r, .. }
            | Step::Deactivate { r, .. } => r,
            _ => &[],
        }
    }

    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("t".into(), json!(self.kind()));
        match self {
            Step::Use { calls, status, .. } => {
                out.insert(
                    "calls".into(),
                    Value::Array(calls.iter().map(call_json).collect()),
                );
                if let Some((p, force)) = status {
                    out.insert("status".into(), json!({ "p": p, "force": force }));
                }
            }
            Step::Status { p, force, .. } => {
                out.insert("p".into(), json!(p));
                out.insert("force".into(), json!(force));
            }
            Step::Repeat {
                call,
                times,
                gap_ms,
                ..
            } => {
                out.insert("call".into(), call_json(call));
                out.insert("times".into(), json!(times));
                out.insert("gapMs".into(), json!(gap_ms));
            }
            Step::Deactivate { p, .. } | Step::Restart { p } => {
                out.insert("p".into(), json!(p));
            }
            Step::Wait { ms } | Step::Jump { ms } => {
                out.insert("ms".into(), json!(ms));
            }
            Step::Net { mode } => {
                out.insert("mode".into(), json!(mode));
            }
            Step::Disk { failing } => {
                out.insert("failing".into(), json!(failing));
            }
        }
        if !self.draws().is_empty() {
            out.insert("r".into(), json!(self.draws()));
        }
        Value::Object(out)
    }

    pub fn from_json(value: &Value) -> Step {
        let r = draws_of(value);
        let p = || value["p"].as_u64().unwrap_or(0) as usize;
        match value["t"].as_str().expect("a step") {
            "use" => Step::Use {
                calls: value["calls"]
                    .as_array()
                    .expect("calls")
                    .iter()
                    .map(call_of)
                    .collect(),
                status: value.get("status").filter(|s| s.is_object()).map(|s| {
                    (
                        s["p"].as_u64().expect("p") as usize,
                        s["force"].as_bool().expect("force"),
                    )
                }),
                r,
            },
            "status" => Step::Status {
                p: p(),
                force: value["force"].as_bool().expect("force"),
                r,
            },
            "repeat" => Step::Repeat {
                call: call_of(&value["call"]),
                times: value["times"].as_u64().expect("times") as u32,
                gap_ms: value["gapMs"].as_i64().expect("gapMs"),
                r,
            },
            "deactivate" => Step::Deactivate { p: p(), r },
            "restart" => Step::Restart { p: p() },
            "wait" => Step::Wait {
                ms: value["ms"].as_i64().expect("ms"),
            },
            "jump" => Step::Jump {
                ms: value["ms"].as_i64().expect("ms"),
            },
            "net" => Step::Net {
                mode: value["mode"].as_str().expect("mode").to_owned(),
            },
            "disk" => Step::Disk {
                failing: value["failing"].as_bool().expect("failing"),
            },
            other => panic!("unknown step {other:?}"),
        }
    }
}

impl Case {
    pub fn to_json(&self) -> Value {
        let meters: Map<String, Value> = self
            .meters
            .iter()
            .map(|(code, m)| {
                (
                    code.clone(),
                    json!({ "total": m.total, "offline": m.offline, "resets": m.resets }),
                )
            })
            .collect();
        let preload: Vec<Value> = self
            .preload
            .iter()
            .map(|p| {
                json!({
                    "meter": p.meter, "n": p.n, "kind": p.kind.as_str(), "agoMs": p.ago_ms, "recorded": p.recorded,
                })
            })
            .collect();
        json!({
            "seed": self.seed,
            "meters": Value::Object(meters),
            "preload": preload,
            "steps": self.steps.iter().map(Step::to_json).collect::<Vec<_>>(),
        })
    }

    pub fn from_json(text: &str) -> Case {
        let value: Value = serde_json::from_str(text).expect("a case is JSON");
        Case {
            seed: value["seed"].as_i64().expect("seed"),
            meters: value["meters"]
                .as_object()
                .expect("meters")
                .iter()
                .map(|(code, m)| {
                    let spec = MeterSpec {
                        total: m["total"].as_u64().expect("total"),
                        offline: m["offline"].as_u64().expect("offline"),
                        resets: m["resets"].as_bool().expect("resets"),
                    };
                    (code.clone(), spec)
                })
                .collect(),
            preload: value["preload"]
                .as_array()
                .expect("preload")
                .iter()
                .map(|p| Preloaded {
                    meter: p["meter"].as_str().expect("meter").to_owned(),
                    n: p["n"].as_u64().expect("n"),
                    kind: SpendKind::parse(p["kind"].as_str().expect("kind")).expect("a kind"),
                    ago_ms: p["agoMs"].as_i64().expect("agoMs"),
                    recorded: p["recorded"].as_bool().expect("recorded"),
                })
                .collect(),
            steps: value["steps"]
                .as_array()
                .expect("steps")
                .iter()
                .map(Step::from_json)
                .collect(),
        }
    }

    pub fn with_steps(&self, steps: Vec<Step>) -> Case {
        Case {
            steps,
            ..self.clone()
        }
    }
}

/// The reference's seeded generator (mulberry32): the same seed, the same sequence.
pub struct Rng(u32);

impl Rng {
    pub fn new(seed: u32) -> Rng {
        Rng(seed)
    }

    pub fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x6d2b79f5);
        let a = self.0;
        let mut t = (a ^ (a >> 15)).wrapping_mul(1 | a);
        t = t.wrapping_add((t ^ (t >> 7)).wrapping_mul(61 | t)) ^ t;
        f64::from(t ^ (t >> 14)) / 4_294_967_296.0
    }
}

struct Gen {
    rng: Rng,
}

impl Gen {
    fn rand(&mut self) -> f64 {
        self.rng.next()
    }

    fn int(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.rand() * (hi - lo + 1) as f64).floor() as i64
    }

    fn pick<T: Clone>(&mut self, xs: &[T]) -> T {
        xs[(self.rand() * xs.len() as f64).floor() as usize].clone()
    }

    fn code(&mut self) -> String {
        if self.rand() < 0.1 {
            "ghost".into()
        } else {
            self.pick(&["exports", "exports", "renders", "credits"])
                .into()
        }
    }

    fn amount(&mut self) -> u64 {
        if self.rand() < 0.7 {
            1
        } else if self.rand() < 0.8 {
            self.int(2, 5) as u64
        } else if self.rand() < 0.7 {
            self.int(6, 14) as u64
        } else {
            500
        }
    }

    fn draws(&mut self) -> Vec<f64> {
        (0..6).map(|_| self.rand()).collect()
    }

    fn call(&mut self, p: Option<usize>, not: Option<&str>) -> Call {
        let process = p.unwrap_or_else(|| self.int(0, 1) as usize);
        let mut meter = self.code();
        while Some(meter.as_str()) == not {
            meter = self.code();
        }
        Call {
            p: process,
            meter,
            n: self.amount(),
        }
    }
}

/// The case a seed makes: the reference's generator, line for line.
pub fn generate(seed: i64) -> Case {
    let mut g = Gen {
        rng: Rng::new(seed as u32),
    };
    let meters = BTreeMap::from([
        (
            "exports".to_owned(),
            MeterSpec {
                total: g.int(8, 150) as u64,
                offline: g.int(1, 12) as u64,
                resets: false,
            },
        ),
        (
            "renders".to_owned(),
            MeterSpec {
                total: g.int(3, 40) as u64,
                offline: 0,
                resets: true,
            },
        ),
        (
            "credits".to_owned(),
            MeterSpec {
                total: g.int(4, 60) as u64,
                offline: g.int(0, 6) as u64,
                resets: false,
            },
        ),
    ]);
    let all = ["exports", "renders", "credits"];
    // A storm: sixteen voids or one short of it, an outage, and spends the allowance cannot cover a minute apart: the cap is reached.
    let storm = g.rand() < 0.12;
    // A journal past the cap, which a release reports 64 of.
    let overfull = !storm && g.rand() < 0.06;
    let mut preload: Vec<Preloaded> = Vec::new();
    if storm || overfull || g.rand() < 0.4 {
        let offline: Vec<&str> = all.iter().copied().filter(|_| g.rand() < 0.3).collect();
        // How many of the entries are voids is drawn once: sixteen or one short of it in a storm, else none to sixteen.
        let voids = if storm {
            g.int(14, 16)
        } else if overfull {
            g.int(0, 13)
        } else {
            g.int(0, 16)
        };
        let count = if overfull {
            g.int(65, 70)
        } else {
            let wanted = if storm {
                voids + g.int(0, 6)
            } else {
                g.pick(&[1, 2, 3, 5, 8, 20, 40, 55, 62, 64])
            };
            wanted.min(64 - offline.len() as i64)
        };
        for i in 0..count {
            let kind = if i < voids {
                SpendKind::Void
            } else {
                SpendKind::Unanswered
            };
            let meter = g.pick(&all).to_owned();
            let n = g.int(1, 2) as u64;
            let ago = if storm {
                g.pick(&[5 * MINUTE, 2 * HOUR, 20 * HOUR])
            } else {
                g.pick(&[1_000, 30_000, 5 * MINUTE, 2 * HOUR, 20 * HOUR])
            };
            let recorded = g.rand() < 0.5;
            preload.push(Preloaded {
                meter,
                n,
                kind,
                ago_ms: ago,
                recorded,
            });
        }
        for m in offline {
            let n = g.int(1, 3) as u64;
            let ago = g.pick(&[5 * MINUTE, 2 * HOUR]);
            let recorded = g.rand() < 0.5;
            preload.push(Preloaded {
                meter: m.to_owned(),
                n,
                kind: SpendKind::Offline,
                ago_ms: ago,
                recorded,
            });
        }
        // Stable, as Array.prototype.sort is: the oldest first.
        preload.sort_by_key(|a| std::cmp::Reverse(a.ago_ms));
        // Past the cap, the newest entries are voids: the release carries 64 and leaves these behind.
        if overfull {
            for _ in 0..g.int(1, 3) {
                let meter = g.pick(&all).to_owned();
                let recorded = g.rand() < 0.7;
                preload.push(Preloaded {
                    meter,
                    n: 1,
                    kind: SpendKind::Void,
                    ago_ms: 500,
                    recorded,
                });
            }
        }
    }

    let mut steps: Vec<Step> = Vec::new();
    if storm {
        steps.push(Step::Net {
            mode: "down".into(),
        });
        let p = g.int(0, 1) as usize;
        let times = g.int(3, 8) as u32;
        let r = g.draws();
        steps.push(Step::Repeat {
            call: Call {
                p,
                meter: "renders".into(),
                n: 1,
            },
            times,
            gap_ms: 61_000,
            r,
        });
    } else if overfull {
        steps.push(Step::Net {
            mode: "good".into(),
        });
        let p = g.int(0, 1) as usize;
        let r = g.draws();
        steps.push(Step::Deactivate { p, r });
    }
    for _ in 0..g.int(8, 26) {
        let x = g.rand();
        if x < 0.3 {
            let c = g.call(None, None);
            steps.push(Step::Use {
                calls: vec![c],
                status: None,
                r: g.draws(),
            });
        } else if x < 0.34 {
            let call = g.call(None, None);
            let times = g.int(2, 14) as u32;
            let gap_ms = g.pick(&[10_000, 61_000, 5 * MINUTE, 70 * MINUTE]);
            steps.push(Step::Repeat {
                call,
                times,
                gap_ms,
                r: g.draws(),
            });
        } else if x < 0.42 {
            let a = g.call(Some(0), None);
            let b = g.call(Some(1), None);
            steps.push(Step::Use {
                calls: vec![a, b],
                status: None,
                r: g.draws(),
            });
        } else if x < 0.48 {
            let first = g.call(None, None);
            let second = g.call(Some(first.p), Some(&first.meter));
            steps.push(Step::Use {
                calls: vec![first, second],
                status: None,
                r: g.draws(),
            });
        } else if x < 0.53 {
            let c = g.call(None, None);
            let p = g.int(0, 1) as usize;
            let force = g.rand() < 0.7;
            steps.push(Step::Use {
                calls: vec![c],
                status: Some((p, force)),
                r: g.draws(),
            });
        } else if x < 0.64 {
            let p = g.int(0, 1) as usize;
            let force = g.rand() < 0.5;
            steps.push(Step::Status {
                p,
                force,
                r: g.draws(),
            });
        } else if x < 0.78 {
            let ms = g.pick(&[
                1_000,
                20_000,
                59_000,
                61_000,
                5 * MINUTE,
                70 * MINUTE,
                3 * HOUR,
                26 * HOUR,
            ]);
            steps.push(Step::Wait { ms });
        } else if x < 0.84 {
            let ms = g.pick(&[
                30_000,
                10 * MINUTE,
                2 * HOUR,
                30 * HOUR,
                -30_000,
                -10 * MINUTE,
                -2 * HOUR,
                -30 * HOUR,
            ]);
            steps.push(Step::Jump { ms });
        } else if x < 0.92 {
            let mode = g.pick(&["good", "down", "down", "flaky", "flaky"]);
            steps.push(Step::Net { mode: mode.into() });
        } else if x < 0.97 {
            let failing = g.rand() < 0.6;
            steps.push(Step::Disk { failing });
        } else if x < 0.985 {
            steps.push(Step::Restart {
                p: g.int(0, 1) as usize,
            });
        } else {
            let p = g.int(0, 1) as usize;
            steps.push(Step::Deactivate { p, r: g.draws() });
        }
    }
    // A release after an outage: spends made while the disk refused, the disk back, and a deactivate, which must carry them.
    if g.rand() < 0.1 {
        let p = g.int(0, 1) as usize;
        steps.push(Step::Net {
            mode: "down".into(),
        });
        steps.push(Step::Disk { failing: true });
        let meter = g.pick(&["exports", "credits"]).to_owned();
        steps.push(Step::Use {
            calls: vec![Call { p, meter, n: 1 }],
            status: None,
            r: g.draws(),
        });
        steps.push(Step::Use {
            calls: vec![Call {
                p,
                meter: "exports".into(),
                n: 1,
            }],
            status: None,
            r: g.draws(),
        });
        steps.push(Step::Disk { failing: false });
        steps.push(Step::Net {
            mode: "good".into(),
        });
        steps.push(Step::Deactivate { p, r: g.draws() });
    }
    Case {
        seed,
        meters,
        preload,
        steps,
    }
}
