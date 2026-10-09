//! The default file storage: atomic writes, reads that only take a MISSING
//! file as no state, the corrupt set-aside, migration, read-only reads, and
//! saves from more than one process.

mod common;

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use common::*;
use keygrant::lease::OPEN_MAJOR;
use keygrant::offline::{merged, About, Asked, Basis};
use keygrant::storage::{
    is_conflict, FileStorage, FileStorageOptions, KEEP_CORRUPT, STALE_TEMP_MS, UPDATE_ATTEMPTS,
};
use keygrant::{
    DeviceIdentity, DeviceKind, Fingerprinter, License, LicenseConfig, LicenseReason, LicenseState,
    LicenseStatus, Patch, StorageAdapter, StoredState,
};
use serde_json::{json, Value};

/// Waits that take no time, recorded.
#[derive(Clone, Default)]
struct Slept(Arc<Mutex<Vec<u64>>>);

impl Slept {
    fn sleep(&self) -> keygrant::storage::Sleep {
        let slept = Arc::clone(&self.0);
        Arc::new(move |d: Duration| {
            lock(&slept).push(u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        })
    }
    fn waits(&self) -> Vec<u64> {
        lock(&self.0).clone()
    }
    fn total(&self) -> u64 {
        self.waits().iter().sum()
    }
}

fn state_dir() -> PathBuf {
    std::env::temp_dir().join("keygrant-memory")
}

fn state_path() -> PathBuf {
    state_dir().join("sluice.json")
}

fn sibling(suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{suffix}", state_path().display()))
}

const SAVED: &str = r#"{"key":"SLUICE-AAAA-BBBB-CCCC-DDDD","activationId":"act_1","lastSeen":1}"#;

fn saved() -> StoredState {
    StoredState::from_json(&serde_json::from_str(SAVED).expect("JSON")).expect("a state")
}

fn over_memory(memory: &MemoryFs, slept: &Slept) -> FileStorage {
    FileStorage::with_options(
        state_path(),
        FileStorageOptions {
            fs: Some(Arc::new(memory.clone())),
            sleep: Some(slept.sleep()),
            ..FileStorageOptions::default()
        },
    )
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).expect("JSON")
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("a folder")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

fn set_age(path: &Path, age: Duration) {
    let file = std::fs::File::options()
        .write(true)
        .open(path)
        .expect("a file");
    file.set_modified(SystemTime::now() - age)
        .expect("an mtime");
}

fn k(key: &str, last_seen: i64) -> StoredState {
    StoredState {
        key: Some(key.into()),
        last_seen: Some(last_seen),
        ..StoredState::default()
    }
}

// --- writes ---------------------------------------------------------------------------

#[test]
fn writes_by_renaming_a_temp_over_the_state_and_leaves_no_temp() {
    let dir = tempfile::tempdir().expect("a temp folder");
    let path = dir.path().join("sluice.json");
    let storage = FileStorage::new(&path);
    storage.write(&k("K", 1)).expect("written");
    storage.write(&k("K", 2)).expect("written");
    assert_eq!(
        parse(&std::fs::read_to_string(&path).expect("the state")),
        json!({ "key": "K", "lastSeen": 2 })
    );
    assert_eq!(names_in(dir.path()), ["sluice.json"]);
}

#[test]
fn replaces_the_state_rather_than_writing_into_it() {
    let dir = tempfile::tempdir().expect("a temp folder");
    let path = dir.path().join("sluice.json");
    let storage = FileStorage::new(&path);
    storage.write(&k("K", 1)).expect("written");
    std::fs::hard_link(&path, dir.path().join("old-state")).expect("a link");
    storage.write(&k("K", 2)).expect("written");
    assert_eq!(
        parse(&std::fs::read_to_string(dir.path().join("old-state")).expect("old")),
        json!({ "key": "K", "lastSeen": 1 })
    );
    assert_eq!(
        parse(&std::fs::read_to_string(&path).expect("new")),
        json!({ "key": "K", "lastSeen": 2 })
    );
}

#[test]
fn clears_the_temps_crashes_left_once_a_write_lands_and_not_another_instances_write_in_flight() {
    let dir = tempfile::tempdir().expect("a temp folder");
    let file = |name: &str, text: &str, age: Option<Duration>| {
        let path = dir.path().join(name);
        std::fs::write(&path, text).expect("written");
        if let Some(age) = age {
            set_age(&path, age);
        }
    };
    let stale = Duration::from_millis(STALE_TEMP_MS as u64);
    file(
        "sluice.json.1.1.crashed.tmp",
        r#"{"key":"CRASHED"}"#,
        Some(stale * 2),
    );
    file("sluice.json.2.2.inflight.tmp", r#"{"key":"IN"#, None);
    // Another product's temp in the same folder is not this file's to clear.
    file("maindeck.json.3.3.other.tmp", "{}", Some(stale * 2));
    // Half the limit either side of it: a machine that stalls between making these files and the write
    // (a busy CI runner stalled past five seconds once) still finds the younger young and the older old.
    file("sluice.json.4.4.younger.tmp", "{}", Some(stale / 2));
    file("sluice.json.5.5.older.tmp", "{}", Some(stale * 3 / 2));
    FileStorage::new(dir.path().join("sluice.json"))
        .write(&k("K", 1))
        .expect("written");
    assert_eq!(
        names_in(dir.path()),
        [
            "maindeck.json.3.3.other.tmp",
            "sluice.json",
            "sluice.json.2.2.inflight.tmp",
            "sluice.json.4.4.younger.tmp"
        ]
    );
    assert_eq!(STALE_TEMP_MS, 60_000.0);
}

#[test]
fn leaves_the_state_as_it_was_and_no_temp_when_a_write_fails() {
    let dir = tempfile::tempdir().expect("a temp folder");
    // The state path is a folder: the rename over it fails.
    let path = dir.path().join("sluice.json");
    std::fs::create_dir(&path).expect("a folder");
    let slept = Slept::default();
    let storage = FileStorage::with_options(
        &path,
        FileStorageOptions {
            sleep: Some(slept.sleep()),
            ..FileStorageOptions::default()
        },
    );
    assert!(storage.write(&k("K", 1)).is_err());
    assert_eq!(names_in(dir.path()), ["sluice.json"]);
}

/// Whole milliseconds, as the storage reads the clock (like JavaScript's `Date.now()`).
fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("after the epoch")
        .as_millis() as f64
}

#[test]
fn clears_a_temp_aged_the_cutoff_and_keeps_a_younger_one() {
    // The storage reads the system clock itself, so the cutoff is pinned to the millisecond by a write
    // made within one: the clock is read before and after it, and a try whose two readings differ is
    // made again, on fresh files. Aged exactly the cutoff is cleared; a millisecond younger is kept.
    let edge = sibling(".1.1.edge.tmp");
    let inside = sibling(".2.2.inside.tmp");
    for _ in 0..1_000 {
        let now = now_ms();
        let memory = MemoryFs::new(&[(&state_path(), SAVED), (&edge, "{}"), (&inside, "{}")]);
        memory.set_mtime(&edge, now - STALE_TEMP_MS);
        memory.set_mtime(&inside, now - STALE_TEMP_MS + 1.0);
        over_memory(&memory, &Slept::default())
            .write(&k("K", 1))
            .expect("written");
        if now_ms() == now {
            assert_eq!(memory.names(), [state_path(), inside]);
            return;
        }
    }
    panic!("no write ran within one millisecond in 1,000 tries");
}

#[test]
fn tries_a_rename_windows_refuses_as_busy_again() {
    for kind in [ErrorKind::PermissionDenied, ErrorKind::ResourceBusy] {
        let memory = MemoryFs::new(&[(&state_path(), SAVED)]);
        memory.fail_next(Op::Rename, &[kind, kind]);
        let slept = Slept::default();
        over_memory(&memory, &slept)
            .write(&k("K", 2))
            .expect("written");
        assert_eq!(
            parse(&memory.file(&state_path()).expect("the state")),
            json!({ "key": "K", "lastSeen": 2 })
        );
        assert_eq!(slept.waits().len(), 2);
        assert_eq!(memory.names(), [state_path()]);
    }
}

#[test]
fn gives_up_on_a_rename_that_stays_busy_keeping_the_state_and_removing_its_temp() {
    let memory = MemoryFs::new(&[(&state_path(), SAVED)]);
    memory.fail_always(Op::Rename, ErrorKind::ResourceBusy);
    let slept = Slept::default();
    let error = over_memory(&memory, &slept)
        .write(&k("K", 1))
        .expect_err("busy");
    assert_eq!(error.kind(), ErrorKind::ResourceBusy);
    assert!(
        (1_000..=2_000).contains(&slept.total()),
        "{}",
        slept.total()
    );
    assert_eq!(memory.names(), [state_path()]);
    assert_eq!(memory.file(&state_path()).as_deref(), Some(SAVED));
}

#[test]
fn does_not_try_again_a_rename_that_failed_other_than_for_a_busy_file() {
    let memory = MemoryFs::new(&[(&state_path(), SAVED)]);
    memory.fail_next(Op::Rename, &[ErrorKind::StorageFull]);
    let slept = Slept::default();
    let error = over_memory(&memory, &slept)
        .write(&k("K", 1))
        .expect_err("full");
    assert_eq!(error.kind(), ErrorKind::StorageFull);
    assert!(slept.waits().is_empty());
    assert_eq!(memory.names(), [state_path()]);
}

#[test]
fn flushes_and_closes_the_temp_before_the_rename_and_the_folder_after() {
    let memory = MemoryFs::new(&[(&state_path(), SAVED)]);
    over_memory(&memory, &Slept::default())
        .write(&k("K", 1))
        .expect("written");
    let steps: Vec<String> = memory
        .ops()
        .iter()
        .map(|op| op.split(' ').next().unwrap_or_default().to_owned())
        .filter(|op| !["Readdir", "MtimeMs", "Mkdir"].contains(&op.as_str()))
        .collect();
    assert_eq!(
        steps,
        ["Create", "Write", "Sync", "Close", "Rename", "SyncDir"]
    );
}

#[test]
fn fails_a_write_whose_temp_cannot_be_flushed_and_fails_nothing_when_the_folder_cannot_be() {
    let memory = MemoryFs::new(&[(&state_path(), SAVED)]);
    memory.fail_next(Op::Sync, &[ErrorKind::Other]);
    assert!(over_memory(&memory, &Slept::default())
        .write(&k("K", 1))
        .is_err());
    assert_eq!(memory.names(), [state_path()]);
    assert_eq!(memory.file(&state_path()).as_deref(), Some(SAVED));
    let memory = MemoryFs::new(&[(&state_path(), SAVED)]);
    memory.fail_next(Op::SyncDir, &[ErrorKind::InvalidInput]);
    over_memory(&memory, &Slept::default())
        .write(&k("K", 1))
        .expect("written");
    assert_eq!(
        parse(&memory.file(&state_path()).expect("the state")),
        json!({ "key": "K", "lastSeen": 1 })
    );
}

// --- reads ----------------------------------------------------------------------------

#[test]
fn reads_none_when_missing_then_round_trips() {
    let dir = tempfile::tempdir().expect("a temp folder");
    let storage = FileStorage::new(dir.path().join("nested").join("license.json"));
    assert_eq!(storage.read(false).expect("read"), None);
    let state = StoredState {
        key: Some("K".into()),
        activation_id: Some("a".into()),
        last_seen: Some(42),
        ..StoredState::default()
    };
    storage.write(&state).expect("written");
    assert_eq!(storage.read(false).expect("read"), Some(state));
}

#[test]
fn reads_the_state_it_had_when_a_write_crashed_before_its_rename() {
    let dir = tempfile::tempdir().expect("a temp folder");
    let path = dir.path().join("sluice.json");
    FileStorage::new(&path)
        .write(&k("OLD", 1))
        .expect("written");
    std::fs::write(
        dir.path().join("sluice.json.123.456.abc.tmp"),
        r#"{"key":"NE"#,
    )
    .expect("written");
    assert_eq!(
        FileStorage::new(&path).read(false).expect("read"),
        Some(k("OLD", 1))
    );
}

#[test]
fn reads_the_newest_whole_temp_when_the_state_is_missing_by_its_time_not_its_name() {
    let dir = tempfile::tempdir().expect("a temp folder");
    let path = dir.path().join("sluice.json");
    let newer = dir.path().join("sluice.json.1.1.a.tmp");
    let older = dir.path().join("sluice.json.9.9.z.tmp");
    let torn = dir.path().join("sluice.json.5.5.m.tmp");
    std::fs::write(&newer, r#"{"key":"NEW"}"#).expect("written");
    std::fs::write(&older, r#"{"key":"OLD"}"#).expect("written");
    std::fs::write(&torn, r#"{"key":"TOR"#).expect("written");
    set_age(&older, Duration::from_secs(30));
    set_age(&newer, Duration::from_secs(20));
    set_age(&torn, Duration::from_secs(10));
    let key = |storage: &FileStorage| storage.read(false).expect("read").and_then(|s| s.key);
    assert_eq!(key(&FileStorage::new(&path)).as_deref(), Some("NEW"));
    // A read-only read reads it too, and writes nothing.
    assert_eq!(
        FileStorage::new(&path)
            .read(true)
            .expect("read")
            .and_then(|s| s.key)
            .as_deref(),
        Some("NEW")
    );
    assert!(!path.exists());
    std::fs::remove_file(&newer).expect("removed");
    assert_eq!(key(&FileStorage::new(&path)).as_deref(), Some("OLD"));
    std::fs::remove_file(&older).expect("removed");
    assert_eq!(key(&FileStorage::new(&path)), None);
}

#[test]
fn reads_a_leftover_temp_read_only_without_writing_anything() {
    let temp = sibling(".7.7.left.tmp");
    let memory = MemoryFs::new(&[(&temp, r#"{"key":"LEFT"}"#)]);
    let read = over_memory(&memory, &Slept::default())
        .read(true)
        .expect("read");
    assert_eq!(read.and_then(|s| s.key).as_deref(), Some("LEFT"));
    assert!(memory.changes().is_empty());
}

#[test]
fn sets_an_unparseable_file_aside_writes_an_empty_state_in_its_place_and_never_takes_a_temp() {
    let dir = tempfile::tempdir().expect("a temp folder");
    let path = dir.path().join("sluice.json");
    std::fs::write(&path, r#"{"key":"SLUICE-AA"#).expect("written");
    std::fs::write(
        dir.path().join("sluice.json.1.1.a.tmp"),
        r#"{"key":"FROM-A-TEMP"}"#,
    )
    .expect("written");
    let storage = FileStorage::new(&path);
    assert_eq!(storage.read(false).expect("read"), None);
    let aside = names_in(dir.path())
        .into_iter()
        .find(|name| {
            name.strip_prefix("sluice.json.")
                .and_then(|rest| rest.strip_suffix(".corrupt"))
                .is_some_and(|ms| !ms.is_empty() && ms.bytes().all(|b| b.is_ascii_digit()))
        })
        .expect("a file set aside as <file>.<ms>.corrupt");
    assert_eq!(
        std::fs::read_to_string(dir.path().join(aside)).expect("aside"),
        r#"{"key":"SLUICE-AA"#
    );
    // An empty state in its place, so no later read takes the temp instead.
    assert_eq!(
        parse(&std::fs::read_to_string(&path).expect("the state")),
        json!({})
    );
    assert_eq!(
        storage.read(false).expect("read"),
        Some(StoredState::default())
    );
}

#[test]
fn keeps_only_the_newest_set_aside_files_by_the_time_in_their_names() {
    let aside = |at: u64| sibling(&format!(".{at}.corrupt"));
    let others = [
        state_dir().join("tunnel.json.100.corrupt"),
        sibling(".notes.corrupt"),
    ];
    let memory = MemoryFs::new(&[
        (&state_path(), r#"{"bad"#),
        (&aside(900), "x"),
        (&aside(1000), "x"),
        (&aside(2000), "x"),
        (&aside(3000), "x"),
        (&others[0], "x"),
        (&others[1], "x"),
    ]);
    assert_eq!(
        over_memory(&memory, &Slept::default())
            .read(false)
            .expect("read"),
        None
    );
    let kept: Vec<PathBuf> = memory
        .names()
        .into_iter()
        .filter(|path| {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            name.strip_prefix("sluice.json.")
                .and_then(|r| r.strip_suffix(".corrupt"))
                .is_some_and(|d| d.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    assert_eq!(kept.len(), KEEP_CORRUPT);
    assert!(kept.contains(&aside(3000)) && kept.contains(&aside(2000)));
    assert!(memory.file(&aside(1000)).is_none() && memory.file(&aside(900)).is_none());
    assert!(memory.file(&others[0]).is_some() && memory.file(&others[1]).is_some());
}

#[test]
fn a_read_only_read_neither_sets_a_bad_file_aside_nor_writes_in_its_place() {
    let memory = MemoryFs::new(&[(&state_path(), r#"{"bad"#)]);
    assert_eq!(
        over_memory(&memory, &Slept::default())
            .read(true)
            .expect("read"),
        None
    );
    assert_eq!(memory.file(&state_path()).as_deref(), Some(r#"{"bad"#));
    assert!(memory.changes().is_empty());
}

#[test]
fn tries_a_failed_read_again_and_fails_after_about_a_second_rather_than_read_no_state() {
    let memory = MemoryFs::new(&[(&state_path(), SAVED)]);
    memory.fail_next(Op::ReadFile, &[ErrorKind::ResourceBusy]);
    let slept = Slept::default();
    assert_eq!(
        over_memory(&memory, &slept).read(false).expect("read"),
        Some(saved())
    );
    assert_eq!(slept.waits().len(), 1);
    for kind in [
        ErrorKind::ResourceBusy,
        ErrorKind::PermissionDenied,
        ErrorKind::Other,
    ] {
        let temp = sibling(".1.1.a.tmp");
        let memory = MemoryFs::new(&[(&state_path(), SAVED), (&temp, r#"{"key":"FROM-A-TEMP"}"#)]);
        memory.fail_always(Op::ReadFile, kind);
        let slept = Slept::default();
        assert_eq!(
            over_memory(&memory, &slept)
                .read(false)
                .expect_err("unreadable")
                .kind(),
            kind
        );
        assert!((500..=1_500).contains(&slept.total()), "{}", slept.total());
        assert!(memory.changes().is_empty());
    }
    // A missing file is not waited on.
    let slept = Slept::default();
    assert_eq!(
        over_memory(&MemoryFs::default(), &slept)
            .read(false)
            .expect("read"),
        None
    );
    assert!(slept.waits().is_empty());
}

#[test]
fn fails_writing_nothing_when_an_unparseable_file_cannot_be_set_aside() {
    let memory = MemoryFs::new(&[(&state_path(), r#"{"key":"SLUICE-AA"#)]);
    memory.fail_always(Op::Rename, ErrorKind::ResourceBusy);
    assert_eq!(
        over_memory(&memory, &Slept::default())
            .read(false)
            .expect_err("busy")
            .kind(),
        ErrorKind::ResourceBusy
    );
    assert_eq!(memory.names(), [state_path()]);
}

#[test]
fn reads_leniently_a_field_of_the_wrong_type_and_keeps_fields_it_does_not_know() {
    let memory = MemoryFs::new(&[(
        &state_path(),
        r#"{"key":"K","lastSeen":"soon","trialStart":1.9,"refused":"maybe","refusedFor":["a",2],"future":{"x":1}}"#,
    )]);
    let storage = over_memory(&memory, &Slept::default());
    let read = storage.read(false).expect("read").expect("a state");
    assert_eq!(
        (
            read.key.as_deref(),
            read.last_seen,
            read.trial_start,
            read.refused
        ),
        (Some("K"), None, Some(1), None)
    );
    assert_eq!(read.refused_for, Some(vec!["a".to_owned()]));
    storage.write(&read).expect("written");
    let written = parse(&memory.file(&state_path()).expect("the state"));
    assert_eq!(written["future"], json!({ "x": 1 }));
}

// --- the state an older SDK kept ---------------------------------------------------------

#[test]
fn migrates_an_older_sdks_state_once_and_reads_the_new_path_after() {
    let dir = tempfile::tempdir().expect("a temp folder");
    let legacy = dir.path().join("app").join(".keygrant").join("sluice.json");
    let path = dir.path().join("user").join("sluice.json");
    std::fs::create_dir_all(legacy.parent().expect("a parent")).expect("a folder");
    std::fs::write(&legacy, SAVED).expect("written");
    let options = || FileStorageOptions {
        migrate_from: Some(legacy.clone()),
        ..FileStorageOptions::default()
    };
    let storage = FileStorage::with_options(&path, options());
    assert_eq!(storage.read(false).expect("read"), Some(saved()));
    assert_eq!(
        parse(&std::fs::read_to_string(&path).expect("copied")),
        parse(SAVED)
    );
    assert_eq!(std::fs::read_to_string(&legacy).expect("left"), SAVED);
    std::fs::write(&legacy, r#"{"key":"CHANGED"}"#).expect("written");
    assert_eq!(storage.read(false).expect("read"), Some(saved()));
    // Ignored when the new path has a state.
    FileStorage::new(&path)
        .write(&k("NEWER", 1))
        .expect("written");
    assert_eq!(
        FileStorage::with_options(&path, options())
            .read(false)
            .expect("read")
            .and_then(|s| s.key)
            .as_deref(),
        Some("NEWER")
    );
}

#[test]
fn migrates_nothing_when_the_old_state_is_missing_or_unparseable_and_never_on_a_read_only_read() {
    let dir = tempfile::tempdir().expect("a temp folder");
    let legacy = dir.path().join(".keygrant").join("sluice.json");
    let path = dir.path().join("user").join("sluice.json");
    let storage = FileStorage::with_options(
        &path,
        FileStorageOptions {
            migrate_from: Some(legacy.clone()),
            ..FileStorageOptions::default()
        },
    );
    assert_eq!(storage.read(false).expect("read"), None);
    std::fs::create_dir_all(legacy.parent().expect("a parent")).expect("a folder");
    std::fs::write(&legacy, r#"{"key":"SLU"#).expect("written");
    assert_eq!(storage.read(false).expect("read"), None);
    assert_eq!(names_in(dir.path()), [".keygrant"]);

    let legacy = std::env::temp_dir()
        .join("keygrant-legacy")
        .join("sluice.json");
    let memory = MemoryFs::new(&[(&legacy, SAVED)]);
    let options = FileStorageOptions {
        migrate_from: Some(legacy.clone()),
        fs: Some(Arc::new(memory.clone())),
        sleep: Some(Slept::default().sleep()),
    };
    let storage = FileStorage::with_options(state_path(), options);
    assert_eq!(storage.read(true).expect("read"), Some(saved()));
    assert!(memory.changes().is_empty());
    // Still read when it cannot be copied, and copied at a later read.
    memory.fail_next(Op::Write, &[ErrorKind::StorageFull]);
    assert_eq!(storage.read(false).expect("read"), Some(saved()));
    assert!(memory.file(&state_path()).is_none());
    assert_eq!(storage.read(false).expect("read"), Some(saved()));
    assert_eq!(
        memory.file(&state_path()).map(|t| parse(&t)),
        Some(parse(SAVED))
    );
}

// --- a License over the default storage ---------------------------------------------------

fn license_over(memory: &MemoryFs, migrate_from: Option<PathBuf>, http: Arc<FakeHttp>) -> License {
    let options = FileStorageOptions {
        migrate_from,
        fs: Some(Arc::new(memory.clone())),
        sleep: Some(Slept::default().sleep()),
    };
    let config = LicenseConfig::new(PRODUCT, "https://api.test", TestKey::new(7).public_jwk())
        .storage(Arc::new(FileStorage::with_options(state_path(), options)))
        .http(http)
        .clock(FakeClock::at(NOW))
        .fingerprinter(Arc::new(FixedFingerprinter("fp-test")))
        .timings(fast_timings())
        .http_timeout(Duration::from_millis(100));
    License::new(config).expect("a valid configuration")
}

fn busy_server() -> Arc<FakeHttp> {
    let http = Arc::new(FakeHttp::default());
    for action in ["activate", "validate", "deactivate", "trial"] {
        http.answer(action, 503, json!({}));
    }
    http
}

#[test]
fn on_a_read_that_stays_busy_status_is_unknown_activate_fails_and_nothing_is_asked_or_written() {
    let memory = MemoryFs::new(&[(&state_path(), SAVED)]);
    memory.fail_always(Op::ReadFile, ErrorKind::ResourceBusy);
    let http = busy_server();
    let license = license_over(&memory, None, Arc::clone(&http));
    let mut unknown = LicenseStatus::new(LicenseState::Unknown);
    unknown.reason = Some(LicenseReason::Storage);
    assert_eq!(license.status(false), unknown);
    assert_eq!(license.refresh(), unknown);
    assert_eq!(license.start_trial(), unknown);
    assert!(matches!(
        license.activate("SLUICE-EEEE-FFFF-GGGG-HHHH"),
        Err(keygrant::ActivateError::Storage(_))
    ));
    assert!(matches!(
        license.deactivate(),
        Err(keygrant::Error::Storage(_))
    ));
    assert!(http.calls().is_empty());
    assert_eq!(memory.file(&state_path()).as_deref(), Some(SAVED));
    assert!(memory.changes().is_empty());
}

/// A status stuck on its read, and the next call run read-only behind it.
fn behind_a_stuck_read(memory: &MemoryFs, migrate_from: Option<PathBuf>) -> LicenseStatus {
    let license = license_over(memory, migrate_from, busy_server());
    let gate = memory.hold_next(Op::ReadFile);
    std::thread::scope(|scope| {
        let _stuck = scope.spawn(|| license.status(false));
        gate.await_reached();
        let answer = license.status(false);
        assert!(memory.changes().is_empty(), "{:?}", memory.changes());
        gate.release();
        answer
    })
}

#[test]
fn behind_a_stuck_read_a_bad_file_is_not_set_aside_an_old_state_not_copied_an_unreadable_one_unknown(
) {
    let mut unlicensed = LicenseStatus::new(LicenseState::Unlicensed);
    unlicensed.stalled = true;
    assert_eq!(
        behind_a_stuck_read(&MemoryFs::new(&[(&state_path(), r#"{"bad"#)]), None),
        unlicensed
    );

    let legacy = std::env::temp_dir()
        .join("keygrant-legacy")
        .join("sluice.json");
    let lease = TestKey::new(7).lease("trial:fp-test", "trial", 13 * 86_400, NOW);
    let trial =
        json!({ "trialStart": NOW - DAY, "trialEndsAt": NOW + 13 * DAY, "trialLease": lease })
            .to_string();
    let answer = behind_a_stuck_read(&MemoryFs::new(&[(&legacy, &trial)]), Some(legacy.clone()));
    assert_eq!((answer.state, answer.stalled), (LicenseState::Trial, true));

    let memory = MemoryFs::new(&[(&state_path(), SAVED)]);
    memory.fail_always(Op::ReadFile, ErrorKind::ResourceBusy);
    let answer = behind_a_stuck_read(&memory, None);
    assert_eq!(
        (answer.state, answer.reason, answer.stalled),
        (LicenseState::Unknown, Some(LicenseReason::Storage), true)
    );

    // The upgrade link from an older SDK's state, beside any call, copies nothing.
    let memory = MemoryFs::new(&[(&legacy, SAVED)]);
    let license = license_over(&memory, Some(legacy), busy_server());
    assert_eq!(
        license
            .upgrade_url("https://buy.test/upgrade")
            .expect("a link")
            .as_deref(),
        Some("https://buy.test/upgrade?client_reference_id=act_1")
    );
    assert!(memory.changes().is_empty());
}

// --- saves from more than one process ---------------------------------------------------------

/// A save as the engine makes one: `patch` merged by `basis` onto the state as stored.
fn save(storage: &FileStorage, patch: Patch, basis: Basis) -> std::io::Result<()> {
    let merge = move |latest: StoredState| {
        let mut next = merged(&latest, &patch, &basis);
        next.last_seen = Some(latest.last_seen.unwrap_or(0).max(NOW));
        next
    };
    storage.update(&merge).expect("the file storage updates")
}

fn patch(json: Value) -> Patch {
    Patch::from_json(json.as_object().expect("an object"))
}

fn file_state(memory: &MemoryFs) -> Value {
    parse(&memory.file(&state_path()).expect("the state"))
}

/// `first` held at its first temp file until `between` has run, then let finish.
fn interleave<T: Send>(
    memory: &MemoryFs,
    first: impl FnOnce() -> T + Send,
    between: impl FnOnce(),
) -> T {
    let gate = memory.hold_next(Op::Create);
    std::thread::scope(|scope| {
        let running = scope.spawn(first);
        gate.await_reached();
        between();
        gate.release();
        running.join().expect("the first save")
    })
}

#[test]
fn stamps_every_save_with_the_next_revision() {
    let memory = MemoryFs::default();
    let storage = over_memory(&memory, &Slept::default());
    save(&storage, patch(json!({ "key": "K" })), Basis::default()).expect("saved");
    assert_eq!(
        (
            file_state(&memory)["key"].clone(),
            file_state(&memory)["rev"].clone()
        ),
        (json!("K"), json!(1))
    );
    save(
        &storage,
        patch(json!({ "checkedMajor": 2 })),
        Basis::default(),
    )
    .expect("saved");
    assert_eq!(file_state(&memory)["rev"], json!(2));
    assert_eq!(file_state(&memory)["checkedMajor"], json!(2));
}

#[test]
fn loses_neither_of_two_saves_that_interleave() {
    for initial in [Some(r#"{"key":"K","rev":4}"#), Some(r#"{"key":"K"}"#), None] {
        let memory = match initial {
            Some(text) => MemoryFs::new(&[(&state_path(), text)]),
            None => MemoryFs::default(),
        };
        let (a, b) = (
            over_memory(&memory, &Slept::default()),
            over_memory(&memory, &Slept::default()),
        );
        interleave(
            &memory,
            || save(&a, patch(json!({ "keptMajor": 2 })), Basis::default()).expect("a saved"),
            || save(&b, patch(json!({ "checkedMajor": 3 })), Basis::default()).expect("b saved"),
        );
        let saved = file_state(&memory);
        assert_eq!(
            (saved["keptMajor"].clone(), saved["checkedMajor"].clone()),
            (json!(2), json!(3)),
            "{initial:?}"
        );
        assert_eq!(memory.names(), [state_path()]);
    }
}

#[test]
fn keeps_what_another_process_removed_in_between() {
    let asked = |at: i64| Basis {
        answer: Some(Asked {
            at,
            activation_id: Some("act_1".into()),
            seen: Some(100),
        }),
        ..Basis::default()
    };
    let cases = [
        (
            json!({ "refused": null }),
            Basis::default(),
            json!({ "keptMajor": 2 }),
            Basis::default(),
            vec!["refused"],
        ),
        (
            json!({ "key": null, "activationId": null }),
            Basis::default(),
            json!({ "keptMajor": 2 }),
            Basis::default(),
            vec!["key", "activationId"],
        ),
        // B's answer, asked for later, clears the refusal A's older answer would set again.
        (
            json!({ "lease": "L2", "refused": null }),
            asked(2_000),
            json!({ "keptMajor": 2, "refused": "revoked", "lease": null }),
            asked(1_000),
            vec!["refused"],
        ),
    ];
    for (b_patch, b_basis, a_patch, a_basis, gone) in cases {
        let memory = MemoryFs::new(&[(
            &state_path(),
            r#"{"key":"K","activationId":"act_1","refused":"revoked","verdictAt":100,"rev":4}"#,
        )]);
        let (a, b) = (
            over_memory(&memory, &Slept::default()),
            over_memory(&memory, &Slept::default()),
        );
        let later = b_basis.answer.is_some();
        interleave(
            &memory,
            || save(&a, patch(a_patch), a_basis).expect("a saved"),
            || save(&b, patch(b_patch), b_basis).expect("b saved"),
        );
        let saved = file_state(&memory);
        assert_eq!(saved["keptMajor"], json!(2));
        for field in gone {
            assert!(saved.get(field).is_none(), "{field} in {saved}");
        }
        if later {
            assert_eq!(
                (saved["lease"].clone(), saved["verdictAt"].clone()),
                (json!("L2"), json!(2_000))
            );
        }
    }
}

#[test]
fn keeps_the_higher_kept_major_another_process_noted_in_between() {
    let memory = MemoryFs::new(&[(
        &state_path(),
        r#"{"key":"K","activationId":"act_1","keptMajor":2,"rev":1}"#,
    )]);
    let (a, b) = (
        over_memory(&memory, &Slept::default()),
        over_memory(&memory, &Slept::default()),
    );
    let about = || Basis {
        about: Some(About {
            activation_id: Some("act_1".into()),
        }),
        ..Basis::default()
    };
    interleave(
        &memory,
        || save(&a, patch(json!({ "keptMajor": 3 })), about()).expect("a saved"),
        || save(&b, patch(json!({ "keptMajor": 5 })), about()).expect("b saved"),
    );
    assert_eq!(file_state(&memory)["keptMajor"], json!(5));
}

#[test]
fn fails_with_a_conflict_after_a_few_tries_when_the_state_keeps_changing_keeping_the_others() {
    let memory = MemoryFs::new(&[(&state_path(), r#"{"key":"K","rev":1}"#)]);
    let other = Arc::new(Mutex::new(1));
    let path = state_path();
    {
        let other = Arc::clone(&other);
        // Another process saves every time this one has read and is about to write.
        memory.before(Op::Create, move |files| {
            let mut rev = lock(&other);
            *rev += 1;
            files.insert(
                path.clone(),
                json!({ "key": "OTHER", "rev": *rev }).to_string(),
            );
        });
    }
    let storage = over_memory(&memory, &Slept::default());
    let error =
        save(&storage, patch(json!({ "keptMajor": 2 })), Basis::default()).expect_err("a conflict");
    assert!(is_conflict(&error), "{error}");
    assert_eq!(memory.count(Op::Create), UPDATE_ATTEMPTS);
    assert_eq!(UPDATE_ATTEMPTS, 5);
    assert_eq!(
        file_state(&memory),
        json!({ "key": "OTHER", "rev": *lock(&other) })
    );
    assert_eq!(memory.names(), [state_path()]);
}

// --- answers saved by two instances of the app --------------------------------------------------

struct OneFile {
    key: TestKey,
    memory: MemoryFs,
}

impl OneFile {
    fn new(stored: impl FnOnce(&TestKey) -> Value) -> OneFile {
        let key = TestKey::new(7);
        let memory = MemoryFs::new(&[(&state_path(), &stored(&key).to_string())]);
        OneFile { key, memory }
    }

    fn sign(&self, at: i64, over: Value) -> String {
        let mut claims = json!({ "sub": KEY });
        for (name, value) in over.as_object().cloned().unwrap_or_default() {
            claims[name] = value;
        }
        self.key.lease_with(at, 30 * 86_400, claims)
    }

    /// An instance whose clock reads `now` and whose server answers each action as `answers` says.
    fn instance(&self, now: i64, answers: Value, fingerprint: Arc<dyn Fingerprinter>) -> License {
        let http = Arc::new(FakeHttp::default());
        for (action, answer) in answers.as_object().cloned().unwrap_or_default() {
            let status = u16::try_from(answer["status"].as_u64().unwrap_or(500)).unwrap_or(500);
            http.answer(&action, status, answer["json"].clone());
        }
        let options = FileStorageOptions {
            fs: Some(Arc::new(self.memory.clone())),
            sleep: Some(Slept::default().sleep()),
            ..FileStorageOptions::default()
        };
        let config = LicenseConfig::new(PRODUCT, "https://api.test", self.key.public_jwk())
            .storage(Arc::new(FileStorage::with_options(state_path(), options)))
            .http(http)
            .clock(FakeClock::at(now))
            .fingerprinter(fingerprint);
        License::new(config).expect("a valid configuration")
    }

    fn saved(&self) -> Value {
        file_state(&self.memory)
    }
}

fn fixed() -> Arc<dyn Fingerprinter> {
    Arc::new(FixedFingerprinter("fp-test"))
}

fn stored_lease(key: &TestKey, over: Value) -> Value {
    let mut claims = json!({ "sub": KEY });
    for (name, value) in over.as_object().cloned().unwrap_or_default() {
        claims[name] = value;
    }
    json!({
        "key": KEY, "activationId": "act_1", "lease": key.lease_with(NOW - DAY, 30 * 86_400, claims),
        "checkedMajor": 1, "lastSeen": NOW - DAY, "rev": 1,
    })
}

fn revoked(key: &str) -> LicenseStatus {
    let mut status = LicenseStatus::new(LicenseState::Invalid);
    status.reason = Some(LicenseReason::Revoked);
    status.key = Some(key.to_owned());
    status
}

#[test]
fn keeps_the_newer_answer_a_refusal_asked_for_later_survives_the_remerge_of_an_older_asks_lease() {
    let file = OneFile::new(|key| stored_lease(key, json!({})));
    let a = file.instance(NOW, json!({ "validate": { "status": 200, "json": { "lease": file.sign(NOW, json!({})), "activationId": "act_1" } } }), fixed());
    let b = file.instance(
        NOW + 1_000,
        json!({ "validate": { "status": 403, "json": { "error": "revoked" } } }),
        fixed(),
    );
    let answered = interleave(
        &file.memory,
        || a.refresh(),
        || assert_eq!(b.refresh(), revoked(KEY)),
    );
    assert_eq!(answered.state, LicenseState::Licensed);
    let saved = file.saved();
    assert_eq!(
        (saved["refused"].clone(), saved["verdictAt"].clone()),
        (json!("revoked"), json!(NOW + 1_000))
    );
    assert!(saved.get("lease").is_none());
    assert_eq!(file.memory.count(Op::Create), 3);
}

#[test]
fn keeps_the_later_asks_answer_when_the_clock_was_put_back_between_the_two_asks() {
    let file = OneFile::new(|key| stored_lease(key, json!({})));
    let a = file.instance(NOW, json!({ "validate": { "status": 200, "json": { "lease": file.sign(NOW, json!({})), "activationId": "act_1" } } }), fixed());
    let b = file.instance(
        NOW - 60_000,
        json!({ "validate": { "status": 403, "json": { "error": "revoked" } } }),
        fixed(),
    );
    interleave(
        &file.memory,
        || a.refresh(),
        || {
            // A save made at NOW + 2 s, before the clock went back, raised the guard.
            let mut raised = file.saved();
            raised["lastSeen"] = json!(NOW + 2_000);
            raised["rev"] = json!(raised["rev"].as_i64().unwrap_or(0) + 1);
            file.memory.put(&state_path(), &raised.to_string());
            b.refresh();
        },
    );
    let saved = file.saved();
    assert_eq!(
        (saved["refused"].clone(), saved["verdictAt"].clone()),
        (json!("revoked"), json!(NOW + 2_000))
    );
    assert!(saved.get("lease").is_none());
}

#[test]
fn keeps_a_refusal_asked_for_later_over_a_reactivation_of_the_same_seat_that_saves_after_it() {
    let file = OneFile::new(|key| stored_lease(key, json!({})));
    let w1 = file.instance(NOW, json!({ "activate": { "status": 200, "json": { "lease": file.sign(NOW, json!({})), "activationId": "act_1" } } }), fixed());
    let w2 = file.instance(
        NOW + 1_000,
        json!({ "validate": { "status": 403, "json": { "error": "revoked" } } }),
        fixed(),
    );
    let activated = interleave(
        &file.memory,
        || w1.activate(KEY),
        || assert_eq!(w2.refresh(), revoked(KEY)),
    );
    assert!(activated.is_ok());
    let saved = file.saved();
    assert_eq!(
        (
            saved["key"].clone(),
            saved["activationId"].clone(),
            saved["refused"].clone(),
            saved["verdictAt"].clone()
        ),
        (
            json!(KEY),
            json!("act_1"),
            json!("revoked"),
            json!(NOW + 1_000)
        )
    );
    assert!(saved.get("lease").is_none());
    // So the revoked device does not license offline.
    assert_eq!(w1.status(false).reason, Some(LicenseReason::Revoked));
}

#[test]
fn a_renewal_that_lands_after_the_activation_it_renewed_is_gone_writes_nothing_of_it() {
    let machine: Arc<dyn Fingerprinter> = Arc::new(IdentifyingFingerprinter(|| DeviceIdentity {
        fingerprint: Some("fp-machine".into()),
        kind: Some(DeviceKind::Machine),
        alternates: Vec::new(),
    }));
    let open = json!({ "major": OPEN_MAJOR, "upgradeUntil": (NOW + 10 * DAY) / 1000 });
    for deactivate in [true, false] {
        let file = OneFile::new(|key| stored_lease(key, open.clone()));
        let lease_b = file.key.lease_with(
            NOW,
            30 * 86_400,
            json!({ "sub": "SLUICE-BBBB-BBBB-BBBB-BBBB", "aid": "act_2" }),
        );
        let w1 = file.instance(
            NOW,
            json!({
                "deactivate": { "status": 200, "json": { "ok": true } },
                "activate": { "status": 200, "json": { "lease": lease_b, "activationId": "act_2" } },
            }),
            fixed(),
        );
        let renewed = file.sign(NOW, open.clone());
        let w2 = file.instance(NOW, json!({ "validate": { "status": 200, "json": { "lease": renewed, "activationId": "act_1" } } }), Arc::clone(&machine));
        interleave(
            &file.memory,
            || w2.refresh(),
            || {
                if deactivate {
                    w1.deactivate().expect("deactivated");
                } else {
                    w1.activate("SLUICE-BBBB-BBBB-BBBB-BBBB")
                        .expect("activated");
                }
            },
        );
        let saved = file.saved();
        assert!(
            saved.get("keptMajor").is_none() && saved.get("deviceKind").is_none(),
            "{saved}"
        );
        if deactivate {
            assert!(saved.get("key").is_none());
        } else {
            assert_eq!(
                (
                    saved["key"].clone(),
                    saved["activationId"].clone(),
                    saved["lease"].clone()
                ),
                (
                    json!("SLUICE-BBBB-BBBB-BBBB-BBBB"),
                    json!("act_2"),
                    json!(lease_b)
                )
            );
        }
    }
}

#[test]
fn the_default_storage_shares_its_file_format_with_the_electron_sdk() {
    // A state as the Electron SDK writes it reads field for field, and is written back the same.
    let electron = json!({
        "key": KEY, "activationId": "act_1", "lease": "a.b.c", "trialStart": 1, "trialEndsAt": 2, "trialLease": "t.l.s",
        "trialAt": 3, "lastSeen": 4, "checkedMajor": 2, "keptMajor": 3, "backoffSince": 5, "backoffMs": 60000,
        "refused": "upgrade", "refusedMajor": 4, "refusedFor": ["c"], "deviceKind": "machine", "verdictAt": 6, "rev": 7,
    });
    let memory = MemoryFs::new(&[(&state_path(), &electron.to_string())]);
    let storage = over_memory(&memory, &Slept::default());
    let read = storage.read(false).expect("read").expect("a state");
    assert_eq!(read.to_json(), electron);
    storage.write(&read).expect("written");
    assert_eq!(file_state(&memory), electron);
}

#[test]
fn reads_a_busy_file_as_busy_and_nothing_else() {
    use keygrant::storage::busy;
    for kind in [ErrorKind::PermissionDenied, ErrorKind::ResourceBusy] {
        assert!(busy(&std::io::Error::from(kind)), "{kind:?}");
    }
    for kind in [
        ErrorKind::NotFound,
        ErrorKind::StorageFull,
        ErrorKind::Other,
    ] {
        assert!(!busy(&std::io::Error::from(kind)), "{kind:?}");
    }
}

#[cfg(windows)]
#[test]
fn reads_windows_sharing_and_lock_violations_as_busy_and_tries_the_rename_again() {
    use keygrant::storage::busy;
    // ERROR_SHARING_VIOLATION and ERROR_LOCK_VIOLATION: an antivirus scan or
    // an indexer holding the file. Neither has an error kind of its own.
    for code in [32, 33] {
        assert!(busy(&std::io::Error::from_raw_os_error(code)), "{code}");
    }
    // ERROR_DISK_FULL is not worth another try.
    assert!(!busy(&std::io::Error::from_raw_os_error(112)));
    let memory = MemoryFs::new(&[(&state_path(), SAVED)]);
    memory.fail_next_os(Op::Rename, &[32, 33]);
    let slept = Slept::default();
    over_memory(&memory, &slept)
        .write(&k("K", 2))
        .expect("written once the file is free");
    assert_eq!(slept.waits().len(), 2);
    assert_eq!(
        parse(&memory.file(&state_path()).expect("the state")),
        json!({ "key": "K", "lastSeen": 2 })
    );
    assert_eq!(memory.names(), [state_path()]);
}
