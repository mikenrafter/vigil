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
use crate::judge::{Answer, Answers, BoxFuture, Judge, JudgeError, JudgeRequest};

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
    script: VecDeque<Result<Answers, JudgeError>>,
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
        self.0.lock().unwrap().script.push_back(result);
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
            let mut state = self.0.lock().unwrap();
            state.requests.push(request.clone());
            match state.script.pop_front() {
                Some(next) => next,
                None => state.fallback.clone(),
            }
        })
    }
    fn healthy(&self) -> BoxFuture<'_, bool> {
        Box::pin(async move { self.0.lock().unwrap().healthy })
    }
}

#[derive(Debug, Clone)]
pub enum Behavior {
    Respond { status: u16, body: String },
    /// Accept and never answer.
    Hang,
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
use crate::watch::{CreateRequest, FireKind, Input, WatchId};

pub fn wid(raw: &str) -> WatchId {
    WatchId::parse(raw).unwrap()
}

/// A tempdir holding the state root (`state/`) and a place for inputs, with a
/// `Vigil` wired to the given clock and a `ScriptedJudge`.
pub struct Env {
    pub dir: tempfile::TempDir,
    pub judge: Arc<ScriptedJudge>,
    pub clock: Arc<dyn Clock>,
    pub vigil: Vigil,
}

impl Env {
    pub fn with_clock(clock: Arc<dyn Clock>) -> Env {
        Env::with_clock_and_config(clock, default_config())
    }
    pub fn with_clock_and_config(clock: Arc<dyn Clock>, config: Config) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let judge = Arc::new(ScriptedJudge::new());
        let vigil = Vigil::new(dir.path().join("state"), clock.clone(), judge.clone(), config);
        Env { dir, judge, clock, vigil }
    }
    pub fn root(&self) -> PathBuf {
        self.dir.path().join("state")
    }
    /// Path for an input file inside the tempdir but outside the state root.
    pub fn input_path(&self, name: &str) -> PathBuf {
        self.dir.path().join("inputs").join(name)
    }
    /// A second `Vigil` over the same root, clock and judge: a second process.
    pub fn another_process(&self) -> Vigil {
        Vigil::new(self.root(), self.clock.clone(), self.judge.clone(), default_config())
    }
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
    }
}
