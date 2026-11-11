//! vigil: an MCP server that keeps an agent's turn open while a cheap judge
//! watches things for it. See `SPEC.md` and `FINDINGS.md` beside this crate.
//!
//! RED phase: every public item below is a stub. Types and signatures are the
//! contract the tests compile against; bodies are `todo!()` unless trivial.
#![allow(dead_code, unused_variables)] // phase 2-3 stubs remain

pub mod clock;
pub mod config;
pub mod engine;
pub mod envelope;
pub mod error;
pub mod fault;
pub mod inputs;
pub mod judge;
pub mod keepalive;
pub mod mcp;
pub mod store;
pub mod testing;
pub mod tick;
pub mod watch;

pub use clock::{Clock, SystemClock, Timestamp};
pub use config::Config;
pub use engine::Vigil;
pub use envelope::{Envelope, Next, OtherWatch, PersistFailure, PersistNote, SecondaryFailure};
pub use error::{CreateError, FailureKind, StoreError, VigilError};
pub use fault::{Attempt, FaultPlan, FaultRule, Stage, WriteKind};
pub use judge::{Answer, Answers, Judge, JudgeError, JudgeRequest, JudgeRule, RuleTest};
pub use keepalive::{KeepaliveRequest, ProgressSink};
pub use store::{Store, StoreLock, WatchLock, WriteOutcome};
pub use tick::TickOutcome;
pub use watch::{
    CreateRequest, Event, FireKind, FireRule, Input, JudgeStatus, WatchFile, WatchId, WatchSpec,
    WatchStatus,
};

#[cfg(test)]
mod disk_full_tests;
