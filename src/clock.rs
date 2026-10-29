//! Wall-clock abstraction. Core logic never calls `Utc::now()` directly, so
//! tests drive time with `testing::ManualClock` or `testing::TokioClock`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A UTC instant. Serialized as RFC 3339 with a `Z` suffix and whole seconds
/// (`2026-10-29T14:03:11Z`, SPEC "Envelope"); sub-second parts are dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub DateTime<Utc>);

impl Timestamp {
    pub fn plus_secs(self, secs: u64) -> Timestamp {
        Timestamp(self.0 + chrono::Duration::seconds(secs as i64))
    }
    pub fn minus_secs(self, secs: u64) -> Timestamp {
        Timestamp(self.0 - chrono::Duration::seconds(secs as i64))
    }
    /// Whole seconds from `earlier` to `self` (negative if `self` is earlier).
    pub fn secs_since(self, earlier: Timestamp) -> i64 {
        (self.0 - earlier.0).num_seconds()
    }
}

impl Serialize for Timestamp {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let text = self.0.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        serializer.serialize_str(&text)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        DateTime::parse_from_rfc3339(&text)
            .map(|parsed| Timestamp(parsed.with_timezone(&Utc)))
            .map_err(serde::de::Error::custom)
    }
}

pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

/// Production clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp(Utc::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{at, ManualClock};

    #[test]
    fn timestamp_serializes_as_utc_rfc3339_whole_seconds() {
        // why: SPEC "Envelope" example: "now": "2026-10-29T14:03:11Z". Pinned
        //      as a literal, not derived from the serializer.
        // proves: CLAIM(timestamp-format)
        let json = serde_json::to_string(&at("2026-10-29T14:03:11Z")).unwrap();
        assert_eq!(json, "\"2026-10-29T14:03:11Z\"");
    }

    #[test]
    fn timestamp_drops_sub_second_precision() {
        // why: envelopes must stay small and match the spec example even when
        //      the system clock has nanoseconds.
        let fractional = at("2026-10-29T14:03:11.750Z");
        assert_eq!(serde_json::to_string(&fractional).unwrap(), "\"2026-10-29T14:03:11Z\"");
    }

    #[test]
    fn timestamp_deserializes_what_it_serializes_and_offsets_normalize_to_utc() {
        // why: watch files are written by one process and read by another;
        //      a +02:00 literal must be read as the same instant in UTC.
        let parsed: Timestamp = serde_json::from_str("\"2026-10-29T16:03:11+02:00\"").unwrap();
        assert_eq!(parsed, at("2026-10-29T14:03:11Z"));
    }

    #[test]
    fn manual_clock_moves_only_when_told() {
        // why: control for the fixture every deterministic test relies on.
        let clock = ManualClock::new(at("2026-10-29T14:03:11Z"));
        assert_eq!(clock.now(), at("2026-10-29T14:03:11Z"));
        clock.advance_secs(90);
        assert_eq!(clock.now(), at("2026-10-29T14:04:41Z"));
    }
}
