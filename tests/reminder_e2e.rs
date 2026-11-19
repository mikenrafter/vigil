//! Transport parity for project reminders. Real binary and probes; no network.
mod common;
use common::*;
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResult},
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use serde_json::{Value, json};

fn run(sandbox: &Sandbox, args: &[&str]) -> Value {
    let output = sandbox.run(args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout_json(&output)
}
fn add(sandbox: &Sandbox) -> Value {
    run(
        sandbox,
        &[
            "remind",
            "add",
            "release",
            "--scope",
            sandbox.cwd().to_str().unwrap(),
            "--days",
            "mon,tue,wed,thu,fri,sat,sun",
            "--tz",
            "UTC",
            "--probe",
            r#"["sh","-c","printf '12'"]"#,
            "--condition",
            "value >= 10",
            "--message",
            "Release: {output}",
        ],
    )
}

#[test]
fn cli_lifecycle_and_real_command_probe() {
    // WHY: prove argv JSON, persisted TOML, current evaluation, advice text and
    // suppression across actual processes agree with the documented CLI.
    let sandbox = Sandbox::new();
    add(&sandbox);
    let list = run(&sandbox, &["remind", "list"]);
    assert_eq!(list[0]["spec"]["probe"], json!(["sh", "-c", "printf '12'"]));
    assert_eq!(
        run(&sandbox, &["remind", "show", "release"])["evaluation"]["kind"],
        "due"
    );
    assert_eq!(
        run(&sandbox, &["advice", "--json"])["reminders"][0]["message"],
        "Release: 12"
    );
    assert_eq!(
        String::from_utf8(sandbox.run(&["advice"]).stdout).unwrap(),
        "release: Release: 12\n"
    );
    run(&sandbox, &["remind", "snooze", "release", "30m"]);
    assert_eq!(
        run(&sandbox, &["remind", "show", "release"])["evaluation"]["kind"],
        "snoozed"
    );
    run(&sandbox, &["remind", "done", "release"]);
    assert_eq!(
        run(&sandbox, &["remind", "show", "release"])["evaluation"]["kind"],
        "done"
    );
    assert_eq!(run(&sandbox, &["advice", "--json"])["reminders"], json!([]));
    run(&sandbox, &["remind", "rm", "release"]);
    assert_eq!(run(&sandbox, &["remind", "list"]), json!([]));
    // Reusing an id starts a fresh lifecycle, not last week's suppression.
    add(&sandbox);
    assert_eq!(
        run(&sandbox, &["remind", "show", "release"])["evaluation"]["kind"],
        "due"
    );
}

#[test]
fn cli_scope_uses_callers_cwd_even_for_a_foreign_watch() {
    // WHY: scope follows the calling process, not the cwd that made the watch.
    let sandbox = Sandbox::new();
    add(&sandbox);
    run(&sandbox, &["remind", "show", "release"]);
    let input = sandbox.input("trigger");
    std::fs::write(&input, "old").unwrap();
    let watch = run(
        &sandbox,
        &[
            "create",
            &json!({"id":"watch","inputs":[{"kind":"file","path":input}],"fire":"change"})
                .to_string(),
        ],
    );
    assert_eq!(watch["watch_id"], "watch");
    std::fs::write(&input, "new").unwrap();
    let mut command = sandbox.command_in(&sandbox.cwd2());
    let output = command.args(["keepalive", "watch"]).output().unwrap();
    assert!(output.status.success());
    assert!(stdout_json(&output).get("reminders").is_none());
    assert_eq!(
        run(&sandbox, &["keepalive", "watch"])["reminders"][0]["kind"],
        "due"
    );
}

#[test]
fn invalid_cli_edits_leave_config_unchanged() {
    // WHY: invalid fields or duplicate ids cannot partly mutate live config.
    let sandbox = Sandbox::new();
    add(&sandbox);
    let path = sandbox.dir.path().join("config/vigil/reminders.toml");
    let before = std::fs::read(&path).unwrap();
    for args in [
        vec![
            "remind",
            "add",
            "new",
            "--scope",
            "relative",
            "--days",
            "wed",
            "--message",
            "due",
        ],
        vec!["remind", "snooze", "release", "0m"],
        vec!["remind", "done", "../escape"],
        vec!["remind", "rm", "missing"],
    ] {
        let output = sandbox.run(&args);
        assert!(!output.status.success(), "{args:?}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
    let output = sandbox.run(&[
        "remind",
        "add",
        "release",
        "--scope",
        sandbox.cwd().to_str().unwrap(),
        "--days",
        "wed",
        "--message",
        "replacement",
    ]);
    assert!(!output.status.success());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;
async fn call(client: &Client, tool: &'static str, args: Value) -> Value {
    let result: CallToolResult = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        client.peer().call_tool(
            CallToolRequestParams::new(tool).with_arguments(args.as_object().unwrap().clone()),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_ne!(result.is_error, Some(true), "{result:?}");
    serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap()
}
#[tokio::test]
async fn mcp_lifecycle_and_delivery_use_server_startup_cwd() {
    // WHY: drive every MCP operation and read its state through the CLI; the
    // shared config and same cwd must produce identical reminder evaluations.
    let sandbox = Sandbox::new();
    let transport = TokioChildProcess::new(sandbox.tokio_command().configure(|c| {
        c.arg("mcp");
    }))
    .unwrap();
    let client = ().serve(transport).await.unwrap();
    let spec = json!({"scope":[sandbox.cwd()],"days":["mon","tue","wed","thu","fri","sat","sun"],"tz":"UTC","probe":["sh","-c","echo 12"],"condition":"value >= 10","message":"Ship {value}","period":"day"});
    call(&client, "remind_add", json!({"id":"release","spec":spec})).await;
    assert_eq!(
        call(&client, "remind_list", json!({})).await[0]["spec"]["period"],
        "day"
    );
    assert_eq!(
        call(&client, "remind_show", json!({"id":"release"})).await["evaluation"]["kind"],
        "due"
    );
    assert_eq!(
        call(&client, "advice", json!({})).await["reminders"][0]["message"],
        "Ship 12"
    );
    let input = sandbox.input("mcp-trigger");
    std::fs::write(&input, "old").unwrap();
    call(
        &client,
        "watch_create",
        json!({"id":"mcp-watch","inputs":[{"kind":"file","path":input}],"fire":"change"}),
    )
    .await;
    std::fs::write(&input, "new").unwrap();
    assert_eq!(
        call(&client, "keepalive", json!({"watch_id":"mcp-watch"})).await["reminders"][0]["kind"],
        "due"
    );
    call(
        &client,
        "remind_snooze",
        json!({"id":"release","duration":"1h"}),
    )
    .await;
    assert_eq!(
        run(&sandbox, &["remind", "show", "release"])["evaluation"]["kind"],
        "snoozed"
    );
    call(&client, "remind_done", json!({"id":"release"})).await;
    assert_eq!(
        run(&sandbox, &["remind", "show", "release"])["evaluation"]["kind"],
        "done"
    );
    call(&client, "remind_rm", json!({"id":"release"})).await;
    assert_eq!(run(&sandbox, &["remind", "list"]), json!([]));
    client.cancel().await.unwrap();
}

#[test]
fn example_release_probe_uses_real_local_git_repositories() {
    // WHY: C1/C2: the example policy must work against real refs, tags and
    // missing blobs, not fake git output that mirrors the implementation.
    let output = std::process::Command::new("python3")
        .args(["-I", "-m", "unittest", "discover", "-s"])
        .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples"))
        .args(["-p", "test_release_probe.py"])
        .output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn real_sleeping_probe_does_not_delay_advice_or_a_fired_watch() {
    // WHY: R2: exercise subprocess/runtime shutdown as well as virtual readers.
    // A cold `sleep 15` must not hold CLI stdout or a fired watch past 2 seconds.
    let sandbox = Sandbox::new();
    run(&sandbox, &["remind", "add", "slow", "--scope", sandbox.cwd().to_str().unwrap(),
        "--days", "mon,tue,wed,thu,fri,sat,sun", "--tz", "UTC", "--probe", r#"["sleep","15"]"#,
        "--timeout-s", "19", "--message", "Ready"]);
    let started = std::time::Instant::now();
    let report = run(&sandbox, &["advice", "--json"]);
    assert!(started.elapsed() < std::time::Duration::from_secs(2), "advice waited for sleep 15");
    assert_eq!(report["reminders"][0]["kind"], "unavailable");
    let input = sandbox.input("slow-watch");
    std::fs::write(&input, "old").unwrap();
    run(&sandbox, &["create", &json!({"id":"watch","inputs":[{"kind":"file","path":input}],"fire":"change"}).to_string()]);
    std::fs::write(&input, "new").unwrap();
    let started = std::time::Instant::now();
    let result = run(&sandbox, &["keepalive", "watch"]);
    assert_eq!(result["status"], "fired");
    assert!(started.elapsed() < std::time::Duration::from_secs(2), "fired watch waited for sleep 15");
}
