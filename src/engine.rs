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

#[derive(Default)]
pub(crate) struct Session {
    pub created: BTreeSet<WatchId>,
    pub session_ticks: u64,
}

pub struct Vigil {
    pub(crate) store: Store,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) judge: Arc<dyn Judge>,
    pub(crate) config: Config,
    pub(crate) session: Mutex<Session>,
}

impl Vigil {
    pub fn new(root: impl Into<PathBuf>, clock: Arc<dyn Clock>, judge: Arc<dyn Judge>, config: Config) -> Vigil {
        Vigil {
            store: Store::new(root),
            clock,
            judge,
            config,
            session: Mutex::new(Session::default()),
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// `watch_create`. Validates, rejects a duplicate id of a pending watch,
    /// (for fire = judge) checks the judge is reachable, writes the file. No
    /// tick runs. Also prunes old finished watches.
    pub async fn create(&self, request: CreateRequest) -> Result<Created, VigilError> {
        todo!()
    }

    /// `watch_status`: non-blocking.
    pub async fn status(&self, id: &WatchId) -> Result<StatusReport, VigilError> {
        todo!()
    }

    /// `watch_list`: every watch in the state root, any session. Prunes.
    pub async fn list(&self) -> Result<Vec<WatchSummary>, VigilError> {
        todo!()
    }

    /// `watch_cancel`.
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
    use crate::watch::{FireKind, WatchStatus};

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
        //      runs until the first keepalive."
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
    async fn duplicate_id_of_a_pending_watch_is_an_error_but_a_finished_one_can_be_reused() {
        // why: SPEC "watch_create": "An id that already exists for a pending
        //      watch is an error." Hence a finished (cancelled) watch's id may
        //      be reused and the new watch starts clean.
        // proves: CLAIM(duplicate-id)
        let (clock, env) = fixture();
        let path = env.input_path("a.log");
        let id = env.vigil.create(change_request("dup", &path, 20)).await.unwrap().watch_id;
        let second = env.vigil.create(change_request("dup", &path, 20)).await;
        assert_eq!(second, Err(VigilError::Create(CreateError::DuplicateId("dup".into()))));

        env.vigil.cancel(&id).await.unwrap();
        clock.advance_secs(100);
        env.vigil.create(change_request("dup", &path, 20)).await.unwrap();
        let file = env.vigil.store().load(&id).unwrap();
        assert_eq!(file.status, WatchStatus::Pending);
        assert_eq!(file.created_at, spec_instant().plus_secs(100));
    }

    #[tokio::test]
    async fn invalid_requests_write_nothing() {
        // why: validation errors happen before any disk write (SPEC
        //      "watch_create"); a bad id must not leave a file behind.
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
        let (_clock, env) = fixture();
        let path = env.input_path("a.log");
        let id = env.vigil.create(change_request("w", &path, 20)).await.unwrap().watch_id;
        let report = env.vigil.status(&id).await.unwrap();
        let e = &report.envelope;
        assert_eq!((e.status, e.tick, e.elapsed_s, e.deadline_in_s), (WatchStatus::Pending, 0, 0, 3600));
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
    async fn cancel_finishes_the_watch_and_stops_ticking() {
        // why: SPEC "watch_cancel"; status becomes cancelled, finished_at set
        //      (starts the 7-day prune clock), and a cancelled watch no longer ticks.
        let (clock, env) = fixture();
        let path = env.input_path("a.log");
        let id = env.vigil.create(change_request("w", &path, 20)).await.unwrap().watch_id;
        clock.advance_secs(5);
        let envelope = env.vigil.cancel(&id).await.unwrap();
        assert_eq!(envelope.status, WatchStatus::Cancelled);
        let file = env.vigil.store().load(&id).unwrap();
        assert_eq!(file.finished_at, Some(spec_instant().plus_secs(5)));
        assert_eq!(env.vigil.tick_due(&id).await.unwrap(), TickOutcome::Finished(WatchStatus::Cancelled));
    }

    #[tokio::test]
    async fn prune_removes_finished_watches_older_than_seven_days_and_keeps_the_rest() {
        // why: D4 / SPEC "State": 7 days after finishing, by any vigil run.
        //      Drives the real path: create + cancel in the past via the clock.
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
