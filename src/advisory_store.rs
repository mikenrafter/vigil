//! The advisories file (`advisories.toml`): read on every call, edited by
//! `vigil advisory ...` under a lock, atomically, keeping the operator's
//! comments (SPEC "Advisories: the file").
//!
//! ```toml
//! [advisories.boat-budget]
//! probe = { kind = "cmd", argv = ["boat", "limits", "--json"] }
//! extract = { name = "hours", json = "planIncluded.vmHoursRemaining+creditBalanceHours" }
//! condition = "hours <= threshold"
//! ...
//! [advisories.boat-budget.params]
//! threshold = 10
//! ```
//!
//! Invariants: CLAIM(advisory-seed) a file that does not exist is created
//! once: with the default `boat-budget` advisory when `boat` is on PATH at
//! that moment, else with no advisory (a header and a commented example). A
//! file that exists is never given defaults back.
//! CLAIM(advisory-lock) every edit holds `<file>.lock` across its read, check
//! and rename, so concurrent edits do not lose each other. A refused edit
//! leaves the file's bytes untouched.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;
use toml_edit::{DocumentMut, Item, Table, TableLike};

use crate::advisory::{default_boat_budget, Advisory, AdvisoryError, AdvisoryId, AdvisorySpec, MAX_ADVISORIES};

/// One table of the file: a usable advisory, or why it is not (a hand edit
/// that no longer validates must not hide the other advisories).
#[derive(Debug, Clone)]
pub enum Entry {
    Ready(Box<Advisory>),
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
    /// The `PATH` searched for `boat` when the file is seeded.
    search_path: Option<String>,
}

const TABLE: &str = "advisories";

const SEED_HEADER: &str = "# vigil advisories: rules that tell agents what to do, delivered on every keepalive\n\
# envelope and by `vigil advice`. vigil re-reads this file on every call, so edits apply at once.\n\
# Change a number at runtime:  vigil advisory set boat-budget threshold=15\n\n";

/// The seed when `boat` is not on PATH: nothing is active; the example shows
/// the shape of an advisory.
const EMPTY_SEED_HEADER: &str = "# vigil advisories: rules that tell agents what to do, delivered on every keepalive\n\
# envelope and by `vigil advice`. vigil re-reads this file on every call, so edits apply at once.\n\
# Add one with `vigil advisory add ID JSON`, or write a table here by hand.\n\
#\n\
# No advisory is active: `boat` was not on PATH when this file was created.\n\
# An example that warns when a Boat plan runs low (remove the leading `# `):\n\
#\n";

impl AdvisoryStore {
    /// `search_path` is the `PATH` to look for `boat` in when the file is
    /// first seeded.
    pub fn new(path: PathBuf, search_path: Option<String>) -> AdvisoryStore {
        AdvisoryStore { path, search_path }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn file_error(&self, reason: impl std::fmt::Display) -> AdvisoryError {
        AdvisoryError::File { path: self.path.display().to_string(), reason: reason.to_string() }
    }

    /// The text of the file we would seed: the `boat-budget` advisory with
    /// the `boat` found on PATH, or, with no `boat`, no advisory at all.
    fn seed_text(&self) -> String {
        match crate::config::find_on_path("boat", self.search_path.as_deref()) {
            Some(boat) => format!("{SEED_HEADER}{}", boat_budget_doc(&boat.to_string_lossy())),
            None => {
                let example: String = boat_budget_doc("boat").to_string().lines().map(|line| format!("# {line}\n")).collect();
                format!("{EMPTY_SEED_HEADER}{example}")
            }
        }
    }

    /// Read the file now. A missing file yields (and writes, best effort) the
    /// default advisory. A file that is not valid TOML is an error; a table
    /// that is not a valid advisory is a `Broken` entry.
    pub fn load(&self) -> Result<AdvisorySet, AdvisoryError> {
        // CLAIM(advisory-reread) ↳ the file is read here, on every call
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => self.seed()?,
            Err(error) => return Err(self.file_error(error)),
        };
        Ok(AdvisorySet { entries: entries_of(&self.parse(&text)?) })
    }

    /// CLAIM(advisory-seed): under the lock, write the default only if the file
    /// is still absent; whatever is there afterwards is what is returned. A
    /// file that cannot be written (read-only config) still gives the default
    /// in memory: an advisory that cannot be seeded is better than none.
    fn seed(&self) -> Result<String, AdvisoryError> {
        let seed = self.seed_text();
        let seeded = (|| -> std::io::Result<String> {
            let _lock = self.lock()?;
            match std::fs::read_to_string(&self.path) {
                Ok(existing) => Ok(existing),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    self.write_atomically(&seed)?;
                    Ok(seed.clone())
                }
                Err(error) => Err(error),
            }
        })();
        Ok(seeded.unwrap_or(seed))
    }

    fn parse(&self, text: &str) -> Result<DocumentMut, AdvisoryError> {
        text.parse::<DocumentMut>().map_err(|error| self.file_error(error))
    }

    fn lock(&self) -> std::io::Result<std::fs::File> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut name = self.path.file_name().unwrap_or_default().to_os_string();
        name.push(".lock");
        let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(self.path.with_file_name(name))?;
        file.lock()?;
        Ok(file)
    }

    /// tmp + fsync + rename: readers see the old file or the new one.
    fn write_atomically(&self, text: &str) -> std::io::Result<()> {
        let mut name = self.path.file_name().unwrap_or_default().to_os_string();
        name.push(".tmp");
        let tmp = self.path.with_file_name(name);
        let written = (|| {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&tmp, &self.path)
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written
    }

    /// CLAIM(advisory-lock): read, check, change and rename all under one
    /// lock; a refused change writes nothing.
    fn edit<T>(&self, change: impl FnOnce(&mut DocumentMut) -> Result<T, AdvisoryError>) -> Result<T, AdvisoryError> {
        let _lock = self.lock().map_err(|error| self.file_error(error))?;
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => self.seed_text(),
            Err(error) => return Err(self.file_error(error)),
        };
        let mut doc = self.parse(&text)?;
        let outcome = change(&mut doc)?;
        self.write_atomically(&doc.to_string()).map_err(|error| self.file_error(error))?;
        Ok(outcome)
    }

    /// Add `spec` as `id`. An existing id is an error unless `replace`.
    pub fn add(&self, id: &str, spec: AdvisorySpec, replace: bool) -> Result<Advisory, AdvisoryError> {
        let advisory = Advisory::new(AdvisoryId::parse(id)?, spec)?;
        self.edit(|doc| {
            let table = table_mut(doc, &self.path)?;
            let exists = table.contains_key(id);
            if exists && !replace {
                return Err(AdvisoryError::Exists(id.to_string()));
            }
            if !exists && table.len() >= MAX_ADVISORIES {
                return Err(AdvisoryError::TooMany(MAX_ADVISORIES));
            }
            table.insert(id, Item::Table(spec_to_table(advisory.spec())));
            Ok(advisory.clone())
        })
    }

    /// Apply `name=value` settings to `id` (see `Advisory::with_settings`).
    pub fn set(&self, id: &str, assignments: &[String]) -> Result<Advisory, AdvisoryError> {
        self.edit(|doc| {
            let table = table_mut(doc, &self.path)?;
            let current = usable(id, table)?;
            let changed = current.with_settings(assignments)?;
            let item = table.get_mut(id).and_then(Item::as_table_like_mut).expect("usable() found it");
            write_changes(item, current.spec(), changed.spec());
            Ok(changed)
        })
    }

    pub fn remove(&self, id: &str) -> Result<(), AdvisoryError> {
        self.edit(|doc| {
            let table = table_mut(doc, &self.path)?;
            if table.remove(id).is_none() {
                return Err(unknown(id, table));
            }
            Ok(())
        })
    }

    /// One advisory, which must be usable.
    pub fn get(&self, id: &str) -> Result<Advisory, AdvisoryError> {
        let set = self.load()?;
        let ids = set.ids();
        match set.entries.into_iter().find(|entry| entry.id() == id) {
            Some(Entry::Ready(advisory)) => Ok(*advisory),
            Some(Entry::Broken { id, reason }) => Err(AdvisoryError::invalid(&format!("advisory {id}"), reason)),
            None => Err(AdvisoryError::Unknown { id: id.to_string(), known: known_list(&ids) }),
        }
    }
}

/// A document holding only the `boat-budget` advisory that runs `boat`.
fn boat_budget_doc(boat: &str) -> DocumentMut {
    let (id, spec) = default_boat_budget(boat);
    let mut doc = DocumentMut::new();
    let mut advisories = Table::new();
    advisories.set_implicit(true);
    advisories.insert(id.as_str(), Item::Table(spec_to_table(&spec)));
    doc.insert(TABLE, Item::Table(advisories));
    doc
}

fn known_list(ids: &[String]) -> String {
    if ids.is_empty() { "none".to_string() } else { ids.join(", ") }
}

fn unknown(id: &str, table: &Table) -> AdvisoryError {
    let ids: Vec<String> = table.iter().map(|(key, _)| key.to_string()).collect();
    AdvisoryError::Unknown { id: id.to_string(), known: known_list(&ids) }
}

fn table_mut<'a>(doc: &'a mut DocumentMut, path: &Path) -> Result<&'a mut Table, AdvisoryError> {
    if !doc.contains_key(TABLE) {
        let mut advisories = Table::new();
        advisories.set_implicit(true);
        doc.insert(TABLE, Item::Table(advisories));
    }
    doc[TABLE].as_table_mut().ok_or_else(|| AdvisoryError::File { path: path.display().to_string(), reason: format!("`{TABLE}` must be a table, with one [{TABLE}.<id>] table per advisory") })
}

/// The entry `id` as a checked advisory, or the reason it cannot be one.
fn usable(id: &str, table: &Table) -> Result<Advisory, AdvisoryError> {
    let item = table.get(id).ok_or_else(|| unknown(id, table))?;
    match entry_of(id, item) {
        Entry::Ready(advisory) => Ok(*advisory),
        Entry::Broken { id, reason } => Err(AdvisoryError::invalid(&format!("advisory {id}"), reason)),
    }
}

fn entries_of(doc: &DocumentMut) -> Vec<Entry> {
    match doc.get(TABLE).and_then(Item::as_table_like) {
        Some(table) => table.iter().map(|(id, item)| entry_of(id, item)).collect(),
        None => Vec::new(),
    }
}

fn entry_of(id: &str, item: &Item) -> Entry {
    let broken = |reason: String| Entry::Broken { id: id.to_string(), reason };
    let spec: AdvisorySpec = match serde_json::from_value(item_to_json(item)) {
        Ok(spec) => spec,
        Err(error) => return broken(format!("not an advisory: {error}")),
    };
    match AdvisoryId::parse(id).and_then(|parsed| Advisory::new(parsed, spec)) {
        Ok(advisory) => Entry::Ready(Box::new(advisory)),
        Err(error) => broken(error.to_string()),
    }
}

// ---- TOML <-> JSON --------------------------------------------------------

pub(crate) fn item_to_json(item: &Item) -> Value {
    match item {
        Item::None => Value::Null,
        Item::Value(value) => value_to_json(value),
        Item::Table(table) => Value::Object(table.iter().map(|(key, item)| (key.to_string(), item_to_json(item))).collect()),
        Item::ArrayOfTables(tables) => Value::Array(tables.iter().map(|table| Value::Object(table.iter().map(|(key, item)| (key.to_string(), item_to_json(item))).collect())).collect()),
    }
}

fn value_to_json(value: &toml_edit::Value) -> Value {
    use toml_edit::Value as Toml;
    match value {
        Toml::String(text) => Value::String(text.value().clone()),
        Toml::Integer(number) => Value::from(*number.value()),
        // NaN and infinity have no JSON form; a string makes the spec fail to
        // parse with a type error instead of the number vanishing.
        Toml::Float(number) => serde_json::Number::from_f64(*number.value()).map_or_else(|| Value::String(format!("non-finite number {}", number.value())), Value::Number),
        Toml::Boolean(flag) => Value::Bool(*flag.value()),
        Toml::Datetime(stamp) => Value::String(stamp.value().to_string()),
        Toml::Array(items) => Value::Array(items.iter().map(value_to_json).collect()),
        Toml::InlineTable(table) => Value::Object(table.iter().map(|(key, value)| (key.to_string(), value_to_json(value))).collect()),
    }
}

pub(crate) fn json_to_value(value: &Value) -> Option<toml_edit::Value> {
    Some(match value {
        Value::Null => return None,
        Value::Bool(flag) => (*flag).into(),
        Value::Number(number) => match number.as_i64() {
            Some(whole) => whole.into(),
            None => number.as_f64()?.into(),
        },
        Value::String(text) => text.as_str().into(),
        Value::Array(items) => items.iter().filter_map(json_to_value).collect::<toml_edit::Array>().into(),
        Value::Object(map) => {
            let mut table = toml_edit::InlineTable::new();
            for (key, value) in map {
                if let Some(value) = json_to_value(value) {
                    table.insert(key, value);
                }
            }
            table.into()
        }
    })
}

/// `10.0` is written `10`, `7.5` as `7.5`.
fn number_value(number: f64) -> toml_edit::Value {
    if number.fract() == 0.0 && number.abs() < 1e15 { (number as i64).into() } else { number.into() }
}

/// The canonical on-disk form of a spec: scalar fields, `probe` and `extract`
/// as inline tables, `params` as its own table.
fn spec_to_table(spec: &AdvisorySpec) -> Table {
    let json = serde_json::to_value(spec).expect("a spec always serializes");
    let mut table = Table::new();
    for key in ["probe", "extract", "condition", "message_when_true", "message_when_false", "cache_s"] {
        if let Some(value) = json.get(key).and_then(json_to_value) {
            table.insert(key, Item::Value(value));
        }
    }
    if !spec.params.is_empty() {
        let mut params = Table::new();
        for (name, number) in &spec.params {
            params.insert(name, Item::Value(number_value(*number)));
        }
        table.insert("params", Item::Table(params));
    }
    table
}

/// Apply only what differs between `old` and `new` to the entry in place, so
/// comments and layout the operator wrote survive.
fn write_changes(entry: &mut dyn TableLike, old: &AdvisorySpec, new: &AdvisorySpec) {
    for (key, before, after) in [
        ("condition", &old.condition, &new.condition),
        ("message_when_true", &old.message_when_true, &new.message_when_true),
        ("message_when_false", &old.message_when_false, &new.message_when_false),
    ] {
        if before != after {
            replace_value(entry, key, after.as_str().into());
        }
    }
    if old.cache_s != new.cache_s {
        replace_value(entry, "cache_s", (new.cache_s.unwrap_or(crate::advisory::DEFAULT_CACHE_S) as i64).into());
    }
    for (name, number) in &new.params {
        if old.params.get(name) != Some(number)
            && let Some(params) = entry.get_mut("params").and_then(Item::as_table_like_mut)
        {
            replace_value(params, name, number_value(*number));
        }
    }
}

/// Set `key`, keeping the comment and spacing around the value it replaces.
fn replace_value(table: &mut dyn TableLike, key: &str, mut value: toml_edit::Value) {
    // In place, so the key keeps the comment lines above it too.
    if let Some(Item::Value(old)) = table.get_mut(key) {
        *value.decor_mut() = old.decor().clone();
        *old = value;
        return;
    }
    table.insert(key, Item::Value(value));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisory::default_boat_budget;
    use serde_json::json;

    /// A `bin` directory under `dir` holding an executable `boat`; its path.
    fn boat_bin(dir: &tempfile::TempDir) -> PathBuf {
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        crate::testing::write_script(&bin.join("boat"), "exit 0");
        bin
    }

    /// A store whose PATH has `boat` on it (the seed has `boat-budget`).
    fn store_in(dir: &tempfile::TempDir) -> AdvisoryStore {
        let bin = boat_bin(dir);
        AdvisoryStore::new(dir.path().join("cfg/advisories.toml"), Some(format!("/nonexistent:{}", bin.display())))
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
        // why: "Seed it only if absent". First use on a host that has `boat`
        //      on PATH has no file; the default boat-budget must be there,
        //      running the `boat` that was found, and be on disk so the
        //      operator can see and edit it.
        // proves: CLAIM(advisory-seed)
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        assert!(!store.path().exists());
        let set = store.load().unwrap();
        assert_eq!(set.ids(), ["boat-budget"]);
        let boat = dir.path().join("bin/boat");
        assert_eq!(ready(&set, "boat-budget").spec(), &default_boat_budget(&boat.to_string_lossy()).1);
        let text = std::fs::read_to_string(store.path()).expect("seeded to disk");
        assert!(text.contains("[advisories.boat-budget]") && text.contains("threshold"), "{text}");
        assert_eq!(store.load().unwrap().ids(), ["boat-budget"], "and it loads back");
    }

    #[test]
    fn without_boat_on_path_the_seed_has_no_advisory_and_a_commented_example() {
        // why: `boat` is optional. A host without it must not get an advisory
        //      that fails on every call; the seeded file explains why it is
        //      empty and shows the example, which parses once uncommented.
        //      PATH entries that do not exist, or hold no `boat`, count as
        //      absent; so does no PATH at all.
        // proves: CLAIM(advisory-seed), CLAIM(tool-presence)
        for search_path in [None, Some("/nonexistent:/also-not-there".to_string())] {
            let dir = tempfile::tempdir().unwrap();
            let store = AdvisoryStore::new(dir.path().join("cfg/advisories.toml"), search_path.clone());
            let set = store.load().unwrap();
            assert!(set.entries.is_empty(), "{search_path:?}: {:?}", set.ids());
            let text = std::fs::read_to_string(store.path()).expect("seeded to disk");
            assert!(text.contains("`boat` was not on PATH") && text.contains("# [advisories.boat-budget]"), "{text}");
            assert!(text.lines().all(|line| line.is_empty() || line.starts_with('#')), "nothing active: {text}");
            let example: String = text[text.find("# [advisories.").unwrap()..].lines().map(|line| format!("{}\n", line.strip_prefix("# ").unwrap_or(line))).collect();
            std::fs::write(store.path(), &example).unwrap();
            assert_eq!(ready(&store.load().unwrap(), "boat-budget").spec(), &default_boat_budget("boat").1, "the example is the default with a bare `boat`");
        }
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
        assert!(store.set("boat-budget", &["threshold=15".into(), "condition=hours <= gone".into()]).is_err());
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
        store.add("many", spec(json!({"params": params, "condition": "free < p0", "message_when_true": "Low {free}", "message_when_false": "Plenty {free}"})), false).unwrap();
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
