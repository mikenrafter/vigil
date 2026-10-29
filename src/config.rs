//! Runtime configuration read from the environment at the binary edge.

use std::time::Duration;

pub const DEFAULT_JUDGE_URL: &str = "http://127.0.0.1:4010";
pub const DEFAULT_JUDGE_STATE_BYTES: usize = 2560;
pub const DEFAULT_DIGEST_MAX_BYTES: usize = 8192;
/// W2: `VIGIL_JUDGE_STATE_BYTES` is clamped to at least this (1024 prompt +
/// 1536 observations), so the judge state never exceeds the setting.
pub const MIN_JUDGE_STATE_BYTES: usize = 2560;
/// W3: `VIGIL_DIGEST_MAX_BYTES` is clamped to at least this, so the
/// `[digest cut]` marker (12 bytes) always fits with content beside it.
pub const MIN_DIGEST_MAX_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// `VIGIL_JUDGE_URL` (D5).
    pub judge_url: String,
    /// `VIGIL_JUDGE_STATE_BYTES`, already clamped to >= 2560 by
    /// `from_lookup` (W2).
    pub judge_state_bytes: usize,
    /// `VIGIL_DIGEST_MAX_BYTES`, already clamped to >= 64 by `from_lookup`
    /// (W3).
    pub digest_max_bytes: usize,
    /// Judge predict timeout (SPEC: default 10 s).
    pub judge_timeout: Duration,
    /// Judge `/health` timeout (SPEC: 2 s).
    pub health_timeout: Duration,
}

impl Config {
    /// Build from an environment lookup (`|name| std::env::var(name).ok()` in
    /// the binary; a closure over a map in tests). Unset or unparsable
    /// numeric values fall back to the defaults above; parsed values below
    /// `MIN_JUDGE_STATE_BYTES` / `MIN_DIGEST_MAX_BYTES` are raised to them
    /// (W2, W3). Values above the floors are taken as given.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Config {
        let number = |name: &str, default: usize, floor: usize| {
            lookup(name)
                .and_then(|raw| raw.trim().parse::<usize>().ok())
                .map_or(default, |value| value.max(floor))
        };
        Config {
            judge_url: lookup("VIGIL_JUDGE_URL").unwrap_or_else(|| DEFAULT_JUDGE_URL.to_string()),
            judge_state_bytes: number("VIGIL_JUDGE_STATE_BYTES", DEFAULT_JUDGE_STATE_BYTES, MIN_JUDGE_STATE_BYTES),
            digest_max_bytes: number("VIGIL_DIGEST_MAX_BYTES", DEFAULT_DIGEST_MAX_BYTES, MIN_DIGEST_MAX_BYTES),
            judge_timeout: Duration::from_secs(10),
            health_timeout: Duration::from_secs(2),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn unset_environment_gives_the_documented_defaults() {
        // why: SPEC "Tick logic"/"Envelope"/D5/Failure rules: judge URL
        //      http://127.0.0.1:4010, state 2560 bytes, digest 8192 bytes,
        //      judge timeout 10 s, health timeout 2 s.
        // proves: CLAIM(config-defaults)
        let config = Config::from_lookup(lookup(&[]));
        assert_eq!(config.judge_url, "http://127.0.0.1:4010");
        assert_eq!(config.judge_state_bytes, 2560);
        assert_eq!(config.digest_max_bytes, 8192);
        assert_eq!(config.judge_timeout, Duration::from_secs(10));
        assert_eq!(config.health_timeout, Duration::from_secs(2));
    }

    #[test]
    fn environment_overrides_each_documented_variable_when_above_the_floors() {
        // why: the three variables named in SPEC must each be honoured. Values
        //      above the W2/W3 floors (2560 / 64) are taken as given: the
        //      above-floor override half of T6.
        // proves: CLAIM(config-env)
        let config = Config::from_lookup(lookup(&[
            ("VIGIL_JUDGE_URL", "http://10.0.0.5:9"),
            ("VIGIL_JUDGE_STATE_BYTES", "4000"),
            ("VIGIL_DIGEST_MAX_BYTES", "200"),
        ]));
        assert_eq!(config.judge_url, "http://10.0.0.5:9");
        assert_eq!(config.judge_state_bytes, 4000);
        assert_eq!(config.digest_max_bytes, 200);
    }

    #[test]
    fn judge_state_bytes_below_2560_is_raised_to_2560_and_the_floor_is_inclusive() {
        // why: W2 (T6): VIGIL_JUDGE_STATE_BYTES is clamped to at least 2560
        //      (1024-byte prompt + 1536 observation bytes), so total judge
        //      state never exceeds the setting. Boundary: 2559 -> 2560, 2560
        //      stays, 2561 stays. 0 and 1 parse as numbers and are raised too;
        //      garbage and negative numbers do not parse and take the default
        //      (also 2560, so those two rows would pass without the clamp: the
        //      numeric rows are the ones that bite).
        // proves: CLAIM(config-floors)
        let cases = [
            ("0", 2560), ("1", 2560), ("100", 2560), ("1536", 2560), ("2559", 2560),
            ("2560", 2560), ("2561", 2561), ("4000", 4000), ("1000000", 1_000_000),
            ("garbage", 2560), ("-5", 2560), ("", 2560),
        ];
        for (raw, expected) in cases {
            let config = Config::from_lookup(lookup(&[("VIGIL_JUDGE_STATE_BYTES", raw)]));
            assert_eq!(config.judge_state_bytes, expected, "VIGIL_JUDGE_STATE_BYTES={raw:?}");
        }
        assert_eq!(MIN_JUDGE_STATE_BYTES, 2560, "the floor is 1024 prompt + 1536 observations");
    }

    #[test]
    fn digest_max_bytes_below_64_is_raised_to_64_and_the_floor_is_inclusive() {
        // why: W3 (T6): VIGIL_DIGEST_MAX_BYTES is clamped to at least 64 so the
        //      12-byte `[digest cut]` marker always fits with content beside
        //      it. Boundary: 63 -> 64, 64 stays, 65 stays; 0, 1 and 12 (the
        //      marker length) are raised. Unparsable values take the 8192
        //      default.
        // proves: CLAIM(config-floors)
        let cases = [
            ("0", 64), ("1", 64), ("12", 64), ("63", 64), ("64", 64), ("65", 65),
            ("200", 200), ("8192", 8192), ("garbage", 8192), ("-1", 8192),
        ];
        for (raw, expected) in cases {
            let config = Config::from_lookup(lookup(&[("VIGIL_DIGEST_MAX_BYTES", raw)]));
            assert_eq!(config.digest_max_bytes, expected, "VIGIL_DIGEST_MAX_BYTES={raw:?}");
        }
        assert_eq!(MIN_DIGEST_MAX_BYTES, 64);
    }

    #[test]
    fn the_two_floors_are_independent() {
        // why: W2/W3: clamping one variable must not touch the other (a shared
        //      clamp helper that returns the larger floor would pass the two
        //      tests above for one of them only).
        let config = Config::from_lookup(lookup(&[("VIGIL_JUDGE_STATE_BYTES", "10"), ("VIGIL_DIGEST_MAX_BYTES", "3000")]));
        assert_eq!((config.judge_state_bytes, config.digest_max_bytes), (2560, 3000));
        let config = Config::from_lookup(lookup(&[("VIGIL_JUDGE_STATE_BYTES", "9000"), ("VIGIL_DIGEST_MAX_BYTES", "10")]));
        assert_eq!((config.judge_state_bytes, config.digest_max_bytes), (9000, 64));
    }
}
