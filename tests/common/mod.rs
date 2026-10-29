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
