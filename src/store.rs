//! Watch files under `<root>/watches/`, atomic writes, locks, pruning.

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

/// Exclusive lock on `watches/<id>.lock`; released on drop.
pub struct WatchLock {
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
    pub fn lock_path(&self, id: &WatchId) -> PathBuf {
        self.watches_dir().join(format!("{}.lock", id.as_str()))
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
    /// Block until the exclusive lock is held.
    pub fn lock(&self, id: &WatchId) -> Result<WatchLock, StoreError> {
        todo!()
    }
    /// `Ok(None)` if another holder has it.
    pub fn try_lock(&self, id: &WatchId) -> Result<Option<WatchLock>, StoreError> {
        todo!()
    }
    /// Delete finished watches (and their lock files) whose `finished_at +
    /// 7 days <= now`. Pending watches are never pruned. Returns deleted ids.
    pub fn prune(&self, now: Timestamp) -> Result<Vec<WatchId>, StoreError> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{at, change_request, judge_request, wid};
    use crate::watch::{WatchSpec, WatchStatus};
    use serde_json::Value;

    fn sample(id: &str) -> WatchFile {
        let spec = WatchSpec::parse(judge_request(id, Path::new("/tmp/build.log"), 20)).unwrap();
        WatchFile::new(spec, at("2026-10-29T14:03:11Z"))
    }

    fn finished(id: &str, status: WatchStatus, finished_at: Timestamp) -> WatchFile {
        let spec = WatchSpec::parse(change_request(id, Path::new("/tmp/x"), 20)).unwrap();
        let mut file = WatchFile::new(spec, finished_at.minus_secs(60));
        file.status = status;
        file.finished_at = Some(finished_at);
        file
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
        //      tick, last_tick_at, judge_strikes, events, delivered_through.
        //      Read as raw JSON so a writer/reader that agree on a wrong name
        //      cannot pass.
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
        assert_eq!(raw["delivered_through"], 0);
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
    fn lock_is_exclusive_and_released_on_drop() {
        // why: SPEC "State": a tick holds an exclusive lock on
        //      `watches/<id>.lock`; a second holder must not get it until the first drops.
        //      Positive control: after the drop the lock is available.
        // proves: CLAIM(tick-lock)
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let id = wid("w");
        let first = store.lock(&id).unwrap();
        assert!(store.try_lock(&id).unwrap().is_none(), "second holder must be refused");
        assert!(dir.path().join("watches").join("w.lock").exists(), "lock file lives in watches/");
        drop(first);
        assert!(store.try_lock(&id).unwrap().is_some(), "free after drop");
    }

    #[test]
    fn blocking_lock_waits_for_the_holder() {
        // why: the due-time recheck only works if the second ticker blocks and
        //      then proceeds (SPEC "State"); it must not error out or spin.
        // proves: CLAIM(tick-lock)
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let id = wid("w");
        let guard = store.lock(&id).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let waiter = {
            let (store, id) = (store.clone(), id.clone());
            std::thread::spawn(move || {
                let _held = store.lock(&id).unwrap();
                sender.send(()).unwrap();
            })
        };
        drop(guard);
        receiver.recv_timeout(std::time::Duration::from_secs(5)).expect("waiter never acquired the lock");
        waiter.join().unwrap();
    }

    #[test]
    fn list_returns_watches_in_id_order_and_ignores_lock_and_tmp_files() {
        // why: lock files and crashed `.tmp` files share the directory and
        //      must not be parsed as watches.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.save(&sample("b")).unwrap();
        store.save(&sample("a")).unwrap();
        let _lock = store.lock(&wid("a")).unwrap();
        std::fs::write(dir.path().join("watches").join("c.json.tmp"), "{ partial").unwrap();
        let ids: Vec<String> = store.list().unwrap().iter().map(|w| w.spec.id.to_string()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn prune_deletes_finished_watches_seven_days_after_they_finish() {
        // why: D4 / SPEC "State": finished watches are deleted 7 days after
        //      they finish. Boundaries at +-1 s around 7 d = 604800 s; every
        //      finished status counts; pending watches are never pruned, even
        //      when ancient; the lock file goes with the watch.
        // proves: CLAIM(prune-7d)
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let now = at("2026-10-29T14:03:11Z");
        let seven_days = 7 * 24 * 3600;
        store.save(&finished("fired-old", WatchStatus::Fired, now.minus_secs(seven_days + 1))).unwrap();
        store.save(&finished("expired-old", WatchStatus::Expired, now.minus_secs(seven_days + 1))).unwrap();
        store.save(&finished("cancelled-old", WatchStatus::Cancelled, now.minus_secs(8 * 24 * 3600))).unwrap();
        store.save(&finished("fired-recent", WatchStatus::Fired, now.minus_secs(seven_days - 1))).unwrap();
        let mut ancient_pending = sample("pending-ancient");
        ancient_pending.created_at = now.minus_secs(30 * 24 * 3600);
        store.save(&ancient_pending).unwrap();
        drop(store.lock(&wid("fired-old")).unwrap());

        let mut deleted: Vec<String> = store.prune(now).unwrap().iter().map(|id| id.to_string()).collect();
        deleted.sort();
        assert_eq!(deleted, vec!["cancelled-old", "expired-old", "fired-old"]);
        let mut remaining: Vec<String> = store.list().unwrap().iter().map(|w| w.spec.id.to_string()).collect();
        remaining.sort();
        assert_eq!(remaining, vec!["fired-recent", "pending-ancient"]);
        assert!(!dir.path().join("watches").join("fired-old.lock").exists(), "lock file removed with the watch");
    }
}
