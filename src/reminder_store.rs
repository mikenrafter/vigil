//! Reminder config and suppression state. This module owns all writes.
//! CLAIM(reminder-state): locked read/change/atomic rename prevents lost done,
//! snooze and event claims [reminder_tests::event_claim_is_atomic].
//! No defaults are installed; a missing config is an empty set.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use toml_edit::{DocumentMut, Item, Table};

use crate::advisory_store::{item_to_json, json_to_value};
use crate::clock::Timestamp;
use crate::reminder::{Reminder, ReminderSpec, Result, check_id, invalid};

#[derive(Clone)]
pub struct ReminderStore {
    pub path: PathBuf,
    pub root: PathBuf,
}

#[derive(Debug)]
pub struct Entry {
    pub id: String,
    pub raw: Value,
    pub reminder: Result<Reminder>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReminderState {
    pub done_period: Option<String>,
    pub snooze_until: Option<Timestamp>,
    pub event_period: Option<String>,
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("path needs parent"))?;
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

fn lock(path: &Path) -> Result<crate::store::StoreLock> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    // A reminder must not hold a wait hostage to another editor.
    lock.try_lock()
        .map_err(|e| invalid(format!("{}: {e}", path.display())))?;
    // ↳ CLAIM(lock-guard-release): StoreLock explicitly unlocks before close.
    Ok(crate::store::StoreLock { file: lock })
}

impl ReminderStore {
    fn document(&self) -> Result<DocumentMut> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => text
                .parse()
                .map_err(|e| invalid(format!("{}: {e}", self.path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
            Err(e) => Err(invalid(format!("{}: {e}", self.path.display()))),
        }
    }
    pub fn entries(&self) -> Result<Vec<Entry>> {
        let doc = self.document()?;
        let Some(item) = doc.get("reminders") else {
            return Ok(Vec::new());
        };
        let table = item
            .as_table_like()
            .ok_or_else(|| invalid("reminders must be a table"))?;
        if table.len() > 32 {
            return Err(invalid("at most 32 reminders"));
        }
        Ok(table
            .iter()
            .map(|(id, item)| {
                let raw = item_to_json(item);
                let reminder = serde_json::from_value::<ReminderSpec>(raw.clone())
                    .map_err(|e| invalid(e.to_string()))
                    .and_then(|spec| Reminder::new(id, spec));
                Entry {
                    id: id.into(),
                    raw,
                    reminder,
                }
            })
            .collect())
    }
    pub fn get(&self, id: &str) -> Result<Reminder> {
        check_id(id)?;
        self.entries()?
            .into_iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| invalid(format!("no reminder named {id:?}")))?
            .reminder
    }
    pub fn add(&self, id: &str, raw: Value) -> Result<Value> {
        check_id(id)?;
        let spec =
            serde_json::from_value(raw).map_err(|e| invalid(format!("invalid spec: {e}")))?;
        let reminder = Reminder::new(id, spec)?;
        let _lock = lock(&self.path.with_extension("toml.lock"))?;
        let mut doc = self.document()?;
        if doc.get("reminders").is_none() {
            doc["reminders"] = Item::Table(Table::new());
        }
        let table = doc["reminders"]
            .as_table_like_mut()
            .ok_or_else(|| invalid("reminders must be a table"))?;
        if table.contains_key(id) {
            return Err(invalid(format!(
                "reminder {id:?} already exists; rm it before replacing"
            )));
        }
        if table.len() >= 32 {
            return Err(invalid("at most 32 reminders"));
        }
        let raw = serde_json::to_value(&reminder.spec).map_err(|e| invalid(e.to_string()))?;
        let mut spec = Table::new();
        for (key, value) in raw.as_object().expect("spec is object") {
            if let Some(value) = json_to_value(value) {
                spec.insert(key, Item::Value(value));
            }
        }
        // Remove/re-add starts a fresh lifecycle; a failed config write leaves
        // only an inactive state reset, never a partially written config.
        self.update_state(id, |state| {
            *state = ReminderState::default();
            Ok(())
        })?;
        table.insert(id, Item::Table(spec));
        atomic_write(&self.path, doc.to_string().as_bytes())?;
        Ok(json!({"id": id, "spec": raw}))
    }
    pub fn remove(&self, id: &str) -> Result<Value> {
        check_id(id)?;
        let _lock = lock(&self.path.with_extension("toml.lock"))?;
        let mut doc = self.document()?;
        let table = doc
            .get_mut("reminders")
            .and_then(Item::as_table_like_mut)
            .ok_or_else(|| invalid(format!("no reminder named {id:?}")))?;
        if table.remove(id).is_none() {
            return Err(invalid(format!("no reminder named {id:?}")));
        }
        atomic_write(&self.path, doc.to_string().as_bytes())?;
        Ok(json!({"removed": id}))
    }
    pub fn state(&self, id: &str) -> Result<ReminderState> {
        check_id(id)?;
        let path = self.root.join("reminders").join(format!("{id}.json"));
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| invalid(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ReminderState::default()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn update_state<T>(
        &self,
        id: &str,
        update: impl FnOnce(&mut ReminderState) -> Result<T>,
    ) -> Result<T> {
        check_id(id)?;
        let path = self.root.join("reminders").join(format!("{id}.json"));
        let _lock = lock(&path.with_extension("lock"))?;
        let mut state = self.state(id)?;
        let answer = update(&mut state)?;
        atomic_write(
            &path,
            &serde_json::to_vec(&state).map_err(|e| invalid(e.to_string()))?,
        )?;
        Ok(answer)
    }
    /// A global per-reminder, per-period claim. Lists remain due until done.
    pub fn claim_event(&self, id: &str, period: &str, now: Timestamp) -> Result<bool> {
        self.update_state(id, |state| {
            if state.event_period.as_deref() == Some(period)
                || state.done_period.as_deref() == Some(period)
                || state.snooze_until.is_some_and(|until| now < until)
            {
                return Ok(false);
            }
            state.event_period = Some(period.into());
            Ok(true)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_reminder_lock_unlocks_while_a_duplicate_descriptor_is_open() {
        // WHY: CLAIM(lock-guard-release): a fork/dup must not extend a completed
        // reminder edit's flock. Close-only guards leave the next edit refused.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reminders.lock");
        let guard = lock(&path).unwrap();
        let inherited = guard.file.try_clone().unwrap();
        drop(guard);
        let next = lock(&path);
        assert!(next.is_ok(), "dropped reminder guard still locked: {:?}", next.err());
        drop(inherited);
    }
}
