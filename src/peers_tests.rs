//! Pilot proofs for SPEC "Orchestrator peers". Literal registry fixtures also
//! pin the reader independently of the production writer.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::hold::HoldRequest;
use crate::keepalive::KeepaliveRequest;
use crate::testing::{Env, TokioClock, spec_instant};
use crate::watch::WatchId;

fn env() -> Env {
    let mut env = Env::with_clock(Arc::new(TokioClock::new(spec_instant())));
    env.vigil = env.vigil.with_session_key(Some("pilot-session".into()));
    env
}

fn id(name: &str) -> WatchId {
    WatchId::parse(name).unwrap()
}

async fn hold(env: &Env, name: &str) {
    env.vigil.hold(HoldRequest { id: Some(name.into()), ..HoldRequest::default() }).await.unwrap();
}

fn request(orchestrator: Option<bool>) -> KeepaliveRequest {
    KeepaliveRequest { watch_id: Some(id("main")), max_s: Some(30), orchestrator, confirm: None }
}

/// A real foreign pid, reaped even if an assertion panics. Its lifetime, not a
/// timing guess about another test process, controls stale-record fixtures.
struct Peer(std::process::Child);
impl Peer {
    fn new() -> Peer {
        Peer(std::process::Command::new("sleep").arg("3600").spawn().unwrap())
    }
    fn pid(&self) -> u32 { self.0.id() }
}
impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn fixture(env: &Env, pid: u32, session: Option<&str>, orchestrator: bool, names: &[&str]) -> PathBuf {
    let dir = env.root().join("calls");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{pid}-900.json"));
    // Fixture clock starts 7 s before main's call. The 30 s main window
    // therefore returns an informational peer with waited_s = 37.
    let watches: Vec<Value> = names.iter().map(|name| json!({
        "watch_id": name, "kind": "hold", "expect_s": 120, "inputs": "process:4242"
    })).collect();
    crate::store::write_atomic(&path, &json!({
        "pid": pid, "session": session, "orchestrator": orchestrator,
        "started_at": env.clock.now().0 - chrono::Duration::seconds(7), "watch_ids": names,
        "watches": watches, "max_s": 270
    })).unwrap();
    path
}

fn records(env: &Env) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(env.root().join("calls")) else { return vec![] };
    entries.filter_map(Result::ok).filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .map(|entry| serde_json::from_slice(&std::fs::read(entry.path()).unwrap()).unwrap()).collect()
}

/// Poll through the real blocking entry point without advancing paused time.
/// Negative cases prove registration too, so they cannot pass vacuously when
/// the entire peer feature is absent.
async fn registered_call(env: &Env, orchestrator: Option<bool>) -> Value {
    let mut call = Box::pin(env.vigil.keepalive(request(orchestrator)));
    tokio::select! {
        biased;
        result = &mut call => panic!("expected a quiet blocking call, got {result:?}"),
        () = tokio::task::yield_now() => {}
    }
    let own: Vec<_> = records(env).into_iter().filter(|record| record["pid"] == std::process::id() && record["watch_ids"] == json!(["main"])).collect();
    assert_eq!(own.len(), 1, "every blocking call must register itself");
    serde_json::to_value(call.await.unwrap()).unwrap()
}

#[tokio::test(start_paused = true)]
async fn foreign_orchestrator_ends_the_call_once_per_record() {
    // why: a delegated orchestrator wait must wake the main session now.
    // catches: reporting only at timeout; missing kind; repeating on re-call.
    // proves: CLAIM(peers-flag), CLAIM(peers-once)
    let env = env();
    hold(&env, "main").await;
    let peer = Peer::new();
    let path = fixture(&env, peer.pid(), Some("pilot-session"), true, &["rogue-one", "rogue-two"]);
    let started_at = records(&env)[0]["started_at"].as_str().unwrap().to_string();
    let start = tokio::time::Instant::now();
    let answer = serde_json::to_value(env.vigil.keepalive(request(Some(true))).await.unwrap()).unwrap();
    assert_eq!(answer["next"], "act on events", "foreign orchestrator must end the call");
    assert_eq!(start.elapsed(), Duration::ZERO, "already present at the first sense pass");
    assert_eq!(answer["status"], "pending");
    assert_eq!(answer["events"].as_array().unwrap().len(), 1);
    assert_eq!(answer["events"][0]["kind"], "foreign_orchestrator");
    let summary = answer["events"][0]["summary"].as_str().unwrap();
    for text in ["rogue-one", "rogue-two", &peer.pid().to_string(), &started_at] {
        assert!(summary.contains(text), "missing {text}: {summary}");
    }
    assert!(path.exists(), "the foreign call is still in flight");
    // A fresh engine also must not repeat this same peer record (CLI calls).
    let other = env.another_process().with_session_key(Some("pilot-session".into()));
    let again = other.keepalive(request(Some(true))).await.unwrap();
    assert!(again.events.is_empty(), "raised once per record across calls");
    assert_eq!(again.waited_s, Some(30));
}

#[tokio::test(start_paused = true)]
async fn ordinary_peer_is_information_with_the_spec_wire_fields() {
    // why: legitimate subagent waits are visible without waking the main call.
    // catches: flagging all peers; missing fields; measuring the main's age.
    // proves: CLAIM(peers-info)
    let env = env();
    hold(&env, "main").await;
    let peer = Peer::new();
    fixture(&env, peer.pid(), Some("pilot-session"), false, &["sub"]);
    let answer = serde_json::to_value(env.vigil.keepalive(request(Some(true))).await.unwrap()).unwrap();
    assert_eq!(answer["peers"], json!([{
        "watch_id": "sub", "kind": "hold", "expect_s": 120, "waited_s": 37, "inputs": "process:4242"
    }]));
    assert_eq!(answer["waited_s"], 30);
    assert_eq!(answer["next"], "call keepalive again");
    assert_eq!(answer["events"], json!([]));
}

#[tokio::test(start_paused = true)]
async fn own_pid_and_multiple_confirmed_holds_are_not_foreign() {
    // why: multiple main-session holds must never flag one another.
    // catches: excluding only the primary id or only the calling pid; any
    //      confirmed watch wrongly exempting a mixed, foreign call.
    // proves: CLAIM(peers-own)
    let env = env();
    for name in ["main", "own-a", "own-b"] {
        hold(&env, name).await;
    }
    for name in ["own-a", "own-b"] {
        let watch = env.vigil.store().load(&id(name)).unwrap();
        env.vigil.mark_orchestrator_confirmed(&watch).unwrap();
    }
    let peer = Peer::new();
    fixture(&env, peer.pid(), Some("pilot-session"), true, &["own-a", "own-b"]);
    fixture(&env, std::process::id(), Some("pilot-session"), true, &["same-pid"]);
    let answer = registered_call(&env, Some(true)).await;
    assert_eq!(answer["events"], json!([]));
    assert!(answer.get("peers").is_none());
    assert_eq!(answer["waited_s"], 30);
    fixture(&env, peer.pid(), Some("pilot-session"), true, &["own-a", "unconfirmed"]);
    let mixed = env.vigil.keepalive(request(Some(true))).await.unwrap();
    assert_eq!(mixed.next, crate::Next::ActOnEvents, "EVERY watch must be confirmed to count as own");
}

#[tokio::test(start_paused = true)]
async fn plain_calls_do_not_scan_or_flag_peers_and_register_effective_mode() {
    // why: a hold's stored default must not turn a plain gated call into an
    //      orchestrator, and ordinary callers must leave the registry alone.
    // catches: looking at the requested/stored flag instead of the gate.
    // proves: CLAIM(peers-scope), CLAIM(peers-registry)
    let mut env = env();
    env.config.orchestrator_confirm = true;
    env.vigil = env.another_process().with_session_key(Some("pilot-session".into()));
    hold(&env, "main").await;
    let peer = Peer::new();
    fixture(&env, peer.pid(), Some("pilot-session"), true, &["rogue"]);
    let stale = fixture(&env, u32::MAX, Some("pilot-session"), true, &["stale"]);
    let mut call = Box::pin(env.vigil.keepalive(request(None)));
    tokio::select! { biased; r = &mut call => panic!("unexpected return: {r:?}"), () = tokio::task::yield_now() => {} }
    let own: Vec<_> = records(&env).into_iter().filter(|r| r["pid"] == std::process::id()).collect();
    assert_eq!(own.len(), 1, "plain calls register too");
    assert_eq!(own[0]["orchestrator"], false, "effective mode after confirmation gate");
    let answer = serde_json::to_value(call.await.unwrap()).unwrap();
    assert_eq!(answer["events"], json!([]));
    assert!(answer.get("peers").is_none());
    assert!(stale.exists(), "plain mode must not scan or prune");
}

#[tokio::test(start_paused = true)]
async fn stale_dead_pid_is_ignored_and_removed() {
    // why: a killed caller cannot leave a permanent red flag on disk.
    // catches: trusting files without checking pid liveness; ignoring but
    //      never pruning stale records.
    // proves: CLAIM(peers-stale)
    let env = env();
    hold(&env, "main").await;
    let peer = Peer::new();
    let pid = peer.pid();
    drop(peer);
    let stale = fixture(&env, pid, Some("pilot-session"), true, &["dead"]);
    let answer = env.vigil.keepalive(request(Some(true))).await.unwrap();
    assert!(!stale.exists(), "next reader must prune the dead call");
    assert!(answer.events.is_empty());
    assert_eq!(answer.waited_s, Some(30));
}

#[tokio::test(start_paused = true)]
async fn different_sessions_are_invisible() {
    // why: sharing the state root does not make unrelated sessions peers.
    // catches: scanning globally; filtering events but leaking info peers.
    // proves: CLAIM(peers-session)
    let env = env();
    hold(&env, "main").await;
    let a = Peer::new();
    let b = Peer::new();
    fixture(&env, a.pid(), Some("other-session"), true, &["rogue"]);
    fixture(&env, b.pid(), Some("other-session"), false, &["sub"]);
    let answer = registered_call(&env, Some(true)).await;
    assert_eq!(answer["events"], json!([]));
    assert!(answer.get("peers").is_none());
    assert_eq!(answer["waited_s"], 30);
}

#[tokio::test(start_paused = true)]
async fn no_session_means_no_peers() {
    // why: callers with no discoverable session cannot claim one another.
    // catches: treating two absent session ids as the same session.
    // proves: CLAIM(peers-session)
    let mut env = env();
    env.vigil = env.vigil.with_session_key(None);
    hold(&env, "main").await;
    let peer = Peer::new();
    fixture(&env, peer.pid(), None, true, &["rogue"]);
    let answer = registered_call(&env, Some(true)).await;
    assert_eq!(answer["events"], json!([]));
    assert!(answer.get("peers").is_none());
}

#[tokio::test(start_paused = true)]
async fn call_registry_is_atomic_unique_and_removed_on_return_cancel_or_drop() {
    // why: peers describe active calls only, including several in one pid.
    // catches: one file per pid; recording requested max instead of the clamp;
    //      cleanup only on success; a guard dropped before the blocking wait.
    // proves: CLAIM(peers-registry)
    let env = env();
    hold(&env, "main").await;
    let mut short = request(Some(false));
    short.max_s = Some(1); // shipped minimum is 30, not the requested 1
    let mut a = Box::pin(env.vigil.keepalive(short));
    let mut b = Box::pin(env.vigil.keepalive(request(Some(false))));
    tokio::select! { biased; r = &mut a => panic!("unexpected return: {r:?}"), () = tokio::task::yield_now() => {} }
    tokio::select! { biased; r = &mut b => panic!("unexpected return: {r:?}"), () = tokio::task::yield_now() => {} }
    let active = records(&env);
    assert_eq!(active.len(), 2, "same-pid calls need unique records");
    for record in active {
        assert_eq!(record["pid"], std::process::id());
        assert_eq!(record["session"], "pilot-session");
        assert_eq!(record["orchestrator"], false);
        assert_eq!(record["started_at"], json!(spec_instant()));
        assert_eq!(record["max_s"], 30);
        assert_eq!(record["watch_ids"], json!(["main"]));
        assert_eq!(record["watches"], json!([{"watch_id": "main", "kind": "hold", "expect_s": null, "inputs": ""}]));
    }
    drop(a);
    assert_eq!(records(&env).len(), 1, "dropping a future unregisters it");
    b.await.unwrap();
    assert!(records(&env).is_empty(), "normal return unregisters it");
    let (send, recv) = tokio::sync::oneshot::channel::<()>();
    let mut call = Box::pin(env.vigil.keepalive_cancellable(request(Some(false)), &crate::keepalive::NoProgress, async { let _ = recv.await; }));
    tokio::select! { biased; r = &mut call => panic!("unexpected return: {r:?}"), () = tokio::task::yield_now() => {} }
    assert_eq!(records(&env).len(), 1);
    send.send(()).unwrap();
    assert!(call.await.unwrap().is_none());
    assert!(records(&env).is_empty(), "explicit cancellation unregisters it");
}

#[tokio::test(start_paused = true)]
async fn a_peer_arriving_during_the_call_is_seen_at_the_next_sense_pass() {
    // why: peers start while the main session is already waiting.
    // catches: scanning at entry only or adding a timer slower than sensing.
    // proves: CLAIM(peers-flag)
    let env = env();
    hold(&env, "main").await;
    let peer = Peer::new();
    let start = tokio::time::Instant::now();
    let mut call = Box::pin(env.vigil.keepalive(request(Some(true))));
    tokio::select! { biased; r = &mut call => panic!("unexpected return: {r:?}"), () = tokio::task::yield_now() => {} }
    fixture(&env, peer.pid(), Some("pilot-session"), true, &["late"]);
    let answer = serde_json::to_value(call.await.unwrap()).unwrap();
    assert_eq!(answer["next"], "act on events");
    assert_eq!(answer["events"][0]["kind"], "foreign_orchestrator");
    assert_eq!(start.elapsed(), crate::keepalive::OUTBOX_POLL);
}

#[tokio::test(start_paused = true)]
async fn ordinary_watch_metadata_is_registered_and_an_error_unregisters_it() {
    // why: the registry covers watches as well as holds, even on failure.
    // catches: hardcoding hold kind; losing input or expect_s; error leaks.
    // proves: CLAIM(peers-registry)
    let env = env();
    let path = env.input_path("build.log");
    crate::testing::write_file(&path, "waiting\n");
    let mut create = crate::testing::change_request("main", &path, 30);
    create.expect_s = Some(120);
    env.vigil.create(create).await.unwrap();
    let mut call = Box::pin(env.vigil.keepalive(request(Some(false))));
    tokio::select! { biased; r = &mut call => panic!("unexpected return: {r:?}"), () = tokio::task::yield_now() => {} }
    let active = records(&env);
    assert_eq!(active.len(), 1);
    assert_eq!(active[0]["watches"], json!([{
        "watch_id": "main", "kind": "watch", "expect_s": 120,
        "inputs": format!("file:{}", path.display())
    }]));
    std::fs::remove_file(env.vigil.store().watch_path(&id("main"))).unwrap();
    assert!(call.await.is_err(), "watch disappeared while waiting");
    assert!(records(&env).is_empty(), "error paths unregister too");
}
