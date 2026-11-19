//! Feature-only tests. Time, process output and cwd are explicit; no real config.
use crate::KeepaliveRequest;
use crate::reminder::{Reminder, ReminderKind};
use crate::testing::*;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

fn spec(extra: Value) -> Value {
    let mut raw = json!({"scope":["/repos/edge"],"days":["wed","thu","fri"],"tz":"America/Denver","window":"09:00-18:00","message":"Fork release due", "period":"week"});
    for (key, value) in extra.as_object().unwrap() {
        raw[key] = value.clone();
    }
    raw
}
fn reminder(extra: Value) -> Reminder {
    Reminder::new("release", serde_json::from_value(spec(extra)).unwrap()).unwrap()
}

#[test]
fn schedule_edges_and_timezone() {
    // WHY: CLAIM(reminder-time): Denver is UTC-7 in November, UTC-6 in July;
    // local Wed 09:00 is inclusive and 18:00 exclusive, Sat is excluded.
    let rule = reminder(json!({}));
    for (utc, due) in [
        ("2026-11-11T15:59:59Z", false),
        ("2026-11-11T16:00:00Z", true),
        ("2026-11-12T00:59:59Z", true),
        ("2026-11-12T01:00:00Z", false),
        ("2026-11-13T16:00:00Z", true),
        ("2026-11-14T16:00:00Z", false),
        ("2026-07-08T15:00:00Z", true),
        ("2026-07-08T14:59:59Z", false),
    ] {
        assert_eq!(rule.schedule(at(utc)).0, due, "{utc}");
    }
    let midnight = reminder(json!({"window":null}));
    assert!(!midnight.schedule(at("2026-11-11T06:59:59Z")).0); // Tue 23:59:59
    assert!(midnight.schedule(at("2026-11-11T07:00:00Z")).0); // Wed 00:00
    assert_eq!(rule.schedule(at("2027-01-01T16:00:00Z")).1, "2026-W53");
}

#[test]
fn repeated_dst_hour_uses_each_instants_local_wall_time() {
    // WHY: the fall-back hour occurs twice; both 01:30 instants in Denver
    // satisfy the same window, and 02:00 is outside it (CLAIM(reminder-time)).
    let rule = reminder(json!({"days":["sun"], "window":"01:15-01:45"}));
    assert!(rule.schedule(at("2026-11-01T07:30:00Z")).0);
    assert!(rule.schedule(at("2026-11-01T08:30:00Z")).0);
    assert!(!rule.schedule(at("2026-11-01T09:00:00Z")).0);
}

#[test]
fn scope_uses_components() {
    // WHY: CLAIM(reminder-scope): edge-heavy and sibling trees are independent.
    let rule = reminder(json!({}));
    for (path, due) in [
        ("/repos/edge", true),
        ("/repos/edge/lib", true),
        ("/repos/edge-heavy", false),
        ("/repos/edge.research", false),
        ("/repos/edge.worktrees/a", false),
        ("/other/repos/edge", false),
        ("/repos/edge/../other", false),
    ] {
        assert_eq!(rule.in_scope(Path::new(path)), due, "{path}");
    }
    let siblings =
        reminder(json!({"scope":["/repos/edge", "/repos/edge.research", "/repos/edge.worktrees"]}));
    assert!(siblings.in_scope(Path::new("/repos/edge.research/notes")));
    assert!(siblings.in_scope(Path::new("/repos/edge.worktrees/feature")));
    assert!(!siblings.in_scope(Path::new("/repos/edge.worktrees-old")));
}

#[test]
fn invalid_specs_refuse_ambiguous_schedules_and_unbounded_probes() {
    // WHY: bad operator input must not become an always-due rule or hang waits.
    for extra in [
        json!({"days":["wednesday"]}),
        json!({"days":[]}),
        json!({"scope":["relative"]}),
        json!({"tz":"Mars"}),
        json!({"window":"18:00-09:00"}),
        json!({"window":"09:00-09:00"}),
        json!({"probe":[]}),
        json!({"timeout_s":20}),
        json!({"cache_s":86401}),
        json!({"condition":"value > 1"}),
        json!({"probe":["x"],"condition":"value < missing"}),
    ] {
        assert!(
            Reminder::new("r", serde_json::from_value(spec(extra.clone())).unwrap()).is_err(),
            "{extra}"
        );
    }
}

struct Rig {
    env: Env,
    cwd: std::path::PathBuf,
}
impl Rig {
    fn new(now: &str) -> Self {
        let mut env = Env::with_clock_and_config(Arc::new(TokioClock::new(at(now))), default_config());
        // Keep the config under Env's tempdir instead of a real user file.
        env.config.reminders_file = Some(env.dir.path().join("reminders.toml"));
        let cwd = env.dir.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        // Reminder latency is measured in virtual time, including the credit
        // check. A real `uw` child can run past a paused-time timeout.
        env.vigil = env.another_process_in(&cwd)
            .with_credit_probe(ScriptedCreditProbe::new(credit_unknown(crate::credit::CreditSource::Transcripts)));
        Self { env, cwd }
    }
    fn add(&self, extra: Value) {
        let mut raw = spec(
            json!({"scope":[self.cwd],"days":["mon","tue","wed","thu","fri","sat","sun"],"window":null}),
        );
        for (k, v) in extra.as_object().unwrap() {
            raw[k] = v.clone();
        }
        self.env.vigil.remind_add("release", raw).unwrap();
    }
    async fn kind(&self) -> Value {
        self.env.vigil.remind_show("release").await.unwrap()["evaluation"]["kind"].clone()
    }
    async fn quiet(&self, name: &str) -> crate::WatchId {
        let path = self.env.input_path(name);
        write_file(&path, "quiet");
        self.env
            .vigil
            .create(change_request(name, &path, 60))
            .await
            .unwrap()
            .watch_id
    }
}

#[tokio::test(start_paused = true)]
async fn done_and_snooze_follow_local_periods_across_processes() {
    // WHY: CLAIM(reminder-time/state): done expires at local Monday, not UTC
    // Monday, and snooze is an absolute duration that may cross periods.
    let rig = Rig::new("2026-11-15T23:00:00Z"); // Sunday 16:00 Denver
    rig.add(json!({}));
    assert_eq!(rig.kind().await, "due");
    rig.env.vigil.remind_done("release").unwrap();
    assert_eq!(
        rig.env
            .another_process_in(&rig.cwd)
            .remind_show("release")
            .await
            .unwrap()["evaluation"]["kind"],
        "done"
    );
    tokio::time::advance(Duration::from_secs(7 * 3600 + 3599)).await; // Mon 06:59:59Z
    assert_eq!(rig.kind().await, "done");
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(rig.kind().await, "due");
    rig.env.vigil.remind_snooze("release", "1h").unwrap();
    tokio::time::advance(Duration::from_secs(3599)).await;
    assert_eq!(rig.kind().await, "snoozed");
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(rig.kind().await, "due");
    rig.env.vigil.remind_rm("release").unwrap();
    rig.add(json!({"period":"day"}));
    rig.env.vigil.remind_done("release").unwrap();
    tokio::time::advance(Duration::from_secs(22 * 3600 + 3599)).await;
    assert_eq!(rig.kind().await, "done");
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(rig.kind().await, "due");
}

#[tokio::test(start_paused = true)]
async fn probes_due_not_due_failure_and_cache_boundaries() {
    // WHY: a missing release probe must never claim a release is due. Raw
    // success/failure is cached, but edited conditions apply immediately.
    let rig = Rig::new("2026-11-11T16:00:00Z");
    rig.add(json!({"probe":["release-probe"],"cache_s":60,"condition":"value >= 10","message":"{value} changes: {output}"}));
    rig.env.reader.set_text("cmd:release-probe", "exit=0\n12");
    let shown = rig.env.vigil.remind_show("release").await.unwrap();
    assert_eq!(shown["evaluation"]["kind"], "due");
    assert_eq!(shown["evaluation"]["message"], "12 changes: 12");
    rig.env.reader.set_text("cmd:release-probe", "exit=0\n9");
    tokio::time::advance(Duration::from_secs(59)).await;
    assert_eq!(rig.kind().await, "due");
    assert_eq!(rig.env.reader.started_count("cmd:release-probe"), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(rig.kind().await, "not_due");
    rig.env
        .reader
        .set_text("cmd:release-probe", "exit=7\nnetwork gone");
    tokio::time::advance(Duration::from_secs(60)).await;
    assert_eq!(rig.kind().await, "unavailable");
    let list = rig.env.vigil.envelope_reminders().await.unwrap();
    assert_eq!(
        list[0].message,
        "reminder release unavailable: probe exited with 7: network gone"
    );
    rig.env.reader.set_text("cmd:release-probe", "exit=1\n");
    tokio::time::advance(Duration::from_secs(59)).await;
    assert_eq!(rig.kind().await, "unavailable");
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(rig.kind().await, "not_due");
    rig.env
        .reader
        .set_text("cmd:release-probe", "exit=0\nnot a number");
    tokio::time::advance(Duration::from_secs(60)).await;
    assert_eq!(rig.kind().await, "unavailable");
}

#[tokio::test(start_paused = true)]
async fn probe_timeout_and_exit_only_predicate() {
    // WHY: same outer probe deadline as advisories; a hung reader is explicit.
    let rig = Rig::new("2026-11-11T16:00:00Z");
    rig.add(json!({"probe":["release-probe"],"cache_s":0}));
    rig.env
        .reader
        .set_text("cmd:release-probe", "exit=0\nready");
    assert_eq!(rig.kind().await, "due");
    rig.env.reader.set_text("cmd:release-probe", "exit=1\n");
    assert_eq!(rig.kind().await, "not_due");
    rig.env
        .reader
        .set_sleep("cmd:release-probe", Duration::from_secs(3600));
    let start = tokio::time::Instant::now();
    assert_eq!(rig.kind().await, "unavailable");
    assert_eq!(start.elapsed(), Duration::from_secs(5));
}

fn request(id: crate::WatchId, long: bool) -> KeepaliveRequest {
    KeepaliveRequest {
        watch_id: Some(id),
        max_s: Some(if long { 3000 } else { 30 }),
        session: false,
        orchestrator: Some(long),
        confirm: None,
    }
}

#[tokio::test(start_paused = true)]
async fn envelopes_and_advice_only_deliver_in_scope_and_reread_edits() {
    // WHY: exercise production keepalive and advice delivery, including a
    // watch created elsewhere, fired return, quiet return and hand-edited file.
    let rig = Rig::new("2026-11-11T16:00:00Z");
    rig.add(json!({}));
    let id = rig.quiet("quiet").await;
    let envelope = rig
        .env
        .vigil
        .keepalive(request(id.clone(), false))
        .await
        .unwrap();
    assert_eq!(envelope.reminders.unwrap()[0].message, "Fork release due");
    let other = rig.env.another_process_in(rig.env.dir.path());
    let envelope = other.keepalive(request(id.clone(), false)).await.unwrap();
    assert!(
        serde_json::to_value(envelope)
            .unwrap()
            .get("reminders")
            .is_none()
    );
    assert!(other.envelope_reminders().await.is_none());
    assert_eq!(
        rig.env.vigil.advice_report().await.unwrap()["reminders"][0]["kind"],
        "due"
    );
    let path = rig.env.config.reminders_file.as_ref().unwrap();
    let text = std::fs::read_to_string(path)
        .unwrap()
        .replace("Fork release due", "Ship the fork");
    std::fs::write(path, text).unwrap();
    append_file(&rig.env.input_path("quiet"), "changed");
    let fired = rig.env.vigil.keepalive(request(id, false)).await.unwrap();
    assert_eq!(fired.status, crate::WatchStatus::Fired);
    assert_eq!(fired.reminders.unwrap()[0].message, "Ship the fork");
    rig.env.vigil.remind_done("release").unwrap();
    assert_eq!(rig.env.vigil.envelope_reminders().await.unwrap().len(), 0);
}

#[tokio::test(start_paused = true)]
async fn mid_wait_event_fires_once_and_rearms_next_period() {
    // WHY: CLAIM(reminder-event): a long call starting before the window must
    // end when due, with one event; a fresh process cannot re-fire this week.
    // Virtual time only; this does not run an orchestrator CLI to wait on work.
    let rig = Rig::new("2026-11-11T15:59:40Z");
    rig.add(json!({"window":"09:00-18:00"}));
    let id = rig.quiet("wait").await;
    let result = rig
        .env
        .vigil
        .keepalive(request(id.clone(), true))
        .await
        .unwrap();
    let wire = serde_json::to_value(&result).unwrap();
    assert_eq!(wire["status"], "pending");
    assert_eq!(wire["next"], "act on events");
    assert_eq!(wire["events"].as_array().unwrap().len(), 1);
    assert_eq!(wire["events"][0]["kind"], "reminder_due");
    assert_eq!(wire["events"][0]["reminder_id"], "release");
    assert_eq!(wire["events"][0]["period"], "2026-W46");
    assert_eq!(result.waited_s, Some(30));
    let second = rig
        .env
        .another_process_in(&rig.cwd)
        .keepalive(request(id, true))
        .await
        .unwrap();
    assert!(second.events.is_empty());
    assert_eq!(second.reminders.unwrap()[0].kind, ReminderKind::Due);
    let store = rig.env.vigil.reminder_store().unwrap();
    assert!(
        store
            .claim_event("release", "2026-W47", at("2026-11-18T16:00:00Z"))
            .unwrap()
    );
}

#[test]
fn event_claim_is_atomic() {
    // WHY: CLAIM(reminder-state): concurrent sessions must not both notify.
    let rig = Rig::new("2026-11-11T16:00:00Z");
    rig.add(json!({}));
    let store = rig.env.vigil.reminder_store().unwrap();
    let gate = Arc::new(std::sync::Barrier::new(3));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let store = store.clone();
            let gate = gate.clone();
            std::thread::spawn(move || {
                gate.wait();
                store
                    .claim_event("release", "2026-W46", at("2026-11-11T16:00:00Z"))
                    .unwrap_or(false)
            })
        })
        .collect();
    gate.wait();
    assert_eq!(
        handles
            .into_iter()
            .map(|h| h.join().unwrap() as usize)
            .sum::<usize>(),
        1
    );
    assert!(
        !store
            .claim_event("release", "2026-W46", at("2026-11-11T16:00:00Z"))
            .unwrap()
    );
}

#[tokio::test(start_paused = true)]
async fn invalid_hand_edit_and_corrupt_state_are_unavailable() {
    // WHY: malformed config/state must not masquerade as due or disappear in
    // the active project; a broken neighbor must not leak outside its scope.
    let rig = Rig::new("2026-11-11T16:00:00Z");
    rig.add(json!({}));
    let path = rig.env.config.reminders_file.as_ref().unwrap();
    std::fs::write(
        path,
        std::fs::read_to_string(path)
            .unwrap()
            .replace("America/Denver", "Mars"),
    )
    .unwrap();
    assert_eq!(
        rig.env.vigil.envelope_reminders().await.unwrap()[0].kind,
        ReminderKind::Unavailable
    );
    assert!(
        rig.env
            .another_process_in(rig.env.dir.path())
            .envelope_reminders()
            .await
            .is_none()
    );
    rig.env.vigil.remind_rm("release").unwrap();
    rig.add(json!({}));
    std::fs::write(rig.env.root().join("reminders/release.json"), "garbled").unwrap();
    assert_eq!(rig.kind().await, "unavailable");
}

#[tokio::test(start_paused = true)]
async fn file_wide_failure_has_a_diagnostic_without_leaking_a_scope() {
    // WHY: even when scope cannot be recovered, a config failure must be
    // visible without delivering project messages outside a known scope.
    let rig = Rig::new("2026-11-11T16:00:00Z");
    rig.add(json!({}));
    std::fs::write(rig.env.config.reminders_file.as_ref().unwrap(), "[broken").unwrap();
    let report = rig.env.vigil.advice_report().await.unwrap();
    assert!(report.get("reminders").is_none());
    assert!(
        report["reminders_error"]
            .as_str()
            .unwrap()
            .starts_with("reminders unavailable:")
    );
    let id = rig.quiet("broken-config").await;
    let envelope = rig.env.vigil.keepalive(request(id, false)).await.unwrap();
    assert!(envelope.reminders.is_none());
    assert!(envelope.reminders_error.unwrap().contains("reminders.toml"));
}

#[tokio::test(start_paused = true)]
async fn edited_condition_reuses_output_and_suppression_skips_probes() {
    // WHY: raw output belongs to a probe, not the condition/message; editing
    // a threshold must apply immediately without extra commands. Out-of-scope,
    // outside-schedule, done and snoozed evaluations must not launch probes.
    let rig = Rig::new("2026-11-11T16:00:00Z");
    rig.add(json!({"probe":["release-probe"],"condition":"value >= 10","cache_s":600}));
    rig.env.reader.set_text("cmd:release-probe", "exit=0\n12");
    assert_eq!(rig.kind().await, "due");
    let path = rig.env.config.reminders_file.as_ref().unwrap();
    std::fs::write(
        path,
        std::fs::read_to_string(path)
            .unwrap()
            .replace("value >= 10", "value >= 20"),
    )
    .unwrap();
    assert_eq!(rig.kind().await, "not_due");
    assert_eq!(rig.env.reader.started_count("cmd:release-probe"), 1);
    rig.env.vigil.remind_done("release").unwrap();
    tokio::time::advance(Duration::from_secs(601)).await;
    assert_eq!(rig.kind().await, "done");
    assert!(
        rig.env
            .another_process_in(rig.env.dir.path())
            .envelope_reminders()
            .await
            .is_none()
    );
    assert_eq!(rig.env.reader.started_count("cmd:release-probe"), 1);
    rig.env.vigil.remind_rm("release").unwrap();
    rig.add(json!({"probe":["release-probe"],"window":"17:00-18:00"}));
    assert_eq!(rig.kind().await, "outside_schedule");
    assert_eq!(rig.env.reader.started_count("cmd:release-probe"), 1);
    rig.env.vigil.remind_rm("release").unwrap();
    rig.add(json!({"probe":["release-probe"]}));
    rig.env.vigil.remind_snooze("release", "1h").unwrap();
    assert_eq!(rig.kind().await, "snoozed");
    assert_eq!(rig.env.reader.started_count("cmd:release-probe"), 1);
}

#[tokio::test(start_paused = true)]
async fn numeric_json_extraction_and_missing_input_do_not_invent_a_value() {
    // WHY: reuse advisory JSON extraction with a named value and parameter;
    // absent input is unavailable, never a zero that accidentally passes.
    let rig = Rig::new("2026-11-11T16:00:00Z");
    rig.add(json!({"probe":["release-probe"], "extract":{"json":"changed", "name":"lines"}, "params":{"minimum":100}, "condition":"lines >= minimum", "cache_s":0, "message":"{value} lines"}));
    rig.env
        .reader
        .set_text("cmd:release-probe", "exit=0\n{\"changed\":100}");
    let notice = rig.env.vigil.remind_show("release").await.unwrap();
    assert_eq!(notice["evaluation"]["message"], "100 lines");
    assert_eq!(notice["evaluation"]["kind"], "due");
    rig.env.reader.set_text("cmd:release-probe", "exit=0\n{}");
    assert_eq!(rig.kind().await, "unavailable");
}

#[tokio::test(start_paused = true)]
async fn slow_probe_cannot_deliver_after_window_end_or_concurrent_done() {
    // WHY: schedule and suppression must still hold when the answer arrives,
    // not merely when a command starts (CLAIM(reminder-time/state)).
    let rig = Rig::new("2026-11-12T00:59:58Z"); // Wed 17:59:58 Denver
    rig.add(json!({"window":"09:00-18:00", "probe":["release-probe"], "cache_s":0}));
    rig.env
        .reader
        .set_text("cmd:release-probe", "exit=0\nready");
    rig.env
        .reader
        .set_sleep("cmd:release-probe", Duration::from_secs(3));
    assert_eq!(rig.kind().await, "outside_schedule");
    let rig = Rig::new("2026-11-11T16:00:00Z");
    rig.add(json!({"probe":["release-probe"], "cache_s":0}));
    rig.env
        .reader
        .set_text("cmd:release-probe", "exit=0\nready");
    rig.env
        .reader
        .set_sleep("cmd:release-probe", Duration::from_secs(5));
    let (kind, ()) = tokio::join!(rig.kind(), async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.env.vigil.remind_done("release").unwrap();
    });
    assert_eq!(kind, "done");
}

#[tokio::test(start_paused = true)]
async fn cold_reminder_probes_never_delay_returns_or_advice() {
    // WHY: R2 / CLAIM(reminder-latency): a 15s reminder cannot delay a terminal
    // watch, a budget return or advice, or consume the advice hook's 20s limit.
    for route in ["terminal", "return", "advice"] {
        let rig = Rig::new("2026-11-11T16:00:00Z");
        rig.add(json!({"probe":["release-probe"], "timeout_s":19}));
        rig.env.reader.set_text("cmd:release-probe", "exit=0\nready");
        rig.env.reader.set_sleep("cmd:release-probe", Duration::from_secs(15));
        let id = rig.quiet(route).await;
        if route == "terminal" {
            append_file(&rig.env.input_path(route), "changed");
            rig.env.vigil.tick_due(&id).await.unwrap();
        }
        let started = tokio::time::Instant::now();
        if route == "advice" {
            rig.env.vigil.advice_report().await.unwrap();
            assert!(started.elapsed() < Duration::from_secs(1), "advice waited for reminder");
        } else {
            let result = rig.env.vigil.keepalive(request(id, route == "terminal")).await.unwrap();
            if route == "terminal" {
                assert_eq!(result.status, crate::WatchStatus::Fired);
                assert!(started.elapsed() < Duration::from_secs(1), "terminal waited for reminder");
            } else {
                assert!(started.elapsed() <= Duration::from_secs(30), "return exceeded max_s for reminder");
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn all_cold_probes_start_concurrently_and_cache_the_total_deadline() {
    // WHY: R2: no serial N*timeout cost, no probe starvation behind a hung
    // neighbor, and failures last beyond the next 30s check, even cache_s=0.
    let rig = Rig::new("2026-11-11T16:00:00Z");
    rig.add(json!({"probe":["release-probe"], "cache_s":0}));
    let mut other = spec(json!({"scope":[rig.cwd], "window":null, "probe":["other-probe"], "cache_s":0}));
    other["days"] = json!(["wed"]);
    rig.env.vigil.remind_add("other", other).unwrap();
    for label in ["cmd:release-probe", "cmd:other-probe"] {
        rig.env.reader.set_text(label, "exit=0\nready");
        rig.env.reader.set_sleep(label, Duration::from_secs(15));
    }
    let started = tokio::time::Instant::now();
    rig.env.vigil.envelope_reminders().await;
    assert!(started.elapsed() < Duration::from_secs(1), "cache delivery awaited a probe");
    tokio::time::sleep(Duration::from_secs(6)).await;
    for label in ["cmd:release-probe", "cmd:other-probe"] {
        assert_eq!(rig.env.reader.started_count(label), 1, "every probe starts in the same batch");
    }
    let notices = rig.env.vigil.envelope_reminders().await.unwrap();
    assert_eq!(notices.len(), 2);
    assert!(notices.iter().all(|n| n.kind == ReminderKind::Unavailable && n.message.contains("5s")), "{notices:?}");
    tokio::time::sleep(Duration::from_secs(30)).await;
    rig.env.vigil.envelope_reminders().await;
    tokio::task::yield_now().await;
    for label in ["cmd:release-probe", "cmd:other-probe"] {
        assert_eq!(rig.env.reader.started_count(label), 1, "failure was retried before the check interval");
    }
}

#[test]
fn output_interpolation_has_a_utf8_byte_bound() {
    // WHY: C3: a verbose probe must not inject 64KiB into every envelope.
    let rule = reminder(json!({"message":"Check: {output}"}));
    let (_, message, _) = rule.judge(&"é".repeat(1000)).unwrap();
    assert!(message.len() <= "Check: ".len() + 400);
    assert!(message.ends_with("..."));
}

#[tokio::test(start_paused = true)]
async fn a_background_probe_cannot_hold_a_watch_fired_by_another_process() {
    // WHY: R2 / CLAIM(S4-outbox): while probes sleep, external terminal state
    // must still be observed on the next <=5s outbox pass.
    let rig = Rig::new("2026-11-11T16:00:00Z");
    rig.add(json!({"probe":["release-probe"], "timeout_s":19}));
    rig.env.reader.set_text("cmd:release-probe", "exit=0\nready");
    rig.env.reader.set_sleep("cmd:release-probe", Duration::from_secs(15));
    let id = rig.quiet("external").await;
    let other = rig.env.another_process_in(&rig.cwd);
    let started = tokio::time::Instant::now();
    let (result, ()) = tokio::join!(rig.env.vigil.keepalive(request(id.clone(), true)), async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        append_file(&rig.env.input_path("external"), "changed");
        let mut watch = other.load_watch(&id).unwrap();
        watch.last_tick_at = None; // Simulate an independent due tick.
        other.store().save(&watch).unwrap();
        other.tick_due(&id).await.unwrap();
    });
    assert_eq!(result.unwrap().status, crate::WatchStatus::Fired);
    assert!(started.elapsed() <= Duration::from_secs(5), "watch waited for background probe");
}

#[tokio::test(start_paused = true)]
async fn a_completed_background_probe_is_delivered_on_a_later_pass() {
    // WHY: R2: spawning alone is insufficient; a later sense pass must pick
    // up the cached zero-TTL result and claim one early event without ending
    // the watch. A new background refresh may start while delivering it.
    let rig = Rig::new("2026-11-11T16:00:00Z");
    rig.add(json!({"probe":["release-probe"], "cache_s":0}));
    rig.env.reader.set_text("cmd:release-probe", "exit=0\nready");
    rig.env.reader.set_sleep("cmd:release-probe", Duration::from_secs(3));
    let id = rig.quiet("later").await;
    let result = rig.env.vigil.keepalive(request(id, true)).await.unwrap();
    assert_eq!(result.waited_s, Some(30));
    assert_eq!(result.status, crate::WatchStatus::Pending);
    assert_eq!(result.events.len(), 1);
    assert_eq!(result.events[0].input, "reminder:release");
    assert_eq!(result.reminders.unwrap()[0].checked_at, at("2026-11-11T16:00:03Z"), "delivery reuses the first probe's snapshot at the 30s pass");
}

#[tokio::test(start_paused = true)]
async fn a_slow_reminder_keeps_the_existing_advice_within_the_hook_timeout() {
    // WHY: R2: a cold 15s boat-budget probe still reaches timeout-20's stdout
    // when there is also a cold 15s reminder. Reminders add zero probe wait.
    let mut rig = Rig::new("2026-11-11T16:00:00Z");
    rig.env.config.advisories_file = Some(rig.env.dir.path().join("advisories.toml"));
    std::fs::write(rig.env.config.advisories_file.as_ref().unwrap(), "[advisories]\n").unwrap();
    rig.env.vigil = rig.env.another_process_in(&rig.cwd);
    rig.env.vigil.advisory_add("budget", json!({
        "probe":{"kind":"cmd","argv":["budget-probe"],"timeout_s":19},
        "extract":{"name":"free","json":"free"}, "condition":"free > 0",
        "message_when_true":"Use Boat", "message_when_false":"Use qex"
    }), false).unwrap();
    rig.add(json!({"probe":["release-probe"], "timeout_s":19}));
    rig.env.reader.set_text("cmd:budget-probe", "exit=0\n{\"free\":15}");
    for label in ["cmd:release-probe", "cmd:budget-probe"] {
        rig.env.reader.set_sleep(label, Duration::from_secs(15));
    }
    let started = tokio::time::Instant::now();
    let report = tokio::time::timeout(Duration::from_secs(20), rig.env.vigil.advice_report()).await.unwrap().unwrap();
    assert_eq!(report["advice"][0]["message"], "Use Boat");
    assert_eq!(started.elapsed(), Duration::from_secs(15));
}

#[test]
fn removing_and_readding_recovers_corrupt_suppression_and_cache() {
    // WHY: R4: the documented rm/add recovery must work without parsing the
    // corrupt state it is supposed to replace, and must discard old cache.
    let rig = Rig::new("2026-11-11T16:00:00Z");
    rig.add(json!({}));
    let state = rig.env.root().join("reminders/release.json");
    let cache = rig.env.root().join("reminder-cache/release.json");
    std::fs::write(&state, "broken").unwrap();
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    std::fs::write(&cache, "old").unwrap();
    rig.env.vigil.remind_rm("release").unwrap();
    assert!(!state.exists());
    assert!(!cache.exists());
    // Even an orphaned corrupt state before add must be reset directly.
    std::fs::write(&state, "broken again").unwrap();
    rig.add(json!({}));
    let state = rig.env.vigil.reminder_store().unwrap().state("release").unwrap();
    assert_eq!(state.done_period, None);
    assert_eq!(state.event_period, None);
}

#[cfg(unix)]
#[test]
fn an_unchanged_event_claim_does_not_replace_the_state_file() {
    // WHY: R3: repeated checks of an already claimed period must not rewrite
    // and fsync the same state every 30s. Replacement changes the inode.
    use std::os::unix::fs::MetadataExt;
    let rig = Rig::new("2026-11-11T16:00:00Z");
    rig.add(json!({}));
    let store = rig.env.vigil.reminder_store().unwrap();
    let now = at("2026-11-11T16:00:00Z");
    assert!(store.claim_event("release", "2026-W46", now).unwrap());
    let path = rig.env.root().join("reminders/release.json");
    let inode = std::fs::metadata(&path).unwrap().ino();
    assert!(!store.claim_event("release", "2026-W46", now).unwrap());
    assert_eq!(std::fs::metadata(path).unwrap().ino(), inode);
}
