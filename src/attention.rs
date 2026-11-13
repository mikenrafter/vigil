//! Early attention (specs/auto-interval.md AI5, S6): when a watch has run
//! longer than its owner expected, or its inputs have gone quiet while its
//! process still runs, the call ends early with one `attention` event.
//!
//! The rules here are pure: given a watch, the time and whether the call
//! window is long, [`due_reasons`] says which reasons are due. `Vigil`
//! (keepalive.rs) turns each due reason into an event, persists it with the
//! watch and delivers it like any other event.
//!
//! W42-H3: a hold's items raise their own `overdue` (per item, once, from the
//! item's own clock, `items.rs`); [`Attention`] names either kind, and
//! [`due`], [`next_due_at`], [`event`], [`mark_raised`] are the one entry the
//! keepalive loop uses for both.
//!
//! Invariants: a reason is raised at most once per watch (`attention_raised`
//! is saved together with the event, S6), an item's overdue at most once per
//! item (`overdue_raised`, D3); a finished watch raises nothing (A15);
//! `Stalled` needs a process input that the last tick saw running.
//! Mating parts: `tick.rs` (keeps `Activity`), `watch::WatchFile` (stores the
//! flags and the event until it is acknowledged), `items.rs` (item clocks).

use serde::{Deserialize, Serialize};

use crate::clock::Timestamp;
use crate::watch::{Event, EventDetail, FireRule, Input, WatchFile, WatchStatus, MAX_SUMMARY_BYTES};

/// AI5: "1.5x or 9 minutes over, whichever comes first" (build-loop budget rule).
pub const OVERDUE_EXTRA_CAP_S: u64 = 540;
/// AI5 / D3: the stall limit applied to a long window when the watch sets
/// neither `expect_s` nor `stall_s`.
pub const DEFAULT_STALL_S: u64 = 900;
/// AI5 (A1): a call window above this is "long".
pub const LONG_WINDOW_S: u64 = 270;

/// Why a watch needs the agent's attention. Wire: `"overdue"` / `"stalled"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionReason {
    /// It has run past its `expect_s` budget.
    Overdue,
    /// Nothing changed for `stall_s` while its process input still runs.
    Stalled,
}

/// The wire `reason` of an attention event: a watch-level reason, or W42-H3
/// `ended` (an item of a hold ended; an item's own overdue is `overdue`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventReason {
    Overdue,
    Stalled,
    Ended,
}

impl From<AttentionReason> for EventReason {
    fn from(reason: AttentionReason) -> EventReason {
        match reason {
            AttentionReason::Overdue => EventReason::Overdue,
            AttentionReason::Stalled => EventReason::Stalled,
        }
    }
}

/// One attention event a watch can owe its agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Attention {
    /// A watch-level reason (the watch's own `expect_s` / `stall_s`, AI5).
    Watch(AttentionReason),
    /// Item `index` of a hold ran past its own `expect_s` (W42-H3, D2).
    ItemOverdue(usize),
}

/// Every attention due for `watch` at `now`: the watch-level reasons (as
/// [`due_reasons`]), then each item that is overdue, in item order.
pub fn due(watch: &WatchFile, now: Timestamp, long_window: bool) -> Vec<Attention> {
    let mut due: Vec<Attention> = due_reasons(watch, now, long_window).into_iter().map(Attention::Watch).collect();
    if watch.status == WatchStatus::Pending {
        for (index, item) in watch.items.iter().flatten().enumerate() {
            if crate::items::overdue_at(item).is_some_and(|at| now >= at) {
                due.push(Attention::ItemOverdue(index));
            }
        }
    }
    due
}

/// The earliest moment an overdue (the watch's or an item's) can still come
/// due: a wake of its own however rarely the watch ticks (AI5, D2).
pub fn next_due_at(watch: &WatchFile) -> Option<Timestamp> {
    if watch.status != WatchStatus::Pending {
        return None;
    }
    let items = watch.items.iter().flatten().filter_map(crate::items::overdue_at);
    overdue_at(watch).into_iter().chain(items).min()
}

/// The event for `attention` (watch-level: [`event_for`]; an item: its
/// `item overdue: <label>`, CLAIM(item-event-id)).
pub fn event(watch: &WatchFile, attention: Attention, now: Timestamp) -> Option<Event> {
    match attention {
        Attention::Watch(reason) => Some(event_for(watch, reason, now)),
        Attention::ItemOverdue(index) => {
            let item = watch.items.as_ref()?.get(index)?;
            Some(crate::items::overdue_event(&watch.spec.id, watch.tick, index, item))
        }
    }
}

/// Record that `attention` was raised, so it is never raised again.
pub fn mark_raised(watch: &mut WatchFile, attention: Attention) {
    match attention {
        Attention::Watch(reason) => watch.attention_raised.push(reason),
        Attention::ItemOverdue(index) => {
            if let Some(item) = watch.items.as_mut().and_then(|items| items.get_mut(index)) {
                item.overdue_raised = true;
            }
        }
    }
}

/// Is `attention` already raised in `watch` (by this process or another)?
pub fn is_raised(watch: &WatchFile, attention: Attention) -> bool {
    match attention {
        Attention::Watch(reason) => watch.attention_raised.contains(&reason),
        Attention::ItemOverdue(index) => watch.items.as_ref().and_then(|items| items.get(index)).is_some_and(|item| item.overdue_raised),
    }
}

/// What the ticks have seen so far, kept in the watch file so a new process
/// continues the same stall clock (A15: once per reason holds across processes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    /// When any input last changed (a tick that saw a change).
    pub last_change_at: Timestamp,
    /// When a `file`, `glob` or `cmd` input last changed: the clock of the
    /// default stall rule (AI5 / D3).
    pub last_output_change_at: Timestamp,
    /// Did the latest tick see a `process` input running?
    pub process_running: bool,
}

/// The elapsed time since create at which `overdue` fires, in whole seconds:
/// `min(1.5 x expect_s, expect_s + 540)`. A15: inclusive; an odd `expect_s`
/// rounds the 1.5x up (the spec leaves it open).
pub fn overdue_after_s(expect_s: u64) -> u64 {
    expect_s.saturating_mul(3).div_ceil(2).min(expect_s.saturating_add(OVERDUE_EXTRA_CAP_S))
}

/// The moment `overdue` fires for `watch`, if it still can.
pub fn overdue_at(watch: &WatchFile) -> Option<Timestamp> {
    let expect_s = watch.spec.expect_s?;
    (watch.status == WatchStatus::Pending && !watch.attention_raised.contains(&AttentionReason::Overdue))
        .then(|| watch.created_at.plus_secs(overdue_after_s(expect_s)))
}

/// The reasons that are due for `watch` at `now`, in the order they are
/// raised (overdue, then stalled). `long_window` is whether this call's window
/// is longer than [`LONG_WINDOW_S`] (the default stall applies to long calls only).
pub fn due_reasons(watch: &WatchFile, now: Timestamp, long_window: bool) -> Vec<AttentionReason> {
    if watch.status != WatchStatus::Pending {
        return Vec::new();
    }
    let mut due = Vec::new();
    if overdue_at(watch).is_some_and(|at| now >= at) {
        due.push(AttentionReason::Overdue);
    }
    if !watch.attention_raised.contains(&AttentionReason::Stalled) {
        let activity = watch.activity.unwrap_or(Activity { last_change_at: watch.created_at, last_output_change_at: watch.created_at, process_running: false });
        // A15: "neither field set" is literal; `expect_s` alone turns the default off.
        let rule = match (watch.spec.stall_s, watch.spec.expect_s) {
            (Some(stall_s), _) => Some((stall_s, activity.last_change_at)),
            // H8: a hold waits by definition; the default stall never applies to it.
            (None, None) if long_window && !matches!(watch.spec.fire, FireRule::Hold) => Some((DEFAULT_STALL_S, activity.last_output_change_at)),
            (None, _) => None,
        };
        if let Some((stall_s, since)) = rule
            && activity.process_running
            // ↳ CLAIM(S8-overflow): compared as unsigned seconds, so a huge limit is never reached and a clock that went backwards is no quiet time
            && u64::try_from(now.secs_since(since)).is_ok_and(|quiet| quiet >= stall_s)
        {
            due.push(AttentionReason::Stalled);
        }
    }
    due
}

/// The event for `reason`, with its W1 id `<watch>:<tick>:<n>` where `n`
/// counts the reasons raised before it, so two events at one tick differ (S6).
pub fn event_for(watch: &WatchFile, reason: AttentionReason, now: Timestamp) -> Event {
    let age = format_span(now.secs_since(watch.created_at).max(0) as u64);
    let (input, summary) = match reason {
        AttentionReason::Overdue => {
            let expected = format_span(watch.spec.expect_s.unwrap_or(0));
            ("watch".to_string(), format!("overdue: running for {age}, expected {expected}"))
        }
        AttentionReason::Stalled => {
            let label = watch.spec.inputs.iter().find(|input| matches!(input, Input::Process { .. })).map_or_else(|| "watch".to_string(), Input::label);
            let quiet = watch.activity.map_or(0, |activity| now.secs_since(activity.last_change_at).max(0) as u64);
            (label.clone(), format!("stalled: no input changed for {} while {label} runs", format_span(quiet)))
        }
    };
    Event {
        event_id: Event::make_id(watch.spec.id.as_str(), watch.tick, watch.attention_raised.len()),
        tick: watch.tick,
        watch_id: watch.spec.id.to_string(),
        input,
        summary: crate::tick::cap_bytes(&summary, MAX_SUMMARY_BYTES),
        judge: None,
        redelivered: false,
        detail: Some(EventDetail::Attention { reason: Some(reason.into()) }),
    }
}

/// `4m 30s`, `1h 5m`, `45s`: the same tiers as the clock summary.
fn format_span(seconds: u64) -> String {
    crate::envelope::format_span(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overdue_threshold_is_one_and_a_half_times_or_nine_minutes_over_whichever_is_first() {
        // why: AI5 / D2 (the build-loop budget rule). Hand-computed oracle,
        //      all expect_s even so 1.5x is exact (A15):
        //        60   -> min(90, 600)     = 90
        //        100  -> min(150, 640)    = 150
        //        1000 -> min(1500, 1540)  = 1500
        //        1078 -> min(1617, 1618)  = 1617   (1.5x still smaller by 1)
        //        1080 -> min(1620, 1620)  = 1620   (crossover)
        //        1082 -> min(1623, 1622)  = 1622   (+540 smaller by 1)
        //        2000 -> min(3000, 2540)  = 2540
        //        3600 -> min(5400, 4140)  = 4140
        //      The two neighbours of the crossover kill a swapped min/max and
        //      a mistyped 540.
        // proves: CLAIM(AI5-threshold)
        let cases = [(60, 90), (100, 150), (1000, 1500), (1078, 1617), (1080, 1620), (1082, 1622), (2000, 2540), (3600, 4140)];
        for (expect_s, expected) in cases {
            assert_eq!(overdue_after_s(expect_s), expected, "expect_s {expect_s}");
        }
    }

    #[test]
    fn the_shipped_attention_constants_are_the_spec_numbers() {
        // why: AI5 / D3 fix 540 s, 900 s and the 270 s "long" line. A change
        //      detector on purpose: each is a spec number, not an
        //      implementation choice.
        // proves: CLAIM(AI5-threshold)
        assert_eq!((OVERDUE_EXTRA_CAP_S, DEFAULT_STALL_S, LONG_WINDOW_S), (540, 900, 270));
    }

    #[test]
    fn limits_at_the_edge_of_u64_never_fire_and_never_panic() {
        // why: review S8. `plus_secs(overdue_after_s(u64::MAX))` wrapped to a
        //      negative duration (a panic), and `stall_s as i64` made u64::MAX
        //      -1 ("stalled for -1 s" is always true). A limit that cannot be
        //      reached is "never": with a live process, a created-at far in the
        //      past and a `now` 100 years later, neither reason is due. Control
        //      with the same watch and a 1 s limit: both are.
        // proves: CLAIM(AI5), CLAIM(S8-overflow)
        use crate::testing::{at, change_request};
        let created = at("2026-10-29T14:00:00Z");
        let now = at("2126-10-29T14:00:00Z");
        let watch_with = |limit: u64| {
            let mut spec = crate::watch::WatchSpec::parse(change_request("w", std::path::Path::new("/tmp/x.log"), 20)).unwrap();
            (spec.expect_s, spec.stall_s) = (Some(limit), Some(limit));
            let mut watch = WatchFile::new(spec, created);
            watch.activity = Some(Activity { last_change_at: created, last_output_change_at: created, process_running: true });
            watch
        };
        assert_eq!(due_reasons(&watch_with(1), now, false), vec![AttentionReason::Overdue, AttentionReason::Stalled], "control");
        for limit in [u64::MAX, u64::MAX / 2, i64::MAX as u64, i64::MAX as u64 + 1] {
            let watch = watch_with(limit);
            let _ = overdue_at(&watch);
            assert_eq!(due_reasons(&watch, now, false), Vec::new(), "limit {limit}");
        }
    }
}
