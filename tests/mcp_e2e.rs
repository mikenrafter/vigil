//! End-to-end suite for `vigil mcp`. Lives in tests/ because it spawns the
//! built binary (CARGO_BIN_EXE_vigil) as a child process and speaks real
//! stdio MCP to it through rmcp's TokioChildProcess client. Real seconds are
//! unavoidable here (a process boundary has no fake clock); every test uses
//! `interval_s: 1` and a file that is written to, so each finishes in < 5 s
//! and nothing waits out the 30 s max_s floor.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;

/// A real qex job id (tests/fixtures/qex) for the hold tests.
const J1: &str = "0d31c8ac-25e4-4ab2-8a3b-9b24ccd533c9";
use rmcp::{
    ClientHandler, RoleClient, ServiceExt,
    model::{CallToolRequestParams, CallToolResult, ProgressNotificationParam},
    service::{NotificationContext, Peer, RunningService},
    transport::{ConfigureCommandExt, TokioChildProcess},
};

type Client<H = ()> = RunningService<RoleClient, H>;

async fn connect(sandbox: &Sandbox) -> (Client, u32) {
    let transport = TokioChildProcess::new(sandbox.tokio_command().configure(|c| {
        c.arg("mcp");
    }))
    .unwrap();
    let pid = transport.id().expect("child pid");
    let client = ().serve(transport).await.expect("initialize handshake");
    (client, pid)
}

fn args(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    value.as_object().unwrap().clone()
}

async fn call(peer: &Peer<RoleClient>, tool: &'static str, arguments: serde_json::Value) -> CallToolResult {
    peer.call_tool(CallToolRequestParams::new(tool).with_arguments(args(arguments))).await.unwrap()
}

fn text_json(result: &CallToolResult) -> serde_json::Value {
    assert_eq!(result.content.len(), 1, "one JSON text block");
    let text = &result.content[0].as_text().expect("text content").text;
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

fn watch_args(id: &str, path: &std::path::Path) -> serde_json::Value {
    serde_json::json!({"id": id, "prompt": "", "inputs": [{"kind": "file", "path": path}],
        "interval_s": 1, "deadline_s": 3600, "fire": "change"})
}

#[tokio::test]
async fn initialize_sends_short_instructions_that_teach_the_loop() {
    // why: SPEC "Packaging" + V20 (S20): server `instructions` (sent at
    //      initialize) must be under 512 characters and say: create a watch,
    //      call keepalive repeatedly while `next` is exactly "call keepalive
    //      again", and stop on anything else ("act on events", or the
    //      watch-ended text), so the agent needs no skill and an expired watch
    //      cannot loop forever.
    // proves: CLAIM(instructions)
    let sandbox = Sandbox::new();
    let (client, _) = connect(&sandbox).await;
    let instructions = client.peer_info().and_then(|info| info.instructions.clone()).expect("instructions present");
    assert!(instructions.chars().count() < 512, "{} chars", instructions.chars().count());
    for needle in ["keepalive", "watch_create", "call keepalive again", "act on events", "watch ended"] {
        assert!(instructions.contains(needle), "missing {needle:?} in {instructions:?}");
    }
    assert!(instructions.to_lowercase().contains("stop"), "must say when to stop: {instructions:?}");
    // why: W45-C3: an agent that is done must find `cold` without a skill.
    // proves: W45-C3
    // catches: the instructions line for `cold` left out.
    assert!(instructions.contains("call `cold`, post a short summary, end the turn"), "missing the cold line in {instructions:?}");
    // why: W48 (owner 2026-11-14): a foreground MCP keepalive in a main session is interrupted by
    //      every owner message; the 3-day transcript sweep found 22 such calls, each following
    //      these instructions. A main session that has background shells waits with the CLI.
    // proves: W48-I1
    // catches: instructions that still send every model to the foreground tool.
    assert!(instructions.contains("vigil keepalive <id> --orchestrator") && instructions.contains("in the background"),
            "missing the background CLI wait for main sessions in {instructions:?}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn exactly_the_spec_tools_are_listed_with_descriptions() {
    // why: SPEC "MCP tools": watch_create, keepalive, watch_status,
    //      watch_list, watch_cancel, and "Advisories": advice, advisory_* - no
    //      more, no fewer. keepalive's schema
    //      exposes watch_id and max_s.
    //      W42-D7: the list is exact and includes `hold_done` (a pure-MCP
    //      agent must be able to end a `none` item).
    //      W45-C2: and `cold`.
    // proves: CLAIM(tool-list), W42-D7, W45-C2
    // catches: a hold_done CLI subcommand with no MCP tool.
    let sandbox = Sandbox::new();
    let (client, _) = connect(&sandbox).await;
    let tools = client.list_all_tools().await.unwrap();
    let mut names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    // The five watch tools of the SPEC, `hold` (SPEC "Holds"), `hold_add` (W42-H2), `hold_done` (W42-D7) and the six advisory tools (SPEC "Advisories"), plus six reminder tools (SPEC "Reminders").
    assert_eq!(names, vec!["advice", "advisory_add", "advisory_list", "advisory_rm", "advisory_set", "advisory_show", "cold", "hold", "hold_add", "hold_done", "keepalive", "remind_add", "remind_done", "remind_list", "remind_rm", "remind_show", "remind_snooze", "watch_cancel", "watch_create", "watch_list", "watch_status"]);
    for tool in &tools {
        assert!(tool.description.as_deref().is_some_and(|d| !d.is_empty()), "{} has no description", tool.name);
    }
    let keepalive = tools.iter().find(|t| t.name == "keepalive").unwrap();
    let properties = keepalive.input_schema.get("properties").and_then(|p| p.as_object()).expect("properties");
    assert!(properties.contains_key("watch_id") && properties.contains_key("max_s"));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn watch_create_declares_its_inputs_and_judge_fields_as_objects() {
    // why: with an untyped `items` schema, Codex (gpt-6-luna) sent each input
    //      as a JSON-encoded string and the call failed (host-b smoke run).
    // proves: CLAIM(create-schema-objects)
    let sandbox = Sandbox::new();
    let (client, _) = connect(&sandbox).await;
    let tools = client.list_all_tools().await.unwrap();
    let create = tools.iter().find(|t| t.name == "watch_create").unwrap();
    let schema = serde_json::Value::Object((*create.input_schema).clone());
    let properties = &schema["properties"];
    assert_eq!(properties["inputs"]["items"]["type"], "object", "{schema}");
    for field in ["judge_questions", "judge_rule"] {
        assert!(properties[field].to_string().contains("\"object\""), "{field}: {}", properties[field]);
    }
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn create_then_one_write_then_keepalive_returns_the_fired_envelope() {
    // why: SPEC acceptance: the agent creates a watch, then calls only
    //      keepalive and receives the failure line. V1 (S1): the baseline is
    //      taken at create, so ONE write between create and the first
    //      keepalive (no repeating appender, which used to hide the gap) is
    //      seen on the very first tick (tick 1). Checks the envelope: status
    //      fired, the line in events and digest, real timestamps,
    //      session_ticks 1, watch_id (V8), event_id (V3, W1: `build:1:0`),
    //      next "act on events". Must complete well inside 5 s.
    // proves: CLAIM(baseline-at-create)
    let sandbox = Sandbox::new();
    let input = sandbox.input("build.log");
    std::fs::write(&input, "boot\n").unwrap();
    let (client, _) = connect(&sandbox).await;

    let created = call(client.peer(), "watch_create", watch_args("build", &input)).await;
    assert_ne!(created.is_error, Some(true), "{created:?}");
    let created = text_json(&created);
    assert_eq!(created["watch_id"], "build");
    assert_eq!(created["status"], "pending");

    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(&input).unwrap();
        writeln!(file, "ERROR: linking failed").unwrap();
    }
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(5), call(client.peer(), "keepalive", serde_json::json!({"watch_id": "build"})))
        .await
        .expect("keepalive should return on fire, not wait out max_s");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_ne!(result.is_error, Some(true), "a fire is a normal result");
    let envelope = text_json(&result);
    assert_eq!(envelope["status"], "fired");
    assert_eq!(envelope["watch_id"], "build");
    assert_eq!(envelope["next"], "act on events");
    assert_eq!(envelope["session_ticks"], 1);
    assert_eq!(envelope["tick"], 1, "the write before the first call is seen by the first tick");
    assert!(envelope["now"].as_str().unwrap().ends_with('Z'));
    assert!(envelope["events"][0]["summary"].as_str().unwrap().contains("ERROR: linking failed"));
    assert!(envelope["digest"].as_str().unwrap().contains("ERROR: linking failed"));
    assert_eq!(envelope["events"][0]["watch_id"], "build");
    assert_eq!(envelope["events"][0]["event_id"], "build:1:0", "W1: ids are <watch>:<tick>:<n>, n from 0");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn real_failures_set_is_error_and_a_judge_watch_needs_a_reachable_judge() {
    // why: SPEC "MCP tools": "set isError only for real failures". A bad id,
    //      an unknown watch, and a fire=judge watch while the judge is
    //      unreachable (SPEC "Packaging": error at watch_create; the sandbox
    //      judge URL is a closed port) are failures; a plain create is not.
    // proves: CLAIM(is-error-only-on-failure)
    let sandbox = Sandbox::new();
    let input = sandbox.input("a.log");
    std::fs::write(&input, "x\n").unwrap();
    let (client, _) = connect(&sandbox).await;
    let peer = client.peer();

    let bad_id = call(peer, "watch_create", watch_args("bad id!", &input)).await;
    assert_eq!(bad_id.is_error, Some(true));
    let unknown = call(peer, "keepalive", serde_json::json!({"watch_id": "ghost", "max_s": 30})).await;
    assert_eq!(unknown.is_error, Some(true));
    let judge = call(
        peer,
        "watch_create",
        serde_json::json!({"id": "j", "prompt": "p", "inputs": [{"kind": "file", "path": input}], "fire": "judge",
            "judge_questions": {"q": {"type": "noul", "instructions": "?"}}, "judge_rule": {"question": "q", "noul_gte": 0.5}}),
    )
    .await;
    assert_eq!(judge.is_error, Some(true), "judge down at create time");
    let https = call(
        peer,
        "watch_create",
        serde_json::json!({"id": "tls", "inputs": [{"kind": "url", "url": "https://example.com/"}], "fire": "change"}),
    )
    .await;
    assert_eq!(https.is_error, Some(true), "V19: https is a create-time error");
    let fine = call(peer, "watch_create", watch_args("fine", &input)).await;
    assert_ne!(fine.is_error, Some(true));
    let status = call(peer, "watch_status", serde_json::json!({"watch_id": "fine"})).await;
    assert_ne!(status.is_error, Some(true));
    assert_eq!(text_json(&status)["status"], "pending");
    let listed = text_json(&call(peer, "watch_list", serde_json::json!({})).await);
    assert!(listed.to_string().contains("fine"));
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_killed_server_is_replaced_by_one_that_continues_the_same_watch() {
    // why: SPEC acceptance: "Killing vigil mcp mid-watch and calling keepalive
    //      with the watch id from a new process does not reset elapsed time or
    //      the tick count." Server A is SIGKILLed while its keepalive is
    //      blocked; server B resumes: created_at/deadline_at in the file are
    //      unchanged, tick continues past A's count, elapsed_s covers A's
    //      lifetime, and session_ticks restarts at 1 (in-memory, per process).
    // proves: CLAIM(restart-continuity)
    let sandbox = Sandbox::new();
    let input = sandbox.input("a.log");
    std::fs::write(&input, "boot\n").unwrap();
    let (client_a, pid_a) = connect(&sandbox).await;
    let created = text_json(&call(client_a.peer(), "watch_create", watch_args("w", &input)).await);
    let blocked = {
        let peer = client_a.peer().clone();
        tokio::spawn(async move {
            let _ = peer
                .call_tool(CallToolRequestParams::new("keepalive").with_arguments(args(serde_json::json!({"watch_id": "w", "max_s": 30}))))
                .await;
        })
    };
    assert!(
        wait_until_async(Duration::from_secs(5), || sandbox.watch_file("w").is_some_and(|w| w["tick"].as_u64() >= Some(3))).await,
        "server A never reached tick 3"
    );
    let before = sandbox.watch_file("w").unwrap();
    assert!(std::process::Command::new("kill").args(["-9", &pid_a.to_string()]).status().unwrap().success());
    blocked.abort();
    drop(client_a);

    let (client_b, _) = connect(&sandbox).await;
    let _appender = Appender::start(&input, "after-restart");
    let result = tokio::time::timeout(Duration::from_secs(5), call(client_b.peer(), "keepalive", serde_json::json!({"watch_id": "w"})))
        .await
        .expect("replacement server should resume and fire");
    let envelope = text_json(&result);
    assert_eq!(envelope["status"], "fired");
    assert!(envelope["tick"].as_u64().unwrap() > before["tick"].as_u64().unwrap(), "tick continues: {envelope}");
    assert!(envelope["elapsed_s"].as_u64().unwrap() >= 2, "elapsed time spans server A's life: {envelope}");
    assert_eq!(envelope["session_ticks"], 1);
    let after = sandbox.watch_file("w").unwrap();
    assert_eq!(after["created_at"], before["created_at"]);
    assert_eq!(after["deadline_at"], before["deadline_at"]);
    assert_eq!(after["created_at"], created["created_at"]);
    client_b.cancel().await.unwrap();
}

/// Records every progress notification and forwards it on an async channel,
/// so a test can AWAIT the next one (with a timeout) instead of polling.
#[derive(Clone)]
struct ProgressRecorder {
    seen: Arc<Mutex<Vec<(f64, Option<String>)>>>,
    tx: tokio::sync::mpsc::UnboundedSender<(f64, Option<String>)>,
}

impl ProgressRecorder {
    fn new() -> (ProgressRecorder, tokio::sync::mpsc::UnboundedReceiver<(f64, Option<String>)>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (ProgressRecorder { seen: Arc::new(Mutex::new(Vec::new())), tx }, rx)
    }
}

impl ClientHandler for ProgressRecorder {
    async fn on_progress(&self, params: ProgressNotificationParam, _context: NotificationContext<RoleClient>) {
        self.seen.lock().unwrap().push((params.progress, params.message.clone()));
        let _ = self.tx.send((params.progress, params.message));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocked_keepalive_emits_a_strictly_increasing_counter_with_the_watch_tick_in_the_message() {
    // why: SPEC "Delivery" path 2 (best effort) + V18 (S18): while blocked,
    //      emit MCP notifications/progress. `progress` is a call-local counter
    //      that only increases (per-watch tick counts could go 10, 1, 11, 2),
    //      and the watch id and its tick count ride in `message`. The watch
    //      has no change at first, so the call spans several ticks; after the
    //      first notification arrives ONE line is appended and the call fires.
    //      The rmcp client attaches a progressToken itself (FINDINGS).
    //      T3: the round-2 test blocked the test's own (single-thread) runtime
    //      in `wait_until` right after spawning the request, so the task that
    //      sends the request and the handler that receives the notification
    //      could not run and a correct server failed. Now the runtime is
    //      multi-thread AND every wait is an `await` on an async channel with
    //      a timeout; nothing in this test blocks a worker thread.
    // proves: CLAIM(progress-counter)
    let sandbox = Sandbox::new();
    let input = sandbox.input("a.log");
    std::fs::write(&input, "boot\n").unwrap();
    let (recorder, mut notifications) = ProgressRecorder::new();
    let transport = TokioChildProcess::new(sandbox.tokio_command().configure(|c| {
        c.arg("mcp");
    }))
    .unwrap();
    let client = recorder.clone().serve(transport).await.unwrap();
    call(client.peer(), "watch_create", watch_args("progwatch", &input)).await;
    let blocked = {
        let peer = client.peer().clone();
        tokio::spawn(async move {
            peer.call_tool(CallToolRequestParams::new("keepalive").with_arguments(args(serde_json::json!({"watch_id": "progwatch"}))))
                .await
                .unwrap()
        })
    };
    let first = tokio::time::timeout(Duration::from_secs(5), notifications.recv())
        .await
        .expect("no progress arrived for tick 1")
        .expect("notification channel closed");
    assert!(first.1.as_deref().unwrap_or_default().contains("progwatch"), "{first:?}");
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(&input).unwrap();
        writeln!(file, "x").unwrap();
    }
    let result = tokio::time::timeout(Duration::from_secs(5), blocked).await.expect("keepalive did not return after the write").unwrap();
    assert_eq!(text_json(&result)["status"], "fired");
    let second = tokio::time::timeout(Duration::from_secs(2), notifications.recv())
        .await
        .expect("fewer than two progress notifications arrived")
        .expect("notification channel closed");
    assert!(second.0 > first.0, "progress must increase: {first:?} then {second:?}");

    let seen = recorder.seen.lock().unwrap().clone();
    assert!(seen.windows(2).all(|w| w[0].0 < w[1].0), "progress must strictly increase: {seen:?}");
    for (index, (_, message)) in seen.iter().enumerate() {
        let message = message.as_deref().unwrap_or_default();
        assert!(message.contains("progwatch"), "message names the watch: {message:?}");
        assert!(message.contains(&(index + 1).to_string()), "message carries the watch tick {}: {message:?}", index + 1);
    }
    client.cancel().await.unwrap();
}

// ---- code review round 1: C2 (cancellation), C9 (signals) ----

/// Send `tools/call` and keep the request handle, so the test can send
/// `notifications/cancelled` for it later.
async fn start_call(peer: &Peer<RoleClient>, tool: &'static str, arguments: serde_json::Value) -> rmcp::service::RequestHandle<RoleClient> {
    use rmcp::model::{CallToolRequest, ClientRequest};
    use rmcp::service::PeerRequestOptions;
    let request = ClientRequest::CallToolRequest(CallToolRequest::new(CallToolRequestParams::new(tool).with_arguments(args(arguments))));
    peer.send_cancellable_request(request, PeerRequestOptions::no_options()).await.unwrap()
}

/// The pid a test command wrote, or 0 while the file is absent or partial.
fn pid_in(pid_file: &std::path::Path) -> u32 {
    std::fs::read_to_string(pid_file).ok().and_then(|text| text.trim().parse().ok()).unwrap_or(0)
}

/// A cmd watch input whose reads wait until `gate` exists, writing its pid
/// first. With `gate` present it just prints `ok`.
fn gated_cmd_args(id: &str, gate: &std::path::Path, pid_file: &std::path::Path) -> serde_json::Value {
    let script = format!("echo $$ > '{}'; while [ ! -e '{}' ]; do sleep 0.05; done; echo ok", pid_file.display(), gate.display());
    serde_json::json!({"id": id, "inputs": [{"kind": "cmd", "argv": ["sh", "-c", script], "timeout_s": 19}],
        "interval_s": 1, "deadline_s": 3600, "fire": "change"})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_keepalive_stops_sensing_and_does_not_consume_a_finished_result() {
    // why: code review C2: rmcp signals `RequestContext.ct` on
    //      notifications/cancelled but never drops the handler, so the
    //      cancelled call kept sensing and could mark a terminal result
    //      returned (V7) while rmcp discarded the response, hiding it from
    //      later no-id calls. Setup: watch `s` has a command that hangs on a
    //      gate. A no-id keepalive starts s's tick; watch `f` is then
    //      cancelled (finished, unreturned) while the call is in flight; the
    //      client then cancels the request. The server must stop: s's command group dies, s's file is
    //      untouched (tick 0). After the gate opens, a NEW no-id keepalive
    //      must still return f's terminal result (cancelled), because the
    //      cancelled call did not mark it returned.
    // proves: CLAIM(cancel-stops-work)
    let sandbox = Sandbox::new();
    let (input, gate, pid_file) = (sandbox.input("f.log"), sandbox.input("gate"), sandbox.input("s.pid"));
    std::fs::write(&input, "boot\n").unwrap();
    std::fs::write(&gate, "").unwrap();
    let (client, _) = connect(&sandbox).await;
    let peer = client.peer();
    assert_ne!(call(peer, "watch_create", watch_args("f", &input)).await.is_error, Some(true));
    assert_ne!(call(peer, "watch_create", gated_cmd_args("s", &gate, &pid_file)).await.is_error, Some(true));
    std::fs::remove_file(&gate).unwrap();
    std::fs::remove_file(&pid_file).unwrap();

    let handle = start_call(peer, "keepalive", serde_json::json!({})).await;
    assert!(wait_until_async(Duration::from_secs(5), || pid_in(&pid_file) != 0).await, "s's tick never started");
    let cmd_pid = pid_in(&pid_file);
    // f finishes while the call is in flight (review round 3: a watch already
    // finished when a call starts is returned at once, before any tick).
    assert_eq!(text_json(&call(peer, "watch_cancel", serde_json::json!({"watch_id": "f"})).await)["status"], "cancelled");
    handle.cancel(Some("test".into())).await.unwrap();
    assert!(
        wait_until_async(Duration::from_secs(3), || proc_is_dead(cmd_pid)).await,
        "the cancelled call kept sensing: s's command is still running"
    );
    assert_eq!(sandbox.watch_file("s").unwrap()["tick"], 0, "the cancelled call wrote nothing");

    std::fs::write(&gate, "").unwrap();
    let next = tokio::time::timeout(Duration::from_secs(5), call(peer, "keepalive", serde_json::json!({})))
        .await
        .expect("a fresh no-id keepalive should return f's result");
    let envelope = text_json(&next);
    assert_eq!(envelope["watch_id"], "f", "{envelope}");
    assert_eq!(envelope["status"], "cancelled", "{envelope}");
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_watch_create_stops_reading_its_baseline() {
    // why: code review C2 for `watch_create`: the baseline read of a hanging
    //      command must stop when the client cancels the request, not run on
    //      for the command's whole timeout.
    // proves: CLAIM(cancel-stops-work)
    let sandbox = Sandbox::new();
    let (gate, pid_file) = (sandbox.input("gate"), sandbox.input("s.pid"));
    let (client, _) = connect(&sandbox).await;
    let handle = start_call(client.peer(), "watch_create", gated_cmd_args("s", &gate, &pid_file)).await;
    assert!(wait_until_async(Duration::from_secs(5), || pid_in(&pid_file) != 0).await, "the baseline read never started");
    let cmd_pid = pid_in(&pid_file);
    handle.cancel(None).await.unwrap();
    assert!(wait_until_async(Duration::from_secs(3), || proc_is_dead(cmd_pid)).await, "the cancelled create kept reading");
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_to_the_mcp_server_exits_it_and_kills_the_commands_it_was_running() {
    // why: code review C9 for `vigil mcp`: a terminated server must cancel
    //      active work so command process groups (their own sessions) do not
    //      outlive it.
    // proves: CLAIM(signal-cleanup)
    let sandbox = Sandbox::new();
    let (go, pid_file) = (sandbox.input("go"), sandbox.input("cmd.pid"));
    let created = sandbox.run(&["create", &hanging_cmd_watch_json("w", &go, &pid_file)]);
    assert!(created.status.success());
    std::fs::write(&go, "").unwrap();
    let (client, server_pid) = connect(&sandbox).await;
    let _blocked = start_call(client.peer(), "keepalive", serde_json::json!({"watch_id": "w"})).await;
    assert!(wait_until_async(Duration::from_secs(5), || pid_in(&pid_file) != 0).await, "the tick never started");
    let cmd_pid = pid_in(&pid_file);
    send_signal("-TERM", server_pid);
    assert!(wait_until_async(Duration::from_secs(2), || proc_is_dead(server_pid)).await, "the server did not exit");
    assert!(wait_until_async(Duration::from_secs(2), || proc_is_dead(cmd_pid)).await, "the command outlived the server");
}

// ---- surviving a full disk (specs/disk-full.md DF1-DF5) ------------------
//
// The server process is started with `VIGIL_TEST_FAULT` (src/fault.rs), so
// its store writes fail while the test's other processes (the CLI used to
// create a watch) see a healthy disk.

async fn connect_faulted(sandbox: &Sandbox, plan: &str) -> Client {
    let transport = TokioChildProcess::new(sandbox.tokio_faulted_command(plan).configure(|c| {
        c.arg("mcp");
    }))
    .unwrap();
    ().serve(transport).await.expect("initialize handshake")
}

fn append_line(path: &std::path::Path, line: &str) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{line}").unwrap();
}

async fn keepalive_within(client: &Client, arguments: serde_json::Value) -> CallToolResult {
    tokio::time::timeout(Duration::from_secs(10), call(client.peer(), "keepalive", arguments)).await.expect("keepalive should return, not wait out max_s")
}

#[tokio::test]
async fn keepalive_over_mcp_on_a_full_disk_returns_the_envelope_with_persist_not_a_tool_error() {
    // why: DF1/DF2 through real stdio MCP: the incident was a tool error
    //      ("io error: No space left on device") with no envelope. Now the
    //      call is NOT isError, carries the fired envelope (event "w:1:0",
    //      summary "boom") and the persist note, and the watch file on disk is
    //      byte-identical (DF6). DF3a: the envelope carries an event that was
    //      not saved, so `redelivery: true` is present.
    // proves: CLAIM(DF1), CLAIM(DF2), CLAIM(DF3a), CLAIM(DF6)
    let sandbox = Sandbox::new();
    let input = sandbox.input("w.log");
    std::fs::write(&input, "boot\n").unwrap();
    assert!(sandbox.run(&["create", &change_watch_json("w", &input, 1)]).status.success());
    append_line(&input, "boom");
    let before = sandbox.watch_bytes("w");
    let client = connect_faulted(&sandbox, DISK_STAYS_FULL).await;

    let result = keepalive_within(&client, serde_json::json!({"watch_id": "w"})).await;

    assert_ne!(result.is_error, Some(true), "a failed save is not a tool error: {result:?}");
    let envelope = text_json(&result);
    assert_eq!(envelope["status"], "fired");
    assert_eq!(envelope["next"], "act on events");
    assert_eq!(envelope["events"][0]["event_id"], "w:1:0");
    assert_eq!(envelope["events"][0]["summary"], "boom");
    assert_failed_persist(&envelope, "disk_full", "No space left on device", &sandbox.watch_path("w"), true, Some(true));
    assert_eq!(sandbox.watch_bytes("w"), before);
    assert!(sandbox.tmp_files().is_empty());
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn the_same_mcp_server_delivers_an_unsaved_result_again_to_a_no_id_keepalive() {
    // why: DF3 "in the MCP server": the agent's normal call has NO watch_id.
    //      The disk is full only while a sentinel file exists, so one server
    //      creates a watch (healthy), fails a tick (sentinel present), then
    //      retries (sentinel gone). Call 2 must return the same event again,
    //      with no persist field, not "no watches to cover". Control: call 3,
    //      after a persisted delivery, IS "no watches to cover" (V7 once per
    //      process still holds).
    // proves: CLAIM(DF3)
    let sandbox = Sandbox::new();
    let input = sandbox.input("w.log");
    std::fs::write(&input, "boot\n").unwrap();
    let sentinel = sandbox.dir.path().join("disk-is-full");
    let plan = format!("watch:28:while={0},reserve:28:while={0}", sentinel.display());
    let client = connect_faulted(&sandbox, &plan).await;
    let created = call(client.peer(), "watch_create", watch_args("w", &input)).await;
    assert_ne!(created.is_error, Some(true), "create on a healthy disk: {created:?}");
    append_line(&input, "boom");

    std::fs::write(&sentinel, "").unwrap();
    let lost = text_json(&keepalive_within(&client, serde_json::json!({})).await);
    assert_eq!(lost["status"], "fired");
    assert_eq!(lost["persist"]["ok"], false, "precondition: the save failed: {lost}");

    std::fs::remove_file(&sentinel).unwrap();
    let again = keepalive_within(&client, serde_json::json!({})).await;
    assert_ne!(again.is_error, Some(true), "the unsaved result must still be covered: {again:?}");
    let again = text_json(&again);
    // S1: the same event, now flagged as a redelivery by the server that knows
    // its save failed; the first delivery (`lost`) carries no flag.
    assert!(lost["events"][0].get("redelivered").is_none(), "{lost}");
    assert_eq!(again["events"][0]["redelivered"], true, "{again}");
    let mut redelivered_event = again["events"][0].clone();
    redelivered_event.as_object_mut().unwrap().remove("redelivered");
    assert_eq!(serde_json::json!([redelivered_event]), lost["events"], "the same event apart from the flag");
    assert!(again.get("persist").is_none(), "{again}");
    assert_eq!(sandbox.watch_file("w").unwrap()["status"], "fired");

    let nothing = keepalive_within(&client, serde_json::json!({})).await;
    assert_eq!(nothing.is_error, Some(true), "control: delivered and saved, nothing left to cover");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn watch_create_over_mcp_on_a_full_disk_is_an_error_that_names_the_root_and_says_disk_full() {
    // why: DF5 through MCP: the agent must be able to tell "the disk is full"
    //      from a generic failure, and find which directory to free. DF5b:
    //      the message contains the state root path and the lowercase words
    //      `disk full`.
    // proves: CLAIM(DF5), CLAIM(DF5b)
    let sandbox = Sandbox::new();
    let input = sandbox.input("w.log");
    std::fs::write(&input, "boot\n").unwrap();
    let client = connect_faulted(&sandbox, DISK_STAYS_FULL).await;

    let created = call(client.peer(), "watch_create", watch_args("w", &input)).await;

    assert_eq!(created.is_error, Some(true), "{created:?}");
    let text = created.content[0].as_text().expect("text content").text.clone();
    assert!(text.contains(sandbox.state_root().to_str().unwrap()), "should name the state root: {text}");
    assert!(text.contains("disk full"), "should contain the lowercase words `disk full`: {text}");
    assert!(sandbox.watch_file("w").is_none());
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn keepalive_over_mcp_lists_a_secondary_watchs_failed_save_in_persist_failures() {
    // why: S5 / DF2a through the real server: the agent calls keepalive with
    //      no id, which covers all of its watches. Two quiet watches, "a"
    //      (deadline 3600 s, the primary: nearest deadline) and "b" (7200 s).
    //      The disk is full for b.json only while a sentinel exists. Before S5
    //      the envelope (about "a", no `persist`) hid b's failed save. Now it
    //      carries `persist_failures: [{watch_id, kind, error, path}]` with
    //      exactly b, and the call returns at once (DF1a) instead of waiting
    //      out max_s (the 30 s floor). The healthy control (neither key on
    //      the wire) is in the engine test: a healthy quiet call lasts max_s.
    // proves: CLAIM(S5), CLAIM(DF2a), CLAIM(DF1a)
    let sandbox = Sandbox::new();
    let (a_input, b_input) = (sandbox.input("a.log"), sandbox.input("b.log"));
    std::fs::write(&a_input, "boot\n").unwrap();
    std::fs::write(&b_input, "boot\n").unwrap();
    let sentinel = sandbox.dir.path().join("disk-is-full");
    let plan = format!("watch:28:while={0}:b.json,reserve:28:while={0}", sentinel.display());
    let client = connect_faulted(&sandbox, &plan).await;
    for (id, input, deadline_s) in [("a", &a_input, 3600), ("b", &b_input, 7200)] {
        let mut arguments = watch_args(id, input);
        arguments["deadline_s"] = serde_json::json!(deadline_s);
        let created = call(client.peer(), "watch_create", arguments).await;
        assert_ne!(created.is_error, Some(true), "create {id} on a healthy disk: {created:?}");
    }
    std::fs::write(&sentinel, "").unwrap();
    let result = keepalive_within(&client, serde_json::json!({})).await;

    assert_ne!(result.is_error, Some(true), "a failed save is not a tool error: {result:?}");
    let envelope = text_json(&result);
    assert_eq!((envelope["watch_id"].as_str(), envelope["status"].as_str()), (Some("a"), Some("pending")), "{envelope}");
    assert!(envelope.get("persist").is_none(), "a's own save worked: {envelope}");
    let failures = envelope["persist_failures"].as_array().unwrap_or_else(|| panic!("no persist_failures: {envelope}"));
    assert_eq!(failures.len(), 1, "{envelope}");
    let mut keys: Vec<&str> = failures[0].as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, ["error", "kind", "path", "watch_id"], "{envelope}");
    assert_eq!(failures[0]["watch_id"], "b");
    assert_eq!(failures[0]["kind"], "disk_full");
    assert_eq!(failures[0]["path"], sandbox.watch_path("b").to_str().unwrap());
    assert!(failures[0]["error"].as_str().unwrap().contains("No space left on device"), "{envelope}");
    assert!(sandbox.tmp_files().is_empty());
    client.cancel().await.unwrap();
}


// ---- auto-interval (specs/auto-interval.md) --------------------------------
//
// As in cli_e2e.rs: the sandbox HOME holds the transcript, `VIGIL_UW_COMMAND`
// is a fake `uw`, and every watch fires on its first tick so no call waits out
// its window; what is checked is the window vigil chose and reported.

async fn connect_with_env(sandbox: &Sandbox, env: &[(&str, &std::ffi::OsStr)]) -> Client {
    let transport = TokioChildProcess::new(sandbox.tokio_command().configure(|c| {
        c.arg("mcp");
        for (name, value) in env {
            c.env(name, value);
        }
    }))
    .unwrap();
    ().serve(transport).await.expect("initialize handshake")
}

fn transcript_sandbox() -> (Sandbox, std::path::PathBuf) {
    use vigil::testing::{assistant_line, write_script, write_transcript};
    let sandbox = Sandbox::new();
    write_transcript(&sandbox.home().join(".claude/projects"), "p", "sess-1", &[assistant_line(0, 500), assistant_line(900, 0)]);
    let fake_uw = sandbox.dir.path().join("fake-uw");
    write_script(&fake_uw, "echo '{\"usage\": []}'");
    (sandbox, fake_uw)
}

/// Review S3: the first orchestrator call on a state root owes its due credit
/// check even when its watch fires at once, so its `cache` also holds
/// `credit_check` (the fake `uw` reports no credits). Returns `cache` without
/// that key, after asserting it is there and says "not on credits".
fn cache_without_credit_check(envelope: &serde_json::Value) -> serde_json::Value {
    let mut cache = envelope["cache"].clone();
    let check = cache.as_object_mut().expect("cache object").remove("credit_check").expect("S3: an orchestrator call that fires at once still makes its due credit check");
    assert_eq!((&check["on_credits"], &check["source"]), (&serde_json::json!(false), &serde_json::json!("uw")), "{check}");
    cache
}

async fn fired_keepalive(client: &Client, sandbox: &Sandbox, id: &str, extra_create: serde_json::Value, keepalive: serde_json::Value) -> serde_json::Value {
    let input = sandbox.input(&format!("{id}.log"));
    std::fs::write(&input, "boot\n").unwrap();
    let mut create = watch_args(id, &input);
    for (key, value) in extra_create.as_object().unwrap() {
        create[key] = value.clone();
    }
    let created = call(client.peer(), "watch_create", create).await;
    assert_ne!(created.is_error, Some(true), "{created:?}");
    append_line(&input, "go");
    let result = keepalive_within(client, keepalive).await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    text_json(&result)
}

#[tokio::test]
async fn the_mcp_schemas_declare_the_new_fields() {
    // why: AI1' / AI5: an agent only passes what the schema shows it.
    //      keepalive gains `orchestrator` (boolean); watch_create gains
    //      `expect_s`, `stall_s` (integers) and `orchestrator` (boolean).
    // proves: CLAIM(AI1'), CLAIM(AI5)
    let sandbox = Sandbox::new();
    let (client, _) = connect(&sandbox).await;
    let tools = client.list_all_tools().await.unwrap();
    let schema = |name: &str| serde_json::Value::Object((*tools.iter().find(|t| t.name == name).unwrap().input_schema).clone());
    let keepalive = schema("keepalive");
    assert!(keepalive["properties"]["orchestrator"].to_string().contains("boolean"), "{keepalive}");
    let create = schema("watch_create");
    for field in ["expect_s", "stall_s"] {
        assert!(create["properties"][field].to_string().contains("integer"), "{field}: {create}");
    }
    assert!(create["properties"]["orchestrator"].to_string().contains("boolean"), "{create}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn an_mcp_orchestrator_call_gets_the_one_hour_window_and_a_plain_call_does_not() {
    // why: AI1' + AI3 over real stdio MCP, server started with
    //      CLAUDE_CODE_SESSION_ID=sess-1. `orchestrator: true` -> 3000 s,
    //      1h / claude-transcript; the same server, same transcript, no flag
    //      (a second watch) -> 270 s, unknown / default. Also the clock block
    //      is present on the wire.
    // proves: CLAIM(AI1'), CLAIM(AI3), CLAIM(AI4)
    let (sandbox, fake_uw) = transcript_sandbox();
    let client = connect_with_env(&sandbox, &[("CLAUDE_CODE_SESSION_ID", "sess-1".as_ref()), ("VIGIL_UW_COMMAND", fake_uw.as_os_str())]).await;
    let long = fired_keepalive(&client, &sandbox, "a", serde_json::json!({}), serde_json::json!({"watch_id": "a", "orchestrator": true})).await;
    assert_eq!(long["max_s"], 3000);
    assert_eq!(cache_without_credit_check(&long), serde_json::json!({"ttl": "1h", "source": "claude-transcript", "interval_s": 3000}));
    assert!(long["clock"]["summary"].as_str().is_some_and(|line| !line.is_empty() && !line.contains('\n')), "{long}");
    let plain = fired_keepalive(&client, &sandbox, "b", serde_json::json!({}), serde_json::json!({"watch_id": "b"})).await;
    assert_eq!(plain["max_s"], 270);
    assert_eq!(plain["cache"], serde_json::json!({"ttl": "unknown", "source": "default", "interval_s": 270}));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn a_stored_orchestrator_flag_and_an_explicit_max_s_over_mcp() {
    // why: AI1': `orchestrator: true` stored by watch_create is honoured by a
    //      bare keepalive (3000 s); an explicit max_s wins over it (max_s 45
    //      -> 45 s, source explicit).
    // proves: CLAIM(AI1'), CLAIM(AI3)
    let (sandbox, fake_uw) = transcript_sandbox();
    let client = connect_with_env(&sandbox, &[("CLAUDE_CODE_SESSION_ID", "sess-1".as_ref()), ("VIGIL_UW_COMMAND", fake_uw.as_os_str())]).await;
    let stored = fired_keepalive(&client, &sandbox, "a", serde_json::json!({"orchestrator": true, "expect_s": 600}), serde_json::json!({"watch_id": "a"})).await;
    assert_eq!(stored["cache"]["interval_s"], 3000);
    let explicit = fired_keepalive(&client, &sandbox, "b", serde_json::json!({"orchestrator": true}), serde_json::json!({"watch_id": "b", "max_s": 45})).await;
    assert_eq!(explicit["cache"], serde_json::json!({"ttl": "unknown", "source": "explicit", "interval_s": 45}));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn an_mcp_server_started_by_codex_never_exceeds_270_s() {
    // why: AI1' / A2: AGENT=codex -> 5m / codex-default, 270 s, and even an
    //      explicit max_s of 9999 is clamped to 270 (Codex MCP tool timeout is
    //      300 s). A Claude session id in the same environment would win (A3),
    //      so it is absent here.
    // proves: CLAIM(AI1'), CLAIM(AI3)
    let (sandbox, fake_uw) = transcript_sandbox();
    let client = connect_with_env(&sandbox, &[("AGENT", "codex".as_ref()), ("VIGIL_UW_COMMAND", fake_uw.as_os_str())]).await;
    let derived = fired_keepalive(&client, &sandbox, "a", serde_json::json!({}), serde_json::json!({"watch_id": "a", "orchestrator": true})).await;
    assert_eq!(cache_without_credit_check(&derived), serde_json::json!({"ttl": "5m", "source": "codex-default", "interval_s": 270}));
    let clamped = fired_keepalive(&client, &sandbox, "b", serde_json::json!({}), serde_json::json!({"watch_id": "b", "orchestrator": true, "max_s": 9999})).await;
    assert_eq!(clamped["max_s"], 270);
    client.cancel().await.unwrap();
}

// ---- holds (SPEC "Holds", H1-H12) ------------------------------------------

#[tokio::test]
async fn hold_is_a_listed_tool_with_for_until_idle_and_deadline_in_its_schema() {
    // why: H11: the `hold` tool takes the same specs as `vigil hold`. `for` is
    //      an array of strings (clients that send a string fail, see
    //      CLAIM(create-schema-objects)), plus until_idle, deadline_s,
    //      expect_s, orchestrator and id.
    // proves: CLAIM(H11)
    let sandbox = Sandbox::new();
    let (client, _) = connect(&sandbox).await;
    let tools = client.list_all_tools().await.unwrap();
    let hold = tools.iter().find(|t| t.name == "hold").expect("the hold tool is listed");
    assert!(hold.description.as_deref().is_some_and(|d| d.contains("hold")));
    let schema = serde_json::Value::Object((*hold.input_schema).clone());
    let properties = &schema["properties"];
    for field in ["id", "for", "until_idle", "deadline_s", "expect_s", "orchestrator"] {
        assert!(properties.get(field).is_some(), "{field} missing: {schema}");
    }
    assert_eq!(properties["for"]["items"]["type"], "string", "{schema}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn the_hold_tool_holds_on_qex_work_and_a_keepalive_releases_it_with_the_reason() {
    // why: H1, H6, H11 over the wire. The server's PATH has the fake qex. One
    //      job already completed -> `keepalive` on the hold fires at once with
    //      "hold released: ...". A bare hold (no arguments) is accepted and
    //      pending. A bad spec and an unknown job are tool errors (isError)
    //      naming the problem, and write no watch.
    // proves: CLAIM(H1), CLAIM(H6), CLAIM(H11), CLAIM(H2), CLAIM(H3)
    let sandbox = Sandbox::new();
    let qex = FakeQex::new(&sandbox);
    qex.set_job(J1, "completed");
    let client = connect_with_env(&sandbox, &[("PATH", std::ffi::OsStr::new(&qex.path()))]).await;
    let peer = client.peer();

    let created = call(peer, "hold", serde_json::json!({"id": "h", "for": [format!("qex:{J1}")]})).await;
    assert_ne!(created.is_error, Some(true), "{created:?}");
    let created = text_json(&created);
    assert_eq!((created["watch_id"].as_str(), created["status"].as_str()), (Some("h"), Some("pending")));
    let released = text_json(&call(peer, "keepalive", serde_json::json!({"watch_id": "h"})).await);
    assert_eq!((released["status"].as_str(), released["next"].as_str()), (Some("fired"), Some("act on events")), "{released}");
    // W42-D6/D9: the item's own event, then the release that counts and names the items.
    let summaries: Vec<&str> = released["events"].as_array().unwrap().iter().map(|event| event["summary"].as_str().unwrap()).collect();
    assert_eq!(summaries, [format!("item ended: qex:{J1}: completed"), format!("hold released: 1 item ended (qex:{J1})")], "{released}");

    let bare = call(peer, "hold", serde_json::json!({"id": "bare"})).await;
    assert_ne!(bare.is_error, Some(true), "{bare:?}");
    let status = text_json(&call(peer, "watch_status", serde_json::json!({"watch_id": "bare"})).await);
    assert_eq!((status["status"].as_str(), status["next"].as_str()), (Some("pending"), Some("call keepalive again")), "{status}");
    assert_eq!(sandbox.watch_file("bare").unwrap()["orchestrator"], true);

    let bad = call(peer, "hold", serde_json::json!({"id": "bad", "for": ["nonsense"]})).await;
    assert_eq!(bad.is_error, Some(true));
    assert!(bad.content[0].as_text().unwrap().text.contains("nonsense"));
    let typo = call(peer, "hold", serde_json::json!({"id": "typo", "for": ["qex:typo"]})).await;
    assert_eq!(typo.is_error, Some(true));
    assert!(typo.content[0].as_text().unwrap().text.contains("not found"));
    assert!(sandbox.watch_file("bad").is_none() && sandbox.watch_file("typo").is_none());
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn the_hold_add_tool_is_listed_with_its_arguments() {
    // why: W42-H2. `hold_add {watch_id, for, expect_s, label}`: one item per
    //      call, so `for` is one string (not an array like `hold`'s). The
    //      `hold` tool gains `label` for its first item (W42-H1).
    // proves: W42-H2, W42-H1
    // catches: a tool missing from the server; `for` typed as an array (a
    //      client sending one spec string would fail); `label` missing.
    let sandbox = Sandbox::new();
    let (client, _) = connect(&sandbox).await;
    let tools = client.list_all_tools().await.unwrap();
    let add = tools.iter().find(|t| t.name == "hold_add").expect("the hold_add tool is listed");
    assert!(add.description.as_deref().is_some_and(|d| d.contains("hold")));
    let schema = serde_json::Value::Object((*add.input_schema).clone());
    for field in ["watch_id", "for", "expect_s", "label"] {
        assert!(schema["properties"].get(field).is_some(), "{field} missing: {schema}");
    }
    assert_eq!(schema["properties"]["for"]["type"], "string", "{schema}");
    let hold = tools.iter().find(|t| t.name == "hold").unwrap();
    assert!(hold.input_schema.get("properties").and_then(|p| p.get("label")).is_some(), "hold takes a label for its first item");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn hold_add_over_the_wire_adds_an_item_that_the_keepalive_reports_and_an_ended_hold_refuses() {
    // why: W42-H2, W42-H3 over MCP. Hold on a running qex job (label first),
    //      add a marker-file item (label marker); the job ends -> keepalive
    //      returns `item ended: first: completed` with the hold pending; the
    //      marker appears -> the hold is released; a further hold_add is a
    //      tool error that says the hold ended.
    // proves: W42-H2, W42-H3
    // catches: a hold_add tool that is not wired to the engine (error or
    //      no-op); an item that is not persisted; adding to an ended hold
    //      reported as success.
    let sandbox = Sandbox::new();
    let qex = FakeQex::new(&sandbox);
    qex.set_job(J1, "running");
    let client = connect_with_env(&sandbox, &[("PATH", std::ffi::OsStr::new(&qex.path()))]).await;
    let peer = client.peer();
    let marker = sandbox.input("done.marker");

    let created = call(peer, "hold", serde_json::json!({"id": "h", "for": [format!("qex:{J1}")], "label": "first"})).await;
    assert_ne!(created.is_error, Some(true), "{created:?}");
    let added = call(peer, "hold_add", serde_json::json!({"watch_id": "h", "for": format!("file:{}", marker.display()), "label": "marker", "expect_s": 600})).await;
    assert_ne!(added.is_error, Some(true), "{added:?}");

    qex.set_job(J1, "completed");
    let first = text_json(&call(peer, "keepalive", serde_json::json!({"watch_id": "h", "max_s": 30})).await);
    assert_eq!((first["status"].as_str(), first["next"].as_str()), (Some("pending"), Some("act on events")), "{first}");
    assert_eq!(first["events"][0]["summary"], "item ended: first: completed", "{first}");

    std::fs::write(&marker, "done\n").unwrap();
    let released = text_json(&call(peer, "keepalive", serde_json::json!({"watch_id": "h", "max_s": 45})).await);
    assert_eq!(released["status"], "fired", "{released}");

    let late = call(peer, "hold_add", serde_json::json!({"watch_id": "h", "for": "pid:1", "label": "late"})).await;
    assert_eq!(late.is_error, Some(true), "{late:?}");
    assert!(late.content[0].as_text().unwrap().text.contains("ended"));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn the_hold_done_tool_is_listed_with_watch_id_and_label() {
    // why: W42-D7. `hold_done {watch_id, label}` ends a `none` item.
    // proves: W42-D7
    // catches: a tool whose schema lacks `label` (it could not name the item)
    //      or `watch_id`.
    let sandbox = Sandbox::new();
    let (client, _) = connect(&sandbox).await;
    let tools = client.list_all_tools().await.unwrap();
    let done = tools.iter().find(|t| t.name == "hold_done").expect("the hold_done tool is listed");
    assert!(done.description.as_deref().is_some_and(|d| d.contains("hold")));
    let schema = serde_json::Value::Object((*done.input_schema).clone());
    for field in ["watch_id", "label"] {
        assert!(schema["properties"].get(field).is_some(), "{field} missing: {schema}");
    }
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn an_initial_none_item_is_ended_by_the_hold_done_tool_and_an_unknown_label_is_a_tool_error() {
    // why: W42-D7 + W42-D8. A pure-MCP agent holds on a T3 delegate with
    //      `hold {for: ["none"], label: "x"}` (initial `none` item through
    //      the MCP front door), then ends it with `hold_done`. Success: the
    //      keepalive then fires with `item ended: x: done` AND the release
    //      (the last item, D6). An unknown label is a tool error (isError)
    //      naming the label, and the hold stays live.
    // proves: W42-D7, W42-D8, W42-D6
    // catches: a hold_done tool that is not wired (error or no-op); an unknown
    //      label reported as success; `for: ["none"]` refused at the MCP
    //      front door although the CLI allows it.
    let sandbox = Sandbox::new();
    let (client, _) = connect(&sandbox).await;
    let peer = client.peer();
    let created = call(peer, "hold", serde_json::json!({"id": "h", "for": ["none"], "label": "x"})).await;
    assert_ne!(created.is_error, Some(true), "{created:?}");

    let wrong = call(peer, "hold_done", serde_json::json!({"watch_id": "h", "label": "nope"})).await;
    assert_eq!(wrong.is_error, Some(true), "{wrong:?}");
    assert!(wrong.content[0].as_text().unwrap().text.contains("nope"));
    let status = text_json(&call(peer, "watch_status", serde_json::json!({"watch_id": "h"})).await);
    assert_eq!(status["status"], "pending", "a refused done leaves the hold live: {status}");

    let done = call(peer, "hold_done", serde_json::json!({"watch_id": "h", "label": "x"})).await;
    assert_ne!(done.is_error, Some(true), "{done:?}");
    let released = text_json(&call(peer, "keepalive", serde_json::json!({"watch_id": "h", "max_s": 30})).await);
    assert_eq!(released["status"], "fired", "{released}");
    let summaries: Vec<&str> = released["events"].as_array().unwrap().iter().map(|event| event["summary"].as_str().unwrap()).collect();
    assert!(summaries.contains(&"item ended: x: done"), "{released}");
    assert!(summaries.iter().any(|summary| summary.starts_with("hold released:")), "{released}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn expect_s_given_through_the_mcp_tools_raises_overdue_for_the_initial_and_the_added_item() {
    // why: W42-D8. `expect_s` on `hold` (initial item) and on `hold_add`
    //      (added item) must each produce an overdue event naming its label.
    //      expect_s 2 -> overdue at min(3, 2 + 540) = 3 s; real time (a
    //      process boundary has no fake clock), so the test sleeps 4 s once
    //      and the first keepalive tick sees both.
    // proves: W42-D8, W42-H3
    // catches: an MCP `expect_s` accepted by the schema but dropped on the
    //      way to the engine (no overdue, silently).
    let sandbox = Sandbox::new();
    let qex = FakeQex::new(&sandbox);
    qex.set_job(J1, "running");
    let client = connect_with_env(&sandbox, &[("PATH", std::ffi::OsStr::new(&qex.path()))]).await;
    let peer = client.peer();
    let created = call(peer, "hold", serde_json::json!({"id": "h", "for": [format!("qex:{J1}")], "label": "build", "expect_s": 2})).await;
    assert_ne!(created.is_error, Some(true), "{created:?}");
    let added = call(peer, "hold_add", serde_json::json!({"watch_id": "h", "for": "none", "label": "t3", "expect_s": 2})).await;
    assert_ne!(added.is_error, Some(true), "{added:?}");
    tokio::time::sleep(Duration::from_secs(4)).await;
    let envelope = text_json(&call(peer, "keepalive", serde_json::json!({"watch_id": "h", "max_s": 30})).await);
    let mut summaries: Vec<&str> = envelope["events"].as_array().unwrap().iter().map(|event| event["summary"].as_str().unwrap()).collect();
    summaries.sort();
    assert_eq!(summaries, vec!["item far overdue: build", "item far overdue: t3", "item overdue: build", "item overdue: t3"], "{envelope}");
    assert_eq!(envelope["status"], "pending", "{envelope}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn hold_add_over_the_wire_resolves_qex_names_refuses_ambiguous_ones_and_duplicate_labels() {
    // why: W42-D8 + W42-D5 through MCP. `hold_add {for: "qex:nightly"}` stores
    //      the full job id (like `hold`); an ambiguous name is a tool error
    //      naming the cause; a duplicate label is a tool error naming the
    //      label; the stored watch is unchanged by either refusal.
    // proves: W42-D8, W42-D5
    // catches: hold_add that stores the name instead of the id; an ambiguous
    //      name accepted; a duplicate label silently shadowing the first item.
    let sandbox = Sandbox::new();
    let qex = FakeQex::new(&sandbox);
    qex.set_job(J1, "running");
    qex.set_name("nightly", J1);
    qex.set_ambiguous("dupe");
    let client = connect_with_env(&sandbox, &[("PATH", std::ffi::OsStr::new(&qex.path()))]).await;
    let peer = client.peer();
    let created = call(peer, "hold", serde_json::json!({"id": "h", "for": ["none"], "label": "first"})).await;
    assert_ne!(created.is_error, Some(true), "{created:?}");
    let added = call(peer, "hold_add", serde_json::json!({"watch_id": "h", "for": "qex:nightly", "label": "n"})).await;
    assert_ne!(added.is_error, Some(true), "{added:?}");
    let stored = sandbox.watch_file("h").unwrap();
    assert!(stored.to_string().contains(J1), "the full id is stored: {stored}");

    let ambiguous = call(peer, "hold_add", serde_json::json!({"watch_id": "h", "for": "qex:dupe", "label": "d"})).await;
    assert_eq!(ambiguous.is_error, Some(true), "{ambiguous:?}");
    let text = ambiguous.content[0].as_text().unwrap().text.clone();
    assert!(text.contains("dupe") && text.contains("ambiguous"), "{text}");
    let duplicate = call(peer, "hold_add", serde_json::json!({"watch_id": "h", "for": "pid:1", "label": "first"})).await;
    assert_eq!(duplicate.is_error, Some(true), "{duplicate:?}");
    assert!(duplicate.content[0].as_text().unwrap().text.contains("first"));
    assert_eq!(sandbox.watch_file("h").unwrap(), stored, "refusals write nothing");
    client.cancel().await.unwrap();
}

const COLD_NEXT: &str = "Going cold: post a short summary of this session (task, progress, in-flight ids, tools, the user's standing rules), then end your turn. Nothing will wake you.";

#[tokio::test]
async fn cold_without_ids_cancels_only_the_watches_this_server_created() {
    // why: W45-C2: `cold` with no ids ends this session's own watches, as
    //      keepalive without an id covers them; another session's watch
    //      (here made through the CLI, a different process) stays pending
    //      and is listed under `pending`, so the agent can see what it left.
    // proves: W45-C2
    // catches: cold cancelling every pending watch in the state root; cold
    //      cancelling nothing; a reply without the instruction in `next`.
    let sandbox = Sandbox::new();
    let mine = sandbox.input("mine.log");
    let theirs = sandbox.input("theirs.log");
    std::fs::write(&mine, "boot\n").unwrap();
    std::fs::write(&theirs, "boot\n").unwrap();
    assert!(sandbox.command().args(["create", &change_watch_json("theirs", &theirs, 1)]).output().unwrap().status.success());
    let (client, _) = connect(&sandbox).await;
    assert!(!call(client.peer(), "watch_create", watch_args("mine", &mine)).await.is_error.unwrap_or(false));

    let cold = call(client.peer(), "cold", serde_json::json!({})).await;
    assert!(!cold.is_error.unwrap_or(false), "{cold:?}");
    let cold = text_json(&cold);
    assert_eq!(cold["cancelled"], serde_json::json!(["mine"]));
    assert_eq!(cold["pending"], serde_json::json!(["theirs"]));
    assert_eq!(cold["next"], COLD_NEXT);
    assert_eq!(sandbox.watch_file("mine").unwrap()["status"], "cancelled");
    assert_eq!(sandbox.watch_file("theirs").unwrap()["status"], "pending");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn cold_with_ids_cancels_exactly_those_and_an_unknown_id_is_an_error() {
    // why: W45-C2: named ids are cancelled even when this server did not
    //      create them; an id that does not exist is a tool error, not a
    //      silent success that leaves the agent believing it went cold.
    // proves: W45-C2
    // catches: cold ignoring `watch_ids`; an unknown id reported as cancelled.
    let sandbox = Sandbox::new();
    let theirs = sandbox.input("theirs.log");
    std::fs::write(&theirs, "boot\n").unwrap();
    assert!(sandbox.command().args(["create", &change_watch_json("theirs", &theirs, 1)]).output().unwrap().status.success());
    let (client, _) = connect(&sandbox).await;

    let ghost = call(client.peer(), "cold", serde_json::json!({"watch_ids": ["ghost"]})).await;
    assert_eq!(ghost.is_error, Some(true), "{ghost:?}");
    let cold = text_json(&call(client.peer(), "cold", serde_json::json!({"watch_ids": ["theirs"]})).await);
    assert_eq!(cold["cancelled"], serde_json::json!(["theirs"]));
    assert_eq!(cold["pending"], serde_json::json!([]));
    assert_eq!(sandbox.watch_file("theirs").unwrap()["status"], "cancelled");
    client.cancel().await.unwrap();
}


#[tokio::test]
async fn w46_mcp_cap_classes_are_declared_stored_and_refused_as_tool_errors() {
    // why: W46-K1/K3/K4 must work through real MCP, with the same admission error and visible classes.
    // catches: wiring --cap only in CLI, dropping cap from schemas, or returning successful refusals.
    let sandbox = Sandbox::new();
    let (client, _) = connect(&sandbox).await;
    let tools = client.list_all_tools().await.unwrap();
    for name in ["hold", "hold_add"] {
        let tool = tools.iter().find(|tool| tool.name == name).unwrap();
        let schema = serde_json::Value::Object((*tool.input_schema).clone());
        assert!(schema["properties"].get("cap").is_some(), "{schema}");
        assert!(schema.to_string().contains("work") && schema.to_string().contains("review"), "{schema}");
    }
    let peer = client.peer();
    let created = call(peer, "hold", serde_json::json!({"id": "h", "for": ["none"], "label": "first", "cap": "work"})).await;
    assert_ne!(created.is_error, Some(true), "{created:?}");
    assert_eq!(sandbox.watch_file("h").unwrap()["items"][0]["cap"], "work");
    let before = sandbox.watch_bytes("h");
    let refused = call(peer, "hold_add", serde_json::json!({"watch_id": "h", "for": "none", "label": "second", "cap": "work"})).await;
    assert_eq!(refused.is_error, Some(true), "{refused:?}");
    assert_eq!(refused.content[0].as_text().unwrap().text, "agent cap reached: 1 working Claude runs (cap 1): first. Wait for one to end, or the owner raises it: vigil cap set --work 2 --until <time>");
    assert_eq!(sandbox.watch_bytes("h"), before);
    let review = call(peer, "hold_add", serde_json::json!({"watch_id": "h", "for": "none", "label": "reviewer", "cap": "review"})).await;
    assert_ne!(review.is_error, Some(true), "{review:?}");
    let status = text_json(&call(peer, "watch_status", serde_json::json!({"watch_id": "h"})).await);
    assert_eq!(status["items"][1]["cap"], "review");
    assert_eq!(status["cap"], serde_json::json!({"work": 1, "work_limit": 1, "review": 1, "review_limit": 1}));
    let too_many = call(peer, "hold", serde_json::json!({"id": "too-many", "for": ["pid:1", "until:2099-01-01T00:00:00Z"], "cap": "work"})).await;
    assert_eq!(too_many.is_error, Some(true), "{too_many:?}");
    assert!(sandbox.watch_file("too-many").is_none());
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn w46_mcp_cold_empty_ids_cancels_nothing_but_absent_ids_cancel_mine() {
    // why: W46-C5 distinguishes an explicit empty list from an absent watch_ids argument.
    // catches: mapping Some([]) to None and accidentally cancelling this server's live holds.
    let sandbox = Sandbox::new();
    let (client, _) = connect(&sandbox).await;
    assert_ne!(call(client.peer(), "hold", serde_json::json!({"id": "mine"})).await.is_error, Some(true));
    let empty = text_json(&call(client.peer(), "cold", serde_json::json!({"watch_ids": []})).await);
    assert_eq!(empty["cancelled"], serde_json::json!([]));
    assert_eq!(empty["pending"], serde_json::json!(["mine"]));
    assert_eq!(empty["next"], COLD_NEXT);
    assert_eq!(sandbox.watch_file("mine").unwrap()["status"], "pending");
    let absent = text_json(&call(client.peer(), "cold", serde_json::json!({})).await);
    assert_eq!(absent["cancelled"], serde_json::json!(["mine"]));
    assert_eq!(absent["pending"], serde_json::json!([]));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn w46_mcp_cold_mixed_ids_cancels_valid_and_reports_unknown_as_tool_error() {
    // why: W46-C6 preserves unknown-id tool errors while making progress on valid ids.
    // catches: preflight loading every id before cancelling, or silently discarding unknown ids.
    let sandbox = Sandbox::new();
    let (client, _) = connect(&sandbox).await;
    assert_ne!(call(client.peer(), "hold", serde_json::json!({"id": "valid"})).await.is_error, Some(true));
    let result = call(client.peer(), "cold", serde_json::json!({"watch_ids": ["ghost", "valid"]})).await;
    assert_eq!(result.is_error, Some(true), "{result:?}");
    assert!(result.content[0].as_text().unwrap().text.contains("ghost"));
    assert_eq!(sandbox.watch_file("valid").unwrap()["status"], "cancelled");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn w46_mcp_cold_reports_unsaved_cancellation_in_warnings() {
    // why: W46-C7 exposes cancel persistence failure in the MCP result's warnings list.
    // catches: dropping the failed-save envelope or emitting warnings only on CLI stderr.
    let sandbox = Sandbox::new();
    assert!(sandbox.run(&["hold", "--id", "unsaved"]).status.success());
    let before = sandbox.watch_bytes("unsaved");
    let client = connect_faulted(&sandbox, "watch:5:all").await;
    let result = call(client.peer(), "cold", serde_json::json!({"watch_ids": ["unsaved"]})).await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let result = text_json(&result);
    let warnings = result["warnings"].as_array().expect("warnings array");
    assert_eq!(warnings.len(), 1);
    let warning = warnings[0].as_str().unwrap();
    assert!(warning.contains("unsaved") && warning.contains("Input/output error"), "{warning}");
    assert_eq!(result["pending"], serde_json::json!(["unsaved"]));
    assert_eq!(sandbox.watch_bytes("unsaved"), before);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn w46_mcp_cold_cancels_a_hold_with_live_items() {
    // why: W46-C8 applies cold to a populated hold without requiring its work to end.
    // catches: refusing live-item holds or returning next: watch ended instead of the cold instruction.
    let sandbox = Sandbox::new();
    let (client, _) = connect(&sandbox).await;
    assert_ne!(call(client.peer(), "hold", serde_json::json!({"id": "h", "for": ["none"], "label": "live"})).await.is_error, Some(true));
    let result = text_json(&call(client.peer(), "cold", serde_json::json!({})).await);
    assert_eq!(result["cancelled"], serde_json::json!(["h"]));
    assert_eq!(result["next"], COLD_NEXT);
    assert_eq!(sandbox.watch_file("h").unwrap()["status"], "cancelled");
    assert!(sandbox.watch_file("h").unwrap()["items"][0].get("ended_at").is_none());
    client.cancel().await.unwrap();
}


#[tokio::test]
async fn fix1_mcp_keepalive_accepts_confirm_and_registers_mcp_transport() {
    // why: the daily token must cross the actual MCP boundary, with safe pid attribution.
    // catches: dropping confirm in the tool adapter or recording the MCP server as CLI.
    let sandbox = Sandbox::new();
    let client = connect_with_env(&sandbox, &[("VIGIL_ORCHESTRATOR_CONFIRM", std::ffi::OsStr::new("on"))]).await;
    let peer = client.peer();
    assert_ne!(call(peer, "hold", serde_json::json!({"id": "confirmed", "for": ["none"], "label": "work"})).await.is_error, Some(true));
    let challenge = call(peer, "keepalive", serde_json::json!({"watch_id": "confirmed", "orchestrator": true, "max_s": 30})).await;
    assert_eq!(challenge.is_error, Some(true), "{challenge:?}");
    let text = &challenge.content[0].as_text().unwrap().text;
    let start = text.find("confirm \"").expect("challenge token") + "confirm \"".len();
    let token = &text[start..start + 8];
    let wait = call(peer, "keepalive", serde_json::json!({"watch_id": "confirmed", "orchestrator": true, "confirm": token, "max_s": 30}));
    let finish = async {
        let record = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(entries) = std::fs::read_dir(sandbox.state_root().join("calls")) {
                    for entry in entries.flatten() {
                        if entry.path().extension().is_some_and(|ext| ext == "json") {
                            if let Ok(bytes) = std::fs::read(entry.path()) {
                                if let Ok(record) = serde_json::from_slice::<serde_json::Value>(&bytes) { return record; }
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("confirmed keepalive registers before blocking");
        assert_eq!(record["transport"], "mcp");
        assert_eq!(record["orchestrator"], true);
        let marker = sandbox.state_root().join("orchestrator/watches/confirmed.json");
        assert!(marker.is_file(), "the token confirms this live watch");
        assert_ne!(call(peer, "watch_cancel", serde_json::json!({"watch_id": "confirmed"})).await.is_error, Some(true));
    };
    let (result, ()) = tokio::join!(wait, finish);
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert_eq!(text_json(&result)["status"], "cancelled");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn mcp_session_selection_and_brief_output_use_the_same_engine() {
    // why: both new switches must survive the MCP schema and front door.
    // catches: schema accepts flags but drops them before keepalive.
    let sandbox = Sandbox::new();
    let mut command = sandbox.tokio_command();
    command.arg("mcp").env("CODEX_SESSION_ID", "mcp-quiet-session");
    let client = ().serve(TokioChildProcess::new(command).unwrap()).await.unwrap();
    let created = call(client.peer(), "hold", serde_json::json!({"id": "h", "for": ["until:2020-01-01T00:00:00Z"], "orchestrator": false})).await;
    assert_ne!(created.is_error, Some(true));
    let result = call(client.peer(), "keepalive", serde_json::json!({"session": true, "brief": true, "orchestrator": false})).await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let text = &result.content[0].as_text().unwrap().text;
    assert!(text.starts_with("act: ") && text.contains("item ended:") && text.contains("advice: off"), "{text}");
    assert_eq!(text.lines().count(), 2);
    client.cancel().await.unwrap();
}
