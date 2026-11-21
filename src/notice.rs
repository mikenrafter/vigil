//! Notices: one-time announcements every agent session sees exactly once.
//! STUB for the red phase; bodies arrive in the green commit.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::clock::Timestamp;

pub const MAX_NOTICES: usize = 64;
pub const MAX_MESSAGE_BYTES: usize = 2000;

#[derive(Debug, thiserror::Error)]
pub enum NoticeError {
    #[error("notice: {0}")]
    Invalid(String),
}

impl From<std::io::Error> for NoticeError {
    fn from(error: std::io::Error) -> Self {
        NoticeError::Invalid(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, NoticeError>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notice {
    pub id: String,
    pub message: String,
    pub created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct NoticeRequest {
    pub id: String,
    pub message: String,
    pub expires_at: Option<Timestamp>,
    pub scope: Vec<PathBuf>,
}

/// Who a delivery is recorded against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoticeSession {
    /// An agent session or thread id (explicit, from the environment, or ancestry).
    Agent(String),
    /// No identity: one run of the calling process, keyed by pid and start time.
    ProcessRun(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeliveredNotice {
    pub id: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeIdentity {
    Session,
    ProcessRun,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoticeDelivery {
    pub identity: NoticeIdentity,
    pub session: Option<String>,
    pub notices: Vec<DeliveredNotice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Notices recorded as delivered but not yet handed to the caller for good.
#[must_use = "keep() the claim once the notices reached the agent, release() it when they did not"]
pub struct NoticeClaim {
    delivery: NoticeDelivery,
}

impl NoticeClaim {
    pub fn delivery(&self) -> &NoticeDelivery {
        &self.delivery
    }
    pub fn keep(self) -> NoticeDelivery {
        unimplemented!()
    }
    pub fn release(self) {
        unimplemented!()
    }
}

#[derive(Clone)]
pub struct NoticeStore {
    pub root: PathBuf,
}

impl NoticeStore {
    pub fn add(&self, now: Timestamp, request: NoticeRequest) -> Result<Notice> {
        unimplemented!()
    }
    pub fn list(&self) -> Result<Vec<Notice>> {
        unimplemented!()
    }
    pub fn remove(&self, id: &str) -> Result<()> {
        unimplemented!()
    }
    pub fn claim(&self, session: &NoticeSession, now: Timestamp, cwd: &Path) -> Result<NoticeClaim> {
        unimplemented!()
    }
}
