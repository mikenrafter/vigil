//! The blocking `keepalive` call and the `wait` variant.

use crate::envelope::Envelope;
use crate::error::VigilError;
use crate::watch::WatchId;

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

/// One best-effort progress notification (SPEC delivery path 2, V18).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressUpdate {
    /// Call-local counter: starts fresh for every `keepalive` call and only
    /// increases (+1 per tick run in the call, across all covered watches).
    /// This is what goes in the MCP `progress` field.
    pub progress: u64,
    /// The watch that just ticked and its per-watch tick count: these go in
    /// the MCP `message`, never in `progress` (counts of different watches
    /// would arrive as 10, 1, 11, 2).
    pub watch_id: WatchId,
    pub tick: u64,
}

/// Best-effort progress. Called once per tick run inside a blocking call.
pub trait ProgressSink: Send + Sync {
    fn progress(&self, update: &ProgressUpdate);
}

pub struct NoProgress;
impl ProgressSink for NoProgress {
    fn progress(&self, _update: &ProgressUpdate) {}
}

/// Process exit code of `vigil wait`: fired 0, expired 2, cancelled 3.
/// (1 is reserved for errors; a still-pending envelope has no code.)
pub fn wait_exit_code(envelope: &Envelope) -> Option<i32> {
    todo!()
}

impl crate::engine::Vigil {
    /// Block until a covered watch fires or expires, or `max_s` (clamped)
    /// elapses. SPEC "keepalive" + V2/V7/V8/V14/V26.
    ///
    /// Covered: with an id, that watch; with none, this process's pending
    /// watches plus its finished watches whose result it has not yet returned
    /// (V7). V2: the whole call, lock waits and input/judge I/O included, is
    /// bounded by `max_s` on a monotonic clock, and a tick that cannot finish
    /// inside the budget writes nothing. Envelope: `watch_id` and all watch
    /// fields describe the earliest-finished covered watch, else the one with
    /// the nearest deadline, ties by lowest id (V8); `events` holds the
    /// terminal events of all covered finished watches; a pending envelope's
    /// `digest` is only what changed during THIS call (V14).
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
    /// returns only when the watch is finished, with ONE final envelope that
    /// holds the whole event batch (no streaming, R8/V21).
    pub async fn wait(&self, id: &WatchId) -> Result<Envelope, VigilError> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Vigil;
    use crate::envelope::{Next, DIGEST_CUT_MARKER};
    use crate::judge::JudgeError;
    use crate::testing::*;
    use crate::watch::{Input, JudgeStatus, WatchStatus};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::time::Instant;

    // Most tests here run on paused tokio time with `TokioClock`, so sleeping
    // 270 s of virtual time is instant and `now` moves with it. Only std::fs
    // inputs and the in-memory ScriptedJudge are used under auto-advance:
    // sockets or child processes there would let virtual time race ahead of
    // real I/O. The V2 tests that need a real child or socket start UNPAUSED
    // (create + baseline in real time) and call `tokio::time::pause()` just
    // before the budgeted call.

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

    fn no_id(max_s: Option<u64>) -> KeepaliveRequest {
        KeepaliveRequest { watch_id: None, max_s }
    }

    async fn quiet_watch(env: &Env, vigil: &Vigil, id: &str, interval_s: u64, deadline_s: u64) -> (crate::watch::WatchId, std::path::PathBuf) {
        let path = env.input_path(&format!("{id}.log"));
        write_file(&path, "old\n");
        let mut request = change_request(id, &path, interval_s);
        request.deadline_s = Some(deadline_s);
        (vigil.create(request).await.unwrap().watch_id, path)
    }

    fn numbered(prefix: &str, n: usize) -> String {
        (0..n).map(|i| format!("{prefix}-line-{i:04}\n")).collect()
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
            watch_id: wid("w"),
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

    // ---- blocking window (V26) ----

    #[tokio::test(start_paused = true)]
    async fn a_quiet_call_with_a_far_deadline_returns_pending_after_exactly_270_s() {
        // why: SPEC "Interval and long deadlines": a deadline hours away
        //      returns `pending` after at most 270 s; the pending envelope
        //      carries `tick` and `now`. Interval 20 -> ticks at 0,20,...,260 =
        //      14 ticks (the next, 280, is past the window). now = start+270 s;
        //      deadline_in_s = 7200-270 = 6930. Empty events/digest, V20: "call
        //      keepalive again", V26: max_s reports the ceiling used and
        //      waited_s the time really waited, V8: watch_id.
        // proves: CLAIM(pending-at-270)
        let env = paused_env();
        let vigil = process(&env);
        quiet_watch(&env, &vigil, "w", 20, 7200).await;
        let started = Instant::now();
        let envelope = vigil.keepalive(KeepaliveRequest::default()).await.unwrap();

        assert_eq!(started.elapsed(), Duration::from_secs(270));
        assert_eq!(envelope.watch_id, wid("w"));
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
        //      reports the value used" (V26 `max_s`), and the call really
        //      blocks that long (virtual time).
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
    async fn a_blocked_call_returns_early_when_the_watch_fires_and_waited_s_is_the_real_wait() {
        // why: SPEC "keepalive": "Blocks until any covered watch fires ... or
        //      max_s elapses". The file is written at t=45 s; ticks run at
        //      0,20,40,60, so the fire is seen at t=60 (tick 4). V26: waited_s
        //      is the time actually waited (60), distinct from max_s (270).
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
        assert_eq!(envelope.events[0].event_id, "build:4");
        assert!(envelope.events[0].summary.contains("ERROR: linking failed"));
        assert!(envelope.digest.contains("ERROR: linking failed"));
    }

    // ---- V3: terminal result replay ----

    #[tokio::test(start_paused = true)]
    async fn a_terminal_result_is_replayed_unchanged_by_every_later_call_with_the_same_event_ids() {
        // why: V3 (S3): the terminal result is stored once and replayed on
        //      every later keepalive, wait or watch_status; delivery is at
        //      least once, so a lost reply is recoverable. Fire at tick 2,
        //      then: keepalive (same process), keepalive (new process, later),
        //      wait, status. Every one returns the identical events (incl.
        //      event_id "w:2"), digest, status, tick and next, instantly.
        //      The old `events_are_returned_once` demanded EMPTY events on the
        //      second call, which loses the event if the first reply was lost.
        // proves: CLAIM(terminal-replay)
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
        assert_eq!((first.status, first.events.len(), first.events[0].event_id.as_str()), (WatchStatus::Fired, 1, "w:2"));

        tokio::time::advance(Duration::from_secs(1000)).await;
        let restarted = process(&env);
        let started = Instant::now();
        let replays = [
            vigil.keepalive(by_id(&id, None)).await.unwrap(),
            restarted.keepalive(by_id(&id, Some(30))).await.unwrap(),
            restarted.wait(&id).await.unwrap(),
            restarted.status(&id).await.unwrap().envelope,
        ];
        assert_eq!(started.elapsed(), Duration::ZERO, "a finished watch never blocks");
        for (index, replay) in replays.iter().enumerate() {
            assert_eq!(replay.status, WatchStatus::Fired, "replay {index}");
            assert_eq!(replay.events, first.events, "replay {index}: identical events and event_id");
            assert_eq!(replay.digest, first.digest, "replay {index}");
            assert_eq!((replay.tick, replay.next, &replay.watch_id), (first.tick, first.next, &first.watch_id), "replay {index}");
        }
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

    // ---- coverage: D2, V7, V8 ----

    #[tokio::test(start_paused = true)]
    async fn keepalive_without_an_id_covers_only_watches_this_process_created() {
        // why: D2 / SPEC "keepalive": no watch_id covers the watches this
        //      vigil mcp process created; another session's watches need
        //      their id. Control: process B's no-id call does tick B's watch.
        // proves: CLAIM(own-watches-only)
        let env = paused_env();
        let (a, b) = (process(&env), process(&env));
        let (mine, _) = quiet_watch(&env, &a, "mine", 20, 7200).await;
        let (theirs, _) = quiet_watch(&env, &b, "theirs", 20, 7200).await;
        a.keepalive(no_id(Some(30))).await.unwrap();
        let store = env.vigil.store();
        assert_eq!(store.load(&mine).unwrap().tick, 2, "ticks at 0 and 20 within 30 s");
        assert_eq!(store.load(&theirs).unwrap().tick, 0, "not covered by A");

        b.keepalive(no_id(Some(30))).await.unwrap();
        assert!(store.load(&theirs).unwrap().tick >= 2, "control: B covers its own watch");
    }

    #[tokio::test(start_paused = true)]
    async fn no_id_keepalive_returns_the_result_of_an_own_watch_cancelled_by_another_process() {
        // why: V7 (S7): the owner skipped terminal results because no-id
        //      selection looked only at pending watches. A's watch is
        //      cancelled by B between A's calls: A's next no-id call returns at
        //      once with the cancelled result (not "no watches", not a 30 s
        //      wait). Once returned it is not covered again. Control: B's own
        //      cancelled watch is never A's business (D2).
        // proves: CLAIM(own-finished-unreturned)
        let env = paused_env();
        let (a, b) = (process(&env), process(&env));
        let (mine, _) = quiet_watch(&env, &a, "mine", 20, 7200).await;
        let (theirs, _) = quiet_watch(&env, &b, "theirs", 20, 7200).await;
        b.cancel(&mine).await.unwrap();
        b.cancel(&theirs).await.unwrap();

        let started = Instant::now();
        let envelope = a.keepalive(no_id(None)).await.unwrap();
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!((envelope.watch_id.as_str(), envelope.status, envelope.next), ("mine", WatchStatus::Cancelled, Next::WatchEnded));
        assert_eq!(a.keepalive(no_id(None)).await.unwrap_err(), VigilError::NothingToCover, "already returned: nothing left");
    }

    #[tokio::test(start_paused = true)]
    async fn no_id_keepalive_returns_the_events_of_an_own_watch_fired_by_another_process() {
        // why: V7 (S7): same gap with a FIRE: B ticks A's watch past its
        //      change; A's next no-id call returns the fired result with the
        //      event instead of skipping it. After A has returned it, a
        //      restarted process (which never created the watch) covers
        //      nothing: no-id coverage is per creating process (D2, "an
        //      in-memory set, so a restart may replay once" - by id).
        // proves: CLAIM(own-finished-unreturned)
        let env = paused_env();
        let (a, b) = (process(&env), process(&env));
        let (mine, path) = quiet_watch(&env, &a, "mine", 20, 7200).await;
        append_file(&path, "ERROR: while A was away\n");
        assert_eq!(b.tick_due(&mine).await.unwrap(), crate::tick::TickOutcome::Ticked);

        let envelope = a.keepalive(no_id(None)).await.unwrap();
        assert_eq!((envelope.status, envelope.next), (WatchStatus::Fired, Next::ActOnEvents));
        assert_eq!(envelope.events.len(), 1);
        assert!(envelope.events[0].summary.contains("while A was away"));
        assert_eq!(a.keepalive(no_id(None)).await.unwrap_err(), VigilError::NothingToCover);
        assert_eq!(process(&env).keepalive(no_id(None)).await.unwrap_err(), VigilError::NothingToCover);
    }

    #[tokio::test(start_paused = true)]
    async fn a_watch_that_fired_inside_this_call_is_not_covered_again_by_the_next_no_id_call() {
        // why: V7: "whose terminal result this process has not yet returned".
        //      A fire returned by the call itself marks it returned; the next
        //      no-id call is NothingToCover (R4), while an explicit id still
        //      replays it (V3). Together they fix the contract: by-id = at
        //      least once, no-id = once per process.
        // proves: CLAIM(own-finished-unreturned)
        let env = paused_env();
        let vigil = process(&env);
        let (id, path) = quiet_watch(&env, &vigil, "w", 20, 3600).await;
        append_file(&path, "boom\n");
        let first = vigil.keepalive(no_id(None)).await.unwrap();
        assert_eq!(first.status, WatchStatus::Fired);
        assert_eq!(vigil.keepalive(no_id(None)).await.unwrap_err(), VigilError::NothingToCover);
        assert_eq!(vigil.keepalive(by_id(&id, None)).await.unwrap().events, first.events);
    }

    #[tokio::test(start_paused = true)]
    async fn the_envelope_describes_the_covered_watch_with_the_nearest_deadline_ties_by_lowest_id() {
        // why: V8 (S8): `watch_id` names the primary watch and every watch
        //      field (tick, elapsed_s, deadline_in_s) describes THAT watch.
        //      Covered, pending: "far" (7200 s) and "near" (3600 s):
        //      near wins, deadline_in_s = 3600 - 90 (see the arithmetic below). Then two watches with the
        //      same deadline: "a-tie" beats "b-tie" although "b-tie" was
        //      created first. Tick counts differ per watch (near: 5, far: 2)
        //      so a mixed-up envelope would show the wrong one.
        // proves: CLAIM(envelope-primary-watch)
        let env = paused_env();
        let vigil = process(&env);
        quiet_watch(&env, &vigil, "far", 20, 7200).await;
        let (near, _) = quiet_watch(&env, &vigil, "near", 20, 3600).await;
        for _ in 0..3 {
            vigil.tick_due(&near).await.unwrap();
            tokio::time::advance(Duration::from_secs(20)).await;
        }
        let envelope = vigil.keepalive(no_id(Some(30))).await.unwrap();
        let stored = env.vigil.store().load(&near).unwrap();
        assert_eq!(envelope.watch_id, near);
        assert_eq!(envelope.deadline_in_s, 3600 - 90, "60 s of pre-ticking + 30 s call");
        assert_eq!(envelope.elapsed_s, 90);
        assert_eq!(envelope.tick, stored.tick);
        assert_eq!(stored.tick, 5, "near: 3 earlier ticks + ticks at 0 and 20 of the call");
        assert_eq!(env.vigil.store().load(&wid("far")).unwrap().tick, 2, "far ticked less: a mixed-up envelope would show 2");

        let tie_env = paused_env();
        let tie = process(&tie_env);
        quiet_watch(&tie_env, &tie, "b-tie", 20, 3600).await;
        quiet_watch(&tie_env, &tie, "a-tie", 20, 3600).await;
        let tied = tie.keepalive(no_id(Some(30))).await.unwrap();
        assert_eq!(tied.watch_id.as_str(), "a-tie");
    }

    #[tokio::test(start_paused = true)]
    async fn when_several_covered_watches_finished_the_earliest_finished_one_is_the_primary() {
        // why: V8 (S8): "the earliest-finished covered watch if any finished,
        //      else the nearest deadline". Two own watches are fired by
        //      another process, "z-first" at t, "a-second" 5 s later; the id
        //      order and the deadlines would both pick "a-second", only
        //      finished_at picks "z-first". Events of BOTH watches arrive in
        //      the one envelope (R4, kept); both count as returned afterwards.
        // proves: CLAIM(envelope-primary-watch)
        let env = paused_env();
        let (a, b) = (process(&env), process(&env));
        let (z, z_path) = quiet_watch(&env, &a, "z-first", 20, 3600).await;
        let (second, second_path) = quiet_watch(&env, &a, "a-second", 20, 3600).await;
        append_file(&z_path, "z fired\n");
        append_file(&second_path, "a fired\n");
        b.tick_due(&z).await.unwrap();
        tokio::time::advance(Duration::from_secs(5)).await;
        b.tick_due(&second).await.unwrap();

        let envelope = a.keepalive(no_id(None)).await.unwrap();
        assert_eq!(envelope.watch_id, z);
        assert_eq!((envelope.status, envelope.next), (WatchStatus::Fired, Next::ActOnEvents));
        let mut ids: Vec<&str> = envelope.events.iter().map(|e| e.event_id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["a-second:1", "z-first:1"]);
        assert_eq!(a.keepalive(no_id(None)).await.unwrap_err(), VigilError::NothingToCover, "both results were returned");
    }

    // ---- deadlines ----

    #[tokio::test(start_paused = true)]
    async fn a_blocked_call_wakes_at_the_deadline_and_reports_expired() {
        // why: SPEC "Deadline: checked ... at each keepalive entry and exit.
        //      Once passed, status expired". Interval 30 and deadline 100 s:
        //      ticks at 0,30,60,90 and the call must wake at 100 s (not 120,
        //      not 270) and return expired with the V20 terminal instruction.
        // proves: CLAIM(deadline-expiry)
        let env = paused_env();
        let vigil = process(&env);
        let (id, _) = quiet_watch(&env, &vigil, "w", 30, 100).await;
        let started = Instant::now();
        let envelope = vigil.keepalive(by_id(&id, None)).await.unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(100));
        assert_eq!(envelope.status, WatchStatus::Expired);
        assert_eq!((envelope.waited_s, envelope.max_s), (Some(100), Some(270)));
        assert_eq!(envelope.deadline_in_s, 0);
        assert_eq!(envelope.next, Next::WatchEnded);
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
        assert_eq!(envelope.next, Next::WatchEnded);
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

    // ---- V18: progress ----

    #[derive(Default)]
    struct RecordingProgress(Mutex<Vec<ProgressUpdate>>);
    impl ProgressSink for RecordingProgress {
        fn progress(&self, update: &ProgressUpdate) {
            self.0.lock().unwrap().push(update.clone());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn progress_is_a_call_local_counter_that_only_increases_across_watches_with_different_histories() {
        // why: V18 (S18): per-watch tick counts as MCP progress can arrive as
        //      11, 1, 12, 2 (not allowed to go down). Watch "x" is ticked 10
        //      times before the call (interval 20), "y" never; the call (30 s)
        //      then ticks x at 0 and 20 (counts 11, 12) and y at 0 and 25
        //      (counts 1, 2). The `progress` values must strictly increase
        //      (1,2,3,4 from a fresh counter) while the per-watch counts ride
        //      along. Precondition asserted: the raw tick counts alone DO go
        //      down (11 then 1), so a tick-count implementation fails.
        // proves: CLAIM(progress-counter)
        let env = paused_env();
        let vigil = process(&env);
        let (x, _) = quiet_watch(&env, &vigil, "x", 20, 7200).await;
        quiet_watch(&env, &vigil, "y", 25, 7200).await;
        for _ in 0..10 {
            vigil.tick_due(&x).await.unwrap();
            tokio::time::advance(Duration::from_secs(20)).await;
        }
        assert_eq!(env.vigil.store().load(&x).unwrap().tick, 10);

        let sink = RecordingProgress::default();
        vigil.keepalive_with_progress(no_id(Some(30)), &sink).await.unwrap();
        let updates = sink.0.lock().unwrap().clone();
        let counters: Vec<u64> = updates.iter().map(|u| u.progress).collect();
        let ticks: Vec<(&str, u64)> = updates.iter().map(|u| (u.watch_id.as_str(), u.tick)).collect();
        assert_eq!(updates.len(), 4, "{ticks:?}");
        assert!(counters.windows(2).all(|w| w[0] < w[1]), "progress must strictly increase: {counters:?}");
        assert!(updates.iter().map(|u| u.tick).collect::<Vec<_>>().windows(2).any(|w| w[0] > w[1]), "precondition: raw tick counts go down: {ticks:?}");
        for expected in [("x", 11), ("x", 12), ("y", 1), ("y", 2)] {
            assert!(ticks.contains(&expected), "{expected:?} missing from {ticks:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_progress_counter_restarts_with_every_call() {
        // why: V18: "call-local": each keepalive call starts its own counter,
        //      so a second call's first update is the same number as the
        //      first call's (MCP progress is per request token).
        // proves: CLAIM(progress-counter)
        let env = paused_env();
        let vigil = process(&env);
        let (id, _) = quiet_watch(&env, &vigil, "w", 20, 7200).await;
        let (one, two) = (RecordingProgress::default(), RecordingProgress::default());
        vigil.keepalive_with_progress(by_id(&id, Some(30)), &one).await.unwrap();
        vigil.keepalive_with_progress(by_id(&id, Some(30)), &two).await.unwrap();
        let first_of = |sink: &RecordingProgress| sink.0.lock().unwrap().first().map(|u| u.progress);
        assert!(first_of(&one).is_some() && first_of(&two).is_some(), "both calls tick at least once");
        assert_eq!(first_of(&one), first_of(&two));
    }

    // ---- V14 / V27: digests ----

    fn judged_no_fire() -> crate::config::Config {
        default_config()
    }

    #[tokio::test(start_paused = true)]
    async fn a_pending_digest_accumulates_every_change_of_the_call_and_quiet_returns_do_not_repeat_it() {
        // why: V14 (S14): a pending envelope's digest is the content that
        //      changed during THIS call, accumulated in memory. Interval 12,
        //      window 30: ticks at 0, 12, 24. "first-change" is written at
        //      t=5 (judged at 12), "second-change" at t=18 (judged at 24);
        //      both answered below the threshold, so nothing fires. The
        //      return at t=30 carries BOTH, oldest first. The next quiet call
        //      must carry an empty digest: "quiet returns never repeat earlier
        //      content". Two judge calls prove both changes were really seen.
        // proves: CLAIM(digest-accumulate)
        let env = paused_env_with(judged_no_fire());
        let vigil = process(&env);
        let path = env.input_path("j.log");
        write_file(&path, "old\n");
        let id = vigil.create(judge_request("j", &path, 12)).await.unwrap().watch_id;
        env.judge.set_fallback(Ok(noul_answers("failed", 0.1, 0.9)));
        let call = {
            let (vigil, id) = (vigil.clone(), id.clone());
            tokio::spawn(async move { vigil.keepalive(by_id(&id, Some(30))).await.unwrap() })
        };
        tokio::time::sleep(Duration::from_secs(5)).await;
        append_file(&path, "first-change\n");
        tokio::time::sleep(Duration::from_secs(13)).await;
        append_file(&path, "second-change\n");
        let busy = call.await.unwrap();

        assert_eq!(env.judge.call_count(), 2);
        assert_eq!((busy.status, busy.next), (WatchStatus::Pending, Next::CallKeepaliveAgain));
        assert!(busy.events.is_empty());
        let (first, second) = (busy.digest.find("first-change"), busy.digest.find("second-change"));
        assert!(first.is_some() && second.is_some(), "both changes in the digest: {:?}", busy.digest);
        assert!(first < second, "oldest first: {:?}", busy.digest);

        let quiet = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!(quiet.status, WatchStatus::Pending);
        assert_eq!(quiet.digest, "", "a quiet return must not repeat earlier content");
    }

    #[tokio::test(start_paused = true)]
    async fn a_pending_digest_is_cut_to_the_configured_bound_marker_included() {
        // why: V14 + V27 (S27): the accumulated pending digest is cut to
        //      VIGIL_DIGEST_MAX_BYTES (64) INCLUDING the marker, newest end
        //      kept. 100 noise lines + LAST-LINE are written before the call.
        //      This also proves the config reaches the second process (S22).
        // proves: CLAIM(digest-cut)
        let mut config = default_config();
        config.digest_max_bytes = 64;
        let env = paused_env_with(config);
        let vigil = process(&env);
        let path = env.input_path("j.log");
        write_file(&path, "old\n");
        let id = vigil.create(judge_request("j", &path, 20)).await.unwrap().watch_id;
        env.judge.set_fallback(Ok(noul_answers("failed", 0.1, 0.9)));
        append_file(&path, &format!("{}LAST-LINE\n", numbered("noise", 100)));
        let envelope = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!(envelope.status, WatchStatus::Pending);
        assert!(envelope.digest.len() <= 64, "{} bytes: {:?}", envelope.digest.len(), envelope.digest);
        assert!(envelope.digest.ends_with(DIGEST_CUT_MARKER), "{:?}", envelope.digest);
        assert!(envelope.digest.contains("LAST-LINE"), "newest end survives: {:?}", envelope.digest);
    }

    #[tokio::test(start_paused = true)]
    async fn a_terminal_digest_is_cut_to_the_bound_and_a_short_one_is_not() {
        // why: SPEC "Envelope" + V27: a fired envelope's digest (last_digest)
        //      is bounded by VIGIL_DIGEST_MAX_BYTES (64) with the marker
        //      inside the bound; the newest line survives. Control: a short
        //      digest under the same config carries no marker.
        // proves: CLAIM(digest-cut)
        let mut config = default_config();
        config.digest_max_bytes = 64;
        let env = paused_env_with(config);
        let vigil = process(&env);
        let (big, big_path) = quiet_watch(&env, &vigil, "big", 20, 3600).await;
        let (small, small_path) = quiet_watch(&env, &vigil, "small", 20, 3600).await;
        append_file(&big_path, &format!("{}LAST-LINE\n", numbered("noise", 100)));
        let cut = vigil.keepalive(by_id(&big, None)).await.unwrap();
        assert_eq!(cut.status, WatchStatus::Fired);
        assert!(cut.digest.ends_with(DIGEST_CUT_MARKER), "{:?}", cut.digest);
        assert!(cut.digest.contains("LAST-LINE"));
        assert!(cut.digest.len() <= 64, "{} bytes", cut.digest.len());

        append_file(&small_path, "tiny\n");
        let whole = vigil.keepalive(by_id(&small, None)).await.unwrap();
        assert!(whole.digest.contains("tiny"));
        assert!(!whole.digest.contains(DIGEST_CUT_MARKER));
    }

    // ---- V2: one call budget ----

    #[tokio::test]
    async fn a_slow_cmd_cannot_push_a_call_past_max_s_and_the_aborted_tick_writes_nothing() {
        // why: V2 (S2, blocker): the 270 s ceiling did not constrain input
        //      reads. The cmd input sleeps 100 s (own timeout 60 s, above the
        //      30 s budget) once a marker file exists. The call must return at
        //      exactly max_s = 30 s (virtual, monotonic), pending, and the
        //      aborted tick must write NOTHING: the watch file is byte-for-byte
        //      what it was (tick 0, no last_tick_at), so the appended line is
        //      not consumed. After the marker is removed the next call sees
        //      the change and fires. Real create/baseline, paused budgeted
        //      call (a child process cannot run under auto-advance).
        // proves: CLAIM(call-budget)
        let env = paused_env();
        let vigil = process(&env);
        let path = env.input_path("w.log");
        let marker = env.input_path("slow.marker");
        write_file(&path, "old\n");
        let mut request = change_request("w", &path, 20);
        request.inputs.push(Input::Cmd {
            argv: vec!["sh".into(), "-c".into(), format!("if [ -e '{}' ]; then sleep 100; fi; echo ok", marker.display())],
            timeout_s: Some(60.0),
            cwd: None,
        });
        let id = vigil.create(request).await.unwrap().watch_id;
        write_file(&marker, "");
        append_file(&path, "ERROR: appended before the slow call\n");
        let before = std::fs::read(vigil.store().watch_path(&id)).unwrap();

        tokio::time::pause();
        let started = Instant::now();
        let envelope = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(30), "the call must end exactly at max_s");
        tokio::time::resume();
        assert_eq!(envelope.status, WatchStatus::Pending);
        assert_eq!((envelope.waited_s, envelope.max_s), (Some(30), Some(30)));
        assert_eq!(std::fs::read(vigil.store().watch_path(&id)).unwrap(), before, "an aborted tick writes nothing");

        std::fs::remove_file(&marker).unwrap();
        let next = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!(next.status, WatchStatus::Fired, "the change was not consumed by the aborted tick");
        assert!(next.digest.contains("appended before the slow call"));
        assert_eq!(next.tick, 1);
    }

    #[tokio::test]
    async fn a_hanging_url_cannot_push_a_call_past_max_s_and_the_aborted_tick_writes_nothing() {
        // why: V2 (S2): same contract for a url input whose server accepts and
        //      never answers. The URL's own timeout (10 s) is shorter than the
        //      30 s window, so the tick that starts at t=25 (interval 25) has
        //      only 5 s of budget left: the read must be capped at the budget
        //      left, the call must end at 30 s with nothing written, and the
        //      appended line must still fire on the next call. Tick 1 runs in
        //      real time before the pause (real sockets).
        // proves: CLAIM(call-budget)
        let server = FakeHttpServer::start(Behavior::Respond { status: 200, body: "ok".into() });
        let env = paused_env();
        let vigil = process(&env);
        let path = env.input_path("w.log");
        write_file(&path, "old\n");
        let mut request = change_request("w", &path, 25);
        request.inputs.push(Input::Url { url: format!("{}/health", server.url()) });
        let id = vigil.create(request).await.unwrap().watch_id;
        assert_eq!(vigil.tick_due(&id).await.unwrap(), crate::tick::TickOutcome::Ticked);
        append_file(&path, "ERROR: appended after tick 1\n");
        server.set_behavior(Behavior::Hang);
        let before = std::fs::read(vigil.store().watch_path(&id)).unwrap();

        tokio::time::pause();
        let started = Instant::now();
        let envelope = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(30));
        tokio::time::resume();
        assert_eq!(envelope.status, WatchStatus::Pending);
        assert_eq!(std::fs::read(vigil.store().watch_path(&id)).unwrap(), before, "an aborted tick writes nothing");

        server.set_behavior(Behavior::Respond { status: 200, body: "ok".into() });
        let next = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!(next.status, WatchStatus::Fired);
        assert!(next.digest.contains("appended after tick 1"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_judge_is_capped_at_the_budget_left_and_the_change_is_asked_again_next_call() {
        // why: V2 (S2): the judge request (10 s by default) is "capped at the
        //      budget left". judge_timeout is set to 25 s so only the budget
        //      can stop it: interval 20, change written at t=5, the tick at
        //      t=20 asks a judge that answers after 100 s with 10 s of budget
        //      left. The call ends at exactly 30 s, pending; the aborted tick
        //      wrote nothing (tick 1, no strike, no pending_change, judge
        //      status untouched). The next call asks again (2nd request, same
        //      line) and fires on the answer.
        // proves: CLAIM(call-budget)
        let mut config = default_config();
        config.judge_timeout = Duration::from_secs(25);
        let env = paused_env_with(config);
        let vigil = process(&env);
        let path = env.input_path("j.log");
        write_file(&path, "old\n");
        let id = vigil.create(judge_request("j", &path, 20)).await.unwrap().watch_id;
        env.judge.push_delayed(Duration::from_secs(100), Ok(noul_answers("failed", 0.99, 0.9)));
        let call = {
            let (vigil, id) = (vigil.clone(), id.clone());
            tokio::spawn(async move { vigil.keepalive(by_id(&id, Some(30))).await.unwrap() })
        };
        let started = Instant::now();
        tokio::time::sleep(Duration::from_secs(5)).await;
        append_file(&path, "ERROR: slow judge case\n");
        let envelope = call.await.unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(30));
        assert_eq!(envelope.status, WatchStatus::Pending);
        let file = env.vigil.store().load(&id).unwrap();
        assert_eq!((file.tick, file.judge_strikes), (1, 0), "aborted tick 2 wrote nothing");
        assert!(file.pending_change.is_none());
        assert_eq!(env.judge.call_count(), 1);

        env.judge.push(Ok(noul_answers("failed", 0.99, 0.9)));
        let next = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!(next.status, WatchStatus::Fired);
        assert_eq!(env.judge.call_count(), 2, "the unconsumed change is asked again");
        assert!(env.judge.requests()[1].state.contains("ERROR: slow judge case"));
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_for_the_lock_counts_against_the_budget() {
        // why: V2 (S2): "Waiting for it counts against the call budget". The
        //      global lock is held by the test for the whole first call: the
        //      call must give up at exactly max_s = 30 s with a pending
        //      envelope and write nothing (it could not even tick), instead of
        //      blocking until the holder lets go. NOTE for the implementer:
        //      this runs on a current-thread runtime, so the lock wait must
        //      not block the thread (poll try_lock or spawn_blocking). After
        //      the release the change fires.
        // proves: CLAIM(call-budget)
        let env = paused_env();
        let vigil = process(&env);
        let (id, path) = quiet_watch(&env, &vigil, "w", 20, 3600).await;
        append_file(&path, "ERROR: behind the lock\n");
        let before = std::fs::read(vigil.store().watch_path(&id)).unwrap();
        let guard = env.vigil.store().lock().unwrap();

        let started = Instant::now();
        let envelope = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(30));
        assert_eq!(envelope.status, WatchStatus::Pending);
        drop(guard);
        assert_eq!(std::fs::read(vigil.store().watch_path(&id)).unwrap(), before);

        let next = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!(next.status, WatchStatus::Fired);
    }

    // ---- wait ----

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
            tokio::spawn(async move { vigil.wait(&id).await.unwrap() })
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
        let envelope = vigil.wait(&id).await.unwrap();
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
            tokio::spawn(async move { vigil.wait(&id).await.unwrap() })
        };
        tokio::time::sleep(Duration::from_secs(100)).await;
        env.another_process().cancel(&id).await.unwrap();
        let envelope = call.await.unwrap();
        assert_eq!(envelope.status, WatchStatus::Cancelled);
        assert!((100..=120).contains(&envelope.elapsed_s), "elapsed {}", envelope.elapsed_s);
        assert_eq!(wait_exit_code(&envelope), Some(3));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_returns_one_final_envelope_holding_the_whole_event_batch() {
        // why: S21/V21/R8: `wait` has no streaming. Two inputs change in the
        //      same tick; the single returned envelope carries both events, in
        //      input order, equal to what is stored. (Replaces the
        //      `--stream`/EventSink test, whose sink contract is gone.)
        // proves: CLAIM(wait-final-batch)
        let env = paused_env();
        let vigil = process(&env);
        let first = env.input_path("first.log");
        let second = env.input_path("second.log");
        write_file(&first, "a\n");
        write_file(&second, "b\n");
        let mut request = change_request("w", &first, 20);
        request.inputs.push(Input::File { path: second.clone(), tail_bytes: None });
        let id = vigil.create(request).await.unwrap().watch_id;
        append_file(&first, "first-new\n");
        append_file(&second, "second-new\n");
        let envelope = vigil.wait(&id).await.unwrap();
        assert_eq!(envelope.status, WatchStatus::Fired);
        assert_eq!(envelope.events.len(), 2, "the whole batch in one envelope");
        assert!(envelope.events[0].summary.contains("first-new"), "input order: {:?}", envelope.events);
        assert!(envelope.events[1].summary.contains("second-new"));
        assert_eq!(envelope.events, env.vigil.store().load(&id).unwrap().events);
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
        let envelope = vigil.wait(&id).await.unwrap();
        assert_eq!((envelope.status, started.elapsed()), (WatchStatus::Cancelled, Duration::ZERO));
        assert_eq!(envelope.next, Next::WatchEnded);
        assert_eq!(
            vigil.wait(&crate::testing::wid("ghost")).await.unwrap_err(),
            VigilError::NotFound("ghost".into())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_envelope_for_an_unreachable_judge_is_labelled_down() {
        // why: V20-adjacent control for the judge_status wire value: when the
        //      judge fails, a pending return reports judge_status "down" (and
        //      the change is not dropped).
        let env = paused_env();
        let vigil = process(&env);
        let path = env.input_path("j.log");
        write_file(&path, "old\n");
        let id = vigil.create(judge_request("j", &path, 20)).await.unwrap().watch_id;
        env.judge.set_fallback(Err(JudgeError::Unreachable("down".into())));
        append_file(&path, "ERROR: x\n");
        let envelope = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!(envelope.judge_status, JudgeStatus::Down);
    }
}
