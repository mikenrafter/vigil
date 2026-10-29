//! Watch types: the request as the agent sends it, the validated spec, and
//! the persisted watch file.
//!
//! Mating parts: `CreateRequest` (wire, raw) -> `WatchSpec::parse` (checked)
//! -> `WatchFile` (persisted by `store`).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::clock::Timestamp;
use crate::error::CreateError;
use crate::judge::{Answer, JudgeRule, RuleTest};

pub const MIN_INTERVAL_S: u64 = 1;
pub const MAX_INTERVAL_S: u64 = 3600;
pub const DEFAULT_INTERVAL_S: u64 = 20;
pub const DEFAULT_DEADLINE_S: u64 = 3600;
/// V12: a watch has at most this many inputs (so at most 16 events per tick).
pub const MAX_INPUTS: usize = 16;
/// V12: `prompt` is at most this many bytes.
pub const MAX_PROMPT_BYTES: usize = 1024;
/// V19: `deadline_s` is 1..=604800 (7 days).
pub const MIN_DEADLINE_S: u64 = 1;
pub const MAX_DEADLINE_S: u64 = 604_800;
/// V19 + X4 + X7: `cmd.timeout_s` is 0.1..=19 (X7: worst tick reserve is
/// 19 + 10 judge + 1 = 30 s, which fits the smallest call).
pub const MIN_TIMEOUT_S: f64 = 0.1;
/// R3: a `cmd` without `timeout_s`, and every `url` / `port` read, waits at
/// most this long.
pub const DEFAULT_TIMEOUT_S: f64 = 10.0;
pub const MAX_TIMEOUT_S: f64 = 19.0;
/// X2: an event `summary` is at most this many bytes (cut on a UTF-8
/// boundary).
pub const MAX_SUMMARY_BYTES: usize = 200;
/// W7: the persisted pending change keeps at most this many bytes of text per
/// input (newest kept, `[truncated]` when cut).
pub const MAX_PENDING_BYTES_PER_INPUT: usize = 64 * 1024;

/// A validated watch id: `[A-Za-z0-9_-]{1,64}`. Parse, do not validate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct WatchId(String);

impl WatchId {
    pub fn parse(raw: &str) -> Result<WatchId, CreateError> {
        let valid = (1..=64).contains(&raw.len())
            && raw.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
        if valid {
            Ok(WatchId(raw.to_string()))
        } else {
            Err(CreateError::InvalidId(raw.to_string()))
        }
    }
    /// A fresh, valid id. Two calls never return the same id.
    pub fn generate() -> WatchId {
        // CLAIM(generated-ids-distinct): the counter is unique within a
        // process; wall-clock nanoseconds and the pid separate processes.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        WatchId(format!("w-{nanos:x}-{:x}-{count:x}", std::process::id()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WatchId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for WatchId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        WatchId::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GlobReport {
    /// Report matches that appeared since the last tick.
    Created,
}

impl Default for GlobReport {
    fn default() -> Self {
        GlobReport::Created
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortWant {
    Open,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessWant {
    Exited,
    Running,
}

/// One wait primitive (SPEC "watch_create").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Input {
    File {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tail_bytes: Option<u64>,
    },
    Glob {
        pattern: String,
        #[serde(default)]
        report: GlobReport,
    },
    Cmd {
        argv: Vec<String>,
        /// Seconds; fractions allowed. Default 10. V19 + X4: 0.1..=20.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_s: Option<f64>,
        /// Working directory. V11: absent at the wire; `watch_create` stores
        /// the creating process's cwd here (absolute) so a resume from another
        /// directory runs the command in the same place.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
    },
    Url {
        url: String,
    },
    Port {
        host: String,
        port: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        want: Option<PortWant>,
    },
    Process {
        pid: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        want: Option<ProcessWant>,
    },
}

impl Input {
    /// Stable label used in events and digests: `file:/tmp/build.log`,
    /// `glob:<pattern>`, `cmd:<argv joined by spaces>`, `url:<url>`,
    /// `port:<host>:<port>`, `process:<pid>`.
    pub fn label(&self) -> String {
        match self {
            Input::File { path, .. } => format!("file:{}", path.display()),
            Input::Glob { pattern, .. } => format!("glob:{pattern}"),
            Input::Cmd { argv, .. } => format!("cmd:{}", argv.join(" ")),
            Input::Url { url } => format!("url:{url}"),
            Input::Port { host, port, .. } => format!("port:{host}:{port}"),
            Input::Process { pid, .. } => format!("process:{pid}"),
        }
    }
    /// Does this input carry a `want` (used by fire = condition)?
    pub fn has_want(&self) -> bool {
        match self {
            Input::Port { want, .. } => want.is_some(),
            Input::Process { want, .. } => want.is_some(),
            _ => false,
        }
    }
    /// The longest this input can keep one read waiting (X4 tick reserve):
    /// `cmd` its `timeout_s` (default 10 s); `url` and `port` 10 s (the
    /// reader's fixed timeout); `file`, `glob` and `process` read local state
    /// without waiting, so `Duration::ZERO`.
    pub fn read_timeout(&self) -> std::time::Duration {
        let default = std::time::Duration::from_secs_f64(DEFAULT_TIMEOUT_S);
        match self {
            Input::Cmd { timeout_s, .. } => timeout_s.map_or(default, std::time::Duration::from_secs_f64),
            Input::Url { .. } | Input::Port { .. } => default,
            Input::File { .. } | Input::Glob { .. } | Input::Process { .. } => std::time::Duration::ZERO,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FireKind {
    Judge,
    Change,
    Condition,
}

/// Judge questions in Laya's own schema, passed through unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JudgeQuestions(pub Map<String, Value>);

/// The request exactly as the agent sends it (all optional-ish; unchecked).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default)]
    pub prompt: String,
    pub inputs: Vec<Input>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_s: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_s: Option<u64>,
    pub fire: FireKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_questions: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_rule: Option<JudgeRule>,
}

/// The fire rule with its data: judge fields exist only for `Judge`, so an
/// invalid combination is unrepresentable once parsed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "fire", rename_all = "snake_case")]
pub enum FireRule {
    Judge {
        judge_questions: JudgeQuestions,
        judge_rule: JudgeRule,
    },
    Change,
    Condition,
}

fn validate_input(input: &Input) -> Result<(), CreateError> {
    match input {
        Input::Cmd { argv, timeout_s, .. } => {
            if argv.is_empty() {
                return Err(CreateError::EmptyArgv);
            }
            if let Some(timeout) = *timeout_s
                && !(MIN_TIMEOUT_S..=MAX_TIMEOUT_S).contains(&timeout)
            {
                return Err(CreateError::TimeoutOutOfRange(timeout));
            }
        }
        Input::Url { url } => {
            let is_http = url.get(..7).is_some_and(|scheme| scheme.eq_ignore_ascii_case("http://"));
            if !is_http {
                return Err(CreateError::UnsupportedUrlScheme(url.clone()));
            }
        }
        Input::File { .. } | Input::Glob { .. } | Input::Port { .. } | Input::Process { .. } => {}
    }
    Ok(())
}

fn validate_judge(questions: &Map<String, Value>, rule: &JudgeRule) -> Result<(), CreateError> {
    if questions.is_empty() {
        return Err(CreateError::EmptyQuestions);
    }
    let Some(question) = questions.get(&rule.question) else {
        return Err(CreateError::UnknownRuleQuestion(rule.question.clone()));
    };
    let question_type = question.get("type").and_then(Value::as_str);
    if question_type != Some(rule.test.question_type()) {
        return Err(CreateError::RuleKindMismatch {
            question: rule.question.clone(),
            question_type: question_type.map(str::to_string),
            rule_kind: rule.test.question_type().to_string(),
        });
    }
    let in_unit_range = |field: &str, value: f64| {
        if (0.0..=1.0).contains(&value) {
            Ok(())
        } else {
            Err(CreateError::ProbabilityOutOfRange { field: field.to_string(), value })
        }
    };
    if let RuleTest::NoulGte(probability) = rule.test {
        in_unit_range("judge_rule.noul_gte", probability)?;
    }
    in_unit_range("judge_rule.min_confidence", rule.min_confidence)
}

/// A checked watch definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchSpec {
    pub id: WatchId,
    pub prompt: String,
    pub inputs: Vec<Input>,
    /// Already clamped to 1..=3600 (default 20).
    pub interval_s: u64,
    /// Seconds from creation to the deadline (default 3600).
    pub deadline_s: u64,
    #[serde(flatten)]
    pub fire: FireRule,
}

impl WatchSpec {
    /// Validate a raw request (SPEC "watch_create" + V12 + V19). Does not
    /// touch disk, so duplicate-id and judge-reachability checks live in
    /// `Vigil::create`.
    pub fn parse(request: CreateRequest) -> Result<WatchSpec, CreateError> {
        let id = match request.id.as_deref() {
            Some(raw) => WatchId::parse(raw)?,
            None => WatchId::generate(),
        };
        if request.inputs.len() > MAX_INPUTS {
            return Err(CreateError::TooManyInputs(request.inputs.len()));
        }
        if request.prompt.len() > MAX_PROMPT_BYTES {
            return Err(CreateError::PromptTooLong(request.prompt.len()));
        }
        let deadline_s = request.deadline_s.unwrap_or(DEFAULT_DEADLINE_S);
        if !(MIN_DEADLINE_S..=MAX_DEADLINE_S).contains(&deadline_s) {
            return Err(CreateError::DeadlineOutOfRange(deadline_s));
        }
        let interval_s = request.interval_s.unwrap_or(DEFAULT_INTERVAL_S).clamp(MIN_INTERVAL_S, MAX_INTERVAL_S);
        for input in &request.inputs {
            validate_input(input)?;
        }
        let fire = match request.fire {
            FireKind::Judge => {
                let (Some(questions), Some(rule)) = (request.judge_questions, request.judge_rule) else {
                    return Err(CreateError::MissingJudgeFields);
                };
                validate_judge(&questions, &rule)?;
                FireRule::Judge { judge_questions: JudgeQuestions(questions), judge_rule: rule }
            }
            FireKind::Change | FireKind::Condition => {
                if request.judge_questions.is_some() || request.judge_rule.is_some() {
                    return Err(CreateError::UnexpectedJudgeFields);
                }
                if request.fire == FireKind::Change {
                    FireRule::Change
                } else {
                    if !request.inputs.iter().any(Input::has_want) {
                        return Err(CreateError::ConditionNeedsWant);
                    }
                    FireRule::Condition
                }
            }
        };
        Ok(WatchSpec { id, prompt: request.prompt, inputs: request.inputs, interval_s, deadline_s, fire })
    }

    /// V11: resolve relative file paths, glob patterns and `cmd` working
    /// directories against `cwd` (the creating process's cwd); a `cmd` without
    /// a `cwd` gets `cwd`. Absolute values are left alone.
    pub fn absolutize(mut self, cwd: &std::path::Path) -> WatchSpec {
        for input in &mut self.inputs {
            match input {
                Input::File { path, .. } => *path = cwd.join(&*path),
                Input::Glob { pattern, .. } => {
                    // `Path::join` keeps an absolute right-hand side as is.
                    *pattern = cwd.join(&*pattern).to_string_lossy().into_owned();
                }
                Input::Cmd { cwd: command_cwd, .. } => {
                    *command_cwd = Some(match command_cwd.take() {
                        Some(given) => cwd.join(given),
                        None => cwd.to_path_buf(),
                    });
                }
                Input::Url { .. } | Input::Port { .. } | Input::Process { .. } => {}
            }
        }
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchStatus {
    Pending,
    Fired,
    Expired,
    Cancelled,
}

impl WatchStatus {
    pub fn is_finished(self) -> bool {
        self != WatchStatus::Pending
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgeStatus {
    Ok,
    Down,
    #[default]
    Skipped,
}

/// V9: what identifies "the same file at the same place in time".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_secs: i64,
    pub mtime_nanos: u32,
}

/// What `read_input` remembers between ticks, per input (same order as
/// `WatchSpec::inputs`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct InputState {
    /// Non-file inputs: hash of the last reading (V9: files are tracked by
    /// position, not hash).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// File inputs: byte offset already reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// File inputs (V9): `(dev, inode, size, mtime)` at the last read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<FileStamp>,
    /// Process inputs (V17): `/proc/<pid>/stat` field 22 recorded at baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proc_start: Option<u64>,
    /// Glob inputs: matches already seen. X2: at most
    /// `inputs::MAX_KNOWN_GLOB_PATHS` (1000) paths are persisted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known: Vec<String>,
}

/// New content of one changed input, waiting for the judge (or for delivery).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedInput {
    pub label: String,
    pub text: String,
}

/// A change not yet resolved by a judge answer. Survives ticks and restarts
/// (SPEC "Failure rules": a pending change counts as a change). W7: the text
/// of each `ChangedInput` is persisted at most `MAX_PENDING_BYTES_PER_INPUT`
/// (+ the `[truncated]` marker) bytes: newest kept, oldest dropped.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PendingChange {
    pub changed: Vec<ChangedInput>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// V3 + W1: stable across replays, always `<watch_id>:<tick>:<n>` with n
    /// counting from 0 within the tick (the first event is `:0`), see
    /// `Event::make_id`.
    pub event_id: String,
    pub tick: u64,
    pub watch_id: String,
    pub input: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge: Option<BTreeMap<String, Answer>>,
}

impl Event {
    /// V3 + W1 id of the `index`-th (0-based) event emitted at `tick`:
    /// `<watch_id>:<tick>:<index>`, so index 0 is `build:17:0`.
    pub fn make_id(watch_id: &str, tick: u64, index: usize) -> String {
        format!("{watch_id}:{tick}:{index}")
    }
}

/// How a watch ended (V3/V4). `Pending` is deliberately not representable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalStatus {
    Fired,
    Expired,
    Cancelled,
}

impl From<TerminalStatus> for WatchStatus {
    fn from(status: TerminalStatus) -> WatchStatus {
        match status {
            TerminalStatus::Fired => WatchStatus::Fired,
            TerminalStatus::Expired => WatchStatus::Expired,
            TerminalStatus::Cancelled => WatchStatus::Cancelled,
        }
    }
}

/// The terminal result of a watch (V3). Stored once by `WatchFile::finish`,
/// replayed unchanged forever after.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Terminal {
    pub status: TerminalStatus,
    pub finished_at: Timestamp,
    pub events: Vec<Event>,
    pub digest: String,
}

/// `WatchFile::finish` on a watch that already has a terminal result (V4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("watch already finished as {0:?}")]
pub struct AlreadyFinished(pub WatchStatus);

/// The persisted watch (`watches/<id>.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchFile {
    #[serde(flatten)]
    pub spec: WatchSpec,
    pub status: WatchStatus,
    pub created_at: Timestamp,
    pub deadline_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<Timestamp>,
    pub tick: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tick_at: Option<Timestamp>,
    pub input_states: Vec<InputState>,
    pub judge_strikes: u32,
    pub judge_status: JudgeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_change: Option<PendingChange>,
    /// V3: the terminal events only (empty while pending, empty for expired
    /// and cancelled). There is no `delivered_through`. Each event's
    /// `summary` is at most `MAX_SUMMARY_BYTES` (X2).
    pub events: Vec<Event>,
    /// Last observed change; the terminal digest (V14).
    pub last_digest: String,
}

impl WatchFile {
    /// A new pending watch: tick 0, `deadline_at = now + spec.deadline_s`.
    pub fn new(spec: WatchSpec, now: Timestamp) -> WatchFile {
        WatchFile {
            deadline_at: now.plus_secs(spec.deadline_s),
            spec,
            status: WatchStatus::Pending,
            created_at: now,
            finished_at: None,
            tick: 0,
            last_tick_at: None,
            input_states: Vec::new(),
            judge_strikes: 0,
            judge_status: JudgeStatus::default(),
            pending_change: None,
            events: Vec::new(),
            last_digest: String::new(),
        }
    }
    /// V3/V4: store the terminal result. Succeeds exactly once; afterwards the
    /// watch is immutable and this returns `Err` without touching anything.
    pub fn finish(&mut self, terminal: Terminal) -> Result<(), AlreadyFinished> {
        if self.status.is_finished() {
            return Err(AlreadyFinished(self.status));
        }
        self.status = terminal.status.into();
        self.finished_at = Some(terminal.finished_at);
        self.events = terminal.events;
        self.last_digest = terminal.digest;
        self.pending_change = None;
        Ok(())
    }
    /// V16: a pending watch whose `deadline_at` is at or before `now`.
    pub fn is_overdue(&self, now: Timestamp) -> bool {
        self.status == WatchStatus::Pending && self.deadline_at <= now
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judge::RuleTest;
    use crate::testing::{at, change_request, failed_questions, failed_rule, judge_request};
    use serde_json::json;
    use std::path::Path;

    fn judge_req() -> CreateRequest {
        judge_request("w", Path::new("/tmp/x.log"), 20)
    }

    // ---- ids ----

    #[test]
    fn id_accepts_the_documented_charset_and_length_bounds() {
        // why: SPEC "watch_create": ids are [A-Za-z0-9_-]{1,64}; both bounds
        //      (1 and 64) and every class must be accepted.
        // proves: CLAIM(id-charset)
        for ok in ["a", "Z", "0", "_", "-", "Az09_-", &"a".repeat(64)] {
            assert!(WatchId::parse(ok).is_ok(), "should accept {ok:?}");
        }
    }

    #[test]
    fn id_rejects_everything_outside_the_charset_and_length_bounds() {
        // why: ids become file names (`watches/<id>.json`); path separators,
        //      dots, spaces, non-ASCII and over-length ids must be refused
        //      (SPEC "watch_create"; also keeps writes inside the state root).
        // proves: CLAIM(id-charset)
        let too_long = "a".repeat(65);
        for bad in ["", too_long.as_str(), "has space", "a/b", "../x", "a.b", "é", "a\nb", "a\0b"] {
            assert!(
                matches!(WatchId::parse(bad), Err(CreateError::InvalidId(_))),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn generated_ids_are_valid_and_distinct() {
        // why: SPEC "watch_create": "a generated id is used when absent"; two
        //      watches created without ids must not collide.
        let first = WatchId::generate();
        let second = WatchId::generate();
        assert!(WatchId::parse(first.as_str()).is_ok(), "generated id {first} must satisfy the charset");
        assert_ne!(first, second);
    }

    #[test]
    fn id_deserialization_goes_through_the_same_validation() {
        // why: a hand-edited or hostile watch file / MCP argument must not
        //      smuggle in an id that parse() would refuse (parse, don't validate).
        assert!(serde_json::from_str::<WatchId>("\"build-1\"").is_ok());
        assert!(serde_json::from_str::<WatchId>("\"../etc\"").is_err());
    }

    // ---- interval / deadline ----

    fn parsed_interval(requested: Option<u64>) -> u64 {
        let mut req = change_request("w", Path::new("/tmp/x"), 20);
        req.interval_s = requested;
        WatchSpec::parse(req).unwrap().interval_s
    }

    #[test]
    fn interval_defaults_to_20_and_is_clamped_to_1_through_3600() {
        // why: D10 / task contract: default 20, clamped (not rejected) to 1..=3600.
        // proves: CLAIM(interval-clamp)
        assert_eq!(parsed_interval(None), 20);
        assert_eq!(parsed_interval(Some(0)), 1, "0 clamps up to the minimum");
        assert_eq!(parsed_interval(Some(1)), 1);
        assert_eq!(parsed_interval(Some(5)), 5, "in-range values pass through");
        assert_eq!(parsed_interval(Some(3600)), 3600);
        assert_eq!(parsed_interval(Some(99_999)), 3600, "over the maximum clamps down");
    }

    #[test]
    fn deadline_defaults_to_3600_seconds() {
        // why: SPEC example uses deadline_s 3600; absent deadline takes that
        //      default (Defaults taken in the red report).
        let mut req = change_request("w", Path::new("/tmp/x"), 20);
        req.deadline_s = None;
        assert_eq!(WatchSpec::parse(req).unwrap().deadline_s, 3600);
    }

    // ---- judge field rules ----

    #[test]
    fn judge_fire_requires_questions_and_rule() {
        // why: SPEC "watch_create": judge_questions and judge_rule are required
        //      with fire = "judge". Each missing alone, and both missing, is an error.
        let mut no_questions = judge_req();
        no_questions.judge_questions = None;
        let mut no_rule = judge_req();
        no_rule.judge_rule = None;
        let mut neither = judge_req();
        neither.judge_questions = None;
        neither.judge_rule = None;
        for req in [no_questions, no_rule, neither] {
            assert_eq!(WatchSpec::parse(req), Err(CreateError::MissingJudgeFields));
        }
        // positive control: the complete request is accepted.
        assert!(WatchSpec::parse(judge_req()).is_ok());
    }

    #[test]
    fn judge_fields_are_rejected_with_change_and_condition() {
        // why: SPEC "watch_create": judge fields are "rejected otherwise".
        for fire in [FireKind::Change, FireKind::Condition] {
            let mut with_questions = change_request("w", Path::new("/tmp/x"), 20);
            with_questions.fire = fire;
            with_questions.inputs.push(Input::Port { host: "127.0.0.1".into(), port: 1, want: Some(PortWant::Open) });
            with_questions.judge_questions = Some(failed_questions());
            assert_eq!(WatchSpec::parse(with_questions), Err(CreateError::UnexpectedJudgeFields), "{fire:?} + questions");

            let mut with_rule = change_request("w", Path::new("/tmp/x"), 20);
            with_rule.fire = fire;
            with_rule.inputs.push(Input::Port { host: "127.0.0.1".into(), port: 1, want: Some(PortWant::Open) });
            with_rule.judge_rule = Some(failed_rule());
            assert_eq!(WatchSpec::parse(with_rule), Err(CreateError::UnexpectedJudgeFields), "{fire:?} + rule");
        }
    }

    #[test]
    fn empty_judge_questions_are_rejected() {
        // why: SPEC: vigil checks that judge_questions is a non-empty object.
        let mut req = judge_req();
        req.judge_questions = Some(serde_json::Map::new());
        assert_eq!(WatchSpec::parse(req), Err(CreateError::EmptyQuestions));
    }

    #[test]
    fn rule_question_must_be_a_key_of_the_questions() {
        // why: SPEC: "the rule's question is a key in it".
        let mut req = judge_req();
        req.judge_rule = Some(JudgeRule { question: "nope".into(), test: RuleTest::NoulGte(0.5), min_confidence: 0.0 });
        assert_eq!(WatchSpec::parse(req), Err(CreateError::UnknownRuleQuestion("nope".into())));
    }

    #[test]
    fn rule_kind_must_match_question_type() {
        // why: SPEC: "The rule's kind must match the question's type; a
        //      mismatch is an error at watch_create". All six off-diagonal
        //      pairs of {noul, choice, score}; the three diagonal pairs pass.
        let kinds = [
            ("noul", RuleTest::NoulGte(0.5)),
            ("choice", RuleTest::ChoiceIn(vec!["a".into()])),
            ("score", RuleTest::ScoreGte(2.0)),
        ];
        for (question_type, _) in &kinds {
            for (rule_type, test) in &kinds {
                let mut req = judge_req();
                req.judge_questions = Some(
                    json!({"q": {"type": question_type, "instructions": "?"}}).as_object().unwrap().clone(),
                );
                req.judge_rule = Some(JudgeRule { question: "q".into(), test: test.clone(), min_confidence: 0.0 });
                let outcome = WatchSpec::parse(req);
                if question_type == rule_type {
                    assert!(outcome.is_ok(), "{question_type} question + {rule_type} rule should be fine");
                } else {
                    assert!(
                        matches!(outcome, Err(CreateError::RuleKindMismatch { .. })),
                        "{question_type} question + {rule_type} rule should mismatch, got {outcome:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn question_without_a_known_type_cannot_match_any_rule() {
        // why: a rule cannot be checked against a question whose type is
        //      absent or unknown (SPEC: mismatch is a create-time error).
        for question in [json!({"instructions": "?"}), json!({"type": "bool", "instructions": "?"})] {
            let mut req = judge_req();
            req.judge_questions = Some(json!({"failed": question}).as_object().unwrap().clone());
            assert!(matches!(WatchSpec::parse(req), Err(CreateError::RuleKindMismatch { .. })));
        }
    }

    #[test]
    fn questions_are_kept_unchanged() {
        // why: SPEC: judge_questions is "passed to Laya unchanged" - unknown
        //      keys and nested criteria must survive validation.
        let questions = json!({"failed": {"type": "noul", "instructions": "?",
            "criteria": {"true": "build failed", "false": "fine"}, "extra": [1, 2]}})
        .as_object()
        .unwrap()
        .clone();
        let mut req = judge_req();
        req.judge_questions = Some(questions.clone());
        let spec = WatchSpec::parse(req).unwrap();
        match spec.fire {
            FireRule::Judge { judge_questions, .. } => assert_eq!(judge_questions.0, questions),
            other => panic!("expected a judge rule, got {other:?}"),
        }
    }

    // ---- condition ----

    #[test]
    fn condition_needs_at_least_one_input_with_a_want() {
        // why: SPEC "watch_create": "condition requires at least one input
        //      with want". File/url/cmd/glob have no want; port/process
        //      without a want do not count.
        let mut no_want = change_request("w", Path::new("/tmp/x"), 20);
        no_want.fire = FireKind::Condition;
        no_want.inputs.push(Input::Port { host: "127.0.0.1".into(), port: 1, want: None });
        no_want.inputs.push(Input::Process { pid: 1, want: None });
        assert_eq!(WatchSpec::parse(no_want), Err(CreateError::ConditionNeedsWant));

        for with_want in [
            Input::Port { host: "127.0.0.1".into(), port: 1, want: Some(PortWant::Closed) },
            Input::Process { pid: 1, want: Some(ProcessWant::Exited) },
        ] {
            let mut ok = change_request("w", Path::new("/tmp/x"), 20);
            ok.fire = FireKind::Condition;
            ok.inputs.push(with_want);
            assert!(WatchSpec::parse(ok).is_ok());
        }
    }

    #[test]
    fn change_fire_does_not_need_wants() {
        // why: control for the condition rule above: only `condition` demands wants.
        assert!(WatchSpec::parse(change_request("w", Path::new("/tmp/x"), 20)).is_ok());
    }

    // ---- wire format ----

    #[test]
    fn the_spec_example_request_parses_verbatim() {
        // why: parity with the literal watch_create example in SPEC
        //      "watch_create" (all six input kinds, judge fields); the wire
        //      format must accept it exactly as written.
        let literal = r#"{
          "id": "optional-stable-id",
          "prompt": "Tell me if the build log shows a failure or finishes.",
          "inputs": [
            {"kind": "file",    "path": "/tmp/build.log", "tail_bytes": 4096},
            {"kind": "glob",    "pattern": "result*/", "report": "created"},
            {"kind": "cmd",     "argv": ["systemctl", "is-active", "foo"], "timeout_s": 5},
            {"kind": "url",     "url": "http://localhost:8080/health"},
            {"kind": "port",    "host": "127.0.0.1", "port": 5432, "want": "open"},
            {"kind": "process", "pid": 1234, "want": "exited"}
          ],
          "interval_s": 20,
          "deadline_s": 3600,
          "fire": "judge",
          "judge_questions": {"failed": {"type": "noul", "instructions": "Did the build fail?"}},
          "judge_rule": {"question": "failed", "noul_gte": 0.8}
        }"#;
        let request: CreateRequest = serde_json::from_str(literal).unwrap();
        assert_eq!(request.inputs.len(), 6);
        let spec = WatchSpec::parse(request).unwrap();
        assert_eq!(spec.id.as_str(), "optional-stable-id");
        assert_eq!(spec.interval_s, 20);
    }

    #[test]
    fn input_labels_follow_kind_colon_target() {
        // why: SPEC envelope example shows `"input": "file:/tmp/build.log"`;
        //      other kinds use the same `<kind>:<target>` shape.
        assert_eq!(
            Input::File { path: "/tmp/build.log".into(), tail_bytes: None }.label(),
            "file:/tmp/build.log"
        );
        assert_eq!(Input::Glob { pattern: "result*/".into(), report: GlobReport::Created }.label(), "glob:result*/");
        assert_eq!(Input::Url { url: "http://h/x".into() }.label(), "url:http://h/x");
        assert_eq!(Input::Port { host: "127.0.0.1".into(), port: 5432, want: None }.label(), "port:127.0.0.1:5432");
        assert_eq!(Input::Process { pid: 1234, want: None }.label(), "process:1234");
        assert!(Input::Cmd { argv: vec!["systemctl".into(), "is-active".into()], timeout_s: None, cwd: None }
            .label()
            .starts_with("cmd:systemctl is-active"));
    }

    // ---- watch file ----

    #[test]
    fn new_watch_file_is_pending_with_an_absolute_deadline() {
        // why: SPEC "State": created_at and deadline_at are absolute UTC, so
        //      elapsed time survives restarts. deadline_at = created_at + deadline_s.
        // proves: CLAIM(absolute-deadline)
        let mut req = change_request("w", Path::new("/tmp/x"), 20);
        req.deadline_s = Some(3600);
        let spec = WatchSpec::parse(req).unwrap();
        let now = at("2026-10-29T14:03:11Z");
        let file = WatchFile::new(spec, now);
        assert_eq!(file.status, WatchStatus::Pending);
        assert_eq!(file.created_at, now);
        assert_eq!(file.deadline_at, at("2026-10-29T15:03:11Z"));
        assert_eq!(file.tick, 0);
        assert_eq!(file.last_tick_at, None);
        assert!(file.events.is_empty());
        assert!(file.finished_at.is_none() && file.pending_change.is_none());
        assert_eq!(file.input_states.len(), 0, "baseline is recorded by Vigil::create (V1), not by the constructor");
    }

    // ---- V3 / V4: terminal result is set once ----

    fn terminal(status: TerminalStatus, at_s: &str, tick: u64, summary: &str) -> Terminal {
        Terminal {
            status,
            finished_at: at(at_s),
            events: vec![Event {
                event_id: Event::make_id("w", tick, 0),
                tick,
                watch_id: "w".into(),
                input: "file:/tmp/x".into(),
                summary: summary.into(),
                judge: None,
            }],
            digest: summary.into(),
        }
    }

    #[test]
    fn finish_stores_the_terminal_result_once_and_refuses_a_second() {
        // why: V3/V4 (S3, S4): terminal states are immutable. The first result
        //      stored wins; a later fire/expire/cancel returns Err and leaves
        //      status, finished_at, events and digest exactly as stored.
        //      Positive control: the first call changes the file.
        // proves: CLAIM(terminal-immutable)
        let spec = WatchSpec::parse(change_request("w", Path::new("/tmp/x"), 20)).unwrap();
        let mut file = WatchFile::new(spec, at("2026-10-29T14:03:11Z"));
        file.finish(terminal(TerminalStatus::Fired, "2026-10-29T14:04:00Z", 3, "first")).unwrap();
        assert_eq!(file.status, WatchStatus::Fired);
        assert_eq!(file.finished_at, Some(at("2026-10-29T14:04:00Z")));
        assert_eq!(file.events.len(), 1);
        assert_eq!(file.last_digest, "first");

        let before = file.clone();
        for later in [TerminalStatus::Cancelled, TerminalStatus::Expired, TerminalStatus::Fired] {
            let outcome = file.finish(terminal(later, "2026-10-29T14:09:00Z", 9, "second"));
            assert_eq!(outcome, Err(AlreadyFinished(WatchStatus::Fired)), "{later:?} must be refused");
            assert_eq!(file, before, "{later:?} must not change anything");
        }
    }

    #[test]
    fn event_ids_are_always_watch_id_colon_tick_colon_n_counting_from_zero() {
        // why: V3 + W1 (T1): `event_id` is ALWAYS `<watch_id>:<tick>:<n>`,
        //      n counting from 0 within the tick, so two inputs changing in one
        //      tick get distinct ids and the shape never depends on how many
        //      events a tick has. The first event is `:0`, NOT the bare
        //      `<watch_id>:<tick>` of the round-2 default. Literals, not
        //      derived from the function. Ids stay distinct across ticks and
        //      across n (":1:10" vs ":11:0" would collide only without the
        //      colons).
        // proves: CLAIM(event-id)
        assert_eq!(Event::make_id("build", 17, 0), "build:17:0");
        assert_eq!(Event::make_id("build", 17, 1), "build:17:1");
        assert_eq!(Event::make_id("build", 17, 2), "build:17:2");
        assert_eq!(Event::make_id("w", 1, 10), "w:1:10");
        assert_eq!(Event::make_id("w", 11, 0), "w:11:0");
        assert_ne!(Event::make_id("w", 1, 10), Event::make_id("w", 11, 0));
        assert_ne!(Event::make_id("build", 17, 1), Event::make_id("build", 18, 0));
    }

    #[test]
    fn an_overdue_pending_watch_is_detected_at_the_deadline_inclusive() {
        // why: V16: a pending watch past its deadline counts as expired.
        //      deadline_at = created + 3600 s; 1 s before is not overdue, the
        //      deadline instant itself is (matches "deadline passed: yes or
        //      no" with `deadline_at <= now`).
        let spec = WatchSpec::parse(change_request("w", Path::new("/tmp/x"), 20)).unwrap();
        let file = WatchFile::new(spec, at("2026-10-29T14:03:11Z"));
        assert!(!file.is_overdue(at("2026-10-29T15:03:10Z")));
        assert!(file.is_overdue(at("2026-10-29T15:03:11Z")));
        assert!(file.is_overdue(at("2026-11-29T15:03:11Z")));
        let mut done = file.clone();
        done.finish(terminal(TerminalStatus::Cancelled, "2026-10-29T14:04:00Z", 1, "x")).unwrap();
        assert!(!done.is_overdue(at("2027-01-01T00:00:00Z")), "a finished watch is not 'overdue pending'");
    }

    // ---- V19: create-time validation ----

    fn with_rule(test: RuleTest, min_confidence: f64) -> CreateRequest {
        let mut req = judge_req();
        req.judge_rule = Some(JudgeRule { question: "failed".into(), test, min_confidence });
        req
    }

    #[test]
    fn noul_probability_must_lie_in_0_to_1_inclusive() {
        // why: V19 (S19): judge probabilities in 0..=1. Boundaries 0 and 1
        //      pass; just outside, NaN and infinities are errors, not clamps.
        // proves: CLAIM(validation)
        for ok in [0.0, 0.5, 1.0] {
            assert!(WatchSpec::parse(with_rule(RuleTest::NoulGte(ok), 0.0)).is_ok(), "{ok} should pass");
        }
        for bad in [-0.0001, 1.0001, 2.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(
                matches!(WatchSpec::parse(with_rule(RuleTest::NoulGte(bad), 0.0)), Err(CreateError::ProbabilityOutOfRange { .. })),
                "noul_gte {bad} must be rejected"
            );
        }
    }

    #[test]
    fn min_confidence_must_lie_in_0_to_1_inclusive_for_every_rule_kind() {
        // why: V19: `min_confidence` in 0..=1, checked for noul, choice and
        //      score rules alike (score_gte itself is not a probability).
        // proves: CLAIM(validation)
        let tests = [RuleTest::NoulGte(0.5), RuleTest::ChoiceIn(vec!["a".into()]), RuleTest::ScoreGte(2.0)];
        for test in &tests {
            let kind = test.question_type();
            let build = |min_confidence: f64| {
                let mut req = with_rule(test.clone(), min_confidence);
                req.judge_questions = Some(json!({"failed": {"type": kind, "instructions": "?"}}).as_object().unwrap().clone());
                req
            };
            for ok in [0.0, 1.0] {
                assert!(WatchSpec::parse(build(ok)).is_ok(), "{kind} min_confidence {ok}");
            }
            for bad in [-0.01, 1.01, f64::NAN] {
                assert!(
                    matches!(WatchSpec::parse(build(bad)), Err(CreateError::ProbabilityOutOfRange { .. })),
                    "{kind} min_confidence {bad} must be rejected"
                );
            }
        }
        // score_gte is a level, not a probability: 7.5 is fine.
        let mut req = with_rule(RuleTest::ScoreGte(7.5), 0.0);
        req.judge_questions = Some(json!({"failed": {"type": "score", "instructions": "?"}}).as_object().unwrap().clone());
        assert!(WatchSpec::parse(req).is_ok());
    }

    #[test]
    fn deadline_s_is_one_second_to_seven_days_and_overflow_is_an_error() {
        // why: V19 (S19): deadline_s 1..=604800 (7 days = 7*24*3600). 0 and
        //      604801 are errors, so is u64::MAX (would overflow the absolute
        //      deadline_at), never a panic or a clamp.
        // proves: CLAIM(validation)
        let with_deadline = |d: u64| {
            let mut req = change_request("w", Path::new("/tmp/x"), 20);
            req.deadline_s = Some(d);
            WatchSpec::parse(req)
        };
        assert_eq!(with_deadline(1).unwrap().deadline_s, 1);
        assert_eq!(with_deadline(604_800).unwrap().deadline_s, 604_800);
        for bad in [0, 604_801, u64::MAX] {
            assert_eq!(with_deadline(bad), Err(CreateError::DeadlineOutOfRange(bad)), "deadline_s {bad}");
        }
    }

    #[test]
    fn cmd_argv_must_not_be_empty() {
        // why: V19: `cmd.argv` non-empty (an empty argv has no program to run).
        //      Control: one element is fine.
        // proves: CLAIM(validation)
        let cmd = |argv: Vec<String>| {
            let mut req = change_request("w", Path::new("/tmp/x"), 20);
            req.inputs = vec![Input::Cmd { argv, timeout_s: None, cwd: None }];
            WatchSpec::parse(req)
        };
        assert_eq!(cmd(vec![]), Err(CreateError::EmptyArgv));
        assert!(cmd(vec!["true".into()]).is_ok());
    }

    #[test]
    fn url_inputs_must_be_http_https_and_other_schemes_are_errors() {
        // why: V19/D7 (S19): http only (no TLS stack on 8 GB hosts). https,
        //      ftp, file and scheme-less strings are errors naming the offender;
        //      Positive control: http (any case of the scheme) is accepted.
        // proves: CLAIM(validation)
        let url = |u: &str| {
            let mut req = change_request("w", Path::new("/tmp/x"), 20);
            req.inputs = vec![Input::Url { url: u.into() }];
            WatchSpec::parse(req)
        };
        assert!(url("http://localhost:8080/health").is_ok());
        assert!(url("HTTP://localhost/").is_ok());
        for bad in ["https://example.com/", "ftp://example.com/", "file:///etc/passwd", "localhost:8080", ""] {
            assert!(
                matches!(url(bad), Err(CreateError::UnsupportedUrlScheme(ref got)) if got == bad),
                "{bad:?} must be rejected as an unsupported scheme, got {:?}",
                url(bad)
            );
        }
    }

    #[test]
    fn cmd_timeout_s_must_lie_in_a_tenth_of_a_second_to_twenty_seconds() {
        // why: V19 as changed by X4 (T5): timeout_s 0.1..=20 (was 60), both
        //      boundaries inclusive. With a 60 s timeout a valid command could
        //      outlast every >= 30 s keepalive call and, with whole-tick
        //      rollback, never let a tick finish. 20.5, 21, 30 and the old
        //      ceiling 60 are errors now; NaN, infinity, negative and zero stay
        //      errors (an unbounded or zero wait is unsafe).
        // proves: CLAIM(timeout-ceiling)
        let cmd = |t: Option<f64>| {
            let mut req = change_request("w", Path::new("/tmp/x"), 20);
            req.inputs = vec![Input::Cmd { argv: vec!["true".into()], timeout_s: t, cwd: None }];
            WatchSpec::parse(req)
        };
        for ok in [None, Some(0.1), Some(10.0), Some(18.9), Some(19.0)] {
            assert!(cmd(ok).is_ok(), "{ok:?}");
        }
        for bad in [0.0, 0.05, -1.0, 19.1, 20.0, 20.5, 21.0, 30.0, 60.0, 60.5, 3600.0, f64::NAN, f64::INFINITY] {
            assert!(matches!(cmd(Some(bad)), Err(CreateError::TimeoutOutOfRange(_))), "timeout_s {bad}");
        }
    }

    #[test]
    fn read_timeout_is_what_an_input_can_keep_a_read_waiting() {
        // why: X4 (T5): the tick reserve is built from "max(input timeouts)".
        //      cmd: its timeout_s, default 10 (R3); url and port: the reader's
        //      fixed 10 s (DEFAULT_CMD_TIMEOUT_S applies to all waiting
        //      inputs, default taken); file, glob and process read local state
        //      and never wait. Literal durations, boundary 20 s included.
        // proves: CLAIM(tick-reserve)
        use std::time::Duration;
        let cmd = |t: Option<f64>| Input::Cmd { argv: vec!["x".into()], timeout_s: t, cwd: None };
        assert_eq!(cmd(None).read_timeout(), Duration::from_secs(10));
        assert_eq!(cmd(Some(19.0)).read_timeout(), Duration::from_secs(19));
        assert_eq!(cmd(Some(0.5)).read_timeout(), Duration::from_millis(500));
        assert_eq!(Input::Url { url: "http://127.0.0.1:1/".into() }.read_timeout(), Duration::from_secs(10));
        assert_eq!(Input::Port { host: "127.0.0.1".into(), port: 1, want: None }.read_timeout(), Duration::from_secs(10));
        assert_eq!(Input::File { path: "/tmp/x".into(), tail_bytes: None }.read_timeout(), Duration::ZERO);
        assert_eq!(Input::Glob { pattern: "/tmp/*".into(), report: GlobReport::Created }.read_timeout(), Duration::ZERO);
        assert_eq!(Input::Process { pid: 1, want: None }.read_timeout(), Duration::ZERO);
    }

    // ---- V12: size limits ----

    #[test]
    fn a_watch_has_at_most_sixteen_inputs() {
        // why: V12 (S12): 16 inputs ok, 17 is an error carrying the count.
        // proves: CLAIM(limits)
        let with_inputs = |n: usize| {
            let mut req = change_request("w", Path::new("/tmp/x"), 20);
            req.inputs = (0..n).map(|i| Input::File { path: format!("/tmp/f{i}").into(), tail_bytes: None }).collect();
            WatchSpec::parse(req)
        };
        assert!(with_inputs(16).is_ok());
        assert_eq!(with_inputs(17), Err(CreateError::TooManyInputs(17)));
        assert_eq!(with_inputs(1000), Err(CreateError::TooManyInputs(1000)));
    }

    #[test]
    fn prompt_is_at_most_1024_bytes_not_chars() {
        // why: V12 (S12/S15): the prompt is bounded in BYTES so the judge
        //      state keeps room for observations. 1024 ASCII bytes pass, 1025
        //      fail; 341 x "日" = 1023 bytes pass, 342 x "日" = 1026 bytes fail
        //      although both are far below 1024 chars.
        // proves: CLAIM(limits)
        let with_prompt = |p: String| {
            let mut req = change_request("w", Path::new("/tmp/x"), 20);
            req.prompt = p;
            WatchSpec::parse(req)
        };
        assert!(with_prompt("a".repeat(1024)).is_ok());
        assert_eq!(with_prompt("a".repeat(1025)), Err(CreateError::PromptTooLong(1025)));
        assert!(with_prompt("日".repeat(341)).is_ok());
        assert_eq!(with_prompt("日".repeat(342)), Err(CreateError::PromptTooLong(1026)));
    }

    // ---- V11: relative paths resolve against the creating cwd ----

    #[test]
    fn absolutize_resolves_relative_paths_globs_and_cmd_cwd_against_the_creator_cwd() {
        // why: V11 (S11): relative paths, glob patterns and the cmd working
        //      directory are resolved against the CREATING process's cwd and
        //      stored absolute, so resuming from another directory means the
        //      same thing. Absolute values and a given absolute cmd cwd are
        //      untouched (positive controls); a relative cmd cwd joins.
        // proves: CLAIM(creator-cwd)
        let mut req = change_request("w", Path::new("rel/build.log"), 20);
        req.inputs.push(Input::Glob { pattern: "out/*.txt".into(), report: GlobReport::Created });
        req.inputs.push(Input::Glob { pattern: "/abs/*.txt".into(), report: GlobReport::Created });
        req.inputs.push(Input::File { path: "/abs/keep.log".into(), tail_bytes: Some(10) });
        req.inputs.push(Input::Cmd { argv: vec!["pwd".into()], timeout_s: None, cwd: None });
        req.inputs.push(Input::Cmd { argv: vec!["pwd".into()], timeout_s: None, cwd: Some("/already/abs".into()) });
        req.inputs.push(Input::Cmd { argv: vec!["pwd".into()], timeout_s: None, cwd: Some("sub".into()) });
        let spec = WatchSpec::parse(req).unwrap().absolutize(Path::new("/work/a"));
        assert_eq!(spec.inputs[0], Input::File { path: "/work/a/rel/build.log".into(), tail_bytes: None });
        assert_eq!(spec.inputs[1], Input::Glob { pattern: "/work/a/out/*.txt".into(), report: GlobReport::Created });
        assert_eq!(spec.inputs[2], Input::Glob { pattern: "/abs/*.txt".into(), report: GlobReport::Created });
        assert_eq!(spec.inputs[3], Input::File { path: "/abs/keep.log".into(), tail_bytes: Some(10) });
        assert_eq!(spec.inputs[4], Input::Cmd { argv: vec!["pwd".into()], timeout_s: None, cwd: Some("/work/a".into()) });
        assert_eq!(spec.inputs[5], Input::Cmd { argv: vec!["pwd".into()], timeout_s: None, cwd: Some("/already/abs".into()) });
        assert_eq!(spec.inputs[6], Input::Cmd { argv: vec!["pwd".into()], timeout_s: None, cwd: Some("/work/a/sub".into()) });
    }
}
