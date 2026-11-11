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
    pub on_credits: bool,
    pub source: CreditSource,
    pub windows: Vec<serde_json::Value>,
}

/// The envelope's `cache.credit_check` (AI2'): the newest check of a long call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreditCheck {
    pub at: Timestamp,
    pub source: CreditSource,
    pub on_credits: bool,
}

/// The seam the engine checks credit state through. Production:
/// `SystemCreditProbe`; tests plug in `testing::ScriptedCreditProbe`, because
/// a subprocess under paused tokio time lets virtual time race ahead of the
/// real I/O (the same reason `InputReader` exists).
pub trait CreditProbe: Send + Sync {
    /// One check at wall-clock `now`. Infallible: when no source can say, the
    /// answer is "not on credits" from the transcript source.
    fn check<'a>(&'a self, now: Timestamp) -> BoxFuture<'a, CreditState>;
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
        Box::pin(async move { CreditState { on_credits: false, source: CreditSource::Transcripts, windows: Vec::new() } }) // RED
    }
}

/// Parse `uw --json status` (A10): only `usage[]` entries whose `provider` is
/// the string `"ClaudeCode"` count; any of their `windows[]` with
/// `exceeded: true` means on credits. `None`: not JSON, or no `usage` array.
pub fn parse_uw_status(json: &str) -> Option<CreditState> {
    let _ = json;
    None // RED
}

/// The transcript fallback (A10): any `<projects_dir>/*/*.jsonl` (never
/// deeper, so never `subagents/`) modified within `RECENT_TRANSCRIPT_S` of
/// `now` whose newest cache write is 5m after an earlier 1h write.
pub fn transcript_credit_state(projects_dir: &Path, now: Timestamp) -> CreditState {
    let _ = (projects_dir, now);
    CreditState { on_credits: false, source: CreditSource::Transcripts, windows: Vec::new() } // RED
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
        assert!(state.on_credits);
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
        assert!(!fine.on_credits, "Codex and t3code windows are exceeded in this fixture");
        assert_eq!(fine.windows, vec![json!({"window": {"Rolling": {"minutes": 10080}}, "exceeded": false})]);
        assert!(parse_uw_status(&uw_json(&[true], true)).expect("valid uw output parses").on_credits);
    }

    #[test]
    fn any_one_exceeded_claude_window_among_several_is_enough() {
        // why: AI2' "any ClaudeCode window": a weekly window can be exceeded
        //      while the 5-hour one is not. Three windows, middle one true.
        // proves: CLAIM(AI2')
        let state = parse_uw_status(&uw_json(&[false, true, false], false)).expect("valid uw output parses");
        assert!(state.on_credits);
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
        assert!(!none_for_claude.on_credits);
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
        assert!(state.on_credits);
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
        assert!(!transcript_credit_state(root.path(), spec_instant()).on_credits);
        // Positive control: one real shift among them flips the verdict.
        fresh(root.path(), "p", "shifted", &shifted(), 30);
        assert!(transcript_credit_state(root.path(), spec_instant()).on_credits);
    }

    #[test]
    fn only_transcripts_modified_in_the_last_ten_minutes_count() {
        // why: AI2' "modified in the last 10 minutes": an old session that
        //      once dropped to 5m says nothing about now. Ages 599 s (counts)
        //      and 601 s (does not) bracket the 600 s line (A10: <= 600 s).
        // proves: CLAIM(AI2')
        let root = tempfile::tempdir().unwrap();
        let path = fresh(root.path(), "p", "S1", &shifted(), 599);
        assert!(transcript_credit_state(root.path(), spec_instant()).on_credits, "599 s old");
        set_age(&path, spec_instant(), 601);
        assert!(!transcript_credit_state(root.path(), spec_instant()).on_credits, "601 s old");
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
        assert!(!transcript_credit_state(root.path(), spec_instant()).on_credits, "subagent only");
        fresh(root.path(), "p", "S1", &shifted(), 30);
        assert!(transcript_credit_state(root.path(), spec_instant()).on_credits, "same content, main path");
    }

    #[test]
    fn unreadable_or_empty_transcripts_are_ignored() {
        // why: the fallback is the last resort; a junk file, an empty file and
        //      a missing projects dir must each answer "not on credits", not
        //      panic or error.
        // proves: CLAIM(AI2')
        let root = tempfile::tempdir().unwrap();
        fresh(root.path(), "p", "junk", &["not json".to_string()], 10);
        fresh(root.path(), "p", "empty", &[], 10);
        assert!(!transcript_credit_state(root.path(), spec_instant()).on_credits);
        assert!(!transcript_credit_state(&root.path().join("missing"), spec_instant()).on_credits);
        // Positive control: junk beside a real shifted transcript does not hide it.
        fresh(root.path(), "p", "real", &shifted(), 10);
        assert!(transcript_credit_state(root.path(), spec_instant()).on_credits);
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
        assert!(state.on_credits);
        assert_eq!(state.source, CreditSource::Uw);
        assert_eq!(std::fs::read_to_string(&args).unwrap().trim(), "--json status");

        fresh(projects.path(), "p", "S1", &shifted(), 30);
        let uw_off = fake_uw(dir.path(), &format!("cat <<'EOF'\n{}\nEOF", uw_json(&[false], false)));
        let state = probe(&uw_off, projects.path()).check(spec_instant()).await;
        assert!(!state.on_credits, "uw says no, the transcript must not override it");
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
            assert!(state.on_credits, "{name}");
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
    }

    #[test]
    fn the_credit_check_has_the_wire_shape_of_ai2() {
        // why: AI2' `cache.credit_check: {at, source, on_credits}` as literal
        //      JSON, with the whole-second `Z` timestamp.
        // proves: CLAIM(AI2')
        let check = CreditCheck { at: at("2026-10-29T14:07:11Z"), source: CreditSource::Uw, on_credits: true };
        assert_eq!(serde_json::to_string(&check).unwrap(), r#"{"at":"2026-10-29T14:07:11Z","source":"uw","on_credits":true}"#);
        let check = CreditCheck { at: at("2026-10-29T14:07:11Z"), source: CreditSource::Transcripts, on_credits: false };
        assert_eq!(serde_json::to_string(&check).unwrap(), r#"{"at":"2026-10-29T14:07:11Z","source":"transcripts","on_credits":false}"#);
    }
}
