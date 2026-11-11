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
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn exactly_the_five_spec_tools_are_listed_with_descriptions() {
    // why: SPEC "MCP tools": watch_create, keepalive, watch_status,
    //      watch_list, watch_cancel - no more, no fewer. keepalive's schema
    //      exposes watch_id and max_s.
    // proves: CLAIM(tool-list)
    let sandbox = Sandbox::new();
    let (client, _) = connect(&sandbox).await;
    let tools = client.list_all_tools().await.unwrap();
    let mut names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(names, vec!["keepalive", "watch_cancel", "watch_create", "watch_list", "watch_status"]);
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
    //      byte-identical (DF6).
    // proves: CLAIM(DF1), CLAIM(DF2), CLAIM(DF6)
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
    assert_failed_persist(&envelope, "disk_full", "No space left on device", &sandbox.watch_path("w"));
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
    assert_eq!(again["events"], lost["events"]);
    assert!(again.get("persist").is_none(), "{again}");
    assert_eq!(sandbox.watch_file("w").unwrap()["status"], "fired");

    let nothing = keepalive_within(&client, serde_json::json!({})).await;
    assert_eq!(nothing.is_error, Some(true), "control: delivered and saved, nothing left to cover");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn watch_create_over_mcp_on_a_full_disk_is_an_error_that_names_the_root_and_says_disk_full() {
    // why: DF5 through MCP: the agent must be able to tell "the disk is full"
    //      from a generic failure, and find which directory to free.
    // proves: CLAIM(DF5)
    let sandbox = Sandbox::new();
    let input = sandbox.input("w.log");
    std::fs::write(&input, "boot\n").unwrap();
    let client = connect_faulted(&sandbox, DISK_STAYS_FULL).await;

    let created = call(client.peer(), "watch_create", watch_args("w", &input)).await;

    assert_eq!(created.is_error, Some(true), "{created:?}");
    let text = created.content[0].as_text().expect("text content").text.clone();
    assert!(text.contains(sandbox.state_root().to_str().unwrap()), "should name the state root: {text}");
    assert!(text.to_lowercase().contains("disk full"), "should say the disk is full: {text}");
    assert!(sandbox.watch_file("w").is_none());
    client.cancel().await.unwrap();
}
