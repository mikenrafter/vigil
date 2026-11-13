//! Engine-level tests for hold items (W42; spec:
//! knowledge/research/agentic-practice-2026-11/vigil-hold-items-spec.md).
//! Cross-cutting, like `hold_tests.rs` whose helpers they reuse: each test
//! drives `Vigil::hold` / `hold_add` / `hold_done` / `keepalive` on paused
//! tokio time and reads the envelope as the agent sees it.
//!
//! Claim ids are `W42-H1`..`W42-H5` (the hold-items spec's own H1-H5), to keep
//! them apart from SPEC "Holds" CLAIM(H1)-CLAIM(H12).
//!
//! Choices where the spec is silent (red phase; green changes one only by
//! changing the test and saying why):
//!   - An item's label defaults to its work spec (`qex:b`).
//!   - An initial `--for` input given with no `--label` stays a plain input:
//!     it ends silently, exactly as before (old holds unchanged). Items that
//!     carry a label, or were added with `hold add`, raise `item ended` events.
//!   - The item whose ending also releases the hold raises only the release
//!     event (`hold released: ...`), not an `item ended` event as well.
//!   - `item ended` events have `kind: "attention"`; `why` is the item's
//!     reading (`completed`, `reached`), or `done` for `hold done`.
//!   - An item's `expect_s` clock starts when the item is added.
//!   - `--for none` needs a label (it could never be ended otherwise).

use std::time::Duration;

use serde_json::Value;

use crate::hold::{HoldItemRequest, HoldRequest};
use crate::hold_tests::{call, qex, request_for, rig, watch_file};
use crate::keepalive::KeepaliveRequest;
use crate::testing::at;
use crate::watch::{Input, WatchId};
use crate::Vigil;

fn wid(id: &str) -> WatchId {
    WatchId::parse(id).unwrap()
}

fn item(work: &str, expect_s: Option<u64>, label: Option<&str>) -> HoldItemRequest {
    HoldItemRequest { work: work.into(), expect_s, label: label.map(String::from) }
}

/// `hold_add` that must succeed, failing by assertion (not by unwrap) so a
/// missing feature reads as a failed claim.
async fn add_ok(vigil: &Vigil, id: &str, request: HoldItemRequest) {
    let result = vigil.hold_add(&wid(id), request.clone()).await;
    assert!(result.is_ok(), "hold_add({id}, {request:?}) should succeed: {result:?}");
}

/// A one-item front-door request: `vigil hold --for WORK --label LABEL --expect-s N`.
fn first_item(work: &str, label: &str, expect_s: Option<u64>) -> HoldRequest {
    HoldRequest { label: Some(label.into()), expect_s, ..request_for(&[work]) }
}

fn summaries(envelope: &Value) -> Vec<String> {
    envelope["events"].as_array().unwrap().iter().map(|event| event["summary"].as_str().unwrap_or_default().to_string()).collect()
}

#[tokio::test(start_paused = true)]
async fn hold_add_puts_an_item_into_a_live_hold_and_the_hold_then_waits_for_it() {
    // why: W42-H2. An item added to a live hold must count: with `a` done and
    //      the added `b` still running the hold stays live; it releases only
    //      when `b` ends too.
    // proves: W42-H2
    // catches: a `hold_add` that returns Ok but does not store the item (the
    //      hold would release as soon as `a` ends), and one that stores it
    //      but leaves the release rule on the original inputs.
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.reader.set_text("qex:b", "running");
    env.vigil.hold(first_item("qex:a", "a", None)).await.unwrap();
    add_ok(&env.vigil, "h", item("qex:b", None, Some("b"))).await;

    env.reader.set_text("qex:a", "completed");
    let after_a = call(&env.vigil, "h", Some(30)).await;
    assert_eq!(after_a["status"], "pending", "a ended, the added b still runs: {after_a}");
    env.reader.set_text("qex:b", "completed");
    let released = call(&env.vigil, "h", Some(60)).await;
    assert_eq!(released["status"], "fired", "{released}");
}

#[tokio::test(start_paused = true)]
async fn hold_add_to_an_ended_hold_fails_says_so_and_changes_nothing() {
    // why: W42-H2. "Adding to an ended hold fails and says so." A released hold
    //      has nothing left to keep warm; silently accepting the item would
    //      park work on a watch nobody calls keepalive on. The message names
    //      the hold and says it has ended; the stored watch is untouched. A
    //      cancelled hold is refused too.
    // proves: W42-H2
    // catches: an add that appends to any watch file whatever its status, an
    //      add that fails with a message that does not say the hold ended,
    //      and one that reopens the hold.
    let env = rig();
    env.reader.set_text("qex:a", "completed");
    env.vigil.hold(first_item("qex:a", "a", None)).await.unwrap();
    assert_eq!(call(&env.vigil, "h", Some(30)).await["status"], "fired", "setup: the hold is released");

    let error = env.vigil.hold_add(&wid("h"), item("qex:b", None, Some("b"))).await.expect_err("adding to a released hold must fail").to_string();
    assert!(error.contains('h') && error.contains("ended"), "the message names the hold and says it ended: {error}");
    assert_eq!(watch_file(&env.vigil, "h").spec.inputs, vec![qex("a")], "a refused add writes nothing");

    env.reader.set_text("qex:c", "running");
    let mut cancelled = first_item("qex:c", "c", None);
    cancelled.id = Some("gone".into());
    env.vigil.hold(cancelled).await.unwrap();
    env.vigil.cancel(&wid("gone")).await.unwrap();
    let refused = env.vigil.hold_add(&wid("gone"), item("qex:b", None, Some("b"))).await;
    assert!(refused.is_err(), "a cancelled hold is ended too: {refused:?}");
    assert_eq!(watch_file(&env.vigil, "gone").spec.inputs, vec![qex("c")]);
}

#[tokio::test(start_paused = true)]
async fn an_item_ending_raises_one_attention_event_and_the_hold_stays_live_until_the_last_item_ends() {
    // why: W42-H3. Each item ends on its own: one `item ended: <label>: <why>`
    //      attention event per item, delivered once, hold still pending. An
    //      item added without a label is named by its spec. The last item's
    //      end is the release (one `hold released` event, no extra item event).
    // proves: W42-H3
    // catches: releasing the hold when the first item ends; an item end that
    //      is silent (the agent never learns `a` finished); an item event
    //      repeated on every call; a label that is lost (event names the
    //      wrong thing); a release that also emits a second item event.
    let env = rig();
    for job in ["a", "b", "c"] {
        env.reader.set_text(&format!("qex:{job}"), "running");
    }
    env.vigil.hold(first_item("qex:a", "a", None)).await.unwrap();
    add_ok(&env.vigil, "h", item("qex:b", None, None)).await;
    add_ok(&env.vigil, "h", item("qex:c", None, Some("c"))).await;

    env.reader.set_text("qex:a", "completed");
    let first = call(&env.vigil, "h", Some(30)).await;
    assert_eq!((first["status"].as_str(), first["next"].as_str()), (Some("pending"), Some("act on events")), "{first}");
    assert_eq!(summaries(&first), vec!["item ended: a: completed"], "{first}");
    assert_eq!(first["events"][0]["kind"], "attention", "{first}");

    let quiet = call(&env.vigil, "h", Some(30)).await;
    assert_eq!((quiet["status"].as_str(), quiet["next"].as_str(), summaries(&quiet).len()), (Some("pending"), Some("call keepalive again"), 0), "a delivered item event is not repeated: {quiet}");

    env.reader.set_text("qex:b", "failed");
    let second = call(&env.vigil, "h", Some(30)).await;
    assert_eq!(summaries(&second), vec!["item ended: qex:b: failed"], "an unlabelled added item is named by its spec: {second}");
    assert_eq!(second["status"], "pending", "{second}");

    env.reader.set_text("qex:c", "completed");
    let released = call(&env.vigil, "h", Some(60)).await;
    assert_eq!(released["status"], "fired", "{released}");
    let events = summaries(&released);
    assert_eq!(events.len(), 1, "the last item's end is the release, nothing more: {released}");
    assert!(events[0].starts_with("hold released:") && events[0].contains("qex:c completed"), "{released}");
    assert_eq!(released["events"][0]["input"], "hold");
}

#[tokio::test(start_paused = true)]
async fn a_bare_hold_never_releases_but_one_that_got_items_releases_when_they_have_all_ended() {
    // why: W42-H3. "The hold itself releases when every item has ended AND the
    //      hold is not bare; a bare hold (no items ever) lives until
    //      cancelled." "All items ended" is vacuously true for zero items, so
    //      the bare case needs its own control; and once an item has been
    //      added the hold is no longer bare.
    // proves: W42-H3, W42-H5
    // catches: a release rule that is `items.all(ended)` without the bare
    //      guard (the bare hold fires at once); a bare guard that stays on
    //      after an item was added (the hold never releases).
    let env = rig();
    env.vigil.hold(request_for(&[])).await.unwrap();
    let bare = call(&env.vigil, "h", Some(30)).await;
    assert_eq!((bare["status"].as_str(), bare["next"].as_str(), summaries(&bare).len()), (Some("pending"), Some("call keepalive again"), 0), "control: a bare hold stays live: {bare}");

    add_ok(&env.vigil, "h", item("none", None, Some("x"))).await;
    let live = call(&env.vigil, "h", Some(30)).await;
    assert_eq!(live["status"], "pending", "the item has not ended: {live}");
    let done = env.vigil.hold_done(&wid("h"), "x").await;
    assert!(done.is_ok(), "{done:?}");
    let released = call(&env.vigil, "h", Some(60)).await;
    assert_eq!(released["status"], "fired", "all items ended on a hold that is no longer bare: {released}");
}

#[tokio::test(start_paused = true)]
async fn each_item_has_its_own_overdue_clock_that_names_the_label_and_fires_once() {
    // why: W42-H3 + W42-H1. `slow` (expect 100 s, from `vigil hold --expect-s
    //      100 --label slow`) is overdue at min(1.5 x 100, 100 + 540) = 150 s;
    //      `fast` (expect 1000 s, via hold_add) at min(1500, 1540) = 1500 s.
    //      The event is an attention/overdue one whose summary names the label
    //      of THAT item only, raised once; `fast` does not borrow `slow`'s
    //      clock and `slow` is not raised again when `fast` goes overdue.
    // proves: W42-H3, W42-H1
    // catches: one hold-wide overdue (summary names no label, or both);
    //      overdue re-raised on every tick; the later item's clock ignored
    //      (only the watch-level expect_s is read); the first item's
    //      `--expect-s` dropped by the front door.
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.reader.set_text("qex:b", "running");
    env.vigil.hold(first_item("qex:a", "slow", Some(100))).await.unwrap();
    add_ok(&env.vigil, "h", item("qex:b", Some(1000), Some("fast"))).await;

    let first = call(&env.vigil, "h", Some(200)).await;
    assert_eq!((first["status"].as_str(), first["next"].as_str(), first["waited_s"].as_u64()), (Some("pending"), Some("act on events"), Some(150)), "{first}");
    assert_eq!(first["events"].as_array().unwrap().len(), 1, "{first}");
    assert_eq!((first["events"][0]["kind"].as_str(), first["events"][0]["reason"].as_str()), (Some("attention"), Some("overdue")), "{first}");
    let summary = first["events"][0]["summary"].as_str().unwrap();
    assert!(summary.contains("slow") && !summary.contains("fast"), "names the overdue item only: {summary}");

    let again = call(&env.vigil, "h", Some(200)).await;
    assert_eq!((again["status"].as_str(), summaries(&again).len()), (Some("pending"), 0), "overdue is raised once, and fast is not due at 350 s: {again}");

    tokio::time::advance(Duration::from_secs(1400)).await;
    let later = call(&env.vigil, "h", Some(30)).await;
    let later_events = summaries(&later);
    assert_eq!(later_events.len(), 1, "only fast is newly overdue: {later}");
    assert!(later_events[0].contains("fast") && !later_events[0].contains("slow"), "{later}");
}

#[tokio::test(start_paused = true)]
async fn an_added_items_overdue_clock_starts_when_it_is_added() {
    // why: W42-H3. Work added to a hold 1000 s after the hold began, with
    //      expect_s 100, is overdue 150 s after the add, not at once.
    // proves: W42-H3
    // catches: overdue measured from the hold's created_at (the added item
    //      would be overdue on the first call).
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.reader.set_text("qex:b", "running");
    env.vigil.hold(first_item("qex:a", "a", None)).await.unwrap();
    tokio::time::advance(Duration::from_secs(1000)).await;
    add_ok(&env.vigil, "h", item("qex:b", Some(100), Some("late"))).await;

    let early = call(&env.vigil, "h", Some(120)).await;
    assert_eq!((early["status"].as_str(), summaries(&early).len()), (Some("pending"), 0), "120 s after the add is before 150 s: {early}");
    let due = call(&env.vigil, "h", Some(60)).await;
    let events = summaries(&due);
    assert_eq!(events.len(), 1, "{due}");
    assert!(events[0].contains("late"), "{due}");
}

#[tokio::test(start_paused = true)]
async fn an_item_that_ended_is_never_reported_overdue() {
    // why: W42-H3. `a` (expect 100 s) ends at once; 200 s later only its
    //      `item ended` event was ever raised, not an overdue one. `b` runs on
    //      and has no expectation.
    // proves: W42-H3
    // catches: an overdue check that does not skip ended items.
    let env = rig();
    env.reader.set_text("qex:a", "completed");
    env.reader.set_text("qex:b", "running");
    env.vigil.hold(first_item("qex:a", "a", Some(100))).await.unwrap();
    add_ok(&env.vigil, "h", item("qex:b", None, Some("b"))).await;

    let ended = call(&env.vigil, "h", Some(30)).await;
    assert_eq!(summaries(&ended), vec!["item ended: a: completed"], "{ended}");
    let later = call(&env.vigil, "h", Some(200)).await;
    assert_eq!((later["status"].as_str(), summaries(&later).len()), (Some("pending"), 0), "a is over; nothing is overdue: {later}");
}

#[tokio::test(start_paused = true)]
async fn one_keepalive_on_the_hold_id_covers_every_item_even_from_another_process() {
    // why: W42-H4. All items live in ONE watch: `list` shows one hold, and one
    //      keepalive on its id (the orchestrator's call) senses the item added
    //      last. Items added by one process are seen by a keepalive from
    //      another (they are stored in the watch file).
    // proves: W42-H4
    // catches: one watch per item (list shows three); a keepalive that only
    //      reads the original input; items kept in memory of the adding
    //      process.
    let env = rig();
    for job in ["a", "b", "c"] {
        env.reader.set_text(&format!("qex:{job}"), "running");
    }
    env.vigil.hold(first_item("qex:a", "a", None)).await.unwrap();
    add_ok(&env.vigil, "h", item("qex:b", None, Some("b"))).await;
    add_ok(&env.vigil, "h", item("qex:c", None, Some("c"))).await;
    let watches = env.vigil.list().await.unwrap();
    assert_eq!(watches.len(), 1, "one watch for a hold with three items: {watches:?}");

    env.reader.set_text("qex:c", "completed");
    let other = env.another_process();
    let request = KeepaliveRequest { watch_id: Some(wid("h")), max_s: Some(30), orchestrator: Some(true) };
    let envelope = serde_json::to_value(other.keepalive(request).await.unwrap()).unwrap();
    assert_eq!(summaries(&envelope), vec!["item ended: c: completed"], "{envelope}");
}

#[tokio::test(start_paused = true)]
async fn a_none_item_is_ended_by_hold_done_and_a_wrong_label_is_refused() {
    // why: W42-H5. A T3 delegate cannot be sensed, so a bare labelled item
    //      (`--for none`) is ended by `vigil hold done <id> <label>`; the end
    //      raises `item ended: t3: done`; the hold stays live for the other
    //      item. A label that names no item is an error naming the label,
    //      not a silent success (the agent would think the delegate was
    //      released).
    // proves: W42-H5
    // catches: `done` that is a no-op or ends nothing; `done` that ends the
    //      whole hold; `done` with an unknown label returning Ok; a none item
    //      that ends by itself.
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.vigil.hold(first_item("qex:a", "build", None)).await.unwrap();
    add_ok(&env.vigil, "h", item("none", Some(600), Some("t3"))).await;
    let quiet = call(&env.vigil, "h", Some(30)).await;
    assert_eq!((quiet["status"].as_str(), summaries(&quiet).len()), (Some("pending"), 0), "a none item never ends by itself: {quiet}");

    let unknown = env.vigil.hold_done(&wid("h"), "nope").await.expect_err("an unknown label is an error").to_string();
    assert!(unknown.contains("nope"), "the error names the label: {unknown}");
    let done = env.vigil.hold_done(&wid("h"), "t3").await;
    assert!(done.is_ok(), "{done:?}");
    let ended = call(&env.vigil, "h", Some(30)).await;
    assert_eq!((ended["status"].as_str(), ended["next"].as_str()), (Some("pending"), Some("act on events")), "{ended}");
    assert_eq!(summaries(&ended), vec!["item ended: t3: done"], "{ended}");

    env.reader.set_text("qex:a", "completed");
    assert_eq!(call(&env.vigil, "h", Some(60)).await["status"], "fired");
}

#[tokio::test(start_paused = true)]
async fn a_none_item_with_expect_s_goes_overdue_naming_its_label() {
    // why: W42-H5. The reason for a bare labelled item is to watch a delegate
    //      that cannot be sensed, by its deadline: `expect_s 100` raises the
    //      overdue event at 150 s naming `t3`.
    // proves: W42-H5, W42-H3
    // catches: a none item that carries no expectation; an overdue summary
    //      without the label.
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.vigil.hold(first_item("qex:a", "build", None)).await.unwrap();
    add_ok(&env.vigil, "h", item("none", Some(100), Some("t3"))).await;
    let envelope = call(&env.vigil, "h", Some(200)).await;
    assert_eq!((envelope["status"].as_str(), envelope["waited_s"].as_u64()), (Some("pending"), Some(150)), "{envelope}");
    assert_eq!(envelope["events"].as_array().unwrap().len(), 1, "{envelope}");
    assert_eq!(envelope["events"][0]["reason"], "overdue", "{envelope}");
    assert!(envelope["events"][0]["summary"].as_str().unwrap().contains("t3"), "{envelope}");
}

#[tokio::test(start_paused = true)]
async fn a_none_item_without_a_label_is_refused_at_both_front_doors() {
    // why: W42-H5 (our choice). Only `hold done <id> <label>` ends a none
    //      item, so one without a label could never end. Refused with the
    //      word `label`, and nothing is written.
    // proves: W42-H5
    // catches: accepting an un-endable item (a hold that lives 7 days).
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.vigil.hold(first_item("qex:a", "build", None)).await.unwrap();
    let added = env.vigil.hold_add(&wid("h"), item("none", Some(100), None)).await.expect_err("a none item needs a label").to_string();
    assert!(added.contains("label"), "{added}");
    assert_eq!(watch_file(&env.vigil, "h").spec.inputs, vec![qex("a")], "nothing written");

    let mut request = request_for(&["none"]);
    request.id = Some("n".into());
    let created = env.vigil.hold(request).await.expect_err("`--for none` without --label").to_string();
    assert!(created.contains("label"), "{created}");
    assert!(!env.vigil.store().watch_path(&wid("n")).exists());
}

#[tokio::test(start_paused = true)]
async fn an_until_item_ends_when_its_time_is_reached() {
    // why: W42-H5. `until:<RFC3339>` is an item kind: it ends at its time with
    //      `item ended: eta: reached` and the hold keeps waiting for the other
    //      item. (The reader is scripted: the clock reading itself is covered
    //      by the SPEC "Holds" tests.)
    // proves: W42-H5
    // catches: `until` items rejected by hold_add, or that never raise their
    //      item event.
    let env = rig();
    env.reader.set_text("qex:a", "running");
    let eta = Input::Until { at: at("2026-11-13T09:00:00Z") }.label();
    env.reader.set_text(&eta, "waiting");
    env.vigil.hold(first_item("qex:a", "build", None)).await.unwrap();
    add_ok(&env.vigil, "h", item("until:2026-11-13T09:00:00Z", None, Some("eta"))).await;
    let waiting = call(&env.vigil, "h", Some(30)).await;
    assert_eq!((waiting["status"].as_str(), summaries(&waiting).len()), (Some("pending"), 0), "{waiting}");
    env.reader.set_text(&eta, "reached");
    let reached = call(&env.vigil, "h", Some(60)).await;
    assert_eq!(summaries(&reached), vec!["item ended: eta: reached"], "{reached}");
    assert_eq!(reached["status"], "pending", "{reached}");
}

#[tokio::test(start_paused = true)]
async fn old_holds_without_labels_behave_exactly_as_before() {
    // why: W42-H1 "today's flags keep working" (control, passes before the
    //      feature). An unlabelled two-`--for` hold stays silent when one
    //      input ends and releases with the unchanged summary; a one-item
    //      hold made with `--label` releases with the single release event
    //      (no item event beside it).
    // proves: W42-H1, W42-H3
    // catches: an implementation that turns every old input into an item that
    //      raises `item ended` (changing every existing hold's envelopes) or
    //      that emits two events on the last item.
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.reader.set_text("qex:b", "running");
    env.vigil.hold(request_for(&["qex:a", "qex:b"])).await.unwrap();
    env.reader.set_text("qex:a", "completed");
    let half = call(&env.vigil, "h", Some(30)).await;
    assert_eq!((half["status"].as_str(), half["next"].as_str(), summaries(&half).len()), (Some("pending"), Some("call keepalive again"), 0), "{half}");
    env.reader.set_text("qex:b", "completed");
    let released = call(&env.vigil, "h", Some(60)).await;
    assert_eq!(summaries(&released), vec!["hold released: qex:a completed; qex:b completed"], "{released}");
    assert!(released["events"][0].get("kind").is_none(), "{released}");

    env.reader.set_text("qex:c", "completed");
    let mut single = first_item("qex:c", "only", None);
    single.id = Some("single".into());
    env.vigil.hold(single).await.unwrap();
    let once = call(&env.vigil, "single", Some(30)).await;
    assert_eq!((once["status"].as_str(), summaries(&once)), (Some("fired"), vec!["hold released: qex:c completed".to_string()]), "{once}");
}
