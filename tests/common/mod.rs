//! Shared helpers for the end-to-end suites. Not a test file itself.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub fn vigil_bin() -> &'static str {
    env!("CARGO_BIN_EXE_vigil")
}

/// A sandbox: state root, HOME, cwd and an inputs dir, all inside one tempdir.
pub struct Sandbox {
    pub dir: tempfile::TempDir,
}

impl Sandbox {
    pub fn new() -> Sandbox {
        let sandbox = Sandbox { dir: tempfile::tempdir().unwrap() };
        for sub in ["state-home", "home", "cwd", "cwd2", "inputs", "config"] {
            std::fs::create_dir_all(sandbox.dir.path().join(sub)).unwrap();
        }
        sandbox
    }
    pub fn state_home(&self) -> PathBuf {
        self.dir.path().join("state-home")
    }
    /// `$XDG_STATE_HOME/vigil`
    pub fn state_root(&self) -> PathBuf {
        self.state_home().join("vigil")
    }
    pub fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }
    pub fn cwd(&self) -> PathBuf {
        self.dir.path().join("cwd")
    }
    /// A second working directory (V11: resume from another directory).
    pub fn cwd2(&self) -> PathBuf {
        self.dir.path().join("cwd2")
    }
    pub fn input(&self, name: &str) -> PathBuf {
        self.dir.path().join("inputs").join(name)
    }
    /// A `vigil` command with the environment pointed into the sandbox and
    /// a judge URL that nothing listens on.
    pub fn command(&self) -> Command {
        let mut command = Command::new(vigil_bin());
        self.configure(&mut command);
        command
    }
    pub fn configure(&self, command: &mut Command) {
        command
            .current_dir(self.cwd())
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.home())
            .env("XDG_STATE_HOME", self.state_home())
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            // Advisories are off in every suite that is not about them, so no
            // probe runs and nothing is written into HOME or XDG_CONFIG_HOME
            // (the confinement tests compare those byte for byte).
            .env("VIGIL_ADVISORIES_FILE", "off")
            // The orchestrator challenge has its own tests; elsewhere it is off.
            .env("VIGIL_ORCHESTRATOR_CONFIRM", "off")
            .env("VIGIL_JUDGE_URL", "http://127.0.0.1:9");
    }
    pub fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    /// Like `command()` but started from `cwd`.
    pub fn command_in(&self, cwd: &Path) -> Command {
        let mut command = self.command();
        command.current_dir(cwd);
        command
    }
    /// Every file under the sandbox EXCEPT `state_root()` (exactly
    /// `state-home/vigil`; siblings in `state-home` are still included, S24),
    /// with its bytes.
    pub fn snapshot_outside_state_root(&self) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
        let root = self.state_root();
        list_files(self.dir.path()).into_iter().filter(|(p, _)| !p.starts_with(&root)).collect()
    }
}

/// Differences between two snapshots, compared by exact bytes (X5, S24).
/// `expected` maps every path that is ALLOWED to differ to the exact bytes it
/// must hold afterwards (a file the test itself appended to, or created):
/// anything else, an unexpected append included, must be byte-identical to
/// `before`. Reports new files, vanished files and files whose bytes differ
/// from what was expected.
pub fn snapshot_diff(
    before: &std::collections::BTreeMap<PathBuf, Vec<u8>>,
    after: &std::collections::BTreeMap<PathBuf, Vec<u8>>,
    expected: &std::collections::BTreeMap<PathBuf, Vec<u8>>,
) -> Vec<String> {
    let paths: std::collections::BTreeSet<&PathBuf> = before.keys().chain(after.keys()).chain(expected.keys()).collect();
    let mut problems = Vec::new();
    for path in paths {
        let want = expected.get(path).or_else(|| before.get(path));
        match (want, after.get(path)) {
            (None, None) => {}
            (Some(w), Some(a)) if w == a => {}
            (None, Some(_)) => problems.push(format!("new file {}", path.display())),
            (Some(_), None) => problems.push(format!("removed {}", path.display())),
            (Some(w), Some(a)) => problems.push(format!("modified {} ({} bytes expected, {} found)", path.display(), w.len(), a.len())),
        }
    }
    problems
}

pub fn stdout_json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {:?} stderr {:?}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr)))
}

/// Appends a line to `path` every 100 ms until dropped, so a change always
/// lands after the watch's baseline tick without the test guessing timing.
pub struct Appender {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Appender {
    pub fn start(path: &Path, line: &'static str) -> Appender {
        use std::io::Write;
        let stop = Arc::new(AtomicBool::new(false));
        let (flag, path) = (stop.clone(), path.to_path_buf());
        let thread = std::thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path).unwrap();
                writeln!(file, "{line}").unwrap();
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        Appender { stop, thread: Some(thread) }
    }
}

impl Drop for Appender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Async twin of `wait_until`: polls with `tokio::time::sleep`, so it never
/// blocks the runtime thread (T3: a blocking wait on a current-thread runtime
/// keeps the task that would satisfy the condition from running).
pub async fn wait_until_async(limit: Duration, mut check: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < limit {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Poll `check` every 50 ms for up to `limit`; real time, used only to wait
/// for another process to reach a state. BLOCKS the calling thread: use
/// `wait_until_async` inside async tests.
pub fn wait_until(limit: Duration, mut check: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < limit {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

pub fn list_files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(list_files(&path));
        } else {
            out.push((path.clone(), std::fs::read(&path).unwrap()));
        }
    }
    out.sort();
    out
}

/// The CLI `create` JSON for a fire=change watch on `path`.
pub fn change_watch_json(id: &str, path: &Path, interval_s: u64) -> String {
    serde_json::json!({
        "id": id,
        "prompt": "",
        "inputs": [{"kind": "file", "path": path}],
        "interval_s": interval_s,
        "deadline_s": 3600,
        "fire": "change"
    })
    .to_string()
}

impl Sandbox {
    /// Same environment as `command()`, for rmcp's tokio child transport.
    pub fn tokio_command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(vigil_bin());
        command
            .current_dir(self.cwd())
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.home())
            .env("XDG_STATE_HOME", self.state_home())
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            // Advisories are off in every suite that is not about them, so no
            // probe runs and nothing is written into HOME or XDG_CONFIG_HOME
            // (the confinement tests compare those byte for byte).
            .env("VIGIL_ADVISORIES_FILE", "off")
            // The orchestrator challenge has its own tests; elsewhere it is off.
            .env("VIGIL_ORCHESTRATOR_CONFIRM", "off")
            .env("VIGIL_JUDGE_URL", "http://127.0.0.1:9");
        command
    }

    /// Read a field from a watch file written by any vigil process.
    pub fn watch_file(&self, id: &str) -> Option<serde_json::Value> {
        let bytes = std::fs::read(self.state_root().join("watches").join(format!("{id}.json"))).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

/// Run a spawned child to completion, killing it if it exceeds `limit`.
pub fn finish_within(mut child: std::process::Child, limit: Duration) -> Output {
    let finished = wait_until(limit, || matches!(child.try_wait(), Ok(Some(_))));
    if !finished {
        let _ = child.kill();
        let _ = child.wait();
        panic!("child did not exit within {limit:?}");
    }
    child.wait_with_output().unwrap()
}

/// Dead = gone, or a zombie waiting for someone else to reap it. Read from
/// /proc, independent of the code under test.
pub fn proc_is_dead(pid: u32) -> bool {
    let Ok(line) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { return true };
    line.rfind(')').and_then(|end| line[end + 1..].trim_start().chars().next()).is_none_or(|state| state == 'Z')
}

/// A watch whose only input is a command that, once `go` exists, records its
/// pid in `pid_file` and sleeps 60 s (otherwise it prints `ok` at once). The
/// create-time baseline runs without `go`, so create returns promptly; the
/// test then creates `go` to make the next tick hang in a long-lived child.
pub fn hanging_cmd_watch_json(id: &str, go: &Path, pid_file: &Path) -> String {
    let script = format!("if [ -e '{}' ]; then echo $$ > '{}'; sleep 60; fi; echo ok", go.display(), pid_file.display());
    serde_json::json!({
        "id": id,
        "inputs": [{"kind": "cmd", "argv": ["sh", "-c", script], "timeout_s": 19}],
        "interval_s": 1,
        "deadline_s": 3600,
        "fire": "change"
    })
    .to_string()
}

/// Wait for a pid file written by a test command and parse it.
pub fn wait_for_pid(pid_file: &Path, limit: Duration) -> u32 {
    assert!(wait_until(limit, || std::fs::read_to_string(pid_file).is_ok_and(|text| text.trim().parse::<u32>().is_ok())), "no pid in {}", pid_file.display());
    std::fs::read_to_string(pid_file).unwrap().trim().parse().unwrap()
}

pub fn send_signal(signal: &str, pid: u32) {
    assert!(Command::new("kill").args([signal, &pid.to_string()]).status().unwrap().success());
}

// ---- disk full (specs/disk-full.md) ------------------------------------

/// The seam a spawned `vigil` reads its write-fault plan from (src/fault.rs
/// documents the syntax). Set on one command at a time, so a test chooses
/// which PROCESS sees a full disk; unset, vigil behaves as in production.
pub const FAULT_ENV: &str = "VIGIL_TEST_FAULT";

/// A disk that is full and stays full, for the watch file and the reserve.
pub const DISK_STAYS_FULL: &str = "watch:28:all,reserve:28:all";

impl Sandbox {
    pub fn reserve(&self) -> PathBuf {
        self.state_root().join(".reserve")
    }
    pub fn watch_path(&self, id: &str) -> PathBuf {
        self.state_root().join("watches").join(format!("{id}.json"))
    }
    pub fn watch_bytes(&self, id: &str) -> Vec<u8> {
        std::fs::read(self.watch_path(id)).unwrap()
    }
    /// Every `.tmp` file under the state root (none when it does not exist).
    pub fn tmp_files(&self) -> Vec<PathBuf> {
        if !self.state_root().exists() {
            return Vec::new();
        }
        list_files(&self.state_root()).into_iter().map(|(path, _)| path).filter(|path| path.to_string_lossy().ends_with(".tmp")).collect()
    }
    /// The plan text for a disk that is full only while the reserve exists and
    /// cannot make a new one: only deleting the reserve lets a write through.
    pub fn only_the_reserve_to_give(&self) -> String {
        format!("watch:28:while={},reserve:28:all", self.reserve().display())
    }
    /// A `vigil` command whose store writes fail by `plan`.
    pub fn faulted_command(&self, plan: &str) -> Command {
        let mut command = self.command();
        command.env(FAULT_ENV, plan);
        command
    }
    pub fn tokio_faulted_command(&self, plan: &str) -> tokio::process::Command {
        let mut command = self.tokio_command();
        command.env(FAULT_ENV, plan);
        command
    }
}

/// Run `command` to completion (stdout and stderr captured) within `limit`.
pub fn output_within(mut command: Command, limit: Duration) -> Output {
    command.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    finish_within(command.spawn().unwrap(), limit)
}

/// Non-empty stderr lines.
pub fn stderr_lines(output: &Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stderr).lines().filter(|line| !line.trim().is_empty()).map(str::to_string).collect()
}

/// The `persist` note of a failed write, field by field against the spec
/// (DF2, DF3a, S4): `ok`, `kind`, `error`, `path`, plus `redelivery: true`
/// exactly when something is at risk (the envelope has events, or a terminal
/// status that was not saved); with `redelivery == false` the key must be
/// ABSENT. `retried`: `Some(true)` when the rescue deleted the reserve and the
/// retry failed too, so the note also holds `first_error` (S4); `Some(false)`
/// when no retry was made, so the key is ABSENT; `None` when real time decides
/// (a CLI call).
pub fn assert_failed_persist(envelope: &serde_json::Value, kind: &str, os_text: &str, file: &Path, redelivery: bool, retried: Option<bool>) {
    let persist = envelope.get("persist").unwrap_or_else(|| panic!("the envelope has no `persist` field: {envelope}"));
    let object = persist.as_object().unwrap_or_else(|| panic!("persist is not an object: {persist}"));
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort();
    let mut expected: Vec<&str> = vec!["error", "kind", "ok", "path"];
    if redelivery {
        expected.push("redelivery");
    }
    match retried {
        Some(true) => expected.push("first_error"),
        Some(false) => {}
        None => {
            if keys.contains(&"first_error") {
                expected.push("first_error");
            }
        }
    }
    expected.sort();
    assert_eq!(keys, expected, "redelivery {redelivery}, retried {retried:?}: {persist}");
    if redelivery {
        assert_eq!(persist["redelivery"], true, "{persist}");
    }
    assert_eq!(persist["ok"], false, "{persist}");
    assert_eq!(persist["kind"], kind, "{persist}");
    assert_eq!(persist["path"], file.to_str().unwrap(), "{persist}");
    let error = persist["error"].as_str().unwrap_or_else(|| panic!("error is not a string: {persist}"));
    assert!(error.contains(os_text), "error {error:?} should carry the OS message {os_text:?}");
    if retried == Some(true) {
        let first = persist["first_error"].as_str().unwrap_or_else(|| panic!("first_error is not a string: {persist}"));
        assert!(first.contains(os_text), "first_error {first:?} should carry the OS message {os_text:?}");
    }
}

/// DF4: the retry succeeded on the reserve's space: exactly these three keys.
pub fn reserve_used_note() -> serde_json::Value {
    serde_json::json!({"ok": true, "used_reserve": true, "kind": "disk_full"})
}

/// A stand-in `qex` for hold tests (W36) that answers the way the installed
/// qex does (outputs captured 2026-11-12, `tests/fixtures/qex`):
///
/// - `qex status REF --json --no-logs` prints the full real status record with
///   `id` and `state` replaced, for a job set with `set_job`; a name set with
///   `set_name` answers for its job; a name set with `set_ambiguous` answers
///   exit 127 and the real "names N jobs" text; a job set with `set_removed`
///   answers exit 127 and the real "existed ... deleted its record" text;
///   anything else is exit 127 and "there is no job or pipeline ...".
/// - `qex list ARGS` prints what `set_list("ARGS", output)` stored for exactly
///   those arguments; any other query is exit 2, so a hold that asks the wrong
///   question fails instead of getting a convenient `[]`.
///
/// Every call's arguments are appended to `calls.log`. Put it first on a
/// command's PATH with `path`.
pub struct FakeQex {
    dir: PathBuf,
}

const STATUS_RECORD: &str = include_str!("../fixtures/qex/status_completed.json");
const LIST_RECORD: &str = include_str!("../fixtures/qex/list_active_running.json");
const REMOVED_TEXT: &str = include_str!("../fixtures/qex/status_removed_completed.err");
const AMBIGUOUS_TEXT: &str = include_str!("../fixtures/qex/status_ambiguous.err");
/// The id inside `status_completed.json` and `status_removed_completed.err`.
const FIXTURE_ID: &str = "0d31c8ac-25e4-4ab2-8a3b-9b24ccd533c9";

fn list_key(args: &str) -> String {
    args.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect()
}

impl FakeQex {
    pub fn new(sandbox: &Sandbox) -> FakeQex {
        let dir = sandbox.dir.path().join("fake-qex");
        for sub in ["jobs", "alias", "removed", "ambiguous", "list", "bin"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let script = format!(
            r#"#!/bin/sh
d='{d}'
echo "$*" >> "$d/calls.log"
case "$1" in
  status)
    ref="$2"
    id="$ref"
    if [ -f "$d/alias/$ref" ]; then id=$(cat "$d/alias/$ref"); fi
    if [ -f "$d/jobs/$id" ]; then cat "$d/jobs/$id"
    elif [ -f "$d/removed/$id" ]; then cat "$d/removed/$id" >&2; exit 127
    elif [ -f "$d/ambiguous/$ref" ]; then cat "$d/ambiguous/$ref" >&2; exit 127
    else echo "qex: there is no job or pipeline with the id or the name \`$ref\`" >&2; exit 127; fi ;;
  list)
    key=$(printf '%s' "$*" | tr -c 'A-Za-z0-9.-' '_')
    if [ -f "$d/list/$key" ]; then cat "$d/list/$key"; else echo "fake qex: unexpected $*" >&2; exit 2; fi ;;
  *) echo "fake qex: unexpected $*" >&2; exit 2 ;;
esac
"#,
            d = dir.display()
        );
        let bin = dir.join("bin/qex");
        std::fs::write(&bin, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        FakeQex { dir }
    }
    /// Job `id` is in `state`: `qex status id` prints the real record shape.
    pub fn set_job(&self, id: &str, state: &str) {
        let mut record: serde_json::Value = serde_json::from_str(STATUS_RECORD).unwrap();
        record["id"] = id.into();
        record["state"] = state.into();
        std::fs::write(self.dir.join("jobs").join(id), serde_json::to_string_pretty(&record).unwrap() + "\n").unwrap();
        let _ = std::fs::remove_file(self.dir.join("removed").join(id));
    }
    /// `qex status NAME` (or a prefix) answers for job `id`.
    pub fn set_name(&self, name: &str, id: &str) {
        std::fs::write(self.dir.join("alias").join(name), id).unwrap();
    }
    /// `qex status NAME` is ambiguous between two jobs, in qex's own words.
    pub fn set_ambiguous(&self, name: &str) {
        let _ = std::fs::remove_file(self.dir.join("alias").join(name));
        let text = AMBIGUOUS_TEXT.replace("vigil-fixture", name);
        std::fs::write(self.dir.join("ambiguous").join(name), text).unwrap();
    }
    /// Job `id` finished and `qex clean` removed its record: `qex status id` now
    /// says so, in qex's own words, and keeps the `completed` state.
    pub fn set_removed(&self, id: &str) {
        let _ = std::fs::remove_file(self.dir.join("jobs").join(id));
        std::fs::write(self.dir.join("removed").join(id), REMOVED_TEXT.replace(FIXTURE_ID, id)).unwrap();
    }
    /// `qex list ARGS` (the arguments after `list`, joined by spaces) prints
    /// `output` exactly.
    pub fn set_list(&self, args: &str, output: &str) {
        std::fs::write(self.dir.join("list").join(list_key(&format!("list {args}"))), output).unwrap();
    }
    /// `count` records shaped like `qex list --json` entries (the captured running
    /// job re-keyed), as qex prints them, in state `state`, for `set_list`.
    pub fn list_of(count: usize, state: &str) -> String {
        let template = serde_json::from_str::<Vec<serde_json::Value>>(LIST_RECORD).unwrap().remove(0);
        let jobs: Vec<_> = (0..count)
            .map(|index| {
                let mut job = template.clone();
                job["id"] = format!("{index:08x}-6827-4b1d-ba7f-1abc7fc9c0f8").into();
                job["state"] = state.into();
                job
            })
            .collect();
        serde_json::to_string_pretty(&jobs).unwrap() + "\n"
    }
    /// The arguments of every call so far, one string per call.
    pub fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("calls.log")).unwrap_or_default().lines().map(str::to_string).collect()
    }
    /// The PATH value that finds this `qex` first.
    pub fn path(&self) -> String {
        format!("{}:{}", self.dir.join("bin").display(), std::env::var("PATH").unwrap_or_default())
    }
}
