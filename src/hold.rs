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

impl Work {
    /// Parse `qex:<job>`, `pid:<pid>`, `file:<path>`, `until:<RFC3339>` or
    /// `idle:<dir>` (H2).
    pub fn parse(spec: &str) -> Result<Work, CreateError> {
        Err(CreateError::InvalidWork { spec: spec.to_string(), reason: "not implemented".to_string() })
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

/// Has the work behind `input` ended, given what its last reading said (H2)?
pub fn ended(input: &Input, text: &str) -> bool {
    false
}

/// The reading of a `qex status ID --json --no-logs` run (`cmd_text` is the
/// command reader's text, `exit=<code>\n<output>`, or its error text): the
/// job's state name, `gone` when qex does not know the job, else `error: ...`.
pub fn qex_status_reading(cmd_text: &str) -> String {
    "error: not implemented".to_string()
}

/// The reading of a `qex list --under DIR --all --json` run: `idle`, or
/// `busy: N active (ID, ...)`, or `error: ...`.
pub fn qex_idle_reading(cmd_text: &str) -> String {
    "error: not implemented".to_string()
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
}

impl Vigil {
    /// Create a hold (H1-H5, H10).
    pub async fn hold(&self, request: HoldRequest) -> Result<Created, VigilError> {
        Err(CreateError::HoldRejects("hold (not implemented)").into())
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
            assert_eq!(qex_status_reading(&text), state);
        }
        assert_eq!(qex_status_reading("exit=0\nqex: a hint on stderr\n{\"state\": \"running\"}\n"), "running");
    }

    #[test]
    fn qex_not_knowing_the_job_is_gone_and_every_other_failure_is_an_error_not_an_end() {
        // why: H2 (decision): qex prunes old jobs, so its "there is no job"
        //      answer (exit 127, captured) ends a hold; but a qex that is not
        //      installed, times out, answers garbage or exits for any other
        //      reason must NEVER end it: the cost of an early release is the
        //      lost cache.
        // proves: CLAIM(H2), CLAIM(H3)
        assert_eq!(qex_status_reading("exit=127\nqex: there is no job or pipeline with the id or the name `nosuchid`\n"), "gone");
        for text in [
            "cannot run qex: No such file or directory (os error 2)",
            "timeout after 10s",
            "exit=1\nqex: the queue is locked\n",
            "exit=127\nsh: qex: not found\n",
            "exit=0\nnot json at all",
            "exit=0\n{\"no_state\": 1}",
            "exit=0\n",
        ] {
            let reading = qex_status_reading(text);
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
