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
