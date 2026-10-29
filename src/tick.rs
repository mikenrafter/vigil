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
    /// tick is due immediately). Takes the exclusive watch lock and repeats the
    /// due check under it; rewrites the watch file atomically at the end.
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
        /// A fire=change watch on a fresh file containing "old\n"; baseline tick already run.
        async fn baselined_change_watch(&self, id: &str) -> (WatchId, std::path::PathBuf) {
            let path = self.env.input_path(&format!("{id}.log"));
            write_file(&path, "old\n");
            let watch = self.create(change_request(id, &path, 20)).await;
            assert_eq!(self.tick(&watch).await, TickOutcome::Ticked);
            (watch, path)
        }
        /// A fire=judge watch on a file containing "old\n"; baseline tick already run.
        async fn baselined_judge_watch(&self, id: &str) -> (WatchId, std::path::PathBuf) {
            let path = self.env.input_path(&format!("{id}.log"));
            write_file(&path, "old\n");
            let watch = self.create(judge_request(id, &path, 20)).await;
            assert_eq!(self.tick(&watch).await, TickOutcome::Ticked);
            (watch, path)
        }
    }

    // ---- cadence ----

    #[tokio::test]
    async fn the_first_tick_is_due_immediately_and_only_baselines() {
        // why: SPEC "watch_create": no tick runs until the first keepalive;
        //      then the first tick is due at once and, per the reader contract,
        //      is a baseline that cannot fire a fire=change watch.
        // proves: CLAIM(baseline-first-read)
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
        //      Event carries tick (2), watch id, input label and the changed
        //      line as summary (SPEC envelope example); the watch is finished
        //      (D1) and stops ticking.
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
        // why: a condition is a state test, not a change test (like mcp-await's
        //      wait-for-port): "wait until the build process has exited" must
        //      return at once if it already has. AMBIGUOUS in SPEC (tick step 2
        //      says unchanged inputs stop the tick); see Defaults taken.
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
        //      must still carry it at expiry.
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
        assert!(file.finished_at.is_some());
        assert_eq!(f.tick(&id).await, TickOutcome::Finished(WatchStatus::Expired), "stays expired");
    }

    #[tokio::test]
    async fn a_deadline_that_passed_while_nothing_ran_expires_on_the_next_call() {
        // why: SPEC "Failure rules": "a watch whose deadline passed while no
        //      keepalive ran reports expired on the next call" - the deadline
        //      is an absolute time in the file.
        // proves: CLAIM(absolute-deadline)
        let f = fixture();
        let path = f.env.input_path("w.log");
        write_file(&path, "old\n");
        let mut request = change_request("w", &path, 20);
        request.deadline_s = Some(100);
        let id = f.create(request).await;
        f.clock.advance_secs(100_000);
        assert_eq!(f.tick(&id).await, TickOutcome::Finished(WatchStatus::Expired));
        assert_eq!(f.load(&id).status, WatchStatus::Expired);
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

    // ---- concurrency ----

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_ticks_on_one_watch_produce_exactly_one_tick() {
        // why: SPEC "State": two sessions on one watch must not double-tick;
        //      the exclusive lock plus a due-time recheck under it make the
        //      losers return NotDue. 8 racers across 2 "processes", twice
        //      (positive control: after one interval a new race again yields
        //      exactly one tick, so the first win was not a fluke of nothing being due).
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
        // why: SPEC step 4: cut to VIGIL_JUDGE_STATE_BYTES (here 120). 2 KB of
        //      new log must arrive as <= 120 bytes with the newest line kept.
        let clock = Arc::new(ManualClock::new(spec_instant()));
        let mut config = default_config();
        config.judge_state_bytes = 120;
        let env = Env::with_clock_and_config(clock.clone(), config);
        let path = env.input_path("big.log");
        write_file(&path, "old\n");
        let id = env.vigil.create(judge_request("w", &path, 20)).await.unwrap().watch_id;
        env.vigil.tick_due(&id).await.unwrap();
        env.judge.set_fallback(Ok(noul_answers("failed", 0.0, 1.0)));
        let big: String = (0..200).map(|i| format!("entry-{i:04}\n")).collect();
        append_file(&path, &big);
        clock.advance_secs(20);
        env.vigil.tick_due(&id).await.unwrap();
        let state = &env.judge.requests()[0].state;
        assert!(state.len() <= 120, "{} bytes", state.len());
        assert!(state.contains("entry-0199"));
    }

    // ---- confinement ----

    #[tokio::test]
    async fn a_full_lifecycle_writes_only_json_and_lock_files_under_the_state_root() {
        // why: SPEC non-goals/"State": nothing is written outside
        //      `$XDG_STATE_HOME/vigil/`, and no `.tmp` is left behind. Scans the
        //      whole tempdir (state root + input dir + a would-be cwd/HOME)
        //      after create, ticks, fire, cancel and prune. Positive control:
        //      the scan does find the watch file, and it detects a stray file
        //      we plant ourselves.
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
            assert!(file.starts_with(state.join("watches")), "stray file outside the state root: {}", file.display());
            let name = file.file_name().unwrap().to_string_lossy().to_string();
            assert!(name.ends_with(".json") || name.ends_with(".lock"), "unexpected file {name}");
        }
        assert!(files.iter().any(|p| p.ends_with("watches/fired.json")), "control: scan sees the watch file");
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
