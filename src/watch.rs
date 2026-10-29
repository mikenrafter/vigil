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
use crate::judge::{Answer, JudgeRule};

pub const MIN_INTERVAL_S: u64 = 1;
pub const MAX_INTERVAL_S: u64 = 3600;
pub const DEFAULT_INTERVAL_S: u64 = 20;
pub const DEFAULT_DEADLINE_S: u64 = 3600;
pub const MAX_EVENTS: usize = 50;

/// A validated watch id: `[A-Za-z0-9_-]{1,64}`. Parse, do not validate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct WatchId(String);

impl WatchId {
    pub fn parse(raw: &str) -> Result<WatchId, CreateError> {
        todo!()
    }
    /// A fresh, valid id. Two calls never return the same id.
    pub fn generate() -> WatchId {
        todo!()
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
        todo!("deserialize a string, then WatchId::parse")
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
        /// Seconds; fractions allowed. Default 10.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_s: Option<f64>,
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
        todo!()
    }
    /// Does this input carry a `want` (used by fire = condition)?
    pub fn has_want(&self) -> bool {
        todo!()
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
    /// Validate a raw request (SPEC "watch_create"). Does not touch disk, so
    /// duplicate-id and judge-reachability checks live in `Vigil::create`.
    pub fn parse(request: CreateRequest) -> Result<WatchSpec, CreateError> {
        todo!()
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

/// What `read_input` remembers between ticks, per input (same order as
/// `WatchSpec::inputs`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct InputState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// File inputs: byte offset already reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// Glob inputs: matches already seen.
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
/// (SPEC "Failure rules": a pending change counts as a change).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PendingChange {
    pub changed: Vec<ChangedInput>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub tick: u64,
    pub watch_id: String,
    pub input: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge: Option<BTreeMap<String, Answer>>,
}

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
    /// At most `MAX_EVENTS`, oldest dropped first.
    pub events: Vec<Event>,
    /// Highest event tick already returned to the agent.
    pub delivered_through: u64,
    /// Digest of the last change, shown at expiry.
    pub last_digest: String,
}

impl WatchFile {
    /// A new pending watch: tick 0, `deadline_at = now + spec.deadline_s`.
    pub fn new(spec: WatchSpec, now: Timestamp) -> WatchFile {
        todo!()
    }
    /// Append, keeping only the newest `MAX_EVENTS`.
    pub fn push_event(&mut self, event: Event) {
        todo!()
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
        assert!(Input::Cmd { argv: vec!["systemctl".into(), "is-active".into()], timeout_s: None }
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
        assert_eq!(file.delivered_through, 0);
    }

    #[test]
    fn events_are_capped_to_the_newest_fifty() {
        // why: SPEC "State": "the events list (last 50)". Push 60 (ticks
        //      1..=60): 50 remain and the oldest kept is tick 11.
        let spec = WatchSpec::parse(change_request("w", Path::new("/tmp/x"), 20)).unwrap();
        let mut file = WatchFile::new(spec, at("2026-10-29T14:03:11Z"));
        for tick in 1..=60 {
            file.push_event(Event { tick, watch_id: "w".into(), input: "file:/tmp/x".into(), summary: "s".into(), judge: None });
        }
        assert_eq!(file.events.len(), 50);
        assert_eq!(file.events.first().unwrap().tick, 11);
        assert_eq!(file.events.last().unwrap().tick, 60);
    }
}
