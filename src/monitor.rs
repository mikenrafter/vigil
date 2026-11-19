//! The orchestrator's credit check (docs/specs/auto-interval.md AI2', AI8, S2, S5,
//! S7, S9, N1, N4): when it runs, how it is serialized across processes, and
//! what it records.
//!
//! One check, under `<root>/credit.lock`:
//!
//! 1. re-read `credit-state.json`; if another process checked less than a
//!    cadence ago, adopt its time and stop (S9: two engines, one probe);
//! 2. probe (bounded by the probe's timeout and the call budget, S7);
//! 3. compare the verdict with the recorded KNOWN state (S5): unknown changes
//!    nothing; the first known verdict is a baseline; a different known state
//!    is a CHANGE;
//! 4. on a change run the hook (bounded, S7) and build the event;
//! 5. commit `credit-state.json`, still under the lock.
//!
//! Invariants: the lock is held from step 1 to the end of step 5, so a hook
//! and the record that it ran cannot be separated by another process (N1);
//! the record is written AFTER the hook, so a crash or a failed write between
//! them makes the next check run the hook again: delivery is AT-LEAST-ONCE and
//! hooks must be idempotent (N4); cancelling the call drops the future, which
//! kills the probe's and the hook's process groups and releases the lock (S7).
//! Mating parts: `credit.rs` (probe, hook, wire types), `store.rs` (lock and
//! state file), `keepalive.rs` (the loop that asks when a check is due).

use std::time::Duration;

use tokio::time::Instant;

use crate::cache::CODEX_MAX_WINDOW_S;
use crate::clock::Timestamp;
use crate::credit::{hook_input, CreditCheck, CreditSource, CreditState, CreditStateFile, HookOutcome, CALL_MARGIN};
use crate::engine::Vigil;
use crate::error::VigilError;
use crate::watch::{Event, EventDetail, WatchId};

/// The longest a check waits for `credit.lock` held by another process. A
/// check that cannot get it in time is skipped and left to the next call; it
/// never holds a watch's answer for longer.
pub const CREDIT_LOCK_WAIT: Duration = Duration::from_secs(5);

const LOCK_POLL: Duration = Duration::from_millis(2);

/// When this call checks credit state (S2, R2-A2): at its first pass if the
/// last check is a cadence old, and, for a long window, again every cadence.
/// A short window never checks again inside the call. A check that was
/// skipped (no budget, lock busy) is not retried in the same call.
#[derive(Debug)]
pub(crate) struct CreditPlan {
    every: Duration,
    long_window: bool,
    /// Time of the newest check anyone made on this state root (from
    /// `credit-state.json`, then from our own checks); `None`: never.
    last_checked: Option<Timestamp>,
    entry_considered: bool,
    gave_up: bool,
}

impl CreditPlan {
    pub(crate) fn new(every: Duration, window_s: u64, last_checked: Option<Timestamp>) -> CreditPlan {
        CreditPlan { every, long_window: window_s > CODEX_MAX_WINDOW_S, last_checked, entry_considered: false, gave_up: false }
    }

    /// Is a check due at `now`? Asked once per pass of the call's loop; the
    /// first ask is the entry check.
    pub(crate) fn is_due(&mut self, now: Timestamp) -> bool {
        if self.gave_up {
            return false;
        }
        let first = !self.entry_considered;
        self.entry_considered = true;
        if !first && !self.long_window {
            return false;
        }
        self.last_checked.is_none_or(|checked| now.duration_since(checked) >= self.every)
    }

    /// When the next check falls due, so the loop can sleep until then.
    pub(crate) fn next_due_at(&self) -> Option<Timestamp> {
        if self.gave_up || (self.entry_considered && !self.long_window) {
            return None;
        }
        self.last_checked.map(|checked| checked.plus_duration(self.every))
    }
}

/// What a check produced for the call.
#[derive(Debug, Default)]
pub(crate) struct CreditStep {
    /// The newest check this step made, for `cache.credit_check`.
    pub check: Option<CreditCheck>,
    /// Set when the known state changed: ends the call (AI8).
    pub event: Option<Event>,
}

impl Vigil {
    /// The plan for one orchestrator call, with the cadence taken from the
    /// state file (S9).
    pub(crate) fn credit_plan(&self, window_s: u64) -> CreditPlan {
        let last_checked = self.store.load_credit_state().map(|recorded| recorded.checked_at);
        CreditPlan::new(self.config.credit_check_every, window_s, last_checked)
    }

    /// Run one due check (see the module docs). `anchor` is the watch the
    /// event is attached to. Never fails the call: whatever goes wrong
    /// (no lock, no budget, a failed write) skips or degrades the check.
    pub(crate) async fn run_credit_step(&self, plan: &mut CreditPlan, budget_end: Instant, anchor: (&WatchId, u64)) -> Result<CreditStep, VigilError> {
        let left = || budget_end.saturating_duration_since(Instant::now());
        // S7: a check is started only if its probe could finish, margin kept.
        let need = self.credit_probe.timeout() + CALL_MARGIN;
        if left() < need {
            plan.gave_up = true;
            return Ok(CreditStep::default());
        }
        let lock_deadline = Instant::now() + (left() - need).min(CREDIT_LOCK_WAIT);
        let lock = loop {
            match self.store.try_lock_credit() {
                Ok(Some(lock)) => break lock,
                Ok(None) if Instant::now() < lock_deadline => tokio::time::sleep(LOCK_POLL).await,
                Ok(None) | Err(_) => {
                    plan.gave_up = true;
                    return Ok(CreditStep::default());
                }
            }
        };
        if left() < need {
            plan.gave_up = true;
            return Ok(CreditStep::default());
        }

        // ↳ CLAIM(S9-adopt): another process's fresher record makes this check unnecessary [spec S9; auto_interval_tests::two_engines_share_a_real_credit_flock_and_run_the_change_hook_once]
        let recorded = self.store.load_credit_state();
        let started = self.clock.now();
        if let Some(recorded) = recorded.as_ref().filter(|recorded| started.duration_since(recorded.checked_at) < self.config.credit_check_every) {
            plan.last_checked = Some(recorded.checked_at);
            return Ok(CreditStep::default());
        }

        let probe_budget = self.credit_probe.timeout().min(left().saturating_sub(CALL_MARGIN));
        let state = match tokio::time::timeout(probe_budget, self.credit_probe.check(started)).await {
            Ok(state) => state,
            Err(_) => CreditState::unknown(CreditSource::Transcripts),
        };
        let check = CreditCheck { at: started, source: state.source, on_credits: state.on_credits };
        let known_before = recorded.and_then(|recorded| recorded.on_credits);

        let mut event = None;
        let record = match (state.on_credits, known_before) {
            // S5: unknown keeps the last known state, still counts as a check.
            (None, known) => CreditStateFile { on_credits: known, checked_at: started },
            // First known verdict: a baseline, never a change.
            (Some(verdict), None) => CreditStateFile { on_credits: Some(verdict), checked_at: started },
            (Some(verdict), Some(before)) if verdict == before => CreditStateFile { on_credits: Some(verdict), checked_at: started },
            (Some(verdict), Some(_)) => {
                let hook = match &self.config.on_credit_change {
                    Some(command) => {
                        let Some(budget) = left().checked_sub(CALL_MARGIN).filter(|budget| !budget.is_zero()) else {
                            // No time left for the hook: leave the state as it was
                            // so the next call applies the change.
                            plan.gave_up = true;
                            return Ok(CreditStep::default());
                        };
                        Some(self.run_hook(command, &hook_input(&state, started), self.config.credit_hook_timeout.min(budget)).await)
                    }
                    None => None,
                };
                event = Some(credit_event(anchor, started, verdict, &state, hook));
                CreditStateFile { on_credits: Some(verdict), checked_at: started }
            }
        };
        // ↳ CLAIM(N4): the record is written after the hook; a failed write leaves the old record, so the next check runs the hook again [spec N4; auto_interval_tests::a_hook_whose_state_write_failed_runs_again_at_the_next_check]
        let _ = self.store.save_credit_state(&lock, &record);
        // Whether or not the record could be written, do not ask again before a
        // cadence has passed inside this call.
        plan.last_checked = Some(started);
        drop(lock);
        Ok(CreditStep { check: Some(check), event })
    }

    /// The hook through the runner seam, cut at `timeout` here too (S7): a
    /// runner that does not enforce it cannot overrun the call.
    async fn run_hook(&self, command: &str, input: &serde_json::Value, timeout: Duration) -> HookOutcome {
        match tokio::time::timeout(timeout, self.hook_runner.run(command, input, timeout)).await {
            Ok(outcome) => outcome,
            Err(_) => HookOutcome { command: command.to_string(), exit_code: None, timed_out: true, output: String::new() },
        }
    }
}

fn credit_event(anchor: (&WatchId, u64), at: Timestamp, on_credits: bool, state: &CreditState, hook: Option<HookOutcome>) -> Event {
    let direction = if on_credits { "usage credits are now being used" } else { "usage credits are no longer being used" };
    Event {
        event_id: format!("credit:{}", at.0.format("%Y%m%dT%H%M%SZ")),
        tick: anchor.1,
        watch_id: anchor.0.to_string(),
        input: "credit".to_string(),
        summary: format!("{direction} (source {})", match state.source {
            CreditSource::Uw => "uw",
            CreditSource::Transcripts => "transcripts",
        }),
        judge: None,
        redelivered: false,
        detail: Some(EventDetail::CreditStateChanged { on_credits: Some(on_credits), source: Some(state.source), windows: state.windows.clone(), hook }),
    }
}
