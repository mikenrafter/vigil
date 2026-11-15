//! The advisory operations the CLI and the MCP tools share, so the two
//! surfaces cannot drift (parity: `tests/advisory_e2e.rs` drives both).
//! Every result is a JSON value; every failure an [`AdvisoryError`].
//!
//! Reason: the model is an untrusted caller of `advisory_add` and
//! `advisory_set`; one validated path (`Advisory::new`) sits behind both.

use serde_json::{json, Value};

use crate::advisory::{Advisory, AdvisoryError, AdvisoryId, AdvisorySpec};
use crate::advisory_eval::{advise, Advice};
use crate::advisory_store::{AdvisorySet, Entry};
use crate::engine::Vigil;

/// One line per advisory for `advisory list`.
fn summary(entry: &Entry) -> Value {
    match entry {
        Entry::Ready(advisory) => {
            let spec = advisory.spec();
            json!({
                "id": advisory.id().as_str(),
                "status": "ok",
                "params": spec.params,
                "condition": spec.condition,
                "cache_s": advisory.cache_s(),
                "probe": spec.probe.label(),
            })
        }
        Entry::Broken { id, reason } => json!({"id": id, "status": "broken", "reason": reason}),
    }
}

fn detail(advisory: &Advisory) -> Value {
    json!({"id": advisory.id().as_str(), "spec": advisory.spec()})
}

impl Vigil {
    /// `{"advice": [...]}`, the shape of `vigil advice --json` and the MCP
    /// `advice` tool. `Err(Disabled)` when advisories are off and no scoped
    /// reminder or config diagnostic can be delivered.
    pub async fn advice_report(&self) -> Result<Value, AdvisoryError> {
        let advice: Option<Vec<Advice>> = self.envelope_advice().await;
        let reminders = self.envelope_reminders().await;
        let reminders_error = self.reminders_error();
        // Advice remains usable for project reminders when advisories are off.
        if advice.is_none() && reminders.is_none() && reminders_error.is_none() { return Err(AdvisoryError::Disabled); }
        let mut report = json!({});
        if let Some(advice) = advice { report["advice"] = json!(advice); }
        if let Some(reminders) = reminders { report["reminders"] = json!(reminders); }
        if let Some(error) = reminders_error { report["reminders_error"] = json!(error); }
        Ok(report)
    }

    pub fn advisory_list(&self) -> Result<Value, AdvisoryError> {
        let set: AdvisorySet = self.advisory_store()?.load()?;
        Ok(Value::Array(set.entries.iter().map(summary).collect()))
    }

    /// The spec and the current evaluation of one advisory.
    pub async fn advisory_show(&self, id: &str) -> Result<Value, AdvisoryError> {
        let advisory = self.advisory_store()?.get(id)?;
        let set = AdvisorySet { entries: vec![Entry::Ready(advisory.clone())] };
        let advice = advise(&set, self.reader.as_ref(), self.clock.as_ref(), &self.advisory_cache_dir()).await;
        let mut shown = detail(&advisory);
        shown["advice"] = serde_json::to_value(&advice[0]).expect("advice serializes");
        Ok(shown)
    }

    /// `spec` is the advisory object as JSON. The id is checked first, so a
    /// bad id is reported as such whatever else is wrong.
    pub fn advisory_add(&self, id: &str, spec: Value, replace: bool) -> Result<Value, AdvisoryError> {
        AdvisoryId::parse(id)?;
        let spec: AdvisorySpec = serde_json::from_value(spec).map_err(|error| AdvisoryError::invalid("advisory", error.to_string()))?;
        Ok(detail(&self.advisory_store()?.add(id, spec, replace)?))
    }

    pub fn advisory_set(&self, id: &str, assignments: &[String]) -> Result<Value, AdvisoryError> {
        if assignments.is_empty() {
            return Err(AdvisoryError::invalid("setting", "give at least one name=value, e.g. threshold=15"));
        }
        Ok(detail(&self.advisory_store()?.set(id, assignments)?))
    }

    pub fn advisory_remove(&self, id: &str) -> Result<Value, AdvisoryError> {
        self.advisory_store()?.remove(id)?;
        Ok(json!({"removed": id}))
    }
}
