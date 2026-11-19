//! End-to-end suite for advisories through the real binary: the CLI
//! (`vigil advice`, `vigil advisory ...`, `keepalive`) and the MCP
//! tools. A fake `boat` script stands in for the Boat CLI, so the default
//! `boat-budget` advisory is exercised exactly as shipped (argv, extraction,
//! condition, messages, seeding) without a network. Real processes, no fake
//! clock; every watch fires on its first tick so nothing waits out a window.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::*;
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResult},
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use serde_json::{json, Value};

/// A sandbox whose `bin/boat` prints a captured `boat limits --json`
/// (trimmed to the fields vigil reads) and counts its runs. vigil runs with
/// `PATH` = that `bin` plus a `tools` directory holding only `env`, `sh` and
/// `cat`, so whether `boat` is "on PATH" never depends on the host.
struct Rig {
    sandbox: Sandbox,
    advisories: PathBuf,
    runs: PathBuf,
    path: String,
}

/// `tools/` with links to the host's `env`, `sh` and `cat`: what the probes
/// and the fake `boat` need, and nothing else.
fn tools_dir(sandbox: &Sandbox) -> PathBuf {
    let tools = sandbox.dir.path().join("tools");
    std::fs::create_dir_all(&tools).unwrap();
    let host = std::env::var("PATH").unwrap_or_default();
    for name in ["env", "sh", "cat"] {
        let found = std::env::split_paths(&host).map(|dir| dir.join(name)).find(|p| p.is_file()).unwrap_or_else(|| panic!("{name} is not on PATH"));
        std::os::unix::fs::symlink(found, tools.join(name)).unwrap();
    }
    tools
}

fn boat_json(remaining: f64) -> String {
    format!(r#"{{"accessTier":"standard","planIncluded":{{"periodEnd":"2026-12-11T05:35:10.000Z","vmHours":555.56,"vmHoursRemaining":{remaining}}},"creditBalanceHours":0}}"#)
}

impl Rig {
    fn new(remaining: f64) -> Rig {
        let sandbox = Sandbox::new();
        let bin = sandbox.dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = format!("{}:{}", bin.display(), tools_dir(&sandbox).display());
        let runs = sandbox.dir.path().join("boat-runs");
        let data = sandbox.dir.path().join("boat.json");
        std::fs::write(&data, boat_json(remaining)).unwrap();
        // The shipped probe is `env BOAT_NO_UPDATE=1 <boat found on PATH> limits --json`.
        vigil::testing::write_script(&bin.join("boat"), &format!("[ \"$BOAT_NO_UPDATE\" = 1 ] || exit 9\n[ \"$1 $2\" = 'limits --json' ] || exit 8\necho run >> '{}'\ncat '{}'", runs.display(), data.display()));
        let advisories = sandbox.dir.path().join("config/vigil/advisories.toml");
        Rig { sandbox, advisories, runs, path }
    }
    fn boat(&self) -> PathBuf {
        self.sandbox.dir.path().join("bin/boat")
    }
    fn set_remaining(&self, remaining: f64) {
        std::fs::write(self.sandbox.dir.path().join("boat.json"), boat_json(remaining)).unwrap();
    }
    fn probe_runs(&self) -> usize {
        std::fs::read_to_string(&self.runs).map_or(0, |text| text.lines().count())
    }
    fn command(&self) -> std::process::Command {
        let mut command = self.sandbox.command();
        command.env("VIGIL_ADVISORIES_FILE", &self.advisories).env("PATH", &self.path);
        command
    }
    fn run(&self, args: &[&str]) -> std::process::Output {
        let mut command = self.command();
        command.args(args).stdin(std::process::Stdio::null());
        output_within(command, Duration::from_secs(60))
    }
    fn advice_json(&self) -> Value {
        let output = self.run(&["advice", "--json"]);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        stdout_json(&output)
    }
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn only(advice: &Value) -> &Value {
    let list = advice["advice"].as_array().unwrap_or_else(|| panic!("no advice list: {advice}"));
    assert_eq!(list.len(), 1, "{advice}");
    &list[0]
}

#[test]
fn advice_runs_the_shipped_boat_budget_advisory_and_seeds_the_file() {
    // why: `vigil advice [--json]` is what hooks call; with `boat` on PATH
    //      the default boat-budget is seeded when the file is absent and,
    //      with 479.01 plan hours and no credit left, says to prefer Boat. The
    //      plain form prints `id: message`, one line.
    // proves: CLAIM(advisory-boat-budget), CLAIM(advisory-seed), CLAIM(advisory-delivery)
    let rig = Rig::new(479.01);
    assert!(!rig.advisories.exists());
    let advice = rig.advice_json();
    let entry = only(&advice);
    assert_eq!(entry["id"], "boat-budget");
    assert_eq!(entry["kind"], "advice");
    let message = entry["message"].as_str().unwrap();
    assert!(message.contains("479") && message.contains("prefer Boat"), "{message}");
    let seeded = std::fs::read_to_string(&rig.advisories).unwrap();
    assert!(seeded.contains("[advisories.boat-budget]") && seeded.contains(&format!("{:?}", rig.boat().display().to_string())), "the probe runs the boat found on PATH: {seeded}");

    let plain = rig.run(&["advice"]);
    assert!(plain.status.success());
    assert_eq!(String::from_utf8_lossy(&plain.stdout), format!("boat-budget: {message}\n"));
    assert_eq!(rig.probe_runs(), 1, "the second call came from the cache in the state directory");
}

#[test]
fn without_boat_on_path_nothing_is_seeded_and_advice_is_empty() {
    // why: `boat` is optional. With no `boat` on PATH the first use writes a
    //      file with no active advisory (a header and a commented example),
    //      `vigil advice` prints nothing and exits 0, and no probe runs.
    // proves: CLAIM(advisory-seed), CLAIM(tool-presence)
    let rig = Rig::new(479.01);
    std::fs::remove_file(rig.boat()).unwrap();
    assert_eq!(rig.advice_json()["advice"], json!([]));
    let plain = rig.run(&["advice"]);
    assert!(plain.status.success(), "{}", stderr_of(&plain));
    assert!(plain.stdout.is_empty(), "{}", String::from_utf8_lossy(&plain.stdout));
    let seeded = std::fs::read_to_string(&rig.advisories).unwrap();
    assert!(seeded.contains("`boat` was not on PATH") && seeded.contains("# [advisories.boat-budget]"), "{seeded}");
    assert_eq!(rig.probe_runs(), 0);
}

#[test]
fn set_threshold_changes_the_advice_immediately_with_no_new_probe() {
    // why: `vigil advisory set boat-budget threshold=500` takes effect at
    //      once. 479 hours left: prefer Boat. Threshold 500: 479 <= 500, so run heavy
    //      work locally. The probe ran once for both answers.
    // proves: CLAIM(advisory-immediate), CLAIM(advisory-set)
    let rig = Rig::new(479.01);
    assert!(only(&rig.advice_json())["message"].as_str().unwrap().contains("prefer Boat"));
    let set = rig.run(&["advisory", "set", "boat-budget", "threshold=500"]);
    assert!(set.status.success(), "{}", stderr_of(&set));
    assert_eq!(stdout_json(&set)["spec"]["params"]["threshold"], 500.0);
    let message = only(&rig.advice_json())["message"].as_str().unwrap().to_string();
    assert!(message.contains("qex") && message.contains("479") && message.contains("500"), "{message}");
    assert_eq!(rig.probe_runs(), 1);
    // And a hand edit is the same thing.
    let text = std::fs::read_to_string(&rig.advisories).unwrap();
    std::fs::write(&rig.advisories, text.replace("threshold = 500", "threshold = 5")).unwrap();
    assert!(only(&rig.advice_json())["message"].as_str().unwrap().contains("prefer Boat"));
}

#[test]
fn a_probe_that_fails_gives_an_unavailable_entry_not_silence() {
    // why: "advisory <id> unavailable: <reason>". The file is seeded while
    //      `boat` is there (`advisory list` runs no probe); then the fake boat
    //      is removed, so the probe (run through `env`) exits 127 with env's
    //      message; `vigil advice` still exits 0 (hooks must not break) and
    //      says why.
    // proves: CLAIM(advisory-unavailable)
    let rig = Rig::new(479.01);
    assert!(rig.run(&["advisory", "list"]).status.success());
    assert_eq!(rig.probe_runs(), 0, "listing does not probe");
    std::fs::remove_file(rig.boat()).unwrap();
    let output = rig.run(&["advice"]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(text.starts_with("boat-budget: advisory boat-budget unavailable: probe exited with 127: env:"), "{text}");
    assert!(text.contains("No such file"), "{text}");
    let json = rig.advice_json();
    assert_eq!(only(&json)["kind"], "unavailable");
}

#[test]
fn advisory_commands_validate_and_explain_their_errors() {
    // why: owner item 3: unknown ids, bad expressions and missing params are
    //      errors with a clear message (exit 1, message on stderr, nothing on
    //      stdout), and a refused command changes nothing.
    // proves: CLAIM(advisory-valid), CLAIM(advisory-set)
    let rig = Rig::new(479.01);
    rig.advice_json();
    let before = std::fs::read(&rig.advisories).unwrap();
    for (args, needles) in [
        (vec!["advisory", "set", "nope", "threshold=1"], vec!["no advisory named", "boat-budget"]),
        (vec!["advisory", "set", "boat-budget", "treshold=1"], vec!["no parameter", "threshold"]),
        (vec!["advisory", "set", "boat-budget", "threshold=abc"], vec!["threshold", "number"]),
        (vec!["advisory", "set", "boat-budget", "condition=hours <= gone"], vec!["gone"]),
        (vec!["advisory", "set", "boat-budget"], vec![]),
        (vec!["advisory", "show", "nope"], vec!["no advisory named"]),
        (vec!["advisory", "rm", "nope"], vec!["no advisory named"]),
        (vec!["advisory", "add", "bad id", "{}"], vec!["bad advisory id"]),
        (vec!["advisory", "add", "x", "{\"cach_s\": 5}"], vec!["cach_s"]),
        (vec!["advisory", "add", "boat-budget", "{}"], vec![]),
    ] {
        let output = rig.run(&args);
        assert!(!output.status.success(), "{args:?} should fail");
        assert!(output.stdout.is_empty(), "{args:?}: {}", String::from_utf8_lossy(&output.stdout));
        let stderr = stderr_of(&output);
        assert!(stderr.starts_with("error: ") || stderr.contains("Usage"), "{args:?}: {stderr}");
        for needle in needles {
            assert!(stderr.contains(needle), "{args:?} should mention {needle:?}: {stderr}");
        }
    }
    assert_eq!(std::fs::read(&rig.advisories).unwrap(), before);
}

#[test]
fn add_show_list_and_rm_round_trip() {
    // why: owner item 3: the full CLI surface. Add a second advisory from JSON
    //      (argument form), see it in list and show, change it, remove it; the
    //      default stays untouched throughout.
    // proves: CLAIM(advisory-set), CLAIM(advisory-valid)
    let rig = Rig::new(479.01);
    let spec = json!({
        "probe": {"kind": "cmd", "argv": ["sh", "-c", "echo 'free 42 mb'"]},
        "extract": {"name": "free", "regex": "free (\\d+)"},
        "params": {"floor": 100},
        "condition": "free < floor",
        "message_when_true": "Low memory: {free} MB (< {floor})",
        "message_when_false": "Memory fine: {free} MB"
    });
    let added = rig.run(&["advisory", "add", "mem", &spec.to_string()]);
    assert!(added.status.success(), "{}", stderr_of(&added));
    let list = stdout_json(&rig.run(&["advisory", "list"]));
    let ids: Vec<&str> = list.as_array().unwrap().iter().map(|a| a["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["boat-budget", "mem"], "{list}");
    let shown = stdout_json(&rig.run(&["advisory", "show", "mem"]));
    assert_eq!(shown["id"], "mem");
    assert_eq!(shown["spec"]["params"]["floor"], 100.0);
    assert_eq!(shown["advice"]["message"], "Low memory: 42 MB (< 100)");
    assert!(rig.run(&["advisory", "set", "mem", "floor=10"]).status.success());
    let advice = rig.advice_json();
    let messages: Vec<&str> = advice["advice"].as_array().unwrap().iter().map(|a| a["message"].as_str().unwrap()).collect();
    assert_eq!(messages[1], "Memory fine: 42 MB");
    assert!(rig.run(&["advisory", "rm", "mem"]).status.success());
    assert_eq!(stdout_json(&rig.run(&["advisory", "list"])).as_array().unwrap().len(), 1);
    // add also reads stdin when no JSON argument is given
    let mut child = rig.command().args(["advisory", "add", "mem"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    std::io::Write::write_all(&mut stdin, spec.to_string().as_bytes()).unwrap();
    drop(stdin); // end of input
    let output = finish_within(child, Duration::from_secs(30));
    assert!(output.status.success(), "{}", stderr_of(&output));
}

#[test]
fn keepalive_envelopes_carry_the_advice() {
    // why: owner item 4: "every keepalive envelope (CLI and MCP) carries an
    //      advice list" (it said keepalive/wait; `vigil wait` is gone, W42-W1).
    //      CLI half: a watch that fires on its first tick, read through
    //      `keepalive`.
    // proves: CLAIM(advisory-delivery)
    let rig = Rig::new(479.01);
    let input = rig.sandbox.input("w.log");
    for (id, subcommand) in [("k", "keepalive")] {
        std::fs::write(&input, "boot\n").unwrap();
        assert!(rig.run(&["create", &change_watch_json(id, &input, 1)]).status.success());
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new().append(true).open(&input).unwrap();
            writeln!(file, "go").unwrap();
        }
        let output = rig.run(&[subcommand, id]);
        assert!(output.status.success(), "{subcommand}: {}", stderr_of(&output));
        let envelope = stdout_json(&output);
        assert_eq!(envelope["status"], "fired");
        let entry = &envelope["advice"][0];
        assert_eq!(entry["id"], "boat-budget", "{subcommand}: {envelope}");
        assert!(entry["message"].as_str().unwrap().contains("prefer Boat"));
    }
    let status = stdout_json(&rig.run(&["status", "k"]));
    assert!(status.get("advice").is_none(), "status does not carry advice");
}

// ---- MCP ----------------------------------------------------------------

type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

async fn connect(rig: &Rig) -> Client {
    let transport = TokioChildProcess::new(rig.sandbox.tokio_command().configure(|c| {
        c.arg("mcp").env("VIGIL_ADVISORIES_FILE", &rig.advisories).env("PATH", &rig.path);
    }))
    .unwrap();
    ().serve(transport).await.expect("initialize handshake")
}

async fn call(client: &Client, tool: &'static str, arguments: Value) -> CallToolResult {
    let arguments = arguments.as_object().unwrap().clone();
    tokio::time::timeout(Duration::from_secs(60), client.peer().call_tool(CallToolRequestParams::new(tool).with_arguments(arguments))).await.expect("tool call returns").unwrap()
}

fn text(result: &CallToolResult) -> String {
    result.content[0].as_text().expect("text content").text.clone()
}

fn json_of(result: &CallToolResult) -> Value {
    assert_ne!(result.is_error, Some(true), "{}", text(result));
    serde_json::from_str(&text(result)).unwrap_or_else(|e| panic!("not JSON ({e}): {}", text(result)))
}

#[tokio::test]
async fn the_mcp_tools_list_show_set_add_remove_and_advise() {
    // why: owner item 3: "matching MCP tools (advisory_set, advisory_list,
    //      ...)". The model drives the same file: list sees the default, set
    //      threshold=500 flips the advice at once (probe ran once), add and rm
    //      work, and every failure is isError with the CLI's message.
    // proves: CLAIM(advisory-set), CLAIM(advisory-immediate), CLAIM(advisory-valid)
    let rig = Rig::new(479.01);
    let client = connect(&rig).await;
    let tools: Vec<String> = client.list_all_tools().await.unwrap().iter().map(|t| t.name.to_string()).collect();
    for name in ["advice", "advisory_list", "advisory_show", "advisory_add", "advisory_set", "advisory_rm"] {
        assert!(tools.contains(&name.to_string()), "{name} in {tools:?}");
    }
    let listed = json_of(&call(&client, "advisory_list", json!({})).await);
    assert_eq!(listed[0]["id"], "boat-budget");
    let advice = json_of(&call(&client, "advice", json!({})).await);
    assert!(advice["advice"][0]["message"].as_str().unwrap().contains("prefer Boat"), "{advice}");
    let set = json_of(&call(&client, "advisory_set", json!({"id": "boat-budget", "settings": {"threshold": 500}})).await);
    assert_eq!(set["spec"]["params"]["threshold"], 500.0);
    let advice = json_of(&call(&client, "advice", json!({})).await);
    assert!(advice["advice"][0]["message"].as_str().unwrap().contains("qex"), "{advice}");
    assert_eq!(rig.probe_runs(), 1);
    let shown = json_of(&call(&client, "advisory_show", json!({"id": "boat-budget"})).await);
    assert_eq!(shown["spec"]["extract"]["name"], "hours");

    for (tool, arguments, needle) in [
        ("advisory_set", json!({"id": "nope", "settings": {"threshold": 1}}), "no advisory named"),
        ("advisory_set", json!({"id": "boat-budget", "settings": {"treshold": 1}}), "no parameter"),
        ("advisory_set", json!({"id": "boat-budget", "settings": {"condition": "hours <= gone"}}), "gone"),
        ("advisory_show", json!({"id": "nope"}), "no advisory named"),
        ("advisory_rm", json!({"id": "nope"}), "no advisory named"),
        ("advisory_add", json!({"id": "bad id", "spec": {}}), "bad advisory id"),
    ] {
        let result = call(&client, tool, arguments.clone()).await;
        assert_eq!(result.is_error, Some(true), "{tool} {arguments}");
        assert!(text(&result).contains(needle), "{tool}: {}", text(&result));
    }

    let spec = json!({
        "probe": {"kind": "cmd", "argv": ["sh", "-c", "echo 7"]}, "extract": {"regex": "\\d+"},
        "condition": "value < 10", "message_when_true": "tiny {value}", "message_when_false": "big {value}"
    });
    let added = json_of(&call(&client, "advisory_add", json!({"id": "tiny", "spec": spec})).await);
    assert_eq!(added["id"], "tiny");
    let again = call(&client, "advisory_add", json!({"id": "tiny", "spec": spec})).await;
    assert_eq!(again.is_error, Some(true));
    assert!(text(&again).contains("already exists"));
    let replaced = json_of(&call(&client, "advisory_add", json!({"id": "tiny", "spec": spec, "replace": true})).await);
    assert_eq!(replaced["id"], "tiny");
    json_of(&call(&client, "advisory_rm", json!({"id": "tiny"})).await);
    assert_eq!(json_of(&call(&client, "advisory_list", json!({})).await).as_array().unwrap().len(), 1);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn an_mcp_keepalive_result_carries_the_advice() {
    // why: owner item 4, MCP half: the agent sees `advice` in the very result
    //      it already reads. A watch that fires on its first tick.
    // proves: CLAIM(advisory-delivery)
    let rig = Rig::new(4.0);
    let client = connect(&rig).await;
    let input = rig.sandbox.input("w.log");
    std::fs::write(&input, "boot\n").unwrap();
    let created = call(&client, "watch_create", json!({"id": "w", "prompt": "", "inputs": [{"kind": "file", "path": input}], "interval_s": 1, "deadline_s": 3600, "fire": "change"})).await;
    assert_ne!(created.is_error, Some(true), "{}", text(&created));
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(&input).unwrap();
        writeln!(file, "go").unwrap();
    }
    let envelope = json_of(&call(&client, "keepalive", json!({})).await);
    assert_eq!(envelope["status"], "fired");
    let message = envelope["advice"][0]["message"].as_str().unwrap();
    assert!(message.contains("qex") && message.contains("4 hours"), "4 plan hours + 0 credit hours is below the 10-hour threshold: {message}");
    rig.set_remaining(500.0);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn the_server_instructions_mention_advice() {
    // why: the agent needs no skill to act on advice: the instructions (sent
    //      at initialize) say results carry `advice` to follow. They must stay
    //      under 512 characters (Codex reads the first 512).
    // proves: CLAIM(instructions), CLAIM(advisory-delivery)
    let rig = Rig::new(479.01);
    let client = connect(&rig).await;
    let instructions = client.peer_info().and_then(|info| info.instructions.clone()).expect("instructions");
    assert!(instructions.contains("advice"), "{instructions}");
    assert!(instructions.chars().count() < 512, "{} chars", instructions.chars().count());
    client.cancel().await.unwrap();
}
