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
    vigil.keepalive(KeepaliveRequest { watch_id: Some(id(name)), max_s: Some(90), orchestrator: Some(false), ..Default::default() }).await.unwrap()
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

#[tokio::test(start_paused = true)]
async fn session_wait_discovers_new_holds_while_waiting() {
    // why: a single background call must cover later work without rearming.
    // catches: taking a fixed snapshot of session holds at call start.
    let mut env = rig();
    env.vigil.session_key = Some("session-a".into());
    env.vigil.hold(HoldRequest { id: Some("anchor".into()), ..Default::default() }).await.unwrap();
    let fresh = Vigil::new(env.vigil.store().root(), env.clock.clone(), env.judge.clone(), crate::testing::default_config())
        .with_session_key(Some("session-a".into())).with_reader(env.reader.clone());
    let request = serde_json::from_value(json!({"session":true,"orchestrator":false,"max_s":60})).unwrap();
    let add_later = async {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        env.vigil.hold(HoldRequest { id: Some("later".into()), work: vec!["until:2020-01-01T00:00:00Z".into()], ..Default::default() }).await.unwrap();
    };
    let (answer, ()) = tokio::join!(fresh.keepalive(request), add_later);
    let answer = answer.unwrap();
    assert_eq!(answer.watch_id.as_str(), "later");
    assert!((10..=15).contains(&answer.waited_s.unwrap()), "discovery within the 5 s outbox pass: {:?}", answer.waited_s);
}

#[tokio::test(start_paused = true)]
async fn history_compaction_preserves_sensed_inputs_and_unique_event_ids() {
    // why: freeing slots must not read the wrong input or recycle an event id.
    // proves: CLAIM(hold-history)
    // catches: pruning items without remapping inputs or retaining a sequence.
    let env = rig();
    env.reader.set_text("qex:anchor", "running");
    env.vigil.hold(request_for(&["qex:anchor"])).await.unwrap();
    let mut ids = std::collections::BTreeSet::new();
    for n in 0..70 {
        let job = format!("qex:job-{n}");
        env.reader.set_text(&job, "completed");
        env.vigil.hold_add(&id("h"), HoldItemRequest { work: job, label: Some(format!("job-{n}")), expect_s: None, ..Default::default() }).await.unwrap();
        let answer = wait(&env.vigil, "h").await;
        assert_eq!(answer.status, crate::watch::WatchStatus::Pending);
        for event in &answer.events { assert!(ids.insert(event.event_id.clone()), "duplicate {}", event.event_id); }
    }
    let stored = watch_file(&env.vigil, "h");
    assert!(stored.spec.inputs.len() <= 2, "only live inputs and the last completion: {}", stored.spec.inputs.len());
    env.reader.set_text("qex:anchor", "completed");
    let answer = wait(&env.vigil, "h").await;
    assert_eq!(answer.status, crate::watch::WatchStatus::Fired);
    assert!(answer.events.iter().any(|e| e.summary == "item ended: qex:anchor: completed"));
}

#[tokio::test(start_paused = true)]
async fn a_failed_done_save_keeps_the_whole_previous_state() {
    // why: completion and its note must commit together, including compaction.
    // catches: persisting ended_at separately or dropping a failed note.
    use crate::fault::{FaultPlan, FaultRule, When, WriteKind};
    let env = rig();
    env.vigil.hold(HoldRequest { work: vec!["none".into()], label: Some("delegate".into()), ..request_for(&[]) }).await.unwrap();
    let before = watch_file(&env.vigil, "h");
    let failing = env.another_process().with_write_faults(FaultPlan::from_rules(vec![
        FaultRule::new(WriteKind::Watch, 5, When::Every),
    ]));
    assert!(failing.hold_done(&id("h"), "delegate").await.is_err());
    assert_eq!(watch_file(&env.vigil, "h"), before);
    env.vigil.hold_done(&id("h"), "delegate").await.unwrap();
    assert_eq!(wait(&env.vigil, "h").await.events[0].summary, "item ended: delegate: done");
}

#[tokio::test(start_paused = true)]
async fn a_failed_session_acknowledgement_redelivers_across_processes() {
    // why: stdout may succeed while the acknowledgement disk write fails.
    // catches: excluding a finished hold before its durable acknowledgement.
    use crate::fault::{FaultPlan, FaultRule, When, WriteKind};
    let mut env = rig();
    env.vigil.session_key = Some("session-a".into());
    env.vigil.hold(HoldRequest { work: vec!["until:2020-01-01T00:00:00Z".into()], ..request_for(&[]) }).await.unwrap();
    env.vigil.tick_due(&id("h")).await.unwrap();
    let fresh = || Vigil::new(env.vigil.store().root(), env.clock.clone(), env.judge.clone(), crate::testing::default_config())
        .with_session_key(Some("session-a".into()));
    let failing = fresh().with_write_faults(FaultPlan::from_rules(vec![FaultRule::new(WriteKind::Watch, 5, When::Every)]));
    let request = KeepaliveRequest { session: true, orchestrator: Some(false), max_s: Some(30), ..Default::default() };
    let first = failing.keepalive(request.clone()).await.unwrap();
    let second = fresh().keepalive(request.clone()).await.unwrap();
    assert_eq!(second.events, first.events);
    assert!(fresh().keepalive(request).await.is_err(), "healthy ack is durable");
}

#[tokio::test(start_paused = true)]
async fn session_scope_refuses_missing_identity_and_mixed_scopes() {
    // why: missing attribution must never accidentally cover another session.
    // catches: falling back to every hold or silently ignoring an explicit id.
    let env = rig();
    let request = KeepaliveRequest { session: true, orchestrator: Some(false), max_s: Some(30), ..Default::default() };
    assert!(env.vigil.keepalive(request.clone()).await.unwrap_err().to_string().contains("session identity"));
    assert!(env.vigil.keepalive(KeepaliveRequest { watch_id: Some(id("h")), ..request }).await.unwrap_err().to_string().contains("mutually exclusive"));
}

#[tokio::test(start_paused = true)]
async fn brief_preserves_all_advice_instructions_and_unavailable_values() {
    // why: brief output is still the operator's heads-up display, not just numbers.
    // catches: showing only numeric values or hiding unavailable probes.
    use crate::advisory_eval::{Advice, AdviceKind};
    let env = rig();
    env.vigil.hold(request_for(&[])).await.unwrap();
    let mut answer = wait(&env.vigil, "h").await;
    answer.advice = Some(vec![Advice { id: "boat-budget".into(), kind: AdviceKind::Advice, message: "38% left.\nPrefer Boat.".into(), checked_at: answer.now, value: Some(38.0) }, Advice::unavailable("codex-weekly", "probe failed", answer.now)]);
    let brief = answer.brief();
    assert_eq!(brief.lines().count(), 2);
    assert!(brief.contains("boat-budget 38: 38% left. Prefer Boat."), "{brief}");
    assert!(brief.contains("advisory codex-weekly unavailable: probe failed"), "{brief}");
    assert!(brief.contains("next: call keepalive again"), "{brief}");
    answer.advice = Some(vec![]);
    assert!(answer.brief().contains("advice: none"));
    answer.advice = None;
    assert!(answer.brief().contains("advice: off"));
}

#[tokio::test(start_paused = true)]
async fn ending_an_old_live_item_after_history_fills_still_returns_its_report() {
    // why: the oldest-added item can be the last to complete and be pruned.
    // catches: looking up the result after compaction removed that item.
    let env = rig();
    env.vigil.hold(HoldRequest { work: vec!["none".into()], label: Some("old".into()), ..request_for(&[]) }).await.unwrap();
    for n in 0..65 {
        let label = format!("task-{n}");
        env.vigil.hold_add(&id("h"), item(&label)).await.unwrap();
        env.vigil.hold_done(&id("h"), &label).await.unwrap();
    }
    let report = env.vigil.hold_done(&id("h"), "old").await.unwrap();
    assert_eq!(report.label, "old");
    assert!(report.ended_at.is_some());
    let answer = wait(&env.vigil, "h").await;
    assert!(answer.events.iter().any(|event| event.summary == "item ended: old: done"));
}

#[tokio::test(start_paused = true)]
async fn session_wait_cannot_confirm_unconfirmed_holds_even_with_a_token() {
    // why: session selection must preserve the existing per-watch guard.
    // catches: using --confirm on a session wait to confirm every hold implicitly.
    let mut env = rig();
    env.vigil.session_key = Some("session-a".into());
    env.vigil.hold(request_for(&[])).await.unwrap();
    let guarded = Vigil::new(env.vigil.store().root(), env.clock.clone(), env.judge.clone(), crate::Config {
        orchestrator_confirm: true, ..crate::testing::default_config()
    }).with_session_key(Some("session-a".into()));
    let request = KeepaliveRequest { session: true, orchestrator: Some(true), confirm: Some("cannot-confirm-a-session".into()), max_s: Some(30), ..Default::default() };
    assert!(matches!(guarded.keepalive(request.clone()).await, Err(crate::VigilError::OrchestratorUnconfirmed(_))));
    assert!(!guarded.orchestrator_confirmed(&watch_file(&env.vigil, "h")));
    guarded.mark_orchestrator_confirmed(&watch_file(&env.vigil, "h")).unwrap();
    let answer = guarded.keepalive(KeepaliveRequest { confirm: None, ..request }).await.unwrap();
    assert_eq!(answer.waited_s, Some(30));
}

#[tokio::test(start_paused = true)]
async fn quiet_terminal_holds_still_require_confirmation_because_they_keep_waiting() {
    // why: terminal results used to return immediately; quiet releases now wait.
    // catches: bypassing the guard by first ending an unconfirmed hold in plain mode.
    let env = rig();
    env.vigil.hold(HoldRequest { work: vec!["none".into()], label: Some("x".into()), ..request_for(&[]) }).await.unwrap();
    env.vigil.hold_done(&id("h"), "x").await.unwrap();
    env.vigil.tick_due(&id("h")).await.unwrap();
    assert_eq!(watch_file(&env.vigil, "h").status, crate::watch::WatchStatus::Fired);
    let guarded = Vigil::new(env.vigil.store().root(), env.clock.clone(), env.judge.clone(), crate::Config {
        orchestrator_confirm: true, ..crate::testing::default_config()
    });
    let result = guarded.keepalive(KeepaliveRequest { watch_id: Some(id("h")), orchestrator: Some(true), max_s: Some(30), ..Default::default() }).await;
    assert!(matches!(result, Err(crate::VigilError::OrchestratorUnconfirmed(_))), "{result:?}");
    guarded.mark_orchestrator_confirmed(&watch_file(&env.vigil, "h")).unwrap();
    let implicit = guarded.keepalive(KeepaliveRequest { watch_id: Some(id("h")), max_s: Some(30), ..Default::default() }).await.unwrap();
    assert!(implicit.cache.unwrap().credit_check.is_some(), "confirmed quiet holds retain stored main-session mode");
}

/// A second process of the same session, sharing the rig's store and clock.
fn peer_of(env: &crate::testing::Env, session: &str) -> Vigil {
    Vigil::new(env.vigil.store().root(), env.clock.clone(), env.judge.clone(), crate::testing::default_config())
        .with_session_key(Some(session.into())).with_reader(env.reader.clone())
}
fn session_request(max_s: u64) -> KeepaliveRequest {
    KeepaliveRequest { session: true, orchestrator: Some(false), max_s: Some(max_s), ..Default::default() }
}

#[tokio::test(start_paused = true)]
async fn session_wait_still_discovers_new_holds_while_a_tick_waits_for_a_held_lock() {
    // why: R1. A hold whose lock another process keeps must not stop the call
    //      from looking for new holds; the five-second pass is a promise.
    // catches: running the discovery pass only between rounds, so a round
    //      stuck on one lock postpones it to the end of the window.
    let mut env = rig();
    env.vigil.session_key = Some("session-a".into());
    env.vigil.hold(HoldRequest { id: Some("anchor".into()), ..Default::default() }).await.unwrap();
    let lock = env.vigil.store().try_lock_watch(&id("anchor")).unwrap().unwrap();
    let waiter = peer_of(&env, "session-a");
    let add_later = async {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        env.vigil.hold(HoldRequest { id: Some("later".into()), work: vec!["until:2020-01-01T00:00:00Z".into()], ..Default::default() }).await.unwrap();
    };
    let (answer, ()) = tokio::join!(waiter.keepalive(session_request(60)), add_later);
    drop(lock);
    let answer = answer.unwrap();
    assert_eq!(answer.watch_id.as_str(), "later", "{answer:?}");
    assert!((10..=15).contains(&answer.waited_s.unwrap()), "discovery within the 5 s pass: {:?}", answer.waited_s);
}

#[tokio::test(start_paused = true)]
async fn a_ready_autonomous_item_end_returns_at_once_while_another_hold_is_locked() {
    // why: R1. An item that ended by itself in a hold that stays live is
    //      call-ending attention, already saved; it must not wait for a tick
    //      that is stuck on an unrelated hold's lock.
    // catches: a round that ignores `raised_attention` and waits out the window.
    let mut env = rig();
    env.vigil.session_key = Some("session-a".into());
    env.vigil.hold(HoldRequest { id: Some("locked".into()), ..Default::default() }).await.unwrap();
    env.vigil.hold(HoldRequest { id: Some("work".into()), work: vec!["until:2020-01-01T00:00:00Z".into()], ..Default::default() }).await.unwrap();
    env.vigil.hold_add(&id("work"), item("anchor")).await.unwrap();
    let lock = env.vigil.store().try_lock_watch(&id("locked")).unwrap().unwrap();
    let answer = env.vigil.keepalive(session_request(60)).await.unwrap();
    drop(lock);
    assert!(answer.events.iter().any(|e| e.summary.contains("item ended:") && e.is_call_ending()), "{answer:?}");
    assert_eq!(answer.waited_s, Some(0), "ready event held behind an unrelated lock: {answer:?}");
}

#[tokio::test(start_paused = true)]
async fn another_transports_delivery_does_not_abort_a_session_wait() {
    // why: R2. A named wait in another process may deliver the last quiet
    //      terminal hold while a session wait is still holding its window.
    // catches: treating an empty refresh of the session scope as NothingToCover.
    let mut env = rig();
    env.vigil.session_key = Some("session-a".into());
    env.vigil.hold(HoldRequest { work: vec!["none".into()], label: Some("manual".into()), ..request_for(&[]) }).await.unwrap();
    env.vigil.hold_done(&id("h"), "manual").await.unwrap();
    env.vigil.tick_due(&id("h")).await.unwrap();
    let waiter = peer_of(&env, "session-a");
    let other = async {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        peer_of(&env, "session-a").keepalive(KeepaliveRequest { watch_id: Some(id("h")), max_s: Some(30), orchestrator: Some(false), ..Default::default() }).await.unwrap();
    };
    let (answer, ()) = tokio::join!(waiter.keepalive(session_request(90)), other);
    let answer = answer.expect("a concurrent delivery must not end the held call with an error");
    assert_eq!(answer.waited_s, Some(90));
    assert_eq!(answer.watch_id.as_str(), "h");
    assert!(answer.events.iter().any(|e| e.summary == "item ended: manual: done"), "the quiet note still rides the return: {answer:?}");
}

#[tokio::test(start_paused = true)]
async fn ready_completion_does_not_wait_for_an_overdue_attention_lock() {
    // why: R2-1. The post-tick attention pass must not delay a saved result.
    // proves: CLAIM(R2-attention)
    // catches: serial attention lock waits after a ready tick ends its round.
    let mut env = rig();
    env.vigil.session_key = Some("session-a".into());
    env.vigil.hold(HoldRequest { id: Some("locked".into()), work: vec!["none".into()], label: Some("late".into()), expect_s: Some(1), ..Default::default() }).await.unwrap();
    let lock = env.vigil.store().try_lock_watch(&id("locked")).unwrap().unwrap();
    env.vigil.hold(HoldRequest { id: Some("writable".into()), work: vec!["none".into()], label: Some("also-late".into()), expect_s: Some(1), ..Default::default() }).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    env.vigil.hold(HoldRequest { id: Some("work".into()), work: vec!["until:2020-01-01T00:00:00Z".into()], ..Default::default() }).await.unwrap();
    env.vigil.hold_add(&id("work"), item("anchor")).await.unwrap();
    let answer = env.vigil.keepalive(session_request(30)).await.unwrap();
    drop(lock);
    assert!(answer.events.iter().any(|e| e.watch_id == "writable" && e.summary.contains("item far overdue:")), "writable secondary attention must ride this return: {answer:?}");
    assert!(answer.events.iter().any(|e| e.summary.contains("item ended:") && e.is_call_ending()), "{answer:?}");
    assert_eq!(answer.waited_s, Some(0), "ready completion blocked by an overdue attention lock: {answer:?}");
}

async fn slow_session_completion(discover: bool) {
    let mut env = rig();
    env.vigil.session_key = Some("session-a".into());
    env.reader.set_text("qex:slow", "completed");
    env.vigil.hold(HoldRequest { id: Some("slow".into()), work: vec!["qex:slow".into()], ..Default::default() }).await.unwrap();
    env.reader.set_sleep("qex:slow", std::time::Duration::from_secs(8));
    let reads_before = env.reader.started_count("qex:slow");
    let add_later = async {
        if discover {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            for n in 0..6 {
                env.vigil.hold(HoldRequest { id: Some(format!("later-{n}")), ..Default::default() }).await.unwrap();
                if n < 5 { tokio::time::sleep(std::time::Duration::from_secs(5)).await; }
            }
        }
    };
    let (answer, ()) = tokio::join!(env.vigil.keepalive(session_request(30)), add_later);
    let answer = answer.unwrap();
    assert_eq!(answer.waited_s, Some(8), "discovery must preserve the original eight-second read: {answer:?}");
    assert!(answer.events.iter().any(|e| e.summary == "item ended: qex:slow: completed"), "{answer:?}");
    assert_eq!(env.reader.started_count("qex:slow") - reads_before, 1, "an unfinished read must not restart");
    assert_eq!(watch_file(&env.vigil, "slow").tick, 1);
}

#[tokio::test(start_paused = true)]
async fn slow_session_reader_completes_without_discovery() {
    // why: positive control for the slow reader used by the starvation regression.
    // proves: CLAIM(R2-discovery)
    slow_session_completion(false).await;
}

#[tokio::test(start_paused = true)]
async fn repeated_discovery_preserves_a_slow_session_tick() {
    // why: R2-2. New holds must join sensing without starving active reads.
    // proves: CLAIM(R2-discovery)
    // catches: ending the round and restarting all unfinished ticks on discovery.
    slow_session_completion(true).await;
}

#[tokio::test(start_paused = true)]
async fn overdue_attention_contention_keeps_discovery_running_between_ticks() {
    // why: R2-1. A non-due tick must not move the lock wait into attention.
    // proves: CLAIM(R2-attention)
    // catches: waiting for the attention lock or spinning on its past threshold.
    let mut env = rig();
    env.vigil.session_key = Some("session-a".into());
    env.vigil.hold(HoldRequest { id: Some("locked".into()), work: vec!["none".into()], label: Some("late".into()), expect_s: Some(1), ..Default::default() }).await.unwrap();
    env.vigil.tick_due(&id("locked")).await.unwrap();
    let lock = env.vigil.store().try_lock_watch(&id("locked")).unwrap().unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let attempts = crate::fault::FaultPlan::none();
    let waiter = peer_of(&env, "session-a").with_write_faults(attempts.clone());
    let add_later = async {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        env.vigil.hold(HoldRequest { id: Some("later".into()), work: vec!["until:2020-01-01T00:00:00Z".into()], ..Default::default() }).await.unwrap();
    };
    let (answer, ()) = tokio::join!(waiter.keepalive(session_request(30)), add_later);
    // Scans at 0, 5, 10 and possibly 15 s, plus last-call bookkeeping.
    let retries = attempts.attempts(crate::fault::WriteKind::LockWait);
    assert!((3..=5).contains(&retries), "attention must retry at scan pace, not spin: {retries}");
    drop(lock);
    let answer = answer.unwrap();
    assert_eq!(answer.watch_id.as_str(), "later", "{answer:?}");
    assert!((10..=15).contains(&answer.waited_s.unwrap()), "attention contention must retain the five-second scan: {answer:?}");
    let resumed = env.vigil.keepalive(session_request(30)).await.unwrap();
    assert_eq!(resumed.waited_s, Some(0), "skipped attention remains due");
    assert!(resumed.events.iter().any(|e| e.watch_id == "locked" && e.summary.contains("item far overdue:")), "{resumed:?}");
}
