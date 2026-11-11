//! Engine-level tests for surviving a full disk (specs/disk-full.md, claims
//! DF1-DF6). Cross-cutting on purpose: one scenario drives the store, the tick,
//! `keepalive`, `create`, `cancel` and the envelope together, so it lives in
//! one module instead of being split across four. Store-only claims are tested
//! beside the store (`store.rs`), the seam beside the seam (`fault.rs`), and
//! the process boundary in `tests/cli_e2e.rs` / `tests/mcp_e2e.rs`.
//!
//! The expected wire shapes are copied from the spec, not from any code:
//!
//! * failed write:  `"persist": {"ok": false, "kind": "disk_full" | "io",
//!   "error": "<message>", "path": "<file>", "redelivery": true}` (DF2, DF3);
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

/// The failed-write note, field by field against the spec (DF2, DF3).
fn assert_failed_persist(envelope: &Value, kind: &str, os_text: &str, file: &Path) {
    let persist = persist_of(envelope);
    let object = persist.as_object().unwrap_or_else(|| panic!("persist is not an object: {persist}"));
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, ["error", "kind", "ok", "path", "redelivery"], "{persist}");
    assert_eq!(persist["ok"], false, "{persist}");
    assert_eq!(persist["kind"], kind, "{persist}");
    assert_eq!(persist["redelivery"], true, "{persist}");
    assert_eq!(persist["path"], file.to_str().unwrap(), "{persist}");
    let error = persist["error"].as_str().unwrap_or_else(|| panic!("error is not a string: {persist}"));
    assert!(error.contains(os_text), "error {error:?} should carry the OS message {os_text:?}");
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
    // why: DF2: `persist = {ok:false, kind, error, path, redelivery:true}`;
    //      `kind` is `disk_full` for ENOSPC (28) and EDQUOT (122), `io` for
    //      anything else. The OS texts are glibc's strerror, written out here
    //      (not produced by the code under test). `path` is the watch file.
    //      Reserve creation fails too, so a disk_full write really is lost.
    // proves: CLAIM(DF2), CLAIM(DF3)
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
            assert_failed_persist(&envelope, kind, os_text, &fixture.watch_file());
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
fn a_quiet_tick_that_cannot_save_returns_pending_with_persist_and_does_not_spin() {
    // why: DF1/DF2 for the common case, a tick that found nothing. The save
    //      still fails, so the persisted `last_tick_at` never advances and the
    //      watch looks due again at once: a naive loop re-ticks forever (and
    //      hammers a full disk). The call must come back (the watchdog fails a
    //      spin) as `pending` / "call keepalive again" with persist.ok = false,
    //      and must not read more often than a healthy call would: a 30 s call
    //      at interval 20 reads at t=0 and t=20, plus the baseline read at
    //      create = 3 reads at most; at least 2 (the baseline and tick 1),
    //      else nothing was sensed. Which of "return at once" or "keep ticking
    //      in memory" is chosen is left to the implementation.
    // proves: CLAIM(DF1), CLAIM(DF2)
    scenario(|| async {
        let env = paused_env();
        env.reader.set_text("cmd:probe", "ok");
        let mut request = change_request("q", Path::new("/unused"), 20);
        request.inputs = vec![crate::watch::Input::Cmd { argv: vec!["probe".into()], timeout_s: None, cwd: None }];
        let id = env.vigil.create(request).await.unwrap().watch_id;
        let plan = FaultPlan::from_rules(disk_stays_full(28));
        let vigil = env.another_process().with_write_faults(plan);

        let envelope = vigil.keepalive(by_id(&id, Some(30))).await.expect("a failed save must not fail the call");
        let wire = wire(&envelope);

        assert_eq!((envelope.status, envelope.next), (WatchStatus::Pending, Next::CallKeepaliveAgain));
        assert_eq!(persist_of(&wire)["ok"], false);
        assert_eq!(persist_of(&wire)["kind"], "disk_full");
        let reads = env.reader.started_count("cmd:probe");
        assert!((2..=3).contains(&reads), "{reads} reads: baseline + at most the ticks at t=0 and t=20");
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

// ---- DF5: every write goes through the same path --------------------------

#[test]
fn a_create_on_a_full_disk_fails_with_a_message_that_names_the_root_and_says_disk_full() {
    // why: DF5: "A create that still fails returns an error whose message names
    //      the state root and says the disk is full (not a bare `io error`)."
    //      Nothing is left behind: no watch file, no .tmp. The substring
    //      "disk full" (case-insensitive) is the contract this test fixes; the
    //      spec's own wording is looser (see the red report).
    // proves: CLAIM(DF5)
    scenario(|| async {
        let env = paused_env();
        let input = env.input_path("w.log");
        write_file(&input, "old\n");
        let vigil = env.another_process().with_write_faults(FaultPlan::from_rules(disk_stays_full(28)));

        let error = vigil.create(change_request("w", &input, 20)).await.expect_err("nothing can be written");

        let text = error.to_string();
        assert!(text.contains(env.root().to_str().unwrap()), "should name the state root {}: {text}", env.root().display());
        assert!(text.to_lowercase().contains("disk full"), "should say the disk is full: {text}");
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
fn a_cancel_that_cannot_be_saved_is_never_a_silent_success() {
    // why: DF5 names cancel as a write that goes through the retry path, but
    //      the spec only says what a failing CREATE returns. For cancel two
    //      outcomes are defensible: an error naming the state root and
    //      saying "disk full" (like create), or an envelope with
    //      persist.ok=false. Silently reporting a cancel that never reached
    //      disk is the one wrong answer. Either way the watch file is
    //      byte-identical and no .tmp is left (DF6). To be settled in the
    //      spec (red report, A3).
    // proves: CLAIM(DF5), CLAIM(DF6)
    scenario(|| async {
        let fixture = Fixture::new().await;
        let before = fixture.watch_bytes();
        let (vigil, _) = fixture.faulted(disk_stays_full(28));

        let outcome = vigil.cancel(&fixture.id).await;

        let loud = match &outcome {
            Err(error) => {
                let text = error.to_string();
                text.contains(fixture.env.root().to_str().unwrap()) && text.to_lowercase().contains("disk full")
            }
            Ok(envelope) => wire(envelope).get("persist").is_some_and(|persist| persist["ok"] == false),
        };
        assert!(loud, "a cancel that was not saved must say so: {outcome:?}");
        assert_eq!(fixture.watch_bytes(), before);
        assert!(tmp_files(&fixture.env.root()).is_empty());
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
    //      bare `io error: ... (os error 28)`.
    // proves: CLAIM(DF5)
    scenario(|| async {
        let fixture = Fixture::new().await;
        std::fs::remove_file(fixture.env.root().join("locks").join("w.lock")).unwrap();
        let (vigil, _) = fixture.faulted(vec![rule(WriteKind::Lock, 28, When::Every)]);

        let error = vigil.keepalive(by_id(&fixture.id, None)).await.expect_err("no lock, no tick");

        let text = error.to_string();
        assert!(text.contains(fixture.env.root().to_str().unwrap()), "{text}");
        assert!(text.to_lowercase().contains("disk full"), "{text}");
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
