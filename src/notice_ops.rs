//! The notice operations the CLI, the hook command and the MCP tools share, so
//! the surfaces cannot drift (parity: `tests/notice_e2e.rs` drives all of them).
//! Session identity lives here: one function collects every name a call sees.

use std::path::Path;

use serde_json::{Value, json};

use crate::engine::Vigil;
use crate::notice::{Notice, NoticeClaim, NoticeRequest, NoticeSession, NoticeStore, Result};

/// Notice-only session environment variables, in primary-label precedence order.
/// Every present non-blank variable is recorded in the session's name set.
pub const NOTICE_SESSION_ENV_VARS: &[&str] = &[
    "T3CODE_THREAD_ID",
    "CLAUDE_CODE_SESSION_ID",
    "CODEX_SESSION_ID",
    "CODEX_THREAD_ID",
    "ANTIGRAVITY_CONVERSATION_ID",
];

const HOOK_PAYLOAD_FIELDS: &[&str] = &[
    "session_id",
    "sessionId",
    "conversationId",
    "conversation_id",
    "thread_id",
    "threadId",
];

/// Field 22 of `/proc/<pid>/stat`: the process start time in clock ticks.
/// Together with the pid it names one process run even when pids are reused.
fn start_time(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = stat.rsplit_once(')')?.1;
    // `tail` begins at field 3 (state); field 22 is index 19.
    tail.split_whitespace().nth(19).map(str::to_string)
}

/// Every distinct non-blank session id a hook payload carries. Claude Code and
/// Codex send `session_id`; Antigravity sends `conversationId`; `thread_id`,
/// `threadId` and the other spellings cover thread-keyed harnesses.
pub fn hook_session_ids(payload: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    for field in HOOK_PAYLOAD_FIELDS {
        if let Some(id) = payload[*field].as_str().map(str::trim).filter(|id| !id.is_empty())
            && !ids.iter().any(|seen| seen == id)
        {
            ids.push(id.to_string());
        }
    }
    ids
}

/// The first non-blank session id a hook payload carries.
pub fn hook_session_id(payload: &Value) -> Option<String> {
    hook_session_ids(payload).into_iter().next()
}

/// Collect every notice session name a call sees: all present identity
/// environment variables plus any hook payload ids, deduplicated in order.
/// Agent-process ancestry is used last and only when neither environment nor
/// payload provides a name. Peers and holds retain their own identity order.
pub fn session_keys_from_lookup<S: AsRef<str>>(
    lookup: impl FnMut(&str) -> Option<String>,
    payload_ids: &[S],
) -> Vec<String> {
    session_keys_with_ancestor(lookup, payload_ids, || crate::peers::session_key_from_lookup(|_| None))
}

pub(crate) fn session_keys_with_ancestor<S: AsRef<str>>(
    mut lookup: impl FnMut(&str) -> Option<String>,
    payload_ids: &[S],
    ancestor: impl FnOnce() -> Option<String>,
) -> Vec<String> {
    let mut names = Vec::new();
    for env_name in NOTICE_SESSION_ENV_VARS {
        if let Some(raw) = lookup(env_name) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() && !names.iter().any(|seen| seen == trimmed) {
                names.push(trimmed.to_string());
            }
        }
    }
    for raw in payload_ids {
        let trimmed = raw.as_ref().trim();
        if !trimmed.is_empty() && !names.iter().any(|seen| seen == trimmed) {
            names.push(trimmed.to_string());
        }
    }
    if names.is_empty()
        && let Some(raw) = ancestor()
    {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            names.push(trimmed.to_string());
        }
    }
    names
}

impl Vigil {
    pub fn notice_store(&self) -> NoticeStore {
        NoticeStore { root: self.store.root().into() }
    }

    pub fn notice_add(&self, request: NoticeRequest) -> Result<Notice> {
        self.notice_store().add(self.clock.now(), request)
    }

    /// Every notice with an `expired` flag, oldest first. Expired notices stay
    /// listed (and removable) but are never delivered.
    pub fn notice_list(&self) -> Result<Value> {
        let now = self.clock.now();
        let store = self.notice_store();
        let listed = store.list()?;
        if listed.iter().any(|notice| notice.is_expired(now)) {
            store.prune_stale_records(now);
        }
        Ok(Value::Array(
            listed
                .into_iter()
                .map(|notice| {
                    let expired = notice.is_expired(now);
                    let mut value = serde_json::to_value(&notice).expect("notice serializes");
                    value["expired"] = json!(expired);
                    value
                })
                .collect(),
        ))
    }

    pub fn notice_rm(&self, id: &str) -> Result<Value> {
        let store = self.notice_store();
        store.remove(id)?;
        store.prune_stale_records(self.clock.now());
        Ok(json!({"removed": id}))
    }

    /// Whose records a delivery checks and updates: an explicit id alone, else
    /// every notice session name this call sees (all identity env vars, plus
    /// hook payload ids; ancestry only when none exist), else one run of the
    /// calling process. A blank explicit id is an error, never the fallback.
    pub fn notice_session(&self, explicit: Option<&str>) -> Result<NoticeSession> {
        if let Some(raw) = explicit {
            return NoticeSession::single(raw);
        }
        let keys: Vec<&str> = self
            .notice_session_keys
            .iter()
            .map(String::as_str)
            .filter(|key| !key.trim().is_empty())
            .collect();
        if !keys.is_empty() {
            return NoticeSession::agent(keys);
        }
        if let Some(key) = self.session_key.as_deref().filter(|key| !key.trim().is_empty()) {
            return NoticeSession::single(key);
        }
        // The MCP server is one long process; a CLI call is short, so its
        // parent (the shell, hook runner or agent that keeps calling it) is the run.
        let pid = match self.transport {
            crate::peers::CallTransport::Mcp => std::process::id(),
            crate::peers::CallTransport::Cli => crate::peers::proc_stat(Path::new("/proc"), std::process::id()).map_or(0, |(_, _, parent)| parent),
        };
        Ok(NoticeSession::ProcessRun(format!("{pid}:{}", start_time(pid).unwrap_or_else(|| "unknown".into()))))
    }

    /// Record every pending notice for the session as delivered and return the
    /// claim. The caller prints, then `keep()`s; when printing fails it
    /// `release()`s. `cwd` defaults to this process's directory.
    pub fn claim_notices(&self, explicit_session: Option<&str>, cwd: Option<&Path>) -> Result<NoticeClaim> {
        let session = self.notice_session(explicit_session)?;
        self.notice_store().claim(&session, self.clock.now(), cwd.unwrap_or(&self.cwd))
    }
}
