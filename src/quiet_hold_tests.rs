//! Quiet holds: regressions at the public engine seam, with virtual time.
use crate::hold::{HoldItemRequest, HoldRequest};
use crate::hold_tests::{rig, request_for, watch_file};
use crate::keepalive::KeepaliveRequest;
use crate::watch::WatchId;
use crate::Vigil;
use serde_json::json;

fn id(name: &str) -> WatchId { WatchId::parse(name).unwrap() }
fn item(label: &str) -> HoldItemRequest {
    HoldItemRequest { work: "none".into(), label: Some(label.into()), ..Default::default() }
}
async fn wait(vigil: &Vigil, name: &str) -> crate::Envelope {
    vigil.keepalive(KeepaliveRequest { watch_id: Some(id(name)), max_s: Some(60), orchestrator: Some(false), ..Default::default() }).await.unwrap()
}

#[tokio::test(start_paused = true)]
async fn explicit_done_is_a_durable_note_even_when_it_releases_the_hold() {
    // why: caller already handled completion; another turn wastes cache warming.
    // catches: treating either the item or the final release as an interrupt.
    let env = rig();
    env.vigil.hold(HoldRequest { work: vec!["none".into()], label: Some("delegate".into()), ..request_for(&[]) }).await.unwrap();
    env.vigil.hold_done(&id("h"), "delegate").await.unwrap();
    let (first, delivery) = env.vigil.keepalive_uncommitted(
        KeepaliveRequest { watch_id: Some(id("h")), max_s: Some(60), orchestrator: Some(false), ..Default::default() },
        &crate::keepalive::NoProgress, std::future::pending::<()>()).await.unwrap().unwrap();
    assert_eq!(first.waited_s, Some(60));
    assert_eq!(first.events[0].summary, "item ended: delegate: done");
    assert!(!first.events[0].is_call_ending());
    assert_eq!(first.status, crate::watch::WatchStatus::Fired);
    // A lost stdout must leave the complete result available to a fresh reader.
    drop(delivery);
    let again = wait(&env.vigil, "h").await;
    assert_eq!(again.events.iter().map(|e| &e.event_id).collect::<Vec<_>>(), first.events.iter().map(|e| &e.event_id).collect::<Vec<_>>());
}

#[tokio::test(start_paused = true)]
async fn done_notes_ride_the_next_autonomous_completion_and_do_not_wake_early() {
    // why: explicit and sensed work must share one wait without losing either.
    // catches: quiet notes dropped, or carried outbox notes waking a later pass.
    let env = rig();
    env.reader.set_text("qex:a", "running");
    env.vigil.hold(request_for(&["qex:a"])).await.unwrap();
    env.vigil.hold_add(&id("h"), item("delegate")).await.unwrap();
    let complete = async {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        env.vigil.hold_done(&id("h"), "delegate").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        env.reader.set_text("qex:a", "completed");
    };
    let (answer, ()) = tokio::join!(wait(&env.vigil, "h"), complete);
    assert_eq!(answer.waited_s, Some(60)); // sensed at the 60 s tick
    assert!(answer.events.iter().any(|e| e.summary == "item ended: delegate: done"));
    assert!(answer.events.iter().any(|e| e.summary.contains("completed") && e.is_call_ending()));
}

#[tokio::test(start_paused = true)]
async fn ended_items_do_not_consume_live_capacity_and_history_is_bounded() {
    // why: a busy session must reuse a hold rather than open one every 16 tasks.
    // catches: cap on total items, unbounded history, input indices after pruning.
    let env = rig();
    env.reader.set_text("qex:anchor", "running");
    env.vigil.hold(request_for(&["qex:anchor"])).await.unwrap();
    for n in 0..100 {
        let label = format!("task-{n}");
        env.vigil.hold_add(&id("h"), item(&label)).await.unwrap();
        env.vigil.hold_done(&id("h"), &label).await.unwrap();
    }
    let stored = watch_file(&env.vigil, "h");
    assert!(stored.items.as_ref().unwrap().len() <= 65, "64 ended plus one live");
    assert!(stored.items.as_ref().unwrap().iter().any(|i| i.label == "task-99"));
    assert_eq!(stored.deadline_at, stored.created_at.plus_secs(604800));
    for n in 0..15 { env.vigil.hold_add(&id("h"), item(&format!("live-{n}"))).await.unwrap(); }
    assert!(env.vigil.hold_add(&id("h"), item("overflow")).await.is_err());
    assert_eq!(watch_file(&env.vigil, "h").spec.inputs.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn session_wait_finds_owned_holds_across_instances_and_keeps_delivery_durable() {
    // why: CLI creates and waits in separate processes; unrelated sessions stay out.
    // catches: process-local selection, global selection, repeated terminal delivery.
    let mut env = rig();
    env.vigil.session_key = Some("session-a".into());
    env.vigil.hold(HoldRequest { id: Some("owned".into()), work: vec!["until:2020-01-01T00:00:00Z".into()], ..Default::default() }).await.unwrap();
    env.vigil.session_key = Some("session-b".into());
    env.vigil.hold(HoldRequest { id: Some("foreign".into()), ..Default::default() }).await.unwrap();
    let fresh = Vigil::new(env.vigil.store().root(), env.clock.clone(), env.judge.clone(), crate::testing::default_config())
        .with_session_key(Some("session-a".into()));
    let request: KeepaliveRequest = serde_json::from_value(json!({"session":true,"orchestrator":false,"max_s":30})).unwrap();
    let result = fresh.keepalive(request.clone()).await.unwrap();
    assert_eq!(result.watch_id.as_str(), "owned");
    let next = Vigil::new(env.vigil.store().root(), env.clock.clone(), env.judge.clone(), crate::testing::default_config())
        .with_session_key(Some("session-a".into()));
    assert!(next.keepalive(request).await.is_err(), "delivered terminal hold is skipped across processes");
}
