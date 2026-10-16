//! The journal of unsynced spends under the engine: what this process could not
//! save is kept in memory, counts everywhere the stored state is read, and is
//! saved with the next save that can, as a trial the disk refused is kept
//! (`trials.rs`). A process that ends first loses the report, never the
//! customer's use.

use std::io;
use std::ops::Deref;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Mutex, MutexGuard};

use super::{panicked, Engine};
use crate::journal::{bounded_voids, compacted, land_journal, JournalOp};
use crate::meters::{SpendEntry, SpendKind};
use crate::offline::{guarded_now, merged, saved_last_seen, Basis};
use crate::state::{Patch, StoredState};

/// Journal changes this process could not save: the disk refused them, or the
/// call was behind one that has not finished.
#[derive(Default)]
pub(crate) struct Held {
    /// The changes, in the order they were made.
    ops: Vec<JournalOp>,
    /// Saves in flight: while one is, `ops` is only added to, never folded
    /// (`Engine::keep_unsaved`).
    saving: usize,
}

/// The stored state as this process sees it: the stored one with what it could
/// not save landed on it. `carried`: a held spend is in `spends`, so a request
/// must not report them as they are (`Engine::reportable`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct View {
    pub state: StoredState,
    pub carried: bool,
}

impl View {
    /// A view that carries nothing held.
    pub fn plain(state: StoredState) -> View {
        View {
            state,
            carried: false,
        }
    }

    /// The same view of `state`: what it carries is unchanged.
    pub fn with(&self, state: StoredState) -> View {
        View {
            state,
            carried: self.carried,
        }
    }
}

impl Deref for View {
    type Target = StoredState;

    fn deref(&self) -> &StoredState {
        &self.state
    }
}

impl Engine {
    fn held(&self) -> MutexGuard<'_, Held> {
        self.held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The journal changes this process holds because a save failed.
    pub fn held_changes(&self) -> Vec<JournalOp> {
        self.held().ops.clone()
    }

    /// The stored state, as the storage has it.
    ///
    /// A storage adapter that panics is a state that cannot be read: an error,
    /// which `status()` and `start_trial()` answer as `Unknown`.
    pub fn stored(&self, read_only: bool) -> io::Result<StoredState> {
        let storage = &self.storage;
        let read = catch_unwind(AssertUnwindSafe(|| storage.read(read_only)))
            .unwrap_or_else(|_| Err(panicked("read")));
        Ok(read?.unwrap_or_default())
    }

    /// The stored state with the journal changes this process could not save
    /// landed on it, as they will be; `read_only` behind a call that has not
    /// finished, so the read writes nothing either.
    pub fn view(&self, read_only: bool) -> io::Result<View> {
        let state = self.stored(read_only)?;
        let held = self.held_changes();
        if held.is_empty() {
            return Ok(View::plain(state));
        }
        let landed = land_journal(
            &state,
            &held,
            guarded_now(self.clock.now(), state.last_seen),
        )
        .state;
        let carried = landed.spends.is_some();
        Ok(View {
            state: landed,
            carried,
        })
    }

    /// [`view`](Engine::view)'s state alone.
    pub fn load(&self, read_only: bool) -> io::Result<StoredState> {
        self.view(read_only).map(|view| view.state)
    }

    /// The journal a request reports for `view` (the state a call read): the
    /// journal as stored, with what this process could not save that opens an
    /// entry of its own, once, by its id: an `Unanswered` spend and a `Void`. A
    /// spend held in memory that was made offline is withheld: it joins the
    /// entry of another when it lands, so a report of it under its own id would
    /// be counted a second time when the entry it joined is reported. It is
    /// reported once it has landed, which the next call that can save does. The
    /// entries the server has since confirmed are left out: there is no need to
    /// say again what it holds. A view that carried held spends is judged the
    /// same, though they may have landed since.
    pub fn reportable(&self, view: &View) -> Vec<SpendEntry> {
        let held = self.held_changes();
        if !view.carried && held.is_empty() {
            return view.spends.clone().unwrap_or_default();
        }
        let Ok(stored) = self.stored(true) else {
            return Vec::new();
        };
        if stored.activation_id != view.activation_id {
            return Vec::new();
        }
        let reportable: Vec<JournalOp> = held
            .into_iter()
            .filter(|op| match op {
                JournalOp::Spend(spend) => spend.kind != SpendKind::Offline,
                JournalOp::Confirmed { .. } => true,
                _ => false,
            })
            .collect();
        let now = guarded_now(self.clock.now(), stored.last_seen);
        land_journal(&stored, &reportable, now)
            .state
            .spends
            .unwrap_or_default()
    }

    /// Persist a patch onto the state as it is stored NOW, merged by what it is
    /// based on, and advance the clock guard (or set it to `guard`).
    pub fn save(&self, patch: &Patch, guard: Option<i64>, basis: &Basis) -> io::Result<()> {
        self.save_with(patch, guard, basis, &[]).map(|_| ())
    }

    /// [`save`](Engine::save) with changes to the journal (`ops`), landed on the
    /// state as stored now, after the ones this process could not save before.
    /// Whether each of `ops` was refused, the journal being full or its limit
    /// forbidding it. Fails when the save fails; the changes stay held until
    /// one lands.
    pub fn save_with(
        &self,
        patch: &Patch,
        guard: Option<i64>,
        basis: &Basis,
        ops: &[JournalOp],
    ) -> io::Result<Vec<bool>> {
        let (before, all) = {
            let mut held = self.held();
            let before = held.ops.len();
            let mut all = held.ops.clone();
            all.extend_from_slice(ops);
            held.saving += 1;
            (before, all)
        };
        let refused = Mutex::new(Vec::new());
        let merge = |latest: StoredState| {
            let clock_now = self.clock.now();
            let landed = land_journal(&latest, &all, guarded_now(clock_now, latest.last_seen));
            *refused.lock().unwrap_or_else(|p| p.into_inner()) = landed.refused;
            let mut next = merged(&landed.state, patch, basis);
            next.last_seen = Some(saved_last_seen(latest.last_seen, clock_now, guard));
            next
        };
        // A storage adapter that panics is a save that failed.
        let saved = catch_unwind(AssertUnwindSafe(|| {
            if let Some(result) = self.storage.update(&merge) {
                return result;
            }
            let latest = self.stored(false)?;
            self.storage.write(&merge(latest))
        }))
        .unwrap_or_else(|_| Err(panicked("save")));
        let mut held = self.held();
        held.saving -= 1;
        saved?;
        let landed = before.min(held.ops.len());
        held.ops.drain(..landed);
        let refused = refused.into_inner().unwrap_or_else(|p| p.into_inner());
        Ok(refused.into_iter().skip(before).collect())
    }

    /// [`save_with`](Engine::save_with), whose failure fails nothing: the changes
    /// it could not save are kept for this process, those the journal had room
    /// for (judged on `view`, the state the call read, when the save failed).
    /// Whether each of `ops` was refused.
    pub fn save_ops(
        &self,
        view: &StoredState,
        patch: &Patch,
        basis: &Basis,
        ops: &[JournalOp],
    ) -> Vec<bool> {
        match self.save_with(patch, None, basis, ops) {
            Ok(refused) => refused,
            Err(_) => self.keep_unsaved(view, ops),
        }
    }

    /// Keep `ops` for this process, those the journal has room for on `view`.
    /// Whether each was refused.
    pub fn keep_unsaved(&self, view: &StoredState, ops: &[JournalOp]) -> Vec<bool> {
        let now = guarded_now(self.clock.now(), view.last_seen);
        let refused = land_journal(view, ops, now).refused;
        let mut held = self.held();
        // A spend is told to the customer from here on: it lands whatever the
        // journal holds by then. A void is no spend told, so it is held to the
        // journal's rules, and to the newest `MAX_PENDING_VOIDS` here.
        for (op, refused) in ops.iter().zip(&refused) {
            if !refused {
                held.ops.push(told(op));
            }
        }
        if held.saving == 0 {
            held.ops = bounded_voids(compacted(&held.ops));
        }
        refused
    }

    /// Save what this process could not, best-effort, at the start of a call
    /// that may write.
    pub fn save_unsaved_journal(&self) {
        if !self.held_changes().is_empty() {
            self.save_best_effort(&Patch::default(), None, &Basis::default());
        }
    }
}

/// `op` as it is kept in memory: a spend marked told.
fn told(op: &JournalOp) -> JournalOp {
    match op {
        JournalOp::Spend(spend) if spend.kind != SpendKind::Void => {
            JournalOp::Spend(crate::journal::SpendOp {
                told: true,
                ..spend.clone()
            })
        }
        other => other.clone(),
    }
}
