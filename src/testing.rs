//! Test infrastructure shared by unit tests and `tests/`: injectable clocks,
//! a scripted in-memory judge, and a tiny threaded HTTP server that plays
//! Laya (or any URL input). Not product code.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::clock::{Clock, Timestamp};
use crate::config::Config;
use crate::inputs::{read_input, InputReader, Reading};
use crate::judge::{Answer, Answers, BoxFuture, Judge, JudgeError, JudgeRequest};
use crate::watch::{Input, InputState};

/// `2026-10-29T14:03:11Z`, the instant used in the SPEC envelope example.
pub fn spec_instant() -> Timestamp {
    at("2026-10-29T14:03:11Z")
}

pub fn at(rfc3339: &str) -> Timestamp {
    Timestamp(DateTime::parse_from_rfc3339(rfc3339).unwrap().with_timezone(&Utc))
}

/// Config with shipped defaults spelled out (so tests do not depend on
/// `Config::from_lookup`).
pub fn default_config() -> Config {
    Config {
        judge_url: "http://127.0.0.1:4010".into(),
        judge_state_bytes: 2560,
        digest_max_bytes: 8192,
        judge_timeout: Duration::from_secs(10),
        health_timeout: Duration::from_secs(2),
        claude_projects_dir: None,
        uw_command: "uw".into(),
        on_credit_change: None,
        credit_check_every: Duration::from_secs(240),
        credit_hook_timeout: Duration::from_secs(10),
        advisories_file: None,
        home: None,
    }
}

/// A clock that only moves when told to.
pub struct ManualClock(Mutex<Timestamp>);

impl ManualClock {
    pub fn new(start: Timestamp) -> ManualClock {
        ManualClock(Mutex::new(start))
    }
    pub fn advance_secs(&self, secs: u64) {
        let mut now = self.0.lock().unwrap();
        *now = now.plus_secs(secs);
    }
    pub fn set(&self, to: Timestamp) {
        *self.0.lock().unwrap() = to;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        *self.0.lock().unwrap()
    }
}

/// Wall clock that follows tokio's (possibly paused) virtual time: use with
/// `#[tokio::test(start_paused = true)]` so `sleep(270s)` also moves `now()`.
pub struct TokioClock {
    start_wall: Timestamp,
    start_instant: tokio::time::Instant,
}

impl TokioClock {
    pub fn new(start_wall: Timestamp) -> TokioClock {
        TokioClock { start_wall, start_instant: tokio::time::Instant::now() }
    }
}

impl Clock for TokioClock {
    fn now(&self) -> Timestamp {
        let elapsed = tokio::time::Instant::now() - self.start_instant;
        Timestamp(self.start_wall.0 + chrono::Duration::from_std(elapsed).unwrap())
    }
}

pub fn noul_answers(question: &str, noul: f64, confidence: f64) -> Answers {
    let mut answers = Answers::new();
    answers.insert(question.to_string(), Answer::Noul { noul, confidence });
    answers
}

struct ScriptState {
    script: VecDeque<(Duration, Result<Answers, JudgeError>)>,
    fallback: Result<Answers, JudgeError>,
    requests: Vec<JudgeRequest>,
    healthy: bool,
}

/// In-memory judge: replays `push`ed results, then the fallback; records every
/// request. Safe under paused tokio time (no sockets).
pub struct ScriptedJudge(Mutex<ScriptState>);

impl ScriptedJudge {
    /// Fallback answer is "unreachable" so an unscripted call is loud.
    pub fn new() -> ScriptedJudge {
        ScriptedJudge(Mutex::new(ScriptState {
            script: VecDeque::new(),
            fallback: Err(JudgeError::Unreachable("unscripted".into())),
            requests: Vec::new(),
            healthy: true,
        }))
    }
    pub fn push(&self, result: Result<Answers, JudgeError>) {
        self.0.lock().unwrap().script.push_back((Duration::ZERO, result));
    }
    /// Like `push`, but the answer arrives only after `delay` of (tokio, so
    /// possibly virtual) time: a slow judge for budget tests (V2).
    pub fn push_delayed(&self, delay: Duration, result: Result<Answers, JudgeError>) {
        self.0.lock().unwrap().script.push_back((delay, result));
    }
    pub fn set_fallback(&self, result: Result<Answers, JudgeError>) {
        self.0.lock().unwrap().fallback = result;
    }
    pub fn set_healthy(&self, healthy: bool) {
        self.0.lock().unwrap().healthy = healthy;
    }
    pub fn requests(&self) -> Vec<JudgeRequest> {
        self.0.lock().unwrap().requests.clone()
    }
    pub fn call_count(&self) -> usize {
        self.0.lock().unwrap().requests.len()
    }
}

impl Judge for ScriptedJudge {
    fn predict<'a>(&'a self, request: &'a JudgeRequest) -> BoxFuture<'a, Result<Answers, JudgeError>> {
        Box::pin(async move {
            let (delay, result) = {
                let mut state = self.0.lock().unwrap();
                state.requests.push(request.clone());
                match state.script.pop_front() {
                    Some(next) => next,
                    None => (Duration::ZERO, state.fallback.clone()),
                }
            };
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            result
        })
    }
    fn healthy(&self) -> BoxFuture<'_, bool> {
        Box::pin(async move { self.0.lock().unwrap().healthy })
    }
}

/// A gate a scripted read waits on: closed until `open()`. Lets a test hold
/// one watch's sensing "in flight" for exactly as long as it wants, with no
/// sleeps and no dependence on tokio's (paused) time (X3).
#[derive(Clone)]
pub struct Gate(Arc<tokio::sync::Semaphore>);

impl Gate {
    pub fn closed() -> Gate {
        Gate(Arc::new(tokio::sync::Semaphore::new(0)))
    }
    /// Let every current and future waiter through.
    pub fn open(&self) {
        self.0.add_permits(1 << 20);
    }
    async fn pass(&self) {
        self.0.acquire().await.expect("gate semaphore is never closed").forget();
    }
}

/// One read the `ScriptedReader` served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadRecord {
    pub label: String,
    /// Tokio time (virtual under `start_paused`) when the read began.
    pub started: tokio::time::Instant,
    /// `None` while the read is still in flight (or was dropped mid-way).
    pub finished: Option<tokio::time::Instant>,
}

#[derive(Default)]
struct ReaderScript {
    text: Option<String>,
    sleep: Duration,
    gate: Option<Gate>,
}

#[derive(Default)]
struct ReaderState {
    scripts: std::collections::HashMap<String, ReaderScript>,
    reads: Vec<ReadRecord>,
}

/// An `InputReader` for tests (X3, X4). A label with a script (`set_text`,
/// `set_sleep`, `set_gate`) is served by the script: wait for its gate, sleep
/// its (tokio, so possibly virtual) duration, then report its text; the
/// reading is `changed` when the text differs from the previous reading's
/// stored text (the first read of an input is a baseline, never a change).
/// Only `cmd` inputs can be scripted, keyed `cmd:<argv joined by spaces>`;
/// every other input (and every unscripted `cmd`) goes to the real
/// `read_input`, so file inputs still behave like files.
pub struct ScriptedReader {
    state: Mutex<ReaderState>,
    started: tokio::sync::Notify,
}

impl ScriptedReader {
    pub fn new() -> ScriptedReader {
        ScriptedReader { state: Mutex::new(ReaderState::default()), started: tokio::sync::Notify::new() }
    }
    /// What the input with `label` reads (and so whether it changed).
    pub fn set_text(&self, label: &str, text: &str) {
        self.state.lock().unwrap().scripts.entry(label.to_string()).or_default().text = Some(text.to_string());
    }
    /// How long each read of `label` takes (tokio time).
    pub fn set_sleep(&self, label: &str, sleep: Duration) {
        self.state.lock().unwrap().scripts.entry(label.to_string()).or_default().sleep = sleep;
    }
    /// Reads of `label` wait for this gate before anything else.
    pub fn set_gate(&self, label: &str, gate: Option<Gate>) {
        self.state.lock().unwrap().scripts.entry(label.to_string()).or_default().gate = gate;
    }
    /// Every scripted read so far, in start order.
    pub fn reads(&self) -> Vec<ReadRecord> {
        self.state.lock().unwrap().reads.clone()
    }
    /// Reads of `label` started so far.
    pub fn started_count(&self, label: &str) -> usize {
        self.state.lock().unwrap().reads.iter().filter(|r| r.label == label).count()
    }
    /// Wait until at least `count` reads of `label` have started.
    pub async fn wait_started(&self, label: &str, count: usize) {
        loop {
            let notified = self.started.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.started_count(label) >= count {
                return;
            }
            notified.await;
        }
    }
}

/// The script key of an input: `cmd:<argv joined by spaces>`, the same text
/// `Input::label()` gives a `cmd` input. Only `cmd` inputs can be scripted;
/// the key is computed here so the fixture works before `label()` does.
fn script_key(input: &Input) -> Option<String> {
    match input {
        Input::Cmd { argv, .. } => Some(format!("cmd:{}", argv.join(" "))),
        _ => None,
    }
}

impl InputReader for ScriptedReader {
    fn read<'a>(&'a self, input: &'a Input, previous: &'a InputState) -> BoxFuture<'a, Reading> {
        Box::pin(async move {
            let Some(label) = script_key(input) else {
                return read_input(input, previous).await;
            };
            let script = {
                let mut state = self.state.lock().unwrap();
                match state.scripts.get(&label) {
                    None => None,
                    Some(script) => {
                        let copy = (script.text.clone(), script.sleep, script.gate.clone());
                        let index = state.reads.len();
                        state.reads.push(ReadRecord { label: label.clone(), started: tokio::time::Instant::now(), finished: None });
                        Some((copy, index))
                    }
                }
            };
            let Some(((text, sleep, gate), index)) = script else {
                return read_input(input, previous).await;
            };
            self.started.notify_waiters();
            if let Some(gate) = gate {
                gate.pass().await;
            }
            if !sleep.is_zero() {
                tokio::time::sleep(sleep).await;
            }
            let text = text.unwrap_or_else(|| "ok".to_string());
            let changed = previous.hash.as_ref().is_some_and(|h| *h != text);
            let mut state = InputState::default();
            state.hash = Some(text.clone());
            self.state.lock().unwrap().reads[index].finished = Some(tokio::time::Instant::now());
            Reading { label, text, changed, is_error: false, want_met: None, state }
        })
    }
}

/// Deletion events (inotify `IN_DELETE`) in the exact order the kernel saw
/// them across several directories, from ONE inotify instance. This is how a
/// test proves "A was deleted before B" (X3 prune ordering) without sampling.
pub struct DeletionLog {
    fd: i32,
    dirs: std::collections::HashMap<i32, String>,
}

unsafe extern "C" {
    fn inotify_init1(flags: i32) -> i32;
    fn inotify_add_watch(fd: i32, path: *const std::ffi::c_char, mask: u32) -> i32;
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    fn close(fd: i32) -> i32;
}

impl DeletionLog {
    const IN_DELETE: u32 = 0x200;
    const IN_NONBLOCK: i32 = 0o4000;
    const IN_CLOEXEC: i32 = 0o2000000;

    /// Watch each `(label, directory)` for deletions. Directories must exist.
    pub fn start(dirs: &[(&str, &Path)]) -> DeletionLog {
        use std::os::unix::ffi::OsStrExt;
        let fd = unsafe { inotify_init1(Self::IN_NONBLOCK | Self::IN_CLOEXEC) };
        assert!(fd >= 0, "inotify_init1 failed: {}", std::io::Error::last_os_error());
        let mut map = std::collections::HashMap::new();
        for (label, dir) in dirs {
            let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).unwrap();
            let wd = unsafe { inotify_add_watch(fd, c.as_ptr(), Self::IN_DELETE) };
            assert!(wd >= 0, "inotify_add_watch({}) failed: {}", dir.display(), std::io::Error::last_os_error());
            map.insert(wd, label.to_string());
        }
        DeletionLog { fd, dirs: map }
    }

    /// Everything deleted since `start` (or the previous `drain`), as
    /// `"<label>/<file name>"`, oldest first.
    pub fn drain(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = unsafe { read(self.fd, buf.as_mut_ptr(), buf.len()) };
            if n <= 0 {
                break;
            }
            let mut offset = 0usize;
            while offset + 16 <= n as usize {
                let wd = i32::from_ne_bytes(buf[offset..offset + 4].try_into().unwrap());
                let len = u32::from_ne_bytes(buf[offset + 12..offset + 16].try_into().unwrap()) as usize;
                let name_bytes = &buf[offset + 16..offset + 16 + len];
                let name = String::from_utf8_lossy(name_bytes.split(|b| *b == 0).next().unwrap_or(&[])).to_string();
                if let Some(label) = self.dirs.get(&wd) {
                    out.push(format!("{label}/{name}"));
                }
                offset += 16 + len;
            }
        }
        out
    }
}

impl Drop for DeletionLog {
    fn drop(&mut self) {
        unsafe { close(self.fd) };
    }
}

#[derive(Debug, Clone)]
pub enum Behavior {
    Respond { status: u16, body: String },
    /// Accept and never answer.
    Hang,
    /// `302` with this `Location` and an empty body.
    Redirect { location: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub body: String,
}

struct ServerShared {
    behavior: Mutex<Behavior>,
    requests: Mutex<Vec<RecordedRequest>>,
    stop: AtomicBool,
}

/// Threaded HTTP/1.1 server on 127.0.0.1:0. `stop()` closes the listener, so
/// later connections are refused.
pub struct FakeHttpServer {
    addr: SocketAddr,
    shared: Arc<ServerShared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeHttpServer {
    pub fn start(behavior: Behavior) -> FakeHttpServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = Arc::new(ServerShared {
            behavior: Mutex::new(behavior),
            requests: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
        });
        let thread_shared = shared.clone();
        let thread = std::thread::spawn(move || {
            let mut parked: Vec<TcpStream> = Vec::new();
            while !thread_shared.stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let shared = thread_shared.clone();
                        if let Some(held) = serve_one(stream, &shared) {
                            parked.push(held);
                        }
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(2)),
                }
            }
        });
        FakeHttpServer { addr, shared, thread: Some(thread) }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
    pub fn port(&self) -> u16 {
        self.addr.port()
    }
    pub fn set_behavior(&self, behavior: Behavior) {
        *self.shared.behavior.lock().unwrap() = behavior;
    }
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.shared.requests.lock().unwrap().clone()
    }
    pub fn request_count(&self) -> usize {
        self.shared.requests.lock().unwrap().len()
    }
    /// Close the listener and wait for the thread; connections now refuse.
    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for FakeHttpServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Returns the stream if it must be kept open (Hang).
fn serve_one(mut stream: TcpStream, shared: &ServerShared) -> Option<TcpStream> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        raw.extend_from_slice(&chunk[..n]);
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let content_length: usize = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);
    while raw.len() < header_end + content_length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&raw[header_end..]).to_string();
    shared.requests.lock().unwrap().push(RecordedRequest { method, path, body });
    let behavior = shared.behavior.lock().unwrap().clone();
    match behavior {
        Behavior::Hang => Some(stream),
        Behavior::Redirect { location } => {
            let reply = format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(reply.as_bytes());
            None
        }
        Behavior::Respond { status, body } => {
            let reply = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes());
            None
        }
    }
}

/// Laya's response for one `noul` question (FINDINGS section 1, literal).
pub fn laya_noul_body(question: &str, noul: f64, confidence: f64) -> String {
    format!(
        r#"{{"model":"laya-rl-agent","answers":{{"{question}":{{"type":"noul","noul":{noul},"confidence":{confidence},"action":{{"act_probability":0.71}}}}}},"usage":{{"input_tokens":120,"output_tokens":0}}}}"#
    )
}

// ---- fixtures -------------------------------------------------------------

use std::path::{Path, PathBuf};

use crate::engine::Vigil;
use crate::judge::{JudgeRule, RuleTest};
use crate::watch::{CreateRequest, FireKind, WatchId};

pub fn wid(raw: &str) -> WatchId {
    WatchId::parse(raw).unwrap()
}

/// A tempdir holding the state root (`state/`) and a place for inputs, with a
/// `Vigil` wired to the given clock and a `ScriptedJudge`.
pub struct Env {
    pub dir: tempfile::TempDir,
    pub judge: Arc<ScriptedJudge>,
    /// Shared by `vigil` and every `another_process()`; unscripted labels fall
    /// through to the real `read_input`.
    pub reader: Arc<ScriptedReader>,
    pub clock: Arc<dyn Clock>,
    pub config: Config,
    pub vigil: Vigil,
}

impl Env {
    pub fn with_clock(clock: Arc<dyn Clock>) -> Env {
        Env::with_clock_and_config(clock, default_config())
    }
    pub fn with_clock_and_config(clock: Arc<dyn Clock>, config: Config) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let judge = Arc::new(ScriptedJudge::new());
        let reader = Arc::new(ScriptedReader::new());
        let vigil = Vigil::new(dir.path().join("state"), clock.clone(), judge.clone(), config.clone()).with_reader(reader.clone());
        Env { dir, judge, reader, clock, config, vigil }
    }
    pub fn root(&self) -> PathBuf {
        self.dir.path().join("state")
    }
    /// Path for an input file inside the tempdir but outside the state root.
    pub fn input_path(&self, name: &str) -> PathBuf {
        self.dir.path().join("inputs").join(name)
    }
    /// A second `Vigil` over the same root, clock, judge, reader AND
    /// configuration: a second process. (S22: it used to fall back to
    /// `default_config()`, silently discarding the Env's limits.)
    pub fn another_process(&self) -> Vigil {
        Vigil::new(self.root(), self.clock.clone(), self.judge.clone(), self.config.clone()).with_reader(self.reader.clone())
    }
    /// Like `another_process`, started from the working directory `cwd`
    /// (V11: relative inputs resolve against the creating process's cwd).
    pub fn another_process_in(&self, cwd: &Path) -> Vigil {
        self.another_process().with_cwd(cwd.to_path_buf())
    }
}

/// Pids currently BLOCKED waiting for an flock on `lock_file`, read from the
/// kernel's lock table (`/proc/locks`, `N: -> FLOCK ADVISORY WRITE <pid>
/// <maj>:<min>:<inode> ...`; the `->` marks a waiter). This is how tests prove
/// contention instead of guessing with sleeps (S25).
pub fn blocked_flock_waiters(lock_file: &Path) -> Vec<u32> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(lock_file).unwrap();
    let (dev, inode) = (metadata.dev(), metadata.ino());
    // glibc major()/minor() for a 64-bit dev_t.
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    let table = std::fs::read_to_string("/proc/locks").unwrap_or_default();
    let mut waiters = Vec::new();
    for line in table.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.get(1) != Some(&"->") {
            continue;
        }
        // N: -> FLOCK ADVISORY WRITE pid maj:min:ino start end
        let (Some(pid), Some(id)) = (tokens.get(5), tokens.get(6)) else { continue };
        let mut parts = id.split(':');
        let (Some(maj), Some(min), Some(ino)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let same = u64::from_str_radix(maj, 16).ok() == Some(major)
            && u64::from_str_radix(min, 16).ok() == Some(minor)
            && ino.parse::<u64>().ok() == Some(inode);
        if same {
            if let Ok(pid) = pid.parse() {
                waiters.push(pid);
            }
        }
    }
    waiters
}

pub fn write_file(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

pub fn append_file(path: &Path, text: &str) {
    use std::io::Write;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
    file.write_all(text.as_bytes()).unwrap();
}

/// fire = change on one file input.
pub fn change_request(id: &str, path: &Path, interval_s: u64) -> CreateRequest {
    CreateRequest {
        id: Some(id.to_string()),
        prompt: String::new(),
        inputs: vec![Input::File { path: path.to_path_buf(), tail_bytes: None }],
        interval_s: Some(interval_s),
        deadline_s: Some(3600),
        fire: FireKind::Change,
        judge_questions: None,
        judge_rule: None,
        expect_s: None,
        stall_s: None,
        orchestrator: None,
    }
}

/// The SPEC example question: `failed` (noul), fire when noul >= 0.8.
pub fn failed_questions() -> serde_json::Map<String, serde_json::Value> {
    serde_json::json!({"failed": {"type": "noul", "instructions": "Did the build fail?"}})
        .as_object()
        .unwrap()
        .clone()
}

pub fn failed_rule() -> JudgeRule {
    JudgeRule { question: "failed".into(), test: RuleTest::NoulGte(0.8), min_confidence: 0.0 }
}

/// fire = judge on one file input, with the SPEC example question and rule.
pub fn judge_request(id: &str, path: &Path, interval_s: u64) -> CreateRequest {
    CreateRequest {
        id: Some(id.to_string()),
        prompt: "Tell me if the build log shows a failure or finishes.".into(),
        inputs: vec![Input::File { path: path.to_path_buf(), tail_bytes: None }],
        interval_s: Some(interval_s),
        deadline_s: Some(3600),
        fire: FireKind::Judge,
        judge_questions: Some(failed_questions()),
        judge_rule: Some(failed_rule()),
        expect_s: None,
        stall_s: None,
        orchestrator: None,
    }
}

// ---- auto-interval fixtures (specs/auto-interval.md) ----------------------

/// One assistant record in the shape Claude Code 2.1.301 writes to
/// `~/.claude/projects/*/<session>.jsonl` (captured 2026-11-11 from a live
/// session; trimmed to the keys vigil may read). `cache_creation` is the
/// per-TTL write split; the nested `iterations[]` copy is real and must not
/// confuse a reader.
pub fn assistant_line(one_hour_tokens: u64, five_minute_tokens: u64) -> String {
    let split = serde_json::json!({
        "ephemeral_1h_input_tokens": one_hour_tokens,
        "ephemeral_5m_input_tokens": five_minute_tokens,
    });
    serde_json::json!({
        "parentUuid": "00000000-0000-0000-0000-000000000000",
        "isSidechain": false,
        "type": "assistant",
        "timestamp": "2026-11-11T07:00:22.032Z",
        "message": {
            "role": "assistant",
            "model": "claude-sonnet-5-5",
            "usage": {
                "input_tokens": 2,
                "cache_creation_input_tokens": one_hour_tokens + five_minute_tokens,
                "cache_read_input_tokens": 154199,
                "output_tokens": 470,
                "cache_creation": split,
                "iterations": [{"type": "message", "cache_creation": split}],
            },
        },
    })
    .to_string()
}

/// The same record as a subagent turn inside the main file (`isSidechain`).
pub fn sidechain_line(one_hour_tokens: u64, five_minute_tokens: u64) -> String {
    let mut record: serde_json::Value = serde_json::from_str(&assistant_line(one_hour_tokens, five_minute_tokens)).unwrap();
    record["isSidechain"] = serde_json::Value::Bool(true);
    record.to_string()
}

/// A user record whose tool result quotes `text` (for example `cat` of a
/// transcript): a naive substring search finds usage numbers in it.
pub fn user_line_quoting(text: &str) -> String {
    serde_json::json!({
        "isSidechain": false,
        "type": "user",
        "message": {"role": "user", "content": [{"type": "tool_result", "content": text}]},
    })
    .to_string()
}

/// `<projects>/<project>/<session>.jsonl` with one record per line.
pub fn write_transcript(projects_dir: &Path, project: &str, session: &str, lines: &[String]) -> std::path::PathBuf {
    let path = projects_dir.join(project).join(format!("{session}.jsonl"));
    let mut text = lines.join("\n");
    if !lines.is_empty() {
        text.push('\n');
    }
    write_file(&path, &text);
    path
}

/// `<projects>/<project>/<session>/subagents/<agent>.jsonl`.
pub fn write_subagent_transcript(projects_dir: &Path, project: &str, session: &str, agent: &str, lines: &[String]) -> std::path::PathBuf {
    let path = projects_dir.join(project).join(session).join("subagents").join(format!("{agent}.jsonl"));
    let mut text = lines.join("\n");
    text.push('\n');
    write_file(&path, &text);
    path
}

/// Make `path` look last-modified `age_s` seconds before the wall-clock
/// instant `now` (the injected clock), so mtime checks do not depend on the
/// host clock.
pub fn set_age(path: &Path, now: Timestamp, age_s: u64) {
    let seconds = (now.0.timestamp() - age_s as i64).max(0) as u64;
    let modified = std::time::UNIX_EPOCH + Duration::from_secs(seconds);
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(modified).unwrap();
}

/// An executable `#!/bin/sh` script (a fake `uw`, a hook).
pub fn write_script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    write_file(path, &format!("#!/bin/sh\n{body}\n"));
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A credit verdict for `ScriptedCreditProbe`.
pub fn credit_state(on_credits: bool, source: crate::credit::CreditSource) -> crate::credit::CreditState {
    // A10: the `uw` source carries its ClaudeCode windows as evidence; the
    // transcript source has none.
    let windows = match source {
        crate::credit::CreditSource::Uw => vec![serde_json::json!({"window": {"Rolling": {"minutes": 10080}}, "exceeded": on_credits})],
        crate::credit::CreditSource::Transcripts => Vec::new(),
    };
    crate::credit::CreditState { on_credits: Some(on_credits), source, windows }
}

/// S5: a check that could not say.
pub fn credit_unknown(source: crate::credit::CreditSource) -> crate::credit::CreditState {
    crate::credit::CreditState { on_credits: None, source, windows: Vec::new() }
}

/// `n` `uw`-style windows, all `exceeded` as given (an oversized evidence list).
pub fn credit_state_with_windows(on_credits: bool, count: usize) -> crate::credit::CreditState {
    let windows = (0..count).map(|n| serde_json::json!({"window": {"Rolling": {"minutes": 60 + n}}, "exceeded": on_credits})).collect();
    crate::credit::CreditState { on_credits: Some(on_credits), source: crate::credit::CreditSource::Uw, windows }
}

/// One run of the hook as `ScriptedHookRunner` saw it.
#[derive(Debug, Clone)]
pub struct HookCall {
    /// Tokio time (virtual under `start_paused`) when the hook was started.
    pub at: tokio::time::Instant,
    pub command: String,
    /// The JSON the engine would have put on the hook's stdin.
    pub input: serde_json::Value,
    /// The timeout the engine allowed.
    pub timeout: Duration,
}

type HookEffect = Box<dyn Fn(&serde_json::Value) + Send + Sync>;

/// A `CreditHookRunner` for paused-time tests: records every run, optionally
/// takes `delay` of tokio time and calls `effect` with the stdin JSON (a test's
/// stand-in for "the hook edits settings and Claude Code reacts").
pub struct ScriptedHookRunner {
    calls: Mutex<Vec<HookCall>>,
    effect: Mutex<Option<HookEffect>>,
    delay: Mutex<Duration>,
}

impl ScriptedHookRunner {
    pub fn new() -> Arc<ScriptedHookRunner> {
        Arc::new(ScriptedHookRunner { calls: Mutex::new(Vec::new()), effect: Mutex::new(None), delay: Mutex::new(Duration::ZERO) })
    }
    pub fn set_effect(&self, effect: impl Fn(&serde_json::Value) + Send + Sync + 'static) {
        *self.effect.lock().unwrap() = Some(Box::new(effect));
    }
    pub fn set_delay(&self, delay: Duration) {
        *self.delay.lock().unwrap() = delay;
    }
    pub fn calls(&self) -> Vec<HookCall> {
        self.calls.lock().unwrap().clone()
    }
    /// Seconds after `start` (tokio time) of each run, in order.
    pub fn offsets_from(&self, start: tokio::time::Instant) -> Vec<u64> {
        self.calls.lock().unwrap().iter().map(|call| call.at.duration_since(start).as_secs()).collect()
    }
}

impl crate::credit::CreditHookRunner for ScriptedHookRunner {
    fn run<'a>(&'a self, command: &'a str, input: &'a serde_json::Value, timeout: Duration) -> BoxFuture<'a, crate::credit::HookOutcome> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(HookCall { at: tokio::time::Instant::now(), command: command.to_string(), input: input.clone(), timeout });
            let delay = *self.delay.lock().unwrap();
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if let Some(effect) = self.effect.lock().unwrap().as_ref() {
                effect(input);
            }
            crate::credit::HookOutcome { command: command.to_string(), exit_code: Some(0), timed_out: false, output: String::new() }
        })
    }
}

/// S8: a `Read + Seek` over in-memory bytes that remembers what was really
/// read: the number of bytes handed out and the lowest file offset any read
/// started at. "Bytes returned" is what a function gives back; this is what
/// it pulled from the file.
pub struct CountingReader {
    inner: std::io::Cursor<Vec<u8>>,
    bytes_read: u64,
    lowest_offset: Option<u64>,
}

impl CountingReader {
    pub fn new(bytes: Vec<u8>) -> CountingReader {
        CountingReader { inner: std::io::Cursor::new(bytes), bytes_read: 0, lowest_offset: None }
    }
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }
    /// The lowest offset a read began at; `None` if nothing was read.
    pub fn lowest_offset(&self) -> Option<u64> {
        self.lowest_offset
    }
}

impl std::io::Read for CountingReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let at = self.inner.position();
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.bytes_read += n as u64;
            self.lowest_offset = Some(self.lowest_offset.map_or(at, |lowest| lowest.min(at)));
        }
        Ok(n)
    }
}

impl std::io::Seek for CountingReader {
    fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(to)
    }
}

/// An assistant record exactly `total_len` bytes long (without the newline),
/// padded through an extra `pad` field: for the 64 KiB line-length bound.
/// Panics if the record cannot be that short.
pub fn assistant_line_of_len(one_hour_tokens: u64, five_minute_tokens: u64, total_len: usize) -> String {
    let mut record: serde_json::Value = serde_json::from_str(&assistant_line(one_hour_tokens, five_minute_tokens)).unwrap();
    record["pad"] = serde_json::Value::String(String::new());
    let base = record.to_string().len();
    assert!(total_len >= base, "a record needs at least {base} bytes");
    record["pad"] = serde_json::Value::String("p".repeat(total_len - base));
    let line = record.to_string();
    assert_eq!(line.len(), total_len);
    line
}

/// A `CreditProbe` that answers from a script (then a steady verdict) and
/// records when it was asked, in tokio time (virtual under `start_paused`) and
/// wall-clock time.
pub struct ScriptedCreditProbe {
    script: Mutex<VecDeque<crate::credit::CreditState>>,
    steady: Mutex<crate::credit::CreditState>,
    asked: Mutex<Vec<(tokio::time::Instant, Timestamp)>>,
}

impl ScriptedCreditProbe {
    /// Always `steady` once the script is used up.
    pub fn new(steady: crate::credit::CreditState) -> Arc<ScriptedCreditProbe> {
        Arc::new(ScriptedCreditProbe { script: Mutex::new(VecDeque::new()), steady: Mutex::new(steady), asked: Mutex::new(Vec::new()) })
    }
    /// The next unanswered check gets `state`.
    pub fn push(&self, state: crate::credit::CreditState) {
        self.script.lock().unwrap().push_back(state);
    }
    pub fn set_steady(&self, state: crate::credit::CreditState) {
        *self.steady.lock().unwrap() = state;
    }
    /// How many checks were made.
    pub fn asked_count(&self) -> usize {
        self.asked.lock().unwrap().len()
    }
    /// Seconds after `start` (tokio time) of each check, in order.
    pub fn offsets_from(&self, start: tokio::time::Instant) -> Vec<u64> {
        self.asked.lock().unwrap().iter().map(|(instant, _)| instant.duration_since(start).as_secs()).collect()
    }
}

impl crate::credit::CreditProbe for ScriptedCreditProbe {
    fn check<'a>(&'a self, now: Timestamp) -> BoxFuture<'a, crate::credit::CreditState> {
        Box::pin(async move {
            self.asked.lock().unwrap().push((tokio::time::Instant::now(), now));
            let next = self.script.lock().unwrap().pop_front();
            next.unwrap_or_else(|| self.steady.lock().unwrap().clone())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn another_process_keeps_the_environments_configuration() {
        // why: S22: `another_process()` replaced the Env's config with
        //      defaults, so a 64-byte digest test silently ran with 8192 bytes.
        //      Every field must carry over. Control for the fixture itself.
        let mut config = default_config();
        config.digest_max_bytes = 64;
        config.judge_state_bytes = 4000;
        config.judge_timeout = Duration::from_secs(25);
        config.judge_url = "http://10.9.8.7:1".into();
        let env = Env::with_clock_and_config(Arc::new(ManualClock::new(spec_instant())), config.clone());
        assert_eq!(env.vigil.config(), &config);
        assert_eq!(env.another_process().config(), &config);
        assert_eq!(env.another_process_in(Path::new("/tmp")).config(), &config);
        assert_ne!(&config, &default_config(), "the custom config must differ from the defaults for this test to mean anything");
    }

    #[tokio::test(start_paused = true)]
    async fn scripted_reader_sleeps_virtual_time_reports_text_changes_and_records_reads() {
        // why: X3/X4 fixture control. Every X4 timing claim (a 15 s input, two
        //      10 s inputs finishing together) rests on this reader taking
        //      exactly the scripted virtual time and on `changed` meaning
        //      "text differs from the stored text" (baseline never a change).
        let reader = ScriptedReader::new();
        let input = Input::Cmd { argv: vec!["fake".into()], timeout_s: Some(20.0), cwd: None };
        reader.set_text("cmd:fake", "v1");
        reader.set_sleep("cmd:fake", Duration::from_secs(15));
        let started = tokio::time::Instant::now();
        let baseline = reader.read(&input, &InputState::default()).await;
        assert_eq!(started.elapsed(), Duration::from_secs(15));
        assert!(!baseline.changed, "the first read of an input is a baseline");
        let same = reader.read(&input, &baseline.state).await;
        assert!(!same.changed);
        reader.set_text("cmd:fake", "v2");
        let changed = reader.read(&input, &same.state).await;
        assert!(changed.changed && changed.text == "v2");
        let records = reader.reads();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].finished.unwrap() - records[0].started, Duration::from_secs(15));

    }

    #[tokio::test]
    async fn scripted_reader_gate_holds_a_read_until_opened() {
        // why: X3 fixture control: "A is still sensing" is proven by a gate,
        //      not by a sleep. The read must not finish before `open()` and
        //      must finish after; `wait_started` must observe the start.
        let reader = Arc::new(ScriptedReader::new());
        let gate = Gate::closed();
        reader.set_gate("cmd:held", Some(gate.clone()));
        let input = Input::Cmd { argv: vec!["held".into()], timeout_s: None, cwd: None };
        let task = tokio::spawn({
            let reader = reader.clone();
            async move { reader.read(&input, &InputState::default()).await.text }
        });
        tokio::time::timeout(Duration::from_secs(2), reader.wait_started("cmd:held", 1)).await.expect("read never started");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!task.is_finished(), "read finished while its gate was closed");
        gate.open();
        assert_eq!(tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap(), "ok");
    }

    #[test]
    fn deletion_log_reports_deletions_in_the_order_the_kernel_saw_them_across_directories() {
        // why: X3 fixture control: the prune-ordering test trusts this probe.
        //      Delete b/second.x BEFORE a/first.x although a < b: the log must
        //      say so, name each directory by its label, and ignore creations.
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("first.x"), "").unwrap();
        std::fs::write(b.join("second.x"), "").unwrap();
        let log = DeletionLog::start(&[("a", &a), ("b", &b)]);
        std::fs::write(a.join("created-after.x"), "").unwrap();
        std::fs::remove_file(b.join("second.x")).unwrap();
        std::fs::remove_file(a.join("first.x")).unwrap();
        assert_eq!(log.drain(), vec!["b/second.x".to_string(), "a/first.x".to_string()]);
        assert!(log.drain().is_empty(), "drain consumes");
    }

    #[test]
    fn blocked_flock_waiters_sees_a_real_waiter_and_nothing_when_uncontended() {
        // why: S25: the contention probe itself needs a positive control, or
        //      "no waiters" proves nothing. Holder + a blocked thread -> the
        //      waiter is listed; with no contention the list is empty.
        use std::os::fd::AsRawFd;
        unsafe extern "C" {
            fn flock(fd: i32, operation: i32) -> i32;
        }
        const LOCK_EX: i32 = 2;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("probe.lock");
        let holder = std::fs::File::create(&path).unwrap();
        assert_eq!(unsafe { flock(holder.as_raw_fd(), LOCK_EX) }, 0);
        assert!(blocked_flock_waiters(&path).is_empty(), "holder alone is not a waiter");
        let waiter_path = path.clone();
        let waiter = std::thread::spawn(move || {
            let file = std::fs::File::open(&waiter_path).unwrap();
            unsafe { flock(file.as_raw_fd(), LOCK_EX) };
        });
        let started = std::time::Instant::now();
        while blocked_flock_waiters(&path).is_empty() && started.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(blocked_flock_waiters(&path), vec![std::process::id()]);
        drop(holder);
        waiter.join().unwrap();
        assert!(blocked_flock_waiters(&path).is_empty());
    }
}
