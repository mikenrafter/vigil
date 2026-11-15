//! Optional quiet-overdue behavior, separate from explicit completion notes.
use crate::hold::{HoldRequest, HoldItemRequest};
use crate::hold_tests::{rig, request_for};
use crate::keepalive::KeepaliveRequest;
use crate::watch::WatchId;

fn id() -> WatchId { WatchId::parse("h").unwrap() }
async fn wait(vigil: &crate::Vigil, max_s: u64) -> crate::Envelope {
    vigil.keepalive(KeepaliveRequest { watch_id: Some(id()), max_s: Some(max_s), orchestrator: Some(false), ..Default::default() }).await.unwrap()
}

#[tokio::test(start_paused = true)]
async fn ordinary_overdue_is_a_note_and_twice_expected_time_wakes_once() {
    // why: a mildly late delegate should not spend a turn, but a stuck one must.
    // catches: normal overdue still waking, far overdue never waking or repeating.
    let env = rig();
    env.vigil.hold(HoldRequest { work: vec!["none".into()], label: Some("delegate".into()), expect_s: Some(100), ..request_for(&[]) }).await.unwrap();
    let normal = wait(&env.vigil, 180).await;
    assert_eq!(normal.waited_s, Some(180));
    assert_eq!(normal.events.len(), 1);
    assert_eq!(normal.events[0].summary, "item overdue: delegate");
    assert!(normal.events[0].is_note());
    assert_eq!(normal.next, crate::envelope::Next::CallKeepaliveAgain);
    let far = wait(&env.vigil, 60).await;
    assert_eq!(far.waited_s, Some(20)); // added 0 + 2*100, after previous 180 s
    assert_eq!(far.events.len(), 1);
    assert_eq!(far.events[0].summary, "item far overdue: delegate");
    assert!(far.events[0].is_call_ending());
    assert_eq!(wait(&env.another_process(), 30).await.events.len(), 0);
}

#[tokio::test(start_paused = true)]
async fn note_escalation_and_later_completion_have_distinct_ids_even_between_ticks() {
    // why: 1 s estimates put normal and far overdue between the same 30 s ticks.
    // proves: CLAIM(overdue-event-epoch)
    // catches: reusing the normal event id for escalation or later manual done.
    let env = rig();
    env.vigil.hold(HoldRequest { work: vec!["none".into()], label: Some("a".into()), expect_s: Some(1), ..request_for(&[]) }).await.unwrap();
    env.vigil.hold_add(&id(), HoldItemRequest { work: "none".into(), label: Some("b".into()), expect_s: Some(1), ..Default::default() }).await.unwrap();
    let far = wait(&env.vigil, 30).await;
    assert_eq!(far.waited_s, Some(2));
    assert_eq!(far.events.len(), 4);
    let mut ids = std::collections::BTreeSet::new();
    for event in &far.events { assert!(ids.insert(event.event_id.clone()), "{}", event.event_id); }
    env.vigil.hold_done(&id(), "a").await.unwrap();
    let completed = wait(&env.vigil, 30).await;
    assert_eq!(completed.events.len(), 1);
    assert!(ids.insert(completed.events[0].event_id.clone()));
}

#[tokio::test(start_paused = true)]
async fn a_gone_pid_still_wakes_before_its_estimate() {
    // why: soft overdue must never suppress a genuine autonomous process end.
    // catches: applying note suppression to all process events.
    let env = rig();
    env.reader.set_text("process:4242", "running");
    env.vigil.hold(HoldRequest { work: vec!["pid:4242".into()], label: Some("build".into()), expect_s: Some(100), ..request_for(&[]) }).await.unwrap();
    let finish = async {
        tokio::time::sleep(std::time::Duration::from_secs(40)).await;
        env.reader.set_text("process:4242", "exited");
    };
    let (answer, ()) = tokio::join!(wait(&env.vigil, 90), finish);
    assert_eq!(answer.waited_s, Some(60));
    assert!(answer.events.iter().any(|event| event.is_call_ending() && event.summary.contains("exited")));
}

#[tokio::test(start_paused = true)]
async fn a_failed_escalation_save_keeps_its_identity_and_event_epoch_on_retry() {
    // why: a retry must not reuse a far-overdue id for later completion.
    // catches: restored attention without restoring its event epoch.
    use crate::fault::{FaultPlan, FaultRule, When, WriteKind};
    let env = rig();
    env.vigil.hold(HoldRequest { work: vec!["none".into()], label: Some("a".into()), expect_s: Some(100), ..request_for(&[]) }).await.unwrap();
    let normal = wait(&env.vigil, 180).await;
    assert_eq!(normal.events.len(), 1);
    let mut stored = env.vigil.store().load(&id()).unwrap();
    stored.spec.interval_s = 3600;
    stored.last_tick_at = Some(normal.now);
    env.vigil.store().save(&stored).unwrap();
    let failing = env.another_process().with_write_faults(FaultPlan::from_rules(vec![FaultRule::new(WriteKind::Watch, 5, When::Nth(1))]));
    let first = wait(&failing, 60).await;
    assert!(matches!(first.persist, Some(crate::PersistNote::NotSaved { .. })));
    assert_eq!(first.events[0].summary, "item far overdue: a");
    let second = wait(&failing, 60).await;
    assert_eq!(second.events[0].event_id, first.events[0].event_id);
    assert!(second.events[0].redelivered);
    env.vigil.hold_done(&id(), "a").await.unwrap();
    let completed = wait(&env.vigil, 30).await;
    assert_ne!(completed.events[0].event_id, second.events[0].event_id);
}

#[test]
fn far_clock_reads_older_items_and_saturates_damaged_limits() {
    // why: additive state must load old files, including oversized hand edits.
    // catches: required escalation flags or multiplication wrapping before timestamp saturation.
    let mut item: crate::items::HoldItem = serde_json::from_value(serde_json::json!({
        "label": "old", "added_at": "2026-11-13T10:00:00Z", "expect_s": 100
    })).unwrap();
    assert!(!item.far_overdue_raised);
    assert_eq!(crate::items::far_overdue_at(&item), Some(crate::testing::at("2026-11-13T10:03:20Z")));
    item.expect_s = Some(u64::MAX);
    assert_eq!(crate::items::far_overdue_at(&item), Some(crate::Timestamp(chrono::DateTime::<chrono::Utc>::MAX_UTC)));
    item.far_overdue_raised = true;
    assert_eq!(crate::items::far_overdue_at(&item), None);
}
