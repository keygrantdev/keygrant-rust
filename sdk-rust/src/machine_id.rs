//! The operating system's own id for this machine, which the default
//! fingerprinter hashes. It survives renaming the computer, which the host
//! name does not.
//!
//!  - Windows: `MachineGuid` under `HKLM\SOFTWARE\Microsoft\Cryptography`,
//!    read in-process from the 64-bit registry view (`KEY_WOW64_64KEY`), which
//!    a 32-bit process under WOW64 otherwise never sees.
//!  - macOS: `IOPlatformUUID` of the platform expert device, from
//!    `/usr/sbin/ioreg -rd1 -c IOPlatformExpertDevice`.
//!  - Linux (and anything else): `/etc/machine-id`, else
//!    `/var/lib/dbus/machine-id`, unless it is on a filesystem in memory or
//!    was written at this boot.
//!
//! What a read gives is one of three, and the difference is what keeps a
//! device's identity from ever changing because a read was slow:
//!  - `Id`: the machine's id.
//!  - `Unavailable`: there is none to read, and there will not be on the next
//!    try either. The fingerprint falls back to the host name, consistently.
//!  - `Transient`: the read did not finish (a timeout, a process killed, an
//!    error nobody expected). There is NO fingerprint this time, and the next
//!    call reads again.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::bounded::bounded;
use crate::wire::js_trim;

/// How long `status()` may spend reading the machine id.
pub const MACHINE_ID_TIMEOUT: Duration = Duration::from_millis(2_000);

/// How long `activate()` may spend reading it: a person is waiting, and a seat is at stake.
pub const ACTIVATE_MACHINE_ID_TIMEOUT: Duration = Duration::from_millis(10_000);

/// How far before the boot time an id may have been written and still count
/// as written at this boot (ms): `btime` is whole seconds.
pub const BOOT_SLACK_MS: f64 = 5_000.0;

/// What reading the machine id gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineIdReading {
    /// The machine's id.
    Id(String),
    /// There is none to read, now or on the next try.
    Unavailable,
    /// The read did not finish: no fingerprint this time.
    Transient,
}

/// Why a read failed: for good, or this time only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReadFailure {
    /// Nothing to read, now or on the next try (missing, not ours to read, a
    /// command that ran and said no).
    Unavailable,
    /// The read did not finish (killed at its time box, or anything unexpected).
    Transient,
}

/// Where the machine id is read from: the real system, or a stand-in in tests.
pub trait MachineIdSource: Send + Sync {
    /// The platform as Node names it: `win32`, `darwin`, `linux`, ...
    fn platform(&self) -> String;
    /// The `MachineGuid` registry value as stored (Windows).
    fn windows_machine_guid(&self) -> Result<String, ReadFailure>;
    /// A command's standard output, killed at `timeout`. A command that ran
    /// and exited non-zero, or could not be run because it is not there or
    /// not ours to run, is `Unavailable`; one killed or failing oddly is `Transient`.
    fn run(&self, command: &str, args: &[&str], timeout: Duration) -> Result<String, ReadFailure>;
    /// A file's text.
    fn read_file(&self, path: &str) -> Result<String, ReadFailure>;
    /// When a file's content was last written (ms since the epoch). The
    /// default cannot tell, and then the id's age is not judged.
    fn mtime_ms(&self, path: &str) -> Result<f64, ReadFailure> {
        let _ = path;
        Err(ReadFailure::Unavailable)
    }
}

/// The real system.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemMachineIdSource;

impl MachineIdSource for SystemMachineIdSource {
    fn platform(&self) -> String {
        node_platform().to_owned()
    }

    fn windows_machine_guid(&self) -> Result<String, ReadFailure> {
        windows_machine_guid()
    }

    fn run(&self, command: &str, args: &[&str], timeout: Duration) -> Result<String, ReadFailure> {
        run_with_timeout(command, args, timeout)
    }

    fn read_file(&self, path: &str) -> Result<String, ReadFailure> {
        std::fs::read(path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .map_err(|e| classify_io(&e))
    }

    fn mtime_ms(&self, path: &str) -> Result<f64, ReadFailure> {
        let modified = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map_err(|e| classify_io(&e))?;
        Ok(match modified.duration_since(UNIX_EPOCH) {
            Ok(since) => since.as_secs_f64() * 1000.0,
            Err(before) => -(before.duration().as_secs_f64() * 1000.0),
        })
    }
}

/// Whether an I/O failure is for good (missing, not ours to read, not a
/// file) or this time only (anything else).
pub fn classify_io(error: &std::io::Error) -> ReadFailure {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::NotFound
        | ErrorKind::PermissionDenied
        | ErrorKind::NotADirectory
        | ErrorKind::IsADirectory => ReadFailure::Unavailable,
        _ => ReadFailure::Transient,
    }
}

/// Where Windows keeps the machine id, under `HKEY_LOCAL_MACHINE`.
#[cfg(windows)]
pub(crate) const CRYPTOGRAPHY_KEY: &str = r"SOFTWARE\Microsoft\Cryptography";

/// The value that holds it.
#[cfg(windows)]
pub(crate) const MACHINE_GUID_VALUE: &str = "MachineGuid";

/// How the key is opened: to read, in the 64-bit view (`KEY_WOW64_64KEY`).
/// The view matters to a 32-bit (i686) build: under WOW64 it would otherwise
/// read the 32-bit view, where there is no `MachineGuid`, and every such
/// machine would fall back to its host name.
#[cfg(windows)]
pub(crate) const MACHINE_GUID_ACCESS: u32 =
    winreg::enums::KEY_READ | winreg::enums::KEY_WOW64_64KEY;

#[cfg(windows)]
fn windows_machine_guid() -> Result<String, ReadFailure> {
    use winreg::enums::{RegType, HKEY_LOCAL_MACHINE};
    use winreg::types::FromRegValue;
    use winreg::RegKey;
    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(CRYPTOGRAPHY_KEY, MACHINE_GUID_ACCESS)
        .map_err(|e| classify_io(&e))?;
    let raw = key
        .get_raw_value(MACHINE_GUID_VALUE)
        .map_err(|e| classify_io(&e))?;
    // `reg query` printed it only as a REG_SZ: anything else held no id.
    if raw.vtype != RegType::REG_SZ {
        return Err(ReadFailure::Unavailable);
    }
    String::from_reg_value(&raw).map_err(|_| ReadFailure::Unavailable)
}

#[cfg(not(windows))]
fn windows_machine_guid() -> Result<String, ReadFailure> {
    Err(ReadFailure::Unavailable)
}

/// A command's standard output, the command killed at `timeout`.
fn run_with_timeout(
    command: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<String, ReadFailure> {
    let mut builder = Command::new(command);
    builder
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: no console flashes up for it.
        builder.creation_flags(0x0800_0000);
    }
    let mut child = builder.spawn().map_err(|e| classify_io(&e))?;
    let stdout = child.stdout.take();
    // Read on a thread of its own, so a full pipe never stops the command;
    // the output is waited for no longer than the bound either (a child the
    // command left running could hold the pipe open after it ends).
    let (sender, output) = mpsc::sync_channel(1);
    let reading = thread::Builder::new()
        .name("keygrant-command".into())
        .spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut stdout) = stdout {
                let _ = stdout.read_to_end(&mut bytes);
            }
            let _ = sender.send(bytes);
        });
    if reading.is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ReadFailure::Transient);
    }
    let deadline = Instant::now().checked_add(timeout);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    // An exit code: the command ran and said no. None: a signal ended it.
                    return Err(if status.code().is_some() {
                        ReadFailure::Unavailable
                    } else {
                        ReadFailure::Transient
                    });
                }
                let left =
                    deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
                let bytes = match left {
                    Some(left) => output.recv_timeout(left).ok(),
                    None => output.recv().ok(),
                };
                return bytes
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .ok_or(ReadFailure::Transient);
            }
            Ok(None) if deadline.is_some_and(|deadline| Instant::now() >= deadline) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ReadFailure::Transient);
            }
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            Err(_) => {
                let _ = child.kill();
                return Err(ReadFailure::Transient);
            }
        }
    }
}

/// This platform as Node's `os.platform()` names it.
pub fn node_platform() -> &'static str {
    node_platform_of(std::env::consts::OS)
}

/// This build's architecture as Node's `os.arch()` names it: the binary's,
/// as Node's is (an x64 build under Rosetta or WOW64 is `x64`).
pub fn node_arch() -> &'static str {
    node_arch_of(std::env::consts::ARCH)
}

/// The platform Node names for Rust's `std::env::consts::OS` value `os`.
pub fn node_platform_of(os: &str) -> &str {
    match os {
        "windows" => "win32",
        "macos" => "darwin",
        "solaris" | "illumos" => "sunos",
        other => other,
    }
}

/// The architecture Node names for Rust's `std::env::consts::ARCH` value `arch`.
pub fn node_arch_of(arch: &str) -> &str {
    match arch {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        "powerpc64" => "ppc64",
        "powerpc" => "ppc",
        "loongarch64" => "loong64",
        "mips64" => "mips64el",
        other => other,
    }
}

/// The machine-id files Linux keeps: systemd's, else the D-Bus one.
const LINUX_MACHINE_ID_FILES: [&str; 2] = ["/etc/machine-id", "/var/lib/dbus/machine-id"];

/// The path whose filesystem says whether the id is transient.
const MACHINE_ID: &str = "/etc/machine-id";

/// This machine's id, or why there is none. Never takes much longer than
/// `timeout`: abandoned half a second after it.
pub fn read_machine_id(source: Arc<dyn MachineIdSource>, timeout: Duration) -> MachineIdReading {
    bounded(
        timeout.saturating_add(Duration::from_millis(500)),
        move || read_from(source.as_ref(), timeout),
    )
    .unwrap_or(MachineIdReading::Transient)
}

fn read_from(source: &dyn MachineIdSource, timeout: Duration) -> MachineIdReading {
    match source.platform().as_str() {
        // `reg query` printed `MachineGuid REG_SZ <value>`, and its first run
        // of non-blank characters was the id (`\S+`).
        "win32" => match source.windows_machine_guid() {
            Ok(value) => id_in(value.split(is_regex_space).find(|part| !part.is_empty())),
            Err(failure) => failed(failure),
        },
        "darwin" => match source.run(
            "/usr/sbin/ioreg",
            &["-rd1", "-c", "IOPlatformExpertDevice"],
            timeout,
        ) {
            Ok(output) => id_in(platform_uuid(&output)),
            Err(failure) => failed(failure),
        },
        _ => linux_machine_id(source),
    }
}

fn linux_machine_id(source: &dyn MachineIdSource) -> MachineIdReading {
    // An id on a filesystem in memory is new every boot: no id to hold a device to.
    if transient_machine_id(source) {
        return MachineIdReading::Unavailable;
    }
    for path in LINUX_MACHINE_ID_FILES {
        let reading = match source.read_file(path) {
            Ok(text) => {
                let id = js_trim(&text);
                if id.is_empty() {
                    MachineIdReading::Unavailable
                } else {
                    MachineIdReading::Id(id.to_owned())
                }
            }
            Err(failure) => failed(failure),
        };
        match reading {
            MachineIdReading::Unavailable => continue,
            // Written at this boot: a volatile root, whatever its filesystem.
            MachineIdReading::Id(_) if written_this_boot(source, path) => {
                return MachineIdReading::Unavailable
            }
            other => return other,
        }
    }
    MachineIdReading::Unavailable
}

fn failed(failure: ReadFailure) -> MachineIdReading {
    match failure {
        ReadFailure::Unavailable => MachineIdReading::Unavailable,
        ReadFailure::Transient => MachineIdReading::Transient,
    }
}

/// The id a read found, trimmed; none, or nothing left, is unavailable.
fn id_in(value: Option<&str>) -> MachineIdReading {
    match value.map(js_trim).filter(|id| !id.is_empty()) {
        Some(id) => MachineIdReading::Id(id.to_owned()),
        None => MachineIdReading::Unavailable,
    }
}

/// The `IOPlatformUUID` in `ioreg` output (`"IOPlatformUUID" = "<uuid>"`).
pub fn platform_uuid(output: &str) -> Option<&str> {
    const NAME: &str = "\"IOPlatformUUID\"";
    let mut from = 0;
    while let Some(found) = output[from..].find(NAME) {
        let after = &output[from + found + NAME.len()..];
        if let Some(rest) = after.trim_start_matches(is_regex_space).strip_prefix('=') {
            if let Some(quoted) = rest.trim_start_matches(is_regex_space).strip_prefix('"') {
                if let Some(end) = quoted.find('"') {
                    if end > 0 {
                        return Some(&quoted[..end]);
                    }
                }
            }
        }
        from += found + NAME.len();
    }
    None
}

/// What a JavaScript regular expression's `\s` matches.
fn is_regex_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{FEFF}'
}

/// Filesystems held in memory: an id on one of them is written afresh every boot.
const IN_MEMORY: [&str; 2] = ["tmpfs", "ramfs"];

/// Whether /etc/machine-id lives on a filesystem in memory. False when
/// mountinfo cannot be read, which leaves the id read as it always was.
fn transient_machine_id(source: &dyn MachineIdSource) -> bool {
    match source.read_file("/proc/self/mountinfo") {
        Ok(info) => {
            machine_id_filesystem(&info).is_some_and(|fstype| IN_MEMORY.contains(&fstype.as_str()))
        }
        Err(_) => false,
    }
}

struct Mount {
    id: String,
    parent: String,
    point: String,
    fstype: String,
}

/// The type of the filesystem /etc/machine-id is on, from
/// `/proc/self/mountinfo`: the mount whose mount point is the longest prefix
/// of the path on a path boundary; of several at that point, the one on top.
/// None when no line can be read so.
pub fn machine_id_filesystem(mountinfo: &str) -> Option<String> {
    let mut mounts = Vec::new();
    for line in mountinfo.split('\n') {
        let fields: Vec<&str> = line.split(' ').collect();
        let separator = fields
            .iter()
            .skip(6)
            .position(|field| *field == "-")
            .map(|at| at + 6);
        let fstype = separator.and_then(|at| fields.get(at + 1));
        let (Some(id), Some(parent), Some(raw), Some(fstype)) =
            (fields.first(), fields.get(1), fields.get(4), fstype)
        else {
            continue;
        };
        let point = unescape_octal(raw);
        let holds =
            point == MACHINE_ID || point == "/" || MACHINE_ID.starts_with(&format!("{point}/"));
        if holds {
            mounts.push(Mount {
                id: (*id).to_owned(),
                parent: (*parent).to_owned(),
                point,
                fstype: (*fstype).to_owned(),
            });
        }
    }
    let deepest = mounts.iter().map(|mount| mount.point.len()).max()?;
    let at_point: Vec<&Mount> = mounts
        .iter()
        .filter(|mount| mount.point.len() == deepest)
        .collect();
    top_mount(&at_point).map(|mount| mount.fstype.clone())
}

/// Of mounts at one point, the one on top: the one no other there names as
/// its parent. When that does not settle it, the last listed.
fn top_mount<'a>(at_point: &[&'a Mount]) -> Option<&'a Mount> {
    let tops: Vec<&Mount> = at_point
        .iter()
        .enumerate()
        .filter(|(index, mount)| {
            !at_point
                .iter()
                .enumerate()
                .any(|(other, candidate)| other != *index && candidate.parent == mount.id)
        })
        .map(|(_, mount)| *mount)
        .collect();
    match tops.as_slice() {
        [only] => Some(*only),
        _ => at_point.last().copied(),
    }
}

/// A mount point with its `\ooo` octal escapes undone.
fn unescape_octal(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::with_capacity(raw.len());
    let mut at = 0;
    while at < chars.len() {
        let octal = chars
            .get(at + 1..at + 4)
            .filter(|digits| digits.iter().all(|d| ('0'..='7').contains(d)));
        match (chars[at], octal) {
            ('\\', Some(digits)) => {
                let code = digits
                    .iter()
                    .fold(0u32, |n, d| n * 8 + (*d as u32 - '0' as u32));
                out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                at += 4;
            }
            (c, _) => {
                out.push(c);
                at += 1;
            }
        }
    }
    out
}

/// Whether the machine-id file was written at this boot: its mtime no older
/// than the boot time (`btime` in `/proc/stat`) less [`BOOT_SLACK_MS`]. False
/// when either cannot be read. mtime only, never ctime.
fn written_this_boot(source: &dyn MachineIdSource, path: &str) -> bool {
    let (Ok(written), Ok(stat)) = (source.mtime_ms(path), source.read_file("/proc/stat")) else {
        return false;
    };
    match boot_time(&stat) {
        Some(btime) => written >= btime * 1000.0 - BOOT_SLACK_MS,
        None => false,
    }
}

/// `btime` from `/proc/stat` (seconds): the line `btime <digits>`.
pub fn boot_time(stat: &str) -> Option<f64> {
    stat.split(['\n', '\r', '\u{2028}', '\u{2029}'])
        .filter_map(|line| line.strip_prefix("btime "))
        .find(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|digits| digits.parse::<f64>().ok())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use winreg::enums::{KEY_QUERY_VALUE, KEY_WOW64_32KEY, KEY_WOW64_64KEY};

    #[test]
    fn reads_the_machine_guid_from_the_64_bit_view_whatever_this_build_is() {
        // Without KEY_WOW64_64KEY a 32-bit build reads the 32-bit view, where
        // there is no MachineGuid: every such machine would be its host name.
        assert_eq!(MACHINE_GUID_ACCESS & KEY_WOW64_64KEY, KEY_WOW64_64KEY);
        assert_eq!(MACHINE_GUID_ACCESS & KEY_WOW64_32KEY, 0);
        assert_eq!(MACHINE_GUID_ACCESS & KEY_QUERY_VALUE, KEY_QUERY_VALUE);
        assert_eq!(CRYPTOGRAPHY_KEY, r"SOFTWARE\Microsoft\Cryptography");
        assert_eq!(MACHINE_GUID_VALUE, "MachineGuid");
        // And it is there to read on this machine.
        let guid = windows_machine_guid().expect("a MachineGuid");
        assert!(!js_trim(&guid).is_empty());
    }
}
