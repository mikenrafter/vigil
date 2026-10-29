//! Runtime configuration read from the environment at the binary edge.

use std::time::Duration;

pub const DEFAULT_JUDGE_URL: &str = "http://127.0.0.1:4010";
pub const DEFAULT_JUDGE_STATE_BYTES: usize = 2560;
pub const DEFAULT_DIGEST_MAX_BYTES: usize = 8192;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// `VIGIL_JUDGE_URL` (D5).
    pub judge_url: String,
    /// `VIGIL_JUDGE_STATE_BYTES`.
    pub judge_state_bytes: usize,
    /// `VIGIL_DIGEST_MAX_BYTES`.
    pub digest_max_bytes: usize,
    /// Judge predict timeout (SPEC: default 10 s).
    pub judge_timeout: Duration,
    /// Judge `/health` timeout (SPEC: 2 s).
    pub health_timeout: Duration,
}

impl Config {
    /// Build from an environment lookup (`|name| std::env::var(name).ok()` in
    /// the binary; a closure over a map in tests). Unset or unparsable
    /// numeric values fall back to the defaults above.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Config {
        todo!()
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
    fn environment_overrides_each_documented_variable() {
        // why: the three variables named in SPEC must each be honoured.
        let config = Config::from_lookup(lookup(&[
            ("VIGIL_JUDGE_URL", "http://10.0.0.5:9"),
            ("VIGIL_JUDGE_STATE_BYTES", "100"),
            ("VIGIL_DIGEST_MAX_BYTES", "200"),
        ]));
        assert_eq!(config.judge_url, "http://10.0.0.5:9");
        assert_eq!(config.judge_state_bytes, 100);
        assert_eq!(config.digest_max_bytes, 200);
    }
}
