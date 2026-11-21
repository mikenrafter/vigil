//! Notices: one-time announcements delivered once per session.
//!
//! A notice is operator-written text ("Gemini Neon is now a reviewer; reread
//! these skills") with an optional expiry and optional directory scopes. It is
//! stored once, under `<state root>/notices/<id>.json`. What each session name
//! has already seen is stored under `<state root>/notice-delivery/<hash>.json`:
//! a map from notice id to the creation time of the notice that was delivered.
//! A call may see several names for the same session (for example
//! `T3CODE_THREAD_ID`, `CODEX_SESSION_ID`, `CODEX_THREAD_ID` and a hook payload
//! id); a notice is pending only when none of those names records its
//! generation, and deliveries are recorded under every name the call sees under
//! one delivery-directory lock (`notice-delivery/.lock`); each name is written in turn.
//! Reminders and advisories are untouched; neither can express "once per session".
//!
//! Mating parts: `notice_ops` (the CLI/MCP operations and session identity),
//! `keepalive` (envelope delivery, release on a dropped `Delivery`) and the
//! `vigil notices` command in `main.rs` (hook delivery).
//!
//! CLAIM(notice-once): a notice is handed to a session at most once across all
//! names a call sees and any names bridged by overlapping calls, and a failed
//! print or cancellation before commit releases only the generations claimed by
//! that call across those names. Crashes after the durable claim and MCP
//! transport failures after handler return can lose delivery
//! [docs/spec.md, "Notices"]. Claim, release and cleanup serialize on
//! `notice-delivery/.lock`
//! [notice_tests::concurrent_claims_by_one_session_deliver_each_notice_exactly_once,
//! notice_tests::concurrent_overlapping_name_sets_deliver_each_notice_exactly_once,
//! notice_tests::releasing_a_claim_makes_the_notices_pending_again,
//! notice_e2e::a_failed_write_to_stdout_leaves_the_notice_pending,
//! notice_e2e::codex_t3_bridge_both_orders_links_codex_payload_and_t3_names].
//! CLAIM(notice-records): each session name has its own record file, so
//! concurrent sessions cannot overwrite one another's deliveries and a corrupt
//! file affects only calls that see that name
//! [notice_tests::concurrent_sessions_each_get_every_notice_and_none_loses_its_record,
//! notice_tests::corrupt_record_is_isolated_to_calls_seeing_that_name].
//! CLAIM(notice-expiry): `expires_at` is the first instant a notice is no longer
//! shown [notice_tests::an_expired_notice_is_never_shown_and_the_boundary_is_exclusive].
//! CLAIM(notice-generation): the record keys on id and creation time, so a
//! removed and re-added id is a new notice [notice_tests::a_readded_id_is_a_new_notice_for_every_session].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::advisory::AdvisoryId;
use crate::clock::Timestamp;
use crate::reminder::normalized;
use crate::reminder_store::atomic_write;
use crate::store::StoreLock;

pub const MAX_NOTICES: usize = 64;
pub const MAX_MESSAGE_BYTES: usize = 2000;
const MAX_SCOPES: usize = 16;
const MAX_SESSION_BYTES: usize = 512;
/// How long a claim waits for another process holding the directory lock.
/// The critical section is a few small reads and renames, so this bounds a
/// stuck process while leaving headroom for concurrent multi-name sessions.
const LOCK_WAIT: Duration = Duration::from_millis(100);
const RELEASE_LOCK_WAIT: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum NoticeError {
    #[error("invalid notice id {0:?}: must match [A-Za-z0-9_-]{{1,64}}")]
    BadId(String),
    #[error("notice message must be 1..={MAX_MESSAGE_BYTES} bytes after trimming, got {0}")]
    BadMessage(usize),
    #[error("notice already expired: expires_at {0:?} is not after now")]
    AlreadyExpired(Timestamp),
    #[error("notice scope needs 0..={MAX_SCOPES} absolute directory prefixes: {0}")]
    BadScope(String),
    #[error("notice {0:?} already exists; rm it before replacing")]
    Duplicate(String),
    #[error("no notice named {0:?}")]
    Missing(String),
    #[error("at most {MAX_NOTICES} notices")]
    TooMany,
    #[error("invalid session id: {0}")]
    BadSession(String),
    #[error("notice state is locked by another process ({0})")]
    Busy(PathBuf),
    #[error("notice state: {0}")]
    State(String),
}

impl From<std::io::Error> for NoticeError {
    fn from(error: std::io::Error) -> Self {
        NoticeError::State(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, NoticeError>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notice {
    pub id: String,
    pub message: String,
    pub created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope: Vec<PathBuf>,
}

impl Notice {
    /// `expires_at` is the first instant the notice is no longer valid.
    /// ↳ CLAIM(notice-expiry)
    pub fn is_expired(&self, now: Timestamp) -> bool {
        self.expires_at.is_some_and(|expires| now >= expires)
    }

    /// No scope means everywhere; otherwise the normalized cwd must lie under
    /// one scope by path components (`/a/proj-other` is not under `/a/proj`).
    pub fn in_scope(&self, cwd: &Path) -> bool {
        self.scope.is_empty() || {
            let cwd = normalized(cwd);
            self.scope.iter().any(|scope| cwd.starts_with(scope))
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct NoticeRequest {
    pub id: String,
    pub message: String,
    pub expires_at: Option<Timestamp>,
    pub scope: Vec<PathBuf>,
}

/// One validated record key under `<state root>/notice-delivery/`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum RecordKey {
    Agent(String),
    ProcessRun(String),
}

impl RecordKey {
    fn namespaced(&self) -> String {
        match self {
            RecordKey::Agent(id) => format!("agent:{id}"),
            RecordKey::ProcessRun(run) => format!("run:{run}"),
        }
    }

    /// A session id is arbitrary text (a hook payload field), so it only ever
    /// names a file through this hash.
    fn record_file_name(&self) -> String {
        let digest = Sha256::digest(self.namespaced().as_bytes());
        let hex: String = digest.iter().take(16).map(|byte| format!("{byte:02x}")).collect();
        format!("{hex}.json")
    }
}

/// Who a delivery is recorded against.
///
/// An agent session may be visible under several names at once (for example
/// `T3CODE_THREAD_ID`, `CODEX_SESSION_ID`, `CODEX_THREAD_ID` and a hook
/// payload id). The first name is the primary display label; deliveries are
/// checked against the union of all names and recorded under every name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoticeSession {
    /// One or more validated agent session or thread names in precedence order.
    Agent { primary: String, aliases: Vec<String> },
    /// No identity: one run of the calling process, keyed by pid and start time.
    ProcessRun(String),
}

impl NoticeSession {
    /// Validate a single agent session id.
    pub fn single(raw: &str) -> Result<Self> {
        Ok(NoticeSession::Agent { primary: check_session(raw)?, aliases: Vec::new() })
    }

    /// Validate and deduplicate a non-empty list of agent session names,
    /// preserving precedence order. Refuses an empty list or any invalid name.
    pub fn agent<I, S>(names: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut unique = Vec::new();
        for raw in names {
            let checked = check_session(raw.as_ref())?;
            if !unique.contains(&checked) {
                unique.push(checked);
            }
        }
        let mut iter = unique.into_iter();
        let primary = iter.next().ok_or_else(|| NoticeError::BadSession("blank".into()))?;
        Ok(NoticeSession::Agent { primary, aliases: iter.collect() })
    }

    fn record_keys(&self) -> Vec<RecordKey> {
        match self {
            NoticeSession::Agent { primary, aliases } => {
                let mut keys = Vec::with_capacity(1 + aliases.len());
                keys.push(RecordKey::Agent(primary.clone()));
                for alias in aliases {
                    let key = RecordKey::Agent(alias.clone());
                    if !keys.contains(&key) {
                        keys.push(key);
                    }
                }
                keys
            }
            NoticeSession::ProcessRun(run) => vec![RecordKey::ProcessRun(run.clone())],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeliveredNotice {
    pub id: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeIdentity {
    Session,
    ProcessRun,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoticeDelivery {
    pub identity: NoticeIdentity,
    pub session: Option<String>,
    pub notices: Vec<DeliveredNotice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct SessionRecord {
    session: String,
    delivered: BTreeMap<String, Timestamp>,
}

#[cfg(test)]
pub(crate) enum WriteAction {
    Continue,
    Fail(std::io::Error),
}

#[cfg(not(test))]
pub(crate) enum WriteAction {
    Continue,
}

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    match std::fs::File::open(dir).and_then(|file| file.sync_all()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => Ok(()),
        Err(error) => Err(error),
    }
}

/// Notices recorded as delivered but not yet handed to the caller for good.
#[must_use = "keep() the claim once the notices reached the agent, release() it when they did not"]
pub struct NoticeClaim {
    store: NoticeStore,
    session: NoticeSession,
    claimed: Vec<(String, Timestamp)>,
    delivery: NoticeDelivery,
}

impl NoticeClaim {
    pub fn delivery(&self) -> &NoticeDelivery {
        &self.delivery
    }

    /// The notices reached the agent: the record stays as written.
    pub fn keep(self) -> NoticeDelivery {
        self.delivery
    }

    /// The notices did not reach the agent: make them pending again across
    /// every name this claim updated. Only the generations this claim recorded
    /// are removed, so already-delivered notices bridged by this call and any
    /// notice re-added and delivered since stay delivered. Best effort: when
    /// the delivery directory cannot be locked or written the notices stay
    /// recorded as delivered (at most once wins over at least once for a broken
    /// state directory).
    /// ↳ CLAIM(notice-once)
    pub fn release(self) {
        let _ = self.release_internal(|_, _| WriteAction::Continue);
    }

    pub(crate) fn release_internal(&self, before_write: impl FnMut(usize, &Path) -> WriteAction) -> Result<()> {
        if self.claimed.is_empty() {
            return Ok(());
        }
        let _directory_lock = NoticeStore::lock_with_wait(&self.store.delivery_lock_path(), RELEASE_LOCK_WAIT)?;
        let keys = self.session.record_keys();
        let mut writes = Vec::new();
        for key in &keys {
            let mut record = self.store.read_record_key(key)?;
            let before = record.delivered.clone();
            for (id, created_at) in &self.claimed {
                if record.delivered.get(id) == Some(created_at) {
                    record.delivered.remove(id);
                }
            }
            if record.delivered != before {
                record.session = key.namespaced().chars().take(MAX_SESSION_BYTES).collect();
                let bytes = serde_json::to_vec(&record).map_err(|error| NoticeError::State(error.to_string()))?;
                writes.push((key.record_file_name(), bytes));
            }
        }
        self.store.commit_writes_locked(&writes, before_write)
    }
}

#[derive(Clone)]
pub struct NoticeStore {
    /// The vigil state root.
    pub root: PathBuf,
}

impl NoticeStore {
    fn notices_dir(&self) -> PathBuf {
        self.root.join("notices")
    }

    fn delivery_dir(&self) -> PathBuf {
        self.root.join("notice-delivery")
    }

    /// Exclusive lock on `path`, waiting a bounded time. Dropping the guard
    /// unlocks (`StoreLock`, CLAIM(lock-guard-release)).
    fn lock(path: &Path) -> Result<StoreLock> {
        Self::lock_with_wait(path, LOCK_WAIT)
    }

    fn lock_with_wait(path: &Path, wait: Duration) -> Result<StoreLock> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
        let started = std::time::Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(StoreLock { file }),
                Err(std::fs::TryLockError::WouldBlock) if started.elapsed() < wait => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(std::fs::TryLockError::WouldBlock) => return Err(NoticeError::Busy(path.to_path_buf())),
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
    }

    fn notice_path(&self, id: &str) -> PathBuf {
        self.notices_dir().join(format!("{id}.json"))
    }

    pub fn add(&self, now: Timestamp, request: NoticeRequest) -> Result<Notice> {
        AdvisoryId::parse(&request.id).map_err(|_| NoticeError::BadId(request.id.clone()))?;
        let message = request.message.trim().to_string();
        if message.is_empty() || message.len() > MAX_MESSAGE_BYTES {
            return Err(NoticeError::BadMessage(message.len()));
        }
        if let Some(expires) = request.expires_at
            && expires <= now
        {
            return Err(NoticeError::AlreadyExpired(expires));
        }
        if request.scope.len() > MAX_SCOPES {
            return Err(NoticeError::BadScope(format!("{} entries", request.scope.len())));
        }
        if let Some(relative) = request.scope.iter().find(|scope| !scope.is_absolute()) {
            return Err(NoticeError::BadScope(format!("{} is not absolute", relative.display())));
        }
        let notice = Notice {
            id: request.id,
            message,
            created_at: now,
            expires_at: request.expires_at,
            scope: request.scope.iter().map(|scope| normalized(scope)).collect(),
        };
        let _lock = Self::lock(&self.notices_dir().join(".lock"))?;
        let existing = self.list()?;
        if existing.iter().any(|other| other.id == notice.id) || self.notice_path(&notice.id).exists() {
            return Err(NoticeError::Duplicate(notice.id));
        }
        if existing.len() >= MAX_NOTICES {
            return Err(NoticeError::TooMany);
        }
        let bytes = serde_json::to_vec_pretty(&notice).map_err(|error| NoticeError::State(error.to_string()))?;
        atomic_write(&self.notice_path(&notice.id), &bytes)?;
        Ok(notice)
    }

    /// Every stored notice, oldest first, ties by id. A file that does not
    /// parse is skipped: one hand-edited file must not stop every delivery.
    pub fn list(&self) -> Result<Vec<Notice>> {
        let entries = match std::fs::read_dir(self.notices_dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut notices: Vec<Notice> = entries
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .filter_map(|path| std::fs::read(path).ok())
            .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
            .collect();
        notices.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        Ok(notices)
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        AdvisoryId::parse(id).map_err(|_| NoticeError::BadId(id.to_string()))?;
        let _lock = Self::lock(&self.notices_dir().join(".lock"))?;
        match std::fs::remove_file(self.notice_path(id)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(NoticeError::Missing(id.to_string())),
            Err(error) => Err(error.into()),
        }
    }

    fn delivery_lock_path(&self) -> PathBuf {
        self.delivery_dir().join(".lock")
    }

    fn record_path_for_key(&self, key: &RecordKey) -> PathBuf {
        self.delivery_dir().join(key.record_file_name())
    }

    fn read_record_key(&self, key: &RecordKey) -> Result<SessionRecord> {
        let path = self.record_path_for_key(key);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                NoticeError::State(format!(
                    "corrupt delivery record {}: {error}; delete this file to reset; live notices are then shown to this session once more",
                    path.display()
                ))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(SessionRecord::default()),
            Err(error) => Err(error.into()),
        }
    }

    /// Write each name in turn under `notice-delivery/.lock`. Printing only
    /// follows a full commit; an error leaves completed writes in place.
    fn commit_writes_locked(
        &self,
        writes: &[(String, Vec<u8>)],
        mut before_write: impl FnMut(usize, &Path) -> WriteAction,
    ) -> Result<()> {
        for (index, (file, bytes)) in writes.iter().enumerate() {
            let path = self.delivery_dir().join(file);
            match before_write(index, &path) {
                WriteAction::Continue => {}
                #[cfg(test)]
                WriteAction::Fail(error) => return Err(error.into()),
            }
            atomic_write(&path, bytes)?;
        }
        if writes.len() > 1 {
            sync_dir(&self.delivery_dir())?;
        }
        Ok(())
    }

    /// Record every pending notice for `session` as delivered under every name
    /// in `session` and return them. Pending: not expired, inside scope for
    /// `cwd`, and not present in any name's record. Even when nothing is
    /// pending, a call seeing multiple names bridges any already-delivered live
    /// generations across every name in the set so later single-name calls see
    /// them. A notice outside scope is not recorded, so a later call from
    /// inside the scope still gets it.
    /// A notice removed between list() and the record update can still be delivered once.
    /// ↳ CLAIM(notice-once) ↳ CLAIM(notice-records) ↳ CLAIM(notice-expiry)
    pub fn claim(&self, session: &NoticeSession, now: Timestamp, cwd: &Path) -> Result<NoticeClaim> {
        self.claim_internal(session, now, cwd, || {}, |_, _| WriteAction::Continue)
    }

    // The callback lets the race regression pause after the unlocked snapshot.
    pub(crate) fn claim_with_snapshot_hook(
        &self,
        session: &NoticeSession,
        now: Timestamp,
        cwd: &Path,
        after_snapshot: impl FnOnce(),
    ) -> Result<NoticeClaim> {
        self.claim_internal(session, now, cwd, after_snapshot, |_, _| WriteAction::Continue)
    }

    #[cfg(test)]
    pub(crate) fn claim_with_write_hook(
        &self,
        session: &NoticeSession,
        now: Timestamp,
        cwd: &Path,
        before_write: impl FnMut(usize, &Path) -> WriteAction,
    ) -> Result<NoticeClaim> {
        self.claim_internal(session, now, cwd, || {}, before_write)
    }

    fn claim_internal(
        &self,
        session: &NoticeSession,
        now: Timestamp,
        cwd: &Path,
        after_snapshot: impl FnOnce(),
        before_write: impl FnMut(usize, &Path) -> WriteAction,
    ) -> Result<NoticeClaim> {
        let keys = session.record_keys();
        let all = self.list()?;
        let live: BTreeMap<&str, Timestamp> = all.iter().map(|notice| (notice.id.as_str(), notice.created_at)).collect();
        let candidates: Vec<&Notice> = all.iter().filter(|notice| !notice.is_expired(now) && notice.in_scope(cwd)).collect();
        // Fast path: already equal records need no lock or writes.
        let records: Vec<SessionRecord> = keys.iter().map(|key| self.read_record_key(key)).collect::<Result<_>>()?;
        let mut already_delivered: BTreeMap<String, Timestamp> = BTreeMap::new();
        for record in &records {
            for (id, created_at) in &record.delivered {
                if live.get(id.as_str()) == Some(created_at) {
                    already_delivered.insert(id.clone(), *created_at);
                }
            }
        }
        let nothing_to_do = candidates.iter().all(|notice| already_delivered.get(&notice.id) == Some(&notice.created_at))
            && records.iter().all(|record| record.delivered == already_delivered);
        after_snapshot();
        let claimed = if nothing_to_do {
            Vec::new()
        } else {
            let _directory_lock = Self::lock(&self.delivery_lock_path())?;
            let all = self.list()?;
            let live: BTreeMap<&str, Timestamp> = all.iter().map(|notice| (notice.id.as_str(), notice.created_at)).collect();
            let candidates: Vec<&Notice> = all.iter().filter(|notice| !notice.is_expired(now) && notice.in_scope(cwd)).collect();
            let mut records: Vec<SessionRecord> = keys.iter().map(|key| self.read_record_key(key)).collect::<Result<_>>()?;
            // Union of all live delivered generations across every name this call
            // sees. Removed or replaced notices are excluded so the records stay
            // bounded by the live set and a replaced notice is pending again.
            // ↳ CLAIM(notice-generation)
            let mut already_delivered: BTreeMap<String, Timestamp> = BTreeMap::new();
            for record in &records {
                for (id, created_at) in &record.delivered {
                    if live.get(id.as_str()) == Some(created_at) {
                        already_delivered.insert(id.clone(), *created_at);
                    }
                }
            }
            let pending: Vec<Notice> = candidates
                .iter()
                .copied()
                .filter(|notice| !already_delivered.contains_key(&notice.id))
                .cloned()
                .collect();
            let mut target_delivered = already_delivered;
            for notice in &pending {
                target_delivered.insert(notice.id.clone(), notice.created_at);
            }
            let mut writes = Vec::new();
            for (key, record) in keys.iter().zip(records.iter_mut()) {
                if record.delivered != target_delivered {
                    record.session = key.namespaced().chars().take(MAX_SESSION_BYTES).collect();
                    record.delivered = target_delivered.clone();
                    let bytes = serde_json::to_vec(record).map_err(|error| NoticeError::State(error.to_string()))?;
                    writes.push((key.record_file_name(), bytes));
                }
            }
            self.commit_writes_locked(&writes, before_write)?;
            pending
        };
        let (identity, label, note) = match session {
            NoticeSession::Agent { primary, .. } => (NoticeIdentity::Session, Some(primary.clone()), None),
            NoticeSession::ProcessRun(_) => (
                NoticeIdentity::ProcessRun,
                None,
                Some(
                    "no agent session identity: these notices are shown at most once per run of the calling process; \
                     pass --session ID to key them to an agent session"
                        .to_string(),
                ),
            ),
        };
        let delivery = NoticeDelivery {
            identity,
            session: label,
            notices: claimed
                .iter()
                .map(|notice| DeliveredNotice { id: notice.id.clone(), message: notice.message.clone(), expires_at: notice.expires_at })
                .collect(),
            note,
        };
        Ok(NoticeClaim {
            store: self.clone(),
            session: session.clone(),
            claimed: claimed.into_iter().map(|notice| (notice.id, notice.created_at)).collect(),
            delivery,
        })
    }

    /// Maintenance on notice removal or an operator listing expired notices.
    /// Runs under the single delivery-directory lock (which is never unlinked),
    /// retires records whose delivered generations are all dead. Legacy
    /// per-record locks remain for one release to preserve old lock inodes.
    pub(crate) fn prune_stale_records(&self, now: Timestamp) {
        let dir = self.delivery_dir();
        if !dir.exists() {
            return;
        }
        let Ok(_directory_lock) = Self::lock(&self.delivery_lock_path()) else { return };
        let Ok(notices) = self.list() else { return };
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        let paths: Vec<PathBuf> = entries.filter_map(std::result::Result::ok).map(|entry| entry.path()).collect();
        for path in &paths {
            if !path.extension().is_some_and(|ext| ext == "json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(path) else { continue };
            let Ok(record) = serde_json::from_slice::<SessionRecord>(&bytes) else { continue };
            if record.delivered.iter().all(|(id, created_at)| {
                !notices.iter().any(|notice| notice.id == *id && notice.created_at == *created_at && !notice.is_expired(now))
            }) {
                let _ = std::fs::remove_file(path);
            }
        }

    }
}

/// Reject session text that cannot be one id: blank, over-long or with
/// control characters (a hook that found no id must fail loudly, not fall back).
pub fn check_session(raw: &str) -> Result<String> {
    let session = raw.trim();
    if session.is_empty() {
        return Err(NoticeError::BadSession("blank".into()));
    }
    if session.len() > MAX_SESSION_BYTES || session.chars().any(char::is_control) {
        return Err(NoticeError::BadSession("too long or contains control characters".into()));
    }
    Ok(session.to_string())
}
