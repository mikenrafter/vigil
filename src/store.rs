//! Watch files under `<root>/watches/`, atomic writes, the one global lock,
//! pruning.

use std::fs::File;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::clock::Timestamp;
use crate::error::StoreError;
use crate::watch::{WatchFile, WatchId};

/// Finished watches are deleted this long after they finish (D4).
pub const PRUNE_AFTER_SECS: u64 = 7 * 24 * 3600;

/// `$XDG_STATE_HOME/vigil`, else `$HOME/.local/state/vigil`. An empty
/// variable counts as unset (XDG Base Directory spec). `None` if neither.
pub fn state_root(xdg_state_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    todo!()
}

/// Serialize `value` to JSON, write `<path>.tmp` beside it, fsync, rename over
/// `path`. On any failure `path` keeps its previous content and no `.tmp`
/// remains. (SPEC "State": writes are atomic.)
pub fn write_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    todo!()
}

/// Exclusive `flock` on `<root>/lock`; released on drop. V4/V5: this is the
/// only lock. Every mutation (create, tick, cancel, prune) holds it for its
/// whole read-modify-write. The file is created on demand and NEVER deleted
/// (deleting it would let a waiter hold the old inode while another process
/// locks a new one).
pub struct StoreLock {
    file: File,
}

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Store {
        Store { root: root.into() }
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
    /// `<root>/lock`, the single global lock file (V5).
    pub fn lock_path(&self) -> PathBuf {
        self.root.join("lock")
    }
    /// Atomic save of one watch (creates `watches/` on demand).
    pub fn save(&self, watch: &WatchFile) -> Result<(), StoreError> {
        todo!()
    }
    pub fn load(&self, id: &WatchId) -> Result<WatchFile, StoreError> {
        todo!()
    }
    /// Every readable watch file, in id order.
    pub fn list(&self) -> Result<Vec<WatchFile>, StoreError> {
        todo!()
    }
    /// Block until the global exclusive lock is held (creates the lock file
    /// and the root on demand).
    pub fn lock(&self) -> Result<StoreLock, StoreError> {
        todo!()
    }
    /// `Ok(None)` if another holder has it.
    pub fn try_lock(&self) -> Result<Option<StoreLock>, StoreError> {
        todo!()
    }
    /// Delete watch files whose finish time + 7 days <= now (D4). The finish
    /// time of a finished watch is `finished_at`; of an overdue pending watch
    /// it is `deadline_at` (V16); a pending watch whose deadline is still in
    /// the future is never deleted. The caller holds the lock (it is not taken
    /// here, so `prune` can run inside another mutation). The global lock file
    /// is never deleted. Returns the deleted ids.
    pub fn prune(&self, now: Timestamp) -> Result<Vec<WatchId>, StoreError> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{at, blocked_flock_waiters, change_request, judge_request, wid};
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
    fn lock_is_one_global_file_in_the_root_exclusive_and_released_on_drop() {
        // why: V4/V5 (S4, S5): ONE lock for everything at `<root>/lock`
        //      (not per-watch files). A second holder must not get it until the
        //      first drops. Positive control: after the drop it is available.
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
    fn the_lock_file_is_never_deleted_by_prune_or_by_dropping_the_lock() {
        // why: V5 (S5): deleting a lock file splits synchronization (a waiter
        //      holds the old inode while another process locks a new one).
        //      Same (dev, inode) after lock/unlock/prune cycles that DO delete
        //      watch files. Positive control: prune really deleted something.
        // proves: CLAIM(lock-never-deleted)
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let now = at("2026-10-29T14:03:11Z");
        store.save(&finished("old", TerminalStatus::Cancelled, now.minus_secs(30 * 24 * 3600))).unwrap();
        drop(store.lock().unwrap());
        let identity = lock_identity(&store.lock_path());
        let guard = store.lock().unwrap();
        let deleted = store.prune(now).unwrap();
        drop(guard);
        assert_eq!(deleted, vec![wid("old")], "control: prune deleted a watch");
        assert!(store.lock_path().exists());
        assert_eq!(lock_identity(&store.lock_path()), identity, "same inode after prune");
        drop(store.lock().unwrap());
        assert_eq!(lock_identity(&store.lock_path()), identity, "same inode after another lock cycle");
    }

    #[test]
    fn no_per_watch_lock_files_are_ever_created() {
        // why: V4/V5: the old `watches/<id>.lock` files are gone; saving and
        //      locking leave only `<id>.json` under watches/ and `lock` in the root.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.save(&sample("a")).unwrap();
        drop(store.lock().unwrap());
        let mut names: Vec<String> = std::fs::read_dir(dir.path().join("watches")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into()).collect();
        names.sort();
        assert_eq!(names, vec!["a.json"]);
        let mut root: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into()).collect();
        root.sort();
        assert_eq!(root, vec!["lock", "watches"]);
    }

    #[test]
    fn list_returns_watches_in_id_order_and_ignores_lock_and_tmp_files() {
        // why: lock files and crashed `.tmp` files share the directory and
        //      must not be parsed as watches.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.save(&sample("b")).unwrap();
        store.save(&sample("a")).unwrap();
        let _lock = store.lock().unwrap();
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
}
