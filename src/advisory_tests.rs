//! Engine-level tests for advisories: what `Vigil` puts on envelopes and what
//! `Vigil::advice` returns (SPEC "Advisories: delivery"). The pure parts are
//! tested beside their code in `advisory*.rs`; the binary and MCP surfaces in
//! `tests/cli_e2e.rs` and `tests/mcp_e2e.rs`.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::keepalive::KeepaliveRequest;
use crate::testing::*;
use crate::Vigil;

const PROBE: &str = "cmd:probe-cmd";

const ADVISORIES: &str = r#"
[advisories.mem]
probe = { kind = "cmd", argv = ["probe-cmd"] }
extract = { name = "free", json = "free" }
condition = "free < floor"
message_when_true = "Low: {free} (< {floor})"
message_when_false = "Plenty: {free} (>= {floor})"
cache_s = 600
[advisories.mem.params]
floor = 20
"#;

struct Rig {
    _config_dir: tempfile::TempDir,
    file: PathBuf,
    env: Env,
}

fn rig_with(content: Option<&str>) -> Rig {
    let config_dir = tempfile::tempdir().unwrap();
    let file = config_dir.path().join("vigil/advisories.toml");
    if let Some(content) = content {
        write_file(&file, content);
    }
    let mut config = default_config();
    config.advisories_file = Some(file.clone());
    let env = Env::with_clock_and_config(Arc::new(TokioClock::new(spec_instant())), config);
    env.reader.set_text(PROBE, "exit=0\n{\"free\": 15}");
    Rig { _config_dir: config_dir, file, env }
}

fn rig() -> Rig {
    rig_with(Some(ADVISORIES))
}

async fn fired_watch(env: &Env, name: &str) -> crate::WatchId {
    let path = env.input_path(&format!("{name}.log"));
    write_file(&path, "old\n");
    let id = env.vigil.create(change_request(name, &path, 20)).await.unwrap().watch_id;
    append_file(&path, "new\n");
    id
}

fn call(id: &crate::WatchId) -> KeepaliveRequest {
    KeepaliveRequest { watch_id: Some(id.clone()), max_s: Some(30), orchestrator: None, confirm: None, session: false }
}

fn wire(envelope: &crate::Envelope) -> Value {
    serde_json::to_value(envelope).unwrap()
}

fn messages(envelope: &Value) -> Vec<String> {
    envelope["advice"].as_array().unwrap_or_else(|| panic!("no advice list: {envelope}")).iter().map(|a| a["message"].as_str().unwrap().to_string()).collect()
}

#[tokio::test(start_paused = true)]
async fn a_keepalive_return_carries_the_current_advice() {
    // why: owner: "every keepalive/wait envelope carries an advice list (id +
    //      message)". One advisory, probe says 15, floor 20 -> the true message.
    //      Both a fired return and a quiet (pending) return have it.
    // proves: CLAIM(advisory-delivery)
    let rig = rig();
    let id = fired_watch(&rig.env, "w").await;
    let fired = rig.env.vigil.keepalive_cancellable(call(&id), &crate::keepalive::NoProgress, std::future::pending::<()>()).await.unwrap().unwrap();
    assert_eq!(fired.status, crate::WatchStatus::Fired);
    let advice = &wire(&fired)["advice"];
    assert_eq!(advice[0]["id"], "mem");
    assert_eq!(advice[0]["message"], "Low: 15 (< 20)");
    assert_eq!(advice[0]["kind"], "advice");

    let quiet_path = rig.env.input_path("q.log");
    write_file(&quiet_path, "old\n");
    let quiet = rig.env.vigil.create(change_request("q", &quiet_path, 20)).await.unwrap().watch_id;
    let pending = rig.env.vigil.keepalive_cancellable(call(&quiet), &crate::keepalive::NoProgress, std::future::pending::<()>()).await.unwrap().unwrap();
    assert_eq!(pending.status, crate::WatchStatus::Pending);
    assert_eq!(messages(&wire(&pending)), ["Low: 15 (< 20)"]);
}

#[tokio::test(start_paused = true)]
async fn status_and_cancel_do_not_carry_advice() {
    // why: like `cache` and `clock`, advice rides on blocking returns only; a
    //      status poll must stay cheap and must not run probes.
    // proves: CLAIM(advisory-delivery)
    let rig = rig();
    let path = rig.env.input_path("s.log");
    write_file(&path, "old\n");
    let id = rig.env.vigil.create(change_request("s", &path, 20)).await.unwrap().watch_id;
    let status = rig.env.vigil.status(&id).await.unwrap();
    assert!(wire(&status.envelope).get("advice").is_none());
    let cancelled = rig.env.vigil.cancel(&id).await.unwrap();
    assert!(wire(&cancelled).get("advice").is_none());
    assert_eq!(rig.env.reader.started_count(PROBE), 0);
}

#[tokio::test(start_paused = true)]
async fn with_advisories_off_there_is_no_advice_key_at_all() {
    // why: the sandboxes of every other test (and a deployment that sets
    //      VIGIL_ADVISORIES_FILE=off) must see byte-identical envelopes to
    //      before this feature.
    // proves: CLAIM(advisory-delivery), CLAIM(advisory-file)
    let env = Env::with_clock(Arc::new(TokioClock::new(spec_instant())));
    let path = env.input_path("w.log");
    write_file(&path, "old\n");
    let id = env.vigil.create(change_request("w", &path, 20)).await.unwrap().watch_id;
    append_file(&path, "new\n");
    let envelope = env.vigil.keepalive_cancellable(call(&id), &crate::keepalive::NoProgress, std::future::pending::<()>()).await.unwrap().unwrap();
    assert!(wire(&envelope).get("advice").is_none());
    assert_eq!(env.vigil.advice().await, Err(crate::AdvisoryError::Disabled));
    assert_eq!(env.vigil.advisory_store().unwrap_err(), crate::AdvisoryError::Disabled);
}

#[tokio::test(start_paused = true)]
async fn a_file_with_no_advisories_gives_an_empty_list_not_a_missing_key() {
    // why: "advisories on, nothing to say" and "advisories off" differ; an agent
    //      or hook can tell the first from the second.
    // proves: CLAIM(advisory-delivery)
    let rig = rig_with(Some("# nothing\n"));
    let id = fired_watch(&rig.env, "w").await;
    let envelope = rig.env.vigil.keepalive_cancellable(call(&id), &crate::keepalive::NoProgress, std::future::pending::<()>()).await.unwrap().unwrap();
    assert_eq!(wire(&envelope)["advice"], json!([]));
}

#[tokio::test(start_paused = true)]
async fn an_unreadable_file_is_one_explicit_unavailable_entry() {
    // why: never silence: a file that is not TOML must show on the envelope
    //      as an entry that names the file, while the keepalive result itself
    //      is unharmed.
    // proves: CLAIM(advisory-unavailable)
    let rig = rig_with(Some("this is [not toml"));
    let id = fired_watch(&rig.env, "w").await;
    let envelope = rig.env.vigil.keepalive_cancellable(call(&id), &crate::keepalive::NoProgress, std::future::pending::<()>()).await.unwrap().unwrap();
    assert_eq!(envelope.status, crate::WatchStatus::Fired);
    let advice = &wire(&envelope)["advice"];
    assert_eq!(advice.as_array().unwrap().len(), 1);
    assert_eq!(advice[0]["id"], "advisories");
    assert_eq!(advice[0]["kind"], "unavailable");
    let message = advice[0]["message"].as_str().unwrap();
    assert!(message.starts_with("advisory advisories unavailable: ") && message.contains("advisories.toml"), "{message}");
    assert!(rig.env.vigil.advice().await.is_err(), "and `Vigil::advice` reports the error to the CLI");
}

#[tokio::test(start_paused = true)]
async fn a_threshold_change_shows_on_the_very_next_return_without_a_new_probe() {
    // why: owner: `vigil advisory set boat-budget threshold=15` "changes the
    //      threshold immediately". Probe says 15. floor 20 -> Low. Set floor=10
    //      through the store (as the CLI/MCP do): the next keepalive says
    //      Plenty, and the probe ran once in total.
    // proves: CLAIM(advisory-immediate), CLAIM(advisory-reread)
    let rig = rig();
    let first = fired_watch(&rig.env, "a").await;
    let one = rig.env.vigil.keepalive_cancellable(call(&first), &crate::keepalive::NoProgress, std::future::pending::<()>()).await.unwrap().unwrap();
    assert_eq!(messages(&wire(&one)), ["Low: 15 (< 20)"]);
    rig.env.vigil.advisory_store().unwrap().set("mem", &["floor=10".to_string()]).unwrap();
    let second = fired_watch(&rig.env, "b").await;
    let two = rig.env.vigil.keepalive_cancellable(call(&second), &crate::keepalive::NoProgress, std::future::pending::<()>()).await.unwrap().unwrap();
    assert_eq!(messages(&wire(&two)), ["Plenty: 15 (>= 10)"]);
    assert_eq!(rig.env.reader.started_count(PROBE), 1);
    assert_eq!(rig.env.vigil.advice().await.unwrap()[0].message, "Plenty: 15 (>= 10)");
}

#[tokio::test(start_paused = true)]
async fn a_hand_edited_file_is_picked_up_by_a_second_process() {
    // why: "re-read on every call": a second Vigil over the same state and
    //      file sees an edit made after the first one answered.
    // proves: CLAIM(advisory-reread)
    let rig = rig();
    assert_eq!(rig.env.vigil.advice().await.unwrap()[0].message, "Low: 15 (< 20)");
    write_file(&rig.file, &ADVISORIES.replace("floor = 20", "floor = 5"));
    let other: Vigil = rig.env.another_process();
    assert_eq!(other.advice().await.unwrap()[0].message, "Plenty: 15 (>= 5)");
}

#[tokio::test(start_paused = true)]
async fn the_probe_cache_lives_under_the_state_root() {
    // why: SPEC "State": vigil writes only under $XDG_STATE_HOME/vigil (and the
    //      advisories file under the config home). The cache must not go
    //      elsewhere.
    // proves: CLAIM(advisory-cache)
    let rig = rig();
    rig.env.vigil.advice().await.unwrap();
    let cache = rig.env.root().join("advisory-cache/mem.json");
    assert!(cache.is_file(), "{}", cache.display());
    assert_eq!(rig.env.vigil.advisory_cache_dir(), rig.env.root().join("advisory-cache"));
}
