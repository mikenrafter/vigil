//! The blocking `keepalive` call and the `wait` variant.

use crate::envelope::Envelope;
use crate::error::VigilError;
use crate::watch::{Event, WatchId};

pub const MIN_MAX_S: u64 = 30;
pub const MAX_MAX_S: u64 = 270;
pub const DEFAULT_MAX_S: u64 = 270;

/// Clamp the agent's `max_s` to 30..=270 (absent = 270). Never rejects.
pub fn clamp_max_s(requested: Option<u64>) -> u64 {
    todo!()
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct KeepaliveRequest {
    #[serde(default)]
    pub watch_id: Option<WatchId>,
    #[serde(default)]
    pub max_s: Option<u64>,
}

/// Best-effort progress (SPEC delivery path 2). Called once per tick run
/// inside a blocking call, with the watch's new tick count.
pub trait ProgressSink: Send + Sync {
    fn progress(&self, tick: u64);
}

pub struct NoProgress;
impl ProgressSink for NoProgress {
    fn progress(&self, _tick: u64) {}
}

/// Receives each event exactly once, in order, as `wait --stream` sees it.
pub trait EventSink: Send + Sync {
    fn event(&self, event: &Event);
}

/// Process exit code of `vigil wait`: fired 0, expired 2, cancelled 3.
/// (1 is reserved for errors; a still-pending envelope has no code.)
pub fn wait_exit_code(envelope: &Envelope) -> Option<i32> {
    todo!()
}

impl crate::engine::Vigil {
    /// Block until a covered watch fires or expires, or `max_s` (clamped)
    /// elapses. SPEC "keepalive".
    pub async fn keepalive(&self, request: KeepaliveRequest) -> Result<Envelope, VigilError> {
        self.keepalive_with_progress(request, &NoProgress).await
    }

    pub async fn keepalive_with_progress(
        &self,
        request: KeepaliveRequest,
        progress: &dyn ProgressSink,
    ) -> Result<Envelope, VigilError> {
        todo!()
    }

    /// `vigil wait`: like keepalive for one watch but with no 270 s ceiling:
    /// returns only when the watch is finished. Events are passed to `sink`
    /// once each, in order, then the final envelope is returned.
    pub async fn wait(&self, id: &WatchId, sink: &dyn EventSink) -> Result<Envelope, VigilError> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Vigil;
    use crate::envelope::{Next, DIGEST_CUT_MARKER};
    use crate::testing::*;
    use crate::watch::{JudgeStatus, WatchStatus};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::time::Instant;

    // All tests here run on paused tokio time with `TokioClock`, so sleeping
    // 270 s of virtual time is instant and `now` moves with it. Only std::fs
    // inputs are used: sockets or spawn_blocking under auto-advance would let
    // virtual time race ahead of real I/O.

    fn paused_env() -> Env {
        Env::with_clock(Arc::new(TokioClock::new(spec_instant())))
    }

    fn paused_env_with(config: crate::config::Config) -> Env {
        Env::with_clock_and_config(Arc::new(TokioClock::new(spec_instant())), config)
    }

    fn process(env: &Env) -> Arc<Vigil> {
        Arc::new(env.another_process())
    }

    fn by_id(id: &crate::watch::WatchId, max_s: Option<u64>) -> KeepaliveRequest {
        KeepaliveRequest { watch_id: Some(id.clone()), max_s }
    }

    async fn quiet_watch(env: &Env, vigil: &Vigil, id: &str, interval_s: u64, deadline_s: u64) -> (crate::watch::WatchId, std::path::PathBuf) {
        let path = env.input_path(&format!("{id}.log"));
        write_file(&path, "old\n");
        let mut request = change_request(id, &path, interval_s);
        request.deadline_s = Some(deadline_s);
        (vigil.create(request).await.unwrap().watch_id, path)
    }

    // ---- clamp ----

    #[test]
    fn max_s_is_clamped_to_30_through_270_and_defaults_to_270() {
        // why: SPEC "Interval and long deadlines": clamped, not rejected: 30
        //      minimum, 270 maximum, default 270 (task examples: 5->30,
        //      9999->270, absent->270). Boundaries 29/30/31 and 269/270/271.
        // proves: CLAIM(max-s-clamp)
        let cases = [
            (None, 270), (Some(0), 30), (Some(5), 30), (Some(29), 30), (Some(30), 30), (Some(31), 31),
            (Some(100), 100), (Some(269), 269), (Some(270), 270), (Some(271), 270), (Some(9999), 270),
            (Some(u64::MAX), 270),
        ];
        for (requested, expected) in cases {
            assert_eq!(clamp_max_s(requested), expected, "requested {requested:?}");
        }
    }

    fn envelope_with(status: WatchStatus) -> Envelope {
        Envelope {
            status,
            now: spec_instant(),
            tick: 1,
            session_ticks: 1,
            elapsed_s: 0,
            deadline_in_s: 0,
            waited_s: None,
            max_s: None,
            events: vec![],
            digest: String::new(),
            judge_status: JudgeStatus::Skipped,
            next: Next::ActOnEvents,
        }
    }

    #[test]
    fn wait_exit_codes_are_0_fired_2_expired_3_cancelled() {
        // why: coordinator scope addition: `vigil wait` exits 0 fired, 2
        //      expired, 3 cancelled (1 is for errors, so never returned here);
        //      a still-pending envelope has no exit code.
        // proves: CLAIM(wait-exit-codes)
        assert_eq!(wait_exit_code(&envelope_with(WatchStatus::Fired)), Some(0));
        assert_eq!(wait_exit_code(&envelope_with(WatchStatus::Expired)), Some(2));
        assert_eq!(wait_exit_code(&envelope_with(WatchStatus::Cancelled)), Some(3));
        assert_eq!(wait_exit_code(&envelope_with(WatchStatus::Pending)), None);
    }

    // ---- blocking window ----

    #[tokio::test(start_paused = true)]
    async fn a_quiet_call_with_a_far_deadline_returns_pending_after_exactly_270_s() {
        // why: SPEC "Interval and long deadlines": a deadline hours away
        //      returns `pending` after at most 270 s; the pending envelope
        //      carries `tick` and `now`. Interval 20 -> ticks at 0,20,...,260 =
        //      14 ticks (the next, 280, is past the window). now = start+270 s;
        //      deadline_in_s = 7200-270 = 6930. Empty events/digest, "call
        //      keepalive again", D9: max_s reports the ceiling used.
        // proves: CLAIM(pending-at-270)
        let env = paused_env();
        let vigil = process(&env);
        quiet_watch(&env, &vigil, "w", 20, 7200).await;
        let started = Instant::now();
        let envelope = vigil.keepalive(KeepaliveRequest::default()).await.unwrap();

        assert_eq!(started.elapsed(), Duration::from_secs(270));
        assert_eq!(envelope.status, WatchStatus::Pending);
        assert_eq!(envelope.tick, 14);
        assert_eq!(envelope.now, spec_instant().plus_secs(270));
        assert_eq!((envelope.elapsed_s, envelope.deadline_in_s), (270, 6930));
        assert_eq!((envelope.waited_s, envelope.max_s), (Some(270), Some(270)));
        assert_eq!(envelope.session_ticks, 1);
        assert!(envelope.events.is_empty());
        assert_eq!(envelope.digest, "");
        assert_eq!(envelope.next, Next::CallKeepaliveAgain);
        assert_eq!(envelope.judge_status, JudgeStatus::Skipped);
        let wire = serde_json::to_string(&envelope).unwrap();
        assert!(wire.len() < 400, "quiet envelope should be a few hundred bytes, got {}: {wire}", wire.len());
    }

    #[tokio::test(start_paused = true)]
    async fn max_s_below_the_floor_and_above_the_ceiling_are_clamped_and_reported() {
        // why: SPEC: max_s 5 -> 30, 9999 -> 270, absent -> 270, "the envelope
        //      reports the value used" (D9 `max_s`), and the call really blocks
        //      that long (virtual time).
        // proves: CLAIM(max-s-clamp)
        for (requested, used) in [(Some(5), 30), (Some(9999), 270), (None, 270)] {
            let env = paused_env();
            let vigil = process(&env);
            let (id, _) = quiet_watch(&env, &vigil, "w", 20, 7200).await;
            let started = Instant::now();
            let envelope = vigil.keepalive(by_id(&id, requested)).await.unwrap();
            assert_eq!(started.elapsed(), Duration::from_secs(used), "requested {requested:?}");
            assert_eq!(envelope.max_s, Some(used), "requested {requested:?}");
            assert_eq!(envelope.waited_s, Some(used), "requested {requested:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocked_call_returns_early_when_the_watch_fires() {
        // why: SPEC "keepalive": "Blocks until any covered watch fires ... or
        //      max_s elapses". The file is written at t=45 s; ticks run at
        //      0,20,40,60, so the fire is seen at t=60 (tick 4): waited_s 60,
        //      far short of 270. The event and digest carry the line.
        // proves: CLAIM(early-return-on-fire)
        let env = paused_env();
        let vigil = process(&env);
        let (id, path) = quiet_watch(&env, &vigil, "build", 20, 3600).await;
        let started = Instant::now();
        let call = {
            let (vigil, id) = (vigil.clone(), id.clone());
            tokio::spawn(async move { vigil.keepalive(by_id(&id, None)).await.unwrap() })
        };
        tokio::time::sleep(Duration::from_secs(45)).await;
        append_file(&path, "ERROR: linking failed\n");
        let envelope = call.await.unwrap();

        assert_eq!(started.elapsed(), Duration::from_secs(60));
        assert_eq!(envelope.status, WatchStatus::Fired);
        assert_eq!((envelope.tick, envelope.waited_s, envelope.max_s), (4, Some(60), Some(270)));
        assert_eq!(envelope.next, Next::ActOnEvents);
        assert_eq!(envelope.events.len(), 1);
        assert_eq!(envelope.events[0].watch_id, "build");
        assert!(envelope.events[0].summary.contains("ERROR: linking failed"));
        assert!(envelope.digest.contains("ERROR: linking failed"));
    }

    #[tokio::test(start_paused = true)]
    async fn events_are_returned_once() {
        // why: SPEC "State": delivered_through = "highest event tick already
        //      returned, so each event is returned once". A second keepalive
        //      on the fired watch returns at once with no events.
        // proves: CLAIM(events-once)
        let env = paused_env();
        let vigil = process(&env);
        let (id, path) = quiet_watch(&env, &vigil, "w", 20, 3600).await;
        let call = {
            let (vigil, id) = (vigil.clone(), id.clone());
            tokio::spawn(async move { vigil.keepalive(by_id(&id, None)).await.unwrap() })
        };
        tokio::time::sleep(Duration::from_secs(5)).await;
        append_file(&path, "boom\n");
        let first = call.await.unwrap();
        assert_eq!(first.events.len(), 1);
        assert_eq!(env.vigil.store().load(&id).unwrap().delivered_through, first.events[0].tick);

        let started = Instant::now();
        let second = vigil.keepalive(by_id(&id, None)).await.unwrap();
        assert_eq!(started.elapsed(), Duration::ZERO, "a finished watch does not block");
        assert_eq!(second.status, WatchStatus::Fired);
        assert!(second.events.is_empty(), "already delivered");
    }

    #[tokio::test(start_paused = true)]
    async fn session_ticks_counts_keepalive_returns_per_process() {
        // why: SPEC "Envelope": session_ticks "counts keepalive returns for
        //      this vigil mcp process (in memory; it restarts at 0 with the
        //      process)". Two returns -> 1, 2; a new process starts again at 1.
        // proves: CLAIM(session-ticks)
        let env = paused_env();
        let first_process = process(&env);
        let (id, _) = quiet_watch(&env, &first_process, "w", 20, 7200).await;
        assert_eq!(first_process.keepalive(by_id(&id, Some(30))).await.unwrap().session_ticks, 1);
        assert_eq!(first_process.keepalive(by_id(&id, Some(30))).await.unwrap().session_ticks, 2);
        let restarted = process(&env);
        assert_eq!(restarted.keepalive(by_id(&id, Some(30))).await.unwrap().session_ticks, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn keepalive_without_an_id_covers_only_watches_this_process_created() {
        // why: D2 / SPEC "keepalive": no watch_id covers the pending watches
        //      this vigil mcp process created; another session's watches need
        //      their id. Control: process B's no-id call does tick B's watch.
        // proves: CLAIM(own-watches-only)
        let env = paused_env();
        let (a, b) = (process(&env), process(&env));
        let (mine, _) = quiet_watch(&env, &a, "mine", 20, 7200).await;
        let (theirs, _) = quiet_watch(&env, &b, "theirs", 20, 7200).await;
        a.keepalive(KeepaliveRequest { watch_id: None, max_s: Some(30) }).await.unwrap();
        let store = env.vigil.store();
        assert_eq!(store.load(&mine).unwrap().tick, 2, "ticks at 0 and 20 within 30 s");
        assert_eq!(store.load(&theirs).unwrap().tick, 0, "not covered by A");

        b.keepalive(KeepaliveRequest { watch_id: None, max_s: Some(30) }).await.unwrap();
        assert!(store.load(&theirs).unwrap().tick >= 2, "control: B covers its own watch");
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocked_call_wakes_at_the_deadline_and_reports_expired() {
        // why: SPEC "Deadline: checked ... at each keepalive entry and exit.
        //      Once passed, status expired". Interval 30 and deadline 100 s:
        //      ticks at 0,30,60,90 and the call must wake at 100 s (not 120,
        //      not 270) and return expired.
        // proves: CLAIM(deadline-expiry)
        let env = paused_env();
        let vigil = process(&env);
        let (id, _) = quiet_watch(&env, &vigil, "w", 30, 100).await;
        let started = Instant::now();
        let envelope = vigil.keepalive(by_id(&id, None)).await.unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(100));
        assert_eq!(envelope.status, WatchStatus::Expired);
        assert_eq!(envelope.waited_s, Some(100));
        assert_eq!(envelope.deadline_in_s, 0);
        assert_ne!(envelope.next, Next::CallKeepaliveAgain);
    }

    #[tokio::test(start_paused = true)]
    async fn a_deadline_that_passed_between_calls_expires_immediately() {
        // why: SPEC: "a watch whose deadline passed while no keepalive ran
        //      reports expired on the next call" - without waiting or ticking first.
        // proves: CLAIM(absolute-deadline)
        let env = paused_env();
        let vigil = process(&env);
        let (id, _) = quiet_watch(&env, &vigil, "w", 20, 100).await;
        tokio::time::advance(Duration::from_secs(10_000)).await;
        let started = Instant::now();
        let envelope = vigil.keepalive(by_id(&id, None)).await.unwrap();
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(envelope.status, WatchStatus::Expired);
        assert_eq!(envelope.waited_s, Some(0));
    }

    #[tokio::test(start_paused = true)]
    async fn cadence_holds_across_calls() {
        // why: D3 / SPEC "keepalive": ticks fall at last_tick_at + interval_s
        //      whichever call is running; an immediate re-call adds no extra
        //      tick. Interval 25 -> ticks at 25k s. Calls cover [0,30], [30,60],
        //      [60,90]: cumulative ticks 2 (t=0,25), 3 (t=50), 4 (t=75). A
        //      re-call that ticked again at 30 or 60 would show 3 or 4 too early.
        // proves: CLAIM(cadence)
        let env = paused_env();
        let vigil = process(&env);
        let (id, _) = quiet_watch(&env, &vigil, "w", 25, 7200).await;
        let mut ticks = Vec::new();
        for _ in 0..3 {
            ticks.push(vigil.keepalive(by_id(&id, Some(30))).await.unwrap().tick);
        }
        assert_eq!(ticks, vec![2, 3, 4]);
    }

    #[derive(Default)]
    struct RecordingProgress(Mutex<Vec<u64>>);
    impl ProgressSink for RecordingProgress {
        fn progress(&self, tick: u64) {
            self.0.lock().unwrap().push(tick);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn progress_is_reported_with_the_tick_count_once_per_tick() {
        // why: SPEC delivery path 2: while blocked, emit progress with the
        //      tick count. Interval 20 over a 270 s window -> ticks 1..=14, in
        //      order (MCP progress must increase).
        // proves: CLAIM(progress-ticks)
        let env = paused_env();
        let vigil = process(&env);
        let (id, _) = quiet_watch(&env, &vigil, "w", 20, 7200).await;
        let sink = RecordingProgress::default();
        vigil.keepalive_with_progress(by_id(&id, None), &sink).await.unwrap();
        assert_eq!(*sink.0.lock().unwrap(), (1..=14).collect::<Vec<u64>>());
    }

    #[tokio::test(start_paused = true)]
    async fn an_over_long_digest_is_cut_and_says_so_while_a_short_one_is_not() {
        // why: SPEC "Envelope": digest bounded by VIGIL_DIGEST_MAX_BYTES
        //      (here 64); "If cut, say so." The newest line survives. Control:
        //      a short digest in the same config carries no marker.
        // proves: CLAIM(digest-cut)
        let mut config = default_config();
        config.digest_max_bytes = 64;
        let env = paused_env_with(config);
        let vigil = process(&env);
        let (big, big_path) = quiet_watch(&env, &vigil, "big", 20, 3600).await;
        let (small, small_path) = quiet_watch(&env, &vigil, "small", 20, 3600).await;
        let call = {
            let (vigil, big) = (vigil.clone(), big.clone());
            tokio::spawn(async move { vigil.keepalive(by_id(&big, None)).await.unwrap() })
        };
        tokio::time::sleep(Duration::from_secs(5)).await;
        let lines: String = (0..100).map(|i| format!("noise line {i:03}\n")).collect();
        append_file(&big_path, &format!("{lines}LAST-LINE\n"));
        let cut = call.await.unwrap();
        assert!(cut.digest.contains(DIGEST_CUT_MARKER), "{:?}", cut.digest);
        assert!(cut.digest.contains("LAST-LINE"));
        assert!(cut.digest.len() <= 64 + DIGEST_CUT_MARKER.len() + 1, "{} bytes", cut.digest.len());

        let call = {
            let (vigil, small) = (vigil.clone(), small.clone());
            tokio::spawn(async move { vigil.keepalive(by_id(&small, None)).await.unwrap() })
        };
        tokio::time::sleep(Duration::from_secs(5)).await;
        append_file(&small_path, "tiny\n");
        let whole = call.await.unwrap();
        assert!(whole.digest.contains("tiny"));
        assert!(!whole.digest.contains(DIGEST_CUT_MARKER));
    }

    // ---- wait ----

    #[derive(Default)]
    struct CollectingSink(Mutex<Vec<crate::watch::Event>>);
    impl EventSink for CollectingSink {
        fn event(&self, event: &crate::watch::Event) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn wait_has_no_270_s_ceiling_and_returns_the_fired_envelope() {
        // why: coordinator scope addition: `vigil wait` blocks with no ceiling
        //      until the watch fires. The file is written at t=995 s, so with
        //      20 s ticks the fire is seen at t=1000 s - well past 270.
        // proves: CLAIM(wait-no-ceiling)
        let env = paused_env();
        let vigil = process(&env);
        let (id, path) = quiet_watch(&env, &vigil, "w", 20, 7200).await;
        let call = {
            let (vigil, id) = (vigil.clone(), id.clone());
            tokio::spawn(async move { vigil.wait(&id, &CollectingSink::default()).await.unwrap() })
        };
        tokio::time::sleep(Duration::from_secs(995)).await;
        append_file(&path, "done\n");
        let envelope = call.await.unwrap();
        assert_eq!(envelope.status, WatchStatus::Fired);
        assert_eq!(envelope.elapsed_s, 1000);
        assert_eq!(envelope.events.len(), 1);
        assert_eq!(wait_exit_code(&envelope), Some(0));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_returns_expired_at_the_deadline() {
        // why: coordinator scope addition: wait ends on expiry too (exit 2).
        //      Deadline 500 s, interval 20.
        let env = paused_env();
        let vigil = process(&env);
        let (id, _) = quiet_watch(&env, &vigil, "w", 20, 500).await;
        let started = Instant::now();
        let envelope = vigil.wait(&id, &CollectingSink::default()).await.unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(500));
        assert_eq!(envelope.status, WatchStatus::Expired);
        assert_eq!(wait_exit_code(&envelope), Some(2));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_returns_cancelled_within_one_interval_of_the_cancel() {
        // why: coordinator scope addition: wait ends when the watch is
        //      cancelled (exit 3), noticed on a tick: cancel at t=100 s with
        //      20 s ticks is seen no later than t=120 s.
        let env = paused_env();
        let vigil = process(&env);
        let (id, _) = quiet_watch(&env, &vigil, "w", 20, 7200).await;
        let call = {
            let (vigil, id) = (vigil.clone(), id.clone());
            tokio::spawn(async move { vigil.wait(&id, &CollectingSink::default()).await.unwrap() })
        };
        tokio::time::sleep(Duration::from_secs(100)).await;
        env.another_process().cancel(&id).await.unwrap();
        let envelope = call.await.unwrap();
        assert_eq!(envelope.status, WatchStatus::Cancelled);
        assert!((100..=120).contains(&envelope.elapsed_s), "elapsed {}", envelope.elapsed_s);
        assert_eq!(wait_exit_code(&envelope), Some(3));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_stream_emits_each_event_once_in_order_before_returning() {
        // why: coordinator scope addition: --stream prints each event once, as
        //      it happens, in order. Two inputs change in the same tick, so two
        //      events must reach the sink, in input order, with no duplicates.
        // proves: CLAIM(wait-stream-once)
        let env = paused_env();
        let vigil = process(&env);
        let first = env.input_path("first.log");
        let second = env.input_path("second.log");
        write_file(&first, "a\n");
        write_file(&second, "b\n");
        let mut request = change_request("w", &first, 20);
        request.inputs.push(crate::watch::Input::File { path: second.clone(), tail_bytes: None });
        let id = vigil.create(request).await.unwrap().watch_id;
        let sink = Arc::new(CollectingSink::default());
        let call = {
            let (vigil, id, sink) = (vigil.clone(), id.clone(), sink.clone());
            tokio::spawn(async move { vigil.wait(&id, sink.as_ref()).await.unwrap() })
        };
        tokio::time::sleep(Duration::from_secs(5)).await;
        append_file(&first, "first-new\n");
        append_file(&second, "second-new\n");
        let envelope = call.await.unwrap();
        assert_eq!(envelope.status, WatchStatus::Fired);
        let streamed = sink.0.lock().unwrap().clone();
        assert_eq!(streamed.len(), 2, "each event exactly once");
        assert!(streamed[0].summary.contains("first-new"), "input order: {streamed:?}");
        assert!(streamed[1].summary.contains("second-new"));
        assert_eq!(streamed, env.vigil.store().load(&id).unwrap().events);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_on_a_finished_watch_returns_at_once_and_unknown_ids_error() {
        // why: coordinator scope: same watch files as keepalive. A watch that
        //      already fired must not block `wait`; a typo is an error (exit 1).
        let env = paused_env();
        let vigil = process(&env);
        let (id, _) = quiet_watch(&env, &vigil, "w", 20, 3600).await;
        vigil.cancel(&id).await.unwrap();
        let started = Instant::now();
        let envelope = vigil.wait(&id, &CollectingSink::default()).await.unwrap();
        assert_eq!((envelope.status, started.elapsed()), (WatchStatus::Cancelled, Duration::ZERO));
        assert_eq!(
            vigil.wait(&crate::testing::wid("ghost"), &CollectingSink::default()).await.unwrap_err(),
            VigilError::NotFound("ghost".into())
        );
    }
}
