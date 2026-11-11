//! Runtime configuration read from the environment at the binary edge.

use std::path::PathBuf;
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
/// AI2' (A10): the `uw` binary run for the credit check (`VIGIL_UW_COMMAND`).
pub const DEFAULT_UW_COMMAND: &str = "uw";
/// AI2': the credit check repeats this often during a long call (240 s).
pub const DEFAULT_CREDIT_CHECK_EVERY: Duration = Duration::from_secs(240);
/// AI8: the credit-change hook is killed after this long (10 s).
pub const DEFAULT_CREDIT_HOOK_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// AI1' (A7): where Claude Code keeps session transcripts. Order:
    /// `VIGIL_CLAUDE_PROJECTS_DIR`, `$CLAUDE_CONFIG_DIR/projects`,
    /// `$HOME/.claude/projects`; `None` when none of them can be formed.
    pub claude_projects_dir: Option<PathBuf>,
    /// AI2' (A10): `VIGIL_UW_COMMAND`, default `uw`.
    pub uw_command: PathBuf,
    /// AI8 (A11): `VIGIL_ON_CREDIT_CHANGE`; empty or unset is `None`.
    pub on_credit_change: Option<String>,
    /// AI2': pause between credit checks inside a long call (default 240 s).
    pub credit_check_every: Duration,
    /// AI8: how long the hook may run (default 10 s).
    pub credit_hook_timeout: Duration,
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
        let non_empty = |name: &str| lookup(name).filter(|value| !value.is_empty());
        let claude_projects_dir = non_empty("VIGIL_CLAUDE_PROJECTS_DIR")
            .map(PathBuf::from)
            .or_else(|| non_empty("CLAUDE_CONFIG_DIR").map(|dir| PathBuf::from(dir).join("projects")))
            .or_else(|| non_empty("HOME").map(|home| PathBuf::from(home).join(".claude/projects")));
        Config {
            judge_url: lookup("VIGIL_JUDGE_URL").unwrap_or_else(|| DEFAULT_JUDGE_URL.to_string()),
            judge_state_bytes: number("VIGIL_JUDGE_STATE_BYTES", DEFAULT_JUDGE_STATE_BYTES, MIN_JUDGE_STATE_BYTES),
            digest_max_bytes: number("VIGIL_DIGEST_MAX_BYTES", DEFAULT_DIGEST_MAX_BYTES, MIN_DIGEST_MAX_BYTES),
            judge_timeout: Duration::from_secs(10),
            health_timeout: Duration::from_secs(2),
            claude_projects_dir,
            uw_command: PathBuf::from(non_empty("VIGIL_UW_COMMAND").unwrap_or_else(|| DEFAULT_UW_COMMAND.to_string())),
            on_credit_change: non_empty("VIGIL_ON_CREDIT_CHANGE"),
            credit_check_every: DEFAULT_CREDIT_CHECK_EVERY,
            credit_hook_timeout: DEFAULT_CREDIT_HOOK_TIMEOUT,
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
    fn the_auto_interval_settings_have_their_documented_defaults() {
        // why: AI2' (240 s credit check), AI8 (10 s hook timeout, no hook by
        //      default), A10 (`uw` on PATH), A7 (no HOME: nowhere to look).
        //      The shipped values, not the overrides tests use elsewhere.
        // proves: CLAIM(AI2'), CLAIM(AI8)
        let config = Config::from_lookup(lookup(&[]));
        assert_eq!(config.credit_check_every, Duration::from_secs(240));
        assert_eq!(config.credit_hook_timeout, Duration::from_secs(10));
        assert_eq!(config.uw_command, PathBuf::from("uw"));
        assert_eq!(config.on_credit_change, None);
        assert_eq!(config.claude_projects_dir, None);
    }

    #[test]
    fn the_claude_projects_dir_follows_vigil_then_claude_config_then_home() {
        // why: A7: Claude Code relocates ~/.claude with CLAUDE_CONFIG_DIR, and
        //      tests (and odd installs) need a vigil-specific override.
        //      Order: VIGIL_CLAUDE_PROJECTS_DIR, $CLAUDE_CONFIG_DIR/projects,
        //      $HOME/.claude/projects. An empty value is unset, so a blank
        //      override cannot point vigil at the current directory.
        // proves: CLAIM(AI1')
        let dir = |pairs: &[(&str, &str)]| Config::from_lookup(lookup(pairs)).claude_projects_dir;
        assert_eq!(dir(&[("HOME", "/home/u")]), Some(PathBuf::from("/home/u/.claude/projects")));
        assert_eq!(dir(&[("HOME", "/home/u"), ("CLAUDE_CONFIG_DIR", "/cfg")]), Some(PathBuf::from("/cfg/projects")));
        assert_eq!(
            dir(&[("HOME", "/home/u"), ("CLAUDE_CONFIG_DIR", "/cfg"), ("VIGIL_CLAUDE_PROJECTS_DIR", "/v")]),
            Some(PathBuf::from("/v"))
        );
        assert_eq!(dir(&[("HOME", "/home/u"), ("CLAUDE_CONFIG_DIR", ""), ("VIGIL_CLAUDE_PROJECTS_DIR", "")]), Some(PathBuf::from("/home/u/.claude/projects")));
        assert_eq!(dir(&[("HOME", "")]), None);
        assert_eq!(dir(&[("CLAUDE_CONFIG_DIR", "/cfg")]), Some(PathBuf::from("/cfg/projects")), "no HOME needed when the config dir is given");
    }

    #[test]
    fn the_uw_command_and_the_credit_hook_come_from_the_environment() {
        // why: A10 `VIGIL_UW_COMMAND` (tests must be able to replace `uw`),
        //      A11 `VIGIL_ON_CREDIT_CHANGE` (the AI8 hook). Empty is unset,
        //      like VIGIL_TEST_FAULT="" (b2a0709).
        // proves: CLAIM(AI8)
        let config = Config::from_lookup(lookup(&[("VIGIL_UW_COMMAND", "/opt/bin/uw"), ("VIGIL_ON_CREDIT_CHANGE", "claude-cache-ttl-policy --flag")]));
        assert_eq!(config.uw_command, PathBuf::from("/opt/bin/uw"));
        assert_eq!(config.on_credit_change.as_deref(), Some("claude-cache-ttl-policy --flag"));
        let blank = Config::from_lookup(lookup(&[("VIGIL_UW_COMMAND", ""), ("VIGIL_ON_CREDIT_CHANGE", "")]));
        assert_eq!(blank.uw_command, PathBuf::from("uw"));
        assert_eq!(blank.on_credit_change, None);
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
