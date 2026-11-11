//! Write fault injection for the store (specs/disk-full.md, "Test seam").
//!
//! RED PHASE STUB. The types and signatures below are the contract the red
//! tests compile against. `check` never fails, `attempts` counts nothing and
//! `parse` accepts everything, so with this stub the store behaves exactly as
//! before; the green phase makes them real and wires `check` into the store.
//!
//! Why a runtime plan and not `cfg(test)`: the end-to-end suites spawn the
//! built `vigil` binary, and a `cfg(test)` or cargo-feature hook is invisible
//! to it. A feature would need `required-features` on the e2e targets, which
//! makes a plain `cargo test` skip them silently. So the plan travels in an
//! environment variable ([`ENV_VAR`]) that `main.rs` reads at the edge (like
//! `XDG_STATE_HOME`); unset, the plan is [`FaultPlan::none`] and no code path
//! differs from production. Accepted risk: someone exports the variable on a
//! real host. It is named `..._TEST_...` and an unparseable value is a hard
//! error, never silently ignored.
//!
//! Contract of the seam (what the green phase must provide):
//!
//! * The store calls [`FaultPlan::check`] once per ATTEMPT of each kind of
//!   write, so the retry after a disk-full failure is a second attempt:
//!   - [`WriteKind::Watch`]: inside `write_atomic`, AFTER `<path>.tmp` was
//!     created and BEFORE any byte is written (a stand-in for ENOSPC on
//!     `write_all`), so the `.tmp` cleanup path is on the line;
//!   - [`WriteKind::Lock`]: only when `open_lock_file` has to CREATE the lock
//!     file (opening an existing one needs no disk space and is not an
//!     attempt);
//!   - [`WriteKind::Reserve`]: before the best-effort creation of
//!     `<root>/.reserve`.
//! * An `Err` from `check` is the injected failure: an `io::Error` built with
//!   `io::Error::from_raw_os_error(errno)`, so `raw_os_error()` is the chosen
//!   errno.
//! * Rules are matched per kind; `Nth` / `Range` count attempts of that kind
//!   only, 1-based, shared by every clone of the plan.
//!
//! Spec syntax of [`FaultPlan::parse`] (the environment variable): rules
//! separated by `,`, each `KIND:ERRNO:WHEN`, with `KIND` one of `watch`,
//! `lock`, `reserve`; `ERRNO` a decimal errno; `WHEN` one of `all`, `N`
//! (only the N-th attempt), `N..M` (attempts N through M), `N..` (attempt N
//! and every later one), `while=PATH` (every attempt while PATH exists: a
//! file the test creates or removes models a disk that is full or has room,
//! across process boundaries). N is at least 1.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The environment variable a spawned `vigil` reads its [`FaultPlan`] from.
pub const ENV_VAR: &str = "VIGIL_TEST_FAULT";

/// What a store write is for; a rule selects by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WriteKind {
    /// A watch file (`.tmp` write of `write_atomic`).
    Watch,
    /// Creation of a lock file.
    Lock,
    /// Creation of `<root>/.reserve`.
    Reserve,
}

/// Which attempts of a kind a rule fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum When {
    Every,
    /// Only the N-th attempt (1-based).
    Nth(u64),
    /// Attempts `first..=last`, or `first` onwards when `last` is `None`.
    Range { first: u64, last: Option<u64> },
    /// Every attempt while this path exists.
    WhileExists(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaultRule {
    pub kind: WriteKind,
    /// Raw OS error number: 28 (ENOSPC), 122 (EDQUOT), 5 (EIO), 13 (EACCES)...
    pub errno: i32,
    pub when: When,
}

#[derive(Debug, Default)]
struct FaultState {
    rules: Vec<FaultRule>,
    attempts: HashMap<WriteKind, u64>,
}

/// The faults to inject. Clones share rules and counters (the store is
/// `Clone`; "the 2nd write" must not depend on which clone wrote).
#[derive(Debug, Clone, Default)]
pub struct FaultPlan {
    shared: Arc<Mutex<FaultState>>,
}

impl FaultPlan {
    /// Production: never fails anything.
    pub fn none() -> FaultPlan {
        FaultPlan::default()
    }

    pub fn from_rules(rules: Vec<FaultRule>) -> FaultPlan {
        FaultPlan { shared: Arc::new(Mutex::new(FaultState { rules, attempts: HashMap::new() })) }
    }

    /// Parse the [`ENV_VAR`] syntax (see the module docs). `Err` carries a
    /// message naming what is wrong.
    pub fn parse(spec: &str) -> Result<FaultPlan, String> {
        let _ = spec;
        Ok(FaultPlan::none()) // STUB: green parses.
    }

    /// Called by the store before one attempt of `kind`; `Err` is the injected
    /// failure. Counts the attempt.
    pub fn check(&self, kind: WriteKind) -> io::Result<()> {
        let _ = kind;
        Ok(()) // STUB: green matches the rules and counts.
    }

    /// How many attempts of `kind` were checked so far (all clones).
    pub fn attempts(&self, kind: WriteKind) -> u64 {
        let _ = kind;
        0 // STUB: green returns the counter.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These pin the SEAM itself. Every other disk-full test injects its fault
    // through it; if the seam fired at the wrong attempt, those tests would
    // prove nothing (or fail for the wrong reason). Co-located with the seam.

    fn errno_of(result: io::Result<()>) -> Option<i32> {
        result.err().map(|error| error.raw_os_error().expect("an injected error carries a raw os error"))
    }

    /// The raw errno of each of `count` consecutive `kind` attempts.
    fn run(plan: &FaultPlan, kind: WriteKind, count: usize) -> Vec<Option<i32>> {
        (0..count).map(|_| errno_of(plan.check(kind))).collect()
    }

    fn one(kind: WriteKind, errno: i32, when: When) -> FaultPlan {
        FaultPlan::from_rules(vec![FaultRule { kind, errno, when }])
    }

    #[test]
    fn an_empty_plan_never_fails_anything() {
        // why: control for every other test here: production runs with
        //      `FaultPlan::none()`, which must inject nothing, whatever the kind.
        // proves: CLAIM(DF-seam-inert)
        let plan = FaultPlan::none();
        for kind in [WriteKind::Watch, WriteKind::Lock, WriteKind::Reserve] {
            assert_eq!(run(&plan, kind, 3), vec![None, None, None], "{kind:?}");
        }
    }

    #[test]
    fn every_fails_each_attempt_of_its_kind_with_the_chosen_errno_and_leaves_other_kinds_alone() {
        // why: spec "Test seam": "every write fail with a chosen io::Error
        //      (raw os error 28 / 122 / other)". Selectivity matters: a Watch
        //      rule must not fail the reserve or a lock, or "reserve creation
        //      is best effort" could not be tested apart from the write.
        //      Counters are per kind: 3 Watch attempts, 1 Lock, 1 Reserve.
        // proves: CLAIM(DF-seam)
        let plan = one(WriteKind::Watch, 28, When::Every);
        assert_eq!(run(&plan, WriteKind::Watch, 3), vec![Some(28), Some(28), Some(28)]);
        assert_eq!(run(&plan, WriteKind::Lock, 1), vec![None]);
        assert_eq!(run(&plan, WriteKind::Reserve, 1), vec![None]);
        assert_eq!((plan.attempts(WriteKind::Watch), plan.attempts(WriteKind::Lock), plan.attempts(WriteKind::Reserve)), (3, 1, 1));
    }

    #[test]
    fn nth_and_range_fail_exactly_the_named_attempts_counting_from_one() {
        // why: spec "Test seam": "the Nth write ... fail". 1-based: Nth(1) is
        //      the first attempt. Range{2, Some(3)} is attempts 2 and 3;
        //      Range{2, None} is 2 and every later one. Expected sequences are
        //      written out by hand.
        // proves: CLAIM(DF-seam)
        assert_eq!(run(&one(WriteKind::Watch, 122, When::Nth(2)), WriteKind::Watch, 4), vec![None, Some(122), None, None]);
        assert_eq!(run(&one(WriteKind::Watch, 5, When::Nth(1)), WriteKind::Watch, 2), vec![Some(5), None]);
        assert_eq!(
            run(&one(WriteKind::Lock, 5, When::Range { first: 2, last: Some(3) }), WriteKind::Lock, 4),
            vec![None, Some(5), Some(5), None]
        );
        assert_eq!(
            run(&one(WriteKind::Reserve, 28, When::Range { first: 2, last: None }), WriteKind::Reserve, 4),
            vec![None, Some(28), Some(28), Some(28)]
        );
    }

    #[test]
    fn while_exists_follows_a_path_so_a_test_can_flip_the_disk_between_full_and_free() {
        // why: spec "Test seam" must work for a spawned binary, where a test
        //      cannot call a method on the plan. A path the test creates or
        //      removes is the only switch both sides see. Also models DF4 in
        //      full: "full until the reserve is deleted" is `while=<root>/.reserve`.
        // proves: CLAIM(DF-seam)
        let dir = tempfile::tempdir().unwrap();
        let switch = dir.path().join("disk-is-full");
        let plan = one(WriteKind::Watch, 28, When::WhileExists(switch.clone()));
        assert_eq!(errno_of(plan.check(WriteKind::Watch)), None, "path absent: the disk has room");
        std::fs::write(&switch, "").unwrap();
        assert_eq!(errno_of(plan.check(WriteKind::Watch)), Some(28), "path present: the disk is full");
        std::fs::remove_file(&switch).unwrap();
        assert_eq!(errno_of(plan.check(WriteKind::Watch)), None, "path removed: room again");
    }

    #[test]
    fn clones_of_a_plan_share_one_attempt_counter() {
        // why: `Store` is `Clone` and the engine hands copies around; "fail the
        //      2nd write" must mean the 2nd write of the process, not of
        //      whichever clone happened to make it.
        // proves: CLAIM(DF-seam)
        let plan = one(WriteKind::Watch, 28, When::Nth(2));
        let clone = plan.clone();
        assert_eq!(errno_of(plan.check(WriteKind::Watch)), None, "attempt 1 via the original");
        assert_eq!(errno_of(clone.check(WriteKind::Watch)), Some(28), "attempt 2 via the clone");
        assert_eq!(plan.attempts(WriteKind::Watch), 2);
        assert_eq!(clone.attempts(WriteKind::Watch), 2);
    }

    #[test]
    fn parse_builds_the_plan_the_documented_syntax_names() {
        // why: the spawned binary receives its plan as text (VIGIL_TEST_FAULT).
        //      Each row is a spec, the kind it is checked against, and the
        //      errno of each of the first attempts, written out by hand from
        //      the grammar in the module docs.
        // proves: CLAIM(DF-seam)
        let cases: [(&str, WriteKind, Vec<Option<i32>>); 6] = [
            ("watch:28:all", WriteKind::Watch, vec![Some(28), Some(28)]),
            ("watch:5:2", WriteKind::Watch, vec![None, Some(5), None]),
            ("watch:122:1..2", WriteKind::Watch, vec![Some(122), Some(122), None]),
            ("watch:28:3..", WriteKind::Watch, vec![None, None, Some(28), Some(28)]),
            ("lock:13:1", WriteKind::Lock, vec![Some(13), None]),
            ("reserve:28:all", WriteKind::Reserve, vec![Some(28), Some(28)]),
        ];
        for (spec, kind, expected) in cases {
            let plan = FaultPlan::parse(spec).unwrap_or_else(|error| panic!("{spec:?} should parse: {error}"));
            assert_eq!(run(&plan, kind, expected.len()), expected, "{spec:?}");
        }
        let both = FaultPlan::parse("watch:28:1,reserve:5:all").expect("two rules, comma separated");
        assert_eq!(run(&both, WriteKind::Watch, 2), vec![Some(28), None]);
        assert_eq!(run(&both, WriteKind::Reserve, 2), vec![Some(5), Some(5)]);
        let dir = tempfile::tempdir().unwrap();
        let switch = dir.path().join("full");
        let guarded = FaultPlan::parse(&format!("watch:28:while={}", switch.display())).expect("while=PATH");
        assert_eq!(errno_of(guarded.check(WriteKind::Watch)), None);
        std::fs::write(&switch, "").unwrap();
        assert_eq!(errno_of(guarded.check(WriteKind::Watch)), Some(28));
    }

    #[test]
    fn parse_refuses_a_malformed_plan_instead_of_injecting_nothing() {
        // why: a typo in VIGIL_TEST_FAULT that parsed as "no faults" would make
        //      every e2e disk-full test pass vacuously (fault never fires).
        //      Unknown kind, non-numeric errno, attempt 0 (1-based), a missing
        //      field, an inverted range, an unknown WHEN.
        // proves: CLAIM(DF-seam)
        for bad in ["bogus:28:all", "watch:x:all", "watch:28:0", "watch:28", "watch:28:2..1", "watch:28:sometimes", "watch:28:all,,lock:5:1"] {
            assert!(FaultPlan::parse(bad).is_err(), "{bad:?} must be refused");
        }
    }
}
