//! CLI presentation and cross-process session selection.
mod common;
use common::*;
#[test]
fn brief_session_wait_reports_completion_and_advice_as_plain_text() {
    // why: the main session needs readable returns without parsing JSON.
    // catches: flags absent from CLI or formatter omitting next/advice.
    let sandbox = Sandbox::new();
    let make = sandbox.command().env("CODEX_SESSION_ID", "quiet-test").args([
        "hold", "--id", "owned", "--for", "until:2020-01-01T00:00:00Z", "--orchestrator", "false"
    ]).output().unwrap();
    assert!(make.status.success(), "{}", String::from_utf8_lossy(&make.stderr));
    let out = sandbox.command().env("CODEX_SESSION_ID", "quiet-test").args(["keepalive", "--session", "--brief"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("act: "), "{text}");
    assert!(text.contains("item ended:") && text.contains("advice: off") && text.contains("next: "), "{text}");
    assert!(text.lines().count() <= 2, "{text}");
}
