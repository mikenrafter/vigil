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
    // DF4 (specs/disk-full.md): the reserve is the one extra file, exactly
    // 256 KiB when present (its presence is pinned by the disk-full tests).
    if let Some(position) = under_root.iter().position(|name| name == ".reserve") {
        under_root.remove(position);
        assert_eq!(std::fs::metadata(root.join(".reserve")).unwrap().len(), 262_144);
    }
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

// ---- code review round 1: C9 (signals), C10 (prune never blocks) ----

/// Start `vigil <args>` on a watch whose next tick hangs in a long-running
/// child, signal vigil, and check it exits with `code` and takes the child's
/// whole process group with it.
fn signal_stops_vigil_and_its_command_group(args: &[&str], signal: &str, code: i32) {
    let sandbox = Sandbox::new();
    let (go, pid_file) = (sandbox.input("go"), sandbox.input("cmd.pid"));
    let created = sandbox.run(&["create", &hanging_cmd_watch_json("w", &go, &pid_file)]);
    assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
    std::fs::write(&go, "").unwrap();

    let child = piped({
        let mut command = sandbox.command();
        command.args(args);
        command
    });
    let vigil_pid = child.id();
    let cmd_pid = wait_for_pid(&pid_file, Duration::from_secs(5));
    assert!(!proc_is_dead(cmd_pid), "precondition: the command is running");
    send_signal(signal, vigil_pid);

    let output = finish_within(child, Duration::from_secs(2));
    assert_eq!(output.status.code(), Some(code), "exit code after {signal}: {:?}", output.status);
    assert!(
        wait_until(Duration::from_secs(2), || proc_is_dead(cmd_pid)),
        "the command's process group outlived vigil after {signal}"
    );
}

#[test]
fn sigterm_to_a_waiting_vigil_exits_143_and_kills_the_command_it_was_running() {
    // why: code review C9: SIGTERM ended vigil without dropping its futures,
    //      and command inputs run in their own session, so a stopped
    //      background `vigil wait` left its command tree running with its
    //      timeout gone. The binary now handles the signal: cancel active
    //      work, let the command cleanup run, exit 143.
    // proves: CLAIM(signal-cleanup)
    signal_stops_vigil_and_its_command_group(&["wait", "w"], "-TERM", 143);
}

#[test]
fn sigint_to_a_blocked_keepalive_exits_130_and_kills_the_command_it_was_running() {
    // why: code review C9, same for Ctrl-C on `vigil keepalive`: exit 130.
    // proves: CLAIM(signal-cleanup)
    signal_stops_vigil_and_its_command_group(&["keepalive", "w", "--max-s", "30"], "-INT", 130);
}

#[tokio::test]
async fn a_busy_global_lock_never_stops_housekeeping_from_letting_a_command_finish() {
    // why: code review C10: the binary prunes before every command, and
    //      prune waited on the global lock without limit, so one stale watch
    //      plus a held global lock hung an otherwise instant `keepalive` on
    //      an already finished watch. Prune now only try-locks the global
    //      lock and skips what it cannot take. A stale finished watch (made
    //      10 days ago with a manual clock) is the prune candidate, a fresh
    //      cancelled one the watch asked about; the test holds the global
    //      lock; the keepalive must exit 0 well inside the limit.
    //      Control: the stale watch is still there afterwards (skipped, not
    //      deleted), and is pruned once the lock is free.
    // proves: CLAIM(prune-never-blocks)
    use std::sync::Arc;
    use vigil::testing::{change_request, default_config, ManualClock, ScriptedJudge};
    let sandbox = Sandbox::new();
    let now = vigil::Timestamp(chrono::Utc::now());
    let clock = Arc::new(ManualClock::new(now.minus_secs(10 * 24 * 3600)));
    let library = vigil::Vigil::new(sandbox.state_root(), clock.clone(), Arc::new(ScriptedJudge::new()), default_config());
    let input = sandbox.input("a.log");
    library.create(change_request("stale", &input, 20)).await.unwrap();
    library.cancel(&vigil::WatchId::parse("stale").unwrap()).await.unwrap();
    clock.set(now);
    library.create(change_request("done", &input, 20)).await.unwrap();
    library.cancel(&vigil::WatchId::parse("done").unwrap()).await.unwrap();

    let store = vigil::Store::new(sandbox.state_root());
    let global = store.lock().unwrap();
    let child = piped({
        let mut command = sandbox.command();
        command.args(["keepalive", "done", "--max-s", "30"]);
        command
    });
    let output = finish_within(child, Duration::from_secs(5));
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(stdout_json(&output)["status"], "cancelled");
    assert!(sandbox.watch_file("stale").is_some(), "control: prune skipped the candidate it could not lock");
    drop(global);

    assert!(sandbox.run(&["list"]).status.success());
    assert!(sandbox.watch_file("stale").is_none(), "control: pruned once the lock is free");
}

// ---- code review round 2: C14 ----

#[test]
fn sigterm_to_vigil_create_waiting_on_stdin_exits_143() {
    // why: code review C14: `vigil create` read stdin with a blocking read on
    //      the task that polls the signal branches, so SIGINT/SIGTERM were
    //      ignored until stdin closed. Stdin is held open and silent; the
    //      signal must end the process with 143 inside 2 s.
    // proves: CLAIM(signal-cleanup)
    let sandbox = Sandbox::new();
    let mut child = piped({
        let mut command = sandbox.command();
        command.arg("create").stdin(Stdio::piped());
        command
    });
    wait_for_exec(&child);
    // Signal first only once the handler is installed (SIGTERM = bit 14 of
    // SigCgt); before that the default action kills the process.
    let handled = || {
        std::fs::read_to_string(format!("/proc/{}/status", child.id())).is_ok_and(|status| {
            status.lines().find_map(|line| line.strip_prefix("SigCgt:")).and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok()).is_some_and(|mask| mask & (1 << 14) != 0)
        })
    };
    assert!(wait_until(Duration::from_secs(3), handled), "vigil never installed its SIGTERM handler");
    // ... and only once some thread is parked in a pipe read (stdin): a
    // signal that lands earlier is seen by the runtime before the read starts.
    let reading_stdin = || {
        std::fs::read_dir(format!("/proc/{}/task", child.id())).is_ok_and(|tasks| {
            tasks.flatten().any(|task| std::fs::read_to_string(task.path().join("wchan")).is_ok_and(|wchan| wchan.trim() == "anon_pipe_read"))
        })
    };
    assert!(wait_until(Duration::from_secs(3), reading_stdin), "vigil never blocked reading stdin");
    let held_open = child.stdin.take().unwrap();
    send_signal("-TERM", child.id());
    let output = finish_within(child, Duration::from_secs(2));
    drop(held_open);
    assert_eq!(output.status.code(), Some(143), "{:?}", output.status);
}

// ---- surviving a full disk (specs/disk-full.md DF1-DF6) -----------------
//
// The disk cannot really be filled from a test, so the spawned binary is told
// to fail its own store writes through `VIGIL_TEST_FAULT` (src/fault.rs;
// `Sandbox::faulted_command`). Only the command that gets the variable sees a
// full disk, so "create healthy, then keepalive on a full disk, then keepalive
// healthy" is three processes, as in life. Every run is bounded by
// `output_within`.

const RUN: Duration = Duration::from_secs(20);

/// A pending `fire = change` watch "w" whose input has changed since create:
/// the first tick fires. Returns the input path.
fn change_ready(sandbox: &Sandbox) -> std::path::PathBuf {
    let input = sandbox.input("w.log");
    create_via_cli(sandbox, "w", &input);
    append_once(&input, "boom");
    input
}

fn run_with(sandbox: &Sandbox, plan: Option<&str>, args: &[&str]) -> std::process::Output {
    let mut command = match plan {
        Some(plan) => sandbox.faulted_command(plan),
        None => sandbox.command(),
    };
    command.args(args);
    output_within(command, RUN)
}

#[test]
fn keepalive_on_a_full_disk_prints_the_envelope_exits_0_and_names_the_state_root_once_on_stderr() {
    // why: DF1/DF2, the incident through the real binary: `vigil keepalive`
    //      used to exit 1 with "io error: No space left on device" and print
    //      nothing. Now: exit 0, the full fired envelope on stdout (tick 1,
    //      event "w:1:0", summary "boom"), a persist note with the right kind
    //      per errno (28 -> disk_full, 5 -> io), exactly ONE line on stderr
    //      and that line names the state root. DF3a: the envelope has an
    //      event that was not saved, so `redelivery: true` is present. DF6:
    //      the watch file is byte-identical and no .tmp remains.
    // proves: CLAIM(DF1), CLAIM(DF2), CLAIM(DF3a), CLAIM(DF6)
    //      S4: the disk-full run retried (create left a reserve to free) and
    //      so carries `first_error`; EIO is not retried.
    for (plan, kind, os_text, retried) in [
        (DISK_STAYS_FULL, "disk_full", "No space left on device", true),
        ("watch:5:all", "io", "Input/output error", false),
    ] {
        let sandbox = Sandbox::new();
        change_ready(&sandbox);
        let before = sandbox.watch_bytes("w");

        let output = run_with(&sandbox, Some(plan), &["keepalive", "w"]);

        assert_eq!(output.status.code(), Some(0), "{plan}: stderr {:?}", String::from_utf8_lossy(&output.stderr));
        let envelope = stdout_json(&output);
        assert_eq!(envelope["status"], "fired", "{plan}");
        assert_eq!(envelope["next"], "act on events");
        assert_eq!(envelope["tick"], 1);
        assert_eq!(envelope["events"][0]["event_id"], "w:1:0");
        assert_eq!(envelope["events"][0]["summary"], "boom");
        assert_failed_persist(&envelope, kind, os_text, &sandbox.watch_path("w"), true, Some(retried));
        let lines = stderr_lines(&output);
        assert_eq!(lines.len(), 1, "{plan}: exactly one stderr line, got {lines:?}");
        assert!(lines[0].contains(sandbox.state_root().to_str().unwrap()), "{plan}: the line must name the state root: {:?}", lines[0]);
        assert_eq!(sandbox.watch_bytes("w"), before, "{plan}: the failed save must not touch the file");
        assert!(sandbox.tmp_files().is_empty(), "{plan}: {:?}", sandbox.tmp_files());
    }
}

#[test]
fn the_next_process_on_a_healthy_disk_reports_the_events_the_full_disk_run_could_not_save() {
    // why: DF3 across separate CLI processes: process 1 fails to save and
    //      exits 0; process 2 starts clean, senses the same change from the
    //      unchanged file, and prints the same single event (same event_id and
    //      summary), now persisted: no persist field, empty stderr, the file
    //      says fired. Events are never silently dropped.
    // proves: CLAIM(DF3)
    let sandbox = Sandbox::new();
    change_ready(&sandbox);
    let lost = stdout_json(&run_with(&sandbox, Some(DISK_STAYS_FULL), &["keepalive", "w"]));
    assert_eq!(lost["persist"]["ok"], false, "precondition: the first run could not save: {lost}");

    let output = run_with(&sandbox, None, &["keepalive", "w"]);

    assert_eq!(output.status.code(), Some(0));
    let again = stdout_json(&output);
    assert_eq!(again["status"], "fired");
    assert_eq!(again["events"], lost["events"], "the same events again");
    assert_eq!(again["events"].as_array().unwrap().len(), 1);
    assert!(again.get("persist").is_none(), "{again}");
    assert!(stderr_lines(&output).is_empty(), "{:?}", stderr_lines(&output));
    assert_eq!(sandbox.watch_file("w").unwrap()["status"], "fired");
}

#[test]
fn wait_keeps_its_exit_code_when_the_final_save_fails() {
    // why: DF1: "`wait` keeps its outcome code" (W6: fired 0, expired 3). The
    //      envelope is printed with persist.ok=false and the exit code is the
    //      one the outcome decides, not 1. Fired: the change is sensed on the
    //      first tick. Expired: deadline_s 1, nothing changes. DF1a: the first
    //      tick cannot save and returns a pending envelope, `wait` sleeps one
    //      interval (1 s) and calls again, the watch is overdue and the
    //      expiry write is the one that fails: ONE envelope on stdout, exit 3.
    //      DF3a: both are terminal statuses that were not saved, so both say
    //      `redelivery: true`.
    // proves: CLAIM(DF1), CLAIM(DF1a), CLAIM(DF2), CLAIM(DF3a)
    let fired = Sandbox::new();
    change_ready(&fired);
    let output = run_with(&fired, Some(DISK_STAYS_FULL), &["wait", "w"]);
    assert_eq!(output.status.code(), Some(0), "stderr {:?}", String::from_utf8_lossy(&output.stderr));
    let envelope = stdout_json(&output);
    assert_eq!(envelope["status"], "fired");
    assert_failed_persist(&envelope, "disk_full", "No space left on device", &fired.watch_path("w"), true, Some(true));

    let expired = Sandbox::new();
    let input = expired.input("e.log");
    std::fs::write(&input, "boot\n").unwrap();
    let short = serde_json::json!({"id": "e", "prompt": "", "inputs": [{"kind": "file", "path": input}], "interval_s": 1, "deadline_s": 1, "fire": "change"});
    assert!(expired.run(&["create", &short.to_string()]).status.success());
    let output = run_with(&expired, Some(DISK_STAYS_FULL), &["wait", "e"]);
    assert_eq!(output.status.code(), Some(3), "stderr {:?}", String::from_utf8_lossy(&output.stderr));
    let envelope = stdout_json(&output);
    assert_eq!(envelope["status"], "expired");
    assert_failed_persist(&envelope, "disk_full", "No space left on device", &expired.watch_path("e"), true, None);
}

#[test]
fn the_reserve_exists_after_a_healthy_run_and_saves_the_tick_when_the_disk_is_full() {
    // why: DF4 end to end. A healthy `create` leaves `<root>/.reserve` of
    //      256 KiB (262144 bytes). Then a keepalive on a disk that is full
    //      except for that reserve: the reserve is deleted, the write is
    //      retried and lands (the file on disk says fired), the process exits
    //      0, and the envelope's persist is exactly
    //      {"ok":true,"used_reserve":true,"kind":"disk_full"}.
    // proves: CLAIM(DF4)
    let sandbox = Sandbox::new();
    change_ready(&sandbox);
    let reserve = std::fs::metadata(sandbox.reserve()).unwrap_or_else(|error| panic!("create left no reserve at {}: {error}", sandbox.reserve().display()));
    assert_eq!(reserve.len(), 262_144, "256 KiB = 256 * 1024");

    let output = run_with(&sandbox, Some(&sandbox.only_the_reserve_to_give()), &["keepalive", "w"]);

    assert_eq!(output.status.code(), Some(0), "stderr {:?}", String::from_utf8_lossy(&output.stderr));
    let envelope = stdout_json(&output);
    assert_eq!(envelope["status"], "fired");
    assert_eq!(envelope["persist"], common::reserve_used_note());
    assert_eq!(sandbox.watch_file("w").unwrap()["status"], "fired", "the retry persisted the tick");
    assert!(!sandbox.reserve().exists(), "the reserve was spent");
}

#[test]
fn a_healthy_run_prints_no_persist_field_and_nothing_on_stderr() {
    // why: DF2: "wire format unchanged when all is well", and the CLI's extra
    //      stderr line exists only for a failed write. Control for the tests
    //      above, from the same scenario with no fault plan.
    // proves: CLAIM(DF2)
    let sandbox = Sandbox::new();
    change_ready(&sandbox);
    let output = run_with(&sandbox, None, &["keepalive", "w"]);
    assert_eq!(output.status.code(), Some(0));
    let envelope = stdout_json(&output);
    assert_eq!(envelope["status"], "fired");
    assert!(envelope.get("persist").is_none(), "{envelope}");
    assert!(stderr_lines(&output).is_empty(), "{:?}", stderr_lines(&output));
}

#[test]
fn create_on_a_full_disk_exits_1_with_a_line_that_names_the_root_and_says_disk_full() {
    // why: DF5: "A create that still fails returns an error whose message names
    //      the state root and says the disk is full (not a bare `io error`)."
    //      Through the binary: exit 1 (a real failure, nothing was created),
    //      the stderr line names the root and contains the lowercase words
    //      `disk full` (DF5b), and no watch file or .tmp is left.
    // proves: CLAIM(DF5), CLAIM(DF5b)
    let sandbox = Sandbox::new();
    let input = sandbox.input("w.log");
    std::fs::write(&input, "boot\n").unwrap();
    let output = run_with(&sandbox, Some(DISK_STAYS_FULL), &["create", &change_watch_json("w", &input, 1)]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(stderr.contains(sandbox.state_root().to_str().unwrap()), "should name the state root: {stderr}");
    assert!(stderr.contains("disk full"), "should contain the lowercase words `disk full`: {stderr}");
    assert!(sandbox.watch_file("w").is_none());
    assert!(sandbox.tmp_files().is_empty());
}

#[test]
fn cancel_on_a_disk_that_only_has_the_reserve_to_give_is_saved_and_says_so() {
    // why: DF5: cancel goes through the same reserve-and-retry path. The file
    //      ends up cancelled, the reserve is spent, exit 0, and (inferred
    //      from DF4, red report A3) the envelope carries the used_reserve note.
    // proves: CLAIM(DF5), CLAIM(DF4)
    let sandbox = Sandbox::new();
    create_via_cli(&sandbox, "w", &sandbox.input("w.log"));
    assert!(sandbox.reserve().exists(), "precondition: create left a reserve");

    let output = run_with(&sandbox, Some(&sandbox.only_the_reserve_to_give()), &["cancel", "w"]);

    assert_eq!(output.status.code(), Some(0), "stderr {:?}", String::from_utf8_lossy(&output.stderr));
    let envelope = stdout_json(&output);
    assert_eq!(envelope["status"], "cancelled");
    assert_eq!(envelope["persist"], common::reserve_used_note());
    assert_eq!(sandbox.watch_file("w").unwrap()["status"], "cancelled");
    assert!(!sandbox.reserve().exists());
}

#[test]
fn a_malformed_fault_plan_is_refused_loudly_instead_of_injecting_nothing() {
    // why: the seam's own safety: if a typo in VIGIL_TEST_FAULT silently meant
    //      "no faults", every test above would pass without the disk ever
    //      being full. The binary must exit 1 and name the variable. Control:
    //      the same command with no variable works.
    // proves: CLAIM(DF-seam)
    let sandbox = Sandbox::new();
    let output = run_with(&sandbox, Some("this is not a fault plan"), &["list"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains(FAULT_ENV), "{:?}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(run_with(&sandbox, None, &["list"]).status.code(), Some(0), "control");
}

// ---- round 2: amendments DF1a, DF3a, DF5a, DF7 (specs/disk-full.md) ------

#[test]
fn a_quiet_keepalive_on_a_full_disk_returns_pending_at_once_without_a_redelivery_flag() {
    // why: DF1a (red report A1): a tick whose save failed ends the call. A
    //      quiet watch looks due again at once (its `last_tick_at` never
    //      moved), so without the rule the process either spins on the full
    //      disk or ticks in memory until `max_s` (the default is 270 s). Here
    //      the call must return within 5 s of real time (the interval is 1 s):
    //      exit 0, `pending` / "call keepalive again", persist.ok=false with
    //      kind disk_full. DF3a: nothing is at risk (no events, status not
    //      terminal), so the `redelivery` key is ABSENT. Exactly one stderr
    //      line, naming the state root (DF2).
    // proves: CLAIM(DF1a), CLAIM(DF3a), CLAIM(DF2)
    let sandbox = Sandbox::new();
    create_via_cli(&sandbox, "w", &sandbox.input("w.log"));
    let before = sandbox.watch_bytes("w");
    let mut command = sandbox.faulted_command(DISK_STAYS_FULL);
    command.args(["keepalive", "w"]);

    let output = output_within(command, Duration::from_secs(5));

    assert_eq!(output.status.code(), Some(0), "stderr {:?}", String::from_utf8_lossy(&output.stderr));
    let envelope = stdout_json(&output);
    assert_eq!(envelope["status"], "pending");
    assert_eq!(envelope["next"], "call keepalive again");
    assert_eq!(envelope["events"].as_array().map(Vec::len), Some(0), "{envelope}");
    assert_failed_persist(&envelope, "disk_full", "No space left on device", &sandbox.watch_path("w"), false, Some(true));
    let lines = stderr_lines(&output);
    assert_eq!(lines.len(), 1, "exactly one stderr line, got {lines:?}");
    assert!(lines[0].contains(sandbox.state_root().to_str().unwrap()), "{:?}", lines[0]);
    assert_eq!(sandbox.watch_bytes("w"), before);
}

#[test]
fn cancel_on_a_full_disk_prints_the_envelope_with_persist_and_leaves_the_watch_pending() {
    // why: DF5a (red report A3): a cancel that cannot be saved, even after the
    //      reserve, is not an error and not a silent success: it prints the
    //      cancelled envelope with persist.ok=false (like a tick), exits 0
    //      (the exit code of `cancel` does not change, D2), writes one stderr
    //      line naming the state root, and the file on disk is the old pending
    //      one, byte for byte, with no .tmp. DF3a: a terminal status was not
    //      saved, so `redelivery: true`.
    // proves: CLAIM(DF5a), CLAIM(DF3a), CLAIM(DF6)
    let sandbox = Sandbox::new();
    create_via_cli(&sandbox, "w", &sandbox.input("w.log"));
    let before = sandbox.watch_bytes("w");

    let output = run_with(&sandbox, Some(DISK_STAYS_FULL), &["cancel", "w"]);

    assert_eq!(output.status.code(), Some(0), "stderr {:?}", String::from_utf8_lossy(&output.stderr));
    let envelope = stdout_json(&output);
    assert_eq!(envelope["status"], "cancelled");
    assert_failed_persist(&envelope, "disk_full", "No space left on device", &sandbox.watch_path("w"), true, Some(true));
    let lines = stderr_lines(&output);
    assert_eq!(lines.len(), 1, "exactly one stderr line, got {lines:?}");
    assert!(lines[0].contains(sandbox.state_root().to_str().unwrap()), "{:?}", lines[0]);
    assert_eq!(sandbox.watch_bytes("w"), before, "the cancel was not saved, so the file is as it was");
    assert_eq!(sandbox.watch_file("w").unwrap()["status"], "pending");
    assert!(sandbox.tmp_files().is_empty(), "{:?}", sandbox.tmp_files());
}

#[test]
fn a_create_whose_baseline_cannot_be_saved_leaves_no_placeholder_so_the_same_id_can_be_created_again() {
    // why: DF7 (red report A7): `create` first writes a placeholder that
    //      reserves the id (attempt 1 of the watch writes), then the baseline
    //      (attempt 2 onwards fail here: `watch:28:2..`). Before the fix the
    //      error was returned but the placeholder stayed, so the agent's retry
    //      with the same id got `duplicate id`. Deleting a file works on a
    //      full disk, so the placeholder must be removed. Process 1 exits 1
    //      naming the root and `disk full`; no watch file, no .tmp. Process 2
    //      (space exists again) creates the same id: exit 0 and a pending
    //      watch file.
    // proves: CLAIM(DF7), CLAIM(DF5b)
    let sandbox = Sandbox::new();
    let input = sandbox.input("w.log");
    std::fs::write(&input, "boot\n").unwrap();

    let failed = run_with(&sandbox, Some("watch:28:2.."), &["create", &change_watch_json("w", &input, 1)]);

    assert_eq!(failed.status.code(), Some(1), "stderr {:?}", String::from_utf8_lossy(&failed.stderr));
    let stderr = String::from_utf8_lossy(&failed.stderr).to_string();
    assert!(stderr.contains(sandbox.state_root().to_str().unwrap()) && stderr.contains("disk full"), "{stderr}");
    assert!(sandbox.watch_file("w").is_none(), "the placeholder must be removed, found {:?}", sandbox.watch_file("w"));
    assert!(!sandbox.watch_path("w").exists());
    assert!(sandbox.tmp_files().is_empty(), "{:?}", sandbox.tmp_files());

    let retry = run_with(&sandbox, None, &["create", &change_watch_json("w", &input, 1)]);
    assert_eq!(retry.status.code(), Some(0), "the same id must be free again: {:?}", String::from_utf8_lossy(&retry.stderr));
    assert_eq!(sandbox.watch_file("w").unwrap()["status"], "pending");
}

// ---- round 3: seam stages (specs/disk-full.md S4) --------------------------

#[test]
fn a_malformed_stage_in_a_fault_plan_is_refused_loudly_and_a_well_formed_one_is_not() {
    // why: S4 + the seam's own safety (see the test above): `watch@renmae`
    //      parsed as "never fires" would make every stage test below pass
    //      without the disk ever failing at that stage. The binary must exit 1
    //      and name the variable for a misspelt stage, a different case, an
    //      empty stage, and a stage the kind does not have (`lock@sync`).
    //      Control: a valid staged plan lets a command that writes nothing run.
    // proves: CLAIM(DF-seam), CLAIM(S4)
    let sandbox = Sandbox::new();
    for bad in ["watch@renmae:28:all", "watch@RENAME:28:all", "watch@:28:all", "lock@sync:28:all", "reserve@rename:28:all"] {
        let output = run_with(&sandbox, Some(bad), &["list"]);
        assert_eq!(output.status.code(), Some(1), "{bad:?} must be refused, stderr {:?}", String::from_utf8_lossy(&output.stderr));
        assert!(String::from_utf8_lossy(&output.stderr).contains(FAULT_ENV), "{bad:?}: {:?}", String::from_utf8_lossy(&output.stderr));
    }
    for good in ["watch@rename:28:all", "reserve@short:28:1", "watch@cleanup:5:all:w.json"] {
        assert_eq!(run_with(&sandbox, Some(good), &["list"]).status.code(), Some(0), "{good:?} is a valid plan");
    }
}

#[test]
fn a_failure_at_the_rename_stage_through_the_binary_keeps_the_old_file_and_still_answers() {
    // why: S4 through the real binary and the real env var: the stage syntax
    //      reaches the store. The rename is the commit point: with a complete
    //      `.tmp` but a failed rename the old watch file is byte-identical,
    //      no `.tmp` remains, and the call still prints the full fired
    //      envelope with persist.ok = false, kind disk_full, exit 0 (DF1/DF2).
    //      (`reserve:28:all` so the rescue has no room to re-create anything.)
    // proves: CLAIM(S4), CLAIM(DF6), CLAIM(DF1)
    let sandbox = Sandbox::new();
    change_ready(&sandbox);
    let before = sandbox.watch_bytes("w");

    let output = run_with(&sandbox, Some("watch@rename:28:all,reserve:28:all"), &["keepalive", "w"]);

    assert_eq!(output.status.code(), Some(0), "stderr {:?}", String::from_utf8_lossy(&output.stderr));
    let envelope = stdout_json(&output);
    assert_eq!(envelope["status"], "fired");
    assert_eq!(envelope["events"][0]["event_id"], "w:1:0");
    assert_failed_persist(&envelope, "disk_full", "No space left on device", &sandbox.watch_path("w"), true, Some(true));
    assert_eq!(sandbox.watch_bytes("w"), before, "the old file is still the watch");
    assert!(sandbox.tmp_files().is_empty(), "{:?}", sandbox.tmp_files());
}

