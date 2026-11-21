//! The notice operations the CLI, the hook command and the MCP tools share, so
//! the surfaces cannot drift (parity: `tests/notice_e2e.rs` drives all of them).
//! Session identity lives here: one function decides whose record a delivery
//! goes to.

use std::path::Path;

use serde_json::{Value, json};

use crate::engine::Vigil;
use crate::notice::{Notice, NoticeClaim, NoticeRequest, NoticeSession, NoticeStore, Result, check_session};

/// Field 22 of `/proc/<pid>/stat`: the process start time in clock ticks.
/// Together with the pid it names one process run even when pids are reused.
fn start_time(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = stat.rsplit_once(')')?.1;
    // `tail` begins at field 3 (state); field 22 is index 19.
    tail.split_whitespace().nth(19).map(str::to_string)
}

/// The session id a hook payload carries. Claude Code and Codex send
/// `session_id`; Antigravity sends `conversationId`; `thread_id`, `threadId`
/// and the other spellings cover a thread-keyed harness. The first non-blank
/// string wins; none means the caller falls back to the environment identity.
pub fn hook_session_id(payload: &Value) -> Option<String> {
    ["session_id", "sessionId", "conversationId", "conversation_id", "thread_id", "threadId"]
        .iter()
        .filter_map(|field| payload[*field].as_str())
        .map(str::trim)
        .find(|id| !id.is_empty())
        .map(str::to_string)
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
        let listed = self.notice_store().list()?;
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
        self.notice_store().remove(id)?;
        Ok(json!({"removed": id}))
    }

    /// Whose record a delivery goes to: an explicit id, else this process's
    /// agent identity (`session_key`: Claude session id, Codex, T3 thread, or an
    /// agent ancestor), else one run of the calling process. An explicit id that
    /// is blank is an error, never the fallback.
    pub fn notice_session(&self, explicit: Option<&str>) -> Result<NoticeSession> {
        if let Some(raw) = explicit {
            return Ok(NoticeSession::Agent(check_session(raw)?));
        }
        if let Some(key) = self.session_key.as_deref().filter(|key| !key.trim().is_empty()) {
            return Ok(NoticeSession::Agent(check_session(key)?));
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
