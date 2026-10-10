//! Who this device is, for holding a lease to it and telling the server: as
//! far as can be told this call, and never guessed.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::adapters::{DeviceIdentity, Fingerprinter};
use crate::lease::fingerprint_claim;
use crate::status::DeviceKind;

/// Who this device is, as far as this call could tell.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Device {
    /// This run's own fingerprint, or none when it cannot be told this time.
    pub own: Option<String>,
    /// What `own` hashes, when the fingerprinter says (and there is an `own`).
    pub kind: Option<DeviceKind>,
    /// Whether this run could tell who the device is.
    pub can_tell: bool,
    /// Every fingerprint this device answers to: its own first, then its alternates.
    pub fingerprints: Vec<String>,
    /// Their `fp` claims.
    pub claims: Vec<String>,
    /// The claim of `own` alone, when there is one.
    pub own_claim: Option<String>,
    /// The host-name fingerprint to activate under when the machine id cannot
    /// be read even in an activation's time, or none.
    pub hostname: Option<String>,
}

impl Device {
    /// The device as the reference's conformance vectors write it: `own` and
    /// `hostname` always there (`null` for none), `ownClaim` and `kind` only
    /// when there is one.
    pub fn to_json(&self) -> Value {
        let mut out = json!({
            "own": self.own,
            "canTell": self.can_tell,
            "fingerprints": self.fingerprints,
            "claims": self.claims,
            "hostname": self.hostname,
        });
        if let Some(own_claim) = &self.own_claim {
            out["ownClaim"] = json!(own_claim);
        }
        if let Some(kind) = self.kind {
            out["kind"] = json!(kind.as_str());
        }
        out
    }
}

/// Who this device is, from what its fingerprinter said this time
/// (`identity`), on an install keyed as `install_kind` (`StoredState::device_kind`),
/// held to its device or not (`device_hold`); `claim_of` is
/// [`fingerprint_claim`], which a reader may remember.
///
/// Install memory: once a machine id has keyed this install, a host-name
/// answer is not this device either. The id being unreadable now (an
/// endpoint-security tool, a command that failed once) says nothing about
/// whether the machine changed, so it is "cannot tell", never the host name. A
/// fingerprint of no kind (a custom `get()`) is still its own.
///
/// Install memory applies only to a held device. Not held (`device_hold`
/// false), a device has no kind and no own claim, so no seat is keyed to it,
/// and it is judged as a run that cannot tell, so no lease is refused for its
/// device and the server is told no fingerprints. Its own still activates.
pub fn device_from(
    identity: &DeviceIdentity,
    install_kind: Option<DeviceKind>,
    device_hold: bool,
    mut claim_of: impl FnMut(&str) -> String,
) -> Device {
    let keyed_as = if device_hold { install_kind } else { None };
    // A kind with no fingerprint says nothing.
    let read = identity.fingerprint.as_ref().and(identity.kind);
    let own = if read == Some(DeviceKind::Hostname) && keyed_as == Some(DeviceKind::Machine) {
        None
    } else {
        identity.fingerprint.clone()
    };
    let mut fingerprints: Vec<String> = own.iter().cloned().collect();
    fingerprints.extend(
        identity
            .alternates
            .iter()
            .filter(|f| Some(*f) != own.as_ref())
            .cloned(),
    );
    let hostname = if read == Some(DeviceKind::Hostname) {
        identity.fingerprint.clone()
    } else {
        identity.alternates.first().cloned()
    };
    let claims: Vec<String> = fingerprints.iter().map(|f| claim_of(f)).collect();
    let device = Device {
        can_tell: own.is_some(),
        own_claim: own.as_ref().and_then(|_| claims.first().cloned()),
        kind: own.as_ref().and(read),
        own,
        fingerprints,
        claims,
        hostname,
    };
    if device_hold {
        device
    } else {
        Device {
            kind: None,
            own_claim: None,
            can_tell: false,
            ..device
        }
    }
}

/// The fingerprint an activation is made under, and what it hashes; or none:
/// an activation answers `Network` for the app to retry.
///
/// The device's own, given the time an activation deserves. When even that
/// cannot be told: on an install already keyed by a machine id, nothing (a
/// seat under the host name would read as another device to every normal
/// run after it); otherwise the host name, so a machine whose id never reads
/// in time can still activate. A fingerprinter with no host-name alternate
/// (a custom `get()`) activates nothing.
pub(crate) fn activation_identity(
    device: &Device,
    install_kind: Option<DeviceKind>,
) -> Option<(String, Option<DeviceKind>)> {
    if let Some(own) = &device.own {
        return Some((own.clone(), device.kind));
    }
    if install_kind == Some(DeviceKind::Machine) {
        return None;
    }
    device
        .hostname
        .clone()
        .map(|hostname| (hostname, Some(DeviceKind::Hostname)))
}

/// Reads, and once settled keeps, who this device is.
pub(crate) struct DeviceIdentityReader {
    fingerprinter: Arc<dyn Fingerprinter>,
    known: Mutex<Option<DeviceIdentity>>,
    claims: Mutex<HashMap<String, String>>,
    /// The read of the fingerprinter running now, if any.
    inflight: Arc<Mutex<Option<Arc<Flight>>>>,
    /// How long past its timeout `identify` is waited on.
    grace: Duration,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl DeviceIdentityReader {
    /// A reader of `fingerprinter`, whose `identify` is waited on `grace`
    /// past the time it is given ([`IDENTIFY_GRACE`] but in tests).
    pub fn new(fingerprinter: Arc<dyn Fingerprinter>, grace: Duration) -> Self {
        DeviceIdentityReader {
            fingerprinter,
            known: Mutex::new(None),
            claims: Mutex::new(HashMap::new()),
            inflight: Arc::new(Mutex::new(None)),
            grace,
        }
    }

    /// Who this device is ([`device_from`]), reading for at most about
    /// `timeout`, on an install keyed as `install_kind`, held to its device
    /// unless `device_hold` is false. A fingerprinter that fails, or does not
    /// answer in time, gives "cannot tell", not kept: a failed read must
    /// neither stand in for the device nor stick.
    pub fn read(
        &self,
        timeout: Duration,
        install_kind: Option<DeviceKind>,
        device_hold: bool,
    ) -> Device {
        let known = lock(&self.known).clone();
        let identity = match known {
            Some(identity) => identity,
            None => self.identity(timeout),
        };
        if identity.fingerprint.is_some() {
            *lock(&self.known) = Some(identity.clone());
        }
        device_from(&identity, install_kind, device_hold, |fingerprint| {
            self.claim_of(fingerprint)
        })
    }

    /// A fingerprint's claim ([`fingerprint_claim`]), remembered.
    fn claim_of(&self, fingerprint: &str) -> String {
        lock(&self.claims)
            .entry(fingerprint.to_owned())
            .or_insert_with(|| fingerprint_claim(fingerprint))
            .clone()
    }

    /// The fingerprinter's answer, bounded: `identify` a second past
    /// `timeout` (the reader's grace), `get` (for a fingerprinter without
    /// `identify`) `timeout` from when it was called.
    ///
    /// A read is shared: a call that comes while one is running waits on it,
    /// up to its own bound, rather than start another, when that read was
    /// given at least the call's `timeout` and is not past its own bound. A
    /// call with a longer bound reads for itself, as a read given less time
    /// may give up before the call would (an activation is not answered by a
    /// status's read); and a read past its bound (a fingerprinter that ignores
    /// its timeout, or never returns) is not waited on by a later call, which
    /// reads again: a failed read is not kept. A call that waits on a read
    /// given longer still gives up at its own bound.
    ///
    /// A fingerprinter that never returns (only a custom one can: the default
    /// bounds its own reads) so strands a thread on every read of the device,
    /// up to three a `status()`; README "Parity with the reference" counts them.
    fn identity(&self, timeout: Duration) -> DeviceIdentity {
        let started = Instant::now();
        let Some(flight) = self.flight(started, timeout) else {
            return DeviceIdentity::default();
        };
        let mut phase = lock(&flight.phase);
        loop {
            let deadline = match &*phase {
                Phase::Done(identity) => return identity.clone(),
                waiting => bound(started, timeout, self.grace, waiting),
            };
            phase = match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return DeviceIdentity::default();
                    }
                    flight
                        .changed
                        .wait_timeout(phase, deadline - now)
                        .map(|(guard, _)| guard)
                        .unwrap_or_else(|poisoned| poisoned.into_inner().0)
                }
                None => flight
                    .changed
                    .wait(phase)
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
            };
        }
    }

    /// The read in flight, when it serves a call made at `started` and
    /// bounded by `timeout` ([`Flight::serves`]); else a new one, given that
    /// bound. None when no thread can be started.
    fn flight(&self, started: Instant, timeout: Duration) -> Option<Arc<Flight>> {
        let mut slot = lock(&self.inflight);
        if let Some(flight) = slot.as_ref() {
            if flight.serves(started, timeout) {
                return Some(Arc::clone(flight));
            }
        }
        let flight = Arc::new(Flight {
            started,
            timeout,
            grace: self.grace,
            phase: Mutex::new(Phase::Identifying),
            changed: Condvar::new(),
        });
        let (ours, inflight) = (Arc::clone(&flight), Arc::clone(&self.inflight));
        let fingerprinter = Arc::clone(&self.fingerprinter);
        // The guard is made on the read's own thread: were the thread not
        // started, nothing of it is dropped here, under the slot's lock.
        let spawned = thread::Builder::new()
            .name("keygrant-fingerprint".into())
            .spawn(move || {
                let settle = Settle {
                    flight: ours,
                    slot: inflight,
                    answer: None,
                };
                read_identity(fingerprinter.as_ref(), timeout, settle);
            });
        if spawned.is_err() {
            return None;
        }
        // A read this one replaces runs on to its end, for the calls waiting
        // on it, and is no longer handed out.
        *slot = Some(Arc::clone(&flight));
        Some(flight)
    }
}

/// How long `identify` may run past the timeout it is given (the reference's
/// second).
pub(crate) const IDENTIFY_GRACE: Duration = Duration::from_secs(1);

/// Until when a read begun at `started` and bounded by `timeout` is waited
/// on, in `phase`: `identify` `grace` past `timeout`; `get` `timeout`
/// from when it was called, or from `started` when that is later. None
/// when there is no such bound (one past what an `Instant` holds), or the
/// read is done.
fn bound(started: Instant, timeout: Duration, grace: Duration, phase: &Phase) -> Option<Instant> {
    match phase {
        Phase::Identifying => started.checked_add(timeout.saturating_add(grace)),
        Phase::Getting(since) => started.max(*since).checked_add(timeout),
        Phase::Done(_) => None,
    }
}

/// One read of the fingerprinter, shared by the calls it serves ([`Flight::serves`]).
struct Flight {
    /// When the read began, the timeout it was given, and the grace past it.
    started: Instant,
    timeout: Duration,
    grace: Duration,
    phase: Mutex<Phase>,
    changed: Condvar,
}

impl Flight {
    /// Whether a call made at `now` and bounded by `timeout` waits on this
    /// read rather than start its own: when the read was given at least as
    /// long, and is done or not past its own bound.
    fn serves(&self, now: Instant, timeout: Duration) -> bool {
        if self.timeout < timeout {
            return false;
        }
        match &*lock(&self.phase) {
            Phase::Done(_) => true,
            running => {
                bound(self.started, self.timeout, self.grace, running).is_none_or(|end| now < end)
            }
        }
    }
}

enum Phase {
    /// `identify` is running.
    Identifying,
    /// `identify` has none to give; `get` has run since then.
    Getting(Instant),
    /// The answer (none to be told, when the fingerprinter failed or panicked).
    Done(DeviceIdentity),
}

/// The read's end, however it ends: its answer, or "cannot tell" when it
/// panicked; and the read no longer in flight.
struct Settle {
    flight: Arc<Flight>,
    slot: Arc<Mutex<Option<Arc<Flight>>>>,
    answer: Option<DeviceIdentity>,
}

impl Drop for Settle {
    fn drop(&mut self) {
        *lock(&self.flight.phase) = Phase::Done(self.answer.take().unwrap_or_default());
        self.flight.changed.notify_all();
        let mut slot = lock(&self.slot);
        if slot
            .as_ref()
            .is_some_and(|flight| Arc::ptr_eq(flight, &self.flight))
        {
            *slot = None;
        }
    }
}

/// The fingerprinter's answer: `identify`'s, else `get`'s, of unknown kind.
fn read_identity(fingerprinter: &dyn Fingerprinter, timeout: Duration, mut settle: Settle) {
    if let Some(identity) = fingerprinter.identify(timeout) {
        settle.answer = Some(identity);
        return;
    }
    *lock(&settle.flight.phase) = Phase::Getting(Instant::now());
    settle.flight.changed.notify_all();
    if let Ok(fingerprint) = fingerprinter.get() {
        settle.answer = Some(DeviceIdentity {
            fingerprint: Some(fingerprint),
            kind: None,
            alternates: Vec::new(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::AdapterError;

    /// A gate a read waits at until it is opened.
    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        opened: Condvar,
    }

    impl Gate {
        fn wait(&self) {
            let mut open = lock(&self.open);
            while !*open {
                open = self
                    .opened
                    .wait(open)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        }
        fn open(&self) {
            *lock(&self.open) = true;
            self.opened.notify_all();
        }
    }

    /// A machine whose id takes `takes` to read and is read only in a read
    /// given at least `needs` (a read given less gives up: none this time);
    /// its first read waits at `gate` first when there is one. It notes the
    /// timeout each read was given.
    struct Machine {
        needs: Duration,
        takes: Duration,
        gate: Option<Gate>,
        reads: Mutex<Vec<Duration>>,
    }

    impl Machine {
        fn new(needs: Duration, takes: Duration, gate: Option<Gate>) -> Arc<Machine> {
            Arc::new(Machine {
                needs,
                takes,
                gate,
                reads: Mutex::new(Vec::new()),
            })
        }
        fn reads(&self) -> Vec<Duration> {
            lock(&self.reads).clone()
        }
    }

    impl Fingerprinter for Machine {
        fn get(&self) -> Result<String, AdapterError> {
            Err("identify answers".into())
        }
        fn identify(&self, timeout: Duration) -> Option<DeviceIdentity> {
            let first = {
                let mut reads = lock(&self.reads);
                reads.push(timeout);
                reads.len() == 1
            };
            if let (true, Some(gate)) = (first, &self.gate) {
                gate.wait();
            }
            thread::sleep(self.takes);
            let read = timeout >= self.needs;
            Some(DeviceIdentity {
                fingerprint: read.then(|| "fp-machine".to_owned()),
                kind: read.then_some(DeviceKind::Machine),
                alternates: vec!["fp-host".into()],
            })
        }
    }

    /// A status's bound, and an activation's: the status's given up on an id
    /// that needs more.
    const SHORT: Duration = Duration::from_millis(200);
    const LONG: Duration = Duration::from_secs(20);

    fn reader_over(machine: &Arc<Machine>, grace: Duration) -> Arc<DeviceIdentityReader> {
        let fingerprinter: Arc<dyn Fingerprinter> = machine.clone();
        Arc::new(DeviceIdentityReader::new(fingerprinter, grace))
    }

    /// A read on a thread of its own.
    fn read_beside(
        reader: &Arc<DeviceIdentityReader>,
        timeout: Duration,
    ) -> thread::JoinHandle<Device> {
        let reader = Arc::clone(reader);
        thread::spawn(move || reader.read(timeout, None, true))
    }

    /// Wait (up to 30 s) until `done`.
    fn wait_until(what: &str, done: impl Fn() -> bool) {
        let until = Instant::now() + Duration::from_secs(30);
        while !done() {
            assert!(Instant::now() < until, "{what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_for_reads(machine: &Machine, count: usize) {
        wait_until("the reads", || machine.reads().len() >= count);
    }

    /// How many calls wait on the read in flight: every holder of it but the
    /// slot and the read's own thread.
    fn waiting(reader: &DeviceIdentityReader) -> usize {
        lock(&reader.inflight)
            .as_ref()
            .map_or(0, |flight| Arc::strong_count(flight) - 2)
    }

    #[test]
    fn a_call_with_a_longer_bound_reads_for_itself_and_one_with_a_shorter_waits_on_the_read() {
        // A status's read gives up on an id that needs 5 s; an activation,
        // given longer, reads it, though it comes while the status's read runs.
        let machine = Machine::new(Duration::from_secs(5), Duration::from_millis(300), None);
        let reader = reader_over(&machine, IDENTIFY_GRACE);
        let status = read_beside(&reader, SHORT);
        wait_for_reads(&machine, 1);
        let activation = reader.read(LONG, None, true);
        assert_eq!(activation.own.as_deref(), Some("fp-machine"));
        assert!(!status.join().expect("the status").can_tell);
        assert_eq!(machine.reads(), [SHORT, LONG]);

        // The other way round, a call with a shorter bound waits on the read
        // already running, which was given longer: held until both wait on it.
        let gate = Gate::default();
        let machine = Machine::new(Duration::from_secs(5), Duration::ZERO, Some(gate));
        let reader = reader_over(&machine, IDENTIFY_GRACE);
        let activation = read_beside(&reader, LONG);
        wait_until("the activation waits", || waiting(&reader) == 1);
        let status = read_beside(&reader, LONG / 2);
        wait_until("the status waits", || waiting(&reader) == 2);
        if let Some(gate) = &machine.gate {
            gate.open();
        }
        let status = status.join().expect("the status");
        assert_eq!(status.own.as_deref(), Some("fp-machine"));
        assert!(activation.join().expect("the activation").can_tell);
        assert_eq!(machine.reads(), [LONG]);
    }

    #[test]
    fn a_call_waiting_on_a_read_given_longer_gives_up_at_its_own_bound() {
        // An activation's read does not answer. A status that comes meanwhile
        // waits on it only as long as a status may (1.2 s), not as long as
        // the activation may (21 s): behind a stuck call, a read-only status
        // would otherwise wait out an activation's bound.
        let machine = Machine::new(Duration::ZERO, Duration::ZERO, Some(Gate::default()));
        let reader = reader_over(&machine, IDENTIFY_GRACE);
        let activation = read_beside(&reader, LONG);
        wait_until("the activation waits", || waiting(&reader) == 1);
        let started = Instant::now();
        let status = reader.read(SHORT, None, true);
        let took = started.elapsed();
        assert!(!status.can_tell);
        assert!(took >= SHORT + IDENTIFY_GRACE, "{took:?}");
        assert!(took < Duration::from_secs(10), "{took:?}");
        if let Some(gate) = &machine.gate {
            gate.open();
        }
        let activation = activation.join().expect("the activation");
        assert_eq!(activation.own.as_deref(), Some("fp-machine"));
        assert_eq!(machine.reads(), [LONG]);
    }

    #[test]
    fn a_read_past_its_bound_is_not_waited_on_and_the_next_call_reads_again() {
        // The first read never comes back until the end of the test; the
        // second answers at once, with 3 s to spare.
        let bound = Duration::from_secs(2);
        let machine = Machine::new(Duration::ZERO, Duration::ZERO, Some(Gate::default()));
        let reader = reader_over(&machine, IDENTIFY_GRACE);
        let started = Instant::now();
        let first = reader.read(bound, None, true);
        assert!(!first.can_tell);
        assert!(started.elapsed() >= bound + IDENTIFY_GRACE);
        // Past its bound: the next call, bounded no longer, does not wait on
        // it, and reads.
        let next = reader.read(bound, None, true);
        assert_eq!(next.own.as_deref(), Some("fp-machine"));
        assert_eq!(machine.reads(), [bound, bound]);
        if let Some(gate) = &machine.gate {
            gate.open();
        }
    }
}
