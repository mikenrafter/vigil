//! Per-hold Claude run admission (W46-K1..K5). The owner sets expiring limits;
//! a hold's lock protects counting live items and appending their replacement.

use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};

use crate::clock::Timestamp;
use crate::engine::Vigil;
use crate::error::{ItemError, StoreError, VigilError};
use crate::items::HoldItem;

/// Only explicitly classified Claude runs consume a slot (W46-K1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum CapClass {
    Work,
    Review,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapSource {
    Default,
    Owner,
}

/// The effective limits; an expired owner setting reads exactly as defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapLimits {
    pub work: NonZeroUsize,
    pub review: NonZeroUsize,
    pub until: Option<Timestamp>,
    pub source: CapSource,
}

impl Default for CapLimits {
    fn default() -> Self {
        Self { work: NonZeroUsize::MIN, review: NonZeroUsize::MIN, until: None, source: CapSource::Default }
    }
}

/// W46-K4: live counts and effective limits, present only with classed items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapStatus {
    pub work: usize,
    pub work_limit: usize,
    pub review: usize,
    pub review_limit: usize,
}

/// The items of `class` that still hold a slot (W46-K5).
fn live_items(items: &[HoldItem], class: CapClass) -> impl Iterator<Item = &HoldItem> {
    items.iter().filter(move |item| item.cap == Some(class) && item.ended_at.is_none())
}

impl CapLimits {
    fn limit(&self, class: CapClass) -> usize {
        match class { CapClass::Work => self.work.get(), CapClass::Review => self.review.get() }
    }

    pub(crate) fn status(&self, items: &[HoldItem]) -> CapStatus {
        let live = |class| live_items(items, class).count();
        CapStatus { work: live(CapClass::Work), work_limit: self.work.get(), review: live(CapClass::Review), review_limit: self.review.get() }
    }

    /// CLAIM(W46-K3): callers hold the hold lock across this check and append
    /// [spec K3; w46_two_racing_work_adds_have_exactly_one_winner_at_limit_one].
    /// `labels` is the new batch; creation checks every initial item together.
    pub(crate) fn admit(&self, class: CapClass, items: &[HoldItem], labels: &[&str]) -> Result<(), ItemError> {
        let live: Vec<_> = live_items(items, class).collect();
        let limit = self.limit(class);
        if live.len().saturating_add(labels.len()) <= limit {
            return Ok(());
        }
        // The smallest limit that admits the whole batch. With nothing live this is a
        // creation: nothing is running, so there is nothing to wait for.
        let needed = live.len().saturating_add(labels.len());
        let (prefix, adjective, settings) = match class {
            CapClass::Work => ("agent", "working", format!("--work {needed}")),
            CapClass::Review => ("reviewer", "reviewing", format!("--work {} --review {needed}", self.work)),
        };
        let (names, count, way_out) = if live.is_empty() {
            (labels.to_vec(), labels.len(), "The owner raises it")
        } else {
            (live.iter().map(|item| item.label.as_str()).collect(), live.len(), "Wait for one to end, or the owner raises it")
        };
        Err(ItemError::CapReached(format!("{prefix} cap reached: {count} {adjective} Claude runs (cap {limit}): {}. {way_out}: vigil cap set {settings} --until <time>", names.join(", "))))
    }
}

impl Vigil {
    /// W46-K2: read on every operation so expiry needs no reset or daemon.
    /// A file that cannot be parsed is `Corrupt` and names the way out; admission
    /// fails closed on it, while displays and `cap set` do not need it (S1).
    pub fn cap_limits(&self) -> Result<CapLimits, VigilError> {
        let path = self.store.root().join("cap.json");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(CapLimits::default()),
            Err(error) => return Err(StoreError::io(error).into()),
        };
        let limits: CapLimits = serde_json::from_slice(&bytes).map_err(|error| StoreError::Corrupt(format!("{}: {error}; the owner can replace it with `vigil cap reset`", path.display())))?;
        Ok(if limits.source == CapSource::Owner && limits.until.is_some_and(|until| until > self.clock.now()) { limits } else { CapLimits::default() })
    }

    /// Shell-only owner control. Omitted review keeps the effective review limit
    /// (the default when cap.json cannot be read: this call replaces that file).
    pub async fn cap_set(&self, work: NonZeroUsize, review: Option<NonZeroUsize>, until: Timestamp) -> Result<CapLimits, VigilError> {
        if until <= self.clock.now() {
            return Err(ItemError::CapUntil.into());
        }
        let _lock = self.lock_global().await?;
        if until <= self.clock.now() {
            return Err(ItemError::CapUntil.into());
        }
        let limits = CapLimits { work, review: review.unwrap_or_else(|| self.cap_limits().map_or(CapLimits::default().review, |current| current.review)), until: Some(until), source: CapSource::Owner };
        crate::store::write_atomic(&self.store.root().join("cap.json"), &limits)?;
        Ok(limits)
    }

    pub async fn cap_reset(&self) -> Result<CapLimits, VigilError> {
        let _lock = self.lock_global().await?;
        let limits = CapLimits::default();
        crate::store::write_atomic(&self.store.root().join("cap.json"), &limits)?;
        Ok(limits)
    }
}
