//! Engine-level tests for hold items (W42; spec:
//! knowledge/research/agentic-practice-2026-11/vigil-hold-items-spec.md).
//! Cross-cutting, like `hold_tests.rs` whose helpers they reuse: each test
//! drives `Vigil::hold` / `hold_add` / `hold_done` / `keepalive` on paused
//! tokio time and reads the envelope as the agent sees it.
//!
//! Claim ids are `W42-H1`..`W42-H5` (the hold-items spec's own H1-H5), to keep
//! them apart from SPEC "Holds" CLAIM(H1)-CLAIM(H12).
//!
//! Decisions (orchestrator; "Spec round 1 decisions" D1-D8 at the end of the
//! spec replace two earlier red-phase choices):
//!   - An item's label defaults to its work spec (`qex:b`); labels are unique
//!     per hold (D5).
//!   - EVERY item of a hold created by this version raises `item ended`, with
//!     or without a label, and the item whose end releases the hold raises it
//!     too, beside the release event (D6). A hold stored without an `items`
//!     field (older version) keeps the old silent behaviour.
//!   - `item ended` events have `kind: "attention"`; `why` is the item's
//!     reading (`completed`, `reached`), or `done` for `hold done`.
//!   - An item's `expect_s` clock starts when the item is added; overdue is
//!     min(1.5 x expect_s, expect_s + 540) from there (D2).
//!   - `--for none` needs a label (it could never be ended otherwise).
//! Claim ids are `W42-H1`..`W42-H5` and `W42-D1`..`W42-D8` for the round-1
//! decisions.

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
    HoldItemRequest { work: work.into(), expect_s, label: label.map(String::from), cap: None }
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
    //      one that reopens the hold, and one that refuses but still rewrites
    //      part of the file (a touched `tick`, `items` or `last_tick_at` that
    //      an inputs-only comparison would not see; D-nit: whole-watch compare).
    let env = rig();
    env.reader.set_text("qex:a", "completed");
    env.vigil.hold(first_item("qex:a", "a", None)).await.unwrap();
    assert_eq!(call(&env.vigil, "h", Some(30)).await["status"], "fired", "setup: the hold is released");
    let released_watch = watch_file(&env.vigil, "h");

    let error = env.vigil.hold_add(&wid("h"), item("qex:b", None, Some("b"))).await.expect_err("adding to a released hold must fail").to_string();
    assert!(error.contains('h') && error.contains("ended"), "the message names the hold and says it ended: {error}");
    assert_eq!(watch_file(&env.vigil, "h"), released_watch, "a refused add writes nothing: the whole stored watch is unchanged");

    env.reader.set_text("qex:c", "running");
    let mut cancelled = first_item("qex:c", "c", None);
    cancelled.id = Some("gone".into());
    env.vigil.hold(cancelled).await.unwrap();
    env.vigil.cancel(&wid("gone")).await.unwrap();
    let cancelled_watch = watch_file(&env.vigil, "gone");
    let refused = env.vigil.hold_add(&wid("gone"), item("qex:b", None, Some("b"))).await;
    assert!(refused.is_err(), "a cancelled hold is ended too: {refused:?}");
    assert_eq!(watch_file(&env.vigil, "gone"), cancelled_watch, "the cancelled watch is unchanged too");
}

#[tokio::test(start_paused = true)]
async fn an_item_ending_raises_one_attention_event_and_the_hold_stays_live_until_the_last_item_ends() {
    // why: W42-H3. Each item ends on its own: one `item ended: <label>: <why>`
    //      attention event per item, delivered once, hold still pending. An
    //      item added without a label is named by its spec. D6: the last
    //      item's end raises its own `item ended` event beside the release.
    // proves: W42-H3, W42-D6
    // catches: releasing the hold when the first item ends; an item end that
    //      is silent (the agent never learns `a` finished); an item event
    //      repeated on every call; a label that is lost (event names the
    //      wrong thing); a release that swallows the last item's event (the
    //      red-phase "release only" rule D6 replaced).
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
    // D6: the last item raises `item ended` too, beside the release.
    let events = summaries(&released);
    assert_eq!(events.len(), 2, "the last item's end raises its own event and the release: {released}");
    assert!(events.contains(&"item ended: c: completed".to_string()), "{released}");
    let release = released["events"].as_array().unwrap().iter().find(|event| event["input"] == "hold").expect("a release event");
    assert!(release["summary"].as_str().unwrap().starts_with("hold released:"), "{released}");
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
async fn an_error_reading_never_ends_or_un_ends_an_item() {
    // why: Fable S3. `a` (expect 100 s) ends; a later read fails
    //      (`error: timeout after 10s`, a flaky qex) and then reads
    //      `completed` again. A failed read says nothing about the work: `a`
    //      stays ended through it, so there is no `item overdue: a` (its
    //      overdue is 150 s, the error read is at 200 s) and no second
    //      `item ended: a`. `b` runs on so the hold stays live.
    // proves: S3
    // catches: an error reading that un-ends the item (forgets ended_at and
    //      last_end_event): overdue then fires at 200 s and the next
    //      `completed` raises a second `item ended`.
    let env = rig();
    env.reader.set_text("qex:a", "completed");
    env.reader.set_text("qex:b", "running");
    env.vigil.hold(first_item("qex:a", "a", Some(100))).await.unwrap();
    add_ok(&env.vigil, "h", item("qex:b", None, Some("b"))).await;
    let ended = call(&env.vigil, "h", Some(30)).await;
    assert_eq!(summaries(&ended), vec!["item ended: a: completed"], "setup: {ended}");

    env.reader.set_text("qex:a", "error: timeout after 10s");
    let errored = call(&env.vigil, "h", Some(200)).await;
    assert_eq!((errored["status"].as_str(), summaries(&errored)), (Some("pending"), Vec::<String>::new()), "{errored}");

    env.reader.set_text("qex:a", "completed");
    let again = call(&env.vigil, "h", Some(30)).await;
    assert_eq!((again["status"].as_str(), summaries(&again)), (Some("pending"), Vec::<String>::new()), "no second end event: {again}");
}

#[tokio::test(start_paused = true)]
async fn an_item_that_only_ever_reads_errors_still_goes_overdue_once() {
    // why: Fable S3, control for the test above: an error reading must not
    //      hide a stuck item. `a` (expect 100 s, overdue at 150 s) never
    //      reads anything but an error, and is raised overdue once.
    // proves: S3
    // catches: an error reading that counts as an end (the item never goes
    //      overdue); overdue raised on every tick.
    let env = rig();
    env.reader.set_text("qex:a", "error: timeout after 10s");
    env.vigil.hold(first_item("qex:a", "a", Some(100))).await.unwrap();
    let due = call(&env.vigil, "h", Some(200)).await;
    assert_eq!(summaries(&due), vec!["item overdue: a"], "{due}");
    let after = call(&env.vigil, "h", Some(30)).await;
    assert_eq!(summaries(&after), Vec::<String>::new(), "{after}");
}

#[tokio::test(start_paused = true)]
async fn status_lists_a_holds_items_live_or_ended_and_a_watch_without_items_has_no_key() {
    // why: the agent reads the item count and states from `vigil status
    //      <hold>` (build-loop pre-launch). `a` ended, `b` runs. A plain
    //      watch's status has no `items` key at all.
    // proves: status-items
    // catches: items missing from status; an ended item shown live; the key
    //      present (null or empty) for a watch that has no items.
    let env = rig();
    env.reader.set_text("qex:a", "completed");
    env.reader.set_text("qex:b", "running");
    env.vigil.hold(first_item("qex:a", "a", None)).await.unwrap();
    add_ok(&env.vigil, "h", item("qex:b", None, Some("b"))).await;
    call(&env.vigil, "h", Some(30)).await;
    let status = serde_json::to_value(env.vigil.status(&wid("h")).await.unwrap()).unwrap();
    assert_eq!(status["items"], serde_json::json!([{"label": "a", "state": "ended"}, {"label": "b", "state": "live"}]), "{status}");

    let plain = env.input_path("p.log");
    let id = env.vigil.create(crate::testing::change_request("plain", &plain, 20)).await.unwrap().watch_id;
    let plain_status = serde_json::to_value(env.vigil.status(&id).await.unwrap()).unwrap();
    assert!(plain_status.get("items").is_none(), "{plain_status}");
}

#[tokio::test(start_paused = true)]
async fn the_seventeenth_item_is_refused_with_make_a_new_hold() {
    // why: ended items count toward the cap of 16, so the refusal must say
    //      what to do: make a new hold. Item 1 ended; 15 more fill the hold.
    // proves: D5
    // catches: a refusal that does not tell the agent the way out.
    let env = rig();
    env.reader.set_text("qex:a", "completed");
    env.vigil.hold(first_item("qex:a", "i0", None)).await.unwrap();
    for n in 1..16 {
        env.reader.set_text(&format!("qex:j{n}"), "running");
        add_ok(&env.vigil, "h", item(&format!("qex:j{n}"), None, Some(&format!("i{n}")))).await;
    }
    let refused = env.vigil.hold_add(&wid("h"), item("none", None, Some("i16"))).await.expect_err("the 17th is refused").to_string();
    assert!(refused.contains("make a new hold") && refused.contains("at most 16"), "{refused}");
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
    assert!(unknown.contains("its items are: build, t3"), "and lists the hold's labels: {unknown}");
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


// ---- spec round 1 decisions (D1-D8) ---------------------------------------------

fn event_ids(envelope: &Value) -> Vec<String> {
    envelope["events"].as_array().unwrap().iter().map(|event| event["event_id"].as_str().expect("every event has an event_id").to_string()).collect()
}

fn distinct(ids: &[String]) -> bool {
    let mut sorted = ids.to_vec();
    sorted.sort();
    sorted.dedup();
    sorted.len() == ids.len()
}

/// One keepalive whose delivery the caller never commits (the client went
/// away before the envelope arrived), as in `auto_interval_tests` S6.
async fn call_uncommitted(vigil: &Vigil, id: &str, max_s: u64) -> Value {
    let request = KeepaliveRequest { watch_id: Some(wid(id)), max_s: Some(max_s), orchestrator: None };
    let (envelope, delivery) = vigil.keepalive_uncommitted(request, &crate::keepalive::NoProgress, std::future::pending::<()>()).await.unwrap().expect("not cancelled");
    drop(delivery);
    serde_json::to_value(&envelope).unwrap()
}

fn raw_watch(env: &crate::testing::Env, id: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(env.root().join(format!("watches/{id}.json"))).unwrap()).unwrap()
}

// -- D1 ----------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_retried_qex_item_is_live_again_blocks_the_release_and_raises_a_new_event_when_it_ends_again() {
    // why: W42-D1. Items follow the existing sensed-state rule (SPEC Holds: a
    //      job that fails and is queued for a retry reads live again). `a`
    //      ends (event), is retried (`queued`), then `b` ends: the hold must
    //      stay live because `a` is live NOW. When `a` ends again the hold
    //      releases and `a` raises a NEW `item ended` event (new event_id).
    // proves: W42-D1
    // catches: an item whose ended state is sticky (ended_at never cleared):
    //      the hold releases when `b` ends while the retried `a` still runs;
    //      and one that raises `item ended` only once per item (no second
    //      event for `a`), or reuses the first event_id.
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.reader.set_text("qex:b", "running");
    env.vigil.hold(request_for(&["qex:a", "qex:b"])).await.unwrap();

    env.reader.set_text("qex:a", "completed");
    let first = call(&env.vigil, "h", Some(30)).await;
    assert_eq!((first["status"].as_str(), summaries(&first)), (Some("pending"), vec!["item ended: qex:a: completed".to_string()]), "{first}");
    let first_id = event_ids(&first).remove(0);

    env.reader.set_text("qex:a", "queued");
    let retried = call(&env.vigil, "h", Some(30)).await;
    assert_eq!((retried["status"].as_str(), summaries(&retried).len()), (Some("pending"), 0), "a retry is quiet: {retried}");

    env.reader.set_text("qex:b", "completed");
    let blocked = call(&env.vigil, "h", Some(30)).await;
    assert_eq!(blocked["status"], "pending", "a is live again, so b ending must not release the hold: {blocked}");
    assert_eq!(summaries(&blocked), vec!["item ended: qex:b: completed"], "{blocked}");

    env.reader.set_text("qex:a", "completed");
    let released = call(&env.vigil, "h", Some(60)).await;
    assert_eq!(released["status"], "fired", "both are ended at this tick: {released}");
    let events = summaries(&released);
    assert_eq!(events.iter().filter(|summary| summary.as_str() == "item ended: qex:a: completed").count(), 1, "a ended again, one new event: {released}");
    assert!(!events.iter().any(|summary| summary == "item ended: qex:b: completed"), "b's event is not repeated: {released}");
    let ids = event_ids(&released);
    assert!(distinct(&ids) && !ids.contains(&first_id), "a new event, not the first one again: {ids:?} vs {first_id}");
}

// -- D2 ----------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn an_items_overdue_uses_the_capped_formula_so_expect_s_2000_is_overdue_at_2540_not_3000() {
    // why: W42-D2. The hold formula is min(1.5 x expect_s, expect_s + 540):
    //      for 2000 s the cap wins (2000 + 540 = 2540, the plain 1.5x would be
    //      3000). Hand arithmetic: 1.5 x 2000 = 3000 > 2540. The first call
    //      (window 2500 s) ends before 2540, the next one crosses it.
    // proves: W42-D2
    // catches: an item clock that uses a bare 1.5x (the event would come at
    //      3000 s, not inside the second window); one that fires inside the
    //      first window (grace dropped).
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.vigil.hold(first_item("qex:a", "big", Some(2000))).await.unwrap();

    let before = call(&env.vigil, "h", Some(2500)).await;
    assert_eq!((before["status"].as_str(), summaries(&before).len()), (Some("pending"), 0), "2500 s is before 2540 s: {before}");
    let after = call(&env.vigil, "h", Some(100)).await;
    assert_eq!((after["events"][0]["kind"].as_str(), after["events"][0]["reason"].as_str()), (Some("attention"), Some("overdue")), "2600 s is past 2540 s: {after}");
    assert!(after["events"][0]["summary"].as_str().unwrap().contains("big"), "{after}");
    // S13: exactly at 2540 s of the hold's life, not merely inside the second
    //      window: the second call starts at 2500 s, so it waits 40 s.
    // catches: an overdue moment that is not a wake of its own (found only at
    //      the next 30 s tick, 2550 s), or a formula off by a few seconds.
    assert_eq!((after["elapsed_s"].as_u64(), after["waited_s"].as_u64()), (Some(2540), Some(40)), "{after}");
}

// -- D3 ----------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn two_items_ending_in_one_tick_give_two_events_with_distinct_event_ids() {
    // why: W42-D3. Every item event has its own event_id. `a` and `b` are
    //      both completed when the tick runs (`c` still runs, so the hold
    //      stays live): the envelope carries two events, one per item, and
    //      their ids differ (ids are the acknowledgement keys; a clash would
    //      lose one of them).
    // proves: W42-D3
    // catches: one merged event naming both items; two events sharing an
    //      event_id (watch:tick:index with the index not advancing); an item
    //      loop that stops after the first ended item.
    let env = rig();
    for job in ["a", "b", "c"] {
        env.reader.set_text(&format!("qex:{job}"), "running");
    }
    env.vigil.hold(first_item("qex:a", "a", None)).await.unwrap();
    add_ok(&env.vigil, "h", item("qex:b", None, Some("b"))).await;
    add_ok(&env.vigil, "h", item("qex:c", None, Some("c"))).await;
    env.reader.set_text("qex:a", "completed");
    env.reader.set_text("qex:b", "completed");
    let envelope = call(&env.vigil, "h", Some(30)).await;
    assert_eq!(envelope["status"], "pending", "{envelope}");
    let mut events = summaries(&envelope);
    events.sort();
    assert_eq!(events, vec!["item ended: a: completed", "item ended: b: completed"], "{envelope}");
    assert!(distinct(&event_ids(&envelope)), "distinct event ids: {envelope}");
}

#[tokio::test(start_paused = true)]
async fn two_items_going_overdue_in_one_tick_give_two_events_with_distinct_event_ids() {
    // why: W42-D3 for overdue. Both items were added at the same instant with
    //      expect_s 100, so both are overdue at the same tick (150 s). Reason
    //      text is `item overdue: <label>`; two events, two ids.
    // proves: W42-D3
    // catches: a single hold-wide overdue flag (one event only); a flag per
    //      reason instead of per item (the second item is swallowed); equal
    //      event ids.
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.reader.set_text("qex:b", "running");
    env.vigil.hold(first_item("qex:a", "one", Some(100))).await.unwrap();
    add_ok(&env.vigil, "h", item("qex:b", Some(100), Some("two"))).await;
    let envelope = call(&env.vigil, "h", Some(200)).await;
    let mut events = summaries(&envelope);
    events.sort();
    assert_eq!(events, vec!["item overdue: one", "item overdue: two"], "{envelope}");
    assert!(envelope["events"].as_array().unwrap().iter().all(|event| event["reason"] == "overdue" && event["kind"] == "attention"), "{envelope}");
    assert!(distinct(&event_ids(&envelope)), "distinct event ids: {envelope}");
}

#[tokio::test(start_paused = true)]
async fn a_restart_before_and_after_acknowledgement_never_repeats_an_overdue_event() {
    // why: W42-D3. The overdue flag is persisted per item. Process 1 raises
    //      the event but its delivery is never committed: process 2 gets the
    //      SAME event again (same event_id, redelivered: true). That call is
    //      committed; process 3 (another restart) gets nothing: the flag
    //      survived, so the item is not raised overdue a second time.
    // proves: W42-D3
    // catches: an overdue flag kept only in memory (process 3 raises a new
    //      overdue); an event not stored in the outbox (process 2 gets
    //      nothing, the agent never learns); a redelivery with a new id.
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.reader.set_text("qex:b", "running");
    env.vigil.hold(first_item("qex:a", "slow", Some(100))).await.unwrap();
    add_ok(&env.vigil, "h", item("qex:b", None, Some("b"))).await;

    let first = call_uncommitted(&env.vigil, "h", 200).await;
    assert_eq!(summaries(&first), vec!["item overdue: slow"], "{first}");
    let first_id = event_ids(&first).remove(0);

    let second = call(&env.another_process(), "h", Some(30)).await;
    assert_eq!((event_ids(&second), second["events"][0]["redelivered"].as_bool()), (vec![first_id], Some(true)), "before acknowledgement: the same event again: {second}");

    let third = call(&env.another_process(), "h", Some(200)).await;
    assert_eq!((third["status"].as_str(), summaries(&third).len()), (Some("pending"), 0), "after acknowledgement: not raised again: {third}");
}

#[tokio::test(start_paused = true)]
async fn a_restart_before_and_after_acknowledgement_never_repeats_an_item_ended_event() {
    // why: W42-D3. Same shape for `item ended`: ended_at and last_end_event
    //      are persisted per item. An item that stays ended is not announced
    //      again by a later process.
    // proves: W42-D3
    // catches: per-item ended state kept in memory (a restarted process
    //      announces `a` again at its first tick); an unpersisted outbox.
    let env = rig();
    env.reader.set_text("qex:a", "completed");
    env.reader.set_text("qex:b", "running");
    env.vigil.hold(first_item("qex:a", "a", None)).await.unwrap();
    add_ok(&env.vigil, "h", item("qex:b", None, Some("b"))).await;

    let first = call_uncommitted(&env.vigil, "h", 30).await;
    assert_eq!(summaries(&first), vec!["item ended: a: completed"], "{first}");
    let first_id = event_ids(&first).remove(0);
    let second = call(&env.another_process(), "h", Some(30)).await;
    assert_eq!((event_ids(&second), second["events"][0]["redelivered"].as_bool()), (vec![first_id], Some(true)), "{second}");
    let third = call(&env.another_process(), "h", Some(60)).await;
    assert_eq!((third["status"].as_str(), summaries(&third).len()), (Some("pending"), 0), "{third}");
}

#[tokio::test(start_paused = true)]
async fn an_items_overdue_clock_survives_a_restart() {
    // why: W42-D3 "restart keeps overdue clocks". `slow` (expect 100 s,
    //      overdue at 150 s) is watched for 100 s by process 1, then by a new
    //      process for another 100 s: the event must come inside the second
    //      window (150 s < 200 s). Restarting the clock at the new process
    //      would put the deadline at 100 + 150 = 250 s.
    // proves: W42-D3
    // catches: added_at not persisted (re-stamped by the process that loads
    //      the hold), which pushes the overdue past the second window.
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.vigil.hold(first_item("qex:a", "slow", Some(100))).await.unwrap();
    let early = call(&env.vigil, "h", Some(100)).await;
    assert_eq!((early["status"].as_str(), summaries(&early).len()), (Some("pending"), 0), "{early}");
    let later = call(&env.another_process(), "h", Some(100)).await;
    assert_eq!(summaries(&later), vec!["item overdue: slow"], "{later}");
}

// -- D4 ----------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn an_item_added_before_the_releasing_tick_keeps_the_hold_alive() {
    // why: W42-D4 add-before-release. `a` has already ended when `b` is added
    //      and nothing has ticked yet: the next tick must see two items, so
    //      the hold stays pending with only `a`'s item event. (The opposite
    //      order, release-before-add, is the ended-hold refusal above and the
    //      lock test in cli_e2e.)
    // proves: W42-D4
    // catches: an add that is applied to a stale copy and lost, or a hold
    //      whose release was already decided from the pre-add items.
    let env = rig();
    env.reader.set_text("qex:a", "completed");
    env.reader.set_text("qex:b", "running");
    env.vigil.hold(first_item("qex:a", "a", None)).await.unwrap();
    add_ok(&env.vigil, "h", item("qex:b", None, Some("b"))).await;
    let envelope = call(&env.vigil, "h", Some(30)).await;
    assert_eq!((envelope["status"].as_str(), summaries(&envelope)), (Some("pending"), vec!["item ended: a: completed".to_string()]), "{envelope}");
}

#[tokio::test(start_paused = true)]
async fn a_running_keepalive_picks_up_an_item_added_by_another_process_at_the_next_tick() {
    // why: W42-D4. One keepalive call (window 300 s) is running; at 45 s a
    //      different process adds `b` (running, expect 40 s). The call
    //      reloads the hold every tick, so `b`'s overdue deadline (45 + 60 =
    //      105 s) is seen at the next tick after it, inside the same call.
    // proves: W42-D4, W42-H4
    // catches: a keepalive that snapshots the hold when the call starts (the
    //      call runs to its 300 s end with no event); a deadline for the
    //      added item measured from the call start.
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.reader.set_text("qex:b", "running");
    env.vigil.hold(first_item("qex:a", "a", None)).await.unwrap();
    let adder = env.another_process();
    let add = async move {
        tokio::time::sleep(Duration::from_secs(45)).await;
        add_ok(&adder, "h", item("qex:b", Some(40), Some("b"))).await;
    };
    let (envelope, ()) = tokio::join!(call(&env.vigil, "h", Some(300)), add);
    assert_eq!(summaries(&envelope), vec!["item overdue: b"], "{envelope}");
    let waited = envelope["waited_s"].as_u64().unwrap();
    assert!((105..=140).contains(&waited), "overdue 60 s after the add at 45 s, at the next tick: waited {waited}: {envelope}");
}

// -- D5 ----------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_duplicate_label_is_refused_naming_it_and_an_unlabelled_item_is_labelled_by_its_spec() {
    // why: W42-D5. Labels are unique per hold; an unlabelled item's label is
    //      its work spec, so that counts too: adding `qex:a` unlabelled to a
    //      hold that already has an unlabelled `qex:a`, or labelling an item
    //      `qex:a` there, is a duplicate. The error names the label; the
    //      whole stored watch is unchanged.
    // proves: W42-D5
    // catches: uniqueness checked on explicit labels only (the spec-labelled
    //      clash passes); a refusal after the item was already written;
    //      `done` that would end the wrong one of two same-named items.
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.reader.set_text("qex:b", "running");
    env.vigil.hold(first_item("qex:b", "build", None)).await.unwrap();
    add_ok(&env.vigil, "h", item("qex:a", None, None)).await;
    let stored = watch_file(&env.vigil, "h");
    for (what, request, label) in [
        ("same explicit label", item("qex:b", None, Some("build")), "build"),
        ("unlabelled spec equal to an unlabelled item", item("qex:a", None, None), "qex:a"),
        ("explicit label equal to another item's spec label", item("qex:b", None, Some("qex:a")), "qex:a"),
    ] {
        let error = env.vigil.hold_add(&wid("h"), request).await.expect_err(what).to_string();
        assert!(error.contains(label), "{what}: the error names the label {label:?}: {error}");
        assert_eq!(watch_file(&env.vigil, "h"), stored, "{what}: nothing written");
    }
}

#[tokio::test(start_paused = true)]
async fn several_for_make_several_items_expect_s_applies_to_each_and_a_label_with_several_for_is_refused() {
    // why: W42-D5. `vigil hold --for A --for B --expect-s 100` is two items,
    //      each with expect_s 100 (both overdue at 150 s, each named by its
    //      spec). `--label` with more than one `--for` is refused (which item
    //      would it name?), writing nothing.
    // proves: W42-D5, W42-H1
    // catches: expect_s applied to the first item only (one overdue event);
    //      the label silently given to the first/every item; the hold
    //      created before the refusal.
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.reader.set_text("qex:b", "running");
    let refused = HoldRequest { label: Some("both".into()), ..request_for(&["qex:a", "qex:b"]) };
    let error = env.vigil.hold(refused).await.expect_err("a label with two --for").to_string();
    assert!(error.contains("label"), "{error}");
    assert!(!env.vigil.store().watch_path(&wid("h")).exists(), "nothing written");

    env.vigil.hold(HoldRequest { expect_s: Some(100), ..request_for(&["qex:a", "qex:b"]) }).await.unwrap();
    let envelope = call(&env.vigil, "h", Some(200)).await;
    let mut events = summaries(&envelope);
    events.sort();
    assert_eq!(events, vec!["item overdue: qex:a", "item overdue: qex:b"], "{envelope}");
}

#[tokio::test(start_paused = true)]
async fn hold_done_refuses_a_sensed_item_and_an_item_that_already_ended() {
    // why: W42-D5. `done` is for `none` items only. A sensed item (qex)
    //      cannot be ended by hand: refused, says why, the item stays live.
    //      `done` twice on a none item: the second is refused with "already
    //      ended" and raises no second event.
    // proves: W42-D5
    // catches: `done` that ends any item (a running qex job would be
    //      dropped from the hold); a second `done` that succeeds and raises a
    //      second `item ended` event.
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.vigil.hold(first_item("qex:a", "build", None)).await.unwrap();
    add_ok(&env.vigil, "h", item("none", None, Some("t3"))).await;

    let sensed = env.vigil.hold_done(&wid("h"), "build").await.expect_err("a sensed item cannot be ended by hand").to_string();
    assert!(sensed.contains("build") && (sensed.contains("sensed") || sensed.contains("none")), "the error says why: {sensed}");
    let quiet = call(&env.vigil, "h", Some(30)).await;
    assert_eq!((quiet["status"].as_str(), summaries(&quiet).len()), (Some("pending"), 0), "the sensed item is still live and silent: {quiet}");

    env.vigil.hold_done(&wid("h"), "t3").await.expect("the first done works");
    let again = env.vigil.hold_done(&wid("h"), "t3").await.expect_err("done twice").to_string();
    assert!(again.contains("already ended") && again.contains("t3"), "{again}");
    let ended = call(&env.vigil, "h", Some(30)).await;
    assert_eq!(summaries(&ended), vec!["item ended: t3: done"], "exactly one event for t3: {ended}");
}

#[tokio::test(start_paused = true)]
async fn items_count_toward_the_sixteen_input_cap() {
    // why: W42-D5. "Items count toward the existing input cap" (SPEC Holds:
    //      at most 16 inputs). 15 created + 1 added = 16 is allowed; the 17th
    //      is refused naming 16, writing nothing.
    // proves: W42-D5
    // catches: items stored outside the cap (a hold grows without bound); a
    //      cap off by one in either direction (15 or 17).
    let env = rig();
    let specs: Vec<String> = (0..15).map(|index| format!("file:/vigil-test-absent/{index}")).collect();
    let specs: Vec<&str> = specs.iter().map(String::as_str).collect();
    env.vigil.hold(request_for(&specs)).await.unwrap();
    add_ok(&env.vigil, "h", item("file:/vigil-test-absent/15", None, Some("sixteenth"))).await;
    let full = watch_file(&env.vigil, "h");
    let error = env.vigil.hold_add(&wid("h"), item("file:/vigil-test-absent/16", None, Some("seventeenth"))).await.expect_err("the 17th item").to_string();
    assert!(error.contains("16"), "the error names the cap: {error}");
    assert_eq!(watch_file(&env.vigil, "h"), full, "nothing written");
}

// -- D6 ----------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn every_item_of_a_new_hold_raises_item_ended_unlabelled_ones_and_the_last_one_included() {
    // why: W42-D6 (replaces the red-phase "initial unlabelled inputs are
    //      silent" and "release only" rules). Two unlabelled `--for`: `a`
    //      ending is announced by its spec; `b` ending releases the hold AND
    //      raises its own event; a labelled single item does the same. Event
    //      ids are distinct.
    // proves: W42-D6
    // catches: the old silent initial inputs; the last item swallowed by
    //      the release event; the release event dropped in favour of the
    //      item event.
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.reader.set_text("qex:b", "running");
    env.vigil.hold(request_for(&["qex:a", "qex:b"])).await.unwrap();
    env.reader.set_text("qex:a", "completed");
    let half = call(&env.vigil, "h", Some(30)).await;
    assert_eq!((half["status"].as_str(), summaries(&half)), (Some("pending"), vec!["item ended: qex:a: completed".to_string()]), "{half}");
    env.reader.set_text("qex:b", "completed");
    let released = call(&env.vigil, "h", Some(60)).await;
    assert_eq!(released["status"], "fired", "{released}");
    assert_eq!(released["events"].as_array().unwrap().len(), 2, "{released}");
    assert!(summaries(&released).contains(&"item ended: qex:b: completed".to_string()), "{released}");
    assert!(released["events"].as_array().unwrap().iter().any(|event| event["input"] == "hold" && event["summary"].as_str().unwrap().starts_with("hold released:")), "{released}");
    assert!(distinct(&event_ids(&released)), "{released}");

    env.reader.set_text("qex:c", "completed");
    let mut single = first_item("qex:c", "only", None);
    single.id = Some("single".into());
    env.vigil.hold(single).await.unwrap();
    let once = call(&env.vigil, "single", Some(30)).await;
    assert_eq!((once["status"].as_str(), once["events"].as_array().unwrap().len()), (Some("fired"), 2), "item event plus release: {once}");
    assert!(summaries(&once).contains(&"item ended: only: completed".to_string()), "{once}");
}

#[tokio::test(start_paused = true)]
async fn a_hold_stored_without_the_items_field_keeps_the_old_silent_behaviour() {
    // why: W42-D6. A hold written by an older vigil has no `items` field and
    //      must behave exactly as it did: no item events, release summary by
    //      input. The fixture is made from a real new hold with the `items`
    //      key deleted from its JSON (and a positive control that the new
    //      hold does store `items`, so this is not a vacuous strip).
    // proves: W42-D6
    // catches: treating a missing `items` as "make every input an item now"
    //      (item events appear in old holds); a deserialiser that rejects
    //      the old file; a new hold that stores no `items` at all.
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.reader.set_text("qex:b", "running");
    env.vigil.hold(request_for(&["qex:a", "qex:b"])).await.unwrap();
    let mut raw = raw_watch(&env, "h");
    assert!(raw.get("items").is_some(), "control: a hold created by this version stores `items`: {raw}");
    raw.as_object_mut().unwrap().remove("items");
    std::fs::write(env.root().join("watches/h.json"), serde_json::to_string_pretty(&raw).unwrap()).unwrap();

    env.reader.set_text("qex:a", "completed");
    let half = call(&env.vigil, "h", Some(30)).await;
    assert_eq!((half["status"].as_str(), summaries(&half).len()), (Some("pending"), 0), "{half}");
    env.reader.set_text("qex:b", "completed");
    let released = call(&env.vigil, "h", Some(30)).await;
    assert_eq!(summaries(&released), vec!["hold released: qex:a completed; qex:b completed"], "{released}");
    assert!(released["events"][0].get("kind").is_none(), "{released}");
}

#[tokio::test(start_paused = true)]
async fn the_release_summary_names_the_item_count_first_and_as_many_labels_as_fit_the_cap() {
    // why: W42-D6. "The release summary names the item count and as many
    //      labels as fit its cap" (200 bytes, MAX_SUMMARY_BYTES). Sixteen
    //      none-items with 40-byte labels cannot all fit (16 x 40 = 640 > 200):
    //      the count must not be the part that gets cut, the first label is
    //      there and the last one is not. A 3-item hold names all three.
    //      Wording pinned: `<N> items`.
    // proves: W42-D6
    // catches: the count appended after the labels (truncated away); an
    //      uncapped summary; a summary with no labels.
    let env = rig();
    let labels: Vec<String> = (0..16).map(|index| format!("delegate-{index:02}-{}", "x".repeat(28))).collect();
    assert!(labels.iter().all(|label| label.len() == 40));
    env.vigil.hold(first_item("none", &labels[0], None)).await.unwrap();
    for label in &labels[1..] {
        add_ok(&env.vigil, "h", item("none", None, Some(label))).await;
    }
    for label in &labels {
        let done = env.vigil.hold_done(&wid("h"), label).await;
        assert!(done.is_ok(), "{done:?}");
    }
    let released = call(&env.vigil, "h", Some(30)).await;
    assert_eq!(released["status"], "fired", "{released}");
    let release = released["events"].as_array().unwrap().iter().find(|event| event["input"] == "hold").expect("a release event")["summary"].as_str().unwrap().to_string();
    assert!(release.len() <= 200, "{} bytes: {release}", release.len());
    assert!(release.starts_with("hold released:") && release.contains("16 items"), "{release}");
    assert!(release.contains(&labels[0]) && !release.contains(&labels[15]), "{release}");
    // S12: the labels are a prefix that fits, and every label that fits is
    //      there. "hold released: 16 items ended (" is 31 bytes, each label 40,
    //      each ", " 2, the closing ")" 1: four labels are 31 + 4 x 40 + 3 x 2 +
    //      1 = 198 <= 200, a fifth would be 240. So exactly labels 0-3.
    // catches: a fit check that reserves room it does not use or is off by one
    //      (only three labels shown), or an ellipsis that pushes the fourth out.
    assert!(release.contains(&labels[3]) && !release.contains(&labels[4]), "the fourth label fits, the fifth does not: {release}");
    assert_eq!(release, format!("hold released: 16 items ended ({})", labels[..4].join(", ")), "D9 wording");

    let small = rig();
    small.vigil.hold(first_item("none", "build", None)).await.unwrap();
    add_ok(&small.vigil, "h", item("none", None, Some("tests"))).await;
    add_ok(&small.vigil, "h", item("none", None, Some("deploy"))).await;
    for label in ["build", "tests", "deploy"] {
        let done = small.vigil.hold_done(&wid("h"), label).await;
        assert!(done.is_ok(), "{done:?}");
    }
    let released = call(&small.vigil, "h", Some(30)).await;
    let release = released["events"].as_array().unwrap().iter().find(|event| event["input"] == "hold").expect("a release event")["summary"].as_str().unwrap().to_string();
    assert!(release.contains("3 items") && ["build", "tests", "deploy"].iter().all(|label| release.contains(label)), "{release}");
    assert_eq!(release, "hold released: 3 items ended (build, tests, deploy)", "D9 wording, labels in item order");
}

// -- D8 ----------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn hold_add_resolves_a_qex_name_to_the_full_id_and_refuses_an_ambiguous_name_like_creation() {
    // why: W42-D8 (S8-S10). `hold add --for qex:nightly` stores the job id
    //      the name resolves to (a reused name must not redirect the item),
    //      as `vigil hold` does. The item then follows the full id: scripting
    //      only `qex:<full id>` as completed ends it. An ambiguous name is
    //      refused naming the cause and the watch is unchanged.
    // proves: W42-D8
    // catches: hold_add that skips resolution and stores the name (the item
    //      reads `qex:nightly`, which is never scripted, and never ends);
    //      an ambiguous name accepted; an error without the name.
    let env = rig();
    let full = "0d31c8ac-25e4-4ab2-8a3b-9b24ccd533c9";
    env.reader.set_text("qex:a", "running");
    env.reader.set_text(&format!("qex:{full}"), "running");
    env.reader.set_qex_resolution("nightly", Ok(full.to_string()));
    env.reader.set_qex_resolution("dup", Err(crate::error::CreateError::QexJobAmbiguous { job: "dup".into(), detail: "`dup` names 2 jobs. Give the id".into() }));
    env.vigil.hold(first_item("qex:a", "a", None)).await.unwrap();
    add_ok(&env.vigil, "h", item("qex:nightly", None, Some("nightly"))).await;
    assert!(std::fs::read_to_string(env.root().join("watches/h.json")).unwrap().contains(full), "the full id is stored");

    let before = watch_file(&env.vigil, "h");
    let ambiguous = env.vigil.hold_add(&wid("h"), item("qex:dup", None, Some("d"))).await.expect_err("ambiguous").to_string();
    assert!(ambiguous.contains("dup") && ambiguous.contains("ambiguous"), "{ambiguous}");
    assert_eq!(watch_file(&env.vigil, "h"), before, "nothing written");

    env.reader.set_text(&format!("qex:{full}"), "completed");
    let ended = call(&env.vigil, "h", Some(30)).await;
    assert_eq!(summaries(&ended), vec!["item ended: nightly: completed"], "{ended}");
}

// ---- spec review round 2 leftovers (S4, S11, S12, S13) ---------------------------

/// Run `operation` while this test holds the watch lock of `h`, and let
/// `meddle` change the stored hold (as a concurrent writer that held the lock
/// would) once `operation` has had time to reach the lock. Fails if
/// `operation` finished while the lock was held (it did not wait for it).
async fn while_locked<T>(env: &crate::testing::Env, operation: impl std::future::Future<Output = T>, meddle: impl FnOnce(&mut Value)) -> T {
    let guard = env.vigil.store().lock_watch(&wid("h")).unwrap();
    let finished = std::cell::Cell::new(false);
    let run = async {
        let outcome = operation.await;
        finished.set(true);
        outcome
    };
    let interfere = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!finished.get(), "the operation finished while the watch lock was held: it took no lock");
        let mut raw = raw_watch(env, "h");
        meddle(&mut raw);
        std::fs::write(env.root().join("watches/h.json"), serde_json::to_string_pretty(&raw).unwrap()).unwrap();
        drop(guard);
    };
    let (outcome, ()) = tokio::join!(run, interfere);
    outcome
}

#[tokio::test(start_paused = true)]
async fn a_hold_done_waiting_on_the_lock_keeps_an_item_that_was_added_meanwhile() {
    // why: W42-D4 / S4. `hold done` is a read-modify-write of the hold. One that
    //      loads the hold before it takes the watch lock (or takes none) writes
    //      its stale copy back and silently drops an item another process added
    //      in between: that item's work would never hold the session. Here the
    //      test holds the lock, `hold done t3` must wait for it, and while it
    //      waits a concurrent add lands (`late`, a copy of the none item t3
    //      under another label, as the add would store it). Afterwards BOTH
    //      changes are there: t3 is announced ended, `late` still holds the
    //      session and can itself be ended by `done`.
    // proves: W42-D4
    // catches: hold_done that loads before locking (stale write drops `late`:
    //      the final `done late` is refused as an unknown label, and the hold
    //      releases early); hold_done that takes no lock (finishes while the
    //      lock is held).
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.vigil.hold(first_item("qex:a", "build", None)).await.unwrap();
    add_ok(&env.vigil, "h", item("none", None, Some("t3"))).await;
    let other = env.another_process();
    let done = while_locked(&env, other.hold_done(&wid("h"), "t3"), |raw| {
        let mut late = raw["items"][1].clone();
        late["label"] = "late".into();
        raw["items"].as_array_mut().unwrap().push(late);
    })
    .await;
    assert!(done.is_ok(), "{done:?}");

    let ended = call(&env.vigil, "h", Some(30)).await;
    assert_eq!((ended["status"].as_str(), summaries(&ended)), (Some("pending"), vec!["item ended: t3: done".to_string()]), "{ended}");
    env.reader.set_text("qex:a", "completed");
    // 60 s: the call above returned at once on its item event, so the next tick is 30 s away.
    let still = call(&env.vigil, "h", Some(60)).await;
    assert_eq!((still["status"].as_str(), summaries(&still)), (Some("pending"), vec!["item ended: build: completed".to_string()]), "late (added while done waited) still holds the session: {still}");
    env.vigil.hold_done(&wid("h"), "late").await.expect("the item added meanwhile is still there");
    assert_eq!(call(&env.vigil, "h", Some(60)).await["status"], "fired");
}

#[tokio::test(start_paused = true)]
async fn a_hold_add_waiting_on_the_lock_keeps_a_done_made_meanwhile() {
    // why: W42-D4 / S4, the other order. `hold add` waits for the lock while a
    //      concurrent `hold done t3` lands (t3 gets its `ended_at`, as done
    //      stores it). The add must append to the hold as it is AFTER the done:
    //      t3 is still announced ended, and the added `b` holds the session.
    // proves: W42-D4
    // catches: hold_add that loads before locking (its stale copy un-ends t3:
    //      no `item ended: t3: done` event ever comes); hold_add without a lock.
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.reader.set_text("qex:b", "running");
    env.vigil.hold(first_item("qex:a", "build", None)).await.unwrap();
    add_ok(&env.vigil, "h", item("none", None, Some("t3"))).await;
    let other = env.another_process();
    let added = while_locked(&env, other.hold_add(&wid("h"), item("qex:b", None, Some("b"))), |raw| {
        raw["items"][1]["ended_at"] = raw["created_at"].clone();
    })
    .await;
    assert!(added.is_ok(), "{added:?}");

    let ended = call(&env.vigil, "h", Some(30)).await;
    assert_eq!((ended["status"].as_str(), summaries(&ended)), (Some("pending"), vec!["item ended: t3: done".to_string()]), "the done made while the add waited survived: {ended}");
    env.reader.set_text("qex:a", "completed");
    // 60 s: the call above returned at once on its item event, so the next tick is 30 s away.
    let still = call(&env.vigil, "h", Some(60)).await;
    assert_eq!((still["status"].as_str(), summaries(&still)), (Some("pending"), vec!["item ended: build: completed".to_string()]), "b holds the session: {still}");
    env.reader.set_text("qex:b", "completed");
    assert_eq!(call(&env.vigil, "h", Some(60)).await["status"], "fired");
}

#[tokio::test(start_paused = true)]
async fn hold_add_and_hold_done_on_an_expired_hold_are_refused_and_change_nothing() {
    // why: W42-D4 / S4: "recheck status and expiry under the lock". A hold
    //      whose deadline has passed is ended even if no keepalive has written
    //      `expired` yet (V16): adding work to it, or ending one of its items,
    //      would act on a hold nobody keeps alive. Both refuse, saying it
    //      ended, and the stored file is byte-for-byte what it was. Control:
    //      the same done one second before the deadline works.
    // proves: W42-D4, W42-H2
    // catches: a status check that reads only `status` (still `pending` on
    //      disk) and accepts the add or done; a refusal that writes the expiry
    //      or the item anyway.
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.vigil.hold(HoldRequest { deadline_s: Some(100), ..first_item("qex:a", "build", None) }).await.unwrap();
    add_ok(&env.vigil, "h", item("none", None, Some("t3"))).await;
    add_ok(&env.vigil, "h", item("none", None, Some("t4"))).await;
    tokio::time::advance(Duration::from_secs(99)).await;
    env.vigil.hold_done(&wid("h"), "t4").await.expect("control: before the deadline done works");
    tokio::time::advance(Duration::from_secs(2)).await;

    let before = std::fs::read(env.root().join("watches/h.json")).unwrap();
    let added = env.vigil.hold_add(&wid("h"), item("none", None, Some("late"))).await.expect_err("add to an expired hold").to_string();
    assert!(added.contains("ended"), "{added}");
    let done = env.vigil.hold_done(&wid("h"), "t3").await.expect_err("done on an expired hold").to_string();
    assert!(done.contains("ended"), "{done}");
    assert_eq!(std::fs::read(env.root().join("watches/h.json")).unwrap(), before, "refusals write nothing");
}

#[tokio::test(start_paused = true)]
async fn sixteen_none_items_fill_a_hold_and_a_seventeenth_of_any_kind_is_refused() {
    // why: W42-D5 / S11. The cap counts ITEMS, not sensed inputs: a `none`
    //      item has no input, so a cap on `spec.inputs` alone would let a hold
    //      grow without bound. One none item at creation + 15 added = 16 (the
    //      cap, allowed); the 17th, none or sensed, is refused naming 16 and
    //      the file is byte-for-byte unchanged. Mixed hold: 4 sensed + 12
    //      none = 16, then the same two refusals.
    // proves: W42-D5
    // catches: a cap checked against `spec.inputs.len()` (17 none items pass);
    //      a cap off by one (the 16th refused, or a 17th accepted); a refusal
    //      that still writes.
    let env = rig();
    env.reader.set_text("qex:s", "running");
    env.vigil.hold(first_item("none", "n00", None)).await.unwrap();
    for index in 1..16 {
        add_ok(&env.vigil, "h", item("none", None, Some(&format!("n{index:02}")))).await;
    }
    let full = std::fs::read(env.root().join("watches/h.json")).unwrap();
    for refused in [item("none", None, Some("n16")), item("qex:s", None, Some("s16"))] {
        let error = env.vigil.hold_add(&wid("h"), refused.clone()).await.expect_err("the 17th item").to_string();
        assert!(error.contains("16"), "{refused:?}: the error names the cap: {error}");
        assert_eq!(std::fs::read(env.root().join("watches/h.json")).unwrap(), full, "{refused:?}: nothing written");
    }

    for job in ["a", "b", "c", "d"] {
        env.reader.set_text(&format!("qex:{job}"), "running");
    }
    let mut mixed = request_for(&["qex:a", "qex:b", "qex:c", "qex:d"]);
    mixed.id = Some("m".into());
    env.vigil.hold(mixed).await.unwrap();
    for index in 0..12 {
        add_ok(&env.vigil, "m", item("none", None, Some(&format!("t{index:02}")))).await;
    }
    let full = std::fs::read(env.root().join("watches/m.json")).unwrap();
    for refused in [item("none", None, Some("t12")), item("qex:s", None, Some("s"))] {
        let error = env.vigil.hold_add(&wid("m"), refused.clone()).await.expect_err("the 17th item of a mixed hold").to_string();
        assert!(error.contains("16"), "{refused:?}: {error}");
        assert_eq!(std::fs::read(env.root().join("watches/m.json")).unwrap(), full, "{refused:?}: nothing written");
    }
}


fn w46_request(value: Value) -> HoldRequest {
    serde_json::from_value(value).unwrap()
}

#[tokio::test(start_paused = true)]
async fn w46_expired_owner_cap_falls_back_to_one_on_the_next_admission() {
    // why: W46-K2 restores the default work limit on the next read once the owner's until expires.
    // catches: caching the raised limit forever, using wall time, or continuing to admit under expired settings.
    let env = rig();
    let now = env.clock.now();
    std::fs::create_dir_all(env.vigil.store().root()).unwrap();
    std::fs::write(env.vigil.store().root().join("cap.json"), serde_json::to_vec(&serde_json::json!({
        "work": 2, "review": 1, "until": now.plus_secs(10), "source": "owner"
    })).unwrap()).unwrap();
    env.vigil.hold(w46_request(serde_json::json!({"id": "h", "for": ["none"], "label": "first", "cap": "work"}))).await.unwrap();
    let second: HoldItemRequest = serde_json::from_value(serde_json::json!({"for": "none", "label": "second", "cap": "work"})).unwrap();
    assert!(env.vigil.hold_add(&wid("h"), second).await.is_ok(), "control: the unexpired raise admits two");
    tokio::time::advance(Duration::from_secs(10)).await;
    let before = std::fs::read(env.vigil.store().watch_path(&wid("h"))).unwrap();
    let third = serde_json::from_value(serde_json::json!({"for": "none", "label": "third", "cap": "work"})).unwrap();
    let error = env.vigil.hold_add(&wid("h"), third).await.expect_err("expiry restores limit one").to_string();
    assert!(error.contains("2 working Claude runs (cap 1): first, second"), "{error}");
    assert_eq!(std::fs::read(env.vigil.store().watch_path(&wid("h"))).unwrap(), before);
}

#[tokio::test(start_paused = true)]
async fn w46_keepalive_cap_counts_follow_sensed_ends_and_the_slot_can_be_reused() {
    // why: W46-K4/K5 show live class counts in real keepalive envelopes and free a slot on a sensed end.
    // catches: counting all stored items, attaching cap only to status, or keeping ended slots occupied.
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.vigil.hold(w46_request(serde_json::json!({"id": "h", "for": ["qex:a"], "label": "agent", "cap": "work"}))).await.unwrap();
    add_ok(&env.vigil, "h", item("none", None, Some("session"))).await;
    let live = call(&env.vigil, "h", Some(30)).await;
    assert_eq!(live["cap"], serde_json::json!({"work": 1, "work_limit": 1, "review": 0, "review_limit": 1}));
    env.reader.set_text("qex:a", "completed");
    let ended = call(&env.vigil, "h", Some(30)).await;
    assert_eq!(ended["status"], "pending");
    assert_eq!(ended["cap"], serde_json::json!({"work": 0, "work_limit": 1, "review": 0, "review_limit": 1}));
    let replacement = serde_json::from_value(serde_json::json!({"for": "none", "label": "replacement", "cap": "work"})).unwrap();
    assert!(env.vigil.hold_add(&wid("h"), replacement).await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn w46_old_stored_items_without_cap_load_as_uncounted() {
    // why: W46-K1 preserves old item files as uncounted instead of retrospectively classifying their work.
    // catches: defaulting missing cap to work, or requiring cap when reading old stored items.
    let env = rig();
    env.vigil.hold(first_item("none", "old", None)).await.unwrap();
    let path = env.vigil.store().watch_path(&wid("h"));
    let mut old: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    old["items"][0].as_object_mut().unwrap().remove("cap");
    std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    let classed = serde_json::from_value(serde_json::json!({"for": "none", "label": "classed", "cap": "work"})).unwrap();
    assert!(env.vigil.hold_add(&wid("h"), classed).await.is_ok());
    let status = serde_json::to_value(env.vigil.status(&wid("h")).await.unwrap()).unwrap();
    assert!(status["items"][0].get("cap").is_none());
    assert_eq!(status["cap"]["work"], 1);
}

#[tokio::test(start_paused = true)]
async fn w46_unclassed_hold_envelope_remains_byte_identical() {
    // why: W46-K4 leaves the pre-W46 envelope byte-identical when every item is uncounted.
    // catches: serializing cap:null, cap defaults, or changing the existing envelope field order.
    let env = rig();
    env.vigil.hold(first_item("none", "sol", None)).await.unwrap();
    let watch = watch_file(&env.vigil, "h");
    let bytes = serde_json::to_string(&env.vigil.envelope_for(&watch, watch.created_at).unwrap()).unwrap();
    let expected = r#"{"watch_id":"h","status":"pending","now":"2026-10-29T14:03:11Z","tick":0,"session_ticks":0,"elapsed_s":0,"deadline_in_s":604800,"events":[],"digest":"","judge_status":"skipped","next":"call keepalive again"}"#;
    assert_eq!(bytes, expected);
    let quiet = call(&env.vigil, "h", Some(30)).await;
    assert!(quiet.get("cap").is_none(), "{quiet}");
}

#[tokio::test(start_paused = true)]
async fn w46_bad_cap_json_does_not_break_keepalive_cancel_or_cap_set_but_admission_fails_closed() {
    // why: W46 S1: local data is untrusted, so a hand-edited cap.json must not stop an orchestrator's keepalive loop or a cancel.
    // catches: envelope_for or cap_set propagating the cap.json parse error, admission failing open on a bad file, or the counts silently vanishing (S2, S3).
    let env = rig();
    env.vigil.hold(w46_request(serde_json::json!({"id": "h", "for": ["none"], "label": "first", "cap": "work"}))).await.unwrap();
    env.vigil.hold(w46_request(serde_json::json!({"id": "c", "for": ["none"], "label": "first", "cap": "work"}))).await.unwrap();
    let path = env.vigil.store().root().join("cap.json");
    std::fs::write(&path, r#"{"work":0}"#).unwrap();
    let kept = call(&env.vigil, "h", Some(30)).await;
    assert_eq!(kept["status"], "pending", "{kept}");
    // S2: the counts are left out rather than shown as defaults; S3: the envelope says why.
    assert!(kept.get("cap").is_none(), "{kept}");
    assert!(kept["cap_error"].as_str().is_some_and(|error| error.contains("cap.json") && error.contains("vigil cap reset")), "{kept}");
    assert!(env.vigil.status(&wid("h")).await.is_ok());
    let cancelled = env.vigil.cancel(&wid("c")).await.expect("cancel of a classed hold with a bad cap.json");
    assert_eq!(cancelled.status, crate::WatchStatus::Cancelled);
    let second: HoldItemRequest = serde_json::from_value(serde_json::json!({"for": "none", "label": "second", "cap": "work"})).unwrap();
    let refused = env.vigil.hold_add(&wid("h"), second).await.expect_err("admission fails closed on a bad cap.json").to_string();
    assert!(refused.contains("vigil cap reset"), "{refused}");
    let until = env.clock.now().plus_secs(60);
    let set = env.vigil.cap_set(std::num::NonZeroUsize::new(2).unwrap(), None, until).await.expect("cap set replaces a bad cap.json");
    assert_eq!((set.work.get(), set.review.get()), (2, 1));
}

#[tokio::test(start_paused = true)]
async fn w46_cap_on_a_bare_hold_is_refused_like_expect_s() {
    // why: W46 review: `--cap` classes items, so with no items it would be silently ignored.
    // catches: accepting a cap that has nothing to apply to, or refusing a bare hold that sets no cap.
    let env = rig();
    let error = env.vigil.hold(w46_request(serde_json::json!({"id": "h", "cap": "work"}))).await.expect_err("cap without items").to_string();
    assert!(error.contains("cap applies to each item") && error.contains("vigil hold add"), "{error}");
    assert!(env.vigil.store().load(&wid("h")).is_err(), "nothing was written");
    env.vigil.hold(w46_request(serde_json::json!({"id": "bare"}))).await.expect("control: a bare hold without a cap");
}
