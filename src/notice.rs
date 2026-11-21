//! Notices: one-time announcements every agent session sees exactly once.
//!
//! A notice is operator-written text ("Gemini Neon is now a reviewer; reread
//! these skills") with an optional expiry and optional directory scopes. It is
//! stored once, under `<state root>/notices/<id>.json`. What each session has
//! already seen is stored per session, under
//! `<state root>/notice-delivery/<hash of session>.json`: a map from notice id
//! to the creation time of the notice that was delivered. Reminders and
//! advisories are untouched; neither can express "once per session".
//!
//! Mating parts: `notice_ops` (the CLI/MCP operations and session identity),
//! `keepalive` (envelope delivery, release on a dropped `Delivery`) and the
//! `vigil notices` command in `main.rs` (hook delivery).
//!
//! CLAIM(notice-once): a notice is handed to a session at most once, and a
//! failed print or cancellation before commit releases the claim for retry.
//! Crashes and MCP transport failures after handler return can lose delivery
//! [docs/spec.md, "Notices"]. Claim and release use the session record's lock
//! [notice_tests::concurrent_claims_by_one_session_deliver_each_notice_exactly_once,
//! notice_tests::releasing_a_claim_makes_the_notices_pending_again,
//! notice_e2e::a_failed_write_to_stdout_leaves_the_notice_pending].
//! CLAIM(notice-records): each session has its own record file, so concurrent
//! sessions cannot overwrite one another's deliveries
//! [notice_tests::concurrent_sessions_each_get_every_notice_and_none_loses_its_record].
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
/// Minimum age for pruning records whose delivered notices are all no longer live.
const RECORD_RETENTION: Duration = Duration::from_secs(30 * 86_400);
/// How long a claim waits for another process holding the same record's lock.
/// The critical section is one small read and one rename, so this is a bound
/// for a stuck process, not a typical wait.
const LOCK_WAIT: Duration = Duration::from_millis(25);

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

/// Who a delivery is recorded against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoticeSession {
    /// An agent session or thread id (explicit, from the environment, or ancestry).
    Agent(String),
    /// No identity: one run of the calling process, keyed by pid and start time.
    ProcessRun(String),
}

impl NoticeSession {
    fn namespaced(&self) -> String {
        match self {
            NoticeSession::Agent(id) => format!("agent:{id}"),
            NoticeSession::ProcessRun(run) => format!("run:{run}"),
        }
    }
    /// A session id is arbitrary text (a hook payload field), so it only ever
    /// names a file through this hash.
    fn record_name(&self) -> String {
        let digest = Sha256::digest(self.namespaced().as_bytes());
        digest.iter().take(16).map(|byte| format!("{byte:02x}")).collect()
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

    /// The notices did not reach the agent: make them pending again. Only the
    /// generation this claim recorded is removed, so a notice re-added and
    /// delivered since stays delivered. Best effort: when the record cannot be
    /// locked or written the notices stay recorded as delivered (at most once
    /// wins over at least once for a broken state directory).
    /// ↳ CLAIM(notice-once)
    pub fn release(self) {
        if self.claimed.is_empty() {
            return;
        }
        let _ = self.store.update_record(&self.session, |record| {
            for (id, created_at) in &self.claimed {
                if record.delivered.get(id) == Some(created_at) {
                    record.delivered.remove(id);
                }
            }
        });
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
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
        let started = std::time::Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(StoreLock { file }),
                Err(std::fs::TryLockError::WouldBlock) if started.elapsed() < LOCK_WAIT => {
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

    fn record_path(&self, session: &NoticeSession) -> PathBuf {
        self.delivery_dir().join(format!("{}.json", session.record_name()))
    }

    fn read_record(&self, session: &NoticeSession) -> Result<SessionRecord> {
        match std::fs::read(self.record_path(session)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| NoticeError::State(format!("corrupt delivery record {}: {error}", self.record_path(session).display()))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(SessionRecord::default()),
            Err(error) => Err(error.into()),
        }
    }

    /// Read-modify-write one session's record under its lock; the file is
    /// written (atomically) only when the update changed it.
    fn update_record<T>(&self, session: &NoticeSession, update: impl FnOnce(&mut SessionRecord) -> T) -> Result<T> {
        let path = self.record_path(session);
        let _lock = Self::lock(&path.with_extension("lock"))?;
        let mut record = self.read_record(session)?;
        let before = record.delivered.clone();
        record.session = session.namespaced().chars().take(MAX_SESSION_BYTES).collect();
        let answer = update(&mut record);
        if record.delivered != before || !path.exists() {
            let bytes = serde_json::to_vec(&record).map_err(|error| NoticeError::State(error.to_string()))?;
            atomic_write(&path, &bytes)?;
        }
        Ok(answer)
    }

    /// Record every pending notice for `session` as delivered and return them.
    /// Pending: not expired, inside scope for `cwd`, and not yet in the
    /// session's record. A notice outside scope is not recorded, so a later
    /// call from inside the scope still gets it.
    /// ↳ CLAIM(notice-once) ↳ CLAIM(notice-records) ↳ CLAIM(notice-expiry)
    pub fn claim(&self, session: &NoticeSession, now: Timestamp, cwd: &Path) -> Result<NoticeClaim> {
        let all = self.list()?;
        let live: BTreeMap<&str, Timestamp> = all.iter().map(|notice| (notice.id.as_str(), notice.created_at)).collect();
        let candidates: Vec<&Notice> = all.iter().filter(|notice| !notice.is_expired(now) && notice.in_scope(cwd)).collect();
        // Fast path, no lock and no write: a keepalive in a session with
        // nothing pending (the usual case) must leave the state root as it was.
        // A stale read can only miss a notice released a moment ago; the next
        // call delivers it.
        let seen = self.read_record(session)?;
        let nothing_to_do = candidates.iter().all(|notice| seen.delivered.get(&notice.id) == Some(&notice.created_at))
            && seen.delivered.iter().all(|(id, created_at)| live.get(id.as_str()) == Some(created_at));
        let claimed = if nothing_to_do {
            Vec::new()
        } else {
            self.update_record(session, |record| {
                // Forget notices that were removed or replaced (same id, other
                // creation time): the record stays bounded by the live set, and
                // a replaced notice is pending again.
                // ↳ CLAIM(notice-generation)
                record.delivered.retain(|id, created_at| live.get(id.as_str()) == Some(created_at));
                let pending: Vec<&Notice> = candidates.iter().copied().filter(|notice| !record.delivered.contains_key(&notice.id)).collect();
                for notice in &pending {
                    record.delivered.insert(notice.id.clone(), notice.created_at);
                }
                pending.into_iter().cloned().collect::<Vec<Notice>>()
            })?
        };
        if !claimed.is_empty() {
            self.prune_stale_records(now);
        }
        let (identity, label, note) = match session {
            NoticeSession::Agent(id) => (NoticeIdentity::Session, Some(id.clone()), None),
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

    /// Best-effort removal of old records only after every delivered generation
    /// is removed or expired. Lock files stay in place so their inode is stable.
    fn prune_stale_records(&self, now: Timestamp) {
        let Ok(entries) = std::fs::read_dir(self.delivery_dir()) else { return };
        for entry in entries.filter_map(std::result::Result::ok) {
            let path = entry.path();
            if !path.extension().is_some_and(|ext| ext == "json") {
                continue;
            }
            let stale = entry.metadata().and_then(|meta| meta.modified()).ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > RECORD_RETENTION);
            if !stale {
                continue;
            }
            let Ok(_lock) = Self::lock(&path.with_extension("lock")) else { continue };
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let Ok(record) = serde_json::from_slice::<SessionRecord>(&bytes) else { continue };
            // Read the current live set while holding the record lock: a
            // concurrent delivery must not be pruned using an older snapshot.
            let Ok(notices) = self.list() else { continue };
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
