//! Holds (SPEC "Holds", claims H1-H12): a watch whose inputs are the work an
//! orchestrator is waiting on, so it ends exactly when the work ends and the
//! agent never re-arms it while work is live.
//!
//! This module owns the front door (`Work` specs, `HoldRequest`,
//! `Vigil::hold`) and the pure readers of qex's output. The rest lives where
//! the watch machinery lives: `Input::{Qex, QexIdle, PathExists, Until}` and
//! `FireRule::Hold` in `watch.rs`, their reads in `inputs.rs`, the release in
//! `tick.rs`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::clock::Timestamp;
use crate::engine::{Created, Vigil};
use crate::error::{CreateError, VigilError};
use crate::watch::{CreateRequest, FireKind, Input, ProcessWant, MAX_DEADLINE_S};

/// H5: a hold lives this long unless asked for less (the longest any watch lives).
pub const HOLD_DEADLINE_S: u64 = MAX_DEADLINE_S;
/// H5: seconds between a hold's ticks (each runs a `qex` command).
pub const HOLD_INTERVAL_S: u64 = 30;

/// One unit of work a hold waits on (the `--for` spec).
#[derive(Debug, Clone, PartialEq)]
pub enum Work {
    Qex(String),
    Pid(u32),
    Exists(PathBuf),
    Until(Timestamp),
    Idle(PathBuf),
}

const USAGE: &str = "expected qex:ID, pid:N, file:PATH, until:RFC3339 or idle:DIR";

/// A qex job id, id prefix or name: what is safe to hand to `qex status` as a
/// single argument that can never read as an option.
fn valid_job(job: &str) -> bool {
    (1..=128).contains(&job.len())
        && !job.starts_with('-')
        && job.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

impl Work {
    /// Parse `qex:<job>`, `pid:<pid>`, `file:<path>`, `until:<RFC3339>` or
    /// `idle:<dir>` (H2).
    pub fn parse(spec: &str) -> Result<Work, CreateError> {
        let bad = |reason: &str| CreateError::InvalidWork { spec: spec.to_string(), reason: reason.to_string() };
        let (kind, value) = spec.split_once(':').ok_or_else(|| bad(USAGE))?;
        match kind {
            "qex" if value.is_empty() => Err(bad("empty job id")),
            "qex" if !valid_job(value) => Err(bad("job id must be 1-128 letters, digits, '.', '_' or '-', and not start with '-'")),
            "qex" => Ok(Work::Qex(value.to_string())),
            "pid" => match value.parse::<u32>() {
                Ok(pid) if pid > 0 => Ok(Work::Pid(pid)),
                _ => Err(bad("pid must be a positive integer")),
            },
            "file" if value.is_empty() => Err(bad("empty path")),
            "file" => Ok(Work::Exists(PathBuf::from(value))),
            "until" => chrono::DateTime::parse_from_rfc3339(value)
                .map(|at| Work::Until(Timestamp(at.with_timezone(&chrono::Utc))))
                .map_err(|_| bad("time must be RFC 3339, e.g. 2026-11-12T20:00:00Z")),
            "idle" if value.is_empty() => Err(bad("empty directory")),
            "idle" => Ok(Work::Idle(PathBuf::from(value))),
            _ => Err(bad(USAGE)),
        }
    }

    pub fn into_input(self) -> Input {
        match self {
            Work::Qex(job) => Input::Qex { job },
            Work::Pid(pid) => Input::Process { pid, want: Some(ProcessWant::Exited) },
            Work::Exists(path) => Input::PathExists { path },
            Work::Until(at) => Input::Until { at },
            Work::Idle(dir) => Input::QexIdle { dir },
        }
    }
}

/// The qex states that mean the job has stopped (`qex list --state` names them).
/// Only these end a hold: a state not listed (a newer qex's) is NOT an end, and
/// the live ones (queued, starting, running) are simply not listed.
const ENDED_STATES: [&str; 8] = ["completed", "failed", "killed", "timeout", "expired", "oom", "cancelled", "skipped"];

/// Has the work behind `input` ended, given what its last reading said (H2)?
/// An unknown qex state is NOT an end: a newer qex must not release holds early.
pub fn ended(input: &Input, text: &str) -> bool {
    match input {
        Input::Qex { .. } => text == "gone" || ENDED_STATES.contains(&text),
        Input::QexIdle { .. } => text == "idle",
        Input::PathExists { .. } => text == "present",
        Input::Until { .. } => text == "reached",
        Input::Process { .. } => text == "exited",
        _ => false,
    }
}

/// `error: <first line>`, bounded.
fn error_reading(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("no output");
    format!("error: {}", crate::tick::cap_bytes(line, 160))
}

/// Split the command reader's `exit=<code>\n<output>` into code and output;
/// `None` for its own error texts (spawn failure, timeout).
fn split_exit(cmd_text: &str) -> Option<(&str, &str)> {
    let rest = cmd_text.strip_prefix("exit=")?;
    Some(rest.split_once('\n').unwrap_or((rest, "")))
}

/// The JSON value in `output` between the first `open` and the last `close`
/// (stderr hints may surround it).
fn json_in(output: &str, open: char, close: char) -> Option<serde_json::Value> {
    let (from, to) = (output.find(open)?, output.rfind(close)?);
    serde_json::from_str(output.get(from..=to)?).ok()
}

/// The lines of qex's output with its `qex: ` prefix and indentation removed.
fn sentences(output: &str) -> impl Iterator<Item = &str> {
    output.lines().filter(|line| !line.starts_with(' ')).map(|line| line.trim().strip_prefix("qex: ").unwrap_or(line.trim()))
}

/// qex's own "there is no job ..." sentence: no record, no history of it.
/// (Matched at the start of a sentence: an ambiguity listing indents the job
/// names it prints, so a job NAMED like this sentence is not mistaken for it.)
fn says_no_such_job(output: &str) -> bool {
    sentences(output).any(|sentence| sentence.starts_with("there is no job"))
}

/// qex's removed-record sentence for exactly this job id (`describe_missing`
/// in qex/src/history.rs): "the job ID (NAME) existed ...". qex still accepted
/// the id, and the record was deleted by `qex clean` or the history prune.
fn says_record_removed(output: &str, job: &str) -> bool {
    let lead = format!("the job {job} (");
    sentences(output).any(|sentence| sentence.starts_with(&lead) && sentence.contains(") existed"))
}

/// qex's ambiguity sentence ("`NAME` names N jobs. Give the id ..."), if any.
fn ambiguity(output: &str) -> Option<&str> {
    sentences(output).find(|sentence| sentence.starts_with('`') && sentence.contains("` names ") && sentence.contains(" jobs"))
}

/// A canonical qex job id (a UUID), as opposed to a name or an id prefix.
fn is_job_id(job: &str) -> bool {
    job.len() == 36 && job.bytes().enumerate().all(|(at, byte)| if matches!(at, 8 | 13 | 18 | 23) { byte == b'-' } else { byte.is_ascii_hexdigit() })
}

/// The reading of a `qex status JOB --json --no-logs` run (`cmd_text` is the
/// command reader's text, `exit=<code>\n<output>`, or its error text): the
/// job's state name; `gone` when qex has no such job or says it deleted this
/// job's record (both end the work); else `error: ...`. An ambiguous name also
/// exits 127 and is an error: the job exists.
pub fn qex_status_reading(job: &str, cmd_text: &str) -> String {
    let Some((code, output)) = split_exit(cmd_text) else { return error_reading(cmd_text) };
    match code {
        "0" => match json_in(output, '{', '}').as_ref().and_then(|value| value.get("state")).and_then(serde_json::Value::as_str) {
            Some(state) if (1..=32).contains(&state.len()) && state.bytes().all(|byte| byte.is_ascii_lowercase() || byte == b'_') => state.to_string(),
            _ => error_reading("qex answered no state"),
        },
        "127" if says_no_such_job(output) || says_record_removed(output, job) => "gone".to_string(),
        _ => error_reading(&format!("exit {code}: {output}")),
    }
}

/// H3: which job a `qex:JOB` spec is, from the text of `qex status JOB --json
/// --no-logs`. A name or id prefix resolves to the full id in the record; an
/// unknown job, an ambiguous name, or a qex that cannot answer is refused with
/// its cause. A full id qex accepted but whose record was removed is a real job
/// that ended (the first tick releases the hold). A full id needs no lookup,
/// so a qex that cannot answer does not refuse it.
pub fn qex_resolution(job: &str, cmd_text: &str) -> Result<String, CreateError> {
    let unknown = |reason: String| if is_job_id(job) { Ok(job.to_string()) } else { Err(CreateError::QexJobUnresolved { job: job.to_string(), reason }) };
    let Some((code, output)) = split_exit(cmd_text) else { return unknown(error_reading(cmd_text)) };
    match code {
        "0" => match json_in(output, '{', '}').as_ref().and_then(|value| value.get("id")).and_then(serde_json::Value::as_str) {
            Some(id) if valid_job(id) => Ok(id.to_string()),
            _ => unknown("qex answered no job id".to_string()),
        },
        "127" if says_no_such_job(output) => Err(CreateError::QexJobNotFound(job.to_string())),
        "127" if says_record_removed(output, job) => Ok(job.to_string()),
        "127" if ambiguity(output).is_some() => Err(CreateError::QexJobAmbiguous { job: job.to_string(), detail: crate::tick::cap_bytes(ambiguity(output).unwrap_or_default(), 160) }),
        _ => unknown(error_reading(&format!("exit {code}: {output}"))),
    }
}

/// The `qex list` arguments of an idle reading (H10): the jobs at or under
/// `dir` that have not ended (`active` is queued, starting and running), never
/// the history. Qex keeps terminal jobs for a day or more, and listing them is
/// megabytes against the 64 KiB command capture.
pub fn idle_argv(dir: &std::path::Path) -> Vec<String> {
    ["qex", "list", "--under"].into_iter().map(String::from).chain([dir.to_string_lossy().into_owned()]).chain(["--state", "active", "--json"].map(String::from)).collect()
}

/// The reading of an `idle_argv` run: `idle`, or `busy: N active (ID, ...)`, or
/// `error: ...`. Every job qex returns for the query is active unless its
/// record says it ended, so a state a newer qex adds counts as active. A list
/// the command capture cut short is still plainly busy: it is read for the job
/// ids it holds, and the count is then a lower bound (`N+`).
pub fn qex_idle_reading(cmd_text: &str) -> String {
    let Some((code, output)) = split_exit(cmd_text) else { return error_reading(cmd_text) };
    if code != "0" {
        return error_reading(&format!("exit {code}: {output}"));
    }
    let (ids, more): (Vec<String>, bool) = match json_in(output, '[', ']') {
        Some(serde_json::Value::Array(jobs)) => (
            jobs.iter()
                .filter(|job| !job.get("state").and_then(serde_json::Value::as_str).is_some_and(|state| ENDED_STATES.contains(&state)))
                .map(|job| job.get("id").and_then(serde_json::Value::as_str).unwrap_or("?").to_string())
                .collect(),
            false,
        ),
        _ if output.contains(crate::inputs::TRUNCATED_MARKER) => {
            // The cut JSON is qex's pretty-printed array: a job's own `id` is the
            // only key at four spaces of indent (nested records sit deeper).
            let ids = output.lines().filter_map(|line| line.strip_prefix("    \"id\": \"")?.split('"').next().map(str::to_string)).collect::<Vec<_>>();
            if ids.is_empty() {
                return error_reading("qex answered no job list");
            }
            (ids, true)
        }
        _ => return error_reading("qex answered no job list"),
    };
    if ids.is_empty() {
        return "idle".to_string();
    }
    let mut shown: Vec<String> = ids.iter().take(3).map(|id| id.chars().take(8).collect()).collect();
    if ids.len() > 3 {
        shown.push("...".to_string());
    }
    format!("busy: {}{} active ({})", ids.len(), if more { "+" } else { "" }, shown.join(", "))
}

/// `vigil hold` / the `hold` tool (H11).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HoldRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// `qex:ID`, `pid:N`, `file:PATH`, `until:RFC3339`, `idle:DIR`.
    #[serde(default, rename = "for", skip_serializing_if = "Vec::is_empty")]
    pub work: Vec<String>,
    /// Sugar for `idle:<cwd>` (H10).
    #[serde(default)]
    pub until_idle: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_s: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_s: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestrator: Option<bool>,
    /// W42-H1: the label of the hold's first item (`--label`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// W42-H2: one item to add to a live hold (`vigil hold add`, MCP `hold_add`).
/// `work` is a `--for` spec, or `none` for a bare labelled item that only
/// `hold done` (or `expect_s`) acts on (W42-H5).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HoldItemRequest {
    #[serde(rename = "for")]
    pub work: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_s: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Vigil {
    /// RED STUB (W42-H2): add an item to the live hold `id`. Not implemented.
    pub async fn hold_add(&self, id: &crate::watch::WatchId, item: HoldItemRequest) -> Result<Created, VigilError> {
        let _ = item;
        Err(VigilError::NotFound(format!("{id}: hold items are not implemented")))
    }

    /// RED STUB (W42-H5): end the item `label` of the live hold `id`. Not implemented.
    pub async fn hold_done(&self, id: &crate::watch::WatchId, label: &str) -> Result<(), VigilError> {
        Err(VigilError::NotFound(format!("{id}/{label}: hold items are not implemented")))
    }
}

impl Vigil {
    /// Create a hold (H1-H5, H10).
    ///
    /// Every spec is parsed before anything is read or written. Each `qex` job
    /// is resolved to its full job id, which is what the hold stores (H3): a
    /// name reused later cannot make the hold read another job, a job qex does
    /// not know is refused (a typo must not become a hold), and so is a name
    /// that is ambiguous.
    pub async fn hold(&self, request: HoldRequest) -> Result<Created, VigilError> {
        let mut inputs = request.work.iter().map(|spec| Work::parse(spec).map(Work::into_input)).collect::<Result<Vec<_>, _>>()?;
        if request.until_idle {
            inputs.push(Work::Idle(self.cwd.clone()).into_input());
        }
        for input in &mut inputs {
            if let Input::Qex { job } = input {
                *job = self.reader.resolve_qex(job).await?;
            }
        }
        self.create(CreateRequest {
            id: request.id,
            prompt: String::new(),
            inputs,
            interval_s: Some(HOLD_INTERVAL_S),
            deadline_s: request.deadline_s,
            fire: FireKind::Hold,
            judge_questions: None,
            judge_rule: None,
            expect_s: request.expect_s,
            stall_s: None,
            orchestrator: request.orchestrator,
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::at;
    use serde_json::json;

    fn wire(work: &str) -> serde_json::Value {
        serde_json::to_value(Work::parse(work).unwrap().into_input()).unwrap()
    }

    #[test]
    fn each_work_spec_becomes_the_documented_input() {
        // why: H2 table. The wire shapes are what a watch file and a
        //      `watch_create` carry, so they are pinned as literals. A pid is
        //      held on `exited` (H4 requires the want).
        // proves: CLAIM(H2)
        assert_eq!(wire("qex:ab12cd"), json!({"kind": "qex", "job": "ab12cd"}));
        assert_eq!(wire("pid:4242"), json!({"kind": "process", "pid": 4242, "want": "exited"}));
        assert_eq!(wire("file:/tmp/done.marker"), json!({"kind": "path_exists", "path": "/tmp/done.marker"}));
        assert_eq!(wire("until:2026-11-12T20:00:00Z"), json!({"kind": "until", "at": "2026-11-12T20:00:00Z"}));
        assert_eq!(wire("idle:/work/repo"), json!({"kind": "qex_idle", "dir": "/work/repo"}));
        // An offset is read as the same instant in UTC.
        assert_eq!(Work::parse("until:2026-11-12T22:00:00+02:00").unwrap(), Work::Until(at("2026-11-12T20:00:00Z")));
        // A job may be named by an id prefix or a name; a path may contain ':'.
        assert_eq!(Work::parse("qex:nix-build_x.1").unwrap(), Work::Qex("nix-build_x.1".into()));
        assert_eq!(Work::parse("file:/a:b/c").unwrap(), Work::Exists("/a:b/c".into()));
    }

    #[test]
    fn a_bad_work_spec_is_an_error_that_names_the_spec_and_the_reason() {
        // why: H2/H11: errors, not guesses. Each bad spec is refused with its
        //      own text in the message; `qex:` values that qex could read as an
        //      option (leading '-') or that carry spaces are refused so a spec
        //      can never become a qex flag.
        // proves: CLAIM(H2)
        let cases = [
            ("", "work"),
            ("nonsense", "qex:"),
            ("boat:bx_1", "qex:"),
            ("qex:", "empty"),
            ("qex:-r", "job"),
            ("qex:a b", "job"),
            ("pid:abc", "pid"),
            ("pid:0", "pid"),
            ("pid:99999999999", "pid"),
            ("file:", "empty"),
            ("until:tomorrow", "RFC 3339"),
            ("idle:", "empty"),
        ];
        for (spec, needle) in cases {
            let error = Work::parse(spec).expect_err(spec).to_string();
            assert!(error.contains(&format!("{spec:?}")), "{spec:?}: the message names the spec: {error}");
            assert!(error.contains(needle), "{spec:?}: expected {needle:?} in {error}");
        }
    }

    #[test]
    fn qex_status_output_reads_as_the_job_state() {
        // why: H2: the reading is qex's own state word; terminal and live
        //      states are as `qex list --help` names them. Real shape of
        //      `qex status ID --json --no-logs` (captured 2026-11-12): one
        //      object with a "state" key, after the command reader's
        //      `exit=0\n`. stderr noise before the JSON is tolerated.
        // proves: CLAIM(H2)
        for state in ["queued", "starting", "running", "completed", "failed", "killed", "timeout", "expired", "oom", "cancelled", "skipped"] {
            let text = format!("exit=0\n{{\"id\": \"8c4e\", \"state\": \"{state}\", \"exit_code\": null}}\n");
            assert_eq!(qex_status_reading("8c4e", &text), state);
        }
        assert_eq!(qex_status_reading("8c4e", "exit=0\nqex: a hint on stderr\n{\"state\": \"running\"}\n"), "running");
    }

    #[test]
    fn qex_not_knowing_the_job_is_gone_and_every_other_failure_is_an_error_not_an_end() {
        // why: H2 (decision): qex prunes old jobs, so its "there is no job"
        //      answer (exit 127, captured) ends a hold; but a qex that is not
        //      installed, times out, answers garbage or exits for any other
        //      reason must NEVER end it: the cost of an early release is the
        //      lost cache.
        // proves: CLAIM(H2), CLAIM(H3)
        assert_eq!(qex_status_reading("nosuchid", "exit=127\nqex: there is no job or pipeline with the id or the name `nosuchid`\n"), "gone");
        for text in [
            "cannot run qex: No such file or directory (os error 2)",
            "timeout after 10s",
            "exit=1\nqex: the queue is locked\n",
            "exit=127\nsh: qex: not found\n",
            "exit=0\nnot json at all",
            "exit=0\n{\"no_state\": 1}",
            "exit=0\n",
        ] {
            let reading = qex_status_reading("x", text);
            assert!(reading.starts_with("error: "), "{text:?} -> {reading:?}");
            assert!(!ended(&Input::Qex { job: "x".into() }, &reading), "{text:?} must not end a hold");
            assert!(reading.len() <= 200, "bounded: {reading:?}");
        }
    }

    #[test]
    fn qex_list_output_reads_as_idle_or_busy_by_the_live_states() {
        // why: H10: idle means no job in queued, starting or running. Jobs in
        //      any other state are history. A busy reading names the count and
        //      the first ids (8 chars) so the agent sees what it still waits on.
        // proves: CLAIM(H10)
        let job = |id: &str, state: &str| json!({"id": id, "state": state, "cwd": "/w"});
        let list = |jobs: Vec<serde_json::Value>| format!("exit=0\n{}\n", serde_json::Value::Array(jobs));
        assert_eq!(qex_idle_reading(&list(vec![])), "idle");
        assert_eq!(qex_idle_reading(&list(vec![job("a1", "completed"), job("a2", "failed"), job("a3", "cancelled")])), "idle");
        assert_eq!(qex_idle_reading(&list(vec![job("aaaaaaaa-1111", "running"), job("a2", "completed")])), "busy: 1 active (aaaaaaaa)");
        assert_eq!(
            qex_idle_reading(&list(vec![job("11111111-x", "queued"), job("22222222-x", "starting"), job("33333333-x", "running")])),
            "busy: 3 active (11111111, 22222222, 33333333)"
        );
        let many: Vec<_> = (0..5).map(|n| job(&format!("{n}0000000-x"), "running")).collect();
        assert_eq!(qex_idle_reading(&list(many)), "busy: 5 active (00000000, 10000000, 20000000, ...)");
        for text in ["cannot run qex: No such file or directory (os error 2)", "exit=1\nqex: boom\n", "exit=0\n{\"not\": \"an array\"}", "exit=0\n"] {
            let reading = qex_idle_reading(text);
            assert!(reading.starts_with("error: "), "{text:?} -> {reading:?}");
            assert!(!ended(&Input::QexIdle { dir: "/w".into() }, &reading), "an unreadable queue is not an idle one");
        }
    }

    // ---- real qex output (captured 2026-11-12 from the installed qex, a fork
    //      of 0.33; files under tests/fixtures/qex) ----

    const COMPLETED_ID: &str = "0d31c8ac-25e4-4ab2-8a3b-9b24ccd533c9";
    const STATUS_COMPLETED: &str = include_str!("../tests/fixtures/qex/status_completed.json");
    const STATUS_REMOVED: &str = include_str!("../tests/fixtures/qex/status_removed_completed.err");
    const STATUS_UNKNOWN_ID: &str = include_str!("../tests/fixtures/qex/status_unknown_id.err");
    const STATUS_UNKNOWN_NAME: &str = include_str!("../tests/fixtures/qex/status_unknown_name.err");
    const STATUS_AMBIGUOUS: &str = include_str!("../tests/fixtures/qex/status_ambiguous.err");
    const LIST_RUNNING: &str = include_str!("../tests/fixtures/qex/list_active_running.json");
    const LIST_EMPTY: &str = include_str!("../tests/fixtures/qex/list_active_empty.json");
    const LIST_UNDER_MISSING: &str = include_str!("../tests/fixtures/qex/list_under_missing.err");

    /// What the command reader hands over: `exit=<code>\n<stdout then stderr>`.
    fn ran(code: i32, stdout: &str, stderr: &str) -> String {
        format!("exit={code}\n{stdout}{stderr}")
    }

    #[test]
    fn a_removed_record_is_an_end_in_every_form_qex_words_it_and_never_for_another_job() {
        // why: S2. `qex clean` (or the history prune) before the next tick must
        //      release the hold, not strand it: qex still ACCEPTS the id but
        //      answers exit 127 with "the job ID (NAME) existed ...". The first
        //      case is a real capture; the other three are the other branches
        //      of qex's `describe_missing` (qex/src/history.rs), worded as in
        //      that source (derived, not captured: they need a cancelled job
        //      and a hand-damaged state directory). A record removed for ANOTHER
        //      job id is not this job's end; an exit other than 127 is not either.
        // proves: CLAIM(H2)
        let removed = ran(127, "", STATUS_REMOVED);
        assert!(STATUS_REMOVED.contains("existed and its state was `completed`"), "fixture is the real message");
        assert_eq!(qex_status_reading(COMPLETED_ID, &removed), "gone");
        let derived = [
            format!("qex: the job {COMPLETED_ID} (nightly) existed and its state was `cancelled`. Something deleted its record 3m ago, with `qex clean` or by a deletion of the state directory. The job NEVER RAN. Submit it again if you still want the work.\n"),
            format!("qex: the job {COMPLETED_ID} (nightly) existed. Something deleted its record 3m ago. qex does not know how that job ended.\n"),
            format!("qex: the job {COMPLETED_ID} (nightly) existed, and qex accepted it 3m ago, but its record is gone and qex did not delete it. Something removed the state directory. The job may have run.\n"),
        ];
        for text in derived {
            assert_eq!(qex_status_reading(COMPLETED_ID, &ran(127, "", &text)), "gone", "{text}");
        }
        let other = "07193384-6827-4b1d-ba7f-1abc7fc9c0f8";
        let reading = qex_status_reading(other, &removed);
        assert!(reading.starts_with("error: "), "another job's removal is not this job's end: {reading}");
        assert!(qex_status_reading(COMPLETED_ID, &ran(1, "", STATUS_REMOVED)).starts_with("error: "), "only exit 127 is a missing job");
    }

    #[test]
    fn unknown_jobs_are_gone_and_an_ambiguous_name_is_an_error_never_an_end() {
        // why: S2. Real captures: an unknown full id and an unknown name are
        //      `gone` (exit 127), but an AMBIGUOUS name also exits 127 and
        //      must not end a hold: it says the job exists. The ambiguity text
        //      lists job names after two spaces, so a job named "there is no
        //      job" must not read as qex's own sentence either.
        // proves: CLAIM(H2)
        assert_eq!(qex_status_reading("00000000-0000-0000-0000-000000000000", &ran(127, "", STATUS_UNKNOWN_ID)), "gone");
        assert_eq!(qex_status_reading("nosuchname-zzz", &ran(127, "", STATUS_UNKNOWN_NAME)), "gone");
        assert!(STATUS_AMBIGUOUS.contains("names 2 jobs"), "fixture is the real message");
        let reading = qex_status_reading("vigil-fixture", &ran(127, "", STATUS_AMBIGUOUS));
        assert!(reading.starts_with("error: ") && reading.contains("names 2 jobs"), "{reading}");
        let tricky = "qex: `x` names 2 jobs. Give the id of the job that you want.\n  07193384-6827-4b1d-ba7f-1abc7fc9c0f8 there is no job\n";
        assert!(qex_status_reading("x", &ran(127, "", tricky)).starts_with("error: "));
    }

    #[test]
    fn a_real_status_record_reads_as_its_state() {
        // why: H2 with the whole real record (all 50 keys, pretty-printed), not
        //      a one-key object.
        // proves: CLAIM(H2)
        assert_eq!(qex_status_reading(COMPLETED_ID, &ran(0, STATUS_COMPLETED, "")), "completed");
    }

    /// `n` records shaped like a real `qex list --json` entry (the captured
    /// running job, re-keyed), pretty-printed as qex prints them.
    fn active_list(n: usize) -> String {
        let template: serde_json::Value = serde_json::from_str::<Vec<serde_json::Value>>(LIST_RUNNING).unwrap().remove(0);
        let jobs: Vec<_> = (0..n)
            .map(|index| {
                let mut job = template.clone();
                job["id"] = json!(format!("{index:08x}-6827-4b1d-ba7f-1abc7fc9c0f8"));
                job["sequence"] = json!(1000 + index);
                job
            })
            .collect();
        serde_json::to_string_pretty(&jobs).unwrap() + "\n"
    }

    #[test]
    fn idle_asks_qex_for_active_jobs_under_the_directory_only() {
        // why: S1. The query must be bounded by what is wanted: active jobs
        //      (`--state active` is queued, starting and running; the
        //      installed qex implements it as "not terminal"), scoped with
        //      `--under`. Without it qex returns every terminal job as well:
        //      2.5 MB for 991 jobs on this host, far over the 64 KiB capture.
        // proves: CLAIM(H10)
        let argv = idle_argv(std::path::Path::new("/work/repo"));
        assert_eq!(argv, ["qex", "list", "--under", "/work/repo", "--state", "active", "--json"]);
    }

    #[test]
    fn an_empty_and_a_real_running_list_read_as_idle_and_busy() {
        // why: H10 with real captures: `[]` and one real running record.
        // proves: CLAIM(H10)
        assert_eq!(qex_idle_reading(&ran(0, LIST_EMPTY, "")), "idle");
        assert_eq!(qex_idle_reading(&ran(0, LIST_RUNNING, "")), "busy: 1 active (54f37003)");
    }

    #[test]
    fn a_busy_list_longer_than_the_capture_still_reads_busy_not_as_an_error() {
        // why: S1. 40 real-shaped records are over 80 KB; the command reader
        //      keeps 64 KiB and appends its marker, so the JSON is cut in the
        //      middle of a record. The queue is plainly busy, and an "error"
        //      here would hold a finished session for seven days or release
        //      nothing. The reading names a lower bound on the count.
        // proves: CLAIM(H10)
        let list = active_list(40);
        assert!(list.len() > crate::inputs::MAX_CAPTURE_BYTES, "the list must exceed the capture: {} bytes", list.len());
        let kept = &list[..crate::inputs::MAX_CAPTURE_BYTES];
        let text = format!("exit=0\n{kept}\n{}", crate::inputs::TRUNCATED_MARKER);
        let reading = qex_idle_reading(&text);
        assert!(reading.starts_with("busy: ") && !reading.contains("error"), "{reading}");
        assert!(reading.contains("00000000"), "{reading}");
        // A truncated capture that holds no record at all is unreadable, not idle.
        assert!(qex_idle_reading("exit=0\n[\n  {\n    \"na").starts_with("error: "));
    }

    #[test]
    fn a_missing_directory_and_unknown_states_do_not_release_an_idle_hold() {
        // why: S1 and the hardening note. A directory that no longer exists
        //      makes `qex list --under` exit 1 (real capture): an error, not
        //      idle. Whatever qex returns for an active query counts as active,
        //      except a record that says it is terminal.
        // proves: CLAIM(H10)
        let reading = qex_idle_reading(&ran(1, "", LIST_UNDER_MISSING));
        assert!(reading.starts_with("error: ") && reading.contains("does not exist"), "{reading}");
        let future = format!("exit=0\n{}", json!([{"id": "abcdef01-0000", "state": "paused"}, {"id": "abcdef02-0000"}]));
        assert_eq!(qex_idle_reading(&future), "busy: 2 active (abcdef01, abcdef02)");
        let mixed = format!("exit=0\n{}", json!([{"id": "abcdef01-0000", "state": "completed"}]));
        assert_eq!(qex_idle_reading(&mixed), "idle");
    }

    #[test]
    fn a_name_or_prefix_resolves_to_the_full_job_id_and_everything_else_is_refused_by_cause() {
        // why: S3. The hold stores the full id qex gives back. Real captures:
        //      a unique name or prefix answers with the record (its "id");
        //      an unknown name or id is not found; an ambiguous name is refused
        //      naming the cause; a removed record of a FULL id is accepted (the
        //      id is a real job that ended, so the first tick releases); a qex
        //      that cannot answer refuses a name (no identity to store) but not
        //      a full id (it already is one).
        // proves: CLAIM(H3)
        let found = ran(0, STATUS_COMPLETED, "");
        for given in ["vigil-fixture", "0d31c8ac", COMPLETED_ID] {
            assert_eq!(qex_resolution(given, &found).unwrap(), COMPLETED_ID, "{given}");
        }
        assert_eq!(
            qex_resolution("nosuchname-zzz", &ran(127, "", STATUS_UNKNOWN_NAME)).unwrap_err(),
            CreateError::QexJobNotFound("nosuchname-zzz".into())
        );
        assert_eq!(
            qex_resolution("00000000-0000-0000-0000-000000000000", &ran(127, "", STATUS_UNKNOWN_ID)).unwrap_err(),
            CreateError::QexJobNotFound("00000000-0000-0000-0000-000000000000".into())
        );
        let ambiguous = qex_resolution("vigil-fixture", &ran(127, "", STATUS_AMBIGUOUS)).unwrap_err();
        assert!(matches!(&ambiguous, CreateError::QexJobAmbiguous { job, detail } if job == "vigil-fixture" && detail.contains("names 2 jobs")), "{ambiguous:?}");
        assert_eq!(qex_resolution(COMPLETED_ID, &ran(127, "", STATUS_REMOVED)).unwrap(), COMPLETED_ID);
        for down in ["timeout after 10s", "cannot run qex: No such file or directory (os error 2)", "exit=1\nqex: the queue is locked\n", "exit=0\nnot json", "exit=0\n{\"state\": \"running\"}\n"] {
            let refused = qex_resolution("vigil-fixture", down).unwrap_err();
            assert!(matches!(refused, CreateError::QexJobUnresolved { .. }), "{down}: {refused:?}");
            assert_eq!(qex_resolution(COMPLETED_ID, down).unwrap(), COMPLETED_ID, "a full id needs no lookup: {down}");
        }
    }

    #[test]
    fn ended_is_decided_by_the_reading_text_of_each_input_kind() {
        // why: H2 table, "ended when": the single rule the scripted reader and
        //      the production reader share.
        // proves: CLAIM(H2)
        let qex = Input::Qex { job: "x".into() };
        for (text, want) in [("queued", false), ("starting", false), ("running", false), ("completed", true), ("failed", true), ("killed", true), ("timeout", true), ("expired", true), ("oom", true), ("cancelled", true), ("skipped", true), ("gone", true), ("error: boom", false), ("weird-new-state", false), ("", false)] {
            assert_eq!(ended(&qex, text), want, "qex {text:?}");
        }
        let idle = Input::QexIdle { dir: "/w".into() };
        assert_eq!((ended(&idle, "idle"), ended(&idle, "busy: 1 active (a)"), ended(&idle, "error: x")), (true, false, false));
        let exists = Input::PathExists { path: "/p".into() };
        assert_eq!((ended(&exists, "present"), ended(&exists, "missing")), (true, false));
        let until = Input::Until { at: at("2026-11-12T20:00:00Z") };
        assert_eq!((ended(&until, "reached"), ended(&until, "waiting")), (true, false));
        let process = Input::Process { pid: 1, want: Some(ProcessWant::Exited) };
        assert_eq!((ended(&process, "exited"), ended(&process, "running")), (true, false));
        // Inputs that are not work never "end".
        assert!(!ended(&Input::Url { url: "http://x".into() }, "reached"));
    }
}
