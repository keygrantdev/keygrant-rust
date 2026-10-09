//! The file operations under the default storage, and what it does around
//! them: tries again on a busy file, a temp file synced to the disk, and the
//! clearing of what crashes and corrupt files leave behind.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use crate::adapters::system_now_ms;
use crate::state::StoredState;

/// A new file being written: what is written is flushed to the disk by `sync`.
pub trait StorageFile: Send {
    /// Write the whole text.
    fn write(&mut self, data: &str) -> io::Result<()>;
    /// Flush it to the disk.
    fn sync(&mut self) -> io::Result<()>;
    /// Close it.
    fn close(self: Box<Self>) -> io::Result<()>;
}

/// The file operations the default storage uses: the real ones
/// ([`RealFs`]), or stand-ins (in tests, or a sandbox of the app's own).
pub trait StorageFs: Send + Sync {
    /// A file's text (invalid UTF-8 replaced, as Node reads `utf8`).
    fn read_file(&self, path: &Path) -> io::Result<String>;
    /// A new file to write, failing when there is one by that name.
    fn create(&self, path: &Path) -> io::Result<Box<dyn StorageFile>>;
    /// Rename `from` over `to`.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Remove a file; a missing one is no error.
    fn rm(&self, path: &Path) -> io::Result<()>;
    /// The names in a folder.
    fn readdir(&self, dir: &Path) -> io::Result<Vec<String>>;
    /// When a file was last written (ms since the epoch).
    fn mtime_ms(&self, path: &Path) -> io::Result<f64>;
    /// Make a folder and its parents.
    fn mkdir(&self, dir: &Path) -> io::Result<()>;
    /// Flush a folder's entries (a rename in it) to the disk.
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;
}

/// The real file system.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealFs;

struct RealFile(File);

impl StorageFile for RealFile {
    fn write(&mut self, data: &str) -> io::Result<()> {
        self.0.write_all(data.as_bytes())
    }
    fn sync(&mut self) -> io::Result<()> {
        self.0.sync_all()
    }
    fn close(self: Box<Self>) -> io::Result<()> {
        drop(self);
        Ok(())
    }
}

impl StorageFs for RealFs {
    fn read_file(&self, path: &Path) -> io::Result<String> {
        std::fs::read(path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }
    fn create(&self, path: &Path) -> io::Result<Box<dyn StorageFile>> {
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        Ok(Box::new(RealFile(file)))
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
    fn rm(&self, path: &Path) -> io::Result<()> {
        match std::fs::remove_file(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
    fn readdir(&self, dir: &Path) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            if let Some(name) = entry?.file_name().to_str() {
                names.push(name.to_owned());
            }
        }
        Ok(names)
    }
    fn mtime_ms(&self, path: &Path) -> io::Result<f64> {
        let modified = std::fs::metadata(path)?.modified()?;
        Ok(match modified.duration_since(UNIX_EPOCH) {
            Ok(since) => since.as_secs_f64() * 1000.0,
            Err(before) => -(before.duration().as_secs_f64() * 1000.0),
        })
    }
    fn mkdir(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)
    }
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        // Windows opens no handle on a folder to flush; NTFS journals the rename itself.
        if cfg!(windows) {
            return Ok(());
        }
        File::open(dir)?.sync_all()
    }
}

/// Waits between tries of a read that failed other than for a missing file:
/// about a second in all (ms).
pub const READ_BACKOFF_MS: [u64; 5] = [50, 100, 200, 300, 350];

/// Waits between tries of a rename a busy file refused: about a second and a
/// half in all (ms).
pub const RENAME_BACKOFF_MS: [u64; 5] = [50, 100, 200, 400, 800];

/// How many set-aside `.corrupt` files of a state file are kept: the newest.
pub const KEEP_CORRUPT: usize = 3;

/// How old a temp file must be (ms) to be a crash's leftover rather than
/// another app instance's write in flight.
pub const STALE_TEMP_MS: f64 = 60_000.0;

/// An error a busy file gives a rename on Windows (an antivirus scan, an
/// indexer, a backup), worth another try: `EBUSY`, `EPERM`, `EACCES`, and
/// Windows' sharing and lock violations.
pub fn busy(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::ResourceBusy
    ) || (cfg!(windows) && matches!(error.raw_os_error(), Some(32) | Some(33)))
}

/// A missing file.
pub fn missing(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

/// How the storage sleeps between tries.
pub type Sleep = std::sync::Arc<dyn Fn(Duration) + Send + Sync>;

/// `work`, tried again after each wait in `backoff` while its error is worth
/// another try; the last error otherwise.
pub(crate) fn retrying<T>(
    mut work: impl FnMut() -> io::Result<T>,
    backoff: &[u64],
    worth_again: impl Fn(&io::Error) -> bool,
    sleep: &Sleep,
) -> io::Result<T> {
    for wait in backoff {
        match work() {
            Ok(value) => return Ok(value),
            Err(error) if worth_again(&error) => sleep(Duration::from_millis(*wait)),
            Err(error) => return Err(error),
        }
    }
    work()
}

/// A file's text, tried again for about a second on any failure but a
/// missing file; `None` when it is missing.
pub(crate) fn read_text(
    fs: &dyn StorageFs,
    path: &Path,
    sleep: &Sleep,
) -> io::Result<Option<String>> {
    match retrying(
        || fs.read_file(path),
        &READ_BACKOFF_MS,
        |e| !missing(e),
        sleep,
    ) {
        Ok(text) => Ok(Some(text)),
        Err(error) if missing(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A name no other write shares: this process, the time, and a counter.
fn unique() -> String {
    let count = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{}{}", base36(u64::from(nanos)), base36(count))
}

fn base36(mut n: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8_lossy(&out).into_owned()
}

/// A state written whole to a new temp file in `dir` and flushed to the disk
/// before it is closed. Removed when it fails.
pub(crate) fn write_temp(
    fs: &dyn StorageFs,
    dir: &Path,
    prefix: &str,
    state: &StoredState,
) -> io::Result<PathBuf> {
    fs.mkdir(dir)?;
    let temp = dir.join(format!(
        "{prefix}{}.{}.{}.tmp",
        std::process::id(),
        system_now_ms(),
        unique()
    ));
    let written = (|| {
        let mut file = fs.create(&temp)?;
        let wrote = file
            .write(&state.to_json().to_string())
            .and_then(|()| file.sync());
        let closed = file.close();
        wrote.and(closed)
    })();
    match written {
        Ok(()) => Ok(temp),
        Err(error) => {
            let _ = fs.rm(&temp);
            Err(error)
        }
    }
}

/// The temp files of a storage file in `dir`.
fn temps(fs: &dyn StorageFs, dir: &Path, prefix: &str) -> io::Result<Vec<String>> {
    Ok(fs
        .readdir(dir)?
        .into_iter()
        .filter(|name| name.starts_with(prefix) && name.ends_with(".tmp"))
        .collect())
}

/// The newest temp file of a storage file that parses, when the file itself
/// is missing. Never fails.
pub(crate) fn leftover(fs: &dyn StorageFs, dir: &Path, prefix: &str) -> Option<StoredState> {
    let names = temps(fs, dir, prefix).ok()?;
    let mut dated = Vec::with_capacity(names.len());
    for name in names {
        let at = fs.mtime_ms(&dir.join(&name)).ok()?;
        dated.push((name, at));
    }
    dated.sort_by(|a, b| b.1.total_cmp(&a.1));
    for (name, _) in dated {
        let Ok(text) = fs.read_file(&dir.join(&name)) else {
            continue;
        };
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            return StoredState::from_json(&value);
        }
    }
    None
}

/// Remove a storage file's temp files older than [`STALE_TEMP_MS`]. Best-effort.
pub(crate) fn clear_stale(fs: &dyn StorageFs, dir: &Path, prefix: &str) {
    let Ok(names) = temps(fs, dir, prefix) else {
        return;
    };
    let now = system_now_ms() as f64;
    for name in names {
        let path = dir.join(&name);
        let at = fs.mtime_ms(&path).unwrap_or(now);
        if now - at >= STALE_TEMP_MS {
            let _ = fs.rm(&path);
        }
    }
}

/// Remove all but the newest [`KEEP_CORRUPT`] set-aside files of a state
/// file, newest by the time in their names. Best-effort.
pub(crate) fn prune_corrupt(fs: &dyn StorageFs, dir: &Path, prefix: &str) {
    let Ok(names) = fs.readdir(dir) else {
        return;
    };
    let mut aside: Vec<(String, f64)> = names
        .into_iter()
        .filter_map(|name| {
            let digits = name.strip_prefix(prefix)?.strip_suffix(".corrupt")?;
            let numeric = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
            let at = digits.parse::<f64>().ok().filter(|_| numeric)?;
            Some((name, at))
        })
        .collect();
    aside.sort_by(|a, b| b.1.total_cmp(&a.1));
    for (name, _) in aside.into_iter().skip(KEEP_CORRUPT) {
        let _ = fs.rm(&dir.join(name));
    }
}
