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
        for sub in ["state-home", "home", "cwd", "inputs", "config"] {
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

/// Poll `check` every 50 ms for up to `limit`; real time, used only to wait
/// for another process to reach a state.
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
