//! Project reminders: schedule and scope first, then a bounded cached probe.
//! State is owned by ReminderStore; CLI and MCP share reminder_ops.
//! CLAIM(reminder-scope): component prefixes, after path normalization, gate
//! all delivery [SPEC Reminders; reminder_tests::scope_uses_components].
//! CLAIM(reminder-time): windows are [start,end), weeks start Monday in the
//! reminder's zone [SPEC Reminders; reminder_tests schedule and suppression].

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use chrono::{Datelike, Local, NaiveTime};
use serde::{Deserialize, Serialize};

use crate::advisory::{Advisory, AdvisoryId, AdvisorySpec, ExtractSpec};
use crate::clock::Timestamp;
use crate::watch::Input;

#[derive(Debug, thiserror::Error)]
#[error("reminder: {0}")]
pub struct ReminderError(pub String);

impl From<std::io::Error> for ReminderError {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, ReminderError>;
pub fn invalid(reason: impl Into<String>) -> ReminderError {
    ReminderError(reason.into())
}
pub fn check_id(id: &str) -> Result<()> {
    AdvisoryId::parse(id)
        .map(|_| ())
        .map_err(|e| invalid(e.to_string()))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Period {
    Day,
    #[default]
    Week,
}

/// Wire/TOML form. Probe is argv JSON, never implicitly a shell command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReminderSpec {
    pub scope: Vec<PathBuf>,
    pub days: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extract: Option<ExtractSpec>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    pub message: String,
    #[serde(default)]
    pub period: Period,
    #[serde(default = "default_cache")]
    pub cache_s: u64,
    #[serde(default = "default_timeout")]
    pub timeout_s: u64,
}
fn default_cache() -> u64 {
    600
}
fn default_timeout() -> u64 {
    10
}

#[derive(Debug, Clone)]
pub struct Reminder {
    pub id: String,
    pub spec: ReminderSpec,
    weekdays: Vec<u32>,
    window: Option<(NaiveTime, NaiveTime)>,
    zone: Option<chrono_tz::Tz>,
    numeric: Option<Advisory>,
}

pub fn normalized(path: &Path) -> PathBuf {
    let mut clean = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                clean.pop();
            }
            Component::CurDir => {}
            other => clean.push(other.as_os_str()),
        }
    }
    clean.canonicalize().unwrap_or(clean)
}

impl Reminder {
    pub fn new(id: &str, mut spec: ReminderSpec) -> Result<Self> {
        check_id(id)?;
        if spec.scope.is_empty()
            || spec.scope.len() > 16
            || spec.scope.iter().any(|p| !p.is_absolute())
        {
            return Err(invalid("scope needs 1..=16 absolute directory prefixes"));
        }
        spec.scope = spec.scope.iter().map(|p| normalized(p)).collect();
        if spec.days.is_empty() || spec.days.len() > 7 {
            return Err(invalid("days needs 1..=7 weekdays"));
        }
        let mut weekdays = Vec::new();
        for day in &mut spec.days {
            *day = day.to_ascii_lowercase();
            let number = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]
                .iter()
                .position(|d| *d == day.as_str())
                .ok_or_else(|| {
                    invalid(format!(
                        "unknown weekday {day:?}; use mon,tue,wed,thu,fri,sat,sun"
                    ))
                })? as u32;
            if weekdays.contains(&number) {
                return Err(invalid(format!("duplicate weekday {day}")));
            }
            weekdays.push(number);
        }
        let window = spec
            .window
            .as_deref()
            .map(|raw| {
                let parse = |text: &str| -> Result<NaiveTime> {
                    if text.len() != 5 {
                        return Err(invalid("window must be HH:MM-HH:MM"));
                    }
                    NaiveTime::parse_from_str(text, "%H:%M")
                        .map_err(|_| invalid("window must be HH:MM-HH:MM"))
                };
                let (start, end) = raw
                    .split_once('-')
                    .ok_or_else(|| invalid("window must be HH:MM-HH:MM"))?;
                let (start, end) = (parse(start)?, parse(end)?);
                if start >= end {
                    return Err(invalid(
                        "window start must precede end; overnight windows are not supported",
                    ));
                }
                Ok((start, end))
            })
            .transpose()?;
        let zone = spec
            .tz
            .as_deref()
            .map(|tz| {
                tz.parse::<chrono_tz::Tz>()
                    .map_err(|_| invalid(format!("unknown IANA timezone {tz:?}")))
            })
            .transpose()?;
        if spec.message.trim().is_empty() || spec.message.len() > 2048 {
            return Err(invalid("message needs 1..=2048 bytes"));
        }
        if spec.cache_s > 86400 {
            return Err(invalid("cache_s must be 0..=86400"));
        }
        if !(1..=19).contains(&spec.timeout_s) {
            return Err(invalid("timeout_s must be 1..=19"));
        }
        let numeric = if let Some(condition) = &spec.condition {
            let probe = spec
                .probe
                .as_ref()
                .ok_or_else(|| invalid("condition needs a probe"))?;
            let extract = spec.extract.clone().unwrap_or(ExtractSpec {
                regex: Some(r"^\s*([-+]?\d+(?:\.\d+)?)\s*$".into()),
                ..Default::default()
            });
            Some(
                Advisory::new(
                    AdvisoryId::parse(id).map_err(|e| invalid(e.to_string()))?,
                    AdvisorySpec {
                        probe: Input::Cmd {
                            argv: probe.clone(),
                            timeout_s: Some(spec.timeout_s as f64),
                            cwd: None,
                        },
                        extract,
                        params: spec.params.clone(),
                        condition: condition.clone(),
                        message_when_true: "{value}".into(),
                        message_when_false: "{value}".into(),
                        cache_s: Some(spec.cache_s),
                    },
                )
                .map_err(|e| invalid(e.to_string()))?,
            )
        } else {
            if spec.extract.is_some() || !spec.params.is_empty() {
                return Err(invalid("extract and params need condition"));
            }
            None
        };
        if let Some(probe) = &spec.probe {
            crate::watch::validate_input(&Input::Cmd {
                argv: probe.clone(),
                timeout_s: Some(spec.timeout_s as f64),
                cwd: None,
            })
            .map_err(|e| invalid(e.to_string()))?;
        }
        Ok(Self {
            id: id.into(),
            spec,
            weekdays,
            window,
            zone,
            numeric,
        })
    }

    pub fn in_scope(&self, cwd: &Path) -> bool {
        let cwd = normalized(cwd);
        self.spec.scope.iter().any(|scope| cwd.starts_with(scope))
    }

    pub fn schedule(&self, now: Timestamp) -> (bool, String) {
        let (date, time) = match self.zone {
            Some(zone) => {
                let local = now.0.with_timezone(&zone);
                (local.date_naive(), local.time())
            }
            None => {
                let local = now.0.with_timezone(&Local);
                (local.date_naive(), local.time())
            }
        };
        let due = self
            .weekdays
            .contains(&date.weekday().num_days_from_monday())
            && self
                .window
                .is_none_or(|(start, end)| time >= start && time < end);
        let period = match self.spec.period {
            Period::Day => date.to_string(),
            Period::Week => {
                let week = date.iso_week();
                format!("{}-W{:02}", week.year(), week.week())
            }
        };
        (due, period)
    }

    pub fn judge(&self, output: &str) -> std::result::Result<(bool, String, Option<f64>), String> {
        let (due, value, rendered) = match &self.numeric {
            Some(rule) => {
                let value = rule.extract_value(output)?;
                let verdict = rule.judge(value);
                (verdict.condition_true, Some(value), verdict.message)
            }
            None => (true, None, String::new()),
        };
        let message = self
            .spec
            .message
            .replace("{output}", output.trim())
            .replace("{value}", &rendered);
        Ok((due, message, value))
    }

    pub fn probe(&self) -> Option<Input> {
        // The first scope is the stable probe cwd across all caller directories.
        self.spec.probe.as_ref().map(|argv| Input::Cmd {
            argv: argv.clone(),
            timeout_s: Some(self.spec.timeout_s as f64),
            cwd: Some(self.spec.scope[0].clone()),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReminderKind {
    Due,
    Unavailable,
    NotDue,
    Done,
    Snoozed,
    OutOfScope,
    OutsideSchedule,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReminderNotice {
    pub id: String,
    pub kind: ReminderKind,
    pub message: String,
    pub checked_at: Timestamp,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    pub period: String,
}
impl ReminderNotice {
    pub fn unavailable(id: &str, reason: impl std::fmt::Display, now: Timestamp) -> Self {
        Self {
            id: id.into(),
            kind: ReminderKind::Unavailable,
            message: format!("reminder {id} unavailable: {reason}"),
            checked_at: now,
            value: None,
            period: String::new(),
        }
    }
}
