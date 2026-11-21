//! Notice tests. Time and cwd are explicit; sessions are named, never ambient.
//! Reason: SPEC "Notices": a notice reaches every agent session exactly once,
//! and no session's delivery record can be lost to another's.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

use crate::KeepaliveRequest;
use crate::clock::Clock;
use crate::notice::{NoticeIdentity, NoticeRequest, NoticeSession, NoticeStore};
use crate::testing::*;

struct Rig {
    env: Env,
    clock: Arc<ManualClock>,
    cwd: PathBuf,
}

impl Rig {
    fn new() -> Rig {
        let clock = Arc::new(ManualClock::new(at("2026-11-20T12:00:00Z")));
        let env = Env::with_clock(clock.clone());
        let cwd = env.dir.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        Rig { env, clock, cwd }
    }
    fn vigil(&self) -> crate::Vigil {
        self.env.another_process_in(&self.cwd).with_session_key(None)
    }
    fn add(&self, id: &str, message: &str) {
        self.env
            .vigil
            .notice_add(NoticeRequest { id: id.into(), message: message.into(), ..Default::default() })
            .unwrap();
    }
    /// Claim as `session` and keep: what a hook (`vigil notices`) does.
    fn deliver(&self, session: &str) -> Vec<String> {
        self.deliver_in(session, &self.cwd)
    }
    fn deliver_in(&self, session: &str, cwd: &Path) -> Vec<String> {
        let claim = self.vigil().claim_notices(Some(session), Some(cwd)).unwrap();
        claim.keep().notices.into_iter().map(|n| n.id).collect()
    }
}

fn ids(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn add_list_and_rm_round_trip_and_refuse_bad_input_without_changing_state() {
    // why: the operator surface. A refused add must leave nothing behind.
    // catches: add that writes before validating; rm of a missing id that succeeds;
    //          a duplicate id that overwrites.
    let rig = Rig::new();
    let past = at("2026-11-20T11:59:59Z");
    rig.env
        .vigil
        .notice_add(NoticeRequest {
            id: "neon".into(),
            message: "Gemini Neon is a reviewer".into(),
            expires_at: Some(at("2026-12-01T00:00:00Z")),
            scope: vec![rig.cwd.clone()],
        })
        .unwrap();
    let list = rig.env.vigil.notice_list().unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], "neon");
    assert_eq!(list[0]["message"], "Gemini Neon is a reviewer");
    assert_eq!(list[0]["expires_at"], "2026-12-01T00:00:00Z");
    assert_eq!(list[0]["created_at"], "2026-11-20T12:00:00Z");
    assert_eq!(list[0]["expired"], false);

    let refused = [
        NoticeRequest { id: "neon".into(), message: "again".into(), ..Default::default() },
        NoticeRequest { id: "../escape".into(), message: "x".into(), ..Default::default() },
        NoticeRequest { id: "".into(), message: "x".into(), ..Default::default() },
        NoticeRequest { id: "empty".into(), message: "  ".into(), ..Default::default() },
        NoticeRequest { id: "long".into(), message: "x".repeat(2001), ..Default::default() },
        NoticeRequest { id: "past".into(), message: "x".into(), expires_at: Some(past), ..Default::default() },
        NoticeRequest { id: "rel".into(), message: "x".into(), scope: vec!["relative".into()], ..Default::default() },
    ];
    for request in refused {
        let label = request.id.clone();
        assert!(rig.env.vigil.notice_add(request).is_err(), "{label}");
    }
    let list = rig.env.vigil.notice_list().unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1, "a refused add changed state");
    assert_eq!(list[0]["message"], "Gemini Neon is a reviewer");

    assert!(rig.env.vigil.notice_rm("missing").is_err());
    assert_eq!(rig.env.vigil.notice_rm("neon").unwrap()["removed"], "neon");
    assert_eq!(rig.env.vigil.notice_list().unwrap().as_array().unwrap().len(), 0);
}

#[test]
fn at_most_sixty_four_notices() {
    // why: the store is bounded like reminders (32) so a runaway script cannot fill it.
    // catches: no cap; a cap off by one.
    let rig = Rig::new();
    for n in 0..64 {
        rig.add(&format!("n{n}"), "m");
    }
    let refused = rig.env.vigil.notice_add(NoticeRequest { id: "n64".into(), message: "m".into(), ..Default::default() });
    assert!(refused.is_err());
    assert_eq!(rig.env.vigil.notice_list().unwrap().as_array().unwrap().len(), 64);
}

#[test]
fn a_notice_reaches_each_session_exactly_once() {
    // why: SPEC "Notices": once per agent session. A reminder marked done by one
    //      agent vanishes for all; a notice must not.
    // catches: global done flag (A's delivery hides it from B); no record at all
    //          (A sees it twice).
    let rig = Rig::new();
    rig.add("neon", "reread the skills");
    assert_eq!(rig.deliver("session-a"), ids(&["neon"]));
    assert_eq!(rig.deliver("session-a"), ids(&[]), "A saw it twice");
    assert_eq!(rig.deliver("session-b"), ids(&["neon"]), "A's delivery hid it from B");
    assert_eq!(rig.deliver("session-b"), ids(&[]));
    assert_eq!(rig.deliver("session-a"), ids(&[]));
}

#[test]
fn the_delivery_record_survives_a_new_process() {
    // why: hooks and keepalive are separate processes; the record is on disk.
    // catches: an in-memory delivered set.
    let rig = Rig::new();
    rig.add("neon", "m");
    assert_eq!(rig.deliver("s"), ids(&["neon"]));
    let later = rig.env.another_process_in(&rig.cwd).with_session_key(None);
    assert!(later.claim_notices(Some("s"), Some(&rig.cwd)).unwrap().keep().notices.is_empty());
}

#[test]
fn only_notices_added_after_a_delivery_are_new() {
    // why: the pending set is notices minus this session's record, not "everything".
    // catches: a record that marks the whole directory delivered; a record that
    //          is overwritten instead of merged.
    let rig = Rig::new();
    rig.add("one", "first");
    assert_eq!(rig.deliver("s"), ids(&["one"]));
    rig.add("two", "second");
    assert_eq!(rig.deliver("s"), ids(&["two"]));
    rig.add("three", "third");
    rig.add("four", "fourth");
    // Same instant (manual clock), so the tie breaks by id: "four" < "three".
    assert_eq!(rig.deliver("s"), ids(&["four", "three"]), "oldest first, then by id on a tie");
    assert_eq!(rig.deliver("s"), ids(&[]));
}

#[test]
fn delivery_order_is_oldest_first() {
    // why: announcements read in the order they were made.
    // catches: directory order (alphabetical here: "a" would come first).
    let rig = Rig::new();
    rig.add("z-first", "1");
    rig.clock.advance_secs(5);
    rig.add("a-second", "2");
    assert_eq!(rig.deliver("s"), ids(&["z-first", "a-second"]));
}

#[test]
fn an_expired_notice_is_never_shown_and_the_boundary_is_exclusive() {
    // why: SPEC "Notices": expired notices are never shown. expires_at is the
    //      first instant it is no longer valid (11:59:59 shows; 12:00:00 does not).
    // catches: `<=` instead of `<` on the boundary; expiry ignored; expiry checked
    //          only at add time.
    let rig = Rig::new();
    rig.env
        .vigil
        .notice_add(NoticeRequest { id: "soon".into(), message: "m".into(), expires_at: Some(at("2026-11-20T12:10:00Z")), ..Default::default() })
        .unwrap();
    rig.clock.advance_secs(599);
    assert_eq!(rig.deliver("early"), ids(&["soon"]));
    rig.clock.advance_secs(1);
    assert_eq!(rig.deliver("late"), ids(&[]), "shown at its expiry instant");
    let list = rig.env.vigil.notice_list().unwrap();
    assert_eq!(list[0]["expired"], true, "list keeps the notice and marks it");
    rig.clock.advance_secs(86_400);
    assert_eq!(rig.deliver("much-later"), ids(&[]));
}

#[test]
fn a_scoped_notice_waits_for_a_session_inside_its_scope_and_is_not_consumed_outside() {
    // why: scope uses path components like reminders (CLAIM(reminder-scope)). A
    //      session outside the scope must not burn the notice for later.
    // catches: starts_with on strings ("proj-other" matches "proj"); marking a
    //          notice delivered when the scope check failed.
    let rig = Rig::new();
    let scoped = rig.env.dir.path().join("proj");
    let sibling = rig.env.dir.path().join("proj-other");
    let inside = scoped.join("src");
    for dir in [&scoped, &sibling, &inside] {
        std::fs::create_dir_all(dir).unwrap();
    }
    rig.env
        .vigil
        .notice_add(NoticeRequest { id: "scoped".into(), message: "m".into(), scope: vec![scoped.clone()], ..Default::default() })
        .unwrap();
    assert_eq!(rig.deliver_in("s", &sibling), ids(&[]));
    assert_eq!(rig.deliver_in("s", rig.env.dir.path()), ids(&[]));
    assert_eq!(rig.deliver_in("s", &inside), ids(&["scoped"]), "an out-of-scope call consumed it");
    assert_eq!(rig.deliver_in("s", &scoped), ids(&[]));
}

#[test]
fn a_notice_with_no_scope_is_delivered_from_any_directory() {
    // why: the default is everywhere (the neon announcement).
    // catches: an empty scope list read as "matches nothing".
    let rig = Rig::new();
    rig.add("everywhere", "m");
    assert_eq!(rig.deliver_in("s", Path::new("/")), ids(&["everywhere"]));
}

#[test]
fn a_readded_id_is_a_new_notice_for_every_session() {
    // why: rm then add with the same id is a new announcement, even for sessions
    //      that saw the old one (the record keys on id AND creation time).
    // catches: a record keyed on id alone.
    let rig = Rig::new();
    rig.add("neon", "old text");
    assert_eq!(rig.deliver("s"), ids(&["neon"]));
    rig.env.vigil.notice_rm("neon").unwrap();
    rig.clock.advance_secs(60);
    rig.add("neon", "new text");
    let claim = rig.vigil().claim_notices(Some("s"), Some(&rig.cwd)).unwrap();
    let delivery = claim.keep();
    assert_eq!(delivery.notices.len(), 1);
    assert_eq!(delivery.notices[0].message, "new text");
}

#[test]
fn releasing_a_claim_makes_the_notices_pending_again() {
    // why: if the output never reached the agent (closed stdout, cancelled call),
    //      the notice must not be lost.
    // catches: release that does nothing; release that clears the whole record.
    let rig = Rig::new();
    rig.add("one", "1");
    assert_eq!(rig.deliver("s"), ids(&["one"]));
    rig.add("two", "2");
    let claim = rig.vigil().claim_notices(Some("s"), Some(&rig.cwd)).unwrap();
    assert_eq!(claim.delivery().notices.len(), 1);
    claim.release();
    assert_eq!(rig.deliver("s"), ids(&["two"]), "released notice not redelivered, or 'one' came back");
    assert_eq!(rig.deliver("s"), ids(&[]));
}

#[test]
fn a_release_does_not_undo_a_newer_delivery_of_a_readded_notice() {
    // why: release removes only the generation it claimed.
    // catches: release keyed on id alone.
    let rig = Rig::new();
    rig.add("neon", "old");
    let stale = rig.vigil().claim_notices(Some("s"), Some(&rig.cwd)).unwrap();
    rig.env.vigil.notice_rm("neon").unwrap();
    rig.clock.advance_secs(60);
    rig.add("neon", "new");
    assert_eq!(rig.deliver("s"), ids(&["neon"]));
    stale.release();
    assert_eq!(rig.deliver("s"), ids(&[]), "the stale release un-delivered the new notice");
}

#[test]
fn removing_a_notice_prunes_it_from_every_record_on_next_use() {
    // why: records stay bounded by the live notice set.
    // catches: records that grow forever.
    let rig = Rig::new();
    rig.add("gone", "m");
    rig.deliver("s");
    rig.env.vigil.notice_rm("gone").unwrap();
    rig.deliver("s");
    let dir = rig.env.root().join("notice-delivery");
    let files: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
    assert_eq!(files.len(), 1);
    let record = std::fs::read_to_string(&files[0]).unwrap();
    assert!(!record.contains("gone"), "{record}");
}

#[test]
fn session_ids_cannot_escape_the_state_directory() {
    // why: --session is attacker-shaped text (a transcript path, a thread id).
    // catches: a session id used as a file name.
    let rig = Rig::new();
    rig.add("neon", "m");
    for hostile in ["../../escape", "a/b", ".", "..", "x\0y", &"s".repeat(400)] {
        let result = rig.vigil().claim_notices(Some(hostile), Some(&rig.cwd));
        if let Ok(claim) = result {
            claim.keep();
        }
    }
    let state = rig.env.root();
    let mut stack = vec![state.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            assert!(path.starts_with(&state));
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    assert!(!rig.env.dir.path().join("escape").exists());
    assert!(!state.join("../escape").exists());
}

#[test]
fn concurrent_claims_by_one_session_deliver_each_notice_exactly_once() {
    // why: invariant "concurrent sessions never lose a delivery record" and
    //      "exactly once": a hook and a keepalive race in one session.
    // catches: read-modify-write on the record without a lock (both claim it, or
    //          one overwrites the other's record).
    let rig = Rig::new();
    for n in 0..8 {
        rig.add(&format!("n{n}"), "m");
    }
    let threads = 8;
    let barrier = Arc::new(Barrier::new(threads));
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let store = NoticeStore { root: rig.env.root() };
            let barrier = barrier.clone();
            let cwd = rig.cwd.clone();
            let now = rig.clock.now();
            std::thread::spawn(move || {
                barrier.wait();
                let session = NoticeSession::Agent("same".into());
                store.claim(&session, now, &cwd).unwrap().keep().notices.into_iter().map(|n| n.id).collect::<Vec<_>>()
            })
        })
        .collect();
    let mut all: Vec<String> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
    all.sort();
    let mut expected: Vec<String> = (0..8).map(|n| format!("n{n}")).collect();
    expected.sort();
    assert_eq!(all, expected, "each notice exactly once across the racing claims");
}

#[test]
fn concurrent_sessions_each_get_every_notice_and_none_loses_its_record() {
    // why: different sessions write different records; none may clobber another.
    // catches: one shared delivery file; a shared temp name.
    let rig = Rig::new();
    for n in 0..4 {
        rig.add(&format!("n{n}"), "m");
    }
    let sessions = 12;
    let barrier = Arc::new(Barrier::new(sessions));
    let handles: Vec<_> = (0..sessions)
        .map(|s| {
            let store = NoticeStore { root: rig.env.root() };
            let barrier = barrier.clone();
            let cwd = rig.cwd.clone();
            let now = rig.clock.now();
            std::thread::spawn(move || {
                let session = NoticeSession::Agent(format!("session-{s}"));
                barrier.wait();
                let first = store.claim(&session, now, &cwd).unwrap().keep().notices.len();
                let second = store.claim(&session, now, &cwd).unwrap().keep().notices.len();
                (first, second)
            })
        })
        .collect();
    for handle in handles {
        assert_eq!(handle.join().unwrap(), (4, 0));
    }
}

#[test]
fn with_no_identity_a_notice_is_delivered_once_per_process_run_and_says_so() {
    // why: invariant: no identity delivers at most once per process run and says
    //      so; never on every call.
    // catches: no-identity treated as "deliver always"; an unlabeled delivery.
    let rig = Rig::new();
    rig.add("neon", "m");
    let vigil = rig.vigil();
    let first = vigil.claim_notices(None, Some(&rig.cwd)).unwrap().keep();
    assert_eq!(first.notices.len(), 1);
    assert_eq!(first.identity, NoticeIdentity::ProcessRun);
    assert_eq!(first.session, None);
    let note = first.note.expect("a run-keyed delivery says so");
    assert!(note.contains("no agent session identity"), "{note}");
    let again = vigil.claim_notices(None, Some(&rig.cwd)).unwrap().keep();
    assert!(again.notices.is_empty(), "repeated on a second call");
    // A second CLI process of the same caller (same parent) is the same run.
    let sibling = rig.vigil();
    assert!(sibling.claim_notices(None, Some(&rig.cwd)).unwrap().keep().notices.is_empty());
}

#[test]
fn an_explicit_session_beats_the_environment_identity_and_the_two_do_not_share_a_record() {
    // why: --session wins; the engine's own key is a different session.
    // catches: explicit ignored; explicit and ambient merged into one record.
    let rig = Rig::new();
    rig.add("neon", "m");
    let vigil = rig.env.another_process_in(&rig.cwd).with_session_key(Some("ambient".into()));
    let explicit = vigil.claim_notices(Some("explicit"), Some(&rig.cwd)).unwrap().keep();
    assert_eq!(explicit.identity, NoticeIdentity::Session);
    assert_eq!(explicit.session.as_deref(), Some("explicit"));
    assert_eq!(explicit.note, None);
    let ambient = vigil.claim_notices(None, Some(&rig.cwd)).unwrap().keep();
    assert_eq!(ambient.session.as_deref(), Some("ambient"));
    assert_eq!(ambient.notices.len(), 1, "the explicit delivery hid it from the ambient session");
    assert!(vigil.claim_notices(None, Some(&rig.cwd)).unwrap().keep().notices.is_empty());
}

#[test]
fn a_blank_explicit_session_is_an_error_not_the_ambient_session() {
    // why: `--session ""` from a hook whose jq found nothing must not silently
    //      fall back and consume the wrong record.
    // catches: empty string treated as absent.
    let rig = Rig::new();
    rig.add("neon", "m");
    assert!(rig.vigil().claim_notices(Some("  "), Some(&rig.cwd)).is_err());
}

// ---- keepalive envelope -------------------------------------------------

async fn fired_watch(rig: &Rig, name: &str) -> crate::WatchId {
    let path = rig.env.input_path(name);
    write_file(&path, "old\n");
    let id = rig.env.vigil.create(change_request(name, &path, 20)).await.unwrap().watch_id;
    append_file(&path, "new\n");
    id
}

fn request(id: crate::WatchId) -> KeepaliveRequest {
    KeepaliveRequest { watch_id: Some(id), max_s: Some(30), orchestrator: None, confirm: None, session: false }
}

#[tokio::test(start_paused = true)]
async fn the_keepalive_envelope_carries_a_notice_once_per_session() {
    // why: SPEC "Notices": the keepalive envelope delivers pending notices, once.
    // proves: CLAIM(notice-once)
    // catches: no notices field; a field that repeats on every return.
    let clock = Arc::new(TokioClock::new(spec_instant()));
    let mut rig = Rig::new();
    rig.env = Env::with_clock(clock);
    let one = fired_watch(&rig, "w1").await;
    let two = fired_watch(&rig, "w2").await;
    rig.env.vigil.notice_add(NoticeRequest { id: "neon".into(), message: "reread skills".into(), ..Default::default() }).unwrap();
    let vigil = rig.env.another_process().with_session_key(Some("agent-1".into()));
    let first = vigil.keepalive(request(one)).await.unwrap();
    assert_eq!(first.notices.len(), 1);
    assert_eq!(first.notices[0].id, "neon");
    assert_eq!(first.notices[0].message, "reread skills");
    assert_eq!(first.notices_note, None);
    let second = vigil.keepalive(request(two)).await.unwrap();
    assert!(second.notices.is_empty(), "repeated on the next return");
    let wire = serde_json::to_value(&second).unwrap();
    assert!(wire.get("notices").is_none(), "an empty list must be absent from the wire: {wire}");
}

#[tokio::test(start_paused = true)]
async fn a_hook_delivery_and_the_keepalive_envelope_share_one_record() {
    // why: the same session must see a notice once across surfaces: `vigil
    //      notices` (hook) and keepalive (envelope).
    // proves: CLAIM(notice-once)
    // catches: separate delivered sets per surface.
    let clock = Arc::new(TokioClock::new(spec_instant()));
    let mut rig = Rig::new();
    rig.env = Env::with_clock(clock);
    let one = fired_watch(&rig, "w1").await;
    rig.env.vigil.notice_add(NoticeRequest { id: "neon".into(), message: "m".into(), ..Default::default() }).unwrap();
    let vigil = rig.env.another_process().with_session_key(Some("agent-1".into()));
    let hooked = vigil.claim_notices(None, None).unwrap().keep();
    assert_eq!(hooked.notices.len(), 1);
    let envelope = vigil.keepalive(request(one)).await.unwrap();
    assert!(envelope.notices.is_empty(), "keepalive repeated what the hook delivered");
}

#[tokio::test(start_paused = true)]
async fn an_envelope_that_never_reached_the_agent_leaves_its_notices_pending() {
    // why: N3 (print, then commit): an uncommitted Delivery is dropped when stdout
    //      fails or the MCP call is cancelled; the notice must come back.
    // proves: CLAIM(notice-once)
    // catches: claiming at build time with no release when the Delivery is dropped.
    let clock = Arc::new(TokioClock::new(spec_instant()));
    let mut rig = Rig::new();
    rig.env = Env::with_clock(clock);
    let one = fired_watch(&rig, "w1").await;
    let two = fired_watch(&rig, "w2").await;
    rig.env.vigil.notice_add(NoticeRequest { id: "neon".into(), message: "m".into(), ..Default::default() }).unwrap();
    let vigil = rig.env.another_process().with_session_key(Some("agent-1".into()));
    let (envelope, delivery) = vigil
        .keepalive_uncommitted(request(one), &crate::keepalive::NoProgress, std::future::pending::<()>())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(envelope.notices.len(), 1);
    drop(delivery); // the print failed: never committed
    let retried = vigil.keepalive(request(two)).await.unwrap();
    assert_eq!(retried.notices.len(), 1, "a notice that was never delivered was lost");
}

#[tokio::test(start_paused = true)]
async fn a_committed_envelope_keeps_its_notices_delivered() {
    // why: the other half of the previous test: commit must keep, not release.
    // catches: Delivery that always releases on drop.
    let clock = Arc::new(TokioClock::new(spec_instant()));
    let mut rig = Rig::new();
    rig.env = Env::with_clock(clock);
    let one = fired_watch(&rig, "w1").await;
    let two = fired_watch(&rig, "w2").await;
    rig.env.vigil.notice_add(NoticeRequest { id: "neon".into(), message: "m".into(), ..Default::default() }).unwrap();
    let vigil = rig.env.another_process().with_session_key(Some("agent-1".into()));
    let (envelope, delivery) = vigil
        .keepalive_uncommitted(request(one), &crate::keepalive::NoProgress, std::future::pending::<()>())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(envelope.notices.len(), 1);
    vigil.commit_delivery(delivery);
    assert!(vigil.keepalive(request(two)).await.unwrap().notices.is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_brief_form_shows_each_notice() {
    // why: `keepalive --brief` is what the main session reads; a notice missing
    //      there is invisible.
    // catches: notices only in the JSON form.
    let clock = Arc::new(TokioClock::new(spec_instant()));
    let mut rig = Rig::new();
    rig.env = Env::with_clock(clock);
    let one = fired_watch(&rig, "w1").await;
    rig.env.vigil.notice_add(NoticeRequest { id: "neon".into(), message: "reread\nskills".into(), ..Default::default() }).unwrap();
    let vigil = rig.env.another_process().with_session_key(Some("agent-1".into()));
    let brief = vigil.keepalive(request(one)).await.unwrap().brief();
    assert!(brief.contains("notice neon: reread skills"), "{brief}");
}

#[tokio::test(start_paused = true)]
async fn an_envelope_with_no_identity_labels_its_notices() {
    // why: invariant: no identity -> say so in the output.
    // catches: a run-keyed delivery that looks like a session delivery.
    let clock = Arc::new(TokioClock::new(spec_instant()));
    let mut rig = Rig::new();
    rig.env = Env::with_clock(clock);
    let one = fired_watch(&rig, "w1").await;
    rig.env.vigil.notice_add(NoticeRequest { id: "neon".into(), message: "m".into(), ..Default::default() }).unwrap();
    let vigil = rig.env.another_process().with_session_key(None);
    let envelope = vigil.keepalive(request(one)).await.unwrap();
    assert_eq!(envelope.notices.len(), 1);
    assert!(envelope.notices_note.as_deref().is_some_and(|n| n.contains("no agent session identity")), "{:?}", envelope.notices_note);
}

fn tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                files.push((path, bytes));
            }
        }
    }
    files.sort();
    files
}

#[test]
fn a_claim_with_nothing_pending_leaves_the_state_root_untouched() {
    // why: every keepalive claims. A session with nothing pending (no notices at
    //      all, or all delivered) must write no record and no lock, or every
    //      vigil user's state root fills with files (cli_e2e's
    //      nothing_is_written_outside_the_state_root pins the same for keepalive).
    // catches: a claim that creates its record and lock before looking for work.
    // Positive control: the same probe sees the files a real delivery creates.
    let rig = Rig::new();
    let before = tree(&rig.env.root());
    assert_eq!(rig.deliver("s"), ids(&[]));
    assert_eq!(tree(&rig.env.root()), before, "no notices: claim wrote files");
    rig.add("neon", "m");
    let with_notice = tree(&rig.env.root());
    assert_eq!(rig.deliver("s"), ids(&["neon"]));
    let delivered = tree(&rig.env.root());
    assert_ne!(delivered, with_notice, "control: a delivery must change the tree");
    assert_eq!(rig.deliver("s"), ids(&[]));
    assert_eq!(tree(&rig.env.root()), delivered, "all delivered: claim wrote files");
}
