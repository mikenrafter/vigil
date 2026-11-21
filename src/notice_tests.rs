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
fn removing_a_notice_cleans_dead_records() {
    // notice rm performs maintenance; the next delivery remains read-only.
    let rig = Rig::new();
    rig.add("gone", "m");
    rig.deliver("s");
    rig.env.vigil.notice_rm("gone").unwrap();
    assert_eq!(rig.deliver("s"), ids(&[]));
    let files: Vec<_> = std::fs::read_dir(rig.env.root().join("notice-delivery")).unwrap()
        .map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
    assert_eq!(files.len(), 0);
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
                let session = NoticeSession::single("same").unwrap();
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
                let session = NoticeSession::single(&format!("session-{s}")).unwrap();
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

#[test]
fn old_records_survive_while_any_delivered_notice_is_live() {
    // CLAIM(notice-once): a long-idle session must never hear a live notice twice,
    // and the delivery directory uses one `.lock` file rather than per-record locks.
    use std::time::{Duration, SystemTime};
    let rig = Rig::new();
    rig.add("live", "m");
    rig.deliver("idle");
    rig.deliver("unlocked-idle");
    let dir = rig.env.root().join("notice-delivery");
    let paths: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).filter(|p| p.file_name().unwrap() != ".lock").collect();
    assert_eq!(paths.len(), 2, "only the two .json records (no per-record .lock files): {paths:?}");
    assert!(paths.iter().all(|p| p.extension().is_some_and(|ext| ext == "json")));
    let old = SystemTime::now() - Duration::from_secs(40 * 86400);
    for path in &paths {
        std::fs::File::options().write(true).open(path).unwrap().set_modified(old).unwrap();
    }
    rig.deliver("new-session"); // delivery does not run maintenance
    for path in &paths { assert!(path.exists(), "pruned {}", path.display()); }
    assert_eq!(rig.deliver("idle"), ids(&[]));
    assert_eq!(rig.deliver("unlocked-idle"), ids(&[]));
    rig.env.vigil.notice_rm("live").unwrap();
    rig.add("replacement", "m");
    rig.deliver("cleanup-trigger");
    for path in &paths {
        assert!(!path.exists(), "{}", path.display());
    }
    assert!(dir.join(".lock").exists(), "the single directory lock is never unlinked");
}

#[test]
fn corrupt_record_errors_without_redelivery_or_overwrite() {
    // CLAIM(notice-once): corruption must not silently reset delivery memory.
    let rig = Rig::new();
    rig.add("live", "m");
    rig.deliver("s");
    let path = std::fs::read_dir(rig.env.root().join("notice-delivery")).unwrap()
        .map(|e| e.unwrap().path()).find(|p| p.extension().is_some_and(|e| e == "json")).unwrap();
    std::fs::write(&path, b"{broken").unwrap();
    let result = rig.vigil().claim_notices(Some("s"), None);
    assert!(result.is_err(), "corrupt record was silently treated as empty");
    assert_eq!(std::fs::read(path).unwrap(), b"{broken");
}

#[test]
fn a_contended_notice_lock_returns_a_short_busy_error() {
    // Busy notice state must not stall a Tokio worker for five seconds.
    let rig = Rig::new();
    rig.add("live", "m");
    let lock = std::fs::File::options().write(true).open(rig.env.root().join("notices/.lock")).unwrap();
    lock.lock().unwrap();
    let started = std::time::Instant::now();
    let result = rig.env.vigil.notice_add(NoticeRequest { id: "other".into(), message: "m".into(), ..Default::default() });
    assert!(matches!(result, Err(crate::notice::NoticeError::Busy(_))));
    assert!(started.elapsed() < std::time::Duration::from_millis(250), "lock blocked for {:?}", started.elapsed());
}

#[test]
fn a_claim_does_not_erase_a_delivery_added_after_its_snapshot() {
    // CLAIM(notice-once): H lists, operator adds x, K delivers x, H updates.
    let rig = Rig::new();
    rig.add("first", "m");
    let store = rig.vigil().notice_store();
    let session = NoticeSession::single("s").unwrap();
    let claim = store.claim_with_snapshot_hook(&session, rig.clock.now(), &rig.cwd, || {
        rig.add("x", "new");
        assert_eq!(rig.deliver("s"), ids(&["first", "x"]));
    }).unwrap().keep();
    assert!(claim.notices.is_empty(), "the other caller already delivered every notice");
    assert_eq!(rig.deliver("s"), ids(&[]), "stale retain erased the x delivery");
}

#[test]
fn notice_cleanup_runs_on_remove_and_expiry_but_not_during_delivery() {
    // CLAIM(notice-once): cleanup must not extend the durable-claim/print gap.
    use std::time::{Duration, SystemTime};
    let rig = Rig::new();
    rig.add("old", "m");
    rig.deliver("s");
    let dir = rig.env.root().join("notice-delivery");
    let record = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|ext| ext == "json")).unwrap();
    assert!(!record.with_extension("lock").exists(), "single directory lock creates no per-record lock");
    std::fs::File::options().write(true).open(&record).unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(40 * 86400)).unwrap();
    // Simulate an operator removing a notice file before this delivery.
    std::fs::remove_file(rig.env.root().join("notices/old.json")).unwrap();
    rig.add("new", "m");
    rig.deliver("other");
    assert!(record.exists(), "delivery must not sweep other sessions");
    rig.env.vigil.notice_rm("new").unwrap();
    assert!(!record.exists(), "notice rm must sweep dead records");

    rig.env.vigil.notice_add(NoticeRequest {
        id: "expires".into(), message: "m".into(),
        expires_at: Some(at("2026-11-20T12:00:02Z")), ..Default::default()
    }).unwrap();
    rig.deliver("expires-session");
    let record = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|ext| ext == "json")).unwrap();
    rig.clock.advance_secs(2);
    assert_eq!(rig.env.vigil.notice_list().unwrap()[0]["expired"], true);
    assert!(!record.exists(), "listing expired notices must sweep dead records");
}

#[test]
fn cleanup_skips_while_directory_lock_is_held_and_never_unlinks_directory_lock() {
    // A busy delivery directory leaves records intact; a later cleanup retires
    // dead records after unlock, and the single `.lock` file is never unlinked.
    let rig = Rig::new();
    rig.add("old", "m");
    rig.deliver("s");
    let dir = rig.env.root().join("notice-delivery");
    let record = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|ext| ext == "json")).unwrap();
    let dir_lock_path = dir.join(".lock");
    let lock = std::fs::File::options().write(true).open(&dir_lock_path).unwrap();
    lock.lock().unwrap();
    rig.env.vigil.notice_rm("old").unwrap();
    assert!(record.exists(), "held directory lock must defer cleanup");
    assert!(dir_lock_path.exists());
    drop(lock);
    rig.add("trigger", "m");
    rig.env.vigil.notice_rm("trigger").unwrap();
    assert!(!record.exists());
    assert!(dir_lock_path.exists(), "directory lock must never be unlinked");
}

// ---- set-of-names identity, bridging, transactions, cancellation --------

#[test]
fn identity_collection_gathers_all_env_and_payload_names_and_uses_ancestor_only_last() {
    // why: SPEC "Notices": all present identity env vars plus hook payload ids
    //      are collected in order and deduplicated; ancestry is used only when
    //      nothing else exists.
    // catches: first-match stopping after T3CODE_THREAD_ID; including ancestor
    //          alongside env or payload; failing to deduplicate or skip blanks.
    use crate::notice_ops::{hook_session_ids, session_keys_with_ancestor};
    use serde_json::json;

    let all_env = |name: &str| match name {
        "T3CODE_THREAD_ID" => Some(" t3-1 ".into()),
        "CLAUDE_CODE_SESSION_ID" => Some("claude-1".into()),
        "CODEX_SESSION_ID" => Some("codex-1".into()),
        "CODEX_THREAD_ID" => Some("codex-1".into()), // duplicate of CODEX_SESSION_ID
        "ANTIGRAVITY_CONVERSATION_ID" => Some("agy-1".into()),
        _ => None,
    };
    let payload = hook_session_ids(&json!({"session_id": "payload-1", "thread_id": "t3-1", "conversationId": "  "}));
    assert_eq!(payload, vec!["payload-1", "t3-1"]);
    let collected = session_keys_with_ancestor(all_env, &payload, || Some("ancestor:999".into()));
    assert_eq!(
        collected,
        vec!["t3-1", "claude-1", "codex-1", "agy-1", "payload-1"],
        "must collect all env + payload names without duplicates and without ancestor"
    );

    // Payload alone suppresses ancestor.
    let payload_only = session_keys_with_ancestor(|_| None, &["hook-only"], || Some("ancestor:999".into()));
    assert_eq!(payload_only, vec!["hook-only"]);

    // Blank env and blank payload fall back to ancestor.
    let ancestor_only = session_keys_with_ancestor(|_| Some("  ".into()), &["   "], || Some("ancestor:999".into()));
    assert_eq!(ancestor_only, vec!["ancestor:999"]);

    // Nothing anywhere returns empty.
    let none = session_keys_with_ancestor::<&str>(|_| None, &[], || None);
    assert!(none.is_empty());
}

#[test]
fn bridging_links_names_in_both_orders_including_out_of_scope_calls_and_states_disjoint_limit() {
    // why: a bridging call seeing {A, B} suppresses notices already in A or B
    //      and copies live delivered generations to every name in the set, even
    //      when called outside a notice's scope; disjoint sets stay separate
    //      until a call sees both.
    // proves: CLAIM(notice-once)
    // catches: skipping write when pending is empty (breaks bridging); checking
    //          only the first name; only bridging in-scope notices.
    let rig = Rig::new();
    let store = rig.vigil().notice_store();
    let now = rig.clock.now();
    let proj = rig.env.dir.path().join("proj");
    let other = rig.env.dir.path().join("other");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(&other).unwrap();

    // Order 1: A -> {A, B} -> B (with scoped notice bridged from outside scope!).
    rig.env
        .vigil
        .notice_add(NoticeRequest { id: "scoped".into(), message: "m".into(), scope: vec![proj.clone()], ..Default::default() })
        .unwrap();
    let a = NoticeSession::single("a").unwrap();
    let b = NoticeSession::single("b").unwrap();
    let ab = NoticeSession::agent(["a", "b"]).unwrap();

    assert_eq!(store.claim(&a, now, &proj).unwrap().keep().notices.len(), 1);
    // Bridge runs in `other` (outside `proj`), prints nothing, links `scoped` to `b`.
    assert!(store.claim(&ab, now, &other).unwrap().keep().notices.is_empty());
    assert!(store.claim(&b, now, &proj).unwrap().keep().notices.is_empty(), "b must see bridged out-of-scope delivery");

    // Steady-state silence on {A, B} writes nothing.
    let snapshot = tree(&rig.env.root());
    assert!(store.claim(&ab, now, &proj).unwrap().keep().notices.is_empty());
    assert_eq!(tree(&rig.env.root()), snapshot, "already-bridged steady state must not rewrite files");

    // Order 2: B -> {A, B} -> A.
    rig.add("second", "2");
    assert_eq!(store.claim(&b, now, &proj).unwrap().keep().notices.len(), 1);
    assert!(store.claim(&ab, now, &proj).unwrap().keep().notices.is_empty());
    assert!(store.claim(&a, now, &proj).unwrap().keep().notices.is_empty());

    // Disjoint limit: X and Y share nothing until a call sees both {X, Y}.
    let x = NoticeSession::single("disjoint-x").unwrap();
    let y = NoticeSession::single("disjoint-y").unwrap();
    let xy = NoticeSession::agent(["disjoint-x", "disjoint-y"]).unwrap();
    assert_eq!(store.claim(&x, now, &proj).unwrap().keep().notices.len(), 2);
    assert_eq!(store.claim(&y, now, &proj).unwrap().keep().notices.len(), 2);
    rig.add("after-bridge", "3");
    assert_eq!(store.claim(&xy, now, &proj).unwrap().keep().notices.len(), 1);
    assert!(store.claim(&x, now, &proj).unwrap().keep().notices.is_empty());
    assert!(store.claim(&y, now, &proj).unwrap().keep().notices.is_empty());
}

#[test]
fn concurrent_overlapping_name_sets_deliver_each_notice_exactly_once() {
    // why: racing threads seeing overlapping subsets ({a,b}, {b,c}, {a}, {c},
    //      {a,b,c}) must serialize on the directory lock and deliver each notice
    //      exactly once across the whole group.
    // proves: CLAIM(notice-once)
    // catches: missing directory lock; split locks per record; partial visibility.
    let rig = Rig::new();
    for n in 0..8 {
        rig.add(&format!("n{n}"), "m");
    }
    let shapes: Vec<Vec<&str>> = vec![
        vec!["a", "b"],
        vec!["b", "c"],
        vec!["a", "b", "c"],
        vec!["a", "b"],
        vec!["b", "c"],
        vec!["a", "b", "c"],
        vec!["a", "c"],
        vec!["a", "b", "c"],
    ];
    let barrier = Arc::new(Barrier::new(shapes.len()));
    let handles: Vec<_> = shapes
        .into_iter()
        .map(|names| {
            let store = NoticeStore { root: rig.env.root() };
            let barrier = barrier.clone();
            let cwd = rig.cwd.clone();
            let now = rig.clock.now();
            let session = NoticeSession::agent(names).unwrap();
            std::thread::spawn(move || {
                barrier.wait();
                store.claim(&session, now, &cwd).unwrap().keep().notices.into_iter().map(|n| n.id).collect::<Vec<_>>()
            })
        })
        .collect();
    let mut all: Vec<String> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
    all.sort();
    let mut expected: Vec<String> = (0..8).map(|n| format!("n{n}")).collect();
    expected.sort();
    assert_eq!(all, expected, "every notice delivered exactly once across overlapping sets");

    let store = rig.vigil().notice_store();
    for name in ["a", "b", "c"] {
        let single = NoticeSession::single(name).unwrap();
        assert!(
            store.claim(&single, rig.clock.now(), &rig.cwd).unwrap().keep().notices.is_empty(),
            "name {name} missed a delivery"
        );
    }
}

#[test]
fn interrupted_claim_keeps_written_names_without_a_journal() {
    // CLAIM(notice-once): printing follows a full commit. An interrupted
    // claim may lose delivery under A; B can receive it once and bridge A.
    use crate::notice::WriteAction;
    let rig = Rig::new();
    rig.add("fresh", "m");
    let store = rig.vigil().notice_store();
    let now = rig.clock.now();
    let a = NoticeSession::single("a").unwrap();
    let b = NoticeSession::single("b").unwrap();
    let ab = NoticeSession::agent(["a", "b"]).unwrap();
    assert!(store.claim_with_write_hook(&ab, now, &rig.cwd, |index, _| {
        if index == 1 { WriteAction::Fail(std::io::Error::other("disk full")) } else { WriteAction::Continue }
    }).is_err());
    assert!(!rig.env.root().join("notice-delivery/.tx").exists());
    assert!(store.claim(&a, now, &rig.cwd).unwrap().keep().notices.is_empty());
    assert_eq!(store.claim(&b, now, &rig.cwd).unwrap().keep().notices[0].id, "fresh");
    assert!(store.claim(&ab, now, &rig.cwd).unwrap().keep().notices.is_empty());
}

#[test]
fn interrupted_release_and_overlapping_bridge_can_lose_delivery() {
    // CLAIM(notice-once): failed output releases best effort. A partial
    // release leaves B claimed; {A,B} bridges it back without printing.
    use crate::notice::WriteAction;
    let rig = Rig::new();
    rig.add("fresh", "m");
    let store = rig.vigil().notice_store();
    let now = rig.clock.now();
    let a = NoticeSession::single("a").unwrap();
    let ab = NoticeSession::agent(["a", "b"]).unwrap();
    let claim = store.claim(&ab, now, &rig.cwd).unwrap();
    assert!(claim.release_internal(|index, _| {
        if index == 1 { WriteAction::Fail(std::io::Error::other("disk full")) } else { WriteAction::Continue }
    }).is_err());
    let a_record: serde_json::Value = serde_json::from_slice(&std::fs::read(
        rig.env.root().join("notice-delivery").join("cd2c73a4b7ee7306e63a70685a60788d.json")
    ).unwrap()).unwrap();
    assert_eq!(a_record["delivered"], serde_json::json!({}));
    assert!(store.claim(&ab, now, &rig.cwd).unwrap().keep().notices.is_empty());
    assert!(store.claim(&a, now, &rig.cwd).unwrap().keep().notices.is_empty());

    rig.add("next", "m");
    let claim = store.claim(&ab, now, &rig.cwd).unwrap();
    let bc = NoticeSession::agent(["b", "c"]).unwrap();
    assert!(store.claim(&bc, now, &rig.cwd).unwrap().keep().notices.is_empty());
    claim.release();
    assert!(store.claim(&bc, now, &rig.cwd).unwrap().keep().notices.is_empty());
    assert!(store.claim(&ab, now, &rig.cwd).unwrap().keep().notices.is_empty());
}

#[tokio::test(start_paused = true)]
async fn cancelled_keepalive_releases_multi_name_claim_across_all_names_and_keeps_bridged_and_newer() {
    // why: N3 cancellation on a multi-name session must release ONLY the
    //      generations that call claimed across all names, preserving bridged
    //      deliveries and never undoing a newer re-added generation.
    // proves: CLAIM(notice-once)
    // catches: releasing only the primary name; releasing bridged notices;
    //          releasing by id without checking generation timestamp.
    let clock = Arc::new(TokioClock::new(spec_instant()));
    let mut rig = Rig::new();
    rig.env = Env::with_clock(clock);
    let one = fired_watch(&rig, "w1").await;
    let two = fired_watch(&rig, "w2").await;

    rig.env.vigil.notice_add(NoticeRequest { id: "prior".into(), message: "old".into(), ..Default::default() }).unwrap();
    let only_t3 = rig.env.another_process().with_notice_session_keys(vec!["t3".into()]);
    assert_eq!(only_t3.claim_notices(None, None).unwrap().keep().notices.len(), 1);

    rig.env.vigil.notice_add(NoticeRequest { id: "fresh".into(), message: "new".into(), ..Default::default() }).unwrap();
    let both = rig.env.another_process().with_notice_session_keys(vec!["t3".into(), "codex".into()]);
    let (envelope, delivery) = both
        .keepalive_uncommitted(request(one), &crate::keepalive::NoProgress, std::future::pending::<()>())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(envelope.notices.len(), 1);
    assert_eq!(envelope.notices[0].id, "fresh");
    drop(delivery); // cancelled before commit!

    // Calling via `codex` alone sees `fresh` (released on every name) and does
    // NOT see `prior` (bridged before release, never claimed by the cancelled call).
    let only_codex = rig.env.another_process().with_notice_session_keys(vec!["codex".into()]);
    let retried = only_codex.keepalive(request(two)).await.unwrap();
    let retried_ids: Vec<_> = retried.notices.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(retried_ids, vec!["fresh"]);

    // Stale multi-name release does not undo a newer generation of a re-added id.
    rig.env.vigil.notice_add(NoticeRequest { id: "gen".into(), message: "v1".into(), ..Default::default() }).unwrap();
    let stale = both.claim_notices(None, None).unwrap();
    rig.env.vigil.notice_rm("gen").unwrap();
    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    rig.env.vigil.notice_add(NoticeRequest { id: "gen".into(), message: "v2".into(), ..Default::default() }).unwrap();
    assert_eq!(both.claim_notices(None, None).unwrap().keep().notices[0].message, "v2");
    stale.release();
    assert!(only_t3.claim_notices(None, None).unwrap().keep().notices.is_empty());
    assert!(only_codex.claim_notices(None, None).unwrap().keep().notices.is_empty());
}

#[test]
fn corrupt_record_is_isolated_to_calls_seeing_that_name() {
    // why: CLAIM(notice-records): corrupting one name's record errors any call
    //      that includes that name without overwriting any file, while another
    //      name that does not include the corrupt file continues working.
    // proves: CLAIM(notice-records)
    // catches: single shared file where one corrupt byte breaks every session;
    //          multi-name claim partially updating healthy records before
    //          detecting a corrupt sibling record.
    let rig = Rig::new();
    rig.add("live", "m");
    let store = rig.vigil().notice_store();
    let now = rig.clock.now();
    let a = NoticeSession::single("iso-a").unwrap();
    let b = NoticeSession::single("iso-b").unwrap();
    let ab = NoticeSession::agent(["iso-a", "iso-b"]).unwrap();

    assert_eq!(store.claim(&b, now, &rig.cwd).unwrap().keep().notices.len(), 1);
    let b_path = std::fs::read_dir(rig.env.root().join("notice-delivery"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|ext| ext == "json"))
        .unwrap();
    std::fs::write(&b_path, b"{corrupt").unwrap();

    // `{a, b}` errors without writing `a` or modifying `b`.
    let err = store.claim(&ab, now, &rig.cwd);
    assert!(err.is_err());
    assert_eq!(std::fs::read(&b_path).unwrap(), b"{corrupt");
    let json_count = std::fs::read_dir(rig.env.root().join("notice-delivery"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .count();
    assert_eq!(json_count, 1, "failed multi-name claim must not create sibling record");

    // `a` alone is completely unaffected by `b`'s corruption.
    assert_eq!(store.claim(&a, now, &rig.cwd).unwrap().keep().notices.len(), 1);
}

#[test]
fn release_waits_past_claim_timeout_for_a_busy_directory() {
    // CLAIM(notice-once): failed-print release gets a longer contention budget.
    // A handshake ensures the competing lock is held before release starts.
    let rig = Rig::new();
    rig.add("fresh", "m");
    let store = rig.vigil().notice_store();
    let session = NoticeSession::single("busy-release").unwrap();
    let claim = store.claim(&session, rig.clock.now(), &rig.cwd).unwrap();
    let path = rig.env.root().join("notice-delivery/.lock");
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let blocker = std::thread::spawn(move || {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.lock().unwrap();
        ready_tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
    });
    ready_rx.recv().unwrap();
    let result = claim.release_internal(|_, _| crate::notice::WriteAction::Continue);
    blocker.join().unwrap();
    result.unwrap();
    assert_eq!(store.claim(&session, rig.clock.now(), &rig.cwd).unwrap().keep().notices[0].id, "fresh");
}
