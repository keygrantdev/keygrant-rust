//! A running trial's launch, reported so the server sees the trial come back:
//! its funnel counts a trial seen a day after it started, and a running trial
//! otherwise reaches the server only when it asks. At most once a day
//! ([`trial_report_due`]), on a thread of its own that no call waits on.

use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use serde_json::{Map, Value};

use crate::engine::Engine;
use crate::license::License;
use crate::offline::{Basis, DAY_MS};
use crate::state::{present, Patch, StoredState};
use crate::status::DeviceKind;

/// How often, at most, a running trial reports its launch (`trial_report_due`,
/// ms), a trial answer (`trialAt`) counting as one.
pub const TRIAL_REPORT_MS: i64 = DAY_MS;

/// Whether a running trial reports its launch at `now` (the guarded clock):
/// neither its last trial answer (`trialAt`, a `start_trial()` or a heal)
/// nor its last report (`trialReportedAt`) is within [`TRIAL_REPORT_MS`]
/// before `now`. A stamp that is absent, or at least that long before `now`,
/// is not within it; one ahead of `now` is.
pub fn trial_report_due(state: &StoredState, now: i64) -> bool {
    let recent = |at: Option<i64>| at.is_some_and(|at| now.saturating_sub(at) < TRIAL_REPORT_MS);
    !recent(state.trial_at) && !recent(state.trial_reported_at)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl License {
    /// A running trial's launch, reported: with no key held, at most once a
    /// day (`trial_report_due`). The stamp (`trialReportedAt`) is saved
    /// first, and nothing is sent when it cannot be, so "once a day" holds;
    /// then `trial` is POSTed on a thread of its own, never waited on, so a
    /// launch never waits on it and never fails for it offline
    /// ([`send_launch`]). Its answer is not read: a change to the trial
    /// reaches the device at `start_trial()`.
    pub(crate) fn report_launch(&self, state: &StoredState, now: i64) {
        if present(&state.key).is_some() || !trial_report_due(state, now) {
            return;
        }
        let stamp = Patch {
            trial_reported_at: Some(Some(now)),
            ..Patch::default()
        };
        if !self
            .engine
            .save_best_effort(&stamp, None, &Basis::default())
        {
            return;
        }
        let engine = Arc::clone(&self.engine);
        let install_kind = state.device_kind;
        let client_start = state.trial_start;
        let spawned = thread::Builder::new()
            .name("keygrant-launch-report".into())
            .spawn(move || send_launch(&engine, install_kind, client_start));
        // No thread for it: nothing is sent, as when the request fails.
        if let Ok(report) = spawned {
            *lock(&self.launch_report) = Some(report);
        }
    }

    /// Returns once the launch report in flight, if any, has ended. Test
    /// support, not part of the API: the SDK itself never waits on a report.
    #[doc(hidden)]
    pub fn launch_report_settled(&self) {
        let report = lock(&self.launch_report).take();
        if let Some(report) = report {
            let _ = report.join();
        }
    }
}

/// The report itself: the device read for a status's time, then `trial` with
/// the body `start_trial()` sends (the stored start as `clientStart`, no
/// stash read), bounded as a status check is. A device that cannot be told
/// sends nothing; every failure is ignored.
fn send_launch(engine: &Engine, install_kind: Option<DeviceKind>, client_start: Option<i64>) {
    let device = engine.device(engine.timings.machine_id, install_kind);
    let Some(own) = device.own else {
        return;
    };
    let mut body = Map::new();
    body.insert("fingerprint".into(), Value::from(own));
    if let Some(kind) = device.kind {
        body.insert("fingerprintKind".into(), Value::from(kind.as_str()));
    }
    if let Some(start) = client_start {
        body.insert("clientStart".into(), Value::from(start));
    }
    let _ = engine.post(
        "trial",
        Value::Object(body),
        Some(engine.timings.status_check),
    );
}
