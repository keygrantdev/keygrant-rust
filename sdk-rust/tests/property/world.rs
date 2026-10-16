//! The world a property test of `use_meter` runs the real `License` in
//! (`net.rs` is the machine; `use_property.rs` the test): the steps of a case,
//! and the oracle that judges every answer and what must hold between steps and
//! at the end.
//!
//! The oracle is written from the contract and takes nothing from the code it
//! judges but the `License` itself and the shape of a stored state: its constants,
//! its figures and its waits are written again below, so a change to the code
//! cannot move the code and what judges it together.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use keygrant::journal::JournalOp;
use keygrant::{LicenseStatus, SpendEntry, SpendKind, StoredState, UseError, UseResult};
use serde_json::Value;

use super::case::{Call, Case, Step};
use super::net::{label, CallValue, Checked, Net, PropertyFailure, Req, Running, Shared};
use super::server::PropServer;
use crate::common::{TestKey, KEY, NOW};

const MINUTE: i64 = 60_000;
const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;

/// The contract's numbers: the journal's entries and voids.
const ENTRIES: usize = 64;
const VOIDS: usize = 16;

// --- the oracle's own reading of the contract ----------------------------------------------

fn entries_of(state: &StoredState) -> Vec<SpendEntry> {
    state.spends.clone().unwrap_or_default()
}

/// The unsynced spends of a meter, its `offline` and `unanswered` entries, never a void.
fn unsynced_in(spends: &[SpendEntry], meter: &str) -> u64 {
    spends
        .iter()
        .filter(|e| e.meter == meter && e.kind != SpendKind::Void)
        .map(|e| e.n)
        .sum()
}

/// The payload of a lease, read here and not by the SDK: base64url, then JSON.
fn payload_of(lease: &str) -> Value {
    let text = lease.split('.').nth(1).unwrap_or_default();
    let mut bytes = Vec::new();
    let (mut buffer, mut bits) = (0u32, 0);
    for c in text.bytes() {
        let sextet = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            _ => 63,
        };
        buffer = (buffer << 6) | u32::from(sextet);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// A meter's base figure (left, offline): the stored one when its time is at or after the lease's second, else the lease's own.
fn base_figure(state: &StoredState, meter: &str) -> Option<(u64, u64)> {
    let (_, lease) = (state.key.as_ref()?, state.lease.as_ref()?);
    let claims = payload_of(lease);
    let iat = claims["iat"].as_i64().unwrap_or(0);
    if let Some(stored) = state
        .meter_figures
        .as_ref()
        .and_then(|figures| figures.get(meter))
    {
        if stored.at >= iat * 1000 {
            return Some((stored.left, stored.offline));
        }
    }
    let met = claims["met"].get(meter)?;
    Some((met["left"].as_u64()?, met["offline"].as_u64()?))
}

/// The journal's wait: k unanswered or void entries (none ahead of `now`), a minute doubling to an hour after the newest.
fn journal_wait(spends: &[SpendEntry], now: i64) -> bool {
    let opened: Vec<&SpendEntry> = spends
        .iter()
        .filter(|e| e.kind != SpendKind::Offline && e.at <= now)
        .collect();
    let Some(newest) = opened.iter().map(|e| e.at).max() else {
        return false;
    };
    now < newest + HOUR.min(MINUTE * (1i64 << (opened.len() - 1).min(6)))
}

/// The wait of a spend the allowance refuses: a void under a minute old, by the guarded clock and the device clock.
fn void_wait(spends: &[SpendEntry], now: i64, clock_now: i64) -> bool {
    spends.iter().any(|e| {
        e.kind == SpendKind::Void && e.at <= now && e.at <= clock_now && now < e.at + MINUTE
    })
}

/// The wait an unanswered ask leaves, by the device clock.
fn ask_wait(state: &StoredState, clock_now: i64) -> bool {
    state.backoff_since.is_some_and(|since| {
        state.backoff_ms == Some(MINUTE) && clock_now >= since && clock_now < since + MINUTE
    })
}

/// What the state said of a spend before the call.
struct Pre {
    licensed: bool,
    covered: bool,
    /// What `use_meter` must answer as `left` for a spend made offline: the figure less the unsynced, less the spend.
    left_after: i64,
    entry_count: usize,
    open_offline: bool,
    /// A wait of the covered spends runs: the no-answer wait (device clock) or the journal's (guarded clock).
    waiting: bool,
    /// The wait of a spend the allowance refuses runs.
    void_waiting: bool,
}

/// A spend refused after the server recorded it: the take-back is the void's. `full`: the journal was full when the
/// spend was made, so the void may have found no room.
struct Phantom {
    id: String,
    meter: String,
    n: u64,
    full: bool,
}

pub struct World {
    case: Case,
    shared: Arc<Shared>,
    step_no: usize,
    /// Every `use_meter` the customer was told was made, by meter.
    acked: HashMap<String, u64>,
    /// What the customer was told was made while only memory held it and has not been reported since, by meter: the overage the allowance may show.
    told: HashMap<String, u64>,
    told_ops: usize,
    /// What the contract allows to be lost unreported, by meter.
    loss_allowed: HashMap<String, u64>,
    phantoms: Vec<Phantom>,
    /// The ids of the voids each process held in memory at the last check.
    held_voids: [HashSet<String>; 2],
}

fn credit(into: &mut HashMap<String, u64>, meter: &str, n: u64) {
    if let Some(sum) = into.get_mut(meter) {
        *sum += n;
    }
}

impl World {
    pub fn new(case: Case, tracing: bool) -> World {
        let key = TestKey::new(7);
        let server = PropServer::new(case.meters.clone(), key.clone(), NOW);
        let mut acked = HashMap::new();
        let mut told = HashMap::new();
        let mut loss_allowed = HashMap::new();
        for m in case.meters.keys() {
            acked.insert(m.clone(), 0);
            told.insert(m.clone(), 0);
            loss_allowed.insert(m.clone(), 0);
        }
        let pre_id = |i: usize| format!("{:p<22}", format!("pre{i:03}"));
        let spends: Vec<SpendEntry> = case
            .preload
            .iter()
            .enumerate()
            .map(|(i, e)| SpendEntry {
                id: pre_id(i),
                meter: e.meter.clone(),
                n: e.n,
                at: NOW - e.ago_ms,
                kind: e.kind,
            })
            .collect();
        let state = StoredState {
            key: Some(KEY.into()),
            activation_id: Some("act_1".into()),
            last_seen: Some(NOW),
            spends: Some(spends.clone()).filter(|spends| !spends.is_empty()),
            ..StoredState::default()
        };
        let shared = Shared::new(state, server, key, NOW);
        let mut phantoms = Vec::new();
        {
            let mut inner = shared.lock();
            for (i, e) in case.preload.iter().enumerate() {
                inner.pre_ids.insert(pre_id(i));
                if e.recorded {
                    inner.server.recorded.insert(
                        pre_id(i),
                        super::server::Held {
                            meter: e.meter.clone(),
                            n: e.n,
                        },
                    );
                }
                if e.kind == SpendKind::Void {
                    phantoms.push(Phantom {
                        id: pre_id(i),
                        meter: e.meter.clone(),
                        n: e.n,
                        full: false,
                    });
                } else {
                    credit(&mut acked, &e.meter, e.n);
                }
            }
            inner.tracing = tracing;
        }
        // A journal that begins past the cap is one that spends told to the customer once landed past it.
        let told_ops = spends.len().saturating_sub(ENTRIES);
        for e in spends.iter().filter(|e| e.kind != SpendKind::Void) {
            credit(&mut told, &e.meter, e.n);
        }
        let procs = vec![shared.process(0), shared.process(1)];
        {
            let mut inner = shared.lock();
            inner.procs = procs;
            let lease = inner.server.lease("act_1");
            inner.state.lease = Some(lease);
        }
        World {
            case,
            shared,
            step_no: 0,
            acked,
            told,
            told_ops,
            loss_allowed,
            phantoms,
            held_voids: [HashSet::new(), HashSet::new()],
        }
    }

    /// What happened, for reading a failing case.
    pub fn trace(&self) -> Vec<String> {
        self.shared.lock().trace.clone()
    }

    fn proc(&self, p: usize) -> Arc<keygrant::License> {
        Arc::clone(&self.shared.lock().procs[p])
    }

    fn procs(&self) -> Vec<Arc<keygrant::License>> {
        self.shared.lock().procs.clone()
    }

    fn fail(&self, invariant: &str, message: impl Into<String>) -> PropertyFailure {
        let step = self
            .case
            .steps
            .get(self.step_no)
            .map_or("end".to_owned(), |s| s.to_json().to_string());
        PropertyFailure::new(
            invariant,
            format!("{} (step {}: {step})", message.into(), self.step_no),
        )
    }

    fn tracing(&self) -> bool {
        self.shared.lock().tracing
    }

    fn note(&self, line: String) {
        self.shared.lock().trace.push(line);
    }

    fn lbl(&self, id: &str) -> String {
        label(&mut self.shared.lock(), id)
    }

    fn current(&self) -> StoredState {
        self.shared.lock().state.clone()
    }

    fn held(&self, p: usize) -> Vec<JournalOp> {
        self.proc(p).held_changes()
    }

    fn view_entries(&self, p: usize) -> Vec<SpendEntry> {
        entries_of(&self.proc(p).view().unwrap_or_default())
    }

    // --- the steps ----------------------------------------------------------------------

    pub fn run(&mut self) -> Checked<()> {
        let steps = self.case.steps.clone();
        for (i, step) in steps.iter().enumerate() {
            self.step_no = i;
            if self.tracing() {
                let mut shown = step.to_json();
                if let Some(map) = shown.as_object_mut() {
                    map.remove("r");
                }
                let (net, disk) = (
                    self.shared.lock().net,
                    self.shared.disk_failing.load(Ordering::SeqCst),
                );
                let clock = self.shared.clock_now() - NOW;
                self.note(format!(
                    "step {i}: {shown} net={net:?} disk={} clock={clock}",
                    if disk { "failing" } else { "ok" }
                ));
            }
            self.step(step)?;
            if self.tracing() {
                self.describe();
            }
            self.check()?;
        }
        self.step_no = steps.len();
        self.converge()
    }

    fn set_draws(&self, r: &[f64]) {
        let mut inner = self.shared.lock();
        inner.rs = if r.is_empty() { vec![0.5] } else { r.to_vec() };
        inner.rk = 0;
    }

    fn step(&mut self, s: &Step) -> Checked<()> {
        match s {
            Step::Wait { ms } => {
                self.pass(*ms);
                Ok(())
            }
            Step::Repeat {
                call,
                times,
                gap_ms,
                r,
            } => {
                self.set_draws(r);
                for _ in 0..*times {
                    self.pass(*gap_ms);
                    self.ensure_active()?;
                    self.use_calls(std::slice::from_ref(call), None)?;
                    self.check()?;
                }
                Ok(())
            }
            Step::Jump { ms } => {
                let offset = self.shared.offset.load(Ordering::SeqCst);
                self.shared
                    .offset
                    .store((offset + ms).clamp(-3 * DAY, 3 * DAY), Ordering::SeqCst);
                Ok(())
            }
            Step::Net { mode } => {
                self.shared.lock().net = Net::parse(mode);
                Ok(())
            }
            Step::Disk { failing } => {
                self.shared.disk_failing.store(*failing, Ordering::SeqCst);
                Ok(())
            }
            Step::Status { p, force, r } => {
                self.set_draws(r);
                self.ensure_active()?;
                let running = self.launch_status(*p, *force);
                self.shared.settle(self.step_no)?;
                self.shared.drive(self.step_no)?;
                self.expect_status(&running)
            }
            Step::Deactivate { p, r } => {
                self.set_draws(r);
                self.ensure_active()?;
                self.deactivate(*p)
            }
            Step::Restart { p } => {
                self.restart(*p);
                Ok(())
            }
            Step::Use { calls, status, r } => {
                self.set_draws(r);
                self.ensure_active()?;
                self.use_calls(calls, *status)
            }
        }
    }

    fn launch_status(&self, p: usize, force: bool) -> Arc<Running> {
        self.shared.launch(p, move |license| {
            license.status(force);
            CallValue::Status
        })
    }

    fn expect_status(&self, running: &Running) -> Checked<()> {
        match running.take() {
            Ok(CallValue::Status) => Ok(()),
            Ok(_) => Err(self.fail("status-rejected", "a call that was not a status")),
            Err(panic) => Err(self.fail("status-rejected", panic)),
        }
    }

    /// Time passes, for the server and the device alike.
    fn pass(&self, ms: i64) {
        let now = self.shared.true_now.fetch_add(ms, Ordering::SeqCst) + ms;
        let mut inner = self.shared.lock();
        inner.server.now = inner.server.now.max(now);
    }

    /// A release drops the journal of the seat. The releasing process lands what it holds first, so the release carries
    /// its journal: the contract lets it drop an entry only when the server got none of it (no 200) or it is past the 64
    /// a report holds; a void it was meant to carry and did not, in any other case, is a fault. What another process held
    /// only in memory is lost with the seat.
    fn deactivate(&mut self, p: usize) -> Checked<()> {
        let journal = self.view_entries(p);
        let others: Vec<JournalOp> = (0..2)
            .filter(|i| *i != p)
            .flat_map(|i| self.held(i))
            .collect();
        self.shared.lock().releases.clear();
        let running = self
            .shared
            .launch(p, |license| CallValue::Deactivate(license.deactivate()));
        self.shared.settle(self.step_no)?;
        self.shared.drive(self.step_no)?;
        match running.take() {
            Ok(CallValue::Deactivate(Ok(_))) => {}
            Ok(CallValue::Deactivate(Err(error))) => {
                // It fails, the key kept, when the state cannot be saved; any other reason is a bug.
                if !self.shared.disk_failing.load(Ordering::SeqCst) {
                    return Err(self.fail("deactivate-rejected", error.to_string()));
                }
                return Ok(());
            }
            Ok(_) => return Err(self.fail("deactivate-rejected", "a call that was not a release")),
            Err(panic) => return Err(self.fail("deactivate-rejected", panic)),
        }
        let (carried, applied, recorded): (HashSet<String>, bool, HashMap<String, u64>) = {
            let inner = self.shared.lock();
            let carried = inner
                .releases
                .iter()
                .filter(|r| r.0)
                .flat_map(|r| r.1.iter().cloned())
                .collect();
            let recorded = inner
                .server
                .recorded
                .iter()
                .map(|(id, held)| (id.clone(), held.n))
                .collect();
            (carried, inner.releases.iter().any(|r| r.0), recorded)
        };
        for (index, e) in journal.iter().enumerate() {
            // The report holds the first 64 of the journal in the order the SDK keeps it: only what the server got none of (no 200) or what is past them may be dropped.
            let may_drop = !applied || index >= ENTRIES;
            if e.kind == SpendKind::Void {
                let voided = self.shared.lock().server.voided.contains(&e.id);
                if carried.contains(&e.id) || voided {
                    continue;
                }
                if !may_drop {
                    let who = self.lbl(&e.id);
                    return Err(self.fail(
                        "release-dropped-void",
                        format!("a release left a void of the journal behind: {who} is entry {index} of {} ({} carried)", journal.len(), carried.len()),
                    ));
                }
                self.shared.lock().lost_voids.insert(e.id.clone());
            } else if may_drop {
                // What the server holds of it already is not lost; the rest is, and the contract allows it.
                credit(
                    &mut self.loss_allowed,
                    &e.meter,
                    e.n.saturating_sub(recorded.get(&e.id).copied().unwrap_or(0)),
                );
            }
        }
        self.lose(&others);
        Ok(())
    }

    /// What a process held only in memory is lost with it: the spends are the customer's to have been told of.
    fn lose(&mut self, held: &[JournalOp]) {
        for op in held {
            if let JournalOp::Spend(spend) = op {
                if spend.kind == SpendKind::Void {
                    self.shared.lock().lost_voids.insert(spend.id.clone());
                } else {
                    credit(&mut self.loss_allowed, &spend.meter, spend.n);
                }
            }
        }
    }

    /// The app ends and starts again: what its process held only in memory is gone, and the customer was told it was made.
    fn restart(&mut self, p: usize) {
        let held = self.held(p);
        self.lose(&held);
        self.held_voids[p] = HashSet::new();
        let fresh = self.shared.process(p);
        self.shared.lock().procs[p] = fresh;
    }

    /// The seat is taken (on a good network and disk) when a release dropped it: the app activates again.
    fn ensure_active(&mut self) -> Checked<()> {
        if self.current().key.is_some() {
            return Ok(());
        }
        let (net, disk) = (
            self.shared.lock().net,
            self.shared.disk_failing.load(Ordering::SeqCst),
        );
        self.shared.lock().net = Net::Good;
        self.shared.disk_failing.store(false, Ordering::SeqCst);
        let running = self
            .shared
            .launch(0, |license| CallValue::Activate(license.activate(KEY)));
        self.shared.settle(self.step_no)?;
        self.shared.drive(self.step_no)?;
        match running.take() {
            Ok(CallValue::Activate(Ok(()))) => {}
            Ok(CallValue::Activate(Err(error))) => {
                return Err(self.fail("reactivation", error.to_string()))
            }
            Ok(_) => return Err(self.fail("reactivation", "a call that was not an activation")),
            Err(panic) => return Err(self.fail("reactivation", panic)),
        }
        self.shared.lock().net = net;
        self.shared.disk_failing.store(disk, Ordering::SeqCst);
        Ok(())
    }

    fn pre_of(&self, call: &Call) -> Pre {
        let state = self.proc(call.p).view().unwrap_or_default();
        let spends = entries_of(&state);
        let figure = base_figure(&state, &call.meter);
        let unsynced = unsynced_in(&spends, &call.meter);
        let clock_now = self.shared.clock_now();
        let guarded = clock_now.max(state.last_seen.unwrap_or(0));
        let left = figure.map_or(0, |(left, _)| left.saturating_sub(unsynced));
        Pre {
            licensed: state.key.is_some() && state.lease.is_some(),
            covered: figure
                .is_some_and(|(_, offline)| unsynced + call.n <= offline && call.n <= left),
            left_after: left as i64 - call.n as i64,
            entry_count: spends.len(),
            open_offline: spends
                .iter()
                .any(|e| e.kind == SpendKind::Offline && e.meter == call.meter),
            waiting: ask_wait(&state, clock_now) || journal_wait(&spends, guarded),
            void_waiting: void_wait(&spends, guarded, clock_now),
        }
    }

    fn use_calls(&mut self, calls: &[Call], status: Option<(usize, bool)>) -> Checked<()> {
        let concurrent = calls.len() > 1 || status.is_some();
        // Behind a call that has not finished a process saves nothing: what it spends offline is kept in memory, and told.
        let behind = calls
            .iter()
            .enumerate()
            .any(|(i, a)| calls.iter().enumerate().any(|(j, b)| i < j && a.p == b.p))
            || status.is_some_and(|(p, _)| calls.iter().any(|c| c.p == p));
        let risky = self.shared.disk_failing.load(Ordering::SeqCst) || behind;
        let pres: Vec<Pre> = calls.iter().map(|c| self.pre_of(c)).collect();
        for reqs in &mut self.shared.lock().use_reqs {
            reqs.clear();
        }
        let mut running = Vec::new();
        for call in calls {
            let (meter, n) = (call.meter.clone(), call.n);
            running.push(self.shared.launch(call.p, move |license| {
                CallValue::Use(license.use_meter(&meter, n))
            }));
            self.shared.settle(self.step_no)?;
        }
        let side = match status {
            Some((p, force)) => {
                let side = self.launch_status(p, force);
                self.shared.settle(self.step_no)?;
                Some(side)
            }
            None => None,
        };
        self.shared.drive(self.step_no)?;
        // What each process sees now: a spend after the first of two may meet a journal the first filled.
        let views: Vec<Vec<SpendEntry>> = (0..2).map(|p| self.view_entries(p)).collect();
        for (i, call) in calls.iter().enumerate() {
            let result = match running[i].take() {
                Ok(CallValue::Use(result)) => result,
                Ok(_) => return Err(self.fail("use-rejected", "a call that was not a spend")),
                Err(panic) => return Err(self.fail("use-rejected", panic)),
            };
            let reqs: Vec<Req> = self.shared.lock().use_reqs[call.p]
                .iter()
                .filter(|q| q.meter == call.meter)
                .cloned()
                .collect();
            if self.tracing() {
                self.note(format!(
                    "  p{} use {} {} => {result:?}",
                    call.p, call.meter, call.n
                ));
            }
            self.judge(
                call,
                &pres[i],
                result,
                &reqs,
                concurrent,
                risky,
                &views[call.p],
            )?;
        }
        if let Some(side) = side {
            self.expect_status(&side)?;
        }
        Ok(())
    }

    /// What the contract says of an answer, and the ledger of what the customer was told.
    #[allow(clippy::too_many_arguments)]
    fn judge(
        &mut self,
        call: &Call,
        pre: &Pre,
        res: UseResult,
        reqs: &[Req],
        concurrent: bool,
        risky: bool,
        view: &[SpendEntry],
    ) -> Checked<()> {
        let asked = !reqs.is_empty();
        let alone = !concurrent && pre.licensed;
        let (n, meter) = (call.n, call.meter.as_str());
        if alone && !pre.covered && pre.void_waiting && asked {
            return Err(self.fail(
                "uncovered-asked-in-wait",
                format!("{n} of {meter} asked the server while a void under a minute old stands"),
            ));
        }
        if alone && pre.covered && asked == pre.waiting {
            let what = if asked {
                "asked the server while a wait runs"
            } else {
                "did not ask the server though no wait runs"
            };
            return Err(self.fail("covered-ask-rule", format!("{n} of {meter} {what}")));
        }
        if res.ok {
            // What is left is never told high. Made offline it is the figure less the unsynced, less the spend; online the server's own is at most what it has.
            let left = res.left.unwrap_or(0);
            if alone && res.offline && left as i64 != pre.left_after {
                return Err(self.fail("left-wrong", format!("{n} of {meter} made offline answered {left} left, the figure and the unsynced say {}", pre.left_after)));
            }
            let server_left = self.shared.lock().server.left(meter);
            if alone && !res.offline && left > server_left {
                return Err(self.fail(
                    "left-high",
                    format!("{n} of {meter} answered {left} left, the server has {server_left}"),
                ));
            }
            credit(&mut self.acked, meter, n);
            if res.offline && risky {
                credit(&mut self.told, meter, n);
                self.told_ops += 1;
            }
            return Ok(());
        }
        // A void finds no room when the journal, less the oldest void if sixteen stand and the new one takes its place, still holds the cap; one that landed, or is held, found its room.
        let crowded = pre.entry_count.max(view.len())
            - usize::from(view.iter().filter(|e| e.kind == SpendKind::Void).count() >= VOIDS)
            >= ENTRIES;
        let held: Vec<JournalOp> = (0..2).flat_map(|p| self.held(p)).collect();
        let placed = |id: &str| {
            view.iter().any(|e| e.id == id) || held.iter().any(|op| matches!(op, JournalOp::Spend(s) if s.kind == SpendKind::Void && s.id == id))
        };
        // A process's requests of one meter are not told apart by call, so a spend the customer was told was made (another call of the step, asked and
        // unanswered, then kept as a plain entry or reported as one) is not a refused spend whose void is owed.
        let reported = self.shared.lock().reported_plain.clone();
        let told_id = |id: &str| {
            reported.contains(id)
                || view.iter().any(|e| e.id == id && e.kind != SpendKind::Void)
                || held.iter().any(|op| matches!(op, JournalOp::Spend(s) if s.kind != SpendKind::Void && s.id == id))
        };
        for q in reqs {
            if q.applied && !q.answered && !told_id(&q.id) {
                self.phantoms.push(Phantom {
                    id: q.id.clone(),
                    meter: q.meter.clone(),
                    n: q.n,
                    full: crowded && !placed(&q.id),
                });
            }
        }
        if concurrent
            || !pre.licensed
            || (res.error != Some(UseError::OfflineLimit) && res.error != Some(UseError::NoMeter))
        {
            return Ok(());
        }
        if pre.covered && res.error == Some(UseError::OfflineLimit) {
            // A covered spend is refused only for the journal's size: it would open a 65th entry.
            let capped = pre.entry_count >= ENTRIES && (asked || !pre.open_offline);
            if !capped {
                return Err(self.fail(
                    "covered-refused",
                    format!(
                        "{n} of {meter} was covered by the allowance and refused (journal of {})",
                        pre.entry_count
                    ),
                ));
            }
        }
        if !pre.covered && !asked && !pre.void_waiting {
            return Err(self.fail(
                "uncovered-unasked",
                format!("{n} of {meter} was refused without asking, and no void wait runs"),
            ));
        }
        Ok(())
    }

    fn describe(&self) {
        let show = |e: &SpendEntry| {
            format!(
                "{}:{}:{}:{}",
                e.kind.as_str(),
                self.lbl(&e.id),
                e.meter,
                e.n
            )
        };
        let show_op = |op: &JournalOp| match op {
            JournalOp::Confirmed { entries, .. } => format!("confirmed:{}", entries.len()),
            JournalOp::Figures { .. } => "figures".to_owned(),
            JournalOp::Activated { .. } => "activated".to_owned(),
            JournalOp::Deactivated => "deactivated".to_owned(),
            JournalOp::Spend(s) => format!(
                "{}{}:{}:{}:{}",
                s.kind.as_str(),
                if s.told { "!" } else { "" },
                self.lbl(&s.id),
                s.meter,
                s.n
            ),
        };
        let stored: Vec<String> = entries_of(&self.current()).iter().map(show).collect();
        self.note(format!("  stored [{}]", stored.join(" ")));
        for p in 0..2 {
            let held: Vec<String> = self.held(p).iter().map(show_op).collect();
            self.note(format!("  p{p} holds [{}]", held.join(" ")));
        }
    }

    // --- what must hold between steps, and at the end ---------------------------------------------

    /// A void held in memory that is gone, though it neither landed nor reached the server: sixteen were held and the oldest went, or the journal found no room for it.
    fn account_voids(&mut self) -> Checked<()> {
        let stored = entries_of(&self.current());
        for p in 0..2 {
            let now: HashSet<String> = self
                .held(p)
                .into_iter()
                .filter_map(|op| match op {
                    JournalOp::Spend(s) if s.kind == SpendKind::Void => Some(s.id),
                    _ => None,
                })
                .collect();
            let before = self.held_voids[p].clone();
            for id in &before {
                let (voided, lost, refused) = {
                    let inner = self.shared.lock();
                    (
                        inner.server.voided.contains(id),
                        inner.lost_voids.contains(id),
                        inner.refused_voids.contains(id),
                    )
                };
                if now.contains(id) || stored.iter().any(|e| e.id == *id) || voided || lost {
                    continue;
                }
                if before.len() >= VOIDS || refused {
                    self.shared.lock().lost_voids.insert(id.clone());
                } else {
                    let who = self.lbl(id);
                    return Err(self.fail(
                        "void-dropped",
                        format!(
                            "a void held in memory is gone: {who} ({} held, {} stored)",
                            before.len(),
                            stored.len()
                        ),
                    ));
                }
            }
            self.held_voids[p] = now;
        }
        Ok(())
    }

    /// The phantoms the server recorded and no void has taken back.
    fn outstanding(&self) -> Vec<(String, String, u64, bool)> {
        let inner = self.shared.lock();
        self.phantoms
            .iter()
            .filter(|x| {
                inner.server.recorded.contains_key(&x.id) && !inner.server.voided.contains(&x.id)
            })
            .map(|x| (x.id.clone(), x.meter.clone(), x.n, x.full))
            .collect()
    }

    fn check(&mut self) -> Checked<()> {
        if let Some(violation) = self.shared.lock().violation.clone() {
            return Err(self.fail("void-replaced", violation));
        }
        let stored = entries_of(&self.current());
        let distinct: HashSet<&str> = stored.iter().map(|e| e.id.as_str()).collect();
        if distinct.len() != stored.len() {
            return Err(self.fail("duplicate-id", "the journal holds one id twice"));
        }
        if stored.len() > ENTRIES + self.told_ops {
            return Err(self.fail(
                "journal-size",
                format!(
                    "{} entries stored, past the cap and what was told",
                    stored.len()
                ),
            ));
        }
        if stored.iter().filter(|e| e.kind == SpendKind::Void).count() > VOIDS {
            return Err(self.fail("void-cap", "more voids stored than the cap"));
        }
        for (m, spec) in &self.case.meters {
            let unsynced = unsynced_in(&stored, m);
            let told = self.told[m];
            if unsynced > spec.offline + told {
                return Err(self.fail(
                    "allowance",
                    format!(
                        "{unsynced} of {m} unsynced past its allowance {} and the {told} told",
                        spec.offline
                    ),
                ));
            }
        }
        let mut holding = false;
        for p in 0..2 {
            let held = self.held(p);
            let voids = held
                .iter()
                .filter(|op| matches!(op, JournalOp::Spend(s) if s.kind == SpendKind::Void))
                .count();
            if voids > VOIDS {
                return Err(self.fail(
                    "unsaved-voids",
                    "a process holds more voids in memory than the cap",
                ));
            }
            if held
                .iter()
                .any(|op| matches!(op, JournalOp::Spend(s) if s.kind == SpendKind::Void && s.told))
            {
                return Err(self.fail("void-told", "a void was marked told"));
            }
            if held.len() > ENTRIES + VOIDS + 16 {
                return Err(self.fail(
                    "unsaved-size",
                    format!("{} changes held in memory", held.len()),
                ));
            }
            holding |= !held.is_empty();
        }
        self.account_voids()?;
        // The server never holds more than the customer was told and the refusals it recorded: a spend counted twice shows here at once.
        let outstanding = self.outstanding();
        for m in self.case.meters.keys() {
            let phantom: u64 = outstanding.iter().filter(|x| x.1 == *m).map(|x| x.2).sum();
            let used = self.shared.lock().server.used(m);
            if used > self.acked[m] + phantom {
                return Err(self.fail("counted-twice", format!("{m}: the server holds {used}, the customer was told {} (and {phantom} refused but recorded)", self.acked[m])));
            }
        }
        // What was told while only memory held it excuses the allowance only while it is unreported: once a meter's journal drains, the loosening ends.
        let held_all: Vec<JournalOp> = (0..2).flat_map(|p| self.held(p)).collect();
        for m in self.case.meters.keys() {
            let held_here = held_all.iter().any(|op| matches!(op, JournalOp::Spend(s) if s.kind != SpendKind::Void && s.meter == *m));
            if unsynced_in(&stored, m) == 0 && !held_here {
                self.told.insert(m.clone(), 0);
            }
        }
        if stored.is_empty() && !holding {
            self.told_ops = 0;
        }
        Ok(())
    }

    /// The network and the disk come back, time passes, and every journal is reported: then the books are balanced.
    fn converge(&mut self) -> Checked<()> {
        {
            let mut inner = self.shared.lock();
            inner.net = Net::Good;
            inner.rs = vec![0.0];
            inner.rk = 0;
        }
        self.shared.disk_failing.store(false, Ordering::SeqCst);
        let now = self.shared.true_now.fetch_add(3 * HOUR, Ordering::SeqCst) + 3 * HOUR;
        {
            let mut inner = self.shared.lock();
            inner.server.now = inner.server.now.max(now);
        }
        self.ensure_active()?;
        // A journal past 64 entries is reported 64 at a time, so more than one check-in each may be needed.
        let mut round = 0;
        while round < 4
            && (round == 0
                || !entries_of(&self.current()).is_empty()
                || (0..2).any(|p| !self.held(p).is_empty()))
        {
            for p in 0..2 {
                let running = self.launch_status(p, true);
                self.shared.settle(self.step_no)?;
                self.shared.drive(self.step_no)?;
                self.expect_status(&running)?;
                self.account_voids()?;
            }
            round += 1;
        }
        if let Some(violation) = self.shared.lock().violation.clone() {
            return Err(self.fail("void-replaced", violation));
        }
        let left_over = entries_of(&self.current());
        if !left_over.is_empty() {
            let shown: Vec<String> = left_over
                .iter()
                .map(|e| format!("{}:{}", e.kind.as_str(), self.lbl(&e.id)))
                .collect();
            return Err(self.fail(
                "journal-left",
                format!(
                    "the journal still holds {} after every process reported",
                    shown.join(" ")
                ),
            ));
        }
        if (0..2).any(|p| !self.held(p).is_empty()) {
            return Err(self.fail("unsaved-left", "a process still holds changes in memory"));
        }
        // A refusal the server recorded is taken back, unless that very void was lost as the contract allows: replaced by a newer one, dropped with a release, held by a process that ended, or never given room.
        for (id, meter, n, full) in self.outstanding() {
            let lost = self.shared.lock().lost_voids.contains(&id);
            if !full && !lost {
                let who = self.lbl(&id);
                return Err(self.fail("void-lost", format!("{meter}: {n} refused after the server recorded it ({who}), and its void did not reach the server though nothing lost it")));
            }
        }
        let shown: Vec<LicenseStatus> = self.procs().iter().map(|p| p.status(false)).collect();
        let outstanding = self.outstanding();
        for (m, spec) in &self.case.meters {
            let used = self.shared.lock().server.used(m);
            let owed: u64 = outstanding.iter().filter(|x| x.1 == *m).map(|x| x.2).sum();
            let (acked, lost) = (self.acked[m], self.loss_allowed[m]);
            if used + lost < (acked + owed) {
                return Err(self.fail("lost", format!("{m}: the server holds {used}, the customer was told {acked} (and {owed} refused but recorded)")));
            }
            if used > acked + owed {
                return Err(self.fail("counted-twice", format!("{m}: the server holds {used}, the customer was told {acked} (and {owed} refused but recorded)")));
            }
            let left = spec.total.saturating_sub(used);
            for status in &shown {
                let figure = status.meters.get(m);
                if figure.is_none_or(|f| f.left != left || f.unsynced != 0) {
                    return Err(self.fail(
                        "figures",
                        format!("{m}: status shows {figure:?}, the server has {left} left"),
                    ));
                }
            }
        }
        Ok(())
    }
}
