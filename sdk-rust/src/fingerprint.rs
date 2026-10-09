//! The default fingerprinter: the operating system's machine id, hashed, as
//! the Electron SDK's default fingerprinter makes it, so a machine has the same
//! fingerprint under either SDK.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::adapters::{AdapterError, DeviceIdentity, Fingerprinter};
use crate::lease::hex_sha256;
use crate::machine_id::{
    node_arch, node_platform, read_machine_id, MachineIdReading, MachineIdSource,
    SystemMachineIdSource, ACTIVATE_MACHINE_ID_TIMEOUT,
};
use crate::status::DeviceKind;

/// A machine's fingerprint from what identifies it, hashed so the raw
/// identifier never leaves it: hex SHA-256 of `keygrant|<raw>`.
pub fn hash_fingerprint(raw: &str) -> String {
    hex_sha256(&format!("keygrant|{raw}"))
}

/// The host-name fingerprint: the host name, platform and architecture (as
/// Node names them: `win32`/`darwin`/`linux`, `x64`/`arm64`/`ia32`/`arm`),
/// hashed. The fingerprint when the machine has no id to read, and offered as
/// an alternate beside the machine id's.
pub fn hostname_fingerprint() -> String {
    host_names_fingerprint(host_names().as_ref())
}

/// What a host-name fingerprint hashes, as Node names them ([`host_fingerprint`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostNames {
    /// The host name ([`host_name`]).
    pub host: String,
    /// The platform, as Node names it (`win32`, `darwin`, `linux`, ...).
    pub platform: String,
    /// The architecture, as Node names it (`x64`, `arm64`, `ia32`, `arm`, ...).
    pub arch: String,
}

/// This machine's host names, or none when the host name cannot be read.
pub fn host_names() -> Option<HostNames> {
    host_name().map(|host| HostNames {
        host,
        platform: node_platform().to_owned(),
        arch: node_arch().to_owned(),
    })
}

/// The host-name fingerprint of these names; [`UNKNOWN_HOST`]'s when they
/// could not be read (none).
fn host_names_fingerprint(names: Option<&HostNames>) -> String {
    match names {
        Some(names) => host_fingerprint(&names.host, &names.platform, &names.arch),
        None => hash_fingerprint(UNKNOWN_HOST),
    }
}

/// Who this device is, as the default fingerprinter answers (`identify`),
/// from a reading of its machine id ([`read_machine_id`]) and its host names
/// (none when they cannot be read):
///  - an id: the machine id's fingerprint ([`machine_fingerprint`]), kind
///    `Machine`, with the host name's ([`host_fingerprint`]) as an alternate,
///    none when the two are the same;
///  - no id to read: the host name's, kind `Hostname`, no alternate;
///  - a read that did not finish: no fingerprint this time, and the host
///    name's as an alternate.
///
/// The host name's is [`UNKNOWN_HOST`]'s when the names cannot be read.
pub fn identity_of(reading: &MachineIdReading, host: Option<&HostNames>) -> DeviceIdentity {
    let legacy = host_names_fingerprint(host);
    let settled = settled_on(reading, &legacy);
    with_host_alternate(settled, legacy)
}

/// What a reading of the machine id settles on: its fingerprint; the host
/// name's when there is no id to read; none when the read did not finish,
/// which settles nothing.
fn settled_on(reading: &MachineIdReading, host_fingerprint: &str) -> Option<Settled> {
    match reading {
        MachineIdReading::Id(id) => Some(Settled {
            fingerprint: machine_fingerprint(id),
            kind: DeviceKind::Machine,
        }),
        MachineIdReading::Unavailable => Some(Settled {
            fingerprint: host_fingerprint.to_owned(),
            kind: DeviceKind::Hostname,
        }),
        MachineIdReading::Transient => None,
    }
}

/// Who a settled fingerprint is (none: nobody this time), with the host
/// name's (`legacy`) as an alternate when it is another.
fn with_host_alternate(known: Option<Settled>, legacy: String) -> DeviceIdentity {
    let Some(known) = known else {
        return DeviceIdentity {
            fingerprint: None,
            kind: None,
            alternates: vec![legacy],
        };
    };
    let alternates = if known.fingerprint == legacy {
        Vec::new()
    } else {
        vec![legacy]
    };
    DeviceIdentity {
        fingerprint: Some(known.fingerprint),
        kind: Some(known.kind),
        alternates,
    }
}

/// This machine's host name as Node's `os.hostname()` reads it, or none when
/// it cannot be read. On Windows that is `GetHostNameW`'s answer, which
/// `GetComputerNameExW(ComputerNameDnsHostname)` gives: on a cluster node
/// running under a cluster's network name, the virtual name, where the
/// "physical" name the `hostname` crate reads would differ. Elsewhere it is
/// `gethostname()`, as Node's is.
pub fn host_name() -> Option<String> {
    #[cfg(windows)]
    {
        std::panic::catch_unwind(whoami::devicename_os)
            .ok()
            .and_then(Result::ok)
            .map(|name| name.to_string_lossy().into_owned())
    }
    #[cfg(not(windows))]
    {
        hostname::get()
            .ok()
            .map(|name| name.to_string_lossy().into_owned())
    }
}

/// What [`hostname_fingerprint`] hashes, given the host name and Node's names
/// for the platform and architecture: every SDK composes it the same way, so
/// a machine is one device whichever reads it.
pub fn host_fingerprint(host: &str, platform: &str, arch: &str) -> String {
    hash_fingerprint(&format!("{host}|{platform}|{arch}"))
}

/// What is hashed when not even the host name can be read.
pub const UNKNOWN_HOST: &str = "unknown-host";

/// The fingerprint of a machine id as [`read_machine_id`] gives it.
pub fn machine_fingerprint(id: &str) -> String {
    hash_fingerprint(&format!("machine-id|{id}"))
}

/// A settled fingerprint, and what it hashes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Settled {
    fingerprint: String,
    kind: DeviceKind,
}

/// The system fingerprinter's settled fingerprint, kept for the process.
static PROCESS_FINGERPRINT: Mutex<Option<Settled>> = Mutex::new(None);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The default fingerprinter: the machine id (which survives renaming the
/// computer), hashed; the host name's fingerprint when the machine has no id
/// to read, and `identify` says which. A read that did not finish gives NO
/// fingerprint and is not kept, so the next call reads again. A settled
/// fingerprint is kept for the process (for this fingerprinter, when given
/// its own source).
///
/// `get()` waits as long as an activation does and fails when the id still
/// could not be read; the SDK itself uses `identify`. A wrapper around this
/// fingerprinter should delegate `identify` too, to keep the kind and the
/// host-name alternate.
pub struct DefaultFingerprinter {
    source: Option<Arc<dyn MachineIdSource>>,
    own: Mutex<Option<Settled>>,
}

impl std::fmt::Debug for DefaultFingerprinter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DefaultFingerprinter")
            .field("custom_source", &self.source.is_some())
            .finish()
    }
}

impl Default for DefaultFingerprinter {
    fn default() -> Self {
        DefaultFingerprinter::new()
    }
}

impl DefaultFingerprinter {
    /// The fingerprinter over the real system, its answer kept for the process.
    pub fn new() -> Self {
        DefaultFingerprinter {
            source: None,
            own: Mutex::new(None),
        }
    }

    /// The fingerprinter over another source (a stand-in, in tests), its
    /// answer kept for this fingerprinter.
    pub fn with_source(source: Arc<dyn MachineIdSource>) -> Self {
        DefaultFingerprinter {
            source: Some(source),
            own: Mutex::new(None),
        }
    }

    fn settled(&self) -> Option<Settled> {
        match self.source {
            Some(_) => lock(&self.own).clone(),
            None => lock(&PROCESS_FINGERPRINT).clone(),
        }
    }

    fn keep(&self, found: &Settled) {
        match self.source {
            Some(_) => *lock(&self.own) = Some(found.clone()),
            None => *lock(&PROCESS_FINGERPRINT) = Some(found.clone()),
        }
    }

    /// A reading of this machine's id, bounded by `timeout`.
    fn read(&self, timeout: Duration) -> MachineIdReading {
        let source: Arc<dyn MachineIdSource> = match &self.source {
            Some(source) => Arc::clone(source),
            None => Arc::new(SystemMachineIdSource),
        };
        read_machine_id(source, timeout)
    }
}

impl Fingerprinter for DefaultFingerprinter {
    fn get(&self) -> Result<String, AdapterError> {
        self.identify(ACTIVATE_MACHINE_ID_TIMEOUT)
            .and_then(|identity| identity.fingerprint)
            .ok_or_else(|| "this machine's id could not be read in time".into())
    }

    /// [`identity_of`] this machine's reading and host names; a fingerprint
    /// settled on is kept, and answered from then on with the host name's
    /// alternate as of each call.
    fn identify(&self, timeout: Duration) -> Option<DeviceIdentity> {
        let host = host_names();
        if let Some(known) = self.settled() {
            return Some(with_host_alternate(
                Some(known),
                host_names_fingerprint(host.as_ref()),
            ));
        }
        let identity = identity_of(&self.read(timeout), host.as_ref());
        if let (Some(fingerprint), Some(kind)) = (&identity.fingerprint, identity.kind) {
            self.keep(&Settled {
                fingerprint: fingerprint.clone(),
                kind,
            });
        }
        Some(identity)
    }
}
