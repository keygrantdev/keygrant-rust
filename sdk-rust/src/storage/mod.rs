//! The SDK's default state file: where it lives, and how it is read and
//! written so that neither a crash nor a busy disk ever costs a customer
//! their licence. The file format and the default folder are the Electron
//! SDK's, so the two share a state file.

mod fs;

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

pub use fs::{
    busy, missing, RealFs, Sleep, StorageFile, StorageFs, KEEP_CORRUPT, READ_BACKOFF_MS,
    RENAME_BACKOFF_MS, STALE_TEMP_MS,
};
use fs::{clear_stale, leftover, prune_corrupt, read_text, retrying, write_temp};

use crate::adapters::{system_now_ms, StorageAdapter};
use crate::state::StoredState;

/// How many times a save is merged onto a state another process changed
/// under it before it fails.
pub const UPDATE_ATTEMPTS: usize = 5;

/// The error a save fails with when the state kept changing under it
/// ([`UPDATE_ATTEMPTS`] times): wrapped in an `io::Error`, find it with
/// [`is_conflict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conflict;

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the state was saved by another process")
    }
}

impl std::error::Error for Conflict {}

/// Whether an error is a [`Conflict`].
pub fn is_conflict(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<Conflict>())
}

fn conflict() -> io::Error {
    io::Error::other(Conflict)
}

/// Where the default storage keeps a product's state: a folder of this
/// user's on THIS machine, never the working directory, and never one that
/// roams between machines:
///  - Windows: `%LOCALAPPDATA%\KeyGrant`, else `~\AppData\Local\KeyGrant`
///  - macOS: `~/Library/Application Support/KeyGrant`
///  - Linux and others: `$XDG_CONFIG_HOME/keygrant`, else `~/.config/keygrant`
///
/// An empty or relative variable is taken as unset. The home folder and the
/// variables are taken as the system gives them (`OsStr`), whatever their
/// encoding. `None` only when there is no home folder to put it in.
pub fn default_storage_path(product: &str) -> Option<PathBuf> {
    let home = std::env::home_dir().filter(|home| !home.as_os_str().is_empty())?;
    let env = |name: &str| std::env::var_os(name);
    Some(storage_path_for(
        product,
        crate::machine_id::node_platform(),
        &home,
        &env,
    ))
}

/// [`default_storage_path`] for a given system: its platform (as Node names
/// it), home folder and environment.
pub(crate) fn storage_path_for(
    product: &str,
    platform: &str,
    home: &Path,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> PathBuf {
    let file = format!("{product}.json");
    let set = |name: &str, absolute: fn(&str) -> bool| {
        env(name)
            .filter(|value| !value.is_empty() && absolute(&value.to_string_lossy()))
            .map(PathBuf::from)
    };
    match platform {
        "win32" => set("LOCALAPPDATA", win_absolute)
            .unwrap_or_else(|| home.join("AppData").join("Local"))
            .join("KeyGrant")
            .join(file),
        "darwin" => home
            .join("Library")
            .join("Application Support")
            .join("KeyGrant")
            .join(file),
        _ => set("XDG_CONFIG_HOME", |value| value.starts_with('/'))
            .unwrap_or_else(|| home.join(".config"))
            .join("keygrant")
            .join(file),
    }
}

/// Whether a Windows path is absolute, as Node's `win32.isAbsolute` judges it.
fn win_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    let separator = |b: u8| b == b'\\' || b == b'/';
    match bytes {
        [first, ..] if separator(*first) => true,
        [drive, b':', third, ..] => drive.is_ascii_alphabetic() && separator(*third),
        _ => false,
    }
}

/// Where SDKs before the per-user default kept a product's state: relative
/// to the working directory.
pub fn legacy_storage_path(product: &str) -> Option<PathBuf> {
    Some(
        std::env::current_dir()
            .ok()?
            .join(".keygrant")
            .join(format!("{product}.json")),
    )
}

/// How a [`FileStorage`] is made.
#[derive(Clone, Default)]
pub struct FileStorageOptions {
    /// A state an older SDK kept elsewhere, copied to the new path the first
    /// time the new one is missing (the old file is left where it is).
    pub migrate_from: Option<PathBuf>,
    /// The file operations (the real ones when `None`).
    pub fs: Option<Arc<dyn StorageFs>>,
    /// How to wait between tries (`thread::sleep` when `None`).
    pub sleep: Option<Sleep>,
}

/// JSON file storage, written so that a crash, a power cut or another
/// instance of the app never costs the licence.
///
/// Read: only a MISSING file is "no state". Then its newest whole temp file
/// is read (a write that finished but was never renamed in), else the state
/// an older SDK kept (`migrate_from`), copied over once. Any other failure to
/// read is tried again for about a second and then fails. A file that reads
/// but does not parse is set aside (renamed to `<file>.<time>.corrupt`, the
/// newest [`KEEP_CORRUPT`] kept) and an empty state written in its place. A
/// read-only read does neither the copy nor the set-aside: it writes nothing.
///
/// Written atomically: to a temp file beside it, flushed to the disk, then
/// renamed over it (tried again for a while when Windows says the file is
/// busy), and on POSIX the folder flushed after. A write that lands clears
/// the temps crashes left.
///
/// Saved (`update`) without losing another process's save: every save stamps
/// the state with the next revision (`rev`), and just before its rename reads
/// the file again; when another process has saved since, the patch is merged
/// onto that newer state and tried again, [`UPDATE_ATTEMPTS`] times at most.
#[derive(Clone)]
pub struct FileStorage {
    path: PathBuf,
    dir: PathBuf,
    prefix: String,
    fs: Arc<dyn StorageFs>,
    migrate_from: Option<PathBuf>,
    sleep: Sleep,
}

impl fmt::Debug for FileStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileStorage")
            .field("path", &self.path)
            .field("migrate_from", &self.migrate_from)
            .finish()
    }
}

/// What a load read: the state, and the revision of the FILE it was read
/// from (none when it came from anywhere else).
struct Loaded {
    state: Option<StoredState>,
    rev: Option<i64>,
}

/// The revision a file's text carries.
#[derive(PartialEq)]
enum Revision {
    /// Missing, or unstamped.
    Of(Option<i64>),
    /// It does not parse: equal to nothing.
    Unreadable,
}

impl FileStorage {
    /// Storage at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        FileStorage::with_options(path, FileStorageOptions::default())
    }

    /// Storage at `path`, made as `options` say.
    pub fn with_options(path: impl Into<PathBuf>, options: FileStorageOptions) -> Self {
        let path = path.into();
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        FileStorage {
            prefix: format!("{name}."),
            dir,
            path,
            fs: options.fs.unwrap_or_else(|| Arc::new(RealFs)),
            migrate_from: options.migrate_from,
            sleep: options
                .sleep
                .unwrap_or_else(|| Arc::new(std::thread::sleep)),
        }
    }

    /// The state file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A whole state put in place, once `allowed` (when given) has not failed,
    /// just before the rename.
    fn place(
        &self,
        state: &StoredState,
        allowed: Option<&dyn Fn() -> io::Result<()>>,
    ) -> io::Result<()> {
        let fs = self.fs.as_ref();
        let temp = write_temp(fs, &self.dir, &self.prefix, state)?;
        let rename_in = || {
            if let Some(allowed) = allowed {
                allowed()?;
            }
            fs.rename(&temp, &self.path)
        };
        if let Err(error) = retrying(rename_in, &RENAME_BACKOFF_MS, busy, &self.sleep) {
            let _ = fs.rm(&temp);
            return Err(error);
        }
        let _ = fs.sync_dir(&self.dir);
        clear_stale(fs, &self.dir, &self.prefix);
        Ok(())
    }

    fn load(&self, read_only: bool) -> io::Result<Loaded> {
        let fs = self.fs.as_ref();
        let Some(text) = read_text(fs, &self.path, &self.sleep)? else {
            let state = leftover(fs, &self.dir, &self.prefix).or_else(|| self.migrated(!read_only));
            return Ok(Loaded { state, rev: None });
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(value) => {
                let state = StoredState::from_json(&value);
                let rev = state.as_ref().and_then(|state| state.rev);
                Ok(Loaded { state, rev })
            }
            Err(_) if read_only => Ok(Loaded {
                state: None,
                rev: None,
            }),
            Err(_) => {
                // Kept aside before anything is written in its place; a rename
                // that cannot be made fails, as a read that cannot be made does.
                let mut aside = self.path.clone().into_os_string();
                aside.push(format!(".{}.corrupt", system_now_ms()));
                let aside = PathBuf::from(aside);
                retrying(
                    || fs.rename(&self.path, &aside),
                    &RENAME_BACKOFF_MS,
                    busy,
                    &self.sleep,
                )?;
                let _ = self.place(&StoredState::default(), None);
                prune_corrupt(fs, &self.dir, &self.prefix);
                Ok(Loaded {
                    state: None,
                    rev: None,
                })
            }
        }
    }

    /// The state an older SDK kept, copied to the new path once (`copy`) and
    /// returned. Never fails: a failed copy is tried again next time.
    fn migrated(&self, copy: bool) -> Option<StoredState> {
        let from = self.migrate_from.as_ref()?;
        let text = self.fs.read_file(from).ok()?;
        let value = serde_json::from_str::<Value>(&text).ok()?;
        let state = StoredState::from_json(&value)?;
        if copy {
            let _ = self.place(&state, None);
        }
        Some(state)
    }

    /// Fails with [`Conflict`] when the file's revision is no longer `rev`.
    fn unchanged(&self, rev: Option<i64>) -> io::Result<()> {
        let text = match self.fs.read_file(&self.path) {
            Ok(text) => Some(text),
            Err(error) if missing(&error) => None,
            Err(error) => return Err(error),
        };
        let revision = match text {
            None => Revision::Of(None),
            Some(text) => match serde_json::from_str::<Value>(&text) {
                Ok(value) => {
                    Revision::Of(StoredState::from_json(&value).and_then(|state| state.rev))
                }
                Err(_) => Revision::Unreadable,
            },
        };
        if revision == Revision::Of(rev) {
            Ok(())
        } else {
            Err(conflict())
        }
    }
}

impl StorageAdapter for FileStorage {
    fn read(&self, read_only: bool) -> io::Result<Option<StoredState>> {
        Ok(self.load(read_only)?.state)
    }

    fn write(&self, state: &StoredState) -> io::Result<()> {
        self.place(state, None)
    }

    fn update(&self, merge: &dyn Fn(StoredState) -> StoredState) -> Option<io::Result<()>> {
        Some(self.update_now(merge))
    }
}

impl FileStorage {
    fn update_now(&self, merge: &dyn Fn(StoredState) -> StoredState) -> io::Result<()> {
        let mut attempt = 1;
        loop {
            let base = self.load(false)?;
            let current = base.state.unwrap_or_default();
            let next_rev = current.rev.unwrap_or(0).saturating_add(1);
            let mut next = merge(current);
            next.rev = Some(next_rev);
            match self.place(&next, Some(&|| self.unchanged(base.rev))) {
                Ok(()) => return Ok(()),
                Err(error) if !is_conflict(&error) || attempt >= UPDATE_ATTEMPTS => {
                    return Err(error)
                }
                Err(_) => attempt += 1,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        move |name| {
            pairs
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| OsString::from(*v))
        }
    }

    fn path_for(
        platform: &str,
        home: &str,
        env: &'static [(&'static str, &'static str)],
    ) -> PathBuf {
        storage_path_for("sluice", platform, Path::new(home), &env_of(env))
    }

    #[test]
    fn keeps_state_per_user_on_this_machine_on_macos_and_linux() {
        // Compared as paths, so on any host.
        let darwin = path_for("darwin", "/Users/jo", &[]);
        assert_eq!(
            darwin,
            Path::new("/Users/jo/Library/Application Support/KeyGrant/sluice.json")
        );
        let linux = |env| path_for("linux", "/home/jo", env);
        let default = Path::new("/home/jo/.config/keygrant/sluice.json");
        assert_eq!(
            linux(&[("XDG_CONFIG_HOME", "/home/jo/.cfg")]),
            Path::new("/home/jo/.cfg/keygrant/sluice.json")
        );
        assert_eq!(linux(&[]), default);
        assert_eq!(linux(&[("XDG_CONFIG_HOME", "")]), default);
        assert_eq!(linux(&[("XDG_CONFIG_HOME", ".cfg")]), default);
        assert_eq!(
            linux(&[("XDG_CONFIG_HOME", "/cfg/")]),
            Path::new("/cfg/keygrant/sluice.json")
        );
    }

    #[cfg(windows)]
    #[test]
    fn keeps_state_per_user_on_this_machine_on_windows() {
        let windows = |env| path_for("win32", r"C:\Users\jo", env);
        let expected = Path::new(r"C:\Users\jo\AppData\Local\KeyGrant\sluice.json");
        assert_eq!(
            windows(&[("LOCALAPPDATA", r"C:\Users\jo\AppData\Local")]),
            expected
        );
        assert_eq!(
            windows(&[("LOCALAPPDATA", r"D:\Profiles\jo\Local\")]),
            Path::new(r"D:\Profiles\jo\Local\KeyGrant\sluice.json")
        );
        // Roaming AppData follows the user to every machine of a domain.
        assert_eq!(
            windows(&[("APPDATA", r"C:\Users\jo\AppData\Roaming")]),
            expected
        );
        assert_eq!(windows(&[]), expected);
        assert_eq!(windows(&[("LOCALAPPDATA", "")]), expected);
        assert_eq!(windows(&[("LOCALAPPDATA", r"AppData\Local")]), expected);
        assert_eq!(windows(&[("LOCALAPPDATA", "C:Local")]), expected);
    }

    #[test]
    fn judges_a_windows_path_absolute_as_node_does() {
        for absolute in [r"C:\x", "C:/x", r"\\server\share", r"\x", "/x", r"z:\"] {
            assert!(win_absolute(absolute), "{absolute}");
        }
        for relative in ["", "C:", "C:x", r"AppData\Local", "1:/x", "x"] {
            assert!(!win_absolute(relative), "{relative}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn keeps_a_home_folder_whose_name_is_not_unicode() {
        use std::os::windows::ffi::OsStringExt;
        // A lone surrogate: a name Windows allows and UTF-8 cannot hold.
        let mut name: Vec<u16> = r"C:\Users\jo".encode_utf16().collect();
        name.push(0xD800);
        let home = PathBuf::from(OsString::from_wide(&name));
        let path = storage_path_for("sluice", "win32", &home, &|_| None);
        assert!(path.starts_with(&home));
        assert!(path.ends_with(Path::new(r"AppData\Local\KeyGrant\sluice.json")));
    }

    #[cfg(unix)]
    #[test]
    fn keeps_a_home_folder_whose_name_is_not_unicode() {
        use std::os::unix::ffi::OsStringExt;
        let home = PathBuf::from(OsString::from_vec(b"/home/j\xffo".to_vec()));
        let path = storage_path_for("sluice", "linux", &home, &|_| None);
        assert!(path.starts_with(&home));
        assert!(path.ends_with(".config/keygrant/sluice.json"));
    }
}
