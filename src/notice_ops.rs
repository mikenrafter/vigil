//! Shared CLI/MCP operations over notices. STUB for the red phase.

use std::path::Path;

use serde_json::Value;

use crate::engine::Vigil;
use crate::notice::{Notice, NoticeClaim, NoticeRequest, NoticeSession, NoticeStore, Result};

impl Vigil {
    pub fn notice_store(&self) -> NoticeStore {
        NoticeStore { root: self.store.root().into() }
    }
    pub fn notice_add(&self, request: NoticeRequest) -> Result<Notice> {
        unimplemented!()
    }
    pub fn notice_list(&self) -> Result<Value> {
        unimplemented!()
    }
    pub fn notice_rm(&self, id: &str) -> Result<Value> {
        unimplemented!()
    }
    pub fn notice_session(&self, explicit: Option<&str>) -> Result<NoticeSession> {
        unimplemented!()
    }
    pub fn claim_notices(&self, explicit_session: Option<&str>, cwd: Option<&Path>) -> Result<NoticeClaim> {
        unimplemented!()
    }
}
