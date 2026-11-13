//! Engine-level tests for holds (SPEC "Holds", H1-H12). Cross-cutting, like
//! `auto_interval_tests.rs`: each test drives `Vigil::hold` / `keepalive` on
//! paused tokio time and reads the envelope as the agent sees it (JSON), so
//! the wire format is what is pinned. Work inputs are scripted through
//! `ScriptedReader` (`set_text("qex:a", "running")`), whose `want_met` comes
//! from the same `hold::ended` rule production uses.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::cache::AgentEnv;
use crate::credit::CreditSource;
use crate::hold::HoldRequest;
use crate::keepalive::KeepaliveRequest;
use crate::testing::*;
use crate::watch::{CreateRequest, FireKind, FireRule, Input, ProcessWant, WatchFile, WatchId};
use crate::{SystemClock, Vigil};

const WEEK: u64 = 604_800;

pub(crate) fn rig() -> Env {
    Env::with_clock(Arc::new(TokioClock::new(spec_instant())))
}

pub(crate) fn qex(job: &str) -> Input {
    Input::Qex { job: job.into() }
}

/// A `fire = hold` request over `inputs`.
fn hold_request(id: &str, inputs: Vec<Input>) -> CreateRequest {
    CreateRequest {
        id: Some(id.into()),
        prompt: String::new(),
        inputs,
        interval_s: Some(30),
        deadline_s: None,
        fire: FireKind::Hold,
        judge_questions: None,
        judge_rule: None,
        expect_s: None,
        stall_s: None,
        orchestrator: None,
    }
}

pub(crate) async fn call(vigil: &Vigil, id: &str, max_s: Option<u64>) -> Value {
    let request = KeepaliveRequest { watch_id: Some(WatchId::parse(id).unwrap()), max_s, orchestrator: None };
    serde_json::to_value(vigil.keepalive(request).await.unwrap()).unwrap()
}

pub(crate) fn watch_file(vigil: &Vigil, id: &str) -> WatchFile {
    vigil.store().load(&WatchId::parse(id).unwrap()).unwrap()
}

struct Sleeper(std::process::Child);

impl Sleeper {
    fn start() -> Sleeper {
        Sleeper(std::process::Command::new("sleep").arg("600").spawn().unwrap())
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(start_paused = true)]
async fn a_hold_stays_pending_while_any_work_is_live_and_releases_when_all_of_it_has_ended() {
    // why: H1, H6, H7. Two jobs. While either is queued/running every call is
    //      `pending` / "call keepalive again" with NO events (a quiet hold costs
    //      a few hundred tokens). One job ending is not enough. When the second
    //      ends the hold fires once: `fired`, next "act on events", one event
    //      `input: "hold"` whose summary says why, in input order.
    // proves: CLAIM(H1), CLAIM(H6), CLAIM(H7)
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.reader.set_text("qex:b", "queued");
    env.vigil.create(hold_request("h", vec![qex("a"), qex("b")])).await.unwrap();

    let quiet = call(&env.vigil, "h", Some(30)).await;
    assert_eq!((quiet["status"].as_str(), quiet["next"].as_str(), quiet["events"].as_array().map(Vec::len)), (Some("pending"), Some("call keepalive again"), Some(0)), "{quiet}");

    env.reader.set_text("qex:a", "completed");
    let half = call(&env.vigil, "h", Some(60)).await;
    assert_eq!((half["status"].as_str(), half["next"].as_str(), half["events"].as_array().map(Vec::len)), (Some("pending"), Some("call keepalive again"), Some(0)), "one of two ended: {half}");

    env.reader.set_text("qex:b", "failed");
    let released = call(&env.vigil, "h", Some(60)).await;
    assert_eq!(released["status"], "fired", "{released}");
    assert_eq!(released["next"], "act on events");
    let events = released["events"].as_array().unwrap();
    assert_eq!(events.len(), 1, "{released}");
    assert_eq!(events[0]["input"], "hold");
    assert_eq!(events[0]["summary"], "hold released: qex:a completed; qex:b failed");
    assert!(events[0].get("kind").is_none(), "an ordinary event, not an attention one: {released}");
    assert_eq!(events[0]["watch_id"], "h");
}

#[tokio::test(start_paused = true)]
async fn a_read_error_does_not_release_a_hold_but_gone_and_a_finished_pid_do() {
    // why: H2 (decision). qex failing to answer (`error: ...`) or answering a
    //      state vigil does not know must keep the hold live; only a known
    //      terminal state or `gone` ends the job. A process input ends on
    //      `exited` (real pid: a child that has been killed and reaped).
    // proves: CLAIM(H2)
    let env = rig();
    env.reader.set_text("qex:a", "running");
    let mut child = std::process::Command::new("sleep").arg("600").spawn().unwrap();
    let inputs = vec![qex("a"), Input::Process { pid: child.id(), want: Some(ProcessWant::Exited) }];
    env.vigil.create(hold_request("h", inputs)).await.unwrap();

    env.reader.set_text("qex:a", "error: cannot run qex: No such file or directory (os error 2)");
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(call(&env.vigil, "h", Some(30)).await["status"], "pending", "qex unreadable: still held");
    env.reader.set_text("qex:a", "some-state-from-a-newer-qex");
    assert_eq!(call(&env.vigil, "h", Some(60)).await["status"], "pending", "unknown state: still held");

    env.reader.set_text("qex:a", "gone");
    let released = call(&env.vigil, "h", Some(60)).await;
    assert_eq!(released["status"], "fired", "{released}");
    assert_eq!(released["events"][0]["summary"], format!("hold released: qex:a gone; process:{} exited", child.id()));
}

#[tokio::test(start_paused = true)]
async fn a_hold_outlives_the_default_deadline_and_a_bare_hold_never_fires() {
    // why: H5. The failure this feature exists for: a 24 h / 1 h dummy watch
    //      lapsing. A hold's deadline is the 7-day maximum, so after 2 h of
    //      quiet it is pending (control: an ordinary watch with the default
    //      3600 s deadline has expired). A bare hold (no work) is live until
    //      cancelled; it only expires at 7 days, with the ordinary "watch
    //      ended" next.
    // proves: CLAIM(H5)
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.vigil.create(hold_request("with-work", vec![qex("a")])).await.unwrap();
    env.vigil.create(hold_request("bare", vec![])).await.unwrap();
    let path = env.input_path("c.log");
    write_file(&path, "x\n");
    let mut control = change_request("control", &path, 60);
    control.deadline_s = None;
    env.vigil.create(control).await.unwrap();

    tokio::time::advance(Duration::from_secs(2 * 3600)).await;
    assert_eq!(call(&env.vigil, "with-work", Some(30)).await["status"], "pending");
    let bare = call(&env.vigil, "bare", Some(30)).await;
    assert_eq!((bare["status"].as_str(), bare["next"].as_str()), (Some("pending"), Some("call keepalive again")), "{bare}");
    assert_eq!(bare["events"], json!([]));
    assert_eq!(call(&env.vigil, "control", Some(30)).await["status"], "expired", "control: a default watch is gone after 2 h");

    tokio::time::advance(Duration::from_secs(WEEK)).await;
    let expired = call(&env.vigil, "bare", Some(30)).await;
    assert_eq!((expired["status"].as_str(), expired["next"].as_str()), (Some("expired"), Some("watch ended; create a new watch if still needed")), "{expired}");
    assert_eq!(expired["events"], json!([]), "expiry adds no event (R5)");
}

#[tokio::test(start_paused = true)]
async fn a_hold_deadline_defaults_to_seven_days_may_be_lowered_and_not_raised() {
    // why: H5. 604800 s is the longest any watch lives (V19), so it is the
    //      hold's default. A lower value is honoured; a higher one is the
    //      existing DeadlineOutOfRange error (no silent clamp).
    // proves: CLAIM(H5)
    let env = rig();
    env.reader.set_text("qex:a", "running");
    let created = env.vigil.create(hold_request("h", vec![qex("a")])).await.unwrap();
    assert_eq!(created.deadline_at, created.created_at.plus_secs(WEEK));
    let mut short = hold_request("short", vec![qex("a")]);
    short.deadline_s = Some(3600);
    let created = env.vigil.create(short).await.unwrap();
    assert_eq!(created.deadline_at, created.created_at.plus_secs(3600));
    let mut long = hold_request("long", vec![qex("a")]);
    long.deadline_s = Some(WEEK + 1);
    let error = env.vigil.create(long).await.unwrap_err().to_string();
    assert!(error.contains("604800"), "{error}");
}

#[tokio::test(start_paused = true)]
async fn a_hold_takes_work_inputs_only_and_no_stall_limit() {
    // why: H4. Files, globs, commands, URLs and ports are not work (use
    //      fire = condition); a process input must say `want: exited`;
    //      `stall_s` is refused. Each refusal names the thing. `expect_s` and
    //      the bare hold are fine. Nothing is written for a refused hold.
    // proves: CLAIM(H4)
    let env = rig();
    let refused: Vec<(&str, Vec<Input>, &str)> = vec![
        ("file", vec![Input::File { path: "/tmp/x.log".into(), tail_bytes: None }], "file:/tmp/x.log"),
        ("cmd", vec![Input::Cmd { argv: vec!["true".into()], timeout_s: None, cwd: None }], "cmd:true"),
        ("url", vec![Input::Url { url: "http://127.0.0.1:9/".into() }], "url:http://127.0.0.1:9/"),
        ("port", vec![Input::Port { host: "127.0.0.1".into(), port: 9, want: None }], "port:127.0.0.1:9"),
        ("process", vec![Input::Process { pid: 1, want: None }], "process:1"),
        ("running", vec![Input::Process { pid: 1, want: Some(ProcessWant::Running) }], "process:1"),
    ];
    for (id, inputs, needle) in refused {
        let error = env.vigil.create(hold_request(id, inputs)).await.expect_err(id).to_string();
        assert!(error.contains(needle) && error.contains("hold"), "{id}: {error}");
        assert!(!env.vigil.store().watch_path(&WatchId::parse(id).unwrap()).exists(), "{id}: nothing written");
    }
    let mut stalled = hold_request("stalled", vec![qex("a")]);
    stalled.stall_s = Some(60);
    let error = env.vigil.create(stalled).await.unwrap_err().to_string();
    assert!(error.contains("stall_s") && error.contains("hold"), "{error}");

    env.reader.set_text("qex:a", "running");
    let mut expecting = hold_request("expecting", vec![qex("a")]);
    expecting.expect_s = Some(600);
    env.vigil.create(expecting).await.unwrap();
    env.vigil.create(hold_request("bare", vec![])).await.unwrap();
    let all_kinds = vec![qex("a"), Input::QexIdle { dir: "/w".into() }, Input::PathExists { path: "/p".into() }, Input::Until { at: spec_instant() }, Input::Process { pid: 1, want: Some(ProcessWant::Exited) }];
    env.reader.set_text("idle:/w", "idle");
    env.vigil.create(hold_request("all-kinds", all_kinds)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_hold_is_an_orchestrator_watch_by_default_and_other_watches_are_not() {
    // why: H9. Holds are for main sessions: orchestrator defaults to true in
    //      the stored watch; an explicit false wins. Old fire rules keep
    //      their default of false (H12), so nothing changes for them.
    // proves: CLAIM(H9), CLAIM(H12)
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.vigil.create(hold_request("default", vec![qex("a")])).await.unwrap();
    let mut off = hold_request("off", vec![qex("a")]);
    off.orchestrator = Some(false);
    env.vigil.create(off).await.unwrap();
    let path = env.input_path("c.log");
    write_file(&path, "x\n");
    env.vigil.create(change_request("change", &path, 60)).await.unwrap();
    let flag = |id: &str| watch_file(&env.vigil, id).spec.orchestrator;
    assert_eq!((flag("default"), flag("off"), flag("change")), (true, false, false));
    assert!(matches!(watch_file(&env.vigil, "default").spec.fire, FireRule::Hold));
}

fn claude_vigil(env: &Env, projects: &std::path::Path) -> Vigil {
    let mut config = env.config.clone();
    config.claude_projects_dir = Some(projects.to_path_buf());
    config.uw_command = std::path::PathBuf::from("/nonexistent/vigil-tests-must-not-run-uw");
    let probe = ScriptedCreditProbe::new(credit_state(false, CreditSource::Uw));
    Vigil::new(env.root(), env.clock.clone(), env.judge.clone(), config)
        .with_reader(env.reader.clone())
        .with_agent_env(AgentEnv { claude_session_id: Some("sess-1".into()), codex: false })
        .with_credit_probe(probe)
}

#[tokio::test(start_paused = true)]
async fn a_hold_call_with_no_flag_gets_the_orchestrator_window_on_a_one_hour_cache() {
    // why: H9 + the owner's rule: the main session holds with vigil and the
    //      50 minute window is what makes that cheap. A no-id keepalive with
    //      neither `orchestrator` nor `max_s` covers the hold, which stores
    //      orchestrator = true, so on a 1 h transcript the call blocks 3000 s.
    //      Control: the same call over a plain watch is 270 s.
    // proves: CLAIM(H9)
    let env = rig();
    let projects = tempfile::tempdir().unwrap();
    write_transcript(projects.path(), "p", "sess-1", &[assistant_line(0, 500), assistant_line(900, 0)]);
    let vigil = claude_vigil(&env, projects.path());
    env.reader.set_text("qex:a", "running");
    vigil.create(hold_request("h", vec![qex("a")])).await.unwrap();
    let envelope = serde_json::to_value(vigil.keepalive(KeepaliveRequest { watch_id: None, max_s: None, orchestrator: None }).await.unwrap()).unwrap();
    assert_eq!((envelope["status"].as_str(), envelope["max_s"].as_u64()), (Some("pending"), Some(3000)), "{envelope}");
    assert_eq!((envelope["cache"]["ttl"].as_str(), envelope["cache"]["interval_s"].as_u64()), (Some("1h"), Some(3000)));
    assert_eq!(envelope["next"], "call keepalive again");

    let other = rig();
    let plain = claude_vigil(&other, projects.path());
    let path = other.input_path("c.log");
    write_file(&path, "x\n");
    plain.create(change_request("c", &path, 60)).await.unwrap();
    let envelope = serde_json::to_value(plain.keepalive(KeepaliveRequest { watch_id: None, max_s: None, orchestrator: None }).await.unwrap()).unwrap();
    assert_eq!(envelope["max_s"], 270, "control: {envelope}");
}

#[tokio::test(start_paused = true)]
async fn a_hold_on_a_running_process_never_raises_the_default_stall() {
    // why: H8. The default stall rule (900 s without file/glob/cmd output while
    //      a process input runs) would wake the orchestrator every 15 minutes
    //      for a hold, which is waiting by definition. A long window over a
    //      hold on a live pid returns pending at 3000 s with no event.
    //      Control (the existing rule, AI5): the same pid in a `condition`
    //      watch with a file raises `stalled` at 900.
    // proves: CLAIM(H8)
    let env = rig();
    let projects = tempfile::tempdir().unwrap();
    let vigil = claude_vigil(&env, projects.path());
    let sleeper = Sleeper::start();
    let pid = sleeper.0.id();
    vigil.create(hold_request("h", vec![Input::Process { pid, want: Some(ProcessWant::Exited) }])).await.unwrap();
    let envelope = serde_json::to_value(vigil.keepalive(KeepaliveRequest { watch_id: Some(WatchId::parse("h").unwrap()), max_s: Some(3000), orchestrator: Some(true) }).await.unwrap()).unwrap();
    assert_eq!((envelope["status"].as_str(), envelope["waited_s"].as_u64()), (Some("pending"), Some(3000)), "{envelope}");
    assert_eq!(envelope["events"], json!([]));
    assert!(envelope["cache"].get("interval_s").is_some());

    let path = env.input_path("c.log");
    write_file(&path, "x\n");
    let mut control = change_request("control", &path, 20);
    control.fire = FireKind::Condition;
    control.inputs.push(Input::Process { pid, want: Some(ProcessWant::Exited) });
    vigil.create(control).await.unwrap();
    let envelope = serde_json::to_value(vigil.keepalive(KeepaliveRequest { watch_id: Some(WatchId::parse("control").unwrap()), max_s: Some(3000), orchestrator: Some(true) }).await.unwrap()).unwrap();
    assert_eq!(envelope["events"][0]["reason"], "stalled", "control: {envelope}");
}

#[tokio::test(start_paused = true)]
async fn expect_s_on_a_hold_raises_overdue_like_any_watch() {
    // why: H4: `expect_s` is allowed on a hold. Work expected in 100 s that is
    //      still running at 150 s (min(1.5 x, x + 540)) wakes the agent with
    //      an `attention` event; the hold stays live.
    // proves: CLAIM(H4)
    let env = rig();
    env.reader.set_text("qex:a", "running");
    let mut request = hold_request("h", vec![qex("a")]);
    request.expect_s = Some(100);
    env.vigil.create(request).await.unwrap();
    let envelope = call(&env.vigil, "h", Some(200)).await;
    assert_eq!((envelope["status"].as_str(), envelope["next"].as_str()), (Some("pending"), Some("act on events")), "{envelope}");
    assert_eq!((envelope["events"][0]["kind"].as_str(), envelope["events"][0]["reason"].as_str()), (Some("attention"), Some("overdue")), "{envelope}");
    assert_eq!(envelope["waited_s"], 150);
}

// ---- the front door: Vigil::hold ----------------------------------------

pub(crate) fn request_for(work: &[&str]) -> HoldRequest {
    HoldRequest { id: Some("h".into()), work: work.iter().map(|spec| spec.to_string()).collect(), ..HoldRequest::default() }
}

#[tokio::test(start_paused = true)]
async fn vigil_hold_builds_a_hold_watch_from_work_specs() {
    // why: H11. `--for` specs become the stored inputs in order, the watch is
    //      `fire = hold`, orchestrator true, interval 30 s, deadline 7 days;
    //      a relative `file:` resolves against the creating cwd (V11).
    // proves: CLAIM(H11), CLAIM(H1)
    let env = rig();
    env.reader.set_text("qex:ab12", "running");
    let vigil = env.another_process().with_cwd("/work/repo".into());
    let created = vigil.hold(request_for(&["qex:ab12", "pid:1", "file:done.marker", "until:2026-11-13T00:00:00Z"])).await.unwrap();
    assert_eq!(created.deadline_at, created.created_at.plus_secs(WEEK));
    let stored = watch_file(&vigil, "h");
    assert!(matches!(stored.spec.fire, FireRule::Hold));
    assert!(stored.spec.orchestrator);
    assert_eq!(stored.spec.interval_s, 30);
    let wire = serde_json::to_value(&stored.spec.inputs).unwrap();
    assert_eq!(
        wire,
        json!([
            {"kind": "qex", "job": "ab12"},
            {"kind": "process", "pid": 1, "want": "exited"},
            {"kind": "path_exists", "path": "/work/repo/done.marker"},
            {"kind": "until", "at": "2026-11-13T00:00:00Z"},
        ])
    );
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(env.root().join("watches/h.json")).unwrap()).unwrap();
    assert_eq!(raw["fire"], "hold", "the wire name of the fire rule");
}

#[tokio::test(start_paused = true)]
async fn a_bare_hold_has_no_inputs_and_orchestrator_can_be_turned_off() {
    // why: H5/H11: `vigil hold` with nothing to wait on is the bare hold; the
    //      orchestrator flag can be set false.
    // proves: CLAIM(H5), CLAIM(H9)
    let env = rig();
    let mut request = request_for(&[]);
    request.orchestrator = Some(false);
    env.vigil.hold(request).await.unwrap();
    let stored = watch_file(&env.vigil, "h");
    assert!(stored.spec.inputs.is_empty() && !stored.spec.orchestrator && matches!(stored.spec.fire, FireRule::Hold));
    let quiet = call(&env.vigil, "h", Some(30)).await;
    assert_eq!((quiet["status"].as_str(), quiet["next"].as_str()), (Some("pending"), Some("call keepalive again")));
}

#[tokio::test(start_paused = true)]
async fn a_hold_on_a_job_qex_does_not_know_is_refused_and_leaves_nothing() {
    // why: H3. A typo in a job id must be an error now, not a hold that
    //      quietly lives 7 days (or, worse, releases at once). qex's "no such
    //      job" reads `gone` at creation. Nothing is written, so the id can
    //      be used again. Control: a job that exists is accepted.
    // proves: CLAIM(H3)
    let env = rig();
    env.reader.set_text("qex:nope", "gone");
    env.reader.set_text("qex:real", "running");
    let error = env.vigil.hold(request_for(&["qex:real", "qex:nope"])).await.unwrap_err().to_string();
    assert!(error.contains("nope") && error.contains("not found"), "{error}");
    assert!(!env.vigil.store().watch_path(&WatchId::parse("h").unwrap()).exists());
    env.vigil.hold(request_for(&["qex:real"])).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_bad_work_spec_is_refused_before_anything_is_read_or_written() {
    // why: H2/H11 through the engine: the parse error reaches the caller, no
    //      watch file exists.
    // proves: CLAIM(H2)
    let env = rig();
    let error = env.vigil.hold(request_for(&["qex:real", "tomorrow:morning"])).await.unwrap_err().to_string();
    assert!(error.contains("tomorrow:morning"), "{error}");
    assert!(!env.vigil.store().watch_path(&WatchId::parse("h").unwrap()).exists());
}

#[tokio::test(start_paused = true)]
async fn until_idle_holds_on_the_creating_directory_and_releases_when_the_queue_there_is_idle() {
    // why: H10. `until_idle` is `idle:<cwd>`. Busy -> pending; idle -> released
    //      with the reason naming the directory. Another directory's jobs are
    //      not this hold's business (the reader is asked about this dir only).
    // proves: CLAIM(H10)
    let env = rig();
    env.reader.set_text("idle:/work/repo", "busy: 2 active (aaaaaaaa, bbbbbbbb)");
    let vigil = env.another_process().with_cwd("/work/repo".into());
    let mut request = request_for(&[]);
    request.until_idle = true;
    vigil.hold(request).await.unwrap();
    assert_eq!(serde_json::to_value(&watch_file(&vigil, "h").spec.inputs).unwrap(), json!([{"kind": "qex_idle", "dir": "/work/repo"}]));
    assert_eq!(call(&vigil, "h", Some(30)).await["status"], "pending");
    env.reader.set_text("idle:/work/repo", "idle");
    let released = call(&vigil, "h", Some(60)).await;
    assert_eq!(released["status"], "fired", "{released}");
    assert_eq!(released["events"][0]["summary"], "hold released: idle:/work/repo idle");
}

#[tokio::test]
async fn until_a_time_and_a_marker_file_end_a_hold_on_their_own() {
    // why: H2. `until:` ends at the wall clock (virtual here); `file:` ends
    //      when the path exists. Both are the real readers (not scripted):
    //      the marker is a real file, the time is the real clock of the
    //      reader, so this test runs in real time (interval 1 s).
    // proves: CLAIM(H2)
    let env = Env::with_clock(Arc::new(SystemClock));
    let marker = env.input_path("done.marker");
    let past = crate::Timestamp(chrono::Utc::now() - chrono::Duration::seconds(5));
    let mut request = hold_request("h", vec![Input::PathExists { path: marker.clone() }, Input::Until { at: past }]);
    request.interval_s = Some(1);
    env.vigil.create(request).await.unwrap();
    // Not scripted -> the production reader runs.
    let first = tokio::time::timeout(Duration::from_millis(1500), env.vigil.keepalive(KeepaliveRequest { watch_id: Some(WatchId::parse("h").unwrap()), max_s: Some(30), orchestrator: None })).await;
    assert!(first.is_err(), "the marker does not exist yet: the hold is still live");
    write_file(&marker, "done\n");
    let released = serde_json::to_value(env.vigil.keepalive(KeepaliveRequest { watch_id: Some(WatchId::parse("h").unwrap()), max_s: Some(30), orchestrator: None }).await.unwrap()).unwrap();
    assert_eq!(released["status"], "fired", "{released}");
    let summary = released["events"][0]["summary"].as_str().unwrap();
    assert!(summary.starts_with("hold released: exists:") && summary.contains("present") && summary.contains("reached"), "{summary}");
}

// ---- wire compatibility (H12) -------------------------------------------

#[test]
fn watch_files_written_before_holds_still_load_unchanged() {
    // why: H12. Literal watch files as the pre-hold binary wrote them (a change
    //      watch and a condition watch): they must load, keep their fire rule
    //      and a false orchestrator flag, and serialize back to the same JSON
    //      value. `fire: "hold"` and the new kinds are additions only.
    // proves: CLAIM(H12)
    let change = json!({
        "id": "old-change", "prompt": "", "inputs": [{"kind": "file", "path": "/tmp/a.log"}],
        "interval_s": 20, "deadline_s": 3600, "fire": "change",
        "status": "pending", "created_at": "2026-11-01T10:00:00Z", "deadline_at": "2026-11-01T11:00:00Z",
        "tick": 3, "input_states": [{"offset": 5}], "judge_strikes": 0, "judge_status": "skipped",
        "events": [], "last_digest": ""
    });
    let condition = json!({
        "id": "old-cond", "prompt": "p", "inputs": [{"kind": "process", "pid": 7, "want": "exited"}],
        "interval_s": 20, "deadline_s": 3600, "fire": "condition", "orchestrator": true,
        "status": "pending", "created_at": "2026-11-01T10:00:00Z", "deadline_at": "2026-11-01T11:00:00Z",
        "tick": 0, "input_states": [], "judge_strikes": 0, "judge_status": "skipped",
        "events": [], "last_digest": ""
    });
    for (value, orchestrator) in [(change, false), (condition, true)] {
        let loaded: WatchFile = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(loaded.spec.orchestrator, orchestrator);
        assert!(!matches!(loaded.spec.fire, FireRule::Hold));
        assert_eq!(serde_json::to_value(&loaded).unwrap(), value);
    }
    // And the new rule is spelled `hold`.
    let hold = serde_json::to_value(crate::watch::WatchSpec::parse(hold_request("h", vec![])).unwrap()).unwrap();
    assert_eq!(hold["fire"], "hold");
}

const JOB_ID: &str = "0d31c8ac-25e4-4ab2-8a3b-9b24ccd533c9";

#[tokio::test(start_paused = true)]
async fn a_hold_made_on_a_name_follows_the_job_it_resolved_to_even_after_the_name_is_reused() {
    // why: S3. `qex:build` named one job when the hold was made. A later job
    //      reuses the name, and qex then answers "names 2 jobs" for it (real
    //      capture in hold::tests). The stored input is the FULL id, so the
    //      hold neither reads the name again nor goes ambiguous: it follows
    //      the first job to its end. The name's own script reads as an error to
    //      prove it is never asked.
    // proves: CLAIM(H3)
    let env = rig();
    env.reader.set_qex_resolution("build", Ok(JOB_ID.to_string()));
    env.reader.set_text(&format!("qex:{JOB_ID}"), "running");
    env.reader.set_text("qex:build", "error: `build` names 2 jobs");
    env.vigil.hold(request_for(&["qex:build"])).await.unwrap();
    assert_eq!(watch_file(&env.vigil, "h").spec.inputs, vec![qex(JOB_ID)], "the full id is what is stored");

    let quiet = call(&env.vigil, "h", Some(30)).await;
    assert_eq!(quiet["status"], "pending", "{quiet}");
    env.reader.set_text(&format!("qex:{JOB_ID}"), "completed");
    let released = call(&env.vigil, "h", Some(60)).await;
    assert_eq!(released["status"], "fired", "{released}");
    assert_eq!(released["events"][0]["summary"], format!("hold released: qex:{JOB_ID} completed"));
    assert_eq!(env.reader.started_count("qex:build"), 0, "the name is never read after creation");
}

#[tokio::test(start_paused = true)]
async fn a_hold_on_an_ambiguous_or_unresolvable_name_is_refused_with_the_cause_and_leaves_nothing() {
    // why: S3/H3. A name that already names two jobs has no identity to store,
    //      and a qex that cannot answer cannot tell which job a name is. Both
    //      refuse, each naming its cause, and write nothing (the id stays
    //      usable). Control: a full id is accepted.
    // proves: CLAIM(H3)
    use crate::error::CreateError;
    let env = rig();
    env.reader.set_qex_resolution("dup", Err(CreateError::QexJobAmbiguous { job: "dup".into(), detail: "`dup` names 2 jobs".into() }));
    env.reader.set_qex_resolution("mute", Err(CreateError::QexJobUnresolved { job: "mute".into(), reason: "timeout after 10s".into() }));
    env.reader.set_text(&format!("qex:{JOB_ID}"), "running");
    let ambiguous = env.vigil.hold(request_for(&["qex:dup"])).await.unwrap_err().to_string();
    assert!(ambiguous.contains("dup") && ambiguous.contains("ambiguous") && ambiguous.contains("full job id"), "{ambiguous}");
    let unresolved = env.vigil.hold(request_for(&["qex:mute"])).await.unwrap_err().to_string();
    assert!(unresolved.contains("mute") && unresolved.contains("timeout"), "{unresolved}");
    assert!(!env.vigil.store().watch_path(&WatchId::parse("h").unwrap()).exists());
    env.vigil.hold(request_for(&[&format!("qex:{JOB_ID}")])).await.unwrap();
}
