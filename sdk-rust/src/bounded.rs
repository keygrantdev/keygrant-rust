//! Work the SDK holds to a time bound, whatever the work does: run on a thread
//! of its own and abandoned when it does not answer in time (as the Electron
//! SDK abandons a timed-out promise). Abandoned work runs on until it ends,
//! with nobody waiting for it.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// `work`'s answer, or `None` when it did not answer within `bound`, panicked,
/// or no thread could be started for it.
pub(crate) fn bounded<T, F>(bound: Duration, work: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    let spawned = thread::Builder::new()
        .name("keygrant-bounded".into())
        .spawn(move || {
            let _ = sender.send(work());
        });
    if spawned.is_err() {
        return None;
    }
    receiver.recv_timeout(bound).ok()
}
