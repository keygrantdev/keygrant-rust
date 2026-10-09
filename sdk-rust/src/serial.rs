//! One call at a time on a `License`.
//!
//! Each public call reads the stored state, may ask the server, and writes
//! the state back; calls that overlapped could write back a snapshot taken
//! before another's answer, and both would ask the server the same question.
//! In turn, each starts from what the last one left.
//!
//! A call waits for the one before it to finish, but not past that call's
//! bound (the HTTP bound plus the lock grace, and more for a call that may
//! make a second request) from when it STARTED running. Requests are
//! bounded, but storage, fingerprint and registry adapters are not (cutting a
//! storage write short could leave it half-written), so a call stuck on one
//! is left to finish on its own, and the calls behind it go ahead READ-ONLY:
//! they answer from what is stored, with no ask and no write, for as long as
//! any call before them is still unfinished. A write of theirs could be
//! reverted by the stuck call's late one, which carries a whole state taken
//! before them; not writing is the only way a late write cannot undo anything.

use std::collections::HashSet;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub(crate) struct Serial {
    inner: Mutex<Inner>,
    changed: Condvar,
}

struct Inner {
    /// The ticket the next call takes.
    next: u64,
    /// The last call that has started (calls start in ticket order).
    started: u64,
    /// When it started.
    started_at: Instant,
    /// How long the calls behind it wait for it from then: its own bound.
    started_bound: Duration,
    /// Calls that have finished, kept until the call after them starts.
    settled: HashSet<u64>,
    /// Calls running with writes allowed that have not finished.
    writing: usize,
}

impl Serial {
    pub fn new() -> Self {
        Serial {
            inner: Mutex::new(Inner {
                next: 1,
                started: 0,
                started_at: Instant::now(),
                started_bound: Duration::ZERO,
                settled: HashSet::from([0]),
                writing: 0,
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Run `work` once every call before it has finished, or once the call
    /// before it has run its own bound since it started; `work` is told
    /// whether it must write nothing (a call before it is still unfinished).
    /// `bound` is this call's: how long the call after it waits for it.
    pub fn run<T>(&self, bound: Duration, work: impl FnOnce(bool) -> T) -> T {
        let (ticket, read_only) = self.start(bound);
        let _settle = Settle {
            serial: self,
            ticket,
            writer: !read_only,
        };
        work(read_only)
    }

    fn start(&self, bound: Duration) -> (u64, bool) {
        let mut inner = self.lock();
        let ticket = inner.next;
        inner.next += 1;
        let previous = ticket - 1;
        while inner.started < previous {
            inner = self
                .changed
                .wait(inner)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        // The grace runs from when the call before started, however late this
        // one was queued: it is that call's timer, begun when it began, and
        // as long as that call's own bound.
        let deadline = inner.started_at.checked_add(inner.started_bound);
        while !inner.settled.contains(&previous) {
            match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        break;
                    }
                    inner = self
                        .changed
                        .wait_timeout(inner, deadline - now)
                        .map(|(guard, _)| guard)
                        .unwrap_or_else(|poisoned| poisoned.into_inner().0);
                }
                None => {
                    inner = self
                        .changed
                        .wait(inner)
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                }
            }
        }
        inner.settled.remove(&previous);
        let read_only = inner.writing > 0;
        if !read_only {
            inner.writing += 1;
        }
        inner.started = ticket;
        inner.started_at = Instant::now();
        inner.started_bound = bound;
        self.changed.notify_all();
        (ticket, read_only)
    }
}

/// Marks a call finished when it ends, however it ends.
struct Settle<'a> {
    serial: &'a Serial,
    ticket: u64,
    writer: bool,
}

impl Drop for Settle<'_> {
    fn drop(&mut self) {
        let mut inner = self.serial.lock();
        if self.writer {
            inner.writing -= 1;
        }
        // Wanted only by the call after it, while that one has not started.
        if inner.started == self.ticket {
            inner.settled.insert(self.ticket);
        }
        self.serial.changed.notify_all();
    }
}
