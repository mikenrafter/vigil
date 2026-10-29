//! Input readers: file, glob, cmd, url, port, process.
//!
//! Contract shared by all readers: the first read of an input (no previous
//! hash) is a baseline and never counts as `changed`. A read error is a
//! reading with `is_error = true` whose `text` carries the error; the error
//! text is hashed, so a repeated identical error is not a new change.

use crate::watch::{Input, InputState};

#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// `Input::label()`.
    pub label: String,
    /// What the agent/judge should see: for files, only the bytes added since
    /// the stored offset (cut to the last `tail_bytes`); for the rest, the
    /// current observation.
    pub text: String,
    pub changed: bool,
    pub is_error: bool,
    /// `Some(met)` for port/process inputs that carry a `want`.
    pub want_met: Option<bool>,
    /// State to persist for the next read.
    pub state: InputState,
}

pub const DEFAULT_TAIL_BYTES: u64 = 4096;
pub const DEFAULT_CMD_TIMEOUT_S: f64 = 10.0;

/// Read one input. cmd text is `exit=<code>\n<stdout><stderr>`; on timeout the
/// child is killed and the text is `timeout after <n>s`. url text is
/// `HTTP <status>\n<body>`. port text is `open` or `closed`. process text is
/// `running` or `exited`. glob text lists newly created matches, one per line.
pub async fn read_input(input: &Input, previous: &InputState) -> Reading {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{append_file, write_file, Behavior, FakeHttpServer};
    use crate::watch::{GlobReport, PortWant, ProcessWant};
    use std::net::TcpListener;
    use std::path::Path;
    use std::time::Duration;

    fn file_input(path: &Path, tail_bytes: Option<u64>) -> Input {
        Input::File { path: path.to_path_buf(), tail_bytes }
    }

    // ---- file ----

    #[tokio::test]
    async fn file_first_read_is_a_baseline_not_a_change() {
        // why: contract in the module doc: otherwise every fire=change watch
        //      on an existing file would fire on its first tick.
        // proves: CLAIM(baseline-first-read)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.log");
        write_file(&path, "old\n");
        let reading = read_input(&file_input(&path, None), &InputState::default()).await;
        assert!(!reading.changed);
        assert!(!reading.is_error);
        assert_eq!(reading.state.offset, Some(4), "offset = bytes already seen (\"old\\n\")");
        assert!(reading.state.hash.is_some());
        assert_eq!(reading.label, format!("file:{}", path.display()));
    }

    #[tokio::test]
    async fn file_reports_only_bytes_added_since_the_last_return() {
        // why: SPEC "Envelope": digest is "already trimmed to what is new
        //      since the last return". 4 bytes baseline + 10 appended = offset 14;
        //      a later 2-byte append reports only itself.
        // proves: CLAIM(offset-new-since-last)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.log");
        write_file(&path, "old\n");
        let input = file_input(&path, None);
        let baseline = read_input(&input, &InputState::default()).await;

        append_file(&path, "new1\nnew2\n");
        let second = read_input(&input, &baseline.state).await;
        assert!(second.changed);
        assert_eq!(second.text, "new1\nnew2\n");
        assert_eq!(second.state.offset, Some(14));

        append_file(&path, "x\n");
        let third = read_input(&input, &second.state).await;
        assert!(third.changed);
        assert_eq!(third.text, "x\n", "earlier lines must not be repeated");
        assert_eq!(third.state.offset, Some(16));
    }

    #[tokio::test]
    async fn file_unchanged_read_reports_nothing() {
        // why: SPEC tick step 2: nothing changed means no judge call, so the
        //      reader must say `changed = false` for an identical file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.log");
        write_file(&path, "same\n");
        let input = file_input(&path, None);
        let baseline = read_input(&input, &InputState::default()).await;
        let again = read_input(&input, &baseline.state).await;
        assert!(!again.changed);
        assert_eq!(again.text, "");
        assert_eq!(again.state, baseline.state);
    }

    #[tokio::test]
    async fn file_tail_bytes_keeps_only_the_last_bytes_of_the_new_content() {
        // why: SPEC input `tail_bytes`: cap the reading to the file's tail.
        //      Appending "0123456789" with tail_bytes = 4 keeps "6789".
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.log");
        write_file(&path, "a\n");
        let input = file_input(&path, Some(4));
        let baseline = read_input(&input, &InputState::default()).await;
        append_file(&path, "0123456789");
        let second = read_input(&input, &baseline.state).await;
        assert!(second.changed);
        assert_eq!(second.text, "6789");
    }

    #[tokio::test]
    async fn file_that_shrinks_is_read_again_from_the_start() {
        // why: log rotation/truncation: an offset beyond the file length must
        //      not hide the fresh content (it would read zero bytes forever).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.log");
        write_file(&path, "a long line of old text\n");
        let input = file_input(&path, None);
        let baseline = read_input(&input, &InputState::default()).await;
        write_file(&path, "fresh\n");
        let after = read_input(&input, &baseline.state).await;
        assert!(after.changed);
        assert_eq!(after.text, "fresh\n");
        assert_eq!(after.state.offset, Some(6));
    }

    #[tokio::test]
    async fn missing_file_is_an_error_reading_that_counts_as_a_change_once() {
        // why: SPEC "Failure rules": "Input read error (missing file, ...):
        //      report it as a change with the error text; it is not a crash."
        //      The same error repeated is not a new change; recovery is.
        // proves: CLAIM(error-as-change)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.log");
        write_file(&path, "here\n");
        let input = file_input(&path, None);
        let baseline = read_input(&input, &InputState::default()).await;

        std::fs::remove_file(&path).unwrap();
        let gone = read_input(&input, &baseline.state).await;
        assert!(gone.changed);
        assert!(gone.is_error);
        assert!(gone.text.contains(&path.display().to_string()), "error text names the file: {:?}", gone.text);

        let still_gone = read_input(&input, &gone.state).await;
        assert!(!still_gone.changed, "an identical repeated error is not a new change");

        write_file(&path, "back\n");
        let back = read_input(&input, &still_gone.state).await;
        assert!(back.changed);
        assert!(!back.is_error);
        assert_eq!(back.text, "back\n");
    }

    #[tokio::test]
    async fn a_file_missing_at_baseline_is_an_error_baseline_not_a_change() {
        // why: first-read contract applies to errors too; the watch must not
        //      fire at tick 1 just because the file does not exist yet.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-yet.log");
        let reading = read_input(&file_input(&path, None), &InputState::default()).await;
        assert!(reading.is_error);
        assert!(!reading.changed);
    }

    // ---- glob ----

    #[tokio::test]
    async fn glob_reports_created_matches_and_ignores_removals() {
        // why: SPEC input `{"kind":"glob","report":"created"}`: only new
        //      matches are news. Removing a match is not a change (control
        //      for the "created" semantics).
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir.path().join("a.txt"), "");
        let input = Input::Glob { pattern: format!("{}/*.txt", dir.path().display()), report: GlobReport::Created };
        let baseline = read_input(&input, &InputState::default()).await;
        assert!(!baseline.changed);

        write_file(&dir.path().join("b.txt"), "");
        let created = read_input(&input, &baseline.state).await;
        assert!(created.changed);
        assert!(created.text.contains("b.txt"));
        assert!(!created.text.contains("a.txt"), "already-known matches are not reported: {:?}", created.text);

        std::fs::remove_file(dir.path().join("a.txt")).unwrap();
        let removed = read_input(&input, &created.state).await;
        assert!(!removed.changed, "removal is not a creation");
    }

    // ---- cmd ----

    fn sh(script: &str, timeout_s: Option<f64>) -> Input {
        Input::Cmd { argv: vec!["sh".into(), "-c".into(), script.into()], timeout_s }
    }

    #[tokio::test]
    async fn cmd_reports_exit_code_and_output() {
        // why: SPEC "cmd (argv, exit code + output)". Format fixed in the
        //      module doc: `exit=<code>\n<output>`. A non-zero exit is an
        //      observation, not a read error.
        let reading = read_input(&sh("echo hi; exit 3", None), &InputState::default()).await;
        assert_eq!(reading.text, "exit=3\nhi\n");
        assert!(!reading.is_error);
        assert!(reading.label.starts_with("cmd:"));
    }

    #[tokio::test]
    async fn cmd_detects_change_in_output_and_ignores_identical_output() {
        // why: SPEC tick step 1-2: results are hashed; same output twice is
        //      "unchanged" (no judge call), different output is a change.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.txt");
        write_file(&path, "active\n");
        let input = Input::Cmd { argv: vec!["cat".into(), path.display().to_string()], timeout_s: None };
        let baseline = read_input(&input, &InputState::default()).await;
        let same = read_input(&input, &baseline.state).await;
        assert!(!same.changed);
        write_file(&path, "failed\n");
        let different = read_input(&input, &same.state).await;
        assert!(different.changed);
        assert_eq!(different.text, "exit=0\nfailed\n");
    }

    #[tokio::test]
    async fn cmd_that_cannot_start_is_an_error_reading() {
        // why: SPEC "Failure rules": read errors are changes with the error
        //      text, never a crash.
        let input = Input::Cmd { argv: vec!["/nonexistent/vigil-binary".into()], timeout_s: None };
        let reading = read_input(&input, &InputState::default()).await;
        assert!(reading.is_error);
        assert!(reading.text.contains("/nonexistent/vigil-binary"), "{:?}", reading.text);
    }

    #[tokio::test]
    async fn cmd_is_killed_when_it_exceeds_timeout_s() {
        // why: SPEC input `timeout_s`: a hung command must not hold the tick.
        //      The child is `exec sleep 30` so its pid (written to a file) is
        //      the process that must be gone afterwards. Uses a 0.3 s real
        //      timeout: a process kill is not simulable with a fake clock.
        // proves: CLAIM(cmd-timeout-kill)
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let script = format!("echo $$ > {}; exec sleep 30", pid_file.display());
        let started = std::time::Instant::now();
        let reading = tokio::time::timeout(
            Duration::from_secs(10),
            read_input(&sh(&script, Some(0.3)), &InputState::default()),
        )
        .await
        .expect("read_input must return after the timeout");
        assert!(started.elapsed() < Duration::from_secs(5), "must not wait out the 30 s sleep");
        assert!(reading.is_error);
        assert!(reading.text.contains("timeout"), "{:?}", reading.text);
        let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        assert!(!Path::new(&format!("/proc/{pid}")).exists(), "child {pid} must be killed and reaped");
    }

    // ---- url ----

    #[tokio::test]
    async fn url_reports_status_and_body_and_detects_changes() {
        // why: SPEC input `url` (http GET). Format: `HTTP <status>\n<body>`.
        //      Same response = unchanged; a changed body or status = change.
        let server = FakeHttpServer::start(Behavior::Respond { status: 200, body: "ok".into() });
        let input = Input::Url { url: format!("{}/health", server.url()) };
        let baseline = read_input(&input, &InputState::default()).await;
        assert_eq!(baseline.text, "HTTP 200\nok");
        assert!(!baseline.changed);
        assert_eq!(server.requests()[0].method, "GET");
        assert_eq!(server.requests()[0].path, "/health");

        let same = read_input(&input, &baseline.state).await;
        assert!(!same.changed);

        server.set_behavior(Behavior::Respond { status: 503, body: "down".into() });
        let degraded = read_input(&input, &same.state).await;
        assert!(degraded.changed);
        assert_eq!(degraded.text, "HTTP 503\ndown");
        assert!(!degraded.is_error, "a 503 is an observation, not a read failure");
    }

    #[tokio::test]
    async fn url_connection_refused_is_an_error_as_change() {
        // why: SPEC "Failure rules": refused connections are changes with the
        //      error text. Baseline against a live server first, then stop it.
        // proves: CLAIM(error-as-change)
        let mut server = FakeHttpServer::start(Behavior::Respond { status: 200, body: "ok".into() });
        let input = Input::Url { url: format!("{}/", server.url()) };
        let baseline = read_input(&input, &InputState::default()).await;
        server.stop();
        let refused = read_input(&input, &baseline.state).await;
        assert!(refused.changed);
        assert!(refused.is_error);
        assert!(!refused.text.is_empty());
    }

    // ---- port ----

    #[tokio::test]
    async fn port_reports_open_and_closed_and_whether_the_want_is_met() {
        // why: SPEC input `port` with `want: open`. Listening -> "open" and
        //      want met; after the listener closes -> "closed", a change, want unmet.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let input = Input::Port { host: "127.0.0.1".into(), port, want: Some(PortWant::Open) };
        let baseline = read_input(&input, &InputState::default()).await;
        assert_eq!(baseline.text, "open");
        assert_eq!(baseline.want_met, Some(true));
        assert!(!baseline.changed);

        drop(listener);
        let closed = read_input(&input, &baseline.state).await;
        assert_eq!(closed.text, "closed");
        assert_eq!(closed.want_met, Some(false));
        assert!(closed.changed);
    }

    #[tokio::test]
    async fn port_without_a_want_reports_no_want_status_and_want_closed_inverts() {
        // why: `want_met` is None unless a want exists (condition rule only
        //      counts inputs with a want); want=closed is met by a closed port.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let no_want = Input::Port { host: "127.0.0.1".into(), port, want: None };
        assert_eq!(read_input(&no_want, &InputState::default()).await.want_met, None);
        drop(listener);
        let want_closed = Input::Port { host: "127.0.0.1".into(), port, want: Some(PortWant::Closed) };
        assert_eq!(read_input(&want_closed, &InputState::default()).await.want_met, Some(true));
    }

    // ---- process ----

    #[tokio::test]
    async fn process_reports_running_then_exited() {
        // why: SPEC input `process` with `want: exited`. Uses a real spawned
        //      child; it is reaped (wait) so /proc no longer lists it.
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let input = Input::Process { pid, want: Some(ProcessWant::Exited) };
        let baseline = read_input(&input, &InputState::default()).await;
        assert_eq!(baseline.text, "running");
        assert_eq!(baseline.want_met, Some(false), "want exited, still running");
        assert!(!baseline.changed);

        child.kill().unwrap();
        child.wait().unwrap();
        let exited = read_input(&input, &baseline.state).await;
        assert_eq!(exited.text, "exited");
        assert_eq!(exited.want_met, Some(true));
        assert!(exited.changed);
    }
}
