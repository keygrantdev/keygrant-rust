//! The machine id the default fingerprinter hashes: per platform, and the
//! id / unavailable / transient distinction.

mod common;

use std::collections::HashMap;
use std::io::{Error as IoError, ErrorKind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{lock, PATIENT};
use keygrant::machine_id::{
    boot_time, classify_io, machine_id_filesystem, node_arch, node_arch_of, node_platform,
    node_platform_of, platform_uuid, read_machine_id, MachineIdReading, MachineIdSource,
    ReadFailure, SystemMachineIdSource, BOOT_SLACK_MS, MACHINE_ID_TIMEOUT,
};
use keygrant::{
    hash_fingerprint, hostname_fingerprint, DefaultFingerprinter, DeviceIdentity, DeviceKind,
    Fingerprinter,
};

const GUID: &str = "3f2504e0-4f89-11d3-9a0c-0305e82c3301";
const MAC_OUTPUT: &str = "+-o J314sAP  <class IOPlatformExpertDevice, id 0x100000110, registered>\n    {\n      \"IOPlatformSerialNumber\" = \"C02XXXXXXX\"\n      \"IOPlatformUUID\" = \"564D8A1C-3F6E-4E3B-9F2A-1B2C3D4E5F60\"\n    }";

type Run = (String, Vec<String>, Duration);

/// A stand-in system: the registry value, what each command prints (or how
/// it fails), what each file holds (or how reading it fails), and a file's
/// mtime. Anything not named is not there.
#[derive(Default)]
struct System {
    platform: String,
    guid: Option<Result<String, ReadFailure>>,
    outputs: HashMap<String, Result<String, ReadFailure>>,
    files: HashMap<String, Result<String, ReadFailure>>,
    mtime: Option<Result<f64, ReadFailure>>,
    runs: Mutex<Vec<Run>>,
    guid_reads: AtomicUsize,
}

impl System {
    fn on(platform: &str) -> Self {
        System {
            platform: platform.into(),
            ..System::default()
        }
    }
    fn guid(mut self, guid: Result<&str, ReadFailure>) -> Self {
        self.guid = Some(guid.map(str::to_owned));
        self
    }
    fn output(mut self, command: &str, output: Result<&str, ReadFailure>) -> Self {
        self.outputs
            .insert(command.into(), output.map(str::to_owned));
        self
    }
    fn file(mut self, path: &str, text: Result<&str, ReadFailure>) -> Self {
        self.files.insert(path.into(), text.map(str::to_owned));
        self
    }
    fn mtime(mut self, mtime: Result<f64, ReadFailure>) -> Self {
        self.mtime = Some(mtime);
        self
    }
    fn arc(self) -> Arc<System> {
        Arc::new(self)
    }
}

impl MachineIdSource for System {
    fn platform(&self) -> String {
        self.platform.clone()
    }
    fn windows_machine_guid(&self) -> Result<String, ReadFailure> {
        self.guid_reads.fetch_add(1, Ordering::SeqCst);
        self.guid.clone().unwrap_or(Err(ReadFailure::Unavailable))
    }
    fn run(&self, command: &str, args: &[&str], timeout: Duration) -> Result<String, ReadFailure> {
        lock(&self.runs).push((
            command.into(),
            args.iter().map(|a| (*a).to_owned()).collect(),
            timeout,
        ));
        self.outputs
            .get(command)
            .cloned()
            .unwrap_or(Err(ReadFailure::Unavailable))
    }
    fn read_file(&self, path: &str) -> Result<String, ReadFailure> {
        self.files
            .get(path)
            .cloned()
            .unwrap_or(Err(ReadFailure::Unavailable))
    }
    fn mtime_ms(&self, _path: &str) -> Result<f64, ReadFailure> {
        self.mtime.unwrap_or(Err(ReadFailure::Unavailable))
    }
}

fn read(source: Arc<System>) -> MachineIdReading {
    read_machine_id(source, MACHINE_ID_TIMEOUT)
}

fn id(text: &str) -> MachineIdReading {
    MachineIdReading::Id(text.into())
}

fn fallback() -> String {
    hostname_fingerprint()
}

// --- Windows -----------------------------------------------------------------------------

#[test]
fn reads_the_windows_machine_guid_from_the_registry_value() {
    assert_eq!(read(System::on("win32").guid(Ok(GUID)).arc()), id(GUID));
    // As `reg query` printed it: the first run of non-blank characters, trimmed.
    assert_eq!(
        read(
            System::on("win32")
                .guid(Ok(&format!("  \t{GUID}\r\n")))
                .arc()
        ),
        id(GUID)
    );
    assert_eq!(
        read(
            System::on("win32")
                .guid(Ok(&format!("{GUID} trailing")))
                .arc()
        ),
        id(GUID)
    );
    assert_eq!(
        read(System::on("win32").guid(Ok("   ")).arc()),
        MachineIdReading::Unavailable
    );
    assert_eq!(
        read(
            System::on("win32")
                .guid(Err(ReadFailure::Unavailable))
                .arc()
        ),
        MachineIdReading::Unavailable
    );
    assert_eq!(
        read(System::on("win32").guid(Err(ReadFailure::Transient)).arc()),
        MachineIdReading::Transient
    );
    let fingerprint = DefaultFingerprinter::with_source(System::on("win32").guid(Ok(GUID)).arc())
        .get()
        .expect("a fingerprint");
    assert_eq!(fingerprint, hash_fingerprint(&format!("machine-id|{GUID}")));
    assert!(!fingerprint.contains("3f2504e0"));
}

// --- macOS ---------------------------------------------------------------------------------

#[test]
fn reads_the_macos_platform_uuid_with_ioreg() {
    let source = System::on("darwin")
        .output("/usr/sbin/ioreg", Ok(MAC_OUTPUT))
        .arc();
    assert_eq!(
        read(Arc::clone(&source)),
        id("564D8A1C-3F6E-4E3B-9F2A-1B2C3D4E5F60")
    );
    let runs = lock(&source.runs).clone();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        (runs[0].0.as_str(), runs[0].1.clone()),
        (
            "/usr/sbin/ioreg",
            vec![
                "-rd1".to_owned(),
                "-c".into(),
                "IOPlatformExpertDevice".into()
            ]
        )
    );
    assert_eq!(runs[0].2, MACHINE_ID_TIMEOUT);
    assert_eq!(
        read(
            System::on("darwin")
                .output("/usr/sbin/ioreg", Ok("no uuid here"))
                .arc()
        ),
        MachineIdReading::Unavailable
    );
    assert_eq!(
        read(
            System::on("darwin")
                .output("/usr/sbin/ioreg", Err(ReadFailure::Transient))
                .arc()
        ),
        MachineIdReading::Transient
    );
}

#[test]
fn finds_the_platform_uuid_however_it_is_spaced_and_trims_it() {
    assert_eq!(platform_uuid("\"IOPlatformUUID\"=\"A-B\""), Some("A-B"));
    assert_eq!(
        platform_uuid("\"IOPlatformUUID\"  =  \" A B \""),
        Some(" A B ")
    );
    assert_eq!(
        platform_uuid("\"IOPlatformUUID\" = \"\"\n\"IOPlatformUUID\" = \"LATER\""),
        Some("LATER")
    );
    assert_eq!(platform_uuid("\"IOPlatformUUID\" : \"X\""), None);
    assert_eq!(
        read(
            System::on("darwin")
                .output("/usr/sbin/ioreg", Ok("\"IOPlatformUUID\" = \" A B \""))
                .arc()
        ),
        id("A B")
    );
}

// --- Linux ---------------------------------------------------------------------------------

#[test]
fn reads_etc_machine_id_on_linux_else_the_dbus_one_trimmed() {
    let linux = |files: &[(&str, Result<&str, ReadFailure>)]| {
        let mut system = System::on("linux");
        for (path, text) in files {
            system = system.file(path, *text);
        }
        read(system.arc())
    };
    assert_eq!(
        linux(&[("/etc/machine-id", Ok("a1b2c3d4e5f6\n"))]),
        id("a1b2c3d4e5f6")
    );
    assert_eq!(
        linux(&[("/var/lib/dbus/machine-id", Ok("dbus0001\n"))]),
        id("dbus0001")
    );
    assert_eq!(
        linux(&[
            ("/etc/machine-id", Ok("\n")),
            ("/var/lib/dbus/machine-id", Ok("dbus0002"))
        ]),
        id("dbus0002")
    );
    assert_eq!(
        linux(&[
            ("/etc/machine-id", Err(ReadFailure::Unavailable)),
            ("/var/lib/dbus/machine-id", Ok("dbus0003"))
        ]),
        id("dbus0003")
    );
    assert_eq!(
        linux(&[
            ("/etc/machine-id", Err(ReadFailure::Transient)),
            ("/var/lib/dbus/machine-id", Ok("dbus0004"))
        ]),
        MachineIdReading::Transient
    );
    assert_eq!(linux(&[]), MachineIdReading::Unavailable);
}

const ROOT: &str = "22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw";
const OVER_MACHINE_ID: &str =
    "29 23 0:25 /machine-id /etc/machine-id ro,relatime shared:5 - tmpfs tmpfs rw";
const ROOT_TMPFS: &str = "1 0 0:1 / / rw,relatime shared:1 - tmpfs tmpfs rw";

fn with_mounts(lines: &[&str]) -> Arc<System> {
    System::on("linux")
        .file(
            "/proc/self/mountinfo",
            Ok(&format!("{}\n", lines.join("\n"))),
        )
        .file("/etc/machine-id", Ok("a1b2c3d4e5f6\n"))
        .arc()
}

#[test]
fn has_no_id_to_hold_a_device_to_when_a_tmpfs_is_mounted_at_etc_machine_id() {
    let source = with_mounts(&[ROOT, OVER_MACHINE_ID]);
    assert_eq!(read(Arc::clone(&source)), MachineIdReading::Unavailable);
    // So the host name, a `hostname` fingerprint, never held to its device.
    let identity = DefaultFingerprinter::with_source(source).identify(PATIENT);
    let identity = identity.expect("the default fingerprinter identifies");
    assert_eq!(
        (identity.fingerprint, identity.kind),
        (Some(fallback()), Some(DeviceKind::Hostname))
    );
}

#[test]
fn classifies_by_the_filesystem_that_holds_the_id() {
    let cases: [(&str, Vec<&str>, bool); 16] = [
        ("nothing mounted at it", vec![ROOT], true),
        (
            "the path as a mount's root, not its mount point",
            vec![
                ROOT,
                "30 23 0:25 /etc/machine-id /run/host-id ro,relatime - tmpfs tmpfs rw",
            ],
            true,
        ),
        (
            "a mount point it is only a prefix of",
            vec![
                ROOT,
                "31 23 0:26 / /etc/machine-id.d rw,relatime - tmpfs tmpfs rw",
            ],
            true,
        ),
        ("an impermanent root", vec![ROOT_TMPFS], false),
        (
            "an impermanent /etc",
            vec![ROOT, "2 1 0:2 / /etc rw,relatime shared:2 - tmpfs none rw"],
            false,
        ),
        (
            "a tmpfs /etc with no optional field",
            vec![ROOT, "2 1 0:2 / /etc rw - tmpfs none rw"],
            false,
        ),
        (
            "a tmpfs /etc with two optional fields",
            vec![ROOT, "2 1 0:2 / /etc rw shared:2 master:1 - tmpfs tmpfs rw"],
            false,
        ),
        (
            "an overmount listed before the mount it covers",
            vec![
                "5 29 8:1 /etc/machine-id /etc/machine-id ro - ext4 /dev/sda1 rw",
                ROOT,
                OVER_MACHINE_ID,
            ],
            true,
        ),
        (
            "a mount point that is a prefix of the path but not a parent",
            vec![ROOT, "2 1 0:2 / /et rw - tmpfs tmpfs rw"],
            true,
        ),
        ("systemd's transient id", vec![ROOT, OVER_MACHINE_ID], false),
        (
            "an escaped mount point that unescapes to /etc",
            vec![ROOT, "2 1 0:2 / /et\\143 rw - tmpfs tmpfs rw"],
            false,
        ),
        (
            "a ramfs /etc",
            vec![ROOT, "2 1 0:2 / /etc rw master:4 - ramfs ramfs rw"],
            false,
        ),
        (
            "a stable id bound in from a disk",
            vec![
                ROOT,
                "3 1 8:1 /etc/machine-id /etc/machine-id ro,relatime - ext4 /dev/sda1 rw",
            ],
            true,
        ),
        (
            "a persisted id over an impermanent root",
            vec![
                ROOT_TMPFS,
                "3 1 8:2 /persist/etc/machine-id /etc/machine-id rw - btrfs /dev/sda2 rw",
            ],
            true,
        ),
        (
            "two mounts at one point, neither over the other, a tmpfs last",
            vec![
                ROOT,
                "3 1 8:1 /etc/machine-id /etc/machine-id ro - ext4 /dev/sda1 rw",
                "4 1 0:25 /machine-id /etc/machine-id ro - tmpfs tmpfs rw",
            ],
            false,
        ),
        (
            "two mounts at one point, neither over the other, a disk last",
            vec![
                ROOT,
                "4 1 0:25 /machine-id /etc/machine-id ro - tmpfs tmpfs rw",
                "3 1 8:1 /etc/machine-id /etc/machine-id ro - ext4 /dev/sda1 rw",
            ],
            true,
        ),
    ];
    for (case, lines, is_id) in cases {
        let expected = if is_id {
            id("a1b2c3d4e5f6")
        } else {
            MachineIdReading::Unavailable
        };
        assert_eq!(read(with_mounts(&lines)), expected, "{case}");
    }
    // A disk bind-mounted over a tmpfs one at the same point: the last is the one seen.
    let over = [
        ROOT,
        OVER_MACHINE_ID,
        "5 29 8:1 /etc/machine-id /etc/machine-id ro - ext4 /dev/sda1 rw",
    ];
    assert_eq!(read(with_mounts(&over)), id("a1b2c3d4e5f6"));
    assert_eq!(machine_id_filesystem(""), None);
    assert_eq!(machine_id_filesystem("garbage\n1 2 3\n"), None);
}

#[test]
fn reads_the_id_as_always_when_mountinfo_cannot_be_read() {
    for unreadable in [
        None,
        Some(ReadFailure::Transient),
        Some(ReadFailure::Unavailable),
    ] {
        let mut system = System::on("linux").file("/etc/machine-id", Ok("a1b2c3d4e5f6\n"));
        if let Some(failure) = unreadable {
            system = system.file("/proc/self/mountinfo", Err(failure));
        }
        assert_eq!(read(system.arc()), id("a1b2c3d4e5f6"));
    }
}

const BTIME: f64 = 1_757_000_000.0;
const BOOTED: f64 = BTIME * 1000.0;

fn booted(mtime: Result<f64, ReadFailure>, stat: Result<&str, ReadFailure>) -> Arc<System> {
    System::on("linux")
        .file("/proc/self/mountinfo", Ok(&format!("{ROOT}\n")))
        .file("/etc/machine-id", Ok("a1b2c3d4e5f6\n"))
        .file("/proc/stat", stat)
        .mtime(mtime)
        .arc()
}

#[test]
fn an_id_written_at_this_boot_is_no_id_to_hold_a_device_to() {
    let stat = "cpu 1 2 3\nbtime 1757000000\n";
    assert_eq!(
        read(booted(Ok(BOOTED + 3_000.0), Ok(stat))),
        MachineIdReading::Unavailable
    );
    assert_eq!(
        read(booted(Ok(BOOTED - 90.0 * 86_400_000.0), Ok(stat))),
        id("a1b2c3d4e5f6")
    );
    // The slack before btime counts as this boot, and not a millisecond more.
    assert_eq!(
        read(booted(Ok(BOOTED - BOOT_SLACK_MS), Ok(stat))),
        MachineIdReading::Unavailable
    );
    assert_eq!(
        read(booted(Ok(BOOTED - BOOT_SLACK_MS - 1.0), Ok(stat))),
        id("a1b2c3d4e5f6")
    );
    // Read as always when /proc/stat or the file's time cannot be read.
    assert_eq!(
        read(booted(Ok(BOOTED + 3_000.0), Err(ReadFailure::Unavailable))),
        id("a1b2c3d4e5f6")
    );
    assert_eq!(
        read(booted(Ok(BOOTED + 3_000.0), Ok("cpu 1 2 3\n"))),
        id("a1b2c3d4e5f6")
    );
    assert_eq!(
        read(booted(Err(ReadFailure::Unavailable), Ok(stat))),
        id("a1b2c3d4e5f6")
    );
    assert_eq!(boot_time("cpu 1\r\nbtime 42\r\n"), Some(42.0));
    assert_eq!(boot_time("xbtime 42\nbtime x\n"), None);
}

// --- the default fingerprinter --------------------------------------------------------------

#[test]
fn survives_a_rename_the_fingerprint_follows_the_machine_id() {
    let source = || {
        System::on("linux")
            .file("/etc/machine-id", Ok("same-machine"))
            .arc()
    };
    let before = DefaultFingerprinter::with_source(source())
        .get()
        .expect("a fingerprint");
    let after = DefaultFingerprinter::with_source(source())
        .get()
        .expect("a fingerprint");
    assert_eq!(before, after);
    assert_ne!(before, fallback());
}

#[test]
fn falls_back_to_the_host_name_for_good_when_the_machine_has_no_id_to_read() {
    let sources = [
        System::on("win32"),
        System::on("win32").guid(Err(ReadFailure::Unavailable)),
        System::on("darwin").output("/usr/sbin/ioreg", Ok("no uuid here")),
        System::on("darwin"),
        System::on("linux"),
        System::on("linux")
            .file("/etc/machine-id", Err(ReadFailure::Unavailable))
            .file("/var/lib/dbus/machine-id", Err(ReadFailure::Unavailable)),
        System::on("freebsd"),
    ];
    for source in sources {
        let source = source.arc();
        assert_eq!(read(Arc::clone(&source)), MachineIdReading::Unavailable);
        let fingerprinter = DefaultFingerprinter::with_source(source);
        assert_eq!(fingerprinter.get().expect("a fingerprint"), fallback());
        // Settled: kept, with no host-name alternate besides itself.
        let identity = fingerprinter.identify(Duration::from_millis(1));
        assert_eq!(
            identity,
            Some(DeviceIdentity {
                fingerprint: Some(fallback()),
                kind: Some(DeviceKind::Hostname),
                alternates: vec![]
            })
        );
    }
}

#[test]
fn gives_no_fingerprint_for_a_read_that_did_not_finish_reads_again_and_keeps_the_id_once_it_has_it()
{
    for source in [
        System::on("win32").guid(Err(ReadFailure::Transient)),
        System::on("darwin").output("/usr/sbin/ioreg", Err(ReadFailure::Transient)),
        System::on("linux").file("/etc/machine-id", Err(ReadFailure::Transient)),
    ] {
        let identity = DefaultFingerprinter::with_source(source.arc()).identify(MACHINE_ID_TIMEOUT);
        assert_eq!(
            identity,
            Some(DeviceIdentity {
                fingerprint: None,
                kind: None,
                alternates: vec![fallback()]
            })
        );
    }
    let machine = hash_fingerprint(&format!("machine-id|{GUID}"));
    let flaky = Arc::new(Flaky {
        reads: AtomicUsize::new(0),
    });
    let fingerprinter = DefaultFingerprinter::with_source(flaky.clone());
    assert_eq!(
        fingerprinter.identify(PATIENT).and_then(|i| i.fingerprint),
        None
    );
    assert_eq!(
        fingerprinter.identify(PATIENT).and_then(|i| i.fingerprint),
        Some(machine.clone())
    );
    assert_eq!(
        fingerprinter.identify(PATIENT).and_then(|i| i.fingerprint),
        Some(machine)
    );
    // Read twice: the one that did not finish, and the one kept after it.
    assert_eq!(flaky.reads.load(Ordering::SeqCst), 2);
    // And get() fails, rather than stand in, when the id cannot be read.
    let never = DefaultFingerprinter::with_source(
        System::on("win32").guid(Err(ReadFailure::Transient)).arc(),
    );
    assert!(never.get().is_err());
}

/// A Windows source whose first read does not finish.
struct Flaky {
    reads: AtomicUsize,
}

impl MachineIdSource for Flaky {
    fn platform(&self) -> String {
        "win32".into()
    }
    fn windows_machine_guid(&self) -> Result<String, ReadFailure> {
        if self.reads.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(ReadFailure::Transient);
        }
        Ok(GUID.into())
    }
    fn run(&self, _: &str, _: &[&str], _: Duration) -> Result<String, ReadFailure> {
        Err(ReadFailure::Unavailable)
    }
    fn read_file(&self, _: &str) -> Result<String, ReadFailure> {
        Err(ReadFailure::Unavailable)
    }
}

/// A source whose every read never finishes.
struct Hanging;

impl MachineIdSource for Hanging {
    fn platform(&self) -> String {
        "darwin".into()
    }
    fn windows_machine_guid(&self) -> Result<String, ReadFailure> {
        std::thread::sleep(Duration::from_secs(60));
        Err(ReadFailure::Transient)
    }
    fn run(&self, _: &str, _: &[&str], _: Duration) -> Result<String, ReadFailure> {
        std::thread::sleep(Duration::from_secs(60));
        Err(ReadFailure::Transient)
    }
    fn read_file(&self, _: &str) -> Result<String, ReadFailure> {
        std::thread::sleep(Duration::from_secs(60));
        Err(ReadFailure::Transient)
    }
}

#[test]
fn gives_no_fingerprint_once_the_time_box_is_out_when_the_read_never_finishes() {
    let started = Instant::now();
    assert_eq!(
        read_machine_id(Arc::new(Hanging), Duration::from_millis(200)),
        MachineIdReading::Transient
    );
    let took = started.elapsed();
    // At the bound, not when the read would end (60 s on).
    assert!(
        took >= Duration::from_millis(690) && took < Duration::from_secs(30),
        "{took:?}"
    );
}

#[test]
fn offers_the_host_name_fingerprint_as_an_alternate_beside_the_machine_ids() {
    let identity = DefaultFingerprinter::with_source(System::on("win32").guid(Ok(GUID)).arc())
        .identify(PATIENT);
    assert_eq!(
        identity,
        Some(DeviceIdentity {
            fingerprint: Some(hash_fingerprint(&format!("machine-id|{GUID}"))),
            kind: Some(DeviceKind::Machine),
            alternates: vec![fallback()],
        })
    );
}

#[test]
fn reads_again_after_a_read_that_did_not_finish_and_keeps_a_settled_one() {
    let source = System::on("win32").guid(Ok(GUID)).arc();
    let fingerprinter = DefaultFingerprinter::with_source(source.clone());
    for _ in 0..3 {
        fingerprinter.identify(PATIENT);
    }
    assert_eq!(source.guid_reads.load(Ordering::SeqCst), 1);
}

#[test]
fn tells_a_failure_for_good_from_one_this_time_only() {
    for kind in [
        ErrorKind::NotFound,
        ErrorKind::PermissionDenied,
        ErrorKind::NotADirectory,
        ErrorKind::IsADirectory,
    ] {
        assert_eq!(
            classify_io(&IoError::from(kind)),
            ReadFailure::Unavailable,
            "{kind:?}"
        );
    }
    for kind in [
        ErrorKind::WouldBlock,
        ErrorKind::Other,
        ErrorKind::TimedOut,
        ErrorKind::Interrupted,
        ErrorKind::ResourceBusy,
    ] {
        assert_eq!(
            classify_io(&IoError::from(kind)),
            ReadFailure::Transient,
            "{kind:?}"
        );
    }
}

// --- the real system ---------------------------------------------------------------------

#[test]
fn names_every_platform_and_architecture_as_node_does() {
    // Rust's `std::env::consts` names on the left, Node's on the right: a
    // machine hashes the same host-name fingerprint under either SDK.
    for (os, node) in [
        ("windows", "win32"),
        ("macos", "darwin"),
        ("linux", "linux"),
        ("freebsd", "freebsd"),
        ("openbsd", "openbsd"),
        ("netbsd", "netbsd"),
        ("android", "android"),
        ("aix", "aix"),
        ("solaris", "sunos"),
        ("illumos", "sunos"),
    ] {
        assert_eq!(node_platform_of(os), node, "{os}");
    }
    for (arch, node) in [
        ("x86_64", "x64"),
        ("aarch64", "arm64"),
        ("x86", "ia32"),
        ("arm", "arm"),
        ("powerpc64", "ppc64"),
        ("powerpc", "ppc"),
        ("s390x", "s390x"),
        ("riscv64", "riscv64"),
        ("loongarch64", "loong64"),
        ("mips64", "mips64el"),
    ] {
        assert_eq!(node_arch_of(arch), node, "{arch}");
    }
    // And this build is named by the same tables.
    assert_eq!(node_platform(), node_platform_of(std::env::consts::OS));
    assert_eq!(node_arch(), node_arch_of(std::env::consts::ARCH));
}

/// A command that prints `keygrant`, one that exits 3, and one that runs ~30 s.
#[cfg(windows)]
fn commands() -> [(&'static str, Vec<&'static str>); 3] {
    [
        ("cmd", vec!["/C", "echo keygrant"]),
        ("cmd", vec!["/C", "exit 3"]),
        ("ping", vec!["-n", "30", "127.0.0.1"]),
    ]
}

#[cfg(unix)]
fn commands() -> [(&'static str, Vec<&'static str>); 3] {
    [
        ("sh", vec!["-c", "echo keygrant"]),
        ("sh", vec!["-c", "exit 3"]),
        ("sleep", vec!["30"]),
    ]
}

#[test]
fn runs_a_command_and_tells_a_refusal_from_a_read_that_did_not_finish() {
    let source = SystemMachineIdSource;
    let bound = Duration::from_secs(10);
    let [(echo, echo_args), (exit, exit_args), (slow, slow_args)] = commands();
    let output = source.run(echo, &echo_args, bound);
    assert_eq!(
        output.map(|text| text.trim().to_owned()),
        Ok("keygrant".to_owned())
    );
    // The command ran and said no: for good.
    assert_eq!(
        source.run(exit, &exit_args, bound),
        Err(ReadFailure::Unavailable)
    );
    // There is no such command: for good.
    let missing = if cfg!(windows) {
        r"C:\keygrant\no-such-command.exe"
    } else {
        "/nonexistent/keygrant-no-such-command"
    };
    assert_eq!(
        source.run(missing, &[], bound),
        Err(ReadFailure::Unavailable)
    );
    // Killed at its bound: this time only, and at the bound, not 30 s on.
    let started = Instant::now();
    let answer = source.run(slow, &slow_args, Duration::from_millis(300));
    let took = started.elapsed();
    assert_eq!(answer, Err(ReadFailure::Transient));
    assert!(
        took >= Duration::from_millis(290) && took < Duration::from_secs(20),
        "{took:?}"
    );
}

#[test]
fn does_not_wait_past_the_bound_for_output_a_left_running_child_holds() {
    // The command ends at once, but a child it left running holds its output
    // open for 30 s more: the read is given up at the bound, not then.
    let (command, args): (&str, Vec<&str>) = if cfg!(windows) {
        (
            "cmd",
            vec!["/C", "start /b ping -n 31 127.0.0.1 & echo done"],
        )
    } else {
        ("sh", vec!["-c", "sleep 30 & echo done"])
    };
    let started = Instant::now();
    let answer = SystemMachineIdSource.run(command, &args, Duration::from_millis(1_500));
    let took = started.elapsed();
    assert_eq!(answer, Err(ReadFailure::Transient));
    assert!(
        took >= Duration::from_millis(1_400) && took < Duration::from_secs(20),
        "{took:?}"
    );
}

#[cfg(unix)]
#[test]
fn a_command_ended_by_a_signal_did_not_finish() {
    let answer = SystemMachineIdSource.run("sh", &["-c", "kill -KILL $$"], Duration::from_secs(10));
    assert_eq!(answer, Err(ReadFailure::Transient));
}

#[test]
fn the_system_fingerprinter_tells_who_this_machine_is() {
    let reading = read_machine_id(Arc::new(SystemMachineIdSource), Duration::from_secs(10));
    assert_ne!(reading, MachineIdReading::Transient);
    let identity = DefaultFingerprinter::new()
        .identify(Duration::from_secs(10))
        .expect("an identity");
    let fingerprint = identity.fingerprint.expect("a fingerprint");
    assert_eq!(fingerprint.len(), 64);
    assert!(fingerprint.bytes().all(|b| b.is_ascii_hexdigit()));
    if let MachineIdReading::Id(id) = reading {
        assert_eq!(fingerprint, hash_fingerprint(&format!("machine-id|{id}")));
        assert_eq!(identity.kind, Some(DeviceKind::Machine));
        assert_eq!(identity.alternates, vec![fallback()]);
    }
    // Kept for the process: a second fingerprinter answers the same at once.
    let again = DefaultFingerprinter::new()
        .identify(Duration::from_millis(1))
        .and_then(|i| i.fingerprint);
    assert_eq!(again, Some(fingerprint));
}
