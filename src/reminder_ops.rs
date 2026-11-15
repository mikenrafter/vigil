//! Shared CLI/MCP operations and envelope evaluation. Probes are trusted local
//! operator commands, with the same reader and deadline as advisories.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::advisory::{FAILURE_CACHE_S, probe_body};
use crate::advisory_eval::read_probe;
use crate::clock::Timestamp;
use crate::engine::Vigil;
use crate::reminder::{Reminder, ReminderKind, ReminderNotice, Result, invalid};
use crate::reminder_store::{ReminderStore, atomic_write};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ProbeOutcome {
    Output { output: String },
    NotDue,
    Failed { reason: String },
}
#[derive(Serialize, Deserialize)]
struct CachedProbe {
    fingerprint: String,
    at: Timestamp,
    outcome: ProbeOutcome,
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
            json!({"id": id, "spec": reminder.spec, "evaluation": self.evaluate_reminder(&store, &reminder).await}),
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
    async fn reminder_probe(&self, reminder: &Reminder) -> (ProbeOutcome, Timestamp) {
        let Some(probe) = reminder.probe() else {
            return (
                ProbeOutcome::Output {
                    output: String::new(),
                },
                self.clock.now(),
            );
        };
        // Include the resolved cwd: one id must never reuse another repo's probe.
        let fingerprint = serde_json::to_string(&probe).expect("input serializes");
        let path = self
            .store
            .root()
            .join("reminder-cache")
            .join(format!("{}.json", reminder.id));
        let now = self.clock.now();
        if let Some(cached) = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<CachedProbe>(&bytes).ok())
        {
            let ttl = if matches!(cached.outcome, ProbeOutcome::Failed { .. }) {
                reminder.spec.cache_s.min(FAILURE_CACHE_S)
            } else {
                reminder.spec.cache_s
            };
            let age = now.secs_since(cached.at);
            if cached.fingerprint == fingerprint && age >= 0 && (age as u64) < ttl {
                return (cached.outcome, cached.at);
            }
        }
        let outcome = match read_probe(&probe, self.reader.as_ref()).await {
            Ok(reading)
                if !reading.is_error && reading.text.split('\n').next() == Some("exit=1") =>
            {
                ProbeOutcome::NotDue
            }
            Ok(reading) => match probe_body(&probe, &reading) {
                Ok(output) => ProbeOutcome::Output { output },
                Err(reason) => ProbeOutcome::Failed { reason },
            },
            Err(reason) => ProbeOutcome::Failed { reason },
        };
        let at = self.clock.now();
        let cached = CachedProbe {
            fingerprint,
            at,
            outcome: outcome.clone(),
        };
        // Cache writes are best effort; suppression state writes are not.
        if let Ok(bytes) = serde_json::to_vec(&cached) {
            let _ = atomic_write(&path, &bytes);
        }
        (outcome, at)
    }
    fn reminder_gate(&self, store: &ReminderStore, reminder: &Reminder) -> ReminderNotice {
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
        let state = match store.state(&reminder.id) {
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
        store: &ReminderStore,
        reminder: &Reminder,
    ) -> ReminderNotice {
        let gate = self.reminder_gate(store, reminder);
        if gate.kind != ReminderKind::Due {
            return gate;
        }
        let (outcome, at) = self.reminder_probe(reminder).await;
        // A probe can cross a window/period boundary, or another session can
        // suppress it while it runs. Decide delivery from the current gate.
        let mut notice = self.reminder_gate(store, reminder);
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
    /// File-wide failures have no recoverable scope, so report separately.
    pub(crate) fn reminders_error(&self) -> Option<String> {
        if self.config.reminders_file.is_none() {
            return None;
        }
        self.reminder_store()
            .and_then(|store| store.entries())
            .err()
            .map(|error| format!("reminders unavailable: {error}"))
    }

    /// None outside all configured scopes; Some([]) in scope but not due.
    pub async fn envelope_reminders(&self) -> Option<Vec<ReminderNotice>> {
        if self.config.reminders_file.is_none() {
            return None;
        }
        let store = self.reminder_store().ok()?;
        let entries = match store.entries() {
            Ok(entries) => entries,
            Err(_) => return None,
        };
        let mut in_scope = false;
        let mut notices = Vec::new();
        for entry in entries {
            match entry.reminder {
                Ok(reminder) if reminder.in_scope(&self.cwd) => {
                    in_scope = true;
                    let notice = self.evaluate_reminder(&store, &reminder).await;
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
        in_scope.then_some(notices)
    }
    pub(crate) async fn reminder_events(
        &self,
        watch: &crate::watch::WatchFile,
    ) -> Vec<crate::watch::Event> {
        let notices = self.envelope_reminders().await.unwrap_or_default();
        let Ok(store) = self.reminder_store() else {
            return Vec::new();
        };
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
