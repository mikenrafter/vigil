//! The advisories file (`advisories.toml`): read on every call, edited by
//! `vigil advisory ...` under a lock, atomically, keeping the operator's
//! comments (SPEC "Advisories: the file").
//!
//! ```toml
//! [advisories.boat-budget]
//! probe = { kind = "cmd", argv = ["boat", "limits", "--json"] }
//! extract = { name = "pct", json = "planIncluded.vmHoursRemaining", of = "planIncluded.vmHours", scale = 100 }
//! condition = "pct <= threshold"
//! ...
//! [advisories.boat-budget.params]
//! threshold = 10
//! ```
//!
//! Invariants: CLAIM(advisory-seed) a file that does not exist is created with
//! the default advisory, once; a file that exists is never given defaults back.
//! CLAIM(advisory-lock) every edit holds `<file>.lock` across its read, check
//! and rename, so concurrent edits do not lose each other. A refused edit
//! leaves the file's bytes untouched.

use std::path::{Path, PathBuf};

use crate::advisory::{Advisory, AdvisoryError, AdvisoryId, AdvisorySpec};

/// One table of the file: a usable advisory, or why it is not (a hand edit
/// that no longer validates must not hide the other advisories).
#[derive(Debug, Clone)]
pub enum Entry {
    Ready(Advisory),
    Broken { id: String, reason: String },
}

impl Entry {
    pub fn id(&self) -> &str {
        match self {
            Entry::Ready(advisory) => advisory.id().as_str(),
            Entry::Broken { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AdvisorySet {
    pub entries: Vec<Entry>,
}

impl AdvisorySet {
    pub fn ids(&self) -> Vec<String> {
        self.entries.iter().map(|entry| entry.id().to_string()).collect()
    }
}

#[derive(Debug, Clone)]
pub struct AdvisoryStore {
    path: PathBuf,
    /// For the default advisory's absolute `boat` path.
    home: Option<String>,
}

impl AdvisoryStore {
    pub fn new(path: PathBuf, home: Option<String>) -> AdvisoryStore {
        AdvisoryStore { path, home }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Read the file now. A missing file yields (and writes, best effort) the
    /// default advisory. A file that is not valid TOML is an error; a table
    /// that is not a valid advisory is a `Broken` entry.
    pub fn load(&self) -> Result<AdvisorySet, AdvisoryError> {
        Ok(AdvisorySet::default())
    }
    /// Add `spec` as `id`. An existing id is an error unless `replace`.
    pub fn add(&self, id: &str, spec: AdvisorySpec, replace: bool) -> Result<Advisory, AdvisoryError> {
        let _ = (id, spec, replace);
        Err(AdvisoryError::Disabled)
    }
    /// Apply `name=value` settings to `id` (see `Advisory::with_settings`).
    pub fn set(&self, id: &str, assignments: &[String]) -> Result<Advisory, AdvisoryError> {
        let _ = (id, assignments);
        Err(AdvisoryError::Disabled)
    }
    pub fn remove(&self, id: &str) -> Result<(), AdvisoryError> {
        let _ = id;
        Err(AdvisoryError::Disabled)
    }
    /// One advisory, which must be usable.
    pub fn get(&self, id: &str) -> Result<Advisory, AdvisoryError> {
        let _ = id;
        Err(AdvisoryError::Disabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisory::default_boat_budget;
    use serde_json::json;

    fn store_in(dir: &tempfile::TempDir) -> AdvisoryStore {
        AdvisoryStore::new(dir.path().join("cfg/advisories.toml"), Some("/home/u".to_string()))
    }

    fn spec(extra: serde_json::Value) -> AdvisorySpec {
        let mut base = json!({
            "probe": {"kind": "cmd", "argv": ["probe-cmd"]},
            "extract": {"name": "free", "regex": "(\\d+) free"},
            "params": {"floor": 20, "ceiling": 90},
            "condition": "free < floor",
            "message_when_true": "Low: {free} (< {floor})",
            "message_when_false": "Plenty: {free}"
        });
        for (key, value) in extra.as_object().unwrap() {
            base[key] = value.clone();
        }
        serde_json::from_value(base).unwrap()
    }

    fn ready<'a>(set: &'a AdvisorySet, id: &str) -> &'a Advisory {
        match set.entries.iter().find(|entry| entry.id() == id) {
            Some(Entry::Ready(advisory)) => advisory,
            other => panic!("{id}: {other:?} in {:?}", set.ids()),
        }
    }

    #[test]
    fn a_missing_file_yields_the_default_and_writes_it_once() {
        // why: owner: "Seed it only if absent". First use on a host has no
        //      file; the default boat-budget must be there and be on disk so
        //      the operator can see and edit it.
        // proves: CLAIM(advisory-seed)
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        assert!(!store.path().exists());
        let set = store.load().unwrap();
        assert_eq!(set.ids(), ["boat-budget"]);
        assert_eq!(ready(&set, "boat-budget").spec(), &default_boat_budget(Some("/home/u")).1);
        let text = std::fs::read_to_string(store.path()).expect("seeded to disk");
        assert!(text.contains("[advisories.boat-budget]") && text.contains("threshold"), "{text}");
        assert_eq!(store.load().unwrap().ids(), ["boat-budget"], "and it loads back");
    }

    #[test]
    fn an_existing_file_is_never_given_the_default_back() {
        // why: "do not overwrite user edits": a file with only another
        //      advisory, an emptied file (after `rm boat-budget`), and an
        //      edited threshold are all the operator's decision. Bytes stay.
        // proves: CLAIM(advisory-seed)
        for content in [
            "# mine\n[advisories.other]\nprobe = { kind = \"cmd\", argv = [\"x\"] }\nextract = { regex = \"(\\\\d+)\" }\ncondition = \"value < 1\"\nmessage_when_true = \"t\"\nmessage_when_false = \"f\"\n",
            "",
            "# nothing here on purpose\n",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let store = store_in(&dir);
            std::fs::create_dir_all(store.path().parent().unwrap()).unwrap();
            std::fs::write(store.path(), content).unwrap();
            let set = store.load().unwrap();
            assert!(!set.ids().contains(&"boat-budget".to_string()), "{content:?} -> {:?}", set.ids());
            assert_eq!(std::fs::read_to_string(store.path()).unwrap(), content);
        }
    }

    #[test]
    fn edits_made_by_hand_are_seen_on_the_next_load_with_no_restart() {
        // why: owner: "re-read on every call so edits take effect with no
        //      restart". Load, change the file on disk, load again.
        // proves: CLAIM(advisory-reread)
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        assert_eq!(ready(&store.load().unwrap(), "boat-budget").spec().params["threshold"], 10.0);
        let text = std::fs::read_to_string(store.path()).unwrap();
        std::fs::write(store.path(), text.replace("threshold = 10", "threshold = 33")).unwrap();
        assert_eq!(ready(&store.load().unwrap(), "boat-budget").spec().params["threshold"], 33.0);
    }

    #[test]
    fn set_changes_the_threshold_on_disk_and_keeps_comments() {
        // why: `vigil advisory set boat-budget threshold=15` is the primary
        //      runtime control. The operator's own comments and unrelated
        //      advisories must survive an edit (they hand-edit this file too).
        // proves: CLAIM(advisory-set), CLAIM(advisory-lock)
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        store.load().unwrap();
        let text = std::fs::read_to_string(store.path()).unwrap();
        std::fs::write(store.path(), format!("# my comment: keep me\n{text}")).unwrap();
        store.add("mem", spec(json!({})), false).unwrap();
        let changed = store.set("boat-budget", &["threshold=15".to_string()]).unwrap();
        assert_eq!(changed.spec().params["threshold"], 15.0);
        let after = std::fs::read_to_string(store.path()).unwrap();
        assert!(after.starts_with("# my comment: keep me\n"), "{after}");
        let set = store.load().unwrap();
        assert_eq!(ready(&set, "boat-budget").spec().params["threshold"], 15.0);
        assert_eq!(ready(&set, "mem").spec(), &spec(json!({})), "the other advisory is untouched");
        assert_eq!(set.ids().len(), 2);
    }

    #[test]
    fn a_refused_edit_leaves_the_file_byte_for_byte() {
        // why: the model is an untrusted caller; a bad set/add must have no
        //      partial effect. Includes a multi-assignment whose LAST item is
        //      bad (the first must not be applied).
        // proves: CLAIM(advisory-set), CLAIM(advisory-valid)
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        store.load().unwrap();
        let before = std::fs::read(store.path()).unwrap();
        let unknown_id = store.set("nope", &["threshold=1".to_string()]).unwrap_err();
        assert!(matches!(&unknown_id, AdvisoryError::Unknown { id, known } if id == "nope" && known.contains("boat-budget")), "{unknown_id}");
        assert!(matches!(store.set("boat-budget", &["threshold=15".into(), "treshold=1".into()]), Err(AdvisoryError::UnknownParam { .. })));
        assert!(store.set("boat-budget", &["threshold=15".into(), "condition=pct <= gone".into()]).is_err());
        assert!(store.set("boat-budget", &["threshold=abc".into()]).is_err());
        assert!(store.add("bad id", spec(json!({})), false).is_err());
        assert!(store.add("worse", spec(json!({"condition": "free < nothing"})), false).is_err());
        assert!(matches!(store.add("boat-budget", spec(json!({})), false), Err(AdvisoryError::Exists(_))));
        assert!(matches!(store.remove("nope"), Err(AdvisoryError::Unknown { .. })));
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
    }

    #[test]
    fn add_replace_and_remove_round_trip_the_spec() {
        // why: parity between the wire spec and the file: what `add` writes,
        //      `load` returns equal (including params order-independence), and
        //      replace/remove do what they say. After `rm`, the default does
        //      not come back (the file exists).
        // proves: CLAIM(advisory-seed), CLAIM(advisory-valid)
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        let original = spec(json!({"cache_s": 90}));
        store.add("mem", original.clone(), false).unwrap();
        assert_eq!(ready(&store.load().unwrap(), "mem").spec(), &original);
        let replaced = spec(json!({"message_when_false": "Different {free}"}));
        store.add("mem", replaced.clone(), true).unwrap();
        assert_eq!(ready(&store.load().unwrap(), "mem").spec(), &replaced);
        store.remove("boat-budget").unwrap();
        store.remove("mem").unwrap();
        assert!(store.load().unwrap().entries.is_empty(), "no reseed once the file exists");
    }

    #[test]
    fn a_broken_table_is_reported_but_does_not_hide_the_others() {
        // why: a hand edit with a typo (`condition = "free <"`) must show up
        //      as that advisory being unavailable with a reason, while
        //      boat-budget keeps working. A file that is not TOML at all is an
        //      error naming the file.
        // proves: CLAIM(advisory-unavailable)
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        store.load().unwrap();
        let text = std::fs::read_to_string(store.path()).unwrap();
        std::fs::write(store.path(), format!("{text}\n[advisories.typo]\nprobe = {{ kind = \"cmd\", argv = [\"x\"] }}\nextract = {{ regex = \"(\\\\d+)\" }}\ncondition = \"value <\"\nmessage_when_true = \"t\"\nmessage_when_false = \"f\"\n")).unwrap();
        let set = store.load().unwrap();
        assert_eq!(set.ids(), ["boat-budget", "typo"]);
        ready(&set, "boat-budget");
        assert!(matches!(set.entries.iter().find(|e| e.id() == "typo"), Some(Entry::Broken { reason, .. }) if reason.contains("condition")));
        assert!(matches!(store.get("typo"), Err(AdvisoryError::Invalid { .. })));
        std::fs::write(store.path(), "this is [not toml").unwrap();
        let error = store.load().unwrap_err();
        assert!(matches!(&error, AdvisoryError::File { path, .. } if path.ends_with("advisories.toml")), "{error}");
        assert!(store.set("boat-budget", &["threshold=1".into()]).is_err());
        assert_eq!(std::fs::read_to_string(store.path()).unwrap(), "this is [not toml", "an unparsable file is never rewritten");
    }

    #[test]
    fn at_most_32_advisories() {
        // why: bounds what an untrusted caller can make vigil probe on every
        //      call. Boundary: 32 allowed, the 33rd refused. (boat-budget is
        //      one of the 32.)
        // proves: CLAIM(advisory-valid)
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        store.load().unwrap();
        for n in 1..32 {
            store.add(&format!("a{n}"), spec(json!({})), false).unwrap();
        }
        assert_eq!(store.load().unwrap().entries.len(), 32);
        assert_eq!(store.add("a32", spec(json!({})), false).unwrap_err(), AdvisoryError::TooMany(32));
        store.add("a1", spec(json!({"cache_s": 5})), true).expect("replacing does not grow the file");
    }

    #[test]
    fn concurrent_edits_do_not_lose_each_other() {
        // why: CLI and MCP sessions edit the same file. Eight threads each set
        //      a different parameter; without the lock across read-modify-write
        //      some updates would be overwritten. All eight must be present.
        // proves: CLAIM(advisory-lock)
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        let params: serde_json::Map<String, serde_json::Value> = (0..8).map(|n| (format!("p{n}"), json!(0))).collect();
        store.add("many", spec(json!({"params": params, "condition": "free < p0"})), false).unwrap();
        std::thread::scope(|scope| {
            for n in 0..8 {
                let store = store.clone();
                scope.spawn(move || {
                    for round in 1..=5 {
                        store.set("many", &[format!("p{n}={}", n * 10 + round)]).unwrap();
                    }
                });
            }
        });
        let set = store.load().unwrap();
        let advisory = ready(&set, "many");
        for n in 0..8 {
            assert_eq!(advisory.spec().params[&format!("p{n}")], (n * 10 + 5) as f64, "p{n}");
        }
    }

    #[test]
    fn no_temporary_file_is_left_behind() {
        // why: the write is tmp + rename; a leftover `.tmp` next to the
        //      operator's file would be clutter and a sign of a failed commit.
        // proves: CLAIM(advisory-lock)
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        store.load().unwrap();
        store.set("boat-budget", &["threshold=12".to_string()]).unwrap();
        let names: Vec<String> = std::fs::read_dir(store.path().parent().unwrap()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(names.iter().all(|name| !name.ends_with(".tmp")), "{names:?}");
        assert!(names.contains(&"advisories.toml".to_string()), "{names:?}");
    }
}
