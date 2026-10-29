//! `Vigil`: the deep module. Everything the CLI and the MCP front do goes
//! through here, against one state root, one clock and one judge.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::clock::{Clock, Timestamp};
use crate::config::Config;
use crate::envelope::Envelope;
use crate::error::VigilError;
use crate::judge::Judge;
use crate::store::Store;
use crate::watch::{CreateRequest, WatchId, WatchStatus};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Created {
    pub watch_id: WatchId,
    pub status: WatchStatus,
    pub created_at: Timestamp,
    pub deadline_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchSummary {
    pub id: WatchId,
    pub status: WatchStatus,
    pub tick: u64,
    pub created_at: Timestamp,
    pub deadline_at: Timestamp,
}

/// `watch_status`: the envelope plus judge reachability.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusReport {
    #[serde(flatten)]
    pub envelope: Envelope,
    pub judge_reachable: bool,
}

/// In-memory, per-process state (the only thing a restart loses; V7).
#[derive(Default)]
pub(crate) struct Session {
    /// Ids of watches this process created (D2).
    pub created: BTreeSet<WatchId>,
    /// Ids whose terminal result this process has already returned (V7), so a
    /// no-id `keepalive` stops covering them.
    pub returned: BTreeSet<WatchId>,
    pub session_ticks: u64,
}

pub struct Vigil {
    pub(crate) store: Store,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) judge: Arc<dyn Judge>,
    pub(crate) config: Config,
    /// V11: relative inputs resolve against this at `create`.
    pub(crate) cwd: PathBuf,
    pub(crate) session: Mutex<Session>,
}

impl Vigil {
    /// The creating cwd is the process's current directory.
    pub fn new(root: impl Into<PathBuf>, clock: Arc<dyn Clock>, judge: Arc<dyn Judge>, config: Config) -> Vigil {
        Vigil {
            store: Store::new(root),
            clock,
            judge,
            config,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            session: Mutex::new(Session::default()),
        }
    }

    /// Override the working directory used to resolve relative inputs (V11).
    pub fn with_cwd(mut self, cwd: PathBuf) -> Vigil {
        self.cwd = cwd;
        self
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// `watch_create`. Validates (V12, V19), resolves relative inputs against
    /// the cwd (V11), rejects an id that still has a file (V6), (for fire =
    /// judge) checks the judge is reachable, records the BASELINE of every
    /// input (V1: so a change between create and the first `keepalive` is a
    /// change), and writes the file, all under the global lock (V4). No tick
    /// runs (`tick` stays 0). Also prunes.
    pub async fn create(&self, request: CreateRequest) -> Result<Created, VigilError> {
        todo!()
    }

    /// `watch_status`: non-blocking, does not tick. For a finished (or overdue,
    /// V16) watch it replays the stored terminal result (V3).
    pub async fn status(&self, id: &WatchId) -> Result<StatusReport, VigilError> {
        todo!()
    }

    /// `watch_list`: every watch in the state root, any session. Prunes.
    pub async fn list(&self) -> Result<Vec<WatchSummary>, VigilError> {
        todo!()
    }

    /// `watch_cancel`. Cancel on an already finished watch returns the stored
    /// result unchanged (V4). A pending watch past its deadline is already
    /// expired (V16) and cancel returns that.
    pub async fn cancel(&self, id: &WatchId) -> Result<Envelope, VigilError> {
        todo!()
    }

    /// Delete finished watches older than 7 days; returns deleted ids.
    pub fn prune(&self) -> Result<Vec<WatchId>, VigilError> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::Next;
    use crate::error::CreateError;
    use crate::testing::*;
    use crate::tick::TickOutcome;
    use crate::watch::{FireKind, Input, WatchStatus};

    fn fixture() -> (Arc<ManualClock>, Env) {
        let clock = Arc::new(ManualClock::new(spec_instant()));
        let env = Env::with_clock(clock.clone());
        (clock, env)
    }

    fn json_files(env: &Env) -> Vec<String> {
        let dir = env.root().join("watches");
        match std::fs::read_dir(dir) {
            Ok(entries) => entries
                .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
                .filter(|n| n.ends_with(".json"))
                .collect(),
            Err(_) => vec![],
        }
    }

    #[tokio::test]
    async fn create_writes_a_pending_watch_and_runs_no_tick() {
        // why: SPEC "watch_create": "Writes the watch file and returns
        //      {watch_id, status: pending, created_at, deadline_at}. No tick
        //      runs until the first keepalive." (V1 moves only the BASELINE to
        //      create; the tick count stays 0 and last_tick_at stays unset.)
        // proves: CLAIM(create-no-tick)
        let (_clock, env) = fixture();
        let path = env.input_path("a.log");
        write_file(&path, "x\n");
        let created = env.vigil.create(change_request("build", &path, 20)).await.unwrap();
        assert_eq!(created.watch_id.as_str(), "build");
        assert_eq!(created.status, WatchStatus::Pending);
        assert_eq!(created.created_at, spec_instant());
        assert_eq!(created.deadline_at, at("2026-10-29T15:03:11Z"));
        let file = env.vigil.store().load(&created.watch_id).unwrap();
        assert_eq!((file.tick, file.last_tick_at), (0, None));
        assert_eq!(json_files(&env), vec!["build.json"]);
    }

    #[tokio::test]
    async fn create_records_the_baseline_of_every_input_at_creation() {
        // why: V1 (S1, was R1): the baseline is recorded at `watch_create`,
        //      not at the first tick. "old\n" is 4 bytes, so the file input's
        //      offset is 4 and its (dev, inode, size) stamp is stored; a
        //      process input stores the pid's start time (V17). One state per
        //      input, in input order.
        // proves: CLAIM(baseline-at-create)
        let (_clock, env) = fixture();
        let path = env.input_path("a.log");
        write_file(&path, "old\n");
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let mut request = change_request("w", &path, 20);
        request.inputs.push(Input::Process { pid: child.id(), want: None });
        let id = env.vigil.create(request).await.unwrap().watch_id;
        let file = env.vigil.store().load(&id).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(file.input_states.len(), 2);
        assert_eq!(file.input_states[0].offset, Some(4));
        let stamp = file.input_states[0].file.expect("file stamp recorded at create");
        assert_eq!(stamp.size, 4);
        assert!(file.input_states[1].proc_start.is_some(), "process start time recorded at create");
        assert_eq!(file.tick, 0);
    }

    #[tokio::test]
    async fn create_without_an_id_generates_distinct_valid_ids() {
        // why: SPEC "watch_create": "a generated id is used when absent".
        let (_clock, env) = fixture();
        let path = env.input_path("a.log");
        let mut request = change_request("x", &path, 20);
        request.id = None;
        let first = env.vigil.create(request.clone()).await.unwrap().watch_id;
        let second = env.vigil.create(request).await.unwrap().watch_id;
        assert_ne!(first, second);
        assert_eq!(json_files(&env).len(), 2);
    }

    #[tokio::test]
    async fn an_id_that_still_has_a_file_cannot_be_reused_pending_or_finished() {
        // why: V6 (S6): reusing a finished id could erase an undelivered
        //      terminal result and redirect an old waiter. Any id with a file
        //      (pending, cancelled, fired, expired) is a DuplicateId. The
        //      stored result survives the refused attempt untouched.
        //      Control: a never-used id works, and once prune deleted the
        //      file the id is free again.
        // proves: CLAIM(no-id-reuse)
        let (clock, env) = fixture();
        let path = env.input_path("a.log");
        write_file(&path, "old\n");
        let dup = |id: &str| VigilError::Create(CreateError::DuplicateId(id.into()));

        env.vigil.create(change_request("pending", &path, 20)).await.unwrap();
        assert_eq!(env.vigil.create(change_request("pending", &path, 20)).await, Err(dup("pending")));

        env.vigil.create(change_request("cancelled", &path, 20)).await.unwrap();
        env.vigil.cancel(&wid("cancelled")).await.unwrap();
        env.vigil.create(change_request("fired", &path, 20)).await.unwrap();
        append_file(&path, "boom\n");
        assert_eq!(env.vigil.tick_due(&wid("fired")).await.unwrap(), TickOutcome::Ticked);
        env.vigil.create(change_request("expired", &path, 20)).await.unwrap();
        clock.advance_secs(3601);
        assert_eq!(env.vigil.tick_due(&wid("expired")).await.unwrap(), TickOutcome::Finished(WatchStatus::Expired));
        let fired_before = env.vigil.store().load(&wid("fired")).unwrap();
        assert_eq!(fired_before.status, WatchStatus::Fired, "precondition: fired watch holds an event");
        assert!(!fired_before.events.is_empty());

        for id in ["cancelled", "fired", "expired"] {
            assert_eq!(env.vigil.create(change_request(id, &path, 20)).await, Err(dup(id)), "finished id {id}");
        }
        assert_eq!(env.vigil.store().load(&wid("fired")).unwrap(), fired_before, "refused create must not touch the stored result");

        env.vigil.create(change_request("fresh", &path, 20)).await.unwrap();
        clock.advance_secs(8 * 24 * 3600);
        env.vigil.prune().unwrap();
        assert!(env.vigil.store().load(&wid("cancelled")).is_err(), "precondition: pruned");
        env.vigil.create(change_request("cancelled", &path, 20)).await.unwrap();
    }

    #[tokio::test]
    async fn invalid_requests_write_nothing() {
        // why: validation errors happen before any disk write (SPEC
        //      "watch_create"); a bad id must not leave a file behind. V19
        //      errors (https, empty argv) and V12 limits go through the same path.
        let (_clock, env) = fixture();
        let mut request = change_request("ok", &env.input_path("a"), 20);
        request.id = Some("bad id/../".into());
        assert!(matches!(
            env.vigil.create(request).await,
            Err(VigilError::Create(CreateError::InvalidId(_)))
        ));
        let mut judge_missing = change_request("ok", &env.input_path("a"), 20);
        judge_missing.fire = FireKind::Judge;
        assert!(matches!(
            env.vigil.create(judge_missing).await,
            Err(VigilError::Create(CreateError::MissingJudgeFields))
        ));
        let mut https = change_request("ok", &env.input_path("a"), 20);
        https.inputs = vec![Input::Url { url: "https://example.com/".into() }];
        assert!(matches!(
            env.vigil.create(https).await,
            Err(VigilError::Create(CreateError::UnsupportedUrlScheme(_)))
        ));
        let mut too_many = change_request("ok", &env.input_path("a"), 20);
        too_many.inputs = (0..17).map(|i| Input::File { path: format!("/tmp/f{i}").into(), tail_bytes: None }).collect();
        assert_eq!(env.vigil.create(too_many).await, Err(VigilError::Create(CreateError::TooManyInputs(17))));
        assert!(json_files(&env).is_empty());
    }

    #[tokio::test]
    async fn judge_watches_require_a_reachable_judge_at_create_time() {
        // why: SPEC "Packaging": "where [laya] is unreachable, fire = judge
        //      returns an error at watch_create instead of failing later".
        //      change/condition watches do not need the judge. Control: the
        //      same request succeeds once the judge is healthy.
        // proves: CLAIM(judge-reachable-at-create)
        let (_clock, env) = fixture();
        let path = env.input_path("a.log");
        env.judge.set_healthy(false);
        assert_eq!(
            env.vigil.create(judge_request("j", &path, 20)).await,
            Err(VigilError::Create(CreateError::JudgeUnreachable))
        );
        assert!(json_files(&env).is_empty(), "no file for a refused watch");
        env.vigil.create(change_request("c", &path, 20)).await.unwrap();
        env.judge.set_healthy(true);
        env.vigil.create(judge_request("j", &path, 20)).await.unwrap();
    }

    #[tokio::test]
    async fn status_reports_a_fresh_watch_and_judge_reachability() {
        // why: SPEC "watch_status ... also reports judge reachability".
        //      Fresh watch: tick 0, elapsed 0, deadline 3600 s away, no events,
        //      pending, "call keepalive again"; non-blocking so no waited_s/max_s.
        //      V8: the envelope names the watch it describes.
        let (_clock, env) = fixture();
        let path = env.input_path("a.log");
        let id = env.vigil.create(change_request("w", &path, 20)).await.unwrap().watch_id;
        let report = env.vigil.status(&id).await.unwrap();
        let e = &report.envelope;
        assert_eq!((e.status, e.tick, e.elapsed_s, e.deadline_in_s), (WatchStatus::Pending, 0, 0, 3600));
        assert_eq!(e.watch_id, id);
        assert_eq!(e.now, spec_instant());
        assert!(e.events.is_empty());
        assert_eq!(e.next, Next::CallKeepaliveAgain);
        assert_eq!((e.waited_s, e.max_s), (None, None));
        assert!(report.judge_reachable);
        env.judge.set_healthy(false);
        assert!(!env.vigil.status(&id).await.unwrap().judge_reachable);
    }

    #[tokio::test]
    async fn unknown_ids_are_not_found() {
        // why: callers must be able to tell a typo from a finished watch.
        let (_clock, env) = fixture();
        let ghost = wid("ghost");
        assert_eq!(env.vigil.status(&ghost).await.unwrap_err(), VigilError::NotFound("ghost".into()));
        assert_eq!(env.vigil.cancel(&ghost).await.unwrap_err(), VigilError::NotFound("ghost".into()));
    }

    #[tokio::test]
    async fn list_shows_watches_from_every_session() {
        // why: SPEC "watch_list": non-blocking read of the state root. Unlike
        //      keepalive (D2), list is not limited to this process's watches.
        let (_clock, env) = fixture();
        let other = env.another_process();
        let path = env.input_path("a.log");
        env.vigil.create(change_request("mine", &path, 20)).await.unwrap();
        other.create(change_request("theirs", &path, 20)).await.unwrap();
        other.cancel(&wid("theirs")).await.unwrap();
        let mut seen: Vec<(String, WatchStatus)> =
            env.vigil.list().await.unwrap().into_iter().map(|s| (s.id.to_string(), s.status)).collect();
        seen.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(seen, vec![("mine".into(), WatchStatus::Pending), ("theirs".into(), WatchStatus::Cancelled)]);
    }

    #[tokio::test]
    async fn an_overdue_pending_watch_reads_as_expired_through_list_and_status() {
        // why: V16 (S16): a pending watch past its absolute deadline "counts
        //      as expired" for every reader, even when nothing ever ticked it.
        //      Control: before the deadline the same watch reads pending.
        // proves: CLAIM(overdue-is-expired)
        let (clock, env) = fixture();
        let path = env.input_path("a.log");
        let id = env.vigil.create(change_request("abandoned", &path, 20)).await.unwrap().watch_id;
        clock.advance_secs(3599);
        assert_eq!(env.vigil.list().await.unwrap()[0].status, WatchStatus::Pending);
        clock.advance_secs(1);
        assert_eq!(env.vigil.list().await.unwrap()[0].status, WatchStatus::Expired);
        let status = env.vigil.status(&id).await.unwrap().envelope;
        assert_eq!(status.status, WatchStatus::Expired);
        assert_eq!(status.next, Next::WatchEnded);
        assert_eq!(status.deadline_in_s, 0);
    }

    #[tokio::test]
    async fn cancel_finishes_the_watch_and_stops_ticking() {
        // why: SPEC "watch_cancel"; status becomes cancelled, finished_at set
        //      (starts the 7-day prune clock), and a cancelled watch no longer
        //      ticks. V20: its `next` is the terminal instruction, and V8 names the watch.
        let (clock, env) = fixture();
        let path = env.input_path("a.log");
        let id = env.vigil.create(change_request("w", &path, 20)).await.unwrap().watch_id;
        clock.advance_secs(5);
        let envelope = env.vigil.cancel(&id).await.unwrap();
        assert_eq!(envelope.status, WatchStatus::Cancelled);
        assert_eq!(envelope.watch_id, id);
        assert_eq!(envelope.next, Next::WatchEnded);
        assert!(envelope.events.is_empty());
        let file = env.vigil.store().load(&id).unwrap();
        assert_eq!(file.finished_at, Some(spec_instant().plus_secs(5)));
        assert_eq!(env.vigil.tick_due(&id).await.unwrap(), TickOutcome::Finished(WatchStatus::Cancelled));
    }

    #[tokio::test]
    async fn cancel_on_a_finished_watch_returns_the_stored_result_and_changes_nothing() {
        // why: V4 (S4): terminal states are immutable. Cancelling a FIRED
        //      watch must return the fired result (with its event and the
        //      same event_id), leave status/finished_at/events byte-identical,
        //      and a second cancel on a CANCELLED watch keeps the first
        //      finished_at. Control: cancel on a pending watch does change it.
        // proves: CLAIM(terminal-immutable)
        let (clock, env) = fixture();
        let path = env.input_path("a.log");
        write_file(&path, "old\n");
        let fired = env.vigil.create(change_request("fired", &path, 20)).await.unwrap().watch_id;
        append_file(&path, "boom\n");
        env.vigil.tick_due(&fired).await.unwrap();
        let stored = env.vigil.store().load(&fired).unwrap();
        assert_eq!(stored.status, WatchStatus::Fired);
        let bytes_before = std::fs::read(env.vigil.store().watch_path(&fired)).unwrap();

        clock.advance_secs(500);
        let cancelled_fired = env.vigil.cancel(&fired).await.unwrap();
        assert_eq!(cancelled_fired.status, WatchStatus::Fired, "cancel after fire returns the fired result");
        assert_eq!(cancelled_fired.next, Next::ActOnEvents);
        assert_eq!(cancelled_fired.events, stored.events);
        assert_eq!(std::fs::read(env.vigil.store().watch_path(&fired)).unwrap(), bytes_before, "file untouched");

        let first = env.vigil.create(change_request("c", &path, 20)).await.unwrap().watch_id;
        let first_cancel = env.vigil.cancel(&first).await.unwrap();
        clock.advance_secs(500);
        let second_cancel = env.vigil.cancel(&first).await.unwrap();
        assert_eq!(first_cancel.status, WatchStatus::Cancelled, "control: a pending watch is cancelled");
        assert_eq!(second_cancel.status, WatchStatus::Cancelled);
        assert_eq!(env.vigil.store().load(&first).unwrap().finished_at, Some(spec_instant().plus_secs(500)), "first finished_at wins");
    }

    #[tokio::test]
    async fn cancel_after_the_deadline_returns_expired_not_cancelled() {
        // why: V4 + V16 (S4: "specify deadline/fire/cancel precedence"): the
        //      deadline passed before the cancel arrived, so the watch already
        //      "counts as expired" with finished_at = deadline_at. DEFAULT
        //      TAKEN (flagged in .red-report.md): expiry wins over a later
        //      cancel. One second before the deadline the cancel still wins.
        // proves: CLAIM(terminal-precedence)
        let (clock, env) = fixture();
        let path = env.input_path("a.log");
        let late = env.vigil.create(change_request("late", &path, 20)).await.unwrap().watch_id;
        let early = env.vigil.create(change_request("early", &path, 20)).await.unwrap().watch_id;
        clock.advance_secs(3599);
        assert_eq!(env.vigil.cancel(&early).await.unwrap().status, WatchStatus::Cancelled);
        clock.advance_secs(2);
        let envelope = env.vigil.cancel(&late).await.unwrap();
        assert_eq!(envelope.status, WatchStatus::Expired);
        let file = env.vigil.store().load(&late).unwrap();
        assert_eq!(file.status, WatchStatus::Expired);
        assert_eq!(file.finished_at, Some(file.deadline_at), "V16: finish time of an overdue watch is its deadline");
    }

    #[tokio::test]
    async fn status_replays_a_terminal_result_with_event_ids_unchanged() {
        // why: V3 (S3): the terminal result is stored once and replayed on
        //      every watch_status; events carry the stable `event_id`
        //      `<watch_id>:<tick>`. Two status calls from two processes at
        //      different times return identical events and digest. Control:
        //      a pending watch's status has no events.
        // proves: CLAIM(terminal-replay)
        let (clock, env) = fixture();
        let path = env.input_path("a.log");
        write_file(&path, "old\n");
        let id = env.vigil.create(change_request("w", &path, 20)).await.unwrap().watch_id;
        assert!(env.vigil.status(&id).await.unwrap().envelope.events.is_empty());
        append_file(&path, "ERROR: boom\n");
        env.vigil.tick_due(&id).await.unwrap();
        let first = env.vigil.status(&id).await.unwrap().envelope;
        clock.advance_secs(600);
        let second = env.another_process().status(&id).await.unwrap().envelope;
        assert_eq!(first.status, WatchStatus::Fired);
        assert_eq!(first.events.len(), 1);
        assert_eq!(first.events[0].event_id, "w:1");
        assert_eq!(first.events, second.events);
        assert_eq!(first.digest, second.digest);
        assert!(first.digest.contains("ERROR: boom"));
        assert_eq!((first.next, second.next), (Next::ActOnEvents, Next::ActOnEvents));
        assert_eq!((first.watch_id.clone(), second.watch_id), (id, first.watch_id));
    }

    #[tokio::test]
    async fn prune_removes_finished_watches_older_than_seven_days_and_keeps_the_rest() {
        // why: D4 / SPEC "State": 7 days after finishing, by any vigil run.
        //      Drives the real path: create + cancel in the past via the clock.
        //      "live" was created 2 days ago with a 1 h deadline: overdue but
        //      only for ~2 days, so V16 keeps it too.
        // proves: CLAIM(prune-7d)
        let (clock, env) = fixture();
        let path = env.input_path("a.log");
        let now = spec_instant();
        clock.set(now.minus_secs(9 * 24 * 3600));
        env.vigil.create(change_request("old", &path, 20)).await.unwrap();
        clock.set(now.minus_secs(8 * 24 * 3600));
        env.vigil.cancel(&wid("old")).await.unwrap();
        clock.set(now.minus_secs(2 * 24 * 3600));
        env.vigil.create(change_request("recent", &path, 20)).await.unwrap();
        env.vigil.cancel(&wid("recent")).await.unwrap();
        env.vigil.create(change_request("live", &path, 20)).await.unwrap();
        clock.set(now);
        let deleted = env.vigil.prune().unwrap();
        assert_eq!(deleted, vec![wid("old")]);
        let mut left = json_files(&env);
        left.sort();
        assert_eq!(left, vec!["live.json", "recent.json"]);
    }

    #[tokio::test]
    async fn prune_also_removes_abandoned_pending_watches_but_never_the_lock_file() {
        // why: V16 (S16) + V5 (S5): a pending watch nobody ever called again
        //      (created 20 days ago, deadline 1 h later) is pruned through the
        //      real create/prune path, and the global lock file stays.
        // proves: CLAIM(prune-overdue)
        let (clock, env) = fixture();
        let path = env.input_path("a.log");
        let now = spec_instant();
        clock.set(now.minus_secs(20 * 24 * 3600));
        env.vigil.create(change_request("abandoned", &path, 20)).await.unwrap();
        clock.set(now);
        assert_eq!(env.vigil.prune().unwrap(), vec![wid("abandoned")]);
        assert!(json_files(&env).is_empty());
        assert!(env.root().join("lock").exists(), "the lock file is never deleted");
    }

    #[tokio::test]
    async fn listing_prunes_too() {
        // why: SPEC "State": finished watches are deleted "by any vigil run";
        //      `list` is the cheapest run that does not otherwise write.
        let (clock, env) = fixture();
        let path = env.input_path("a.log");
        let now = spec_instant();
        clock.set(now.minus_secs(30 * 24 * 3600));
        env.vigil.create(change_request("old", &path, 20)).await.unwrap();
        env.vigil.cancel(&wid("old")).await.unwrap();
        clock.set(now);
        assert!(env.vigil.list().await.unwrap().is_empty());
        assert!(json_files(&env).is_empty());
    }
}
