//! Spending a key's meters under `License`: `use_meter` asks the server to
//! spend (exact, atomic, idempotent by the spend's id), and when it gets no
//! answer spends from the device's offline allowance, keeping the spend in the
//! journal (`engine/journal.rs`) to report with the next request.
//!
//! A meter never takes a licence away, and licensing fails in favour of the
//! customer: no answer is never a refusal of the licence, only of a spend past
//! the allowance, and the journal never fails a renewal.

use std::panic::{catch_unwind, AssertUnwindSafe};

use serde_json::Value;

use super::License;
use crate::engine::{Holder, View};
use crate::journal::{land_journal, JournalOp, SpendOp};
use crate::lease::LeaseClaims;
use crate::meters::{
    asked_for, figures_for, left_of, new_spend_id, offline_spend, spends_reported, use_asks_now,
    use_outcome, OfflineVerdict, SpendKind, UseError, UseInput, UseOutcome, UseOutcomeKind,
    UseResult,
};
use crate::offline::{Asked, Basis, LeaseCheck};
use crate::state::{present, Patch, StoredState};
use crate::status::{LicenseSource, LicenseState};

/// One call to `use_meter`, as the steps below read it.
struct Spending<'a> {
    /// The state the call read, with what this process could not save landed on it.
    view: View,
    /// The claims of the lease that licenses this build.
    claims: LeaseClaims,
    meter: &'a str,
    n: u64,
    /// The activation the spend is about.
    about: String,
    /// The spend's id.
    id: String,
    /// When (the guarded clock) the spend is made.
    at: i64,
    /// Behind a call that has not finished: nothing is saved.
    read_only: bool,
    /// What the offline allowance says of the spend as the call read the state.
    verdict: OfflineVerdict,
}

/// What asking the server came to.
enum Ask {
    /// The server answered: this is the result.
    Answered(UseResult),
    /// No usable answer: the spend was sent, and the server may hold it.
    Doubt,
}

impl License {
    /// Spend `n` uses of the key's meter `meter`, before the work they count; do
    /// the work only when the result is `ok`.
    ///
    /// Online it is exact: the server takes `n` only while the meter has room
    /// for all of it ([`UseError::UsedUp`], with `left`, otherwise). With no
    /// answer (no network, a timeout, a 5xx, a 429 or an answer the SDK cannot
    /// use) it spends from this device's offline allowance for the meter, never
    /// past what the device last heard is left ([`UseError::OfflineLimit`]),
    /// answers `offline: true`, and reports the spend with its next request. A
    /// spend the allowance would refuse asks the server first, whatever the last
    /// outage left behind, so a customer who is back online is not refused for
    /// it (once a minute while KeyGrant stays out of reach); one the allowance
    /// covers is not asked again for a minute after an outage, growing to an
    /// hour.
    ///
    /// Needs a key's lease that licenses this build here
    /// ([`UseError::NotLicensed`] otherwise, asking nothing); a device trial and
    /// an offline licence file have no meters. `n` from 1 to 1,000,000,000
    /// ([`UseError::InvalidAmount`] otherwise) and a meter code of 1 to 32
    /// UTF-16 units, matched exactly ([`UseError::NoMeter`] otherwise), both
    /// without asking anything. Two calls are two spends: the spend's id covers
    /// the SDK's own retries and reports, never an app that calls twice. A
    /// platform with no secure random source for the id answers
    /// [`UseError::Storage`] (try again), asking nothing.
    ///
    /// Blocks, the network included (a `use` request waits at most 5 seconds, or
    /// the HTTP bound when that is shorter), and runs to its bounded answer. It
    /// never panics for an outcome: every one is in the result, behind a call
    /// that has not finished too, and what no adapter can tell it (one panicked
    /// where there is no word for it) is [`UseError::Storage`]. Calls on one
    /// `License` run one at a time, this one among them.
    pub fn use_meter(&self, meter: &str, n: u64) -> UseResult {
        let (meter, n) = match asked_for(meter, n) {
            Ok(asked) => asked,
            Err(error) => return UseResult::refused(error, None),
        };
        // Nothing an adapter does may fail a spend: the customer is told to try
        // again shortly.
        catch_unwind(AssertUnwindSafe(|| {
            self.engine
                .serial(|read_only| self.spend(meter, n, read_only))
        }))
        .unwrap_or_else(|_| UseResult::refused(UseError::Storage, None))
    }

    /// The steps of a spend: the state and the licence, then the server when it
    /// should be asked, then the allowance.
    fn spend(&self, meter: &str, n: u64, read_only: bool) -> UseResult {
        if !read_only {
            self.engine.save_unsaved_journal();
        }
        let storage = || UseResult::refused(UseError::Storage, None);
        let Some(view) = self.load_for_status(read_only) else {
            return storage();
        };
        let (Some(claims), Some(about)) = (self.key_claims(&view), view.activation_id.clone())
        else {
            return UseResult::refused(UseError::NotLicensed, None);
        };
        let at = self.engine.effective_now(&view);
        let spends = view.spends.as_deref().unwrap_or_default();
        let verdict = offline_spend(&figures_for(&claims, &view), spends, meter, n);
        let Some(id) = new_spend_id() else {
            return storage();
        };
        let covered = matches!(verdict, OfflineVerdict::Allowed { .. });
        let asks = use_asks_now(&view, self.engine.clock.now(), at, covered);
        let spending = Spending {
            view,
            claims,
            meter,
            n,
            about,
            id,
            at,
            read_only,
            verdict,
        };
        let mut sent = false;
        if asks {
            match self.ask_to_spend(&spending) {
                Ask::Answered(result) => return result,
                Ask::Doubt => sent = true,
            }
        }
        self.spend_offline(&spending, sent)
    }

    /// The claims of the key's lease when the state licenses this build here by
    /// a key's lease: what `status()` would give as licensed from what is stored
    /// (no ask, no write), and not by a licence file. None otherwise.
    fn key_claims(&self, view: &View) -> Option<LeaseClaims> {
        let state: &StoredState = view;
        let status = self.status_now(view, false, true);
        let lease = present(&state.lease)?;
        if status.state != LicenseState::Licensed || status.source == Some(LicenseSource::File) {
            return None;
        }
        let now = self.engine.effective_now(state);
        match self.engine.evaluate(lease, now, &Holder::of(state)) {
            LeaseCheck::Ok { claims, .. } => Some(*claims),
            LeaseCheck::Fail(_) => None,
        }
    }

    /// Ask the server to spend (`use`, bounded by the status check's bound, 5
    /// seconds, or the HTTP bound when shorter), with the journal's report. Its
    /// answer is saved as a validate's is (`use_outcome`) unless this call is
    /// behind one that has not finished; with no usable answer the spend is in
    /// doubt: the server may hold it.
    fn ask_to_spend(&self, sp: &Spending<'_>) -> Ask {
        let state: &StoredState = &sp.view;
        let asked_at = self.engine.clock.now();
        let asked = Asked {
            at: sp.at,
            activation_id: state.activation_id.clone(),
            seen: state.verdict_at,
        };
        let device = self
            .engine
            .device(self.engine.timings.machine_id, state.device_kind);
        let mut body = self.engine.device_body(state, &device);
        body.insert("meter".into(), Value::from(sp.meter));
        body.insert("n".into(), Value::from(sp.n));
        body.insert("id".into(), Value::from(sp.id.clone()));
        if let Some(reports) = spends_reported(&self.engine.reportable(&sp.view)) {
            body.insert("spends".into(), reports);
        }
        body.insert("kids".into(), self.engine.kids());
        let response = self
            .engine
            .post("use", Value::Object(body), Some(self.engine.use_timeout()))
            .ok();
        let outcome = use_outcome(&UseInput {
            response: response.as_ref(),
            asked_at,
            major: self.engine.major,
            device_kind: device.kind,
            own_claim: device.own_claim.as_deref(),
        });
        let ops = journal_ops(state, &outcome);
        // A verdict or a wait is the answer to an ask, stamped as a validate's
        // is; figures and confirmations are not a verdict, so a spend never makes
        // a validate another process asked for earlier look out of date.
        let answer = (!outcome.patch.is_empty()).then_some(Basis {
            answer: Some(asked),
            ..Basis::default()
        });
        if !sp.read_only && (!ops.is_empty() || answer.is_some()) {
            let basis = answer.unwrap_or_default();
            self.engine.save_ops(state, &outcome.patch, &basis, &ops);
        }
        if outcome.kind == UseOutcomeKind::Unanswered {
            return Ask::Doubt;
        }
        let after = land_journal(state, &ops, sp.at).state;
        Ask::Answered(answered(&outcome, sp, &after))
    }

    /// With no answer (or none asked): spend from the allowance
    /// (`offline_spend`). Allowed, the spend is journaled, `Offline` when it was
    /// never sent and `Unanswered` when it was in doubt, with the most the
    /// meter's unsynced spends may come to (`limit`), which the landing judges
    /// again on the state as stored then, so two processes never pass the
    /// allowance together. Refused, a spend in doubt is journaled as a `Void`,
    /// for the server to take it back off if it recorded it, and so is one the
    /// landing refuses (the journal full, or the allowance taken by another
    /// process meanwhile, answered with what is left as the landing found it):
    /// the customer is told `OfflineLimit`, the server is told the spend is
    /// taken back. Voids are kept to `MAX_PENDING_VOIDS`, a new one taking the
    /// place of the oldest, and a void a full journal cannot take is lost.
    fn spend_offline(&self, sp: &Spending<'_>, doubt: bool) -> UseResult {
        let entry = |kind: SpendKind, limit: Option<u64>| {
            JournalOp::Spend(SpendOp {
                kind,
                about: sp.about.clone(),
                id: sp.id.clone(),
                meter: sp.meter.to_owned(),
                n: sp.n,
                at: sp.at,
                limit,
                told: false,
            })
        };
        let (left, limit) = match sp.verdict {
            OfflineVerdict::Allowed { left, limit } => (left, limit),
            OfflineVerdict::NoMeter => {
                if doubt {
                    self.journal(sp, &[entry(SpendKind::Void, None)]);
                }
                return UseResult::refused(UseError::NoMeter, None);
            }
            OfflineVerdict::OfflineLimit { left } => {
                if doubt {
                    self.journal(sp, &[entry(SpendKind::Void, None)]);
                }
                return UseResult::refused(UseError::OfflineLimit, Some(left));
            }
        };
        let kind = if doubt {
            SpendKind::Unanswered
        } else {
            SpendKind::Offline
        };
        if self.journal(sp, &[entry(kind, Some(limit))]).first() == Some(&true) {
            if doubt {
                self.journal(sp, &[entry(SpendKind::Void, None)]);
            }
            return UseResult::refused(UseError::OfflineLimit, Some(self.landed_left(sp)));
        }
        UseResult::spent(left, true)
    }

    /// What is left of the meter by the state as a refused landing found it
    /// (another process had spent meanwhile): the figure less the unsynced spends
    /// there then, never below 0, and 0 when the state cannot be read just now.
    fn landed_left(&self, sp: &Spending<'_>) -> u64 {
        self.load_for_status(sp.read_only)
            .map_or(0, |fresh| left_of(&sp.claims, &fresh, sp.meter))
    }

    /// `ops` saved, or kept in memory when this call may save nothing or the
    /// disk refuses. Whether each was refused.
    fn journal(&self, sp: &Spending<'_>, ops: &[JournalOp]) -> Vec<bool> {
        if sp.read_only {
            self.engine.keep_unsaved(&sp.view, ops)
        } else {
            self.engine
                .save_ops(&sp.view, &Patch::default(), &Basis::default(), ops)
        }
    }

    /// The journal changes this process holds in memory because a save failed.
    /// For the tests; not API.
    #[doc(hidden)]
    pub fn held_changes(&self) -> Vec<JournalOp> {
        self.engine.held_changes()
    }

    /// The stored state as this process sees it: the stored one with what it
    /// could not save landed on it; none when it cannot be read just now. For
    /// the tests; not API.
    #[doc(hidden)]
    pub fn view(&self) -> Option<StoredState> {
        self.engine.load(true).ok()
    }
}

/// The journal changes an answer makes: the reports it confirms and the figures
/// it gives, about the activation asked for.
fn journal_ops(state: &StoredState, outcome: &UseOutcome) -> Vec<JournalOp> {
    let Some(about) = &state.activation_id else {
        return Vec::new();
    };
    let mut ops = Vec::new();
    if !outcome.confirmed.is_empty() {
        ops.push(JournalOp::Confirmed {
            about: about.clone(),
            entries: outcome.confirmed.clone(),
        });
    }
    if !outcome.figures.is_empty() {
        ops.push(JournalOp::Figures {
            about: about.clone(),
            figures: outcome.figures.clone(),
        });
    }
    ops
}

/// What `use_meter` answers for an answer the server gave. A spend: what is left
/// of the meter by the figures as the answer left them (`after`: the base
/// figure, its own from the answer when it gave one, less what this device has
/// not reported). `used-up`: the answer's `left`. A verdict: the refusal, as
/// `status()` will say it.
fn answered(outcome: &UseOutcome, sp: &Spending<'_>, after: &StoredState) -> UseResult {
    match outcome.kind {
        UseOutcomeKind::Spent => UseResult::spent(left_of(&sp.claims, after, sp.meter), false),
        UseOutcomeKind::UsedUp => UseResult::refused(UseError::UsedUp, outcome.left),
        UseOutcomeKind::NoMeter => UseResult::refused(UseError::NoMeter, None),
        UseOutcomeKind::Refused | UseOutcomeKind::Unanswered => UseResult::refused(
            outcome
                .refusal
                .map_or(UseError::NotLicensed, UseError::of_refusal),
            None,
        ),
    }
}
