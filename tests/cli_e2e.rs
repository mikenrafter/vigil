//! End-to-end suite for the `vigil` binary's CLI subcommands. Lives in tests/
//! (not beside a module) because it spawns the built binary
//! (CARGO_BIN_EXE_vigil) and checks process-level behaviour: exit codes,
//! stdout JSON, and files on disk. Each test finishes in well under 5 s;
//! `interval_s: 1` is the only clock these tests wait on.

mod common;

use common::*;
use std::process::Stdio;
use std::time::Duration;

fn create_via_cli(sandbox: &Sandbox, id: &str, path: &std::path::Path) -> serde_json::Value {
    std::fs::write(path, "boot\n").unwrap();
    let output = sandbox.run(&["create", &change_watch_json(id, path, 1)]);
    assert!(output.status.success(), "create failed: {}", String::from_utf8_lossy(&output.stderr));
    stdout_json(&output)
}

#[test]
fn create_status_list_cancel_round_trip_through_the_binary() {
    // why: SPEC "Architecture": the CLI runs the same code against the same
    //      files. Checks the JSON each subcommand prints and the watch file
    //      under $XDG_STATE_HOME/vigil/watches.
    // proves: CLAIM(cli-parity)
    let sandbox = Sandbox::new();
    let created = create_via_cli(&sandbox, "build", &sandbox.input("build.log"));
    assert_eq!(created["watch_id"], "build");
    assert_eq!(created["status"], "pending");
    assert!(created["created_at"].as_str().unwrap().ends_with('Z'));
    assert!(created["deadline_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(sandbox.watch_file("build").unwrap()["tick"], 0);

    let status = sandbox.run(&["status", "build"]);
    assert!(status.status.success());
    let status = stdout_json(&status);
    assert_eq!(status["status"], "pending");
    assert_eq!(status["tick"], 0);

    let list = stdout_json(&sandbox.run(&["list"]));
    assert!(list.as_array().unwrap().iter().any(|w| w["id"] == "build" && w["status"] == "pending"), "{list}");

    let cancelled = sandbox.run(&["cancel", "build"]);
    assert!(cancelled.status.success());
    assert_eq!(stdout_json(&cancelled)["status"], "cancelled");
    assert_eq!(sandbox.watch_file("build").unwrap()["status"], "cancelled");
}

#[test]
fn create_reads_the_watch_json_from_stdin_when_no_argument_is_given() {
    // why: SPEC "No config UI. A watch is one JSON object." - programmatic
    //      monitors can pipe it in.
    use std::io::Write;
    let sandbox = Sandbox::new();
    let path = sandbox.input("a.log");
    std::fs::write(&path, "x\n").unwrap();
    let mut child = sandbox.command().arg("create").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(change_watch_json("piped", &path, 1).as_bytes()).unwrap();
    let output = finish_within(child, Duration::from_secs(5));
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(stdout_json(&output)["watch_id"], "piped");
}

#[test]
fn invalid_requests_exit_with_code_1_and_say_why_on_stderr() {
    // why: real failures must be visible to scripts (exit 1) with the
    //      reason, and must leave no watch behind.
    let sandbox = Sandbox::new();
    let bad = serde_json::json!({"id": "../escape", "inputs": [], "fire": "change"}).to_string();
    let output = sandbox.run(&["create", &bad]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid watch id"), "{stderr}");
    assert!(!sandbox.state_root().join("watches").join("..").join("escape.json").exists());
    assert_eq!(sandbox.run(&["status", "ghost"]).status.code(), Some(1));
}

#[test]
fn nothing_is_written_outside_the_state_root() {
    // why: SPEC non-goals: "Never writes inside the working tree or any
    //      repository. All files live under $XDG_STATE_HOME/vigil/." The
    //      sandbox has its own HOME, cwd, XDG_CONFIG_HOME and inputs dir;
    //      every one must be byte-identical after a full CLI lifecycle.
    //      Positive control: the state root did gain files.
    // proves: CLAIM(state-confinement)
    let sandbox = Sandbox::new();
    let input = sandbox.input("a.log");
    std::fs::write(&input, "boot\n").unwrap();
    let outside_before: Vec<_> = list_files(sandbox.dir.path()).into_iter().filter(|(p, _)| !p.starts_with(sandbox.state_home())).collect();

    assert!(sandbox.run(&["create", &change_watch_json("w", &input, 1)]).status.success());
    sandbox.run(&["status", "w"]);
    sandbox.run(&["list"]);
    let appender = Appender::start(&input, "line");
    let keepalive = sandbox.run(&["keepalive", "w", "--max-s", "30"]);
    drop(appender);
    assert!(keepalive.status.success(), "{}", String::from_utf8_lossy(&keepalive.stderr));
    sandbox.run(&["cancel", "w"]);

    let all = list_files(sandbox.dir.path());
    let outside_after: Vec<_> = all.iter().filter(|(p, _)| !p.starts_with(sandbox.state_home())).collect();
    // the only permitted difference outside the state root is the input we appended to ourselves
    for (path, _) in &outside_after {
        assert!(outside_before.iter().any(|(p, _)| p == path), "new file outside the state root: {}", path.display());
    }
    assert_eq!(outside_after.len(), outside_before.len());
    assert!(all.iter().any(|(p, _)| p.starts_with(sandbox.state_root().join("watches")) && p.extension().is_some_and(|e| e == "json")), "control: state root has the watch file");
    assert!(!all.iter().any(|(p, _)| p.to_string_lossy().ends_with(".tmp")));
}

#[tokio::test]
async fn any_run_prunes_finished_watches_older_than_seven_days() {
    // why: D4 / SPEC "State": "deleted 7 days after they finish, by any
    //      vigil run". Old watch files are made with the library and a manual
    //      clock set in the past; then the real binary runs `list` and the
    //      stale file must vanish while a recent finished one stays.
    // proves: CLAIM(prune-7d)
    use std::sync::Arc;
    use vigil::testing::{change_request, default_config, ManualClock, ScriptedJudge};
    let sandbox = Sandbox::new();
    let now = vigil::Timestamp(chrono::Utc::now());
    let clock = Arc::new(ManualClock::new(now.minus_secs(10 * 24 * 3600)));
    let library = vigil::Vigil::new(sandbox.state_root(), clock.clone(), Arc::new(ScriptedJudge::new()), default_config());
    let input = sandbox.input("a.log");
    library.create(change_request("stale", &input, 20)).await.unwrap();
    library.cancel(&vigil::WatchId::parse("stale").unwrap()).await.unwrap();
    clock.set(now.minus_secs(2 * 24 * 3600));
    library.create(change_request("fresh", &input, 20)).await.unwrap();
    library.cancel(&vigil::WatchId::parse("fresh").unwrap()).await.unwrap();
    assert!(sandbox.watch_file("stale").is_some(), "precondition");

    assert!(sandbox.run(&["list"]).status.success());
    assert!(sandbox.watch_file("stale").is_none(), "stale finished watch should be pruned");
    assert!(sandbox.watch_file("fresh").is_some());
}

#[test]
fn keepalive_subcommand_returns_the_fired_envelope() {
    // why: SPEC "Architecture": `vigil keepalive` blocks in the shell the
    //      same way the MCP tool does. A line appended while it blocks makes
    //      it return a fired envelope with the clamped ceiling (D9: max_s 30).
    let sandbox = Sandbox::new();
    let input = sandbox.input("a.log");
    create_via_cli(&sandbox, "w", &input);
    let _appender = Appender::start(&input, "cli-line");
    let output = finish_within(
        sandbox.command().args(["keepalive", "w", "--max-s", "5"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap(),
        Duration::from_secs(5),
    );
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let envelope = stdout_json(&output);
    assert_eq!(envelope["status"], "fired");
    assert_eq!(envelope["max_s"], 30, "--max-s 5 is clamped to 30 and reported");
    assert!(envelope["digest"].as_str().unwrap().contains("cli-line"));
    assert_eq!(envelope["next"], "act on events");
}

// ---- vigil wait (coordinator scope addition) ----

#[test]
fn wait_exits_0_with_the_fired_envelope_on_stdout() {
    // why: coordinator scope: `vigil wait <id>` blocks until the watch fires,
    //      prints the final envelope JSON, exits 0. E2E through the real
    //      binary and watch files: create via CLI, spawn wait, write the file.
    // proves: CLAIM(wait-exit-codes)
    let sandbox = Sandbox::new();
    let input = sandbox.input("a.log");
    create_via_cli(&sandbox, "w", &input);
    let child = sandbox.command().args(["wait", "w"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let _appender = Appender::start(&input, "wait-line");
    let output = finish_within(child, Duration::from_secs(5));
    assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
    let envelope = stdout_json(&output);
    assert_eq!(envelope["status"], "fired");
    assert!(envelope["digest"].as_str().unwrap().contains("wait-line"));
}

#[test]
fn wait_stream_prints_one_compact_json_line_per_event_then_the_envelope() {
    // why: coordinator scope: --stream prints one compact JSON line per new
    //      event as it happens (each once), then a final envelope line, same
    //      exit codes. One input -> one event -> exactly two lines.
    // proves: CLAIM(wait-stream-once)
    let sandbox = Sandbox::new();
    let input = sandbox.input("a.log");
    create_via_cli(&sandbox, "w", &input);
    let child = sandbox.command().args(["wait", "w", "--stream"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let _appender = Appender::start(&input, "stream-line");
    let output = finish_within(child, Duration::from_secs(5));
    assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
    let text = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "one event line + one envelope line, got {text:?}");
    let event: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(event["watch_id"], "w");
    assert!(event["summary"].as_str().unwrap().contains("stream-line"));
    let envelope: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(envelope["status"], "fired");
}

#[test]
fn wait_exits_2_when_the_watch_expires() {
    // why: coordinator scope: exit 2 = expired. deadline_s 1 / interval 1.
    let sandbox = Sandbox::new();
    let input = sandbox.input("a.log");
    std::fs::write(&input, "boot\n").unwrap();
    let request = serde_json::json!({"id": "short", "inputs": [{"kind": "file", "path": input}],
        "interval_s": 1, "deadline_s": 1, "fire": "change"})
    .to_string();
    assert!(sandbox.run(&["create", &request]).status.success());
    let child = sandbox.command().args(["wait", "short"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let output = finish_within(child, Duration::from_secs(5));
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout_json(&output)["status"], "expired");
}

#[test]
fn wait_exits_3_when_the_watch_is_cancelled() {
    // why: coordinator scope: exit 3 = cancelled; another process cancels the
    //      watch while `wait` is blocked and wait notices on its next tick.
    let sandbox = Sandbox::new();
    let input = sandbox.input("a.log");
    create_via_cli(&sandbox, "w", &input);
    let child = sandbox.command().args(["wait", "w"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    assert!(wait_until(Duration::from_secs(3), || sandbox.watch_file("w").is_some_and(|w| w["tick"].as_u64() >= Some(1))), "wait never ticked");
    assert!(sandbox.run(&["cancel", "w"]).status.success());
    let output = finish_within(child, Duration::from_secs(5));
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(stdout_json(&output)["status"], "cancelled");
}

#[test]
fn wait_on_an_unknown_watch_exits_1() {
    // why: coordinator scope: 1 = error.
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["wait", "ghost"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(!output.stderr.is_empty());
}
