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
fn example_edge_probe_checks_lib_diff_and_abstains_on_git_failure() {
    // WHY: the example is an executable operator policy, not a placeholder.
    // Fake external programs provide known facts; the real example evaluates them.
    let sandbox = Sandbox::new();
    let bin = sandbox.dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    vigil::testing::write_script(&bin.join("gh"), "printf '[]'");
    let git = bin.join("git");
    vigil::testing::write_script(
        &git,
        "case \"$1\" in merge-base) echo abcdef123456;; diff) printf '70\\t40\\tlib/a.dart\\n';; *) exit 2;; esac",
    );
    let probe =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/edge-release-probe.py");
    let run_probe = |threshold: &str| {
        std::process::Command::new("python3")
            .arg(&probe)
            .args([
                "--dir",
                sandbox.cwd().to_str().unwrap(),
                "--tz",
                "UTC",
                "--min-lines",
                threshold,
            ])
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .output()
            .unwrap()
    };
    let due = run_probe("100");
    assert_eq!(
        due.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&due.stderr)
    );
    assert_eq!(
        String::from_utf8(due.stdout).unwrap(),
        "110 changed lib/ lines across 1 files since abcdef123456 on integrate/next; threshold 100\n"
    );
    assert_eq!(run_probe("111").status.code(), Some(1));
    vigil::testing::write_script(&git, "exit 7");
    let failed = run_probe("100");
    assert_eq!(failed.status.code(), Some(2));
    assert!(
        String::from_utf8(failed.stderr)
            .unwrap()
            .starts_with("Release check unavailable:")
    );
}
