//! End-to-end suite for the `vigil` binary's CLI subcommands. Lives in tests/
//! (not beside a module) because it spawns the built binary
//! (CARGO_BIN_EXE_vigil) and checks process-level behaviour: exit codes,
//! stdout JSON, and files on disk. Most tests finish in well under 5 s;
//! red auto-interval waits have explicit deadlines of up to 8 s;
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
fn a_fired_result_is_replayed_by_status_and_keepalive_with_the_same_event_ids() {
    // why: V3 (S3): the terminal result is stored once and replayed unchanged
    //      on every later call, across processes. Three separate `vigil`
    //      processes (keepalive, then status, keepalive) must print the same
    //      events with the same event_id and digest, and none may block.
    // proves: CLAIM(terminal-replay)
    let sandbox = Sandbox::new();
    let input = sandbox.input("a.log");
    create_via_cli(&sandbox, "w", &input);
    append_once(&input, "replay-me");
    let first = stdout_json(&sandbox.run(&["keepalive", "w", "--max-s", "30"]));
    assert_eq!(first["status"], "fired");
    let started = std::time::Instant::now();
    for args in [vec!["status", "w"], vec!["keepalive", "w", "--max-s", "30"]] {
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
fn sigterm_to_a_blocked_keepalive_exits_143_and_kills_the_command_it_was_running() {
    // why: code review C9: SIGTERM ended vigil without dropping its futures,
    //      and command inputs run in their own session, so a stopped
    //      background vigil (found with the old `vigil wait`, removed in
    //      W42-W1) left its command tree running with its timeout gone. The
    //      binary now handles the signal: cancel active work, let the command
    //      cleanup run, exit 143.
    // proves: CLAIM(signal-cleanup)
    signal_stops_vigil_and_its_command_group(&["keepalive", "w", "--max-s", "30"], "-TERM", 143);
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

#[test]
fn an_empty_fault_variable_is_treated_as_unset() {
    // why: review T8: `VIGIL_TEST_FAULT=""` (an exported-but-empty variable,
    //      as `VAR= vigil ...` or a wrapper script leaves it) must mean "no
    //      faults", like an empty XDG_STATE_HOME means unset in `state_root`.
    //      Before, "" went to the plan parser and vigil refused to start
    //      ("invalid VIGIL_TEST_FAULT"). A command that writes (create) must
    //      succeed and its watch file must exist. Control: a malformed
    //      non-empty value still refuses (test above), so this is not "ignore
    //      everything".
    // proves: CLAIM(DF-seam)
    let sandbox = Sandbox::new();
    let mut command = sandbox.faulted_command("");
    let path = sandbox.input("e.log");
    std::fs::write(&path, "boot\n").unwrap();
    command.args(["create", &change_watch_json("e", &path, 1)]);

    let output = output_within(command, RUN);

    assert_eq!(output.status.code(), Some(0), "stderr {:?}", String::from_utf8_lossy(&output.stderr));
    assert!(sandbox.watch_file("e").is_some(), "the write went through");
}

// ---- round 2: amendments DF1a, DF3a, DF5a, DF7 (specs/disk-full.md) ------

#[test]
fn a_quiet_keepalive_on_a_full_disk_is_held_one_interval_then_returns_pending_without_a_redelivery_flag() {
    // why: DF1a (revised after tier-2 review, T1): a tick whose save failed
    //      ends the call. A quiet watch looks due again at once (its
    //      `last_tick_at` never moved), so without the rule the process either
    //      spins on the full disk or ticks in memory until `max_s` (the default
    //      is 270 s). But a call that returns at once leaves nothing pacing the
    //      agent's next call. Now the call is held for min(interval, budget
    //      left) = 1 s (the watch's interval, the budget is 270 s) and then
    //      returns: not before 0.9 s of real time (the hold ran), well within
    //      5 s (it did not tick on until `max_s`); exit 0, `pending` / "call
    //      keepalive again", `waited_s` 1, persist.ok=false with kind
    //      disk_full. DF3a: nothing is at risk (no events, status not
    //      terminal), so the `redelivery` key is ABSENT. Exactly one stderr
    //      line, naming the state root (DF2).
    // proves: CLAIM(DF1a), CLAIM(DF3a), CLAIM(DF2)
    let sandbox = Sandbox::new();
    create_via_cli(&sandbox, "w", &sandbox.input("w.log"));
    let before = sandbox.watch_bytes("w");
    let mut command = sandbox.faulted_command(DISK_STAYS_FULL);
    command.args(["keepalive", "w"]);

    let started = std::time::Instant::now();
    let output = output_within(command, Duration::from_secs(5));
    let elapsed = started.elapsed();

    assert_eq!(output.status.code(), Some(0), "stderr {:?}", String::from_utf8_lossy(&output.stderr));
    assert!(elapsed >= Duration::from_millis(900), "the call must be held one interval (1 s), it took {elapsed:?}");
    let envelope = stdout_json(&output);
    assert_eq!(envelope["status"], "pending");
    assert_eq!(envelope["next"], "call keepalive again");
    assert_eq!(envelope["waited_s"], 1, "{envelope}");
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


// ---- auto-interval (specs/auto-interval.md) --------------------------------
//
// The sandbox HOME is the only place vigil looks for `~/.claude/projects`, and
// `VIGIL_UW_COMMAND` points at a fake `uw`, so none of these touches the real
// transcripts or the real usagewindow. Every watch fires on its first tick, so
// no test waits out a window; what is checked is the window vigil CHOSE
// (`max_s`, `cache`), which is reported on the envelope.

const SESSION: &str = "sess-1";

/// A sandbox with a Claude transcript for `SESSION` whose newest write is `1h`
/// (or `5m` when `one_hour` is false), and a fake `uw` that reports no credits.
fn auto_sandbox(one_hour: bool) -> (Sandbox, std::path::PathBuf) {
    use vigil::testing::{assistant_line, write_script, write_transcript};
    let sandbox = Sandbox::new();
    let lines = if one_hour { vec![assistant_line(0, 500), assistant_line(900, 0)] } else { vec![assistant_line(900, 0), assistant_line(0, 500)] };
    write_transcript(&sandbox.home().join(".claude/projects"), "p", SESSION, &lines);
    let fake_uw = sandbox.dir.path().join("fake-uw");
    write_script(&fake_uw, "echo '{\"usage\": []}'");
    (sandbox, fake_uw)
}

fn create_with(sandbox: &Sandbox, id: &str, extra: serde_json::Value) -> std::process::Output {
    let input = sandbox.input(&format!("{id}.log"));
    std::fs::write(&input, "boot\n").unwrap();
    let mut watch: serde_json::Value = serde_json::from_str(&change_watch_json(id, &input, 1)).unwrap();
    for (key, value) in extra.as_object().unwrap() {
        watch[key] = value.clone();
    }
    sandbox.run(&["create", &watch.to_string()])
}

/// `vigil keepalive <id> <args>` after one write to the watch's input, with
/// extra environment variables, finishing inside 10 s.
fn fire_and_keepalive(sandbox: &Sandbox, fake_uw: &std::path::Path, id: &str, args: &[&str], env: &[(&str, &str)]) -> serde_json::Value {
    append_once(&sandbox.input(&format!("{id}.log")), "go");
    let mut command = sandbox.command();
    command.arg("keepalive").arg(id).args(args).env("VIGIL_UW_COMMAND", fake_uw);
    for (name, value) in env {
        command.env(name, value);
    }
    let output = finish_within(piped(command), Duration::from_secs(10));
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    stdout_json(&output)
}

/// Review S3: the first orchestrator call on a state root owes its due credit
/// check even when its watch fires at once, so its `cache` also holds
/// `credit_check` (the fake `uw` reports no credits). Returns `cache` without
/// that key, after asserting it is there and says "not on credits".
fn cache_without_credit_check(envelope: &serde_json::Value) -> serde_json::Value {
    let mut cache = envelope["cache"].clone();
    let check = cache.as_object_mut().expect("cache object").remove("credit_check").expect("S3: an orchestrator call that fires at once still makes its due credit check");
    assert_eq!((&check["on_credits"], &check["source"]), (&serde_json::json!(false), &serde_json::json!("uw")), "{check}");
    cache
}

#[test]
fn the_cli_orchestrator_flag_on_a_one_hour_cache_chooses_a_3000_s_window() {
    // why: AI1' + AI3 through the real binary: CLAUDE_CODE_SESSION_ID is read
    //      from the environment at the edge, the transcript from the sandbox
    //      HOME (~/.claude/projects/p/sess-1.jsonl, newest write 1h), and the
    //      window reported on the envelope is 3000 s with its reason. The
    //      watch fires at once, so the call returns immediately; AI4's clock
    //      block is there too (first call: since_last_call_s null).
    // proves: CLAIM(AI1'), CLAIM(AI3), CLAIM(AI4)
    let (sandbox, fake_uw) = auto_sandbox(true);
    assert!(create_with(&sandbox, "w", serde_json::json!({})).status.success());
    let envelope = fire_and_keepalive(&sandbox, &fake_uw, "w", &["--orchestrator"], &[("CLAUDE_CODE_SESSION_ID", SESSION)]);
    assert_eq!(envelope["status"], "fired");
    assert_eq!(envelope["max_s"], 3000);
    assert_eq!(envelope["cache"]["ttl"], "1h");
    assert_eq!(envelope["cache"]["source"], "claude-transcript");
    assert_eq!(envelope["cache"]["interval_s"], 3000);
    let clock = envelope["clock"].as_object().expect("clock object");
    let mut names: Vec<&str> = clock.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(names, ["since_last_call_s", "summary", "waited_s", "watch_age_s"]);
    assert!(clock["since_last_call_s"].is_null());
}

#[test]
fn without_the_flag_the_cli_window_stays_270_s() {
    // why: AI1' (Rev 2): same environment, same transcript, no --orchestrator
    //      -> 270 s and "unknown / default" (vigil did not look). A subagent
    //      shares CLAUDE_CODE_SESSION_ID, so the flag is the only difference.
    // proves: CLAIM(AI1'), CLAIM(AI6)
    let (sandbox, fake_uw) = auto_sandbox(true);
    assert!(create_with(&sandbox, "w", serde_json::json!({})).status.success());
    let envelope = fire_and_keepalive(&sandbox, &fake_uw, "w", &[], &[("CLAUDE_CODE_SESSION_ID", SESSION)]);
    assert_eq!(envelope["max_s"], 270);
    assert_eq!(envelope["cache"], serde_json::json!({"ttl": "unknown", "source": "default", "interval_s": 270}));
}

#[test]
fn an_explicit_max_s_beats_the_flag_and_the_cache() {
    // why: AI1' "explicit max_s still wins": --max-s 60 --orchestrator on a 1h
    //      cache -> 60 s, source explicit.
    // proves: CLAIM(AI1'), CLAIM(AI3)
    let (sandbox, fake_uw) = auto_sandbox(true);
    assert!(create_with(&sandbox, "w", serde_json::json!({})).status.success());
    let envelope = fire_and_keepalive(&sandbox, &fake_uw, "w", &["--orchestrator", "--max-s", "60"], &[("CLAUDE_CODE_SESSION_ID", SESSION)]);
    assert_eq!(envelope["max_s"], 60);
    assert_eq!(cache_without_credit_check(&envelope), serde_json::json!({"ttl": "unknown", "source": "explicit", "interval_s": 60}));
}

#[test]
fn a_watch_created_with_orchestrator_true_gets_the_long_window_without_the_flag() {
    // why: AI1' "the watch can also store it at create", and AI5's new create
    //      fields must be accepted over the CLI (expect_s, stall_s). The
    //      keepalive carries no flag.
    // proves: CLAIM(AI1'), CLAIM(AI5)
    let (sandbox, fake_uw) = auto_sandbox(true);
    let created = create_with(&sandbox, "w", serde_json::json!({"orchestrator": true, "expect_s": 600, "stall_s": 120}));
    assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
    let envelope = fire_and_keepalive(&sandbox, &fake_uw, "w", &[], &[("CLAUDE_CODE_SESSION_ID", SESSION)]);
    assert_eq!(envelope["max_s"], 3000);
    assert_eq!(envelope["cache"]["source"], "claude-transcript");
}

#[test]
fn codex_is_recognised_from_the_environment_and_gets_the_codex_default() {
    // why: AI1' / A3: AGENT=codex (no Claude session id) + --orchestrator ->
    //      270 s, 5m / codex-default. AGENT=claude, as on the dev machine, is
    //      not Codex (checked in the same test: it falls to unknown/default).
    // proves: CLAIM(AI1'), CLAIM(AI3)
    let (sandbox, fake_uw) = auto_sandbox(true);
    assert!(create_with(&sandbox, "w", serde_json::json!({})).status.success());
    let envelope = fire_and_keepalive(&sandbox, &fake_uw, "w", &["--orchestrator"], &[("AGENT", "codex")]);
    assert_eq!(cache_without_credit_check(&envelope), serde_json::json!({"ttl": "5m", "source": "codex-default", "interval_s": 270}));

    assert!(create_with(&sandbox, "v", serde_json::json!({})).status.success());
    let claude = fire_and_keepalive(&sandbox, &fake_uw, "v", &["--orchestrator"], &[("AGENT", "claude")]);
    assert_eq!(claude["cache"], serde_json::json!({"ttl": "unknown", "source": "default", "interval_s": 270}));
}

#[test]
fn creating_a_watch_with_a_zero_expect_s_fails() {
    // why: A16 through the CLI: exit status 1 and a message on stderr, nothing
    //      on stdout. (Engine-level coverage is in auto_interval_tests.)
    // proves: CLAIM(AI5)
    let sandbox = Sandbox::new();
    let output = create_with(&sandbox, "w", serde_json::json!({"expect_s": 0}));
    assert_eq!(output.status.code(), Some(1), "stdout {:?}", String::from_utf8_lossy(&output.stdout));
    assert!(!output.stderr.is_empty());
}

#[test]
fn cli_both_agent_signals_cap_both_nesting_directions_at_270() {
    // why: S4. Inherited CODEX_HOME from a Codex parent and inherited Claude
    //      session id in AGENT=codex are ambiguous. Claude-only control proves
    //      the real transcript is otherwise sufficient for 3000.
    // proves: CLAIM(S4)
    let (sandbox, fake_uw) = auto_sandbox(true);
    assert!(create_with(&sandbox, "control", serde_json::json!({})).status.success());
    let control = fire_and_keepalive(&sandbox, &fake_uw, "control", &["--orchestrator"], &[("CLAUDE_CODE_SESSION_ID", SESSION)]);
    assert_eq!(control["cache"]["interval_s"], 3000);
    for (id, signal, value) in [("claude-in-codex", "CODEX_HOME", "/inherited"), ("codex-in-claude", "AGENT", "codex")] {
        assert!(create_with(&sandbox, id, serde_json::json!({})).status.success());
        let answer = fire_and_keepalive(&sandbox, &fake_uw, id, &["--orchestrator"], &[("CLAUDE_CODE_SESSION_ID", SESSION), (signal, value)]);
        assert_eq!(answer["cache"]["interval_s"], 270, "{id}");
        assert_eq!(answer["cache"]["ttl"], "unknown", "{id}");
        assert_eq!(answer["cache"]["source"], "default", "{id}");
    }
}

// ---- N1: two real processes, one credit change, one hook ----

/// `credit.lock` is held by someone else right now (a non-blocking flock probe
/// from outside, independent of how vigil takes it).
fn credit_lock_is_held(root: &std::path::Path) -> bool {
    let Ok(file) = std::fs::File::open(root.join("credit.lock")) else { return false };
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

fn lines_of(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path).map_or(0, |text| text.lines().count())
}

#[test]
fn two_real_processes_race_for_one_credit_change_and_the_lock_covers_hook_and_state_update() {
    // why: N1 / S9 through the real binary. Both processes are orchestrators on
    //      watches of their own, credit-state.json says off, a fake `uw` says
    //      on. Process A takes `credit.lock` and runs the hook, which parks on
    //      a barrier file (`release`); B is started while A is inside the hook.
    //      Then A is frozen a second time, by the test-only fault seam, AFTER
    //      the hook and BEFORE the commit of credit-state.json
    //      (`credit@rename:hold=commit`). Looked at from outside, at both
    //      points: `credit.lock` is held (a non-blocking flock from the test
    //      fails), the hook has run once and only once, B has not run it, and
    //      the state file still says off. A lock released after the hook but
    //      before the state update would show here as a free lock and, a moment
    //      later, as a second hook run by B. At the end exactly one process
    //      reports the change, the hook ran once, and the file says on.
    // proves: CLAIM(N1), CLAIM(S9)
    let (sandbox, fake_uw) = auto_sandbox(false);
    vigil::testing::write_script(&fake_uw, "echo '{\"usage\":[{\"provider\":\"ClaudeCode\",\"windows\":[{\"window\":\"weekly\",\"exceeded\":true}]}]}'");
    assert!(create_with(&sandbox, "a", serde_json::json!({})).status.success());
    assert!(create_with(&sandbox, "b", serde_json::json!({"deadline_s": 12})).status.success());
    std::fs::write(sandbox.state_root().join("credit-state.json"), r#"{"on_credits":false,"checked_at":"2020-01-01T00:00:00Z"}"#).unwrap();

    let barrier = sandbox.dir.path().join("barrier");
    std::fs::create_dir_all(&barrier).unwrap();
    let (log, entered, release, done, commit) =
        (barrier.join("hook.log"), barrier.join("entered"), barrier.join("release"), barrier.join("done"), barrier.join("commit"));
    let hook = barrier.join("hook.sh");
    vigil::testing::write_script(
        &hook,
        &format!(
            "echo \"$$\" >> '{}'\n: > '{}'\nwhile [ ! -e '{}' ]; do sleep 0.05; done\n: > '{}'",
            log.display(),
            entered.display(),
            release.display(),
            done.display()
        ),
    );
    let spawn = |id: &str| {
        let mut command = sandbox.command();
        command
            .args(["keepalive", id, "--orchestrator"])
            .env("CLAUDE_CODE_SESSION_ID", SESSION)
            .env("VIGIL_UW_COMMAND", &fake_uw)
            .env("VIGIL_ON_CREDIT_CHANGE", &hook)
            .env("VIGIL_TEST_FAULT", format!("credit@rename:hold={}:all", commit.display()))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.spawn().unwrap()
    };

    let mut a = spawn("a");
    assert!(wait_until(Duration::from_secs(8), || entered.exists()), "process A should have started the credit hook");
    assert!(credit_lock_is_held(&sandbox.state_root()), "credit.lock must be held while the hook runs");
    let mut b = spawn("b");
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(lines_of(&log), 1, "B must wait for the lock, not run its own hook");
    assert!(matches!(b.try_wait(), Ok(None)), "B is still waiting for the lock");

    std::fs::write(&release, "").unwrap();
    assert!(wait_until(Duration::from_secs(8), || done.exists()), "the hook should finish once released");
    assert!(
        wait_until(Duration::from_secs(8), || barrier.join("commit.waiting").exists()),
        "A should reach the commit of credit-state.json after the hook"
    );
    assert!(credit_lock_is_held(&sandbox.state_root()), "credit.lock must still be held between the hook and the state commit");
    let recorded: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(sandbox.state_root().join("credit-state.json")).unwrap()).unwrap();
    assert_eq!(recorded["on_credits"], false, "the state is not committed yet");
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(lines_of(&log), 1, "B must not run the hook while A has not recorded the change");
    assert!(matches!(a.try_wait(), Ok(None)) && matches!(b.try_wait(), Ok(None)), "both are parked, A at the commit, B at the lock");

    std::fs::write(&commit, "").unwrap();
    let (out_a, out_b) = (finish_within(a, Duration::from_secs(15)), finish_within(b, Duration::from_secs(20)));
    assert!(out_a.status.success() && out_b.status.success(), "{} / {}", String::from_utf8_lossy(&out_a.stderr), String::from_utf8_lossy(&out_b.stderr));
    let (env_a, env_b) = (stdout_json(&out_a), stdout_json(&out_b));
    let changes = |envelope: &serde_json::Value| envelope["events"].as_array().unwrap().iter().filter(|e| e["kind"] == "credit_state_changed").count();
    assert_eq!((changes(&env_a), changes(&env_b)), (1, 0), "exactly one process reports the change: {env_a} / {env_b}");
    assert_eq!(env_b["status"], "expired", "B carried on quietly until its watch ran out");
    assert_eq!(lines_of(&log), 1, "the hook ran once in all");
    let recorded: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(sandbox.state_root().join("credit-state.json")).unwrap()).unwrap();
    assert_eq!(recorded["on_credits"], true);
    assert!(!credit_lock_is_held(&sandbox.state_root()), "the lock is released at the end");
}

// ---- N3: print, then commit ----

/// `vigil <args>` whose standard output is a pipe nobody reads any more (the
/// read end is closed right after the spawn, long before the process has
/// anything to print): every write fails with EPIPE. A descriptor that is not
/// open at all is not testable: Rust's runtime re-opens fds 0-2 as /dev/null
/// before `main`, so such a process cannot tell.
fn run_with_closed_stdout(sandbox: &Sandbox, args: &[&str]) -> std::process::Output {
    let mut command = sandbox.command();
    command.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    drop(child.stdout.take());
    finish_within(child, Duration::from_secs(12))
}

/// `vigil <args>` whose standard output is `/dev/full` (every write fails with ENOSPC).
fn run_with_full_stdout(sandbox: &Sandbox, args: &[&str]) -> std::process::Output {
    let mut command = sandbox.command();
    command.args(args).stdout(Stdio::from(std::fs::OpenOptions::new().write(true).open("/dev/full").unwrap())).stderr(Stdio::piped());
    finish_within(command.spawn().unwrap(), Duration::from_secs(12))
}

#[test]
fn a_failed_stdout_write_leaves_the_event_uncommitted_and_the_next_call_replays_it() {
    // why: N3. The CLI prints the envelope, THEN commits its delivery. An
    //      attention event (watch `expect_s: 2`, due after 3 s) is persisted
    //      with the watch until a delivery is committed. Case 1: stdout is
    //      closed; case 2: stdout is /dev/full. Either way the process must
    //      fail (non-zero, a line on stderr naming stdout), and the NEXT
    //      keepalive must return the same attention event at once, flagged
    //      `redelivered: true` (the agent may or may not have seen it). Control:
    //      with a working stdout the event is delivered once and a later call
    //      (the watch has since expired) does not replay it. A naive println!
    //      panics on EPIPE (exit 101, no clean error) and Rust's own stdout
    //      swallows EBADF, so the delivery must be written and checked by hand.
    // proves: CLAIM(N3)
    let sandbox = Sandbox::new();
    for (id, run) in [
        ("closed", run_with_closed_stdout as fn(&Sandbox, &[&str]) -> std::process::Output),
        ("full", run_with_full_stdout),
    ] {
        assert!(create_with(&sandbox, id, serde_json::json!({"expect_s": 2, "deadline_s": 600})).status.success());
        let failed = run(&sandbox, &["keepalive", id]);
        assert!(!failed.status.success(), "{id}: a failed stdout write must fail the command, it exited {:?}", failed.status.code());
        assert!(String::from_utf8_lossy(&failed.stderr).contains("stdout"), "{id}: stderr should name stdout: {:?}", String::from_utf8_lossy(&failed.stderr));

        let started = std::time::Instant::now();
        let replay = finish_within(piped({
            let mut command = sandbox.command();
            command.args(["keepalive", id, "--max-s", "30"]);
            command
        }), Duration::from_secs(10));
        assert!(started.elapsed() < Duration::from_secs(8), "{id}: the replay must not wait out a window");
        let answer = stdout_json(&replay);
        assert_eq!((&answer["status"], &answer["next"]), (&serde_json::json!("pending"), &serde_json::json!("act on events")), "{id}: {answer}");
        let events = answer["events"].as_array().unwrap();
        assert_eq!(events.len(), 1, "{id}: {answer}");
        assert_eq!((&events[0]["kind"], &events[0]["reason"], &events[0]["redelivered"]), (&serde_json::json!("attention"), &serde_json::json!("overdue"), &serde_json::json!(true)), "{id}: {answer}");
    }

    assert!(create_with(&sandbox, "ok", serde_json::json!({"expect_s": 2, "deadline_s": 7})).status.success());
    let first = finish_within(piped({
        let mut command = sandbox.command();
        command.args(["keepalive", "ok"]);
        command
    }), Duration::from_secs(10));
    let first = stdout_json(&first);
    assert_eq!(first["events"][0]["kind"], "attention", "control: {first}");
    assert!(first["events"][0].get("redelivered").is_none(), "control: the first delivery is not a redelivery: {first}");
    let later = finish_within(piped({
        let mut command = sandbox.command();
        command.args(["keepalive", "ok", "--max-s", "30"]);
        command
    }), Duration::from_secs(12));
    let later = stdout_json(&later);
    assert_eq!(later["status"], "expired", "control: {later}");
    assert_eq!(later["events"], serde_json::json!([]), "control: a delivered event is not replayed: {later}");
}

// ---- holds (SPEC "Holds", H1-H12) ------------------------------------------

/// A qex job id (the real one in `tests/fixtures/qex`) and the `--for` spec for it.
const J1: &str = "0d31c8ac-25e4-4ab2-8a3b-9b24ccd533c9";
const J1_SPEC: &str = "qex:0d31c8ac-25e4-4ab2-8a3b-9b24ccd533c9";

/// `vigil <args>` with a fake `qex` first on PATH, finishing inside 10 s.
fn run_with_qex(sandbox: &Sandbox, qex: &FakeQex, args: &[&str]) -> std::process::Output {
    let mut command = sandbox.command();
    command.args(args).env("PATH", qex.path());
    finish_within(piped(command), Duration::from_secs(10))
}

fn rfc3339(value: &serde_json::Value) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(value.as_str().unwrap()).unwrap().with_timezone(&chrono::Utc)
}

#[test]
fn hold_on_a_finished_qex_job_releases_at_the_first_keepalive_and_says_why() {
    // why: H1, H6, H11 through the real binary. `vigil hold --for qex:ID`
    //      prints the created watch (deadline 7 days out, H5); `keepalive ID`
    //      senses the job through `qex status ID --json --no-logs` (the exact
    //      arguments are recorded by the fake) and returns the release event.
    //      `next` is "act on events"; the event is ordinary (no `kind`).
    // proves: CLAIM(H1), CLAIM(H5), CLAIM(H6), CLAIM(H11)
    let sandbox = Sandbox::new();
    let qex = FakeQex::new(&sandbox);
    qex.set_job(J1, "completed");
    let created = run_with_qex(&sandbox, &qex, &["hold", "--id", "h", "--for", J1_SPEC]);
    assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
    let created = stdout_json(&created);
    assert_eq!((created["watch_id"].as_str(), created["status"].as_str()), (Some("h"), Some("pending")));
    assert_eq!((rfc3339(&created["deadline_at"]) - rfc3339(&created["created_at"])).num_seconds(), 604_800);

    let envelope = run_with_qex(&sandbox, &qex, &["keepalive", "h"]);
    assert!(envelope.status.success(), "{}", String::from_utf8_lossy(&envelope.stderr));
    let envelope = stdout_json(&envelope);
    assert_eq!((envelope["status"].as_str(), envelope["next"].as_str()), (Some("fired"), Some("act on events")), "{envelope}");
    // W42-D6/D9: the item's own `item ended` (attention), then the release
    // (ordinary), which counts the items and names their labels.
    let (item, event) = (&envelope["events"][0], &envelope["events"][1]);
    assert_eq!((item["kind"].as_str(), item["summary"].as_str()), (Some("attention"), Some(format!("item ended: {J1_SPEC}: completed").as_str())), "{envelope}");
    assert_eq!((event["input"].as_str(), event["summary"].as_str()), (Some("hold"), Some(format!("hold released: 1 item ended ({J1_SPEC})").as_str())), "{envelope}");
    assert!(event.get("kind").is_none());
    assert!(qex.calls().contains(&format!("status {J1} --json --no-logs")), "{:?}", qex.calls());
}

#[test]
fn a_hold_on_a_live_job_is_pending_with_call_keepalive_again_until_cancelled() {
    // why: H6/H7: while the job runs, status says pending and the next step is
    //      "call keepalive again" (the same envelope keepalive builds), the
    //      list shows it, and cancel ends it like any watch. The watch file
    //      holds the documented wire shape.
    // proves: CLAIM(H7), CLAIM(H1)
    let sandbox = Sandbox::new();
    let qex = FakeQex::new(&sandbox);
    qex.set_job(J1, "running");
    assert!(run_with_qex(&sandbox, &qex, &["hold", "--id", "h", "--for", J1_SPEC, "--for", "pid:1", "--for", "until:2099-01-01T00:00:00Z"]).status.success());
    let status = stdout_json(&run_with_qex(&sandbox, &qex, &["status", "h"]));
    assert_eq!((status["status"].as_str(), status["next"].as_str()), (Some("pending"), Some("call keepalive again")), "{status}");
    let file = sandbox.watch_file("h").unwrap();
    assert_eq!(file["fire"], "hold");
    assert_eq!(file["orchestrator"], true);
    assert_eq!(
        file["inputs"],
        serde_json::json!([{"kind": "qex", "job": J1}, {"kind": "process", "pid": 1, "want": "exited"}, {"kind": "until", "at": "2099-01-01T00:00:00Z"}])
    );
    assert_eq!(stdout_json(&run_with_qex(&sandbox, &qex, &["cancel", "h"]))["status"], "cancelled");
}

#[test]
fn a_hold_on_an_unknown_qex_job_or_a_bad_spec_exits_1_and_writes_nothing() {
    // why: H2/H3: a typo is an error now, naming the job / the spec.
    // proves: CLAIM(H2), CLAIM(H3)
    let sandbox = Sandbox::new();
    let qex = FakeQex::new(&sandbox);
    let typo = run_with_qex(&sandbox, &qex, &["hold", "--id", "t", "--for", "qex:typo"]);
    assert_eq!(typo.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&typo.stderr);
    assert!(stderr.contains("typo") && stderr.contains("not found"), "{stderr}");
    assert!(sandbox.watch_file("t").is_none());
    let bad = run_with_qex(&sandbox, &qex, &["hold", "--id", "b", "--for", "boat:bx_1"]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("boat:bx_1"));
    assert!(sandbox.watch_file("b").is_none());
}

#[test]
fn a_bare_hold_and_until_idle_are_holds_on_nothing_and_on_the_cwd() {
    // why: H5/H10/H11. `vigil hold` with no work is the bare hold: no inputs,
    //      pending. `--until-idle` holds on the creating cwd through
    //      `qex list --under <cwd> --state active --json`: with an empty queue it is
    //      idle at once and releases with the directory in the reason.
    // proves: CLAIM(H5), CLAIM(H10), CLAIM(H11)
    let sandbox = Sandbox::new();
    let qex = FakeQex::new(&sandbox);
    assert!(run_with_qex(&sandbox, &qex, &["hold", "--id", "bare"]).status.success());
    let bare = sandbox.watch_file("bare").unwrap();
    assert_eq!((bare["fire"].as_str(), bare["inputs"].as_array().map(Vec::len)), (Some("hold"), Some(0)));
    assert_eq!(stdout_json(&run_with_qex(&sandbox, &qex, &["status", "bare"]))["status"], "pending");

    qex.set_list(&format!("--under {} --state active --json", sandbox.cwd().display()), "[]\n");
    assert!(run_with_qex(&sandbox, &qex, &["hold", "--id", "idle", "--until-idle"]).status.success());
    let cwd = sandbox.cwd();
    assert_eq!(sandbox.watch_file("idle").unwrap()["inputs"], serde_json::json!([{"kind": "qex_idle", "dir": cwd}]));
    let envelope = stdout_json(&run_with_qex(&sandbox, &qex, &["keepalive", "idle"]));
    assert_eq!(envelope["status"], "fired", "{envelope}");
    // W42-D6/D9: the item's own event, then the release naming it.
    let summaries: Vec<&str> = envelope["events"].as_array().unwrap().iter().map(|event| event["summary"].as_str().unwrap()).collect();
    assert_eq!(summaries, [format!("item ended: idle:{}: idle", cwd.display()), format!("hold released: 1 item ended (idle:{})", cwd.display())], "{envelope}");
    assert!(qex.calls().contains(&format!("list --under {} --state active --json", cwd.display())), "{:?}", qex.calls());
}

#[test]
fn a_hold_defaults_to_the_orchestrator_window_and_hold_orchestrator_false_turns_it_off() {
    // why: H9 through the real binary: with a 1 h cache transcript, `keepalive`
    //      with NO --orchestrator on a hold chooses 3000 s because the hold
    //      stores orchestrator = true (a plain watch would be 270 s); a hold
    //      made with `--orchestrator false` is 270 s.
    // proves: CLAIM(H9)
    let (sandbox, fake_uw) = auto_sandbox(true);
    let qex = FakeQex::new(&sandbox);
    qex.set_job(J1, "completed");
    let keepalive = |id: &str| {
        let mut command = sandbox.command();
        command.args(["keepalive", id]).env("PATH", qex.path()).env("VIGIL_UW_COMMAND", &fake_uw).env("CLAUDE_CODE_SESSION_ID", SESSION);
        let output = finish_within(piped(command), Duration::from_secs(10));
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        stdout_json(&output)
    };
    assert!(run_with_qex(&sandbox, &qex, &["hold", "--id", "main", "--for", J1_SPEC]).status.success());
    assert!(run_with_qex(&sandbox, &qex, &["hold", "--id", "sub", "--for", J1_SPEC, "--orchestrator", "false"]).status.success());
    let main = keepalive("main");
    assert_eq!((main["max_s"].as_u64(), main["cache"]["ttl"].as_str()), (Some(3000), Some("1h")), "{main}");
    assert_eq!(keepalive("sub")["max_s"], 270);
}

#[test]
fn the_cli_help_documents_hold_and_the_orchestrator_flag() {
    // why: the `--orchestrator` flag exists on `keepalive` (AI1')
    //      but an old vigil in a user profile once shadowed it, and agents
    //      could not find it. The help text of the binary in use must name it
    //      (and say it is for the main session), and `hold` and its options
    //      must be listed. Exit 0, nothing on stderr.
    // proves: CLAIM(H11)
    let sandbox = Sandbox::new();
    for (args, needles) in [
        (&["--help"][..], &["hold"][..]),
        (&["keepalive", "--help"], &["--orchestrator", "main"]),
        (&["hold", "--help"], &["--for", "--until-idle", "--id", "--deadline-s", "--orchestrator"]),
    ] {
        let output = sandbox.run(args);
        assert!(output.status.success(), "{args:?}");
        let text = String::from_utf8_lossy(&output.stdout);
        for needle in needles {
            assert!(text.contains(needle), "{args:?} must mention {needle:?}:\n{text}");
        }
    }
}

#[test]
fn an_idle_hold_reads_only_active_jobs_so_a_long_history_cannot_blind_it() {
    // why: S1. On this host `qex list --under /home/user --all --json` is 2.5 MB
    //      of terminal jobs, over the 64 KiB command capture, and the old query
    //      made an idle queue read "error: qex answered no job list" for ever.
    //      The fake answers the history query with 200 real-shaped records
    //      (>400 KB) and the active query with `[]`: the hold releases on its
    //      first tick. Control: a busy queue of 40 real-shaped records, itself
    //      over the capture, keeps the hold pending.
    // proves: CLAIM(H10)
    let sandbox = Sandbox::new();
    let qex = FakeQex::new(&sandbox);
    let cwd = sandbox.cwd();
    let history = FakeQex::list_of(200, "completed");
    assert!(history.len() > 400_000, "the history must dwarf the capture: {}", history.len());
    qex.set_list(&format!("--under {} --all --json", cwd.display()), &history);
    qex.set_list(&format!("--under {} --state active --json", cwd.display()), "[]\n");
    assert!(run_with_qex(&sandbox, &qex, &["hold", "--id", "idle", "--until-idle"]).status.success());
    let released = stdout_json(&run_with_qex(&sandbox, &qex, &["keepalive", "idle", "--max-s", "30"]));
    assert_eq!(released["status"], "fired", "an idle queue releases despite its history: {released}");

    let busy = FakeQex::list_of(40, "running");
    assert!(busy.len() > 64 * 1024);
    qex.set_list(&format!("--under {} --state active --json", cwd.display()), &busy);
    assert!(run_with_qex(&sandbox, &qex, &["hold", "--id", "busy", "--until-idle"]).status.success());
    // A pending keepalive blocks for its whole window, so it is started, let tick, and killed.
    let mut command = sandbox.command();
    command.args(["keepalive", "busy"]).env("PATH", qex.path());
    let mut child = piped(command);
    let active_query = format!("list --under {} --state active --json", cwd.display());
    assert!(wait_until(Duration::from_secs(10), || qex.calls().iter().any(|call| *call == active_query)), "the hold ticked: {:?}", qex.calls());
    child.kill().unwrap();
    child.wait().unwrap();
    let status = stdout_json(&run_with_qex(&sandbox, &qex, &["status", "busy"]));
    assert_eq!(status["status"], "pending", "{status}");
    assert!(qex.calls().iter().all(|call| !call.contains("--all")), "the history is never asked for: {:?}", qex.calls());
}

#[test]
fn a_hold_on_a_job_whose_record_is_cleaned_before_the_next_tick_releases() {
    // why: S2. The job is running when the hold is made; it finishes and
    //      `qex clean` deletes it. qex still accepts the id but answers exit
    //      127 "the job ID (NAME) existed and its state was `completed`.
    //      Something deleted its record" (real text). The hold releases,
    //      saying `gone`.
    // proves: CLAIM(H2)
    let sandbox = Sandbox::new();
    let qex = FakeQex::new(&sandbox);
    qex.set_job(J1, "running");
    assert!(run_with_qex(&sandbox, &qex, &["hold", "--id", "h", "--for", J1_SPEC]).status.success());
    qex.set_removed(J1);
    let released = stdout_json(&run_with_qex(&sandbox, &qex, &["keepalive", "h", "--max-s", "30"]));
    assert_eq!((released["status"].as_str(), released["next"].as_str()), (Some("fired"), Some("act on events")), "{released}");
    // W42-D6/D9: the item ends `gone`, then the release.
    let summaries: Vec<&str> = released["events"].as_array().unwrap().iter().map(|event| event["summary"].as_str().unwrap()).collect();
    assert_eq!(summaries, [format!("item ended: {J1_SPEC}: gone"), format!("hold released: 1 item ended ({J1_SPEC})")], "{released}");
}

#[test]
fn a_hold_made_on_a_name_stores_the_job_id_and_survives_the_name_being_reused() {
    // why: S3. `--for qex:nightly` resolves to the job id when the hold is
    //      made and stores THAT. A second job then takes the name, so qex
    //      answers "`nightly` names 2 jobs" (real text, exit 127) to a status
    //      by name; the hold must not ask by name again. When the first job
    //      completes it releases, naming the id; the call log shows no status
    //      for the name after creation.
    // proves: CLAIM(H3)
    let sandbox = Sandbox::new();
    let qex = FakeQex::new(&sandbox);
    qex.set_job(J1, "running");
    qex.set_name("nightly", J1);
    assert!(run_with_qex(&sandbox, &qex, &["hold", "--id", "h", "--for", "qex:nightly"]).status.success());
    assert_eq!(sandbox.watch_file("h").unwrap()["inputs"], serde_json::json!([{"kind": "qex", "job": J1}]));

    qex.set_ambiguous("nightly");
    qex.set_job(J1, "completed");
    let released = stdout_json(&run_with_qex(&sandbox, &qex, &["keepalive", "h", "--max-s", "30"]));
    assert_eq!(released["status"], "fired", "{released}");
    // W42-D6/D9: the item is labelled by its spec as given (`qex:nightly`); the stored input is the id.
    let summaries: Vec<&str> = released["events"].as_array().unwrap().iter().map(|event| event["summary"].as_str().unwrap()).collect();
    assert_eq!(summaries, ["item ended: qex:nightly: completed", "hold released: 1 item ended (qex:nightly)"], "{released}");
    let by_name = qex.calls().iter().filter(|call| call.starts_with("status nightly")).count();
    assert_eq!(by_name, 1, "only the creation asked by name: {:?}", qex.calls());

    // A name that is ambiguous at creation is refused, naming the cause.
    let refused = run_with_qex(&sandbox, &qex, &["hold", "--id", "again", "--for", "qex:nightly"]);
    assert_eq!(refused.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("nightly") && stderr.contains("ambiguous") && stderr.contains("names 2 jobs"), "{stderr}");
    assert!(sandbox.watch_file("again").is_none());
}

// ---- hold items (W42) and the removal of `vigil wait` ------------------------
//
// Claim ids W42-H1..W42-H5 / W42-W1 are the hold-items spec's own
// (knowledge/research/agentic-practice-2026-11/vigil-hold-items-spec.md).

#[test]
fn hold_add_and_hold_done_work_across_separate_processes_through_the_binary() {
    // why: W42-H1..H5 through the real binary and watch file. `vigil hold
    //      --for QEX --label build`, then `hold add ID --for none --label t3
    //      --expect-s 600`, then `hold done ID t3` are three separate
    //      processes; a keepalive in a fourth sees `item ended: t3: done` with
    //      the hold still live, and the qex job ending later releases it.
    // proves: W42-H1, W42-H2, W42-H3, W42-H5
    // catches: a CLI whose `hold add` / `hold done` subcommands do not exist
    //      (clap exit 2) or only mutate in memory; a `done` that releases the
    //      whole hold; an add that does not persist the item.
    let sandbox = Sandbox::new();
    let qex = FakeQex::new(&sandbox);
    qex.set_job(J1, "running");
    let created = run_with_qex(&sandbox, &qex, &["hold", "--id", "h", "--for", J1_SPEC, "--label", "build", "--expect-s", "600"]);
    assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
    let added = run_with_qex(&sandbox, &qex, &["hold", "add", "h", "--for", "none", "--label", "t3", "--expect-s", "600"]);
    assert!(added.status.success(), "hold add: {}", String::from_utf8_lossy(&added.stderr));
    let done = run_with_qex(&sandbox, &qex, &["hold", "done", "h", "t3"]);
    assert!(done.status.success(), "hold done: {}", String::from_utf8_lossy(&done.stderr));

    let envelope = stdout_json(&run_with_qex(&sandbox, &qex, &["keepalive", "h", "--max-s", "30", "--orchestrator"]));
    assert_eq!((envelope["status"].as_str(), envelope["next"].as_str()), (Some("pending"), Some("act on events")), "{envelope}");
    assert_eq!(envelope["events"][0]["summary"], "item ended: t3: done", "{envelope}");
    assert_eq!(envelope["events"][0]["kind"], "attention", "{envelope}");

    qex.set_job(J1, "completed");
    let released = stdout_json(&run_with_qex(&sandbox, &qex, &["keepalive", "h", "--max-s", "30", "--orchestrator"]));
    assert_eq!(released["status"], "fired", "{released}");
}

#[test]
fn hold_add_to_an_ended_hold_and_hold_done_with_a_wrong_label_exit_1_and_say_why() {
    // why: W42-H2, W42-H5 through the CLI: errors are exit 1 with the cause on
    //      stderr and nothing on stdout (the same contract as `hold` with a
    //      bad spec above).
    // proves: W42-H2, W42-H5
    // catches: an add that succeeds on a released hold; a `done` that exits 0
    //      for a label that names no item.
    let sandbox = Sandbox::new();
    let qex = FakeQex::new(&sandbox);
    qex.set_job(J1, "completed");
    assert!(run_with_qex(&sandbox, &qex, &["hold", "--id", "h", "--for", J1_SPEC, "--label", "build"]).status.success());
    let released = stdout_json(&run_with_qex(&sandbox, &qex, &["keepalive", "h", "--max-s", "30"]));
    assert_eq!(released["status"], "fired", "setup: {released}");
    let refused = run_with_qex(&sandbox, &qex, &["hold", "add", "h", "--for", "none", "--label", "t3"]);
    assert_eq!(refused.status.code(), Some(1), "{}", String::from_utf8_lossy(&refused.stderr));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("ended"), "{}", String::from_utf8_lossy(&refused.stderr));

    qex.set_job(J1, "running");
    assert!(run_with_qex(&sandbox, &qex, &["hold", "--id", "live", "--for", J1_SPEC, "--label", "build"]).status.success());
    let wrong = run_with_qex(&sandbox, &qex, &["hold", "done", "live", "nope"]);
    assert_eq!(wrong.status.code(), Some(1), "{}", String::from_utf8_lossy(&wrong.stderr));
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("nope"));
}

#[test]
fn an_until_item_in_the_past_ends_at_the_first_keepalive_with_an_item_event() {
    // why: W42-H5 through the real readers (no scripted reader): an `until:`
    //      item whose time has passed ends at the first tick, and the hold
    //      stays live for the other item (a marker file that does not exist).
    // proves: W42-H3, W42-H5
    // catches: until items rejected by the CLI, or ended without an event.
    let sandbox = Sandbox::new();
    let marker = sandbox.input("done.marker");
    let created = sandbox.run(&["hold", "--id", "h", "--for", &format!("file:{}", marker.display()), "--label", "marker"]);
    assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
    let added = sandbox.run(&["hold", "add", "h", "--for", "until:2020-01-01T00:00:00Z", "--label", "eta"]);
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    let envelope = stdout_json(&finish_within(piped({
        let mut command = sandbox.command();
        command.args(["keepalive", "h", "--max-s", "30"]);
        command
    }), Duration::from_secs(10)));
    assert_eq!(envelope["status"], "pending", "{envelope}");
    assert_eq!(envelope["events"][0]["summary"], "item ended: eta: reached", "{envelope}");
}

#[test]
fn vigil_wait_is_an_unknown_subcommand_and_is_not_advertised() {
    // why: W42-W1. `vigil wait` is removed: clap rejects it as an unknown
    //      subcommand (exit 2, the usage code), for a real and an unknown
    //      watch alike, and no help text offers it. Controls: `keepalive`
    //      still works on the same watch, and `hold --help` lists the new
    //      `add` and `done` subcommands.
    // proves: W42-W1
    // catches: `wait` kept as an alias or hidden subcommand (exit 1 or 0); help
    //      text that still tells agents to `wait ID`; a removal that took
    //      keepalive with it.
    let sandbox = Sandbox::new();
    create_via_cli(&sandbox, "w", &sandbox.input("w.log"));
    for args in [&["wait", "w"][..], &["wait", "ghost"], &["wait", "w", "--orchestrator"]] {
        // Bounded: while `wait` exists it blocks on the pending watch, so a hang is killed and fails.
        let mut command = sandbox.command();
        command.args(args);
        let output = finish_within(piped(command), Duration::from_secs(5));
        assert_eq!(output.status.code(), Some(2), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
        assert!(String::from_utf8_lossy(&output.stderr).contains("wait"), "{args:?}: clap names the unknown subcommand");
        assert!(output.stdout.is_empty(), "{args:?}: nothing on stdout");
    }
    for args in [&["--help"][..], &["hold", "--help"], &["keepalive", "--help"]] {
        let text = String::from_utf8_lossy(&sandbox.run(args).stdout).to_string();
        assert!(!text.contains("vigil wait") && !text.contains("`wait ID`") && !text.lines().any(|line| line.trim_start().starts_with("wait ")), "{args:?} still offers wait:\n{text}");
    }
    let hold_help = String::from_utf8_lossy(&sandbox.run(&["hold", "--help"]).stdout).to_string();
    assert!(hold_help.contains("add") && hold_help.contains("done") && hold_help.contains("--label"), "{hold_help}");
    let alive = sandbox.run(&["keepalive", "ghost", "--max-s", "30"]);
    assert_eq!(alive.status.code(), Some(1), "control: keepalive on an unknown watch is a runtime error, not a usage error");
}

#[test]
fn no_wait_code_path_is_left_in_the_source_tree() {
    // why: W42-W1 "remove the subcommand and its code paths". A whole-tree scan
    //      (the legitimate kind of source guard): the identifiers of the old
    //      delivery path must not survive in src/. Positive control: the same
    //      scan finds `fn keepalive`, so a wrong directory or an unreadable
    //      tree cannot pass it.
    // proves: W42-W1
    // catches: the CLI arm removed while `wait_uncommitted`, `wait_exit_code`
    //      or the `Command::Wait` variant live on as dead public API.
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut text = String::new();
    for entry in std::fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            text.push_str(&std::fs::read_to_string(&path).unwrap());
        }
    }
    assert!(text.contains("fn keepalive"), "control: the scan reads the source tree");
    for needle in ["wait_uncommitted", "wait_exit_code", "Command::Wait"] {
        assert!(!text.contains(needle), "{needle} is still in src/");
    }
}

// ---- spec round 1 decisions (W42 D4, D5, D6, D8) through the binary -------------

#[test]
fn hold_add_blocked_on_the_watch_lock_rechecks_the_hold_after_it_gets_the_lock() {
    // why: W42-D4 release-before-add with real processes. The test holds the
    //      watch's lock; `vigil hold add` must WAIT for it (still running,
    //      file untouched). While the lock is held, the hold is released (the
    //      test writes what a keepalive would: status fired). When the lock
    //      is dropped the add must see the released hold and refuse (exit 1,
    //      "ended"), leaving the released file byte for byte as it was.
    // proves: W42-D4
    // catches: an add that loads the watch before taking the lock (it would
    //      append to its stale pending copy and write it over the release);
    //      an add that takes no lock at all (does not wait).
    let sandbox = Sandbox::new();
    let qex = FakeQex::new(&sandbox);
    qex.set_job(J1, "running");
    assert!(run_with_qex(&sandbox, &qex, &["hold", "--id", "h", "--for", J1_SPEC, "--label", "build"]).status.success());
    let store = vigil::Store::new(sandbox.state_root());
    let guard = store.lock_watch(&vigil::WatchId::parse("h").unwrap()).unwrap();
    let mut child = piped({
        let mut command = sandbox.command();
        command.args(["hold", "add", "h", "--for", "none", "--label", "t3"]).env("PATH", qex.path());
        command
    });
    wait_for_exec(&child);
    std::thread::sleep(Duration::from_millis(600));
    assert!(child.try_wait().unwrap().is_none(), "hold add finished while the watch lock was held");

    let path = sandbox.state_root().join("watches/h.json");
    let mut raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    raw["status"] = "fired".into();
    raw["finished_at"] = raw["created_at"].clone();
    std::fs::write(&path, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();
    let released_bytes = std::fs::read(&path).unwrap();
    drop(guard);

    let output = finish_within(child, Duration::from_secs(5));
    assert_eq!(output.status.code(), Some(1), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stderr).contains("ended"), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(std::fs::read(&path).unwrap(), released_bytes, "the released hold is not rewritten");
}

#[test]
fn a_duplicate_label_exits_1_naming_it_and_a_label_with_several_for_is_refused_writing_nothing() {
    // why: W42-D5. A duplicate label is refused (exit 1 like every other runtime refusal; names the label).
    //      `--label` with more than one `--for` is refused; `--for none`
    //      without a label too (it could never be ended). Nothing is written
    //      by any refusal.
    // proves: W42-D5
    // catches: a duplicate accepted, or refused with another code; the label
    //      of a multi-`--for` hold silently given to the first item.
    let sandbox = Sandbox::new();
    let qex = FakeQex::new(&sandbox);
    qex.set_job(J1, "running");
    assert!(run_with_qex(&sandbox, &qex, &["hold", "--id", "d", "--for", J1_SPEC, "--label", "build"]).status.success());
    let before = sandbox.watch_file("d").unwrap();
    let duplicate = run_with_qex(&sandbox, &qex, &["hold", "add", "d", "--for", "none", "--label", "build"]);
    assert_eq!(duplicate.status.code(), Some(1), "{}", String::from_utf8_lossy(&duplicate.stderr));
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("build"), "{}", String::from_utf8_lossy(&duplicate.stderr));
    assert!(duplicate.stdout.is_empty());
    assert_eq!(sandbox.watch_file("d").unwrap(), before, "nothing written");

    let several = run_with_qex(&sandbox, &qex, &["hold", "--id", "m", "--for", J1_SPEC, "--for", "pid:1", "--label", "x"]);
    assert!(!several.status.success() && String::from_utf8_lossy(&several.stderr).contains("label"), "{}", String::from_utf8_lossy(&several.stderr));
    let unlabelled = run_with_qex(&sandbox, &qex, &["hold", "--id", "u", "--for", "none"]);
    assert!(!unlabelled.status.success() && String::from_utf8_lossy(&unlabelled.stderr).contains("label"), "{}", String::from_utf8_lossy(&unlabelled.stderr));
    assert!(sandbox.watch_file("m").is_none() && sandbox.watch_file("u").is_none(), "nothing written");
}

#[test]
fn expect_s_through_the_cli_raises_overdue_for_the_initial_item_and_every_item_of_several_for() {
    // why: W42-D8 + W42-D5. `--expect-s 2` (overdue at min(3, 542) = 3 s)
    //      given to `vigil hold --for A --for B` applies to EACH item, and to
    //      an item added with `hold add --expect-s 2`. Real time (separate
    //      processes): one 4 s sleep, then the first keepalive tick sees all
    //      three.
    // proves: W42-D8, W42-D5, W42-H3
    // catches: --expect-s dropped by the CLI layer; applied to the first
    //      item only; applied to the hold but not to added items.
    let sandbox = Sandbox::new();
    let qex = FakeQex::new(&sandbox);
    qex.set_job(J1, "running");
    let created = run_with_qex(&sandbox, &qex, &["hold", "--id", "h", "--for", J1_SPEC, "--for", "pid:1", "--expect-s", "2"]);
    assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
    let added = run_with_qex(&sandbox, &qex, &["hold", "add", "h", "--for", "none", "--label", "t3", "--expect-s", "2"]);
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    std::thread::sleep(Duration::from_secs(4));
    let envelope = stdout_json(&run_with_qex(&sandbox, &qex, &["keepalive", "h", "--max-s", "30", "--orchestrator"]));
    let mut summaries: Vec<String> = envelope["events"].as_array().unwrap().iter().map(|event| event["summary"].as_str().unwrap().to_string()).collect();
    summaries.sort();
    assert_eq!(summaries, vec![format!("item overdue: {J1_SPEC}"), "item overdue: pid:1".to_string(), "item overdue: t3".to_string()], "{envelope}");
    assert_eq!(envelope["status"], "pending", "{envelope}");
}

#[test]
fn an_initial_for_none_with_a_label_works_through_the_cli_and_its_last_item_raises_item_ended_with_the_release() {
    // why: W42-D8 + W42-D6. `vigil hold --for none --label x` (the first
    //      item is a T3 delegate), `hold done`, then keepalive: fired, with
    //      `item ended: x: done` AND the release event.
    // proves: W42-D8, W42-D6
    // catches: `none` accepted only by `hold add`; a last item whose event is
    //      swallowed by the release.
    let sandbox = Sandbox::new();
    let created = sandbox.run(&["hold", "--id", "n", "--for", "none", "--label", "x", "--expect-s", "600"]);
    assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
    let done = sandbox.run(&["hold", "done", "n", "x"]);
    assert!(done.status.success(), "{}", String::from_utf8_lossy(&done.stderr));
    let envelope = stdout_json(&sandbox.run(&["keepalive", "n", "--max-s", "30"]));
    assert_eq!(envelope["status"], "fired", "{envelope}");
    let summaries: Vec<&str> = envelope["events"].as_array().unwrap().iter().map(|event| event["summary"].as_str().unwrap()).collect();
    assert!(summaries.contains(&"item ended: x: done"), "{envelope}");
    assert!(summaries.iter().any(|summary| summary.starts_with("hold released:")), "{envelope}");
}

#[test]
fn hold_add_resolves_a_qex_name_to_the_full_id_and_refuses_an_ambiguous_name_through_the_cli() {
    // why: W42-D8 (S8-S10) through the binary, like `hold --for qex:nightly`
    //      (see the S3 test above). The stored watch holds the job id the name
    //      resolves to; an ambiguous name exits 1 naming the cause and writes
    //      nothing.
    // proves: W42-D8
    // catches: `hold add` storing the raw name; an ambiguous name accepted.
    let sandbox = Sandbox::new();
    let qex = FakeQex::new(&sandbox);
    qex.set_job(J1, "running");
    qex.set_name("nightly", J1);
    qex.set_ambiguous("dupe");
    assert!(run_with_qex(&sandbox, &qex, &["hold", "--id", "h", "--for", "none", "--label", "first"]).status.success());
    let added = run_with_qex(&sandbox, &qex, &["hold", "add", "h", "--for", "qex:nightly", "--label", "n"]);
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    let stored = sandbox.watch_file("h").unwrap();
    assert!(stored.to_string().contains(J1), "the full id is stored: {stored}");
    assert!(!stored.to_string().contains("\"job\":\"nightly\""), "the name is not stored as the job: {stored}");

    let refused = run_with_qex(&sandbox, &qex, &["hold", "add", "h", "--for", "qex:dupe", "--label", "d"]);
    assert_eq!(refused.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("dupe") && stderr.contains("ambiguous"), "{stderr}");
    assert_eq!(sandbox.watch_file("h").unwrap(), stored, "nothing written");
}
