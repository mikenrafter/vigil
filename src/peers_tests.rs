//! Pilot proofs for SPEC "Orchestrator peers". Literal registry fixtures also
//! pin the reader independently of the production writer.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::hold::HoldRequest;
use crate::keepalive::KeepaliveRequest;
use crate::testing::{Env, ScriptedCreditProbe, TokioClock, credit_unknown, default_config, spec_instant};
use crate::watch::WatchId;

fn env() -> Env {
    let mut env = Env::with_clock(Arc::new(TokioClock::new(spec_instant())));
    // Real `uw` I/O races paused time and changes the peer timing assertions.
    env.vigil = env.vigil.with_session_key(Some("pilot-session".into()))
        .with_credit_probe(ScriptedCreditProbe::new(credit_unknown(crate::credit::CreditSource::Transcripts)));
    env
}

fn id(name: &str) -> WatchId {
    WatchId::parse(name).unwrap()
}

async fn hold(env: &Env, name: &str) {
    env.vigil.hold(HoldRequest { id: Some(name.into()), ..HoldRequest::default() }).await.unwrap();
}

fn request(orchestrator: Option<bool>) -> KeepaliveRequest {
    KeepaliveRequest { watch_id: Some(id("main")), max_s: Some(30), orchestrator, confirm: None, session: false }
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
    let other = env.another_process().with_session_key(Some("pilot-session".into()))
        .with_credit_probe(ScriptedCreditProbe::new(credit_unknown(crate::credit::CreditSource::Transcripts)));
    let again = other.keepalive(request(Some(true))).await.unwrap();
    assert!(again.events.is_empty(), "raised once per record across calls");
    assert_eq!(again.waited_s, Some(30));
}

#[tokio::test(start_paused = true)]
async fn ordinary_peer_is_information_with_the_spec_wire_fields() {
    // why: legitimate subagent waits are visible without waking the main call.
    // catches: flagging all peers; missing fields; measuring the main's age.
    // proves: CLAIM(peers-info)
    let env = gated_env();
    hold(&env, "main").await;
    confirm(&env, "main");
    let peer = Peer::new();
    fixture(&env, peer.pid(), Some("pilot-session"), false, &["sub"]);
    let answer = serde_json::to_value(env.vigil.keepalive(request(Some(true))).await.unwrap()).unwrap();
    assert_eq!(answer["peers"], json!([{
        "watch_id": "sub", "kind": "hold", "mode": "plain", "expect_s": 120, "waited_s": 37, "inputs": "process:4242"
    }]));
    assert_eq!(answer["waited_s"], 30);
    assert_eq!(answer["next"], "call keepalive again");
    assert_eq!(answer["events"], json!([]));
}

#[tokio::test(start_paused = true)]
async fn own_pid_is_excluded_and_other_confirmed_holds_are_information() {
    // why: multiple main-session holds must never flag one another, but other
    //      confirmed holds must remain visible as information.
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
    assert_eq!(answer["peers"], json!([
        {"watch_id": "own-a", "kind": "hold", "mode": "orchestrator", "expect_s": 120, "waited_s": 37, "inputs": "process:4242"},
        {"watch_id": "own-b", "kind": "hold", "mode": "orchestrator", "expect_s": 120, "waited_s": 37, "inputs": "process:4242"}
    ]));
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
    let env = gated_env();
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
        assert_eq!(record["transport"], "cli");
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


fn gated_env() -> Env {
    let config = crate::config::Config { orchestrator_confirm: true, ..default_config() };
    let mut env = Env::with_clock_and_config(Arc::new(TokioClock::new(spec_instant())), config);
    env.vigil = env.vigil.with_session_key(Some("pilot-session".into()))
        .with_credit_probe(ScriptedCreditProbe::new(credit_unknown(crate::credit::CreditSource::Transcripts)));
    env
}

fn confirm(env: &Env, name: &str) {
    let watch = env.vigil.store().load(&id(name)).unwrap();
    env.vigil.mark_orchestrator_confirmed(&watch).unwrap();
}

#[tokio::test(start_paused = true)]
async fn fix1_registry_failure_never_costs_a_keepalive_its_envelope() {
    // why: review blocker, optional peer bookkeeping cannot break any wait mode.
    // catches: propagating either registration or directory scan errors.
    for mode in [Some(false), None, Some(true)] {
        let env = gated_env();
        hold(&env, "main").await;
        if mode == Some(true) { confirm(&env, "main"); }
        std::fs::write(env.root().join("calls"), b"not a directory").unwrap();
        let answer = env.vigil.keepalive(request(mode)).await.expect("registry is best effort");
        assert_eq!(answer.status, crate::watch::WatchStatus::Pending);
        assert_eq!(answer.next, crate::Next::CallKeepaliveAgain);
        assert_eq!(answer.waited_s, Some(30));
    }
}

#[tokio::test(start_paused = true)]
async fn fix1_receipt_failure_skips_one_peer_and_keeps_other_information() {
    // why: failure claiming an optional event must not abort scanning or waiting.
    // catches: propagating receipt errors or discarding the rest of the scan.
    let env = gated_env();
    hold(&env, "main").await;
    confirm(&env, "main");
    let rogue = Peer::new();
    let ordinary = Peer::new();
    fixture(&env, ordinary.pid(), Some("pilot-session"), false, &["ordinary"]);
    let mut call = Box::pin(env.vigil.keepalive(request(Some(true))));
    tokio::select! { biased; r = &mut call => panic!("unexpected return: {r:?}"), () = tokio::task::yield_now() => {} }
    assert!(records(&env).iter().any(|record| record["pid"] == std::process::id()), "registration succeeded before the write failure");
    let path = fixture(&env, rogue.pid(), Some("pilot-session"), true, &["rogue"]);
    use std::os::unix::fs::PermissionsExt;
    let dir = path.parent().unwrap();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let result = call.await;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let answer = result.expect("receipt failure is best effort");
    assert!(answer.events.is_empty());
    assert_eq!(answer.peers.len(), 1);
    assert_eq!(answer.peers[0].watch_id, id("ordinary"));
    assert_eq!(answer.waited_s, Some(30));
}

#[tokio::test(start_paused = true)]
async fn fix1_probe_a_mixed_no_id_call_registers_plain_and_gets_plain_window() {
    // why: a subagent shares the main's MCP server and covers its confirmed hold.
    // catches: any confirmed watch granting orchestrator mode to a mixed call.
    let projects = tempfile::tempdir().unwrap();
    let config = crate::config::Config {
        orchestrator_confirm: true,
        claude_projects_dir: Some(projects.path().into()),
        ..default_config()
    };
    crate::testing::write_transcript(projects.path(), "p", "pilot-session", &[
        crate::testing::assistant_line(0, 500), crate::testing::assistant_line(900, 0),
    ]);
    let mut env = Env::with_clock_and_config(Arc::new(TokioClock::new(spec_instant())), config);
    env.vigil = env.vigil.with_session_key(Some("pilot-session".into())).with_agent_env(crate::cache::AgentEnv {
        claude_session_id: Some("pilot-session".into()), codex: false, antigravity: false,
    });
    hold(&env, "main").await;
    confirm(&env, "main");
    // Positive control: this same session really can get the 3000 s window.
    let mut main = Box::pin(env.vigil.keepalive(KeepaliveRequest {
        watch_id: Some(id("main")), ..KeepaliveRequest::default()
    }));
    tokio::select! { biased; r = &mut main => panic!("unexpected return: {r:?}"), () = tokio::task::yield_now() => {} }
    assert_eq!(records(&env)[0]["max_s"], 3000);
    drop(main);
    let path = env.input_path("build.log");
    crate::testing::write_file(&path, "waiting\n");
    env.vigil.create(crate::testing::change_request("plain", &path, 30)).await.unwrap();
    let mut call = Box::pin(env.vigil.keepalive(KeepaliveRequest::default()));
    tokio::select! { biased; r = &mut call => panic!("unexpected return: {r:?}"), () = tokio::task::yield_now() => {} }
    let active = records(&env);
    assert_eq!(active.len(), 1);
    assert_eq!(active[0]["orchestrator"], false);
    let answer = call.await.unwrap();
    assert_eq!(answer.max_s, Some(270));
    assert_eq!(answer.waited_s, Some(270));
}

#[tokio::test(start_paused = true)]
async fn fix1_confirmed_other_orchestrator_is_visible_with_mode() {
    // why: probe C, confirmation cannot make a caller on other watches invisible.
    // catches: dropping every all-confirmed record; missing informational mode.
    let env = gated_env();
    for name in ["main", "other"] { hold(&env, name).await; confirm(&env, name); }
    let peer = Peer::new();
    fixture(&env, peer.pid(), Some("pilot-session"), true, &["main", "other"]);
    let answer = serde_json::to_value(env.vigil.keepalive(request(Some(true))).await.unwrap()).unwrap();
    assert_eq!(answer["events"], json!([]));
    assert_eq!(answer["next"], "call keepalive again");
    assert_eq!(answer["waited_s"], 30);
    assert_eq!(answer["peers"], json!([{
        "watch_id": "other", "kind": "hold", "mode": "orchestrator", "expect_s": 120,
        "waited_s": 37, "inputs": "process:4242"
    }]));
}

#[tokio::test(start_paused = true)]
async fn fix1_foreign_summary_names_transport_and_mcp_stop_target() {
    // why: the MCP pid is shared, so stopping it would kill the session's server.
    // catches: a bare pid instruction or no transport distinction.
    for transport in ["cli", "mcp"] {
        let env = gated_env();
        hold(&env, "main").await;
        confirm(&env, "main");
        let peer = Peer::new();
        let path = fixture(&env, peer.pid(), Some("pilot-session"), true, &["rogue"]);
        let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        record["transport"] = json!(transport);
        crate::store::write_atomic(&path, &record).unwrap();
        let answer = env.vigil.keepalive(request(Some(true))).await.unwrap();
        assert_eq!(answer.events.len(), 1);
        let summary = &answer.events[0].summary;
        assert!(summary.contains(transport), "{summary}");
        if transport == "mcp" {
            assert!(summary.contains("this pid is the session's vigil MCP server; stop the agent, not the pid"), "{summary}");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn fix1_terminal_primary_keeps_watch_ended_when_a_peer_event_arrives() {
    // why: terminal next is the loop's stop instruction, even with extra events.
    // catches: the unconditional peer override after envelope construction.
    let env = gated_env();
    hold(&env, "main").await;
    confirm(&env, "main");
    env.vigil.cancel(&id("main")).await.unwrap();
    let peer = Peer::new();
    fixture(&env, peer.pid(), Some("pilot-session"), true, &["rogue"]);
    let answer = serde_json::to_value(env.vigil.keepalive(request(Some(true))).await.unwrap()).unwrap();
    assert_eq!(answer["status"], "cancelled");
    assert_eq!(answer["next"], "watch ended; create a new watch if still needed");
    assert!(answer["events"].as_array().unwrap().iter().any(|event| event["kind"] == "foreign_orchestrator"));
}

#[test]
fn antigravity_does_not_change_hold_ownership() {
    // Notice-only identity must not override the existing T3 owner key.
    assert_eq!(crate::peers::session_key_from_lookup(|name| match name {
        "ANTIGRAVITY_CONVERSATION_ID" => Some("conversation".into()),
        "T3CODE_THREAD_ID" => Some("thread".into()),
        _ => None,
    }), Some("thread".into()));
}

// ---- process-tree signal for Antigravity (neon-mcp-bound) ----

/// A fake procfs in a tempdir: `<root>/<pid>/stat` for each `(pid, comm, ppid)`,
/// in the kernel's format `pid (comm) S ppid ...`. Never the real /proc.
fn fake_proc(entries: &[(u32, &str, u32)]) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for (pid, comm, parent) in entries {
        let dir = root.path().join(pid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stat"), format!("{pid} ({comm}) S {parent} {pid} {pid} 0 -1 4194560 100 0 0 0 0 0 0 0 20 0 1 0 1 1000 100 18446744073709551615\n")).unwrap();
    }
    root
}

#[test]
fn an_agy_parent_or_ancestor_is_the_antigravity_process_tree_signal() {
    // why: agy 1.5.13 spawns `vigil mcp` with no ANTIGRAVITY_* variable, but
    //      the child's parent comm is `agy` (evidence-neon-mcp-live.md, run 2;
    //      `agy acp` shell children have the same parent, report-neon-ttl.md).
    //      Walk up from the parent skipping unknown names (e.g. sh, bash, node);
    //      the FIRST known agent name decides (shared with ancestor_session:
    //      claude, .claude-wrapped, codex, .codex-wrapped, agy, .agy-wrapped).
    //      True only when that nearest known agent is agy / .agy-wrapped, so a
    //      Claude or Codex session launched under agy is not throttled by the
    //      tree signal (env markers are still ORed separately), while T3's
    //      node -> agy -> sh -> vigil chain is true. The process itself is not
    //      an ancestor.
    // catches: reading only the env, reading only the direct parent, walking
    //          past a nearer claude/codex ancestor, matching the process itself,
    //          an exact-prefix match such as `agyx`, or a case-insensitive
    //          match on `Agy`.
    // proves: CLAIM(neon-mcp-bound)
    let signal = |entries: &[(u32, &str, u32)]| {
        let root = fake_proc(entries);
        crate::peers::antigravity_in_ancestry(root.path(), entries[0].0)
    };
    assert!(signal(&[(100, "vigil", 90), (90, "agy", 1), (1, "systemd", 0)]), "direct parent is agy (vigil mcp)");
    assert!(signal(&[(100, "vigil", 95), (95, "sh", 90), (90, "agy", 1), (1, "systemd", 0)]), "a wrapper between vigil and agy");
    assert!(signal(&[(100, "vigil", 95), (95, "sh", 90), (90, "agy", 80), (80, "node", 1), (1, "systemd", 0)]), "T3 chain: node -> agy -> sh -> vigil");
    assert!(signal(&[(100, "vigil", 95), (95, "bash", 90), (90, ".agy-wrapped", 1), (1, "systemd", 0)]), "the nix wrapper name");
    assert!(signal(&[(100, "vigil", 95), (95, "agy", 90), (90, "claude", 1), (1, "systemd", 0)]), "agy started under claude: nearest known agent is agy");
    assert!(signal(&[(100, "vigil", 95), (95, "sh", 90), (90, ".agy-wrapped", 80), (80, "codex", 1), (1, "systemd", 0)]), ".agy-wrapped under codex");
    assert!(!signal(&[(100, "vigil", 95), (95, "claude", 90), (90, "agy", 1), (1, "systemd", 0)]), "claude under agy: nearest known agent is claude");
    assert!(!signal(&[(100, "vigil", 95), (95, ".claude-wrapped", 90), (90, "agy", 1), (1, "systemd", 0)]), ".claude-wrapped under agy");
    assert!(!signal(&[(100, "vigil", 95), (95, "codex", 90), (90, "agy", 1), (1, "systemd", 0)]), "codex under agy: nearest known agent is codex");
    assert!(!signal(&[(100, "vigil", 95), (95, ".codex-wrapped", 90), (90, "agy", 1), (1, "systemd", 0)]), ".codex-wrapped under agy");
    assert!(!signal(&[(100, "vigil", 90), (90, "claude", 1), (1, "systemd", 0)]), "claude alone is not Antigravity");
    assert!(!signal(&[(100, "vigil", 90), (90, "codex", 1), (1, "systemd", 0)]), "codex alone is not Antigravity");
    assert!(!signal(&[(100, "vigil", 90), (90, "bash", 1), (1, "systemd", 0)]), "no agent at all");
    assert!(!signal(&[(100, "vigil", 90), (90, "agyx", 1), (1, "systemd", 0)]), "only the exact name counts");
    assert!(!signal(&[(100, "vigil", 90), (90, "Agy", 1), (1, "systemd", 0)]), "names are case-sensitive");
    assert!(!signal(&[(100, "vigil", 90), (90, "my-agy", 1), (1, "systemd", 0)]), "a suffix match is not enough");
    assert!(!signal(&[(100, "agy", 1), (1, "systemd", 0)]), "the process itself is not an ancestor");
    assert!(!signal(&[(100, "agy", 100)]), "self-parent cycle with comm agy terminates with no signal");
    assert!(!signal(&[(100, "vigil", 100)]), "a self-parent cycle terminates with no signal");
    assert!(!signal(&[(100, "vigil", 90), (90, "bash", 100)]), "a two-step cycle terminates with no signal");
    assert!(!signal(&[(100, "vigil", 90)]), "a parent whose stat file is missing ends the walk with no signal");
}

#[test]
fn the_ancestry_walk_never_reads_another_process_environment() {
    // why: invariant: vigil never reads another process's environment, only
    //      names and parent pids. Every process in a tree with NO agy gets an
    //      `environ` and `cmdline` that SAY Antigravity; a detection that
    //      looked there would find the marker and report true. Control: the
    //      same files with an agy comm and empty environ files still report
    //      true, so the false result comes from comm names alone.
    // catches: a detection that reads /proc/<pid>/environ or cmdline.
    // proves: CLAIM(neon-mcp-bound)
    let plant = |root: &tempfile::TempDir, pids: &[u32], environ: &[u8]| {
        for pid in pids {
            std::fs::write(root.path().join(pid.to_string()).join("environ"), environ).unwrap();
            std::fs::write(root.path().join(pid.to_string()).join("cmdline"), b"/usr/bin/agy\0acp\0").unwrap();
        }
    };
    let decoy = fake_proc(&[(100, "vigil", 90), (90, "bash", 1), (1, "systemd", 0)]);
    plant(&decoy, &[100, 90, 1], b"ANTIGRAVITY_AGENT=1\0ANTIGRAVITY_CONVERSATION_ID=c\0");
    assert!(!crate::peers::antigravity_in_ancestry(decoy.path(), 100));

    let real = fake_proc(&[(100, "vigil", 90), (90, "agy", 1), (1, "systemd", 0)]);
    plant(&real, &[100, 90, 1], b"");
    assert!(crate::peers::antigravity_in_ancestry(real.path(), 100));
}

#[test]
fn an_agy_ancestor_does_not_change_session_keys_for_claude_or_codex() {
    // why: `ancestor:<pid>` keys (hold ownership, notices) name the nearest
    //      claude/codex process. The Antigravity signal is separate: agy must
    //      not become a session key, and a claude chain keeps resolving to
    //      its claude pid whether or not agy sits above or below it.
    // catches: adding `agy` to the names ancestor_session matches, which
    //          would give a vigil under agy an `ancestor:<agy pid>` owner.
    // proves: CLAIM(peers-ancestor-keys)
    let agy_only = fake_proc(&[(100, "vigil", 90), (90, "agy", 1), (1, "systemd", 0)]);
    assert_eq!(crate::peers::ancestor_session(agy_only.path(), 100), None);
    let claude_chain = fake_proc(&[(100, "vigil", 95), (95, "bash", 90), (90, "claude", 80), (80, "agy", 1), (1, "systemd", 0)]);
    assert_eq!(crate::peers::ancestor_session(claude_chain.path(), 100), Some("ancestor:90".into()));
    let codex_chain = fake_proc(&[(100, "vigil", 90), (90, "codex", 80), (80, "agy", 1), (1, "systemd", 0)]);
    assert_eq!(crate::peers::ancestor_session(codex_chain.path(), 100), Some("ancestor:90".into()));
}
