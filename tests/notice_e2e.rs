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
    // A fresh session in the scope directory gets `scoped` but never the expired `soon`.
    assert_eq!(text(&sandbox, &["notices", "--session", "fresh"]), "scoped: m\n");
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
        // /dev/full accepts the open and fails every write with ENOSPC (a closed
        // fd 1 is reopened as /dev/null by Rust's runtime, so it would not fail).
        .arg(format!("exec {} --root \"$ROOT\" notices --session s >/dev/full", common::vigil_bin()))
        .env("ROOT", sandbox.state_root())
        .env("HOME", sandbox.home())
        .env("VIGIL_ADVISORIES_FILE", "off")
        .current_dir(sandbox.cwd())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success(), "writing to a full stdout must fail");
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
    let transport = TokioChildProcess::new(sandbox.tokio_command().configure(|command| {
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

#[test]
fn codex_payload_distinguishes_threads_above_a_shared_ancestor() {
    // A shared Codex app-server is only a fallback; payloads name its threads.
    let sandbox = Sandbox::new();
    add(&sandbox, "neon", "m");
    let input = sandbox.input("trigger");
    std::fs::write(&input, "old").unwrap();
    run(&sandbox, &["create", &json!({"id":"w", "inputs":[{"kind":"file","path":input}], "fire":"change"}).to_string()]);
    std::fs::write(&input, "new").unwrap();
    let harness = sandbox.cwd().join("codex");
    std::fs::copy("/bin/sh", &harness).unwrap();
    let output = std::process::Command::new(&harness)
        .args(["-c", r#"printf '%s' '{"session_id":"payload-only"}' | "$VIGIL" --root "$ROOT" notices --from-hook
"$VIGIL" --root "$ROOT" keepalive w"#])
        .env_clear().env("PATH", "/usr/bin:/bin")
        .env("VIGIL", common::vigil_bin()).env("ROOT", sandbox.state_root())
        .env("HOME", sandbox.home()).env("VIGIL_ADVISORIES_FILE", "off")
        .current_dir(sandbox.cwd()).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let (hook, envelope) = stdout.split_once('\n').unwrap();
    assert_eq!(hook, "neon: m");
    let envelope: Value = serde_json::from_str(envelope).unwrap();
    assert_eq!(envelope["notices"], json!([{"id":"neon","message":"m"}]));
    let records = std::fs::read_dir(sandbox.state_root().join("notice-delivery")).unwrap()
        .filter_map(Result::ok).filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json")).count();
    assert_eq!(records, 2);
}

#[test]
fn a_hook_without_any_identity_refuses_without_printing() {
    let sandbox = Sandbox::new();
    add(&sandbox, "neon", "m");
    let mut child = sandbox.command().args(["notices", "--from-hook"]).stdin(Stdio::piped())
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"{}").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("session identity"));
}

#[tokio::test]
async fn codex_t3_shell_and_mcp_share_notices_with_different_session_env() {
    // CLAIM(notice-once): retain the shell's Codex key while MCP sees only T3.
    let sandbox = Sandbox::new();
    add(&sandbox, "neon", "m");
    let command = || {
        // Preserve non-session inherited environment; pin the session names for this shape.
        let mut command = std::process::Command::new(common::vigil_bin());
        command.args(["--root", sandbox.state_root().to_str().unwrap()])
            .current_dir(sandbox.cwd()).env("HOME", sandbox.home()).env("VIGIL_ADVISORIES_FILE", "off")
            .env_remove("T3CODE_THREAD_ID").env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("CODEX_SESSION_ID").env_remove("CODEX_THREAD_ID")
            .env_remove("ANTIGRAVITY_CONVERSATION_ID");
        command
    };
    let mut child = command().env("CODEX_SESSION_ID", "codex-session")
        .env("T3CODE_THREAD_ID", "t3-thread").args(["notices", "--from-hook"])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(br#"{"session_id":"payload-session"}"#).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "neon: m\n");
    let mut mcp = command();
    mcp.arg("mcp").env("T3CODE_THREAD_ID", "t3-thread");
    let transport = TokioChildProcess::new(tokio::process::Command::from(mcp)).unwrap();
    let client = ().serve(transport).await.unwrap();
    let reply = body(&call(&client, "notices", json!({})).await);
    assert_eq!(reply["session"], "t3-thread");
    assert_eq!(reply["notices"], json!([]), "shell delivery must be visible to MCP");
    add(&sandbox, "later", "l");
    assert_eq!(body(&call(&client, "notices", json!({})).await)["notices"], json!([{"id":"later","message":"l"}]));
    let output = command().env("CODEX_SESSION_ID", "codex-session")
        .env("T3CODE_THREAD_ID", "t3-thread").arg("notices").output().unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty(), "MCP delivery must be visible to shell");
    // Every distinct name seen (`t3-thread`, `codex-session`, `payload-session`)
    // holds a record; a Codex-only call sees `later` bridged from `t3-thread`.
    let codex_only = command().env("CODEX_SESSION_ID", "codex-session").arg("notices").output().unwrap();
    assert!(codex_only.status.success());
    assert!(codex_only.stdout.is_empty(), "bridged shell call must link codex-session to later");
    let records = std::fs::read_dir(sandbox.state_root().join("notice-delivery")).unwrap()
        .filter_map(Result::ok).filter(|e| e.path().extension().is_some_and(|ext| ext == "json")).count();
    assert_eq!(records, 3);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn notice_help_and_mcp_descriptions_say_once_per_session() {
    // The documented crash limit must also be reflected in discoverable help.
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["--help"]);
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(!help.contains("exactly once"), "{help}");
    assert!(help.contains("once per session"), "{help}");
    let client = connect(&sandbox, "help-session").await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools.iter().find(|tool| tool.name == "notice_add").unwrap();
    let description = tool.description.as_deref().unwrap();
    assert!(!description.contains("exactly once"), "{description}");
    assert!(description.contains("once per session"), "{description}");
    client.cancel().await.unwrap();
}

#[test]
fn corrupt_record_error_explains_the_reset_and_its_replay_cost() {
    let sandbox = Sandbox::new();
    add(&sandbox, "neon", "m");
    assert_eq!(text(&sandbox, &["notices", "--session", "s"]), "neon: m\n");
    let path = std::fs::read_dir(sandbox.state_root().join("notice-delivery")).unwrap()
        .map(|e| e.unwrap().path()).find(|p| p.extension().is_some_and(|ext| ext == "json")).unwrap();
    std::fs::write(&path, b"{broken").unwrap();
    let output = sandbox.run(&["notices", "--session", "s"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.trim_end().ends_with("delete this file to reset; live notices are then shown to this session once more"), "{error}");
    assert_eq!(std::fs::read(&path).unwrap(), b"{broken");
    std::fs::remove_file(path).unwrap();
    assert_eq!(text(&sandbox, &["notices", "--session", "s"]), "neon: m\n");
    assert_eq!(text(&sandbox, &["notices", "--session", "s"]), "");
}

// ---- set-of-names identity (hardening follow-up) ------------------------

#[tokio::test]
async fn codex_t3_bridge_both_orders_links_codex_payload_and_t3_names() {
    // why: SPEC "Notices": a notice is pending only if no record under any name
    //      the call sees has it, and delivery is recorded under every name the
    //      call sees, including bridging calls that print nothing.
    // proves: CLAIM(notice-once)
    // catches: first-match identity (T3-only ignores CODEX_SESSION_ID,
    //          CODEX_THREAD_ID, payload); bridging call that skips writing
    //          missing names when one name already holds the notice.
    let sandbox = Sandbox::new();
    let base_cmd = || {
        let mut command = sandbox.command();
        command
            .env_remove("T3CODE_THREAD_ID")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("CODEX_SESSION_ID")
            .env_remove("CODEX_THREAD_ID")
            .env_remove("ANTIGRAVITY_CONVERSATION_ID");
        command
    };

    // Order 1: payload-only hook delivers n1 -> bridge sees {T3,
    // CODEX_THREAD_ID, payload} (prints nothing, links all) -> T3-only MCP and
    // Codex-only shell calls see n1 as already delivered.
    add(&sandbox, "n1", "first notice");
    let mut hook1 = base_cmd();
    hook1.args(["notices", "--from-hook"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = hook1.spawn().unwrap();
    child.stdin.take().unwrap().write_all(br#"{"session_id":"payload-1"}"#).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "n1: first notice\n");

    let mut bridge1 = base_cmd();
    bridge1
        .env("T3CODE_THREAD_ID", "t3-1")
        .env("CODEX_SESSION_ID", "cs-1")
        .env("CODEX_THREAD_ID", "ct-1")
        .args(["notices", "--from-hook"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = bridge1.spawn().unwrap();
    child.stdin.take().unwrap().write_all(br#"{"session_id":"payload-1"}"#).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "", "bridge must not repeat n1 already seen under payload-1");

    let mut mcp_cmd = base_cmd();
    mcp_cmd.arg("mcp").env("T3CODE_THREAD_ID", "t3-1");
    let transport = TokioChildProcess::new(tokio::process::Command::from(mcp_cmd)).unwrap();
    let client = ().serve(transport).await.unwrap();
    let reply = body(&call(&client, "notices", json!({})).await);
    assert_eq!(reply["session"], "t3-1");
    assert_eq!(reply["notices"], json!([]), "MCP with T3CODE_THREAD_ID only must see bridged n1");

    let out = base_cmd().env("CODEX_SESSION_ID", "cs-1").arg("notices").output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "n1: first notice\n", "excluded session id stays independent");

    let out = base_cmd().env("CODEX_THREAD_ID", "ct-1").arg("notices").output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "", "CODEX_THREAD_ID only must see bridged n1");

    // Order 2: T3-only MCP delivers n2 -> bridge sees three names (prints nothing,
    // links all) -> payload-only and CODEX_THREAD_ID-only calls
    // see n2 as delivered.
    add(&sandbox, "n2", "second notice");
    let reply = body(&call(&client, "notices", json!({})).await);
    assert_eq!(reply["notices"], json!([{"id": "n2", "message": "second notice"}]));

    let mut bridge2 = base_cmd();
    bridge2
        .env("T3CODE_THREAD_ID", "t3-1")
        .env("CODEX_SESSION_ID", "cs-1")
        .env("CODEX_THREAD_ID", "ct-1")
        .args(["notices", "--from-hook"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = bridge2.spawn().unwrap();
    child.stdin.take().unwrap().write_all(br#"{"session_id":"payload-1"}"#).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "", "bridge must not repeat n2 delivered by MCP");

    let mut payload_only = base_cmd();
    payload_only.args(["notices", "--from-hook"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = payload_only.spawn().unwrap();
    child.stdin.take().unwrap().write_all(br#"{"session_id":"payload-1"}"#).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "", "payload-only hook must see bridged n2");

    let out = base_cmd().env("CODEX_SESSION_ID", "cs-1").arg("notices").output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "n2: second notice\n", "excluded session id stays independent");

    let out = base_cmd().env("CODEX_THREAD_ID", "ct-1").arg("notices").output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "", "CODEX_THREAD_ID only must see bridged n2");

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn claude_t3_shell_hook_and_mcp_share_notices_in_both_orders() {
    // why: Claude-in-T3 can expose CLAUDE_CODE_SESSION_ID and T3CODE_THREAD_ID
    //      together on one call and either one alone on another.
    // proves: CLAIM(notice-once)
    // catches: T3-first dropping CLAUDE_CODE_SESSION_ID when both are set.
    let sandbox = Sandbox::new();
    let base_cmd = || {
        let mut command = sandbox.command();
        command
            .env_remove("T3CODE_THREAD_ID")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("CODEX_SESSION_ID")
            .env_remove("CODEX_THREAD_ID")
            .env_remove("ANTIGRAVITY_CONVERSATION_ID");
        command
    };

    // Order 1: CLAUDE_CODE_SESSION_ID-only -> bridge {T3, CLAUDE} -> T3-only MCP.
    add(&sandbox, "c1", "claude first");
    let out = base_cmd().env("CLAUDE_CODE_SESSION_ID", "claude-s1").arg("notices").output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "c1: claude first\n");

    let out = base_cmd()
        .env("T3CODE_THREAD_ID", "t3-c1")
        .env("CLAUDE_CODE_SESSION_ID", "claude-s1")
        .arg("notices")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "", "bridge must see CLAUDE_CODE_SESSION_ID delivery");

    let mut mcp_cmd = base_cmd();
    mcp_cmd.arg("mcp").env("T3CODE_THREAD_ID", "t3-c1");
    let transport = TokioChildProcess::new(tokio::process::Command::from(mcp_cmd)).unwrap();
    let client = ().serve(transport).await.unwrap();
    assert_eq!(body(&call(&client, "notices", json!({})).await)["notices"], json!([]), "T3-only MCP must see bridged c1");

    // Order 2: T3-only MCP -> bridge {T3, CLAUDE, payload=claude-s1} -> CLAUDE-only hook.
    add(&sandbox, "c2", "claude second");
    assert_eq!(
        body(&call(&client, "notices", json!({})).await)["notices"],
        json!([{"id": "c2", "message": "claude second"}])
    );

    let mut bridge = base_cmd();
    bridge
        .env("T3CODE_THREAD_ID", "t3-c1")
        .env("CLAUDE_CODE_SESSION_ID", "claude-s1")
        .args(["notices", "--from-hook"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = bridge.spawn().unwrap();
    child.stdin.take().unwrap().write_all(br#"{"session_id":"claude-s1"}"#).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "", "bridge must see T3 MCP delivery");

    let mut claude_only = base_cmd();
    claude_only
        .env("CLAUDE_CODE_SESSION_ID", "claude-s1")
        .args(["notices", "--from-hook"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = claude_only.spawn().unwrap();
    child.stdin.take().unwrap().write_all(br#"{"session_id":"claude-s1"}"#).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "", "CLAUDE-only hook must see bridged c2");

    client.cancel().await.unwrap();
}

#[test]
fn codex_outside_t3_two_threads_on_one_app_server_each_get_notices_once() {
    // why: outside T3, one codex app-server hosts multiple threads. Each thread
    //      has its own env/payload names and must NOT write the shared ancestor
    //      record, while recording under all of its own names.
    // proves: CLAIM(notice-once)
    // catches: including ancestor when env/payload exists (hides notice from
    //          thread 2); first-match ignoring payload id when CODEX_SESSION_ID
    //          is also present (repeats notice on payload-only hook).
    let sandbox = Sandbox::new();
    add(&sandbox, "neon", "m");
    let harness = sandbox.cwd().join("codex");
    std::fs::copy("/bin/sh", &harness).unwrap();
    let script = r#"
printf 'A1=%s\n' "$(printf '%s' '{"session_id":"hook-a"}' | CODEX_SESSION_ID=shared-server CODEX_THREAD_ID=codex-a "$VIGIL" --root "$ROOT" notices --from-hook)"
printf 'A2=%s\n' "$(CODEX_SESSION_ID=shared-server CODEX_THREAD_ID=codex-a "$VIGIL" --root "$ROOT" notices)"
printf 'A3=%s\n' "$(printf '%s' '{"session_id":"hook-a"}' | "$VIGIL" --root "$ROOT" notices --from-hook)"
printf 'B1=%s\n' "$(printf '%s' '{"session_id":"hook-b"}' | CODEX_SESSION_ID=shared-server CODEX_THREAD_ID=codex-b "$VIGIL" --root "$ROOT" notices --from-hook)"
printf 'B2=%s\n' "$(CODEX_SESSION_ID=shared-server CODEX_THREAD_ID=codex-b "$VIGIL" --root "$ROOT" notices)"
printf 'B3=%s\n' "$(printf '%s' '{"session_id":"hook-b"}' | "$VIGIL" --root "$ROOT" notices --from-hook)"
printf 'ANC1=%s\n' "$("$VIGIL" --root "$ROOT" notices)"
printf 'ANC2=%s\n' "$("$VIGIL" --root "$ROOT" notices)"
"#;
    let output = std::process::Command::new(&harness)
        .args(["-c", script])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("VIGIL", common::vigil_bin())
        .env("ROOT", sandbox.state_root())
        .env("HOME", sandbox.home())
        .env("VIGIL_ADVISORIES_FILE", "off")
        .current_dir(sandbox.cwd())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let lines: Vec<&str> = std::str::from_utf8(&output.stdout).unwrap().lines().collect();
    assert_eq!(
        lines,
        vec![
            "A1=neon: m",
            "A2=",
            "A3=",
            "B1=neon: m",
            "B2=",
            "B3=",
            "ANC1=neon: m",
            "ANC2=",
        ],
        "each thread gets neon once across all its names without colliding on the shared codex ancestor"
    );
}

#[test]
fn simultaneous_overlapping_name_sets_deliver_each_notice_exactly_once() {
    // why: racing processes of one session that see overlapping subsets of its
    //      names must serialize on one delivery lock and never duplicate.
    // proves: CLAIM(notice-once)
    // catches: per-record locks deadlocking or letting overlapping name sets
    //          both claim the same notice; dropping secondary names.
    let sandbox = Sandbox::new();
    for n in 0..5 {
        add(&sandbox, &format!("n{n}"), "m");
    }
    struct Shape<'a> {
        envs: &'a [(&'a str, &'a str)],
        payload: Option<&'a str>,
    }
    let shapes = [
        Shape { envs: &[("T3CODE_THREAD_ID", "shared"), ("CODEX_SESSION_ID", "codex-1")], payload: None },
        Shape { envs: &[("T3CODE_THREAD_ID", "shared"), ("CODEX_SESSION_ID", "codex-1")], payload: None },
        Shape { envs: &[("T3CODE_THREAD_ID", "shared")], payload: None },
        Shape { envs: &[("T3CODE_THREAD_ID", "shared")], payload: None },
        Shape { envs: &[("T3CODE_THREAD_ID", "shared"), ("CODEX_THREAD_ID", "thread-1")], payload: Some(r#"{"session_id":"payload-1"}"#) },
        Shape { envs: &[("T3CODE_THREAD_ID", "shared"), ("CODEX_THREAD_ID", "thread-1")], payload: Some(r#"{"session_id":"payload-1"}"#) },
    ];
    let mut children = Vec::new();
    for shape in shapes {
        let mut cmd = sandbox.command();
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        for (k, v) in shape.envs {
            cmd.env(k, v);
        }
        if let Some(body) = shape.payload {
            cmd.args(["notices", "--from-hook"]).stdin(Stdio::piped());
            let mut child = cmd.spawn().unwrap();
            child.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
            children.push(child);
        } else {
            cmd.arg("notices");
            children.push(cmd.spawn().unwrap());
        }
    }
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

    // Every single participating name now holds all five deliveries.
    for (env_k, env_v) in [("T3CODE_THREAD_ID", "shared"), ("CODEX_SESSION_ID", "codex-1"), ("CODEX_THREAD_ID", "thread-1")] {
        let out = sandbox.command().env(env_k, env_v).arg("notices").output().unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "", "{env_k}={env_v} repeated a notice");
    }
    assert_eq!(with_stdin(&sandbox, &["notices", "--from-hook"], r#"{"session_id":"payload-1"}"#), "");
}

#[test]
fn failed_stdout_write_releases_every_name_without_undoing_bridged_deliveries() {
    // why: a failed print on a multi-name call must release only the notices
    //      that call claimed across every name, keeping already-delivered
    //      bridged notices intact.
    // proves: CLAIM(notice-once)
    // catches: releasing only the primary name (leaving aliases stuck);
    //          releasing already-delivered notices bridged by the same call.
    let sandbox = Sandbox::new();
    add(&sandbox, "old", "already seen");
    let out = sandbox.command().env("T3CODE_THREAD_ID", "t3-rel").arg("notices").output().unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "old: already seen\n");

    add(&sandbox, "fresh", "must retry");
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "printf '%s' '{{\"session_id\":\"ps-rel\"}}' | exec {} --root \"$ROOT\" notices --from-hook >/dev/full",
            common::vigil_bin()
        ))
        .env("ROOT", sandbox.state_root())
        .env("HOME", sandbox.home())
        .env("VIGIL_ADVISORIES_FILE", "off")
        .env("T3CODE_THREAD_ID", "t3-rel")
        .env("CODEX_SESSION_ID", "cs-rel")
        .current_dir(sandbox.cwd())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success(), "writing to /dev/full must fail");

    // Every participating name released `fresh` AND kept `old` bridged.
    let out = sandbox.command().env("CODEX_SESSION_ID", "cs-rel").arg("notices").output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "fresh: must retry\n");
    assert_eq!(with_stdin(&sandbox, &["notices", "--from-hook"], r#"{"session_id":"ps-rel"}"#), "fresh: must retry\n");
    let out = sandbox.command().env("T3CODE_THREAD_ID", "t3-rel").arg("notices").output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "fresh: must retry\n");
}

#[test]
fn antigravity_env_links_with_hook_payload_and_t3_thread() {
    // why: Antigravity exposes ANTIGRAVITY_CONVERSATION_ID and conversationId.
    //      All present identity env vars plus payload are linked together.
    // proves: CLAIM(notice-once)
    // catches: omitting ANTIGRAVITY_CONVERSATION_ID from the notice env set.
    let sandbox = Sandbox::new();
    add(&sandbox, "neon", "m");
    let out = sandbox
        .command()
        .env("T3CODE_THREAD_ID", "t3-agy")
        .env("ANTIGRAVITY_CONVERSATION_ID", "agy-conv-1")
        .arg("notices")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "neon: m\n");
    assert_eq!(with_stdin(&sandbox, &["notices", "--from-hook"], r#"{"conversationId":"agy-conv-1"}"#), "");
    let out = sandbox.command().env("T3CODE_THREAD_ID", "t3-agy").arg("notices").output().unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "");
}

#[test]
fn legacy_single_record_and_lock_files_remain_readable_linkable_and_prunable() {
    // why: invariant: existing notice records written by vigil 17236db must stay
    //      readable without migration, bridge to new names, and prune cleanly
    //      including any legacy per-record .lock files.
    //      Independent oracle: `printf 'agent:legacy-t3' | sha256sum | cut -c1-32`
    //      == `01c62e9f52b2ee2cdc8a54665190c303`.
    // catches: changing the record hash format or JSON schema incompatibly;
    //          leaving legacy .lock files behind on cleanup.
    let sandbox = Sandbox::new();
    let added = run(&sandbox, &["notice", "add", "neon", "--message", "m"]);
    let created_at = added["created_at"].as_str().unwrap();
    let delivery_dir = sandbox.state_root().join("notice-delivery");
    std::fs::create_dir_all(&delivery_dir).unwrap();
    let record_path = delivery_dir.join("01c62e9f52b2ee2cdc8a54665190c303.json");
    let legacy_lock = delivery_dir.join("01c62e9f52b2ee2cdc8a54665190c303.lock");
    std::fs::write(
        &record_path,
        json!({"session": "agent:legacy-t3", "delivered": {"neon": created_at}}).to_string(),
    )
    .unwrap();
    std::fs::write(&legacy_lock, b"").unwrap();

    // Bridge call sees legacy-t3 + new-codex: must read the literal 17236db
    // record, print nothing, and link new-codex.
    let out = sandbox
        .command()
        .env("T3CODE_THREAD_ID", "legacy-t3")
        .env("CODEX_SESSION_ID", "new-codex")
        .arg("notices")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "");

    let out = sandbox.command().env("CODEX_SESSION_ID", "new-codex").arg("notices").output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "");

    // Removing the notice cleans all dead records AND the legacy .lock file.
    run(&sandbox, &["notice", "rm", "neon"]);
    let leftovers: Vec<_> = std::fs::read_dir(&delivery_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().is_some_and(|name| name != ".lock"))
        .collect();
    assert!(leftovers.is_empty(), "leftover files after rm: {leftovers:?}");
}
