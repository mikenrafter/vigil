//! Watch files under `<root>/watches/`, atomic writes, the per-watch locks
//! under `<root>/locks/`, the short global namespace lock, pruning (X3).

use std::fs::File;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::clock::Timestamp;
use crate::error::StoreError;
use crate::fault::FaultPlan;
use crate::watch::{WatchFile, WatchId};

/// Finished watches are deleted this long after they finish (D4).
pub const PRUNE_AFTER_SECS: u64 = 7 * 24 * 3600;

/// `$XDG_STATE_HOME/vigil`, else `$HOME/.local/state/vigil`. An empty
/// variable counts as unset (XDG Base Directory spec). `None` if neither.
pub fn state_root(xdg_state_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    fn non_empty(value: Option<&str>) -> Option<&str> {
        value.filter(|text| !text.is_empty())
    }
    match (non_empty(xdg_state_home), non_empty(home)) {
        (Some(xdg), _) => Some(Path::new(xdg).join("vigil")),
        (None, Some(home)) => Some(Path::new(home).join(".local/state/vigil")),
        (None, None) => None,
    }
}

fn io_error(error: std::io::Error) -> StoreError {
    StoreError::Io(error.to_string())
}

/// Serialize `value` to JSON, write `<path>.tmp` beside it, fsync, rename over
/// `path`. On any failure `path` keeps its previous content and no `.tmp`
/// remains. (SPEC "State": writes are atomic.)
pub fn write_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    use std::io::Write;
    // Serialize first: a failing value never creates a `.tmp`.
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| StoreError::Serialize(error.to_string()))?;
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    let written = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(io_error(error));
    }
    Ok(())
}

/// Which lock file a [`StoreLock`] guards; decides whether X8 applies.
fn open_lock_file(path: &Path) -> Result<File, StoreError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io_error)?;
    }
    std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path).map_err(io_error)
}

/// X8: is the locked descriptor still the file the path names? False if the
/// path is gone or now names another inode (prune deleted the lock file while
/// we waited).
fn is_same_file(file: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (file.metadata(), std::fs::metadata(path)) {
        (Ok(held), Ok(named)) => held.dev() == named.dev() && held.ino() == named.ino(),
        _ => false,
    }
}

/// Exclusive `flock`, released on drop. Two kinds (X3, superseding the
/// single-lock rule of V4/V5):
///
/// * per-watch, `<root>/locks/<id>.lock`: held across a tick's sensing and
///   write and across cancel (and across create's baseline reads);
/// * global, `<root>/lock`: held only for short namespace operations
///   (reserving an id at create, prune), NEVER during sensing.
///
/// Lock order is watch lock first, then global lock. Lock files are created
/// on demand and are never deleted while their watch file exists (deleting
/// one would let a waiter hold the old inode while another process locks a
/// new one); prune deletes a lock file only after deleting its watch file.
pub struct StoreLock {
    file: File,
}

/// What a successful [`Store::save`] did beyond writing (specs/disk-full.md
/// DF4). RED STUB: always the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WriteOutcome {
    /// The first attempt hit a full disk, the reserve was deleted, and the
    /// retry succeeded.
    pub used_reserve: bool,
}

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
    /// Test seam (`fault.rs`); `FaultPlan::none()` in production.
    faults: FaultPlan,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Store {
        Store { root: root.into(), faults: FaultPlan::none() }
    }
    /// Replace the fault plan (tests, and `main` when `VIGIL_TEST_FAULT` is set).
    pub fn with_faults(mut self, faults: FaultPlan) -> Store {
        self.faults = faults;
        self
    }
    pub fn faults(&self) -> &FaultPlan {
        &self.faults
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn watches_dir(&self) -> PathBuf {
        self.root.join("watches")
    }
    pub fn watch_path(&self, id: &WatchId) -> PathBuf {
        self.watches_dir().join(format!("{}.json", id.as_str()))
    }
    /// `<root>/lock`, the global namespace lock file (X3).
    pub fn lock_path(&self) -> PathBuf {
        self.root.join("lock")
    }
    /// `<root>/locks/`, the per-watch lock files (X3).
    pub fn locks_dir(&self) -> PathBuf {
        self.root.join("locks")
    }
    /// `<root>/locks/<id>.lock` (X3).
    pub fn watch_lock_path(&self, id: &WatchId) -> PathBuf {
        self.locks_dir().join(format!("{}.lock", id.as_str()))
    }
    /// Atomic save of one watch (creates `watches/` on demand).
    pub fn save(&self, watch: &WatchFile) -> Result<WriteOutcome, StoreError> {
        std::fs::create_dir_all(self.watches_dir()).map_err(io_error)?;
        write_atomic(&self.watch_path(&watch.spec.id), watch)?;
        Ok(WriteOutcome::default())
    }
    pub fn load(&self, id: &WatchId) -> Result<WatchFile, StoreError> {
        let bytes = std::fs::read(self.watch_path(id)).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => StoreError::NotFound(id.as_str().to_string()),
            _ => io_error(error),
        })?;
        serde_json::from_slice(&bytes).map_err(|error| StoreError::Corrupt(format!("{id}: {error}")))
    }
    /// Every readable watch file, in id order.
    pub fn list(&self) -> Result<Vec<WatchFile>, StoreError> {
        let entries = match std::fs::read_dir(self.watches_dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(io_error(error)),
        };
        let mut watches: Vec<WatchFile> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".json"))
            .filter_map(|entry| std::fs::read(entry.path()).ok())
            .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
            .collect();
        watches.sort_by(|a: &WatchFile, b| a.spec.id.cmp(&b.spec.id));
        Ok(watches)
    }
    /// Block until the GLOBAL exclusive lock is held (creates the lock file
    /// and the root on demand). Short namespace operations only (X3).
    pub fn lock(&self) -> Result<StoreLock, StoreError> {
        let file = open_lock_file(&self.lock_path())?;
        file.lock().map_err(io_error)?;
        Ok(StoreLock { file })
    }
    /// `Ok(None)` if another holder has the global lock.
    pub fn try_lock(&self) -> Result<Option<StoreLock>, StoreError> {
        let file = open_lock_file(&self.lock_path())?;
        match file.try_lock() {
            Ok(()) => Ok(Some(StoreLock { file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(io_error(error)),
        }
    }
    /// `try_lock`, retried until `end`: a lock released a moment ago can look
    /// held while a forked child that has not yet exec'd still shares its open
    /// file description. `Ok(None)` once `end` has passed and it is still held.
    fn try_lock_until(&self, end: std::time::Instant) -> Result<Option<StoreLock>, StoreError> {
        loop {
            if let Some(lock) = self.try_lock()? {
                return Ok(Some(lock));
            }
            if std::time::Instant::now() >= end {
                return Ok(None);
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    /// Block until the exclusive lock of watch `id` is held (creates
    /// `locks/` and `locks/<id>.lock` on demand). Independent of the global
    /// lock and of every other watch's lock (X3).
    pub fn lock_watch(&self, id: &WatchId) -> Result<StoreLock, StoreError> {
        let path = self.watch_lock_path(id);
        loop {
            let file = open_lock_file(&path)?;
            file.lock().map_err(io_error)?;
            if is_same_file(&file, &path) {
                return Ok(StoreLock { file });
            }
            // X8: the lock file was replaced while we waited; lock the new one.
        }
    }
    /// `Ok(None)` if another holder has watch `id`'s lock.
    pub fn try_lock_watch(&self, id: &WatchId) -> Result<Option<StoreLock>, StoreError> {
        let path = self.watch_lock_path(id);
        loop {
            let file = open_lock_file(&path)?;
            match file.try_lock() {
                Ok(()) if is_same_file(&file, &path) => return Ok(Some(StoreLock { file })),
                Ok(()) => continue, // X8: replaced under us; try the new file.
                Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
                Err(std::fs::TryLockError::Error(error)) => return Err(io_error(error)),
            }
        }
    }
    /// Delete watch files whose finish time + 7 days <= now (D4). The finish
    /// time of a finished watch is `finished_at`; of an overdue pending watch
    /// it is `deadline_at` (V16); a pending watch whose deadline is still in
    /// the future is never deleted. X3: prune takes the locks itself (the
    /// caller must hold none of them): per candidate, its watch lock, then
    /// the global lock, re-checks the candidate under both, deletes the watch
    /// file, THEN that watch's lock file, and releases. A candidate whose
    /// watch lock is held by someone else is skipped or waited for (either is
    /// allowed) but never deleted while held. A busy global lock also skips
    /// the candidate (C10): prune only try-locks, so housekeeping never
    /// blocks the command that runs it. The global lock file is never
    /// deleted. Returns the deleted ids.
    pub fn prune(&self, now: Timestamp) -> Result<Vec<WatchId>, StoreError> {
        let mut deleted = Vec::new();
        let patience_ends = std::time::Instant::now() + PRUNE_LOCK_PATIENCE;
        for candidate in self.list()? {
            if !is_prunable(&candidate, now) {
                continue;
            }
            let id = candidate.spec.id;
            // Lock order: watch lock, then global lock. A busy watch (a tick
            // or cancel in flight) is skipped, never waited for.
            let Some(_watch_lock) = self.try_lock_watch(&id)? else {
                continue;
            };
            // Housekeeping never blocks (C10): a busy global lock means
            // someone is reserving an id; leave this candidate for the next run.
            let Some(_global_lock) = self.try_lock_until(patience_ends)? else {
                continue;
            };
            // Re-check under both locks: the file may have changed since list().
            match self.load(&id) {
                Ok(current) if is_prunable(&current, now) => {}
                _ => continue,
            }
            remove_if_present(&self.watch_path(&id))?;
            remove_if_present(&self.watch_lock_path(&id))?;
            deleted.push(id);
        }
        Ok(deleted)
    }
}

/// How long one `prune` call waits, in total, for a busy global lock before it
/// skips the candidates it cannot lock (C10).
const PRUNE_LOCK_PATIENCE: std::time::Duration = std::time::Duration::from_millis(250);

/// D4/V16: a finished watch, or a pending one past its deadline, is prunable
/// once its finish time (`finished_at`, else `deadline_at`) is 7 days old.
fn is_prunable(watch: &WatchFile, now: Timestamp) -> bool {
    let finished_at = if watch.status.is_finished() {
        watch.finished_at.unwrap_or(watch.deadline_at)
    } else if watch.is_overdue(now) {
        watch.deadline_at
    } else {
        return false;
    };
    finished_at.plus_secs(PRUNE_AFTER_SECS) <= now
}

fn remove_if_present(path: &Path) -> Result<(), StoreError> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(io_error(error)),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{at, blocked_flock_waiters, change_request, judge_request, wid, DeletionLog};
    use crate::watch::{Terminal, TerminalStatus, WatchSpec};
    use serde_json::Value;

    fn sample(id: &str) -> WatchFile {
        let spec = WatchSpec::parse(judge_request(id, Path::new("/tmp/build.log"), 20)).unwrap();
        WatchFile::new(spec, at("2026-10-29T14:03:11Z"))
    }

    fn finished(id: &str, status: TerminalStatus, finished_at: Timestamp) -> WatchFile {
        let spec = WatchSpec::parse(change_request(id, Path::new("/tmp/x"), 20)).unwrap();
        let mut file = WatchFile::new(spec, finished_at.minus_secs(60));
        file.finish(Terminal { status, finished_at, events: vec![], digest: String::new() }).unwrap();
        file
    }

    /// A pending watch created `created_ago` seconds before `now` with the
    /// default 3600 s deadline.
    fn pending(id: &str, now: Timestamp, created_ago: u64) -> WatchFile {
        let spec = WatchSpec::parse(change_request(id, Path::new("/tmp/x"), 20)).unwrap();
        WatchFile::new(spec, now.minus_secs(created_ago))
    }

    fn lock_identity(path: &Path) -> (u64, u64) {
        use std::os::unix::fs::MetadataExt;
        let md = std::fs::metadata(path).unwrap();
        (md.dev(), md.ino())
    }

    #[test]
    fn state_root_prefers_xdg_state_home_then_home() {
        // why: SPEC "State": `$XDG_STATE_HOME/vigil/` default `~/.local/state/vigil/`.
        // proves: CLAIM(state-root)
        assert_eq!(state_root(Some("/x/state"), Some("/home/u")), Some(PathBuf::from("/x/state/vigil")));
        assert_eq!(state_root(None, Some("/home/u")), Some(PathBuf::from("/home/u/.local/state/vigil")));
    }

    #[test]
    fn state_root_treats_an_empty_xdg_variable_as_unset() {
        // why: XDG Base Directory spec: empty values are invalid and ignored;
        //      using "" would write to `/vigil` or the cwd.
        assert_eq!(state_root(Some(""), Some("/home/u")), Some(PathBuf::from("/home/u/.local/state/vigil")));
        assert_eq!(state_root(None, None), None);
        assert_eq!(state_root(Some(""), Some("")), None);
    }

    #[test]
    fn save_then_load_round_trips_with_absolute_times() {
        // why: SPEC "State": the watch file carries absolute created_at and
        //      deadline_at; a new process must read the same values back.
        // proves: CLAIM(absolute-deadline)
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let watch = sample("build");
        store.save(&watch).unwrap();
        let loaded = store.load(&wid("build")).unwrap();
        assert_eq!(loaded, watch);
        assert_eq!(loaded.created_at, at("2026-10-29T14:03:11Z"));
        assert_eq!(loaded.deadline_at, at("2026-10-29T15:03:11Z"));
    }

    #[test]
    fn the_file_on_disk_uses_the_spec_field_names_and_literal_timestamps() {
        // why: parity with SPEC "State": `watches/<id>.json` holds the watch as
        //      created plus status, created_at, deadline_at (absolute UTC),
        //      tick, last_tick_at, judge_strikes, events, last_digest, and (V3)
        //      NO delivered_through. Read as raw JSON so a writer/reader that
        //      agree on a wrong name cannot pass.
        // proves: CLAIM(watch-file-format)
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.save(&sample("build")).unwrap();
        let path = dir.path().join("watches").join("build.json");
        let raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["id"], "build");
        assert_eq!(raw["prompt"], "Tell me if the build log shows a failure or finishes.");
        assert_eq!(raw["fire"], "judge");
        assert_eq!(raw["interval_s"], 20);
        assert_eq!(raw["status"], "pending");
        assert_eq!(raw["created_at"], "2026-10-29T14:03:11Z");
        assert_eq!(raw["deadline_at"], "2026-10-29T15:03:11Z");
        assert_eq!(raw["tick"], 0);
        assert_eq!(raw["judge_strikes"], 0);
        assert!(raw.get("delivered_through").is_none(), "V3: delivered_through is gone");
        assert_eq!(raw["last_digest"], "");
        assert!(raw["events"].as_array().unwrap().is_empty());
        assert_eq!(raw["judge_rule"]["noul_gte"], 0.8);
        assert_eq!(raw["inputs"][0]["kind"], "file");
    }

    #[test]
    fn loading_an_unknown_watch_is_not_found() {
        // why: callers must tell "no such watch" from IO failure (StoreError::NotFound).
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        assert_eq!(store.load(&wid("ghost")), Err(StoreError::NotFound("ghost".into())));
    }

    #[test]
    fn successful_write_leaves_no_tmp_file_and_replaces_the_content() {
        // why: SPEC "State": write `<id>.json.tmp`, fsync, rename. After
        //      success only the final file exists.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.json");
        write_atomic(&path, &serde_json::json!({"v": 1})).unwrap();
        write_atomic(&path, &serde_json::json!({"v": 2})).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap()["v"], 2);
        let names: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into()).collect();
        assert_eq!(names, vec!["w.json".to_string()], "no .tmp left behind");
    }

    /// Serializes a prefix and then fails, like a value that errors midway.
    struct FailsMidway;
    impl Serialize for FailsMidway {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeMap;
            let mut map = serializer.serialize_map(None)?;
            map.serialize_entry("half", "written")?;
            Err(serde::ser::Error::custom("boom"))
        }
    }

    #[test]
    fn failed_serialization_keeps_the_old_file_and_leaves_no_tmp() {
        // why: SPEC "State": writes are atomic - a failure must not corrupt or
        //      truncate the previous watch file, and must not leave a partial
        //      `.tmp` that a later run could mistake for state.
        // proves: CLAIM(atomic-write)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.json");
        write_atomic(&path, &serde_json::json!({"v": "old"})).unwrap();
        let outcome = write_atomic(&path, &FailsMidway);
        assert!(matches!(outcome, Err(StoreError::Serialize(_))), "got {outcome:?}");
        assert_eq!(serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap()["v"], "old");
        let names: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into()).collect();
        assert_eq!(names, vec!["w.json".to_string()]);
    }

    #[test]
    fn a_concurrent_reader_never_sees_a_partial_file() {
        // why: SPEC "State": atomic rename means another session reading a
        //      watch mid-write sees the old or the new file, never a prefix.
        //      A 2 MB payload makes a non-atomic in-place write observable.
        //      Positive control: the reader completed many parses.
        // proves: CLAIM(atomic-write)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.json");
        write_atomic(&path, &serde_json::json!({"fill": "a".repeat(2_000_000)})).unwrap();
        let done = std::sync::Arc::new(AtomicBoolFlag::default());
        let writer = {
            let (path, done) = (path.clone(), done.clone());
            std::thread::spawn(move || {
                for i in 0..40 {
                    let fill = if i % 2 == 0 { "b" } else { "a" };
                    write_atomic(&path, &serde_json::json!({"fill": fill.repeat(2_000_000)})).unwrap();
                }
                done.set();
            })
        };
        let mut parses = 0;
        while !done.is_set() {
            let bytes = std::fs::read(&path).unwrap();
            let value: Value = serde_json::from_slice(&bytes).expect("reader saw a partial file");
            assert_eq!(value["fill"].as_str().unwrap().len(), 2_000_000);
            parses += 1;
        }
        writer.join().unwrap();
        assert!(parses > 0, "control: the reader must have parsed at least once");
    }

    #[derive(Default)]
    struct AtomicBoolFlag(std::sync::atomic::AtomicBool);
    impl AtomicBoolFlag {
        fn set(&self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst)
        }
        fn is_set(&self) -> bool {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[test]
    fn the_global_lock_is_root_lock_exclusive_and_released_on_drop() {
        // why: X3: the global namespace lock stays at `<root>/lock` (it guards
        //      id reservation and prune, nothing else). A second holder must
        //      not get it until the first drops. Positive control: after the
        //      drop it is available.
        // proves: CLAIM(global-lock)
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        assert_eq!(store.lock_path(), dir.path().join("lock"));
        let first = store.lock().unwrap();
        assert!(store.try_lock().unwrap().is_none(), "second holder must be refused");
        assert!(dir.path().join("lock").exists());
        drop(first);
        assert!(store.try_lock().unwrap().is_some(), "free after drop");
    }

    #[test]
    fn watch_locks_are_per_id_files_in_locks_dir_independent_of_each_other_and_of_the_global_lock() {
        // why: X3 (T4), replacing "one lock for everything": each watch has
        //      `locks/<id>.lock`. Holding watch a refuses a second holder of a
        //      but not of b, and neither it nor the global lock blocks the
        //      other: a tick of b (or a create) must never wait behind a's
        //      sensing. Literal paths; positive control: each lock is
        //      available again after its holder drops.
        // proves: CLAIM(per-watch-lock)
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let (a, b) = (wid("a"), wid("b"));
        assert_eq!(store.locks_dir(), dir.path().join("locks"));
        assert_eq!(store.watch_lock_path(&a), dir.path().join("locks").join("a.lock"));

        let held_a = store.lock_watch(&a).unwrap();
        assert!(store.watch_lock_path(&a).is_file(), "created on demand");
        assert!(store.try_lock_watch(&a).unwrap().is_none(), "same watch: refused");
        let held_b = store.try_lock_watch(&b).unwrap().expect("another watch's lock is independent");
        let held_global = store.try_lock().unwrap().expect("the global lock is independent of watch locks");
        assert!(store.try_lock_watch(&b).unwrap().is_none());
        assert!(store.try_lock().unwrap().is_none());
        drop((held_b, held_global));
        assert!(store.try_lock_watch(&b).unwrap().is_some(), "free after drop");
        assert!(store.try_lock().unwrap().is_some(), "free after drop");
        drop(held_a);
        assert!(store.try_lock_watch(&a).unwrap().is_some(), "free after drop");
    }

    #[test]
    fn a_blocked_watch_locker_is_visibly_waiting_then_gets_the_lock_only_after_release() {
        // why: X3 + S25 for the new lock kind: /proc/locks must list the waiter
        //      as BLOCKED on `locks/w.lock` while the guard is held, it must
        //      not have acquired, and only the drop lets it in. Mirrors the
        //      global-lock test so a lock_watch that silently returns without
        //      flock-ing (or polls without blocking forever) is caught.
        // proves: CLAIM(per-watch-lock)
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let id = wid("w");
        let guard = store.lock_watch(&id).unwrap();
        let acquired = std::sync::Arc::new(AtomicBool::new(false));
        let waiter = {
            let (store, id, acquired) = (store.clone(), id.clone(), acquired.clone());
            std::thread::spawn(move || {
                let _held = store.lock_watch(&id).unwrap();
                acquired.store(true, Ordering::SeqCst);
            })
        };
        let started = std::time::Instant::now();
        let mut contended = false;
        while started.elapsed() < std::time::Duration::from_secs(3) {
            if !blocked_flock_waiters(&store.watch_lock_path(&id)).is_empty() {
                contended = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(contended, "the waiter never showed up as blocked in /proc/locks");
        assert!(!acquired.load(Ordering::SeqCst));
        drop(guard);
        waiter.join().unwrap();
        assert!(acquired.load(Ordering::SeqCst));
    }

    #[test]
    fn a_blocked_locker_is_visibly_waiting_then_gets_the_lock_only_after_release() {
        // why: S25: the old test dropped the guard before the waiter could
        //      have contended, so it proved nothing. Here the kernel's own
        //      lock table (/proc/locks) must list the waiter as BLOCKED on the
        //      lock file's inode while we hold it, the waiter must not have
        //      acquired, and only after the drop does it acquire.
        // proves: CLAIM(global-lock)
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let guard = store.lock().unwrap();
        let acquired = std::sync::Arc::new(AtomicBool::new(false));
        let waiter = {
            let (store, acquired) = (store.clone(), acquired.clone());
            std::thread::spawn(move || {
                let _held = store.lock().unwrap();
                acquired.store(true, Ordering::SeqCst);
            })
        };
        let started = std::time::Instant::now();
        let mut contended = false;
        while started.elapsed() < std::time::Duration::from_secs(3) {
            if !blocked_flock_waiters(&store.lock_path()).is_empty() {
                contended = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(contended, "the waiter never showed up as blocked in /proc/locks");
        assert!(!acquired.load(Ordering::SeqCst), "waiter acquired while the guard was held");
        drop(guard);
        waiter.join().unwrap();
        assert!(acquired.load(Ordering::SeqCst), "waiter must acquire after the release");
        assert!(blocked_flock_waiters(&store.lock_path()).is_empty(), "nobody left waiting");
    }

    #[test]
    fn lock_files_are_never_deleted_by_dropping_a_lock_or_while_their_watch_exists() {
        // why: V5 as kept by X3: deleting a lock file splits synchronization (a
        //      waiter holds the old inode while another process locks a new
        //      one). The global lock keeps its inode across lock/unlock and
        //      prune cycles that DO delete other watches; a SURVIVING watch's
        //      lock file keeps its inode too ("never deleted while their watch
        //      file exists"). Positive control: prune really deleted
        //      something. (Prune takes the locks itself, X3, so the test holds
        //      none while calling it.)
        // proves: CLAIM(lock-never-deleted)
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let now = at("2026-10-29T14:03:11Z");
        store.save(&finished("old", TerminalStatus::Cancelled, now.minus_secs(30 * 24 * 3600))).unwrap();
        store.save(&finished("kept", TerminalStatus::Cancelled, now.minus_secs(60))).unwrap();
        drop(store.lock().unwrap());
        drop(store.lock_watch(&wid("old")).unwrap());
        drop(store.lock_watch(&wid("kept")).unwrap());
        let global = lock_identity(&store.lock_path());
        let kept = lock_identity(&store.watch_lock_path(&wid("kept")));

        let deleted = store.prune(now).unwrap();
        assert_eq!(deleted, vec![wid("old")], "control: prune deleted a watch");
        assert!(store.lock_path().exists());
        assert_eq!(lock_identity(&store.lock_path()), global, "same inode after prune");
        assert_eq!(lock_identity(&store.watch_lock_path(&wid("kept"))), kept, "a surviving watch keeps its lock file");
        drop(store.lock().unwrap());
        drop(store.lock_watch(&wid("kept")).unwrap());
        assert_eq!(lock_identity(&store.lock_path()), global, "same inode after another lock cycle");
        assert_eq!(lock_identity(&store.watch_lock_path(&wid("kept"))), kept);
    }

    #[test]
    fn lock_files_exist_only_where_a_lock_was_taken_and_saving_creates_none() {
        // why: X3 replaces "no per-watch lock files are ever created" (which
        //      asserted the opposite of the new design). Saving a watch leaves
        //      only `watches/<id>.json`; taking the watch lock adds exactly
        //      `locks/<id>.lock`; taking the global lock adds exactly `lock`.
        //      Nothing else (no `.lock` beside the json, no `.tmp`).
        // proves: CLAIM(per-watch-lock)
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let names = |sub: &str| -> Vec<String> {
            let mut out: Vec<String> = match std::fs::read_dir(dir.path().join(sub)) {
                // DF4 (specs/disk-full.md): after a save the root also holds
                // `.reserve`. It is not a lock file; its own tests are the
                // "reserve" ones below. Changed from an unfiltered listing.
                Ok(entries) => entries.map(|e| e.unwrap().file_name().to_string_lossy().into()).filter(|name: &String| name != ".reserve").collect(),
                Err(_) => vec![],
            };
            out.sort();
            out
        };
        store.save(&sample("a")).unwrap();
        assert_eq!(names("watches"), vec!["a.json"]);
        // S3 (DF4c) changed this line. Creating the reserve runs under the
        // store-wide lock, so a first save may leave the GLOBAL `lock` file in
        // the root, and only together with the reserve that needed it (today
        // the listing is just `watches`). What this test is about stays
        // exact: a save creates no PER-WATCH lock file, `locks/` does not
        // exist. See the red report, round 3, for the conflict this resolves.
        let root_names = names("");
        assert!(
            root_names == vec!["watches"] || (root_names == vec!["lock", "watches"] && reserve_path(dir.path()).exists()),
            "saving may create the global lock file for the reserve and nothing else: {root_names:?}"
        );
        assert!(!dir.path().join("locks").exists(), "saving creates no per-watch lock file");
        drop(store.lock_watch(&wid("a")).unwrap());
        assert_eq!(names("locks"), vec!["a.lock"]);
        assert_eq!(names("watches"), vec!["a.json"], "no lock file beside the json");
        drop(store.lock().unwrap());
        assert_eq!(names(""), vec!["lock", "locks", "watches"]);
    }

    #[test]
    fn list_returns_watches_in_id_order_and_ignores_lock_and_tmp_files() {
        // why: crashed `.tmp` files and stray `.lock` files in watches/, and
        //      the real lock files under `locks/` and `lock` (X3), must never be
        //      parsed as watches.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.save(&sample("b")).unwrap();
        store.save(&sample("a")).unwrap();
        let _lock = store.lock().unwrap();
        let _watch_lock = store.lock_watch(&wid("a")).unwrap();
        std::fs::write(dir.path().join("watches").join("c.json.tmp"), "{ partial").unwrap();
        std::fs::write(dir.path().join("watches").join("d.lock"), "").unwrap();
        let ids: Vec<String> = store.list().unwrap().iter().map(|w| w.spec.id.to_string()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn prune_deletes_finished_watches_seven_days_after_they_finish() {
        // why: D4 / SPEC "State": finished watches are deleted 7 days after
        //      they finish. Boundary 7 d = 604800 s: exactly 7 d is deleted
        //      (`finished_at + 7d <= now`), 1 s short is kept; every finished
        //      status counts.
        // proves: CLAIM(prune-7d)
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let now = at("2026-10-29T14:03:11Z");
        let seven_days = 7 * 24 * 3600;
        store.save(&finished("fired-old", TerminalStatus::Fired, now.minus_secs(seven_days + 1))).unwrap();
        store.save(&finished("expired-exact", TerminalStatus::Expired, now.minus_secs(seven_days))).unwrap();
        store.save(&finished("cancelled-old", TerminalStatus::Cancelled, now.minus_secs(8 * 24 * 3600))).unwrap();
        store.save(&finished("fired-recent", TerminalStatus::Fired, now.minus_secs(seven_days - 1))).unwrap();

        let mut deleted: Vec<String> = store.prune(now).unwrap().iter().map(|id| id.to_string()).collect();
        deleted.sort();
        assert_eq!(deleted, vec!["cancelled-old", "expired-exact", "fired-old"]);
        let remaining: Vec<String> = store.list().unwrap().iter().map(|w| w.spec.id.to_string()).collect();
        assert_eq!(remaining, vec!["fired-recent"]);
    }

    #[test]
    fn prune_expires_overdue_pending_watches_using_the_deadline_as_the_finish_time() {
        // why: V16 (S16): a pending watch past its absolute deadline counts as
        //      expired with finished_at = deadline_at, so abandoned watches do
        //      not live forever. created 8 days ago + 3600 s deadline = overdue
        //      since 8 d - 1 h > 7 d -> deleted. Created 7 d + 1 h ago =
        //      deadline exactly 7 d ago -> deleted (boundary inclusive);
        //      7 d + 1 h - 1 s ago -> kept. A pending watch with its deadline
        //      still ahead is never pruned, however long ago it was created
        //      (7-day deadline created 6 d ago, control).
        // proves: CLAIM(prune-overdue)
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let now = at("2026-10-29T14:03:11Z");
        let day = 24 * 3600;
        store.save(&pending("abandoned", now, 8 * day)).unwrap();
        store.save(&pending("boundary-in", now, 7 * day + 3600)).unwrap();
        store.save(&pending("boundary-out", now, 7 * day + 3599)).unwrap();
        let mut long_but_live = pending("live", now, 6 * day);
        long_but_live.deadline_at = now.plus_secs(day);
        store.save(&long_but_live).unwrap();

        let mut deleted: Vec<String> = store.prune(now).unwrap().iter().map(|id| id.to_string()).collect();
        deleted.sort();
        assert_eq!(deleted, vec!["abandoned", "boundary-in"]);
        let mut remaining: Vec<String> = store.list().unwrap().iter().map(|w| w.spec.id.to_string()).collect();
        remaining.sort();
        assert_eq!(remaining, vec!["boundary-out", "live"]);
    }

    #[test]
    fn prune_deletes_a_watchs_lock_file_only_after_its_watch_file() {
        // why: X3: "prune deletes a lock file only after deleting its watch
        //      file". The reverse order opens a window where the lock file is
        //      gone while the watch still exists, so another process can create
        //      and lock a NEW lock file and tick the watch concurrently with a
        //      holder of the old one. One inotify instance watches both
        //      directories, so the kernel's own event order is the oracle (no
        //      sampling). Three old watches with lock files, one old watch that
        //      never had a lock file, one survivor. Controls: every old
        //      watch's json deletion is seen (the probe works), the three lock
        //      files are seen deleted too (prune removes them, X3), the
        //      survivor produces no event, and the lock-less watch only its
        //      json.
        // proves: CLAIM(prune-lock-order)
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let now = at("2026-10-29T14:03:11Z");
        for id in ["o1", "o2", "o3", "nolock"] {
            store.save(&finished(id, TerminalStatus::Cancelled, now.minus_secs(30 * 24 * 3600))).unwrap();
        }
        store.save(&finished("keep", TerminalStatus::Cancelled, now.minus_secs(60))).unwrap();
        for id in ["o1", "o2", "o3", "keep"] {
            drop(store.lock_watch(&wid(id)).unwrap());
        }
        let log = DeletionLog::start(&[("watches", &store.watches_dir()), ("locks", &store.locks_dir())]);

        let mut deleted: Vec<String> = store.prune(now).unwrap().iter().map(|id| id.to_string()).collect();
        deleted.sort();
        assert_eq!(deleted, vec!["nolock", "o1", "o2", "o3"]);
        let events = log.drain();
        let position = |name: &str| events.iter().position(|e| e == name);
        for id in ["o1", "o2", "o3"] {
            let (json, lock) = (position(&format!("watches/{id}.json")), position(&format!("locks/{id}.lock")));
            assert!(json.is_some() && lock.is_some(), "{id}: both files must be deleted, events {events:?}");
            assert!(json < lock, "{id}: the lock file was deleted before its watch file: {events:?}");
        }
        assert!(position("watches/nolock.json").is_some(), "{events:?}");
        assert!(!events.iter().any(|e| e.contains("keep")), "the survivor and its lock file are untouched: {events:?}");
        assert!(store.watch_lock_path(&wid("keep")).exists() && store.watch_path(&wid("keep")).exists());
        assert!(!store.watch_lock_path(&wid("o1")).exists());
    }

    #[test]
    fn prune_never_deletes_a_watch_whose_lock_another_holder_has() {
        // why: X3: prune deletes "while holding that watch's lock". A tick or
        //      cancel in flight holds that lock; deleting the watch file under
        //      it would lose the tick's write or resurrect the file. With the
        //      lock held by this test, a prune in another thread must leave
        //      both files in place for 300 ms (it may wait or skip: either is
        //      allowed). After the release a prune finishes the job: both files
        //      are gone (positive control: the watch really was prunable).
        // proves: CLAIM(prune-lock-order)
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let now = at("2026-10-29T14:03:11Z");
        let id = wid("busy");
        store.save(&finished("busy", TerminalStatus::Cancelled, now.minus_secs(30 * 24 * 3600))).unwrap();
        let guard = store.lock_watch(&id).unwrap();
        let pruner = {
            let store = store.clone();
            std::thread::spawn(move || store.prune(now).unwrap())
        };
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(store.watch_path(&id).exists(), "watch file deleted while its lock was held");
        assert!(store.watch_lock_path(&id).exists(), "lock file deleted while it was held");
        drop(guard);
        pruner.join().unwrap();
        store.prune(now).unwrap();
        assert!(!store.watch_path(&id).exists() && !store.watch_lock_path(&id).exists(), "control: prunable once the lock is free");
    }

    #[test]
    fn prune_needs_the_global_lock_to_delete_anything() {
        // why: X3: prune is a namespace operation done under the global lock
        //      (so it cannot interleave with create's id reservation: a
        //      create that checked "id free" must not see its placeholder
        //      pruned). With the global lock held by this test, a prune in
        //      another thread deletes nothing for 300 ms; after the release
        //      the old watch is gone (control).
        // proves: CLAIM(prune-lock-order)
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let now = at("2026-10-29T14:03:11Z");
        store.save(&finished("old", TerminalStatus::Cancelled, now.minus_secs(30 * 24 * 3600))).unwrap();
        let guard = store.lock().unwrap();
        let pruner = {
            let store = store.clone();
            std::thread::spawn(move || store.prune(now).unwrap())
        };
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(store.watch_path(&wid("old")).exists(), "pruned without the global lock");
        drop(guard);
        pruner.join().unwrap();
        store.prune(now).unwrap();
        assert!(!store.watch_path(&wid("old")).exists(), "control: prunable once the lock is free");
    }

    // ---- disk full (specs/disk-full.md DF2, DF4, DF5, DF6) ----

    use crate::fault::{FaultPlan, FaultRule, When, WriteKind};

    /// D1: the reserve is 256 KiB = 256 * 1024 bytes, at `<root>/.reserve`.
    const RESERVE_BYTES: u64 = 262_144;
    const DISK_FULL_ERRNOS: [i32; 2] = [28 /* ENOSPC */, 122 /* EDQUOT */];

    fn rule(kind: WriteKind, errno: i32, when: When) -> FaultRule {
        FaultRule::new(kind, errno, when)
    }

    fn reserve_path(root: &Path) -> PathBuf {
        root.join(".reserve")
    }

    /// A store over `root` with the given faults.
    fn faulted(root: &Path, rules: Vec<FaultRule>) -> (Store, FaultPlan) {
        let plan = FaultPlan::from_rules(rules);
        (Store::new(root).with_faults(plan.clone()), plan)
    }

    /// The disk is full while the reserve exists and cannot make a new reserve:
    /// the model of DF4 (the retry can only succeed because the reserve was
    /// deleted first).
    fn full_until_reserve_is_freed(root: &Path, errno: i32) -> Vec<FaultRule> {
        vec![rule(WriteKind::Watch, errno, When::WhileExists(reserve_path(root))), rule(WriteKind::Reserve, errno, When::Every)]
    }

    /// Every `.tmp` file anywhere under `dir`.
    fn tmp_files(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                found.extend(tmp_files(&path));
            } else if path.to_string_lossy().ends_with(".tmp") {
                found.push(path);
            }
        }
        found
    }

    /// `sample("w")` with a different tick, so "the new content" is visible.
    fn newer(mut watch: WatchFile) -> WatchFile {
        watch.tick += 7;
        watch
    }

    /// DF5b: the state root path and the lowercase words `disk full`.
    fn says_disk_full_naming(error: &StoreError, root: &Path) -> bool {
        let text = error.to_string();
        text.contains(root.to_str().unwrap()) && text.contains("disk full")
    }

    use crate::fault::Stage;

    /// Bytes the filesystem really holds for `path` (`st_blocks * 512`), not
    /// following a symlink.
    fn allocated_bytes(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(path).unwrap().blocks() * 512
    }

    /// DF4b, written from the spec and not from the code: a valid reserve is a
    /// regular file (not a symlink) of exactly 262144 bytes with at least that
    /// much allocated.
    fn is_valid_reserve(path: &Path) -> bool {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata.file_type().is_file() && metadata.len() == RESERVE_BYTES && allocated_bytes(path) >= RESERVE_BYTES,
            Err(_) => false,
        }
    }

    /// `is_valid_reserve` plus "written with real zeros", with a message that
    /// says which part failed.
    fn assert_valid_reserve(root: &Path, context: &str) {
        let reserve = reserve_path(root);
        let metadata = std::fs::symlink_metadata(&reserve).unwrap_or_else(|error| panic!("{context}: no reserve at {}: {error}", reserve.display()));
        assert!(metadata.file_type().is_file(), "{context}: the reserve must be a regular file, not {:?}", metadata.file_type());
        assert_eq!(metadata.len(), RESERVE_BYTES, "{context}: length");
        assert!(
            allocated_bytes(&reserve) >= RESERVE_BYTES,
            "{context}: only {} bytes are allocated for a {RESERVE_BYTES} byte reserve (sparse?)",
            allocated_bytes(&reserve)
        );
        let bytes = std::fs::read(&reserve).unwrap();
        assert!(bytes.iter().all(|byte| *byte == 0), "{context}: the reserve holds real zeros");
    }

    /// Control for every allocation assertion: on THIS filesystem, 256 KiB of
    /// written zeros do count as allocated (a compressing or deduplicating
    /// filesystem would report less and make the assertions unsatisfiable).
    /// Fails loudly instead of skipping.
    fn assert_this_filesystem_allocates_written_zeros() {
        let probe_dir = tempfile::tempdir().unwrap();
        let probe = probe_dir.path().join("zeros");
        std::fs::write(&probe, vec![0u8; RESERVE_BYTES as usize]).unwrap();
        File::open(&probe).unwrap().sync_all().unwrap();
        assert!(
            allocated_bytes(&probe) >= RESERVE_BYTES,
            "this filesystem ({}) reports {} allocated bytes for {RESERVE_BYTES} written zeros: DF4b cannot be tested here",
            probe_dir.path().display(),
            allocated_bytes(&probe)
        );
        let sparse = probe_dir.path().join("sparse");
        File::create(&sparse).unwrap().set_len(RESERVE_BYTES).unwrap();
        assert!(allocated_bytes(&sparse) < RESERVE_BYTES, "this filesystem makes set_len files non-sparse: the sparse cases below cannot be built here");
    }

    #[test]
    fn a_successful_save_leaves_a_256_kib_reserve_at_the_state_root_and_nothing_else_changes() {
        // why: DF4/D1: after any successful write the store creates
        //      `<root>/.reserve`, 256 KiB (256 * 1024 = 262144 bytes), when it
        //      is absent. It sits in the root, not in `watches/`, so `list`
        //      never mistakes it for a watch. A clean write reports
        //      `used_reserve = false` (wire format unchanged when all is well).
        // proves: CLAIM(DF4)
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let outcome = store.save(&sample("w")).unwrap();
        let reserve = reserve_path(dir.path());
        let metadata = std::fs::metadata(&reserve).unwrap_or_else(|error| panic!("no reserve at {}: {error}", reserve.display()));
        assert!(metadata.is_file());
        assert_eq!(metadata.len(), RESERVE_BYTES);
        assert_eq!(outcome, WriteOutcome { used_reserve: false });
        assert_eq!(store.list().unwrap().len(), 1, "the reserve is not a watch");
        assert!(!store.watches_dir().join(".reserve").exists());
        // DF4b (S2): not just 262144 bytes of length: really allocated zeros.
        assert_this_filesystem_allocates_written_zeros();
        assert_valid_reserve(dir.path(), "after the first successful save");
    }

    #[test]
    fn a_valid_reserve_is_not_rewritten() {
        // why: DF4: "created ... when absent". A VALID reserve (DF4b: regular
        //      file, 262144 bytes, all allocated) must be left alone, because
        //      rewriting 256 KiB and fsyncing it on every tick would be
        //      pointless disk traffic. Shown by identity (same inode: no
        //      replace-by-rename), by modification time (no in-place rewrite;
        //      the 30 ms pause outlasts the kernel's coarse timestamp tick, so
        //      a rewrite WOULD change it) and by content. S2 reversed the old
        //      version of this test, which left a ten byte stand-in alone: that
        //      is now an invalid reserve and is replaced (next test).
        // proves: CLAIM(DF4), CLAIM(DF4b)
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.save(&sample("w")).unwrap();
        assert_valid_reserve(dir.path(), "precondition: the first save made a valid reserve");
        let identity = lock_identity(&reserve_path(dir.path()));
        let modified = std::fs::metadata(reserve_path(dir.path())).unwrap().modified().unwrap();
        let bytes = std::fs::read(reserve_path(dir.path())).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));

        store.save(&newer(sample("w"))).unwrap();
        store.save(&sample("other")).unwrap();

        assert_eq!(lock_identity(&reserve_path(dir.path())), identity, "same inode");
        assert_eq!(std::fs::metadata(reserve_path(dir.path())).unwrap().modified().unwrap(), modified, "not rewritten in place");
        assert_eq!(std::fs::read(reserve_path(dir.path())).unwrap(), bytes);
    }

    #[test]
    fn an_invalid_reserve_is_replaced_by_the_next_successful_write() {
        // why: DF4b (S2, reverses the old ten-byte "preserve" control): a
        //      reserve that is short, long, empty, sparse, half sparse or not a
        //      regular file frees nothing (or the wrong thing) when it is
        //      deleted, so the next successful write must replace it with a
        //      valid one: regular file, exactly 262144 bytes, `st_blocks * 512
        //      >= 262144`, real zeros. Each fixture is first shown INVALID by
        //      `is_valid_reserve` (written from the spec), so a fixture that
        //      the filesystem silently made valid cannot pass the test. A
        //      symlink is not a regular file: it is replaced by one, and the
        //      write must NOT go through it (the dangling target must not
        //      appear, the valid file it pointed to must not change).
        //      The "ten bytes" row is the old control, reversed.
        // proves: CLAIM(DF4b)
        assert_this_filesystem_allocates_written_zeros();
        // (name, how the invalid reserve is built). `outside` is a directory
        // beside the state root, for symlink targets.
        let setup = |name: &str, reserve: &Path, outside: &Path| match name {
            "ten bytes" => std::fs::write(reserve, "0123456789").unwrap(),
            "one byte short" => std::fs::write(reserve, vec![0u8; 262_143]).unwrap(),
            "one byte long" => std::fs::write(reserve, vec![0u8; 262_145]).unwrap(),
            "empty" => std::fs::write(reserve, "").unwrap(),
            "sparse, full length" => File::create(reserve).unwrap().set_len(262_144).unwrap(),
            "half real, half sparse" => {
                std::fs::write(reserve, vec![0u8; 131_072]).unwrap();
                std::fs::OpenOptions::new().write(true).open(reserve).unwrap().set_len(262_144).unwrap();
            }
            "dangling symlink" => std::os::unix::fs::symlink(outside.join("not-there"), reserve).unwrap(),
            "symlink to a valid-looking file" => {
                let target = outside.join("valid-looking");
                std::fs::write(&target, vec![0u8; 262_144]).unwrap();
                File::open(&target).unwrap().sync_all().unwrap();
                std::os::unix::fs::symlink(&target, reserve).unwrap();
            }
            other => panic!("unknown fixture {other}"),
        };
        let listing = |dir: &Path| -> Vec<(std::ffi::OsString, u64)> {
            let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().map(|entry| entry.unwrap()).map(|entry| (entry.file_name(), entry.metadata().unwrap().len())).collect();
            entries.sort();
            entries
        };
        let names = [
            "ten bytes",
            "one byte short",
            "one byte long",
            "empty",
            "sparse, full length",
            "half real, half sparse",
            "dangling symlink",
            "symlink to a valid-looking file",
        ];
        for name in names {
            let dir = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            setup(name, &reserve_path(dir.path()), outside.path());
            assert!(!is_valid_reserve(&reserve_path(dir.path())), "{name}: the fixture must be an INVALID reserve");
            let outside_before = listing(outside.path());

            let outcome = Store::new(dir.path()).save(&sample("w")).unwrap_or_else(|error| panic!("{name}: {error}"));

            assert_eq!(outcome, WriteOutcome { used_reserve: false }, "{name}");
            assert_valid_reserve(dir.path(), name);
            assert_eq!(listing(outside.path()), outside_before, "{name}: nothing outside the state root may be written through a symlink");
            assert!(tmp_files(dir.path()).is_empty(), "{name}");
            assert_eq!(Store::new(dir.path()).list().unwrap().len(), 1, "{name}: the reserve is still not a watch");
        }
    }

    #[test]
    fn a_reserve_that_could_not_be_written_whole_never_looks_valid_and_the_next_write_repairs_it() {
        // why: DF4 ("Failure to create ... is never itself an error") + S2 +
        //      S4 (seam stages for the reserve: create, short write). Case 1:
        //      creation fails before the file exists, so there is NO reserve.
        //      Case 2: half of the zeros are written, then the failure; the
        //      store may or may not remove the half file, but whatever stays
        //      must not pass for a reserve (shorter than 262144 bytes), else
        //      a later disk-full would "free" nothing and hide that. In both
        //      cases the watch write itself succeeded and the fault really
        //      fired where it was aimed (`injected`). Once the fault is gone
        //      the next successful write leaves a valid reserve (repair).
        // proves: CLAIM(DF4), CLAIM(DF4b), CLAIM(S4)
        for stage in [Stage::Create, Stage::ShortWrite] {
            let dir = tempfile::tempdir().unwrap();
            let (store, plan) = faulted(dir.path(), vec![rule(WriteKind::Reserve, 28, When::Every).at(stage)]);
            let watch = sample("w");

            store.save(&watch).unwrap_or_else(|error| panic!("{stage:?}: a reserve failure must not fail the save: {error}"));

            assert_eq!(store.load(&wid("w")).unwrap(), watch, "{stage:?}");
            assert!(plan.injected(WriteKind::Reserve, stage) >= 1, "{stage:?}: the fault never fired at its stage");
            match std::fs::symlink_metadata(reserve_path(dir.path())) {
                Err(_) => {}
                Ok(left) => {
                    assert_ne!(stage, Stage::Create, "no file may exist when creation itself failed");
                    assert!(left.len() < RESERVE_BYTES, "{stage:?}: a half-written reserve must not be full length, got {}", left.len());
                }
            }
            assert!(!is_valid_reserve(&reserve_path(dir.path())), "{stage:?}");

            Store::new(dir.path()).save(&newer(watch)).unwrap();
            assert_valid_reserve(dir.path(), &format!("{stage:?}: repaired by the next successful write"));
        }
    }

    #[test]
    fn failing_to_create_the_reserve_is_never_an_error() {
        // why: DF4: "Failure to create or delete the reserve is never itself
        //      an error." The reserve is created on a nearly full disk, where
        //      it often cannot be. The watch write must still succeed and the
        //      creation must actually have been tried (attempts), else this
        //      only shows that nothing happened.
        // proves: CLAIM(DF4)
        let dir = tempfile::tempdir().unwrap();
        let (store, plan) = faulted(dir.path(), vec![rule(WriteKind::Reserve, 28, When::Every)]);
        let watch = sample("w");
        store.save(&watch).unwrap();
        assert_eq!(store.load(&wid("w")).unwrap(), watch);
        assert!(plan.attempts(WriteKind::Reserve) >= 1, "the store never tried to create the reserve");
        assert!(!reserve_path(dir.path()).exists());
    }

    #[test]
    fn disk_full_on_the_first_attempt_frees_the_reserve_retries_once_and_says_it_used_it() {
        // why: DF4 (the incident): on ENOSPC (28) or EDQUOT (122) the store
        //      deletes `.reserve` and retries the same write ONCE. The fault is
        //      "full while the reserve exists", so the retry can only succeed
        //      if the reserve was deleted BEFORE it (order is proven, not
        //      assumed). Reserve creation is also failing (no room to
        //      recreate it), as on a real full disk. Result: new content on
        //      disk, `used_reserve = true`, reserve gone, no .tmp, exactly two
        //      Watch attempts.
        // proves: CLAIM(DF4), CLAIM(DF6)
        for errno in DISK_FULL_ERRNOS {
            let dir = tempfile::tempdir().unwrap();
            let before = sample("w");
            Store::new(dir.path()).save(&before).unwrap();
            assert!(reserve_path(dir.path()).exists(), "precondition: errno {errno}: a reserve exists");

            let (store, plan) = faulted(dir.path(), full_until_reserve_is_freed(dir.path(), errno));
            let after = newer(before);
            let outcome = store.save(&after).unwrap_or_else(|error| panic!("errno {errno}: the retry should have succeeded: {error}"));

            assert_eq!(outcome, WriteOutcome { used_reserve: true }, "errno {errno}");
            assert_eq!(store.load(&wid("w")).unwrap(), after, "errno {errno}: the new content is on disk");
            assert!(!reserve_path(dir.path()).exists(), "errno {errno}: the reserve was spent");
            assert_eq!(plan.attempts(WriteKind::Watch), 2, "errno {errno}: one attempt, one retry");
            assert!(tmp_files(dir.path()).is_empty(), "errno {errno}");
        }
    }

    #[test]
    fn after_spending_the_reserve_a_new_one_is_made_as_soon_as_there_is_room() {
        // why: DF4 says the reserve is created "after any successful write
        //      when absent" and the retry is a successful write. Here the disk
        //      had room for the retry AND for a new reserve (only the watch
        //      write was blocked, by the old reserve), so the next write leaves
        //      the store protected again. Pins the literal reading; see the red
        //      report for the open question about a disk that is truly full.
        // proves: CLAIM(DF4)
        let dir = tempfile::tempdir().unwrap();
        Store::new(dir.path()).save(&sample("w")).unwrap();
        let plan_rules = vec![rule(WriteKind::Watch, 28, When::WhileExists(reserve_path(dir.path())))];
        let (store, _) = faulted(dir.path(), plan_rules);
        assert!(store.save(&newer(sample("w"))).unwrap().used_reserve, "precondition: the reserve was spent");
        assert_eq!(std::fs::metadata(reserve_path(dir.path())).unwrap().len(), RESERVE_BYTES);
    }

    #[test]
    fn a_disk_that_stays_full_fails_after_exactly_one_retry_with_a_message_that_names_the_root() {
        // why: DF4 ("retries the same write once") + DF5 (an error that names
        //      the state root and says the disk is full, not a bare `io error`)
        //      + DF6 (the previous file is intact, no .tmp). Both attempts
        //      fail, so: two attempts, not one and not a loop; the reserve is
        //      gone (it was deleted for the retry); the error is the DiskFull
        //      variant.
        //      DF5b: the message has the root path and lowercase `disk full`.
        // proves: CLAIM(DF4), CLAIM(DF5), CLAIM(DF5b), CLAIM(DF6)
        for errno in DISK_FULL_ERRNOS {
            let dir = tempfile::tempdir().unwrap();
            let before = sample("w");
            Store::new(dir.path()).save(&before).unwrap();
            let bytes_before = std::fs::read(Store::new(dir.path()).watch_path(&wid("w"))).unwrap();

            let (store, plan) =
                faulted(dir.path(), vec![rule(WriteKind::Watch, errno, When::Every), rule(WriteKind::Reserve, errno, When::Every)]);
            let error = store.save(&newer(before.clone())).expect_err("both attempts fail");

            assert!(matches!(error, StoreError::DiskFull(_)), "errno {errno}: got {error:?}");
            assert!(says_disk_full_naming(&error, dir.path()), "errno {errno}: {error}");
            assert_eq!(plan.attempts(WriteKind::Watch), 2, "errno {errno}: one retry, no loop");
            assert!(!reserve_path(dir.path()).exists(), "errno {errno}: deleted for the retry");
            assert_eq!(std::fs::read(store.watch_path(&wid("w"))).unwrap(), bytes_before, "errno {errno}: previous file byte-identical");
            assert_eq!(store.load(&wid("w")).unwrap(), before);
            assert!(tmp_files(dir.path()).is_empty(), "errno {errno}: no .tmp");
        }
    }

    #[test]
    fn with_no_reserve_file_a_full_disk_is_reported_at_the_first_failure_with_no_retry() {
        // why: DF4a (red report A4): "No reserve file, no retry: the first
        //      failure is reported (`used_reserve` absent)." The reserve is
        //      removed after a first save. The fault hits ONLY the first Watch
        //      attempt (`Nth(1)`), so a retry would succeed and hide the
        //      failure; the save must instead fail as DiskFull naming the
        //      root, after exactly ONE attempt, with the old file intact and
        //      no .tmp. Control in the same test: with a reserve the identical
        //      fault is rescued by the retry (2 attempts, `used_reserve`).
        // proves: CLAIM(DF4a), CLAIM(DF5b)
        for errno in DISK_FULL_ERRNOS {
            let rescued = tempfile::tempdir().unwrap();
            Store::new(rescued.path()).save(&sample("w")).unwrap();
            assert!(reserve_path(rescued.path()).exists(), "precondition: errno {errno}: a reserve exists");
            let (store, plan) = faulted(rescued.path(), vec![rule(WriteKind::Watch, errno, When::Nth(1)), rule(WriteKind::Reserve, errno, When::Every)]);
            assert!(store.save(&newer(sample("w"))).unwrap().used_reserve, "control: errno {errno}: with a reserve the retry rescues it");
            assert_eq!(plan.attempts(WriteKind::Watch), 2, "control: errno {errno}");

            let dir = tempfile::tempdir().unwrap();
            let before = sample("w");
            Store::new(dir.path()).save(&before).unwrap();
            let _ = std::fs::remove_file(reserve_path(dir.path()));
            assert!(!reserve_path(dir.path()).exists(), "precondition: errno {errno}: no reserve");
            let bytes_before = std::fs::read(Store::new(dir.path()).watch_path(&wid("w"))).unwrap();

            let (store, plan) = faulted(dir.path(), vec![rule(WriteKind::Watch, errno, When::Nth(1)), rule(WriteKind::Reserve, errno, When::Every)]);
            let error = store.save(&newer(before.clone())).expect_err("nothing to free: the first failure is final");

            assert!(matches!(error, StoreError::DiskFull(_)), "errno {errno}: got {error:?}");
            assert!(says_disk_full_naming(&error, dir.path()), "errno {errno}: {error}");
            assert_eq!(plan.attempts(WriteKind::Watch), 1, "errno {errno}: no reserve, no retry");
            assert_eq!(std::fs::read(store.watch_path(&wid("w"))).unwrap(), bytes_before, "errno {errno}");
            assert!(tmp_files(dir.path()).is_empty(), "errno {errno}");
        }
    }

    #[test]
    fn errors_that_are_not_a_full_disk_neither_retry_nor_touch_the_reserve() {
        // why: DF4: only disk_full deletes the reserve. EIO (5), EACCES (13)
        //      and EROFS (30) would not be fixed by 256 KiB, and deleting the
        //      reserve for them would spend the protection for nothing. One
        //      attempt, reserve byte-for-byte present (same inode, full size),
        //      error is the plain Io variant (DF2: kind `io`), old file intact.
        //      Control: with the fault gone the same save works.
        // proves: CLAIM(DF4), CLAIM(DF2)
        for errno in [5, 13, 30] {
            let dir = tempfile::tempdir().unwrap();
            let before = sample("w");
            Store::new(dir.path()).save(&before).unwrap();
            assert!(reserve_path(dir.path()).exists(), "precondition: errno {errno}: a save leaves a reserve");
            let reserve_before = lock_identity(&reserve_path(dir.path()));

            let (store, plan) = faulted(dir.path(), vec![rule(WriteKind::Watch, errno, When::Every)]);
            let error = store.save(&newer(before.clone())).expect_err("the write fails");

            assert!(matches!(error, StoreError::Io(_)), "errno {errno}: got {error:?}");
            assert_eq!(plan.attempts(WriteKind::Watch), 1, "errno {errno}: no retry");
            assert_eq!(lock_identity(&reserve_path(dir.path())), reserve_before, "errno {errno}: same reserve file");
            assert_eq!(std::fs::metadata(reserve_path(dir.path())).unwrap().len(), RESERVE_BYTES);
            assert_eq!(store.load(&wid("w")).unwrap(), before);
            assert!(tmp_files(dir.path()).is_empty());
        }
        let dir = tempfile::tempdir().unwrap();
        Store::new(dir.path()).save(&sample("w")).unwrap();
        Store::new(dir.path()).save(&newer(sample("w"))).expect("control: no fault, no failure");
    }

    #[test]
    fn a_reserve_that_cannot_be_deleted_does_not_turn_the_retry_into_an_error() {
        // why: DF4: "Failure to create or delete the reserve is never itself
        //      an error." `.reserve` is made a directory, so `remove_file`
        //      fails (EISDIR). The first attempt fails once with ENOSPC; the
        //      retry must still happen and succeed, and the directory is left
        //      alone.
        // proves: CLAIM(DF4)
        let dir = tempfile::tempdir().unwrap();
        let before = sample("w");
        Store::new(dir.path()).save(&before).unwrap();
        assert!(reserve_path(dir.path()).exists(), "precondition: a save leaves a reserve");
        std::fs::remove_file(reserve_path(dir.path())).unwrap();
        std::fs::create_dir(reserve_path(dir.path())).unwrap();

        let (store, plan) = faulted(dir.path(), vec![rule(WriteKind::Watch, 28, When::Nth(1)), rule(WriteKind::Reserve, 28, When::Every)]);
        let after = newer(before);
        store.save(&after).expect("deleting the reserve failed, the retry still runs and succeeds");
        assert_eq!(store.load(&wid("w")).unwrap(), after);
        assert_eq!(plan.attempts(WriteKind::Watch), 2);
        assert!(reserve_path(dir.path()).is_dir());
    }

    #[test]
    fn a_failed_write_removes_its_tmp_file_and_keeps_the_previous_content() {
        // why: DF6: the existing guarantee (store.rs `write_atomic`) must hold
        //      for the new failure paths. The seam fires AFTER `w.json.tmp` is
        //      created, so the cleanup is really exercised: an inotify log of
        //      the watches directory must show that `.tmp` being deleted, and
        //      afterwards only `w.json` is left, byte-identical to before.
        //      Errors tried: ENOSPC and EIO (the retry path and the plain path).
        // proves: CLAIM(DF6)
        for errno in [28, 5] {
            let dir = tempfile::tempdir().unwrap();
            Store::new(dir.path()).save(&sample("w")).unwrap();
            let watches = dir.path().join("watches");
            let bytes_before = std::fs::read(watches.join("w.json")).unwrap();
            let log = DeletionLog::start(&[("watches", watches.as_path())]);

            let (store, _) = faulted(dir.path(), vec![rule(WriteKind::Watch, errno, When::Every)]);
            assert!(store.save(&newer(sample("w"))).is_err(), "precondition: errno {errno}: the write fails");

            let deleted = log.drain();
            assert!(deleted.contains(&"watches/w.json.tmp".to_string()), "errno {errno}: the tmp file was never created and removed: {deleted:?}");
            let mut names: Vec<String> = std::fs::read_dir(&watches).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into()).collect();
            names.sort();
            assert_eq!(names, vec!["w.json".to_string()], "errno {errno}");
            assert_eq!(std::fs::read(watches.join("w.json")).unwrap(), bytes_before, "errno {errno}");
        }
    }

    #[test]
    fn a_real_enospc_from_the_kernel_is_a_disk_full_error_and_still_atomic() {
        // why: DF2/DF6 without the seam: `/dev/full` is a device that answers
        //      every write with a real ENOSPC. `w.json.tmp` is a symlink to it,
        //      so `write_all` fails with the kernel's own errno 28. This is
        //      the independent check that the classification works on a real
        //      error and not only on the one the seam builds. The old file must
        //      be intact and the `.tmp` (the symlink) gone (passes today); the
        //      error must be DiskFull, not a bare Io (red today).
        // proves: CLAIM(DF2), CLAIM(DF6)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.json");
        write_atomic(&path, &serde_json::json!({"v": "old"})).unwrap();
        let tmp = dir.path().join("w.json.tmp");
        std::os::unix::fs::symlink("/dev/full", &tmp).unwrap();
        // Control: this is a real ENOSPC, not just any failure.
        let real = std::fs::write(&tmp, b"x").expect_err("/dev/full refuses writes");
        assert_eq!(real.raw_os_error(), Some(28), "{real}");

        let outcome = write_atomic(&path, &serde_json::json!({"v": "new"}));

        assert_eq!(serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap()["v"], "old");
        assert!(std::fs::symlink_metadata(&tmp).is_err(), "the .tmp entry must be removed");
        assert!(matches!(outcome, Err(StoreError::DiskFull(_))), "ENOSPC must classify as DiskFull, got {outcome:?}");
    }

    /// Take a lock through the named store entry point.
    fn take(store: &Store, entry: &str) -> Result<(), StoreError> {
        let id = wid("w");
        match entry {
            "lock" => store.lock().map(drop),
            "try_lock" => store.try_lock().map(drop),
            "lock_watch" => store.lock_watch(&id).map(drop),
            "try_lock_watch" => store.try_lock_watch(&id).map(drop),
            other => panic!("unknown entry {other}"),
        }
    }
    const LOCK_ENTRIES: [&str; 4] = ["lock", "try_lock", "lock_watch", "try_lock_watch"];

    /// The lock file `entry` creates, which must not exist for its creation to
    /// be an attempt (the seam fires on creation only). S3 (DF4c) makes a
    /// first save create the global `<root>/lock` as a side effect of making
    /// the reserve under the store-wide lock, so the global one is removed
    /// here, and its absence is asserted for every entry point.
    fn forget_the_lock_file_to_create(root: &Path, entry: &str) {
        let store = Store::new(root);
        let file = match entry {
            "lock" | "try_lock" => store.lock_path(),
            _ => store.watch_lock_path(&wid("w")),
        };
        let _ = std::fs::remove_file(&file);
        assert!(!file.exists(), "precondition: {entry}: {} must not exist, so taking the lock has to create it", file.display());
    }

    #[test]
    fn creating_a_lock_file_on_a_full_disk_goes_through_the_reserve_and_retry_on_every_entry_point() {
        // why: DF5: "per-watch lock-file creation" uses the same
        //      reserve-and-retry path. There are four ways to take a lock
        //      (global/per-watch, blocking/try), all through `open_lock_file`;
        //      each is checked, so a retry wired into only one of them fails
        //      here. Rescue: full while the reserve exists, lock created after
        //      the reserve is spent. Persistent: an error naming the root.
        //      DF4a: with no reserve to free, the first failure is final.
        // proves: CLAIM(DF5), CLAIM(DF4a)
        for entry in LOCK_ENTRIES {
            let dir = tempfile::tempdir().unwrap();
            Store::new(dir.path()).save(&sample("w")).unwrap();
            assert!(reserve_path(dir.path()).exists(), "precondition: {entry}: a save leaves a reserve");
            forget_the_lock_file_to_create(dir.path(), entry);
            let rescue = vec![
                rule(WriteKind::Lock, 28, When::WhileExists(reserve_path(dir.path()))),
                rule(WriteKind::Reserve, 28, When::Every),
            ];
            let (store, _) = faulted(dir.path(), rescue);
            take(&store, entry).unwrap_or_else(|error| panic!("{entry}: the retry should have created the lock: {error}"));
            assert!(!reserve_path(dir.path()).exists(), "{entry}: the reserve was spent");

            // A reserve exists here: the retry is made (2 attempts). DF4a
            // (nothing to free, no retry) is the third leg below.
            let stuck = tempfile::tempdir().unwrap();
            Store::new(stuck.path()).save(&sample("w")).unwrap();
            assert!(reserve_path(stuck.path()).exists(), "precondition: {entry}: a save leaves a reserve");
            forget_the_lock_file_to_create(stuck.path(), entry);
            let (store, plan) = faulted(stuck.path(), vec![rule(WriteKind::Lock, 28, When::Every)]);
            let error = take(&store, entry).expect_err("the lock file can never be created");
            assert!(matches!(error, StoreError::DiskFull(_)), "{entry}: {error:?}");
            assert!(says_disk_full_naming(&error, stuck.path()), "{entry}: {error}");
            assert_eq!(plan.attempts(WriteKind::Lock), 2, "{entry}: one retry");

            // DF4a: no reserve, no retry. The fault hits only the first
            // creation attempt, so a retry would have taken the lock.
            let bare = tempfile::tempdir().unwrap();
            Store::new(bare.path()).save(&sample("w")).unwrap();
            let _ = std::fs::remove_file(reserve_path(bare.path()));
            assert!(!reserve_path(bare.path()).exists(), "precondition: {entry}: no reserve");
            forget_the_lock_file_to_create(bare.path(), entry);
            let (store, plan) = faulted(bare.path(), vec![rule(WriteKind::Lock, 28, When::Nth(1)), rule(WriteKind::Reserve, 28, When::Every)]);
            let error = take(&store, entry).expect_err("nothing to free, so the first failure is final");
            assert!(matches!(error, StoreError::DiskFull(_)), "{entry}: {error:?}");
            assert!(says_disk_full_naming(&error, bare.path()), "{entry}: {error}");
            assert_eq!(plan.attempts(WriteKind::Lock), 1, "{entry}: no reserve, no retry (DF4a)");
        }
    }

    #[test]
    fn taking_a_lock_whose_file_exists_needs_no_disk_space_and_is_never_faulted() {
        // why: seam semantics (fault.rs): opening an existing lock file writes
        //      nothing, so a full disk cannot fail it. Control for the test
        //      above (it proves the Lock rule fires on CREATION, not on every
        //      open) and for DF1: a tick on a full disk still gets its lock.
        // proves: CLAIM(DF-seam), CLAIM(DF5)
        let dir = tempfile::tempdir().unwrap();
        for entry in LOCK_ENTRIES {
            take(&Store::new(dir.path()), entry).unwrap_or_else(|error| panic!("{entry}: {error}"));
        }
        let (store, plan) = faulted(dir.path(), vec![rule(WriteKind::Lock, 28, When::Every)]);
        for entry in LOCK_ENTRIES {
            take(&store, entry).unwrap_or_else(|error| panic!("{entry}: existing lock file, full disk: {error}"));
        }
        assert_eq!(plan.attempts(WriteKind::Lock), 0, "no creation was attempted");
    }

    // ---- spec review round 1: S3 (DF4c), S4 (stages) ----

    /// Poll `check` until it holds or `limit` passes; polling is bounded and
    /// on an observable event, never a bare sleep-and-hope.
    fn wait_until(limit: std::time::Duration, mut check: impl FnMut() -> bool) -> bool {
        let end = std::time::Instant::now() + limit;
        while std::time::Instant::now() < end {
            if check() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        check()
    }

    #[test]
    fn two_rescues_racing_on_one_root_spend_the_reserve_exactly_once() {
        // why: DF4c (S3): reserve delete and retry run under the store-wide
        //      lock, so two processes cannot both spend the one reserve.
        //      Two threads (two stores over ONE root, standing in for two
        //      processes, started together by a barrier) save two different
        //      watches, `a` and `b`. Each one's FIRST write fails with ENOSPC
        //      (a per-file `Nth(1)`, so the outcome does not depend on which
        //      thread writes first), no room to re-create a reserve. Whoever
        //      takes the lock first deletes the reserve and its retry
        //      succeeds; the other then finds NO reserve and, by DF4a, reports
        //      the disk full without a retry. Without the lock both can see
        //      the reserve, both delete and retry, both "use" it. Expected in
        //      every round, by construction: exactly one Ok with
        //      `used_reserve`, exactly one `DiskFull` naming the root, three
        //      watch attempts (two firsts and one retry, never two retries),
        //      the saved watch has the new content and the other the old.
        //      Repeated on fresh roots because the unlocked window is narrow;
        //      `two_rescues_serialise_...` below is the deterministic twin.
        // proves: CLAIM(DF4c), CLAIM(DF4a)
        const ROUNDS: usize = 60;
        for round in 0..ROUNDS {
            let dir = tempfile::tempdir().unwrap();
            let healthy = Store::new(dir.path());
            healthy.save(&sample("a")).unwrap();
            healthy.save(&sample("b")).unwrap();
            assert_valid_reserve(dir.path(), &format!("round {round}: precondition"));
            let plan = FaultPlan::from_rules(vec![
                rule(WriteKind::Watch, 28, When::Nth(1)).only("a.json"),
                rule(WriteKind::Watch, 28, When::Nth(1)).only("b.json"),
                rule(WriteKind::Reserve, 28, When::Every),
            ]);
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let threads: Vec<_> = ["a", "b"]
                .into_iter()
                .map(|id| {
                    let store = Store::new(dir.path()).with_faults(plan.clone());
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        (id, store.save(&newer(sample(id))))
                    })
                })
                .collect();
            let results: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();

            let spent: Vec<&str> = results.iter().filter(|(_, result)| matches!(result, Ok(outcome) if outcome.used_reserve)).map(|(id, _)| *id).collect();
            let refused: Vec<&str> = results.iter().filter(|(_, result)| matches!(result, Err(StoreError::DiskFull(_)))).map(|(id, _)| *id).collect();
            assert_eq!(spent.len(), 1, "round {round}: exactly one rescue may consume the reserve, got {results:?}");
            assert_eq!(refused.len(), 1, "round {round}: the other must report the disk full, got {results:?}");
            assert_ne!(spent, refused, "round {round}");
            let error = results.iter().find_map(|(_, result)| result.as_ref().err()).unwrap();
            assert!(says_disk_full_naming(error, dir.path()), "round {round}: {error}");
            assert_eq!(plan.attempts(WriteKind::Watch), 3, "round {round}: two first attempts and ONE retry");
            assert!(!reserve_path(dir.path()).exists(), "round {round}: spent");
            let reader = Store::new(dir.path());
            assert_eq!(reader.load(&wid(spent[0])).unwrap(), newer(sample(spent[0])), "round {round}: the rescued watch is saved");
            assert_eq!(reader.load(&wid(refused[0])).unwrap(), sample(refused[0]), "round {round}: the refused watch is untouched");
            assert!(tmp_files(dir.path()).is_empty(), "round {round}");
        }
    }

    #[test]
    fn two_rescues_serialise_on_the_store_wide_lock_and_a_held_lock_delays_the_reserve_untouched() {
        // why: DF4c (S3), the deterministic twin of the race test. The test
        //      itself holds the store-wide lock (`Store::lock`, the global
        //      `<root>/lock`) while a thread saves a watch whose first write
        //      fails. The thread must reach its first failure (observed
        //      through the attempt counter, not guessed) and then WAIT: the
        //      reserve is not deleted and no retry is made while the lock is
        //      held elsewhere. 300 ms is a settle window for the negative
        //      claim only; the positive claims are the polls before and the
        //      join after. After the release the rescue completes: retry made,
        //      reserve spent, `used_reserve`. An implementation that deletes
        //      the reserve without the lock finishes inside the window and
        //      fails the first assertion. Interpretation recorded in the red
        //      report: the rescue BLOCKS for the lock (it does not try-lock
        //      and give up), else the post-release assertions fail.
        // proves: CLAIM(DF4c)
        let dir = tempfile::tempdir().unwrap();
        Store::new(dir.path()).save(&sample("w")).unwrap();
        assert_valid_reserve(dir.path(), "precondition");
        let plan = FaultPlan::from_rules(vec![rule(WriteKind::Watch, 28, When::Nth(1)), rule(WriteKind::Reserve, 28, When::Every)]);
        let store = Store::new(dir.path()).with_faults(plan.clone());
        let guard = store.lock().unwrap();

        let worker = {
            let store = store.clone();
            std::thread::spawn(move || store.save(&newer(sample("w"))))
        };
        assert!(
            wait_until(std::time::Duration::from_secs(10), || plan.attempts(WriteKind::Watch) >= 1),
            "precondition: the thread never made its first (failing) attempt"
        );
        std::thread::sleep(std::time::Duration::from_millis(300));

        assert!(!worker.is_finished(), "the rescue finished while another holder had the store-wide lock");
        assert!(is_valid_reserve(&reserve_path(dir.path())), "the reserve was touched before the lock was won");
        assert_eq!(plan.attempts(WriteKind::Watch), 1, "no retry before the lock was won");

        drop(guard);
        let outcome = worker.join().unwrap().expect("once the lock is free the rescue goes through");
        assert_eq!(outcome, WriteOutcome { used_reserve: true });
        assert_eq!(plan.attempts(WriteKind::Watch), 2);
        assert!(!reserve_path(dir.path()).exists());
        assert_eq!(store.load(&wid("w")).unwrap(), newer(sample("w")));
    }

    #[test]
    fn a_failure_at_any_write_stage_keeps_the_old_file_and_leaves_no_tmp() {
        // why: S4 + DF6: the seam injects at each stage of `write_atomic`:
        //      create (this is also how inode exhaustion looks: ENOSPC while
        //      creating), short write, sync_all, rename (and write, before the
        //      first byte). Whatever the stage, the old watch file is
        //      byte-identical, no `.tmp` remains, and the error classifies by
        //      its errno: ENOSPC -> DiskFull naming the root, with the retry
        //      (the reserve existed: 2 attempts); EIO -> Io, no retry (1
        //      attempt). `injected` shows the fault fired at the stage aimed
        //      at on EVERY attempt. The inotify log shows WHEN the `.tmp`
        //      lived: never for `create` (the fault comes before the file
        //      exists), created and removed once per attempt for the others.
        //      The rename is the commit point: even with a complete `.tmp`
        //      the old file stays the watch until it succeeds.
        // proves: CLAIM(S4), CLAIM(DF6), CLAIM(DF2)
        let stages = [Stage::Create, Stage::Write, Stage::ShortWrite, Stage::Sync, Stage::Rename];
        for stage in stages {
            for errno in [28, 5] {
                let context = format!("{stage:?}/errno {errno}");
                let dir = tempfile::tempdir().unwrap();
                let before = sample("w");
                Store::new(dir.path()).save(&before).unwrap();
                let bytes_before = std::fs::read(Store::new(dir.path()).watch_path(&wid("w"))).unwrap();
                let watches = dir.path().join("watches");
                let log = DeletionLog::start(&[("watches", watches.as_path())]);
                let (store, plan) = faulted(dir.path(), vec![rule(WriteKind::Watch, errno, When::Every).at(stage), rule(WriteKind::Reserve, errno, When::Every)]);

                let error = store.save(&newer(before.clone())).expect_err(&context);

                let expected_attempts = if errno == 28 { 2 } else { 1 };
                if errno == 28 {
                    assert!(matches!(error, StoreError::DiskFull(_)) && says_disk_full_naming(&error, dir.path()), "{context}: {error:?}");
                } else {
                    assert!(matches!(error, StoreError::Io(_)), "{context}: {error:?}");
                }
                assert_eq!(plan.attempts(WriteKind::Watch), expected_attempts, "{context}");
                assert_eq!(plan.injected(WriteKind::Watch, stage), expected_attempts, "{context}: the fault must fire at its stage on each attempt");
                // inotify merges identical consecutive events that were not read in
                // between, so two attempts can show as ONE deletion: only
                // "never" versus "at least once" is observable here.
                let tmp_deleted = log.drain().into_iter().any(|entry| entry == "watches/w.json.tmp");
                assert_eq!(tmp_deleted, stage != Stage::Create, "{context}: the .tmp exists (and is removed) exactly when the fault is after its creation");
                assert_eq!(std::fs::read(store.watch_path(&wid("w"))).unwrap(), bytes_before, "{context}: old file byte-identical");
                assert!(tmp_files(dir.path()).is_empty(), "{context}");
            }
        }
    }

    #[test]
    fn a_tmp_that_cleanup_could_not_remove_holds_exactly_what_the_failed_stage_had_written() {
        // why: S4: "a failed cleanup of .tmp is best effort and the leftover is
        //      removed at the next successful write". The `cleanup` stage makes
        //      the removal fail, which exposes the `.tmp` at the moment of the
        //      failure and so pins WHAT each stage had done by then. Oracle:
        //      the bytes a healthy save of the same watch writes (in another
        //      directory), called `whole`:
        //        write  -> .tmp is empty (failed before any byte)
        //        short  -> exactly the first half, `whole.len() / 2` bytes
        //                  (a strict, non-empty prefix: a short write)
        //        sync   -> all of `whole` (bytes written, not yet synced)
        //        rename -> all of `whole` (synced, not yet committed)
        //      The save's own error (EIO) is still the one reported (cleanup is
        //      best effort), `injected` shows the cleanup fault fired, and the
        //      old watch file is intact in every case. Then, with the faults
        //      gone, the next successful write of that watch leaves no `.tmp`
        //      and the complete new content (a stale partial `.tmp` must not
        //      block or corrupt it).
        // proves: CLAIM(S4), CLAIM(DF6)
        let healthy = tempfile::tempdir().unwrap();
        Store::new(healthy.path()).save(&newer(sample("w"))).unwrap();
        let whole = std::fs::read(Store::new(healthy.path()).watch_path(&wid("w"))).unwrap();
        assert!(whole.len() > 2, "precondition: a watch file is longer than two bytes, so a half is a strict non-empty prefix");
        let cases: [(Stage, Vec<u8>); 4] = [
            (Stage::Write, Vec::new()),
            (Stage::ShortWrite, whole[..whole.len() / 2].to_vec()),
            (Stage::Sync, whole.clone()),
            (Stage::Rename, whole.clone()),
        ];
        for (stage, expected_leftover) in cases {
            let dir = tempfile::tempdir().unwrap();
            let before = sample("w");
            Store::new(dir.path()).save(&before).unwrap();
            let bytes_before = std::fs::read(Store::new(dir.path()).watch_path(&wid("w"))).unwrap();
            let (store, plan) = faulted(
                dir.path(),
                vec![rule(WriteKind::Watch, 5, When::Every).at(stage), rule(WriteKind::Watch, 5, When::Every).at(Stage::Cleanup)],
            );

            let error = store.save(&newer(before)).expect_err("the write fails");

            assert!(matches!(error, StoreError::Io(_)), "{stage:?}: the write's own error is reported, got {error:?}");
            assert!(plan.injected(WriteKind::Watch, stage) >= 1, "{stage:?}: the stage fault fired");
            assert_eq!(plan.injected(WriteKind::Watch, Stage::Cleanup), 1, "{stage:?}: the cleanup was attempted once and failed");
            let leftovers = tmp_files(dir.path());
            assert_eq!(leftovers, vec![dir.path().join("watches").join("w.json.tmp")], "{stage:?}: the failed cleanup leaves the .tmp");
            assert_eq!(std::fs::read(&leftovers[0]).unwrap(), expected_leftover, "{stage:?}: what the .tmp held when the stage failed");
            assert_eq!(std::fs::read(store.watch_path(&wid("w"))).unwrap(), bytes_before, "{stage:?}: the old file is still the watch");

            Store::new(dir.path()).save(&newer(sample("w"))).unwrap_or_else(|error| panic!("{stage:?}: the next write must not be blocked by a leftover: {error}"));
            assert!(tmp_files(dir.path()).is_empty(), "{stage:?}: removed at the next successful write");
            assert_eq!(std::fs::read(Store::new(dir.path()).watch_path(&wid("w"))).unwrap(), whole, "{stage:?}: complete new content");
        }
    }

    /// Paths this process holds open under `root` whose name is `.reserve`
    /// (a deleted-but-open file reads back as `<path> (deleted)`).
    fn open_reserve_descriptors(root: &Path) -> Vec<String> {
        let wanted = reserve_path(root).display().to_string();
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
            .map(|target| target.display().to_string())
            .filter(|target| target == &wanted || target == &format!("{wanted} (deleted)"))
            .collect()
    }

    #[test]
    fn the_store_never_keeps_the_reserve_open() {
        // why: DF4c (S3): "vigil never keeps the reserve open". A descriptor
        //      held on the reserve would keep its blocks allocated after
        //      `remove_file`, so the rescue would free nothing on a full disk.
        //      After a creating save, after a rescue that deleted it, and after
        //      the repair that re-made it, this process has no descriptor on
        //      `<root>/.reserve` (read from /proc/self/fd; the root is unique to
        //      this test, so other tests' files cannot show up). Positive
        //      control: a descriptor the test opens itself IS found, and
        //      found as "(deleted)" once the file is unlinked.
        // proves: CLAIM(DF4c)
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.save(&sample("w")).unwrap();
        assert_valid_reserve(dir.path(), "precondition");
        assert_eq!(open_reserve_descriptors(dir.path()), Vec::<String>::new(), "after the save that created the reserve");

        let held = File::open(reserve_path(dir.path())).unwrap();
        assert_eq!(open_reserve_descriptors(dir.path()).len(), 1, "control: the probe sees a descriptor that is open");
        std::fs::remove_file(reserve_path(dir.path())).unwrap();
        assert_eq!(open_reserve_descriptors(dir.path()).len(), 1, "control: and still sees it once unlinked");
        drop(held);
        assert_eq!(open_reserve_descriptors(dir.path()), Vec::<String>::new(), "control: and sees none after it is closed");

        let (full, _) = faulted(dir.path(), vec![rule(WriteKind::Watch, 28, When::Nth(1)), rule(WriteKind::Reserve, 28, When::Every)]);
        store.save(&sample("w")).unwrap();
        assert_valid_reserve(dir.path(), "precondition: the save above re-made the reserve");
        assert!(full.save(&newer(sample("w"))).unwrap().used_reserve, "precondition: the rescue deleted the reserve");
        assert_eq!(open_reserve_descriptors(dir.path()), Vec::<String>::new(), "after a rescue");
        store.save(&newer(sample("w"))).unwrap();
        assert_valid_reserve(dir.path(), "after the repair");
        assert_eq!(open_reserve_descriptors(dir.path()), Vec::<String>::new(), "after the repair");
    }

    #[test]
    fn the_state_a_rescue_leaves_when_it_dies_after_deleting_the_reserve_heals_on_the_next_write() {
        // why: S4: "A crash after deleting the reserve and before the retry
        //      leaves no reserve; the next successful write re-creates it."
        //      A crash cannot be injected deterministically, so this builds the
        //      state it leaves: a rescue whose retry also fails has deleted the
        //      reserve and written nothing (old file intact). A later process
        //      with room must then save and re-create a VALID reserve. Also
        //      shows the reserve is absent right after the failed rescue, so a
        //      "restore it on failure" shortcut (which would defeat the point
        //      on a really full disk) is not what makes this pass.
        // proves: CLAIM(S4), CLAIM(DF4)
        let dir = tempfile::tempdir().unwrap();
        let before = sample("w");
        Store::new(dir.path()).save(&before).unwrap();
        assert_valid_reserve(dir.path(), "precondition");
        let (full, plan) = faulted(dir.path(), vec![rule(WriteKind::Watch, 28, When::Every), rule(WriteKind::Reserve, 28, When::Every)]);
        full.save(&newer(before.clone())).expect_err("the retry fails too");
        assert_eq!(plan.attempts(WriteKind::Watch), 2, "precondition: the reserve was deleted and the retry made");
        assert!(!reserve_path(dir.path()).exists(), "the state after the dead rescue: no reserve");
        assert_eq!(Store::new(dir.path()).load(&wid("w")).unwrap(), before);

        let outcome = Store::new(dir.path()).save(&newer(before.clone())).unwrap();

        assert_eq!(outcome, WriteOutcome { used_reserve: false });
        assert_valid_reserve(dir.path(), "after the next successful write");
        assert_eq!(Store::new(dir.path()).load(&wid("w")).unwrap(), newer(before));
    }
}
