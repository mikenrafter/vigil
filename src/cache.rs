//! Cache-aware call window (specs/auto-interval.md AI1', AI3, S4, S8).
//!
//! The agent should not decide how long a keepalive call blocks. A call made
//! by the main orchestrator session gets a window derived from the session's
//! prompt-cache TTL (read from its own Claude Code transcript); every other
//! call keeps the 270 s window. Mating parts: the transcript writer (Claude
//! Code) and `newest_cache_write` (reader); `choose_window` (decision) and the
//! envelope's `cache` field (report).
//!
//! Threat model (technical-excellence section 6): the transcript directory
//! belongs to the same user as vigil, so a planted file is not a privilege
//! boundary; the code still refuses to follow symlinks or open FIFOs and never
//! splices an unchecked session id into a path, so a stray link cannot make a
//! keepalive read an arbitrary file or hang on a pipe (S8). A file swapped
//! between the lookup and the open is accepted risk: the open re-checks that
//! the handle is a regular file.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::credit::CreditCheck;

/// AI1': the default window, and the window for a 5-minute or unknown TTL.
pub const DEFAULT_WINDOW_S: u64 = 270;
/// AI1' / D1: a 1 h TTL minus a 5 minute margin.
pub const ONE_HOUR_WINDOW_S: u64 = 3300;
/// A2: explicit windows are clamped to at least this ...
pub const MIN_WINDOW_S: u64 = 30;
/// ... and at most `ONE_HOUR_WINDOW_S`, or `CODEX_MAX_WINDOW_S` for Codex.
pub const CODEX_MAX_WINDOW_S: u64 = 270;
/// AI1' / D4: only this much of a transcript's end is read.
pub const TRANSCRIPT_TAIL_BYTES: u64 = 512 * 1024;
/// S8: a transcript line longer than this is skipped, not parsed.
pub const MAX_TRANSCRIPT_LINE_BYTES: usize = 64 * 1024;

/// The prompt-cache TTL as vigil knows it. Wire: `"1h"`, `"5m"`, `"unknown"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ttl {
    #[serde(rename = "1h")]
    OneHour,
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "unknown")]
    Unknown,
}

/// Where the window came from. Wire: kebab-case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TtlSource {
    ClaudeTranscript,
    CodexDefault,
    Default,
    Explicit,
}

/// The envelope's `cache` field (AI3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheNote {
    pub ttl: Ttl,
    pub source: TtlSource,
    /// The call window in seconds (A1); equals the envelope's `max_s`.
    pub interval_s: u64,
    /// AI2': the newest credit check made in this call, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit_check: Option<CreditCheck>,
}

impl CacheNote {
    /// The window of a caller that is not the orchestrator, or whose cache is
    /// not known: 270 s, `unknown`, `default`.
    pub const fn unknown_default() -> CacheNote {
        CacheNote { ttl: Ttl::Unknown, source: TtlSource::Default, interval_s: DEFAULT_WINDOW_S, credit_check: None }
    }
}

/// Which agent is calling, read from the environment at the binary edge
/// (A3). The library never reads the environment itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentEnv {
    /// `CLAUDE_CODE_SESSION_ID`; `None` when unset or empty.
    pub claude_session_id: Option<String>,
    /// `AGENT=codex` or any variable named `CODEX_*`.
    pub codex: bool,
}

/// Who is calling, as far as vigil can tell (S4). Derived from [`AgentEnv`]
/// once, so every decision matches on the same four cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity<'a> {
    /// Claude Code, and nothing suggests Codex: its transcript can be trusted.
    Claude { session_id: &'a str },
    /// Codex, and nothing suggests Claude Code: 5 minutes, 300 s tool timeout.
    Codex,
    /// Both signals are present (a Claude Code started from a Codex shell, or
    /// the reverse): vigil cannot say which one is calling, so it assumes the
    /// stricter limits of both (S4).
    Ambiguous,
    /// Neither signal.
    Unknown,
}

impl Identity<'_> {
    /// The largest window an explicit `max_s` may ask for (A2): 3300, the
    /// largest vigil ever picks itself. Codex's tool timeout (300 s) is a hard
    /// limit, so a caller that may be Codex, and one that is both Claude and
    /// Codex, get 270 (R2-A1, S4). The old fixed 270 ceiling is gone for every
    /// other caller: the engine tests that ask for 3300 without any agent
    /// signal pin that.
    pub const fn window_ceiling_s(self) -> u64 {
        match self {
            Identity::Codex | Identity::Ambiguous => CODEX_MAX_WINDOW_S,
            Identity::Claude { .. } | Identity::Unknown => ONE_HOUR_WINDOW_S,
        }
    }
}

impl AgentEnv {
    /// Build from the whole environment (`std::env::vars()` in the binary).
    pub fn from_vars(vars: impl IntoIterator<Item = (String, String)>) -> AgentEnv {
        let mut agent = AgentEnv::default();
        for (name, value) in vars {
            if name == "CLAUDE_CODE_SESSION_ID" && !value.is_empty() {
                agent.claude_session_id = Some(value);
            } else if name == "AGENT" && value == "codex" || name.starts_with("CODEX_") {
                agent.codex = true;
            }
        }
        agent
    }

    pub fn identity(&self) -> Identity<'_> {
        match (self.claude_session_id.as_deref(), self.codex) {
            (Some(_), true) => Identity::Ambiguous,
            (Some(session_id), false) => Identity::Claude { session_id },
            (None, true) => Identity::Codex,
            (None, false) => Identity::Unknown,
        }
    }
}

/// What `choose_window` decides from.
#[derive(Debug, Clone, Copy)]
pub struct WindowRequest<'a> {
    /// The caller's `max_s`, if any (A2).
    pub explicit_s: Option<u64>,
    /// The effective orchestrator flag (A8).
    pub orchestrator: bool,
    pub agent: &'a AgentEnv,
    pub projects_dir: Option<&'a Path>,
}

/// AI1' + AI3: the window for one call and the note that explains it. Reads
/// at most one transcript tail, and only for an orchestrator on Claude Code
/// with no explicit `max_s` (A4).
pub fn choose_window(request: &WindowRequest<'_>) -> CacheNote {
    let identity = request.agent.identity();
    // CLAIM(A2-explicit): an explicit window wins, within 30 s and the identity's ceiling [red-report A2, R2-A1; cache::an_explicit_max_s_wins_and_is_clamped_to_the_transport_limit]
    if let Some(explicit) = request.explicit_s {
        let interval_s = explicit.clamp(MIN_WINDOW_S, identity.window_ceiling_s());
        return CacheNote { ttl: Ttl::Unknown, source: TtlSource::Explicit, interval_s, credit_check: None };
    }
    if !request.orchestrator {
        return CacheNote::unknown_default();
    }
    match identity {
        Identity::Codex => CacheNote { ttl: Ttl::FiveMinutes, source: TtlSource::CodexDefault, interval_s: DEFAULT_WINDOW_S, credit_check: None },
        Identity::Ambiguous | Identity::Unknown => CacheNote::unknown_default(),
        Identity::Claude { session_id } => match request.projects_dir.and_then(|dir| ttl_of_session(dir, session_id)) {
            Some(Ttl::OneHour) => CacheNote { ttl: Ttl::OneHour, source: TtlSource::ClaudeTranscript, interval_s: ONE_HOUR_WINDOW_S, credit_check: None },
            Some(Ttl::FiveMinutes) => CacheNote { ttl: Ttl::FiveMinutes, source: TtlSource::ClaudeTranscript, interval_s: DEFAULT_WINDOW_S, credit_check: None },
            Some(Ttl::Unknown) | None => CacheNote::unknown_default(),
        },
    }
}

/// The shape of a transcript record, as far as vigil reads it (A6): nothing
/// else in the line is looked at, so text quoted inside a tool result or the
/// nested `iterations[]` copy cannot be mistaken for a usage block.
#[derive(Deserialize)]
struct Record {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default, rename = "isSidechain")]
    is_sidechain: bool,
    #[serde(default)]
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    cache_creation: Option<CacheCreation>,
}

#[derive(Deserialize)]
struct CacheCreation {
    #[serde(default)]
    ephemeral_1h_input_tokens: u64,
    #[serde(default)]
    ephemeral_5m_input_tokens: u64,
}

impl Record {
    /// The TTL this record wrote the cache with: `1h` is checked before `5m`
    /// (AI1'); a record that did not write (`0`/`0`), is not a main-session
    /// assistant turn, or has no `cache_creation` object says nothing.
    fn cache_write(&self) -> Option<Ttl> {
        if self.kind.as_deref() != Some("assistant") || self.is_sidechain {
            return None;
        }
        let split = self.message.as_ref()?.usage.as_ref()?.cache_creation.as_ref()?;
        if split.ephemeral_1h_input_tokens > 0 {
            Some(Ttl::OneHour)
        } else if split.ephemeral_5m_input_tokens > 0 {
            Some(Ttl::FiveMinutes)
        } else {
            None
        }
    }
}

/// The TTLs of the cache writes in `tail`, newest first (A6): complete JSON
/// lines only, main-session assistant records only, torn, junk, legacy and
/// over-long (S8) lines skipped. Shared by the window decision (newest write)
/// and the credit fallback (a 5m write after an earlier 1h one).
pub fn cache_writes_newest_first(tail: &[u8]) -> impl Iterator<Item = Ttl> + '_ {
    tail.rsplit(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty() && line.len() <= MAX_TRANSCRIPT_LINE_BYTES)
        .filter_map(|line| serde_json::from_slice::<Record>(line).ok())
        .filter_map(|record| record.cache_write())
}

/// The TTL of the newest usable cache write in `tail` (A6): complete JSON
/// lines only, `type == "assistant"`, not `isSidechain`, top-level
/// `message.usage.cache_creation` with a nonzero write; `ephemeral_1h` is
/// checked before `ephemeral_5m`. S8: a line longer than
/// `MAX_TRANSCRIPT_LINE_BYTES` is skipped like any other unusable line.
/// `None`: no usable write (unknown).
pub fn newest_cache_write(tail: &[u8]) -> Option<Ttl> {
    cache_writes_newest_first(tail).next()
}

/// The last `max_bytes` of `path`, cut at a line start (A6): when the cut
/// falls inside a line that line is dropped; when it falls exactly on a line
/// start the line is kept. A file no longer than `max_bytes` is returned
/// whole. The file is opened through the read meter and must be a regular file.
pub fn read_tail(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    let mut file = crate::meter::MeteredFile::open(path)?;
    read_tail_from(&mut file, max_bytes)
}

/// S8: `read_tail` over any `Read + Seek`: seek to `len - max_bytes - 1` (the
/// byte before the cut says whether it falls on a line start), read to the
/// end, drop a partial first line. At most `max_bytes + 1` bytes are ever
/// read, and none from before that offset, however long the source is.
pub fn read_tail_from<R: Read + Seek>(reader: &mut R, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    let len = reader.seek(SeekFrom::End(0))?;
    if len <= max_bytes {
        reader.seek(SeekFrom::Start(0))?;
        let mut whole = Vec::with_capacity(len as usize);
        reader.take(max_bytes).read_to_end(&mut whole)?;
        return Ok(whole);
    }
    // ↳ CLAIM(S8-tail): one byte before the cut tells whether the cut is a line start; nothing earlier is read [spec S8; cache::read_tail_from_pulls_only_the_last_512_kib_out_of_a_huge_file]
    reader.seek(SeekFrom::Start(len - max_bytes - 1))?;
    let mut bytes = Vec::with_capacity(max_bytes as usize + 1);
    reader.take(max_bytes + 1).read_to_end(&mut bytes)?;
    let cut_is_a_line_start = bytes.first() == Some(&b'\n');
    let keep_from = if cut_is_a_line_start {
        1
    } else {
        // Inside a line: drop it up to and including its newline. A tail that
        // is all one partial line has no complete line at all.
        bytes.iter().position(|&byte| byte == b'\n').map_or(bytes.len(), |newline| newline + 1)
    };
    bytes.drain(..keep_from);
    Ok(bytes)
}

/// `<projects_dir>/*/<session_id>.jsonl` (A5): one level deep, never under a
/// `subagents` directory, newest mtime wins; an id outside
/// `[A-Za-z0-9_-]{1,64}` finds nothing. S8: regular files only; a symlink
/// (to a file or to a project directory) or a FIFO is not a transcript and is
/// never opened.
pub fn find_session_transcript(projects_dir: &Path, session_id: &str) -> Option<PathBuf> {
    let valid = (1..=64).contains(&session_id.len()) && session_id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if !valid {
        return None;
    }
    let file_name = format!("{session_id}.jsonl");
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for project in regular_subdirectories(projects_dir) {
        let candidate = project.join(&file_name);
        let Ok(metadata) = std::fs::symlink_metadata(&candidate) else { continue };
        if !metadata.file_type().is_file() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
        if newest.as_ref().is_none_or(|(best, _)| modified > *best) {
            newest = Some((modified, candidate));
        }
    }
    newest.map(|(_, path)| path)
}

/// The directories directly below `dir` that are real directories (a symlink
/// to a directory is not one, S8), in name order. An unreadable `dir` has none.
pub(crate) fn regular_subdirectories(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect();
    found.sort();
    found
}

/// `newest_cache_write` over the last `TRANSCRIPT_TAIL_BYTES` of the session's
/// transcript; `None` when the file is missing, unreadable or has no usable
/// write.
pub fn ttl_of_session(projects_dir: &Path, session_id: &str) -> Option<Ttl> {
    let path = find_session_transcript(projects_dir, session_id)?;
    newest_cache_write(&read_tail(&path, TRANSCRIPT_TAIL_BYTES).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;

    fn tail(lines: &[String]) -> Vec<u8> {
        let mut text = lines.join("\n");
        text.push('\n');
        text.into_bytes()
    }

    // ---- newest_cache_write (AI1') ----

    #[test]
    fn the_newest_one_hour_write_wins_over_an_older_five_minute_write() {
        // why: AI1': ephemeral_1h_input_tokens > 0 -> 1 h. A session that
        //      moved from 5m to 1h (e.g. the pin was applied) must widen.
        // proves: CLAIM(AI1')
        let lines = [assistant_line(0, 500), assistant_line(900, 0)];
        assert_eq!(newest_cache_write(&tail(&lines)), Some(Ttl::OneHour));
    }

    #[test]
    fn the_newest_five_minute_write_wins_over_an_older_one_hour_write() {
        // why: AI1' + AI2': after usage credits start (and the hook flips
        //      promptCacheTtl to 5m) the session's writes become 5m; an old 1h
        //      write must not keep the window at 3300 s, or the cache goes
        //      cold between calls. Mirror of the test above: "any 1h write"
        //      would pass that one and fail this one.
        // proves: CLAIM(AI1')
        let lines = [assistant_line(900, 0), assistant_line(0, 500)];
        assert_eq!(newest_cache_write(&tail(&lines)), Some(Ttl::FiveMinutes));
    }

    #[test]
    fn turns_that_only_read_the_cache_are_skipped_to_the_newest_real_write() {
        // why: AI1' "nonzero write": most turns write nothing (cache hit);
        //      judging the TTL from a 0/0 turn would answer "unknown" almost
        //      always. Two read-only turns after a 1h write -> still 1h.
        // proves: CLAIM(AI1')
        let lines = [assistant_line(900, 0), assistant_line(0, 0), assistant_line(0, 0)];
        assert_eq!(newest_cache_write(&tail(&lines)), Some(Ttl::OneHour));
    }

    #[test]
    fn when_one_record_wrote_both_ttls_one_hour_wins() {
        // why: AI1' orders the checks: 1h > 0 first, else 5m > 0.
        // proves: CLAIM(AI1')
        assert_eq!(newest_cache_write(&tail(&[assistant_line(100, 100)])), Some(Ttl::OneHour));
    }

    #[test]
    fn a_tail_without_a_write_is_unknown() {
        // why: AI1': nothing found -> unknown (-> 270 s). Empty input, only
        //      read-only turns, and plain garbage all say None.
        // proves: CLAIM(AI1')
        assert_eq!(newest_cache_write(b""), None);
        assert_eq!(newest_cache_write(&tail(&[assistant_line(0, 0)])), None);
        assert_eq!(newest_cache_write(b"not json at all\n"), None);
        // Positive control: the same junk with one real write beside it is found.
        assert_eq!(newest_cache_write(&tail(&[assistant_line(0, 0), "not json".into(), assistant_line(900, 0)])), Some(Ttl::OneHour));
    }

    #[test]
    fn only_main_session_assistant_records_count() {
        // why: A6 + the spec's "Known limit": subagent turns (isSidechain)
        //      use a 5m cache; a sidechain 1h/5m write must not move the main
        //      session's window. Text inside a tool result that merely quotes
        //      a usage block must not count either (a naive grep for
        //      "ephemeral_1h" would). Newest-last order: main 5m, then a
        //      sidechain 1h, then a user record quoting a 1h block -> 5m.
        //      Positive control: the same sequence with the sidechain record
        //      being a plain main record gives 1h, so the flag is what
        //      decided it.
        // proves: CLAIM(AI1')
        let lines = [assistant_line(0, 500), sidechain_line(900, 0), user_line_quoting(&assistant_line(900, 0))];
        assert_eq!(newest_cache_write(&tail(&lines)), Some(Ttl::FiveMinutes));
        let control = [assistant_line(0, 500), assistant_line(900, 0)];
        assert_eq!(newest_cache_write(&tail(&control)), Some(Ttl::OneHour));
    }

    #[test]
    fn torn_garbage_and_legacy_records_are_skipped_not_fatal() {
        // why: A6: a transcript is being appended while vigil reads it, so
        //      the last line can be torn; other lines can be junk. None of
        //      that may turn a known 1h into "unknown", and none may panic.
        //      A record that has only the legacy total
        //      (cache_creation_input_tokens, no cache_creation object) cannot
        //      say which TTL it wrote and is skipped.
        // proves: CLAIM(AI1')
        let legacy = serde_json::json!({"type": "assistant", "isSidechain": false,
            "message": {"role": "assistant", "usage": {"cache_creation_input_tokens": 500}}})
        .to_string();
        let torn = r#"{"type":"assistant","message":{"usage":{"cache_creation":{"ephemeral_5m"#;
        let mut bytes = tail(&[assistant_line(900, 0), legacy, "not json".to_string()]);
        bytes.extend_from_slice(torn.as_bytes());
        assert_eq!(newest_cache_write(&bytes), Some(Ttl::OneHour));
    }

    // ---- read_tail (AI1' / D4) ----

    fn numbered_lines(count: usize, line_bytes: usize) -> Vec<u8> {
        // Line i is "{i:06}" padded with 'x' to line_bytes - 1, plus "\n".
        let mut out = Vec::new();
        for i in 0..count {
            let mut line = format!("{i:06}");
            while line.len() < line_bytes - 1 {
                line.push('x');
            }
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        }
        out
    }

    #[test]
    fn a_file_no_longer_than_the_cap_is_read_whole() {
        // why: D4: the cap only bites on big transcripts; a small one must
        //      come back byte for byte, first line included.
        // proves: CLAIM(AI1'-tail)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let bytes = numbered_lines(10, 100);
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(read_tail(&path, TRANSCRIPT_TAIL_BYTES).unwrap(), bytes);
        assert_eq!(read_tail(&path, bytes.len() as u64).unwrap(), bytes, "exactly the cap is still whole");
    }

    #[test]
    fn a_cut_on_a_line_start_keeps_that_line() {
        // why: A6 boundary. 600 lines of 1024 bytes = 614400 bytes; the cap
        //      512 KiB = 524288 = exactly 512 lines, so the cut lands on the
        //      start of line 88 (600 - 512). That line is complete and must
        //      be kept: 524288 bytes back, first line "000088...". Dropping
        //      it on every cut would lose 1 KiB of the newest history.
        // proves: CLAIM(AI1'-tail)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, numbered_lines(600, 1024)).unwrap();
        let got = read_tail(&path, TRANSCRIPT_TAIL_BYTES).unwrap();
        assert_eq!(got.len(), 524_288);
        assert!(got.starts_with(b"000088"), "first line is {:?}", String::from_utf8_lossy(&got[..8]));
        assert!(got.ends_with(b"xxx\n"));
    }

    #[test]
    fn a_cut_inside_a_line_drops_the_partial_first_line() {
        // why: A6. 700 lines of 1000 bytes = 700000; tail 524288 starts at
        //      offset 175712 = inside line 175 (175 x 1000 = 175000 ..
        //      176000). The partial line is dropped, so the result is lines
        //      176..700 = 524 lines = 524000 bytes, starting "000176". A
        //      torn first line fed to the parser would be a malformed record
        //      at best and a wrong match at worst.
        // proves: CLAIM(AI1'-tail)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, numbered_lines(700, 1000)).unwrap();
        let got = read_tail(&path, TRANSCRIPT_TAIL_BYTES).unwrap();
        assert_eq!(got.len(), 524_000);
        assert!(got.starts_with(b"000176"));
    }

    #[test]
    fn reading_the_tail_of_a_missing_file_is_an_error_not_an_empty_answer() {
        // why: technical-excellence rich interface: the caller decides that an
        //      unreadable transcript means "unknown"; the reader must say it
        //      failed rather than pretend the file was empty.
        let dir = tempfile::tempdir().unwrap();
        assert!(read_tail(&dir.path().join("nope.jsonl"), 1024).is_err());
    }

    // ---- ttl_of_session (AI1' / D4) ----

    #[test]
    fn only_the_last_512_kib_of_a_transcript_is_considered() {
        // why: D4 / AI1': "tail <= 512 KiB". A 1h write followed by more than
        //      512 KiB of other records is out of the window -> unknown
        //      (the call falls back to 270 s, the safe side). Positive
        //      control in the same test: the same write placed after the
        //      filler is found, so the answer is the window, not a broken
        //      reader.
        // proves: CLAIM(AI1'-tail)
        let dir = tempfile::tempdir().unwrap();
        let filler: Vec<String> = (0..700).map(|i| user_line_quoting(&format!("filler {i} {}", "x".repeat(900)))).collect();
        let mut old_write = vec![assistant_line(900, 0)];
        old_write.extend(filler.clone());
        write_transcript(dir.path(), "proj", "sess-a", &old_write);
        assert_eq!(ttl_of_session(dir.path(), "sess-a"), None);

        let mut new_write = filler;
        new_write.push(assistant_line(900, 0));
        write_transcript(dir.path(), "proj", "sess-b", &new_write);
        assert_eq!(ttl_of_session(dir.path(), "sess-b"), Some(Ttl::OneHour));
    }

    // ---- find_session_transcript (A5) ----

    #[test]
    fn a_session_transcript_is_found_one_level_down_and_never_under_subagents() {
        // why: A5 / AI1': `~/.claude/projects/*/<id>.jsonl`. The subagent
        //      transcripts live at projects/<p>/<id>/subagents/*.jsonl and
        //      must not be mistaken for the session's. Layout: proj-a has
        //      S1.jsonl and a S1/subagents/agent-1.jsonl; proj-b has S2.
        // proves: CLAIM(AI1')
        let root = tempfile::tempdir().unwrap();
        let s1 = write_transcript(root.path(), "proj-a", "S1", &[assistant_line(900, 0)]);
        let s2 = write_transcript(root.path(), "proj-b", "S2", &[assistant_line(0, 500)]);
        let sub = root.path().join("proj-a/S1/subagents");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("agent-1.jsonl"), "x\n").unwrap();
        assert_eq!(find_session_transcript(root.path(), "S1"), Some(s1));
        assert_eq!(find_session_transcript(root.path(), "S2"), Some(s2));
        assert_eq!(find_session_transcript(root.path(), "agent-1"), None);
        assert_eq!(find_session_transcript(root.path(), "S3"), None);
    }

    #[test]
    fn when_two_project_dirs_hold_the_session_the_newest_file_wins() {
        // why: A5: a resumed or moved session can leave a stale copy; the
        //      live one is the most recently written. Ages: stale 1000 s old,
        //      live 10 s old.
        // proves: CLAIM(AI1')
        let root = tempfile::tempdir().unwrap();
        let stale = write_transcript(root.path(), "old-project", "S4", &[assistant_line(0, 500)]);
        let live = write_transcript(root.path(), "new-project", "S4", &[assistant_line(900, 0)]);
        set_age(&stale, spec_instant(), 1000);
        set_age(&live, spec_instant(), 10);
        assert_eq!(find_session_transcript(root.path(), "S4"), Some(live));
    }

    #[test]
    fn a_session_id_that_could_escape_the_projects_dir_finds_nothing() {
        // why: A5 / threat model: CLAUDE_CODE_SESSION_ID is spliced into a
        //      path. Only [A-Za-z0-9_-]{1,64} is accepted. Decoys: a file at
        //      projects/outside.jsonl (what "../outside" would reach) and a
        //      real transcript that the glob "*" would match. Positive
        //      control: the 64-char id (the boundary) is found; the 65-char
        //      one is not.
        // proves: CLAIM(AI1'-session-id)
        let root = tempfile::tempdir().unwrap();
        write_transcript(root.path(), "proj", "S1", &[assistant_line(900, 0)]);
        std::fs::write(root.path().join("outside.jsonl"), "x\n").unwrap();
        let ok64 = "a".repeat(64);
        let found = write_transcript(root.path(), "proj", &ok64, &[assistant_line(900, 0)]);
        write_transcript(root.path(), "proj", &"b".repeat(65), &[assistant_line(900, 0)]);
        assert_eq!(find_session_transcript(root.path(), &ok64), Some(found));
        let too_long = "b".repeat(65);
        for bad in ["../outside", "*", "S1/../S1", "", "S 1", "S1.jsonl", too_long.as_str()] {
            assert_eq!(find_session_transcript(root.path(), bad), None, "id {bad:?}");
        }
    }

    // ---- AgentEnv (A3) ----

    fn env_of(pairs: &[(&str, &str)]) -> AgentEnv {
        AgentEnv::from_vars(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())))
    }

    #[test]
    fn the_agent_is_read_from_the_environment() {
        // why: A3 / AI1: Claude by CLAUDE_CODE_SESSION_ID; Codex by AGENT=codex
        //      or any CODEX_* name. `AGENT=claude` (set on the dev machine)
        //      and an empty session id are not signals. Both present: both
        //      fields set; what that means (ambiguous, S4) is choose_window's.
        // proves: CLAIM(AI1')
        assert_eq!(env_of(&[]), AgentEnv { claude_session_id: None, codex: false });
        assert_eq!(env_of(&[("CLAUDE_CODE_SESSION_ID", "abc-1")]), AgentEnv { claude_session_id: Some("abc-1".into()), codex: false });
        assert_eq!(env_of(&[("CLAUDE_CODE_SESSION_ID", "")]), AgentEnv::default());
        assert_eq!(env_of(&[("AGENT", "claude")]), AgentEnv::default());
        assert_eq!(env_of(&[("AGENT", "codex")]), AgentEnv { claude_session_id: None, codex: true });
        assert_eq!(env_of(&[("CODEX_HOME", "/x")]), AgentEnv { claude_session_id: None, codex: true });
        assert_eq!(env_of(&[("CODEX_THREAD_ID", "t")]), AgentEnv { claude_session_id: None, codex: true });
        assert_eq!(env_of(&[("NOT_CODEX_X", "1"), ("MYCODEX_", "1")]), AgentEnv::default(), "only a CODEX_ prefix counts");
        assert_eq!(
            env_of(&[("CLAUDE_CODE_SESSION_ID", "s"), ("CODEX_HOME", "/x")]),
            AgentEnv { claude_session_id: Some("s".into()), codex: true }
        );
    }

    // ---- choose_window (AI1', AI3) ----

    #[derive(Clone, Copy)]
    enum Transcript {
        OneHourWrite,
        FiveMinuteWrite,
        NoWrite,
        Absent,
        /// `<id>.jsonl` exists but is a directory: reading it fails.
        Unreadable,
    }

    fn choose(explicit_s: Option<u64>, orchestrator: bool, claude: bool, codex: bool, transcript: Transcript) -> CacheNote {
        let root = tempfile::tempdir().unwrap();
        let agent = AgentEnv { claude_session_id: claude.then(|| "sess-1".to_string()), codex };
        match transcript {
            Transcript::OneHourWrite => drop(write_transcript(root.path(), "p", "sess-1", &[assistant_line(0, 500), assistant_line(900, 0)])),
            Transcript::FiveMinuteWrite => drop(write_transcript(root.path(), "p", "sess-1", &[assistant_line(900, 0), assistant_line(0, 500)])),
            Transcript::NoWrite => drop(write_transcript(root.path(), "p", "sess-1", &[assistant_line(0, 0)])),
            Transcript::Absent => {}
            Transcript::Unreadable => std::fs::create_dir_all(root.path().join("p/sess-1.jsonl")).unwrap(),
        }
        choose_window(&WindowRequest { explicit_s, orchestrator, agent: &agent, projects_dir: Some(root.path()) })
    }

    fn note(ttl: Ttl, source: TtlSource, interval_s: u64) -> CacheNote {
        CacheNote { ttl, source, interval_s, credit_check: None }
    }

    #[test]
    fn the_window_is_270_unless_an_orchestrator_on_a_known_one_hour_cache() {
        // why: AI1' (Rev 2, Rev 4): the default is always 270 s; only an
        //      orchestrator gets the TTL-derived window. Table, derived from
        //      the spec (3300 = 3600 - 300 margin, D1):
        //        subagent (no flag) on a 1h Claude cache  -> 270, unknown/default
        //        orchestrator, 1h                         -> 3300, 1h/claude-transcript
        //        orchestrator, 5m                         -> 270, 5m/claude-transcript
        //        orchestrator, no usable write / no file / unreadable -> 270, unknown/default
        //        orchestrator on Codex                    -> 270, 5m/codex-default
        //        orchestrator, neither agent              -> 270, unknown/default
        //        Claude id AND codex flag                 -> 270, unknown/default (S4:
        //                                                   ambiguous identity; reverses A3)
        // proves: CLAIM(AI1'), CLAIM(AI3), CLAIM(S4)
        use Transcript::*;
        use Ttl::*;
        use TtlSource::*;
        let cases = [
            ("subagent, 1h cache", choose(None, false, true, false, OneHourWrite), note(Unknown, Default, 270)),
            ("orchestrator, 1h", choose(None, true, true, false, OneHourWrite), note(OneHour, ClaudeTranscript, 3300)),
            ("orchestrator, 5m", choose(None, true, true, false, FiveMinuteWrite), note(FiveMinutes, ClaudeTranscript, 270)),
            ("orchestrator, zero writes", choose(None, true, true, false, NoWrite), note(Unknown, Default, 270)),
            ("orchestrator, no file", choose(None, true, true, false, Absent), note(Unknown, Default, 270)),
            ("orchestrator, unreadable", choose(None, true, true, false, Unreadable), note(Unknown, Default, 270)),
            ("orchestrator, codex", choose(None, true, false, true, Absent), note(FiveMinutes, CodexDefault, 270)),
            ("orchestrator, neither", choose(None, true, false, false, Absent), note(Unknown, Default, 270)),
            ("orchestrator, claude and codex", choose(None, true, true, true, OneHourWrite), note(Unknown, Default, 270)),
            ("subagent, codex", choose(None, false, false, true, Absent), note(Unknown, Default, 270)),
        ];
        for (name, got, want) in cases {
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn an_explicit_max_s_wins_and_is_clamped_to_the_transport_limit() {
        // why: AI1' "explicit max_s still wins, clamped to transport limits
        //      (Codex MCP <= 270 s)" with the A2 bounds: 30..=3300 normally,
        //      30..=270 on Codex. An explicit call reports ttl unknown and
        //      source explicit (A4): vigil did not look. Table:
        //        100            -> 100          (even for an orchestrator on a 1h cache)
        //        5, 0           -> 30           (floor)
        //        3000           -> 3000
        //        9999           -> 3300         (claude), 270 (codex)
        //        271            -> 271 (claude), 270 (codex)
        //        non-orchestrator 1000 -> 1000  (explicit means explicit)
        // proves: CLAIM(AI1'), CLAIM(AI3)
        use Transcript::*;
        let explicit = |s: u64| note(Ttl::Unknown, TtlSource::Explicit, s);
        assert_eq!(choose(Some(100), true, true, false, OneHourWrite), explicit(100));
        assert_eq!(choose(Some(5), true, true, false, OneHourWrite), explicit(30));
        assert_eq!(choose(Some(0), false, false, false, Absent), explicit(30));
        assert_eq!(choose(Some(3000), true, true, false, OneHourWrite), explicit(3000));
        assert_eq!(choose(Some(9999), true, true, false, OneHourWrite), explicit(3300));
        assert_eq!(choose(Some(271), false, true, false, OneHourWrite), explicit(271));
        assert_eq!(choose(Some(1000), false, true, false, OneHourWrite), explicit(1000));
        assert_eq!(choose(Some(9999), true, false, true, Absent), explicit(270));
        assert_eq!(choose(Some(271), false, false, true, Absent), explicit(270));
        assert_eq!(choose(Some(60), true, false, true, Absent), explicit(60));
    }

    #[test]
    fn the_cache_note_has_the_wire_shape_of_ai3() {
        // why: AI3 wire: {"ttl","source","interval_s"} with kebab-case sources
        //      and "1h"/"5m"/"unknown". Pinned as literal JSON, field by
        //      field, for each source; `credit_check` is absent unless set
        //      (AI6: short calls add nothing beyond these three keys).
        // proves: CLAIM(AI3)
        let cases = [
            (note(Ttl::OneHour, TtlSource::ClaudeTranscript, 3300), r#"{"ttl":"1h","source":"claude-transcript","interval_s":3300}"#),
            (note(Ttl::FiveMinutes, TtlSource::CodexDefault, 270), r#"{"ttl":"5m","source":"codex-default","interval_s":270}"#),
            (note(Ttl::Unknown, TtlSource::Default, 270), r#"{"ttl":"unknown","source":"default","interval_s":270}"#),
            (note(Ttl::Unknown, TtlSource::Explicit, 60), r#"{"ttl":"unknown","source":"explicit","interval_s":60}"#),
        ];
        for (value, wire) in cases {
            assert_eq!(serde_json::to_string(&value).unwrap(), wire);
            assert_eq!(serde_json::from_str::<CacheNote>(wire).unwrap(), value);
        }
    }

    // ---- S4: ambiguous identity ----

    #[test]
    fn a_claude_session_id_together_with_a_codex_signal_is_ambiguous_and_gets_270_s() {
        // why: S4 (reverses red default A3). A Claude Code started from a Codex
        //      shell inherits CODEX_*; a Codex started from a Claude Code shell
        //      inherits CLAUDE_CODE_SESSION_ID. vigil cannot tell which one is
        //      calling, and guessing Claude hands a Codex session a 3300 s call
        //      that its 300 s tool timeout kills. Both nesting directions, each
        //      with an orchestrator flag and a transcript whose newest write is
        //      1h: the window stays 270 s, unknown / default, and the
        //      transcript is not trusted. Positive control: the same transcript
        //      with only the Claude signal gives 3300 s, so the 270 comes from
        //      the second signal and not from a broken transcript reader.
        // proves: CLAIM(S4)
        let root = tempfile::tempdir().unwrap();
        write_transcript(root.path(), "p", "sess-1", &[assistant_line(0, 500), assistant_line(900, 0)]);
        let window_for = |pairs: &[(&str, &str)]| {
            let agent = env_of(pairs);
            (agent.clone(), choose_window(&WindowRequest { explicit_s: None, orchestrator: true, agent: &agent, projects_dir: Some(root.path()) }))
        };
        let (claude_only, note_claude) = window_for(&[("CLAUDE_CODE_SESSION_ID", "sess-1")]);
        assert_eq!(claude_only, AgentEnv { claude_session_id: Some("sess-1".into()), codex: false });
        assert_eq!(note_claude, note(Ttl::OneHour, TtlSource::ClaudeTranscript, 3300), "control: Claude alone is trusted");

        let nestings = [
            ("claude started from a codex shell (CODEX_HOME)", vec![("CLAUDE_CODE_SESSION_ID", "sess-1"), ("CODEX_HOME", "/x")]),
            ("claude started from a codex shell (CODEX_THREAD_ID)", vec![("CLAUDE_CODE_SESSION_ID", "sess-1"), ("CODEX_THREAD_ID", "t")]),
            ("codex started from a claude shell (AGENT=codex)", vec![("CLAUDE_CODE_SESSION_ID", "sess-1"), ("AGENT", "codex")]),
        ];
        for (name, pairs) in nestings {
            let (agent, got) = window_for(&pairs);
            assert_eq!(agent, AgentEnv { claude_session_id: Some("sess-1".into()), codex: true }, "{name}: both signals are kept");
            assert_eq!(got, note(Ttl::Unknown, TtlSource::Default, 270), "{name}");
        }
    }

    #[test]
    fn an_explicit_max_s_under_an_ambiguous_identity_keeps_the_codex_ceiling() {
        // why: S4 + A2. If the caller may be Codex its 300 s tool timeout is a
        //      hard limit, so an explicit 9999 under both signals is clamped to
        //      270 (not 3300) and an explicit 60 stays 60. Default taken, the
        //      spec only says "ambiguous -> 270 s" (report: R2-A1). Control:
        //      Claude alone gets 3300 for 9999.
        // proves: CLAIM(S4)
        use Transcript::*;
        assert_eq!(choose(Some(9999), true, true, false, OneHourWrite), note(Ttl::Unknown, TtlSource::Explicit, 3300), "control: Claude alone");
        assert_eq!(choose(Some(9999), true, true, true, OneHourWrite), note(Ttl::Unknown, TtlSource::Explicit, 270));
        assert_eq!(choose(Some(60), true, true, true, OneHourWrite), note(Ttl::Unknown, TtlSource::Explicit, 60));
    }

    // ---- S8: bounded reads ----

    /// The lines `000000xxx...\n`, `000001xxx...\n`, ... each `line_bytes` long.
    fn expected_lines(range: std::ops::Range<usize>, line_bytes: usize) -> Vec<u8> {
        numbered_lines(range.end, line_bytes)[range.start * line_bytes..].to_vec()
    }

    #[test]
    fn read_tail_from_pulls_only_the_last_512_kib_out_of_a_huge_file() {
        // why: S8 "assert bytes READ via the seam, not just bytes returned". A
        //      reader that reads the whole 5 MiB file and then keeps the last
        //      512 KiB returns the right bytes and defeats the bound. The
        //      source is a CountingReader. 5120 lines x 1024 B = 5 242 880 B;
        //      tail cap 524 288 = exactly 512 lines, so the cut is a line start
        //      and the result is lines 4608..5120 (4608 = 5120 - 512). At most
        //      cap + 1 bytes may be read (the byte before the cut says whether
        //      it is a line start) and none before offset 5 242 880 - 524 289 =
        //      4 718 591.
        // proves: CLAIM(S8-tail)
        let bytes = numbered_lines(5120, 1024);
        assert_eq!(bytes.len(), 5_242_880);
        let mut reader = CountingReader::new(bytes);
        let got = read_tail_from(&mut reader, TRANSCRIPT_TAIL_BYTES).unwrap();
        assert_eq!(got, expected_lines(4608..5120, 1024), "the tail bytes");
        assert!(reader.bytes_read() <= 524_289, "read {} bytes of a 5 MiB file", reader.bytes_read());
        assert!(reader.lowest_offset().expect("something was read") >= 4_718_591, "read from offset {:?}", reader.lowest_offset());
    }

    #[test]
    fn read_tail_from_a_cut_inside_a_line_still_reads_at_most_the_cap_plus_one() {
        // why: S8 + A6. 4000 lines x 1000 B = 4 000 000 B; tail 524 288 starts at
        //      3 475 712 = inside line 3475 (3 475 000 .. 3 476 000), so that
        //      partial line is dropped and the result is lines 3476..4000 = 524
        //      lines. The partial line costs no extra reading beyond the +1.
        // proves: CLAIM(S8-tail)
        let mut reader = CountingReader::new(numbered_lines(4000, 1000));
        let got = read_tail_from(&mut reader, TRANSCRIPT_TAIL_BYTES).unwrap();
        assert_eq!(got, expected_lines(3476..4000, 1000));
        assert!(reader.bytes_read() <= 524_289, "read {} bytes", reader.bytes_read());
        assert!(reader.lowest_offset().expect("something was read") >= 3_475_711, "read from offset {:?}", reader.lowest_offset());
    }

    #[test]
    fn read_tail_from_a_small_file_reads_it_once_and_whole() {
        // why: S8 control for the two tests above: below the cap the whole file
        //      comes back (first line included) and no more bytes are read than
        //      the file has (10 lines x 100 B = 1000), so "reads at most cap+1"
        //      is not a reader that always reads a fixed block.
        // proves: CLAIM(S8-tail)
        let bytes = numbered_lines(10, 100);
        let mut reader = CountingReader::new(bytes.clone());
        assert_eq!(read_tail_from(&mut reader, TRANSCRIPT_TAIL_BYTES).unwrap(), bytes);
        assert_eq!(reader.bytes_read(), 1000);
        assert_eq!(reader.lowest_offset(), Some(0));
    }

    #[test]
    fn a_transcript_line_over_64_kib_is_skipped_not_parsed() {
        // why: S8 "lines over 64 KiB skipped". One main-session record carries a
        //      5m write but is 65 537 bytes long (1 over 64 KiB = 65 536); the
        //      older 1h write beside it is normal. The long line must not be
        //      parsed, so the answer is the older 1h. Boundary control: the same
        //      record at exactly 65 536 bytes IS parsed and wins as the newest
        //      (5m). A parser that buffers and parses every line gives 5m for
        //      both.
        // proves: CLAIM(S8-line)
        let older = assistant_line(900, 0);
        let over = assistant_line_of_len(0, 500, 65_537);
        let exactly = assistant_line_of_len(0, 500, 65_536);
        assert_eq!(newest_cache_write(&tail(&[older.clone(), exactly])), Some(Ttl::FiveMinutes), "65 536 bytes is within the bound");
        assert_eq!(newest_cache_write(&tail(&[older, over])), Some(Ttl::OneHour), "65 537 bytes is skipped");
    }

    /// Run `work` on a thread and fail the test, not hang it, when it blocks.
    fn within_5_s<T: Send + 'static>(what: &str, work: impl FnOnce() -> T + Send + 'static) -> T {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(work());
        });
        receiver.recv_timeout(std::time::Duration::from_secs(5)).unwrap_or_else(|_| panic!("{what} did not return in 5 s"))
    }

    #[test]
    fn a_symlinked_transcript_or_project_dir_is_not_followed() {
        // why: S8 "regular files only, symlinks not followed" (threat model: a
        //      planted link under ~/.claude/projects must not make vigil read
        //      an arbitrary file). (1) A link named S1.jsonl pointing at a real
        //      transcript elsewhere finds nothing; (2) a project dir that is a
        //      link to a dir holding S2.jsonl finds nothing. Positive control:
        //      a regular S3.jsonl in a regular project dir is found.
        // proves: CLAIM(S8-symlink)
        let outside = tempfile::tempdir().unwrap();
        let real = write_transcript(outside.path(), "x", "real", &[assistant_line(900, 0)]);
        let real_dir = write_transcript(outside.path(), "dir", "S2", &[assistant_line(900, 0)]);
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("proj")).unwrap();
        std::os::unix::fs::symlink(&real, root.path().join("proj/S1.jsonl")).unwrap();
        std::os::unix::fs::symlink(real_dir.parent().unwrap(), root.path().join("linked-project")).unwrap();
        let regular = write_transcript(root.path(), "proj", "S3", &[assistant_line(900, 0)]);
        assert_eq!(find_session_transcript(root.path(), "S3"), Some(regular));
        assert_eq!(find_session_transcript(root.path(), "S1"), None, "a symlinked file");
        assert_eq!(find_session_transcript(root.path(), "S2"), None, "a file under a symlinked project dir");
        assert_eq!(ttl_of_session(root.path(), "S1"), None);
    }

    #[test]
    fn a_fifo_named_like_a_transcript_is_skipped_without_blocking() {
        // why: S8 "regular files only": opening a FIFO for reading blocks until a
        //      writer shows up, which would hang every keepalive call. The FIFO
        //      is `proj/S4.jsonl`. Both lookups must return None inside 5 s.
        //      Control: a regular file next to it is found.
        // proves: CLAIM(S8-symlink)
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("proj")).unwrap();
        let fifo = root.path().join("proj/S4.jsonl");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status().expect("mkfifo is available").success();
        assert!(made, "mkfifo failed");
        let regular = write_transcript(root.path(), "proj", "S5", &[assistant_line(900, 0)]);
        assert_eq!(find_session_transcript(root.path(), "S5"), Some(regular), "control");
        let dir = root.path().to_path_buf();
        let (found, ttl) = within_5_s("looking up a FIFO transcript", move || (find_session_transcript(&dir, "S4"), ttl_of_session(&dir, "S4")));
        assert_eq!((found, ttl), (None, None));
    }

    // ---- N2: consumption on the production path ----

    #[test]
    fn choosing_a_window_consumes_at_most_the_tail_of_a_huge_real_transcript() {
        // why: N2. `read_tail_from_*` above prove the reader on an in-memory
        //      source; this proves the PRODUCTION path (`choose_window` ->
        //      `ttl_of_session` -> the real file) through the read meter, which
        //      counts the bytes the code really pulled from the file. A 3 MB
        //      transcript (a 5m write first, 3000 lines of ~1 KB filler, the 1h
        //      write last): the answer is 1h, at most 512 KiB + 1 byte
        //      (524 289) were consumed, none from before offset len - 524 289,
        //      and only that file was opened. Control in the same test: a
        //      small transcript is consumed whole from offset 0, so the meter
        //      sees real reads and the bound above is not "the meter saw
        //      nothing".
        // proves: CLAIM(N2), CLAIM(S8-tail)
        let root = tempfile::tempdir().unwrap();
        let mut lines = vec![assistant_line(0, 500)];
        lines.extend((0..3000).map(|i| user_line_quoting(&format!("filler {i} {}", "x".repeat(900)))));
        lines.push(assistant_line(900, 0));
        let big = write_transcript(root.path(), "p", "sess-1", &lines);
        let len = std::fs::metadata(&big).unwrap().len();
        assert!(len > 2_500_000, "the fixture must dwarf the cap, it is {len} bytes");
        let tap = crate::meter::ReadTap::on(root.path());
        let agent = AgentEnv { claude_session_id: Some("sess-1".into()), codex: false };
        let got = choose_window(&WindowRequest { explicit_s: None, orchestrator: true, agent: &agent, projects_dir: Some(root.path()) });
        assert_eq!(got, note(Ttl::OneHour, TtlSource::ClaudeTranscript, 3300));
        let tally = tap.of(&big);
        assert!(tally.bytes_read > 0, "the meter must see the production read: {tally:?}");
        assert!(tally.bytes_read <= 524_289, "consumed {} bytes of a {len}-byte transcript", tally.bytes_read);
        assert!(tally.lowest_offset.expect("a read happened") >= len - 524_289, "read from offset {:?} of {len}", tally.lowest_offset);
        assert_eq!((tally.opens, tap.files_opened()), (1, 1), "only the session's transcript is opened");

        let small = write_transcript(root.path(), "q", "sess-2", &[assistant_line(900, 0)]);
        assert_eq!(ttl_of_session(root.path(), "sess-2"), Some(Ttl::OneHour));
        let small_len = std::fs::metadata(&small).unwrap().len();
        assert_eq!((tap.of(&small).bytes_read, tap.of(&small).lowest_offset), (small_len, Some(0)), "control: a small file is read whole, once");
    }
}
