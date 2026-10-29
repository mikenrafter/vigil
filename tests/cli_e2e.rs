//! End-to-end suite for the `vigil` binary's CLI subcommands. Lives in tests/
//! (not beside a module) because it spawns the built binary
//! (CARGO_BIN_EXE_vigil) and checks process-level behaviour: exit codes,
//! stdout JSON, and files on disk. Each test finishes in well under 5 s;
//! `interval_s: 1` is the only clock these tests wait on. Since V1 the
//! baseline is taken at `create`, so a single write after create is enough
//! to make a watch fire: no repeating appender is needed (and a repeating
//! appender used to hide the old create-to-first-tick gap).

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

fn append_once(path: &std::path::Path, line: &str) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{line}").unwrap();
}

fn piped(mut command: std::process::Command) -> std::process::Child {
    command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
}

#[test]
fn create_status_list_cancel_round_trip_through_the_binary() {
    // why: SPEC "Architecture": the CLI runs the same code against the same
    //      files. Checks the JSON each subcommand prints and the watch file
    //      under $XDG_STATE_HOME/vigil/watches. V8: status names its watch.
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
    assert_eq!(status["watch_id"], "build");

    let list = stdout_json(&sandbox.run(&["list"]));
    assert!(list.as_array().unwrap().iter().any(|w| w["id"] == "build" && w["status"] == "pending"), "{list}");

    let cancelled = sandbox.run(&["cancel", "build"]);
    assert!(cancelled.status.success());
    let cancelled = stdout_json(&cancelled);
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(cancelled["next"], "watch ended; create a new watch if still needed");
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
    let mut child = piped({
        let mut c = sandbox.command();
        c.arg("create").stdin(Stdio::piped());
        c
    });
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
fn an_https_url_input_is_refused_at_create_with_exit_1_and_writes_nothing() {
    // why: V19/D7 (S19): URL inputs are http only; the refusal is a create-time
    //      error through the real binary, not a failure at the first tick.
    //      Control: the same request with http is accepted.
    // proves: CLAIM(validation)
    let sandbox = Sandbox::new();
    let request = |url: &str| serde_json::json!({"id": "u", "inputs": [{"kind": "url", "url": url}], "fire": "change"}).to_string();
    let refused = sandbox.run(&["create", &request("https://example.com/health")]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("https://example.com/health"));
    assert!(sandbox.watch_file("u").is_none());
    assert!(sandbox.run(&["create", &request("http://127.0.0.1:9/health")]).status.success(), "control: http is fine");
}

#[test]
fn nothing_is_written_outside_the_state_root() {
    // why: SPEC non-goals: "Never writes inside the working tree or any
    //      repository. All files live under $XDG_STATE_HOME/vigil/." S24: the
    //      old test excluded ALL of state-home and compared file NAMES. Now
    //      exactly state-home/vigil is excluded, and the bytes of everything
    //      else (HOME, cwd, XDG_CONFIG_HOME, sibling dirs in state-home, the
    //      inputs) must be identical; the single exemption is the input file
    //      we append to ourselves, which may only grow. Positive controls:
    //      the state root gained the lock and the watch file, and the diff
    //      helper flags a stray file we plant in state-home.
    // proves: CLAIM(state-confinement)
    let sandbox = Sandbox::new();
    let input = sandbox.input("a.log");
    std::fs::write(&input, "boot\n").unwrap();
    let before = sandbox.snapshot_outside_state_root();

    assert!(sandbox.run(&["create", &change_watch_json("w", &input, 1)]).status.success());
    sandbox.run(&["status", "w"]);
    sandbox.run(&["list"]);
    append_once(&input, "line");
    let keepalive = sandbox.run(&["keepalive", "w", "--max-s", "30"]);
    assert!(keepalive.status.success(), "{}", String::from_utf8_lossy(&keepalive.stderr));
    sandbox.run(&["cancel", "w"]);

    let after = sandbox.snapshot_outside_state_root();
    assert_eq!(snapshot_diff(&before, &after, &input), Vec::<String>::new());

    let root = sandbox.state_root();
    assert!(root.join("lock").is_file(), "control: the single global lock file exists");
    assert!(root.join("watches").join("w.json").is_file(), "control: the watch file exists");
    let mut under_root: Vec<String> = list_files(&root).into_iter().map(|(p, _)| p.strip_prefix(&root).unwrap().to_string_lossy().to_string()).collect();
    under_root.sort();
    assert_eq!(under_root, vec!["lock", "watches/w.json"], "only the lock and json files, no .tmp, no per-watch lock");

    std::fs::write(sandbox.state_home().join("stray.txt"), "x").unwrap();
    let with_stray = sandbox.snapshot_outside_state_root();
    assert_eq!(snapshot_diff(&after, &with_stray, &input).len(), 1, "control: a stray file in state-home itself is detected");
}

#[tokio::test]
async fn any_run_prunes_finished_watches_older_than_seven_days() {
    // why: D4 / SPEC "State": "deleted 7 days after they finish, by any
    //      vigil run". Old watch files are made with the library and a manual
    //      clock set in the past; then the real binary runs `list` and the
    //      stale file must vanish while a recent finished one stays. V16: an
    //      abandoned pending watch (deadline passed > 7 days ago) goes too,
    //      and the lock file stays.
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
    library.create(change_request("abandoned", &input, 20)).await.unwrap();
    clock.set(now.minus_secs(2 * 24 * 3600));
    library.create(change_request("fresh", &input, 20)).await.unwrap();
    library.cancel(&vigil::WatchId::parse("fresh").unwrap()).await.unwrap();
    assert!(sandbox.watch_file("stale").is_some() && sandbox.watch_file("abandoned").is_some(), "precondition");

    assert!(sandbox.run(&["list"]).status.success());
    assert!(sandbox.watch_file("stale").is_none(), "stale finished watch should be pruned");
    assert!(sandbox.watch_file("abandoned").is_none(), "V16: abandoned pending watch should be pruned");
    assert!(sandbox.watch_file("fresh").is_some());
    assert!(sandbox.state_root().join("lock").exists(), "V5: the lock file is never deleted");
}

#[test]
fn keepalive_subcommand_returns_the_fired_envelope() {
    // why: SPEC "Architecture": `vigil keepalive` blocks in the shell the
    //      same way the MCP tool does. V1: ONE line written after create (no
    //      repeating appender) makes it return a fired envelope on its first
    //      tick, with the clamped ceiling (V26: max_s 30), the stable event_id
    //      (V3) and the watch id (V8).
    // proves: CLAIM(baseline-at-create)
    let sandbox = Sandbox::new();
    let input = sandbox.input("a.log");
    create_via_cli(&sandbox, "w", &input);
    append_once(&input, "cli-line");
    let output = finish_within(piped({
        let mut c = sandbox.command();
        c.args(["keepalive", "w", "--max-s", "5"]);
        c
    }), Duration::from_secs(5));
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let envelope = stdout_json(&output);
    assert_eq!(envelope["status"], "fired");
    assert_eq!(envelope["watch_id"], "w");
    assert_eq!(envelope["tick"], 1, "fired on the very first tick: the baseline was taken at create");
    assert_eq!(envelope["max_s"], 30, "--max-s 5 is clamped to 30 and reported");
    assert_eq!(envelope["events"][0]["event_id"], "w:1");
    assert!(envelope["digest"].as_str().unwrap().contains("cli-line"));
    assert_eq!(envelope["next"], "act on events");
}

#[test]
fn a_fired_result_is_replayed_by_status_keepalive_and_wait_with_the_same_event_ids() {
    // why: V3 (S3): the terminal result is stored once and replayed unchanged
    //      on every later call, across processes. Four separate `vigil`
    //      processes (keepalive, then status, keepalive, wait) must print the
    //      same events with the same event_id and digest, and none may block.
    // proves: CLAIM(terminal-replay)
    let sandbox = Sandbox::new();
    let input = sandbox.input("a.log");
    create_via_cli(&sandbox, "w", &input);
    append_once(&input, "replay-me");
    let first = stdout_json(&sandbox.run(&["keepalive", "w", "--max-s", "30"]));
    assert_eq!(first["status"], "fired");
    let started = std::time::Instant::now();
    for args in [vec!["status", "w"], vec!["keepalive", "w", "--max-s", "30"], vec!["wait", "w"]] {
        let output = sandbox.run(&args);
        let replay = stdout_json(&output);
        assert_eq!(replay["status"], "fired", "{args:?}");
        assert_eq!(replay["events"], first["events"], "{args:?}: identical events including event_id");
        assert_eq!(replay["digest"], first["digest"], "{args:?}");
    }
    assert!(started.elapsed() < Duration::from_secs(5), "replays must not block");
}

#[test]
fn an_id_that_still_has_a_file_is_refused_with_exit_1() {
    // why: V6 (S6): reusing an id (pending OR finished) is an error, through
    //      the real binary; the stored result must be untouched. Control: a
    //      different id works.
    // proves: CLAIM(no-id-reuse)
    let sandbox = Sandbox::new();
    let input = sandbox.input("a.log");
    create_via_cli(&sandbox, "w", &input);
    let again = sandbox.run(&["create", &change_watch_json("w", &input, 1)]);
    assert_eq!(again.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&again.stderr).contains("already exists"), "{}", String::from_utf8_lossy(&again.stderr));

    assert!(sandbox.run(&["cancel", "w"]).status.success());
    let before = std::fs::read(sandbox.state_root().join("watches/w.json")).unwrap();
    let after_cancel = sandbox.run(&["create", &change_watch_json("w", &input, 1)]);
    assert_eq!(after_cancel.status.code(), Some(1), "a finished id cannot be reused either");
    assert_eq!(std::fs::read(sandbox.state_root().join("watches/w.json")).unwrap(), before);
    assert!(sandbox.run(&["create", &change_watch_json("other", &input, 1)]).status.success());
}

#[test]
fn a_watch_created_in_one_directory_is_resumed_from_another_with_the_same_meaning() {
    // why: V11 (S11) through real processes: `create` runs with cwd = cwd/
    //      and a RELATIVE path; the stored watch has the absolute path; then
    //      `keepalive` runs from cwd2/, which holds a decoy file of the same
    //      relative name. Only the file in cwd/ is appended to; the envelope
    //      must report that line and never the decoy.
    // proves: CLAIM(creator-cwd)
    let sandbox = Sandbox::new();
    std::fs::write(sandbox.cwd().join("rel.log"), "boot\n").unwrap();
    std::fs::write(sandbox.cwd2().join("rel.log"), "DECOY-boot\n").unwrap();
    let request = serde_json::json!({"id": "w", "inputs": [{"kind": "file", "path": "rel.log"}], "interval_s": 1, "fire": "change"}).to_string();
    let created = sandbox.command_in(&sandbox.cwd()).args(["create", &request]).output().unwrap();
    assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
    let stored_path = sandbox.watch_file("w").unwrap()["inputs"][0]["path"].as_str().unwrap().to_string();
    assert_eq!(std::path::Path::new(&stored_path), sandbox.cwd().join("rel.log"), "stored absolute");

    append_once(&sandbox.cwd().join("rel.log"), "real-line");
    append_once(&sandbox.cwd2().join("rel.log"), "DECOY-line");
    let output = finish_within(piped({
        let mut c = sandbox.command_in(&sandbox.cwd2());
        c.args(["keepalive", "w", "--max-s", "30"]);
        c
    }), Duration::from_secs(5));
    let envelope = stdout_json(&output);
    assert_eq!(envelope["status"], "fired");
    assert!(envelope["digest"].as_str().unwrap().contains("real-line"));
    assert!(!envelope["digest"].as_str().unwrap().contains("DECOY"));
}

// ---- S25: real processes and the global lock ----

fn wait_for_exec(child: &std::process::Child) {
    // The child is past exec once /proc/<pid>/exe points at the vigil binary.
    let exe = std::fs::canonicalize(vigil_bin()).unwrap();
    assert!(
        wait_until(Duration::from_secs(3), || std::fs::read_link(format!("/proc/{}/exe", child.id())).is_ok_and(|p| p == exe)),
        "child {} never exec'd vigil",
        child.id()
    );
}

#[test]
fn two_real_processes_on_one_watch_exclude_each_other_through_the_global_lock() {
    // why: S25: the old concurrency test used two objects in ONE process, so a
    //      process-local lock would pass. Here two real `vigil keepalive`
    //      children race for one watch that fires on its first tick while the
    //      test holds the global lock (the same flock any vigil process takes,
    //      via the library's Store). Barrier: both children have exec'd; they
    //      are then held for max(500 ms, 4x the control run) and must still be
    //      running with nothing written. CONTROL (same scenario, lock free):
    //      one child finishes and defines how long "an uncontended run" takes,
    //      so "still running after the hold" cannot just mean "slow".
    //      Release: both exit 0 with the SAME fired result, and the file shows
    //      exactly ONE tick and ONE event (the loser saw the winner's result,
    //      V3, instead of ticking again).
    // proves: CLAIM(tick-lock)
    let spawn_keepalive = |sandbox: &Sandbox| {
        let child = piped({
            let mut c = sandbox.command();
            c.args(["keepalive", "w", "--max-s", "30"]);
            c
        });
        wait_for_exec(&child);
        child
    };
    let prepare = || {
        let sandbox = Sandbox::new();
        let input = sandbox.input("a.log");
        create_via_cli(&sandbox, "w", &input);
        append_once(&input, "ERROR: race");
        sandbox
    };

    let control = prepare();
    let started = std::time::Instant::now();
    let control_out = finish_within(spawn_keepalive(&control), Duration::from_secs(5));
    let uncontended = started.elapsed();
    assert_eq!(stdout_json(&control_out)["status"], "fired", "control run");

    let sandbox = prepare();
    let store = vigil::Store::new(sandbox.state_root());
    let guard = store.lock().unwrap();
    let before = std::fs::read(sandbox.state_root().join("watches/w.json")).unwrap();
    let mut children = [spawn_keepalive(&sandbox), spawn_keepalive(&sandbox)];
    let hold = std::cmp::max(Duration::from_millis(500), uncontended * 4);
    std::thread::sleep(hold);
    for child in children.iter_mut() {
        assert!(child.try_wait().unwrap().is_none(), "a child finished while the global lock was held");
    }
    assert_eq!(std::fs::read(sandbox.state_root().join("watches/w.json")).unwrap(), before, "nothing written under contention");
    drop(guard);

    let [first, second] = children;
    let outputs = [finish_within(first, Duration::from_secs(5)), finish_within(second, Duration::from_secs(5))];
    let envelopes: Vec<serde_json::Value> = outputs.iter().map(|o| {
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        stdout_json(o)
    }).collect();
    assert_eq!(envelopes[0]["status"], "fired");
    assert_eq!(envelopes[0]["events"], envelopes[1]["events"], "both processes return the same stored result");
    assert_eq!(envelopes[0]["events"][0]["event_id"], "w:1");
    let file = sandbox.watch_file("w").unwrap();
    assert_eq!(file["tick"], 1, "exactly one tick");
    assert_eq!(file["events"].as_array().unwrap().len(), 1, "exactly one event");
}

#[test]
fn cancel_waits_for_the_lock_in_a_real_process_and_both_cancels_agree() {
    // why: S25 + V4: cancel is a mutation, so it holds the same global lock.
    //      Two real `vigil cancel` children are held behind the test's lock
    //      (still running, file untouched, barrier = both exec'd, hold 600 ms;
    //      a cancel with the lock free takes far less, see the other test's
    //      control). After release both succeed with status cancelled and the
    //      file has one finished_at (immutable terminal state).
    // proves: CLAIM(terminal-immutable)
    let sandbox = Sandbox::new();
    let input = sandbox.input("a.log");
    create_via_cli(&sandbox, "w", &input);
    let store = vigil::Store::new(sandbox.state_root());
    let guard = store.lock().unwrap();
    let mut children: Vec<std::process::Child> = (0..2)
        .map(|_| {
            let child = piped({
                let mut c = sandbox.command();
                c.args(["cancel", "w"]);
                c
            });
            wait_for_exec(&child);
            child
        })
        .collect();
    std::thread::sleep(Duration::from_millis(600));
    for child in children.iter_mut() {
        assert!(child.try_wait().unwrap().is_none(), "cancel finished while the lock was held");
    }
    assert_eq!(sandbox.watch_file("w").unwrap()["status"], "pending");
    drop(guard);
    for child in children {
        let output = finish_within(child, Duration::from_secs(5));
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(stdout_json(&output)["status"], "cancelled");
    }
    let file = sandbox.watch_file("w").unwrap();
    assert_eq!(file["status"], "cancelled");
    assert!(file["finished_at"].as_str().is_some());
}

// ---- vigil wait ----

#[test]
fn wait_exits_0_with_the_fired_envelope_on_stdout() {
    // why: coordinator scope: `vigil wait <id>` blocks until the watch fires,
    //      prints the final envelope JSON, exits 0. E2E through the real
    //      binary and watch files: create via CLI, one write, wait.
    // proves: CLAIM(wait-exit-codes)
    let sandbox = Sandbox::new();
    let input = sandbox.input("a.log");
    create_via_cli(&sandbox, "w", &input);
    append_once(&input, "wait-line");
    let output = finish_within(piped({
        let mut c = sandbox.command();
        c.args(["wait", "w"]);
        c
    }), Duration::from_secs(5));
    assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
    let envelope = stdout_json(&output);
    assert_eq!(envelope["status"], "fired");
    assert!(envelope["digest"].as_str().unwrap().contains("wait-line"));
}

#[test]
fn wait_prints_one_final_envelope_with_the_whole_event_batch() {
    // why: S21/V21/R8: `wait` does not stream. Two inputs change before the
    //      first tick; stdout must be exactly ONE JSON document (one line)
    //      whose `events` holds both events, in input order, and exit 0.
    // proves: CLAIM(wait-final-batch)
    let sandbox = Sandbox::new();
    let (first, second) = (sandbox.input("one.log"), sandbox.input("two.log"));
    std::fs::write(&first, "a\n").unwrap();
    std::fs::write(&second, "b\n").unwrap();
    let request = serde_json::json!({"id": "w", "inputs": [{"kind": "file", "path": first}, {"kind": "file", "path": second}],
        "interval_s": 1, "fire": "change"}).to_string();
    assert!(sandbox.run(&["create", &request]).status.success());
    append_once(&first, "first-new");
    append_once(&second, "second-new");
    let output = finish_within(piped({
        let mut c = sandbox.command();
        c.args(["wait", "w"]);
        c
    }), Duration::from_secs(5));
    assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(text.lines().count(), 1, "one final envelope, nothing streamed: {text:?}");
    let envelope: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    let events = envelope["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    assert!(events[0]["summary"].as_str().unwrap().contains("first-new"));
    assert!(events[1]["summary"].as_str().unwrap().contains("second-new"));
    assert_ne!(events[0]["event_id"], events[1]["event_id"]);
}

#[test]
fn wait_has_no_stream_flag() {
    // why: S21/V21: `--stream` is removed (a watch fires once under D1, so a
    //      stream could only ever print one event). The flag must be refused
    //      and must not print an envelope. NOTE: clap's usage-error exit code
    //      is 2, the same number as "expired" (flagged in .red-report.md), so
    //      only "not success, nothing on stdout" is asserted. This passes in
    //      the red phase because clap already rejects the flag: deliberate
    //      control.
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["wait", "w", "--stream"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "{:?}", String::from_utf8_lossy(&output.stdout));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--stream"));
}

#[test]
fn wait_exits_2_when_the_watch_expires() {
    // why: coordinator scope: exit 2 = expired. deadline_s 1 / interval 1.
    //      V20: the envelope carries the terminal `next`.
    let sandbox = Sandbox::new();
    let input = sandbox.input("a.log");
    std::fs::write(&input, "boot\n").unwrap();
    let request = serde_json::json!({"id": "short", "inputs": [{"kind": "file", "path": input}],
        "interval_s": 1, "deadline_s": 1, "fire": "change"})
    .to_string();
    assert!(sandbox.run(&["create", &request]).status.success());
    let child = piped({
        let mut c = sandbox.command();
        c.args(["wait", "short"]);
        c
    });
    let output = finish_within(child, Duration::from_secs(5));
    assert_eq!(output.status.code(), Some(2));
    let envelope = stdout_json(&output);
    assert_eq!(envelope["status"], "expired");
    assert_eq!(envelope["next"], "watch ended; create a new watch if still needed");
}

#[test]
fn wait_exits_3_when_the_watch_is_cancelled() {
    // why: coordinator scope: exit 3 = cancelled; another process cancels the
    //      watch while `wait` is blocked and wait notices on its next tick.
    let sandbox = Sandbox::new();
    let input = sandbox.input("a.log");
    create_via_cli(&sandbox, "w", &input);
    let child = piped({
        let mut c = sandbox.command();
        c.args(["wait", "w"]);
        c
    });
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
