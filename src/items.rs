//! Hold items (W42; claims W42-H1..H5 and the round decisions W42-D1..D9,
//! from a design note kept outside this repository): one hold, many units of
//! work, each ending on its own.
//!
//! A hold made by `vigil hold` stores `items` beside its inputs. An item is
//! one unit of work: sensed through one input of the hold (`qex:`, `pid:`,
//! `file:`, `until:`, `idle:`), or `none` (a delegated run with no pid or
//! file to watch, such as a T3 Code delegate task, ended by `vigil hold
//! done`). Each has a label unique in its
//! hold, an optional `expect_s`, and its own clocks and flags (D3).
//!
//! Why a module: this is the item model and the ONE place that decides item
//! events. [`tick_items`] decides, at a tick, which items ended (and whether
//! the hold is released); [`overdue_at`] / [`overdue_event`] decide the
//! per-item overdue (raised by the attention pass, `attention::due`). The
//! front doors that change items are `Vigil::hold`, `Vigil::hold_add` and
//! `Vigil::hold_done` (hold.rs); nothing else writes `items`.
//!
//! Invariants (kept under the hold lock):
//! - labels are unique among retained items;
//! - live items count toward MAX_ITEMS; completed inputs retire on admission;
//! - ended history is bounded by MAX_ENDED_HISTORY; undelivered events survive;
//! - an item's sequence is stable when history and inputs are compacted;
//! - `last_end_event` is set only while `ended_at` is.
//!
//! A hold stored without `items` (by an older vigil, or made with
//! `watch_create`) keeps the old behaviour: no item events, the release names
//! each input and its reading (D6, `tick::hold_events`).

use serde::{Deserialize, Serialize};

use crate::attention::{overdue_after_s, EventReason};
use crate::clock::Timestamp;
use crate::error::ItemError;
use crate::hold::Work;
use crate::inputs::Reading;
use crate::tick::cap_bytes;
use crate::watch::{ChangedInput, Event, EventDetail, WatchId, MAX_INPUTS, MAX_SUMMARY_BYTES};

/// Maximum live work items; ended history does not consume capacity.
pub const MAX_ITEMS: usize = MAX_INPUTS;
/// Keep the most recently added 64 ended items; undelivered events remain in the outbox.
pub const MAX_ENDED_HISTORY: usize = 64;
/// An explicit label is at most this many bytes (a label lives in event
/// summaries of at most 200 bytes, and the release lists several).
pub const MAX_LABEL_BYTES: usize = 64;
/// The `--for` spec of an item that nothing senses (W42-H5).
pub const NONE_SPEC: &str = "none";

/// One unit of work of a hold, as stored in the watch file (`items`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HoldItem {
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub meta: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<std::path::PathBuf>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ledger_recorded: bool,
    /// Unique in the hold. `--label`, else the `--for` spec as given (D5).
    pub label: String,
    /// Absent on old files: their original vector index is the identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<usize>,
    /// Reading retained when a completed input is retired to free capacity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reading: Option<String>,
    /// W46-K1: absent means uncounted, including items stored by older vigil.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap: Option<crate::cap::CapClass>,
    /// Index of the input in `spec.inputs` that senses this item; absent for
    /// a `none` item, which only `hold done` ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<usize>,
    /// How long this item should take; overdue after
    /// `min(1.5 x expect_s, expect_s + 540)` from `added_at` (D2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_s: Option<u64>,
    /// D3: when the item was added; its overdue clock starts here.
    pub added_at: Timestamp,
    /// D3: when the item was last seen ended (a tick, or `hold done`);
    /// cleared when a sensed item reads live again (D1: a retried job).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<Timestamp>,
    /// D3: the overdue event was raised (once per item; survives restarts).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub overdue_raised: bool,
    /// Escalation at twice the estimate was raised, once per item.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub far_overdue_raised: bool,
    /// D3: id of the `item ended` event that announced the current end;
    /// absent while the item is live or its end is not announced yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_end_event: Option<String>,
}

/// What an item waits on.
#[derive(Debug, Clone, PartialEq)]
pub enum ItemWork {
    /// Read by an input of the hold at every tick.
    Sensed(Work),
    /// `none`: nothing senses it; `hold done <id> <label>` ends it (W42-H5).
    Unsensed,
}

/// A checked item request: what `vigil hold --for/--label/--expect-s` and
/// `vigil hold add` ask for, before it is placed in a hold. Parse, don't
/// validate: holding one means the spec, label and limit are valid.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemDraft {
    pub label: String,
    pub work: ItemWork,
    pub expect_s: Option<u64>,
}

impl ItemDraft {
    /// The input that would sense this item, before paths are made absolute
    /// or a qex job is resolved; `None` for a `none` item.
    pub fn sensed_input(&self) -> Option<crate::watch::Input> {
        match &self.work {
            ItemWork::Sensed(work) => Some(work.clone().into_input()),
            ItemWork::Unsensed => None,
        }
    }

    /// Check one item request. `spec` is a `--for` spec or `none`; `label`
    /// defaults to `spec` as given; a `none` item needs a label (it could
    /// never be ended otherwise); `expect_s` follows the watch rule (1 s up
    /// to the longest a watch lives).
    pub fn parse(spec: &str, label: Option<&str>, expect_s: Option<u64>) -> Result<ItemDraft, crate::error::VigilError> {
        crate::watch::check_seconds("expect_s", expect_s)?;
        let work = if spec == NONE_SPEC { ItemWork::Unsensed } else { ItemWork::Sensed(Work::parse(spec)?) };
        let label = match (label, &work) {
            (Some(label), _) => checked_label(label)?,
            (None, ItemWork::Unsensed) => return Err(ItemError::NoneNeedsLabel.into()),
            (None, ItemWork::Sensed(_)) => spec.to_string(),
        };
        Ok(ItemDraft { label, work, expect_s })
    }
}

/// An explicit label: 1..=64 bytes, no control characters, not padded.
fn checked_label(label: &str) -> Result<String, ItemError> {
    let reason = if label.is_empty() {
        Some("it is empty")
    } else if label.len() > MAX_LABEL_BYTES {
        Some("it is longer than 64 bytes")
    } else if label.chars().any(char::is_control) {
        Some("it contains a control character")
    } else if label.trim() != label {
        Some("it starts or ends with white space")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(ItemError::InvalidLabel { label: label.to_string(), reason }),
        None => Ok(label.to_string()),
    }
}

impl HoldItem {
    pub(crate) fn event_index(&self, fallback: usize) -> usize { self.sequence.unwrap_or(fallback) }

    /// A live item added at `added_at`, sensed by `spec.inputs[input]` if any.
    pub fn new(label: String, input: Option<usize>, expect_s: Option<u64>, added_at: Timestamp) -> HoldItem {
        HoldItem { meta: Default::default(), cwd: None, ledger_recorded: false, label, sequence: None, end_reading: None, cap: None, input, expect_s, added_at, ended_at: None, overdue_raised: false, far_overdue_raised: false, last_end_event: None }
    }
}

/// What one tick decided about the items of a hold.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ItemTick {
    /// The `item ended` events this tick raises, in item order.
    pub events: Vec<Event>,
    /// W42-H3, D1: every item is ended at this tick and there is at least
    /// one: the hold is released.
    pub released: bool,
    /// Every item that is ended at this tick, as `label` and why (for the
    /// release digest).
    pub ended: Vec<ChangedInput>,
}

/// CLAIM(item-event-id): every item event has its own id,
/// `<watch>:<tick>:<stable item sequence>`, and the release uses the next
/// sequence [W1, D3; proof: an item raises at most one event per tick number:
/// its end only at a tick, at most once per tick; its overdue at most once
/// ever, at a pass after tick T (id T) and only while it is not ended, so
/// never for an item that ended at tick T; `hold done` advances the event
/// epoch before recording its note; escalation uses another epoch after normal
/// overdue notes (CLAIM(overdue-event-epoch)); a hold has no watch-level attention (no watch
/// `expect_s`, no `stall_s`), so no other id scheme shares its ids; tests
/// two_items_ending_in_one_tick_give_two_events_with_distinct_event_ids and
/// two_items_going_overdue_in_one_tick_give_two_events_with_distinct_event_ids].
fn item_event_id(watch_id: &WatchId, tick: u64, index: usize) -> String {
    Event::make_id(watch_id.as_str(), tick, index)
}

/// An attention event about item `index` (`input` is its label).
fn item_event(watch_id: &WatchId, tick: u64, index: usize, item: &HoldItem, summary: String, reason: EventReason) -> Event {
    Event {
        event_id: item_event_id(watch_id, tick, item.event_index(index)),
        tick,
        watch_id: watch_id.to_string(),
        input: item.label.clone(),
        summary: cap_bytes(&summary, MAX_SUMMARY_BYTES),
        judge: None,
        redelivered: false,
        detail: Some(EventDetail::Attention { reason: Some(reason) }),
    }
}

/// W42-H3, D1, D3, D6: decide the items at tick `tick` from its `readings`
/// (one per `spec.inputs`, in order) and update their state.
///
/// An item is ended at this tick when its input reads ended (`want_met`), or,
/// for a `none` item, when `hold done` ended it. An ended item whose end is
/// not announced yet raises one `item ended: <label>: <why>` event (`why` is
/// the reading, or `done`). A sensed item that reads live again (a retried
/// qex job, D1) is live: its end is forgotten and a later end is announced
/// anew. An error reading (`is_error`) changes nothing: the item neither
/// ends nor un-ends, and it is not counted as ended at that tick. The hold
/// is released when every item is ended at this tick and it has at least one (a bare hold never releases, W42-H3).
pub(crate) fn tick_items(items: &mut [HoldItem], watch_id: &WatchId, tick: u64, readings: &[Reading], now: Timestamp) -> ItemTick {
    let mut events = Vec::new();
    let mut ended = Vec::new();
    for (index, item) in items.iter_mut().enumerate() {
        // S3: a failed read says nothing about the work, so it neither ends
        // an item nor un-ends one (ended_at and last_end_event stay as they
        // are); the item is not counted as ended this tick.
        if item.input.and_then(|input| readings.get(input)).is_some_and(|reading| reading.is_error) {
            continue;
        }
        let (is_ended, why) = match item.input {
            // A missing reading (a hand-damaged file) is not an end: a hold errs towards staying live.
            Some(input) => readings.get(input).map_or((false, String::new()), |reading| (reading.want_met == Some(true), reading.text.clone())),
            None => (item.ended_at.is_some(), item.end_reading.clone().unwrap_or_else(|| "done".to_string())),
        };
        if !is_ended {
            item.ended_at = None;
            item.last_end_event = None;
            item.end_reading = None;
            continue;
        }
        if item.last_end_event.is_none() {
            let mut event = item_event(watch_id, tick, index, item, format!("item ended: {}: {why}", item.label), EventReason::Ended);
            if item.input.is_none() { event.detail = Some(EventDetail::Note { reason: Some(EventReason::Ended) }); }
            item.end_reading = Some(why.clone());
            item.ended_at.get_or_insert(now);
            item.last_end_event = Some(event.event_id.clone());
            events.push(event);
        }
        ended.push(ChangedInput { label: item.label.clone(), text: why });
    }
    let released = !items.is_empty() && ended.len() == items.len();
    ItemTick { events, released, ended }
}

/// Record the caller's explicit completion under the hold lock. Advance the
/// event epoch so an overdue at the previous tick cannot share its event id.
pub(crate) fn record_done(watch: &mut crate::watch::WatchFile, index: usize, now: Timestamp) {
    watch.tick += 1;
    let item = &mut watch.items.as_mut().expect("checked hold")[index];
    item.ended_at = Some(now);
    item.end_reading = Some("done".into());
    let mut event = item_event(&watch.spec.id, watch.tick, index, item, format!("item ended: {}: done", item.label), EventReason::Ended);
    event.detail = Some(EventDetail::Note { reason: Some(EventReason::Ended) });
    item.last_end_event = Some(event.event_id.clone());
    watch.events.push(event);
}

/// CLAIM(hold-history): admission retires completed inputs and bounds ended
/// history without changing event identity [history_compaction_preserves_sensed_inputs_and_unique_event_ids].
/// Until admission a sensed retry can still revive an item; afterwards the
/// caller must add a new item for new work. All changes share the hold lock/save.
pub(crate) fn compact_history(watch: &mut crate::watch::WatchFile, retire_inputs: bool) {
    let Some(items) = watch.items.as_mut() else { return };
    for (index, item) in items.iter_mut().enumerate() {
        item.sequence.get_or_insert(index);
        watch.next_item_sequence = watch.next_item_sequence.max(item.event_index(index) + 1);
        if retire_inputs && item.ended_at.is_some() && item.last_end_event.is_some() { item.input = None; }
    }
    let mut remove = items.iter().filter(|item| item.ended_at.is_some() && item.last_end_event.is_some()).count().saturating_sub(MAX_ENDED_HISTORY);
    items.retain(|item| {
        if remove > 0 && item.ended_at.is_some() && item.last_end_event.is_some() { remove -= 1; false } else { true }
    });
    let mut inputs = Vec::new();
    let mut states = Vec::new();
    for item in items {
        if let Some(old) = item.input {
            // Invalid stored indices stay invalid; never manufacture a completion.
            if let Some(input) = watch.spec.inputs.get(old) {
                item.input = Some(inputs.len());
                inputs.push(input.clone());
                states.push(watch.input_states.get(old).cloned().unwrap_or_default());
            } else { item.input = Some(usize::MAX); }
        }
    }
    watch.spec.inputs = inputs;
    watch.input_states = states;
}

/// D2: when `item` goes overdue, if it still can: `expect_s` set, not raised
/// yet, and the item not ended. Counted from `added_at`, so an item added
/// late has its own clock and a restart does not reset it (D3).
pub fn overdue_at(item: &HoldItem) -> Option<Timestamp> {
    let expect_s = item.expect_s?;
    (!item.overdue_raised && item.ended_at.is_none()).then(|| item.added_at.plus_secs(overdue_after_s(expect_s)))
}

/// D3: the overdue event of item `index`, `item overdue: <label>`, raised
/// after tick `tick` (CLAIM(item-event-id)).
pub fn overdue_event(watch_id: &WatchId, tick: u64, index: usize, item: &HoldItem) -> Event {
    let mut event = item_event(watch_id, tick, index, item, format!("item overdue: {}", item.label), EventReason::Overdue);
    event.detail = Some(EventDetail::Note { reason: Some(EventReason::Overdue) });
    event
}

/// A stuck item's next interrupt. Normal overdue remains a note; the separate
/// flag ensures a delivered escalation never repeats after a restart.
pub fn far_overdue_at(item: &HoldItem) -> Option<Timestamp> {
    let expect_s = item.expect_s?;
    (!item.far_overdue_raised && item.ended_at.is_none()).then(|| item.added_at.plus_secs(expect_s.saturating_mul(2)))
}

pub fn far_overdue_event(watch_id: &WatchId, tick: u64, index: usize, item: &HoldItem) -> Event {
    item_event(watch_id, tick, index, item, format!("item far overdue: {}", item.label), EventReason::Overdue)
}

/// D6, D9: `hold released: <N> items ended (<labels>)`, the labels in item
/// order for as many as fit the summary cap (a prefix: the count says how
/// many there are). One item is `1 item ended`. With no label that fits,
/// no parentheses.
pub(crate) fn release_summary(items: &[HoldItem]) -> String {
    let count = items.len();
    let head = format!("hold released: {count} {} ended", if count == 1 { "item" } else { "items" });
    let mut labels = String::new();
    for item in items {
        let piece = if labels.is_empty() { item.label.clone() } else { format!(", {}", item.label) };
        // head + " (" + labels + piece + ")"
        if head.len() + 2 + labels.len() + piece.len() + 1 > MAX_SUMMARY_BYTES {
            break;
        }
        labels.push_str(&piece);
    }
    if labels.is_empty() { cap_bytes(&head, MAX_SUMMARY_BYTES) } else { format!("{head} ({labels})") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::at;
    use crate::watch::InputState;

    fn reading(text: &str, ended: bool) -> Reading {
        Reading { label: "x".into(), text: text.into(), changed: false, is_error: false, want_met: Some(ended), state: InputState::default() }
    }

    fn items() -> Vec<HoldItem> {
        let t0 = at("2026-11-13T10:00:00Z");
        vec![HoldItem::new("build".into(), Some(0), None, t0), HoldItem::new("t3".into(), None, Some(100), t0)]
    }

    #[test]
    fn an_item_request_is_checked_once_with_the_spec_as_its_default_label() {
        // why: D5, W42-H5 at the parse boundary. An unlabelled item is named by
        //      its spec as given; `none` needs a label; a label is bounded and
        //      printable; expect_s follows the watch rule (0 and > 604800 refused).
        // proves: W42-D5, W42-H5
        // catches: a default label taken from the resolved input (`process:1`
        //      instead of the `pid:1` the agent typed); `none` without a label
        //      accepted; an unbounded label.
        let draft = ItemDraft::parse("pid:1", None, Some(60)).unwrap();
        assert_eq!((draft.label.as_str(), draft.work, draft.expect_s), ("pid:1", ItemWork::Sensed(Work::Pid(1)), Some(60)));
        assert_eq!(ItemDraft::parse("none", Some("t3"), None).unwrap().work, ItemWork::Unsensed);
        assert!(ItemDraft::parse("none", None, None).unwrap_err().to_string().contains("label"));
        for bad in ["", " t3", "t3 ", "a\nb", &"x".repeat(65)] {
            assert!(ItemDraft::parse("none", Some(bad), None).is_err(), "{bad:?}");
        }
        assert!(ItemDraft::parse("none", Some(&"x".repeat(64)), None).is_ok(), "64 bytes is the bound");
        assert!(ItemDraft::parse("pid:1", None, Some(0)).unwrap_err().to_string().contains("expect_s"));
        assert!(ItemDraft::parse("pid:1", None, Some(604_801)).unwrap_err().to_string().contains("604800"));
        assert!(ItemDraft::parse("bogus:1", Some("b"), None).unwrap_err().to_string().contains("bogus:1"));
    }

    #[test]
    fn a_tick_announces_each_end_once_forgets_it_when_the_item_is_live_again_and_releases_only_when_all_have_ended() {
        // why: W42-H3, D1 on the pure decision: build ends (one event, id
        //      h:4:0), t3 is done (announced at the same tick, id h:4:1, why
        //      `done`) -> released. A repeat tick with the same readings
        //      announces nothing. A retry (build live) clears its end and blocks
        //      the release; the next end is a new event.
        // proves: W42-H3, W42-D1, CLAIM(item-event-id)
        // catches: an end announced at every tick; a sticky end that releases
        //      while the retried job runs; ids not by item index.
        let id = WatchId::parse("h").unwrap();
        let now = at("2026-11-13T10:05:00Z");
        let mut items = items();
        let live = tick_items(&mut items, &id, 3, &[reading("running", false)], now);
        assert_eq!((live.events.len(), live.released), (0, false));
        items[1].ended_at = Some(now);
        let both = tick_items(&mut items, &id, 4, &[reading("completed", true)], now);
        let summaries: Vec<(&str, &str)> = both.events.iter().map(|event| (event.event_id.as_str(), event.summary.as_str())).collect();
        assert_eq!(summaries, vec![("h:4:0", "item ended: build: completed"), ("h:4:1", "item ended: t3: done")]);
        assert!(both.released);
        assert_eq!(items[0].last_end_event.as_deref(), Some("h:4:0"));
        let again = tick_items(&mut items, &id, 5, &[reading("completed", true)], now);
        assert_eq!((again.events.len(), again.released), (0, true), "announced once");
        let retried = tick_items(&mut items, &id, 6, &[reading("queued", false)], now);
        assert_eq!((retried.events.len(), retried.released, items[0].ended_at, items[0].last_end_event.clone()), (0, false, None, None));
        let ended_again = tick_items(&mut items, &id, 7, &[reading("completed", true)], now);
        assert_eq!(ended_again.events.iter().map(|event| event.event_id.as_str()).collect::<Vec<_>>(), vec!["h:7:0"]);
        assert!(!tick_items(&mut [], &id, 1, &[], now).released, "a bare hold never releases");
    }

    #[test]
    fn an_items_overdue_moment_counts_from_its_own_add_and_stops_when_it_ended_or_was_raised() {
        // why: D2/D3: min(1.5 x expect_s, expect_s + 540) from added_at;
        //      100 -> 150 s, 2000 -> 2540 s (the capped branch). None once raised
        //      or once the item ended.
        // proves: W42-D2, W42-D3
        // catches: a clock from the hold's creation; overdue for an ended item;
        //      overdue raised twice.
        let t0 = at("2026-11-13T10:00:00Z");
        let mut item = HoldItem::new("t3".into(), None, Some(100), t0);
        assert_eq!(overdue_at(&item), Some(at("2026-11-13T10:02:30Z")));
        item.expect_s = Some(2000);
        assert_eq!(overdue_at(&item), Some(t0.plus_secs(2540)));
        item.overdue_raised = true;
        assert_eq!(overdue_at(&item), None);
        let mut ended = HoldItem::new("b".into(), Some(0), Some(100), t0);
        ended.ended_at = Some(t0);
        assert_eq!(overdue_at(&ended), None);
        assert_eq!(overdue_at(&HoldItem::new("c".into(), Some(0), None, t0)), None);
    }

    #[test]
    fn the_release_summary_counts_the_items_and_lists_the_labels_that_fit() {
        // why: D9 wording, singular for one item, no empty parentheses.
        // proves: W42-D6
        // catches: "1 items"; labels past the 200-byte cap; "()" when nothing fits.
        let t0 = at("2026-11-13T10:00:00Z");
        let named = |labels: &[&str]| labels.iter().map(|label| HoldItem::new(label.to_string(), None, None, t0)).collect::<Vec<_>>();
        assert_eq!(release_summary(&named(&["build"])), "hold released: 1 item ended (build)");
        assert_eq!(release_summary(&named(&["a", "b"])), "hold released: 2 items ended (a, b)");
        let long = "x".repeat(64);
        let many: Vec<&str> = vec![long.as_str(); 3];
        let summary = release_summary(&named(&many));
        assert_eq!(summary, format!("hold released: 3 items ended ({long}, {long})"), "28 + 2 + 64 + 2 + 64 + 1 = 161 bytes; a third label would make 227");
        let huge = "y".repeat(180);
        assert_eq!(release_summary(&named(&[huge.as_str()])), "hold released: 1 item ended", "nothing fits: no parentheses");
    }
}
