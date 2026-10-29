//! Error types. One enum per failure domain so callers can handle each case
//! without reading bodies.

use crate::watch::WatchId;

/// Why `watch_create` refused a request (SPEC "watch_create").
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CreateError {
    #[error("invalid watch id {0:?}: must match [A-Za-z0-9_-]{{1,64}}")]
    InvalidId(String),
    /// V6: any id that still has a file (pending or finished) is taken.
    #[error("a watch with id {0:?} already exists (pending or finished); use a fresh id")]
    DuplicateId(String),
    #[error("fire = judge requires both judge_questions and judge_rule")]
    MissingJudgeFields,
    #[error("judge_questions / judge_rule are only allowed with fire = judge")]
    UnexpectedJudgeFields,
    #[error("judge_questions must be a non-empty object")]
    EmptyQuestions,
    #[error("judge_rule.question {0:?} is not a key of judge_questions")]
    UnknownRuleQuestion(String),
    #[error("rule kind {rule_kind} does not match question {question:?} of type {question_type:?}")]
    RuleKindMismatch {
        question: String,
        question_type: Option<String>,
        rule_kind: String,
    },
    #[error("fire = condition needs at least one input with a `want`")]
    ConditionNeedsWant,
    #[error("the judge is unreachable")]
    JudgeUnreachable,
    /// V12: at most 16 inputs per watch.
    #[error("a watch has at most 16 inputs, got {0}")]
    TooManyInputs(usize),
    /// V12: `prompt` is at most 1024 bytes.
    #[error("prompt is at most 1024 bytes, got {0}")]
    PromptTooLong(usize),
    /// V19: judge probabilities and `min_confidence` must lie in 0..=1.
    #[error("{field} must be within 0..=1, got {value}")]
    ProbabilityOutOfRange { field: String, value: f64 },
    /// V19: `deadline_s` must lie in 1..=604800.
    #[error("deadline_s must be within 1..=604800, got {0}")]
    DeadlineOutOfRange(u64),
    /// V19: `cmd.argv` must be non-empty.
    #[error("cmd.argv must not be empty")]
    EmptyArgv,
    /// V19/D7: URL inputs are http only.
    #[error("only http URLs are supported, got {0:?}")]
    UnsupportedUrlScheme(String),
    /// V19 + X4: `timeout_s` must lie in 0.1..=20.
    #[error("timeout_s must be within 0.1..=20, got {0}")]
    TimeoutOutOfRange(f64),
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("watch {0} not found")]
    NotFound(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("serialization error: {0}")]
    Serialize(String),
    #[error("corrupt watch file: {0}")]
    Corrupt(String),
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum VigilError {
    #[error(transparent)]
    Create(#[from] CreateError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("watch {0} not found")]
    NotFound(String),
    /// `keepalive` with no id and no pending watch created by this process.
    #[error("no watches to cover")]
    NothingToCover,
}

impl VigilError {
    pub fn not_found(id: &WatchId) -> Self {
        VigilError::NotFound(id.as_str().to_string())
    }
}
