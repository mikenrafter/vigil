//! The envelope returned to the agent on every `keepalive` return.

use serde::{Deserialize, Serialize};

use crate::clock::Timestamp;
use crate::watch::{Event, JudgeStatus, WatchId, WatchStatus};

/// Appended to a digest that was cut to `VIGIL_DIGEST_MAX_BYTES`.
pub const DIGEST_CUT_MARKER: &str = "[digest cut]";

/// V20: exactly three wire values. The agent loops only on the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Next {
    /// `pending`.
    #[serde(rename = "call keepalive again")]
    CallKeepaliveAgain,
    /// `fired`.
    #[serde(rename = "act on events")]
    ActOnEvents,
    /// `expired` or `cancelled`.
    #[serde(rename = "watch ended; create a new watch if still needed")]
    WatchEnded,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// V8: the watch every watch field below describes.
    pub watch_id: WatchId,
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

/// Cut `digest` so the WHOLE result, `DIGEST_CUT_MARKER` included, is at most
/// `max_bytes` (V27, S27). A digest that already fits is returned untouched
/// and unmarked. Otherwise keep the newest end on a UTF-8 boundary and append
/// the marker (R6). When `max_bytes` is smaller than the marker there is no
/// room to say so: the result still obeys the bound (see `.red-report.md`).
pub fn cut_digest(digest: &str, max_bytes: usize) -> String {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{spec_instant, wid};
    use serde_json::json;

    fn quiet(max_s: Option<u64>) -> Envelope {
        Envelope {
            watch_id: wid(&"w".repeat(64)),
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
        //      cut digest says so. A digest of exactly the limit fits (V27:
        //      the bound is inclusive); one byte more does not.
        // proves: CLAIM(digest-cut)
        assert_eq!(cut_digest("short digest", 8192), "short digest");
        assert!(!cut_digest("short digest", 8192).contains(DIGEST_CUT_MARKER));
        let exact = "x".repeat(40);
        assert_eq!(cut_digest(&exact, 40), exact, "len == limit is not cut");
        let over = "x".repeat(41);
        let cut = cut_digest(&over, 40);
        assert!(cut.ends_with(DIGEST_CUT_MARKER) && cut.len() <= 40, "{cut:?}");
    }

    #[test]
    fn digest_over_the_limit_is_cut_keeps_the_tail_and_the_whole_result_obeys_the_limit() {
        // why: SPEC "Envelope" + V27 (S27): digest "Bounded by
        //      VIGIL_DIGEST_MAX_BYTES ... If cut, say so", and the bound
        //      INCLUDES the `[digest cut]` marker (the old rule allowed
        //      max + marker + 1). Limit 20, marker 12 bytes -> at most 8 bytes
        //      (7 with a newline separator) of content: the end "xxx-tail"
        //      (or "xx-tail") must survive, the head must not.
        // proves: CLAIM(digest-cut)
        let digest = format!("head-{}-tail", "x".repeat(100));
        let cut = cut_digest(&digest, 20);
        assert!(cut.ends_with(DIGEST_CUT_MARKER), "{cut:?}");
        assert!(cut.contains("-tail"), "{cut:?}");
        assert!(!cut.contains("head-"));
        assert!(cut.len() <= 20, "{} bytes: {cut:?}", cut.len());
    }

    #[test]
    fn digest_bound_holds_for_every_tiny_limit() {
        // why: V27 (S27): "including tiny limits". For limits 0..=40 over a
        //      multi-byte digest the result NEVER exceeds the limit. From the
        //      marker's own length (12) up it ends with the marker; with room
        //      for a separator plus one char (14) it also keeps the final
        //      char. Below 12 there is no room for the marker, only the
        //      bound is promised (flagged in .red-report.md).
        // proves: CLAIM(digest-cut)
        let digest = format!("{}END", "é".repeat(60));
        for limit in 0..=40usize {
            let cut = cut_digest(&digest, limit);
            assert!(cut.len() <= limit, "limit {limit}: {} bytes {cut:?}", cut.len());
            if limit >= DIGEST_CUT_MARKER.len() {
                assert!(cut.ends_with(DIGEST_CUT_MARKER), "limit {limit}: {cut:?}");
            }
            if limit >= DIGEST_CUT_MARKER.len() + 4 {
                assert!(cut.contains("END"), "limit {limit}: newest content must survive: {cut:?}");
            }
        }
        assert_eq!(cut_digest(&digest, 0), "");
    }

    #[test]
    fn digest_cut_respects_utf8_boundaries() {
        // why: the cut must never split a multi-byte char. 50 x "é" (2 bytes)
        //      with max 25 and a 12-byte marker leaves 13 bytes (12 if a
        //      1-byte separator is used) -> the largest even count is 12
        //      bytes = 6 chars either way.
        let cut = cut_digest(&"é".repeat(50), 25);
        assert!(cut.len() <= 25, "{} bytes", cut.len());
        let kept = cut.replace(DIGEST_CUT_MARKER, "");
        let kept = kept.trim();
        assert!(kept.chars().all(|c| c == 'é'));
        assert_eq!(kept.len(), 12);
    }

    #[test]
    fn pending_envelope_has_exactly_the_spec_fields_and_wire_strings() {
        // why: parity with SPEC "Envelope" + V8 (watch_id) + V26 (max_s,
        //      waited_s): field names, `now` format, judge_status strings, and
        //      that waited_s/max_s appear on blocking returns only.
        // proves: CLAIM(envelope-format)
        let value = serde_json::to_value(quiet(Some(270))).unwrap();
        let mut keys: Vec<&str> = value.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["deadline_in_s", "digest", "elapsed_s", "events", "judge_status", "max_s", "next", "now",
                 "session_ticks", "status", "tick", "waited_s", "watch_id"]
        );
        assert_eq!(value["watch_id"], "w".repeat(64));
        assert_eq!(value["now"], "2026-10-29T14:03:11Z");
        assert_eq!(value["status"], "pending");
        assert_eq!(value["judge_status"], "skipped");
        assert_eq!(value["events"], json!([]));
        let quiet_non_blocking = serde_json::to_value(quiet(None)).unwrap();
        assert!(quiet_non_blocking.get("waited_s").is_none() && quiet_non_blocking.get("max_s").is_none());
    }

    #[test]
    fn next_has_exactly_the_three_wire_strings() {
        // why: V20 (S20): three exact wire values, asserted as literals (not
        //      "differs from pending"). The expired/cancelled text is the
        //      terminal instruction the agent loop stops on.
        // proves: CLAIM(next-strings)
        assert_eq!(serde_json::to_value(Next::CallKeepaliveAgain).unwrap(), "call keepalive again");
        assert_eq!(serde_json::to_value(Next::ActOnEvents).unwrap(), "act on events");
        assert_eq!(
            serde_json::to_value(Next::WatchEnded).unwrap(),
            "watch ended; create a new watch if still needed"
        );
        for literal in ["call keepalive again", "act on events", "watch ended; create a new watch if still needed"] {
            let parsed: Next = serde_json::from_value(json!(literal)).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap(), literal, "round trip of {literal:?}");
        }
        assert!(serde_json::from_value::<Next>(json!("call keepalive")).is_err());
    }

    #[test]
    fn a_quiet_pending_envelope_is_small() {
        // why: SPEC "Envelope": a quiet hour costs "a few hundred tokens per
        //      tick". Derivation: worst case with a 64-byte watch_id and
        //      5-6 digit counters serializes to 310 bytes (computed by
        //      serializing the struct below in a scratch script); bound 400
        //      fails if a digest or event sneaks in.
        // proves: CLAIM(quiet-envelope-small)
        let text = serde_json::to_string(&quiet(Some(270))).unwrap();
        assert!(text.len() < 400, "{} bytes: {text}", text.len());
    }
}
