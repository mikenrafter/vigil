//! Disk probe cache shared by advisories and reminders. This module owns cache
//! freshness and best-effort atomic writes; callers own their outcome semantics.
//! CLAIM(probe-cache): reuse requires the same fingerprint, nonnegative age and
//! age strictly below the outcome's TTL [advisory_eval cache tests; reminder_tests].
use std::path::PathBuf;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use crate::clock::Timestamp;
use crate::reminder_store::atomic_write;

#[derive(Serialize, Deserialize)]
pub(crate) struct CacheRecord<T> {
    pub fingerprint: String,
    #[serde(alias = "at")]
    pub fetched_at: Timestamp,
    pub outcome: T,
}

pub(crate) struct ProbeCache {
    pub path: PathBuf,
    pub fingerprint: String,
    pub ttl_s: u64,
    pub failure_ttl_s: u64,
}

impl ProbeCache {
    pub fn read<T: DeserializeOwned>(&self, now: Timestamp, failed: impl FnOnce(&T) -> bool) -> Option<CacheRecord<T>> {
        let record: CacheRecord<T> = serde_json::from_slice(&std::fs::read(&self.path).ok()?).ok()?;
        let ttl = if failed(&record.outcome) { self.failure_ttl_s } else { self.ttl_s };
        let age = now.secs_since(record.fetched_at);
        // ↳ CLAIM(probe-cache)
        (record.fingerprint == self.fingerprint && age >= 0 && (age as u64) < ttl).then_some(record)
    }

    pub fn write<T: Serialize>(&self, outcome: T, at: Timestamp) -> CacheRecord<T> {
        let record = CacheRecord { fingerprint: self.fingerprint.clone(), fetched_at: at, outcome };
        if let Ok(bytes) = serde_json::to_vec(&record) {
            let _ = atomic_write(&self.path, &bytes);
        }
        record
    }
}
