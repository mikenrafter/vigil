//! Shared CLI/MCP operations and envelope evaluation. Probes are trusted local
//! operator commands, with the same reader and deadline as advisories.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::advisory::probe_body;
use crate::advisory_eval::read_probe;
use crate::clock::Timestamp;
use crate::engine::Vigil;
use crate::reminder::{Reminder, ReminderKind, ReminderNotice, Result, invalid};
use crate::reminder_store::ReminderStore;
use crate::probe_cache::ProbeCache;
use std::sync::Arc;
use tokio::time::{Duration, Instant};

pub(crate) const REMINDER_REFRESH_BUDGET: Duration = Duration::from_secs(5);
pub(crate) const REMINDER_CHECK_INTERVAL: Duration = Duration::from_secs(30);
const REMINDER_FAILURE_CACHE_S: u64 = 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ProbeOutcome {
    Output { output: String },
    NotDue,
    Failed { reason: String },
}
#[derive(Clone)]
struct ReminderContext {
    store: ReminderStore,
    clock: Arc<dyn crate::clock::Clock>,
    reader: Arc<dyn crate::inputs::InputReader>,
    cwd: std::path::PathBuf,
}

pub fn duration_seconds(text: &str) -> Result<u64> {
    let (digits, scale) = match text.as_bytes().last() {
        Some(b's') => (&text[..text.len() - 1], 1),
        Some(b'm') => (&text[..text.len() - 1], 60),
        Some(b'h') => (&text[..text.len() - 1], 3600),
        Some(b'd') => (&text[..text.len() - 1], 86400),
        _ => (text, 1),
    };
    let seconds = digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(scale))
        .filter(|n| (1..=604800).contains(n))
        .ok_or_else(|| invalid("duration must be 1s..=7d (s, m, h, d or bare seconds)"))?;
    Ok(seconds)
}

impl Vigil {
    pub fn reminder_store(&self) -> Result<ReminderStore> {
        Ok(ReminderStore {
            path: self
                .config
                .reminders_file
                .clone()
                .ok_or_else(|| invalid("reminders are off"))?,
            root: self.store.root().into(),
        })
    }
    pub fn remind_add(&self, id: &str, spec: Value) -> Result<Value> {
        self.reminder_store()?.add(id, spec)
    }
    pub fn remind_rm(&self, id: &str) -> Result<Value> {
        self.reminder_store()?.remove(id)
    }
    pub fn remind_list(&self) -> Result<Value> {
        Ok(Value::Array(
            self.reminder_store()?
                .entries()?
                .into_iter()
                .map(|entry| match entry.reminder {
                    Ok(reminder) => json!({"id": entry.id, "spec": reminder.spec}),
                    Err(error) => json!({"id": entry.id, "error": error.to_string()}),
                })
                .collect(),
        ))
    }
    pub async fn remind_show(&self, id: &str) -> Result<Value> {
        let store = self.reminder_store()?;
        let reminder = store.get(id)?;
        Ok(
            json!({"id": id, "spec": reminder.spec, "evaluation": self.reminder_context(store.clone()).evaluate_reminder(&reminder, true, Instant::now() + REMINDER_REFRESH_BUDGET).await}),
        )
    }
    pub fn remind_done(&self, id: &str) -> Result<Value> {
        let store = self.reminder_store()?;
        let reminder = store.get(id)?;
        let (_, period) = reminder.schedule(self.clock.now());
        store.update_state(id, |state| {
            state.done_period = Some(period.clone());
            Ok(())
        })?;
        Ok(json!({"id": id, "done": period}))
    }
    pub fn remind_snooze(&self, id: &str, duration: &str) -> Result<Value> {
        let seconds = duration_seconds(duration)?;
        let store = self.reminder_store()?;
        store.get(id)?;
        let until = self.clock.now().plus_secs(seconds);
        store.update_state(id, |state| {
            state.snooze_until = Some(until);
            Ok(())
        })?;
        Ok(json!({"id": id, "snooze_until": until}))
    }
    fn reminder_context(&self, store: ReminderStore) -> ReminderContext {
        ReminderContext { store, clock: self.clock.clone(), reader: self.reader.clone(), cwd: self.cwd.clone() }
    }

    fn spawn_reminder_refresh(&self, context: ReminderContext, reminders: Vec<Reminder>) {
        if reminders.is_empty() { return; }
        let mut slot = self.reminder_refresh.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_ref().is_some_and(|task| !task.is_finished()) { return; }
        *slot = Some(tokio::spawn(async move {
            // CLAIM(reminder-latency): all eligible probes share this deadline;
            // delivery reads only the disk cache [SPEC Reminders; reminder_tests].
            let deadline = Instant::now() + REMINDER_REFRESH_BUDGET;
            let mut tasks = tokio::task::JoinSet::new();
            for reminder in reminders {
                let context = context.clone();
                tasks.spawn(async move { context.evaluate_reminder(&reminder, true, deadline).await });
            }
            while tasks.join_next().await.is_some() {}
        }));
    }
}

impl ReminderContext {
    async fn reminder_probe(&self, reminder: &Reminder, refresh: bool, deadline: Instant) -> (ProbeOutcome, Timestamp) {
        let Some(probe) = reminder.probe() else {
            return (ProbeOutcome::Output { output: String::new() }, self.clock.now());
        };
        let cache = ProbeCache {
            path: self.store.root.join("reminder-cache").join(format!("{}.json", reminder.id)),
            fingerprint: serde_json::to_string(&probe).expect("input serializes"),
            // Delivery must retain even a zero-TTL result long enough for the
            // next 30s pass; explicit refresh/show still honors cache_s exactly.
            ttl_s: if refresh { reminder.spec.cache_s } else { reminder.spec.cache_s.max(60) },
            failure_ttl_s: REMINDER_FAILURE_CACHE_S,
        };
        if let Some(record) = cache.read(self.clock.now(), |outcome| match outcome {
            ProbeOutcome::Failed { .. } => true,
            ProbeOutcome::Output { output } => reminder.judge(output).is_err(),
            ProbeOutcome::NotDue => false,
        }) {
            return (record.outcome, record.fetched_at);
        }
        if !refresh {
            return (ProbeOutcome::Failed { reason: "probe cache is cold or expired; refresh pending".into() }, self.clock.now());
        }
        let reading = tokio::time::timeout_at(deadline, read_probe(&probe, self.reader.as_ref())).await
            .unwrap_or_else(|_| Err("reminder probes did not answer within shared 5s deadline".into()));
        let outcome = match reading {
            Ok(reading) if !reading.is_error && reading.text.split('\n').next() == Some("exit=1") => ProbeOutcome::NotDue,
            Ok(reading) => match probe_body(&probe, &reading) {
                Ok(output) => ProbeOutcome::Output { output },
                Err(reason) => ProbeOutcome::Failed { reason },
            },
            Err(reason) => ProbeOutcome::Failed { reason },
        };
        let record = cache.write(outcome, self.clock.now());
        (record.outcome, record.fetched_at)
    }
    fn reminder_gate(&self, reminder: &Reminder) -> ReminderNotice {
        let now = self.clock.now();
        let (scheduled, period) = reminder.schedule(now);
        let notice = |kind| ReminderNotice {
            id: reminder.id.clone(),
            kind,
            message: reminder.spec.message.clone(),
            checked_at: now,
            value: None,
            period: period.clone(),
        };
        if !reminder.in_scope(&self.cwd) {
            return notice(ReminderKind::OutOfScope);
        }
        let state = match self.store.state(&reminder.id) {
            Ok(state) => state,
            Err(error) => return ReminderNotice::unavailable(&reminder.id, error, now),
        };
        if state.done_period.as_deref() == Some(&period) {
            return notice(ReminderKind::Done);
        }
        if state.snooze_until.is_some_and(|until| now < until) {
            return notice(ReminderKind::Snoozed);
        }
        notice(if scheduled {
            ReminderKind::Due
        } else {
            ReminderKind::OutsideSchedule
        })
    }

    pub(crate) async fn evaluate_reminder(
        &self,
        reminder: &Reminder,
        refresh: bool,
        deadline: Instant,
    ) -> ReminderNotice {
        let gate = self.reminder_gate(reminder);
        if gate.kind != ReminderKind::Due {
            return gate;
        }
        let (outcome, at) = self.reminder_probe(reminder, refresh, deadline).await;
        // A probe can cross a window/period boundary, or another session can
        // suppress it while it runs. Decide delivery from the current gate.
        let mut notice = self.reminder_gate(reminder);
        if notice.kind != ReminderKind::Due {
            return notice;
        }
        notice.checked_at = at;
        match outcome {
            ProbeOutcome::NotDue => {
                notice.kind = ReminderKind::NotDue;
                notice
            }
            ProbeOutcome::Failed { reason } => {
                ReminderNotice::unavailable(&reminder.id, reason, at)
            }
            ProbeOutcome::Output { output } => match reminder.judge(&output) {
                Ok((due, message, value)) => {
                    notice.kind = if due {
                        ReminderKind::Due
                    } else {
                        ReminderKind::NotDue
                    };
                    notice.message = message;
                    notice.value = value;
                    notice
                }
                Err(reason) => ReminderNotice::unavailable(&reminder.id, reason, at),
            },
        }
    }
}

impl Vigil {
    /// Cache-only report: one config read supplies both scoped notices and the
    /// file-wide diagnostic. Probe refreshes never hold up delivery.
    pub(crate) async fn reminder_report(&self) -> (Option<Vec<ReminderNotice>>, Option<String>) {
        if self.config.reminders_file.is_none() { return (None, None); }
        let store = match self.reminder_store() {
            Ok(store) => store,
            Err(error) => return (None, Some(format!("reminders unavailable: {error}"))),
        };
        let entries = match store.entries() {
            Ok(entries) => entries,
            Err(error) => return (None, Some(format!("reminders unavailable: {error}"))),
        };
        if entries.is_empty() { return (None, None); }
        let context = self.reminder_context(store);
        let mut refresh = Vec::new();
        let mut in_scope = false;
        let mut notices = Vec::new();
        for entry in entries {
            match entry.reminder {
                Ok(reminder) if reminder.in_scope(&self.cwd) => {
                    in_scope = true;
                    let notice = context.evaluate_reminder(&reminder, false, Instant::now()).await;
                    refresh.push(reminder);
                    if matches!(notice.kind, ReminderKind::Due | ReminderKind::Unavailable) {
                        notices.push(notice);
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    // Even malformed entries need a matching declared scope.
                    if entry.raw["scope"].as_array().is_some_and(|scopes| {
                        scopes
                            .iter()
                            .filter_map(Value::as_str)
                            .map(std::path::Path::new)
                            .filter(|p| p.is_absolute())
                            .any(|scope| {
                                crate::reminder::normalized(&self.cwd)
                                    .starts_with(crate::reminder::normalized(scope))
                            })
                    }) {
                        in_scope = true;
                        notices.push(ReminderNotice::unavailable(
                            &entry.id,
                            error,
                            self.clock.now(),
                        ));
                    }
                }
            }
        }
        self.spawn_reminder_refresh(context, refresh);
        (in_scope.then_some(notices), None)
    }
    /// None outside every configured scope; Some([]) in scope but not due.
    pub async fn envelope_reminders(&self) -> Option<Vec<ReminderNotice>> {
        self.reminder_report().await.0
    }
    pub(crate) async fn reminder_events(
        &self,
        watch_id: &crate::watch::WatchId,
    ) -> Vec<crate::watch::Event> {
        let notices = self.envelope_reminders().await.unwrap_or_default();
        let Ok(store) = self.reminder_store() else {
            return Vec::new();
        };
        if !notices.iter().any(|n| n.kind == ReminderKind::Due) { return Vec::new(); }
        let Ok(watch) = self.load_watch(watch_id) else { return Vec::new(); };
        notices
            .into_iter()
            .filter(|n| n.kind == ReminderKind::Due)
            .filter_map(|notice| {
                match store.claim_event(&notice.id, &notice.period, self.clock.now()) {
                    Ok(true) => Some(crate::watch::Event {
                        event_id: format!("reminder:{}:{}", notice.id, notice.period),
                        tick: watch.tick,
                        watch_id: watch.spec.id.to_string(),
                        input: format!("reminder:{}", notice.id),
                        summary: notice.message,
                        judge: None,
                        redelivered: false,
                        detail: Some(crate::watch::EventDetail::ReminderDue {
                            reminder_id: notice.id,
                            period: notice.period,
                        }),
                    }),
                    _ => None,
                }
            })
            .collect()
    }
}
