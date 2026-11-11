//! `Vigil`: the deep module. Everything the CLI and the MCP front do goes
//! through here, against one state root, one clock and one judge.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::attention::AttentionReason;
use crate::clock::{Clock, Timestamp};
use crate::config::Config;
use crate::envelope::{cut_digest, Envelope, Next, PersistFailure, PersistNote};
use crate::error::{CreateError, StoreError, VigilError};
use crate::inputs::{InputReader, Reading, SystemReader};
use crate::judge::Judge;
use crate::store::{Settled, Store, StoreLock, WatchLock};
use crate::watch::{
    CreateRequest, Event, FireRule, Input, InputState, Terminal, TerminalStatus, WatchFile, WatchId, WatchSpec, WatchStatus,
};

/// How often a contended lock is retried. Polling (not a blocking `flock`)
/// keeps the waiting future cancellable: a dropped call leaves no thread
/// parked on a lock it would later take and release.
const LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(2);

/// A rescue inside a call waits for the store-wide lock until this long
/// before the call budget ends (review S3). The wait is async now, so the
/// budget's own timeout can fire while it runs; stopping short leaves the tick
/// time to return its answer (DF1) instead of being dropped at the same instant.
const RESCUE_BUDGET_MARGIN: std::time::Duration = std::time::Duration::from_millis(100);

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
    /// Watches whose events this process may be repeating (S1, review S2):
    /// the most recent save of the watch failed, or it finished with events
    /// after a failed save and the envelope that carries them has not been
    /// delivered yet. A tick of such a watch follows a failed save, and stored
    /// events of it that this process replays are repeats of an event id the
    /// agent may already hold, possibly with other content. A fresh process
    /// cannot know, which is why this is a process fact. Cleared for a watch
    /// when a successful save leaves nothing to flag, or by `commit_delivery`.
    pub repeat_risk: BTreeSet<WatchId>,
    /// Review S5: the attention events this process built and could not save,
    /// by watch and reason. The envelope that carried them reached the agent,
    /// so the retry must be the SAME event (same id, same text): rebuilding it
    /// from the watch as it is by then (a tick may have moved `tick` in
    /// between) would give the agent a second notification for one fact.
    /// Dropped when a save of the watch lands them, or when the reason is
    /// found raised in the file (someone else saved it). A fresh process
    /// cannot know what a dead one showed; that limit is the same as for
    /// `repeat_risk`.
    pub unsaved_attention: BTreeMap<(WatchId, AttentionReason), Event>,
}

/// How one save of a watch file went, for a caller that must not fail because
/// the disk is full (DF1): the answer it holds is still good.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Persisted {
    /// Written; nothing to report (also: nothing needed writing).
    Saved,
    /// Written, but only after the reserve was spent (DF4): the disk is full.
    SavedOnReserve,
    /// Not written (DF2): the in-memory watch is the truth of this call, the
    /// file on disk is the previous one (DF6).
    NotSaved(PersistFailure),
}

impl Persisted {
    /// The envelope note for this save. `redelivery` (DF3a): is anything at
    /// risk, i.e. does the envelope carry events or a terminal status?
    pub(crate) fn note(&self, redelivery: bool) -> Option<PersistNote> {
        match self {
            Persisted::Saved => None,
            Persisted::SavedOnReserve => Some(PersistNote::ReserveSpent),
            Persisted::NotSaved(failure) => Some(PersistNote::NotSaved { failure: failure.clone(), redelivery }),
        }
    }
}

pub struct Vigil {
    pub(crate) store: Store,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) judge: Arc<dyn Judge>,
    pub(crate) config: Config,
    /// Every input read goes through this (create's baseline and each tick).
    pub(crate) reader: Arc<dyn InputReader>,
    /// V11: relative inputs resolve against this at `create`.
    pub(crate) cwd: PathBuf,
    pub(crate) session: Mutex<Session>,
    /// A3: which agent is calling; read from the environment at the binary
    /// edge and handed over with `with_agent_env`.
    pub(crate) agent: crate::cache::AgentEnv,
    /// A9: every credit check goes through this (tests: `ScriptedCreditProbe`).
    pub(crate) credit_probe: Arc<dyn crate::credit::CreditProbe>,
    /// S7: the operator hook runs through this (tests: `ScriptedHookRunner`).
    pub(crate) hook_runner: Arc<dyn crate::credit::CreditHookRunner>,
}

impl Vigil {
    /// The creating cwd is the process's current directory.
    pub fn new(root: impl Into<PathBuf>, clock: Arc<dyn Clock>, judge: Arc<dyn Judge>, config: Config) -> Vigil {
        let credit_probe = Arc::new(crate::credit::SystemCreditProbe::from_config(&config));
        Vigil {
            store: Store::new(root),
            clock,
            judge,
            config,
            reader: Arc::new(SystemReader),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            session: Mutex::new(Session::default()),
            agent: crate::cache::AgentEnv::default(),
            credit_probe,
            hook_runner: Arc::new(crate::credit::ProcessHookRunner),
        }
    }

    /// Tell the engine which agent calls it (A3). Default: neither Claude
    /// nor Codex.
    pub fn with_agent_env(mut self, agent: crate::cache::AgentEnv) -> Vigil {
        self.agent = agent;
        self
    }

    /// Replace the credit probe (tests: `testing::ScriptedCreditProbe`).
    pub fn with_credit_probe(mut self, probe: Arc<dyn crate::credit::CreditProbe>) -> Vigil {
        self.credit_probe = probe;
        self
    }

    /// Replace the hook runner (tests: `testing::ScriptedHookRunner`).
    pub fn with_hook_runner(mut self, runner: Arc<dyn crate::credit::CreditHookRunner>) -> Vigil {
        self.hook_runner = runner;
        self
    }

    /// Override the working directory used to resolve relative inputs (V11).
    pub fn with_cwd(mut self, cwd: PathBuf) -> Vigil {
        self.cwd = cwd;
        self
    }

    /// Replace the store's write fault plan (tests, and `main` when
    /// `VIGIL_TEST_FAULT` is set; see `fault.rs`).
    pub fn with_write_faults(mut self, faults: crate::fault::FaultPlan) -> Vigil {
        self.store = self.store.with_faults(faults);
        self
    }

    /// Replace the input reader (tests: `testing::ScriptedReader`).
    pub fn with_reader(mut self, reader: Arc<dyn InputReader>) -> Vigil {
        self.reader = reader;
        self
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// `watch_create`. Validates (V12, V19), resolves relative inputs against
    /// the cwd (V11), (for fire = judge) checks the judge is reachable, and
    /// (X3) takes the watch's lock, then the GLOBAL lock only long enough to
    /// reject an id that still has a file (V6) and to write a placeholder
    /// that reserves it, then releases the global lock and records the
    /// BASELINE of every input under the watch lock alone (V1: so a change
    /// between create and the first `keepalive` is a change), each read going
    /// through `self.reader`, and writes the file. The global lock is never
    /// held while inputs are read, so another create or any tick of another
    /// watch proceeds meanwhile. No tick runs (`tick` stays 0). Does not prune
    /// (the binary prunes on every run, `list` prunes too).
    pub async fn create(&self, request: CreateRequest) -> Result<Created, VigilError> {
        let spec = WatchSpec::parse(request)?.absolutize(&self.cwd);
        if matches!(spec.fire, FireRule::Judge { .. }) && !self.judge.healthy().await {
            return Err(CreateError::JudgeUnreachable.into());
        }
        let id = spec.id.clone();
        let _watch_lock = self.lock_watch(&id).await?;
        let mut watch = WatchFile::new(spec, self.clock.now());
        {
            // X3: the global lock only covers the id check and the placeholder
            // write that reserves the id; the baseline is read after it is gone.
            let global = self.lock_global().await?;
            if self.store.watch_path(&id).exists() {
                return Err(CreateError::DuplicateId(id.to_string()).into());
            }
            // DF4c: this save may need the store-wide lock for a rescue; it is
            // held right here, so the save reuses it instead of waiting for
            // itself.
            self.store.save_under(&watch, &global)?;
        }
        // V1: the baseline of every input is recorded now, not at the first tick.
        let baseline = vec![InputState::default(); watch.spec.inputs.len()];
        let readings = self.read_all(&watch.spec.inputs, &baseline).await;
        watch.input_states = readings.into_iter().map(|reading| reading.state).collect();
        if let Err(error) = self.store.save_async(&watch).await {
            // DF7: the placeholder reserved the id, and a create that fails
            // must not burn it. Deleting works on a full disk. Best effort: a
            // placeholder that stays is the pre-DF7 behaviour, not a new loss.
            let _ = std::fs::remove_file(self.store.watch_path(&id));
            return Err(error.into());
        }

        self.session.lock().expect("session mutex poisoned").created.insert(id.clone());
        Ok(Created {
            watch_id: id,
            status: watch.status,
            created_at: watch.created_at,
            deadline_at: watch.deadline_at,
        })
    }

    /// `watch_status`: non-blocking, does not tick. For a finished (or overdue,
    /// V16) watch it replays the stored terminal result (V3).
    pub async fn status(&self, id: &WatchId) -> Result<StatusReport, VigilError> {
        let now = self.clock.now();
        let watch = expired_if_overdue(self.load_watch(id)?, now);
        let envelope = self.envelope_for(&watch, now);
        Ok(StatusReport { envelope, judge_reachable: self.judge.healthy().await })
    }

    /// `watch_list`: every watch in the state root, any session. Prunes.
    pub async fn list(&self) -> Result<Vec<WatchSummary>, VigilError> {
        // Pruning is housekeeping; a failure to prune must not hide the list.
        let _ = self.prune_async().await;
        let now = self.clock.now();
        Ok(self
            .store
            .list()?
            .into_iter()
            .map(|watch| expired_if_overdue(watch, now))
            .map(|watch| WatchSummary {
                id: watch.spec.id,
                status: watch.status,
                tick: watch.tick,
                created_at: watch.created_at,
                deadline_at: watch.deadline_at,
            })
            .collect())
    }

    /// `watch_cancel`. Cancel on an already finished watch returns the stored
    /// result unchanged (V4). A pending watch past its deadline is already
    /// expired (V16) and cancel returns that.
    pub async fn cancel(&self, id: &WatchId) -> Result<Envelope, VigilError> {
        let (_lock, mut watch) = self.lock_and_load(id).await?;
        let now = self.clock.now();
        // DF5a: a cancel that cannot be saved is still answered; the envelope
        // says so, never a silent success. Nothing needed writing: `Saved`.
        let mut persisted = Persisted::Saved;
        if watch.is_overdue(now) {
            // W5: the deadline passed before the cancel arrived; expiry wins.
            watch = expired_if_overdue(watch, now);
            persisted = self.persist(&watch, None).await?;
        } else if !watch.status.is_finished() {
            let terminal = Terminal {
                status: TerminalStatus::Cancelled,
                finished_at: now,
                events: Vec::new(),
                digest: watch.last_digest.clone(),
            };
            watch.finish(terminal).expect("a pending watch accepts its terminal result");
            persisted = self.persist(&watch, None).await?;
        }
        let mut envelope = self.envelope_for(&watch, now);
        // A cancel's envelope always has a terminal status: at risk if unsaved.
        envelope.persist = persisted.note(true);
        Ok(envelope)
    }

    /// Save `watch`, turning a failed WRITE into a value (DF1): the caller
    /// keeps the answer it holds and reports the failure in the envelope.
    /// Anything else the store can fail at (a value that cannot be serialized)
    /// is a real error. A rescue inside the store waits for the store-wide
    /// lock at most `min(2 s, the call budget left)` (DF4e). Also keeps the
    /// process's memory of failed saves (S1). Async because the rescue's wait
    /// yields to the executor (review S3): the waits of several watches
    /// overlap, and a call that is dropped stops waiting.
    pub(crate) async fn persist(&self, watch: &WatchFile, budget_end: Option<tokio::time::Instant>) -> Result<Persisted, VigilError> {
        self.persist_observed(watch, budget_end, &|_| {}).await
    }

    /// [`Vigil::persist`] that tells `settled` how the save stands before each
    /// wait it may still make (review S4). The wait can be dropped with the
    /// future, so a caller holding a result (a tick) keeps it there: `settled`
    /// gets `NotSaved` the moment the first write fails, before the rescue
    /// waits for the lock, and `Saved` before reserve upkeep waits. If the
    /// future is dropped, the last thing `settled` heard is what happened to
    /// the file (CLAIM(DF4c): nothing after it is half done). The process's
    /// memory of failed saves is NOT touched by a report (a rescue that then
    /// succeeds must not flag its events as repeats); whoever collects a report
    /// from a dropped save applies [`Vigil::remember_save`] itself.
    pub(crate) async fn persist_observed(
        &self,
        watch: &WatchFile,
        budget_end: Option<tokio::time::Instant>,
        settled: &(dyn Fn(Persisted) + Sync),
    ) -> Result<Persisted, VigilError> {
        let on_settled = |point: Settled<'_>| {
            let persisted = match point {
                Settled::Written => Persisted::Saved,
                Settled::FirstAttemptFailed(error) => match error.write_failure() {
                    Some(failure) => Persisted::NotSaved(failure.into()),
                    None => return,
                },
            };
            settled(persisted);
        };
        let saved = match budget_end {
            Some(end) => {
                // An absolute moment, not the time left now: the store reads
                // the time left when a wait actually starts, after a slow
                // write has used some of it (review S5).
                let cutoff = end.checked_sub(RESCUE_BUDGET_MARGIN).unwrap_or(end);
                self.store.save_within_async(watch, cutoff, &on_settled).await
            }
            None => self.store.save_async(watch).await,
        };
        let persisted = match saved {
            Ok(outcome) if outcome.used_reserve => Persisted::SavedOnReserve,
            Ok(_) => Persisted::Saved,
            Err(error) => match error.write_failure() {
                Some(failure) => Persisted::NotSaved(failure.into()),
                None => return Err(error.into()),
            },
        };
        self.remember_save(watch, &persisted);
        Ok(persisted)
    }

    /// The process's memory of failed saves (S1), after one save of `watch`:
    /// applied at the end of a save, and by whoever collects the report of a
    /// save that was dropped before its end (the end never ran). Idempotent.
    pub(crate) fn remember_save(&self, watch: &WatchFile, persisted: &Persisted) {
        let mut session = self.session.lock().expect("session mutex poisoned");
        // ↳ CLAIM(S2-provenance): the memory outlives a successful retry for as long as the events it saved are undelivered [review S2; disk_full_tests a_retry_that_saved_keeps_its_redelivered_flag_until_the_envelope_is_really_delivered]
        let events_undelivered = watch.status.is_finished() && !watch.events.is_empty();
        match persisted {
            Persisted::NotSaved(_) => {
                session.repeat_risk.insert(watch.spec.id.clone());
            }
            Persisted::Saved | Persisted::SavedOnReserve => {
                if !events_undelivered {
                    session.repeat_risk.remove(&watch.spec.id);
                }
            }
        }
    }

    /// May this process be repeating events of `id`? Then a tick of it now is
    /// a redelivery (S1), and stored events of it that this process returns
    /// are flagged (review S2).
    pub(crate) fn may_repeat(&self, id: &WatchId) -> bool {
        self.session.lock().expect("session mutex poisoned").repeat_risk.contains(id)
    }

    /// Review S5: the attention event of `reason` that this process built for
    /// `id` and could not save, if there is one.
    pub(crate) fn unsaved_attention(&self, id: &WatchId, reason: AttentionReason) -> Option<Event> {
        self.session.lock().expect("session mutex poisoned").unsaved_attention.get(&(id.clone(), reason)).cloned()
    }

    /// Review S5: after one save of `id` that raised `raised`: a failed save
    /// keeps the events (the agent holds them, the retry must repeat them), a
    /// save that landed drops them.
    pub(crate) fn remember_attention_save(&self, id: &WatchId, raised: &[(AttentionReason, Event)], persisted: &Persisted) {
        let mut session = self.session.lock().expect("session mutex poisoned");
        for (reason, event) in raised {
            match persisted {
                Persisted::NotSaved(_) => {
                    session.unsaved_attention.insert((id.clone(), *reason), event.clone());
                }
                Persisted::Saved | Persisted::SavedOnReserve => {
                    session.unsaved_attention.remove(&(id.clone(), *reason));
                }
            }
        }
    }

    /// Review S5: `reasons` are already raised in `id`'s file (another process
    /// saved them), so a copy of ours that never landed is moot.
    pub(crate) fn forget_unsaved_attention(&self, id: &WatchId, reasons: &[AttentionReason]) {
        let mut session = self.session.lock().expect("session mutex poisoned");
        for reason in reasons {
            session.unsaved_attention.remove(&(id.clone(), *reason));
        }
    }

    /// The events of a stored result as this process returns them: flagged
    /// `redelivered` while it may be repeating them (review S2).
    pub(crate) fn replayed_events(&self, watch: &WatchFile) -> Vec<Event> {
        let repeating = self.may_repeat(&watch.spec.id);
        watch.events.iter().map(|event| Event { redelivered: event.redelivered || repeating, ..event.clone() }).collect()
    }

    /// Delete finished watches older than 7 days; returns deleted ids.
    /// Takes the locks itself (`Store::prune`); parks the calling thread while
    /// it waits for one.
    pub fn prune(&self) -> Result<Vec<WatchId>, VigilError> {
        Ok(self.store.prune(self.clock.now())?)
    }

    /// `prune` for async callers: its waits yield to the executor (review S3).
    pub async fn prune_async(&self) -> Result<Vec<WatchId>, VigilError> {
        Ok(self.store.prune_async(self.clock.now()).await?)
    }

    /// Load a watch, naming the id in `NotFound`.
    pub(crate) fn load_watch(&self, id: &WatchId) -> Result<WatchFile, VigilError> {
        self.store.load(id).map_err(|error| match error {
            StoreError::NotFound(_) => VigilError::not_found(id),
            other => other.into(),
        })
    }

    /// The watch's lock (X3), polled so the wait can be cancelled.
    pub(crate) async fn lock_watch(&self, id: &WatchId) -> Result<WatchLock, VigilError> {
        loop {
            if let Some(lock) = self.store.try_lock_watch_async(id).await? {
                return Ok(lock);
            }
            tokio::time::sleep(LOCK_POLL).await;
        }
    }

    async fn lock_global(&self) -> Result<StoreLock, VigilError> {
        loop {
            if let Some(lock) = self.store.try_lock_async().await? {
                return Ok(lock);
            }
            tokio::time::sleep(LOCK_POLL).await;
        }
    }

    /// Take an existing watch's lock and read it under the lock. An unknown
    /// id fails before any lock file is created; a watch pruned while we
    /// waited is `NotFound` and its recreated lock file is removed again.
    pub(crate) async fn lock_and_load(&self, id: &WatchId) -> Result<(WatchLock, WatchFile), VigilError> {
        self.load_watch(id)?;
        let lock = self.lock_watch(id).await?;
        match self.load_watch(id) {
            Ok(watch) => Ok((lock, watch)),
            Err(error) => {
                let _ = std::fs::remove_file(self.store.watch_lock_path(id));
                Err(error)
            }
        }
    }

    /// Read every input at once through the reader seam (X4); readings come
    /// back in input order. `previous` has one state per input.
    pub(crate) async fn read_all(&self, inputs: &[Input], previous: &[InputState]) -> Vec<Reading> {
        let mut tasks = tokio::task::JoinSet::new();
        for (index, (input, state)) in inputs.iter().zip(previous).enumerate() {
            let (reader, input, state) = (self.reader.clone(), input.clone(), state.clone());
            tasks.spawn(async move { (index, reader.read(&input, &state).await) });
        }
        let mut readings: Vec<Option<Reading>> = (0..inputs.len()).map(|_| None).collect();
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok((index, reading)) => readings[index] = Some(reading),
                Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
                Err(error) => panic!("input read task was cancelled: {error}"),
            }
        }
        readings.into_iter().map(|reading| reading.expect("every spawned read reports back")).collect()
    }

    /// The envelope describing `watch` as of `now` (V8). Non-blocking: no
    /// `waited_s` / `max_s`. A terminal envelope carries the stored events
    /// and `last_digest` cut to the configured bound (V14, V27); a pending
    /// one has no digest (V14: that is built per blocking call).
    pub(crate) fn envelope_for(&self, watch: &WatchFile, now: Timestamp) -> Envelope {
        let end = watch.finished_at.unwrap_or(now);
        let terminal = watch.status.is_finished();
        // Before the literal: the session lock taken for `session_ticks` below
        // lives to the end of the statement, and `replayed_events` takes it too.
        // S6: a pending watch's file holds only the outbox of attention events;
        // `status` neither delivers nor acknowledges them, so it shows none.
        let events = if terminal { self.replayed_events(watch) } else { Vec::new() };
        Envelope {
            watch_id: watch.spec.id.clone(),
            status: watch.status,
            now,
            tick: watch.tick,
            session_ticks: self.session.lock().expect("session mutex poisoned").session_ticks,
            elapsed_s: end.secs_since(watch.created_at).max(0) as u64,
            deadline_in_s: watch.deadline_at.secs_since(end).max(0) as u64,
            waited_s: None,
            max_s: None,
            events,
            digest: if terminal { cut_digest(&watch.last_digest, self.config.digest_max_bytes) } else { String::new() },
            judge_status: watch.judge_status,
            next: match watch.status {
                WatchStatus::Pending => Next::CallKeepaliveAgain,
                WatchStatus::Fired => Next::ActOnEvents,
                WatchStatus::Expired | WatchStatus::Cancelled => Next::WatchEnded,
            },
            others: Vec::new(),
            persist: None,
            persist_failures: Vec::new(),
            cache: None,
            clock: None,
        }
    }
}

/// V16: a pending watch past its deadline reads as expired, finished at the
/// deadline, whether or not anything ever ticked it. Other watches pass
/// through unchanged.
pub(crate) fn expired_if_overdue(mut watch: WatchFile, now: Timestamp) -> WatchFile {
    if watch.is_overdue(now) {
        let terminal = Terminal {
            status: TerminalStatus::Expired,
            finished_at: watch.deadline_at,
            events: Vec::new(),
            digest: watch.last_digest.clone(),
        };
        watch.finish(terminal).expect("an overdue watch is pending, so it accepts a terminal result");
    }
    watch
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::Next;
    use crate::error::CreateError;
    use crate::testing::*;
    use crate::tick::TickOutcome;
    use crate::watch::{FireKind, Input, WatchStatus};
    use std::time::Duration;

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
        let mut slow_cmd = change_request("ok", &env.input_path("a"), 20);
        slow_cmd.inputs = vec![Input::Cmd { argv: vec!["true".into()], timeout_s: Some(21.0), cwd: None }];
        assert!(
            matches!(env.vigil.create(slow_cmd).await, Err(VigilError::Create(CreateError::TimeoutOutOfRange(_)))),
            "X7: timeout_s above 19 is a create-time error"
        );
        assert!(json_files(&env).is_empty());
        assert!(!env.root().join("locks").join("ok.lock").exists(), "validation happens before any lock is taken");
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
        //      `<watch_id>:<tick>:<n>` (W1: the first is `:0`). Two status calls from two processes at
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
        assert_eq!(first.events[0].event_id, "w:1:0");
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
    async fn prune_also_removes_abandoned_pending_watches_and_their_lock_files_but_never_the_global_lock() {
        // why: V16 (S16) + V5 (S5) + X3: a pending watch nobody ever called
        //      again (created 20 days ago, deadline 1 h later) is pruned
        //      through the real create/prune path together with its
        //      `locks/<id>.lock`, and the global lock file stays. Control: the
        //      per-watch lock file existed before the prune (create takes it),
        //      so its absence afterwards is a deletion, not an omission.
        // proves: CLAIM(prune-overdue)
        let (clock, env) = fixture();
        let path = env.input_path("a.log");
        let now = spec_instant();
        clock.set(now.minus_secs(20 * 24 * 3600));
        env.vigil.create(change_request("abandoned", &path, 20)).await.unwrap();
        clock.set(now);
        assert!(env.root().join("locks").join("abandoned.lock").exists(), "control: create took the watch lock");
        assert_eq!(env.vigil.prune().unwrap(), vec![wid("abandoned")]);
        assert!(json_files(&env).is_empty());
        assert!(!env.root().join("locks").join("abandoned.lock").exists(), "X3: the lock file goes with its watch");
        assert!(env.root().join("lock").exists(), "the global lock file is never deleted");
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

    // ---- X3: create does not hold the global lock while reading baselines ----

    fn gated_cmd(name: &str) -> Input {
        Input::Cmd { argv: vec![name.into()], timeout_s: Some(19.0), cwd: None }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn create_does_not_hold_the_global_lock_while_it_reads_baselines() {
        // why: X3 (T4): the old design did all baseline I/O under the one
        //      global lock, so a create whose input hung blocked every other
        //      create, tick and cancel for minutes. Now create takes the watch
        //      lock, then the global lock only to check the id and write a
        //      placeholder, and reads the baseline under the watch lock alone.
        //      Proof, with create A parked inside its baseline read (a gate,
        //      no sleeps): (1) the global lock is free, (2) a second create
        //      "B" completes within 2 s, (3) A's watch lock is held. Then A's
        //      gate opens and A completes with its baseline recorded, and a
        //      second create of A's id, started while A was still reading,
        //      ends as a DuplicateId (V6: whether it waited on A's watch lock
        //      or saw the reserved id at once is the implementation's choice;
        //      it must not create a second watch or hang).
        // proves: CLAIM(create-lock-scope)
        let (_clock, env) = fixture();
        let gate = Gate::closed();
        env.reader.set_gate("cmd:slow-a", Some(gate.clone()));
        let path = env.input_path("b.log");
        write_file(&path, "b\n");
        let mut request_a = change_request("a", &path, 20);
        request_a.inputs = vec![gated_cmd("slow-a")];

        let vigil = Arc::new(env.another_process());
        let create_a = tokio::spawn({
            let vigil = vigil.clone();
            let request = request_a.clone();
            async move { vigil.create(request).await }
        });
        tokio::time::timeout(Duration::from_secs(5), env.reader.wait_started("cmd:slow-a", 1)).await.expect("create A never reached its baseline read");

        let store = env.vigil.store();
        let global = store.try_lock().unwrap();
        assert!(global.is_some(), "the global lock is held while create A reads its baseline");
        drop(global);
        let created_b = tokio::time::timeout(Duration::from_secs(2), env.vigil.create(change_request("b", &path, 20)))
            .await
            .expect("create B blocked behind create A's baseline read");
        assert_eq!(created_b.unwrap().watch_id.as_str(), "b");
        let duplicate_a = tokio::spawn({
            let vigil = Arc::new(env.another_process());
            let request = request_a.clone();
            async move { vigil.create(request).await }
        });
        assert!(store.try_lock_watch(&wid("a")).unwrap().is_none(), "A's watch lock is held across its baseline");
        assert!(!create_a.is_finished());

        gate.open();
        let duplicate = tokio::time::timeout(Duration::from_secs(5), duplicate_a).await.expect("duplicate create hung").unwrap();
        assert_eq!(duplicate, Err(VigilError::Create(CreateError::DuplicateId("a".into()))), "the id is reserved before the baseline is read");
        let created_a = tokio::time::timeout(Duration::from_secs(5), create_a).await.unwrap().unwrap().unwrap();
        assert_eq!(created_a.watch_id.as_str(), "a");
        let file = store.load(&wid("a")).unwrap();
        assert_eq!((file.tick, file.input_states.len()), (0, 1));
        assert!(file.input_states[0].hash.is_some(), "the baseline of A's input was recorded");
        assert!(store.try_lock_watch(&wid("a")).unwrap().is_some(), "A's watch lock is released after create");
    }
}
