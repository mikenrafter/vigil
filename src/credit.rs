//! Account-wide credit state (specs/auto-interval.md AI2', AI8).
//!
//! Usage credits can start because of any session on the account, so a long
//! call re-checks every 240 s, independent of its own transcript. Two
//! sources: `uw --json status` (usagewindow; local, no auth) and, when that
//! fails, recent main-session transcripts that moved from 1h writes to 5m
//! writes. A change of state ends the call with a `credit_state_changed`
//! event and runs an optional operator hook.
//!
//! RED phase: every body is a stub returning a neutral value.
//!
//! Round 2 (specs/auto-interval.md S1-S10): the verdict is tri-state (S5);
//! the last applied state and check time live in `<root>/credit-state.json`,
//! serialized across processes by `<root>/credit.lock` (S9); probes and the
//! hook are bounded and killed with their descendants (S7); every read is
//! bounded (S8).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::clock::Timestamp;
use crate::config::Config;
use crate::judge::BoxFuture;

/// A10: the `uw` call is abandoned (and the transcript fallback used) after this.
pub const UW_TIMEOUT: Duration = Duration::from_secs(5);
/// AI2' fallback: a transcript counts if modified within this many seconds.
pub const RECENT_TRANSCRIPT_S: u64 = 600;
/// A11: the hook's captured output is cut to this many bytes, marker included.
pub const MAX_HOOK_OUTPUT_BYTES: usize = 2048;
/// S8: `uw --json status` output is read up to this many bytes; a longer
/// output is a failed `uw` (the transcript fallback runs).
pub const MAX_UW_OUTPUT_BYTES: usize = 64 * 1024;
/// S8: the JSON the hook receives on stdin is at most this long.
pub const MAX_HOOK_STDIN_BYTES: usize = 4096;
/// S8: the transcript fallback looks at no more than this many files (newest first).
pub const MAX_FALLBACK_TRANSCRIPTS: usize = 20;
/// S7: budget margin kept back from the end of a call (the same 100 ms the
/// call loop keeps).
pub const CALL_MARGIN: Duration = Duration::from_millis(100);
/// S9: lock file serializing probe + hook across processes (`<root>/credit.lock`).
pub const CREDIT_LOCK_FILE: &str = "credit.lock";
/// S2/S9: the last applied state and the time of the last check
/// (`<root>/credit-state.json`).
pub const CREDIT_STATE_FILE: &str = "credit-state.json";

/// Where a credit verdict came from. Wire: `"uw"` / `"transcripts"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CreditSource {
    Uw,
    Transcripts,
}

/// One verdict. `windows` is the evidence: for `uw`, one `{window, exceeded}`
/// object per `ClaudeCode` window (A10); empty for the transcript source.
#[derive(Debug, Clone, PartialEq)]
pub struct CreditState {
    /// S5: `Some(true)` on credits, `Some(false)` not, `None` unknown (no
    /// source could say). Unknown keeps the last known state and never
    /// causes an event or a hook.
    pub on_credits: Option<bool>,
    pub source: CreditSource,
    pub windows: Vec<serde_json::Value>,
}

/// The envelope's `cache.credit_check` (AI2'): the newest check of a long call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreditCheck {
    pub at: Timestamp,
    pub source: CreditSource,
    /// S5: `null` on the wire when the check could not say.
    pub on_credits: Option<bool>,
}

/// S2/S9: `<root>/credit-state.json`. `on_credits` is the last KNOWN state the
/// hook was applied for (or the baseline); `None` until a known state has
/// been seen. `checked_at` is the time of the newest check, unknown ones
/// included; it drives the check cadence of every process on the root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreditStateFile {
    pub on_credits: Option<bool>,
    pub checked_at: Timestamp,
}

/// The seam the engine checks credit state through. Production:
/// `SystemCreditProbe`; tests plug in `testing::ScriptedCreditProbe`, because
/// a subprocess under paused tokio time lets virtual time race ahead of the
/// real I/O (the same reason `InputReader` exists).
pub trait CreditProbe: Send + Sync {
    /// One check at wall-clock `now`. Infallible: when no source can say, the
    /// answer is "not on credits" from the transcript source.
    fn check<'a>(&'a self, now: Timestamp) -> BoxFuture<'a, CreditState>;

    /// S7: the longest one `check` may take. The engine skips a check when
    /// the call has less than this (plus the margin) left.
    fn timeout(&self) -> Duration {
        UW_TIMEOUT
    }
}

/// S7/S8: runs the operator hook. Production: [`ProcessHookRunner`]; tests
/// plug in `testing::ScriptedHookRunner` for paused-time sequences (a real
/// `sh` under paused tokio time lets virtual time race ahead of its I/O).
pub trait CreditHookRunner: Send + Sync {
    /// Run `command` with `input` (already bounded, see [`hook_input`]) on
    /// stdin, for at most `timeout`. Dropping the future kills the hook's
    /// whole process group (S7).
    fn run<'a>(&'a self, command: &'a str, input: &'a serde_json::Value, timeout: Duration) -> BoxFuture<'a, HookOutcome>;
}

/// `sh -c` through [`run_credit_hook`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessHookRunner;

impl CreditHookRunner for ProcessHookRunner {
    fn run<'a>(&'a self, command: &'a str, input: &'a serde_json::Value, timeout: Duration) -> BoxFuture<'a, HookOutcome> {
        Box::pin(run_credit_hook(command, input, timeout))
    }
}

/// S8: the JSON object handed to the hook on stdin: `{on_credits, source, at,
/// windows}`, serialized at most `MAX_HOOK_STDIN_BYTES` long. When the
/// evidence does not fit, `windows` is cut to a prefix (order kept); the other
/// three keys are never dropped.
pub fn hook_input(state: &CreditState, at: Timestamp) -> serde_json::Value {
    let _ = (state, at);
    serde_json::Value::Null // RED
}

/// `uw --json status`, else recent transcripts (A10).
#[derive(Debug, Clone)]
pub struct SystemCreditProbe {
    pub uw_command: PathBuf,
    pub projects_dir: Option<PathBuf>,
    pub uw_timeout: Duration,
}

impl SystemCreditProbe {
    pub fn from_config(config: &Config) -> SystemCreditProbe {
        SystemCreditProbe {
            uw_command: config.uw_command.clone(),
            projects_dir: config.claude_projects_dir.clone(),
            uw_timeout: UW_TIMEOUT,
        }
    }
}

impl CreditProbe for SystemCreditProbe {
    fn check<'a>(&'a self, now: Timestamp) -> BoxFuture<'a, CreditState> {
        Box::pin(async move { CreditState { on_credits: Some(false), source: CreditSource::Transcripts, windows: Vec::new() } }) // RED
    }
}

/// Parse `uw --json status` (A10): only `usage[]` entries whose `provider` is
/// the string `"ClaudeCode"` count; any of their `windows[]` with
/// `exceeded: true` means on credits (`on_credits: Some(..)` always).
/// `None`: not JSON, or no `usage` array.
pub fn parse_uw_status(json: &str) -> Option<CreditState> {
    let _ = json;
    None // RED
}

/// The transcript fallback (A10): any `<projects_dir>/*/*.jsonl` (never
/// deeper, so never `subagents/`) modified within `RECENT_TRANSCRIPT_S` of
/// `now` whose newest cache write is 5m after an earlier 1h write. S8: regular
/// files only (symlinks and FIFOs are skipped, never opened), the newest
/// `MAX_FALLBACK_TRANSCRIPTS` by mtime, each read through its last 512 KiB.
/// S5: a projects dir that cannot be read at all is `on_credits: None`; a
/// readable one with no evidence is `Some(false)`.
pub fn transcript_credit_state(projects_dir: &Path, now: Timestamp) -> CreditState {
    let _ = (projects_dir, now);
    CreditState { on_credits: Some(false), source: CreditSource::Transcripts, windows: Vec::new() } // RED
}

/// What the hook did (AI8, A11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookOutcome {
    pub command: String,
    /// `None` when the hook timed out or could not be started.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// stdout then stderr, at most `MAX_HOOK_OUTPUT_BYTES`.
    pub output: String,
}

/// Run `sh -c <command>` with `state` as one JSON line on stdin. After
/// `timeout` the whole process group is killed. Never fails: every problem is
/// in the outcome.
pub async fn run_credit_hook(command: &str, state: &serde_json::Value, timeout: Duration) -> HookOutcome {
    let _ = (state, timeout);
    HookOutcome { command: command.to_string(), exit_code: None, timed_out: false, output: String::new() } // RED
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    use serde_json::json;

    /// `uw --json status` as captured on 2026-11-11 (trimmed to the keys vigil
    /// may read; key names and nesting are as printed). The Codex window is
    /// `exceeded: true` in the real capture, which is why the provider filter
    /// is load-bearing.
    fn uw_json(claude_exceeded: &[bool], codex_exceeded: bool) -> String {
        let windows: Vec<serde_json::Value> = claude_exceeded
            .iter()
            .map(|&exceeded| json!({"exceeded": exceeded, "pct": 24.0, "window": {"Rolling": {"minutes": 10080}}}))
            .collect();
        json!({
            "keepalive_active_count": 0,
            "last_updated": "2026-11-11T07:00:16.720545746Z",
            "provider_status": [
                {"provider": "ClaudeCode", "consecutive_failures": 0},
                {"provider": {"Other": "t3code"}, "consecutive_failures": 2943}
            ],
            "usage": [
                {"account": "user@example.com", "plan": "Max", "provider": "ClaudeCode", "windows": windows},
                {"account": null, "plan": "Plus", "provider": "Codex",
                 "windows": [{"exceeded": codex_exceeded, "pct": 47.0, "window": {"Rolling": {"minutes": 10080}}}]},
                {"account": null, "plan": null, "provider": {"Other": "t3code"}, "windows": [{"exceeded": true}]}
            ]
        })
        .to_string()
    }

    // ---- parse_uw_status (AI2' step 1) ----

    #[test]
    fn a_claude_code_window_that_is_exceeded_means_on_credits_and_is_kept_as_evidence() {
        // why: AI2' step 1: "any ClaudeCode window with exceeded: true".
        //      `windows` carries the evidence as {window, exceeded} (A10).
        // proves: CLAIM(AI2'), CLAIM(AI8)
        let state = parse_uw_status(&uw_json(&[true], false)).expect("valid uw output parses");
        assert_eq!(state.on_credits, Some(true));
        assert_eq!(state.source, CreditSource::Uw);
        assert_eq!(state.windows, vec![json!({"window": {"Rolling": {"minutes": 10080}}, "exceeded": true})]);
    }

    #[test]
    fn only_claude_code_windows_count_not_codex_or_other_providers() {
        // why: AI2': the real capture on this machine has the Codex window
        //      `exceeded: true` while Claude is fine; counting every provider
        //      would put the orchestrator "on credits" all day. A provider
        //      that is an object ({"Other": ...}) must not break the parse.
        //      Positive control: flip the Claude window and the same shape
        //      reads true.
        // proves: CLAIM(AI2')
        let fine = parse_uw_status(&uw_json(&[false], true)).expect("valid");
        assert_eq!(fine.on_credits, Some(false), "Codex and t3code windows are exceeded in this fixture");
        assert_eq!(fine.windows, vec![json!({"window": {"Rolling": {"minutes": 10080}}, "exceeded": false})]);
        assert_eq!(parse_uw_status(&uw_json(&[true], true)).expect("valid uw output parses").on_credits, Some(true));
    }

    #[test]
    fn any_one_exceeded_claude_window_among_several_is_enough() {
        // why: AI2' "any ClaudeCode window": a weekly window can be exceeded
        //      while the 5-hour one is not. Three windows, middle one true.
        // proves: CLAIM(AI2')
        let state = parse_uw_status(&uw_json(&[false, true, false], false)).expect("valid uw output parses");
        assert_eq!(state.on_credits, Some(true));
        assert_eq!(state.windows.len(), 3);
    }

    #[test]
    fn output_that_is_not_uw_status_is_rejected_so_the_fallback_runs() {
        // why: AI2' step 2 runs "when uw is missing or fails"; a uw that
        //      prints junk, an error object, or JSON without `usage` has
        //      failed. No `ClaudeCode` entry at all is NOT a failure: it is a
        //      valid "not on credits" (the account has no Claude windows).
        // proves: CLAIM(AI2')
        for bad in ["", "not json", "{}", r#"{"usage": 5}"#, r#"{"error": "no providers"}"#, "[]"] {
            assert_eq!(parse_uw_status(bad), None, "input {bad:?}");
        }
        let none_for_claude = parse_uw_status(r#"{"usage": []}"#).expect("an empty usage list is valid");
        assert_eq!(none_for_claude.on_credits, Some(false));
        assert!(none_for_claude.windows.is_empty());
    }

    // ---- transcript_credit_state (AI2' step 2) ----

    /// A main-session transcript last modified `age_s` before `spec_instant()`.
    fn fresh(root: &Path, project: &str, session: &str, lines: &[String], age_s: u64) -> PathBuf {
        let path = write_transcript(root, project, session, lines);
        set_age(&path, spec_instant(), age_s);
        path
    }

    fn shifted() -> Vec<String> {
        vec![assistant_line(900, 0), assistant_line(0, 0), assistant_line(900, 0), assistant_line(0, 500)]
    }

    #[test]
    fn a_recent_main_transcript_that_fell_from_1h_to_5m_means_on_credits() {
        // why: AI2' step 2: "newest cache write is 5m after earlier 1h
        //      writes". Under the 1h pin a 5m write only happens on credits
        //      (AI8's verified mechanism).
        // proves: CLAIM(AI2')
        let root = tempfile::tempdir().unwrap();
        fresh(root.path(), "p", "S1", &shifted(), 60);
        let state = transcript_credit_state(root.path(), spec_instant());
        assert_eq!(state.on_credits, Some(true));
        assert_eq!(state.source, CreditSource::Transcripts);
    }

    #[test]
    fn a_transcript_must_show_the_shift_to_count() {
        // why: AI2': a session that only ever wrote 5m (never pinned, or a
        //      Codex-like client) is not evidence; neither is one that went
        //      5m -> 1h (credits ended). Three fixtures, all fresh, none
        //      shifted down: only-5m, 5m then 1h, only-1h.
        // proves: CLAIM(AI2')
        let root = tempfile::tempdir().unwrap();
        fresh(root.path(), "p", "only5m", &[assistant_line(0, 500), assistant_line(0, 600)], 30);
        fresh(root.path(), "p", "recovered", &[assistant_line(0, 500), assistant_line(900, 0)], 30);
        fresh(root.path(), "p", "only1h", &[assistant_line(900, 0), assistant_line(900, 0)], 30);
        assert_eq!(transcript_credit_state(root.path(), spec_instant()).on_credits, Some(false));
        // Positive control: one real shift among them flips the verdict.
        fresh(root.path(), "p", "shifted", &shifted(), 30);
        assert_eq!(transcript_credit_state(root.path(), spec_instant()).on_credits, Some(true));
    }

    #[test]
    fn only_transcripts_modified_in_the_last_ten_minutes_count() {
        // why: AI2' "modified in the last 10 minutes": an old session that
        //      once dropped to 5m says nothing about now. Ages 599 s (counts)
        //      and 601 s (does not) bracket the 600 s line (A10: <= 600 s).
        // proves: CLAIM(AI2')
        let root = tempfile::tempdir().unwrap();
        let path = fresh(root.path(), "p", "S1", &shifted(), 599);
        assert_eq!(transcript_credit_state(root.path(), spec_instant()).on_credits, Some(true), "599 s old");
        set_age(&path, spec_instant(), 601);
        assert_eq!(transcript_credit_state(root.path(), spec_instant()).on_credits, Some(false), "601 s old");
    }

    #[test]
    fn a_subagent_transcript_never_counts() {
        // why: AI2' "(not under subagents/)": subagents use 5m caches always,
        //      so their transcripts look exactly like a shift. The same
        //      content at the main-session path is the positive control in
        //      this test, so the location is what decides.
        // proves: CLAIM(AI2')
        let root = tempfile::tempdir().unwrap();
        let sub = write_subagent_transcript(root.path(), "p", "S1", "agent-1", &shifted());
        set_age(&sub, spec_instant(), 30);
        assert_eq!(transcript_credit_state(root.path(), spec_instant()).on_credits, Some(false), "subagent only");
        fresh(root.path(), "p", "S1", &shifted(), 30);
        assert_eq!(transcript_credit_state(root.path(), spec_instant()).on_credits, Some(true), "same content, main path");
    }

    #[test]
    fn unreadable_or_empty_transcripts_are_ignored() {
        // why: the fallback is the last resort; a junk file and an empty file
        //      answer "not on credits"; a projects dir that cannot be read at
        //      all answers UNKNOWN (S5: no evidence either way is not "no"),
        //      none of them may panic or error.
        // proves: CLAIM(AI2')
        let root = tempfile::tempdir().unwrap();
        fresh(root.path(), "p", "junk", &["not json".to_string()], 10);
        fresh(root.path(), "p", "empty", &[], 10);
        assert_eq!(transcript_credit_state(root.path(), spec_instant()).on_credits, Some(false));
        assert_eq!(transcript_credit_state(&root.path().join("missing"), spec_instant()).on_credits, None, "S5: a projects dir that cannot be read says unknown, not no");
        // Positive control: junk beside a real shifted transcript does not hide it.
        fresh(root.path(), "p", "real", &shifted(), 10);
        assert_eq!(transcript_credit_state(root.path(), spec_instant()).on_credits, Some(true));
    }

    // ---- SystemCreditProbe (real subprocesses, unpaused) ----

    fn probe(uw: &Path, projects: &Path) -> SystemCreditProbe {
        SystemCreditProbe { uw_command: uw.to_path_buf(), projects_dir: Some(projects.to_path_buf()), uw_timeout: Duration::from_millis(500) }
    }

    fn fake_uw(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("uw");
        write_script(&path, body);
        path
    }

    #[tokio::test]
    async fn the_probe_runs_uw_json_status_and_trusts_it() {
        // why: AI2' step 1. The fake `uw` records its arguments ("--json
        //      status", exactly) and prints exceeded:true. Source is `uw`.
        //      A second fake prints exceeded:false while a fresh transcript
        //      says credits: uw wins when it works (the transcript is only a
        //      fallback).
        // proves: CLAIM(AI2')
        let dir = tempfile::tempdir().unwrap();
        let args = dir.path().join("args");
        let uw = fake_uw(dir.path(), &format!("echo \"$@\" > '{}'\ncat <<'EOF'\n{}\nEOF", args.display(), uw_json(&[true], false)));
        let projects = tempfile::tempdir().unwrap();
        let state = probe(&uw, projects.path()).check(spec_instant()).await;
        assert_eq!(state.on_credits, Some(true));
        assert_eq!(state.source, CreditSource::Uw);
        assert_eq!(std::fs::read_to_string(&args).unwrap().trim(), "--json status");

        fresh(projects.path(), "p", "S1", &shifted(), 30);
        let uw_off = fake_uw(dir.path(), &format!("cat <<'EOF'\n{}\nEOF", uw_json(&[false], false)));
        let state = probe(&uw_off, projects.path()).check(spec_instant()).await;
        assert_eq!(state.on_credits, Some(false), "uw says no, the transcript must not override it");
        assert_eq!(state.source, CreditSource::Uw);
    }

    #[tokio::test]
    async fn when_uw_is_missing_fails_prints_junk_or_hangs_the_transcripts_decide() {
        // why: AI2' step 2 "when uw is missing or fails". Four failure modes,
        //      each over the same fresh shifted transcript: no such binary,
        //      exit 1, exit 0 with junk, and a hang (uw_timeout 500 ms). All
        //      must end in source `transcripts`, on_credits true, and the
        //      hang must not block the check for its own length (30 s).
        // proves: CLAIM(AI2')
        let dir = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        fresh(projects.path(), "p", "S1", &shifted(), 30);
        let missing = dir.path().join("no-such-uw");
        let fails = fake_uw(dir.path(), "echo oops >&2\nexit 1");
        let junk = {
            let sub = dir.path().join("junk");
            std::fs::create_dir_all(&sub).unwrap();
            fake_uw(&sub, "echo 'not json'")
        };
        let hangs = {
            let sub = dir.path().join("hang");
            std::fs::create_dir_all(&sub).unwrap();
            fake_uw(&sub, "sleep 30")
        };
        for (name, uw) in [("missing", missing), ("exit 1", fails), ("junk", junk), ("hang", hangs)] {
            let started = std::time::Instant::now();
            let state = probe(&uw, projects.path()).check(spec_instant()).await;
            assert!(started.elapsed() < Duration::from_secs(5), "{name}: waited {:?}", started.elapsed());
            assert_eq!(state.source, CreditSource::Transcripts, "{name}");
            assert_eq!(state.on_credits, Some(true), "{name}");
        }
    }

    // ---- run_credit_hook (AI8) ----

    fn hook_state() -> serde_json::Value {
        json!({"on_credits": true, "source": "uw", "at": "2026-10-29T14:07:11Z", "windows": [{"exceeded": true}]})
    }

    #[tokio::test]
    async fn the_hook_gets_the_state_as_json_on_stdin() {
        // why: AI8 "with the new state as JSON on stdin": the default hook
        //      (claude-cache-ttl-policy) parses stdin with jq. `cat > out`
        //      captures exactly what the hook was fed; it must parse to the
        //      state, key for key.
        // proves: CLAIM(AI8)
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("stdin.json");
        let outcome = run_credit_hook(&format!("cat > '{}'", out.display()), &hook_state(), Duration::from_secs(10)).await;
        assert_eq!(outcome.exit_code, Some(0));
        assert!(!outcome.timed_out);
        let seen: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(seen, hook_state());
    }

    #[tokio::test]
    async fn the_exit_code_and_both_output_streams_are_captured_and_a_failing_hook_is_not_an_error() {
        // why: AI8 "output and exit code captured in the event". Exit 3 with
        //      text on stdout and on stderr; and a command that does not exist
        //      (sh reports 127). Neither may panic or hide the outcome.
        // proves: CLAIM(AI8)
        let outcome = run_credit_hook("echo out-text; echo err-text >&2; exit 3", &hook_state(), Duration::from_secs(10)).await;
        assert_eq!(outcome.exit_code, Some(3));
        assert!(outcome.output.contains("out-text") && outcome.output.contains("err-text"), "{:?}", outcome.output);
        assert_eq!(outcome.command, "echo out-text; echo err-text >&2; exit 3");
        let missing = run_credit_hook("/nonexistent/vigil-hook", &hook_state(), Duration::from_secs(10)).await;
        assert_eq!(missing.exit_code, Some(127));
    }

    #[tokio::test]
    async fn a_hook_that_outlives_its_timeout_is_killed_with_its_children() {
        // why: AI8 "10 s timeout" (shortened to 300 ms here; the 10 s default
        //      is pinned in config). The hook backgrounds a `sleep 60` and
        //      waits: killing only `sh` would leave the sleeper behind (the
        //      C9 lesson for cmd inputs). Outcome: timed_out, no exit code,
        //      returned well inside 5 s, and the sleeper's pid is dead.
        // proves: CLAIM(AI8)
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let command = format!("sleep 60 & echo $! > '{}'; wait", pid_file.display());
        let started = std::time::Instant::now();
        let outcome = run_credit_hook(&command, &hook_state(), Duration::from_millis(300)).await;
        assert!(started.elapsed() < Duration::from_secs(5), "waited {:?}", started.elapsed());
        assert!(outcome.timed_out);
        assert_eq!(outcome.exit_code, None);
        let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        let mut dead = false;
        for _ in 0..40 {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok();
            dead = match stat {
                None => true,
                Some(line) => line[line.rfind(')').unwrap() + 1..].trim_start().starts_with('Z'),
            };
            if dead {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(dead, "the hook's background child {pid} survived the timeout");
    }

    #[tokio::test]
    async fn hook_output_is_cut_to_the_cap() {
        // why: A11: the output rides in the event, which rides in the agent's
        //      context. 100 000 bytes of 'a' must come back as at most
        //      MAX_HOOK_OUTPUT_BYTES (2048), not empty and not whole.
        // proves: CLAIM(AI8)
        let outcome = run_credit_hook("head -c 100000 /dev/zero | tr '\\0' 'a'", &hook_state(), Duration::from_secs(10)).await;
        assert_eq!(outcome.exit_code, Some(0));
        assert!(outcome.output.contains('a'));
        assert!(outcome.output.len() <= MAX_HOOK_OUTPUT_BYTES, "{} bytes", outcome.output.len());
        // S8: "test against the literal": the cap is 2048 bytes, and 100 000
        // bytes of output fill it exactly (ASCII, so no character is split).
        assert_eq!(outcome.output.len(), 2048, "{} bytes", outcome.output.len());
    }

    #[test]
    fn the_credit_check_has_the_wire_shape_of_ai2() {
        // why: AI2' `cache.credit_check: {at, source, on_credits}` as literal
        //      JSON, with the whole-second `Z` timestamp.
        // proves: CLAIM(AI2')
        let check = CreditCheck { at: at("2026-10-29T14:07:11Z"), source: CreditSource::Uw, on_credits: Some(true) };
        assert_eq!(serde_json::to_string(&check).unwrap(), r#"{"at":"2026-10-29T14:07:11Z","source":"uw","on_credits":true}"#);
        let check = CreditCheck { at: at("2026-10-29T14:07:11Z"), source: CreditSource::Transcripts, on_credits: Some(false) };
        assert_eq!(serde_json::to_string(&check).unwrap(), r#"{"at":"2026-10-29T14:07:11Z","source":"transcripts","on_credits":false}"#);
    }

    // ---- round 2: helpers ----

    fn pid_is_gone(pid: u32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")).ok() {
            None => true,
            Some(line) => line[line.rfind(')').unwrap() + 1..].trim_start().starts_with('Z'),
        }
    }

    async fn gone_within_2_s(pid: u32) -> bool {
        for _ in 0..40 {
            if pid_is_gone(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// Wait (real time, at most 5 s) for a script to write its child's pid.
    async fn pid_written_to(pid_file: &Path) -> Option<u32> {
        for _ in 0..100 {
            if let Some(pid) = std::fs::read_to_string(pid_file).ok().and_then(|text| text.trim().parse().ok()) {
                return Some(pid);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        None
    }

    fn within_5_s<T: Send + 'static>(what: &str, work: impl FnOnce() -> T + Send + 'static) -> T {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(work());
        });
        receiver.recv_timeout(Duration::from_secs(5)).unwrap_or_else(|_| panic!("{what} did not return in 5 s"))
    }

    // ---- S5: unknown is a third answer ----

    #[tokio::test]
    async fn when_neither_uw_nor_the_transcripts_can_answer_the_state_is_unknown_not_off() {
        // why: S5. `uw` is missing AND there is no projects dir to read (none
        //      configured, or the path does not exist): vigil has no evidence.
        //      Reporting "off" would make the engine flip a known "on" to off
        //      and run the hook that restores the 1h TTL while credits are
        //      still being spent. The answer must be None, from the transcript
        //      source (the last one tried). Control: a readable but empty
        //      projects dir is evidence of nothing shifting: Some(false).
        // proves: CLAIM(S5)
        let dir = tempfile::tempdir().unwrap();
        let missing_uw = dir.path().join("no-such-uw");
        let empty = tempfile::tempdir().unwrap();
        let control = probe(&missing_uw, empty.path()).check(spec_instant()).await;
        assert_eq!((control.on_credits, control.source), (Some(false), CreditSource::Transcripts), "control: readable and empty");

        let nowhere = SystemCreditProbe { uw_command: missing_uw.clone(), projects_dir: Some(dir.path().join("no-projects")), uw_timeout: Duration::from_millis(500) };
        let state = nowhere.check(spec_instant()).await;
        assert_eq!((state.on_credits, state.source), (None, CreditSource::Transcripts), "projects dir does not exist");
        let unset = SystemCreditProbe { uw_command: missing_uw, projects_dir: None, uw_timeout: Duration::from_millis(500) };
        assert_eq!(unset.check(spec_instant()).await.on_credits, None, "no projects dir configured");
    }

    #[test]
    fn an_unknown_check_has_a_null_on_credits_on_the_wire() {
        // why: S5 wire: `cache.credit_check.on_credits` is true / false / null.
        //      Pinned as literal JSON, with `credit-state.json`'s shape beside
        //      it (S2/S9): both are written by one process and read by another.
        //      [g] passes against the stub: a data pin, not behaviour.
        // proves: CLAIM(S5), CLAIM(S9)
        let check = CreditCheck { at: at("2026-10-29T14:07:11Z"), source: CreditSource::Transcripts, on_credits: None };
        assert_eq!(serde_json::to_string(&check).unwrap(), r#"{"at":"2026-10-29T14:07:11Z","source":"transcripts","on_credits":null}"#);
        let file = CreditStateFile { on_credits: Some(false), checked_at: at("2026-10-29T14:07:11Z") };
        assert_eq!(serde_json::to_string(&file).unwrap(), r#"{"on_credits":false,"checked_at":"2026-10-29T14:07:11Z"}"#);
        let never_known = CreditStateFile { on_credits: None, checked_at: at("2026-10-29T14:07:11Z") };
        assert_eq!(serde_json::to_string(&never_known).unwrap(), r#"{"on_credits":null,"checked_at":"2026-10-29T14:07:11Z"}"#);
        assert_eq!(serde_json::from_str::<CreditStateFile>(r#"{"on_credits":true,"checked_at":"2026-10-29T14:07:11Z"}"#).unwrap().on_credits, Some(true));
    }

    // ---- S7: cancel and timeout kill descendants ----

    #[tokio::test]
    async fn a_uw_that_times_out_is_killed_with_its_children() {
        // why: S7 + A22 (C9 lesson). The fake `uw` backgrounds a `sleep 60`,
        //      records its pid and waits; killing only `sh` leaves the sleeper
        //      alive for a minute after every timed-out check. uw_timeout is
        //      500 ms; the check falls back to the transcripts and returns, and
        //      the sleeper's pid is gone.
        // proves: CLAIM(S7-kill)
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let uw = fake_uw(dir.path(), &format!("sleep 60 &\necho $! > '{}'\nwait", pid_file.display()));
        let projects = tempfile::tempdir().unwrap();
        let state = probe(&uw, projects.path()).check(spec_instant()).await;
        assert_eq!(state.source, CreditSource::Transcripts, "the hung uw is abandoned");
        let pid = pid_written_to(&pid_file).await.expect("uw was run");
        assert!(gone_within_2_s(pid).await, "uw's background child {pid} survived the timeout");
    }

    #[tokio::test]
    async fn dropping_a_check_in_flight_kills_uw_and_its_children() {
        // why: S7 "cancel during probe". The call (MCP cancel, SIGTERM) drops
        //      the check future while `uw` is running. The timeout is 30 s, so
        //      only the drop can end it. The fake uw's background child must be
        //      gone shortly after.
        // proves: CLAIM(S7-kill)
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let uw = fake_uw(dir.path(), &format!("sleep 60 &\necho $! > '{}'\nwait", pid_file.display()));
        let projects = tempfile::tempdir().unwrap();
        let slow = SystemCreditProbe { uw_command: uw, projects_dir: Some(projects.path().to_path_buf()), uw_timeout: Duration::from_secs(30) };
        let check = slow.check(spec_instant());
        tokio::pin!(check);
        let pid = tokio::select! {
            _ = &mut check => panic!("the check returned before uw could be cancelled"),
            pid = pid_written_to(&pid_file) => pid.expect("uw was run"),
        };
        drop(check);
        assert!(gone_within_2_s(pid).await, "uw's background child {pid} survived the cancel");
    }

    #[tokio::test]
    async fn dropping_a_hook_in_flight_kills_its_whole_process_group() {
        // why: S7 "cancel during hook". Same shape, for run_credit_hook with a
        //      30 s timeout: the future is dropped while the hook waits on its
        //      background child.
        // proves: CLAIM(S7-kill)
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let command = format!("sleep 60 & echo $! > '{}'; wait", pid_file.display());
        let run = run_credit_hook(&command, &hook_state(), Duration::from_secs(30));
        tokio::pin!(run);
        let pid = tokio::select! {
            _ = &mut run => panic!("the hook returned before it could be cancelled"),
            pid = pid_written_to(&pid_file) => pid.expect("the hook was run"),
        };
        drop(run);
        assert!(gone_within_2_s(pid).await, "the hook's background child {pid} survived the cancel");
    }

    // ---- S8: bounded reads ----

    fn json_of_len(total: usize, json: &str) -> String {
        assert!(total >= json.len());
        format!("{}{json}", " ".repeat(total - json.len()))
    }

    #[tokio::test]
    async fn uw_output_is_read_up_to_64_kib_and_no_further() {
        // why: S8 "uw output capped at 64 KiB". The cap is proven at its edge:
        //      the real JSON is preceded by blank padding so that it ENDS at byte
        //      65 536 (accepted: source uw, on credits) or at byte 65 537 (its
        //      last `}` is past the cap, so the read is cut inside the JSON and
        //      the output is junk: source transcripts). A 1 MiB output is
        //      junk too. An implementation that reads everything accepts all
        //      three as uw.
        // proves: CLAIM(S8-uw)
        let dir = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let json = uw_json(&[true], false);
        let mut sources = Vec::new();
        for (total, name) in [(65_536, "exactly 64 KiB"), (65_537, "one byte over"), (1 << 20, "1 MiB")] {
            let data = dir.path().join(format!("out-{total}.json"));
            std::fs::write(&data, json_of_len(total, &json)).unwrap();
            let sub = dir.path().join(format!("d{total}"));
            std::fs::create_dir_all(&sub).unwrap();
            let uw = fake_uw(&sub, &format!("cat '{}'", data.display()));
            let state = SystemCreditProbe { uw_command: uw, projects_dir: Some(projects.path().to_path_buf()), uw_timeout: Duration::from_secs(5) }
                .check(spec_instant())
                .await;
            sources.push((name, state.source));
        }
        assert_eq!(
            sources,
            vec![("exactly 64 KiB", CreditSource::Uw), ("one byte over", CreditSource::Transcripts), ("1 MiB", CreditSource::Transcripts)]
        );
    }

    #[test]
    fn the_hook_stdin_is_one_object_of_at_most_4_kib_and_cuts_the_windows_not_the_verdict() {
        // why: S8 "hook stdin <= 4 KiB". `uw` can list many windows and the hook
        //      reads stdin with jq. (1) A small state passes through whole:
        //      on_credits, source, at, and every window (control). (2) 300
        //      windows (~56 bytes each = 16.8 KB) is cut: the object is still
        //      valid JSON of at most 4096 bytes, keeps on_credits / source / at
        //      untouched, and `windows` is a prefix of the originals (order
        //      kept) with at least 40 of them (4096 - ~110 bytes of wrapper =
        //      ~3980 / 56 = ~71 fit; 40 leaves room for a marker).
        // proves: CLAIM(S8-stdin)
        let small = credit_state_with_windows(true, 3);
        let whole = hook_input(&small, at("2026-10-29T14:07:11Z"));
        assert_eq!(whole["on_credits"], true);
        assert_eq!(whole["source"], "uw");
        assert_eq!(whole["at"], "2026-10-29T14:07:11Z");
        assert_eq!(whole["windows"], serde_json::Value::Array(small.windows.clone()));

        let big = credit_state_with_windows(true, 300);
        let cut = hook_input(&big, at("2026-10-29T14:07:11Z"));
        let text = cut.to_string();
        assert!(text.len() <= 4096, "{} bytes", text.len());
        assert_eq!((&cut["on_credits"], &cut["source"], &cut["at"]), (&json!(true), &json!("uw"), &json!("2026-10-29T14:07:11Z")));
        let kept = cut["windows"].as_array().expect("windows stays an array");
        assert!(kept.len() >= 40 && kept.len() < 300, "kept {} windows", kept.len());
        assert_eq!(kept.as_slice(), &big.windows[..kept.len()], "a prefix, in order");
    }

    #[test]
    fn at_most_the_20_newest_main_transcripts_are_looked_at() {
        // why: S8 "fallback scans at most 20 newest main-session transcripts". The
        //      one shifted transcript (a real 1h -> 5m drop) is the 20th newest
        //      of 20 (19 plain 5m-only files 1..19 s old, shifted 20 s old):
        //      counted. Pushed to 21st (20 plain files 1..20 s old, shifted
        //      21 s old): not looked at, so not on credits. A scan of every
        //      file finds it in both layouts.
        // proves: CLAIM(S8-scan)
        let plain = || vec![assistant_line(0, 500)];
        let layout = |plain_files: u64, shifted_age: u64| {
            let root = tempfile::tempdir().unwrap();
            for age in 1..=plain_files {
                fresh(root.path(), "p", &format!("plain-{age:02}"), &plain(), age);
            }
            fresh(root.path(), "p", "shifted", &shifted(), shifted_age);
            transcript_credit_state(root.path(), spec_instant()).on_credits
        };
        assert_eq!(layout(19, 20), Some(true), "20th newest of 20 is scanned");
        assert_eq!(layout(20, 21), Some(false), "21st newest is not");
    }

    #[test]
    fn a_symlinked_or_fifo_transcript_never_counts_for_the_fallback() {
        // why: S8 "regular files only, symlinks not followed" for the fallback
        //      scan too. A recent shifted transcript reached only through a
        //      symlink (file link, and a project dir link) is ignored; so is a
        //      FIFO named like a transcript, which must not block the scan
        //      (bounded to 5 s). Control: the same content as a regular file
        //      flips the verdict.
        // proves: CLAIM(S8-symlink)
        let outside = tempfile::tempdir().unwrap();
        let real = fresh(outside.path(), "x", "real", &shifted(), 10);
        let real_dir = fresh(outside.path(), "d", "inside", &shifted(), 10);
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("p")).unwrap();
        std::os::unix::fs::symlink(&real, root.path().join("p/linked.jsonl")).unwrap();
        std::os::unix::fs::symlink(real_dir.parent().unwrap(), root.path().join("linked-project")).unwrap();
        let fifo = root.path().join("p/fifo.jsonl");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().expect("mkfifo is available").success());
        let dir = root.path().to_path_buf();
        let state = within_5_s("the fallback scan", move || transcript_credit_state(&dir, spec_instant()));
        assert_eq!(state.on_credits, Some(false), "links and FIFOs are not transcripts");
        fresh(root.path(), "p", "regular", &shifted(), 10);
        assert_eq!(transcript_credit_state(root.path(), spec_instant()).on_credits, Some(true), "control");
    }

    #[test]
    fn the_fallback_reads_only_the_last_512_kib_of_each_transcript() {
        // why: S8 + D4 for the fallback. A recent transcript holds the 1h write
        //      700 lines (~700 KB) before its final 5m write: the shift is
        //      outside the 512 KiB tail, so it is not seen (off). Control: the
        //      same file with only 100 filler lines (~100 KB) between them
        //      shows the shift (on), so the "off" is the bound.
        // proves: CLAIM(S8-tail)
        let filler = |n: usize| -> Vec<String> { (0..n).map(|i| user_line_quoting(&format!("filler {i} {}", "x".repeat(900)))).collect() };
        let build = |fillers: usize| {
            let root = tempfile::tempdir().unwrap();
            let mut lines = vec![assistant_line(900, 0)];
            lines.extend(filler(fillers));
            lines.push(assistant_line(0, 500));
            fresh(root.path(), "p", "S1", &lines, 10);
            transcript_credit_state(root.path(), spec_instant()).on_credits
        };
        assert_eq!(build(100), Some(true), "control: the 1h write is inside the tail");
        assert_eq!(build(700), Some(false), "the 1h write is beyond 512 KiB");
    }
}
