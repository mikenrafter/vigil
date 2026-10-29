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

fn append_once_raw(path: &std::path::Path, bytes: &str) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes.as_bytes()).unwrap();
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
    //      repository. All files live under $XDG_STATE_HOME/vigil/." S24 (still
    //      open after round 1): the exemption for the input we append to
    //      allowed ANY appended suffix, so an unintended append by vigil
    //      itself would pass. X5 closes it: exactly state-home/vigil is
    //      excluded, and the bytes of EVERYTHING else (HOME, cwd,
    //      XDG_CONFIG_HOME, sibling dirs in state-home, the inputs) must be
    //      identical, except the one input file, whose final content must be
    //      exactly "boot\nline\n" (what we wrote plus the one line we
    //      appended, byte for byte). Positive controls: the state root gained
    //      the global lock, the watch's lock file and the watch file; the diff
    //      helper flags a stray file in state-home; and it flags ONE extra byte
    //      appended to the input (the exact thing the old exemption allowed).
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

    let expected: std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> =
        [(input.clone(), b"boot\nline\n".to_vec())].into_iter().collect();
    let after = sandbox.snapshot_outside_state_root();
    assert_eq!(snapshot_diff(&before, &after, &expected), Vec::<String>::new());

    let root = sandbox.state_root();
    assert!(root.join("lock").is_file(), "control: the global lock file exists");
    assert!(root.join("locks").join("w.lock").is_file(), "control: the watch's lock file exists");
    assert!(root.join("watches").join("w.json").is_file(), "control: the watch file exists");
    let mut under_root: Vec<String> = list_files(&root).into_iter().map(|(p, _)| p.strip_prefix(&root).unwrap().to_string_lossy().to_string()).collect();
    under_root.sort();
    assert_eq!(under_root, vec!["lock", "locks/w.lock", "watches/w.json"], "only the lock files and the json file, no .tmp");

    append_once_raw(&input, "!");
    let with_extra_byte = sandbox.snapshot_outside_state_root();
    assert_eq!(snapshot_diff(&before, &with_extra_byte, &expected).len(), 1, "control: one unexpected extra byte in the input is detected");
    std::fs::write(&input, "boot\nline\n").unwrap();
    std::fs::write(sandbox.state_home().join("stray.txt"), "x").unwrap();
    let with_stray = sandbox.snapshot_outside_state_root();
    assert_eq!(snapshot_diff(&before, &with_stray, &expected).len(), 1, "control: a stray file in state-home itself is detected");
}

#[tokio::test]
async fn any_run_prunes_finished_watches_older_than_seven_days() {
    // why: D4 / SPEC "State": "deleted 7 days after they finish, by any
    //      vigil run". Old watch files are made with the library and a manual
    //      clock set in the past; then the real binary runs `list` and the
    //      stale file must vanish while a recent finished one stays. V16: an
    //      abandoned pending watch (deadline passed > 7 days ago) goes too,
    //      and the global lock file stays. X3: a pruned watch's
    //      `locks/<id>.lock` is deleted with it, a surviving watch keeps its
    //      own.
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
    assert!(sandbox.state_root().join("locks").join("stale.lock").exists(), "precondition: create took the watch lock");

    assert!(sandbox.run(&["list"]).status.success());
    assert!(sandbox.watch_file("stale").is_none(), "stale finished watch should be pruned");
    assert!(sandbox.watch_file("abandoned").is_none(), "V16: abandoned pending watch should be pruned");
    assert!(sandbox.watch_file("fresh").is_some());
    assert!(sandbox.state_root().join("lock").exists(), "V5: the global lock file is never deleted");
    assert!(!sandbox.state_root().join("locks").join("stale.lock").exists(), "X3: a pruned watch's lock file goes with it");
    assert!(!sandbox.state_root().join("locks").join("abandoned.lock").exists());
    assert!(sandbox.state_root().join("locks").join("fresh.lock").exists(), "X3: a surviving watch keeps its lock file");
}

#[test]
fn keepalive_subcommand_returns_the_fired_envelope() {
    // why: SPEC "Architecture": `vigil keepalive` blocks in the shell the
    //      same way the MCP tool does. V1: ONE line written after create (no
    //      repeating appender) makes it return a fired envelope on its first
    //      tick, with the clamped ceiling (V26: max_s 30), the stable event_id
    //      (V3, W1: `w:1:0`) and the watch id (V8).
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
    assert_eq!(envelope["events"][0]["event_id"], "w:1:0");
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

// ---- S25 + X3 + X5: real processes and the locks ----

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
fn two_real_processes_on_one_watch_exclude_each_other_through_the_watch_lock() {
    // why: S25 + X3: the old concurrency test used two objects in ONE process,
    //      so a process-local lock would pass. Here two real `vigil keepalive`
    //      children race for one watch that fires on its first tick while the
    //      test holds THAT WATCH'S lock (`locks/w.lock`, the same flock any
    //      vigil process takes, via the library's Store; X3 replaced the
    //      single global lock). Barrier: both children have exec'd; they are
    //      then held for max(500 ms, 4x the control run) and must still be
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
    let guard = store.lock_watch(&vigil::WatchId::parse("w").unwrap()).unwrap();
    let before = std::fs::read(sandbox.state_root().join("watches/w.json")).unwrap();
    let mut children = [spawn_keepalive(&sandbox), spawn_keepalive(&sandbox)];
    let hold = std::cmp::max(Duration::from_millis(500), uncontended * 4);
    std::thread::sleep(hold);
    for child in children.iter_mut() {
        assert!(child.try_wait().unwrap().is_none(), "a child finished while the watch lock was held");
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
    assert_eq!(envelopes[0]["events"][0]["event_id"], "w:1:0");
    let file = sandbox.watch_file("w").unwrap();
    assert_eq!(file["tick"], 1, "exactly one tick");
    assert_eq!(file["events"].as_array().unwrap().len(), 1, "exactly one event");
}

#[test]
fn two_real_processes_evaluate_a_due_tick_once_counted_by_a_cmd_input_that_appends_to_a_file() {
    // why: S25 (still open after round 1) + X5: the lock test above proves
    //      mutual exclusion but not that the work was done once: two unlocked
    //      processes could both evaluate tick 1 and write identical results.
    //      The count must come from the world, not from the watch file. The
    //      watch's only input is a command that appends a line to a counter
    //      file, sleeps 1 s (so two processes that both evaluate overlap for
    //      certain) and prints a state file. Probe control: the create-time
    //      baseline ran the command once, so the counter shows 1 line; it is
    //      then emptied. After the state file changes, two real `vigil
    //      keepalive` children are started back to back: BOTH exit 0 with the
    //      fired result, the counter holds EXACTLY ONE line (one evaluation:
    //      the loser waited for the watch lock and replayed the winner's
    //      result) and everything takes under 5 s.
    // proves: CLAIM(tick-lock)
    let sandbox = Sandbox::new();
    let (counter, state) = (sandbox.input("counter"), sandbox.input("state"));
    std::fs::write(&state, "v1\n").unwrap();
    let script = format!("echo x >> '{}'; sleep 1; cat '{}'", counter.display(), state.display());
    let request = serde_json::json!({"id": "w", "inputs": [{"kind": "cmd", "argv": ["sh", "-c", script], "timeout_s": 10}],
        "interval_s": 1, "deadline_s": 3600, "fire": "change"})
    .to_string();
    assert!(sandbox.run(&["create", &request]).status.success());
    let lines = |path: &std::path::Path| std::fs::read_to_string(path).unwrap_or_default().lines().count();
    assert_eq!(lines(&counter), 1, "probe control: the create-time baseline evaluated the command once");
    std::fs::write(&counter, "").unwrap();
    std::fs::write(&state, "v2\n").unwrap();

    let started = std::time::Instant::now();
    let spawn = || {
        let child = piped({
            let mut c = sandbox.command();
            c.args(["keepalive", "w", "--max-s", "30"]);
            c
        });
        wait_for_exec(&child);
        child
    };
    let (first, second) = (spawn(), spawn());
    let outputs = [finish_within(first, Duration::from_secs(8)), finish_within(second, Duration::from_secs(8))];
    let elapsed = started.elapsed();
    for output in &outputs {
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(stdout_json(output)["status"], "fired");
    }
    assert_eq!(stdout_json(&outputs[0])["events"], stdout_json(&outputs[1])["events"]);
    assert_eq!(lines(&counter), 1, "the due tick was evaluated {} times by two real processes", lines(&counter));
    assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
    assert_eq!(sandbox.watch_file("w").unwrap()["tick"], 1);
}

#[test]
fn a_slow_input_in_one_process_does_not_block_another_process_ticking_a_different_watch() {
    // why: X3 (T4) through real processes: process A is sensing watch "slow"
    //      (a command that starts, records that it started, and sleeps 3 s
    //      before printing the state file) while process B runs `keepalive` on
    //      watch "fast". B must finish (fired) in under 2 s while A's child is
    //      still running: under one global lock held across sensing B would
    //      wait the full 3 s. Barrier: the command's own "started" marker, so
    //      "A is mid-sensing" is observed, not slept for. A then completes and
    //      fires on its own change (state v1 -> v2).
    // proves: CLAIM(per-watch-lock)
    let sandbox = Sandbox::new();
    let (go, started_marker, state) = (sandbox.input("go"), sandbox.input("started"), sandbox.input("state"));
    std::fs::write(&state, "v1\n").unwrap();
    let script = format!(
        "if [ -e '{}' ]; then echo up > '{}'; sleep 3; fi; cat '{}'",
        go.display(), started_marker.display(), state.display()
    );
    let slow = serde_json::json!({"id": "slow", "inputs": [{"kind": "cmd", "argv": ["sh", "-c", script], "timeout_s": 10}],
        "interval_s": 1, "deadline_s": 3600, "fire": "change"})
    .to_string();
    assert!(sandbox.run(&["create", &slow]).status.success());
    let fast_input = sandbox.input("fast.log");
    create_via_cli(&sandbox, "fast", &fast_input);
    append_once(&fast_input, "fast-change");
    std::fs::write(&state, "v2\n").unwrap();
    std::fs::write(&go, "").unwrap();

    let mut slow_child = piped({
        let mut c = sandbox.command();
        c.args(["keepalive", "slow", "--max-s", "30"]);
        c
    });
    assert!(wait_until(Duration::from_secs(5), || started_marker.exists()), "the slow watch never started sensing");
    let began = std::time::Instant::now();
    let fast = sandbox.run(&["keepalive", "fast", "--max-s", "30"]);
    let took = began.elapsed();
    assert!(fast.status.success(), "{}", String::from_utf8_lossy(&fast.stderr));
    assert_eq!(stdout_json(&fast)["status"], "fired");
    assert!(took < Duration::from_secs(2), "the fast watch waited {took:?} behind the slow one");
    assert!(slow_child.try_wait().unwrap().is_none(), "the slow watch must still be sensing when the fast one is done");

    let output = finish_within(slow_child, Duration::from_secs(10));
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(stdout_json(&output)["status"], "fired");
}

#[test]
fn holding_the_global_lock_does_not_block_a_tick_of_an_existing_watch_in_a_real_process() {
    // why: X3: the global lock is for namespace operations only (id
    //      reservation, prune). A real `vigil keepalive` on an existing watch
    //      must complete while another process holds it. Control: the same
    //      call while the WATCH's own lock is held does not complete in the
    //      same window (see the two-process test above for the full proof).
    // proves: CLAIM(per-watch-lock)
    let sandbox = Sandbox::new();
    let input = sandbox.input("a.log");
    create_via_cli(&sandbox, "w", &input);
    append_once(&input, "ERROR: while the global lock is held");
    let store = vigil::Store::new(sandbox.state_root());
    let _global = store.lock().unwrap();
    let output = finish_within(piped({
        let mut c = sandbox.command();
        c.args(["keepalive", "w", "--max-s", "30"]);
        c
    }), Duration::from_secs(5));
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(stdout_json(&output)["status"], "fired");
}

#[test]
fn cancel_waits_for_the_watch_lock_in_a_real_process_and_both_cancels_agree() {
    // why: S25 + V4 + X3: cancel is a mutation, so it holds the watch's lock.
    //      Two real `vigil cancel` children are held behind the test's hold of
    //      `locks/w.lock` (still running, file untouched, barrier = both
    //      exec'd, hold 600 ms; a cancel with the lock free takes far less,
    //      see the other test's control). After release both succeed with
    //      status cancelled and the file has one finished_at (immutable
    //      terminal state).
    // proves: CLAIM(terminal-immutable)
    let sandbox = Sandbox::new();
    let input = sandbox.input("a.log");
    create_via_cli(&sandbox, "w", &input);
    let store = vigil::Store::new(sandbox.state_root());
    let guard = store.lock_watch(&vigil::WatchId::parse("w").unwrap()).unwrap();
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
        assert!(child.try_wait().unwrap().is_none(), "cancel finished while the watch lock was held");
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

// ---- W2/W3 (T6): the floors through the binary ----

#[test]
fn a_below_floor_digest_env_value_is_raised_to_64_by_the_binary() {
    // why: W3 + T6 end to end: the environment is read at the binary's edge.
    //      VIGIL_DIGEST_MAX_BYTES=10 must act as 64: the fired envelope's
    //      digest for a 5 KB change is between 41 and 64 bytes, ends with the
    //      `[digest cut]` marker and keeps the newest line (unclamped it would
    //      be at most 10 bytes: no room for the marker). 200 is above the floor
    //      and is taken as given: (150, 200] bytes.
    // proves: CLAIM(config-floors)
    for (raw, floor, ceiling) in [("10", 40usize, 64usize), ("200", 150, 200)] {
        let sandbox = Sandbox::new();
        let input = sandbox.input("a.log");
        create_via_cli(&sandbox, "w", &input);
        let noise: String = (0..300).map(|i| format!("noise-line-{i:04}\n")).collect();
        append_once_raw(&input, &format!("{noise}LAST-LINE\n"));
        let output = finish_within(piped({
            let mut c = sandbox.command();
            c.env("VIGIL_DIGEST_MAX_BYTES", raw).args(["keepalive", "w", "--max-s", "30"]);
            c
        }), Duration::from_secs(5));
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let envelope = stdout_json(&output);
        let digest = envelope["digest"].as_str().unwrap();
        assert!(digest.len() <= ceiling && digest.len() > floor, "VIGIL_DIGEST_MAX_BYTES={raw}: {} bytes: {digest:?}", digest.len());
        assert!(digest.ends_with("[digest cut]") && digest.contains("LAST-LINE"), "{digest:?}");
    }
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
    assert_eq!((events[0]["event_id"].as_str(), events[1]["event_id"].as_str()), (Some("w:1:0"), Some("w:1:1")), "W1: ids count from 0 within the tick");
}

#[test]
fn wait_has_no_stream_flag() {
    // why: S21/V21: `--stream` is removed (a watch fires once under D1, so a
    //      stream could only ever print one event). The flag must be refused
    //      and must not print an envelope. W6 (T2): clap's usage-error exit
    //      code 2 no longer collides with "expired" (now 3), so the exit code
    //      is asserted to be exactly 2. This passes in the red phase because
    //      clap already rejects the flag: deliberate control.
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["wait", "w", "--stream"]);
    assert_eq!(output.status.code(), Some(2), "a usage error is exit 2");
    assert!(output.stdout.is_empty(), "{:?}", String::from_utf8_lossy(&output.stdout));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--stream"));
}

#[test]
fn wait_exits_3_when_the_watch_expires() {
    // why: W6 (T2): exit 3 = expired (was 2, which is clap's usage-error
    //      code). deadline_s 1 / interval 1. V20: the envelope carries the
    //      terminal `next`.
    // proves: CLAIM(wait-exit-codes)
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
    assert_eq!(output.status.code(), Some(3));
    let envelope = stdout_json(&output);
    assert_eq!(envelope["status"], "expired");
    assert_eq!(envelope["next"], "watch ended; create a new watch if still needed");
}

#[test]
fn wait_exits_4_when_the_watch_is_cancelled() {
    // why: W6 (T2): exit 4 = cancelled (was 3); another process cancels the
    //      watch while `wait` is blocked and wait notices on its next tick.
    // proves: CLAIM(wait-exit-codes)
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
    assert_eq!(output.status.code(), Some(4));
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

#[test]
fn wait_help_lists_every_exit_code() {
    // why: W6 (T2): the exit codes are a contract for scripts and agents, so
    //      `vigil wait --help` must state all five, with the new numbers:
    //      0 fired, 1 error, 2 usage, 3 expired, 4 cancelled (whitespace is
    //      normalized, so clap's line handling does not matter). The text
    //      lives in the clap doc comment of `Command::Wait`, which the red
    //      stub already carries, so this passes in the red phase: deliberate
    //      control (it fails the moment the doc and the codes drift apart).
    // proves: CLAIM(wait-exit-codes)
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["wait", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    let help = String::from_utf8_lossy(&output.stdout).split_whitespace().collect::<Vec<_>>().join(" ");
    for line in ["0 fired", "1 error", "2 usage", "3 expired", "4 cancelled"] {
        assert!(help.contains(line), "help is missing {line:?}: {help}");
    }
    assert!(!help.contains("2 expired") && !help.contains("3 cancelled"), "stale round-2 codes in the help: {help}");
}
