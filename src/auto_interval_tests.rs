//! Engine-level tests for specs/auto-interval.md (AI1'-AI6, AI8; AI7 is
//! removed). Cross-cutting, like `disk_full_tests.rs`: each test drives
//! `Vigil::keepalive` and reads the envelope as the agent sees it (JSON), so
//! the wire format is what is pinned, not the Rust types behind it.
//!
//! Seams (all injected, none of these tests touches the real `~/.claude` or
//! the real `uw`):
//! * transcript root: `Config::claude_projects_dir` -> a tempdir;
//! * agent: `Vigil::with_agent_env`;
//! * credit state: `Vigil::with_credit_probe` + `ScriptedCreditProbe` (and
//!   `Config::uw_command` pointed at a path that does not exist, so a missed
//!   injection fails loudly instead of running `uw`);
//! * time: `TokioClock` on paused tokio time (virtual seconds, instant), or
//!   the real clock for the few tests that must run a real hook process.
//!
//! RED phase: all of these fail against the stubs (no `cache`/`clock` fields,
//! no attention, no credit events), except the few marked "guard".
//!
//! Round 2 (spec review amendments S1-S10) adds, in this file: `wait` (S1, S3),
//! the automatic 1h -> credits -> 5m -> credits end -> 1h sequence and the
//! cadence kept in `credit-state.json` (S2, S9), identity (S4), tri-state (S5),
//! attention events as ordinary events (S6), bounded probes and hook (S7),
//! two processes on one root (S9) and no-id multi-watch calls (S10). The hook
//! is run through `ScriptedHookRunner` where time is paused, and as a real
//! `sh` (real clock, `credit_check_every` of 200 ms) where the process matters.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::Instant;

use crate::cache::AgentEnv;
use crate::clock::Timestamp;
use crate::config::Config;
use crate::credit::CreditSource;
use crate::fault::{FaultPlan, FaultRule, When, WriteKind};
use crate::keepalive::KeepaliveRequest;
use crate::testing::*;
use crate::watch::{CreateRequest, Input, WatchId};
use crate::{SystemClock, Vigil};

// ---------------------------------------------------------------- rig ----

struct Rig {
    env: Env,
    projects: tempfile::TempDir,
}

impl Rig {
    /// Paused-time rig: `TokioClock` starting at the spec instant.
    fn new() -> Rig {
        Rig::build(Arc::new(TokioClock::new(spec_instant())), |_| {})
    }

    fn build(clock: Arc<dyn crate::Clock>, adjust: impl FnOnce(&mut Config)) -> Rig {
        let projects = tempfile::tempdir().unwrap();
        let mut config = default_config();
        config.claude_projects_dir = Some(projects.path().to_path_buf());
        config.uw_command = PathBuf::from("/nonexistent/vigil-tests-must-not-run-uw");
        adjust(&mut config);
        Rig { env: Env::with_clock_and_config(clock, config), projects }
    }

    /// Real clock, for tests that run a real hook process (unpaused time).
    fn real_time(adjust: impl FnOnce(&mut Config)) -> Rig {
        Rig::build(Arc::new(SystemClock), adjust)
    }

    /// A fresh "process" over the rig's state root with the given agent and probe.
    fn vigil(&self, agent: AgentEnv, probe: &Arc<ScriptedCreditProbe>) -> Vigil {
        self.env.another_process().with_agent_env(agent).with_credit_probe(probe.clone())
    }

    /// `vigil` with a scripted hook runner (paused-time hook sequences).
    fn vigil_with_hook(&self, agent: AgentEnv, probe: &Arc<ScriptedCreditProbe>, hook: &Arc<ScriptedHookRunner>) -> Vigil {
        self.vigil(agent, probe).with_hook_runner(hook.clone())
    }

    /// `<root>/credit-state.json` (S2/S9).
    fn credit_file(&self) -> PathBuf {
        self.env.root().join("credit-state.json")
    }

    /// Write `credit-state.json` as another process would have left it: the
    /// last applied state and a last check `age_s` before the rig's clock.
    fn seed_credit_file(&self, on_credits: Option<bool>, age_s: i64) {
        let checked_at = Timestamp(self.env.clock.now().0 - chrono::Duration::seconds(age_s));
        std::fs::create_dir_all(self.env.root()).unwrap();
        std::fs::write(self.credit_file(), json!({"on_credits": on_credits, "checked_at": checked_at}).to_string()).unwrap();
    }

    fn read_credit_file(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.credit_file()).expect("credit-state.json exists")).expect("credit-state.json is JSON")
    }

    /// The session transcript `sess-1` under project `p`.
    fn transcript(&self, lines: &[String]) -> PathBuf {
        write_transcript(self.projects.path(), "p", "sess-1", lines)
    }

    fn one_hour_cache(&self) -> PathBuf {
        self.transcript(&[assistant_line(0, 500), assistant_line(900, 0)])
    }
}

fn claude() -> AgentEnv {
    AgentEnv { claude_session_id: Some("sess-1".into()), codex: false }
}

fn codex() -> AgentEnv {
    AgentEnv { claude_session_id: None, codex: true }
}

fn quiet_probe() -> Arc<ScriptedCreditProbe> {
    ScriptedCreditProbe::new(credit_state(false, CreditSource::Uw))
}

async fn quiet_watch(rig: &Rig, vigil: &Vigil, id: &str, interval_s: u64, deadline_s: u64, tweak: impl FnOnce(&mut CreateRequest)) -> (WatchId, PathBuf) {
    let path = rig.env.input_path(&format!("{id}.log"));
    write_file(&path, "old\n");
    let mut request = change_request(id, &path, interval_s);
    request.deadline_s = Some(deadline_s);
    tweak(&mut request);
    (vigil.create(request).await.unwrap().watch_id, path)
}

/// A fire = judge watch (judge answers "not failed"), so changes do not end it.
async fn judge_watch(rig: &Rig, vigil: &Vigil, id: &str, interval_s: u64, deadline_s: u64, tweak: impl FnOnce(&mut CreateRequest)) -> (WatchId, PathBuf) {
    rig.env.judge.set_fallback(Ok(noul_answers("failed", 0.0, 1.0)));
    let path = rig.env.input_path(&format!("{id}.log"));
    write_file(&path, "old\n");
    let mut request = judge_request(id, &path, interval_s);
    request.deadline_s = Some(deadline_s);
    tweak(&mut request);
    (vigil.create(request).await.unwrap().watch_id, path)
}

async fn call(vigil: &Vigil, id: &WatchId, max_s: Option<u64>, orchestrator: Option<bool>) -> Value {
    let envelope = vigil.keepalive(KeepaliveRequest { watch_id: Some(id.clone()), max_s, orchestrator }).await.unwrap();
    serde_json::to_value(&envelope).unwrap()
}

/// A no-id call (covers this process's pending watches, V7).
async fn call_noid(vigil: &Vigil, max_s: Option<u64>, orchestrator: Option<bool>) -> Value {
    let envelope = vigil.keepalive(KeepaliveRequest { watch_id: None, max_s, orchestrator }).await.unwrap();
    serde_json::to_value(&envelope).unwrap()
}

fn keys(value: &Value) -> Vec<&str> {
    value.as_object().expect("an object").keys().map(String::as_str).collect()
}

/// A real child that stays alive until the test ends (a `process` input).
struct Sleeper(std::process::Child);

impl Sleeper {
    fn start() -> Sleeper {
        Sleeper(std::process::Command::new("sleep").arg("600").spawn().unwrap())
    }
    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn only_event(envelope: &Value) -> &Value {
    let events = envelope["events"].as_array().expect("events array");
    assert_eq!(events.len(), 1, "expected exactly one event: {envelope}");
    &events[0]
}

/// A call that ended early on an `attention` event: the watch is still
/// pending, `next` tells the agent to act (A17), and the call waited `waited`.
fn assert_attention(envelope: &Value, reason: &str, waited: u64) {
    assert_eq!(envelope["status"], "pending", "{envelope}");
    assert_eq!(envelope["next"], "act on events", "{envelope}");
    assert_eq!(envelope["waited_s"], waited, "{envelope}");
    let event = only_event(envelope);
    assert_eq!(event["kind"], "attention", "{event}");
    assert_eq!(event["reason"], reason, "{event}");
    let summary = event["summary"].as_str().expect("summary");
    assert!(!summary.is_empty() && summary.len() <= 200 && !summary.contains('\n'), "{summary:?}");
}

// ------------------------------------------------- AI1' / AI3: the window ----

#[tokio::test(start_paused = true)]
async fn an_orchestrator_on_a_one_hour_cache_waits_3300_s_and_says_why() {
    // why: AI1' + AI3: the point of the feature. The caller says it is the
    //      orchestrator (stored nowhere, passed as the flag), its transcript's
    //      newest write is 1h, so the call blocks 3300 s (3600 s TTL - 300 s
    //      margin, D1) and the envelope says what was chosen and why. Quiet
    //      watch, tick cadence 300 s, deadline 7200 s: after 3300 s the watch
    //      is 3300 s old (elapsed_s) with 3900 s left (7200 - 3300), still
    //      pending, `next` still the loop word. No `cold` key (AI7 removed).
    // proves: CLAIM(AI1'), CLAIM(AI3)
    let rig = Rig::new();
    rig.one_hour_cache();
    let probe = quiet_probe();
    let vigil = rig.vigil(claude(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 300, 7200, |_| {}).await;
    let started = Instant::now();
    let envelope = call(&vigil, &id, None, Some(true)).await;
    assert_eq!(started.elapsed(), Duration::from_secs(3300));
    assert_eq!((&envelope["waited_s"], &envelope["max_s"]), (&json!(3300), &json!(3300)));
    assert_eq!((&envelope["elapsed_s"], &envelope["deadline_in_s"]), (&json!(3300), &json!(3900)));
    assert_eq!((&envelope["status"], &envelope["next"]), (&json!("pending"), &json!("call keepalive again")));
    assert_eq!(envelope["cache"]["ttl"], "1h");
    assert_eq!(envelope["cache"]["source"], "claude-transcript");
    assert_eq!(envelope["cache"]["interval_s"], 3300);
    assert!(envelope.get("cold").is_none(), "AI7 is removed: no cold key, ever");
}

#[tokio::test(start_paused = true)]
async fn the_same_call_without_the_orchestrator_flag_waits_270_s_even_on_a_one_hour_cache() {
    // why: AI1' (Rev 2): "subagents therefore stay at 270 s with no special
    //      instruction": they share the parent's session id and transcript, so
    //      only the explicit flag may widen the window. Same fixture as the
    //      test above, flag absent -> 270 s and the exact AI3 object for a
    //      call that did not look at the cache: unknown / default / 270, and
    //      no `credit_check` (a short call never checks, A9).
    // proves: CLAIM(AI1'), CLAIM(AI3)
    let rig = Rig::new();
    rig.one_hour_cache();
    let probe = quiet_probe();
    let vigil = rig.vigil(claude(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 300, 7200, |_| {}).await;
    let started = Instant::now();
    let envelope = call(&vigil, &id, None, None).await;
    assert_eq!(started.elapsed(), Duration::from_secs(270));
    assert_eq!((&envelope["waited_s"], &envelope["max_s"]), (&json!(270), &json!(270)));
    assert_eq!(envelope["cache"], json!({"ttl": "unknown", "source": "default", "interval_s": 270}));
    assert_eq!(probe.asked_count(), 0, "a 270 s call makes no credit check");
}

#[tokio::test(start_paused = true)]
async fn an_orchestrator_on_a_five_minute_cache_keeps_270_s_and_reports_5m() {
    // why: AI1': 5 min -> 270 s. The newest write is 5m although an older one
    //      was 1h (the session fell to usage credits): the window must follow
    //      the newest, and the envelope must say 5m / claude-transcript.
    // proves: CLAIM(AI1'), CLAIM(AI3)
    let rig = Rig::new();
    rig.transcript(&[assistant_line(900, 0), assistant_line(0, 500)]);
    let probe = quiet_probe();
    let vigil = rig.vigil(claude(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 300, 7200, |_| {}).await;
    let started = Instant::now();
    let envelope = call(&vigil, &id, None, Some(true)).await;
    assert_eq!(started.elapsed(), Duration::from_secs(270));
    assert_eq!(envelope["cache"], json!({"ttl": "5m", "source": "claude-transcript", "interval_s": 270}));
}

#[tokio::test(start_paused = true)]
async fn the_window_is_decided_again_at_every_call() {
    // why: AI1' "detection runs at every call": the first call sees a 1h
    //      write (3300 s); then the session's cache setting flips (a 5m write
    //      lands in the transcript) and the very next call must be 270 s, or
    //      the cache goes cold under a 55 minute wait. The transcript is
    //      appended to between the calls; the Vigil value is the same.
    // proves: CLAIM(AI1')
    let rig = Rig::new();
    let transcript = rig.one_hour_cache();
    let probe = quiet_probe();
    let vigil = rig.vigil(claude(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 300, 20_000, |_| {}).await;
    let first = call(&vigil, &id, None, Some(true)).await;
    assert_eq!((&first["waited_s"], &first["cache"]["ttl"]), (&json!(3300), &json!("1h")));
    append_file(&transcript, &format!("{}\n", assistant_line(0, 700)));
    let second = call(&vigil, &id, None, Some(true)).await;
    assert_eq!((&second["waited_s"], &second["cache"]["ttl"]), (&json!(270), &json!("5m")));
}

#[tokio::test(start_paused = true)]
async fn usage_credits_do_not_change_the_window_only_the_observed_ttl_does() {
    // why: Revision 4: "the interval follows the TTL actually observed in
    //      the session's own transcript (AI1)", minus Revision 2's "later
    //      calls use 270 s": under the 1h pin the cache stays 1h on credits.
    //      The probe says on credits from the first check to the last; the
    //      transcript says 1h; the window stays 3300 s and the check is
    //      recorded in cache.credit_check as on_credits true.
    // proves: CLAIM(AI1'), CLAIM(AI2')
    let rig = Rig::new();
    rig.one_hour_cache();
    let probe = ScriptedCreditProbe::new(credit_state(true, CreditSource::Uw));
    let vigil = rig.vigil(claude(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 300, 7200, |_| {}).await;
    let envelope = call(&vigil, &id, None, Some(true)).await;
    assert_eq!(envelope["waited_s"], 3300);
    assert_eq!(envelope["cache"]["interval_s"], 3300);
    assert_eq!(envelope["cache"]["credit_check"]["on_credits"], true);
    assert!(envelope["events"].as_array().unwrap().is_empty(), "the first check is a baseline, not a change");
}

#[tokio::test(start_paused = true)]
async fn the_orchestrator_flag_can_be_stored_on_the_watch_and_a_false_call_overrides_it() {
    // why: AI1' / A8: "the watch can also store it at create". A call with no
    //      flag follows the stored one (3300 s); an explicit false from the
    //      caller wins (270 s) so a subagent that inherited the watch id is
    //      not stuck with the orchestrator window.
    // proves: CLAIM(AI1')
    let rig = Rig::new();
    rig.one_hour_cache();
    let probe = quiet_probe();
    let vigil = rig.vigil(claude(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 300, 20_000, |request| request.orchestrator = Some(true)).await;
    let stored = call(&vigil, &id, None, None).await;
    assert_eq!((&stored["waited_s"], &stored["cache"]["ttl"]), (&json!(3300), &json!("1h")));
    let overridden = call(&vigil, &id, None, Some(false)).await;
    assert_eq!(overridden["waited_s"], 270);
    assert_eq!(overridden["cache"]["source"], "default");
}

#[tokio::test(start_paused = true)]
async fn an_explicit_max_s_wins_over_the_cache_and_is_clamped_to_the_transport() {
    // why: AI1' "an explicit max_s still wins, clamped to the transport
    //      limit" (A2). Orchestrator on a 1h cache asks for 100 s -> 100 s,
    //      source explicit (A4); asks for 9999 -> 3300 on Claude (ceiling =
    //      the largest window vigil ever picks); on Codex 9999 -> 270 (Codex
    //      tool timeout 300 s). The Codex rig has no transcript at all.
    // proves: CLAIM(AI1'), CLAIM(AI3)
    let rig = Rig::new();
    rig.one_hour_cache();
    let probe = quiet_probe();
    let vigil = rig.vigil(claude(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 300, 40_000, |_| {}).await;
    let short = call(&vigil, &id, Some(100), Some(true)).await;
    assert_eq!((&short["waited_s"], &short["max_s"]), (&json!(100), &json!(100)));
    assert_eq!(short["cache"], json!({"ttl": "unknown", "source": "explicit", "interval_s": 100}));
    let huge = call(&vigil, &id, Some(9999), Some(true)).await;
    assert_eq!((&huge["waited_s"], &huge["cache"]["interval_s"]), (&json!(3300), &json!(3300)));

    let codex_probe = quiet_probe();
    let codex_vigil = rig.vigil(codex(), &codex_probe);
    let on_codex = call(&codex_vigil, &id, Some(9999), Some(true)).await;
    assert_eq!((&on_codex["waited_s"], &on_codex["cache"]["interval_s"]), (&json!(270), &json!(270)));
    assert_eq!(on_codex["cache"]["source"], "explicit");
}

#[tokio::test(start_paused = true)]
async fn a_codex_orchestrator_gets_270_s_from_the_codex_default() {
    // why: AI1' "Codex ... -> 270 s" with AI3's `codex-default`: no TTL is
    //      recorded for Codex, so the owner decision (5 minutes) is reported
    //      as 5m / codex-default.
    // proves: CLAIM(AI1'), CLAIM(AI3)
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(codex(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 300, 7200, |_| {}).await;
    let envelope = call(&vigil, &id, None, Some(true)).await;
    assert_eq!(envelope["waited_s"], 270);
    assert_eq!(envelope["cache"], json!({"ttl": "5m", "source": "codex-default", "interval_s": 270}));
}

// --------------------------------------------------- AI6: short calls ----

#[tokio::test(start_paused = true)]
async fn a_270_s_call_differs_from_todays_envelope_only_by_cache_and_clock() {
    // why: AI6. The scenario of keepalive.rs
    //      `a_quiet_call_with_a_far_deadline_returns_pending_after_exactly_270_s`
    //      (interval 20, deadline 7200): tick 14, now = start + 270, elapsed
    //      270, 6930 left, waited/max 270, session_ticks 1, no events, empty
    //      digest. The key set is today's thirteen keys plus exactly `cache`
    //      and `clock`; a `persist`, `others` or `cold` key would break
    //      "differs only by".
    // proves: CLAIM(AI6)
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 20, 7200, |_| {}).await;
    let envelope = call(&vigil, &id, None, None).await;
    let mut got = keys(&envelope);
    got.sort_unstable();
    let mut want = vec![
        "watch_id", "status", "now", "tick", "session_ticks", "elapsed_s", "deadline_in_s", "waited_s", "max_s", "events", "digest",
        "judge_status", "next", "cache", "clock",
    ];
    want.sort_unstable();
    assert_eq!(got, want);
    assert_eq!(envelope["watch_id"], "w");
    assert_eq!((&envelope["status"], &envelope["tick"], &envelope["session_ticks"]), (&json!("pending"), &json!(14), &json!(1)));
    assert_eq!(envelope["now"], serde_json::to_value(spec_instant().plus_secs(270)).unwrap());
    assert_eq!((&envelope["elapsed_s"], &envelope["deadline_in_s"]), (&json!(270), &json!(6930)));
    assert_eq!((&envelope["waited_s"], &envelope["max_s"]), (&json!(270), &json!(270)));
    assert_eq!((&envelope["events"], &envelope["digest"], &envelope["next"]), (&json!([]), &json!(""), &json!("call keepalive again")));
}

#[tokio::test(start_paused = true)]
async fn a_fired_envelope_also_gets_cache_and_clock_and_nothing_else_new() {
    // why: AI3/AI4 "every envelope" of a blocking return: a fire is the most
    //      important return, and it must carry the same two new fields with
    //      the same key set rule as the quiet one.
    // proves: CLAIM(AI6), CLAIM(AI3), CLAIM(AI4)
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, path) = quiet_watch(&rig, &vigil, "w", 20, 3600, |_| {}).await;
    append_file(&path, "boom\n");
    let envelope = call(&vigil, &id, None, None).await;
    assert_eq!(envelope["status"], "fired");
    let mut got = keys(&envelope);
    got.sort_unstable();
    let mut want = vec![
        "watch_id", "status", "now", "tick", "session_ticks", "elapsed_s", "deadline_in_s", "waited_s", "max_s", "events", "digest",
        "judge_status", "next", "cache", "clock",
    ];
    want.sort_unstable();
    assert_eq!(got, want);
    assert_eq!(envelope["waited_s"], 0, "a fire on the first tick waits no time");
    assert_eq!(envelope["cache"]["interval_s"], 270);
}

#[tokio::test(start_paused = true)]
async fn only_envelopes_that_come_from_a_call_carry_cache_and_clock() {
    // why: A12 as amended by S1. `watch_status` and `watch_cancel` have no
    //      call, so they carry neither `cache` nor `clock`. `wait` does: its
    //      final envelope carries both (S1, the assertion of absence here was
    //      wrong), but still no top-level `waited_s`/`max_s` (those are the
    //      window of ONE call; the clock block holds the wait's total). The
    //      full contents of wait's block are pinned in the next tests.
    //      Positive control: a blocking keepalive carries both, so the
    //      absence on status and cancel means something.
    // proves: CLAIM(AI6), CLAIM(S1)
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, path) = quiet_watch(&rig, &vigil, "w", 20, 3600, |_| {}).await;
    let status = serde_json::to_value(vigil.status(&id).await.unwrap()).unwrap();
    assert!(status.get("cache").is_none() && status.get("clock").is_none(), "{status}");
    let (blocking, _) = quiet_watch(&rig, &vigil, "blocking", 20, 3600, |_| {}).await;
    let blocking = call(&vigil, &blocking, Some(30), None).await;
    assert!(blocking.get("cache").is_some() && blocking.get("clock").is_some(), "{blocking}");
    append_file(&path, "boom\n");
    let waited = serde_json::to_value(vigil.wait(&id).await.unwrap()).unwrap();
    assert_eq!(waited["status"], "fired");
    assert!(waited.get("cache").is_some() && waited.get("clock").is_some(), "S1: wait's final envelope carries cache and clock: {waited}");
    for key in ["waited_s", "max_s"] {
        assert!(waited.get(key).is_none(), "wait's final envelope has no per-call {key}: {waited}");
    }
    let (other, _) = quiet_watch(&rig, &vigil, "x", 20, 3600, |_| {}).await;
    let cancelled = serde_json::to_value(vigil.cancel(&other).await.unwrap()).unwrap();
    assert!(cancelled.get("cache").is_none() && cancelled.get("clock").is_none(), "{cancelled}");
}

// ----------------------------------------------------------- AI4: clock ----

#[tokio::test(start_paused = true)]
async fn the_clock_block_counts_this_call_the_watch_and_the_gap_since_the_last_call() {
    // why: AI4. Call 1 (default window): waited 270, watch age 270, no
    //      earlier call -> since_last_call_s null (the key is present, as
    //      JSON null). The agent then spends 30 s (virtual) before call 2.
    //      Call 2: envelope.now = 270 + 30 + 270 = 570; the previous envelope
    //      was returned at 270, so since_last_call_s = 570 - 270 = 300 (the
    //      example in the spec has "waited 55m" and "last call 55m ago" - the
    //      gap includes the wait, A13); watch_age_s = 570. Summaries follow A14.
    // proves: CLAIM(AI4)
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 300, 7200, |_| {}).await;
    let first = call(&vigil, &id, None, None).await;
    assert_eq!(
        first["clock"],
        json!({"waited_s": 270, "watch_age_s": 270, "since_last_call_s": null, "summary": "waited 4m 30s this call; watching for 4m 30s; first call"})
    );
    tokio::time::sleep(Duration::from_secs(30)).await;
    let second = call(&vigil, &id, None, None).await;
    assert_eq!(
        second["clock"],
        json!({"waited_s": 270, "watch_age_s": 570, "since_last_call_s": 300, "summary": "waited 4m 30s this call; watching for 9m 30s; last call 5m ago"})
    );
    assert_eq!(second["clock"]["watch_age_s"], second["elapsed_s"], "A13: watch_age_s is elapsed_s");
    assert_eq!(second["clock"]["waited_s"], second["waited_s"]);
}

#[tokio::test(start_paused = true)]
async fn the_last_call_time_lives_in_the_watch_file_so_a_new_process_still_knows_it() {
    // why: AI4 "since_last_call_s is from the watch file's last-call
    //      timestamp". Call 1 from one process; call 2 from a fresh one (a CLI
    //      invocation, or a restarted MCP server) with no memory: it must
    //      still report 270 s since the last call (no gap between calls here:
    //      call 1 returned at 270, call 2 returns at 540), with the matching
    //      summary: age 540 s = 9m 0s, since 270 s = 4m (seconds dropped).
    // proves: CLAIM(AI4)
    let rig = Rig::new();
    let probe = quiet_probe();
    let first_process = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &first_process, "w", 3600, 7200, |_| {}).await;
    let first = call(&first_process, &id, None, None).await;
    assert!(first["clock"]["since_last_call_s"].is_null(), "{first}");
    let second_process = rig.vigil(AgentEnv::default(), &probe);
    let second = call(&second_process, &id, None, None).await;
    assert_eq!(second["clock"]["since_last_call_s"], 270);
    assert_eq!(second["clock"]["summary"], "waited 4m 30s this call; watching for 9m 0s; last call 4m ago");
}

#[tokio::test(start_paused = true)]
async fn a_1h_orchestrator_call_reproduces_the_specs_example_summary() {
    // why: AI4's example line: `waited 55m 0s this call; watching for 2h 10m;
    //      last call 55m ago`. With two 3300 s calls back to back (no agent
    //      gap) the second says waited 3300 = 55m 0s, last call 3300 = 55m,
    //      and the watch is 6600 s = 110 min = 1h 50m old (the example's 2h 10m
    //      needs a 600 s gap; the shape is what is pinned here).
    // proves: CLAIM(AI4), CLAIM(AI1')
    let rig = Rig::new();
    rig.one_hour_cache();
    let probe = quiet_probe();
    let vigil = rig.vigil(claude(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 10_000, |_| {}).await;
    call(&vigil, &id, None, Some(true)).await;
    let second = call(&vigil, &id, None, Some(true)).await;
    assert_eq!(second["clock"]["summary"], "waited 55m 0s this call; watching for 1h 50m; last call 55m ago");
    assert_eq!(second["clock"]["since_last_call_s"], 3300);
}

// ----------------------------------------- AI4 x DF: the disk-full note ----

fn every_write_fails() -> FaultPlan {
    FaultPlan::from_rules(vec![FaultRule::new(WriteKind::Watch, 28, When::Every), FaultRule::new(WriteKind::Reserve, 28, When::Every)])
}

#[tokio::test(start_paused = true)]
async fn cache_and_clock_ride_along_with_a_disk_full_persist_note() {
    // why: specs/disk-full.md DF1/DF2 must coexist with AI3/AI4. Every write
    //      fails; the quiet tick cannot be saved, so DF1a holds the call one
    //      watch interval (20 s) and answers pending with `persist.ok = false`.
    //      The same envelope must still carry `cache` (270 s default) and
    //      `clock` (waited 20 s, first call -> null since_last_call_s).
    // proves: CLAIM(AI3), CLAIM(AI4), CLAIM(DF2)
    let rig = Rig::new();
    let probe = quiet_probe();
    let healthy = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &healthy, "w", 20, 3600, |_| {}).await;
    let faulted = rig.vigil(AgentEnv::default(), &probe).with_write_faults(every_write_fails());
    let envelope = call(&faulted, &id, None, None).await;
    assert_eq!(envelope["persist"]["ok"], false, "{envelope}");
    assert_eq!(envelope["waited_s"], 20);
    assert_eq!(envelope["cache"]["interval_s"], 270);
    assert_eq!(envelope["clock"]["waited_s"], 20);
    assert!(envelope["clock"]["since_last_call_s"].is_null());
}

#[tokio::test(start_paused = true)]
async fn failing_to_save_the_last_call_time_is_silent_and_does_not_shorten_or_hold_the_call() {
    // why: A13 + DF1a. The last-call timestamp is bookkeeping: if its save
    //      fails (the disk is full) the envelope must NOT grow a `persist` note
    //      and the call must not be cut short or held (DF1a's hold is for
    //      ticks whose answer was lost). Watch cadence 3600 s, so call 2 has
    //      no tick due and its only write is the timestamp. Call 1 is healthy
    //      (tick 0 saved; returns at 270). Call 2 runs with every write
    //      failing: still waits the full 270 s, no persist / persist_failures
    //      keys, and since_last_call_s is 270 (read from call 1's saved value).
    // proves: CLAIM(AI4), CLAIM(DF2)
    let rig = Rig::new();
    let probe = quiet_probe();
    let healthy = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &healthy, "w", 3600, 7200, |_| {}).await;
    call(&healthy, &id, None, None).await;
    let faulted = rig.vigil(AgentEnv::default(), &probe).with_write_faults(every_write_fails());
    let started = Instant::now();
    let envelope = call(&faulted, &id, None, None).await;
    assert_eq!(started.elapsed(), Duration::from_secs(270));
    assert!(envelope.get("persist").is_none() && envelope.get("persist_failures").is_none(), "{envelope}");
    assert_eq!(envelope["clock"]["since_last_call_s"], 270);
}

// ------------------------------------------------------------ AI5: attention ----

#[tokio::test(start_paused = true)]
async fn a_watch_that_runs_past_one_and_a_half_times_expect_s_ends_the_call_with_one_attention_event() {
    // why: AI5: expect_s 100 -> threshold min(150, 640) = 150 s. The call
    //      (default window 270) ends at exactly 150 s (inclusive, A15) with
    //      status still pending, `next` = "act on events" (A17) and one
    //      attention event, reason overdue. Then it fires ONCE: a second call
    //      from the same process and a third from a fresh one both run their
    //      full 270 s with no events (the flag is in the watch file, so it
    //      holds across processes); a later change still fires the watch with
    //      only its own event.
    // proves: CLAIM(AI5)
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, path) = quiet_watch(&rig, &vigil, "w", 20, 3600, |request| request.expect_s = Some(100)).await;
    let first = call(&vigil, &id, None, None).await;
    assert_attention(&first, "overdue", 150);
    assert_eq!(first["elapsed_s"], 150);

    let second = call(&vigil, &id, None, None).await;
    assert_eq!((&second["waited_s"], &second["events"]), (&json!(270), &json!([])), "{second}");
    let fresh_process = rig.vigil(AgentEnv::default(), &probe);
    let third = call(&fresh_process, &id, None, None).await;
    assert_eq!((&third["waited_s"], &third["events"]), (&json!(270), &json!([])), "{third}");

    append_file(&path, "boom\n");
    let fired = call(&fresh_process, &id, None, None).await;
    assert_eq!(fired["status"], "fired");
    assert_eq!(only_event(&fired).get("kind"), None, "the change event is an ordinary one: {fired}");
}

#[tokio::test(start_paused = true)]
async fn the_overdue_threshold_is_capped_at_nine_minutes_over_expect_s() {
    // why: AI5 / D2: "1.5x or 9 minutes over, whichever comes first". expect_s
    //      2000 -> min(3000, 2540) = 2540. An explicit 3300 s window (long
    //      call, probe injected) must end at 2540, not at 3000 and not at the
    //      window's end.
    // proves: CLAIM(AI5)
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 7200, |request| request.expect_s = Some(2000)).await;
    let envelope = call(&vigil, &id, Some(3300), None).await;
    assert_attention(&envelope, "overdue", 2540);
}

#[tokio::test(start_paused = true)]
async fn the_call_wakes_at_the_attention_time_even_when_the_watch_ticks_far_less_often() {
    // why: AI5 + the wake schedule. Tick cadence 600 s, expect_s 60 ->
    //      threshold 90 s. Only the first tick runs inside a 270 s call, so a
    //      loop that sleeps "to the next tick" would sleep past 90 s. The call
    //      must still end at exactly 90 s.
    // proves: CLAIM(AI5)
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 7200, |request| request.expect_s = Some(60)).await;
    let envelope = call(&vigil, &id, None, None).await;
    assert_attention(&envelope, "overdue", 90);
}

#[tokio::test(start_paused = true)]
async fn a_process_input_that_runs_with_nothing_changing_for_stall_s_ends_the_call() {
    // why: AI5: "no input changed for stall_s while a process input is still
    //      running". File + live process, judge watch (a change would not end
    //      it), cadence 20 s, stall_s 60, created at t = 0. Nothing changes,
    //      so the first tick at which 60 s have passed - the tick at t = 60 -
    //      raises `stalled` (inclusive; stall is read at ticks, A15). Window
    //      is the default 270 s.
    // proves: CLAIM(AI5)
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let sleeper = Sleeper::start();
    let (id, _) =
        judge_watch(&rig, &vigil, "w", 20, 3600, |request| {
            request.stall_s = Some(60);
            request.inputs.push(Input::Process { pid: sleeper.pid(), want: None });
        })
        .await;
    let envelope = call(&vigil, &id, None, None).await;
    assert_attention(&envelope, "stalled", 60);
}

#[tokio::test(start_paused = true)]
async fn a_change_restarts_the_stall_clock() {
    // why: AI5 "no input changed for stall_s": a change at t = 50 (seen by the
    //      tick at t = 60, the judge says "not failed") moves the reference to
    //      60, so the stall (60 s later) is raised by the tick at t = 120 -
    //      not at 60, which a clock that ignores changes would give. The
    //      boundary the same arithmetic gives from the file time (50 + 60 =
    //      110) rounds up to the same tick, so both readings agree on 120.
    // proves: CLAIM(AI5)
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let sleeper = Sleeper::start();
    let (id, path) =
        judge_watch(&rig, &vigil, "w", 20, 3600, |request| {
            request.stall_s = Some(60);
            request.inputs.push(Input::Process { pid: sleeper.pid(), want: None });
        })
        .await;
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(50)).await;
        append_file(&path, "progress\n");
    });
    let envelope = call(&vigil, &id, None, None).await;
    assert_attention(&envelope, "stalled", 120);
}

#[tokio::test(start_paused = true)]
async fn without_a_running_process_there_is_no_stall() {
    // why: AI5 / A15: "stalled" is defined with "while a process input is
    //      still running". (1) A file-only watch with stall_s 60 never
    //      stalls: the 270 s call is quiet. (2) A watch whose process exits
    //      at t = 30 stops being able to stall: the exit is a change (judge
    //      says "not failed"), nothing is running afterwards, the call is
    //      quiet. Positive control at the end: the same setup with the process
    //      alive -> stalled at 60, so "quiet" is not just a broken stall.
    // proves: CLAIM(AI5)
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (file_only, _) = judge_watch(&rig, &vigil, "file-only", 20, 3600, |request| request.stall_s = Some(60)).await;
    let quiet = call(&vigil, &file_only, None, None).await;
    assert_eq!((&quiet["waited_s"], &quiet["events"]), (&json!(270), &json!([])), "{quiet}");

    let mut child = std::process::Command::new("sleep").arg("600").spawn().unwrap();
    let pid = child.id();
    let (exits, _) = judge_watch(&rig, &vigil, "exits", 20, 3600, |request| {
        request.stall_s = Some(60);
        request.inputs.push(Input::Process { pid, want: None });
    })
    .await;
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        let _ = child.kill();
        let _ = child.wait();
    });
    let after_exit = call(&vigil, &exits, None, None).await;
    assert_eq!((&after_exit["waited_s"], &after_exit["events"]), (&json!(270), &json!([])), "{after_exit}");

    // Positive control, same setup with the process kept alive: stalled at 60.
    let alive = Sleeper::start();
    let (control, _) = judge_watch(&rig, &vigil, "control", 20, 3600, |request| {
        request.stall_s = Some(60);
        request.inputs.push(Input::Process { pid: alive.pid(), want: None });
    })
    .await;
    assert_attention(&call(&vigil, &control, None, None).await, "stalled", 60);
}

#[tokio::test(start_paused = true)]
async fn a_long_window_applies_a_900_s_default_stall_to_a_watch_with_a_running_process() {
    // why: AI5 / D3: neither expect_s nor stall_s set, window > 270 s (explicit
    //      3300 here; probe injected), file + running process -> default
    //      stall_s 900, so the tick at t = 900 (cadence 20 s) raises
    //      `stalled`. This is the "hung build is noticed within ~15 minutes"
    //      rule.
    // proves: CLAIM(AI5)
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let sleeper = Sleeper::start();
    let (id, _) = judge_watch(&rig, &vigil, "w", 20, 7200, |request| request.inputs.push(Input::Process { pid: sleeper.pid(), want: None })).await;
    let envelope = call(&vigil, &id, Some(3300), None).await;
    assert_attention(&envelope, "stalled", 900);
}

#[tokio::test(start_paused = true)]
async fn the_default_stall_needs_a_long_window_and_is_off_when_expect_s_is_set() {
    // why: AI5: the default is for LONG ticks ("interval > 270 s") and only
    //      "when neither field is set" (A15, literal reading). (1) Four
    //      default 270 s calls in a row cross t = 900 and stay quiet. (2) A
    //      long call with expect_s 1000 (threshold 1500) is not interrupted at
    //      900: it ends at 1500 as `overdue`, so setting expect_s alone turned
    //      the default stall off.
    // proves: CLAIM(AI5)
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let first = Sleeper::start();
    let (short, _) = judge_watch(&rig, &vigil, "short", 20, 7200, |request| request.inputs.push(Input::Process { pid: first.pid(), want: None })).await;
    for round in 1..=4 {
        let envelope = call(&vigil, &short, None, None).await;
        assert_eq!((&envelope["waited_s"], &envelope["events"]), (&json!(270), &json!([])), "round {round}: {envelope}");
    }

    let second = Sleeper::start();
    let (with_expect, _) = judge_watch(&rig, &vigil, "expect", 20, 7200, |request| {
        request.expect_s = Some(1000);
        request.inputs.push(Input::Process { pid: second.pid(), want: None });
    })
    .await;
    let long = call(&vigil, &with_expect, Some(3300), None).await;
    assert_attention(&long, "overdue", 1500);
}

#[tokio::test(start_paused = true)]
async fn a_zero_expect_s_or_stall_s_is_refused_at_create() {
    // why: A16: 0 would raise attention on the very first tick, which is never
    //      meant. Positive control: 1 is accepted for both.
    // proves: CLAIM(AI5)
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let request = |id: &str| {
        let path = rig.env.input_path(&format!("{id}.log"));
        write_file(&path, "old\n");
        change_request(id, &path, 20)
    };
    let mut zero_expect = request("zero-expect");
    zero_expect.expect_s = Some(0);
    assert!(vigil.create(zero_expect).await.is_err(), "expect_s 0 must be refused");
    let mut zero_stall = request("zero-stall");
    zero_stall.stall_s = Some(0);
    assert!(vigil.create(zero_stall).await.is_err(), "stall_s 0 must be refused");
    let mut one = request("ok");
    (one.expect_s, one.stall_s) = (Some(1), Some(1));
    assert!(vigil.create(one).await.is_ok());
}

// -------------------------------------------- AI2' / AI8: the credit check ----

#[tokio::test(start_paused = true)]
async fn a_long_call_checks_credit_state_at_the_start_and_every_240_s() {
    // why: AI2' "every 240 s" (A9: also at call start). A 3300 s call checks
    //      at 0, 240, ..., 3120 = 14 times (3360 is past the end). The last
    //      check is recorded in cache.credit_check with its wall-clock time
    //      (spec instant + 3120 s), source and verdict.
    // proves: CLAIM(AI2')
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 7200, |_| {}).await;
    let started = Instant::now();
    let envelope = call(&vigil, &id, Some(3300), None).await;
    let expected: Vec<u64> = (0..14).map(|n| n * 240).collect();
    assert_eq!(probe.offsets_from(started), expected);
    assert_eq!(
        envelope["cache"]["credit_check"],
        json!({"at": serde_json::to_value(spec_instant().plus_secs(3120)).unwrap(), "source": "uw", "on_credits": false})
    );
}

#[tokio::test(start_paused = true)]
async fn only_calls_longer_than_270_s_check_credit_state() {
    // why: AI2' "a long call" (A9) and AI6. An explicit 270 s call makes no
    //      check and has no credit_check; an explicit 271 s call is long: it
    //      checks at 0 and 240. Boundary on the "> 270" line.
    // proves: CLAIM(AI2'), CLAIM(AI6)
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 7200, |_| {}).await;
    let short = call(&vigil, &id, Some(270), None).await;
    assert_eq!(probe.asked_count(), 0);
    assert!(short["cache"].get("credit_check").is_none(), "{short}");
    let started = Instant::now();
    let long = call(&vigil, &id, Some(271), None).await;
    assert_eq!(probe.offsets_from(started), vec![0, 240]);
    assert_eq!(long["waited_s"], 271);
    assert!(long["cache"].get("credit_check").is_some());
}

#[tokio::test(start_paused = true)]
async fn the_credit_state_flipping_on_ends_the_call_with_one_event_and_records_the_check() {
    // why: AI8: checks at 0 (baseline: off), 240, 480 (off), 720 (on) -> the
    //      change is seen at t = 720. The call ends there (waited 720, not
    //      3300) with the watch still pending, `next` = "act on events"
    //      (A17), exactly one event kind credit_state_changed carrying
    //      on_credits, source and the windows the probe returned, and the
    //      check itself in cache.credit_check (at = start + 720 s).
    // proves: CLAIM(AI8), CLAIM(AI2')
    let rig = Rig::new();
    let probe = quiet_probe();
    for _ in 0..3 {
        probe.push(credit_state(false, CreditSource::Uw));
    }
    probe.push(credit_state(true, CreditSource::Uw));
    probe.set_steady(credit_state(true, CreditSource::Uw));
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 7200, |_| {}).await;
    let envelope = call(&vigil, &id, Some(3300), None).await;
    assert_eq!((&envelope["status"], &envelope["next"], &envelope["waited_s"]), (&json!("pending"), &json!("act on events"), &json!(720)), "{envelope}");
    let event = only_event(&envelope);
    assert_eq!(event["kind"], "credit_state_changed");
    assert_eq!(event["on_credits"], true);
    assert_eq!(event["source"], "uw");
    assert_eq!(event["windows"], json!([{"window": {"Rolling": {"minutes": 10080}}, "exceeded": true}]));
    assert!(event.get("hook").is_none(), "no hook is configured: {event}");
    assert_eq!(
        envelope["cache"]["credit_check"],
        json!({"at": serde_json::to_value(spec_instant().plus_secs(720)).unwrap(), "source": "uw", "on_credits": true})
    );
}

#[tokio::test(start_paused = true)]
async fn credits_ending_is_reported_too_and_the_same_state_is_reported_only_once() {
    // why: AI8 "either direction" and "one credit_state_changed event". Steady
    //      on: check 0 is the baseline (on), the script flips to off at 240 ->
    //      event on_credits false at 240. A second call over the same Vigil
    //      with the state still off runs its full 3300 s with no event (the
    //      previous state is remembered in credit-state.json, S9), and a
    //      third where it flips on again at 480 ends at 480 with on_credits
    //      true.
    // proves: CLAIM(AI8)
    let rig = Rig::new();
    let probe = ScriptedCreditProbe::new(credit_state(false, CreditSource::Uw));
    probe.push(credit_state(true, CreditSource::Uw));
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 20_000, |_| {}).await;
    let first = call(&vigil, &id, Some(3300), None).await;
    assert_eq!(first["waited_s"], 240, "{first}");
    assert_eq!(only_event(&first)["on_credits"], false);

    let second = call(&vigil, &id, Some(3300), None).await;
    assert_eq!((&second["waited_s"], &second["events"]), (&json!(3300), &json!([])), "{second}");

    probe.push(credit_state(false, CreditSource::Uw));
    probe.push(credit_state(false, CreditSource::Uw));
    probe.push(credit_state(true, CreditSource::Uw));
    let third = call(&vigil, &id, Some(3300), None).await;
    assert_eq!(third["waited_s"], 480, "{third}");
    assert_eq!(only_event(&third)["on_credits"], true);
}

#[tokio::test(start_paused = true)]
async fn the_first_check_a_process_makes_is_a_baseline_never_a_change() {
    // why: A9, kept by S9 for a state root that has no credit-state.json yet.
    //      Credits have been on since before vigil started: the first check
    //      says on, and so does every later one. No event may be invented
    //      because "there was no previous state"; the call runs its full
    //      window and records on_credits true.
    // proves: CLAIM(AI8)
    let rig = Rig::new();
    let probe = ScriptedCreditProbe::new(credit_state(true, CreditSource::Uw));
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 7200, |_| {}).await;
    let envelope = call(&vigil, &id, Some(3300), None).await;
    assert_eq!((&envelope["waited_s"], &envelope["events"]), (&json!(3300), &json!([])), "{envelope}");
    assert_eq!(envelope["cache"]["credit_check"]["on_credits"], true);
}

#[tokio::test(start_paused = true)]
async fn the_event_names_the_source_the_probe_used() {
    // why: AI8 `source`: when `uw` failed and the transcript fallback decided,
    //      the event says `transcripts` (and carries no windows, A10).
    // proves: CLAIM(AI8)
    let rig = Rig::new();
    let probe = ScriptedCreditProbe::new(credit_state(true, CreditSource::Transcripts));
    probe.push(credit_state(false, CreditSource::Transcripts));
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 7200, |_| {}).await;
    let envelope = call(&vigil, &id, Some(3300), None).await;
    let event = only_event(&envelope);
    assert_eq!((&event["kind"], &event["source"], &event["windows"]), (&json!("credit_state_changed"), &json!("transcripts"), &json!([])));
    assert_eq!(envelope["cache"]["credit_check"]["source"], "transcripts");
}

// ------------------------------------------------ AI8: the hook (real time) ----
//
// These start a real `sh` process, so they run on the real clock with a short
// `credit_check_every`; the 240 s and 10 s defaults are pinned in config.rs.
// Every call is wrapped in a timeout: against a missing implementation the
// call would otherwise block for its full 3300 s.

const REAL_CALL_LIMIT: Duration = Duration::from_secs(20);

async fn real_call(vigil: &Vigil, id: &WatchId) -> Value {
    let request = KeepaliveRequest { watch_id: Some(id.clone()), max_s: Some(3300), orchestrator: None };
    let envelope = tokio::time::timeout(REAL_CALL_LIMIT, vigil.keepalive(request))
        .await
        .expect("the call should have ended on the credit event, not run its full window")
        .unwrap();
    serde_json::to_value(&envelope).unwrap()
}

fn off_then_on() -> Arc<ScriptedCreditProbe> {
    let probe = ScriptedCreditProbe::new(credit_state(true, CreditSource::Uw));
    probe.push(credit_state(false, CreditSource::Uw));
    probe
}

#[tokio::test]
async fn the_hook_runs_once_per_change_with_the_state_on_stdin_and_its_result_in_the_event() {
    // why: AI8 step 2. Hook command (from Config::on_credit_change) writes its
    //      stdin to a file, prints "hello", appends a line to a counter and
    //      exits 3. Checks at 0 (off, baseline) and 200 ms (on): one change.
    //      The event carries hook {command, exit_code 3, timed_out false,
    //      output containing "hello"}; the hook's stdin parses to a JSON
    //      object with the same on_credits, source and windows as the event
    //      and the `at` of cache.credit_check. A further call with the state
    //      still on (cancelled after 900 ms - it must still be running, i.e.
    //      no second event) leaves the counter at exactly one run.
    // proves: CLAIM(AI8)
    let scratch = tempfile::tempdir().unwrap();
    let (stdin_file, counter) = (scratch.path().join("stdin.json"), scratch.path().join("count"));
    let command = format!("cat > '{}'; echo hello; echo run >> '{}'; exit 3", stdin_file.display(), counter.display());
    let configured = command.clone();
    let rig = Rig::real_time(move |config| {
        config.credit_check_every = Duration::from_millis(200);
        config.on_credit_change = Some(configured);
    });
    let probe = off_then_on();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 3600, 7200, |_| {}).await;
    let envelope = real_call(&vigil, &id).await;
    let event = only_event(&envelope);
    assert_eq!(event["kind"], "credit_state_changed");
    assert_eq!(event["on_credits"], true);
    assert_eq!(event["hook"]["command"], command.as_str());
    assert_eq!(event["hook"]["exit_code"], 3);
    assert_eq!(event["hook"]["timed_out"], false);
    assert!(event["hook"]["output"].as_str().unwrap().contains("hello"), "{event}");

    let seen: Value = serde_json::from_str(&std::fs::read_to_string(&stdin_file).expect("the hook was run")).unwrap();
    assert_eq!(seen["on_credits"], event["on_credits"]);
    assert_eq!(seen["source"], event["source"]);
    assert_eq!(seen["windows"], event["windows"]);
    assert_eq!(seen["at"], envelope["cache"]["credit_check"]["at"]);

    let request = KeepaliveRequest { watch_id: Some(id.clone()), max_s: Some(3300), orchestrator: None };
    let second = tokio::time::timeout(Duration::from_millis(900), vigil.keepalive(request)).await;
    assert!(second.is_err(), "no change, so no event and no early end: {second:?}");
    assert_eq!(std::fs::read_to_string(&counter).unwrap().lines().count(), 1, "the hook runs once per change");
}

#[tokio::test]
async fn a_hook_that_hangs_is_cut_at_its_timeout_and_the_event_says_so() {
    // why: AI8 "10 s timeout" (Config::credit_hook_timeout shortened to
    //      300 ms; the default is pinned in config.rs). `sleep 30` never ends:
    //      the event must still arrive, with timed_out true and no exit code,
    //      well before the 20 s test limit.
    // proves: CLAIM(AI8)
    let rig = Rig::real_time(|config| {
        config.credit_check_every = Duration::from_millis(200);
        config.on_credit_change = Some("sleep 30".to_string());
        config.credit_hook_timeout = Duration::from_millis(300);
    });
    let probe = off_then_on();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 3600, 7200, |_| {}).await;
    let started = std::time::Instant::now();
    let envelope = real_call(&vigil, &id).await;
    assert!(started.elapsed() < Duration::from_secs(10), "waited {:?}", started.elapsed());
    let event = only_event(&envelope);
    assert_eq!(event["kind"], "credit_state_changed");
    assert_eq!(event["hook"]["timed_out"], true);
    assert!(event["hook"]["exit_code"].is_null(), "{event}");
}

#[tokio::test]
async fn a_hook_that_cannot_run_does_not_stop_the_event() {
    // why: AI8: the hook is an extension point, not a gate. A command that
    //      does not exist (sh exits 127) must leave the event intact, with the
    //      127 recorded.
    // proves: CLAIM(AI8)
    let rig = Rig::real_time(|config| {
        config.credit_check_every = Duration::from_millis(200);
        config.on_credit_change = Some("/nonexistent/vigil-credit-hook".to_string());
    });
    let probe = off_then_on();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 3600, 7200, |_| {}).await;
    let envelope = real_call(&vigil, &id).await;
    let event = only_event(&envelope);
    assert_eq!((&event["kind"], &event["on_credits"], &event["hook"]["exit_code"]), (&json!("credit_state_changed"), &json!(true), &json!(127)));
}
