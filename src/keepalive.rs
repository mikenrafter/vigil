//! The blocking `keepalive` call.

use std::time::Duration;

use tokio::time::Instant;

use std::collections::{BTreeMap, BTreeSet};

use crate::attention::{Attention, LONG_WINDOW_S};
use crate::cache::{choose_window, CacheNote, WindowRequest};
use crate::clock::Timestamp;
use crate::credit::CreditCheck;
use crate::engine::{expired_if_overdue, Persisted};
use crate::envelope::{cut_digest, ClockNote, Envelope, Next, OtherWatch, PersistNote, SecondaryFailure};
use crate::error::VigilError;
use crate::monitor::CreditPlan;
use crate::tick::{digest_of, is_due, Retained, TickOutcome, TickReport};
use crate::watch::{ChangedInput, Event, FireRule, Input, WatchFile, WatchId};

/// `waited_s`: the wait rounded to the nearest second. Deadlines are anchored to
/// stored instants, which keep whole seconds only, while a call can start a few
/// milliseconds past a second; flooring then reported 599 for a 600 s wait.
fn waited_secs(waited: Duration) -> u64 {
    waited.as_secs() + u64::from(waited.subsec_millis() >= 500)
}

/// CLAIM(S4-outbox): how long a waiting call may sleep before it looks at the
/// watches' outboxes again. Attention that another process saves (and then
/// fails to hand over) is found at the next pass; a watch that ticks rarely
/// has no pass of its own for a long time, so this bounds the wait to 5 s
/// [review S4; auto_interval_tests::a_waiting_call_returns_attention_that_another_process_saved_but_could_not_deliver].
/// Each look is one small file read per covered watch.
pub const OUTBOX_POLL: Duration = Duration::from_secs(5);

pub const MIN_MAX_S: u64 = 30;
pub const MAX_MAX_S: u64 = 270;
pub const DEFAULT_MAX_S: u64 = 270;

/// Clamp the agent's `max_s` to 30..=270 (absent = 270). Never rejects.
pub fn clamp_max_s(requested: Option<u64>) -> u64 {
    requested.map_or(DEFAULT_MAX_S, |seconds| seconds.clamp(MIN_MAX_S, MAX_MAX_S))
}

/// X4: a tick is started inside a call only if the call has at least this
/// much budget left: `max(read_timeout of every input) + judge_timeout (only
/// for fire = judge, the only kind that calls the judge) + 1 s`. Default
/// taken: what the watch can actually spend, so file/glob/process inputs and
/// change/condition watches reserve only the 1 s slack (a literal reading
/// would add the judge timeout and 10 s to every watch). Exception: the tick
/// that is due when a call starts runs regardless (a call is >= 30 s, so it
/// always finishes unless a lock wait ate the budget, V2).
pub fn tick_reserve(inputs: &[crate::watch::Input], fire_is_judge: bool, judge_timeout: std::time::Duration) -> std::time::Duration {
    let slowest_read = inputs.iter().map(Input::read_timeout).max().unwrap_or(Duration::ZERO);
    let judge = if fire_is_judge { judge_timeout } else { Duration::ZERO };
    slowest_read + judge + Duration::from_secs(1)
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct KeepaliveRequest {
    #[serde(default)]
    pub watch_id: Option<WatchId>,
    #[serde(default)]
    pub max_s: Option<u64>,
    /// AI1' (A8): this call is the main orchestrator session's, so it gets the
    /// TTL-derived window and the credit checks. `Some(false)` overrides a flag
    /// stored on the watch; `None` defers to stored flags only when every
    /// covered pending watch is confirmed (SPEC Orchestrator confirmation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestrator: Option<bool>,
    /// The day's token, from the challenge (`orchestrator.rs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
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

/// A save that needs saying: the watch as the call left it, and how its save
/// went. Clean saves are not kept (the envelope of a healthy call has no
/// `persist` field).
struct SaveRecord {
    watch: WatchFile,
    persisted: Persisted,
}

/// Everything one blocking call learned that is not (or not yet) on disk:
/// what changed (V14), how risky saves went (DF1, DF2, DF4), and which watches
/// fired events that follow a failed save (S1).
#[derive(Default)]
struct CallRecord {
    changed: Vec<ChangedInput>,
    peer_scan: crate::peers::PeerScan,
    /// Watches whose save in this call spent the reserve or failed, by id.
    saves: BTreeMap<WatchId, SaveRecord>,
    /// S1: watches that ticked in this call right after a failed save.
    redelivered: BTreeSet<WatchId>,
    /// S6: ids of the attention events already in the covered watches' files
    /// when the call began; whatever is returned of them is a redelivery.
    carried: BTreeSet<String>,
    /// AI5: some covered watch raised attention in this call (an attention
    /// pass, or a tick whose hold items ended, W42-H3).
    raised_attention: bool,
    /// AI8: the call's credit event, if the check saw a change (not saved, A18).
    credit_events: Vec<Event>,
    reminder_events: Vec<Event>,
    /// AI2': the newest credit check of this call.
    credit_check: Option<CreditCheck>,
}

impl CallRecord {
    /// Take in one finished tick of `id`.
    fn absorb(&mut self, id: &WatchId, report: TickReport) {
        if report.outcome == TickOutcome::Ticked {
            self.changed.extend(report.changed);
        }
        if report.redelivered {
            self.redelivered.insert(id.clone());
        }
        // W42-H3: items that ended while their hold stays live are attention.
        self.raised_attention |= report.raised_attention;
        let Some(watch) = report.after else { return };
        self.note_save(id, watch, report.persisted);
    }

    /// Remember a save that needs saying. A clean save says nothing; a reserve
    /// spent earlier in the call stays the note unless a later save fails.
    fn note_save(&mut self, id: &WatchId, watch: WatchFile, persisted: Persisted) {
        let nothing_new = match persisted {
            Persisted::Saved => true,
            Persisted::SavedOnReserve => self.saves.contains_key(id),
            Persisted::NotSaved(_) => false,
        };
        if !nothing_new {
            self.saves.insert(id.clone(), SaveRecord { watch, persisted });
        }
    }

    /// Does this call end now because the agent has something to act on that is
    /// not a finished watch (attention, a credit change, or an attention event
    /// that was never acknowledged)?
    fn has_events_for_the_agent(&self) -> bool {
        self.raised_attention || !self.reminder_events.is_empty() || !self.peer_scan.events.is_empty() || !self.credit_events.is_empty() || !self.carried.is_empty()
    }

    /// DF1a: a save that failed ends the call.
    fn any_unsaved(&self) -> bool {
        self.saves.values().any(|record| matches!(record.persisted, Persisted::NotSaved(_)))
    }

    /// The shortest `interval_s` among the covered watches whose save failed
    /// (the pace of the hold, DF1a); `None` when every save worked.
    fn shortest_unsaved_interval(&self) -> Option<u64> {
        self.saves
            .values()
            .filter(|record| matches!(record.persisted, Persisted::NotSaved(_)))
            .map(|record| record.watch.spec.interval_s)
            .min()
    }

    /// The in-memory watch of a covered watch whose save failed: the call
    /// answers from it, the file on disk is the previous one.
    fn unsaved_watch(&self, id: &WatchId) -> Option<&WatchFile> {
        self.saves.get(id).filter(|record| matches!(record.persisted, Persisted::NotSaved(_))).map(|record| &record.watch)
    }

    /// The `persist` note of the envelope that describes `primary`. Its own
    /// save first; otherwise, if any covered watch had to spend the reserve,
    /// that (the agent should learn the disk is full whichever watch noticed).
    fn note_for(&self, primary: &WatchId, redelivery: bool) -> Option<PersistNote> {
        if let Some(note) = self.saves.get(primary).and_then(|record| record.persisted.note(redelivery)) {
            return Some(note);
        }
        self.saves.values().any(|record| record.persisted == Persisted::SavedOnReserve).then_some(PersistNote::ReserveSpent)
    }

    /// S5: every covered watch but `primary` whose save failed, in id order.
    fn secondary_failures(&self, primary: &WatchId) -> Vec<SecondaryFailure> {
        self.saves
            .iter()
            .filter(|(id, _)| *id != primary)
            .filter_map(|(id, record)| match &record.persisted {
                Persisted::NotSaved(failure) => Some(SecondaryFailure {
                    watch_id: id.clone(),
                    kind: failure.kind,
                    error: failure.error.clone(),
                    path: failure.path.clone(),
                }),
                _ => None,
            })
            .collect()
    }
}

/// Poll `futures` together and hand each output to `on_done` as soon as it is
/// ready (completion order, ties by position). `on_done` returns `Ok(true)`
/// to stop: the completions that are already ready are still collected, then
/// what is unfinished is dropped (C12). Stops at the first `Err`, dropping
/// what is still pending. Used so that one slow tick never holds back
/// another's result or its commit (C1).
async fn for_each_completed<F, E>(
    futures: impl IntoIterator<Item = F>,
    mut on_done: impl FnMut(F::Output) -> Result<bool, E>,
) -> Result<(), E>
where
    F: std::future::Future,
{
    let mut pending: Vec<std::pin::Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    let mut stopping = false;
    std::future::poll_fn(|cx| {
        loop {
            let mut index = 0;
            let mut completed_any = false;
            while index < pending.len() {
                match pending[index].as_mut().poll(cx) {
                    std::task::Poll::Ready(output) => {
                        drop(pending.remove(index));
                        completed_any = true;
                        match on_done(output) {
                            Err(error) => return std::task::Poll::Ready(Err(error)),
                            Ok(stop) => stopping |= stop,
                        }
                    }
                    std::task::Poll::Pending => index += 1,
                }
            }
            if pending.is_empty() || (stopping && !completed_any) {
                return std::task::Poll::Ready(Ok(()));
            }
            if !stopping {
                return std::task::Poll::Pending;
            }
            // Stopping: one more pass to pick up what the last completion readied.
        }
    })
    .await
}

/// What one call is, decided before it starts: the window and the note that
/// explains it (AI1', AI3), and whether it is the orchestrator's (S2).
struct CallPlan {
    orchestrator: bool,
    window: CacheNote,
}

impl CallPlan {
    /// AI5 (A1): is the window longer than the default 270 s?
    fn is_long(&self) -> bool {
        self.window.interval_s > LONG_WINDOW_S
    }
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
        let outcome = self.keepalive_cancellable(request, progress, std::future::pending::<()>()).await?;
        Ok(outcome.expect("a call that cannot be cancelled always finishes"))
    }

    /// `keepalive_with_progress` that stops when `cancel` completes (C2): the
    /// sensing is dropped, nothing more is written, and NO result is marked
    /// returned (V7), so a later no-id call still delivers it. `Ok(None)` is
    /// "cancelled". `cancel` is polled first, so once it is ready the call
    /// cannot go on to mark anything returned.
    pub async fn keepalive_cancellable(
        &self,
        request: KeepaliveRequest,
        progress: &dyn ProgressSink,
        cancel: impl std::future::Future<Output = ()>,
    ) -> Result<Option<Envelope>, VigilError> {
        Ok(self.keepalive_uncommitted(request, progress, cancel).await?.map(|(envelope, delivery)| {
            self.commit_delivery(delivery);
            envelope
        }))
    }

    /// `keepalive_cancellable` that leaves the results unmarked: the caller
    /// must `commit_delivery` once the envelope is actually handed over (C2).
    pub async fn keepalive_uncommitted(
        &self,
        request: KeepaliveRequest,
        progress: &dyn ProgressSink,
        cancel: impl std::future::Future<Output = ()>,
    ) -> Result<Option<(Envelope, Delivery)>, VigilError> {
        let mut outcome = self.blocking_call(request, progress, cancel).await?;
        if let Some((envelope, _)) = outcome.as_mut() {
            // CLAIM(advisory-delivery) ↳ every blocking return of keepalive carries `advice`
            envelope.advice = self.envelope_advice().await;
            envelope.reminders = self.envelope_reminders().await;
            envelope.reminders_error = self.reminders_error();
        }
        Ok(outcome)
    }

    /// One blocking call; the watch files record its return as "the last
    /// call" (AI4).
    async fn blocking_call(
        &self,
        request: KeepaliveRequest,
        progress: &dyn ProgressSink,
        cancel: impl std::future::Future<Output = ()>,
    ) -> Result<Option<(Envelope, Delivery)>, VigilError> {
        let covered = self.covered_ids(request.watch_id.as_ref())?;
        let orchestrator = self.gate_orchestrator(request.orchestrator, request.confirm.as_deref(), &covered, request.watch_id.as_ref())?;
        let request = KeepaliveRequest { orchestrator, ..request };
        let plan = self.plan_call(&request, &covered);
        let max_s = plan.window.interval_s;
        let started = Instant::now();
        let budget_end = started + Duration::from_secs(max_s);
        // ↳ CLAIM(peers-registry): keep the guard through return, error or cancellation.
        let _registration = self.register_call(&covered, plan.orchestrator, plan.window.interval_s).ok();
        let finish = async {
            let call = self.sense(&covered, budget_end, progress, &plan).await?;
            let answer = self.keepalive_envelope(&covered, &call, &plan, waited_secs(started.elapsed()))?;
            // ↳ CLAIM(DF1a-hold): the one pacing for MCP and CLI keepalive [spec DF1a revised, review T1; disk_full_tests a_quiet_tick_that_cannot_save_is_held_for_one_interval_and_not_ticked_again, mcp::tests the_mcp_call_is_held_one_interval_after_a_quiet_failed_save_and_a_cancel_ends_the_hold]
            let answer = match self.hold_after_unsaved_quiet_tick(&answer.0, &call, budget_end) {
                None => answer,
                Some(hold) => {
                    tokio::time::sleep(hold).await;
                    // The watch may have passed its deadline while the call slept: the
                    // envelope describes it as it is now (V3), still without ticking.
                    self.keepalive_envelope(&covered, &call, &plan, waited_secs(started.elapsed()))?
                }
            };
            self.remember_last_call(&covered, &call, answer.0.now).await;
            Ok(answer)
        };
        tokio::select! {
            biased;
            () = cancel => Ok(None),
            envelope = finish => envelope.map(Some),
        }
    }

    /// What this call is: its window (AI1', AI3), whether it is the
    /// orchestrator's (A8, S10) and so whether it checks credit state.
    fn plan_call(&self, request: &KeepaliveRequest, covered: &[WatchId]) -> CallPlan {
        // A8: the caller's flag wins, `Some(false)` included; else any covered
        // pending watch that stores the flag makes this an orchestrator call.
        let orchestrator = request.orchestrator.unwrap_or_else(|| {
            covered.iter().filter_map(|id| self.store.load(id).ok()).any(|watch| !watch.status.is_finished() && watch.spec.orchestrator)
        });
        let window = choose_window(&WindowRequest {
            explicit_s: request.max_s,
            orchestrator,
            agent: &self.agent,
            projects_dir: self.config.claude_projects_dir.as_deref(),
        });
        CallPlan { orchestrator, window }
    }

    /// AI4 / A13: record on every covered, still pending watch that an
    /// envelope was returned at `now`. Best effort and silent (a full disk
    /// must not cost the call its answer or hold it): one non-blocking try for
    /// the watch's lock, one write without the reserve, errors ignored. A watch
    /// whose save failed in this call is left alone.
    async fn remember_last_call(&self, covered: &[WatchId], call: &CallRecord, now: Timestamp) {
        for id in covered {
            if call.unsaved_watch(id).is_some() {
                continue;
            }
            let Ok(Some(_lock)) = self.store.try_lock_watch_async(id).await else { continue };
            let Ok(mut watch) = self.store.load(id) else { continue };
            if watch.status.is_finished() {
                continue;
            }
            watch.last_call_at = Some(now);
            let _ = self.store.save_quietly(&watch);
        }
    }

    /// The blocking part of a call: tick covered watches until one finishes,
    /// one cannot be saved (DF1a: the call stops sensing and answers rather
    /// than ticking a full disk in a loop; a quiet answer is then held one
    /// interval by the caller), or the budget ends. Returns what the
    /// call learned (V14, DF1).
    ///
    /// Each round ticks every due covered watch CONCURRENTLY inside the shared
    /// budget, each under its own watch lock (C1), so a slow watch cannot
    /// starve another. A tick runs only if the budget left covers its reserve
    /// (X4), except the one due when the call starts. Expiry never needs a
    /// reserve and its deadline wake is kept when sensing is skipped (C7).
    async fn sense(&self, covered: &[WatchId], budget_end: Instant, progress: &dyn ProgressSink, plan: &CallPlan) -> Result<CallRecord, VigilError> {
        let mut call = CallRecord::default();
        let mut ticks_in_call = 0u64;
        let mut first_pass = true;
        let mut credit = plan.orchestrator.then(|| self.credit_plan(plan.window.interval_s));
        let mut reminder_check = Instant::now();
        loop {
            // CLAIM(reminder-event): one claim per reminder/period, only during
            // long orchestrator calls [SPEC Reminders; reminder_tests mid_wait].
            if plan.orchestrator && plan.is_long() && Instant::now() >= reminder_check {
                let anchor = self.load_watch(&covered[0])?;
                if let Ok(events) = tokio::time::timeout_at(budget_end, self.reminder_events(&anchor)).await {
                    call.reminder_events.extend(events);
                }
                reminder_check = Instant::now() + Duration::from_secs(30);
            }
            // CLAIM(peers-scope): plain calls never scan peers [pilot; peers_tests].
            // CLAIM(peers-flag): every existing sense pass sees foreign
            // orchestrators and ends the call [pilot; peers_tests].
            // CLAIM(peers-info): ordinary peers ride the return as information,
            // refreshed each pass, never as an early wake [pilot; peers_tests].
            if plan.orchestrator {
                call.peer_scan = self.scan_peers(covered);
            }
            // S6, review S4: attention that was raised and saved but never
            // acknowledged (the transport died, or another process saved it
            // while this call waited) is delivered at once, ahead of any
            // sensing. Looked at on EVERY pass, not only at the start of the
            // call. Every way this call raises attention ends the call, so
            // whatever a pending outbox holds at the start of a pass was put
            // there by someone else and is a repeat.
            // C12: a covered watch that is already terminal (finished before
            // this call, or by another process) is returned before any round
            // is awaited, so a held lock elsewhere cannot hold it back.
            // ↳ CLAIM(S4-outbox)
            // ↳ CLAIM(S3-credit): both returns still owe the orchestrator's due check
            let mut any_terminal = false;
            for id in covered {
                let watch = self.load_watch(id)?;
                if watch.status.is_finished() {
                    any_terminal = true;
                } else {
                    call.carried.extend(watch.events.iter().map(|event| event.event_id.clone()));
                }
            }
            if any_terminal || call.has_events_for_the_agent() {
                self.due_credit_check(&mut credit, covered, budget_end, &mut call).await?;
                return Ok(call);
            }
            let now = self.clock.now();
            let mut round: Vec<&WatchId> = Vec::new();
            for id in covered {
                let watch = self.load_watch(id)?;
                if watch.status.is_finished() {
                    continue;
                }
                if watch.is_overdue(now) || (is_due(&watch, now) && (first_pass || !self.reserve_blocks(&watch, budget_end))) {
                    round.push(id);
                }
            }
            // V2: a tick that cannot finish in the budget is dropped before it
            // writes anything (it also checks the deadline itself, C5).
            // Review S4: each tick leaves what it has sensed in its own slot
            // before its save can wait, so a tick dropped inside that wait
            // (the budget, or the round ending below) still has an answer.
            let retained: Vec<Retained> = round.iter().map(|_| Retained::default()).collect();
            let ticks = round.iter().zip(&retained).map(|(&id, slot)| async move {
                (id, tokio::time::timeout_at(budget_end, self.tick_report_retaining(id, Some(budget_end), slot)).await)
            });
            // Take in one finished or retained tick report; `Ok(true)` ends the round.
            let mut take_in = |id: &WatchId, report: TickReport| -> Result<bool, VigilError> {
                let outcome = report.outcome;
                // The watch as the tick left it: saved or not, this is the truth.
                let after = report.after.as_ref().map(|watch| (watch.tick, watch.status.is_finished()));
                call.absorb(id, report);
                let unsaved = call.any_unsaved();
                if outcome == TickOutcome::Ticked {
                    ticks_in_call += 1;
                    let (tick, finished) = match after {
                        Some(after) => after,
                        None => {
                            let watch = self.load_watch(id)?;
                            (watch.tick, watch.status.is_finished())
                        }
                    };
                    progress.progress(&ProgressUpdate { progress: ticks_in_call, watch_id: id.clone(), tick });
                    return Ok(finished || unsaved);
                }
                Ok(matches!(outcome, TickOutcome::Finished(_)) || unsaved)
            };
            // C12: once a covered watch has a terminal result the round stops
            // waiting for unfinished ticks (they write nothing, C5).
            // ↳ CLAIM(DF1a): so does a tick whose save failed, the moment it is absorbed: the other ticks of the round may be waiting on a lock held elsewhere, and the call must not wait for them [spec DF1a, review S1; disk_full_tests a_quiet_failed_save_ends_the_call_without_waiting_for_a_watch_whose_lock_is_held]
            for_each_completed(ticks, |(id, outcome)| -> Result<bool, VigilError> {
                let Ok(report) = outcome else { return Ok(false) };
                take_in(id, report?)
            })
            .await?;
            // ↳ CLAIM(S4-retained): a tick dropped inside its save (the round ended, or the budget did) is not lost: its slot still holds what it had sensed and how the write stood [review S4; disk_full_tests a_rescue_still_waiting_when_another_rescue_gives_up_keeps_its_answer]
            for (&id, slot) in round.iter().zip(&retained) {
                if let Some(report) = slot.take() {
                    if let Some(after) = &report.after {
                        self.remember_save(after, &report.persisted);
                    }
                    take_in(id, report)?;
                }
            }
            first_pass = false;
            // ↳ CLAIM(DF1a): a tick whose save failed ends the call; no in-memory re-ticking, no spin [spec DF1a; disk_full_tests a_quiet_tick_that_cannot_save_ends_the_call_at_once_and_is_not_ticked_again]
            if call.any_unsaved() {
                return Ok(call);
            }

            // AI5: a covered watch that has run too long, or gone quiet while its
            // process runs, raises its attention event (after the round, so a
            // watch that just finished raises nothing, A15, and one that fired
            // in the same round still gets the secondary event, S10).
            for id in covered {
                self.raise_attention(id, plan.is_long(), budget_end, &mut call).await?;
            }
            if call.any_unsaved() {
                return Ok(call);
            }

            let now = self.clock.now();
            let mut any_finished = false;
            let mut next_wake: Option<Timestamp> = None;
            for id in covered {
                let watch = self.load_watch(id)?;
                if watch.status.is_finished() {
                    any_finished = true;
                    continue;
                }
                let due_at = watch.last_tick_at.map_or(now, |last| last.plus_secs(watch.spec.interval_s));
                // A due tick the reserve forbids is not waited for; its deadline still is.
                let wake = if is_due(&watch, now) && self.reserve_blocks(&watch, budget_end) { watch.deadline_at } else { due_at.min(watch.deadline_at) };
                // AI5, W42-D2: an overdue moment (the watch's or an item's) is a wake of its own, however rarely the watch ticks.
                let wake = crate::attention::next_due_at(&watch).map_or(wake, |overdue| wake.min(overdue.max(now)));
                next_wake = Some(next_wake.map_or(wake, |current| current.min(wake)));
            }
            // AI2' / S2 / S3: the orchestrator's credit check, after the watches
            // had their pass, on every way out of the call (a quiet pass, a
            // finished watch, new attention): a call that always returns early
            // would otherwise never monitor.
            // ↳ CLAIM(S3-credit)
            if any_finished || call.raised_attention {
                self.due_credit_check(&mut credit, covered, budget_end, &mut call).await?;
                return Ok(call);
            }
            if self.due_credit_check(&mut credit, covered, budget_end, &mut call).await? {
                return Ok(call);
            }
            if let Some(due) = credit.as_ref().and_then(CreditPlan::next_due_at) {
                next_wake = Some(next_wake.map_or(due, |current| current.min(due)));
            }
            let left = budget_end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(call);
            }
            // Sleep to the next tick or deadline, or to the end of the budget.
            let until_wake = next_wake
                .map(|wake| (wake.0 - self.clock.now().0).to_std().unwrap_or(Duration::ZERO))
                .unwrap_or(left);
            // ↳ CLAIM(S4-outbox): never sleep past the next look at the outboxes
            tokio::time::sleep(until_wake.min(left).min(OUTBOX_POLL)).await;
        }
    }

    /// AI2' / S2 / S3: make the orchestrator's credit check if the plan says one
    /// is due now (a call that is not the orchestrator's has no plan and checks
    /// nothing). The check is recorded in `call` for `cache.credit_check`; when
    /// it saw a change, its event is added and `true` says the call ends. Never
    /// fails the call because of the check itself (see `run_credit_step`).
    ///
    /// CLAIM(S3-credit): every return path of a blocking call runs this once
    /// before it returns, so the cadence holds for calls that always end early
    /// [review S3; auto_interval_tests::a_call_that_returns_carried_attention_still_makes_its_due_credit_check
    /// and the three tests after it].
    async fn due_credit_check(&self, credit: &mut Option<CreditPlan>, covered: &[WatchId], budget_end: Instant, call: &mut CallRecord) -> Result<bool, VigilError> {
        let Some(credit) = credit.as_mut() else { return Ok(false) };
        if !credit.is_due(self.clock.now()) {
            return Ok(false);
        }
        let anchor = self.load_watch(&covered[0])?;
        let step = self.run_credit_step(credit, budget_end, (&anchor.spec.id, anchor.tick)).await?;
        if step.check.is_some() {
            call.credit_check = step.check;
        }
        let changed = step.event.is_some();
        call.credit_events.extend(step.event);
        Ok(changed)
    }

    /// DF1a (revised, review T1): how long to hold a call whose answer is
    /// pending (no events, no terminal status) while a save failed in it. A
    /// failed save leaves the watch due again at once, so a call that simply
    /// returned would let an agent on a disk that stays full call back to back.
    /// The hold is `min(interval, budget left)`, where the interval is the
    /// shortest of the watches that could not be saved (the cadence a healthy
    /// call would tick at), it does NOT tick again, and it runs inside the
    /// cancellable call. `None`: return at once (nothing failed, or the
    /// envelope has events or a terminal status the agent must act on, or no
    /// budget is left).
    fn hold_after_unsaved_quiet_tick(&self, envelope: &Envelope, call: &CallRecord, budget_end: Instant) -> Option<Duration> {
        if envelope.status.is_finished() || !envelope.events.is_empty() {
            return None;
        }
        let interval = call.shortest_unsaved_interval()?;
        let hold = Duration::from_secs(interval).min(budget_end.saturating_duration_since(Instant::now()));
        (!hold.is_zero()).then_some(hold)
    }

    /// AI5 / S6, W42-H3: raise the attention events that are due for `id` (the
    /// watch's own reasons and its items' overdue), if any.
    /// Cheap when nothing is due (one read, no lock). Otherwise the watch's
    /// lock is taken, the rule is evaluated again under it, and the events are
    /// appended to the watch's outbox together with the once-per-reason flags
    /// in ONE save: either both are on disk or neither is (a failed save raises
    /// them again at the next pass, with the same ids).
    ///
    /// CLAIM(V2-attention): like every other wait in a call, the lock wait ends
    /// at `budget_end`, and nothing is saved after it [review S1;
    /// auto_interval_tests::attention_due_at_the_end_of_the_budget_does_not_wait_for_a_watch_lock_held_elsewhere].
    /// An attention that could not be raised in time is still due, so the next
    /// call raises it at once.
    async fn raise_attention(&self, id: &WatchId, long_window: bool, budget_end: Instant, call: &mut CallRecord) -> Result<(), VigilError> {
        if crate::attention::due(&self.load_watch(id)?, self.clock.now(), long_window).is_empty() {
            return Ok(());
        }
        // ↳ CLAIM(V2-attention)
        let Ok(locked) = tokio::time::timeout_at(budget_end, self.lock_and_load(id)).await else { return Ok(()) };
        let (_lock, mut watch) = locked?;
        // The wait may have used the last of the budget: the same strict test a
        // tick makes before it persists (C5).
        if Instant::now() > budget_end {
            return Ok(());
        }
        let now = self.clock.now();
        self.forget_unsaved_attention(&watch);
        let due = crate::attention::due(&watch, now, long_window);
        if due.is_empty() {
            return Ok(());
        }
        // S1, decided before this save changes the answer.
        let repeating = self.may_repeat(id);
        // ↳ CLAIM(S5-identity): an event whose save failed was shown to the
        // agent; the retry is that event, not a new one built from a watch a
        // tick may have moved on [review S5;
        // auto_interval_tests::an_attention_event_keeps_its_id_when_a_tick_lands_between_a_failed_save_and_the_retry].
        // The remembered ones go in first so a new event's id (counted from
        // what is already there) cannot collide with one of them.
        let mut raised: Vec<(Attention, Event)> = Vec::new();
        let mut fresh: Vec<Attention> = Vec::new();
        for attention in due {
            match self.unsaved_attention(id, attention) {
                Some(event) => raised.push((attention, event)),
                None => fresh.push(attention),
            }
        }
        let any_retried = !raised.is_empty();
        for (attention, event) in &raised {
            watch.events.push(event.clone());
            crate::attention::mark_raised(&mut watch, *attention);
        }
        for attention in fresh {
            // Built before it is marked: a watch-level id counts the reasons raised before it.
            let Some(event) = crate::attention::event(&watch, attention, now) else { continue };
            watch.events.push(event.clone());
            crate::attention::mark_raised(&mut watch, attention);
            raised.push((attention, event));
        }
        let persisted = self.persist(&watch, Some(budget_end)).await?;
        self.remember_attention_save(id, &raised, &persisted);
        if repeating || any_retried {
            call.redelivered.insert(id.clone());
        }
        call.raised_attention = true;
        call.note_save(id, watch, persisted);
        Ok(())
    }

    /// X4: true when the budget left is below the watch's tick reserve.
    fn reserve_blocks(&self, watch: &WatchFile, budget_end: Instant) -> bool {
        let reserve = tick_reserve(&watch.spec.inputs, matches!(watch.spec.fire, FireRule::Judge { .. }), self.config.judge_timeout);
        budget_end.saturating_duration_since(Instant::now()) < reserve
    }

    /// The watches a call covers. With an id: that watch (`NotFound` if it
    /// does not exist). Without: this process's pending watches and its
    /// finished watches whose result it has not yet returned (V7, D2).
    fn covered_ids(&self, requested: Option<&WatchId>) -> Result<Vec<WatchId>, VigilError> {
        if let Some(id) = requested {
            self.load_watch(id)?;
            return Ok(vec![id.clone()]);
        }
        let session = self.session.lock().expect("session mutex poisoned");
        let covered: Vec<WatchId> = session
            .created
            .iter()
            .filter(|id| !session.returned.contains(*id))
            .filter(|id| self.store.load(id).is_ok())
            .cloned()
            .collect();
        if covered.is_empty() {
            return Err(VigilError::NothingToCover);
        }
        Ok(covered)
    }

    /// Build the return value of a call (V8, V14, X1, S5) and record what it
    /// delivered. A covered watch whose save failed in this call is read from
    /// `call` (its file is the previous one); every other from disk.
    fn keepalive_envelope(&self, covered: &[WatchId], call: &CallRecord, plan: &CallPlan, waited_s: u64) -> Result<(Envelope, Delivery), VigilError> {
        let now = self.clock.now();
        let watches: Vec<WatchFile> = covered
            .iter()
            .map(|id| match call.unsaved_watch(id) {
                Some(unsaved) => Ok(expired_if_overdue(unsaved.clone(), now)),
                None => self.load_watch(id).map(|watch| expired_if_overdue(watch, now)),
            })
            .collect::<Result<_, _>>()?;
        // V8: earliest finished first, ties by id; else the nearest deadline.
        let mut finished: Vec<&WatchFile> = watches.iter().filter(|watch| watch.status.is_finished()).collect();
        finished.sort_by(|a, b| (a.finished_at, &a.spec.id).cmp(&(b.finished_at, &b.spec.id)));
        let primary = match finished.first() {
            Some(first) => (*first).clone(),
            None => watches
                .iter()
                .min_by(|a, b| (a.deadline_at, &a.spec.id).cmp(&(b.deadline_at, &b.spec.id)))
                .expect("a call covers at least one watch")
                .clone(),
        };

        // S1: events of a tick that follows a failed save say so. Review S2:
        // so do stored events this process replays while it may be repeating
        // them; the in-memory copy of an unsaved result is only a repeat if
        // its own tick followed a failed save (it is the first delivery).
        let mut events: Vec<Event> = finished
            .iter()
            .flat_map(|watch| {
                let own_tick_followed_failed_save = call.redelivered.contains(&watch.spec.id);
                let stored: Vec<Event> =
                    if call.unsaved_watch(&watch.spec.id).is_some() { watch.events.clone() } else { self.replayed_events(watch) };
                stored.into_iter().map(move |event| Event { redelivered: event.redelivered || own_tick_followed_failed_save, ..event })
            })
            .collect();
        // S6: the attention events of the pending covered watches ride along,
        // whichever watch is the primary. One that was already in the file when
        // the call began, or whose raising followed a failed save, is a repeat.
        let mut acknowledgements: Vec<(WatchId, Vec<String>)> = Vec::new();
        for watch in watches.iter().filter(|watch| !watch.status.is_finished() && !watch.events.is_empty()) {
            let unsaved = call.unsaved_watch(&watch.spec.id).is_some();
            let repeat_of_failed_save = call.redelivered.contains(&watch.spec.id);
            events.extend(watch.events.iter().map(|event| Event {
                redelivered: event.redelivered || repeat_of_failed_save || call.carried.contains(&event.event_id),
                ..event.clone()
            }));
            if !unsaved {
                acknowledgements.push((watch.spec.id.clone(), watch.events.iter().map(|event| event.event_id.clone()).collect()));
            }
        }
        events.extend(call.credit_events.iter().cloned());
        events.extend(call.reminder_events.iter().cloned());
        events.extend(call.peer_scan.events.iter().cloned());
        let others: Vec<OtherWatch> = finished
            .iter()
            .filter(|watch| watch.spec.id != primary.spec.id)
            .map(|watch| OtherWatch {
                watch_id: watch.spec.id.clone(),
                status: watch.status,
                event_ids: watch.events.iter().map(|event| event.event_id.clone()).collect(),
            })
            .collect();
        // ↳ CLAIM(DF3): a result that could not be saved is not "returned", so the next no-id call covers it again [spec DF3, V7; disk_full_tests a_no_id_keepalive_does_not_count_an_unsaved_result_as_returned]
        let delivered: Vec<WatchId> =
            finished.iter().map(|watch| watch.spec.id.clone()).filter(|id| call.unsaved_watch(id).is_none()).collect();

        let mut envelope = self.envelope_for(&primary, now)?;
        // Not yet recorded: the caller commits once the envelope is really
        // handed over (C2). The count shown already includes this return.
        envelope.session_ticks += 1;
        envelope.waited_s = Some(waited_s);
        envelope.max_s = Some(plan.window.interval_s);
        envelope.events = events;
        envelope.peers = call.peer_scan.peers.clone();
        envelope.others = others;
        if !primary.status.is_finished() {
            envelope.digest = cut_digest(&digest_of(&call.changed), self.config.digest_max_bytes);
            // A17: an attention or credit event is something to act on, though
            // the watch is still live.
            if envelope.events.iter().any(Event::is_call_ending) {
                envelope.next = Next::ActOnEvents;
            }
        }
        // DF3a: at risk when the envelope has events or a terminal status.
        let at_risk = !envelope.events.is_empty() || envelope.status.is_finished();
        envelope.persist = call.note_for(&primary.spec.id, at_risk);
        envelope.persist_failures = call.secondary_failures(&primary.spec.id);
        // AI3, AI4: how the window was chosen, and the clock the agent cannot see.
        envelope.cache = Some(CacheNote { credit_check: call.credit_check.clone(), ..plan.window.clone() });
        let since_last_call_s = primary.last_call_at.map(|at| now.secs_since(at).max(0) as u64);
        envelope.clock = Some(ClockNote::new(waited_s, envelope.elapsed_s, since_last_call_s));
        Ok((envelope, Delivery { returned: delivered, acknowledged: acknowledgements }))
    }

    /// Record that a call's envelope reached the caller (V7): its terminal
    /// results will not be returned again and `session_ticks` counts it. The
    /// attention events it carried are acknowledged in the watch files (S6),
    /// best effort: an acknowledgement that cannot be saved leaves the event
    /// to be delivered again, flagged `redelivered`.
    pub fn commit_delivery(&self, delivery: Delivery) {
        {
            let mut session = self.session.lock().expect("session mutex poisoned");
            session.session_ticks += 1;
            for id in &delivery.returned {
                // Review S2: delivered, so nothing of it can be a repeat any more.
                session.repeat_risk.remove(id);
            }
            session.returned.extend(delivery.returned);
        }
        for (id, event_ids) in &delivery.acknowledged {
            self.acknowledge(id, event_ids);
        }
    }

    /// Remove the acknowledged events from `id`'s outbox. Waits for the watch's
    /// lock only briefly (a tick of another process may hold it); on any
    /// failure the events stay and are redelivered.
    fn acknowledge(&self, id: &WatchId, event_ids: &[String]) {
        const PATIENCE: Duration = Duration::from_millis(250);
        let give_up = std::time::Instant::now() + PATIENCE;
        let _lock = loop {
            match self.store.try_lock_watch(id) {
                Ok(Some(lock)) => break lock,
                Ok(None) if std::time::Instant::now() < give_up => std::thread::sleep(Duration::from_millis(2)),
                Ok(None) | Err(_) => return,
            }
        };
        let Ok(mut watch) = self.store.load(id) else { return };
        let before = watch.events.len();
        watch.events.retain(|event| !event_ids.contains(&event.event_id));
        if watch.status.is_finished() || watch.events.len() == before {
            return;
        }
        let _ = self.store.save_quietly(&watch);
    }
}

/// What an envelope carries that must be marked once it is really handed over
/// (C2): terminal results (not returned again) and attention events (removed
/// from the watch's outbox, S6).
#[must_use = "an uncommitted delivery leaves its results to be returned again"]
pub struct Delivery {
    returned: Vec<WatchId>,
    acknowledged: Vec<(WatchId, Vec<String>)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waited_secs_rounds_to_the_nearest_second() {
        // why: waited_s reports a wait that may start a few ms past a second
        //      (deadlines are whole-second anchors); floor, ceil and a late
        //      threshold each misreport one of these.
        // catches: floor (599.997 -> 599), ceil (1.4 -> 2), a >= 900 ms threshold (0.5 -> 0).
        for (millis, expected) in [(499, 0), (500, 1), (599_997, 600), (1_400, 1)] {
            assert_eq!(waited_secs(Duration::from_millis(millis)), expected, "{millis} ms");
        }
    }
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
        KeepaliveRequest { watch_id: Some(id.clone()), max_s, orchestrator: None, confirm: None }
    }

    fn no_id(max_s: Option<u64>) -> KeepaliveRequest {
        KeepaliveRequest { watch_id: None, max_s, orchestrator: None, confirm: None }
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
        // AI6 adds `cache` and `clock` (about 190 bytes) to the 244-byte envelope
        // this bound was set for (400): still "a few hundred bytes".
        assert!(wire.len() < 512, "quiet envelope should be a few hundred bytes, got {}: {wire}", wire.len());
    }

    #[tokio::test(start_paused = true)]
    async fn max_s_below_the_floor_and_above_the_ceiling_are_clamped_and_reported() {
        // why: SPEC: max_s 5 -> 30, absent -> 270, "the envelope reports the
        //      value used" (V26 `max_s`), and the call really blocks that long
        //      (virtual time). 9999 -> 3000 since auto-interval A2: the ceiling
        //      of an explicit window is the largest vigil ever picks (3000), 270
        //      only for Codex or an ambiguous caller (pinned in cache.rs and the
        //      Codex e2e tests); this engine has no agent signal at all.
        // proves: CLAIM(max-s-clamp)
        for (requested, used) in [(Some(5), 30), (Some(9999), 3000), (None, 270)] {
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
        assert_eq!(envelope.events[0].event_id, "build:4:0");
        assert!(envelope.events[0].summary.contains("ERROR: linking failed"));
        assert!(envelope.digest.contains("ERROR: linking failed"));
    }

    // ---- V3: terminal result replay ----

    #[tokio::test(start_paused = true)]
    async fn a_terminal_result_is_replayed_unchanged_by_every_later_call_with_the_same_event_ids() {
        // why: V3 (S3): the terminal result is stored once and replayed on
        //      every later keepalive or watch_status; delivery is at
        //      least once, so a lost reply is recoverable. Fire at tick 2,
        //      then: keepalive (same process), keepalive (new process, later),
        //      status. Every one returns the identical events (incl.
        //      event_id "w:2:0", W1), digest, status, tick and next, instantly.
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
        assert_eq!((first.status, first.events.len(), first.events[0].event_id.as_str()), (WatchStatus::Fired, 1, "w:2:0"));

        tokio::time::advance(Duration::from_secs(1000)).await;
        let restarted = process(&env);
        let started = Instant::now();
        let replays = [
            vigil.keepalive(by_id(&id, None)).await.unwrap(),
            restarted.keepalive(by_id(&id, Some(30))).await.unwrap(),
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
        assert_eq!(ids, vec!["a-second:1:0", "z-first:1:0"]);
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

    /// Default config, except a 2 s judge timeout: under X4 a tick starts only
    /// with `judge timeout + 1 s` of budget left, so the default 10 s would
    /// stop the 24 s tick of a 30 s window in the digest test (6 s left < 11
    /// s). The tests that use this are about digests, not the reserve.
    fn judged_no_fire() -> crate::config::Config {
        let mut config = default_config();
        config.judge_timeout = Duration::from_secs(2);
        config
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

    // ---- X1: several covered watches finishing in one return ----

    #[tokio::test(start_paused = true)]
    async fn a_no_id_return_lists_every_other_finished_watch_in_others_including_expired_and_cancelled_ones() {
        // why: X1 (S8): R4/V7/V8 returned the events of all covered finished
        //      watches but an EXPIRED or CANCELLED secondary watch has no
        //      event, so it vanished (and counted as returned). Now the
        //      envelope also carries `others: [{watch_id, status, event_ids}]`
        //      for every other covered watch that finished in this return.
        //      Five own watches: "z-fired" fires at +5 s (the primary: the
        //      earliest finished, V8, although it sorts last by id),
        //      "b-fired2" fires at +10 s, "c-canc" is cancelled at +10 s,
        //      "d-exp" (deadline 100 s) is overdue at the call (+200 s), and
        //      "e-pend" is still pending. Expected: the primary is z-fired;
        //      `events` holds both fired watches' events (ids z-fired:1:0 and
        //      b-fired2:1:0); `others` lists b-fired2 (fired, its event id),
        //      c-canc (cancelled, no ids) and d-exp (expired, no ids), in any
        //      order, and NOT the primary and NOT the pending watch; the digest
        //      is the primary's only ("their digests are not repeated").
        //      Afterwards the finished ones are not returned again (the next
        //      no-id call is about e-pend alone, with no `others` on the
        //      wire), and an explicit id still replays an expired one.
        // proves: CLAIM(envelope-others)
        let env = paused_env();
        let (a, b) = (process(&env), process(&env));
        let (primary, primary_path) = quiet_watch(&env, &a, "z-fired", 20, 3600).await;
        let (second, second_path) = quiet_watch(&env, &a, "b-fired2", 20, 3600).await;
        let (cancelled, _) = quiet_watch(&env, &a, "c-canc", 20, 3600).await;
        let (expired, _) = quiet_watch(&env, &a, "d-exp", 20, 100).await;
        quiet_watch(&env, &a, "e-pend", 20, 3600).await;
        tokio::time::advance(Duration::from_secs(5)).await;
        append_file(&primary_path, "primary fired line\n");
        b.tick_due(&primary).await.unwrap();
        tokio::time::advance(Duration::from_secs(5)).await;
        append_file(&second_path, "secondary fired line\n");
        b.tick_due(&second).await.unwrap();
        b.cancel(&cancelled).await.unwrap();
        tokio::time::advance(Duration::from_secs(190)).await;

        let started = Instant::now();
        let envelope = a.keepalive(no_id(Some(30))).await.unwrap();
        assert_eq!(started.elapsed(), Duration::ZERO, "finished results are returned at once");
        assert_eq!((envelope.watch_id.as_str(), envelope.status, envelope.next), ("z-fired", WatchStatus::Fired, Next::ActOnEvents));
        let mut event_ids: Vec<&str> = envelope.events.iter().map(|e| e.event_id.as_str()).collect();
        event_ids.sort();
        assert_eq!(event_ids, vec!["b-fired2:1:0", "z-fired:1:0"], "the events of both fired watches");
        let mut others = envelope.others.clone();
        others.sort_by(|x, y| x.watch_id.cmp(&y.watch_id));
        let summary: Vec<(&str, WatchStatus, Vec<&str>)> =
            others.iter().map(|o| (o.watch_id.as_str(), o.status, o.event_ids.iter().map(|i| i.as_str()).collect())).collect();
        assert_eq!(
            summary,
            vec![
                ("b-fired2", WatchStatus::Fired, vec!["b-fired2:1:0"]),
                ("c-canc", WatchStatus::Cancelled, vec![]),
                ("d-exp", WatchStatus::Expired, vec![]),
            ]
        );
        assert!(envelope.digest.contains("primary fired line") && !envelope.digest.contains("secondary fired line"), "{:?}", envelope.digest);
        let wire = serde_json::to_value(&envelope).unwrap();
        assert_eq!(wire["others"].as_array().unwrap().len(), 3);
        assert_eq!(wire["others"][0].as_object().unwrap().keys().cloned().collect::<std::collections::BTreeSet<_>>(),
            ["event_ids", "status", "watch_id"].iter().map(|k| k.to_string()).collect());

        let after = a.keepalive(no_id(Some(30))).await.unwrap();
        assert_eq!((after.watch_id.as_str(), after.status), ("e-pend", WatchStatus::Pending), "only the pending watch is still covered");
        assert!(serde_json::to_value(&after).unwrap().get("others").is_none(), "no `others` key when nothing else finished");
        let replay = a.keepalive(by_id(&expired, None)).await.unwrap();
        assert_eq!((replay.status, replay.events.len()), (WatchStatus::Expired, 0));
    }

    // ---- W3 (T6) and X2: bounds through the engine ----

    #[tokio::test(start_paused = true)]
    async fn a_below_floor_digest_env_value_gives_an_effective_limit_of_64_through_the_engine() {
        // why: W3 + T6: digest tests used limits >= 64 only, so they passed
        //      without the clamp. Config comes from the ENVIRONMENT path with
        //      VIGIL_DIGEST_MAX_BYTES=10 (and 1): unclamped, the digest of a
        //      6 KB change is at most 10 (or 1) bytes; with the 64-byte floor
        //      it is between 41 and 64 bytes, ends with the marker and keeps
        //      the newest line. Above-floor override: 200 gives (150, 200].
        // proves: CLAIM(config-floors)
        for (raw, floor, ceiling) in [("10", 40usize, 64usize), ("1", 40, 64), ("200", 150, 200)] {
            let config = crate::config::Config::from_lookup(|name| (name == "VIGIL_DIGEST_MAX_BYTES").then(|| raw.to_string()));
            let env = paused_env_with(config);
            let vigil = process(&env);
            let (id, path) = quiet_watch(&env, &vigil, "w", 20, 3600).await;
            append_file(&path, &format!("{}LAST-LINE\n", numbered("noise", 400)));
            let envelope = vigil.keepalive(by_id(&id, None)).await.unwrap();
            assert_eq!(envelope.status, WatchStatus::Fired);
            let digest = &envelope.digest;
            assert!(digest.len() <= ceiling, "VIGIL_DIGEST_MAX_BYTES={raw}: {} bytes exceed {ceiling}: {digest:?}", digest.len());
            assert!(digest.len() > floor, "VIGIL_DIGEST_MAX_BYTES={raw}: only {} bytes, the effective limit is {ceiling}: {digest:?}", digest.len());
            assert!(digest.ends_with(DIGEST_CUT_MARKER) && digest.contains("LAST-LINE"), "{digest:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_envelope_with_sixteen_changed_inputs_stays_within_digest_limit_plus_sixteen_summaries_plus_fixed_fields() {
        // why: X2 (S12): "an envelope is at most digest limit + 16 inputs x
        //      200 bytes per covered watch, plus fixed fields". 16 file inputs
        //      each get a 3000-byte line in one tick, so 16 events. Bound
        //      derivation: digest <= limit; each summary <= 200; each event's
        //      other fields (event_id, tick, watch_id, the `file:/tmp/...`
        //      label, JSON punctuation) < 300 bytes; envelope fixed fields
        //      (watch_id, timestamps, counters, next, judge_status) < 1024
        //      bytes. For limit 256: 256 + 3200 + 4800 + 1024 = 9280 bytes;
        //      for the default 8192: 17216 bytes. Unbounded summaries alone
        //      would be 16 x 3000 = 48000 bytes. Each summary and the digest
        //      are also asserted individually.
        // proves: CLAIM(envelope-bound)
        for limit in [256usize, 8192] {
            let mut config = default_config();
            config.digest_max_bytes = limit;
            let env = paused_env_with(config);
            let vigil = process(&env);
            let paths: Vec<_> = (0..16).map(|i| env.input_path(&format!("in-{i:02}.log"))).collect();
            for path in &paths {
                write_file(path, "old\n");
            }
            let mut request = change_request("w", &paths[0], 20);
            request.inputs = paths.iter().map(|p| Input::File { path: p.clone(), tail_bytes: None }).collect();
            let id = vigil.create(request).await.unwrap().watch_id;
            for path in &paths {
                append_file(path, &format!("{}\n", "x".repeat(3000)));
            }
            let envelope = vigil.keepalive(by_id(&id, None)).await.unwrap();
            assert_eq!((envelope.status, envelope.events.len()), (WatchStatus::Fired, 16));
            for event in &envelope.events {
                assert!(event.summary.len() <= 200, "limit {limit}: summary of {} bytes", event.summary.len());
            }
            assert!(envelope.digest.len() <= limit, "limit {limit}: digest of {} bytes", envelope.digest.len());
            let wire = serde_json::to_string(&envelope).unwrap();
            let bound = limit + 16 * 200 + 16 * 300 + 1024;
            assert!(wire.len() <= bound, "limit {limit}: envelope is {} bytes, bound {bound}", wire.len());
        }
    }

    // ---- V2 + X4: one call budget, and progress under it ----

    fn cmd(name: &str, timeout_s: Option<f64>) -> Input {
        Input::Cmd { argv: vec![name.into()], timeout_s, cwd: None }
    }

    /// A watch whose inputs are scripted `cmd` inputs (see `ScriptedReader`).
    async fn scripted_watch(vigil: &Vigil, id: &str, inputs: Vec<Input>, interval_s: u64) -> crate::watch::WatchId {
        let mut request = change_request(id, std::path::Path::new("/unused"), interval_s);
        request.inputs = inputs;
        vigil.create(request).await.unwrap().watch_id
    }

    #[tokio::test(start_paused = true)]
    async fn a_reader_that_overruns_the_budget_is_cut_off_at_max_s_and_the_aborted_tick_writes_nothing() {
        // why: V2 (S2, blocker) at the engine level, rebuilt for X4: the engine
        //      itself must cap a read at the budget left even when the reader
        //      ignores its own timeout (a hung syscall, a bug). Real commands
        //      can no longer overrun a 30 s call (timeout_s <= 20 and a tick
        //      starts only with the reserve left), so the overrun is scripted:
        //      the reader takes 100 s. The tick is due at the start of the
        //      call (so it runs, X4), is cut off at exactly max_s = 30 (virtual,
        //      monotonic), and writes NOTHING: the watch file is byte-for-byte
        //      what it was (tick 0, no last_tick_at), so the line appended
        //      before the call is not consumed. After the reader recovers the
        //      next call sees the change and fires.
        // proves: CLAIM(call-budget)
        let env = paused_env();
        let vigil = process(&env);
        let path = env.input_path("w.log");
        write_file(&path, "old\n");
        env.reader.set_text("cmd:overrun", "v1");
        let mut request = change_request("w", &path, 20);
        request.inputs.push(cmd("overrun", Some(19.0)));
        let id = vigil.create(request).await.unwrap().watch_id;
        env.reader.set_sleep("cmd:overrun", Duration::from_secs(100));
        append_file(&path, "ERROR: appended before the slow call\n");
        let before = std::fs::read(vigil.store().watch_path(&id)).unwrap();

        let started = Instant::now();
        let envelope = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(30), "the call must end exactly at max_s");
        assert_eq!(envelope.status, WatchStatus::Pending);
        assert_eq!((envelope.waited_s, envelope.max_s), (Some(30), Some(30)));
        // A13: the only thing a call may add to the file is the time of its own
        // return (`last_call_at`, AI4); the tick's data must be byte-for-byte as before.
        let without_bookkeeping = |bytes: Vec<u8>| {
            let mut watch: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let removed = watch.as_object_mut().unwrap().remove("last_call_at");
            (watch, removed.is_some())
        };
        let (after_watch, had_last_call) = without_bookkeeping(std::fs::read(vigil.store().watch_path(&id)).unwrap());
        assert_eq!(after_watch, without_bookkeeping(before).0, "an aborted tick writes nothing");
        assert!(had_last_call, "the call itself did record when it returned");

        env.reader.set_sleep("cmd:overrun", Duration::ZERO);
        let next = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!(next.status, WatchStatus::Fired, "the change was not consumed by the aborted tick");
        assert!(next.digest.contains("appended before the slow call"));
        assert_eq!(next.tick, 1);
    }

    #[tokio::test]
    async fn a_hanging_real_cmd_ends_as_a_timeout_reading_inside_the_call_and_the_tick_completes() {
        // why: X4 (T5): the round-2 design rolled back the WHOLE tick when a
        //      slow command outlived the call, so a command that always hangs
        //      (here: `sleep 100` once a marker exists, timeout_s 19 = the X7
        //      ceiling) could never let a tick finish or a durable change
        //      fire. Now the 19 s timeout fires INSIDE the 30 s call: the read
        //      becomes an error reading (a change, R3), the tick completes and
        //      is written, and the watch fires on it at about 20 s. Real
        //      create/baseline (marker absent), paused budgeted call: a child
        //      that never exits is exactly the case that behaves under
        //      auto-advance (the timeout timer is the next event).
        // proves: CLAIM(tick-progress)
        let env = paused_env();
        let vigil = process(&env);
        let marker = env.input_path("hang.marker");
        write_file(&env.input_path("seed"), "");
        let mut request = change_request("w", &env.input_path("seed"), 20);
        request.inputs = vec![Input::Cmd {
            argv: vec!["sh".into(), "-c".into(), format!("if [ -e '{}' ]; then sleep 100; fi; echo ok", marker.display())],
            timeout_s: Some(19.0),
            cwd: None,
        }];
        let id = vigil.create(request).await.unwrap().watch_id;
        write_file(&marker, "");

        tokio::time::pause();
        let envelope = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        tokio::time::resume();
        assert_eq!(envelope.status, WatchStatus::Fired, "the timeout reading is a change and the tick completed");
        let waited = envelope.waited_s.unwrap();
        assert!((19..=21).contains(&waited), "fired at the 19 s timeout, not the 30 s budget: waited {waited}");
        assert!(envelope.events[0].input.starts_with("cmd:"), "{:?}", envelope.events[0]);
        assert!(envelope.digest.contains("timeout"), "{:?}", envelope.digest);
        assert_eq!(env.vigil.store().load(&id).unwrap().tick, 1, "the tick was written");
    }

    #[tokio::test]
    async fn a_hanging_url_ends_as_a_timeout_reading_after_its_own_timeout_and_the_tick_completes() {
        // why: X4 (T5) for url inputs, replacing the V2 test that expected an
        //      aborted tick: the url reader's own 10 s timeout is shorter than
        //      any call, the read becomes an error reading, the tick completes
        //      and writes, and the watch fires on that change at about 10 s.
        //      The server answers at create (real sockets, baseline) and then
        //      accepts without ever replying; the budgeted call is paused.
        // proves: CLAIM(tick-progress)
        let server = FakeHttpServer::start(Behavior::Respond { status: 200, body: "ok".into() });
        let env = paused_env();
        let vigil = process(&env);
        write_file(&env.input_path("seed"), "");
        let mut request = change_request("w", &env.input_path("seed"), 20);
        request.inputs = vec![Input::Url { url: format!("{}/health", server.url()) }];
        let id = vigil.create(request).await.unwrap().watch_id;
        server.set_behavior(Behavior::Hang);

        tokio::time::pause();
        let envelope = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        tokio::time::resume();
        assert_eq!(envelope.status, WatchStatus::Fired);
        let waited = envelope.waited_s.unwrap();
        assert!((10..=12).contains(&waited), "fired at the url's 10 s timeout: waited {waited}");
        assert!(envelope.events[0].input.starts_with("url:"), "{:?}", envelope.events[0]);
        assert_eq!(env.vigil.store().load(&id).unwrap().tick, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_judge_is_capped_at_the_budget_left_and_the_change_is_asked_again_next_call() {
        // why: V2 (S2): the judge request is "capped at the budget left", kept
        //      under X4. judge_timeout is 60 s so only the budget can stop it,
        //      which makes the tick's reserve (60 + 1 s) larger than any call:
        //      the tick still runs because it is DUE when the call starts ("a
        //      due tick runs first"). The change is written before the call,
        //      the judge answers after 100 s. The call ends at exactly 30 s,
        //      pending; the aborted tick wrote nothing (tick 0, no strike, no
        //      pending_change, judge status untouched). The next call asks
        //      again (2nd request, same line) and fires on the answer.
        // proves: CLAIM(call-budget)
        let mut config = default_config();
        config.judge_timeout = Duration::from_secs(60);
        let env = paused_env_with(config);
        let vigil = process(&env);
        let path = env.input_path("j.log");
        write_file(&path, "old\n");
        let id = vigil.create(judge_request("j", &path, 20)).await.unwrap().watch_id;
        env.judge.push_delayed(Duration::from_secs(100), Ok(noul_answers("failed", 0.99, 0.9)));
        append_file(&path, "ERROR: slow judge case\n");

        let started = Instant::now();
        let envelope = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(30));
        assert_eq!(envelope.status, WatchStatus::Pending);
        let file = env.vigil.store().load(&id).unwrap();
        assert_eq!((file.tick, file.judge_strikes), (0, 0), "the aborted tick wrote nothing");
        assert!(file.pending_change.is_none());
        assert_eq!(env.judge.call_count(), 1);

        env.judge.push(Ok(noul_answers("failed", 0.99, 0.9)));
        let next = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!(next.status, WatchStatus::Fired);
        assert_eq!(env.judge.call_count(), 2, "the unconsumed change is asked again");
        assert!(env.judge.requests()[1].state.contains("ERROR: slow judge case"));
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_for_the_watch_lock_counts_against_the_budget() {
        // why: V2 (S2): "Waiting for it counts against the call budget", now
        //      for the watch's own lock (X3): another holder (a tick or cancel
        //      in a different process) has `locks/w.lock` for the whole first
        //      call: the call must give up at exactly max_s = 30 s with a
        //      pending envelope and write nothing (it could not even tick),
        //      instead of blocking until the holder lets go. NOTE for the
        //      implementer: this runs on a current-thread runtime, so the lock
        //      wait must not block the thread (poll try_lock or
        //      spawn_blocking). After the release the change fires.
        // proves: CLAIM(call-budget)
        let env = paused_env();
        let vigil = process(&env);
        let (id, path) = quiet_watch(&env, &vigil, "w", 20, 3600).await;
        append_file(&path, "ERROR: behind the lock\n");
        let before = std::fs::read(vigil.store().watch_path(&id)).unwrap();
        let guard = env.vigil.store().lock_watch(&id).unwrap();

        let started = Instant::now();
        let envelope = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(30));
        assert_eq!(envelope.status, WatchStatus::Pending);
        drop(guard);
        assert_eq!(std::fs::read(vigil.store().watch_path(&id)).unwrap(), before);

        let next = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!(next.status, WatchStatus::Fired);
    }

    #[test]
    fn the_tick_reserve_is_max_input_timeout_plus_judge_timeout_for_judge_watches_plus_one_second() {
        // why: X4 (T5): "a tick starts only if the call has at least
        //      max(input timeouts) + judge timeout + 1 s left". Literal
        //      durations; the derivation is in each row. Default taken
        //      (flagged): the judge timeout counts only for fire = judge and
        //      inputs that never wait (file, glob, process) count 0, i.e. only
        //      what the tick can actually spend is reserved.
        // proves: CLAIM(tick-reserve)
        use std::time::Duration as D;
        let ten = D::from_secs(10);
        let file = || Input::File { path: "/tmp/x".into(), tail_bytes: None };
        let glob = || Input::Glob { pattern: "/tmp/*".into(), report: crate::watch::GlobReport::Created };
        let proc = || Input::Process { pid: 1, want: None };
        let url = || Input::Url { url: "http://127.0.0.1:1/".into() };
        let rows: Vec<(Vec<Input>, bool, D, D, &str)> = vec![
            (vec![file()], false, ten, D::from_secs(1), "0 + 0 + 1"),
            (vec![file(), glob(), proc()], false, ten, D::from_secs(1), "local inputs wait 0"),
            (vec![cmd("x", None)], false, ten, D::from_secs(11), "cmd default 10 + 1"),
            (vec![cmd("x", Some(19.0))], false, ten, D::from_secs(20), "19 + 1"),
            (vec![cmd("x", Some(19.0))], true, ten, D::from_secs(30), "19 + 10 + 1"),
            (vec![cmd("x", Some(0.5)), url()], false, ten, D::from_secs(11), "max(0.5, 10) + 1"),
            (vec![file()], true, ten, D::from_secs(11), "0 + 10 + 1"),
            (vec![file()], true, D::from_secs(2), D::from_secs(3), "0 + 2 + 1"),
            (vec![cmd("a", Some(5.0)), cmd("b", Some(19.0)), file()], true, ten, D::from_secs(30), "max(5, 19, 0) + 10 + 1"),
            (vec![cmd("x", Some(0.1))], false, ten, D::from_millis(1100), "0.1 + 1"),
        ];
        for (inputs, judge, judge_timeout, expected, derivation) in rows {
            assert_eq!(tick_reserve(&inputs, judge, judge_timeout), expected, "{derivation}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_tick_starts_only_while_the_budget_left_covers_the_reserve_boundary_inclusive() {
        // why: X4 (T5): "a tick is not started when the remaining budget is
        //      under max(input timeouts)+judge timeout+1 s". Interval 1, reads
        //      take no time. With timeout_s 19 (X7) the reserve is 20 s, so in a
        //      30 s call a tick may start while 30 - t >= 20, i.e. t = 0..=10:
        //      EXACTLY 11 ticks (the tick at t = 10 has exactly 20 s left and
        //      starts: inclusive; t = 11 has 19 and does not). With the default
        //      10 s timeout the reserve is 11 s: t = 0..=19, 20 ticks. The call
        //      still lasts the whole 30 s either way (it waits out max_s, V26).
        // proves: CLAIM(tick-reserve)
        for (timeout_s, expected_ticks) in [(Some(19.0), 11u64), (None, 20)] {
            let env = paused_env();
            let vigil = process(&env);
            env.reader.set_text("cmd:quiet", "same");
            let id = scripted_watch(&vigil, "w", vec![cmd("quiet", timeout_s)], 1).await;
            let started = Instant::now();
            let envelope = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
            assert_eq!(started.elapsed(), Duration::from_secs(30), "timeout_s {timeout_s:?}");
            assert_eq!(envelope.status, WatchStatus::Pending);
            assert_eq!(envelope.tick, expected_ticks, "timeout_s {timeout_s:?}: ticks start at t = 0..={}", 30 - (timeout_s.unwrap_or(10.0) as u64 + 1));
            assert_eq!(env.vigil.store().load(&id).unwrap().tick, expected_ticks);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_due_tick_runs_first_in_a_call_even_when_the_reserve_exceeds_the_budget_and_progress_continues_across_calls() {
        // why: X4 (T5) "a due tick runs first in each call", which is what
        //      makes progress possible when reserve > budget: a judge watch on
        //      a timeout_s 19 (X7) command has reserve 19 + 10 + 1 = 30 s, equal
        //      to the 30 s minimum call (X7 removed the round-2 contradiction;
        //      the exemption is kept for lock waits that ate the budget). Call 1: the tick is due at the
        //      start, so it runs (quiet) although 31 > 30; the next tick (due
        //      at t = 20, 10 s left) must NOT start. Call 2 starts with a due
        //      tick again, runs it first, sees the change and fires: the watch
        //      cannot livelock. Without the exception no tick would ever run.
        // proves: CLAIM(tick-progress)
        let env = paused_env();
        let vigil = process(&env);
        env.reader.set_text("cmd:j", "v1");
        let mut request = judge_request("w", std::path::Path::new("/unused"), 20);
        request.inputs = vec![cmd("j", Some(19.0))];
        let id = vigil.create(request).await.unwrap().watch_id;

        let first = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!((first.status, first.tick), (WatchStatus::Pending, 1), "the due tick ran; the one at t=20 (10 s left < 30) did not");
        assert_eq!(env.judge.call_count(), 0, "nothing changed, so no judge call");

        env.reader.set_text("cmd:j", "v2");
        env.judge.push(Ok(noul_answers("failed", 0.99, 0.9)));
        let started = Instant::now();
        let second = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!(started.elapsed(), Duration::ZERO, "the due tick ran at once, first");
        assert_eq!((second.status, second.tick), (WatchStatus::Fired, 2));
        assert_eq!(env.judge.call_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_15_s_cmd_with_timeout_20_completes_and_fires_across_repeated_30_s_calls() {
        // why: X4 / T5 exactly: Sol's counterexample was a command that takes
        //      longer than the budget allows being restarted and discarded on
        //      every max_s = 30 call, so even a durable change never fires. A
        //      cmd that takes 15 s (virtual: scripted, no real waits) with
        //      timeout_s 19 (reserve 20 s) and interval 1: call 1 runs the due
        //      tick (15 s, quiet, WRITTEN: file tick 1), does not start another
        //      (15 s left < 20), and returns pending at 30 s. The command's
        //      output then changes; call 2 runs the due tick first, it takes
        //      15 s again, and the watch fires at tick 2 with waited_s 15.
        //      Under rollback-everything semantics tick 1 would not persist.
        // proves: CLAIM(tick-progress)
        let env = paused_env();
        let vigil = process(&env);
        env.reader.set_text("cmd:slow", "v1");
        let id = scripted_watch(&vigil, "w", vec![cmd("slow", Some(19.0))], 1).await;
        env.reader.set_sleep("cmd:slow", Duration::from_secs(15));

        let started = Instant::now();
        let first = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(30));
        assert_eq!((first.status, first.tick, first.waited_s), (WatchStatus::Pending, 1, Some(30)));
        assert_eq!(env.vigil.store().load(&id).unwrap().tick, 1, "the 15 s tick was completed and persisted");
        assert_eq!(env.reader.started_count("cmd:slow"), 2, "the create baseline and exactly one tick read: no restart, no second tick");

        env.reader.set_text("cmd:slow", "v2");
        let second = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!((second.status, second.tick), (WatchStatus::Fired, 2));
        assert_eq!(second.waited_s, Some(15), "the due tick ran first and took its 15 s");
        assert_eq!(second.next, Next::ActOnEvents);
    }

    #[tokio::test(start_paused = true)]
    async fn the_inputs_of_one_tick_are_read_concurrently() {
        // why: X4 (T5): "inputs of one tick are read concurrently" (otherwise
        //      16 inputs at 20 s each could never fit any call). Two inputs
        //      that each take 10 s of virtual time must finish the tick in
        //      10 s, not 20: the call returns (fired: both changed) at exactly
        //      t = 10 s, the reader's own records show both reads starting at
        //      the same instant and ending 10 s later, and both events are
        //      there in input order (`:1:0`, `:1:1`).
        // proves: CLAIM(concurrent-reads)
        let env = paused_env();
        let vigil = process(&env);
        env.reader.set_text("cmd:ra", "a1");
        env.reader.set_text("cmd:rb", "b1");
        let id = scripted_watch(&vigil, "w", vec![cmd("ra", None), cmd("rb", None)], 20).await;
        env.reader.set_text("cmd:ra", "a2");
        env.reader.set_text("cmd:rb", "b2");
        env.reader.set_sleep("cmd:ra", Duration::from_secs(10));
        env.reader.set_sleep("cmd:rb", Duration::from_secs(10));

        let started = Instant::now();
        let envelope = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(10), "sequential reads would take 20 s");
        assert_eq!(envelope.status, WatchStatus::Fired);
        assert_eq!(envelope.events.iter().map(|e| e.event_id.as_str()).collect::<Vec<_>>(), vec!["w:1:0", "w:1:1"]);
        let tick_reads: Vec<_> = env.reader.reads().into_iter().filter(|r| r.finished.is_some_and(|f| f - r.started == Duration::from_secs(10))).collect();
        assert_eq!(tick_reads.len(), 2, "both reads took 10 s: {:?}", env.reader.reads());
        assert_eq!(tick_reads[0].started, tick_reads[1].started, "the two reads overlapped completely");
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

    // ---- code review round 1: C1, C6, C7 ----

    #[tokio::test(start_paused = true)]
    async fn a_slow_unchanged_watch_cannot_starve_a_later_watchs_change_within_one_call() {
        // why: code review C1: ticks ran one watch after another in id order,
        //      each cut off at the shared budget. Watch `a` has an unchanged
        //      19 s command, watch `b` a 15 s command whose text changed:
        //      sequentially `b` starts at 19 s, is cut at 30 s and never
        //      commits, on every call, forever. Covered watches must tick
        //      concurrently inside the shared budget (each under its own
        //      lock), so one max_s = 30 call fires `b`, and `b`'s tick is on
        //      disk.
        // proves: CLAIM(tick-progress)
        let env = paused_env();
        let vigil = process(&env);
        env.reader.set_text("cmd:slow-a", "same");
        env.reader.set_text("cmd:fast-b", "v1");
        scripted_watch(&vigil, "a", vec![cmd("slow-a", Some(19.0))], 20).await;
        let b = scripted_watch(&vigil, "b", vec![cmd("fast-b", Some(19.0))], 20).await;
        env.reader.set_sleep("cmd:slow-a", Duration::from_secs(19));
        env.reader.set_sleep("cmd:fast-b", Duration::from_secs(15));
        env.reader.set_text("cmd:fast-b", "v2");

        let started = Instant::now();
        let envelope = vigil.keepalive(no_id(Some(30))).await.unwrap();
        assert_eq!(envelope.watch_id, b, "the changed watch fires in the first call");
        assert_eq!(envelope.status, WatchStatus::Fired);
        assert!(started.elapsed() < Duration::from_secs(30), "returned before the budget ran out: {:?}", started.elapsed());
        assert_eq!(env.vigil.store().load(&b).unwrap().tick, 1, "b's tick was committed");
    }

    #[tokio::test(start_paused = true)]
    async fn a_tick_that_returns_changed_output_after_the_deadline_expires_the_watch_instead_of_firing() {
        // why: code review C6: a tick stamped its result with the time before
        //      sensing, so a command that returned changed output 15 s later
        //      fired a watch whose deadline was 10 s after creation; the
        //      terminal state then blocked expiry. After awaited work the
        //      wall clock is re-checked: past the deadline the watch is
        //      persisted EXPIRED (finished_at = deadline_at, no events), not
        //      fired (R5, V16).
        // proves: CLAIM(deadline-expiry)
        let env = paused_env();
        let vigil = process(&env);
        env.reader.set_text("cmd:late", "v1");
        let mut request = change_request("w", std::path::Path::new("/unused"), 20);
        request.inputs = vec![cmd("late", Some(19.0))];
        request.deadline_s = Some(10);
        let id = vigil.create(request).await.unwrap().watch_id;
        env.reader.set_text("cmd:late", "v2");
        env.reader.set_sleep("cmd:late", Duration::from_secs(15));

        let envelope = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!(envelope.status, WatchStatus::Expired);
        assert!(envelope.events.is_empty(), "{:?}", envelope.events);
        let file = env.vigil.store().load(&id).unwrap();
        assert_eq!(file.status, WatchStatus::Expired, "persisted as expired, not fired");
        assert_eq!(file.finished_at, Some(file.deadline_at));
        assert!(file.events.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn expiry_is_reported_at_the_deadline_even_when_the_tick_reserve_stops_sensing() {
        // why: code review C7: with a 19 s command (reserve 20 s) and a 30 s
        //      call, from 11 s on no tick may start, and the old loop dropped
        //      the watch's deadline wake with the skipped tick, so a deadline
        //      at 15 s was reported only when the call ended at 30 s. Expiry
        //      is independent of the reserve: the call must wake at the
        //      deadline and return the expired envelope at about 15 s.
        // proves: CLAIM(deadline-expiry)
        let env = paused_env();
        let vigil = process(&env);
        env.reader.set_text("cmd:quiet", "same");
        let mut request = change_request("w", std::path::Path::new("/unused"), 1);
        request.inputs = vec![cmd("quiet", Some(19.0))];
        request.deadline_s = Some(15);
        let id = vigil.create(request).await.unwrap().watch_id;

        let started = Instant::now();
        let envelope = vigil.keepalive(by_id(&id, Some(30))).await.unwrap();
        assert_eq!(envelope.status, WatchStatus::Expired);
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_secs(15) && elapsed < Duration::from_secs(17), "expired at the deadline, not at max_s: {elapsed:?}");
        assert!((15..=16).contains(&envelope.waited_s.unwrap()), "{:?}", envelope.waited_s);
    }

    // ---- code review round 2: C12, C13 ----

    #[tokio::test(start_paused = true)]
    async fn a_held_lock_on_a_secondary_watch_cannot_delay_a_ready_result() {
        // why: code review C12: a round waited for every due tick after one had
        //      fired, so when watch `b`'s lock was held by another process the
        //      fired result of `a` was withheld until max_s. V8 needs no wait
        //      for unfinished watches: once a covered watch has a terminal
        //      result, ready completions are collected, unfinished ticks are
        //      dropped (they write nothing, C5) and the call returns. `a`
        //      changes and fires at once; `b`'s lock is held for the whole
        //      call; a 270 s call must return at once with `a` primary and
        //      `b` untouched.
        // proves: CLAIM(call-budget)
        let env = paused_env();
        let vigil = process(&env);
        env.reader.set_text("cmd:a", "v1");
        env.reader.set_text("cmd:b", "same");
        let a = scripted_watch(&vigil, "a", vec![cmd("a", Some(19.0))], 20).await;
        let b = scripted_watch(&vigil, "b", vec![cmd("b", Some(19.0))], 20).await;
        env.reader.set_text("cmd:a", "v2");
        let before = std::fs::read(vigil.store().watch_path(&b)).unwrap();
        let guard = env.vigil.store().lock_watch(&b).unwrap();

        let started = Instant::now();
        let envelope = vigil.keepalive(no_id(Some(270))).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(30), "the ready result waited {:?}", started.elapsed());
        assert_eq!((envelope.watch_id, envelope.status), (a, WatchStatus::Fired));
        drop(guard);
        assert_eq!(std::fs::read(vigil.store().watch_path(&b)).unwrap(), before, "the unfinished tick wrote nothing");
    }

    #[tokio::test(start_paused = true)]
    async fn a_result_already_finished_before_the_call_is_not_held_back_by_a_locked_watch() {
        // why: code review round 3, C12 still open: a round skips watches that
        //      are terminal when the call starts, so with `a` already fired
        //      (unreturned) and `b`'s lock held, the round held only `b` and
        //      the ready result of `a` waited the whole 270 s. Covered terminal
        //      results must be returned before any round is awaited.
        // proves: CLAIM(call-budget)
        let env = paused_env();
        let vigil = process(&env);
        env.reader.set_text("cmd:a", "v1");
        env.reader.set_text("cmd:b", "same");
        let a = scripted_watch(&vigil, "a", vec![cmd("a", Some(19.0))], 20).await;
        let b = scripted_watch(&vigil, "b", vec![cmd("b", Some(19.0))], 20).await;
        env.reader.set_text("cmd:a", "v2");
        vigil.tick_due(&a).await.unwrap();
        assert_eq!(vigil.store().load(&a).unwrap().status, WatchStatus::Fired, "precondition: a finished outside any keepalive");
        let guard = env.vigil.store().lock_watch(&b).unwrap();

        let started = Instant::now();
        let envelope = vigil.keepalive(no_id(Some(270))).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(30), "the ready result waited {:?}", started.elapsed());
        assert_eq!((envelope.watch_id, envelope.status), (a, WatchStatus::Fired));
        drop(guard);
    }

    #[tokio::test(start_paused = true)]
    async fn the_earlier_finisher_is_primary_when_two_ticks_fire_in_one_call() {
        // why: code review C13: with concurrent ticks `finished_at` was the
        //      time before sensing, so a watch whose tick took 19 s tied with
        //      one that took 1 s and won on id. V8: primary is the
        //      earliest-finished. `a` takes 19 s, `b` 1 s, both changed.
        // proves: CLAIM(tick-progress)
        let env = paused_env();
        let vigil = process(&env);
        env.reader.set_text("cmd:slow-a", "v1");
        env.reader.set_text("cmd:fast-b", "v1");
        scripted_watch(&vigil, "a", vec![cmd("slow-a", Some(19.0))], 20).await;
        let b = scripted_watch(&vigil, "b", vec![cmd("fast-b", Some(19.0))], 20).await;
        env.reader.set_text("cmd:slow-a", "v2");
        env.reader.set_text("cmd:fast-b", "v2");
        env.reader.set_sleep("cmd:slow-a", Duration::from_secs(19));
        env.reader.set_sleep("cmd:fast-b", Duration::from_secs(1));

        let envelope = vigil.keepalive(no_id(Some(30))).await.unwrap();
        assert_eq!((envelope.watch_id, envelope.status), (b, WatchStatus::Fired));
    }

    #[tokio::test(start_paused = true)]
    async fn a_fired_watch_is_stamped_with_the_time_sensing_finished() {
        // why: code review C13 at the tick: `finished_at` is when the result
        //      was reached, after the 15 s read, not when the tick began.
        // proves: CLAIM(tick-progress)
        let env = paused_env();
        let vigil = process(&env);
        env.reader.set_text("cmd:slow", "v1");
        let id = scripted_watch(&vigil, "w", vec![cmd("slow", Some(19.0))], 20).await;
        env.reader.set_text("cmd:slow", "v2");
        env.reader.set_sleep("cmd:slow", Duration::from_secs(15));
        let begun = env.vigil.store().load(&id).unwrap().created_at;

        env.vigil.tick_due(&id).await.unwrap();
        let file = env.vigil.store().load(&id).unwrap();
        assert_eq!(file.status, WatchStatus::Fired);
        assert_eq!(file.finished_at, Some(begun.plus_secs(15)));
    }
}
