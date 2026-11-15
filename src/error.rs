//! Error types. One enum per failure domain so callers can handle each case
//! without reading bodies.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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
    /// V19 + X7: `timeout_s` must lie in 0.1..=19.
    #[error("timeout_s must be within 0.1..=19, got {0}")]
    TimeoutOutOfRange(f64),
    /// AI5 (A16): `expect_s` and `stall_s` must be at least one second; zero
    /// would raise attention on the very first tick.
    #[error("{field} must be at least 1 second")]
    BelowOneSecond { field: &'static str },
    /// AI5, review S8: `expect_s` and `stall_s` are at most `MAX_DEADLINE_S`
    /// (604800): no watch lives longer, so a larger limit could never fire, and
    /// values near `u64::MAX` overflow the time arithmetic.
    #[error("{field} must be at most {max} seconds (the longest a watch can run), got {value}")]
    AboveLongestWatch { field: &'static str, max: u64, value: u64 },
    /// W36 (H2): a `--for` / `for` spec that is not `qex:`, `pid:`, `file:`,
    /// `until:` or `idle:`, or whose value is not valid.
    #[error("invalid work {spec:?}: {reason}")]
    InvalidWork { spec: String, reason: String },
    /// W36 (H4): `fire = hold` waits on work, not on files, commands or URLs.
    #[error("a hold waits on work (qex, process with want exited, path_exists, until, qex_idle), not on {0}; use fire = condition for that")]
    NotWork(String),
    /// W36 (H4): a field a hold does not take.
    #[error("{0} is not allowed with fire = hold")]
    HoldRejects(&'static str),
    /// W36 (H3): a hold on a qex job qex does not know is a typo, not a hold.
    #[error("qex job {0:?} not found")]
    QexJobNotFound(String),
    /// W36 (H3): a name or id prefix that names several qex jobs cannot be held on.
    #[error("qex job {job:?} is ambiguous: {detail}; hold on the full job id")]
    QexJobAmbiguous { job: String, detail: String },
    /// W36 (H3): qex did not say which job a name or id prefix is.
    #[error("cannot resolve qex job {job:?} to a job id: {reason}")]
    QexJobUnresolved { job: String, reason: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// What a person can do about a failed write (specs/disk-full.md DF2): free
/// space (`DiskFull`: ENOSPC or EDQUOT) or look at the disk (`Io`: anything
/// else). The wire names are `disk_full` and `io`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    DiskFull,
    Io,
}

impl FailureKind {
    /// How a person reads the kind: the start of a message.
    pub fn label(self) -> &'static str {
        match self {
            FailureKind::DiskFull => "disk full",
            FailureKind::Io => "io error",
        }
    }

    /// CLAIM(DF2): `disk_full` is exactly ENOSPC (28) and EDQUOT (122) [spec
    /// DF2; store tests `the_failed_save_is_described_in_persist_...`]. Linux
    /// numbers; vigil is Linux only (flock, /proc).
    pub fn of(error: &std::io::Error) -> FailureKind {
        match error.raw_os_error() {
            Some(28 | 122) => FailureKind::DiskFull,
            _ => FailureKind::Io,
        }
    }
}

/// Where a failed write was aimed, so its message and the envelope's
/// `persist` note can name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteContext {
    /// The file that could not be written (a watch file, or a lock file).
    pub path: PathBuf,
    /// The state root (DF5b: the message names it); `None` for a bare
    /// `write_atomic` that knows no root.
    pub state_root: Option<PathBuf>,
    /// DF3, S4: the first attempt's OS text when a retry was made and failed
    /// too (then the failure is the retry's).
    pub first_error: Option<String>,
}

/// An I/O failure. `write` is set when the failure is that of a write the
/// store was asked to make; reads leave it `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoFailure {
    /// The OS error text, e.g. `No space left on device (os error 28)`.
    pub os_error: String,
    pub write: Option<WriteContext>,
}

impl IoFailure {
    /// `<what>: <where>: <OS text>`; `what` is `disk full` or `io error`
    /// (DF5b: lowercase `disk full`, and the state root).
    fn describe(&self, what: &str) -> String {
        match &self.write {
            None => format!("{what}: {}", self.os_error),
            Some(write) => match &write.state_root {
                Some(root) => format!("{what}: cannot write {} (state root {}): {}", write.path.display(), root.display(), self.os_error),
                None => format!("{what}: cannot write {}: {}", write.path.display(), self.os_error),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("watch {0} not found")]
    NotFound(String),
    #[error("{}", .0.describe(FailureKind::Io.label()))]
    Io(IoFailure),
    /// ENOSPC or EDQUOT (specs/disk-full.md DF2, DF5).
    #[error("{}", .0.describe(FailureKind::DiskFull.label()))]
    DiskFull(IoFailure),
    #[error("serialization error: {0}")]
    Serialize(String),
    #[error("corrupt watch file: {0}")]
    Corrupt(String),
}

impl StoreError {
    /// A failed read or other non-write I/O.
    pub fn io(error: std::io::Error) -> StoreError {
        StoreError::Io(IoFailure { os_error: error.to_string(), write: None })
    }

    /// A failed write, classified by its errno (DF2). Borrows the error: the
    /// store describes a first failure to a caller before it knows whether a
    /// rescue will replace it (review S4).
    pub fn write_failed(error: &std::io::Error, context: WriteContext) -> StoreError {
        let failure = IoFailure { os_error: error.to_string(), write: Some(context) };
        match FailureKind::of(error) {
            FailureKind::DiskFull => StoreError::DiskFull(failure),
            FailureKind::Io => StoreError::Io(failure),
        }
    }

    /// The failure of a write the store was asked to make, with what the
    /// envelope's `persist` note needs; `None` for every other error (a read,
    /// a missing watch, a corrupt file, a value that cannot be serialized).
    pub fn write_failure(&self) -> Option<WriteFailure<'_>> {
        let (kind, failure) = match self {
            StoreError::DiskFull(failure) => (FailureKind::DiskFull, failure),
            StoreError::Io(failure) => (FailureKind::Io, failure),
            _ => return None,
        };
        let context = failure.write.as_ref()?;
        Some(WriteFailure { kind, os_error: &failure.os_error, path: &context.path, first_error: context.first_error.as_deref() })
    }
}

/// A borrowed view of a failed write, for building the `persist` note.
#[derive(Debug, Clone, Copy)]
pub struct WriteFailure<'a> {
    pub kind: FailureKind,
    pub os_error: &'a str,
    pub path: &'a std::path::Path,
    pub first_error: Option<&'a str>,
}

/// Why `vigil hold` / `hold add` / `hold done` refused to make or change a
/// hold's items (W42; `items.rs`). Every refusal changes nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ItemError {
    #[error("{0}")]
    CapReached(String),
    #[error("cap --until must be a future RFC3339 time")]
    CapUntil,
    /// Items belong to holds only.
    #[error("watch {0} is not a hold; `hold add` and `hold done` change holds made with `vigil hold`")]
    NotAHold(String),
    /// W42-H2, D4: the hold was released, cancelled or has expired (an expired
    /// hold counts even before a keepalive writes it, V16).
    #[error("hold {id} has ended ({how}); nothing keeps it alive any more, make a new hold")]
    HoldEnded { id: String, how: &'static str },
    /// D6: a hold stored without items (an older vigil's, or `watch_create`'s).
    #[error("hold {0} has no items (made by watch_create or an older vigil); make a new hold with `vigil hold`")]
    NoItems(String),
    /// D5: items count toward the input cap.
    #[error("a hold has at most {max} live items, and this one would have {wanted}; wait for work to end or make a new hold")]
    TooManyItems { max: usize, wanted: usize },
    /// D5: labels are unique per hold (an unlabelled item is labelled by its spec).
    #[error("this hold already has an item labelled {0:?}; labels are unique per hold, give --label")]
    DuplicateLabel(String),
    /// D5: `--label` names one item.
    #[error("--label names one item, but {0} were given; use one --for with --label, and `hold add` for more")]
    LabelNeedsOneItem(usize),
    /// W42-H5: only `hold done <id> <label>` ends a `none` item, so it needs a label.
    #[error("a `none` item needs a --label: only `vigil hold done <id> <label>` ends it")]
    NoneNeedsLabel,
    /// An explicit label that cannot be shown in a one-line event.
    #[error("invalid label {label:?}: {reason}")]
    InvalidLabel { label: String, reason: &'static str },
    /// D5: `--expect-s` applies to each item; a hold with no work has none.
    #[error("expect_s applies to each item, and a bare hold has none; add items with `vigil hold add <id> --for ... --expect-s N`")]
    ExpectWithoutItems,
    /// W46-K1: `--cap` classes each item; a hold with no items has nothing to class.
    #[error("cap applies to each item, and a bare hold has none; add items with `vigil hold add <id> --for ... --cap work`")]
    CapWithoutItems,
    /// W42-H5: `hold done` names an item that is not there.
    #[error("hold {id} has no item labelled {label:?}; its items are: {labels}")]
    UnknownLabel { id: String, label: String, labels: String },
    /// D5: a sensed item ends by itself; `hold done` is for `none` items.
    #[error("item {label:?} is sensed ({spec}) and ends by itself; `hold done` ends only `none` items")]
    SensedItem { label: String, spec: String },
    /// D5: `done` on an item that has already ended.
    #[error("item {0:?} already ended")]
    AlreadyEnded(String),
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum VigilError {
    #[error(transparent)]
    Create(#[from] CreateError),
    #[error(transparent)]
    Item(#[from] ItemError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("watch {0} not found")]
    NotFound(String),
    /// `keepalive` with no id and no pending watch created by this process.
    #[error("no watches to cover")]
    NothingToCover,
    #[error("{0}")]
    WaitScope(String),
    #[error(transparent)]
    Advisory(#[from] crate::advisory::AdvisoryError),
    /// Orchestrator mode asked for without the day's token: the challenge.
    #[error("{0}")]
    OrchestratorUnconfirmed(String),
}

impl VigilError {
    pub fn not_found(id: &WatchId) -> Self {
        VigilError::NotFound(id.as_str().to_string())
    }
}
