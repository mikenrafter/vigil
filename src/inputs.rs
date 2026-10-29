//! Input readers: file, glob, cmd, url, port, process.
//!
//! Contract shared by all readers: the first read of an input (default
//! `InputState`) is a baseline and never counts as `changed`; `Vigil::create`
//! performs it (V1). A read error is a reading with `is_error = true` whose
//! `text` carries the error; the error text is hashed, so a repeated identical
//! error is not a new change.
//!
//! File inputs are tracked by position, not hash (V9). Every input reads at
//! most `MAX_CAPTURE_BYTES` per call (V12). A `cmd` runs in its own process
//! group and the whole group is SIGKILLed and reaped on timeout and when the
//! read future is dropped (V13).

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
/// V12: per-input, per-read capture limit (cmd stdout+stderr combined, url
/// body, file new bytes). The excess is skipped and the reading says
/// `TRUNCATED_MARKER`.
pub const MAX_CAPTURE_BYTES: usize = 64 * 1024;
/// V12: a glob reports at most this many matches.
pub const MAX_GLOB_MATCHES: usize = 1000;
pub const TRUNCATED_MARKER: &str = "[truncated]";
pub const DEFAULT_CMD_TIMEOUT_S: f64 = 10.0;

/// The two fields of `/proc/<pid>/stat` the process input needs (V17).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcStat {
    /// Field 3: `R`, `S`, `D`, `Z` (zombie), ...
    pub state: char,
    /// Field 22: start time in clock ticks since boot; with the pid it
    /// identifies one process even after pid reuse.
    pub start_time: u64,
}

/// Parse one `/proc/<pid>/stat` line (proc(5)). Field 2, `(comm)`, may contain
/// spaces and parentheses, so fields are counted after the LAST `)`.
/// `None` for anything that is not a complete stat line.
pub fn parse_proc_stat(line: &str) -> Option<ProcStat> {
    todo!()
}

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

    /// Independent of `parse_proc_stat`: the state letter after the last ')'.
    fn proc_state(pid: u32) -> Option<char> {
        let line = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        line[line.rfind(')')? + 1..].trim_start().chars().next()
    }

    /// Dead = gone, or a zombie waiting for someone else (init) to reap it.
    fn is_dead(pid: u32) -> bool {
        matches!(proc_state(pid), None | Some('Z'))
    }

    fn read_pid(path: &Path) -> u32 {
        std::fs::read_to_string(path).unwrap().trim().parse().unwrap()
    }

    async fn all_dead_within(pids: &[u32], limit: Duration) -> bool {
        let started = std::time::Instant::now();
        while started.elapsed() < limit {
            if pids.iter().all(|p| is_dead(*p)) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    // ---- file ----

    #[tokio::test]
    async fn file_first_read_is_a_baseline_not_a_change() {
        // why: contract in the module doc: otherwise every fire=change watch
        //      on an existing file would fire on its first tick. V9: the
        //      position record (offset + dev/inode/size/mtime) is stored.
        // proves: CLAIM(baseline-first-read)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.log");
        write_file(&path, "old\n");
        let reading = read_input(&file_input(&path, None), &InputState::default()).await;
        assert!(!reading.changed);
        assert!(!reading.is_error);
        assert_eq!(reading.state.offset, Some(4), "offset = bytes already seen (\"old\\n\")");
        let stamp = reading.state.file.expect("V9: file identity recorded");
        assert_eq!(stamp.size, 4);
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
    async fn two_identical_appends_are_two_changes_even_with_quiet_reads_between() {
        // why: V9 (S9): file changes are by position, not by hash of the
        //      reading. Appending the same "tick\n" twice produces identical
        //      text, yet each is a change; a read with no append between is
        //      quiet (positive control that `changed` is not just "always true").
        // proves: CLAIM(positional-file-change)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.log");
        write_file(&path, "old\n");
        let input = file_input(&path, None);
        let baseline = read_input(&input, &InputState::default()).await;

        append_file(&path, "tick\n");
        let first = read_input(&input, &baseline.state).await;
        assert!(first.changed);
        assert_eq!(first.text, "tick\n");

        let quiet = read_input(&input, &first.state).await;
        assert!(!quiet.changed, "control: nothing appended, nothing changed");

        append_file(&path, "tick\n");
        let second = read_input(&input, &quiet.state).await;
        assert!(second.changed, "an identical second append is a second change");
        assert_eq!(second.text, "tick\n");
        assert_eq!(second.state.offset, Some(14));
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
        // why: log truncation: an offset beyond the file length must not hide
        //      the fresh content (V9: a smaller size means rewritten, offset
        //      resets to 0). Rewrites in place (same inode).
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
    async fn a_replacement_file_is_read_from_the_start_even_when_larger_or_the_same_size() {
        // why: V9 (S10): a different (dev, inode) means the file was replaced
        //      (log rotation: new file renamed over the path). The stored
        //      offset would skip fresh content, so offset resets to 0 and the
        //      last tail_bytes are new. Old content "old-line\n" = 9 bytes.
        //      Case 1: a LARGER replacement (the size-only check of the old
        //      design misses this: 9 < 40 looks like a plain append).
        //      Case 2: a SAME-SIZE replacement. The replacement is created
        //      while the old file still exists, so the inode is guaranteed to
        //      differ (no inode reuse).
        // proves: CLAIM(positional-file-change)
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.log");
        write_file(&path, "old-line\n");
        let input = file_input(&path, None);
        let baseline = read_input(&input, &InputState::default()).await;

        let replacement = dir.path().join("a.log.new");
        let larger = "rotated: first\nrotated: second\nrotated: third\n";
        write_file(&replacement, larger);
        let old_inode = std::fs::metadata(&path).unwrap().ino();
        std::fs::rename(&replacement, &path).unwrap();
        assert_ne!(std::fs::metadata(&path).unwrap().ino(), old_inode, "precondition: new inode");
        let after_larger = read_input(&input, &baseline.state).await;
        assert!(after_larger.changed);
        assert_eq!(after_larger.text, larger, "whole replacement is new, not just the part past offset 9");
        assert_eq!(after_larger.state.offset, Some(larger.len() as u64));

        let same_size = "X".repeat(larger.len() - 1) + "\n";
        write_file(&replacement, &same_size);
        std::fs::rename(&replacement, &path).unwrap();
        let after_same = read_input(&input, &after_larger.state).await;
        assert!(after_same.changed, "same-size replacement is still a replacement");
        assert_eq!(after_same.text, same_size);
    }

    #[tokio::test]
    async fn a_same_size_rewrite_in_place_with_a_new_mtime_is_a_change_but_an_untouched_mtime_is_not() {
        // why: V9 (S10): "the same size with a new mtime means the file was
        //      rewritten": reset offset to 0 and treat the last tail_bytes as
        //      new. mtime is set explicitly (std File::set_modified) so the
        //      test does not depend on timestamp granularity. Control (spec
        //      boundary): same size AND same mtime is indistinguishable and
        //      reads as unchanged.
        // proves: CLAIM(positional-file-change)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.log");
        write_file(&path, "aaaa\n");
        let original_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        let input = file_input(&path, None);
        let baseline = read_input(&input, &InputState::default()).await;
        let set_mtime = |t: std::time::SystemTime| {
            std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_modified(t).unwrap();
        };

        std::fs::write(&path, "cccc\n").unwrap();
        set_mtime(original_mtime);
        let hidden = read_input(&input, &baseline.state).await;
        assert!(!hidden.changed, "same size + same mtime: not detectable, not a change");

        std::fs::write(&path, "bbbb\n").unwrap();
        set_mtime(original_mtime + Duration::from_secs(10));
        let rewritten = read_input(&input, &baseline.state).await;
        assert!(rewritten.changed);
        assert_eq!(rewritten.text, "bbbb\n");
        assert_eq!(rewritten.state.offset, Some(5));
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

    #[tokio::test]
    async fn a_huge_append_is_capped_at_64_kib_says_truncated_and_still_advances_the_offset() {
        // why: V12 (S12): a file input reads at most 64 KiB of new bytes per
        //      tick; the excess is skipped and the reading says [truncated].
        //      140000 new bytes (20000 lines of "L00000\n" = 7 bytes) with
        //      tail_bytes 1 MB: the reading is <= 65536 bytes + the marker and
        //      one separator, keeps the NEWEST lines (default taken, flagged in
        //      .red-report.md: the excess skipped is the oldest), and the
        //      offset moves to the end so nothing is re-read. Control: a
        //      small append in the same test has no marker.
        // proves: CLAIM(limits)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.log");
        write_file(&path, "old\n");
        let input = file_input(&path, Some(1_000_000));
        let baseline = read_input(&input, &InputState::default()).await;
        let big: String = (0..20_000).map(|i| format!("L{i:05}\n")).collect();
        append_file(&path, &big);
        let capped = read_input(&input, &baseline.state).await;
        assert!(capped.changed);
        assert!(capped.text.contains(TRUNCATED_MARKER), "must say so");
        assert!(capped.text.len() <= MAX_CAPTURE_BYTES + TRUNCATED_MARKER.len() + 1, "{} bytes", capped.text.len());
        assert!(capped.text.contains("L19999") && !capped.text.contains("L00000"));
        assert_eq!(capped.state.offset, Some(4 + 140_000));

        append_file(&path, "small\n");
        let small = read_input(&input, &capped.state).await;
        assert_eq!(small.text, "small\n");
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

    #[tokio::test]
    async fn a_glob_reports_at_most_1000_matches_and_says_truncated() {
        // why: V12 (S12): "a glob reports at most 1000 matches". 1100 files
        //      appear after the baseline: exactly 1000 match lines are in the
        //      reading (not 1100, not fewer) and the reading says [truncated]
        //      (default taken: the marker rule of the byte caps applies here
        //      too, flagged in .red-report.md). Control: 3 new files report 3
        //      lines and no marker.
        // proves: CLAIM(limits)
        let dir = tempfile::tempdir().unwrap();
        let input = Input::Glob { pattern: format!("{}/*.txt", dir.path().display()), report: GlobReport::Created };
        let baseline = read_input(&input, &InputState::default()).await;
        for i in 0..1100 {
            write_file(&dir.path().join(format!("f-{i:04}.txt")), "");
        }
        let reading = read_input(&input, &baseline.state).await;
        assert!(reading.changed);
        assert_eq!(reading.text.lines().filter(|l| l.contains("f-")).count(), 1000);
        assert!(reading.text.contains(TRUNCATED_MARKER));

        let dir2 = tempfile::tempdir().unwrap();
        let input2 = Input::Glob { pattern: format!("{}/*.txt", dir2.path().display()), report: GlobReport::Created };
        let base2 = read_input(&input2, &InputState::default()).await;
        for i in 0..3 {
            write_file(&dir2.path().join(format!("f-{i}.txt")), "");
        }
        let few = read_input(&input2, &base2.state).await;
        assert_eq!(few.text.lines().filter(|l| l.contains("f-")).count(), 3);
        assert!(!few.text.contains(TRUNCATED_MARKER));
    }

    // ---- cmd ----

    fn sh(script: &str, timeout_s: Option<f64>) -> Input {
        Input::Cmd { argv: vec!["sh".into(), "-c".into(), script.into()], timeout_s, cwd: None }
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
    async fn cmd_runs_in_the_stored_working_directory() {
        // why: V11 (S11): the cwd stored at watch_create is where the command
        //      runs, wherever the resuming process happens to be. The test
        //      process runs in the crate directory, so `pwd` printing the
        //      tempdir proves the child chdir'd. Control: with no cwd the
        //      child inherits the test process cwd instead.
        // proves: CLAIM(creator-cwd)
        let dir = tempfile::tempdir().unwrap().keep().canonicalize().unwrap();
        let stored = Input::Cmd { argv: vec!["pwd".into()], timeout_s: None, cwd: Some(dir.clone()) };
        let reading = read_input(&stored, &InputState::default()).await;
        assert_eq!(reading.text, format!("exit=0\n{}\n", dir.display()));
        let inherited = read_input(&Input::Cmd { argv: vec!["pwd".into()], timeout_s: None, cwd: None }, &InputState::default()).await;
        assert_ne!(inherited.text, reading.text);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn cmd_detects_change_in_output_and_ignores_identical_output() {
        // why: SPEC tick step 1-2: results are hashed; same output twice is
        //      "unchanged" (no judge call), different output is a change.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.txt");
        write_file(&path, "active\n");
        let input = Input::Cmd { argv: vec!["cat".into(), path.display().to_string()], timeout_s: None, cwd: None };
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
        let input = Input::Cmd { argv: vec!["/nonexistent/vigil-binary".into()], timeout_s: None, cwd: None };
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
        let pid = read_pid(&pid_file);
        assert!(!Path::new(&format!("/proc/{pid}")).exists(), "child {pid} must be killed and reaped");
    }

    fn tree_script(dir: &Path) -> String {
        // A shell that does NOT exec: it stays alive as the parent of two
        // background sleeps. S = shell, A and B = descendants.
        format!(
            "sleep 30 & echo $! > {a}; sleep 30 & echo $! > {b}; echo $$ > {s}; wait",
            a = dir.join("a").display(),
            b = dir.join("b").display(),
            s = dir.join("s").display()
        )
    }

    #[tokio::test]
    async fn a_timeout_kills_the_whole_process_group_not_just_the_direct_child() {
        // why: V13 (S13): the old test used `exec`, proving cleanup only for
        //      one direct child. `sh -c 'sleep 30 & sleep 30 & wait'` leaves
        //      shell descendants that survive a plain kill and keep the output
        //      pipe open. After the timeout the shell S and BOTH sleeps A, B
        //      must be dead (S reaped by vigil: gone from /proc; A and B are
        //      orphans reaped by init, so a zombie counts as dead) and the
        //      read must return promptly (a surviving pipe holder would hang it).
        // proves: CLAIM(cmd-group-kill)
        let dir = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();
        let reading = tokio::time::timeout(
            Duration::from_secs(10),
            read_input(&sh(&tree_script(dir.path()), Some(1.0)), &InputState::default()),
        )
        .await
        .expect("read_input must not hang on descendants holding the pipe");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(reading.is_error && reading.text.contains("timeout"), "{:?}", reading.text);
        let (s, a, b) = (read_pid(&dir.path().join("s")), read_pid(&dir.path().join("a")), read_pid(&dir.path().join("b")));
        assert!(!Path::new(&format!("/proc/{s}")).exists(), "shell {s} must be killed and reaped");
        assert!(all_dead_within(&[a, b], Duration::from_secs(3)).await, "descendants {a} and {b} must be killed too");
    }

    #[tokio::test]
    async fn dropping_the_read_kills_the_whole_process_group() {
        // why: V13 (S13): "when the call is cancelled or dropped, the whole
        //      group gets SIGKILL and is reaped" (a keepalive call that hits
        //      its budget or whose client disconnects drops the future). The
        //      read is dropped (select! picks the other branch) once the
        //      shell has written all three pid files; timeout_s is 30 so
        //      only the drop can be what kills them.
        // proves: CLAIM(cmd-group-kill)
        let dir = tempfile::tempdir().unwrap();
        let script = tree_script(dir.path());
        let s_file = dir.path().join("s");
        let (input, previous) = (sh(&script, Some(30.0)), InputState::default());
        let outcome = tokio::select! {
            _ = read_input(&input, &previous) => None,
            _ = async {
                loop {
                    if std::fs::read_to_string(&s_file).is_ok_and(|t| !t.trim().is_empty()) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            } => Some(()),
        };
        assert!(outcome.is_some(), "the read must still be running when we drop it");
        let (s, a, b) = (read_pid(&s_file), read_pid(&dir.path().join("a")), read_pid(&dir.path().join("b")));
        assert!(all_dead_within(&[s, a, b], Duration::from_secs(3)).await, "{s}/{a}/{b} must die with the dropped read");
    }

    #[tokio::test]
    async fn cmd_output_is_capped_at_64_kib_combined_and_says_truncated() {
        // why: V12 (S12): cmd stdout+stderr combined is read up to 64 KiB;
        //      the excess is skipped and the reading says [truncated]. The
        //      script writes 40000 x to stdout and 40000 y to stderr: 80000
        //      bytes, of which exactly 65536 (64 KiB) survive in total.
        //      Control: a small output has no marker.
        // proves: CLAIM(limits)
        let script = "head -c 40000 /dev/zero | tr '\\0' x; head -c 40000 /dev/zero | tr '\\0' y >&2";
        let reading = read_input(&sh(script, None), &InputState::default()).await;
        assert!(reading.text.starts_with("exit=0\n"), "{:?}", &reading.text[..20]);
        let kept = reading.text.chars().filter(|c| *c == 'x' || *c == 'y').count();
        assert_eq!(kept, MAX_CAPTURE_BYTES);
        assert!(reading.text.contains(TRUNCATED_MARKER));
        let small = read_input(&sh("echo ok", None), &InputState::default()).await;
        assert!(!small.text.contains(TRUNCATED_MARKER));
    }

    #[tokio::test]
    async fn endless_output_cannot_hang_or_exhaust_the_reader() {
        // why: V12 + V13 (S12 "endless outputs"): `yes` never stops writing.
        //      With timeout_s 0.5 the read must return within 5 s as a
        //      timeout error with a bounded text, and the process must be gone.
        // proves: CLAIM(limits)
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let script = format!("echo $$ > {}; exec yes", pid_file.display());
        let reading = tokio::time::timeout(Duration::from_secs(10), read_input(&sh(&script, Some(0.5)), &InputState::default()))
            .await
            .expect("endless output must not hang the reader");
        assert!(reading.is_error && reading.text.contains("timeout"));
        assert!(reading.text.len() <= MAX_CAPTURE_BYTES + 256, "{} bytes", reading.text.len());
        assert!(all_dead_within(&[read_pid(&pid_file)], Duration::from_secs(3)).await);
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
    async fn url_body_is_capped_at_64_kib_and_says_truncated() {
        // why: V12 (S12): a url body is read up to 64 KiB. A 200000-byte body
        //      yields "HTTP 200\n" + exactly 65536 body bytes + the marker.
        //      Control: the small body of the earlier test has no marker.
        // proves: CLAIM(limits)
        let server = FakeHttpServer::start(Behavior::Respond { status: 200, body: "x".repeat(200_000) });
        let input = Input::Url { url: format!("{}/", server.url()) };
        let reading = read_input(&input, &InputState::default()).await;
        assert!(reading.text.starts_with("HTTP 200\n"));
        assert_eq!(reading.text.matches('x').count(), MAX_CAPTURE_BYTES);
        assert!(reading.text.contains(TRUNCATED_MARKER));
        server.set_behavior(Behavior::Respond { status: 200, body: "tiny".into() });
        assert!(!read_input(&input, &reading.state).await.text.contains(TRUNCATED_MARKER));
    }

    #[tokio::test]
    async fn a_redirect_to_https_is_a_read_error_not_an_observation() {
        // why: V19/D7 (S19: "redirects to HTTPS are errors"): vigil has no TLS
        //      stack, so a 302 to https must surface as an error reading that
        //      names https, instead of "HTTP 302" or a silent follow.
        // proves: CLAIM(validation)
        let server = FakeHttpServer::start(Behavior::Redirect { location: "https://example.invalid/secure".into() });
        let input = Input::Url { url: format!("{}/", server.url()) };
        let reading = read_input(&input, &InputState::default()).await;
        assert!(reading.is_error, "{:?}", reading.text);
        assert!(reading.text.contains("https"), "{:?}", reading.text);
        assert_eq!(server.request_count(), 1, "control: the redirecting server was really asked");
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

    // ---- process (V17) ----

    #[test]
    fn proc_stat_parses_a_real_capture_field_by_field() {
        // why: V17: the process input depends on /proc/<pid>/stat fields 3
        //      (state) and 22 (starttime). Literal line captured from this
        //      kernel (`cat /proc/self/stat`, Linux 6.18, 2026-10-29), decoded
        //      by hand: 1358223 (cat) R <ppid 1358221> ... field 22 =
        //      12502253 (the 20th token after the closing paren; tokens 10-17
        //      are counters, 18 = priority 20, 19 = nice 0, 20 = threads 1,
        //      21 = itrealvalue 0, 22 = starttime).
        // proves: CLAIM(proc-stat)
        let line = "1358223 (cat) R 1358221 1358221 1358221 0 -1 4194304 301 0 0 0 0 0 0 0 20 0 1 0 12502253 17498112 1187 18446744073709551615 108786689347584 108786690454017 140725688711360 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0 108786690805320 108786690857988 108787691892736 140725688714716 140725688714736 140725688714736 140725688721369 0\n";
        assert_eq!(parse_proc_stat(line), Some(ProcStat { state: 'R', start_time: 12_502_253 }));
    }

    #[test]
    fn proc_stat_counts_fields_after_the_last_paren_and_reads_zombies() {
        // why: V17 + proc(5): comm may contain spaces and parentheses. Line 1
        //      has comm "my (weird) name" and state Z (zombie), starttime
        //      987654 (tokens after the paren: Z=3, 1=4, 4242=5, 4242=6, 0=7,
        //      -1=8, 4228620=9, eight 0s=10..17, 20=18, 0=19, 1=20, 0=21,
        //      987654=22). Line 2 is adversarial: comm "a) S 1 (b" would give
        //      state S / wrong fields if the FIRST paren were used; correct is
        //      R and 555.
        // proves: CLAIM(proc-stat)
        let zombie = "4242 (my (weird) name) Z 1 4242 4242 0 -1 4228620 0 0 0 0 0 0 0 0 20 0 1 0 987654 0 0\n";
        assert_eq!(parse_proc_stat(zombie), Some(ProcStat { state: 'Z', start_time: 987_654 }));
        let adversarial = "31337 (a) S 1 (b) R 1 31337 31337 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 555 0 0\n";
        assert_eq!(parse_proc_stat(adversarial), Some(ProcStat { state: 'R', start_time: 555 }));
    }

    #[test]
    fn proc_stat_rejects_incomplete_lines() {
        // why: V17: a truncated or garbage line must not be read as "running"
        //      or as a start time; None lets the caller treat it as gone.
        assert_eq!(parse_proc_stat(""), None);
        assert_eq!(parse_proc_stat("garbage"), None);
        assert_eq!(parse_proc_stat("4242 (x) S 1 4242 4242 0 -1"), None, "stops before field 22");
        assert_eq!(parse_proc_stat("4242 (x) S 1 4242 4242 0 -1 4 0 0 0 0 0 0 0 0 20 0 1 0 notanumber 0"), None);
    }

    #[tokio::test]
    async fn process_reports_running_then_exited() {
        // why: SPEC input `process` with `want: exited`. Uses a real spawned
        //      child; it is reaped (wait) so /proc no longer lists it. V17:
        //      the baseline records the start time.
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let input = Input::Process { pid, want: Some(ProcessWant::Exited) };
        let baseline = read_input(&input, &InputState::default()).await;
        assert_eq!(baseline.text, "running");
        assert_eq!(baseline.want_met, Some(false), "want exited, still running");
        assert!(!baseline.changed);
        assert!(baseline.state.proc_start.is_some(), "V17: start time recorded");

        child.kill().unwrap();
        child.wait().unwrap();
        let exited = read_input(&input, &baseline.state).await;
        assert_eq!(exited.text, "exited");
        assert_eq!(exited.want_met, Some(true));
        assert!(exited.changed);
    }

    #[tokio::test]
    async fn a_zombie_counts_as_exited() {
        // why: V17 (S17): the old test reaped its child before observing, so a
        //      zombie (state Z, /proc/<pid> still present) was never seen. A
        //      child that has exited but is not yet waited for is exited.
        //      Positive control: a live sleeping child reads "running" with
        //      the same code path.
        // proves: CLAIM(process-exited)
        let mut zombie = std::process::Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        let pid = zombie.id();
        let started = std::time::Instant::now();
        while proc_state(pid) != Some('Z') {
            assert!(started.elapsed() < Duration::from_secs(3), "child never became a zombie");
            std::thread::sleep(Duration::from_millis(10));
        }
        let input = Input::Process { pid, want: Some(ProcessWant::Exited) };
        let reading = read_input(&input, &InputState::default()).await;
        assert_eq!(reading.text, "exited");
        assert_eq!(reading.want_met, Some(true));
        zombie.wait().unwrap();

        let mut alive = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let live = read_input(&Input::Process { pid: alive.id(), want: None }, &InputState::default()).await;
        assert_eq!(live.text, "running");
        alive.kill().unwrap();
        alive.wait().unwrap();
    }

    #[tokio::test]
    async fn a_pid_whose_start_time_differs_is_a_different_process_and_counts_as_exited() {
        // why: V17 (S17): after PID reuse the number is alive but it is not
        //      the process that was watched. A real reuse cannot be forced,
        //      so the recorded start time is altered (+1 tick) while the
        //      process is really running: identity mismatch => "exited" and a
        //      change. Control: the unaltered state reads "running", quiet.
        // proves: CLAIM(process-exited)
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let input = Input::Process { pid: child.id(), want: Some(ProcessWant::Exited) };
        let baseline = read_input(&input, &InputState::default()).await;
        let same = read_input(&input, &baseline.state).await;
        assert_eq!(same.text, "running");
        assert!(!same.changed);

        let mut reused = baseline.state.clone();
        reused.proc_start = Some(reused.proc_start.unwrap() + 1);
        let mismatch = read_input(&input, &reused).await;
        assert_eq!(mismatch.text, "exited");
        assert_eq!(mismatch.want_met, Some(true));
        assert!(mismatch.changed);
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
