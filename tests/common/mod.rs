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
/// (DF2, DF3a): `ok`, `kind`, `error`, `path`, plus `redelivery: true` exactly
/// when something is at risk (the envelope has events, or a terminal status
/// that was not saved); with `redelivery == false` the key must be ABSENT.
pub fn assert_failed_persist(envelope: &serde_json::Value, kind: &str, os_text: &str, file: &Path, redelivery: bool) {
    let persist = envelope.get("persist").unwrap_or_else(|| panic!("the envelope has no `persist` field: {envelope}"));
    let object = persist.as_object().unwrap_or_else(|| panic!("persist is not an object: {persist}"));
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort();
    if redelivery {
        assert_eq!(keys, ["error", "kind", "ok", "path", "redelivery"], "{persist}");
        assert_eq!(persist["redelivery"], true, "{persist}");
    } else {
        assert_eq!(keys, ["error", "kind", "ok", "path"], "nothing is at risk, so no `redelivery` key: {persist}");
    }
    assert_eq!(persist["ok"], false, "{persist}");
    assert_eq!(persist["kind"], kind, "{persist}");
    assert_eq!(persist["path"], file.to_str().unwrap(), "{persist}");
    let error = persist["error"].as_str().unwrap_or_else(|| panic!("error is not a string: {persist}"));
    assert!(error.contains(os_text), "error {error:?} should carry the OS message {os_text:?}");
}

/// DF4: the retry succeeded on the reserve's space: exactly these three keys.
pub fn reserve_used_note() -> serde_json::Value {
    serde_json::json!({"ok": true, "used_reserve": true, "kind": "disk_full"})
}
