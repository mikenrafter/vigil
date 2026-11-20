//! CLI lifecycle tests: the process boundary also checks ledger writes drain at shutdown.
use serde_json::{Value, json};
use std::process::Command;

fn run(root: &std::path::Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_vigil"))
        .args(args).env("XDG_STATE_HOME", root).env("VIGIL_ORCHESTRATOR_CONFIRM", "false")
        .output().unwrap();
    assert!(output.status.success(), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}
fn rows(root: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("vigil/items.jsonl")).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect()
}

#[test]
fn item_ledger_done_cancel_cold_are_once_and_keep_metadata() {
    // why: explicit and whole-hold ends must retain estimates after restart, without duplicate rows.
    // catches: logging only tick events, losing metadata, or logging done again on cancel.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    run(root, &["hold", "--id", "h", "--for", "none", "--label", "pilot", "--expect-s", "60", "--orchestrator", "false", "--meta", "model=opus", "--meta", "brief=/work/brief.md"]);
    run(root, &["hold", "add", "h", "--for", "none", "--label", "other", "--meta", "items=3"]);
    run(root, &["hold", "done", "h", "pilot"]);
    run(root, &["cancel", "h"]);
    run(root, &["cancel", "h"]);
    let entries = rows(root);
    assert_eq!(entries.len(), 2);
    let done = &entries[0];
    assert_eq!(done["v"], 1);
    assert_eq!(done["hold"], "h");
    assert_eq!(done["label"], "pilot");
    assert_eq!(done["spec_kind"], "none");
    assert_eq!(done["expect_s"], 60);
    assert_eq!(done["outcome"], "done");
    assert_eq!(done["overdue_noted"], false);
    assert_eq!(done["meta"], json!({"model":"opus", "brief":"/work/brief.md"}));
    for field in ["started_at", "ended_at"] {
        let stamp = chrono::DateTime::parse_from_rfc3339(done[field].as_str().unwrap()).unwrap();
        assert_eq!(stamp.offset().local_minus_utc(), 0);
    }
    assert!(std::path::Path::new(done["cwd"].as_str().unwrap()).is_absolute());
    assert_eq!(entries[1]["outcome"], "cancelled");
    assert_eq!(entries[1]["expect_s"], Value::Null);
    assert_eq!(entries[1]["meta"], json!({"items":"3"}));
    run(root, &["hold", "--id", "c", "--for", "none", "--label", "cold", "--orchestrator", "false"]);
    run(root, &["cold", "c"]);
    assert_eq!(rows(root)[2]["outcome"], "cold");
}

#[test]
fn item_ledger_sensed_ends_and_ledger_failure_do_not_break_completion() {
    // why: automatic completion and an unwritable ledger must both leave keepalive usable.
    // catches: missing pid/file hooks, repeated entries on terminal reads, propagating append errors.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let marker = root.join("report");
    std::fs::write(&marker, "private report text").unwrap();
    let file = format!("file:{}", marker.display());
    for (id, spec, kind, outcome) in [("p", "pid:99999999", "pid", "exited"), ("f", file.as_str(), "file", "report")] {
        run(root, &["hold", "--id", id, "--for", spec, "--label", id, "--orchestrator", "false"]);
        run(root, &["keepalive", id, "--max-s", "1"]);
        run(root, &["keepalive", id, "--max-s", "1"]);
        let entries = rows(root);
        let row = entries.iter().find(|row| row["hold"] == id).unwrap();
        assert_eq!(row["spec_kind"], kind);
        assert_eq!(row["outcome"], outcome);
    }
    assert_eq!(rows(root).len(), 2);
    assert!(!std::fs::read_to_string(root.join("vigil/items.jsonl")).unwrap().contains("private report text"));
    std::fs::remove_file(root.join("vigil/items.jsonl")).unwrap();
    std::fs::create_dir(root.join("vigil/items.jsonl")).unwrap();
    run(root, &["hold", "--id", "bad", "--for", "none", "--label", "bad", "--orchestrator", "false"]);
    run(root, &["hold", "done", "bad", "bad"]);
    run(root, &["cancel", "bad"]);
}
