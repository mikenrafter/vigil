//! Transport parity for notices. Real binary, real processes, no network.
//! Reason: SPEC "Notices": CLI, hook command, keepalive envelope and MCP tools
//! all draw on one per-session delivery record.
mod common;
use common::*;
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResult},
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use serde_json::{Value, json};
use std::io::Write;
use std::process::Stdio;

fn run(sandbox: &Sandbox, args: &[&str]) -> Value {
    let output = sandbox.run(args);
    assert!(output.status.success(), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
    stdout_json(&output)
}

fn text(sandbox: &Sandbox, args: &[&str]) -> String {
    let output = sandbox.run(args);
    assert!(output.status.success(), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

fn with_stdin(sandbox: &Sandbox, args: &[&str], stdin: &str) -> String {
    let mut child = sandbox.command().args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

fn add(sandbox: &Sandbox, id: &str, message: &str) {
    run(sandbox, &["notice", "add", id, "--message", message]);
}

#[test]
fn cli_lifecycle_add_list_rm() {
    // why: the documented CLI: argv shapes, JSON output, refusal exit codes.
    // catches: a flag that is parsed but dropped (expires, scope).
    let sandbox = Sandbox::new();
    let scope = sandbox.cwd();
    run(&sandbox, &["notice", "add", "neon", "--message", "Gemini Neon reviews now", "--expires", "2099-01-01T00:00:00Z", "--scope", scope.to_str().unwrap()]);
    let list = run(&sandbox, &["notice", "list"]);
    assert_eq!(list[0]["id"], "neon");
    assert_eq!(list[0]["message"], "Gemini Neon reviews now");
    assert_eq!(list[0]["expires_at"], "2099-01-01T00:00:00Z");
    assert_eq!(list[0]["scope"], json!([scope]));
    assert_eq!(list[0]["expired"], false);
    assert!(!sandbox.run(&["notice", "add", "neon", "--message", "dup"]).status.success());
    assert!(!sandbox.run(&["notice", "add", "bad", "--message", "x", "--expires", "2000-01-01T00:00:00Z"]).status.success());
    assert!(!sandbox.run(&["notice", "add", "bad", "--message", "x", "--expires", "tomorrow"]).status.success());
    assert!(!sandbox.run(&["notice", "rm", "missing"]).status.success());
    assert_eq!(run(&sandbox, &["notice", "rm", "neon"])["removed"], "neon");
    assert_eq!(run(&sandbox, &["notice", "list"]), json!([]));
}

#[test]
fn notices_command_prints_pending_once_and_records_it() {
    // why: SPEC "Notices": `vigil notices --session` prints pending notices and
    //      records them as delivered. Text form is `id: message`; nothing when none.
    // proves: CLAIM(notice-once)
    // catches: a command that prints but does not record; one that prints a header on an empty result.
    let sandbox = Sandbox::new();
    assert_eq!(text(&sandbox, &["notices", "--session", "s1"]), "");
    add(&sandbox, "neon", "Gemini Neon is a reviewer");
    assert_eq!(text(&sandbox, &["notices", "--session", "s1"]), "neon: Gemini Neon is a reviewer\n");
    assert_eq!(text(&sandbox, &["notices", "--session", "s1"]), "");
    assert_eq!(text(&sandbox, &["notices", "--session", "s2"]), "neon: Gemini Neon is a reviewer\n");
}

#[test]
fn notices_json_shape() {
    // why: hooks parse this. Fixed keys; empty list still prints an object.
    let sandbox = Sandbox::new();
    add(&sandbox, "neon", "m");
    let first = run(&sandbox, &["notices", "--session", "s1", "--json"]);
    assert_eq!(first["identity"], "session");
    assert_eq!(first["session"], "s1");
    assert_eq!(first["notices"], json!([{"id": "neon", "message": "m"}]));
    assert!(first.get("note").is_none());
    let second = run(&sandbox, &["notices", "--session", "s1", "--json"]);
    assert_eq!(second["notices"], json!([]));
}

#[test]
fn expired_and_out_of_scope_notices_are_not_printed_and_scope_follows_cwd() {
    // why: expired never shown; --cwd selects the scope, and an out-of-scope
    //      call does not consume the notice.
    // catches: --cwd ignored; scope checked against the process cwd only.
    let sandbox = Sandbox::new();
    let scope = sandbox.cwd();
    run(&sandbox, &["notice", "add", "scoped", "--message", "m", "--scope", scope.to_str().unwrap()]);
    let outside = sandbox.cwd2();
    assert_eq!(text(&sandbox, &["notices", "--session", "s", "--cwd", outside.to_str().unwrap()]), "");
    assert_eq!(text(&sandbox, &["notices", "--session", "s", "--cwd", scope.to_str().unwrap()]), "scoped: m\n");
    // Expiry is exercised on a notice that expires in about two seconds.
    let soon = (chrono::Utc::now() + chrono::Duration::seconds(2)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    run(&sandbox, &["notice", "add", "soon", "--message", "m", "--expires", &soon]);
    std::thread::sleep(std::time::Duration::from_millis(3100));
    assert_eq!(text(&sandbox, &["notices", "--session", "fresh"]), "");
}

#[test]
fn from_hook_reads_each_harness_payload_and_they_share_nothing_across_ids() {
    // why: the session id lives under a different name in each harness's hook
    //      payload (Claude/Codex session_id, agy conversationId, others
    //      thread_id/threadId). cwd comes from the payload too.
    // catches: one field name hard-coded; payload cwd ignored; --session not winning.
    let sandbox = Sandbox::new();
    add(&sandbox, "neon", "m");
    let cwd = sandbox.cwd();
    let claude = json!({"session_id": "claude-1", "cwd": cwd, "hook_event_name": "SessionStart"}).to_string();
    let agy = json!({"conversationId": "agy-1", "cwd": cwd}).to_string();
    let codex = json!({"session_id": "codex-1", "turn_id": "t", "cwd": cwd}).to_string();
    let thread = json!({"threadId": "thread-1", "cwd": cwd}).to_string();
    for (payload, label) in [(&claude, "claude-1"), (&agy, "agy-1"), (&codex, "codex-1"), (&thread, "thread-1")] {
        assert_eq!(with_stdin(&sandbox, &["notices", "--from-hook"], payload), "neon: m\n", "{label}");
        assert_eq!(with_stdin(&sandbox, &["notices", "--from-hook"], payload), "", "{label} repeated");
    }
    // The same id arriving by --session is the same session.
    assert_eq!(text(&sandbox, &["notices", "--session", "claude-1"]), "");
    // --session beats the payload.
    assert_eq!(with_stdin(&sandbox, &["notices", "--from-hook", "--session", "explicit"], &claude), "neon: m\n");
    // A payload with no id and no environment identity is a run-keyed delivery, and says so.
    add(&sandbox, "second", "2");
    let anonymous = with_stdin(&sandbox, &["notices", "--from-hook"], &json!({"cwd": cwd}).to_string());
    assert!(anonymous.contains("second: 2"), "{anonymous}");
    assert!(anonymous.contains("no agent session identity"), "{anonymous}");
    // Garbage on stdin is an error, not a silent anonymous delivery.
    let mut child = sandbox.command().args(["notices", "--from-hook"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"not json").unwrap();
    assert!(!child.wait_with_output().unwrap().status.success());
}

#[test]
fn the_environment_identity_is_the_default_session() {
    // why: with CLAUDE_CODE_SESSION_ID set, no --session is needed, and the same
    //      id passed explicitly is the same record.
    // catches: env identity ignored by the notices command.
    let sandbox = Sandbox::new();
    add(&sandbox, "neon", "m");
    let mut command = sandbox.command();
    command.env("CLAUDE_CODE_SESSION_ID", "env-session").args(["notices"]);
    assert_eq!(String::from_utf8(command.output().unwrap().stdout).unwrap(), "neon: m\n");
    assert_eq!(text(&sandbox, &["notices", "--session", "env-session"]), "");
}

#[test]
fn no_identity_is_once_per_calling_process_and_labelled() {
    // why: invariant. The sandbox env is cleared, so there is no identity; the
    //      test process is the common parent of both calls.
    // catches: delivering on every call; delivering with no label.
    let sandbox = Sandbox::new();
    add(&sandbox, "neon", "m");
    let first = text(&sandbox, &["notices"]);
    assert!(first.contains("neon: m"), "{first}");
    assert!(first.contains("no agent session identity"), "{first}");
    assert_eq!(text(&sandbox, &["notices"]), "");
    let json = run(&sandbox, &["notices", "--json"]);
    assert_eq!(json["identity"], "process_run");
    assert_eq!(json["notices"], json!([]));
}

#[test]
fn keepalive_and_the_notices_command_share_one_record() {
    // why: SPEC "Notices": the keepalive envelope and `vigil notices` deliver the
    //      same notice once per session, in either order.
    // proves: CLAIM(notice-once)
    // catches: a keepalive path with its own delivered set.
    let sandbox = Sandbox::new();
    add(&sandbox, "neon", "m");
    let input = sandbox.input("trigger");
    std::fs::write(&input, "old").unwrap();
    for id in ["w1", "w2"] {
        run(&sandbox, &["create", &json!({"id": id, "inputs": [{"kind": "file", "path": input}], "fire": "change"}).to_string()]);
    }
    std::fs::write(&input, "new").unwrap();
    let keepalive = |id: &str| {
        let mut command = sandbox.command();
        command.env("CLAUDE_CODE_SESSION_ID", "agent-9").args(["keepalive", id]);
        stdout_json(&command.output().unwrap())
    };
    let first = keepalive("w1");
    assert_eq!(first["notices"], json!([{"id": "neon", "message": "m"}]));
    assert!(keepalive("w2").get("notices").is_none());
    assert_eq!(text(&sandbox, &["notices", "--session", "agent-9"]), "");
    // And the other order: a hook first, then keepalive.
    add(&sandbox, "later", "l");
    assert_eq!(text(&sandbox, &["notices", "--session", "agent-9"]), "later: l\n");
    run(&sandbox, &["create", &json!({"id": "w3", "inputs": [{"kind": "file", "path": input}], "fire": "change"}).to_string()]);
    std::fs::write(&input, "newer").unwrap();
    assert!(keepalive("w3").get("notices").is_none());
}

#[test]
fn a_failed_write_to_stdout_leaves_the_notice_pending() {
    // why: the claim is recorded before printing so racing callers cannot both
    //      print it, and released when printing fails so it is not lost.
    // proves: CLAIM(notice-once)
    // catches: a command that records and then fails to print (stdout closed).
    let sandbox = Sandbox::new();
    add(&sandbox, "neon", "m");
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("exec {} --root \"$ROOT\" notices --session s >&-", common::vigil_bin()))
        .env("ROOT", sandbox.state_root())
        .env("HOME", sandbox.home())
        .env("VIGIL_ADVISORIES_FILE", "off")
        .current_dir(sandbox.cwd())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success(), "writing to a closed stdout must fail");
    assert_eq!(text(&sandbox, &["notices", "--session", "s"]), "neon: m\n", "the notice was lost");
}

#[test]
fn racing_notices_processes_of_one_session_print_each_notice_exactly_once() {
    // why: a hook and a keepalive can run at the same moment. Real processes.
    // catches: no lock around claim; both print.
    let sandbox = Sandbox::new();
    for n in 0..5 {
        add(&sandbox, &format!("n{n}"), "m");
    }
    let children: Vec<_> = (0..6)
        .map(|_| sandbox.command().args(["notices", "--session", "race"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap())
        .collect();
    let mut printed: Vec<String> = children
        .into_iter()
        .flat_map(|c| {
            let out = c.wait_with_output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8(out.stdout).unwrap().lines().map(str::to_string).collect::<Vec<_>>()
        })
        .collect();
    printed.sort();
    assert_eq!(printed, (0..5).map(|n| format!("n{n}: m")).collect::<Vec<_>>());
}

// ---- MCP ----------------------------------------------------------------

async fn connect(sandbox: &Sandbox, session: &str) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let transport = TokioChildProcess::new(sandbox.command().configure(|command| {
        command.arg("mcp").env("CLAUDE_CODE_SESSION_ID", session);
    }))
    .unwrap();
    ().serve(transport).await.unwrap()
}

async fn call(client: &rmcp::service::RunningService<rmcp::RoleClient, ()>, name: &'static str, args: Value) -> CallToolResult {
    client.call_tool(CallToolRequestParams::new(name).with_arguments(args.as_object().cloned().unwrap_or_default())).await.unwrap()
}

fn body(result: &CallToolResult) -> Value {
    let text = result.content.iter().find_map(|c| c.as_text().map(|t| t.text.clone())).expect("text content");
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

#[tokio::test]
async fn mcp_tools_mirror_the_cli_and_share_its_record() {
    // why: MCP and CLI go through one operation layer (parity, as advisory_e2e).
    //      notice_add / notice_list / notice_rm / notices exist and agree with the CLI.
    // catches: an MCP `notices` with its own delivered set; a missing tool.
    let sandbox = Sandbox::new();
    let client = connect(&sandbox, "mcp-agent").await;
    let added = call(&client, "notice_add", json!({"id": "neon", "message": "m", "expires_at": "2099-01-01T00:00:00Z"})).await;
    assert_ne!(added.is_error, Some(true), "{added:?}");
    assert_eq!(run(&sandbox, &["notice", "list"])[0]["id"], "neon");
    assert_eq!(body(&call(&client, "notice_list", json!({})).await)[0]["id"], "neon");
    let first = body(&call(&client, "notices", json!({})).await);
    assert_eq!(first["notices"], json!([{"id": "neon", "message": "m", "expires_at": "2099-01-01T00:00:00Z"}]));
    assert_eq!(first["session"], "mcp-agent");
    assert_eq!(body(&call(&client, "notices", json!({})).await)["notices"], json!([]));
    // The CLI sees the same session's record.
    assert_eq!(text(&sandbox, &["notices", "--session", "mcp-agent"]), "");
    // An explicit session argument is a different session.
    assert_eq!(body(&call(&client, "notices", json!({"session": "other"})).await)["notices"].as_array().unwrap().len(), 1);
    let refused = call(&client, "notice_add", json!({"id": "neon", "message": "dup"})).await;
    assert_eq!(refused.is_error, Some(true));
    let removed = call(&client, "notice_rm", json!({"id": "neon"})).await;
    assert_ne!(removed.is_error, Some(true));
    assert_eq!(run(&sandbox, &["notice", "list"]), json!([]));
    client.cancel().await.unwrap();
}
