//! One evaluation round of one watch (SPEC "Tick logic").

use crate::clock::Timestamp;
use crate::engine::{expired_if_overdue, Persisted, Vigil};
use crate::error::VigilError;
use crate::inputs::{Reading, TRUNCATED_MARKER};
use crate::judge::{build_state, Answers, JudgeRequest};
use crate::watch::{
    ChangedInput, Event, FireRule, Input, InputState, JudgeStatus, PendingChange, Terminal, TerminalStatus, WatchFile, WatchId, WatchStatus,
    MAX_SUMMARY_BYTES,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickOutcome {
    /// A tick ran (the watch may have fired during it).
    Ticked,
    /// Not due yet, or lost the race after taking the lock.
    NotDue,
    /// The watch is finished (fired, expired, cancelled), possibly just now
    /// because the deadline passed.
    Finished(WatchStatus),
    /// The call budget ran out before the tick could commit; nothing was
    /// written (V2).
    OutOfBudget,
}

/// What one `tick_report` call did, for callers (the keepalive loop) that
/// need more than the outcome: the content that changed during the tick, in
/// input order, for the call-local digest (V14), and how the save went.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TickReport {
    pub outcome: TickOutcome,
    pub changed: Vec<ChangedInput>,
    /// The watch as this tick left it, when the tick wrote (or tried to write)
    /// it. If `persisted` is `NotSaved` this copy is the only place the result
    /// lives, and the caller answers from it (DF1).
    pub after: Option<WatchFile>,
    pub persisted: Persisted,
    /// S1: this process's last save of the watch failed, so events this tick
    /// fired are redeliveries.
    pub redelivered: bool,
}

impl TickReport {
    fn without_changes(outcome: TickOutcome) -> TickReport {
        TickReport { outcome, changed: Vec::new(), after: None, persisted: Persisted::Saved, redelivered: false }
    }

    /// A tick that wrote `after` (or failed to) and changed nothing the digest
    /// reports: an expiry.
    fn wrote(outcome: TickOutcome, after: WatchFile, persisted: Persisted) -> TickReport {
        TickReport { outcome, changed: Vec::new(), after: Some(after), persisted, redelivered: false }
    }
}

/// Where a tick leaves what it has already learned before it waits on
/// anything a dropped call could cut short (review S4). The sensing loop of a
/// call owns one per tick it starts; if the tick's future is dropped (the round
/// ended, the budget ran out) the loop collects whatever is left here, so the
/// tick's answer and the outcome of its save are not lost with it.
///
/// Invariants: holds a report only while the tick is inside its save; the tick
/// clears it when the save returns, so a tick that completed is never collected
/// twice. The report says what the files on disk are (`Persisted`).
#[derive(Default)]
pub(crate) struct Retained(std::sync::Mutex<Option<TickReport>>);

impl Retained {
    fn keep(&self, report: TickReport) {
        *self.0.lock().expect("retained report mutex poisoned") = Some(report);
    }

    fn clear(&self) {
        self.0.lock().expect("retained report mutex poisoned").take();
    }

    /// The report of a tick that was dropped inside its save, if it was.
    pub(crate) fn take(&self) -> Option<TickReport> {
        self.0.lock().expect("retained report mutex poisoned").take()
    }
}

impl Vigil {
    /// Run the watch's tick if `last_tick_at + interval_s` is due (the first
    /// tick is due immediately, and compares against the baseline recorded at
    /// `create`, V1). Takes the WATCH's lock (X3: `locks/<id>.lock`, held
    /// across sensing and the write; never the global lock), re-reads the
    /// watch under it and repeats the due/terminal checks; a finished watch is
    /// returned as is (V3). Reads all inputs of the tick concurrently through
    /// `Vigil::reader` (X4). Caps every event `summary` at 200 bytes (X2) and
    /// the persisted pending change at 64 KiB per input (W7). Rewrites the
    /// watch file atomically at the end.
    pub async fn tick_due(&self, id: &WatchId) -> Result<TickOutcome, VigilError> {
        Ok(self.tick_report(id).await?.outcome)
    }

    /// `tick_due` plus the content the tick found changed.
    pub(crate) async fn tick_report(&self, id: &WatchId) -> Result<TickReport, VigilError> {
        self.tick_report_until(id, None).await
    }

    /// `tick_report` inside a call budget.
    pub(crate) async fn tick_report_until(
        &self,
        id: &WatchId,
        budget_end: Option<tokio::time::Instant>,
    ) -> Result<TickReport, VigilError> {
        self.tick_report_retaining(id, budget_end, &Retained::default()).await
    }

    /// `tick_report_until` that leaves its report in `retained` while it is
    /// inside its save (review S4), for a caller that may drop it there.
    pub(crate) async fn tick_report_retaining(
        &self,
        id: &WatchId,
        budget_end: Option<tokio::time::Instant>,
        retained: &Retained,
    ) -> Result<TickReport, VigilError> {
        // C5: the budget is checked by the tick itself, right before it
        // persists. `timeout_at` alone polls the inner future first, so a tick
        // resumed after the budget with its readings ready would still write.
        let out_of_budget = || budget_end.is_some_and(|end| tokio::time::Instant::now() > end);
        let (_lock, mut watch) = self.lock_and_load(id).await?;
        let now = self.clock.now();
        // S1: decided before this tick's own save changes the answer.
        let redelivered = self.may_repeat(id);

        if watch.status.is_finished() {
            return Ok(TickReport::without_changes(TickOutcome::Finished(watch.status)));
        }
        if watch.is_overdue(now) {
            // R5: expiry runs no final tick and adds no event.
            if out_of_budget() {
                return Ok(TickReport::without_changes(TickOutcome::OutOfBudget));
            }
            let expired = expired_if_overdue(watch, now);
            let report = |persisted| TickReport::wrote(TickOutcome::Finished(expired.status), expired.clone(), persisted);
            let persisted = self.persist_retaining(retained, budget_end, &expired, report).await?;
            return Ok(TickReport::wrote(TickOutcome::Finished(expired.status), expired, persisted));
        }
        if !is_due(&watch, now) {
            return Ok(TickReport::without_changes(TickOutcome::NotDue));
        }

        // A placeholder left by a create that died before its baseline has
        // fewer states than inputs; the missing ones start as baselines.
        let unticked = watch.clone();
        let mut states = watch.input_states.clone();
        states.resize(watch.spec.inputs.len(), InputState::default());
        let readings = self.read_all(&watch.spec.inputs, &states).await;

        let tick = watch.tick + 1;
        let changed: Vec<ChangedInput> = readings
            .iter()
            .filter(|reading| reading.changed)
            .map(|reading| ChangedInput { label: reading.label.clone(), text: reading.text.clone() })
            .collect();
        if !changed.is_empty() {
            watch.last_digest = digest_of(&changed);
        }

        let fired_events = match &watch.spec.fire {
            FireRule::Change => change_events(&watch, tick, &readings),
            FireRule::Condition => condition_events(&mut watch, tick, &readings),
            FireRule::Judge { .. } => {
                if changed.is_empty() && watch.pending_change.is_none() {
                    Vec::new()
                } else {
                    self.judge_step(&mut watch, tick, &changed, now).await?
                }
            }
        };

        // Sensing and judging took time. Past the call budget nothing is
        // written (C5); past the watch's deadline the watch expires instead of
        // firing on a late result (C6, R5: no event, finished at the deadline).
        if out_of_budget() {
            return Ok(TickReport::without_changes(TickOutcome::OutOfBudget));
        }
        let after = self.clock.now();
        if unticked.is_overdue(after) {
            let expired = expired_if_overdue(unticked, after);
            let report = |persisted| TickReport::wrote(TickOutcome::Finished(expired.status), expired.clone(), persisted);
            let persisted = self.persist_retaining(retained, budget_end, &expired, report).await?;
            return Ok(TickReport::wrote(TickOutcome::Finished(expired.status), expired, persisted));
        }

        watch.tick = tick;
        watch.last_tick_at = Some(now);
        watch.input_states = readings.into_iter().map(|reading| reading.state).collect();
        if !fired_events.is_empty() {
            let terminal = Terminal {
                status: TerminalStatus::Fired,
                finished_at: after,
                events: fired_events,
                digest: watch.last_digest.clone(),
            };
            watch.finish(terminal).expect("a pending watch accepts its terminal result");
        }
        // DF1: a write that fails is a value, not an error; the tick's answer is
        // `watch`, saved or not. DF3: nothing was written, so the persisted
        // baseline did not move and the next tick senses the change again.
        // ↳ CLAIM(S4-retained): the sensed answer is left in `retained` before the save can wait, so a dropped tick still has an answer [review S4; disk_full_tests a_rescue_waiting_for_the_store_lock_keeps_its_answer_when_another_watch_ends_the_round]
        let report = |persisted| TickReport {
            outcome: TickOutcome::Ticked,
            changed: changed.clone(),
            after: Some(watch.clone()),
            persisted,
            redelivered,
        };
        let persisted = self.persist_retaining(retained, budget_end, &watch, report).await?;
        Ok(TickReport { outcome: TickOutcome::Ticked, changed, after: Some(watch), persisted, redelivered })
    }

    /// Save `watch` for a tick. `report` builds the tick's report for a given
    /// outcome of the save; it is left in `retained` before the save waits on
    /// anything (review S4) and cleared when the save returns.
    async fn persist_retaining(
        &self,
        retained: &Retained,
        budget_end: Option<tokio::time::Instant>,
        watch: &WatchFile,
        report: impl Fn(Persisted) -> TickReport + Sync,
    ) -> Result<Persisted, VigilError> {
        let settled = |persisted: Persisted| retained.keep(report(persisted));
        let result = self.persist_observed(watch, budget_end, &settled).await;
        retained.clear();
        result
    }

    /// PHASE 2 SEAM (judge). Called by `tick_report` for `fire = judge` only
    /// when something changed this tick (`changed`) or an earlier change is
    /// still pending (`watch.pending_change`). It owns everything judge
    /// related: merging `changed` into `watch.pending_change` (R7, W7 cap),
    /// building the request (`judge::build_state`), calling `self.judge`,
    /// evaluating the rule, and updating `watch.judge_strikes` /
    /// `watch.judge_status` / `watch.pending_change`. It returns the events
    /// the tick fires with (empty: do not fire). `tick_report` already set
    /// `watch.last_digest` and persists the watch afterwards; do not save here.
    async fn judge_step(
        &self,
        watch: &mut WatchFile,
        tick: u64,
        changed: &[ChangedInput],
        now: Timestamp,
    ) -> Result<Vec<Event>, VigilError> {
        let FireRule::Judge { judge_questions, judge_rule } = watch.spec.fire.clone() else {
            return Ok(Vec::new());
        };
        // R7: everything changed since the last judged tick, oldest first.
        let pending = merge_pending(watch.pending_change.take(), changed);
        let request = JudgeRequest {
            state: build_state(&watch.spec.prompt, &pending, self.config.judge_state_bytes),
            questions: judge_questions.0.clone(),
        };
        let verdict = match self.judge.predict(&request).await {
            Ok(answers) => judge_rule.evaluate(&answers).map(|matched| (matched, answers)).map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        };
        match verdict {
            Ok((matched, answers)) => {
                watch.judge_strikes = 0;
                watch.judge_status = JudgeStatus::Ok;
                // A judged change is consumed, whether or not it matched.
                if !matched {
                    return Ok(Vec::new());
                }
                watch.last_digest = digest_of(&pending);
                Ok(vec![judge_event(watch, tick, &pending, None, Some(answers))])
            }
            Err(_) => {
                watch.judge_strikes += 1;
                watch.judge_status = JudgeStatus::Down;
                if watch.judge_strikes < MAX_JUDGE_STRIKES {
                    watch.pending_change = Some(PendingChange { changed: pending });
                    return Ok(Vec::new());
                }
                watch.last_digest = digest_of(&pending);
                Ok(vec![judge_event(watch, tick, &pending, Some("judge unavailable; raw change attached"), None)])
            }
        }
    }
}

/// SPEC "Failure rules": the third consecutive judge failure fires.
const MAX_JUDGE_STRIKES: u32 = 3;

/// W7: the most one input's persisted pending change may hold.
const MAX_PENDING_BYTES_PER_INPUT: usize = 64 * 1024;

/// R7 + W7: append `changed` (newer) to `pending` (older), one entry per
/// input label, and keep the newest `MAX_PENDING_BYTES_PER_INPUT` of each.
fn merge_pending(pending: Option<PendingChange>, changed: &[ChangedInput]) -> Vec<ChangedInput> {
    let mut merged: Vec<ChangedInput> = pending.map(|p| p.changed).unwrap_or_default();
    for input in changed {
        match merged.iter_mut().find(|existing| existing.label == input.label) {
            Some(existing) => {
                if !existing.text.is_empty() && !existing.text.ends_with('\n') {
                    existing.text.push('\n');
                }
                existing.text.push_str(&input.text);
            }
            None => merged.push(input.clone()),
        }
    }
    for input in &mut merged {
        if input.text.len() > MAX_PENDING_BYTES_PER_INPUT {
            let mut start = input.text.len() - MAX_PENDING_BYTES_PER_INPUT;
            while !input.text.is_char_boundary(start) {
                start += 1;
            }
            input.text = format!("{TRUNCATED_MARKER}\n{}", &input.text[start..]);
        }
    }
    merged
}

/// The single event of a judge-fired tick (W1: `:0`). `summary` overrides the
/// default, the newest line of each judged input (X2: at most 200 bytes).
fn judge_event(watch: &WatchFile, tick: u64, judged: &[ChangedInput], summary: Option<&str>, answers: Option<Answers>) -> Event {
    let summary = summary.map(str::to_string).unwrap_or_else(|| {
        let lines: Vec<String> = judged
            .iter()
            .map(|input| match watch.spec.inputs.iter().find(|spec| spec.label() == input.label) {
                Some(spec) => summarize(spec, &input.text),
                None => cap_bytes(input.text.trim(), MAX_SUMMARY_BYTES),
            })
            .collect();
        cap_bytes(&lines.join(" | "), MAX_SUMMARY_BYTES)
    });
    Event {
        event_id: Event::make_id(watch.spec.id.as_str(), tick, 0),
        tick,
        watch_id: watch.spec.id.to_string(),
        input: judged.iter().map(|input| input.label.as_str()).collect::<Vec<_>>().join(", "),
        summary,
        judge: answers,
        redelivered: false,
    }
}

/// D3: the first tick is due at once; later ones at `last_tick_at + interval_s`.
pub(crate) fn is_due(watch: &WatchFile, now: Timestamp) -> bool {
    match watch.last_tick_at {
        None => true,
        Some(last) => now >= last.plus_secs(watch.spec.interval_s),
    }
}

/// The changed content as the agent reads it in a digest: each input's label
/// and its new text.
pub(crate) fn digest_of(changed: &[ChangedInput]) -> String {
    changed
        .iter()
        .map(|input| format!("{}:\n{}", input.label, input.text))
        .collect::<Vec<_>>()
        .join("\n")
}

/// fire = change: every changed input is an event, in input order (W1).
fn change_events(watch: &WatchFile, tick: u64, readings: &[Reading]) -> Vec<Event> {
    let changed_inputs = watch.spec.inputs.iter().zip(readings).filter(|(_, reading)| reading.changed);
    changed_inputs
        .enumerate()
        .map(|(index, (input, reading))| Event {
            event_id: Event::make_id(watch.spec.id.as_str(), tick, index),
            tick,
            watch_id: watch.spec.id.to_string(),
            input: reading.label.clone(),
            summary: summarize(input, &reading.text),
            judge: None,
            redelivered: false,
        })
        .collect()
}

/// fire = condition (R1): evaluated on every tick, changed or not. Fires when
/// every input that has a `want` reports it met; one event names them all.
fn condition_events(watch: &mut WatchFile, tick: u64, readings: &[Reading]) -> Vec<Event> {
    let wanted: Vec<&Reading> = watch
        .spec
        .inputs
        .iter()
        .zip(readings)
        .filter(|(input, _)| input.has_want())
        .map(|(_, reading)| reading)
        .collect();
    if wanted.is_empty() || !wanted.iter().all(|reading| reading.want_met == Some(true)) {
        return Vec::new();
    }
    let met: Vec<ChangedInput> =
        wanted.iter().map(|reading| ChangedInput { label: reading.label.clone(), text: reading.text.clone() }).collect();
    watch.last_digest = digest_of(&met);
    let labels = met.iter().map(|input| input.label.as_str()).collect::<Vec<_>>().join(", ");
    let summary = met.iter().map(|input| format!("{} {}", input.label, input.text)).collect::<Vec<_>>().join("; ");
    vec![Event {
        event_id: Event::make_id(watch.spec.id.as_str(), tick, 0),
        tick,
        watch_id: watch.spec.id.to_string(),
        input: labels,
        summary: cap_bytes(&format!("condition met: {summary}"), MAX_SUMMARY_BYTES),
        judge: None,
        redelivered: false,
    }]
}

/// X2: a one-line event summary of at most `MAX_SUMMARY_BYTES`. A file's
/// summary is its newest non-empty line; other inputs read as short
/// multi-line observations (`exit=1`, `HTTP 503`, ...) and are joined.
pub(crate) fn summarize(input: &Input, text: &str) -> String {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let summary = match input {
        Input::File { .. } => lines.next_back().unwrap_or_default().to_string(),
        _ => lines.collect::<Vec<_>>().join(" | "),
    };
    cap_bytes(&summary, MAX_SUMMARY_BYTES)
}

/// `text` cut to at most `max_bytes`, on a UTF-8 boundary.
pub(crate) fn cap_bytes(text: &str, max_bytes: usize) -> String {
    let mut end = text.len().min(max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
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
        //      Event carries tick (2), the stable event_id "build:2:0" (V3, W1:
        //      always `<watch>:<tick>:<n>`, n from 0),
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
        assert_eq!(event.event_id, "build:2:0");
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
        // why: V3 (S3) + W1 (T1): the id is always `<watch_id>:<tick>:<n>`, n
        //      from 0, so two inputs changing in one tick are `w:1:0` and
        //      `w:1:1` (the round-2 default `w:1` / `w:1:1` is gone). Both
        //      inputs are appended after create, so tick 1 yields both, in
        //      input order.
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
        assert_eq!((events[0].event_id.as_str(), events[1].event_id.as_str()), ("w:1:0", "w:1:1"));
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
        //      the watch lock (X3) plus a due-time recheck under it make the
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
    async fn a_tick_waits_for_its_watch_lock_and_proceeds_after_it_is_released() {
        // why: X3 (was V4/V5 "global lock"): a tick holds ITS WATCH'S lock
        //      (`locks/<id>.lock`) across sensing and write. While another
        //      holder has it, tick_due must not complete and must write
        //      nothing; after the release it completes with the tick
        //      recorded. Contention is shown by contrast, not assumed: the
        //      SAME call on an unlocked watch finishes inside the 300 ms
        //      window (control), the locked one does not. (Blocking flock or
        //      polling is the implementer's choice, so /proc/locks is not
        //      consulted here; the store test does that for the lock types.)
        //      Multi-thread so a blocking flock inside the tick cannot freeze
        //      this test.
        // proves: CLAIM(tick-lock)
        let f = fixture();
        let (id, _) = f.baselined_change_watch("w").await;
        let (control, _) = f.baselined_change_watch("control").await;
        f.clock.advance_secs(20);
        let window = Duration::from_millis(300);

        let uncontended = tokio::time::timeout(window, f.env.another_process().tick_due(&control)).await;
        assert_eq!(uncontended.expect("control: an unlocked tick completes quickly").unwrap(), TickOutcome::Ticked);

        let guard = f.env.vigil.store().lock_watch(&id).unwrap();
        let before = std::fs::read(f.env.vigil.store().watch_path(&id)).unwrap();
        let vigil = Arc::new(f.env.another_process());
        let tick = tokio::spawn({
            let (vigil, id) = (vigil.clone(), id.clone());
            async move { vigil.tick_due(&id).await.unwrap() }
        });
        tokio::time::sleep(window).await;
        assert!(!tick.is_finished(), "tick completed while its watch lock was held");
        assert_eq!(std::fs::read(f.env.vigil.store().watch_path(&id)).unwrap(), before, "nothing written under contention");
        drop(guard);
        assert_eq!(tokio::time::timeout(Duration::from_secs(5), tick).await.expect("tick must proceed after release").unwrap(), TickOutcome::Ticked);
        assert_eq!(f.load(&id).tick, 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tick_does_not_need_the_global_lock_and_other_watches_locks_do_not_block_it() {
        // why: X3 (T4): the global lock is held only for namespace operations
        //      (id reservation, prune), "never during sensing", and a watch's
        //      tick takes only its own watch lock. Holding the global lock AND
        //      another watch's lock must not delay this watch's tick: it
        //      completes inside 2 s (the old single global lock would hang it).
        //      Positive control: holding THIS watch's own lock does block it
        //      (previous test), so the lock being held is a real obstacle.
        // proves: CLAIM(per-watch-lock)
        let f = fixture();
        let (id, _) = f.baselined_change_watch("w").await;
        let (other, _) = f.baselined_change_watch("other").await;
        f.clock.advance_secs(20);
        let store = f.env.vigil.store();
        let _global = store.lock().unwrap();
        let _others_lock = store.lock_watch(&other).unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(2), f.env.another_process().tick_due(&id))
            .await
            .expect("a tick of w blocked behind the global lock or another watch's lock");
        assert_eq!(outcome.unwrap(), TickOutcome::Ticked);
        assert_eq!(f.load(&id).tick, 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_slow_input_on_watch_a_does_not_block_a_tick_of_watch_b() {
        // why: X3 (T4), the headline: sensing for watch A (a cmd that hangs on
        //      a gate here) must not stop watch B from ticking, listing,
        //      cancelling or creating, which the single global lock forbade.
        //      Sequence: A's tick is parked INSIDE its input read (the reader
        //      reports it started; no sleeps decide this). Then B's tick must
        //      finish within 2 s while A is still sensing (A's task is not
        //      finished and A's file still says tick 0), `list` and a cancel of
        //      a third watch must not block either, and the global lock is
        //      free. Finally the gate opens and A completes its own tick.
        // proves: CLAIM(per-watch-lock)
        let f = fixture();
        let slow = Input::Cmd { argv: vec!["slow-a".into()], timeout_s: Some(19.0), cwd: None };
        let mut request_a = change_request("a", Path::new("/unused"), 20);
        request_a.inputs = vec![slow];
        f.env.reader.set_text("cmd:slow-a", "base");
        let a = f.create(request_a).await;
        let (b, b_path) = f.baselined_change_watch("b").await;
        let (c, _) = f.baselined_change_watch("c").await;
        append_file(&b_path, "b changed\n");
        f.clock.advance_secs(20);

        let gate = Gate::closed();
        f.env.reader.set_gate("cmd:slow-a", Some(gate.clone()));
        f.env.reader.set_text("cmd:slow-a", "a-changed");
        let started_before = f.env.reader.started_count("cmd:slow-a");
        let a_task = tokio::spawn({
            let (vigil, a) = (Arc::new(f.env.another_process()), a.clone());
            async move { vigil.tick_due(&a).await.unwrap() }
        });
        tokio::time::timeout(Duration::from_secs(5), f.env.reader.wait_started("cmd:slow-a", started_before + 1)).await.expect("A's tick never reached its input read");

        let b_outcome = tokio::time::timeout(Duration::from_secs(2), f.env.another_process().tick_due(&b))
            .await
            .expect("B's tick blocked behind A's sensing");
        assert_eq!(b_outcome.unwrap(), TickOutcome::Ticked);
        assert_eq!(f.load(&b).status, WatchStatus::Fired, "B really ticked and saw its change");
        assert!(!a_task.is_finished(), "A must still be sensing while B finished");
        assert_eq!(f.load(&a).tick, 0, "A has written nothing yet");
        let listed = tokio::time::timeout(Duration::from_secs(2), f.env.another_process().list()).await.expect("list blocked behind A's sensing").unwrap();
        assert_eq!(listed.len(), 3);
        let cancelled = tokio::time::timeout(Duration::from_secs(2), f.env.another_process().cancel(&c)).await.expect("cancel of C blocked behind A's sensing").unwrap();
        assert_eq!(cancelled.status, WatchStatus::Cancelled);
        assert!(f.env.vigil.store().try_lock().unwrap().is_some(), "the global lock is not held during sensing");

        gate.open();
        assert_eq!(tokio::time::timeout(Duration::from_secs(5), a_task).await.unwrap().unwrap(), TickOutcome::Ticked);
        let file = f.load(&a);
        assert_eq!((file.tick, file.status), (1, WatchStatus::Fired), "A finished its own tick: the change it read fired it");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_cancel_of_watch_a_waits_for_a_tick_in_flight_on_a_and_both_agree() {
        // why: X3: the watch lock is "held across a tick's sensing and write
        //      and across cancel". A tick parked in its read holds A's lock, so
        //      a cancel of A must wait (not interleave, not lose the tick's
        //      result): it stays pending while the gate is closed; once the
        //      gate opens the tick fires A (the read changed), and the cancel
        //      then finds the fired result and returns it unchanged (V4).
        // proves: CLAIM(per-watch-lock)
        let f = fixture();
        let mut request = change_request("a", Path::new("/unused"), 20);
        request.inputs = vec![Input::Cmd { argv: vec!["slow-a".into()], timeout_s: Some(19.0), cwd: None }];
        f.env.reader.set_text("cmd:slow-a", "base");
        let a = f.create(request).await;
        let gate = Gate::closed();
        f.env.reader.set_gate("cmd:slow-a", Some(gate.clone()));
        f.env.reader.set_text("cmd:slow-a", "changed");
        let before = f.env.reader.started_count("cmd:slow-a");
        let tick = tokio::spawn({
            let (vigil, a) = (Arc::new(f.env.another_process()), a.clone());
            async move { vigil.tick_due(&a).await.unwrap() }
        });
        tokio::time::timeout(Duration::from_secs(5), f.env.reader.wait_started("cmd:slow-a", before + 1)).await.unwrap();
        let cancel = tokio::spawn({
            let (vigil, a) = (Arc::new(f.env.another_process()), a.clone());
            async move { vigil.cancel(&a).await.unwrap() }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!cancel.is_finished(), "cancel interleaved with a tick that holds the watch lock");
        assert_eq!(f.load(&a).status, WatchStatus::Pending);

        gate.open();
        assert_eq!(tokio::time::timeout(Duration::from_secs(5), tick).await.unwrap().unwrap(), TickOutcome::Ticked);
        let envelope = tokio::time::timeout(Duration::from_secs(5), cancel).await.unwrap().unwrap();
        assert_eq!(envelope.status, WatchStatus::Fired, "the tick won; cancel returns the stored result");
        assert_eq!(envelope.events, f.load(&a).events);
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
        assert_eq!(file.events[0].event_id, "w:4:0", "W1: even a lone event carries :0");
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

    // ---- W2 (T6): the judge state floor through the engine ----

    #[tokio::test]
    async fn a_below_floor_judge_state_env_value_gives_an_effective_limit_of_2560_through_the_engine() {
        // why: W2 + T6: the old tests used limits >= 4000 or the helper's own
        //      1536 floor, so they passed without the config clamp. Here the
        //      Config comes from the ENVIRONMENT path (`from_lookup`) with
        //      VIGIL_JUDGE_STATE_BYTES=100 and the real engine builds the
        //      state from 4096 bytes of new log (tail_bytes default). Without
        //      the clamp the helper's own floor gives 53 (prompt) + 1536 =
        //      1589 bytes at most; with it the limit is 2560. So the state
        //      must be longer than 2410 (2560 minus 150 for label and
        //      separators, which count inside the budget) and at most 2560.
        //      Above-floor override: 3000 gives (2850, 3000]. The newest line
        //      is kept in both.
        // proves: CLAIM(config-floors)
        for (raw, floor, ceiling) in [("100", 2410usize, 2560usize), ("3000", 2850, 3000)] {
            let config = crate::config::Config::from_lookup(|name| (name == "VIGIL_JUDGE_STATE_BYTES").then(|| raw.to_string()));
            let clock = Arc::new(ManualClock::new(spec_instant()));
            let env = Env::with_clock_and_config(clock.clone(), config);
            let path = env.input_path("big.log");
            write_file(&path, "old\n");
            let id = env.vigil.create(judge_request("w", &path, 20)).await.unwrap().watch_id;
            env.judge.set_fallback(Ok(noul_answers("failed", 0.0, 1.0)));
            let big: String = (0..500).map(|i| format!("entry-{i:04}\n")).collect(); // 6000 bytes, the last 4096 are read
            append_file(&path, &big);
            env.vigil.tick_due(&id).await.unwrap();
            let state = &env.judge.requests()[0].state;
            assert!(state.len() <= ceiling, "VIGIL_JUDGE_STATE_BYTES={raw}: {} bytes exceed {ceiling}", state.len());
            assert!(state.len() > floor, "VIGIL_JUDGE_STATE_BYTES={raw}: only {} bytes, the effective limit is {ceiling}", state.len());
            assert!(state.contains("entry-0499"), "newest line kept");
        }
    }

    // ---- W7 (T7): the persisted pending change is bounded ----

    #[tokio::test]
    async fn a_pending_change_over_64_kib_is_cut_to_the_newest_content_across_failures_and_a_restart() {
        // why: W7 + T7: "the persisted pending change is capped at 64 KiB per
        //      input, newest kept, [truncated] when cut". The outage test
        //      (3.2 KB + 1.5 KB) never got near the cap, so unlimited
        //      accumulation passed. Here ONE input gets two 40800-byte
        //      appends (3400 lines of 12 bytes each; tail_bytes 1 MB so the
        //      file reader keeps all of each) during two judge failures:
        //      2 x 40800 = 81600 > 65536 bytes pending. Then a NEW process
        //      recovers. Arithmetic: 81600 - 65536 = 16064 oldest bytes must
        //      go = about the first 1339 alpha lines; beta (40800) is kept
        //      whole. Persisted after the second failure: one changed entry of
        //      at most 65536 + 12 (marker) + 1 bytes that says [truncated],
        //      holds the newest line (beta-03399) and the oldest kept beta
        //      line (beta-00000) and the newest alpha line (alpha-03399), and
        //      NOT alpha-00000. The recovery request (default 2560-byte limit)
        //      carries the newest content first, identical to the request
        //      before the restart, and the answer fires and clears the pending
        //      change and the strikes.
        // proves: CLAIM(pending-cap)
        let f = fixture();
        let path = f.env.input_path("w7.log");
        write_file(&path, "old\n");
        let mut request = judge_request("w", &path, 20);
        request.inputs = vec![Input::File { path: path.clone(), tail_bytes: Some(1_000_000) }];
        let id = f.create(request).await;
        assert_eq!(f.tick(&id).await, TickOutcome::Ticked);
        f.env.judge.push(Err(JudgeError::Timeout));
        f.env.judge.push(Err(JudgeError::Status(500)));
        f.env.judge.push(Ok(noul_answers("failed", 0.95, 0.9)));
        let lines = |prefix: &str| -> String { (0..3400).map(|i| format!("{prefix}-{i:05}\n")).collect() };
        assert_eq!(lines("alpha").len(), 40_800, "arithmetic of the test: 3400 x 12 bytes");

        append_file(&path, &lines("alpha"));
        f.next_tick(&id, 20).await;
        append_file(&path, &lines("beta"));
        f.next_tick(&id, 20).await;

        let file = f.load(&id);
        assert_eq!((file.status, file.judge_strikes), (WatchStatus::Pending, 2), "precondition: two failures, still pending");
        let pending = file.pending_change.expect("the change is retained");
        assert_eq!(pending.changed.len(), 1, "one input, one persisted entry: {:?}", pending.changed.iter().map(|c| &c.label).collect::<Vec<_>>());
        let text = &pending.changed[0].text;
        assert!(text.len() <= 64 * 1024 + crate::inputs::TRUNCATED_MARKER.len() + 1, "persisted {} bytes", text.len());
        assert!(text.contains(crate::inputs::TRUNCATED_MARKER), "a cut must say so");
        assert!(text.contains("beta-03399") && text.contains("beta-00000"), "the newest content is kept whole");
        assert!(text.contains("alpha-03399"), "the newest of the older content is kept");
        assert!(!text.contains("alpha-00000"), "the oldest content is what is dropped");

        let second_process = f.env.another_process();
        f.clock.advance_secs(20);
        assert_eq!(second_process.tick_due(&id).await.unwrap(), TickOutcome::Ticked);
        let requests = f.env.judge.requests();
        assert_eq!(requests.len(), 3);
        for (index, request) in requests.iter().enumerate().skip(1) {
            let state = &request.state;
            assert!(state.len() <= 2560, "request {index}: {} bytes", state.len());
            assert!(state.starts_with("Tell me if the build log shows a failure or finishes."));
            assert!(state.contains("beta-03399"), "request {index}: newest content first");
            assert!(!state.contains("alpha-00000") && !state.contains("beta-00000"), "request {index}: cut to the budget");
        }
        assert_eq!(requests[1].state, requests[2].state, "the restart does not change what is asked");
        let after = f.load(&id);
        assert_eq!((after.status, after.judge_strikes), (WatchStatus::Fired, 0));
        assert!(after.pending_change.is_none());
        assert_eq!(after.events[0].judge, Some(noul_answers("failed", 0.95, 0.9)));
    }

    // ---- X2: event summaries are bounded ----

    #[tokio::test]
    async fn an_event_summary_is_at_most_200_bytes_for_change_and_judge_events_and_short_lines_are_not_cut() {
        // why: X2 (S12): "an event summary at most 200 bytes" so an envelope
        //      is bounded by the digest limit + 16 x 200 + fixed fields.
        //      Change events: a 5000-byte ASCII line and a 3000 x "é" line
        //      (6000 bytes, cut on a UTF-8 boundary: the String type would
        //      panic on a mid-character slice) each give a summary of at most
        //      200 bytes that still carries content (>= 100 of the line's
        //      characters, so an over-eager cap fails). Judge event: same
        //      bound. Control: a 100-byte line is not cut at all (it fits even if the
        //      summary also carries the input label).
        // proves: CLAIM(summary-cap)
        let f = fixture();
        let cases: Vec<(&str, String, usize)> = vec![
            ("ascii", format!("{}\n", "y".repeat(5000)), 100),
            ("accents", format!("{}\n", "é".repeat(3000)), 40),
            ("short", format!("{}\n", "z".repeat(100)), 100),
        ];
        for (name, line, min_kept) in cases {
            let path = f.env.input_path(&format!("{name}.log"));
            write_file(&path, "old\n");
            let id = f.create(change_request(name, &path, 20)).await;
            append_file(&path, &line);
            f.tick(&id).await;
            let events = f.load(&id).events;
            assert_eq!(events.len(), 1, "{name}");
            let summary = &events[0].summary;
            assert!(summary.len() <= 200, "{name}: summary is {} bytes", summary.len());
            let marker_char = line.chars().next().unwrap();
            assert!(summary.chars().filter(|c| *c == marker_char).count() >= min_kept, "{name}: summary lost its content: {summary:?}");
        }
        let (id, path) = f.baselined_judge_watch("judged").await;
        f.env.judge.push(Ok(noul_answers("failed", 0.99, 0.9)));
        append_file(&path, &format!("{}\n", "j".repeat(5000)));
        f.next_tick(&id, 20).await;
        let events = f.load(&id).events;
        assert_eq!(events.len(), 1);
        assert!(events[0].summary.len() <= 200, "judge event summary is {} bytes", events[0].summary.len());
        assert!(events[0].summary.contains("jjjj"));
    }

    // ---- confinement ----

    #[tokio::test]
    async fn a_full_lifecycle_writes_only_lock_and_json_files_under_the_state_root() {
        // why: SPEC non-goals/"State": nothing is written outside
        //      `$XDG_STATE_HOME/vigil/`, and no `.tmp` is left behind. X3: the
        //      only non-json files are the global `lock` in the root and one
        //      `locks/<id>.lock` per existing watch. Scans the
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
            // DF4 (specs/disk-full.md): the one extra file is the reserve,
            // and only at exactly 256 KiB. Optional here (its presence is
            // pinned by the disk-full tests); anything else stays forbidden.
            let allowed = relative == "lock"
                || (relative == ".reserve" && std::fs::metadata(file).unwrap().len() == 262_144)
                || (relative.starts_with("watches/") && relative.ends_with(".json"))
                || (relative.starts_with("locks/") && relative.ends_with(".lock"));
            assert!(allowed, "unexpected file under the state root: {relative}");
            if let Some(id) = relative.strip_prefix("locks/").and_then(|r| r.strip_suffix(".lock")) {
                assert!(state.join("watches").join(format!("{id}.json")).exists(), "orphan lock file {relative}: its watch file is gone");
            }
        }
        assert!(files.iter().any(|p| p.ends_with("watches/fired.json")), "control: scan sees the watch file");
        assert!(files.iter().any(|p| p == &state.join("lock")), "control: the global lock file exists");
        assert!(files.iter().any(|p| p == &state.join("locks").join("fired.lock")), "control: the watch lock file exists");
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

    // ---- C5: no write after the call budget ----

    #[tokio::test]
    async fn a_tick_whose_budget_is_already_spent_when_it_would_commit_writes_nothing() {
        // why: code review C5: `timeout_at` polls the inner future first, so a
        //      tick that resumed after the budget with its readings ready
        //      could still save offsets and events. The tick itself must check
        //      the monotonic budget deadline immediately before it persists.
        //      Here the deadline is already in the past when the (instant)
        //      readings are ready: the change is on disk waiting, the tick
        //      must report OutOfBudget and leave the watch file byte-for-byte
        //      as it was, so the change is not consumed. Control: with a
        //      deadline in the future the same tick commits and fires.
        // proves: CLAIM(call-budget)
        let f = fixture();
        let (id, path) = f.baselined_change_watch("w").await;
        append_file(&path, "ERROR: ready but too late\n");
        f.clock.advance_secs(20);
        let before = std::fs::read(f.env.vigil.store().watch_path(&id)).unwrap();

        let spent = tokio::time::Instant::now() - Duration::from_secs(1);
        let report = f.env.vigil.tick_report_until(&id, Some(spent)).await.unwrap();
        assert_eq!(report.outcome, TickOutcome::OutOfBudget);
        assert!(report.changed.is_empty());
        assert_eq!(std::fs::read(f.env.vigil.store().watch_path(&id)).unwrap(), before, "an out-of-budget tick writes nothing");

        let open = tokio::time::Instant::now() + Duration::from_secs(60);
        let report = f.env.vigil.tick_report_until(&id, Some(open)).await.unwrap();
        assert_eq!(report.outcome, TickOutcome::Ticked, "control: inside the budget the change is still there");
        assert_eq!(f.load(&id).status, WatchStatus::Fired);
    }
}
