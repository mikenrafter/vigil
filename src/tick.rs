//! One evaluation round of one watch (SPEC "Tick logic").

use crate::engine::Vigil;
use crate::error::VigilError;
use crate::watch::{WatchId, WatchStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickOutcome {
    /// A tick ran (the watch may have fired during it).
    Ticked,
    /// Not due yet, or lost the race after taking the lock.
    NotDue,
    /// The watch is finished (fired, expired, cancelled), possibly just now
    /// because the deadline passed.
    Finished(WatchStatus),
}

impl Vigil {
    /// Run the watch's tick if `last_tick_at + interval_s` is due (the first
    /// tick is due immediately, and compares against the baseline recorded at
    /// `create`, V1). Takes the global lock (V4), re-reads the watch under it
    /// and repeats the due/terminal checks; a finished watch is returned as
    /// is (V3). Rewrites the watch file atomically at the end.
    pub async fn tick_due(&self, id: &WatchId) -> Result<TickOutcome, VigilError> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judge::{Answers, HttpJudge, JudgeError};
    use crate::testing::*;
    use crate::watch::{FireKind, Input, JudgeStatus, PortWant, ProcessWant, WatchFile};
    use std::net::TcpListener;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    struct Fixture {
        clock: Arc<ManualClock>,
        env: Env,
    }

    fn fixture() -> Fixture {
        let clock = Arc::new(ManualClock::new(spec_instant()));
        let env = Env::with_clock(clock.clone());
        Fixture { clock, env }
    }

    impl Fixture {
        fn load(&self, id: &WatchId) -> WatchFile {
            self.env.vigil.store().load(id).unwrap()
        }
        async fn tick(&self, id: &WatchId) -> TickOutcome {
            self.env.vigil.tick_due(id).await.unwrap()
        }
        /// Advance one interval and tick.
        async fn next_tick(&self, id: &WatchId, interval_s: u64) -> TickOutcome {
            self.clock.advance_secs(interval_s);
            self.tick(id).await
        }
        async fn create(&self, request: crate::watch::CreateRequest) -> WatchId {
            self.env.vigil.create(request).await.unwrap().watch_id
        }
        /// A fire=change watch on a fresh file containing "old\n"; the
        /// baseline was taken at create (V1) and the first tick (tick 1, due
        /// at once) has run and found nothing.
        async fn baselined_change_watch(&self, id: &str) -> (WatchId, std::path::PathBuf) {
            let path = self.env.input_path(&format!("{id}.log"));
            write_file(&path, "old\n");
            let watch = self.create(change_request(id, &path, 20)).await;
            assert_eq!(self.tick(&watch).await, TickOutcome::Ticked);
            assert_eq!(self.load(&watch).status, WatchStatus::Pending, "precondition: first tick is quiet");
            (watch, path)
        }
        /// A fire=judge watch on a file containing "old\n"; first tick run, quiet.
        async fn baselined_judge_watch(&self, id: &str) -> (WatchId, std::path::PathBuf) {
            let path = self.env.input_path(&format!("{id}.log"));
            write_file(&path, "old\n");
            let watch = self.create(judge_request(id, &path, 20)).await;
            assert_eq!(self.tick(&watch).await, TickOutcome::Ticked);
            (watch, path)
        }
    }

    fn numbered(prefix: &str, n: usize) -> String {
        (0..n).map(|i| format!("{prefix}-line-{i:04}\n")).collect()
    }

    // ---- baseline at create (V1) ----

    #[tokio::test]
    async fn the_first_tick_is_due_immediately_and_is_quiet_when_nothing_changed_since_create() {
        // why: SPEC "watch_create": no tick runs until the first keepalive;
        //      then the first tick is due at once. V1: it compares against the
        //      baseline recorded at create, so with no change since create it
        //      finds nothing (a fire=change watch on an existing file must not
        //      fire at once).
        // proves: CLAIM(baseline-at-create)
        let f = fixture();
        let (id, _) = f.baselined_change_watch("w").await;
        let file = f.load(&id);
        assert_eq!(file.tick, 1);
        assert_eq!(file.status, WatchStatus::Pending);
        assert!(file.events.is_empty());
        assert_eq!(file.last_tick_at, Some(spec_instant()));
        assert_eq!(file.input_states.len(), 1, "one remembered state per input");
    }

    #[tokio::test]
    async fn a_write_between_create_and_the_first_tick_is_a_change() {
        // why: V1 (S1, blocker): the old design baselined at the first tick,
        //      so a build that failed in the gap between watch_create and the
        //      first keepalive was swallowed (a repeating appender hid this).
        //      ONE write after create, before any tick: the very first tick
        //      (tick 1) must fire with it. Judge variant: the line reaches the
        //      judge. Control: without the write neither watch fires.
        // proves: CLAIM(baseline-at-create)
        let f = fixture();
        let quiet_path = f.env.input_path("quiet.log");
        write_file(&quiet_path, "old\n");
        let quiet = f.create(change_request("quiet", &quiet_path, 20)).await;
        f.tick(&quiet).await;
        assert_eq!(f.load(&quiet).status, WatchStatus::Pending, "control: no write, no fire");

        let path = f.env.input_path("gap.log");
        write_file(&path, "old\n");
        let id = f.create(change_request("gap", &path, 20)).await;
        append_file(&path, "ERROR: failed in the gap\n");
        assert_eq!(f.tick(&id).await, TickOutcome::Ticked);
        let file = f.load(&id);
        assert_eq!((file.status, file.tick), (WatchStatus::Fired, 1));
        assert!(file.events[0].summary.contains("failed in the gap"));

        let jpath = f.env.input_path("jgap.log");
        write_file(&jpath, "old\n");
        let jid = f.create(judge_request("jgap", &jpath, 20)).await;
        f.env.judge.push(Ok(noul_answers("failed", 0.99, 0.9)));
        append_file(&jpath, "ERROR: judged gap\n");
        f.tick(&jid).await;
        assert_eq!(f.load(&jid).status, WatchStatus::Fired);
        assert!(f.env.judge.requests()[0].state.contains("ERROR: judged gap"));
    }

    // ---- cadence ----

    #[tokio::test]
    async fn an_immediate_second_call_does_not_add_a_tick() {
        // why: D3 / SPEC "keepalive": ticks run at last_tick_at + interval_s,
        //      so cadence holds across calls. Due exactly at +20 s (inclusive);
        //      19 s is not due.
        // proves: CLAIM(cadence)
        let f = fixture();
        let (id, _) = f.baselined_change_watch("w").await;
        assert_eq!(f.tick(&id).await, TickOutcome::NotDue);
        f.clock.advance_secs(19);
        assert_eq!(f.tick(&id).await, TickOutcome::NotDue);
        assert_eq!(f.load(&id).tick, 1);
        f.clock.advance_secs(1);
        assert_eq!(f.tick(&id).await, TickOutcome::Ticked);
        let file = f.load(&id);
        assert_eq!(file.tick, 2);
        assert_eq!(file.last_tick_at, Some(spec_instant().plus_secs(20)));
    }

    // ---- change / condition ----

    #[tokio::test]
    async fn change_watch_fires_on_the_first_change_and_attaches_it() {
        // why: SPEC fire = "change": fire on any input change, no model call.
        //      Event carries tick (2), the stable event_id "build:2" (V3),
        //      watch id, input label and the changed line as summary; the
        //      watch is finished (D1) and stops ticking.
        // proves: CLAIM(fire-change)
        let f = fixture();
        let (id, path) = f.baselined_change_watch("build").await;
        append_file(&path, "ERROR: linking failed\n");
        assert_eq!(f.next_tick(&id, 20).await, TickOutcome::Ticked);
        let file = f.load(&id);
        assert_eq!(file.status, WatchStatus::Fired);
        assert_eq!(file.finished_at, Some(spec_instant().plus_secs(20)));
        assert_eq!(file.events.len(), 1);
        let event = &file.events[0];
        assert_eq!(event.tick, 2);
        assert_eq!(event.event_id, "build:2");
        assert_eq!(event.watch_id, "build");
        assert_eq!(event.input, format!("file:{}", path.display()));
        assert!(event.summary.contains("ERROR: linking failed"), "{:?}", event.summary);
        assert!(event.judge.is_none(), "no model call for fire = change");
        assert!(file.last_digest.contains("ERROR: linking failed"));
        assert!(!file.last_digest.contains("old"), "digest is only what is new");
        assert_eq!(f.env.judge.call_count(), 0);

        assert_eq!(f.next_tick(&id, 20).await, TickOutcome::Finished(WatchStatus::Fired));
        assert_eq!(f.load(&id).tick, 2, "a fired watch is finished and no longer ticks");
    }

    #[tokio::test]
    async fn events_of_one_tick_get_distinct_stable_ids_in_input_order() {
        // why: V3 (S3) + the contradiction flagged in .red-report.md: the
        //      id `<watch_id>:<tick>` alone collides when two inputs change in
        //      one tick. Default: first `w:1`, second `w:1:1`. Both inputs are
        //      appended after create, so tick 1 yields both, in input order.
        // proves: CLAIM(event-id)
        let f = fixture();
        let (first, second) = (f.env.input_path("first.log"), f.env.input_path("second.log"));
        write_file(&first, "a\n");
        write_file(&second, "b\n");
        let mut request = change_request("w", &first, 20);
        request.inputs.push(Input::File { path: second.clone(), tail_bytes: None });
        let id = f.create(request).await;
        append_file(&first, "first-new\n");
        append_file(&second, "second-new\n");
        f.tick(&id).await;
        let events = f.load(&id).events;
        assert_eq!(events.len(), 2);
        assert_eq!((events[0].event_id.as_str(), events[1].event_id.as_str()), ("w:1", "w:1:1"));
        assert!(events[0].summary.contains("first-new") && events[1].summary.contains("second-new"));
    }

    #[tokio::test]
    async fn quiet_ticks_never_fire_a_change_watch() {
        // why: control for the test above: only a real change fires. 30 quiet
        //      ticks leave it pending with no events; the next write fires it.
        let f = fixture();
        let (id, path) = f.baselined_change_watch("w").await;
        for _ in 0..30 {
            assert_eq!(f.next_tick(&id, 20).await, TickOutcome::Ticked);
        }
        let file = f.load(&id);
        assert_eq!((file.status, file.tick, file.events.len()), (WatchStatus::Pending, 31, 0));
        append_file(&path, "now\n");
        f.next_tick(&id, 20).await;
        assert_eq!(f.load(&id).status, WatchStatus::Fired);
    }

    fn condition_request(id: &str, inputs: Vec<Input>) -> crate::watch::CreateRequest {
        crate::watch::CreateRequest {
            id: Some(id.into()),
            prompt: String::new(),
            inputs,
            interval_s: Some(20),
            deadline_s: Some(3600),
            fire: FireKind::Condition,
            judge_questions: None,
            judge_rule: None,
        }
    }

    #[tokio::test]
    async fn condition_fires_only_when_every_want_is_met() {
        // why: SPEC fire = "condition": fire when every input that has a want
        //      meets it. Port want-open is met (listener held) from the start;
        //      process want-exited is met only after the child dies. No fire
        //      while either is unmet.
        // proves: CLAIM(fire-condition)
        let f = fixture();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let id = f
            .create(condition_request(
                "both",
                vec![
                    Input::Port { host: "127.0.0.1".into(), port, want: Some(PortWant::Open) },
                    Input::Process { pid: child.id(), want: Some(ProcessWant::Exited) },
                ],
            ))
            .await;
        f.tick(&id).await;
        for _ in 0..3 {
            f.next_tick(&id, 20).await;
        }
        assert_eq!(f.load(&id).status, WatchStatus::Pending, "child still running: not all wants met");

        child.kill().unwrap();
        child.wait().unwrap();
        f.next_tick(&id, 20).await;
        let file = f.load(&id);
        assert_eq!(file.status, WatchStatus::Fired);
        assert_eq!(file.events.len(), 1);
        assert!(file.events[0].judge.is_none());
        drop(listener);
    }

    #[tokio::test]
    async fn condition_does_not_fire_when_only_some_wants_are_met() {
        // why: the converse of the test above: process want-exited is met
        //      (already-dead child) but port want-open is not (nothing
        //      listening), so "all wants" is false and the watch stays pending.
        let f = fixture();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut child = std::process::Command::new("true").spawn().unwrap();
        child.wait().unwrap();
        let id = f
            .create(condition_request(
                "partial",
                vec![
                    Input::Port { host: "127.0.0.1".into(), port, want: Some(PortWant::Open) },
                    Input::Process { pid: child.id(), want: Some(ProcessWant::Exited) },
                ],
            ))
            .await;
        f.tick(&id).await;
        for _ in 0..3 {
            f.next_tick(&id, 20).await;
        }
        assert_eq!(f.load(&id).status, WatchStatus::Pending);
    }

    #[tokio::test]
    async fn condition_already_true_at_the_first_tick_fires_without_a_change() {
        // why: R1 (kept by Sol): "fire = condition is evaluated on every tick,
        //      including the first, changed or not". A state test, not a change
        //      test: "wait until the process has exited" returns at once if it
        //      already has - even though the baseline taken at create (V1)
        //      already saw "exited".
        let f = fixture();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        child.wait().unwrap();
        let id = f
            .create(condition_request("already", vec![Input::Process { pid: child.id(), want: Some(ProcessWant::Exited) }]))
            .await;
        f.tick(&id).await;
        assert_eq!(f.load(&id).status, WatchStatus::Fired);
    }

    // ---- deadline ----

    #[tokio::test]
    async fn deadline_expiry_reports_expired_with_the_last_digest() {
        // why: SPEC "Failure rules"/Deadline: once passed, status expired with
        //      the last digest so the agent knows the final state. The judge
        //      saw a change ("warning: slow") but did not fire; the digest
        //      must still carry it at expiry. R5: expiry runs no final tick
        //      and adds no event.
        // proves: CLAIM(deadline-expiry)
        let f = fixture();
        let (id, path) = f.baselined_judge_watch("w").await;
        f.env.judge.set_fallback(Ok(noul_answers("failed", 0.1, 0.9)));
        append_file(&path, "warning: slow\n");
        f.next_tick(&id, 20).await;
        assert_eq!(f.load(&id).status, WatchStatus::Pending);

        f.clock.advance_secs(3600);
        assert_eq!(f.tick(&id).await, TickOutcome::Finished(WatchStatus::Expired));
        let file = f.load(&id);
        assert_eq!(file.status, WatchStatus::Expired);
        assert!(file.last_digest.contains("warning: slow"), "{:?}", file.last_digest);
        assert!(file.events.is_empty(), "R5: expiry adds no event");
        assert_eq!(file.finished_at, Some(file.deadline_at), "V16: an expired watch finishes at its deadline");
        assert_eq!(f.tick(&id).await, TickOutcome::Finished(WatchStatus::Expired), "stays expired");
    }

    #[tokio::test]
    async fn a_deadline_that_passed_while_nothing_ran_expires_on_the_next_call() {
        // why: SPEC "Failure rules": "a watch whose deadline passed while no
        //      keepalive ran reports expired on the next call" - the deadline
        //      is an absolute time in the file. V16: its finish time is the
        //      deadline itself (not the late call), which starts the 7-day
        //      prune clock at the right moment.
        // proves: CLAIM(absolute-deadline)
        let f = fixture();
        let path = f.env.input_path("w.log");
        write_file(&path, "old\n");
        let mut request = change_request("w", &path, 20);
        request.deadline_s = Some(100);
        let id = f.create(request).await;
        f.clock.advance_secs(100_000);
        assert_eq!(f.tick(&id).await, TickOutcome::Finished(WatchStatus::Expired));
        let file = f.load(&id);
        assert_eq!(file.status, WatchStatus::Expired);
        assert_eq!(file.finished_at, Some(spec_instant().plus_secs(100)));
    }

    // ---- terminal states (V3, V4) ----

    #[tokio::test]
    async fn a_cancelled_watch_ignores_a_later_tick_and_a_fired_watch_ignores_a_later_deadline() {
        // why: V4 (S4): terminal states are immutable. Cancel first, then a
        //      tick over a file that changed: Finished(Cancelled), no event,
        //      tick count and offsets untouched. Fire first, then the deadline
        //      passes: the next tick returns Finished(Fired) and the stored
        //      status, finished_at and events are byte-for-byte unchanged.
        // proves: CLAIM(terminal-immutable)
        let f = fixture();
        let (cancelled, cpath) = f.baselined_change_watch("c").await;
        f.env.vigil.cancel(&cancelled).await.unwrap();
        append_file(&cpath, "after cancel\n");
        let before = f.load(&cancelled);
        assert_eq!(f.next_tick(&cancelled, 20).await, TickOutcome::Finished(WatchStatus::Cancelled));
        assert_eq!(f.load(&cancelled), before);
        assert!(before.events.is_empty());

        let (fired, fpath) = f.baselined_change_watch("f").await;
        append_file(&fpath, "boom\n");
        f.next_tick(&fired, 20).await;
        let stored = std::fs::read(f.env.vigil.store().watch_path(&fired)).unwrap();
        f.clock.advance_secs(10_000);
        assert_eq!(f.tick(&fired).await, TickOutcome::Finished(WatchStatus::Fired));
        assert_eq!(std::fs::read(f.env.vigil.store().watch_path(&fired)).unwrap(), stored);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_cancel_and_fire_leave_one_terminal_state_that_everyone_agrees_on() {
        // why: V4 (S4): "deadline/fire/cancel precedence": the first terminal
        //      state written under the lock wins. 30 rounds race a tick that
        //      fires (the file changed after create) against a cancel from
        //      another process. Whichever wins, the file's status, the cancel
        //      envelope and the tick outcome agree; a Fired file has exactly
        //      its one event, a Cancelled file has none. (Which side wins is
        //      scheduler-dependent, so only the invariant is asserted.)
        // proves: CLAIM(terminal-precedence)
        for round in 0..30 {
            let f = fixture();
            let path = f.env.input_path("w.log");
            write_file(&path, "old\n");
            let id = f.create(change_request("w", &path, 20)).await;
            append_file(&path, "boom\n");
            let (ticker, canceller) = (Arc::new(f.env.another_process()), Arc::new(f.env.another_process()));
            let tick = tokio::spawn({
                let id = id.clone();
                async move { ticker.tick_due(&id).await.unwrap() }
            });
            let cancel = tokio::spawn({
                let id = id.clone();
                async move { canceller.cancel(&id).await.unwrap() }
            });
            let (tick_outcome, cancel_envelope) = (tick.await.unwrap(), cancel.await.unwrap());
            let file = f.load(&id);
            match file.status {
                WatchStatus::Fired => {
                    assert_eq!(file.events.len(), 1, "round {round}");
                    assert_eq!(cancel_envelope.status, WatchStatus::Fired, "round {round}: cancel must report the stored result");
                    assert_eq!(cancel_envelope.events, file.events, "round {round}");
                    assert_eq!(tick_outcome, TickOutcome::Ticked, "round {round}");
                }
                WatchStatus::Cancelled => {
                    assert!(file.events.is_empty(), "round {round}");
                    assert_eq!(cancel_envelope.status, WatchStatus::Cancelled, "round {round}");
                    assert_eq!(tick_outcome, TickOutcome::Finished(WatchStatus::Cancelled), "round {round}");
                }
                other => panic!("round {round}: unexpected terminal state {other:?}"),
            }
        }
    }

    // ---- restart ----

    #[tokio::test]
    async fn a_new_process_keeps_the_deadline_the_tick_count_and_the_cadence() {
        // why: SPEC acceptance: "Killing vigil mcp mid-watch and calling
        //      keepalive with the watch id from a new process does not reset
        //      elapsed time or the tick count." A second Vigil over the same
        //      root stands in for the new process.
        // proves: CLAIM(restart-continuity)
        let f = fixture();
        let (id, path) = f.baselined_change_watch("w").await;
        f.next_tick(&id, 20).await;
        f.next_tick(&id, 20).await;
        let before = f.load(&id);
        assert_eq!(before.tick, 3);

        let second_process = f.env.another_process();
        let report = second_process.status(&id).await.unwrap();
        assert_eq!(report.envelope.tick, 3);
        assert_eq!(report.envelope.elapsed_s, 40);
        assert_eq!(report.envelope.deadline_in_s, 3600 - 40);
        assert_eq!(second_process.tick_due(&id).await.unwrap(), TickOutcome::NotDue, "cadence survives the restart");

        append_file(&path, "new line\n");
        f.clock.advance_secs(20);
        assert_eq!(second_process.tick_due(&id).await.unwrap(), TickOutcome::Ticked);
        let after = f.load(&id);
        assert_eq!(after.tick, 4);
        assert_eq!(after.created_at, before.created_at);
        assert_eq!(after.deadline_at, before.deadline_at);
        assert_eq!(after.status, WatchStatus::Fired);
        assert!(after.last_digest.contains("new line") && !after.last_digest.contains("old"));
    }

    // ---- V11: relative inputs resolve against the creator's cwd ----

    #[tokio::test]
    async fn a_watch_created_in_one_directory_and_resumed_from_another_reads_the_creators_inputs() {
        // why: V11 (S11): relative file paths, glob patterns and cmd working
        //      directories were resolved by whichever process resumed the
        //      watch. Created from dir A with a relative file, a relative glob
        //      and `pwd`; resumed from dir B, which holds DECOY files with the
        //      same relative names and where `pwd` would print B. Only the
        //      file in A is appended to, so exactly ONE event (the A file) is
        //      allowed: a glob or cmd resolved against B would add a second
        //      event (decoy match created / pwd output changed). The stored
        //      watch has absolute paths.
        // proves: CLAIM(creator-cwd)
        let f = fixture();
        let (a, b) = (f.env.dir.path().join("a"), f.env.dir.path().join("b"));
        for dir in [&a, &b] {
            std::fs::create_dir_all(dir.join("out")).unwrap();
        }
        write_file(&a.join("rel.log"), "A-old\n");
        write_file(&b.join("rel.log"), "B-DECOY-old\n");
        write_file(&a.join("out/seed.txt"), "");
        write_file(&b.join("out/decoy.txt"), "");

        let mut request = change_request("w", Path::new("rel.log"), 20);
        request.inputs.push(Input::Glob { pattern: "out/*.txt".into(), report: crate::watch::GlobReport::Created });
        request.inputs.push(Input::Cmd { argv: vec!["pwd".into()], timeout_s: None, cwd: None });
        let creator = f.env.another_process_in(&a);
        let id = creator.create(request).await.unwrap().watch_id;

        let stored = f.load(&id);
        assert_eq!(stored.spec.inputs[0], Input::File { path: a.join("rel.log"), tail_bytes: None });
        assert!(matches!(&stored.spec.inputs[1], Input::Glob { pattern, .. } if pattern == &format!("{}/out/*.txt", a.display())), "{:?}", stored.spec.inputs[1]);
        assert!(matches!(&stored.spec.inputs[2], Input::Cmd { cwd: Some(cwd), .. } if cwd == &a), "{:?}", stored.spec.inputs[2]);
        assert_eq!(stored.input_states[0].offset, Some(6), "baseline read A's file (6 bytes), not B's");

        append_file(&a.join("rel.log"), "A-new\n");
        append_file(&b.join("rel.log"), "B-DECOY-new\n");
        write_file(&b.join("out/decoy2.txt"), "");
        let resumer = f.env.another_process_in(&b);
        f.clock.advance_secs(1);
        assert_eq!(resumer.tick_due(&id).await.unwrap(), TickOutcome::Ticked);
        let file = f.load(&id);
        assert_eq!(file.status, WatchStatus::Fired);
        assert_eq!(file.events.len(), 1, "only A's file changed: {:?}", file.events);
        assert_eq!(file.events[0].input, format!("file:{}", a.join("rel.log").display()));
        assert!(file.last_digest.contains("A-new") && !file.last_digest.contains("DECOY"));
    }

    // ---- concurrency ----

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_ticks_in_one_test_process_produce_exactly_one_tick() {
        // why: SPEC "State": two sessions on one watch must not double-tick;
        //      the global lock plus a due-time recheck under it make the
        //      losers return NotDue. 8 racers across 2 Vigil objects, twice.
        //      S25 caveat: both objects live in ONE OS process, so this alone
        //      could pass with a process-local mutex; the cross-PROCESS proof
        //      is `two_real_processes_*` in tests/cli_e2e.rs.
        // proves: CLAIM(tick-lock)
        let f = fixture();
        let (id, _) = f.baselined_change_watch("w").await;
        let first = Arc::new(f.env.another_process());
        let second = Arc::new(f.env.another_process());

        async fn race(vigils: [&Arc<Vigil>; 2], id: &WatchId) -> usize {
            let mut handles = Vec::new();
            for index in 0..8 {
                let vigil = vigils[index % 2].clone();
                let id = id.clone();
                handles.push(tokio::spawn(async move { vigil.tick_due(&id).await.unwrap() }));
            }
            let mut ticked = 0;
            for handle in handles {
                if handle.await.unwrap() == TickOutcome::Ticked {
                    ticked += 1;
                }
            }
            ticked
        }

        f.clock.advance_secs(20);
        assert_eq!(race([&first, &second], &id).await, 1);
        assert_eq!(f.load(&id).tick, 2);
        f.clock.advance_secs(20);
        assert_eq!(race([&first, &second], &id).await, 1);
        assert_eq!(f.load(&id).tick, 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tick_waits_for_the_global_lock_and_proceeds_after_it_is_released() {
        // why: V4/V5 (S25): every mutation holds the global lock. While
        //      another holder has it, tick_due must not complete and must
        //      write nothing; after the release it completes with the tick
        //      recorded. Contention is shown by contrast, not assumed: the
        //      SAME call on an unlocked watch finishes inside the 300 ms
        //      window (control), the locked one does not. (How the wait is
        //      implemented, blocking flock or polling, is the implementer's
        //      choice, so /proc/locks is not consulted here; the store test
        //      does that for `Store::lock`.) Multi-thread so a blocking flock
        //      inside the tick cannot freeze this test.
        // proves: CLAIM(tick-lock)
        let f = fixture();
        let (id, _) = f.baselined_change_watch("w").await;
        let (control, _) = f.baselined_change_watch("control").await;
        f.clock.advance_secs(20);
        let window = Duration::from_millis(300);

        let uncontended = tokio::time::timeout(window, f.env.another_process().tick_due(&control)).await;
        assert_eq!(uncontended.expect("control: an unlocked tick completes quickly").unwrap(), TickOutcome::Ticked);

        let guard = f.env.vigil.store().lock().unwrap();
        let before = std::fs::read(f.env.vigil.store().watch_path(&id)).unwrap();
        let vigil = Arc::new(f.env.another_process());
        let tick = tokio::spawn({
            let (vigil, id) = (vigil.clone(), id.clone());
            async move { vigil.tick_due(&id).await.unwrap() }
        });
        tokio::time::sleep(window).await;
        assert!(!tick.is_finished(), "tick completed while the lock was held");
        assert_eq!(std::fs::read(f.env.vigil.store().watch_path(&id)).unwrap(), before, "nothing written under contention");
        drop(guard);
        assert_eq!(tokio::time::timeout(Duration::from_secs(5), tick).await.expect("tick must proceed after release").unwrap(), TickOutcome::Ticked);
        assert_eq!(f.load(&id).tick, 2);
    }

    // ---- no judge call without a change ----

    #[tokio::test]
    async fn one_hundred_quiet_ticks_make_zero_judge_requests() {
        // why: SPEC acceptance: "100 quiet ticks produce zero judge requests".
        //      Uses the real HttpJudge against the counting fake server.
        //      Positive control in the same test: one real change moves the
        //      counter to 1, so the 0 is not a broken counter.
        // proves: CLAIM(no-change-no-call)
        let server = FakeHttpServer::start(Behavior::Respond { status: 200, body: laya_noul_body("failed", 0.95, 0.95) });
        let clock = Arc::new(ManualClock::new(spec_instant()));
        let dir = tempfile::tempdir().unwrap();
        let judge = Arc::new(HttpJudge::new(&server.url(), Duration::from_secs(2), Duration::from_secs(2)));
        let vigil = Vigil::new(dir.path().join("state"), clock.clone(), judge, default_config());
        let path = dir.path().join("build.log");
        write_file(&path, "boot\n");
        let id = vigil.create(judge_request("w", &path, 20)).await.unwrap().watch_id;
        assert_eq!(vigil.tick_due(&id).await.unwrap(), TickOutcome::Ticked);
        let health_requests = server.request_count();

        for _ in 0..100 {
            clock.advance_secs(20);
            assert_eq!(vigil.tick_due(&id).await.unwrap(), TickOutcome::Ticked);
        }
        assert_eq!(vigil.store().load(&id).unwrap().tick, 101);
        assert_eq!(server.requests().iter().filter(|r| r.path == "/v1/predict").count(), 0);
        assert_eq!(server.request_count(), health_requests, "no traffic at all while quiet");

        append_file(&path, "ERROR: boom\n");
        clock.advance_secs(20);
        vigil.tick_due(&id).await.unwrap();
        let predicts: Vec<_> = server.requests().into_iter().filter(|r| r.path == "/v1/predict").collect();
        assert_eq!(predicts.len(), 1, "control: a real change makes exactly one request");
        assert!(predicts[0].body.contains("ERROR: boom"));
        assert_eq!(vigil.store().load(&id).unwrap().status, WatchStatus::Fired);
    }

    #[tokio::test]
    async fn two_identical_appends_are_two_changes_for_the_judge() {
        // why: V9 (S9) end to end: "tick\n" appended twice, each judged
        //      below threshold. The old hash-of-reading design saw identical
        //      text and asked the judge once. Two requests, each with the line.
        // proves: CLAIM(positional-file-change)
        let f = fixture();
        let (id, path) = f.baselined_judge_watch("w").await;
        f.env.judge.set_fallback(Ok(noul_answers("failed", 0.1, 0.9)));
        append_file(&path, "tick\n");
        f.next_tick(&id, 20).await;
        f.next_tick(&id, 20).await; // quiet tick between
        assert_eq!(f.env.judge.call_count(), 1);
        append_file(&path, "tick\n");
        f.next_tick(&id, 20).await;
        assert_eq!(f.env.judge.call_count(), 2, "the identical second append is a second change");
        assert!(f.env.judge.requests()[1].state.contains("tick"));
    }

    // ---- judge phase ----

    #[tokio::test]
    async fn judge_watch_fires_when_the_rule_matches_and_records_the_answer() {
        // why: SPEC tick steps 4-5: send {state, questions}; fire if the rule
        //      matches; event carries the judge answers (envelope example).
        //      Questions reach the judge unchanged; state starts with the
        //      prompt and has the new line newest-first.
        // proves: CLAIM(fire-judge)
        let f = fixture();
        let (id, path) = f.baselined_judge_watch("w").await;
        f.env.judge.push(Ok(noul_answers("failed", 0.93, 0.93)));
        append_file(&path, "step 1\nERROR: linking failed\n");
        f.next_tick(&id, 20).await;

        let file = f.load(&id);
        assert_eq!(file.status, WatchStatus::Fired);
        assert_eq!(file.judge_status, JudgeStatus::Ok);
        let event = &file.events[0];
        assert_eq!(event.judge, Some(noul_answers("failed", 0.93, 0.93)));
        assert!(event.summary.contains("ERROR: linking failed"));

        let requests = f.env.judge.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].questions, failed_questions());
        assert!(requests[0].state.starts_with("Tell me if the build log shows a failure or finishes."));
        let state = &requests[0].state;
        assert!(state.find("ERROR: linking failed").unwrap() < state.find("step 1").unwrap(), "newest line first");
        assert!(!state.contains("old"), "only new content is sent");
    }

    #[tokio::test]
    async fn judge_below_threshold_does_not_fire_and_consumes_the_change() {
        // why: an answered-but-negative judgement is a normal outcome: pending,
        //      judge_status ok, and the change is not re-asked on later quiet
        //      ticks (only failures keep a change pending).
        let f = fixture();
        let (id, path) = f.baselined_judge_watch("w").await;
        f.env.judge.push(Ok(noul_answers("failed", 0.79, 0.9)));
        append_file(&path, "all fine\n");
        f.next_tick(&id, 20).await;
        let file = f.load(&id);
        assert_eq!((file.status, file.judge_status), (WatchStatus::Pending, JudgeStatus::Ok));
        assert!(file.pending_change.is_none());
        f.next_tick(&id, 20).await;
        f.next_tick(&id, 20).await;
        assert_eq!(f.env.judge.call_count(), 1);
    }

    #[tokio::test]
    async fn each_judge_failure_mode_marks_down_keeps_the_change_and_does_not_fire() {
        // why: SPEC "Failure rules": judge down, timed out, non-2xx, or an
        //      answer missing the rule's question -> judge_status "down", do
        //      not fire, do not drop the change. One strike each.
        // proves: CLAIM(judge-failure-keeps-change)
        let failures: Vec<Result<Answers, JudgeError>> = vec![
            Err(JudgeError::Unreachable("refused".into())),
            Err(JudgeError::Timeout),
            Err(JudgeError::Status(500)),
            Err(JudgeError::Malformed("junk".into())),
            Ok(noul_answers("some-other-question", 1.0, 1.0)),
        ];
        for failure in failures {
            let f = fixture();
            let (id, path) = f.baselined_judge_watch("w").await;
            f.env.judge.push(failure.clone());
            append_file(&path, "ERROR: x\n");
            f.next_tick(&id, 20).await;
            let file = f.load(&id);
            assert_eq!(file.status, WatchStatus::Pending, "{failure:?}");
            assert_eq!(file.judge_status, JudgeStatus::Down, "{failure:?}");
            assert_eq!(file.judge_strikes, 1, "{failure:?}");
            assert!(file.events.is_empty(), "{failure:?}");
            assert!(file.pending_change.is_some(), "change must be retained: {failure:?}");
        }
    }

    #[tokio::test]
    async fn a_failed_change_is_re_asked_next_tick_even_with_unchanged_inputs() {
        // why: SPEC "Failure rules": "Re-ask next tick with the same pending
        //      change, even if inputs did not change again". The second request
        //      carries the same state; a later success fires with the judge answer.
        // proves: CLAIM(judge-failure-keeps-change)
        let f = fixture();
        let (id, path) = f.baselined_judge_watch("w").await;
        f.env.judge.push(Err(JudgeError::Timeout));
        f.env.judge.push(Ok(noul_answers("failed", 0.9, 0.9)));
        append_file(&path, "ERROR: x\n");
        f.next_tick(&id, 20).await;
        assert_eq!(f.env.judge.call_count(), 1);

        f.next_tick(&id, 20).await; // file did not change again
        assert_eq!(f.env.judge.call_count(), 2, "pending change is re-asked");
        let requests = f.env.judge.requests();
        assert_eq!(requests[0].state, requests[1].state);
        let file = f.load(&id);
        assert_eq!(file.status, WatchStatus::Fired);
        assert_eq!(file.judge_status, JudgeStatus::Ok);
        assert_eq!(file.events[0].judge, Some(noul_answers("failed", 0.9, 0.9)));
        assert!(file.last_digest.contains("ERROR: x"));
    }

    #[tokio::test]
    async fn a_second_change_during_an_outage_is_merged_with_the_first_across_a_restart() {
        // why: R7 + S23: the old retry test changed nothing between attempts,
        //      so dropping the earlier content when a new change arrived still
        //      passed. Here: failure with change A (alpha, 200 lines = 3200
        //      bytes), failure with change B (beta, 100 lines = 1500 bytes),
        //      then a NEW process (another_process) recovers. Every request
        //      after B must carry BOTH changes, newest first (all beta before
        //      any alpha), cut at the budget: default limit 2560 - 53-byte
        //      prompt = 2507 bytes, so B (1500) fits whole and about 1000
        //      bytes of the newest alpha survive; alpha-line-0000 is cut. The
        //      recovery request is identical to the one before the restart,
        //      the answer fires, and the two failures' strikes are reset.
        // proves: CLAIM(judge-merge)
        let f = fixture();
        let (id, path) = f.baselined_judge_watch("w").await;
        f.env.judge.push(Err(JudgeError::Timeout));
        f.env.judge.push(Err(JudgeError::Status(500)));
        f.env.judge.push(Ok(noul_answers("failed", 0.95, 0.9)));

        append_file(&path, &numbered("alpha", 200));
        f.next_tick(&id, 20).await;
        append_file(&path, &numbered("beta", 100));
        f.next_tick(&id, 20).await;
        assert_eq!(f.load(&id).judge_strikes, 2);

        let second_process = f.env.another_process();
        f.clock.advance_secs(20);
        assert_eq!(second_process.tick_due(&id).await.unwrap(), TickOutcome::Ticked);

        let requests = f.env.judge.requests();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].state.contains("alpha-line-0199") && !requests[0].state.contains("beta"), "first failure carried only A");
        for (index, request) in requests.iter().enumerate().skip(1) {
            let state = &request.state;
            assert!(state.contains("beta-line-0099") && state.contains("beta-line-0000"), "request {index} lost B");
            assert!(state.contains("alpha-line-0199"), "request {index} lost the newest of A");
            assert!(!state.contains("alpha-line-0000"), "request {index} must truncate the oldest of A");
            assert!(state.find("beta-line-0000").unwrap() < state.find("alpha-line-0199").unwrap(), "request {index}: newest (B) first");
            assert!(state.len() <= 2560, "request {index}: {} bytes", state.len());
        }
        assert_eq!(requests[1].state, requests[2].state, "restart does not change what is asked");
        let file = f.load(&id);
        assert_eq!((file.status, file.judge_strikes), (WatchStatus::Fired, 0));
        assert_eq!(file.events[0].judge, Some(noul_answers("failed", 0.95, 0.9)));
    }

    #[tokio::test]
    async fn three_consecutive_failures_fire_with_the_raw_change_and_no_judge_field() {
        // why: SPEC "Failure rules": after 3 consecutive failures, fire with
        //      summary "judge unavailable; raw change attached" and judge
        //      absent. Acceptance: "a change still reaches the agent within
        //      three ticks and is labelled as unjudged". Exactly 3 calls.
        // proves: CLAIM(three-strikes)
        let f = fixture();
        let (id, path) = f.baselined_judge_watch("w").await;
        f.env.judge.set_fallback(Err(JudgeError::Unreachable("down".into())));
        append_file(&path, "ERROR: raw line\n");
        f.next_tick(&id, 20).await;
        f.next_tick(&id, 20).await;
        assert_eq!(f.load(&id).status, WatchStatus::Pending, "two strikes are not enough");
        f.next_tick(&id, 20).await;

        let file = f.load(&id);
        assert_eq!(file.status, WatchStatus::Fired);
        assert_eq!(f.env.judge.call_count(), 3);
        assert_eq!(file.events.len(), 1);
        assert_eq!(file.events[0].summary, "judge unavailable; raw change attached");
        assert!(file.events[0].judge.is_none());
        assert_eq!(file.events[0].tick, 4);
        assert_eq!(file.events[0].event_id, "w:4");
        assert!(file.last_digest.contains("ERROR: raw line"), "raw change attached");
        assert_eq!(f.env.vigil.status(&id).await.unwrap().envelope.judge_status, JudgeStatus::Down);
    }

    #[tokio::test]
    async fn a_successful_answer_resets_the_failure_count() {
        // why: SPEC says "3 consecutive failures"; fail, fail, success, then
        //      fail, fail must NOT fire (4 total failures but never 3 in a
        //      row); the third consecutive one does.
        // proves: CLAIM(three-strikes)
        let f = fixture();
        let (id, path) = f.baselined_judge_watch("w").await;
        let down = || Err(JudgeError::Timeout);
        f.env.judge.push(down());
        f.env.judge.push(down());
        f.env.judge.push(Ok(noul_answers("failed", 0.1, 0.9)));
        append_file(&path, "first\n");
        for _ in 0..3 {
            f.next_tick(&id, 20).await;
        }
        let file = f.load(&id);
        assert_eq!((file.status, file.judge_strikes, file.judge_status), (WatchStatus::Pending, 0, JudgeStatus::Ok));

        f.env.judge.push(down());
        f.env.judge.push(down());
        append_file(&path, "second\n");
        f.next_tick(&id, 20).await;
        f.next_tick(&id, 20).await;
        let file = f.load(&id);
        assert_eq!((file.status, file.judge_strikes), (WatchStatus::Pending, 2), "reset: only 2 in a row");

        f.env.judge.push(down());
        f.next_tick(&id, 20).await;
        assert_eq!(f.load(&id).status, WatchStatus::Fired);
    }

    #[tokio::test]
    async fn the_state_sent_to_the_judge_honours_the_configured_byte_limit() {
        // why: SPEC step 4 + V15: cut to VIGIL_JUDGE_STATE_BYTES. The limit is
        //      set to 4000 (above the 1536-byte observation floor so it binds,
        //      and above the 2560 default so a ignored setting is visible):
        //      6 KB of new log must arrive as more than 2560 and at most 4000
        //      bytes with the newest line kept. This also proves the Env's
        //      configuration reaches the Vigil (S22).
        let clock = Arc::new(ManualClock::new(spec_instant()));
        let mut config = default_config();
        config.judge_state_bytes = 4000;
        let env = Env::with_clock_and_config(clock.clone(), config);
        let path = env.input_path("big.log");
        write_file(&path, "old\n");
        let id = env.vigil.create(judge_request("w", &path, 20)).await.unwrap().watch_id;
        env.vigil.tick_due(&id).await.unwrap();
        env.judge.set_fallback(Ok(noul_answers("failed", 0.0, 1.0)));
        let big: String = (0..500).map(|i| format!("entry-{i:04}\n")).collect(); // 6000 bytes
        append_file(&path, &big);
        clock.advance_secs(20);
        env.vigil.tick_due(&id).await.unwrap();
        let state = &env.judge.requests()[0].state;
        assert!(state.len() <= 4000, "{} bytes", state.len());
        assert!(state.len() > 2560, "configured limit ignored: {} bytes", state.len());
        assert!(state.contains("entry-0499"));
    }

    // ---- confinement ----

    #[tokio::test]
    async fn a_full_lifecycle_writes_only_the_lock_and_json_files_under_the_state_root() {
        // why: SPEC non-goals/"State": nothing is written outside
        //      `$XDG_STATE_HOME/vigil/`, and no `.tmp` is left behind. V5: the
        //      only non-json file is the single `lock` in the root. Scans the
        //      whole tempdir (state root + input dir + a would-be HOME) after
        //      create, ticks, fire, cancel and prune. Positive control: the
        //      scan finds the watch file and detects a stray file we plant.
        // proves: CLAIM(state-confinement)
        let f = fixture();
        let home = f.env.dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let (fired, path) = f.baselined_change_watch("fired").await;
        append_file(&path, "x\n");
        f.next_tick(&fired, 20).await;
        let (cancelled, _) = f.baselined_change_watch("cancelled").await;
        f.env.vigil.cancel(&cancelled).await.unwrap();
        f.env.vigil.prune().unwrap();
        let input_snapshot = std::fs::read_to_string(&path).unwrap();

        let files = list_files(f.env.dir.path());
        let state = f.env.root();
        let inputs = f.env.dir.path().join("inputs");
        for file in &files {
            if file.starts_with(&inputs) {
                continue;
            }
            assert!(file.starts_with(&state), "stray file outside the state root: {}", file.display());
            let relative = file.strip_prefix(&state).unwrap().to_string_lossy().to_string();
            let allowed = relative == "lock" || (relative.starts_with("watches/") && relative.ends_with(".json"));
            assert!(allowed, "unexpected file under the state root: {relative}");
        }
        assert!(files.iter().any(|p| p.ends_with("watches/fired.json")), "control: scan sees the watch file");
        assert!(files.iter().any(|p| p == &state.join("lock")), "control: the single global lock file exists");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), input_snapshot, "watched input was not modified");
        assert!(list_files(&home).is_empty());

        write_file(&f.env.dir.path().join("stray.txt"), "x");
        assert!(list_files(f.env.dir.path()).iter().any(|p| p.ends_with("stray.txt")), "control: scan detects a stray file");
    }

    fn list_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.extend(list_files(&path));
            } else {
                out.push(path);
            }
        }
        out
    }
}
