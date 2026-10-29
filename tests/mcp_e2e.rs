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
    // why: SPEC "Packaging": server `instructions` (sent at initialize) must
    //      be under 512 characters and say: create a watch, then call
    //      keepalive repeatedly until `next` says "act on events", so the
    //      agent needs no skill.
    // proves: CLAIM(instructions)
    let sandbox = Sandbox::new();
    let (client, _) = connect(&sandbox).await;
    let instructions = client.peer_info().and_then(|info| info.instructions.clone()).expect("instructions present");
    assert!(instructions.chars().count() < 512, "{} chars", instructions.chars().count());
    for needle in ["keepalive", "watch_create", "act on events"] {
        assert!(instructions.contains(needle), "missing {needle:?} in {instructions:?}");
    }
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
async fn create_then_a_write_then_keepalive_returns_the_fired_envelope() {
    // why: SPEC acceptance: the agent creates a watch, then calls only
    //      keepalive and receives the failure line. Drives create ->
    //      (file written by an appender) -> keepalive over real MCP and checks
    //      the envelope: status fired, the line in events and digest, real
    //      timestamps, ticks counting, session_ticks 1, next "act on events".
    //      Must complete well inside 5 s.
    // proves: CLAIM(early-return-on-fire)
    let sandbox = Sandbox::new();
    let input = sandbox.input("build.log");
    std::fs::write(&input, "boot\n").unwrap();
    let (client, _) = connect(&sandbox).await;

    let created = call(client.peer(), "watch_create", watch_args("build", &input)).await;
    assert_ne!(created.is_error, Some(true), "{created:?}");
    let created = text_json(&created);
    assert_eq!(created["watch_id"], "build");
    assert_eq!(created["status"], "pending");

    let _appender = Appender::start(&input, "ERROR: linking failed");
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(5), call(client.peer(), "keepalive", serde_json::json!({"watch_id": "build"})))
        .await
        .expect("keepalive should return on fire, not wait out max_s");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_ne!(result.is_error, Some(true), "a fire is a normal result");
    let envelope = text_json(&result);
    assert_eq!(envelope["status"], "fired");
    assert_eq!(envelope["next"], "act on events");
    assert_eq!(envelope["session_ticks"], 1);
    assert!(envelope["tick"].as_u64().unwrap() >= 2, "baseline tick + a change tick");
    assert!(envelope["now"].as_str().unwrap().ends_with('Z'));
    assert!(envelope["events"][0]["summary"].as_str().unwrap().contains("ERROR: linking failed"));
    assert!(envelope["digest"].as_str().unwrap().contains("ERROR: linking failed"));
    assert_eq!(envelope["events"][0]["watch_id"], "build");
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
        wait_until(Duration::from_secs(5), || sandbox.watch_file("w").is_some_and(|w| w["tick"].as_u64() >= Some(3))),
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

#[derive(Clone, Default)]
struct ProgressRecorder(Arc<Mutex<Vec<f64>>>);

impl ClientHandler for ProgressRecorder {
    async fn on_progress(&self, params: ProgressNotificationParam, _context: NotificationContext<RoleClient>) {
        self.0.lock().unwrap().push(params.progress);
    }
}

#[tokio::test]
async fn a_blocked_keepalive_emits_increasing_progress_notifications() {
    // why: SPEC "Delivery" path 2 (best effort): while blocked, emit MCP
    //      notifications/progress with the tick count. The rmcp client attaches
    //      a progressToken itself (FINDINGS). Tick 1 happens at once and the
    //      fire needs tick >= 2, so at least [1, 2] must arrive, strictly
    //      increasing.
    // proves: CLAIM(progress-ticks)
    let sandbox = Sandbox::new();
    let input = sandbox.input("a.log");
    std::fs::write(&input, "boot\n").unwrap();
    let recorder = ProgressRecorder::default();
    let transport = TokioChildProcess::new(sandbox.tokio_command().configure(|c| {
        c.arg("mcp");
    }))
    .unwrap();
    let client = recorder.clone().serve(transport).await.unwrap();
    call(client.peer(), "watch_create", watch_args("p", &input)).await;
    let _appender = Appender::start(&input, "x");
    let result = tokio::time::timeout(Duration::from_secs(5), call(client.peer(), "keepalive", serde_json::json!({"watch_id": "p"}))).await.unwrap();
    assert_eq!(text_json(&result)["status"], "fired");
    assert!(wait_until(Duration::from_secs(2), || recorder.0.lock().unwrap().len() >= 2), "no progress notifications arrived");
    let seen = recorder.0.lock().unwrap().clone();
    assert_eq!(seen[0], 1.0);
    assert!(seen.windows(2).all(|w| w[0] < w[1]), "progress must strictly increase: {seen:?}");
    client.cancel().await.unwrap();
}
