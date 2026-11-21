//! Same-session calls (SPEC "Orchestrator peers"). The registry describes
//! blocking calls, not watches between calls. It is operator-owned state, like
//! watch files; session identity is attribution, not an authentication boundary.
//!
//! CLAIM(peers-registry): when registration succeeds, a unique atomic record
//! lives as long as the blocking future, including its error and cancellation
//! paths [peers_tests].
//! CLAIM(peers-session): only equal, nonempty session keys see peers; the binary
//! supplies the environment/ancestry identity, tests supply a key [peers_tests].
//! CLAIM(peers-own): the current pid is excluded; confirmed calls on other
//! watches remain informational [SPEC Orchestrator peers; peers_tests].
//! CLAIM(peers-stale): dead/zombie pids are ignored and pruned [peers_tests].
//! CLAIM(peers-once): a create-new receipt claims a foreign record once across
//! concurrent readers and successive CLI processes [peers_tests].

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::clock::Timestamp;
use crate::engine::Vigil;
use crate::envelope::Peer;
use crate::error::{StoreError, VigilError};
use crate::watch::{Event, EventDetail, FireRule, WatchId};

static NEXT_CALL: AtomicU64 = AtomicU64::new(0);
const INPUT_SUMMARY_BYTES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerKind {
    Hold,
    Watch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerMode {
    Plain,
    Orchestrator,
}

/// The process behind a call's pid: a one-shot CLI or the shared MCP server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallTransport {
    #[default]
    Cli,
    Mcp,
}

impl CallTransport {
    fn as_str(self) -> &'static str {
        match self { Self::Cli => "cli", Self::Mcp => "mcp" }
    }
}

#[derive(Serialize, Deserialize)]
struct CallWatch {
    watch_id: WatchId,
    kind: PeerKind,
    expect_s: Option<u64>,
    inputs: String,
}

#[derive(Serialize, Deserialize)]
struct CallFile {
    pid: u32,
    #[serde(default)]
    transport: CallTransport,
    session: Option<String>,
    orchestrator: bool,
    started_at: Timestamp,
    watch_ids: Vec<WatchId>,
    watches: Vec<CallWatch>,
    max_s: u64,
}

pub(crate) struct CallGuard(PathBuf);
impl Drop for CallGuard {
    fn drop(&mut self) {
        remove_call(&self.0);
    }
}

fn remove_call(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("raised"));
}

#[derive(Default)]
pub(crate) struct PeerScan {
    pub peers: Vec<Peer>,
    pub events: Vec<Event>,
}

impl Vigil {
    /// Best effort: the blocking caller ignores registration failures. No await
    /// separates successful publication and guard ownership.
    pub(crate) fn register_call(&self, covered: &[WatchId], orchestrator: bool, max_s: u64) -> Result<CallGuard, VigilError> {
        let watches = covered.iter().map(|id| {
            let watch = self.store.load(id)?;
            let inputs = watch.spec.inputs.iter().map(|input| input.label()).collect::<Vec<_>>().join("; ");
            Ok(CallWatch {
                watch_id: id.clone(),
                kind: if matches!(watch.spec.fire, FireRule::Hold) { PeerKind::Hold } else { PeerKind::Watch },
                expect_s: watch.spec.expect_s,
                inputs: short_inputs(inputs),
            })
        }).collect::<Result<Vec<_>, VigilError>>()?;
        let record = CallFile {
            pid: std::process::id(), transport: self.transport, session: self.session_key.clone(), orchestrator,
            started_at: self.clock.now(), watch_ids: covered.to_vec(), watches, max_s,
        };
        let dir = self.store.root().join("calls");
        std::fs::create_dir_all(&dir).map_err(StoreError::io)?;
        let path = dir.join(format!("{}-{}.json", record.pid, NEXT_CALL.fetch_add(1, Ordering::Relaxed)));
        // ↳ CLAIM(peers-registry)
        crate::store::write_atomic(&path, &record)?;
        Ok(CallGuard(path))
    }

    /// Called only from an orchestrator sense pass. Invalid/in-flight files
    /// are skipped; only a known-dead pid permits removal. Read errors leave
    /// another process's files alone. Registry and receipt failures are skipped.
    pub(crate) fn scan_peers(&self, covered: &[WatchId]) -> PeerScan {
        let mut scan = PeerScan::default();
        // ↳ CLAIM(peers-session)
        let Some(session) = self.session_key.as_deref().filter(|key| !key.is_empty()) else { return scan };
        let dir = self.store.root().join("calls");
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return scan,
            Err(_) => return scan,
        };
        let mut paths: Vec<_> = entries.filter_map(Result::ok).map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json")).collect();
        paths.sort();
        for path in paths {
            let Some(record) = std::fs::read(&path).ok().and_then(|bytes| serde_json::from_slice::<CallFile>(&bytes).ok()) else { continue };
            // ↳ CLAIM(peers-stale)
            if pid_dead(record.pid) {
                remove_call(&path);
                continue;
            }
            // ↳ CLAIM(peers-own), CLAIM(peers-session)
            if record.session.as_deref() != Some(session) || record.pid == std::process::id() {
                continue;
            }
            let confirmed = !record.watch_ids.is_empty() && record.watch_ids.iter().all(|id| {
                self.store.load(id).ok().is_some_and(|watch| self.orchestrator_confirmed(&watch))
            });
            if record.orchestrator && !confirmed {
                let Some(primary) = covered.first().and_then(|id| self.load_watch(id).ok()) else { continue };
                // ↳ CLAIM(peers-once): no await between claiming and building
                // the event. Like credit events, this is call-local attention.
                match OpenOptions::new().write(true).create_new(true).open(path.with_extension("raised")) {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(_) => continue,
                }
                // A caller may have returned between our read and claim.
                // Its guard cannot remove a receipt created AFTER it dropped.
                if !path.exists() {
                    let _ = std::fs::remove_file(path.with_extension("raised"));
                    continue;
                }
                let names = record.watch_ids.iter().map(WatchId::as_str).collect::<Vec<_>>().join(", ");
                scan.events.push(Event {
                    event_id: format!("{}:peer:{}", primary.spec.id, path.file_stem().unwrap_or_default().to_string_lossy()),
                    tick: primary.tick, watch_id: primary.spec.id.as_str().to_string(), input: "peers".into(),
                    summary: format!("foreign orchestrator: {names}; transport {}; pid {}; started_at {}{}", record.transport.as_str(), record.pid, record.started_at.0.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                        if record.transport == CallTransport::Mcp { "; this pid is the session's vigil MCP server; stop the agent, not the pid" } else { "" }),
                    judge: None, redelivered: false, detail: Some(EventDetail::ForeignOrchestrator),
                });
            } else {
                let mode = if record.orchestrator { PeerMode::Orchestrator } else { PeerMode::Plain };
                scan.peers.extend(record.watches.into_iter()
                    .filter(|watch| !confirmed || !covered.contains(&watch.watch_id))
                    .map(|watch| Peer {
                    mode,
                    watch_id: watch.watch_id, kind: watch.kind, expect_s: watch.expect_s,
                    waited_s: self.clock.now().secs_since(record.started_at).max(0) as u64,
                    inputs: watch.inputs,
                }));
            }
        }
        scan
    }
}

fn short_inputs(mut summary: String) -> String {
    if summary.len() > INPUT_SUMMARY_BYTES {
        let mut end = INPUT_SUMMARY_BYTES - 3;
        while !summary.is_char_boundary(end) { end -= 1; }
        summary.truncate(end);
        summary.push_str("...");
    }
    summary
}

/// Read only names and parent pids from procfs, never another process's env.
pub(crate) fn proc_stat(root: &Path, pid: u32) -> std::io::Result<(String, char, u32)> {
    let stat = std::fs::read_to_string(root.join(pid.to_string()).join("stat"))?;
    let (head, tail) = stat.rsplit_once(')').ok_or_else(|| std::io::Error::other("invalid proc stat"))?;
    let name = head.split_once('(').ok_or_else(|| std::io::Error::other("invalid proc name"))?.1.to_string();
    let mut fields = tail.split_whitespace();
    let state = fields.next().and_then(|field| field.chars().next()).ok_or_else(|| std::io::Error::other("missing process state"))?;
    let parent = fields.next().and_then(|field| field.parse().ok()).ok_or_else(|| std::io::Error::other("missing parent pid"))?;
    Ok((name, state, parent))
}

fn pid_dead(pid: u32) -> bool {
    match proc_stat(Path::new("/proc"), pid) {
        Ok((_, state, _)) => matches!(state, 'Z' | 'X'),
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

/// Agent CLIs vigil recognises by process `comm` name (at most 15 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KnownAgent {
    Claude,
    Codex,
    Antigravity,
}

/// Process names (`comm`, at most 15 bytes) of every agent CLI vigil
/// recognises in a process tree. Shared by [`antigravity_in_ancestry`] and
/// [`ancestor_session`] so the two walks cannot drift. `agy acp` (T3) and
/// `agy --print` show `agy` as the parent of shell children
/// [report-neon-ttl.md]; the same name as the parent of the `vigil mcp` child
/// is measured for `--print` only [evidence-neon-mcp-live.md, run 2]. The
/// Antigravity IDE and its language server are deliberately excluded: a
/// terminal inside the IDE would then slow every Claude session run in it.
const KNOWN_AGENT_COMMS: [(&str, KnownAgent); 6] = [
    ("claude", KnownAgent::Claude),
    (".claude-wrapped", KnownAgent::Claude),
    ("codex", KnownAgent::Codex),
    (".codex-wrapped", KnownAgent::Codex),
    ("agy", KnownAgent::Antigravity),
    (".agy-wrapped", KnownAgent::Antigravity),
];

fn known_agent_comm(name: &str) -> Option<KnownAgent> {
    KNOWN_AGENT_COMMS.iter().find_map(|&(comm, agent)| (comm == name).then_some(agent))
}

/// A real process tree is shallower; the bound also ends a walk through a loop.
const MAX_ANCESTRY_DEPTH: usize = 64;

/// CLAIM(neon-mcp-bound): true when the NEAREST known agent ancestor of `pid`
/// is Antigravity's CLI (`agy` or `.agy-wrapped`)
/// [peers_tests::an_agy_parent_or_ancestor_is_the_antigravity_process_tree_signal].
/// Starts at `pid`'s parent (the process itself is not an ancestor), skips
/// unknown names (e.g. `sh`, `bash`, `node`), and decides at the first name in
/// `KNOWN_AGENT_COMMS`. Env markers are ORed separately in `AgentEnv`. Reads
/// only `stat` (names and parent pids), never another process's environment
/// [peers_tests::the_ancestry_walk_never_reads_another_process_environment].
/// An unreadable link ends the walk with `false`.
pub fn antigravity_in_ancestry(root: &Path, pid: u32) -> bool {
    let mut seen = std::collections::BTreeSet::new();
    seen.insert(pid);
    let Ok((_, _, mut current)) = proc_stat(root, pid) else { return false };
    while current != 0 && seen.len() < MAX_ANCESTRY_DEPTH && seen.insert(current) {
        let Ok((name, _, parent)) = proc_stat(root, current) else { return false };
        if let Some(agent) = known_agent_comm(&name) {
            return agent == KnownAgent::Antigravity;
        }
        current = parent;
    }
    false
}

pub(crate) fn ancestor_session(root: &Path, mut pid: u32) -> Option<String> {
    let mut seen = std::collections::BTreeSet::new();
    while pid != 0 && seen.len() < MAX_ANCESTRY_DEPTH && seen.insert(pid) {
        let (name, _, parent) = proc_stat(root, pid).ok()?;
        if matches!(known_agent_comm(&name), Some(KnownAgent::Claude | KnownAgent::Codex)) {
            return Some(format!("ancestor:{pid}"));
        }
        pid = parent;
    }
    None
}

/// The binary reads environment at its edge; no session env is mutated by the
/// engine or tests. Empty values count as absent. Ancestry is the last resort.
pub fn session_key_from_lookup(mut lookup: impl FnMut(&str) -> Option<String>) -> Option<String> {
    for name in ["CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID", "T3CODE_THREAD_ID"] {
        if let Some(value) = lookup(name).filter(|value| !value.trim().is_empty()) {
            return Some(value);
        }
    }
    ancestor_session(Path::new("/proc"), std::process::id())
}
