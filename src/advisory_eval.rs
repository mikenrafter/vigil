//! Evaluating advisories: probe (through the engine's `InputReader`), reuse a
//! cached probe result for `cache_s`, extract, judge, render (SPEC
//! "Advisories: evaluation").
//!
//! CLAIM(advisory-cache): a probe result is reused only while younger than
//! `cache_s` (failures: at most `FAILURE_CACHE_S`), only for the same probe and
//! extraction (fingerprint), and never when its timestamp is in the future.
//! CLAIM(advisory-immediate): params, condition and messages are applied to the
//! cached number on every call, so an edit shows at once without a new probe.
//! CLAIM(advisory-unavailable): every failure becomes an entry
//! `advisory <id> unavailable: <reason>`; an old value is never shown after its
//! cache time, and nothing is silent.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::advisory::{probe_body, Advisory, FAILURE_CACHE_S};
use crate::advisory_store::{AdvisorySet, Entry};
use crate::clock::{Clock, Timestamp};
use crate::inputs::InputReader;
use crate::watch::InputState;
use crate::probe_cache::ProbeCache;

/// A probe that has not answered after this long is a failure (the cmd's own
/// timeout is at most 19 s; this also bounds `url` and a stuck reader).
pub const PROBE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdviceKind {
    Advice,
    Unavailable,
}

/// One entry of an envelope's `advice` list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Advice {
    pub id: String,
    pub kind: AdviceKind,
    pub message: String,
    /// When the probe behind this entry ran (not when it was asked for).
    pub checked_at: Timestamp,
    /// The extracted number, for an `advice` entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
}

impl Advice {
    pub fn unavailable(id: &str, reason: &str, at: Timestamp) -> Advice {
        Advice { id: id.to_string(), kind: AdviceKind::Unavailable, message: format!("advisory {id} unavailable: {reason}"), checked_at: at, value: None }
    }
}

/// What one probe run learned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Outcome {
    Value { value: f64 },
    Failed { reason: String },
}

/// Shared bounded reader for advisory and reminder probes.
pub(crate) async fn read_probe(input: &crate::watch::Input, reader: &dyn InputReader) -> Result<crate::inputs::Reading, String> {
    tokio::time::timeout(PROBE_DEADLINE, reader.read(input, &InputState::default()))
        .await.map_err(|_| format!("probe did not answer within {}s", PROBE_DEADLINE.as_secs()))
}

async fn probe(advisory: &Advisory, reader: &dyn InputReader) -> Outcome {
    let failed = |reason: String| Outcome::Failed { reason };
    let reading = match read_probe(advisory.probe(), reader).await {
        Ok(reading) => reading,
        Err(reason) => return failed(reason),
    };
    match probe_body(advisory.probe(), &reading).and_then(|body| advisory.extract_value(&body)) {
        Ok(value) => Outcome::Value { value },
        Err(reason) => failed(reason),
    }
}

async fn advise_one(advisory: &Advisory, reader: &dyn InputReader, clock: &dyn Clock, cache_dir: &Path) -> Advice {
    let now = clock.now();
    let cache = ProbeCache {
        path: cache_dir.join(format!("{}.json", advisory.id())),
        fingerprint: advisory.fingerprint(),
        ttl_s: advisory.cache_s(),
        failure_ttl_s: advisory.cache_s().min(FAILURE_CACHE_S),
    };
    let record = match cache.read(now, |outcome| matches!(outcome, Outcome::Failed { .. })) {
        Some(record) => record,
        None => {
            let outcome = probe(advisory, reader).await;
            cache.write(outcome, clock.now())
        }
    };
    let id = advisory.id().as_str();
    match record.outcome {
        // CLAIM(advisory-immediate) ↳ params, condition and messages are applied to the cached number now
        Outcome::Value { value } => Advice { id: id.to_string(), kind: AdviceKind::Advice, message: advisory.judge(value).message, checked_at: record.fetched_at, value: Some(value) },
        Outcome::Failed { reason } => Advice::unavailable(id, &reason, record.fetched_at),
    }
}

/// Evaluate every entry of `set`, in file order.
pub async fn advise(set: &AdvisorySet, reader: &dyn InputReader, clock: &dyn Clock, cache_dir: &Path) -> Vec<Advice> {
    let mut advice = Vec::with_capacity(set.entries.len());
    for entry in &set.entries {
        advice.push(match entry {
            Entry::Broken { id, reason } => Advice::unavailable(id, reason, clock.now()),
            Entry::Ready(advisory) => advise_one(advisory, reader, clock, cache_dir).await,
        });
    }
    advice
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisory::{Advisory, AdvisoryId, AdvisorySpec};
    use crate::advisory_store::Entry;
    use crate::testing::{spec_instant, ManualClock, ScriptedReader};
    use serde_json::json;

    const PROBE_LABEL: &str = "cmd:probe-cmd";

    fn spec(extra: serde_json::Value) -> AdvisorySpec {
        let mut base = json!({
            "probe": {"kind": "cmd", "argv": ["probe-cmd"]},
            "extract": {"name": "free", "json": "free"},
            "params": {"floor": 20},
            "condition": "free < floor",
            "message_when_true": "Low: {free} (< {floor})",
            "message_when_false": "Plenty: {free} (>= {floor})",
            "cache_s": 100
        });
        for (key, value) in extra.as_object().unwrap() {
            base[key] = value.clone();
        }
        serde_json::from_value(base).unwrap()
    }

    fn set_of(specs: &[(&str, AdvisorySpec)]) -> AdvisorySet {
        AdvisorySet { entries: specs.iter().map(|(id, spec)| Entry::Ready(Box::new(Advisory::new(AdvisoryId::parse(id).unwrap(), spec.clone()).unwrap()))).collect() }
    }

    struct Rig {
        dir: tempfile::TempDir,
        reader: ScriptedReader,
        clock: ManualClock,
    }

    impl Rig {
        fn new() -> Rig {
            Rig { dir: tempfile::tempdir().unwrap(), reader: ScriptedReader::new(), clock: ManualClock::new(spec_instant()) }
        }
        fn probe_says(&self, free: i64) {
            self.reader.set_text(PROBE_LABEL, &format!("exit=0\n{{\"free\": {free}}}"));
        }
        async fn advise(&self, set: &AdvisorySet) -> Vec<Advice> {
            advise(set, &self.reader, &self.clock, &self.dir.path().join("advisory-cache")).await
        }
        fn probes(&self) -> usize {
            self.reader.started_count(PROBE_LABEL)
        }
    }

    #[tokio::test]
    async fn a_value_below_the_floor_gives_the_true_message() {
        // why: end to end through the reader: probe text -> extract -> condition
        //      -> message with the number and the param. 15 < 20 -> true.
        //      The entry says when it was checked and carries the number.
        // proves: CLAIM(advisory-immediate)
        let rig = Rig::new();
        rig.probe_says(15);
        let advice = rig.advise(&set_of(&[("mem", spec(json!({})))])).await;
        assert_eq!(advice, vec![Advice { id: "mem".into(), kind: AdviceKind::Advice, message: "Low: 15 (< 20)".into(), checked_at: spec_instant(), value: Some(15.0) }]);
        rig.probe_says(20);
        let later = rig.advise(&set_of(&[("fresh", spec(json!({})))])).await;
        assert_eq!(later[0].message, "Plenty: 20 (>= 20)", "20 is not < 20");
    }

    #[tokio::test]
    async fn the_probe_result_is_reused_for_cache_s_and_then_probed_again() {
        // why: owner: "cache_s (how long a probe result is reused)". cache_s is
        //      100: at +99 s the old number is still current (no new probe), at
        //      +100 s the probe runs again and the NEW number shows. Boundary
        //      on both sides of the age.
        // proves: CLAIM(advisory-cache)
        let rig = Rig::new();
        let set = set_of(&[("mem", spec(json!({})))]);
        rig.probe_says(15);
        assert_eq!(rig.advise(&set).await[0].value, Some(15.0));
        rig.probe_says(50);
        rig.clock.advance_secs(99);
        let cached = rig.advise(&set).await;
        assert_eq!((rig.probes(), cached[0].value), (1, Some(15.0)), "still within cache_s");
        assert_eq!(cached[0].checked_at, spec_instant(), "checked_at says when the probe really ran");
        rig.clock.advance_secs(1);
        let fresh = rig.advise(&set).await;
        assert_eq!((rig.probes(), fresh[0].value), (2, Some(50.0)));
        assert_eq!(fresh[0].checked_at, spec_instant().plus_secs(100));
    }

    #[tokio::test]
    async fn the_cache_is_shared_between_processes_through_the_state_directory() {
        // why: every CLI call is a new process; a hook calling `vigil advice`
        //      before each Bash command must not run `boat limits` each time.
        //      The cache lives on disk: a second evaluation with a different
        //      reader object (a fresh process) must not probe again.
        // proves: CLAIM(advisory-cache)
        let rig = Rig::new();
        rig.probe_says(15);
        let set = set_of(&[("mem", spec(json!({})))]);
        rig.advise(&set).await;
        let second_process = ScriptedReader::new();
        second_process.set_text(PROBE_LABEL, "exit=0\n{\"free\": 99}");
        let advice = advise(&set, &second_process, &rig.clock, &rig.dir.path().join("advisory-cache")).await;
        assert_eq!(second_process.started_count(PROBE_LABEL), 0);
        assert_eq!(advice[0].value, Some(15.0));
    }

    #[tokio::test]
    async fn changing_a_param_applies_at_once_without_a_new_probe() {
        // why: owner: `vigil advisory set boat-budget threshold=15` "changes the
        //      threshold immediately". Cached 18 is above floor 15 -> false;
        //      after the floor becomes 25 -> true, with probes still 1.
        // proves: CLAIM(advisory-immediate)
        let rig = Rig::new();
        rig.probe_says(18);
        let before = set_of(&[("mem", spec(json!({"params": {"floor": 15}})))]);
        assert!(rig.advise(&before).await[0].message.starts_with("Plenty"));
        let after = set_of(&[("mem", spec(json!({"params": {"floor": 25}})))]);
        let advice = rig.advise(&after).await;
        assert_eq!(advice[0].message, "Low: 18 (< 25)");
        assert_eq!(rig.probes(), 1);
    }

    #[tokio::test]
    async fn editing_the_probe_or_extraction_invalidates_the_cache() {
        // why: a cached number belongs to the probe that produced it; after
        //      the operator edits what is probed, showing the old number
        //      would present stale data as the new probe's. Change the
        //      extracted JSON path: the next call probes again.
        // proves: CLAIM(advisory-cache)
        let rig = Rig::new();
        rig.reader.set_text(PROBE_LABEL, "exit=0\n{\"free\": 15, \"other\": 70}");
        rig.advise(&set_of(&[("mem", spec(json!({})))])).await;
        let advice = rig.advise(&set_of(&[("mem", spec(json!({"extract": {"name": "free", "json": "other"}})))])).await;
        assert_eq!((rig.probes(), advice[0].value), (2, Some(70.0)));
    }

    #[tokio::test]
    async fn cache_s_zero_probes_on_every_call() {
        // why: cache_s = 0 is "always fresh" (an operator who accepts the cost).
        // proves: CLAIM(advisory-cache)
        let rig = Rig::new();
        rig.probe_says(15);
        let set = set_of(&[("mem", spec(json!({"cache_s": 0})))]);
        rig.advise(&set).await;
        rig.advise(&set).await;
        assert_eq!(rig.probes(), 2);
    }

    #[tokio::test]
    async fn a_probe_failure_is_an_explicit_entry_and_never_the_old_value() {
        // why: owner: "A probe failure yields an explicit 'advisory <id>
        //      unavailable: <reason>' entry, never silence and never a stale
        //      value presented as fresh". Good probe, then (past cache_s) the
        //      probe exits 7: the entry is unavailable with the reason; the
        //      earlier 15 appears nowhere in it.
        // proves: CLAIM(advisory-unavailable)
        let rig = Rig::new();
        let set = set_of(&[("mem", spec(json!({})))]);
        rig.probe_says(15);
        rig.advise(&set).await;
        rig.clock.advance_secs(100);
        rig.reader.set_text(PROBE_LABEL, "exit=7\nboom: no network");
        let advice = rig.advise(&set).await;
        assert_eq!(advice.len(), 1);
        assert_eq!(advice[0].kind, AdviceKind::Unavailable);
        assert_eq!(advice[0].message, "advisory mem unavailable: probe exited with 7: boom: no network");
        assert_eq!(advice[0].value, None);
        assert!(!advice[0].message.contains("15"));
    }

    #[tokio::test]
    async fn each_failure_has_its_own_reason() {
        // why: not-JSON output, a missing field, a program that does not exist
        //      (real reader, no script) all must name themselves.
        // proves: CLAIM(advisory-unavailable)
        let rig = Rig::new();
        let set = set_of(&[("mem", spec(json!({"cache_s": 0})))]);
        rig.reader.set_text(PROBE_LABEL, "exit=0\nhello");
        assert!(rig.advise(&set).await[0].message.contains("not JSON"));
        rig.reader.set_text(PROBE_LABEL, "exit=0\n{\"other\": 1}");
        assert!(rig.advise(&set).await[0].message.contains("free"));
        let missing = set_of(&[("gone", spec(json!({"probe": {"kind": "cmd", "argv": ["/nonexistent/vigil-probe"]}})))]);
        let advice = rig.advise(&missing).await;
        assert_eq!(advice[0].kind, AdviceKind::Unavailable);
        assert!(advice[0].message.starts_with("advisory gone unavailable: cannot run /nonexistent/vigil-probe"), "{}", advice[0].message);
    }

    #[tokio::test]
    async fn a_failed_probe_is_remembered_for_at_most_30_seconds() {
        // why: a dead probe must not make every keepalive wait out its timeout,
        //      yet must be retried soon. cache_s 100: failure at t=0; at +29 s
        //      no new probe, at +30 s a new probe that now succeeds.
        // proves: CLAIM(advisory-cache), CLAIM(advisory-unavailable)
        let rig = Rig::new();
        let set = set_of(&[("mem", spec(json!({})))]);
        rig.reader.set_text(PROBE_LABEL, "exit=1\nfail");
        rig.advise(&set).await;
        rig.probe_says(15);
        rig.clock.advance_secs(29);
        assert_eq!(rig.advise(&set).await[0].kind, AdviceKind::Unavailable);
        assert_eq!(rig.probes(), 1);
        rig.clock.advance_secs(1);
        let advice = rig.advise(&set).await;
        assert_eq!((rig.probes(), advice[0].kind, advice[0].value), (2, AdviceKind::Advice, Some(15.0)));
    }

    #[tokio::test]
    async fn a_cache_stamped_in_the_future_is_not_trusted() {
        // why: the clock moved backwards (NTP, a restored VM); a "negative age"
        //      must not count as fresh forever.
        // proves: CLAIM(advisory-cache)
        let rig = Rig::new();
        let set = set_of(&[("mem", spec(json!({})))]);
        rig.probe_says(15);
        rig.clock.advance_secs(1000);
        rig.advise(&set).await;
        rig.clock.set(spec_instant());
        rig.probe_says(40);
        assert_eq!(rig.advise(&set).await[0].value, Some(40.0));
    }

    #[tokio::test]
    async fn a_garbled_cache_file_is_ignored() {
        // why: a torn or foreign file in the cache directory must cost one probe,
        //      not an error or a wrong value.
        // proves: CLAIM(advisory-cache)
        let rig = Rig::new();
        let set = set_of(&[("mem", spec(json!({})))]);
        let cache = rig.dir.path().join("advisory-cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("mem.json"), "{ not json").unwrap();
        rig.probe_says(15);
        assert_eq!(rig.advise(&set).await[0].value, Some(15.0));
    }

    #[tokio::test(start_paused = true)]
    async fn a_probe_that_never_answers_becomes_unavailable_after_the_deadline() {
        // why: a hung probe must not hang every keepalive. The reader sleeps
        //      an hour (virtual time); evaluation gives up at PROBE_DEADLINE.
        // proves: CLAIM(advisory-unavailable)
        let rig = Rig::new();
        rig.probe_says(15);
        rig.reader.set_sleep(PROBE_LABEL, std::time::Duration::from_secs(3600));
        let started = tokio::time::Instant::now();
        let advice = rig.advise(&set_of(&[("mem", spec(json!({})))])).await;
        assert_eq!(started.elapsed(), PROBE_DEADLINE);
        assert_eq!(advice[0].message, "advisory mem unavailable: probe did not answer within 20s");
    }

    #[tokio::test]
    async fn a_broken_entry_is_unavailable_and_the_others_still_answer() {
        // why: CLAIM(advisory-unavailable): a hand edit that no longer
        //      validates shows as its own entry; the neighbour is unaffected;
        //      file order is kept.
        // proves: CLAIM(advisory-unavailable)
        let rig = Rig::new();
        rig.probe_says(15);
        let mut set = set_of(&[("mem", spec(json!({})))]);
        set.entries.insert(0, Entry::Broken { id: "typo".into(), reason: "invalid condition: expected `<a> <op> <b>`".into() });
        let advice = rig.advise(&set).await;
        assert_eq!(advice.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["typo", "mem"]);
        assert_eq!(advice[0].message, "advisory typo unavailable: invalid condition: expected `<a> <op> <b>`");
        assert_eq!(advice[1].kind, AdviceKind::Advice);
    }

    #[tokio::test]
    async fn the_wire_form_of_an_entry_has_id_kind_message_and_when() {
        // why: hooks and agents read these keys; pin the JSON names and that
        //      `value` is absent for an unavailable entry.
        // proves: CLAIM(advisory-wire)
        let rig = Rig::new();
        rig.probe_says(15);
        let advice = rig.advise(&set_of(&[("mem", spec(json!({})))])).await;
        assert_eq!(serde_json::to_value(&advice[0]).unwrap(), json!({
            "id": "mem", "kind": "advice", "message": "Low: 15 (< 20)", "checked_at": "2026-10-29T14:03:11Z", "value": 15.0
        }));
        let down = Advice::unavailable("mem", "boom", spec_instant());
        assert_eq!(serde_json::to_value(&down).unwrap(), json!({
            "id": "mem", "kind": "unavailable", "message": "advisory mem unavailable: boom", "checked_at": "2026-10-29T14:03:11Z"
        }));
    }
}
