//! Engine-level tests for surviving a full disk (specs/disk-full.md, claims
//! DF1-DF7, with the amendments DF1a, DF3a, DF3b, DF4a, DF5a, DF5b of
//! "Amendments after red"). Cross-cutting on purpose: one scenario drives the store, the tick,
//! `keepalive`, `create`, `cancel` and the envelope together, so it lives in
//! one module instead of being split across four. Store-only claims are tested
//! beside the store (`store.rs`), the seam beside the seam (`fault.rs`), and
//! the process boundary in `tests/cli_e2e.rs` / `tests/mcp_e2e.rs`.
//!
//! The expected wire shapes are copied from the spec, not from any code:
//!
//! * failed write:  `"persist": {"ok": false, "kind": "disk_full" | "io",
//!   "error": "<message>", "path": "<file>"}` plus `"redelivery": true` only
//!   when the envelope has events or a terminal status that was not saved
//!   (DF2, DF3a);
//! * reserve used:  `"persist": {"ok": true, "used_reserve": true,
//!   "kind": "disk_full"}` (DF4);
//! * clean write:   no `persist` field at all (DF2).
//!
//! The envelope is read as JSON (`serde_json::Value`) because the wire format
//! is the claim; no Rust type for `persist` is assumed by the red phase.
//!
//! Every scenario runs through [`scenario`]: paused tokio time (instant
//! sleeps) plus a real-time watchdog. A tick that fails to save and is then
//! retried in a hot loop never lets virtual time advance, so an ordinary
//! `tokio::time::timeout` could not stop it; the watchdog fails the test
//! instead of hanging the run.

use std::future::Future;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::engine::Vigil;
use crate::envelope::{Envelope, Next};
use crate::error::VigilError;
use crate::fault::{FaultPlan, FaultRule, When, WriteKind};
use crate::keepalive::KeepaliveRequest;
use crate::testing::*;
use crate::watch::{WatchId, WatchStatus};

const WATCHDOG: Duration = Duration::from_secs(20);

/// Run `body` on a paused-time runtime in its own thread; fail (not hang) if
/// it has not finished after [`WATCHDOG`] of real time.
fn scenario<F>(body: impl FnOnce() -> F + Send + 'static)
where
    F: Future<Output = ()>,
{
    let (done, finished) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().unwrap();
            runtime.block_on(body());
        }));
        let _ = done.send(());
        if let Err(panic) = run {
            std::panic::resume_unwind(panic);
        }
    });
    match finished.recv_timeout(WATCHDOG) {
        Ok(()) => {
            if let Err(panic) = worker.join() {
                std::panic::resume_unwind(panic);
            }
        }
        Err(_) => panic!("no result after {WATCHDOG:?} of real time: the call is spinning (a failed save must not be retried in a hot loop)"),
    }
}

fn paused_env() -> Env {
    Env::with_clock(Arc::new(TokioClock::new(spec_instant())))
}

fn rule(kind: WriteKind, errno: i32, when: When) -> FaultRule {
    FaultRule { kind, errno, when }
}

/// Every watch write and every reserve creation fails: a disk that is full
/// and stays full.
fn disk_stays_full(errno: i32) -> Vec<FaultRule> {
    vec![rule(WriteKind::Watch, errno, When::Every), rule(WriteKind::Reserve, errno, When::Every)]
}

/// Full while `<root>/.reserve` exists, no room for a new reserve: only the
/// retry after deleting the reserve can succeed (DF4).
fn disk_has_only_the_reserve_to_give(root: &Path, errno: i32) -> Vec<FaultRule> {
    vec![
        rule(WriteKind::Watch, errno, When::WhileExists(root.join(".reserve"))),
        rule(WriteKind::Reserve, errno, When::Every),
    ]
}

fn by_id(id: &WatchId, max_s: Option<u64>) -> KeepaliveRequest {
    KeepaliveRequest { watch_id: Some(id.clone()), max_s }
}

fn no_id() -> KeepaliveRequest {
    KeepaliveRequest { watch_id: None, max_s: None }
}

fn wire(envelope: &Envelope) -> Value {
    serde_json::to_value(envelope).unwrap()
}

fn persist_of(envelope: &Value) -> &Value {
    envelope.get("persist").unwrap_or_else(|| panic!("the envelope has no `persist` field: {envelope}"))
}

/// The failed-write note, field by field against the spec (DF2, DF3a). The
/// key `redelivery` is `true` when something is at risk (the envelope has
/// events, or a terminal status that was not saved) and ABSENT otherwise.
fn assert_failed_persist(envelope: &Value, kind: &str, os_text: &str, file: &Path, redelivery: bool) {
    let persist = persist_of(envelope);
    let object = persist.as_object().unwrap_or_else(|| panic!("persist is not an object: {persist}"));
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort();
    if redelivery {
        assert_eq!(keys, ["error", "kind", "ok", "path", "redelivery"], "{persist}");
        assert_eq!(persist["redelivery"], true, "{persist}");
    } else {
        assert_eq!(keys, ["error", "kind", "ok", "path"], "nothing is at risk, so no `redelivery` key: {persist}");
    }
    assert_eq!(persist["ok"], false, "{persist}");
    assert_eq!(persist["kind"], kind, "{persist}");
    assert_eq!(persist["path"], file.to_str().unwrap(), "{persist}");
    let error = persist["error"].as_str().unwrap_or_else(|| panic!("error is not a string: {persist}"));
    assert!(error.contains(os_text), "error {error:?} should carry the OS message {os_text:?}");
}

/// A quiet `cmd` watch "q" (interval 20 s, deadline 3600 s) whose probe
/// always answers `ok`: its ticks find nothing, and every tick is one scripted
/// read of `cmd:probe`, which makes ticks countable. The probe's 1 s read
/// timeout keeps the tick reserve (X4: slowest read + 1 s = 2 s) small, so a
/// healthy call ticks again as soon as the next interval is due.
async fn quiet_probe_watch(env: &Env) -> WatchId {
    env.reader.set_text("cmd:probe", "ok");
    let mut request = change_request("q", Path::new("/unused"), 20);
    request.inputs = vec![crate::watch::Input::Cmd { argv: vec!["probe".into()], timeout_s: Some(1.0), cwd: None }];
    env.vigil.create(request).await.unwrap().watch_id
}

fn identity(path: &Path) -> (u64, u64) {
    let metadata = std::fs::metadata(path).unwrap();
    (metadata.dev(), metadata.ino())
}

fn tmp_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(tmp_files(&path));
        } else if path.to_string_lossy().ends_with(".tmp") {
            found.push(path);
        }
    }
    found
}

/// A pending `fire = change` watch "w" (interval 20 s) whose input has
/// changed since the baseline, created by `env.vigil`: the next tick fires.
struct Fixture {
    env: Env,
    id: WatchId,
}

impl Fixture {
    async fn new() -> Fixture {
        let env = paused_env();
        let input = env.input_path("w.log");
        write_file(&input, "old\n");
        let id = env.vigil.create(change_request("w", &input, 20)).await.unwrap().watch_id;
        append_file(&input, "boom\n");
        Fixture { env, id }
    }
    fn watch_file(&self) -> PathBuf {
        self.env.root().join("watches").join("w.json")
    }
    fn watch_bytes(&self) -> Vec<u8> {
        std::fs::read(self.watch_file()).unwrap()
    }
    fn reserve(&self) -> PathBuf {
        self.env.root().join(".reserve")
    }
    /// Another process over the same root whose store writes fail by `rules`.
    fn faulted(&self, rules: Vec<FaultRule>) -> (Vigil, FaultPlan) {
        let plan = FaultPlan::from_rules(rules);
        (self.env.another_process().with_write_faults(plan.clone()), plan)
    }
}

// ---- DF1 + DF2: the answer survives, and the failure is visible ----------

#[test]
fn a_tick_whose_save_fails_returns_the_same_envelope_a_healthy_tick_returns() {
    // why: DF1, the incident: on 2026-11-10 `keepalive` exited 1 with "No space
    //      left on device" and no envelope, so the agent lost the tick's
    //      events. Sensing worked; only the save failed. Two identical worlds:
    //      one healthy, one whose every write fails. The faulted call must
    //      return `Ok`, with the same events, digest, status, next, tick as
    //      the healthy one (everything but `persist`), and those values also
    //      match the hand-derived ones: tick 1 fires event "w:1:0" (W1) with
    //      summary "boom" (the newest line, X2).
    // proves: CLAIM(DF1)
    scenario(|| async {
        let healthy = Fixture::new().await;
        let expected = wire(&healthy.env.another_process().keepalive(by_id(&healthy.id, None)).await.unwrap());

        let broken = Fixture::new().await;
        let (vigil, _) = broken.faulted(disk_stays_full(28));
        let envelope = vigil.keepalive(by_id(&broken.id, None)).await.expect("a failed save must not fail the call");

        assert_eq!(persist_of(&wire(&envelope))["ok"], false, "precondition: the save really failed in this world");
        assert_eq!(envelope.status, WatchStatus::Fired);
        assert_eq!(envelope.next, Next::ActOnEvents);
        assert_eq!((envelope.tick, envelope.events.len()), (1, 1));
        assert_eq!((envelope.events[0].event_id.as_str(), envelope.events[0].summary.as_str()), ("w:1:0", "boom"));
        assert!(envelope.digest.contains("boom"), "{:?}", envelope.digest);

        // Same as the healthy world once the sandbox paths are normalised.
        let mut got = wire(&envelope);
        got.as_object_mut().unwrap().remove("persist");
        let got_text = got.to_string().replace(broken.env.dir.path().to_str().unwrap(), "<dir>");
        let want_text = expected.to_string().replace(healthy.env.dir.path().to_str().unwrap(), "<dir>");
        assert_eq!(got_text, want_text, "the faulted envelope differs from the healthy one in more than `persist`");
    });
}

#[test]
fn the_failed_save_is_described_in_persist_with_the_kind_the_errno_decides() {
    // why: DF2: `persist = {ok:false, kind, error, path}`; `kind` is
    //      `disk_full` for ENOSPC (28) and EDQUOT (122), `io` for anything
    //      else. The OS texts are glibc's strerror, written out here (not
    //      produced by the code under test). `path` is the watch file.
    //      Reserve creation fails too, so a disk_full write really is lost.
    //      The tick fires an event that is not saved, so DF3a says
    //      `redelivery: true`.
    // proves: CLAIM(DF2), CLAIM(DF3a)
    let table = [
        (28, "disk_full", "No space left on device"),
        (122, "disk_full", "Disk quota exceeded"),
        (5, "io", "Input/output error"),
        (13, "io", "Permission denied"),
    ];
    for (errno, kind, os_text) in table {
        scenario(move || async move {
            let fixture = Fixture::new().await;
            let (vigil, _) = fixture.faulted(disk_stays_full(errno));
            let envelope = wire(&vigil.keepalive(by_id(&fixture.id, None)).await.unwrap_or_else(|e| panic!("errno {errno}: {e}")));
            assert_failed_persist(&envelope, kind, os_text, &fixture.watch_file(), true);
        });
    }
}

#[test]
fn a_clean_write_adds_no_persist_field_to_any_envelope() {
    // why: DF2: "A successful write carries no `persist` field (wire format
    //      unchanged when all is well)". A quiet envelope is a few hundred
    //      bytes by design (SPEC "Envelope"). Control for every other test
    //      here: fired, pending and cancelled envelopes of a healthy disk.
    // proves: CLAIM(DF2)
    scenario(|| async {
        let fired = Fixture::new().await;
        let envelope = wire(&fired.env.another_process().keepalive(by_id(&fired.id, None)).await.unwrap());
        assert_eq!(envelope["status"], "fired");
        assert!(envelope.get("persist").is_none(), "{envelope}");

        let quiet = paused_env();
        write_file(&quiet.input_path("q.log"), "old\n");
        let id = quiet.vigil.create(change_request("q", &quiet.input_path("q.log"), 20)).await.unwrap().watch_id;
        let pending = wire(&quiet.vigil.keepalive(by_id(&id, Some(30))).await.unwrap());
        assert_eq!(pending["status"], "pending");
        assert!(pending.get("persist").is_none(), "{pending}");
        let cancelled = wire(&quiet.vigil.cancel(&id).await.unwrap());
        assert_eq!(cancelled["status"], "cancelled");
        assert!(cancelled.get("persist").is_none(), "{cancelled}");
    });
}

#[test]
fn a_quiet_tick_that_cannot_save_ends_the_call_at_once_and_is_not_ticked_again() {
    // why: DF1/DF2 + DF1a for the common case, a tick that found nothing. The
    //      save fails, so the persisted `last_tick_at` never advances and the
    //      watch looks due again at once. DF1a: "A tick whose save failed ends
    //      the call and returns its envelope (no in-memory re-ticking, no
    //      spin)". A 30 s call at interval 20 would tick at t=0 and t=20, so
    //      the healthy twin below reads 3 times (the baseline at create + two
    //      ticks); the faulted call must read exactly 2 (baseline + the one
    //      tick), must not have waited (`waited_s` 0 on paused time), and must
    //      say pending / "call keepalive again" with persist.ok = false. DF3a:
    //      nothing is at risk (no events, status not terminal), so the
    //      `redelivery` key is absent. The watchdog fails a spin.
    // proves: CLAIM(DF1), CLAIM(DF1a), CLAIM(DF2), CLAIM(DF3a)
    scenario(|| async {
        let healthy = paused_env();
        let healthy_id = quiet_probe_watch(&healthy).await;
        healthy.another_process().keepalive(by_id(&healthy_id, Some(30))).await.unwrap();
        assert_eq!(healthy.reader.started_count("cmd:probe"), 3, "control: a healthy 30 s call ticks at t=0 and t=20 after the baseline");

        let env = paused_env();
        let id = quiet_probe_watch(&env).await;
        let plan = FaultPlan::from_rules(disk_stays_full(28));
        let vigil = env.another_process().with_write_faults(plan);

        let envelope = vigil.keepalive(by_id(&id, Some(30))).await.expect("a failed save must not fail the call");
        let wire = wire(&envelope);

        assert_eq!((envelope.status, envelope.next), (WatchStatus::Pending, Next::CallKeepaliveAgain));
        assert_failed_persist(&wire, "disk_full", "No space left on device", &env.root().join("watches").join("q.json"), false);
        assert_eq!(env.reader.started_count("cmd:probe"), 2, "baseline + one tick: a tick whose save failed ends the call");
        assert_eq!(envelope.waited_s, Some(0), "the call returned without sleeping toward the next tick");
    });
}

#[test]
fn wait_sleeps_one_interval_after_a_pending_envelope_that_could_not_be_saved() {
    // why: DF1a: "`vigil wait`, after a pending envelope with persist.ok =
    //      false, sleeps one interval before calling again." `wait` loops
    //      `keepalive` until the watch finishes; a failed save ends each call
    //      at once and leaves the watch due, so without the sleep `wait` calls
    //      again in a hot loop (the watchdog fails that) and hammers a full
    //      disk. Quiet watch, interval 20 s, every write fails. `wait` runs
    //      for 50 s of virtual time, then is stopped: it must still be
    //      running (the watch is not finished). Ticks are scripted reads of
    //      `cmd:probe`: the baseline at create (t=0) and ticks at t=0, 20, 40
    //      (hand-derived: one call per interval, 0 + 20 k < 50 for k = 0..2),
    //      and each read started exactly that long after the first, so the
    //      sleep is one interval, not "some sleep". Each call saved (tried
    //      to) once, so at least 3 attempts to write a watch file.
    // proves: CLAIM(DF1a)
    scenario(|| async {
        let env = paused_env();
        let id = quiet_probe_watch(&env).await;
        let plan = FaultPlan::from_rules(disk_stays_full(28));
        let vigil = env.another_process().with_write_faults(plan.clone());

        let outcome = tokio::time::timeout(Duration::from_secs(50), vigil.wait(&id)).await;

        assert!(outcome.is_err(), "the watch cannot finish within 50 s, so wait must still be running: {outcome:?}");
        assert!(plan.attempts(WriteKind::Watch) >= 3, "precondition: each of the 3 calls tried to save and failed, got {}", plan.attempts(WriteKind::Watch));
        let reads = env.reader.reads();
        let starts: Vec<_> = reads.iter().filter(|read| read.label == "cmd:probe").map(|read| read.started).collect();
        let offsets: Vec<u64> = starts.iter().map(|start| start.duration_since(starts[0]).as_secs()).collect();
        assert_eq!(offsets, [0, 0, 20, 40], "baseline, then one tick per 20 s interval");
    });
}

// ---- DF3: at-least-once ---------------------------------------------------

#[test]
fn a_tick_that_could_not_save_does_not_advance_the_baseline_and_its_events_come_again() {
    // why: DF3: "A tick whose write failed does not advance the persisted
    //      baseline, so its events are reported again on the next successful
    //      tick." Same process, by id. Attempts 1-2 fail (the write and its
    //      retry), then the disk has room. Call 1: fired + persist.ok=false,
    //      and the file on disk is byte-identical (still pending, tick 0).
    //      Call 2: the same event again (same event_id, same summary), no
    //      persist field, and now the file says fired.
    // proves: CLAIM(DF3), CLAIM(DF6)
    scenario(|| async {
        let fixture = Fixture::new().await;
        let before = fixture.watch_bytes();
        let (vigil, _) = fixture.faulted(vec![rule(WriteKind::Watch, 28, When::Range { first: 1, last: Some(2) })]);

        let first = vigil.keepalive(by_id(&fixture.id, None)).await.expect("first call");
        assert_eq!(first.status, WatchStatus::Fired);
        assert_eq!(persist_of(&wire(&first))["ok"], false);
        assert_eq!(fixture.watch_bytes(), before, "the failed tick must not change the file");

        let second = vigil.keepalive(by_id(&fixture.id, None)).await.expect("second call");
        assert_eq!(second.status, WatchStatus::Fired);
        assert_eq!(second.events, first.events, "the same events, same event_id, delivered again");
        assert_eq!(second.events.len(), 1, "once per envelope, not duplicated");
        assert!(wire(&second).get("persist").is_none(), "this write succeeded: {}", wire(&second));
        let stored: Value = serde_json::from_slice(&fixture.watch_bytes()).unwrap();
        assert_eq!((stored["status"].as_str(), stored["tick"].as_u64()), (Some("fired"), Some(1)));
    });
}

#[test]
fn a_separate_process_reports_the_events_a_failed_save_lost() {
    // why: DF3 "across separate CLI processes alike": process A cannot save
    //      and exits; process B is a fresh vigil over the same root with a
    //      healthy disk. B reads the unchanged file, senses the same change,
    //      and fires the same single event, persisted, with no persist field.
    // proves: CLAIM(DF3)
    scenario(|| async {
        let fixture = Fixture::new().await;
        let (process_a, _) = fixture.faulted(disk_stays_full(28));
        let lost = process_a.keepalive(by_id(&fixture.id, None)).await.unwrap();
        assert_eq!(persist_of(&wire(&lost))["ok"], false);

        let process_b = fixture.env.another_process();
        let again = process_b.keepalive(by_id(&fixture.id, None)).await.unwrap();
        assert_eq!(again.events, lost.events);
        assert_eq!(again.events.len(), 1);
        assert_eq!((again.status, again.tick), (WatchStatus::Fired, 1));
        assert!(wire(&again).get("persist").is_none());
        assert_eq!(fixture.env.vigil.store().load(&fixture.id).unwrap().status, WatchStatus::Fired);
    });
}

#[test]
fn a_no_id_keepalive_does_not_count_an_unsaved_result_as_returned() {
    // why: DF3 "in the MCP server": the agent normally calls keepalive WITHOUT
    //      an id, which covers this process's own watches except those whose
    //      result it already returned (V7). If the call whose save failed
    //      marked the watch returned, the next no-id call would say
    //      NothingToCover and the event would be lost for good, though the
    //      file still says pending. The disk is full only while a sentinel
    //      file exists, so the same process can create the watch (healthy),
    //      fail the tick (full), and retry (room again). Control: after a
    //      persisted delivery the watch IS returned (NothingToCover), the
    //      once-per-process rule of V7 is intact.
    // proves: CLAIM(DF3)
    scenario(|| async {
        let env = paused_env();
        let sentinel = env.dir.path().join("disk-is-full");
        let plan = FaultPlan::from_rules(vec![
            rule(WriteKind::Watch, 28, When::WhileExists(sentinel.clone())),
            rule(WriteKind::Reserve, 28, When::WhileExists(sentinel.clone())),
        ]);
        let vigil = env.another_process().with_write_faults(plan);
        let input = env.input_path("w.log");
        write_file(&input, "old\n");
        let id = vigil.create(change_request("w", &input, 20)).await.expect("healthy create").watch_id;
        append_file(&input, "boom\n");

        std::fs::write(&sentinel, "").unwrap();
        let lost = vigil.keepalive(no_id()).await.expect("answer survives");
        assert_eq!(lost.status, WatchStatus::Fired);
        assert_eq!(persist_of(&wire(&lost))["ok"], false);

        std::fs::remove_file(&sentinel).unwrap();
        let again = vigil.keepalive(no_id()).await.expect("the unsaved result must still be covered, not NothingToCover");
        assert_eq!(again.events, lost.events);
        assert!(wire(&again).get("persist").is_none());
        assert_eq!(vigil.store().load(&id).unwrap().status, WatchStatus::Fired);

        assert_eq!(vigil.keepalive(no_id()).await.unwrap_err(), VigilError::NothingToCover, "control: persisted and returned, so done");
    });
}

#[test]
fn a_redelivery_covers_everything_since_the_last_saved_baseline_when_the_input_kept_changing() {
    // why: DF3b: "Redelivered events are those since the last saved baseline:
    //      the unsaved ones plus anything newer." The failed tick did not move
    //      the baseline, so when the input grows again before the next call,
    //      the redelivery is NOT the lost event verbatim: it reports the
    //      change from the saved baseline ("old") to now. Oracle: a twin world
    //      in which both lines were appended before one healthy tick; the
    //      redelivery must equal it (modulo the sandbox path). Hand-derived
    //      checks: tick 1 / event "w:1:0" (the baseline did not move, so the
    //      tick number did not either), and the event differs from the lost
    //      one, which proves the input really was re-read.
    // proves: CLAIM(DF3b), CLAIM(DF3)
    scenario(|| async {
        let twin = Fixture::new().await;
        append_file(&twin.env.input_path("w.log"), "bang\n");
        let oracle = twin.env.another_process().keepalive(by_id(&twin.id, None)).await.unwrap();
        assert_eq!(oracle.events.len(), 1, "oracle: one event for the whole change");

        let fixture = Fixture::new().await;
        let (vigil, _) = fixture.faulted(vec![rule(WriteKind::Watch, 28, When::Range { first: 1, last: Some(2) })]);
        let lost = vigil.keepalive(by_id(&fixture.id, None)).await.expect("first call");
        assert_eq!(persist_of(&wire(&lost))["ok"], false, "precondition: the first save failed");
        assert_eq!(lost.events.len(), 1);

        append_file(&fixture.env.input_path("w.log"), "bang\n");
        let again = vigil.keepalive(by_id(&fixture.id, None)).await.expect("second call");

        assert_ne!(again.events, lost.events, "the input changed again, so the redelivery is not the lost event verbatim");
        assert_eq!(again.events.len(), 1, "once per envelope");
        assert_eq!((again.status, again.tick, again.events[0].event_id.as_str()), (WatchStatus::Fired, 1, "w:1:0"));
        assert!(wire(&again).get("persist").is_none(), "this write succeeded: {}", wire(&again));
        let mut got = wire(&again);
        let mut want = wire(&oracle);
        // `persist` is the note under test; `session_ticks` counts calls made by
        // THIS process (the faulted one answered twice, the twin once).
        for value in [&mut got, &mut want] {
            let object = value.as_object_mut().unwrap();
            object.remove("persist");
            object.remove("session_ticks");
        }
        let got_text = got.to_string().replace(fixture.env.dir.path().to_str().unwrap(), "<dir>");
        let want_text = want.to_string().replace(twin.env.dir.path().to_str().unwrap(), "<dir>");
        assert_eq!(got_text, want_text, "the redelivery must equal one healthy tick over the whole change since the saved baseline");
        let stored: Value = serde_json::from_slice(&fixture.watch_bytes()).unwrap();
        assert_eq!((stored["status"].as_str(), stored["tick"].as_u64()), (Some("fired"), Some(1)));
    });
}

#[test]
fn redelivery_is_flagged_exactly_when_events_or_a_terminal_status_are_at_risk() {
    // why: DF3a: "`redelivery: true` only when something is at risk: the
    //      envelope has events, or a terminal status (fired/expired/cancelled)
    //      that was not saved. Otherwise the field is absent." Four unsaved
    //      envelopes on a full disk, one per row:
    //        fired     - one event "boom"                     -> true
    //        expired   - deadline 30 s passed, no events      -> true (terminal)
    //        cancelled - cancel that cannot be saved          -> true (terminal)
    //        pending   - a quiet tick, nothing to lose        -> ABSENT
    //      The absent row is the control that the flag is not just always on.
    // proves: CLAIM(DF3a)
    scenario(|| async {
        let fired = Fixture::new().await;
        let (vigil, _) = fired.faulted(disk_stays_full(28));
        let envelope = wire(&vigil.keepalive(by_id(&fired.id, None)).await.unwrap());
        assert_eq!(envelope["status"], "fired");
        assert_eq!(envelope["events"].as_array().map(Vec::len), Some(1));
        assert_failed_persist(&envelope, "disk_full", "No space left on device", &fired.watch_file(), true);

        let expired_env = paused_env();
        write_file(&expired_env.input_path("e.log"), "old\n");
        let mut request = change_request("e", &expired_env.input_path("e.log"), 20);
        request.deadline_s = Some(30);
        let id = expired_env.vigil.create(request).await.unwrap().watch_id;
        tokio::time::sleep(Duration::from_secs(31)).await;
        let vigil = expired_env.another_process().with_write_faults(FaultPlan::from_rules(disk_stays_full(28)));
        let envelope = wire(&vigil.keepalive(by_id(&id, None)).await.unwrap());
        assert_eq!(envelope["status"], "expired");
        assert_eq!(envelope["events"].as_array().map(Vec::len), Some(0), "expiry carries no events");
        assert_failed_persist(&envelope, "disk_full", "No space left on device", &expired_env.root().join("watches").join("e.json"), true);

        let cancelled = Fixture::new().await;
        let (vigil, _) = cancelled.faulted(disk_stays_full(28));
        let envelope = wire(&vigil.cancel(&cancelled.id).await.unwrap());
        assert_eq!(envelope["status"], "cancelled");
        assert_failed_persist(&envelope, "disk_full", "No space left on device", &cancelled.watch_file(), true);

        let quiet = paused_env();
        write_file(&quiet.input_path("q.log"), "old\n");
        let id = quiet.vigil.create(change_request("q", &quiet.input_path("q.log"), 20)).await.unwrap().watch_id;
        let vigil = quiet.another_process().with_write_faults(FaultPlan::from_rules(disk_stays_full(28)));
        let envelope = wire(&vigil.keepalive(by_id(&id, Some(30))).await.unwrap());
        assert_eq!(envelope["status"], "pending");
        assert_eq!(envelope["events"].as_array().map(Vec::len), Some(0));
        assert_failed_persist(&envelope, "disk_full", "No space left on device", &quiet.root().join("watches").join("q.json"), false);
    });
}

// ---- DF4: the reserve -----------------------------------------------------

#[test]
fn a_tick_that_only_fits_after_the_reserve_is_deleted_is_saved_and_says_so() {
    // why: DF4: on a disk_full tick write the store deletes `.reserve` and
    //      retries; "If the retry succeeds, the tick is persisted and the
    //      envelope carries persist = {ok:true, used_reserve:true,
    //      kind:disk_full}" - exactly those three keys, so the agent still
    //      learns the disk is full. The file on disk is fired, the reserve is
    //      gone. For EDQUOT as well. A later call (a replay of the stored
    //      result, no write) carries no persist field: the note is not sticky.
    // proves: CLAIM(DF4), CLAIM(DF2)
    for errno in [28, 122] {
        scenario(move || async move {
            let fixture = Fixture::new().await;
            assert!(fixture.reserve().exists(), "precondition: errno {errno}: create left a reserve");
            let (vigil, _) = fixture.faulted(disk_has_only_the_reserve_to_give(&fixture.env.root(), errno));

            let envelope = vigil.keepalive(by_id(&fixture.id, None)).await.unwrap();

            assert_eq!(envelope.status, WatchStatus::Fired);
            assert_eq!(persist_of(&wire(&envelope)), &json!({"ok": true, "used_reserve": true, "kind": "disk_full"}), "errno {errno}");
            assert_eq!(fixture.env.vigil.store().load(&fixture.id).unwrap().status, WatchStatus::Fired, "errno {errno}: persisted");
            assert!(!fixture.reserve().exists(), "errno {errno}: spent");
            let replay = wire(&vigil.keepalive(by_id(&fixture.id, None)).await.unwrap());
            assert!(replay.get("persist").is_none(), "errno {errno}: {replay}");
        });
    }
}

#[test]
fn an_error_that_is_not_a_full_disk_leaves_the_reserve_alone_end_to_end() {
    // why: DF4 "non-disk errors do not touch the reserve", through the real
    //      tick (the store-level twin is in store.rs). EIO: persist.kind is
    //      `io`, ok=false, and `.reserve` is the very same file afterwards.
    // proves: CLAIM(DF4), CLAIM(DF2)
    scenario(|| async {
        let fixture = Fixture::new().await;
        assert!(fixture.reserve().exists(), "precondition: create left a reserve");
        let reserve_before = identity(&fixture.reserve());
        let (vigil, _) = fixture.faulted(vec![rule(WriteKind::Watch, 5, When::Every)]);

        let envelope = wire(&vigil.keepalive(by_id(&fixture.id, None)).await.unwrap());

        assert_eq!(persist_of(&envelope)["kind"], "io");
        assert_eq!(persist_of(&envelope)["ok"], false);
        assert_eq!(identity(&fixture.reserve()), reserve_before);
    });
}

#[test]
fn a_tick_on_a_disk_without_a_reserve_reports_the_first_failure_and_does_not_retry() {
    // why: DF4a: "No reserve file, no retry: the first failure is reported
    //      (`used_reserve` absent)." The reserve is deleted before the tick.
    //      The fault hits ONLY the first write attempt (`Nth(1)`), so a retry
    //      would succeed: the persist note must therefore say ok=false, kind
    //      disk_full, with no `used_reserve` key, the file must be unchanged,
    //      and the watch file was attempted exactly once. Control: the same
    //      one-shot fault WITH a reserve is rescued by the retry
    //      (`used_reserve: true`, 2 attempts).
    // proves: CLAIM(DF4a), CLAIM(DF4)
    scenario(|| async {
        let with_reserve = Fixture::new().await;
        assert!(with_reserve.reserve().exists(), "precondition: create left a reserve");
        let (vigil, plan) = with_reserve.faulted(vec![rule(WriteKind::Watch, 28, When::Nth(1)), rule(WriteKind::Reserve, 28, When::Every)]);
        let rescued = wire(&vigil.keepalive(by_id(&with_reserve.id, None)).await.unwrap());
        assert_eq!(persist_of(&rescued), &json!({"ok": true, "used_reserve": true, "kind": "disk_full"}), "control");
        assert_eq!(plan.attempts(WriteKind::Watch), 2, "control: one failure, one retry");

        let fixture = Fixture::new().await;
        let _ = std::fs::remove_file(fixture.reserve());
        assert!(!fixture.reserve().exists(), "precondition: no reserve");
        let before = fixture.watch_bytes();
        let (vigil, plan) = fixture.faulted(vec![rule(WriteKind::Watch, 28, When::Nth(1)), rule(WriteKind::Reserve, 28, When::Every)]);

        let envelope = wire(&vigil.keepalive(by_id(&fixture.id, None)).await.expect("a failed save must not fail the call"));

        assert_eq!(envelope["status"], "fired");
        assert_failed_persist(&envelope, "disk_full", "No space left on device", &fixture.watch_file(), true);
        assert!(persist_of(&envelope).get("used_reserve").is_none(), "{envelope}");
        assert_eq!(plan.attempts(WriteKind::Watch), 1, "nothing to free, so no retry");
        assert_eq!(fixture.watch_bytes(), before);
    });
}

// ---- DF5: every write goes through the same path --------------------------

#[test]
fn a_create_on_a_full_disk_fails_with_a_message_that_names_the_root_and_says_disk_full() {
    // why: DF5: "A create that still fails returns an error whose message names
    //      the state root and says the disk is full (not a bare `io error`)."
    //      Nothing is left behind: no watch file, no .tmp. DF5b fixes the
    //      contract: the message contains the state root path and the
    //      lowercase words `disk full`.
    // proves: CLAIM(DF5), CLAIM(DF5b)
    scenario(|| async {
        let env = paused_env();
        let input = env.input_path("w.log");
        write_file(&input, "old\n");
        let vigil = env.another_process().with_write_faults(FaultPlan::from_rules(disk_stays_full(28)));

        let error = vigil.create(change_request("w", &input, 20)).await.expect_err("nothing can be written");

        let text = error.to_string();
        assert!(text.contains(env.root().to_str().unwrap()), "should name the state root {}: {text}", env.root().display());
        assert!(text.contains("disk full"), "should contain the lowercase words `disk full`: {text}");
        assert!(!text.starts_with("io error"), "{text}");
        assert!(!env.root().join("watches").join("w.json").exists());
        assert!(!env.root().exists() || tmp_files(&env.root()).is_empty());
    });
}

#[test]
fn a_create_that_fits_once_the_reserve_is_freed_succeeds() {
    // why: DF5: watch create is one of the writes that uses the
    //      reserve-and-retry path. A first watch leaves a reserve; the second
    //      create hits a full disk that only has the reserve to give.
    // proves: CLAIM(DF5), CLAIM(DF4)
    scenario(|| async {
        let env = paused_env();
        write_file(&env.input_path("a.log"), "x\n");
        env.vigil.create(change_request("first", &env.input_path("a.log"), 20)).await.unwrap();
        assert!(env.root().join(".reserve").exists(), "precondition: a reserve exists");
        let vigil = env.another_process().with_write_faults(FaultPlan::from_rules(disk_has_only_the_reserve_to_give(&env.root(), 28)));

        let created = vigil.create(change_request("second", &env.input_path("a.log"), 20)).await.expect("the reserve makes room");

        assert_eq!(created.watch_id.as_str(), "second");
        assert_eq!(env.vigil.store().load(&wid("second")).unwrap().tick, 0, "baseline written");
        assert!(!env.root().join(".reserve").exists(), "spent");
    });
}

#[test]
fn a_cancel_that_fits_once_the_reserve_is_freed_is_saved_and_reports_it() {
    // why: DF5: cancel is a write too. Rescue case: the watch ends up cancelled
    //      on disk, the reserve is spent, and (inferred from DF4's "the
    //      envelope carries ...", see the red report) the cancel envelope
    //      carries the same used_reserve note as a tick would.
    // proves: CLAIM(DF5), CLAIM(DF4)
    scenario(|| async {
        let fixture = Fixture::new().await;
        let (vigil, _) = fixture.faulted(disk_has_only_the_reserve_to_give(&fixture.env.root(), 28));

        let envelope = vigil.cancel(&fixture.id).await.expect("the reserve makes room");

        assert_eq!(envelope.status, WatchStatus::Cancelled);
        assert_eq!(fixture.env.vigil.store().load(&fixture.id).unwrap().status, WatchStatus::Cancelled);
        assert!(!fixture.reserve().exists());
        assert_eq!(persist_of(&wire(&envelope)), &json!({"ok": true, "used_reserve": true, "kind": "disk_full"}));
    });
}

#[test]
fn a_cancel_that_cannot_be_saved_returns_an_envelope_with_persist_not_a_silent_success() {
    // why: DF5a (red report A3): "A cancel that cannot be saved even after the
    //      reserve returns an envelope with persist.ok = false (like a tick),
    //      never a silent success." So: `Ok`, status cancelled, persist
    //      {ok:false, kind:disk_full, error with the OS text, path = the watch
    //      file}, and `redelivery: true` (a terminal status was not saved,
    //      DF3a). DF6: the watch file is byte-identical (still pending) and no
    //      .tmp is left. Both the write and its retry fail: two attempts,
    //      because the reserve existed. A later healthy cancel from another
    //      process really cancels, so nothing was half-done.
    // proves: CLAIM(DF5a), CLAIM(DF3a), CLAIM(DF6)
    scenario(|| async {
        let fixture = Fixture::new().await;
        let before = fixture.watch_bytes();
        let (vigil, plan) = fixture.faulted(disk_stays_full(28));

        let envelope = vigil.cancel(&fixture.id).await.expect("an unsaved cancel is an envelope, not an error");

        assert_eq!(envelope.status, WatchStatus::Cancelled);
        assert_failed_persist(&wire(&envelope), "disk_full", "No space left on device", &fixture.watch_file(), true);
        assert_eq!(plan.attempts(WriteKind::Watch), 2, "the write and one retry (create left a reserve to free)");
        assert_eq!(fixture.watch_bytes(), before);
        assert!(tmp_files(&fixture.env.root()).is_empty());
        let later = fixture.env.another_process().cancel(&fixture.id).await.unwrap();
        assert_eq!(later.status, WatchStatus::Cancelled);
        assert!(wire(&later).get("persist").is_none());
        assert_eq!(fixture.env.vigil.store().load(&fixture.id).unwrap().status, WatchStatus::Cancelled, "the retried cancel is saved");
    });
}

#[test]
fn a_missing_lock_file_is_created_through_the_reserve_and_the_tick_still_answers() {
    // why: DF5: "per-watch lock-file creation" goes through reserve-and-retry.
    //      The lock file is removed (as after a prune), the disk is full until
    //      the reserve is deleted, and creating the lock is what needs the
    //      room. The tick must go on and fire; the lock file exists again and
    //      the reserve is spent. (The watch write itself is healthy here.)
    // proves: CLAIM(DF5), CLAIM(DF1)
    scenario(|| async {
        let fixture = Fixture::new().await;
        let lock = fixture.env.root().join("locks").join("w.lock");
        assert!(fixture.reserve().exists(), "precondition: create left a reserve");
        std::fs::remove_file(&lock).expect("precondition: create left a lock file");
        let rules = vec![
            rule(WriteKind::Lock, 28, When::WhileExists(fixture.reserve())),
            rule(WriteKind::Reserve, 28, When::Every),
        ];
        let (vigil, _) = fixture.faulted(rules);

        let envelope = vigil.keepalive(by_id(&fixture.id, None)).await.expect("the lock is created after freeing the reserve");

        assert_eq!(envelope.status, WatchStatus::Fired);
        assert!(lock.exists());
        assert!(!fixture.reserve().exists(), "spent to create the lock");
    });
}

#[test]
fn a_lock_file_that_can_never_be_created_fails_the_call_with_a_message_that_names_the_root() {
    // why: DF5: without a lock the tick cannot run, so there is no answer to
    //      save; the call fails, but as "disk full" naming the root, not as a
    //      bare `io error: ... (os error 28)`. DF5b: the message contains the
    //      state root path and the lowercase words `disk full`.
    // proves: CLAIM(DF5), CLAIM(DF5b)
    scenario(|| async {
        let fixture = Fixture::new().await;
        std::fs::remove_file(fixture.env.root().join("locks").join("w.lock")).unwrap();
        let (vigil, _) = fixture.faulted(vec![rule(WriteKind::Lock, 28, When::Every)]);

        let error = vigil.keepalive(by_id(&fixture.id, None)).await.expect_err("no lock, no tick");

        let text = error.to_string();
        assert!(text.contains(fixture.env.root().to_str().unwrap()), "{text}");
        assert!(text.contains("disk full"), "lowercase `disk full` (DF5b): {text}");
    });
}

// ---- DF7: a failed create does not burn the id --------------------------

#[test]
fn a_create_whose_baseline_save_fails_removes_its_placeholder_so_the_id_can_be_reused() {
    // why: DF7 (red report A7): `create` writes a placeholder (watch write 1)
    //      to reserve the id, then the baseline (watch write 2). Writes 2 and
    //      3 fail here (the baseline and its retry), so the call fails with
    //      the DF5b error. Before DF7 the placeholder stayed and the agent's
    //      retry with the same id got `DuplicateId`. Now: no `dup.json`, `list`
    //      is empty, no .tmp, and once space exists (a healthy process) the
    //      same id is created. The error is not DuplicateId either.
    //      The placeholder's success leaves a reserve (nothing blocks its
    //      creation), so the failed baseline is retried once: 3 watch attempts
    //      (placeholder ok, baseline fails, retry fails) prove the placeholder
    //      really was written before the failure.
    // proves: CLAIM(DF7), CLAIM(DF5b)
    scenario(|| async {
        let env = paused_env();
        let input = env.input_path("w.log");
        write_file(&input, "old\n");
        let plan = FaultPlan::from_rules(vec![rule(WriteKind::Watch, 28, When::Range { first: 2, last: None })]);
        let vigil = env.another_process().with_write_faults(plan.clone());

        let error = vigil.create(change_request("dup", &input, 20)).await.expect_err("the baseline cannot be written");

        let text = error.to_string();
        assert!(text.contains(env.root().to_str().unwrap()) && text.contains("disk full"), "DF5b: {text}");
        assert_eq!(plan.attempts(WriteKind::Watch), 3, "placeholder (ok), baseline (fails), retry (fails)");
        assert!(!env.root().join("watches").join("dup.json").exists(), "the placeholder must be removed");
        assert!(env.vigil.list().await.unwrap().is_empty());
        assert!(tmp_files(&env.root()).is_empty());

        let created = env.another_process().create(change_request("dup", &input, 20)).await.expect("the id is free again once space exists");
        assert_eq!(created.watch_id.as_str(), "dup");
        assert_eq!(env.vigil.store().load(&wid("dup")).unwrap().tick, 0);
    });
}

// ---- DF6: atomicity -------------------------------------------------------

#[test]
fn a_failed_tick_save_leaves_the_watch_file_byte_identical_and_no_tmp_anywhere() {
    // why: DF6: "on any failure the previous watch file is intact and no .tmp
    //      remains", checked through the engine (the store-level proof is in
    //      store.rs). The envelope is returned (DF1) AND the disk state is
    //      exactly the pre-tick state.
    // proves: CLAIM(DF6), CLAIM(DF1)
    scenario(|| async {
        let fixture = Fixture::new().await;
        let before = fixture.watch_bytes();
        let (vigil, plan) = fixture.faulted(disk_stays_full(28));

        let envelope = vigil.keepalive(by_id(&fixture.id, None)).await.expect("answer survives");

        assert_eq!(envelope.status, WatchStatus::Fired, "precondition: the tick did sense the change");
        assert!(plan.attempts(WriteKind::Watch) >= 1, "precondition: the save was attempted (and failed)");
        assert_eq!(persist_of(&wire(&envelope))["ok"], false, "precondition: the save failed");
        assert_eq!(fixture.watch_bytes(), before);
        assert!(tmp_files(&fixture.env.root()).is_empty());
        let names: Vec<String> =
            std::fs::read_dir(fixture.env.root().join("watches")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into()).collect();
        assert_eq!(names, vec!["w.json".to_string()]);
    });
}
