//! Engine-level tests for docs/specs/auto-interval.md (AI1'-AI6, AI8; AI7 is
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
//! Round 2 (spec review amendments S1-S10) adds, in this file: the automatic 1h -> credits -> 5m -> credits end -> 1h sequence and the
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
use crate::fault::{FaultPlan, FaultRule, Stage, When, WriteKind};
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

    fn read_credit_file_opt(&self) -> Option<Value> {
        std::fs::read_to_string(self.credit_file()).ok().map(|text| serde_json::from_str(&text).expect("credit-state.json is JSON"))
    }

    fn read_credit_file(&self) -> Value {
        let bytes = std::fs::read_to_string(self.credit_file());
        assert!(bytes.is_ok(), "credit-state.json must be persisted: {bytes:?}");
        let file = serde_json::from_str(&bytes.unwrap());
        assert!(file.is_ok(), "credit-state.json must be valid JSON: {file:?}");
        file.unwrap()
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
    AgentEnv { claude_session_id: Some("sess-1".into()), codex: false, antigravity: false }
}

fn codex() -> AgentEnv {
    AgentEnv { claude_session_id: None, codex: true, antigravity: false }
}

fn antigravity() -> AgentEnv {
    AgentEnv { claude_session_id: None, codex: false, antigravity: true }
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
    let envelope = vigil.keepalive(KeepaliveRequest { watch_id: Some(id.clone()), max_s, orchestrator, confirm: None, session: false }).await.unwrap();
    serde_json::to_value(&envelope).unwrap()
}

/// A no-id call (covers this process's pending watches, V7).
async fn call_noid(vigil: &Vigil, max_s: Option<u64>, orchestrator: Option<bool>) -> Value {
    let envelope = vigil.keepalive(KeepaliveRequest { watch_id: None, max_s, orchestrator, confirm: None, session: false }).await.unwrap();
    serde_json::to_value(&envelope).unwrap()
}

/// S2 supersedes the pre-S2 assertions that an orchestrator call's `cache` holds
/// only the three AI3 keys: an orchestrator call also makes its start credit
/// check, which is reported as `credit_check`. Returns `cache` without that key
/// and asserts the check is there and says "not on credits" (the quiet probe).
fn cache_without_start_check(envelope: &Value) -> Value {
    let mut cache = envelope["cache"].clone();
    let check = cache.as_object_mut().expect("cache object").remove("credit_check").expect("S2: an orchestrator call reports its start credit check");
    assert_eq!(check["on_credits"], false, "{check}");
    cache
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
async fn an_orchestrator_on_a_one_hour_cache_waits_3000_s_and_says_why() {
    // why: AI1' + AI3: the point of the feature. The caller says it is the
    //      orchestrator (stored nowhere, passed as the flag), its transcript's
    //      newest write is 1h, so the call blocks 3000 s (3600 s TTL - 600 s
    //      margin, D1; owner 2026-11-13: at most 50 minutes) and the envelope says what was chosen and why. Quiet
    //      watch, tick cadence 300 s, deadline 7200 s: after 3000 s the watch
    //      is 3000 s old (elapsed_s) with 4200 s left (7200 - 3000), still
    //      pending, `next` still the loop word. No `cold` key (AI7 removed).
    // proves: CLAIM(AI1'), CLAIM(AI3)
    let rig = Rig::new();
    rig.one_hour_cache();
    let probe = quiet_probe();
    let vigil = rig.vigil(claude(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 300, 7200, |_| {}).await;
    let started = Instant::now();
    let envelope = call(&vigil, &id, None, Some(true)).await;
    assert_eq!(started.elapsed(), Duration::from_secs(3000));
    assert_eq!((&envelope["waited_s"], &envelope["max_s"]), (&json!(3000), &json!(3000)));
    assert_eq!((&envelope["elapsed_s"], &envelope["deadline_in_s"]), (&json!(3000), &json!(4200)));
    assert_eq!((&envelope["status"], &envelope["next"]), (&json!("pending"), &json!("call keepalive again")));
    assert_eq!(envelope["cache"]["ttl"], "1h");
    assert_eq!(envelope["cache"]["source"], "claude-transcript");
    assert_eq!(envelope["cache"]["interval_s"], 3000);
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
    assert_eq!(cache_without_start_check(&envelope), json!({"ttl": "5m", "source": "claude-transcript", "interval_s": 270}));
}

#[tokio::test(start_paused = true)]
async fn the_window_is_decided_again_at_every_call() {
    // why: AI1' "detection runs at every call": the first call sees a 1h
    //      write (3000 s); then the session's cache setting flips (a 5m write
    //      lands in the transcript) and the very next call must be 270 s, or
    //      the cache goes cold under a 50 minute wait. The transcript is
    //      appended to between the calls; the Vigil value is the same.
    // proves: CLAIM(AI1')
    let rig = Rig::new();
    let transcript = rig.one_hour_cache();
    let probe = quiet_probe();
    let vigil = rig.vigil(claude(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 300, 20_000, |_| {}).await;
    let first = call(&vigil, &id, None, Some(true)).await;
    assert_eq!((&first["waited_s"], &first["cache"]["ttl"]), (&json!(3000), &json!("1h")));
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
    //      transcript says 1h; the window stays 3000 s and the check is
    //      recorded in cache.credit_check as on_credits true.
    // proves: CLAIM(AI1'), CLAIM(AI2')
    let rig = Rig::new();
    rig.one_hour_cache();
    let probe = ScriptedCreditProbe::new(credit_state(true, CreditSource::Uw));
    let vigil = rig.vigil(claude(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 300, 7200, |_| {}).await;
    let envelope = call(&vigil, &id, None, Some(true)).await;
    assert_eq!(envelope["waited_s"], 3000);
    assert_eq!(envelope["cache"]["interval_s"], 3000);
    assert_eq!(envelope["cache"]["credit_check"]["on_credits"], true);
    assert!(envelope["events"].as_array().unwrap().is_empty(), "the first check is a baseline, not a change");
}

#[tokio::test(start_paused = true)]
async fn the_orchestrator_flag_can_be_stored_on_the_watch_and_a_false_call_overrides_it() {
    // why: AI1' / A8: "the watch can also store it at create". A call with no
    //      flag follows the stored one (3000 s); an explicit false from the
    //      caller wins (270 s) so a subagent that inherited the watch id is
    //      not stuck with the orchestrator window.
    // proves: CLAIM(AI1')
    let rig = Rig::new();
    rig.one_hour_cache();
    let probe = quiet_probe();
    let vigil = rig.vigil(claude(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 300, 20_000, |request| request.orchestrator = Some(true)).await;
    let stored = call(&vigil, &id, None, None).await;
    assert_eq!((&stored["waited_s"], &stored["cache"]["ttl"]), (&json!(3000), &json!("1h")));
    let overridden = call(&vigil, &id, None, Some(false)).await;
    assert_eq!(overridden["waited_s"], 270);
    assert_eq!(overridden["cache"]["source"], "default");
}

#[tokio::test(start_paused = true)]
async fn an_explicit_max_s_wins_over_the_cache_and_is_clamped_to_the_transport() {
    // why: AI1' "an explicit max_s still wins, clamped to the transport
    //      limit" (A2). Orchestrator on a 1h cache asks for 100 s -> 100 s,
    //      source explicit (A4); asks for 9999 -> 3000 on Claude (ceiling =
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
    assert_eq!(cache_without_start_check(&short), json!({"ttl": "unknown", "source": "explicit", "interval_s": 100}));
    let huge = call(&vigil, &id, Some(9999), Some(true)).await;
    assert_eq!((&huge["waited_s"], &huge["cache"]["interval_s"]), (&json!(3000), &json!(3000)));

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
    assert_eq!(cache_without_start_check(&envelope), json!({"ttl": "5m", "source": "codex-default", "interval_s": 270}));
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
    //      call, so they carry neither `cache` nor `clock`. (The `vigil wait`
    //      half of S1 went with the command, W42-W1.) Positive control: a
    //      blocking keepalive carries both, so the absence on status and
    //      cancel means something.
    // proves: CLAIM(AI6), CLAIM(S1)
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 20, 3600, |_| {}).await;
    let status = serde_json::to_value(vigil.status(&id).await.unwrap()).unwrap();
    assert!(status.get("cache").is_none() && status.get("clock").is_none(), "{status}");
    let (blocking, _) = quiet_watch(&rig, &vigil, "blocking", 20, 3600, |_| {}).await;
    let blocking = call(&vigil, &blocking, Some(30), None).await;
    assert!(blocking.get("cache").is_some() && blocking.get("clock").is_some(), "{blocking}");
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
    //      example in the spec has "waited 55m" and "last call 55m ago" (before the 50 minute cap) - the
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
    // why: AI4's example line shape: `waited 50m 0s this call; watching for
    //      1h 40m; last call 50m ago` (the spec's example said 55m before the
    //      owner's 2026-11-13 50 minute cap). With two 3000 s calls back to
    //      back (no agent gap) the second says waited 3000 = 50m 0s, last call
    //      3000 = 50m, and the watch is 6000 s = 100 min = 1h 40m old.
    // proves: CLAIM(AI4), CLAIM(AI1')
    let rig = Rig::new();
    rig.one_hour_cache();
    let probe = quiet_probe();
    let vigil = rig.vigil(claude(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 10_000, |_| {}).await;
    call(&vigil, &id, None, Some(true)).await;
    let second = call(&vigil, &id, None, Some(true)).await;
    assert_eq!(second["clock"]["summary"], "waited 50m 0s this call; watching for 1h 40m; last call 50m ago");
    assert_eq!(second["clock"]["since_last_call_s"], 3000);
}

// ----------------------------------------- AI4 x DF: the disk-full note ----

fn every_write_fails() -> FaultPlan {
    FaultPlan::from_rules(vec![FaultRule::new(WriteKind::Watch, 28, When::Every), FaultRule::new(WriteKind::Reserve, 28, When::Every)])
}

#[tokio::test(start_paused = true)]
async fn cache_and_clock_ride_along_with_a_disk_full_persist_note() {
    // why: docs/specs/disk-full.md DF1/DF2 must coexist with AI3/AI4. Every write
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
    //      2000 -> min(3000, 2540) = 2540. An explicit 3000 s window (long
    //      call, probe injected) must end at 2540, not at 3000 (which is both
    //      1.5x and the window's end here).
    // proves: CLAIM(AI5)
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 7200, |request| request.expect_s = Some(2000)).await;
    let envelope = call(&vigil, &id, Some(3000), None).await;
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
    //      3000 here; probe injected), file + running process -> default
    //      stall_s 900, so the tick at t = 900 (cadence 20 s) raises
    //      `stalled`. This is the "hung build is noticed within ~15 minutes"
    //      rule.
    // proves: CLAIM(AI5)
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let sleeper = Sleeper::start();
    let (id, _) = judge_watch(&rig, &vigil, "w", 20, 7200, |request| request.inputs.push(Input::Process { pid: sleeper.pid(), want: None })).await;
    let envelope = call(&vigil, &id, Some(3000), None).await;
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
    let long = call(&vigil, &with_expect, Some(3000), None).await;
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
    // why: AI2' "every 240 s" (A9: also at call start). A 3000 s call checks
    //      at 0, 240, ..., 2880 = 13 times (3120 is past the end). The last
    //      check is recorded in cache.credit_check with its wall-clock time
    //      (spec instant + 2880 s), source and verdict.
    // proves: CLAIM(AI2')
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 7200, |_| {}).await;
    let started = Instant::now();
    let envelope = call(&vigil, &id, Some(3000), Some(true)).await;
    let expected: Vec<u64> = (0..13).map(|n| n * 240).collect();
    assert_eq!(probe.offsets_from(started), expected);
    assert_eq!(
        envelope["cache"]["credit_check"],
        json!({"at": serde_json::to_value(spec_instant().plus_secs(2880)).unwrap(), "source": "uw", "on_credits": false})
    );
}

#[tokio::test(start_paused = true)]
async fn credit_monitoring_follows_the_orchestrator_even_with_a_short_window() {
    // why: S2 replaces A9's window gate. Explicit 271 s without an
    //      orchestrator must not probe; 270 s WITH it checks even though short.
    // proves: CLAIM(S2)
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 20, 7200, |_| {}).await;
    let ordinary = call(&vigil, &id, Some(271), Some(false)).await;
    assert_eq!(probe.asked_count(), 0);
    assert!(ordinary["cache"].get("credit_check").is_none());
    let started = Instant::now();
    let short = call(&vigil, &id, Some(270), Some(true)).await;
    assert_eq!(short["waited_s"], 270);
    assert_eq!(probe.offsets_from(started), vec![0]);
    assert_eq!(short["cache"]["credit_check"]["on_credits"], false);
}

#[tokio::test(start_paused = true)]
async fn the_credit_state_flipping_on_ends_the_call_with_one_event_and_records_the_check() {
    // why: AI8: checks at 0 (baseline: off), 240, 480 (off), 720 (on) -> the
    //      change is seen at t = 720. The call ends there (waited 720, not
    //      3000) with the watch still pending, `next` = "act on events"
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
    let envelope = call(&vigil, &id, Some(3000), Some(true)).await;
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
    //      with the state still off runs its full 3000 s with no event (the
    //      previous state is remembered in credit-state.json, S9), and a
    //      third first checks 60 s in, since the last check was at absolute
    //      3120 and this call starts at 3240. Its third verdict arrives at
    //      600 s (120, 360, 600), reporting on_credits true. S2 keeps cadence.
    // proves: CLAIM(AI8)
    let rig = Rig::new();
    let probe = ScriptedCreditProbe::new(credit_state(false, CreditSource::Uw));
    probe.push(credit_state(true, CreditSource::Uw));
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 20_000, |_| {}).await;
    let first = call(&vigil, &id, Some(3000), Some(true)).await;
    assert_eq!(first["waited_s"], 240, "{first}");
    assert_eq!(only_event(&first)["on_credits"], false);

    let second = call(&vigil, &id, Some(3000), Some(true)).await;
    assert_eq!((&second["waited_s"], &second["events"]), (&json!(3000), &json!([])), "{second}");

    probe.push(credit_state(false, CreditSource::Uw));
    probe.push(credit_state(false, CreditSource::Uw));
    probe.push(credit_state(true, CreditSource::Uw));
    let third = call(&vigil, &id, Some(3000), Some(true)).await;
    assert_eq!(third["waited_s"], 600, "{third}");
    assert_eq!(only_event(&third)["on_credits"], true);
}

#[tokio::test(start_paused = true)]
async fn a_call_that_starts_just_past_a_second_still_reports_the_whole_wait() {
    // why: the credit cadence is anchored to stored whole-second instants, so a
    //      call starting 3 ms past a second waits 599.997 s for a 600 s verdict.
    //      `waited_s` floored that to 599 (the flake in
    //      credits_ending_is_reported_too, which got its offset by chance).
    // catches: waited_s truncating instead of rounding.
    let rig = Rig::new();
    let probe = ScriptedCreditProbe::new(credit_state(false, CreditSource::Uw));
    probe.push(credit_state(true, CreditSource::Uw));
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 20_000, |_| {}).await;
    let first = call(&vigil, &id, Some(3000), Some(true)).await;
    assert_eq!(first["waited_s"], 240, "{first}");
    let second = call(&vigil, &id, Some(3000), Some(true)).await;
    assert_eq!(second["waited_s"], 3000, "{second}");
    probe.push(credit_state(false, CreditSource::Uw));
    probe.push(credit_state(false, CreditSource::Uw));
    probe.push(credit_state(true, CreditSource::Uw));
    tokio::time::sleep(Duration::from_millis(3)).await;
    let third = call(&vigil, &id, Some(3000), Some(true)).await;
    assert_eq!(third["waited_s"], 600, "{third}");
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
    let envelope = call(&vigil, &id, Some(3000), Some(true)).await;
    assert_eq!((&envelope["waited_s"], &envelope["events"]), (&json!(3000), &json!([])), "{envelope}");
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
    let envelope = call(&vigil, &id, Some(3000), Some(true)).await;
    let event = only_event(&envelope);
    assert_eq!((&event["kind"], &event["source"], &event["windows"]), (&json!("credit_state_changed"), &json!("transcripts"), &json!([])));
    assert_eq!(envelope["cache"]["credit_check"]["source"], "transcripts");
}

// ------------------------------------------------ AI8: the hook (real time) ----
//
// These start a real `sh` process, so they run on the real clock with a short
// `credit_check_every`; the 240 s and 10 s defaults are pinned in config.rs.
// Every call is wrapped in a timeout: against a missing implementation the
// call would otherwise block for its full 3000 s.

const REAL_CALL_LIMIT: Duration = Duration::from_secs(20);

async fn real_call_with(vigil: &Vigil, id: &WatchId, max_s: Option<u64>, orchestrator: Option<bool>) -> Value {
    let request = KeepaliveRequest { watch_id: Some(id.clone()), max_s, orchestrator, confirm: None, session: false };
    let result = tokio::time::timeout(REAL_CALL_LIMIT, vigil.keepalive(request)).await;
    assert!(result.is_ok(), "the call should have ended on the credit event, not run its full window");
    let envelope = result.unwrap().unwrap();
    serde_json::to_value(&envelope).unwrap()
}

async fn real_call(vigil: &Vigil, id: &WatchId) -> Value {
    let request = KeepaliveRequest { watch_id: Some(id.clone()), max_s: Some(3000), orchestrator: Some(true), confirm: None, session: false };
    let result = tokio::time::timeout(REAL_CALL_LIMIT, vigil.keepalive(request)).await;
    assert!(result.is_ok(), "the call should have ended on the credit event, not run its full window");
    let envelope = result.unwrap().unwrap();
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

    let request = KeepaliveRequest { watch_id: Some(id.clone()), max_s: Some(3000), orchestrator: Some(true), confirm: None, session: false };
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

// =====================================================================
// Round 2: spec review amendments S1-S10
// =====================================================================

fn is_attention(event: &Value, reason: &str) -> bool {
    event["kind"] == "attention" && event["reason"] == reason
}

/// W1: `<watch_id>:<tick>:<n>`.
fn is_w1_id(id: &str, watch: &str) -> bool {
    let Some(rest) = id.strip_prefix(&format!("{watch}:")) else { return false };
    let parts: Vec<&str> = rest.split(':').collect();
    parts.len() == 2 && parts.iter().all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

// ------------------------------------------------ S1 / S3: wait ----

// ------------------------------------------------ S2: the full sequence ----

#[tokio::test(start_paused = true)]
async fn the_whole_automatic_sequence_1h_then_credits_then_5m_then_credits_end_then_1h_again() {
    // why: S2 (the blocker of review round 1): nothing in the loop may be done
    //      by the agent. One orchestrator, one 1h transcript, a scripted probe
    //      and a scripted hook that does what the default policy hook does and
    //      what Claude Code then does (a write with the new TTL lands in the
    //      transcript: 5m when the state says on credits, 1h when it says off).
    //      Calls, all `orchestrator: true`, no max_s, zero agent gap:
    //        1  t=0     window 3000 (1h). Start check (no credit-state.json):
    //                   off = baseline. In-call check at 240: on -> hook(on)
    //                   writes a 5m line; event credit_state_changed(on); the
    //                   call ends, waited 240.
    //        2  t=240   window 270 (newest write 5m). Last check 0 s old: no
    //                   start check; a 270 s call has no in-call checks. Quiet:
    //                   waited 270, returns at 510.
    //        3  t=510   window 270. Last check (240) is 270 s old >= 240, so the
    //                   270 s call STILL checks, at its start: off -> hook(off)
    //                   writes a 1h line; event (off); waited 0.
    //        4  t=510   window 3000 again (newest write 1h). Last check 0 s old:
    //                   no start check; in-call checks every 240 s from the last
    //                   one: 240, 480, ..., 2880 = 12 of them; waited 3000.
    //      credit-state.json after each call is hand-derived below.
    // proves: CLAIM(S2), CLAIM(AI2'), CLAIM(AI8)
    let hook_cmd = "claude-cache-ttl-policy";
    let rig = Rig::build(Arc::new(TokioClock::new(spec_instant())), |config| config.on_credit_change = Some(hook_cmd.to_string()));
    let transcript = rig.one_hour_cache();
    let probe = quiet_probe();
    probe.push(credit_state(false, CreditSource::Uw));
    probe.push(credit_state(true, CreditSource::Uw));
    probe.push(credit_state(false, CreditSource::Uw));
    let hook = ScriptedHookRunner::new();
    let appended = transcript.clone();
    hook.set_effect(move |input| {
        let line = if input["on_credits"] == true { assistant_line(0, 500) } else { assistant_line(900, 0) };
        append_file(&appended, &format!("{line}\n"));
    });
    let vigil = rig.vigil_with_hook(claude(), &probe, &hook);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 20_000, |_| {}).await;
    let t0 = Instant::now();

    let first = call(&vigil, &id, None, Some(true)).await;
    assert_eq!((&first["waited_s"], &first["next"]), (&json!(240), &json!("act on events")), "{first}");
    assert_eq!((&first["cache"]["ttl"], &first["cache"]["interval_s"]), (&json!("1h"), &json!(3000)));
    let event = only_event(&first);
    assert_eq!((&event["kind"], &event["on_credits"]), (&json!("credit_state_changed"), &json!(true)));
    assert_eq!(event["hook"]["command"], hook_cmd);
    assert_eq!(rig.read_credit_file(), json!({"on_credits": true, "checked_at": serde_json::to_value(spec_instant().plus_secs(240)).unwrap()}));

    let second = call(&vigil, &id, None, Some(true)).await;
    assert_eq!((&second["waited_s"], &second["events"]), (&json!(270), &json!([])), "{second}");
    assert_eq!(second["cache"]["interval_s"], 270);
    assert_eq!(second["cache"]["ttl"], "5m", "the hook's write moved the newest write to 5m");
    assert_eq!(rig.read_credit_file()["checked_at"], serde_json::to_value(spec_instant().plus_secs(240)).unwrap(), "no check in call 2");

    let third = call(&vigil, &id, None, Some(true)).await;
    assert_eq!(third["waited_s"], 0, "{third}");
    assert_eq!(third["cache"]["interval_s"], 270, "decided before the check, from the 5m write");
    let event = only_event(&third);
    assert_eq!((&event["kind"], &event["on_credits"]), (&json!("credit_state_changed"), &json!(false)));
    assert_eq!(rig.read_credit_file(), json!({"on_credits": false, "checked_at": serde_json::to_value(spec_instant().plus_secs(510)).unwrap()}));

    let fourth = call(&vigil, &id, None, Some(true)).await;
    assert_eq!((&fourth["waited_s"], &fourth["events"]), (&json!(3000), &json!([])), "{fourth}");
    assert_eq!((&fourth["cache"]["ttl"], &fourth["cache"]["interval_s"]), (&json!("1h"), &json!(3000)), "the hook restored 1h");
    let later: Vec<u64> = (1..=12).map(|n| n * 240).collect();
    let all_asks = probe.offsets_from(t0);
    let mut expected_all = vec![0, 240, 510];
    expected_all.extend(later.iter().map(|n| 510 + n));
    assert_eq!(all_asks, expected_all, "probe asks over the four calls");
    assert_eq!(fourth["cache"]["credit_check"]["at"], serde_json::to_value(spec_instant().plus_secs(510 + 2880)).unwrap());

    let runs = hook.calls();
    assert_eq!(runs.len(), 2, "one hook run per change");
    assert_eq!((&runs[0].input["on_credits"], &runs[1].input["on_credits"]), (&json!(true), &json!(false)));
    assert_eq!(hook.offsets_from(t0), vec![240, 510]);
}

#[tokio::test(start_paused = true)]
async fn an_orchestrator_call_of_any_window_checks_at_its_start_when_the_last_check_is_old_enough() {
    // why: S2 "every orchestrator call runs the credit check when the last
    //      check ... is >= 240 s old, whatever its window". The cadence is the
    //      `checked_at` in credit-state.json (written here as another process
    //      would have left it): a file 239 s old -> no check; 240 s old -> one
    //      check at offset 0 (the line is >=); no file at all -> one check. A
    //      short window (explicit 30 s) has no in-call checks. Negative
    //      control: the same old file and a NON-orchestrator 30 s call: no
    //      check (only orchestrators check).
    // proves: CLAIM(S2), CLAIM(S9)
    let run = |age: Option<i64>, orchestrator: Option<bool>| async move {
        let rig = Rig::new();
        let probe = quiet_probe();
        if let Some(age) = age {
            rig.seed_credit_file(Some(false), age);
        }
        let vigil = rig.vigil(AgentEnv::default(), &probe);
        let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 7200, |_| {}).await;
        let started = Instant::now();
        let envelope = call(&vigil, &id, Some(30), orchestrator).await;
        assert_eq!(envelope["waited_s"], 30);
        (probe.offsets_from(started), rig.read_credit_file_opt())
    };
    assert_eq!(run(Some(239), Some(true)).await.0, Vec::<u64>::new(), "239 s old");
    let (offsets, file) = run(Some(240), Some(true)).await;
    assert_eq!(offsets, vec![0], "240 s old");
    assert_eq!(file.expect("the check was recorded")["checked_at"], serde_json::to_value(spec_instant()).unwrap());
    assert_eq!(run(None, Some(true)).await.0, vec![0], "no file yet");
    assert_eq!(run(Some(5000), None).await.0, Vec::<u64>::new(), "control: not an orchestrator, not a long window");
}

#[tokio::test(start_paused = true)]
async fn long_calls_keep_the_cadence_of_the_last_check_across_calls_and_processes() {
    // why: S2 + S9. Call 1 (explicit 3000 s) checks at 0, 240, ..., 2880 and
    //      ends at 3000. Call 2, from a FRESH process (a new CLI run: no memory,
    //      its own probe), starts 120 s after the last check, so its first check
    //      is 120 s in (2880 + 240 = 3120 = 3000 + 120) and then every 240 s:
    //      120, 360, ..., 2760 (120 + 240 k < 3000, k = 0..11 = 12 checks). If
    //      the cadence restarted at each call these would be 0, 240, ... .
    // proves: CLAIM(S2), CLAIM(S9)
    let rig = Rig::new();
    let first_probe = quiet_probe();
    let first = rig.vigil(AgentEnv::default(), &first_probe);
    let (id, _) = quiet_watch(&rig, &first, "w", 600, 20_000, |_| {}).await;
    call(&first, &id, Some(3000), Some(true)).await;
    assert_eq!(first_probe.asked_count(), 13);
    let second_probe = quiet_probe();
    let second = rig.vigil(AgentEnv::default(), &second_probe);
    let started = Instant::now();
    call(&second, &id, Some(3000), Some(true)).await;
    let expected: Vec<u64> = (0..12).map(|n| 120 + n * 240).collect();
    assert_eq!(second_probe.offsets_from(started), expected);
}

#[tokio::test]
async fn the_sequence_also_runs_with_a_real_hook_process_on_the_real_clock() {
    // why: S2 with a real `sh` hook (the process boundary the paused test
    //      replaces by a scripted runner). `credit_check_every` is 200 ms; the
    //      hook reads its stdin JSON and appends a 5m line (state on) or a 1h
    //      line (state off) to the session transcript, like the default policy
    //      plus Claude Code. Calls: (1) 1h -> window 3000; checks at 0 (off,
    //      baseline) and ~200 ms (on): event, hook run, call ends. (2) after
    //      250 ms: window 270 (5m); the start check is due, off: event, hook
    //      writes 1h. (3) window 3000 again; the in-call check says on: event.
    //      Every event carries the hook's exit code 0.
    // proves: CLAIM(S2), CLAIM(AI8)
    let scratch = tempfile::tempdir().unwrap();
    let (five, one) = (scratch.path().join("five.line"), scratch.path().join("one.line"));
    std::fs::write(&five, format!("{}\n", assistant_line(0, 500))).unwrap();
    std::fs::write(&one, format!("{}\n", assistant_line(900, 0))).unwrap();
    let rig = Rig::real_time(|config| {
        config.credit_check_every = Duration::from_millis(200);
        let transcript = config.claude_projects_dir.clone().unwrap().join("p/sess-1.jsonl");
        config.on_credit_change = Some(format!(
            "if grep -Eq '\"on_credits\": ?true'; then cat '{}' >> '{}'; else cat '{}' >> '{}'; fi",
            five.display(),
            transcript.display(),
            one.display(),
            transcript.display()
        ));
    });
    rig.one_hour_cache();
    let probe = quiet_probe();
    probe.push(credit_state(false, CreditSource::Uw));
    probe.push(credit_state(true, CreditSource::Uw));
    probe.push(credit_state(false, CreditSource::Uw));
    probe.set_steady(credit_state(true, CreditSource::Uw));
    let vigil = rig.vigil(claude(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 3600, 7200, |_| {}).await;

    let first = real_call_with(&vigil, &id, None, Some(true)).await;
    assert_eq!((&first["cache"]["interval_s"], &only_event(&first)["on_credits"]), (&json!(3000), &json!(true)), "{first}");
    assert_eq!(only_event(&first)["hook"]["exit_code"], 0, "{first}");
    tokio::time::sleep(Duration::from_millis(250)).await;
    let second = real_call_with(&vigil, &id, None, Some(true)).await;
    assert_eq!((&second["cache"]["interval_s"], &only_event(&second)["on_credits"]), (&json!(270), &json!(false)), "{second}");
    assert_eq!(only_event(&second)["hook"]["exit_code"], 0, "{second}");
    let third = real_call_with(&vigil, &id, None, Some(true)).await;
    assert_eq!((&third["cache"]["interval_s"], &only_event(&third)["on_credits"]), (&json!(3000), &json!(true)), "{third}");
}

// ------------------------------------------------ S4: ambiguous identity ----

#[tokio::test(start_paused = true)]
async fn an_orchestrator_with_both_a_claude_and_a_codex_signal_keeps_270_s_but_still_checks_credits() {
    // why: S4 at the engine. The agent value holds a Claude session id AND a
    //      Codex signal (either nesting direction produces it): the window is
    //      270 s, unknown / default, whatever the 1h transcript says. The
    //      orchestrator still runs its start credit check (ambiguity is about
    //      the TTL, not about monitoring). Positive control: the same rig with
    //      the Claude signal alone waits 3000 s.
    // proves: CLAIM(S4), CLAIM(S2)
    let rig = Rig::new();
    rig.one_hour_cache();
    let probe = quiet_probe();
    let ambiguous = rig.vigil(AgentEnv { claude_session_id: Some("sess-1".into()), codex: true, antigravity: false }, &probe);
    let (id, _) = quiet_watch(&rig, &ambiguous, "w", 600, 20_000, |_| {}).await;
    let envelope = call(&ambiguous, &id, None, Some(true)).await;
    assert_eq!(envelope["waited_s"], 270);
    assert_eq!(envelope["cache"]["ttl"], "unknown");
    assert_eq!(envelope["cache"]["source"], "default");
    assert_eq!(probe.asked_count(), 1, "one start check");

    let control_probe = quiet_probe();
    let claude_only = rig.vigil(claude(), &control_probe);
    let control = call(&claude_only, &id, None, Some(true)).await;
    assert_eq!((&control["waited_s"], &control["cache"]["ttl"]), (&json!(3000), &json!("1h")), "control");
}

#[tokio::test(start_paused = true)]
async fn antigravity_calls_wait_150_s_on_every_path_and_mixed_signals_are_bounded_too() {
    // why: engine-level proof that Vigil::keepalive honours the 150 s bound
    //      (agy kills an MCP call at 180 s) for an orchestrator, a subagent
    //      and an explicit max_s, and that Antigravity mixed with Claude or
    //      Codex keeps the bound instead of the old 270 s ambiguous window.
    //      Controls: Claude+Codex without Antigravity still waits 270 s.
    // catches: choosing 150 s in choose_window but clamping the call budget
    //          back to 270 s inside keepalive; leaving the non-orchestrator
    //          default or the mixed identity at 270 s.
    // proves: CLAIM(AI1'), CLAIM(AI3), CLAIM(S4), CLAIM(neon-mcp-bound)
    let rig = Rig::new();
    rig.one_hour_cache();
    let probe = quiet_probe();
    let agy = rig.vigil(antigravity(), &probe);
    let (id, _) = quiet_watch(&rig, &agy, "w", 600, 20_000, |_| {}).await;

    let orch = call(&agy, &id, None, Some(true)).await;
    assert_eq!(orch["waited_s"], 150);
    assert_eq!(orch["max_s"], 150);
    assert_eq!(cache_without_start_check(&orch), json!({"ttl": "5m", "source": "antigravity-measured", "interval_s": 150}));
    assert_eq!(probe.asked_count(), 1, "one start check, no mid-call recheck for 150 s");

    let sub = call(&agy, &id, None, Some(false)).await;
    assert_eq!(sub["waited_s"], 150);
    assert_eq!(sub["max_s"], 150);
    assert_eq!(sub["cache"], json!({"ttl": "unknown", "source": "default", "interval_s": 150}));

    let explicit = call(&agy, &id, Some(9999), Some(false)).await;
    assert_eq!((&explicit["waited_s"], &explicit["max_s"]), (&json!(150), &json!(150)));

    let mixed_claude = rig.vigil(AgentEnv { claude_session_id: Some("sess-1".into()), codex: false, antigravity: true }, &quiet_probe());
    let env_mc = call(&mixed_claude, &id, None, Some(true)).await;
    assert_eq!(env_mc["waited_s"], 150);
    assert_eq!(env_mc["cache"]["ttl"], "unknown");
    assert_eq!(env_mc["cache"]["source"], "default");

    let mixed_codex = rig.vigil(AgentEnv { claude_session_id: None, codex: true, antigravity: true }, &quiet_probe());
    let env_mx = call(&mixed_codex, &id, None, Some(true)).await;
    assert_eq!(env_mx["waited_s"], 150);
    assert_eq!(env_mx["cache"]["ttl"], "unknown");
    assert_eq!(env_mx["cache"]["source"], "default");

    let claude_and_codex = rig.vigil(AgentEnv { claude_session_id: Some("sess-1".into()), codex: true, antigravity: false }, &quiet_probe());
    let control = call(&claude_and_codex, &id, None, Some(true)).await;
    assert_eq!(control["waited_s"], 270, "control: ambiguity without Antigravity keeps 270 s");
}

fn write_advisories_toml(entries: &[(&str, &str, u64)]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("vigil/advisories.toml");
    let mut toml = String::new();
    for (id, cmd, cache_s) in entries {
        toml.push_str(&format!(
            "[advisories.{id}]\nprobe = {{ kind = \"cmd\", argv = [\"{cmd}\"] }}\nextract = {{ name = \"free\", json = \"free\" }}\ncondition = \"free < floor\"\nmessage_when_true = \"Low: {{free}}\"\nmessage_when_false = \"Plenty: {{free}}\"\ncache_s = {cache_s}\n[advisories.{id}.params]\nfloor = 20\n"
        ));
    }
    write_file(&file, &toml);
    (dir, file)
}

#[tokio::test(start_paused = true)]
async fn antigravity_keepalive_delivers_all_real_advice_entries_when_probes_fit_within_the_tail_budget() {
    // why: the advice tail budget (15 s) must let probes that fit serially
    //      deliver real `kind: advice` entries with their extracted values.
    //      3 probes x 4 s = 12 s <= 15 s; 150 s window + 12 s tail = 162 s < 180 s.
    // catches: setting ADVICE_TAIL_BUDGET to 0 or 1 s, dropping advice on
    //          Antigravity, or replacing real advice with an unavailable stub.
    // proves: CLAIM(neon-mcp-bound), CLAIM(advisory-delivery)
    let (_dir, file) = write_advisories_toml(&[("a1", "p1", 1), ("a2", "p2", 1), ("a3", "p3", 1)]);
    let rig = Rig::build(Arc::new(TokioClock::new(spec_instant())), |config| config.advisories_file = Some(file.clone()));
    for (cmd, value) in [("cmd:p1", 11), ("cmd:p2", 22), ("cmd:p3", 33)] {
        rig.env.reader.set_text(cmd, &format!("exit=0\n{{\"free\": {value}}}"));
        rig.env.reader.set_sleep(cmd, Duration::from_secs(4));
    }
    let agy = rig.vigil(antigravity(), &quiet_probe());
    let (id, _) = quiet_watch(&rig, &agy, "w", 600, 20_000, |_| {}).await;
    let started = Instant::now();
    let envelope = call(&agy, &id, None, Some(true)).await;
    let elapsed = started.elapsed();
    assert_eq!(envelope["max_s"], 150);
    assert_eq!(elapsed, Duration::from_secs(162), "150 s window + 3 x 4 s serial probes");
    let advice = envelope["advice"].as_array().expect("advice array");
    assert_eq!(advice.len(), 3, "{advice:?}");
    let got: Vec<(&str, &str, Option<f64>, &str)> = advice
        .iter()
        .map(|e| (e["id"].as_str().unwrap(), e["kind"].as_str().unwrap(), e["value"].as_f64(), e["message"].as_str().unwrap()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("a1", "advice", Some(11.0), "Low: 11"),
            ("a2", "advice", Some(22.0), "Plenty: 22"),
            ("a3", "advice", Some(33.0), "Plenty: 33"),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn an_antigravity_call_returns_in_under_180_s_even_when_every_advisory_probe_is_slow() {
    // why: agy kills an MCP call at 3m0s. The window is 150 s, and advisories
    //      run serially under a shared ADVICE_TAIL_BUDGET (15 s) deadline
    //      passed into `advise`: each probe gets min(its own timeout, time
    //      remaining). Finished probes MUST survive as `kind: advice` with
    //      their values; probes cut off or not reached become individual
    //      `unavailable` entries naming their ids (never a blanket drop of
    //      earlier results). Tested on all 4 Antigravity paths plus a Claude
    //      caller (no regression), and on an all-slow set.
    // catches: no cap on the advice step; an outer timeout that drops partial
    //          results into one blanket `unavailable` entry; starting probes
    //          after the budget is spent; ADVICE_TAIL_BUDGET = 0 or 1 s.
    // proves: CLAIM(neon-mcp-bound), CLAIM(advisory-delivery)
    let (_dir, file) = write_advisories_toml(&[("fast1", "p1", 1), ("slow2", "p2", 1), ("unreached3", "p3", 1)]);
    let rig = Rig::build(Arc::new(TokioClock::new(spec_instant())), |config| config.advisories_file = Some(file.clone()));
    rig.env.reader.set_text("cmd:p1", "exit=0\n{\"free\": 15}");
    rig.env.reader.set_sleep("cmd:p1", Duration::from_secs(4));
    rig.env.reader.set_text("cmd:p2", "exit=0\n{\"free\": 25}");
    rig.env.reader.set_sleep("cmd:p2", Duration::from_secs(19));
    rig.env.reader.set_text("cmd:p3", "exit=0\n{\"free\": 35}");
    rig.env.reader.set_sleep("cmd:p3", Duration::from_secs(4));

    let assert_partial = |advice: &[Value], context: &str| {
        assert_eq!(advice.len(), 3, "{context}: {advice:?}");
        assert_eq!((&advice[0]["id"], &advice[0]["kind"], &advice[0]["value"], &advice[0]["message"]), (&json!("fast1"), &json!("advice"), &json!(15.0), &json!("Low: 15")), "{context}: {advice:?}");
        assert_eq!((&advice[1]["id"], &advice[1]["kind"]), (&json!("slow2"), &json!("unavailable")), "{context}: {advice:?}");
        assert!(advice[1].get("value").is_none(), "{context}: {advice:?}");
        assert!(advice[1]["message"].as_str().unwrap().starts_with("advisory slow2 unavailable:"), "{context}: {}", advice[1]["message"]);
        assert_eq!((&advice[2]["id"], &advice[2]["kind"]), (&json!("unreached3"), &json!("unavailable")), "{context}: {advice:?}");
        assert!(advice[2].get("value").is_none(), "{context}: {advice:?}");
        assert!(advice[2]["message"].as_str().unwrap().starts_with("advisory unreached3 unavailable:"), "{context}: {}", advice[2]["message"]);
    };

    let agy = rig.vigil(antigravity(), &quiet_probe());
    let (id, _) = quiet_watch(&rig, &agy, "w", 600, 20_000, |_| {}).await;
    for (explicit_s, orchestrator) in [(None, Some(true)), (None, Some(false)), (Some(9999), Some(true)), (Some(9999), Some(false))] {
        let started = Instant::now();
        let envelope = call(&agy, &id, explicit_s, orchestrator).await;
        let elapsed = started.elapsed();
        let path = format!("explicit {explicit_s:?}, orchestrator {orchestrator:?}");
        assert_eq!(envelope["max_s"], 150, "{path}");
        assert_eq!(elapsed, Duration::from_secs(165), "{path}: 150 s window + 15 s advice budget");
        assert_partial(envelope["advice"].as_array().expect("advice array"), &path);
        assert_eq!(rig.env.reader.started_count("cmd:p3"), 0, "{path}: unreached probe must not start");
    }

    // Claude caller keeps partial results too (never a blanket unavailable).
    let claude_v = rig.vigil(claude(), &quiet_probe());
    let claude_started = Instant::now();
    let claude_env = call(&claude_v, &id, Some(30), Some(false)).await;
    assert_eq!(claude_started.elapsed(), Duration::from_secs(45), "30 s window + 15 s advice budget");
    assert_partial(claude_env["advice"].as_array().expect("advice array"), "claude partial survival");
    assert_eq!(rig.env.reader.started_count("cmd:p3"), 0, "claude: unreached probe must not start");

    // All three probes slow: every advisory is named individually as unavailable and < 180 s.
    let (_slow_dir, slow_file) = write_advisories_toml(&[("slow1", "s1", 1), ("slow2", "s2", 1), ("slow3", "s3", 1)]);
    let slow_rig = Rig::build(Arc::new(TokioClock::new(spec_instant())), |config| config.advisories_file = Some(slow_file.clone()));
    for n in 1..=3 {
        slow_rig.env.reader.set_text(&format!("cmd:s{n}"), "exit=0\n{\"free\": 15}");
        slow_rig.env.reader.set_sleep(&format!("cmd:s{n}"), Duration::from_secs(19));
    }
    let slow_agy = slow_rig.vigil(antigravity(), &quiet_probe());
    let (slow_id, _) = quiet_watch(&slow_rig, &slow_agy, "ws", 600, 20_000, |_| {}).await;
    let started = Instant::now();
    let slow_env = call(&slow_agy, &slow_id, None, Some(true)).await;
    assert_eq!(started.elapsed(), Duration::from_secs(165));
    let slow_advice = slow_env["advice"].as_array().expect("advice array");
    assert_eq!(slow_advice.len(), 3, "{slow_advice:?}");
    for (idx, name) in ["slow1", "slow2", "slow3"].iter().enumerate() {
        assert_eq!((&slow_advice[idx]["id"], &slow_advice[idx]["kind"]), (&json!(name), &json!("unavailable")), "{slow_advice:?}");
        assert!(slow_advice[idx]["message"].as_str().unwrap().starts_with(&format!("advisory {name} unavailable:")), "{slow_advice:?}");
    }
    assert_eq!((slow_rig.env.reader.started_count("cmd:s2"), slow_rig.env.reader.started_count("cmd:s3")), (0, 0));
}

#[tokio::test(start_paused = true)]
async fn antigravity_advice_report_respects_the_tail_budget_and_preserves_partial_results() {
    // why: the MCP `advice` tool (Vigil::advice_report) also runs inside agy's
    //      180 s tool-call timeout. For any Antigravity caller (alone or mixed
    //      with Claude/Codex), advice_report passes the same ADVICE_TAIL_BUDGET
    //      deadline into `advise`, keeping finished entries and naming unreached
    //      ones as unavailable. Control: Claude and Codex callers have no 15 s
    //      cap on standalone advice_report so slow probes (> 15 s total) succeed.
    // catches: leaving advice_report unbounded for Antigravity, dropping partial
    //          results in advice_report, or regressing Claude/Codex advice_report.
    // proves: CLAIM(neon-mcp-bound), CLAIM(advisory-delivery)
    let (_dir, file) = write_advisories_toml(&[("fast1", "p1", 1), ("slow2", "p2", 1), ("unreached3", "p3", 1)]);
    let rig = Rig::build(Arc::new(TokioClock::new(spec_instant())), |config| config.advisories_file = Some(file.clone()));
    rig.env.reader.set_text("cmd:p1", "exit=0\n{\"free\": 15}");
    rig.env.reader.set_sleep("cmd:p1", Duration::from_secs(4));
    rig.env.reader.set_text("cmd:p2", "exit=0\n{\"free\": 25}");
    rig.env.reader.set_sleep("cmd:p2", Duration::from_secs(19));
    rig.env.reader.set_text("cmd:p3", "exit=0\n{\"free\": 35}");
    rig.env.reader.set_sleep("cmd:p3", Duration::from_secs(4));

    for (label, agent) in [
        ("antigravity", antigravity()),
        ("mixed claude+antigravity", AgentEnv { claude_session_id: Some("sess-1".into()), codex: false, antigravity: true }),
    ] {
        // Expire any cached fast1 entry between cases.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let vigil = rig.vigil(agent, &quiet_probe());
        let started = Instant::now();
        let report = vigil.advice_report().await.unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(15), "{label}");
        let advice = report["advice"].as_array().expect("advice array");
        assert_eq!(advice.len(), 3, "{label}: {advice:?}");
        assert_eq!((&advice[0]["id"], &advice[0]["kind"], &advice[0]["value"]), (&json!("fast1"), &json!("advice"), &json!(15.0)), "{label}");
        assert_eq!((&advice[1]["id"], &advice[1]["kind"]), (&json!("slow2"), &json!("unavailable")), "{label}");
        assert_eq!((&advice[2]["id"], &advice[2]["kind"]), (&json!("unreached3"), &json!("unavailable")), "{label}");
        assert_eq!(rig.env.reader.started_count("cmd:p3"), 0, "{label}: unreached3 must not start");
    }

    // 10 slow advisories (190 s unbounded > 180 s): still returns in 15 s under Antigravity.
    let many_entries: Vec<(String, String, u64)> = (1..=10).map(|n| (format!("s{n}"), format!("m{n}"), 1)).collect();
    let many_refs: Vec<(&str, &str, u64)> = many_entries.iter().map(|(id, cmd, c)| (id.as_str(), cmd.as_str(), *c)).collect();
    let (_many_dir, many_file) = write_advisories_toml(&many_refs);
    let many_rig = Rig::build(Arc::new(TokioClock::new(spec_instant())), |config| config.advisories_file = Some(many_file.clone()));
    for n in 1..=10 {
        many_rig.env.reader.set_text(&format!("cmd:m{n}"), "exit=0\n{\"free\": 15}");
        many_rig.env.reader.set_sleep(&format!("cmd:m{n}"), Duration::from_secs(19));
    }
    let started = Instant::now();
    let report = many_rig.vigil(antigravity(), &quiet_probe()).advice_report().await.unwrap();
    assert_eq!(started.elapsed(), Duration::from_secs(15));
    let many_advice = report["advice"].as_array().expect("advice array");
    assert_eq!(many_advice.len(), 10);
    for n in 1..=10 {
        assert_eq!((&many_advice[n - 1]["id"], &many_advice[n - 1]["kind"]), (&json!(format!("s{n}")), &json!("unavailable")));
    }

    // Positive control: Claude and Codex standalone advice_report has no 15 s cap.
    let (_ctl_dir, ctl_file) = write_advisories_toml(&[("first", "c1", 1), ("second", "c2", 1)]);
    let ctl_rig = Rig::build(Arc::new(TokioClock::new(spec_instant())), |config| config.advisories_file = Some(ctl_file.clone()));
    ctl_rig.env.reader.set_text("cmd:c1", "exit=0\n{\"free\": 10}");
    ctl_rig.env.reader.set_sleep("cmd:c1", Duration::from_secs(10));
    ctl_rig.env.reader.set_text("cmd:c2", "exit=0\n{\"free\": 30}");
    ctl_rig.env.reader.set_sleep("cmd:c2", Duration::from_secs(10));
    for (label, agent) in [("claude", claude()), ("codex", codex())] {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let started = Instant::now();
        let report = ctl_rig.vigil(agent, &quiet_probe()).advice_report().await.unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(20), "{label}: both 10 s probes finish");
        let advice = report["advice"].as_array().expect("advice array");
        assert_eq!(advice.len(), 2, "{label}");
        assert_eq!((&advice[0]["kind"], &advice[0]["value"]), (&json!("advice"), &json!(10.0)), "{label}");
        assert_eq!((&advice[1]["kind"], &advice[1]["value"]), (&json!("advice"), &json!(30.0)), "{label}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_probe_cut_off_by_the_advice_budget_does_not_poison_the_failure_cache() {
    // why: if probe 1 spends 12 s and probe 2 only gets 3 s remaining out of
    //      the 15 s budget, probe 2 is not broken. Writing a 30 s failure
    //      cache record for probe 2 would keep it unavailable on the next call
    //      even though probe 1 is now warm in cache and probe 2 has the full
    //      15 s. Also verify a warm cache hit AFTER a slow probe still returns
    //      its cached value.
    // catches: caching shared-deadline cut-offs in ProbeCache, or skipping
    //          cache reads once remaining time hits zero.
    // proves: CLAIM(neon-mcp-bound), CLAIM(advisory-cache)
    let (_dir, file) = write_advisories_toml(&[("first", "p1", 100), ("second", "p2", 100)]);
    let rig = Rig::build(Arc::new(TokioClock::new(spec_instant())), |config| config.advisories_file = Some(file.clone()));
    rig.env.reader.set_text("cmd:p1", "exit=0\n{\"free\": 15}");
    rig.env.reader.set_sleep("cmd:p1", Duration::from_secs(12));
    rig.env.reader.set_text("cmd:p2", "exit=0\n{\"free\": 25}");
    rig.env.reader.set_sleep("cmd:p2", Duration::from_secs(10));

    let agy = rig.vigil(antigravity(), &quiet_probe());
    let first_call = agy.advice_report().await.unwrap();
    let a1 = first_call["advice"].as_array().unwrap();
    assert_eq!((&a1[0]["id"], &a1[0]["kind"], &a1[0]["value"]), (&json!("first"), &json!("advice"), &json!(15.0)));
    assert_eq!((&a1[1]["id"], &a1[1]["kind"]), (&json!("second"), &json!("unavailable")));

    // Immediately call again (inside FAILURE_CACHE_S = 30 s): first is cached, second runs and succeeds.
    let started = Instant::now();
    let second_call = agy.advice_report().await.unwrap();
    assert_eq!(started.elapsed(), Duration::from_secs(10));
    let a2 = second_call["advice"].as_array().unwrap();
    assert_eq!((&a2[0]["id"], &a2[0]["kind"], &a2[0]["value"]), (&json!("first"), &json!("advice"), &json!(15.0)));
    assert_eq!((&a2[1]["id"], &a2[1]["kind"], &a2[1]["value"]), (&json!("second"), &json!("advice"), &json!(25.0)));

    // Warm cache hit AFTER a slow probe that consumes the whole 15 s budget still survives.
    let (_dir2, file2) = write_advisories_toml(&[("slow", "s1", 1), ("warm", "s2", 100)]);
    let rig2 = Rig::build(Arc::new(TokioClock::new(spec_instant())), |config| config.advisories_file = Some(file2.clone()));
    rig2.env.reader.set_text("cmd:s1", "exit=0\n{\"free\": 10}");
    rig2.env.reader.set_sleep("cmd:s1", Duration::from_secs(1));
    rig2.env.reader.set_text("cmd:s2", "exit=0\n{\"free\": 42}");
    let agy2 = rig2.vigil(antigravity(), &quiet_probe());
    // Seed warm cache.
    let seed = agy2.advice_report().await.unwrap();
    assert_eq!(seed["advice"][1]["value"], 42.0);
    // Let slow expire (cache_s = 1) while warm stays fresh (cache_s = 100), then make slow take 19 s.
    tokio::time::sleep(Duration::from_secs(2)).await;
    rig2.env.reader.set_sleep("cmd:s1", Duration::from_secs(19));
    let started = Instant::now();
    let report = agy2.advice_report().await.unwrap();
    assert_eq!(started.elapsed(), Duration::from_secs(15));
    let a = report["advice"].as_array().unwrap();
    assert_eq!((&a[0]["id"], &a[0]["kind"]), (&json!("slow"), &json!("unavailable")));
    assert_eq!((&a[1]["id"], &a[1]["kind"], &a[1]["value"]), (&json!("warm"), &json!("advice"), &json!(42.0)));
}

// ------------------------------------------------ S5: tri-state ----

#[tokio::test(start_paused = true)]
async fn unknown_checks_never_make_an_event_or_a_hook_and_keep_the_last_state() {
    // why: S5. For EACH known state, unknown checks at 240 and 480 keep it
    //      and do not run the hook. After the full 3000s call, advance 120s
    //      to the next due check (last check 2880, due 3120, call ended 3000) and flip the known state: one event/hook.
    //      Both starting states catch implementations coercing null to bool.
    // proves: CLAIM(S5)
    for known in [false, true] {
        let rig = Rig::build(Arc::new(TokioClock::new(spec_instant())), |c| c.on_credit_change = Some("policy".into()));
        rig.seed_credit_file(Some(known), 1000);
        let probe = ScriptedCreditProbe::new(credit_state(known, CreditSource::Uw));
        probe.push(credit_state(known, CreditSource::Uw));
        probe.push(credit_unknown(CreditSource::Transcripts));
        probe.push(credit_unknown(CreditSource::Transcripts));
        let hook = ScriptedHookRunner::new();
        let vigil = rig.vigil_with_hook(claude(), &probe, &hook);
        let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 7200, |_| {}).await;
        let answer = call(&vigil, &id, Some(3000), Some(true)).await;
        assert_eq!((&answer["waited_s"], &answer["events"]), (&json!(3000), &json!([])));
        assert_eq!(probe.asked_count(), 13);
        assert_eq!(hook.calls().len(), 0);
        assert_eq!(rig.read_credit_file()["on_credits"], known);
        tokio::time::sleep(Duration::from_secs(120)).await;
        probe.set_steady(credit_state(!known, CreditSource::Uw));
        let changed = call(&vigil, &id, Some(30), Some(true)).await;
        assert_eq!(only_event(&changed)["on_credits"], !known);
        assert_eq!(hook.calls().len(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn an_unknown_answer_is_recorded_as_a_check_and_reported_as_null() {
    // why: S5. An orchestrator 271 s call checks at 0 (off) and 240 (unknown). The newest
    //      check is the unknown one: cache.credit_check = {at: +240, source:
    //      transcripts, on_credits: null}, and credit-state.json keeps
    //      on_credits false but moves checked_at to +240 (an unknown check is
    //      still a check; otherwise a failing `uw` would be asked on every
    //      call).
    // proves: CLAIM(S5), CLAIM(S9)
    let rig = Rig::new();
    let probe = quiet_probe();
    probe.push(credit_state(false, CreditSource::Uw));
    probe.push(credit_unknown(CreditSource::Transcripts));
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 7200, |_| {}).await;
    let envelope = call(&vigil, &id, Some(271), Some(true)).await;
    assert_eq!(
        envelope["cache"]["credit_check"],
        json!({"at": serde_json::to_value(spec_instant().plus_secs(240)).unwrap(), "source": "transcripts", "on_credits": null})
    );
    assert_eq!(
        rig.read_credit_file(),
        json!({"on_credits": false, "checked_at": serde_json::to_value(spec_instant().plus_secs(240)).unwrap()})
    );
}

#[tokio::test(start_paused = true)]
async fn a_change_is_still_reported_once_across_unknown_checks() {
    // why: S5 "keeps the last known state". Recorded: on. Checks: unknown (0),
    //      off (240). The change on -> off is seen at 240 and reported once with
    //      the hook run once (state off). If unknown had cleared the recorded
    //      state, the off at 240 would be taken as a baseline and nothing would
    //      be reported; if unknown were read as off, the event would come at 0.
    // proves: CLAIM(S5), CLAIM(AI8)
    let rig = Rig::build(Arc::new(TokioClock::new(spec_instant())), |config| config.on_credit_change = Some("policy".into()));
    rig.seed_credit_file(Some(true), 1000);
    let probe = quiet_probe();
    probe.push(credit_unknown(CreditSource::Uw));
    probe.push(credit_state(false, CreditSource::Uw));
    let hook = ScriptedHookRunner::new();
    let vigil = rig.vigil_with_hook(AgentEnv::default(), &probe, &hook);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 7200, |_| {}).await;
    let envelope = call(&vigil, &id, Some(3000), Some(true)).await;
    assert_eq!(envelope["waited_s"], 240, "{envelope}");
    assert_eq!(only_event(&envelope)["on_credits"], false);
    assert_eq!(hook.calls().len(), 1);
    assert_eq!(hook.calls()[0].input["on_credits"], false);
}

#[tokio::test(start_paused = true)]
async fn when_no_state_was_ever_known_the_first_known_answer_is_the_baseline() {
    // why: S5 x A9/S9 (R2-A3). No credit-state.json; the first check is unknown
    //      (file gets on_credits null), the second says on. There is nothing
    //      known to differ from, so on is the baseline: no event, no hook, the
    //      call runs its 3000 s, and the file now says on.
    // proves: CLAIM(S5), CLAIM(S9)
    let rig = Rig::build(Arc::new(TokioClock::new(spec_instant())), |config| config.on_credit_change = Some("policy".into()));
    let probe = ScriptedCreditProbe::new(credit_state(true, CreditSource::Uw));
    probe.push(credit_unknown(CreditSource::Transcripts));
    let hook = ScriptedHookRunner::new();
    let vigil = rig.vigil_with_hook(AgentEnv::default(), &probe, &hook);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 7200, |_| {}).await;
    let envelope = call(&vigil, &id, Some(3000), Some(true)).await;
    assert_eq!((&envelope["waited_s"], &envelope["events"]), (&json!(3000), &json!([])), "{envelope}");
    assert_eq!(hook.calls().len(), 0);
    assert_eq!(rig.read_credit_file()["on_credits"], true);
}

// ------------------------------------------------ S6: attention events ----

#[tokio::test(start_paused = true)]
async fn each_attention_reason_fires_once_and_the_ids_are_distinct_and_w1_shaped() {
    // why: S6 "generated with distinct ids". A judge watch with a live process,
    //      expect_s 40 (overdue at min(60, 580) = 60) and stall_s 60 (stalled at
    //      the tick at 60): both reasons are due at the same instant and may
    //      come in one envelope or two. Over at most three default calls the
    //      agent must see exactly two events, kind attention, reasons {overdue,
    //      stalled} once each, ids `w:<tick>:<n>` (W1) that differ from each
    //      other (two events at one tick must not both be `w:4:0`), and after
    //      that a full quiet 270 s call: neither fires again.
    // proves: CLAIM(S6), CLAIM(AI5)
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let sleeper = Sleeper::start();
    let (id, _) = judge_watch(&rig, &vigil, "w", 20, 7200, |request| {
        request.expect_s = Some(40);
        request.stall_s = Some(60);
        request.inputs.push(Input::Process { pid: sleeper.pid(), want: None });
    })
    .await;
    let mut seen: Vec<Value> = Vec::new();
    for _ in 0..3 {
        let envelope = call(&vigil, &id, None, None).await;
        seen.extend(envelope["events"].as_array().unwrap().iter().cloned());
        if seen.len() >= 2 {
            break;
        }
    }
    assert_eq!(seen.len(), 2, "{seen:?}");
    let mut reasons: Vec<&str> = seen.iter().map(|event| event["reason"].as_str().unwrap_or("?")).collect();
    reasons.sort_unstable();
    assert_eq!(reasons, ["overdue", "stalled"]);
    assert!(seen.iter().all(|event| event["kind"] == "attention"), "{seen:?}");
    let ids: Vec<&str> = seen.iter().map(|event| event["event_id"].as_str().unwrap()).collect();
    assert!(ids.iter().all(|id| is_w1_id(id, "w")), "{ids:?}");
    assert_ne!(ids[0], ids[1], "two events of one watch share an id: {ids:?}");
    let quiet = call(&vigil, &id, None, None).await;
    assert_eq!((&quiet["waited_s"], &quiet["events"]), (&json!(270), &json!([])), "{quiet}");
}

#[tokio::test(start_paused = true)]
async fn an_attention_event_whose_save_failed_comes_again_flagged_redelivered_with_the_same_id() {
    // why: S6 "persisted with the tick (the one-shot flag is saved with them) ...
    //      redelivered with `redelivered: true` after a failed save". Watch
    //      cadence 600, expect_s 60 (overdue at 90). Call A (healthy, 30 s)
    //      ticks at 0. Call B runs on a Vigil whose every write fails (a full
    //      disk): at t = 90 the attention is raised, its save fails, B returns
    //      it at waited 60 with persist.ok false and persist.redelivery true
    //      (an at-risk envelope, DF3a) and NO redelivered flag (it is the first
    //      delivery). Restore writes in the SAME process. The flag was never
    //      saved, so the attention is due again at once (waited 0): the same
    //      event_id, redelivered true, no persist note. Call D: saved this time,
    //      so a quiet 270 s with no events.
    // proves: CLAIM(S6), CLAIM(DF3)
    let rig = Rig::new();
    let probe = quiet_probe();
    let healthy = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &healthy, "w", 600, 7200, |request| request.expect_s = Some(60)).await;
    let a = call(&healthy, &id, Some(30), None).await;
    assert_eq!((&a["waited_s"], &a["events"]), (&json!(30), &json!([])), "{a}");

    let marker = rig.env.input_path("full");
    write_file(&marker, "full");
    let faults = FaultPlan::from_rules(vec![FaultRule::new(WriteKind::Watch, 5, When::WhileExists(marker.clone()))]);
    let faulted = rig.vigil(AgentEnv::default(), &probe).with_write_faults(faults);
    let b = call(&faulted, &id, None, None).await;
    assert_eq!(b["waited_s"], 60, "{b}");
    let first = only_event(&b);
    assert!(is_attention(first, "overdue"), "{b}");
    assert_eq!((&b["persist"]["ok"], &b["persist"]["redelivery"]), (&json!(false), &json!(true)), "{b}");
    assert!(first.get("redelivered").is_none(), "the first delivery is not a repeat: {first}");
    let first_id = first["event_id"].as_str().unwrap().to_string();
    assert!(is_w1_id(&first_id, "w"), "{first_id}");

    std::fs::remove_file(marker).unwrap();
    let c = call(&faulted, &id, None, None).await;
    assert_eq!(c["waited_s"], 0, "{c}");
    let again = only_event(&c);
    assert_eq!(again["event_id"], first_id.as_str());
    assert_eq!(again["redelivered"], true);
    assert!(c.get("persist").is_none(), "{c}");
    let d = call(&faulted, &id, None, None).await;
    assert_eq!((&d["waited_s"], &d["events"]), (&json!(270), &json!([])), "{d}");
}

#[tokio::test(start_paused = true)]
async fn an_attention_event_that_never_reached_the_caller_comes_again_until_it_is_delivered() {
    // why: S6 "redelivered ... after a cancellation before delivery", and
    //      "delivered and acknowledged like other events (commit_delivery)".
    //      The MCP layer builds the envelope, the client cancels, the Delivery
    //      is dropped uncommitted (`keepalive_uncommitted`, as in disk_full_tests
    //      for terminal results). Two cases, one watch each: the next call comes
    //      from the same process, or from a fresh one (the CLI case: the
    //      un-acknowledged event must be in the watch file). Either way the
    //      next call returns the same event_id at once with redelivered true;
    //      the call after it (the first was delivered and committed) is a quiet
    //      270 s, and so is one from yet another fresh process (the
    //      acknowledgement is persisted too).
    // proves: CLAIM(S6)
    let rig = Rig::new();
    let probe = quiet_probe();
    for fresh_process in [false, true] {
        let name = if fresh_process { "fresh" } else { "same" };
        let first_process = rig.vigil(AgentEnv::default(), &probe);
        let (id, _) = quiet_watch(&rig, &first_process, name, 600, 7200, |request| request.expect_s = Some(60)).await;
        let request = KeepaliveRequest { watch_id: Some(id.clone()), max_s: None, orchestrator: None, confirm: None, session: false };
        let (envelope, delivery) = first_process
            .keepalive_uncommitted(request, &crate::keepalive::NoProgress, std::future::pending::<()>())
            .await
            .unwrap()
            .expect("not cancelled");
        let first = serde_json::to_value(&envelope).unwrap();
        assert_attention(&first, "overdue", 90);
        let first_id = only_event(&first)["event_id"].as_str().unwrap().to_string();
        drop(delivery);

        let later_process;
        let second_process: &Vigil = if fresh_process {
            later_process = rig.vigil(AgentEnv::default(), &probe);
            &later_process
        } else {
            &first_process
        };
        let second = call(second_process, &id, None, None).await;
        assert_eq!(second["waited_s"], 0, "{name}: {second}");
        let again = only_event(&second);
        assert_eq!((again["event_id"].as_str(), &again["redelivered"]), (Some(first_id.as_str()), &json!(true)), "{name}: {second}");
        let third = call(second_process, &id, None, None).await;
        assert_eq!((&third["waited_s"], &third["events"]), (&json!(270), &json!([])), "{name}: {third}");
        let last = rig.vigil(AgentEnv::default(), &probe);
        let fourth = call(&last, &id, None, None).await;
        assert_eq!(fourth["events"], json!([]), "{name}: the acknowledgement is persisted: {fourth}");
    }
}

// ------------------------------------------------ S7: budget and cancel ----

#[tokio::test(start_paused = true)]
async fn a_probe_that_cannot_finish_before_the_call_ends_is_not_started() {
    // why: S7 "a check is skipped when less than its timeout remains (it runs
    //      on the next call)". The probe's timeout is 5 s and the call keeps a
    //      100 ms margin, so a check needs 5.1 s. Explicit 3000 s calls with
    //      the cadence set so that the second check falls 6 s (2994) or 5 s
    //      (2995) before the end: 6 s remains -> the check runs (asks at 0 and
    //      2994); 5 s remains -> skipped (asks at 0 only; exactly 5 s does not
    //      leave 5 s after the margin - R2-A5). The skipped check is not lost:
    //      the next call starts 3000 s after the only check, so its start check
    //      is due at offset 0.
    // proves: CLAIM(S7)
    for (cadence_s, runs) in [(2994u64, true), (2995, false)] {
        let rig = Rig::build(Arc::new(TokioClock::new(spec_instant())), |config| config.credit_check_every = Duration::from_secs(cadence_s));
        let probe = quiet_probe();
        let vigil = rig.vigil(AgentEnv::default(), &probe);
        let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 20_000, |_| {}).await;
        let started = Instant::now();
        let first = call(&vigil, &id, Some(3000), Some(true)).await;
        assert_eq!(first["waited_s"], 3000, "cadence {cadence_s}");
        let expected: Vec<u64> = if runs { vec![0, cadence_s] } else { vec![0] };
        assert_eq!(probe.offsets_from(started), expected, "cadence {cadence_s}");
        if !runs {
            let before = probe.asked_count();
            call(&vigil, &id, Some(30), Some(true)).await;
            assert_eq!(probe.asked_count(), before + 1, "the skipped check runs at the start of the next call");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_fast_probe_leaves_the_remaining_budget_available_to_the_hook() {
    // why: S7. Probe reserve is 5s, but an instant probe does not spend it.
    //      Checks at 2984 and 2986 both leave >=10.1s for the hook, so both
    //      run with the full 10s hook timeout. R2-A5: do not reserve 15.1s
    //      for a check; bound each operation against the actual budget left.
    // proves: CLAIM(S7)
    for cadence in [2984, 2986] {
        let rig = Rig::build(Arc::new(TokioClock::new(spec_instant())), |c| {
            c.credit_check_every = Duration::from_secs(cadence);
            c.on_credit_change = Some("policy".into());
        });
        let probe = ScriptedCreditProbe::new(credit_state(true, CreditSource::Uw));
        probe.push(credit_state(false, CreditSource::Uw));
        let hook = ScriptedHookRunner::new();
        let vigil = rig.vigil_with_hook(claude(), &probe, &hook);
        let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 20_000, |_| {}).await;
        let start = Instant::now();
        let answer = call(&vigil, &id, Some(3000), Some(true)).await;
        assert_eq!(probe.offsets_from(start), vec![0, cadence]);
        assert_eq!(answer["waited_s"], cadence);
        assert_eq!(only_event(&answer)["on_credits"], true);
        assert_eq!(hook.calls().len(), 1);
        assert_eq!(hook.calls()[0].timeout, Duration::from_secs(10));
        assert_eq!(hook.offsets_from(start), vec![cadence]);
    }
}

fn pid_file_value(path: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(path).ok().and_then(|text| text.trim().parse().ok())
}

/// Real time, at most 5 s: resolves once a script has written its child's pid.
async fn child_pid_appears(path: &std::path::Path) {
    for _ in 0..100 {
        if pid_file_value(path).is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn child_is_gone_within_2_s(pid: u32) -> bool {
    for _ in 0..40 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok();
        let gone = match stat {
            None => true,
            Some(line) => line[line.rfind(')').unwrap() + 1..].trim_start().starts_with('Z'),
        };
        if gone {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test]
async fn cancelling_a_call_during_the_uw_probe_leaves_no_uw_descendants() {
    // why: S7 "cancel during probe". The real SystemCreditProbe runs a fake
    //      `uw` that backgrounds a `sleep 60`, writes its pid and waits. A call
    //      with a long window (explicit 600 s) checks at its start; the cancel
    //      future fires as soon as the pid file exists, so the probe is dropped
    //      mid-flight. The call must report cancelled, and the sleeper must be
    //      gone. (The cancel future gives up after 5 s so a missing check ends
    //      the test as a failed assertion, not a hang.)
    // proves: CLAIM(S7-kill)
    let scratch = tempfile::tempdir().unwrap();
    let pid_file = scratch.path().join("pid");
    let uw = scratch.path().join("uw");
    write_script(&uw, &format!("sleep 60 &\necho $! > '{}'\nwait", pid_file.display()));
    let rig = Rig::real_time(|config| config.uw_command = uw.clone());
    let vigil = rig.env.another_process().with_agent_env(AgentEnv::default());
    let (id, _) = quiet_watch(&rig, &vigil, "w", 3600, 7200, |_| {}).await;
    let request = KeepaliveRequest { watch_id: Some(id), max_s: Some(600), orchestrator: Some(true), confirm: None, session: false };
    let cancelled = vigil.keepalive_cancellable(request, &crate::keepalive::NoProgress, child_pid_appears(&pid_file)).await.unwrap();
    assert!(cancelled.is_none(), "the call should have been cancelled mid-probe");
    let pid = pid_file_value(&pid_file);
    assert!(pid.is_some(), "the call must start uw before cancellation");
    let pid = pid.unwrap();
    let _cleanup = ChildCleanup(pid);
    assert!(child_is_gone_within_2_s(pid).await, "uw's child {pid} survived the cancel");
}

#[tokio::test]
async fn cancelling_a_call_during_the_hook_leaves_no_hook_descendants() {
    // why: S7 "cancel during hook". The hook backgrounds a `sleep 60`, writes
    //      its pid and waits (timeout 30 s, so only the cancel can end it). The
    //      probe is scripted (off baseline, then on after 200 ms), the hook is
    //      the real process runner. After the cancel the sleeper is gone.
    // proves: CLAIM(S7-kill)
    let scratch = tempfile::tempdir().unwrap();
    let pid_file = scratch.path().join("pid");
    let command = format!("sleep 60 & echo $! > '{}'; wait", pid_file.display());
    let rig = Rig::real_time(move |config| {
        config.credit_check_every = Duration::from_millis(200);
        config.credit_hook_timeout = Duration::from_secs(30);
        config.on_credit_change = Some(command);
    });
    let probe = off_then_on();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 3600, 7200, |_| {}).await;
    let request = KeepaliveRequest { watch_id: Some(id), max_s: Some(600), orchestrator: Some(true), confirm: None, session: false };
    let cancelled = vigil.keepalive_cancellable(request, &crate::keepalive::NoProgress, child_pid_appears(&pid_file)).await.unwrap();
    assert!(cancelled.is_none(), "the call should have been cancelled mid-hook");
    let pid = pid_file_value(&pid_file);
    assert!(pid.is_some(), "the call must start the hook before cancellation");
    let pid = pid.unwrap();
    let _cleanup = ChildCleanup(pid);
    assert!(child_is_gone_within_2_s(pid).await, "the hook's child {pid} survived the cancel");
}

#[tokio::test]
async fn the_hook_receives_at_most_4_kib_on_stdin_even_with_a_long_window_list() {
    // why: S8 through the engine and a real hook. The probe reports 300
    //      windows; the real hook copies its stdin to a file. The file is at
    //      most 4096 bytes and still one JSON object with the verdict. Control:
    //      the same run with 3 windows passes them all on, so the bound cuts
    //      only the excess.
    // proves: CLAIM(S8-stdin)
    let stdin_of = |windows: usize| async move {
        let scratch = tempfile::tempdir().unwrap();
        let out = scratch.path().join("stdin.json");
        let command = format!("cat > '{}'", out.display());
        let rig = Rig::real_time(move |config| {
            config.credit_check_every = Duration::from_millis(200);
            config.on_credit_change = Some(command);
        });
        let probe = ScriptedCreditProbe::new(credit_state_with_windows(true, windows));
        probe.push(credit_state_with_windows(false, windows));
        let vigil = rig.vigil(AgentEnv::default(), &probe);
        let (id, _) = quiet_watch(&rig, &vigil, "w", 3600, 7200, |_| {}).await;
        real_call_with(&vigil, &id, Some(600), Some(true)).await;
        std::fs::read_to_string(&out).expect("the hook ran")
    };
    let small: Value = serde_json::from_str(&stdin_of(3).await).unwrap();
    assert_eq!(small["windows"].as_array().unwrap().len(), 3);
    let big = stdin_of(300).await;
    assert!(big.len() <= 4096, "{} bytes", big.len());
    let parsed: Value = serde_json::from_str(&big).expect("still one JSON object");
    assert_eq!((&parsed["on_credits"], &parsed["source"]), (&json!(true), &json!("uw")));
}

// ------------------------------------------------ S9: shared state ----

#[tokio::test(start_paused = true)]
async fn a_fresh_process_measures_a_change_against_credit_state_json_not_against_its_memory() {
    // why: S9. The previous in-memory baseline (A9) is gone: process B has
    //      never seen the state, credit-state.json says on (last check 1000 s
    //      ago), the probe says off, so B reports the change at once (waited 0)
    //      and runs the hook once. A memory-based baseline would swallow it.
    // proves: CLAIM(S9)
    let rig = Rig::build(Arc::new(TokioClock::new(spec_instant())), |config| config.on_credit_change = Some("policy".into()));
    rig.seed_credit_file(Some(true), 1000);
    let probe = quiet_probe();
    let hook = ScriptedHookRunner::new();
    let vigil = rig.vigil_with_hook(AgentEnv::default(), &probe, &hook);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 3600, 7200, |_| {}).await;
    let envelope = call(&vigil, &id, Some(30), Some(true)).await;
    assert_eq!((&envelope["waited_s"], &only_event(&envelope)["on_credits"]), (&json!(0), &json!(false)), "{envelope}");
    assert_eq!(hook.calls().len(), 1);
    assert_eq!(rig.read_credit_file()["on_credits"], false);
}

#[tokio::test(start_paused = true)]
async fn after_one_process_reported_a_change_a_later_process_reports_nothing() {
    // why: S9 "a process runs the hook only when its observed known state
    //      differs from the recorded one, then records it". P1 sees off -> on
    //      and records on. P2 (fresh, its own probe, 240 s later so its start
    //      check is due) observes on: equal to the recorded state, so no event,
    //      no second hook run, and its 30 s call is quiet. One hook run in all.
    // proves: CLAIM(S9)
    let rig = Rig::build(Arc::new(TokioClock::new(spec_instant())), |config| config.on_credit_change = Some("policy".into()));
    rig.seed_credit_file(Some(false), 1000);
    let hook = ScriptedHookRunner::new();
    let first_probe = ScriptedCreditProbe::new(credit_state(true, CreditSource::Uw));
    let p1 = rig.vigil_with_hook(AgentEnv::default(), &first_probe, &hook);
    let (id, _) = quiet_watch(&rig, &p1, "w", 3600, 7200, |_| {}).await;
    let one = call(&p1, &id, Some(30), Some(true)).await;
    assert_eq!(only_event(&one)["on_credits"], true, "{one}");

    tokio::time::sleep(Duration::from_secs(240)).await;
    let second_probe = ScriptedCreditProbe::new(credit_state(true, CreditSource::Uw));
    let p2 = rig.vigil_with_hook(AgentEnv::default(), &second_probe, &hook);
    let two = call(&p2, &id, Some(30), Some(true)).await;
    assert_eq!(second_probe.asked_count(), 1, "the check was due");
    assert_eq!((&two["waited_s"], &two["events"]), (&json!(30), &json!([])), "{two}");
    assert_eq!(hook.calls().len(), 1, "one change, one hook run");
}

#[tokio::test(start_paused = true)]
async fn an_unreadable_credit_state_file_is_a_baseline_and_is_rewritten() {
    // why: S9 robustness. A torn write, a hand edit, or an older format must not
    //      crash a call or fire the hook. Three bad contents; each time the probe
    //      says on: no event, no hook, the call runs its 30 s, and the file is
    //      rewritten as valid {on_credits: true, checked_at: now}.
    // proves: CLAIM(S9)
    for (name, content) in [("not json", "not json at all"), ("empty object", "{}"), ("wrong types", r#"{"on_credits": "yes", "checked_at": 5}"#)] {
        let rig = Rig::build(Arc::new(TokioClock::new(spec_instant())), |config| config.on_credit_change = Some("policy".into()));
        std::fs::create_dir_all(rig.env.root()).unwrap();
        std::fs::write(rig.credit_file(), content).unwrap();
        let probe = ScriptedCreditProbe::new(credit_state(true, CreditSource::Uw));
        let hook = ScriptedHookRunner::new();
        let vigil = rig.vigil_with_hook(AgentEnv::default(), &probe, &hook);
        let (id, _) = quiet_watch(&rig, &vigil, "w", 3600, 7200, |_| {}).await;
        let envelope = call(&vigil, &id, Some(30), Some(true)).await;
        assert_eq!((&envelope["waited_s"], &envelope["events"]), (&json!(30), &json!([])), "{name}: {envelope}");
        assert_eq!(hook.calls().len(), 0, "{name}");
        assert_eq!(rig.read_credit_file(), json!({"on_credits": true, "checked_at": serde_json::to_value(spec_instant()).unwrap()}), "{name}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_processes_that_detect_the_same_change_run_the_hook_once() {
    // why: S9 (two Engine instances on one root, real flock, a real hook that
    //      takes 0.4 s). credit-state.json says off; both instances' probes say
    //      on; both start an orchestrator call at the same moment on their own
    //      watch. Without `credit.lock` around probe + hook both would run the
    //      hook (counter 2) and both would end with an event. With it the
    //      loser, once the winner recorded on, observes on == recorded and
    //      carries on quietly (so its call is still running after 3 s). Exactly
    //      one envelope, one hook run, the file says on, the lock file exists.
    // proves: CLAIM(S9)
    let scratch = tempfile::tempdir().unwrap();
    let counter = scratch.path().join("count");
    let command = format!("cat > /dev/null; sleep 0.4; echo run >> '{}'", counter.display());
    let rig = Rig::real_time(move |config| config.on_credit_change = Some(command));
    rig.seed_credit_file(Some(false), 1000);
    let (probe_a, probe_b) = (ScriptedCreditProbe::new(credit_state(true, CreditSource::Uw)), ScriptedCreditProbe::new(credit_state(true, CreditSource::Uw)));
    let a = rig.vigil(AgentEnv::default(), &probe_a);
    let b = rig.vigil(AgentEnv::default(), &probe_b);
    let (id_a, _) = quiet_watch(&rig, &a, "a", 3600, 7200, |_| {}).await;
    let (id_b, _) = quiet_watch(&rig, &b, "b", 3600, 7200, |_| {}).await;
    async fn run(vigil: &Vigil, id: &WatchId) -> Result<Result<crate::Envelope, crate::VigilError>, tokio::time::error::Elapsed> {
        let request = KeepaliveRequest { watch_id: Some(id.clone()), max_s: Some(30), orchestrator: Some(true), confirm: None, session: false };
        tokio::time::timeout(Duration::from_secs(3), vigil.keepalive(request)).await
    }
    let (ra, rb) = tokio::join!(run(&a, &id_a), run(&b, &id_b));
    let finished: Vec<Value> = [ra, rb].into_iter().filter_map(Result::ok).map(|envelope| serde_json::to_value(envelope.unwrap()).unwrap()).collect();
    assert_eq!(finished.len(), 1, "exactly one process reports the change: {finished:?}");
    assert_eq!(only_event(&finished[0])["kind"], "credit_state_changed");
    assert_eq!(std::fs::read_to_string(&counter).unwrap().lines().count(), 1, "the hook ran once");
    assert_eq!(rig.read_credit_file()["on_credits"], true);
    assert!(rig.env.root().join("credit.lock").exists());
}

// ------------------------------------------------ S10: no-id, several watches ----

#[tokio::test(start_paused = true)]
async fn any_covered_orchestrator_watch_makes_a_no_id_call_an_orchestrator_call() {
    // why: S10 (mixed flags). Two watches in one process: `a` stored with
    //      orchestrator true, `b` without. A no-id call with no flag of its own
    //      covers both and is an orchestrator call because `a` is: 3000 s on a
    //      1h cache. Controls, each on its own Vigil: both watches unflagged ->
    //      270 s; an explicit `orchestrator: false` on the call overrides the
    //      stored flag (A8) -> 270 s.
    // proves: CLAIM(S10), CLAIM(AI1')
    let scenario = |flag_a: Option<bool>, call_flag: Option<bool>| async move {
        let rig = Rig::new();
        rig.one_hour_cache();
        let probe = quiet_probe();
        let vigil = rig.vigil(claude(), &probe);
        quiet_watch(&rig, &vigil, "a", 600, 20_000, |request| request.orchestrator = flag_a).await;
        quiet_watch(&rig, &vigil, "b", 600, 20_000, |_| {}).await;
        call_noid(&vigil, None, call_flag).await
    };
    let mixed = scenario(Some(true), None).await;
    assert_eq!((&mixed["waited_s"], &mixed["cache"]["ttl"]), (&json!(3000), &json!("1h")), "{mixed}");
    let none = scenario(None, None).await;
    assert_eq!((&none["waited_s"], &none["cache"]["source"]), (&json!(270), &json!("default")), "control: {none}");
    let overridden = scenario(Some(true), Some(false)).await;
    assert_eq!(overridden["waited_s"], 270, "an explicit false wins: {overridden}");
}

#[tokio::test(start_paused = true)]
async fn attention_on_a_secondary_watch_ends_a_no_id_call_and_is_delivered_once() {
    // why: S10 (secondary watch). `a` (nearest deadline, 3000 s) is the primary
    //      of a no-id envelope; `b` (deadline 7200) is overdue at 90 s
    //      (expect_s 60). The call ends at 90 with a's status pending, `next`
    //      "act on events" and ONE event, b's attention (event.watch_id "b",
    //      id b:...). The next no-id call is quiet for its full 270 s: b's
    //      attention was delivered and acknowledged along with the envelope.
    // proves: CLAIM(S10), CLAIM(S6)
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    quiet_watch(&rig, &vigil, "a", 600, 3000, |_| {}).await;
    quiet_watch(&rig, &vigil, "b", 600, 7200, |request| request.expect_s = Some(60)).await;
    let first = call_noid(&vigil, None, None).await;
    assert_eq!((&first["watch_id"], &first["status"], &first["next"], &first["waited_s"]), (&json!("a"), &json!("pending"), &json!("act on events"), &json!(90)), "{first}");
    let event = only_event(&first);
    assert!(is_attention(event, "overdue"), "{first}");
    assert_eq!(event["watch_id"], "b");
    assert!(is_w1_id(event["event_id"].as_str().unwrap(), "b"), "{event}");
    let second = call_noid(&vigil, None, None).await;
    assert_eq!((&second["waited_s"], &second["events"]), (&json!(270), &json!([])), "{second}");
}

#[tokio::test(start_paused = true)]
async fn when_a_watch_fires_and_another_needs_attention_in_one_call_the_terminal_result_is_primary() {
    // why: S10 (attention vs terminal). `a` is a change watch whose input is
    //      written at t = 15, so its tick at 20 fires it (tick 2, event
    //      `a:2:0`). `b` is a judge watch with a live process and stall_s 20, so
    //      its tick at 20 raises `stalled`. One call, one envelope: the primary
    //      is the fired watch (status fired), events = [a's terminal event,
    //      b's attention] in that order with distinct ids, `others` empty (b
    //      is still pending). The next no-id call covers only b (a was
    //      returned), is quiet for 270 s, and b's attention does not return.
    // proves: CLAIM(S10), CLAIM(S6)
    let rig = Rig::new();
    let probe = quiet_probe();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let sleeper = Sleeper::start();
    let (_, path_a) = quiet_watch(&rig, &vigil, "a", 20, 3600, |_| {}).await;
    judge_watch(&rig, &vigil, "b", 20, 3000, |request| {
        request.stall_s = Some(20);
        request.inputs.push(Input::Process { pid: sleeper.pid(), want: None });
    })
    .await;
    let appender = async {
        tokio::time::sleep(Duration::from_secs(15)).await;
        append_file(&path_a, "boom\n");
    };
    let (first, ()) = tokio::join!(call_noid(&vigil, None, None), appender);
    assert_eq!((&first["watch_id"], &first["status"], &first["next"]), (&json!("a"), &json!("fired"), &json!("act on events")), "{first}");
    let events = first["events"].as_array().unwrap();
    assert_eq!(events.len(), 2, "{first}");
    assert_eq!(events[0]["event_id"], "a:2:0");
    assert!(events[0].get("kind").is_none(), "the terminal event is an ordinary one: {first}");
    assert!(is_attention(&events[1], "stalled") && events[1]["watch_id"] == "b", "{first}");
    assert_ne!(events[0]["event_id"], events[1]["event_id"]);
    assert!(first.get("others").is_none(), "b is pending, not finished: {first}");

    let second = call_noid(&vigil, None, None).await;
    assert_eq!((&second["watch_id"], &second["waited_s"], &second["events"]), (&json!("b"), &json!(270), &json!([])), "{second}");
}

#[tokio::test(start_paused = true)]
async fn a_watch_finishing_at_its_attention_threshold_emits_only_its_terminal_event() {
    // why: A15 says a finished watch emits no attention. S10's same-call
    //      secondary attention belongs to a different, still-pending watch.
    //      Control: the identical threshold without a change emits attention.
    // proves: CLAIM(S10), CLAIM(AI5)
    let rig = Rig::new();
    let vigil = rig.vigil(AgentEnv::default(), &quiet_probe());
    let (control, _) = quiet_watch(&rig, &vigil, "control", 20, 3600, |r| r.expect_s = Some(40)).await;
    assert_attention(&call(&vigil, &control, None, None).await, "overdue", 60);
    let (id, path) = quiet_watch(&rig, &vigil, "w", 20, 3600, |r| r.expect_s = Some(40)).await;
    let appender = async { tokio::time::sleep(Duration::from_secs(55)).await; append_file(&path, "done\n"); };
    let (answer, ()) = tokio::join!(call(&vigil, &id, None, None), appender);
    assert_eq!(answer["status"], "fired");
    assert_eq!(answer["waited_s"], 60);
    assert_eq!(only_event(&answer)["event_id"], "w:4:0");
    assert!(only_event(&answer).get("kind").is_none());
}

// ---- Spec review round 2: S1-S10 -----------------------------------------

#[tokio::test(start_paused = true)]
async fn unknown_checks_keep_either_known_state_without_event_or_hook() {
    // why: S5. On->unknown and off->unknown must preserve last known state
    //      across processes, yet advance checked_at. A subsequent same known
    //      state is quiet; the opposite known state runs one hook (control).
    // proves: CLAIM(S5)
    for known in [false, true] {
        let rig = Rig::build(Arc::new(TokioClock::new(spec_instant())), |c| c.on_credit_change = Some("record".into()));
        rig.seed_credit_file(Some(known), 240);
        let probe = ScriptedCreditProbe::new(credit_unknown(CreditSource::Transcripts));
        let hook = ScriptedHookRunner::new();
        let vigil = rig.vigil_with_hook(claude(), &probe, &hook);
        let (id, _) = quiet_watch(&rig, &vigil, "w", 10, 3600, |_| {}).await;
        let unknown = call(&vigil, &id, Some(30), Some(true)).await;
        assert_eq!(unknown["events"], json!([]));
        assert!(unknown["cache"]["credit_check"].is_object(), "unknown check must be recorded: {unknown}");
        assert!(unknown["cache"]["credit_check"]["on_credits"].is_null());
        assert_eq!(rig.read_credit_file()["on_credits"], known);
        assert_eq!(rig.read_credit_file()["checked_at"], json!(spec_instant()));
        assert_eq!(hook.calls().len(), 0);
        tokio::time::sleep(Duration::from_secs(210)).await;
        probe.set_steady(credit_state(known, CreditSource::Uw));
        let fresh = rig.vigil_with_hook(claude(), &probe, &hook);
        assert_eq!(call(&fresh, &id, Some(30), Some(true)).await["events"], json!([]));
        assert_eq!(hook.calls().len(), 0);
        tokio::time::sleep(Duration::from_secs(210)).await;
        probe.set_steady(credit_state(!known, CreditSource::Uw));
        let changed = call(&fresh, &id, Some(30), Some(true)).await;
        assert_eq!(only_event(&changed)["on_credits"], !known);
        assert_eq!(hook.calls().len(), 1);
    }
}

fn stored_watch(rig: &Rig, id: &WatchId) -> Value {
    serde_json::from_slice(&std::fs::read(rig.env.root().join("watches").join(format!("{id}.json"))).unwrap()).unwrap()
}

#[tokio::test(start_paused = true)]
async fn two_attention_reasons_and_a_later_terminal_event_have_distinct_persisted_ids() {
    // why: S6. Overdue at 30, stall at 60, then ordinary fire: three
    //      distinct IDs must survive the tick save; acknowledging one reason
    //      must not suppress the other or re-emit the first.
    // proves: CLAIM(S6)
    let rig = Rig::new();
    let child = Sleeper::start();
    let vigil = rig.vigil(AgentEnv::default(), &quiet_probe());
    let (id, path) = quiet_watch(&rig, &vigil, "w", 10, 3600, |r| {
        r.expect_s = Some(20);
        r.stall_s = Some(60);
        r.inputs.push(Input::Process { pid: child.pid(), want: None });
    }).await;
    let request = KeepaliveRequest { watch_id: Some(id.clone()), max_s: None, orchestrator: None, confirm: None, session: false };
    let (first, first_delivery) = vigil.keepalive_uncommitted(request.clone(), &crate::keepalive::NoProgress, std::future::pending()).await.unwrap().unwrap();
    let first = serde_json::to_value(first).unwrap();
    assert_attention(&first, "overdue", 30);
    let first_id = only_event(&first)["event_id"].as_str().expect("event id").to_owned();
    assert!(stored_watch(&rig, &id)["events"].as_array().unwrap().iter().any(|e| e["event_id"] == first_id));
    assert_eq!(stored_watch(&rig, &id)["tick"], only_event(&first)["tick"]);
    vigil.commit_delivery(first_delivery);
    let (second, second_delivery) = vigil.keepalive_uncommitted(request, &crate::keepalive::NoProgress, std::future::pending()).await.unwrap().unwrap();
    let second = serde_json::to_value(second).unwrap();
    assert_attention(&second, "stalled", 30);
    let second_id = only_event(&second)["event_id"].as_str().unwrap().to_owned();
    assert_ne!(first_id, second_id);
    assert!(stored_watch(&rig, &id)["events"].as_array().unwrap().iter().any(|e| e["event_id"] == second_id));
    assert_eq!(stored_watch(&rig, &id)["tick"], only_event(&second)["tick"]);
    vigil.commit_delivery(second_delivery);
    append_file(&path, "done\n");
    let terminal = call(&vigil, &id, None, None).await;
    assert_eq!(terminal["status"], "fired");
    let last_id = only_event(&terminal)["event_id"].as_str().unwrap();
    assert_ne!(last_id, first_id);
    assert_ne!(last_id, second_id);
    assert!(only_event(&terminal).get("kind").is_none());
}

#[tokio::test(start_paused = true)]
async fn attention_cancelled_before_delivery_is_replayed_until_commit_delivery() {
    // why: S6. The transport can disconnect after sensing/persistence but
    //      before handing the envelope to the agent. Drop the delivery token
    //      at that boundary. Retry must preserve its ID and mark redelivery;
    //      only commit_delivery acknowledges it. Fresh engine sees the ACK.
    // proves: CLAIM(S6)
    let rig = Rig::new();
    let vigil = rig.vigil(AgentEnv::default(), &quiet_probe());
    let (id, _) = quiet_watch(&rig, &vigil, "w", 10, 3600, |r| r.expect_s = Some(20)).await;
    let request = KeepaliveRequest { watch_id: Some(id.clone()), max_s: None, orchestrator: None, confirm: None, session: false };
    let (first, delivery) = vigil.keepalive_uncommitted(request.clone(), &crate::keepalive::NoProgress, std::future::pending()).await.unwrap().unwrap();
    let first = serde_json::to_value(first).unwrap();
    assert_attention(&first, "overdue", 30);
    let event_id = only_event(&first)["event_id"].clone();
    drop(delivery);
    let (retry, delivery) = vigil.keepalive_uncommitted(request, &crate::keepalive::NoProgress, std::future::pending()).await.unwrap().unwrap();
    let retry = serde_json::to_value(retry).unwrap();
    assert_eq!(only_event(&retry)["event_id"], event_id);
    assert_eq!(only_event(&retry)["redelivered"], true);
    vigil.commit_delivery(delivery);
    let fresh = rig.vigil(AgentEnv::default(), &quiet_probe());
    assert_eq!(call(&fresh, &id, Some(30), None).await["events"], json!([]));
}

struct SlowCreditProbe {
    started: std::sync::Mutex<Vec<Instant>>,
    dropped: std::sync::Mutex<Vec<Instant>>,
    delay: Duration,
    first_off: bool,
}

struct ProbeDrop<'a>(&'a std::sync::Mutex<Vec<Instant>>);
impl Drop for ProbeDrop<'_> {
    fn drop(&mut self) { self.0.lock().unwrap().push(Instant::now()); }
}

impl crate::credit::CreditProbe for SlowCreditProbe {
    fn check<'a>(&'a self, _now: Timestamp) -> crate::judge::BoxFuture<'a, crate::credit::CreditState> {
        Box::pin(async move {
            let _guard = ProbeDrop(&self.dropped);
            let first = { let mut asked = self.started.lock().unwrap(); asked.push(Instant::now()); asked.len() == 1 };
            tokio::time::sleep(self.delay).await;
            credit_state(!(self.first_off && first), CreditSource::Uw)
        })
    }
}

#[tokio::test(start_paused = true)]
async fn a_probe_that_never_answers_is_cut_off_at_five_seconds() {
    // why: S7. A probe advertises 5 s but takes 100 s. Its future must be
    //      dropped after 5 s, not merely at the 30 s call boundary. Drop time
    //      is independently observed by a guard inside the external probe.
    // proves: CLAIM(S7-budget)
    let rig = Rig::new();
    rig.seed_credit_file(Some(false), 240);
    let probe = Arc::new(SlowCreditProbe { started: Default::default(), dropped: Default::default(), delay: Duration::from_secs(100), first_off: false });
    let vigil = rig.env.another_process().with_agent_env(claude()).with_credit_probe(probe.clone());
    let (id, _) = quiet_watch(&rig, &vigil, "w", 10, 3600, |_| {}).await;
    let start = Instant::now();
    let answer = call(&vigil, &id, Some(30), Some(true)).await;
    assert_eq!(probe.started.lock().unwrap().len(), 1);
    assert_eq!(probe.dropped.lock().unwrap().as_slice(), &[start + Duration::from_secs(5)]);
    assert_eq!(start.elapsed(), Duration::from_secs(30));
    assert_eq!(answer["status"], "pending");
    assert_eq!(answer["events"], json!([]));
    assert_eq!(rig.read_credit_file()["on_credits"], false);
}

#[tokio::test(start_paused = true)]
async fn hook_timeout_is_cut_to_remaining_budget_minus_100_ms() {
    // why: S7. Long window 3000, check at 2994 takes 5 s, leaving 1 s.
    //      Reserve 100 ms and give the hook 900 ms. A 20 s hook must time
    //      out at 2999.9; its normal 10 s timeout cannot overrun the call.
    // proves: CLAIM(S7-budget)
    let rig = Rig::build(Arc::new(TokioClock::new(spec_instant())), |c| {
        c.on_credit_change = Some("slow-hook".into());
        c.credit_check_every = Duration::from_secs(2994);
    });
    let probe = Arc::new(SlowCreditProbe { started: Default::default(), dropped: Default::default(), delay: Duration::from_secs(5), first_off: true });
    let hook = ScriptedHookRunner::new();
    hook.set_delay(Duration::from_secs(20));
    let vigil = rig.env.another_process().with_agent_env(claude()).with_credit_probe(probe).with_hook_runner(hook.clone());
    let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 10_000, |_| {}).await;
    let start = Instant::now();
    let answer = call(&vigil, &id, Some(3000), Some(true)).await;
    assert_eq!(hook.calls().len(), 1);
    assert_eq!(hook.calls()[0].timeout, Duration::from_millis(900));
    assert_eq!(start.elapsed(), Duration::from_millis(2_999_900));
    assert_eq!(only_event(&answer)["hook"]["timed_out"], true);
}

fn dead_child(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |s| s[s.rfind(')').unwrap() + 1..].trim_start().starts_with('Z'))
}

struct ChildCleanup(u32);
impl Drop for ChildCleanup {
    fn drop(&mut self) {
        if !dead_child(self.0) { let _ = std::process::Command::new("kill").args(["-KILL", &self.0.to_string()]).status(); }
    }
}

#[tokio::test(start_paused = true)]
async fn two_engines_share_a_real_credit_flock_and_run_the_change_hook_once() {
    // why: S9. Hold a real OS flock while both engines complete 30s calls:
    //      neither may probe/run a hook. Release it, then let both detect
    //      off->on on distinct watches. Only one hook runs. Virtual time
    //      removes scheduler guesses; the companion test runs a real shell
    //      hook concurrently on the real clock.
    // proves: CLAIM(S9)
    use std::os::fd::AsRawFd;
    unsafe extern "C" { fn flock(fd: i32, op: i32) -> i32; }
    let rig = Rig::build(Arc::new(TokioClock::new(spec_instant())), |c| c.on_credit_change = Some("record".into()));
    rig.seed_credit_file(Some(false), 240);
    let holder = std::fs::File::create(rig.env.root().join("credit.lock")).unwrap();
    assert_eq!(unsafe { flock(holder.as_raw_fd(), 2) }, 0);
    let probe = ScriptedCreditProbe::new(credit_state(true, CreditSource::Uw));
    let hook = ScriptedHookRunner::new();
    hook.set_delay(Duration::from_secs(1));
    let a = rig.vigil_with_hook(claude(), &probe, &hook);
    let b = rig.vigil_with_hook(claude(), &probe, &hook);
    let (aid, _) = quiet_watch(&rig, &a, "a", 10, 3600, |_| {}).await;
    let (bid, _) = quiet_watch(&rig, &b, "b", 10, 3600, |_| {}).await;
    let (held_a, held_b) = tokio::join!(call(&a, &aid, Some(30), Some(true)), call(&b, &bid, Some(30), Some(true)));
    let checks_while_locked = probe.asked_count();
    let hooks_while_locked = hook.calls().len();
    assert_eq!(unsafe { flock(holder.as_raw_fd(), 8) }, 0);
    drop(holder);
    assert_eq!(checks_while_locked, 0, "external credit.lock must block both probes");
    assert_eq!(hooks_while_locked, 0, "hook must hold credit.lock too");
    assert_eq!((&held_a["waited_s"], &held_b["waited_s"]), (&json!(30), &json!(30)));
    assert_eq!((&held_a["events"], &held_b["events"]), (&json!([]), &json!([])));
    let (a, b) = tokio::join!(call(&a, &aid, Some(30), Some(true)), call(&b, &bid, Some(30), Some(true)));
    assert_eq!(hook.calls().len(), 1, "same transition must apply once across engines");
    let answers = [a, b];
    let changes = answers.iter().flat_map(|e| e["events"].as_array().unwrap()).filter(|e| e["kind"] == "credit_state_changed").count();
    assert_eq!(changes, 1);
    assert_eq!(probe.asked_count(), 1, "second engine rereads checked_at under flock");
    assert_eq!(rig.read_credit_file()["on_credits"], true);
}

// ------------------------------------------------ N4: at-least-once hook ----

#[tokio::test(start_paused = true)]
async fn a_hook_whose_state_write_failed_runs_again_at_the_next_check() {
    // why: N4 / S9. Hook delivery is AT-LEAST-ONCE: a change counts as applied
    //      only after credit-state.json commits. The state file says off, the
    //      probe says on, and the FIRST write of credit-state.json fails (EIO,
    //      injected after the hook's side effect has already happened).
    //      Call 1: the hook ran once, the agent still learns of the change,
    //      the file is untouched (still off, and still old, so the next
    //      orchestrator call is due to check). Call 2 (same process): the
    //      change is seen again, the hook runs a SECOND time (that is the
    //      at-least-once price, hooks must be idempotent), the write works, the
    //      file says on. Call 3 and 4: nothing new, the hook stays at two
    //      runs, so deduplication works again once the write has committed
    //      (call 4 starts 270 s after the commit, so its check is due and finds
    //      on == recorded).
    // proves: CLAIM(N4), CLAIM(S9)
    let rig = Rig::build(Arc::new(TokioClock::new(spec_instant())), |config| config.on_credit_change = Some("apply-policy".into()));
    rig.seed_credit_file(Some(false), 600);
    let probe = ScriptedCreditProbe::new(credit_state(true, CreditSource::Uw));
    let hook = ScriptedHookRunner::new();
    let faults = FaultPlan::from_rules(vec![FaultRule::new(WriteKind::CreditState, 5, When::Nth(1))]);
    let vigil = rig.vigil_with_hook(AgentEnv::default(), &probe, &hook).with_write_faults(faults.clone());
    let (id, _) = quiet_watch(&rig, &vigil, "w", 3600, 7200, |_| {}).await;

    let first = call(&vigil, &id, Some(270), Some(true)).await;
    assert_eq!(hook.calls().len(), 1, "the hook ran: {first}");
    assert_eq!(faults.injected(WriteKind::CreditState, Stage::Write), 1, "the injected failure must have fired at the state write");
    assert_eq!(only_event(&first)["kind"], "credit_state_changed", "the agent learns at once: {first}");
    assert_eq!(rig.read_credit_file()["on_credits"], false, "the failed write left the old state");
    assert_eq!(rig.read_credit_file()["checked_at"], serde_json::to_value(spec_instant().minus_secs(600)).unwrap(), "and the old check time, so the next call is due");

    let second = call(&vigil, &id, Some(270), Some(true)).await;
    assert_eq!(hook.calls().len(), 2, "the next check re-runs the hook: {second}");
    assert_eq!(hook.calls()[1].input["on_credits"], true);
    let event = only_event(&second);
    assert_eq!((&event["kind"], &event["on_credits"]), (&json!("credit_state_changed"), &json!(true)), "{second}");
    assert_eq!(rig.read_credit_file()["on_credits"], true, "committed this time");

    let third = call(&vigil, &id, Some(270), Some(true)).await;
    assert_eq!((&third["events"], &third["waited_s"]), (&json!([]), &json!(270)), "{third}");
    let fourth = call(&vigil, &id, Some(270), Some(true)).await;
    assert_eq!(fourth["events"], json!([]), "{fourth}");
    assert_eq!(probe.asked_count(), 3, "calls 1, 2 and 4 were due; call 3 started right after the commit");
    assert_eq!(hook.calls().len(), 2, "after the commit the change is deduplicated again");
}


// ============================================================================
// Review round 1 (reviewer 6/10, bar 9): S1-S8. One red test per finding, each
// beside the others because they drive `Vigil::keepalive` end to end.
// ============================================================================

// ---- review S1: attention never waits for a lock past the call budget ------

#[tokio::test(start_paused = true)]
async fn attention_due_at_the_end_of_the_budget_does_not_wait_for_a_watch_lock_held_elsewhere() {
    // why: review S1 (CLAIM(V2): the whole call, lock waits included, is
    //      bounded by max_s). Watch interval 10 s, expect_s 20 -> overdue at
    //      min(30, 560) = 30 s, which is the end of a `max_s` 30 call. Another
    //      process holds the watch's lock for the whole call, so the tick is
    //      dropped at the budget (existing behaviour) and the attention check
    //      that follows must be dropped too: it used to wait for the lock
    //      without a deadline (blocked at 32 s and beyond), and to persist
    //      after the budget once the lock came free.
    //      Expected: the call answers at exactly 30 s, pending, no events, and
    //      NOTHING was persisted after the budget (the once-per-reason flag is
    //      not in the file). The attention is not lost: the next call, once
    //      the lock is free, raises it at once (waited 0).
    // proves: CLAIM(V2), CLAIM(AI5)
    let rig = Rig::new();
    let vigil = rig.vigil(AgentEnv::default(), &quiet_probe());
    let (id, _) = quiet_watch(&rig, &vigil, "w", 10, 3600, |request| request.expect_s = Some(20)).await;
    let held = rig.env.vigil.store().lock_watch(&id).unwrap();

    let started = Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(31), call(&vigil, &id, Some(30), None)).await;
    assert!(outcome.is_ok(), "S1: a max_s 30 call was still waiting for the watch lock after 31 s ({:?} of virtual time)", started.elapsed());
    let answer = outcome.unwrap();
    assert_eq!((&answer["status"], &answer["waited_s"], &answer["events"]), (&json!("pending"), &json!(30), &json!([])), "{answer}");
    assert_eq!(started.elapsed(), Duration::from_secs(30), "the call ends at its budget");
    assert!(stored_watch(&rig, &id).get("attention_raised").is_none(), "nothing may be saved after the budget: {}", stored_watch(&rig, &id));

    drop(held);
    let next = call(&vigil, &id, Some(30), None).await;
    assert_attention(&next, "overdue", 0);
}

// ---- review S2: a hung uw must not starve the transcript fallback ----------

#[tokio::test]
async fn a_uw_that_hangs_still_leaves_the_engine_time_to_read_the_transcripts() {
    // why: review S2. The engine bounds a check by the probe's `timeout()`;
    //      the probe's own `uw` timeout was the same 5 s, so the outer limit
    //      always expired first and the transcript fallback never ran: a hung
    //      `uw` read as "unknown" although a recent main-session transcript
    //      shows the 1h -> 5m transition. Real clock and a real process (a
    //      `uw` that sleeps), with the probe's timeout shortened to 1.5 s so
    //      the test is quick. Expected: the check reports the change within
    //      the probe's timeout, from the transcripts (`credit_state_changed`,
    //      on_credits true, source transcripts), and the state file records
    //      it. Before the fix the call kept going to its 30 s end with no
    //      event (cut here by the 10 s guard) - but only when the two 1.5 s
    //      timers fell in different milliseconds, so this is the end-to-end
    //      guard; the deterministic proof is the probe-level test in
    //      `credit::tests` (the check must finish before `timeout()`).
    // proves: CLAIM(S7-budget), CLAIM(AI2')
    let rig = Rig::real_time(|_| {});
    rig.transcript(&[assistant_line(900, 0), assistant_line(0, 500)]);
    let hanging_uw = rig.env.dir.path().join("hanging-uw");
    write_script(&hanging_uw, "exec sleep 600");
    let probe = crate::credit::SystemCreditProbe {
        uw_command: hanging_uw,
        projects_dir: Some(rig.projects.path().to_path_buf()),
        uw_timeout: Duration::from_millis(1500),
    };
    rig.seed_credit_file(Some(false), 300);
    let vigil = rig.env.another_process().with_agent_env(claude()).with_credit_probe(Arc::new(probe));
    let (id, _) = quiet_watch(&rig, &vigil, "w", 10, 3600, |_| {}).await;

    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(10), call(&vigil, &id, Some(30), Some(true))).await;
    assert!(outcome.is_ok(), "S2: no answer after {:?}: the hung uw cost the call the whole check", started.elapsed());
    let answer = outcome.unwrap();
    let event = only_event(&answer);
    assert_eq!((&event["kind"], &event["on_credits"], &event["source"]), (&json!("credit_state_changed"), &json!(true), &json!("transcripts")), "{answer}");
    assert_eq!(rig.read_credit_file()["on_credits"], true, "the verdict was recorded");
}

// ---- review S3: every return path of an orchestrator call owes its due check ----

/// A rig whose last credit check is 300 s old (older than the 240 s cadence), so
/// every orchestrator call owes an entry check, and a probe that says "off".
fn rig_owing_a_credit_check() -> (Rig, Arc<ScriptedCreditProbe>) {
    let rig = Rig::new();
    rig.seed_credit_file(Some(false), 300);
    (rig, quiet_probe())
}

/// The check this call made, as the agent reads it in `cache.credit_check`.
fn assert_one_check_reported(answer: &Value, probe: &ScriptedCreditProbe, path: &str) {
    assert_eq!(probe.asked_count(), 1, "{path}: the due entry check must run before this return: {answer}");
    assert_eq!(answer["cache"]["credit_check"]["on_credits"], false, "{path}: {answer}");
}

#[tokio::test(start_paused = true)]
async fn a_call_that_returns_carried_attention_still_makes_its_due_credit_check() {
    // why: review S3, path 1. Attention that was raised and saved but never
    //      acknowledged (the transport died) is returned at once by the next
    //      call, ahead of any sensing, and that return skipped the check: a
    //      stream of such calls never monitored credit state. Call 1 is a
    //      plain call that raises the attention (overdue at 90 s) and is
    //      dropped uncommitted. Call 2 is the orchestrator's, the last check
    //      is 300 s old: it must return the carried event (redelivered) AND
    //      carry its check. Control: the same call without the orchestrator
    //      flag asks nobody.
    // proves: CLAIM(S2), CLAIM(S6)
    let (rig, probe) = rig_owing_a_credit_check();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 7200, |request| request.expect_s = Some(60)).await;
    let request = KeepaliveRequest { watch_id: Some(id.clone()), max_s: None, orchestrator: None, confirm: None, session: false };
    let (_, delivery) = vigil.keepalive_uncommitted(request, &crate::keepalive::NoProgress, std::future::pending::<()>()).await.unwrap().unwrap();
    drop(delivery);
    assert_eq!(probe.asked_count(), 0, "control: a plain call checks nothing");

    let answer = call(&vigil, &id, None, Some(true)).await;
    assert_eq!((&answer["waited_s"], &only_event(&answer)["redelivered"]), (&json!(0), &json!(true)), "{answer}");
    assert_one_check_reported(&answer, &probe, "carried attention");
}

#[tokio::test(start_paused = true)]
async fn a_call_that_returns_an_existing_terminal_result_still_makes_its_due_credit_check() {
    // why: review S3, path 2. A watch that finished before the call (here: by
    //      another process) is returned before any round, and that return
    //      skipped the check. Process 1 fires the watch with a plain call;
    //      process 2 asks for it as the orchestrator.
    // proves: CLAIM(S2)
    let (rig, probe) = rig_owing_a_credit_check();
    let first = rig.vigil(AgentEnv::default(), &probe);
    let (id, path) = quiet_watch(&rig, &first, "w", 20, 3600, |_| {}).await;
    append_file(&path, "boom\n");
    assert_eq!(call(&first, &id, None, None).await["status"], "fired");
    assert_eq!(probe.asked_count(), 0, "control: the plain call that fired it checked nothing");

    let second = rig.vigil(AgentEnv::default(), &probe);
    let answer = call(&second, &id, None, Some(true)).await;
    assert_eq!(answer["status"], "fired", "{answer}");
    assert_one_check_reported(&answer, &probe, "existing terminal result");
}

#[tokio::test(start_paused = true)]
async fn a_call_in_which_a_watch_finishes_still_makes_its_due_credit_check_and_reports_a_change_beside_it() {
    // why: review S3, path 3. A watch that fires during the call ends it, and
    //      that return skipped the check: fast jobs bypassed credit
    //      monitoring for good. (1) Probe says "off", file says "off": the
    //      fire carries the check. (2) Probe says "on": the envelope is the
    //      fire's (status fired, first event the ordinary one) with the
    //      credit event beside it, and the state file moves. (3) Controls:
    //      without the orchestrator flag no check; with a check made 100 s
    //      ago (inside the 240 s cadence) no second one.
    // proves: CLAIM(S2), CLAIM(AI8)
    let (rig, probe) = rig_owing_a_credit_check();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (plain, plain_path) = quiet_watch(&rig, &vigil, "plain", 20, 3600, |_| {}).await;
    append_file(&plain_path, "boom\n");
    let control = call(&vigil, &plain, None, None).await;
    assert_eq!((&control["status"], probe.asked_count()), (&json!("fired"), 0), "{control}");
    assert!(control["cache"].get("credit_check").is_none(), "{control}");

    let (quiet, quiet_path) = quiet_watch(&rig, &vigil, "quiet", 20, 3600, |_| {}).await;
    append_file(&quiet_path, "boom\n");
    let answer = call(&vigil, &quiet, None, Some(true)).await;
    assert_eq!(answer["status"], "fired", "{answer}");
    assert_one_check_reported(&answer, &probe, "newly finished");

    rig.seed_credit_file(Some(false), 100);
    let (recent, recent_path) = quiet_watch(&rig, &vigil, "recent", 20, 3600, |_| {}).await;
    append_file(&recent_path, "boom\n");
    let within_cadence = call(&vigil, &recent, None, Some(true)).await;
    assert_eq!((&within_cadence["status"], probe.asked_count()), (&json!("fired"), 1), "a check 100 s ago is not repeated: {within_cadence}");

    rig.seed_credit_file(Some(false), 300);
    probe.push(credit_state(true, CreditSource::Uw));
    let (changing, changing_path) = quiet_watch(&rig, &vigil, "changing", 20, 3600, |_| {}).await;
    append_file(&changing_path, "boom\n");
    let changed = call(&vigil, &changing, None, Some(true)).await;
    assert_eq!((&changed["status"], &changed["next"]), (&json!("fired"), &json!("act on events")), "{changed}");
    let events = changed["events"].as_array().unwrap();
    assert_eq!(events.len(), 2, "the fire plus the credit event: {changed}");
    assert!(events[0].get("kind").is_none(), "the terminal event is an ordinary one, first: {changed}");
    assert_eq!((&events[1]["kind"], &events[1]["on_credits"]), (&json!("credit_state_changed"), &json!(true)), "{changed}");
    assert_eq!(rig.read_credit_file()["on_credits"], true);
}

#[tokio::test(start_paused = true)]
async fn a_call_that_raises_attention_at_its_first_pass_still_makes_its_due_credit_check() {
    // why: review S3, path 4. Attention that is already due when the call
    //      starts is raised at the first pass and ends the call, and that
    //      return skipped the check. Watch overdue at 30 s (expect_s 20), the
    //      call starts at 40 s.
    // proves: CLAIM(S2), CLAIM(AI5)
    let (rig, probe) = rig_owing_a_credit_check();
    let vigil = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &vigil, "w", 600, 7200, |request| request.expect_s = Some(20)).await;
    tokio::time::advance(Duration::from_secs(40)).await;
    let answer = call(&vigil, &id, None, Some(true)).await;
    assert_attention(&answer, "overdue", 0);
    assert_one_check_reported(&answer, &probe, "new attention");
}

// ---- review S4: attention another process saved while this call waits --------

#[tokio::test(start_paused = true)]
async fn a_waiting_call_returns_attention_that_another_process_saved_but_could_not_deliver() {
    // why: review S4. Pending attention was looked for once, at the start of a
    //      call. A call that was already waiting never saw attention another
    //      CLI process persisted (and then failed to print): it ran on to the
    //      end of its window while the agent waited on a delivery that was
    //      already in the watch file. The watch ticks only every 3600 s, so
    //      the waiting call has no pass of its own for a long time: it must
    //      look at the outbox on a short cadence (5 s) as well as at every
    //      pass.
    //      Setup: expect_s 200 (overdue at 300 s, beyond the 270 s window, so
    //      the waiting call never raises it itself). At t = 40 s a second
    //      process, whose clock reads 400 s after creation, runs a call: it
    //      raises the attention, saves it, and its delivery is dropped
    //      (stdout failed). The waiting call must return that event, flagged
    //      `redelivered`, within 5 s of it being saved (waited 40..=45), and
    //      acknowledge it: the next call is quiet.
    // proves: CLAIM(S6), CLAIM(S4-outbox)
    let rig = Rig::new();
    let probe = quiet_probe();
    let waiter = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &waiter, "w", 3600, 7200, |request| request.expect_s = Some(200)).await;

    let waiting = call(&waiter, &id, None, None);
    let other_process = async {
        tokio::time::sleep(Duration::from_secs(40)).await;
        let later_clock = Arc::new(ManualClock::new(spec_instant().plus_secs(400)));
        let other = Vigil::new(rig.env.root(), later_clock, rig.env.judge.clone(), rig.env.config.clone())
            .with_reader(rig.env.reader.clone())
            .with_agent_env(AgentEnv::default());
        let request = KeepaliveRequest { watch_id: Some(id.clone()), max_s: None, orchestrator: None, confirm: None, session: false };
        let (envelope, delivery) = other.keepalive_uncommitted(request, &crate::keepalive::NoProgress, std::future::pending::<()>()).await.unwrap().unwrap();
        assert_attention(&serde_json::to_value(&envelope).unwrap(), "overdue", 0);
        drop(delivery);
    };
    let (answer, ()) = tokio::join!(waiting, other_process);

    let waited = answer["waited_s"].as_u64().unwrap();
    assert!((40..=45).contains(&waited), "S4: the waiting call should notice the saved attention within 5 s, it waited {waited} s: {answer}");
    assert_eq!((&answer["status"], &answer["next"]), (&json!("pending"), &json!("act on events")), "{answer}");
    let event = only_event(&answer);
    assert_eq!((&event["kind"], &event["reason"], &event["redelivered"]), (&json!("attention"), &json!("overdue"), &json!(true)), "{answer}");

    let next = call(&waiter, &id, Some(30), None).await;
    assert_eq!((&next["waited_s"], &next["events"]), (&json!(30), &json!([])), "the delivery was acknowledged: {next}");
}

// ---- review S5: the identity of an attention event survives a failed save ----

#[tokio::test(start_paused = true)]
async fn an_attention_event_keeps_its_id_when_a_tick_lands_between_a_failed_save_and_the_retry() {
    // why: review S5. The id is `<watch>:<tick>:<n>`. A raise whose save fails
    //      is delivered (the envelope carries it, `persist.ok` false); the
    //      retry used to build the event again from the watch as it then was,
    //      so a tick that landed in between gave it a NEW id (`w:1:0` ->
    //      `w:2:0`): the agent was told "something new" and a consumer that
    //      de-duplicates by id acted twice. The retry must be the same event:
    //      same id, `redelivered: true` (even though the tick's own save
    //      succeeded and cleared the process's memory of failed saves).
    //      Setup: cadence 600 s, expect_s 60 (overdue at 90). Call A (healthy,
    //      30 s) ticks once at 0 (tick 1). Call B (every write fails) raises
    //      at 90: event `w:1:0`, first delivery, `persist.ok` false. Writes
    //      recover, 600 s pass, so call C starts with a tick due (tick 2): it
    //      must return `w:1:0` again, flagged. Call D is quiet.
    // proves: CLAIM(S6), CLAIM(S5-identity)
    let rig = Rig::new();
    let probe = quiet_probe();
    let healthy = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &healthy, "w", 600, 7200, |request| request.expect_s = Some(60)).await;
    let a = call(&healthy, &id, Some(30), None).await;
    assert_eq!((&a["tick"], &a["events"]), (&json!(1), &json!([])), "{a}");

    let marker = rig.env.input_path("full");
    write_file(&marker, "full");
    let faults = FaultPlan::from_rules(vec![FaultRule::new(WriteKind::Watch, 5, When::WhileExists(marker.clone()))]);
    let faulted = rig.vigil(AgentEnv::default(), &probe).with_write_faults(faults);
    let b = call(&faulted, &id, None, None).await;
    assert_eq!((&b["waited_s"], &b["persist"]["ok"]), (&json!(60), &json!(false)), "{b}");
    let first_id = only_event(&b)["event_id"].as_str().unwrap().to_string();
    assert_eq!(first_id, "w:1:0", "hand-derived: raised at tick 1, first reason");

    std::fs::remove_file(marker).unwrap();
    tokio::time::advance(Duration::from_secs(600)).await;
    let c = call(&faulted, &id, None, None).await;
    assert_eq!(c["tick"], 2, "control: a tick did land between the failed save and the retry: {c}");
    let again = only_event(&c);
    assert_eq!((again["event_id"].as_str(), &again["redelivered"]), (Some("w:1:0"), &json!(true)), "S5: the retry is the same event: {c}");
    assert!(c.get("persist").is_none(), "{c}");
    let d = call(&faulted, &id, None, None).await;
    assert_eq!((&d["waited_s"], &d["events"]), (&json!(270), &json!([])), "saved this time, and acknowledged: {d}");
}

#[tokio::test(start_paused = true)]
async fn a_retried_attention_event_is_flagged_redelivered_even_when_nothing_else_remembers_the_failure() {
    // why: review S5, mutation check. In the test above the `redelivered` flag
    //      also comes from the tick that precedes the retry (its report says it
    //      followed a failed save), so deleting the flag on the retry itself
    //      went unnoticed. Here a separate tick (`tick_due`, not through a
    //      call) saves successfully after the failed attention save and so
    //      clears the process's memory of failed saves; the next call has no
    //      tick of its own to run. The retry (id `w:1:0`, remembered by the
    //      event cache) must still say `redelivered: true`.
    // proves: CLAIM(S5-identity)
    let rig = Rig::new();
    let probe = quiet_probe();
    let healthy = rig.vigil(AgentEnv::default(), &probe);
    let (id, _) = quiet_watch(&rig, &healthy, "w", 600, 7200, |request| request.expect_s = Some(60)).await;
    call(&healthy, &id, Some(30), None).await;

    let marker = rig.env.input_path("full");
    write_file(&marker, "full");
    let faults = FaultPlan::from_rules(vec![FaultRule::new(WriteKind::Watch, 5, When::WhileExists(marker.clone()))]);
    let faulted = rig.vigil(AgentEnv::default(), &probe).with_write_faults(faults);
    let b = call(&faulted, &id, None, None).await;
    assert_eq!(only_event(&b)["event_id"], "w:1:0", "{b}");

    std::fs::remove_file(marker).unwrap();
    tokio::time::advance(Duration::from_secs(600)).await;
    assert_eq!(faulted.tick_due(&id).await.unwrap(), crate::tick::TickOutcome::Ticked, "the tick that clears the failure memory");
    let c = call(&faulted, &id, None, None).await;
    assert_eq!(c["waited_s"], 0, "{c}");
    let again = only_event(&c);
    assert_eq!((again["event_id"].as_str(), &again["redelivered"]), (Some("w:1:0"), &json!(true)), "{c}");
}

// ---- review S6: a terminal result does not swallow unacknowledged attention ----

/// A watch (cadence 600 s, overdue at 90 s) whose attention was raised and saved
/// by a call whose delivery never got committed (stdout failed). Returns the
/// vigil that made the call, the watch, its input and the attention's event id.
async fn watch_with_undelivered_attention(rig: &Rig, id: &str, deadline_s: u64) -> (Vigil, WatchId, PathBuf, String) {
    let vigil = rig.vigil(AgentEnv::default(), &quiet_probe());
    let (id, path) = quiet_watch(rig, &vigil, id, 600, deadline_s, |request| request.expect_s = Some(60)).await;
    let request = KeepaliveRequest { watch_id: Some(id.clone()), max_s: None, orchestrator: None, confirm: None, session: false };
    let (envelope, delivery) = vigil.keepalive_uncommitted(request, &crate::keepalive::NoProgress, std::future::pending::<()>()).await.unwrap().unwrap();
    let first = serde_json::to_value(&envelope).unwrap();
    assert_attention(&first, "overdue", 90);
    let event_id = only_event(&first)["event_id"].as_str().unwrap().to_string();
    drop(delivery);
    (vigil, id, path, event_id)
}

#[tokio::test(start_paused = true)]
async fn attention_that_was_never_delivered_is_still_delivered_when_the_watch_expires() {
    // why: review S6. `finish` replaced the whole outbox with the terminal
    //      events, so "stdout failed, then the deadline passed" ended in
    //      `events: []` and the agent never learned of the attention.
    //      Deadline 150 s: the attention is raised at 90 s and dropped; 100 s
    //      later (t = 190) a fresh process finds the watch expired. Its
    //      envelope must carry the attention, flagged `redelivered`, and so
    //      must a replay by a third process (the stored result is immutable).
    // proves: CLAIM(S6), CLAIM(V3)
    let rig = Rig::new();
    let (_, id, _, event_id) = watch_with_undelivered_attention(&rig, "w", 150).await;
    tokio::time::advance(Duration::from_secs(100)).await;
    for process in ["fresh", "replay"] {
        let vigil = rig.vigil(AgentEnv::default(), &quiet_probe());
        let answer = call(&vigil, &id, Some(30), None).await;
        assert_eq!(answer["status"], "expired", "{process}: {answer}");
        let event = only_event(&answer);
        assert_eq!((event["event_id"].as_str(), &event["redelivered"], &event["kind"]), (Some(event_id.as_str()), &json!(true), &json!("attention")), "{process}: {answer}");
    }
}

#[tokio::test(start_paused = true)]
async fn attention_that_was_never_delivered_comes_before_the_terminal_event_when_the_watch_fires() {
    // why: review S6, the fire path of the same replacement (`tick` finishes
    //      the watch). A call returns carried attention BEFORE it senses, so
    //      the finish must come from a tick that lands while the outbox is
    //      still unacknowledged: another process's tick, or a tick that was
    //      already past the call's outbox inspection. That tick is driven
    //      directly (`tick_due`) after the change is written and 600 s have
    //      passed. The next call returns BOTH events: the attention first
    //      (redelivered), then the ordinary terminal event, with distinct ids.
    // proves: CLAIM(S6), CLAIM(V3)
    let rig = Rig::new();
    let (vigil, id, path, event_id) = watch_with_undelivered_attention(&rig, "w", 7200).await;
    append_file(&path, "boom\n");
    tokio::time::advance(Duration::from_secs(600)).await;
    let concurrent = rig.vigil(AgentEnv::default(), &quiet_probe());
    assert_eq!(concurrent.tick_due(&id).await.unwrap(), crate::tick::TickOutcome::Ticked, "the concurrent tick fires the watch");
    let answer = call(&vigil, &id, Some(30), None).await;
    assert_eq!(answer["status"], "fired", "{answer}");
    let events = answer["events"].as_array().unwrap();
    assert_eq!(events.len(), 2, "attention and the terminal event: {answer}");
    assert_eq!((events[0]["event_id"].as_str(), &events[0]["redelivered"]), (Some(event_id.as_str()), &json!(true)), "{answer}");
    assert!(events[1].get("kind").is_none() && events[1]["event_id"] != events[0]["event_id"], "{answer}");
}

#[tokio::test(start_paused = true)]
async fn attention_that_was_never_delivered_is_not_lost_by_a_cancel() {
    // why: review S6, the cancel path of the same replacement. The cancel's
    //      envelope is the first the agent sees after the dropped delivery.
    // proves: CLAIM(S6), CLAIM(V3)
    let rig = Rig::new();
    let (vigil, id, _, event_id) = watch_with_undelivered_attention(&rig, "w", 7200).await;
    let cancelled = serde_json::to_value(vigil.cancel(&id).await.unwrap()).unwrap();
    assert_eq!(cancelled["status"], "cancelled", "{cancelled}");
    let event = only_event(&cancelled);
    assert_eq!((event["event_id"].as_str(), &event["redelivered"]), (Some(event_id.as_str()), &json!(true)), "{cancelled}");
}

// ---- review S7: a terminal envelope still knows when the last call was ----

#[tokio::test(start_paused = true)]
async fn a_terminal_envelope_reports_the_gap_since_the_last_call() {
    // why: review S7 (AI4). `finish` cleared `last_call_at` before the
    //      terminal envelope was built, so a watch that fired or expired on
    //      its second-or-later call said `since_last_call_s: null` and "first
    //      call" - the one envelope where the agent most needs the gap.
    //      Call 1 of `fires` (30 s) returns at t = 30, call 1 of `expires`
    //      returns at t = 60 (they run one after the other). Then 100 s pass
    //      (t = 160). (1) The change is written and call 2 fires `fires` at
    //      once: gap = 160 - 30 = 130 s. 400 s later (t = 560, past the 300 s
    //      deadline of `expires`) (2) call 2 finds `expires` expired: gap =
    //      560 - 60 = 500 s. In both the summary must not say "first call".
    // proves: CLAIM(AI4)
    let rig = Rig::new();
    let vigil = rig.vigil(AgentEnv::default(), &quiet_probe());
    let (fires, path) = quiet_watch(&rig, &vigil, "fires", 20, 3600, |_| {}).await;
    let (expires, _) = quiet_watch(&rig, &vigil, "expires", 20, 300, |_| {}).await;
    for id in [&fires, &expires] {
        let first = call(&vigil, id, Some(30), None).await;
        assert!(first["clock"]["since_last_call_s"].is_null(), "control: the first call is the first: {first}");
    }
    tokio::time::advance(Duration::from_secs(100)).await;
    append_file(&path, "boom\n");
    let fired = call(&vigil, &fires, Some(30), None).await;
    assert_eq!(fired["status"], "fired", "{fired}");
    assert_eq!(fired["clock"]["since_last_call_s"], 130, "S7: {fired}");
    assert!(!fired["clock"]["summary"].as_str().unwrap().contains("first call"), "{fired}");

    tokio::time::advance(Duration::from_secs(400)).await;
    let expired = call(&vigil, &expires, Some(30), None).await;
    assert_eq!(expired["status"], "expired", "{expired}");
    assert_eq!(expired["clock"]["since_last_call_s"], 500, "S7: {expired}");
    assert!(!expired["clock"]["summary"].as_str().unwrap().contains("first call"), "{expired}");
}

// ---- review S8: expect_s / stall_s at the edge of u64 -------------------------

#[tokio::test(start_paused = true)]
async fn a_watch_file_with_u64_max_attention_limits_neither_panics_nor_fires() {
    // why: review S8. `create` now refuses such values, but a watch file can
    //      still hold them (hand-edited, or written by another version), and
    //      the arithmetic on them used to overflow: `expect_s` u64::MAX made
    //      the overdue moment a negative duration (a panic), `stall_s` u64::MAX
    //      cast to -1 and fired at the first tick. Both are written into a
    //      stored watch with a live process; a 30 s call must neither panic nor
    //      raise anything (a limit that large is "never").
    // proves: CLAIM(AI5), CLAIM(S8-overflow)
    let rig = Rig::new();
    let vigil = rig.vigil(AgentEnv::default(), &quiet_probe());
    let sleeper = Sleeper::start();
    let (id, _) = judge_watch(&rig, &vigil, "w", 10, 3600, |request| request.inputs.push(Input::Process { pid: sleeper.pid(), want: None })).await;
    let file = rig.env.root().join("watches").join("w.json");
    let mut stored = stored_watch(&rig, &id);
    stored["expect_s"] = json!(u64::MAX);
    stored["stall_s"] = json!(u64::MAX);
    std::fs::write(&file, stored.to_string()).unwrap();

    let guarded = tokio::spawn(async move { call(&vigil, &id, Some(30), None).await }).await;
    assert!(guarded.is_ok(), "S8: the call panicked on a u64::MAX limit: {guarded:?}");
    let answer = guarded.unwrap();
    assert_eq!((&answer["status"], &answer["waited_s"], &answer["events"]), (&json!("pending"), &json!(30), &json!([])), "{answer}");
}
