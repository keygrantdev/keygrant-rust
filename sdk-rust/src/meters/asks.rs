//! When a spend asks the server, and when the journal makes a status ask.

use super::{SpendEntry, SpendKind};
use crate::offline::{ASK_AGAIN_MS, RETRY_UNANSWERED_MS};
use crate::state::StoredState;

/// Whether the journal's wait is running at `now` (the guarded clock): with k
/// `Unanswered` and `Void` entries (k at least 1), until
/// `min(ASK_AGAIN_MS, RETRY_UNANSWERED_MS x 2^(k-1))` after the newest of them
/// was opened: a minute after one, two after two, four, eight, and an hour from
/// the seventh on. A device out of reach asks less and less often, so a person
/// is not kept waiting on a timeout at every use. An entry from after `now` (a
/// clock that jumped ahead and was corrected) is no wait that can be measured,
/// as a device clock set back ends the first wait: it is left out, of the count
/// and of the newest, and the others wait as they would alone, so a corrected
/// clock never stops the asking for days.
pub fn journal_waiting(spends: &[SpendEntry], now: i64) -> bool {
    let opened = spends
        .iter()
        .filter(|entry| entry.kind != SpendKind::Offline && entry.at <= now);
    let (count, newest) = opened.fold((0usize, i64::MIN), |(count, newest), entry| {
        (count + 1, newest.max(entry.at))
    });
    if count == 0 {
        return false;
    }
    // The cap first: 2^(k-1) of a long journal overflows nothing, and from the
    // seventh entry on it is the hour.
    let wait = if count >= 7 {
        ASK_AGAIN_MS
    } else {
        ASK_AGAIN_MS.min(RETRY_UNANSWERED_MS << (count - 1))
    };
    i128::from(now) < i128::from(newest) + i128::from(wait)
}

/// Whether the wait of an uncovered spend runs at `now` (the guarded clock): a
/// `Void`, the take-back of an uncovered spend that was asked and got no
/// answer, opened less than a minute ago. An outage then costs such a spend one
/// ask a minute, and the journal one void a minute at most. A void opened after
/// `now`, or after the device clock (`clock_now`), is no wait that can be
/// measured, as a device clock set back ends the first wait, and is left out:
/// a clock that is behind the guard, which then stands still, never refuses an
/// online-only meter to a customer who is online.
pub fn void_waiting(spends: &[SpendEntry], now: i64, clock_now: i64) -> bool {
    spends.iter().any(|entry| {
        entry.kind == SpendKind::Void
            && entry.at <= now
            && entry.at <= clock_now
            && i128::from(now) < i128::from(entry.at) + i128::from(RETRY_UNANSWERED_MS)
    })
}

/// Whether `use_meter` asks the server. A spend the offline allowance would
/// refuse (`covered` false) asks whatever the waits of an outage (an online
/// customer is not refused for an outage that has passed), but for a wait of
/// its own: not while a void opened less than a minute ago stands
/// ([`void_waiting`], by both clocks). A spend the allowance covers asks every
/// time, except while a no-answer wait runs (`backoffSince` set, `backoffMs`
/// equal to a minute, and the device clock at or after it and before its end: a
/// clock set back asks at once) and while the journal's wait runs
/// ([`journal_waiting`]). An answer of any kind ends both waits.
///
/// `clock_now` is the device clock, which the first wait is judged by; `now`
/// the guarded clock, which the journal's is.
pub fn use_asks_now(state: &StoredState, clock_now: i64, now: i64, covered: bool) -> bool {
    let spends = state.spends.as_deref().unwrap_or_default();
    if !covered {
        return !void_waiting(spends, now, clock_now);
    }
    let waiting = state.backoff_since.is_some_and(|since| {
        state.backoff_ms == Some(RETRY_UNANSWERED_MS)
            && clock_now >= since
            && i128::from(clock_now) < i128::from(since) + i128::from(RETRY_UNANSWERED_MS)
    });
    !waiting && !journal_waiting(spends, now)
}

/// Whether the journal makes `status()` ask a valid lease's server now: it holds
/// an entry and its wait is not running.
pub fn journal_asks(spends: &[SpendEntry], now: i64) -> bool {
    !spends.is_empty() && !journal_waiting(spends, now)
}
