//! vigil: an MCP server that keeps an agent's turn open while a cheap judge
//! watches things for it. See `SPEC.md` and `FINDINGS.md` beside this crate.
//!
//! Module map: `watch`/`store`/`tick` (watches and their files), `keepalive`
//! (the blocking call), `cache` (call window from the prompt-cache TTL),
//! `attention` (early wake rules), `credit` + `monitor` (the orchestrator's
//! credit check and hook), `advisory`/`advisory_store`/`advisory_eval` (rules
//! that tell agents what to do, on every envelope), `envelope` (what the agent reads), `mcp` (the
//! server front), `fault`/`meter`/`testing` (test seams).
#![allow(dead_code, unused_variables)] // fixtures in testing.rs and some seams are used only by tests

pub mod advisory;
pub mod advisory_eval;
pub mod advisory_ops;
pub mod advisory_store;
pub mod attention;
pub mod cache;
pub mod clock;
pub mod config;
pub mod credit;
pub mod engine;
pub mod envelope;
pub mod error;
pub mod fault;
pub mod hold;
pub mod inputs;
pub mod judge;
pub mod meter;
pub(crate) mod monitor;
pub(crate) mod process;
pub mod keepalive;
pub mod mcp;
pub mod store;
pub mod testing;
pub mod tick;
pub mod watch;

pub use advisory::{Advisory, AdvisoryError, AdvisoryId, AdvisorySpec};
pub use advisory_eval::{Advice, AdviceKind};
pub use clock::{Clock, SystemClock, Timestamp};
pub use config::Config;
pub use engine::Vigil;
pub use envelope::{Envelope, Next, OtherWatch, PersistFailure, PersistNote, SecondaryFailure};
pub use error::{CreateError, FailureKind, StoreError, VigilError};
pub use fault::{Attempt, FaultPlan, FaultRule, Stage, WriteKind};
pub use hold::{HoldRequest, Work};
pub use judge::{Answer, Answers, Judge, JudgeError, JudgeRequest, JudgeRule, RuleTest};
pub use keepalive::{KeepaliveRequest, ProgressSink};
pub use store::{Store, StoreLock, WatchLock, WriteOutcome};
pub use tick::TickOutcome;
pub use watch::{
    CreateRequest, Event, FireKind, FireRule, Input, JudgeStatus, WatchFile, WatchId, WatchSpec,
    WatchStatus,
};

#[cfg(test)]
mod advisory_tests;
#[cfg(test)]
mod auto_interval_tests;
#[cfg(test)]
mod disk_full_tests;
#[cfg(test)]
mod hold_tests;
