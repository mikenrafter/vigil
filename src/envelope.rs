//! The envelope returned to the agent on every `keepalive` return.

use serde::{Deserialize, Serialize};

use crate::clock::Timestamp;
use crate::watch::{Event, JudgeStatus, WatchStatus};

/// Appended to a digest that was cut to `VIGIL_DIGEST_MAX_BYTES`.
pub const DIGEST_CUT_MARKER: &str = "[digest cut]";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Next {
    #[serde(rename = "call keepalive again")]
    CallKeepaliveAgain,
    #[serde(rename = "act on events")]
    ActOnEvents,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub status: WatchStatus,
    pub now: Timestamp,
    pub tick: u64,
    pub session_ticks: u64,
    pub elapsed_s: u64,
    pub deadline_in_s: u64,
    /// Seconds actually waited; only on blocking returns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waited_s: Option<u64>,
    /// The clamped ceiling used (D9); only on blocking returns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_s: Option<u64>,
    pub events: Vec<Event>,
    pub digest: String,
    pub judge_status: JudgeStatus,
    pub next: Next,
}

/// Cut `digest` to at most `max_bytes` of content (keeping the newest, i.e.
/// the end) on a UTF-8 boundary; when cut, include `DIGEST_CUT_MARKER`.
/// Total length is at most `max_bytes + DIGEST_CUT_MARKER.len() + 1`.
pub fn cut_digest(digest: &str, max_bytes: usize) -> String {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::spec_instant;
    use serde_json::json;

    fn quiet(max_s: Option<u64>) -> Envelope {
        Envelope {
            status: WatchStatus::Pending,
            now: spec_instant(),
            tick: 99_999,
            session_ticks: 99_999,
            elapsed_s: 604_800,
            deadline_in_s: 604_800,
            waited_s: max_s,
            max_s,
            events: vec![],
            digest: String::new(),
            judge_status: JudgeStatus::Skipped,
            next: Next::CallKeepaliveAgain,
        }
    }

    #[test]
    fn digest_under_the_limit_is_returned_untouched() {
        // why: SPEC "Envelope": only an over-limit digest is cut, and only a
        //      cut digest says so.
        assert_eq!(cut_digest("short digest", 8192), "short digest");
        assert!(!cut_digest("short digest", 8192).contains(DIGEST_CUT_MARKER));
    }

    #[test]
    fn digest_over_the_limit_is_cut_keeps_the_tail_and_says_so() {
        // why: SPEC "Envelope": digest "Bounded by VIGIL_DIGEST_MAX_BYTES ...
        //      If cut, say so." The newest content (the end) is what the agent
        //      needs. Length contract in the fn doc: max + marker + 1 newline.
        // proves: CLAIM(digest-cut)
        let digest = format!("head-{}-tail", "x".repeat(100));
        let cut = cut_digest(&digest, 20);
        assert!(cut.contains(DIGEST_CUT_MARKER), "{cut:?}");
        assert!(cut.contains("-tail"));
        assert!(!cut.contains("head-"));
        assert!(cut.len() <= 20 + DIGEST_CUT_MARKER.len() + 1, "{} bytes", cut.len());
    }

    #[test]
    fn digest_cut_respects_utf8_boundaries() {
        // why: the cut must never split a multi-byte char. 50 x "é" (2 bytes)
        //      with max 21 keeps the largest even byte count <= 21, i.e. 20 bytes = 10 chars.
        let cut = cut_digest(&"é".repeat(50), 21);
        let kept = cut.replace(DIGEST_CUT_MARKER, "");
        let kept = kept.trim();
        assert!(kept.chars().all(|c| c == 'é'));
        assert_eq!(kept.len(), 20);
    }

    #[test]
    fn pending_envelope_has_exactly_the_spec_fields_and_wire_strings() {
        // why: parity with SPEC "Envelope": field names, `now` format, `next`
        //      strings, judge_status strings, and that waited_s/max_s appear
        //      on blocking returns (D9).
        // proves: CLAIM(envelope-format)
        let value = serde_json::to_value(quiet(Some(270))).unwrap();
        let mut keys: Vec<&str> = value.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["deadline_in_s", "digest", "elapsed_s", "events", "judge_status", "max_s", "next", "now",
                 "session_ticks", "status", "tick", "waited_s"]
        );
        assert_eq!(value["now"], "2026-10-29T14:03:11Z");
        assert_eq!(value["status"], "pending");
        assert_eq!(value["next"], "call keepalive again");
        assert_eq!(value["judge_status"], "skipped");
        assert_eq!(value["events"], json!([]));
        assert_eq!(serde_json::to_value(Next::ActOnEvents).unwrap(), "act on events");
        assert!(serde_json::to_value(quiet(None)).unwrap().get("waited_s").is_none());
    }

    #[test]
    fn a_quiet_pending_envelope_is_small() {
        // why: SPEC "Envelope": a quiet hour costs "a few hundred tokens per
        //      tick". Derivation: fixed keys/punctuation/strings ~ 200 bytes,
        //      worst-case numbers (5-6 digits x 6) ~ 40 bytes => ~ 250; bound
        //      400 leaves margin but fails if a digest or event sneaks in.
        // proves: CLAIM(quiet-envelope-small)
        let text = serde_json::to_string(&quiet(Some(270))).unwrap();
        assert!(text.len() < 400, "{} bytes: {text}", text.len());
    }
}
