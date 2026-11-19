//! Write fault injection for the store (docs/specs/disk-full.md, "Test seam", and
//! "Amendments after spec review round 1", S4).
//!
//! TEST-ONLY. The plan is read from `VIGIL_TEST_FAULT`, which assumes a trusted
//! process environment: whoever can set it can make vigil's writes fail (or,
//! if the value is malformed, stop vigil from starting). Never export it on a
//! real host.
//!
//! Why a runtime plan and not `cfg(test)`: the end-to-end suites spawn the
//! built `vigil` binary, and a `cfg(test)` or cargo-feature hook is invisible
//! to it. A feature would need `required-features` on the e2e targets, which
//! makes a plain `cargo test` skip them silently. So the plan travels in an
//! environment variable ([`ENV_VAR`]) that `main.rs` reads at the edge (like
//! `XDG_STATE_HOME`); unset, the plan is [`FaultPlan::none`] and no code path
//! differs from production. Accepted risk: someone exports the variable on a
//! real host. It is named `..._TEST_...` and an unparseable value is a hard
//! error, never silently ignored (S6: documented as test-only).
//!
//! # Contract of the seam
//!
//! One WRITE ATTEMPT, or refused lock attempt for `lock-wait`, is one call
//! of [`FaultPlan::begin`]; the store then
//! passes through the stages of that attempt and asks [`Attempt::at`] at each
//! one. Attempts are counted once, at `begin`, whatever number of stages are
//! asked afterwards, so the retry after a disk-full failure is attempt 2 and
//! `Nth(2)` means "the retry". `Err` from `at` is the injected failure: an
//! `io::Error` built with `io::Error::from_raw_os_error(errno)`.
//!
//! Attempts and their stages, in the order the store passes through them
//! (S4: the rename is the COMMIT POINT; before it the old file is intact):
//!
//! * [`WriteKind::Watch`], one `write_atomic` of a watch file (`begin`'s
//!   target is the watch file, not the `.tmp`):
//!   1. [`Stage::Create`]: instead of `File::create(<path>.tmp)`. Nothing
//!      exists yet. This is also how inode exhaustion (ENOSPC on create) is
//!      injected.
//!   2. [`Stage::Write`]: after the `.tmp` exists, before any byte is written.
//!   3. [`Stage::ShortWrite`]: after the `.tmp` exists. The store writes the
//!      first half of the bytes (`len / 2`, so a strict, non-empty prefix; the
//!      JSON of a watch is far longer than 1 byte), then fails.
//!   4. [`Stage::Sync`]: all bytes written, instead of `sync_all`. The `.tmp`
//!      holds the full content.
//!   5. [`Stage::Rename`]: after the sync, instead of `rename`. The `.tmp`
//!      holds the full content; the old file is still the watch.
//!   6. [`Stage::Cleanup`]: asked only AFTER a failure at stage 2-5, instead
//!      of removing the `.tmp`. A failure here leaves the `.tmp` behind
//!      (best effort: the write's own error is still the one reported).
//! * [`WriteKind::Lock`]: only when a lock file has to be CREATED (opening an
//!   existing one needs no disk space and is not an attempt). Stage
//!   [`Stage::Create`] only. `begin`'s target is the lock file.
//! * [`WriteKind::CreditState`]: one `write_atomic` of `<root>/credit-state.json`
//!   (docs/specs/auto-interval.md S9, N4). The same six stages as a watch file.
//!   `begin`'s target is `credit-state.json`.
//! * [`WriteKind::Reserve`]: the best-effort creation of `<root>/.reserve`.
//!   Stages [`Stage::Create`] (before the file is created) and
//!   [`Stage::ShortWrite`] (the first half of the zeros is written, then the
//!   failure; the store must not leave a reserve that looks valid).
//!   `begin`'s target is `<root>/.reserve`.
//!
//! Rules are matched per kind AND stage; `Nth` / `Range` count attempts of
//! that kind only, 1-based, shared by every clone of the plan. A rule with a
//! file filter ([`FaultRule::only`]) matches only attempts whose target's file
//! NAME equals the filter, and its `Nth` / `Range` count only those attempts
//! (so "the first write of `b.json`" does not depend on how many other
//! watches were written first). [`FaultPlan::attempts`] counts every attempt
//! of the kind, filtered or not.
//!
//! # Time taken by an injected failure (review S5)
//!
//! A real write that fails can take long (a stalled disk). Under a paused test
//! clock a synchronous write takes no virtual time, so the budget arithmetic
//! of a call (`engine.rs`: the cutoff of a rescue's wait) could not be tested
//! against a slow write. [`FaultRule::taking`] makes the injected failure
//! arrive "after" some time: the failing attempt reports it through
//! [`Attempt::time_taken`], and the store lets that much time pass on the
//! call's clock (an `.await`: virtual under a paused tokio clock, a thread
//! sleep in the blocking entry points) right after the first attempt fails and
//! before it decides about a rescue. Only the FIRST attempt of a save is
//! delayed, and only when it fails; a plan without `taking` (and every plan
//! parsed from the environment: there is no syntax for it) delays nothing, so
//! production behaviour is unchanged.
//!
//! # Spec syntax of [`FaultPlan::parse`] (the environment variable)
//!
//! Rules separated by `,`, each `KIND[@STAGE]:ERRNO:WHEN[:FILE]`.
//!
//! * `KIND`: `watch`, `lock`, `reserve`, `credit`, `lock-wait`.
//! * `STAGE`: `create`, `write`, `short`, `sync`, `rename`, `cleanup`,
//!   `contended` (only `lock-wait`), and
//!   only the combinations listed above (`watch@` all six, `lock@create`,
//!   `reserve@create`, `reserve@short`). Omitted: the kind's default stage,
//!   [`WriteKind::default_stage`] (`watch` and `credit` -> `write`, `lock` and
//!   `reserve` -> `create`, `lock-wait` -> `contended`), which is what the pre-stage syntax meant. A name outside the
//!   list (also a different case, an empty one, or a stage the kind does not
//!   have) is an `Err`, never a rule that can never fire.
//! * `ERRNO`: a decimal errno.
//! * `WHEN`: `all`, `N` (only the N-th attempt), `N..M` (attempts N through
//!   M), `N..` (attempt N and every later one), `while=PATH` (every attempt
//!   while PATH exists: a file the test creates or removes models a disk that
//!   is full or has room, across process boundaries). N is at least 1.
//! * `FILE`: optional, a plain file name (no `/`, not empty).
//!
//! Paths in `while=` and file names must not contain `:` or `,`.
//!
//! # Holding a stage (docs/specs/auto-interval.md N1)
//!
//! A rule may HOLD a stage instead of failing it: in the `ERRNO` position write
//! `hold=PATH`. When an attempt the rule selects reaches the stage, it creates
//! `PATH.waiting` (so a test knows the process is parked there) and blocks the
//! calling thread until `PATH` exists (at most [`HOLD_LIMIT`]), then goes on as
//! if nothing happened. A test uses it to freeze a process at a precise point
//! (for example between a hook's side effect and the commit of
//! `credit-state.json`) and look at the world from outside. Same trust model as
//! the failures above: test-only, trusted environment.
//!
//! A refused per-watch or credit-lock attempt uses the same plan, counters,
//! file filter and barrier as writes. It begins only after try_lock reports WouldBlock. Its only
//! stage is `contended`; the target is the lock path. For example,
//! `lock-wait:hold=/tmp/add-ready:1:h.lock` parks the first contended attempt
//! on h.lock and announces it with /tmp/add-ready.waiting. This records a
//! durable synchronization point instead of sampling transient /proc fds.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The environment variable a spawned `vigil` reads its [`FaultPlan`] from.
pub const ENV_VAR: &str = "VIGIL_TEST_FAULT";

/// The longest a [`FaultRule::holding`] rule parks a stage (a test that never
/// releases it must not hang the process for ever).
pub const HOLD_LIMIT: Duration = Duration::from_secs(60);

/// What a store operation is for; a rule selects by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WriteKind {
    /// A watch file (`write_atomic`: `.tmp`, sync, rename).
    Watch,
    /// Creation of a lock file.
    Lock,
    /// Creation of `<root>/.reserve`.
    Reserve,
    /// `<root>/credit-state.json` (docs/specs/auto-interval.md S9).
    CreditState,
    /// An unsuccessful lock acquisition, with the lock path as target.
    LockWait,
}

impl WriteKind {
    /// The stage a rule without an explicit `@STAGE` targets (the pre-stage
    /// meaning of each kind).
    pub fn default_stage(self) -> Stage {
        match self {
            WriteKind::Watch | WriteKind::CreditState => Stage::Write,
            WriteKind::Lock | WriteKind::Reserve => Stage::Create,
            WriteKind::LockWait => Stage::Contended,
        }
    }

    /// The stages the store asks about for this kind, in the order it passes
    /// through them. A rule at any other stage could never fire, so [`parse`]
    /// refuses it. CLAIM(S4): the lists are the module docs above [spec S4].
    ///
    /// [`parse`]: FaultPlan::parse
    pub fn stages(self) -> &'static [Stage] {
        match self {
            WriteKind::Watch | WriteKind::CreditState => {
                &[Stage::Create, Stage::Write, Stage::ShortWrite, Stage::Sync, Stage::Rename, Stage::Cleanup]
            }
            WriteKind::Lock => &[Stage::Create],
            WriteKind::Reserve => &[Stage::Create, Stage::ShortWrite],
            WriteKind::LockWait => &[Stage::Contended],
        }
    }

    fn from_name(name: &str) -> Option<WriteKind> {
        match name {
            "watch" => Some(WriteKind::Watch),
            "lock" => Some(WriteKind::Lock),
            "reserve" => Some(WriteKind::Reserve),
            "credit" => Some(WriteKind::CreditState),
            "lock-wait" => Some(WriteKind::LockWait),
            _ => None,
        }
    }
}

/// A step of one write attempt (see the module docs for the order and for
/// which kind has which stages).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    /// Instead of creating the file (nothing exists yet). Inode exhaustion.
    Create,
    /// After the `.tmp` exists, before any byte is written.
    Write,
    /// After the `.tmp` exists: half the bytes are written, then the failure.
    ShortWrite,
    /// Instead of `sync_all`; the `.tmp` is complete.
    Sync,
    /// Instead of `rename`; the `.tmp` is complete, the old file still rules.
    Rename,
    /// Instead of removing the `.tmp` after a failed attempt.
    Cleanup,
    /// After try_lock refused a lock held by another owner.
    Contended,
}

impl Stage {
    fn from_name(name: &str) -> Option<Stage> {
        match name {
            "create" => Some(Stage::Create),
            "write" => Some(Stage::Write),
            "short" => Some(Stage::ShortWrite),
            "sync" => Some(Stage::Sync),
            "rename" => Some(Stage::Rename),
            "cleanup" => Some(Stage::Cleanup),
            "contended" => Some(Stage::Contended),
            _ => None,
        }
    }
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

impl When {
    /// Does the rule fail attempt number `ordinal` (1-based, counted among the
    /// attempts the rule matches)? `WhileExists` is asked at the stage, not at
    /// `begin`, so a switch flipped between two stages of one attempt counts.
    fn fails(&self, ordinal: u64) -> bool {
        match self {
            When::Every => true,
            When::Nth(n) => ordinal == *n,
            When::Range { first, last } => ordinal >= *first && last.is_none_or(|last| ordinal <= last),
            When::WhileExists(switch) => switch.exists(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaultRule {
    pub kind: WriteKind,
    /// The step of the attempt that fails.
    pub stage: Stage,
    /// Raw OS error number: 28 (ENOSPC), 122 (EDQUOT), 5 (EIO), 13 (EACCES)...
    pub errno: i32,
    pub when: When,
    /// Only attempts whose target file NAME is this; `None`: every attempt.
    pub file: Option<String>,
    /// How long the injected failure takes to arrive (review S5, see the module
    /// docs). Zero for every rule parsed from the environment.
    pub took: Duration,
    /// N1: when set, the rule HOLDS the stage until this path exists instead of
    /// failing it (see the module docs); `errno` is then unused.
    pub hold_until: Option<PathBuf>,
}

impl FaultRule {
    /// A rule at the kind's default stage ([`WriteKind::default_stage`]) for
    /// every file.
    pub fn new(kind: WriteKind, errno: i32, when: When) -> FaultRule {
        FaultRule { kind, stage: kind.default_stage(), errno, when, file: None, took: Duration::ZERO, hold_until: None }
    }
    /// A rule that parks `stage` of the attempts it selects until `until`
    /// exists, then lets them go on (N1). Writes `until.waiting` on arrival.
    pub fn holding(kind: WriteKind, stage: Stage, until: PathBuf, when: When) -> FaultRule {
        FaultRule { kind, stage, errno: 0, when, file: None, took: Duration::ZERO, hold_until: Some(until) }
    }
    /// The same rule at another stage.
    pub fn at(mut self, stage: Stage) -> FaultRule {
        self.stage = stage;
        self
    }
    /// The same rule, its failure arriving only after `took` (review S5).
    pub fn taking(mut self, took: Duration) -> FaultRule {
        self.took = took;
        self
    }
    /// The same rule restricted to attempts whose target file name is `file`.
    pub fn only(mut self, file: &str) -> FaultRule {
        self.file = Some(file.to_string());
        self
    }

    /// Does this rule look at an attempt of `kind` on `target`?
    fn matches(&self, kind: WriteKind, target: &Path) -> bool {
        self.kind == kind && self.file.as_deref().is_none_or(|wanted| target.file_name().is_some_and(|name| name == wanted))
    }
}

#[derive(Debug, Default)]
struct FaultState {
    rules: Vec<FaultRule>,
    /// Attempts begun per kind (every file): the number `attempts` reports.
    attempts: HashMap<WriteKind, u64>,
    /// Per rule (same index as `rules`): attempts it matched so far. `Nth` and
    /// `Range` count these, so a file filter counts only its own file.
    matched: Vec<u64>,
    injected: HashMap<(WriteKind, Stage), u64>,
    /// Attempts parked by a `holding` rule, per kind and stage (N1).
    held: HashMap<(WriteKind, Stage), u64>,
}

/// The faults to inject. Clones share rules and counters (the store is
/// `Clone`; "the 2nd write" must not depend on which clone wrote).
#[derive(Debug, Clone, Default)]
pub struct FaultPlan {
    shared: Arc<Mutex<FaultState>>,
}

/// One write attempt in flight: asks the plan, stage by stage, whether to
/// fail. Obtained from [`FaultPlan::begin`].
#[derive(Debug)]
pub struct Attempt {
    plan: FaultPlan,
    kind: WriteKind,
    /// `(rule index, this attempt's ordinal among the attempts the rule
    /// matches)`, fixed at `begin` so later attempts cannot shift it.
    candidates: Vec<(usize, u64)>,
    /// The `took` of the rule whose failure this attempt last injected.
    time_taken: std::cell::Cell<Duration>,
}

impl Attempt {
    /// `Err` is the failure injected at `stage` of this attempt.
    pub fn at(&self, stage: Stage) -> io::Result<()> {
        let mut state = self.plan.shared.lock().expect("fault plan mutex poisoned");
        let fired = self
            .candidates
            .iter()
            .map(|(index, ordinal)| (&state.rules[*index], *ordinal))
            .find(|(rule, ordinal)| rule.stage == stage && rule.when.fails(*ordinal))
            .map(|(rule, _)| (rule.errno, rule.took, rule.hold_until.clone()));
        match fired {
            None => Ok(()),
            Some((_, _, Some(until))) => {
                *state.held.entry((self.kind, stage)).or_default() += 1;
                drop(state);
                hold_until(&until);
                Ok(())
            }
            Some((errno, took, None)) => {
                *state.injected.entry((self.kind, stage)).or_default() += 1;
                self.time_taken.set(took);
                Err(io::Error::from_raw_os_error(errno))
            }
        }
    }

    /// How long the failure injected by this attempt took to arrive (review
    /// S5); zero when nothing was injected or the rule has no `taking`.
    pub fn time_taken(&self) -> Duration {
        self.time_taken.get()
    }
}

impl FaultPlan {
    /// Production: never fails anything.
    pub fn none() -> FaultPlan {
        FaultPlan::default()
    }

    pub fn from_rules(rules: Vec<FaultRule>) -> FaultPlan {
        let matched = vec![0; rules.len()];
        FaultPlan { shared: Arc::new(Mutex::new(FaultState { rules, matched, ..FaultState::default() })) }
    }

    /// Parse the [`ENV_VAR`] syntax (see the module docs). `Err` carries a
    /// message naming what is wrong. Nothing malformed is ever read as "no
    /// faults": a typo that injected nothing would turn a failure test into a
    /// vacuous pass.
    pub fn parse(spec: &str) -> Result<FaultPlan, String> {
        let rules = spec.split(',').map(parse_rule).collect::<Result<Vec<FaultRule>, String>>()?;
        Ok(FaultPlan::from_rules(rules))
    }

    /// The plan for the value of [`ENV_VAR`] (`None`: not set). Unset and the
    /// empty string both inject nothing, as an empty `XDG_STATE_HOME` is unset
    /// (`store::state_root`): `VIGIL_TEST_FAULT= vigil ...` and wrappers that
    /// export the variable empty must not stop vigil from starting. Any other
    /// value is parsed, and a malformed one is an error (see [`FaultPlan::parse`]).
    pub fn from_env_value(value: Option<&str>) -> Result<FaultPlan, String> {
        match value.filter(|spec| !spec.is_empty()) {
            Some(spec) => FaultPlan::parse(spec),
            None => Ok(FaultPlan::none()),
        }
    }

    /// Start one write attempt of `kind` on `target` and count it. The store
    /// calls this once per attempt, never once per stage.
    pub fn begin(&self, kind: WriteKind, target: &Path) -> Attempt {
        let mut state = self.shared.lock().expect("fault plan mutex poisoned");
        *state.attempts.entry(kind).or_default() += 1;
        let mut candidates = Vec::new();
        for index in 0..state.rules.len() {
            if state.rules[index].matches(kind, target) {
                state.matched[index] += 1;
                candidates.push((index, state.matched[index]));
            }
        }
        Attempt { plan: self.clone(), kind, candidates, time_taken: std::cell::Cell::new(Duration::ZERO) }
    }

    /// How many attempts of `kind` were begun so far (all clones, every file).
    pub fn attempts(&self, kind: WriteKind) -> u64 {
        self.shared.lock().expect("fault plan mutex poisoned").attempts.get(&kind).copied().unwrap_or(0)
    }

    /// How many attempts a `holding` rule has parked so far at `stage` of `kind`
    /// (all clones).
    pub fn held(&self, kind: WriteKind, stage: Stage) -> u64 {
        self.shared.lock().expect("fault plan mutex poisoned").held.get(&(kind, stage)).copied().unwrap_or(0)
    }

    /// How many failures were injected so far at `stage` of `kind` attempts
    /// (all clones). Lets a test show that its fault really fired where it
    /// meant to, instead of the write failing for some other reason.
    pub fn injected(&self, kind: WriteKind, stage: Stage) -> u64 {
        self.shared.lock().expect("fault plan mutex poisoned").injected.get(&(kind, stage)).copied().unwrap_or(0)
    }
}

/// Park the calling thread until `until` exists (N1). Leaves `until.waiting`
/// behind so the test that set the trap can see it was sprung.
fn hold_until(until: &Path) {
    let mut marker = until.as_os_str().to_owned();
    marker.push(".waiting");
    let _ = std::fs::write(PathBuf::from(marker), b"");
    let started = std::time::Instant::now();
    while !until.exists() && started.elapsed() < HOLD_LIMIT {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// `KIND[@STAGE]:ERRNO:WHEN[:FILE]`, where `ERRNO` may be `hold=PATH`.
fn parse_rule(text: &str) -> Result<FaultRule, String> {
    let fields: Vec<&str> = text.split(':').collect();
    if !(3..=4).contains(&fields.len()) {
        return Err(format!("rule {text:?} must be KIND[@STAGE]:ERRNO:WHEN[:FILE] ({} fields, expected 3 or 4)", fields.len()));
    }
    let (kind_name, stage_name) = match fields[0].split_once('@') {
        Some((kind, stage)) => (kind, Some(stage)),
        None => (fields[0], None),
    };
    let kind = WriteKind::from_name(kind_name).ok_or_else(|| format!("rule {text:?}: unknown kind {kind_name:?} (watch, lock, reserve, credit or lock-wait)"))?;
    let stage = match stage_name {
        None => kind.default_stage(),
        Some(name) => {
            let stage = Stage::from_name(name).ok_or_else(|| format!("rule {text:?}: unknown stage {name:?} (create, write, short, sync, rename, cleanup or contended)"))?;
            if !kind.stages().contains(&stage) {
                return Err(format!("rule {text:?}: a {kind_name} operation has no stage {name:?}, so the rule could never fire"));
            }
            stage
        }
    };
    let (errno, hold_until) = match fields[1].strip_prefix("hold=") {
        Some("") => return Err(format!("rule {text:?}: hold= needs a path")),
        Some(path) => (0, Some(PathBuf::from(path))),
        None => {
            let errno: i32 = fields[1].parse().ok().filter(|errno| *errno > 0).ok_or_else(|| format!("rule {text:?}: errno {:?} must be a positive decimal number (or hold=PATH)", fields[1]))?;
            (errno, None)
        }
    };
    let when = parse_when(fields[2]).map_err(|problem| format!("rule {text:?}: {problem}"))?;
    let file = match fields.get(3) {
        None => None,
        Some(name) if name.is_empty() || name.contains('/') => {
            return Err(format!("rule {text:?}: the FILE field {name:?} must be a plain, non-empty file name"));
        }
        Some(name) => Some((*name).to_string()),
    };
    Ok(FaultRule { kind, stage, errno, when, file, took: Duration::ZERO, hold_until })
}

fn parse_when(text: &str) -> Result<When, String> {
    let attempt_number = |digits: &str| -> Result<u64, String> {
        digits.parse::<u64>().ok().filter(|n| *n >= 1).ok_or_else(|| format!("{digits:?} is not an attempt number (1 or more)"))
    };
    if text == "all" {
        return Ok(When::Every);
    }
    if let Some(path) = text.strip_prefix("while=") {
        return if path.is_empty() { Err("while= needs a path".to_string()) } else { Ok(When::WhileExists(PathBuf::from(path))) };
    }
    match text.split_once("..") {
        None => Ok(When::Nth(attempt_number(text)?)),
        Some((first, "")) => Ok(When::Range { first: attempt_number(first)?, last: None }),
        Some((first, last)) => {
            let (first, last) = (attempt_number(first)?, attempt_number(last)?);
            if last < first {
                return Err(format!("the range {text:?} ends before it starts"));
            }
            Ok(When::Range { first, last: Some(last) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These pin the SEAM itself. Every other disk-full test injects its fault
    // through it; if the seam fired at the wrong attempt or stage, those tests
    // would prove nothing (or fail for the wrong reason). Co-located with the
    // seam.

    #[test]
    fn an_unset_or_empty_environment_value_injects_nothing_and_a_malformed_one_is_refused() {
        // why: review T8: `VIGIL_TEST_FAULT=""` is "unset", like an empty
        //      XDG_STATE_HOME; only the empty string and `None` mean "no
        //      faults". The control rows keep it from becoming "ignore every
        //      value": a malformed one and a real plan still behave as before.
        // proves: CLAIM(DF-seam)
        for value in [None, Some("")] {
            let plan = FaultPlan::from_env_value(value).unwrap_or_else(|error| panic!("{value:?} must be accepted: {error}"));
            assert_eq!(run(&plan, WriteKind::Watch, 2), [None, None], "{value:?} injects nothing");
        }
        assert!(FaultPlan::from_env_value(Some("this is not a fault plan")).is_err(), "control: malformed is still an error");
        let plan = FaultPlan::from_env_value(Some("watch:28:all")).unwrap();
        assert_eq!(run(&plan, WriteKind::Watch, 2), [Some(28), Some(28)], "control: a real plan injects");
    }

    #[test]
    fn lock_wait_rules_filter_the_lock_path_and_count_only_contention() {
        // why: CLI race tests must park at real contention on the selected
        // lock, without changing creation-fault counters or a different lock.
        // catches: sharing the creation counter, ignoring the path filter,
        // or accepting a stage which the lock-wait checkpoint never reaches.
        let plan = FaultPlan::parse("lock-wait:5:2:h.lock").unwrap();
        let target = Path::new("/state/locks/h.lock");
        assert!(plan.begin(WriteKind::Lock, target).at(Stage::Create).is_ok());
        assert!(plan.begin(WriteKind::LockWait, Path::new("/state/credit.lock")).at(Stage::Contended).is_ok());
        assert!(plan.begin(WriteKind::LockWait, target).at(Stage::Contended).is_ok());
        assert_eq!(errno_of(plan.begin(WriteKind::LockWait, target).at(Stage::Contended)), Some(5));
        assert_eq!(plan.attempts(WriteKind::Lock), 1);
        assert!(FaultPlan::parse("lock-wait@rename:5:1").is_err());
        assert!(FaultPlan::parse("watch@contended:5:1").is_err());
    }

    const ALL_STAGES: [Stage; 6] = [Stage::Create, Stage::Write, Stage::ShortWrite, Stage::Sync, Stage::Rename, Stage::Cleanup];

    fn errno_of(result: io::Result<()>) -> Option<i32> {
        result.err().map(|error| error.raw_os_error().expect("an injected error carries a raw os error"))
    }

    /// The raw errno of `count` consecutive `kind` attempts, each asked at
    /// `stage` of `target`.
    fn run_at(plan: &FaultPlan, kind: WriteKind, stage: Stage, target: &str, count: usize) -> Vec<Option<i32>> {
        (0..count).map(|_| errno_of(plan.begin(kind, Path::new(target)).at(stage))).collect()
    }

    /// `run_at` at the kind's default stage on an arbitrary file.
    fn run(plan: &FaultPlan, kind: WriteKind, count: usize) -> Vec<Option<i32>> {
        run_at(plan, kind, kind.default_stage(), "/state/watches/w.json", count)
    }

    fn one(kind: WriteKind, errno: i32, when: When) -> FaultPlan {
        FaultPlan::from_rules(vec![FaultRule::new(kind, errno, when)])
    }

    #[test]
    fn a_failure_reports_the_time_it_took_and_only_that_failure_does() {
        // why: review S5 needs a slow failing write under a paused clock (see
        //      the module docs). The seam must report the time of the failure
        //      it injected, only for that attempt, and none for an attempt that
        //      does not fail or a rule without `taking`: otherwise the engine
        //      test would wait on the wrong attempt or on every write.
        // proves: CLAIM(DF-seam), CLAIM(S5-time)
        let slow = Duration::from_millis(950);
        let plan = FaultPlan::from_rules(vec![
            FaultRule::new(WriteKind::Watch, 28, When::Nth(1)).taking(slow),
            FaultRule::new(WriteKind::Watch, 5, When::Nth(2)),
        ]);
        let first = plan.begin(WriteKind::Watch, Path::new("/s/w.json"));
        assert_eq!(first.time_taken(), Duration::ZERO, "nothing failed yet");
        assert_eq!(errno_of(first.at(Stage::Create)), None);
        assert_eq!(first.time_taken(), Duration::ZERO, "a stage that does not fail takes no time");
        assert_eq!(errno_of(first.at(Stage::Write)), Some(28));
        assert_eq!(first.time_taken(), slow);
        let second = plan.begin(WriteKind::Watch, Path::new("/s/w.json"));
        assert_eq!(errno_of(second.at(Stage::Write)), Some(5));
        assert_eq!(second.time_taken(), Duration::ZERO, "a rule without `taking` fails at once");
        assert_eq!(FaultPlan::parse("watch:28:all").unwrap().begin(WriteKind::Watch, Path::new("/s/w.json")).time_taken(), Duration::ZERO, "the environment syntax cannot delay");
    }

    #[test]
    fn an_empty_plan_never_fails_anything() {
        // why: control for every other test here: production runs with
        //      `FaultPlan::none()`, which must inject nothing, whatever the
        //      kind and the stage.
        // proves: CLAIM(DF-seam-inert)
        let plan = FaultPlan::none();
        for kind in [WriteKind::Watch, WriteKind::Lock, WriteKind::Reserve, WriteKind::CreditState] {
            for stage in ALL_STAGES {
                assert_eq!(run_at(&plan, kind, stage, "/state/x", 2), vec![None, None], "{kind:?} {stage:?}");
            }
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
        assert_eq!(run(&plan, WriteKind::Watch, 1), vec![None], "path absent: the disk has room");
        std::fs::write(&switch, "").unwrap();
        assert_eq!(run(&plan, WriteKind::Watch, 1), vec![Some(28)], "path present: the disk is full");
        std::fs::remove_file(&switch).unwrap();
        assert_eq!(run(&plan, WriteKind::Watch, 1), vec![None], "path removed: room again");
    }

    #[test]
    fn clones_of_a_plan_share_one_attempt_counter() {
        // why: `Store` is `Clone` and the engine hands copies around; "fail the
        //      2nd write" must mean the 2nd write of the process, not of
        //      whichever clone happened to make it.
        // proves: CLAIM(DF-seam)
        let plan = one(WriteKind::Watch, 28, When::Nth(2));
        let clone = plan.clone();
        assert_eq!(run(&plan, WriteKind::Watch, 1), vec![None], "attempt 1 via the original");
        assert_eq!(run(&clone, WriteKind::Watch, 1), vec![Some(28)], "attempt 2 via the clone");
        assert_eq!(plan.attempts(WriteKind::Watch), 2);
        assert_eq!(clone.attempts(WriteKind::Watch), 2);
    }

    #[test]
    fn a_rule_fires_only_at_its_stage_and_asking_more_stages_does_not_count_more_attempts() {
        // why: S4: "The seam injects at each stage of write_atomic". Two
        //      things must hold for the stage tests elsewhere to mean
        //      anything. (1) Selectivity: a rule at `Rename` fails the rename
        //      and nothing earlier (a rule that failed `Create` instead would
        //      leave no `.tmp` and pass a rename test for the wrong reason),
        //      and a rule built without a stage sits at the kind's default
        //      (Watch -> Write, Lock and Reserve -> Create). (2) Counting: one
        //      `begin` is one attempt however many stages it is asked at, so
        //      `Nth(2)` is still "the retry" (6 stages asked, 2 attempts).
        //      Expected rows written out by hand.
        // proves: CLAIM(DF-seam), CLAIM(S4)
        for stage in ALL_STAGES {
            let plan = FaultPlan::from_rules(vec![FaultRule::new(WriteKind::Watch, 28, When::Every).at(stage)]);
            let attempt = plan.begin(WriteKind::Watch, Path::new("/state/watches/w.json"));
            let failing: Vec<Stage> = ALL_STAGES.into_iter().filter(|asked| attempt.at(*asked).is_err()).collect();
            assert_eq!(failing, vec![stage], "a Watch rule at {stage:?} must fail that stage only");
            assert_eq!(plan.attempts(WriteKind::Watch), 1, "six stages asked, one attempt");
            assert_eq!(plan.injected(WriteKind::Watch, stage), 1, "{stage:?}: one injected failure, at the named stage");
        }
        let second_attempt_only = FaultPlan::from_rules(vec![FaultRule::new(WriteKind::Watch, 5, When::Nth(2)).at(Stage::Rename)]);
        let first = second_attempt_only.begin(WriteKind::Watch, Path::new("/s/w.json"));
        for stage in ALL_STAGES {
            assert!(first.at(stage).is_ok(), "attempt 1, {stage:?}");
        }
        let second = second_attempt_only.begin(WriteKind::Watch, Path::new("/s/w.json"));
        assert_eq!(errno_of(second.at(Stage::Sync)), None, "attempt 2 passes the sync");
        assert_eq!(errno_of(second.at(Stage::Rename)), Some(5), "attempt 2 fails the rename");

        assert_eq!(FaultRule::new(WriteKind::Watch, 28, When::Every).stage, Stage::Write);
        assert_eq!(FaultRule::new(WriteKind::Lock, 28, When::Every).stage, Stage::Create);
        assert_eq!(FaultRule::new(WriteKind::Reserve, 28, When::Every).stage, Stage::Create);
        assert_eq!(FaultRule::new(WriteKind::Watch, 28, When::Every).at(Stage::ShortWrite).stage, Stage::ShortWrite);
    }

    #[test]
    fn a_file_filter_matches_by_file_name_and_counts_only_the_matching_attempts() {
        // why: the multi-watch tests (S5) and the race test (S3) need "the
        //      write of b.json fails" without depending on the order several
        //      watches are written in, which concurrent ticks do not fix.
        //      Attempts, in order: a, a, b, a, b. `Nth(1)` filtered to b.json
        //      is the THIRD attempt (the first b), not the first; `Nth(2)` is
        //      the fifth. The unfiltered counter still reads 5. A filter that
        //      names a file nobody writes injects nothing (negative control).
        // proves: CLAIM(DF-seam)
        let targets = ["/s/watches/a.json", "/s/watches/a.json", "/s/watches/b.json", "/s/watches/a.json", "/s/watches/b.json"];
        let ask = |plan: &FaultPlan| -> Vec<Option<i32>> {
            targets.iter().map(|target| errno_of(plan.begin(WriteKind::Watch, Path::new(target)).at(Stage::Write))).collect()
        };
        let first_b = FaultPlan::from_rules(vec![FaultRule::new(WriteKind::Watch, 28, When::Nth(1)).only("b.json")]);
        assert_eq!(ask(&first_b), vec![None, None, Some(28), None, None]);
        assert_eq!(first_b.attempts(WriteKind::Watch), 5, "the kind's counter counts every file");
        let second_b = FaultPlan::from_rules(vec![FaultRule::new(WriteKind::Watch, 28, When::Nth(2)).only("b.json")]);
        assert_eq!(ask(&second_b), vec![None, None, None, None, Some(28)]);
        let every_a = FaultPlan::from_rules(vec![FaultRule::new(WriteKind::Watch, 5, When::Every).only("a.json")]);
        assert_eq!(ask(&every_a), vec![Some(5), Some(5), None, Some(5), None]);
        let nobody = FaultPlan::from_rules(vec![FaultRule::new(WriteKind::Watch, 5, When::Every).only("c.json")]);
        assert_eq!(ask(&nobody), vec![None; 5]);
        let by_name_not_path = FaultPlan::from_rules(vec![FaultRule::new(WriteKind::Watch, 5, When::Every).only("b.json")]);
        assert_eq!(errno_of(by_name_not_path.begin(WriteKind::Watch, Path::new("/elsewhere/b.json")).at(Stage::Write)), Some(5), "the name, not the directory");
        assert_eq!(errno_of(by_name_not_path.begin(WriteKind::Watch, Path::new("/s/watches/ab.json")).at(Stage::Write)), None, "the whole name, not a suffix");
    }

    #[test]
    fn parse_builds_the_plan_the_documented_syntax_names() {
        // why: the spawned binary receives its plan as text (VIGIL_TEST_FAULT).
        //      Each row is a spec, the kind and stage it is asked at, and the
        //      errno of each of the first attempts, written out by hand from
        //      the grammar in the module docs. Rows without `@STAGE` pin the
        //      defaults (the pre-stage meaning); rows with one pin every stage
        //      name of every kind; the last two pin the FILE field.
        // proves: CLAIM(DF-seam), CLAIM(S4)
        let cases: [(&str, WriteKind, Stage, Vec<Option<i32>>); 16] = [
            ("watch:28:all", WriteKind::Watch, Stage::Write, vec![Some(28), Some(28)]),
            ("watch:5:2", WriteKind::Watch, Stage::Write, vec![None, Some(5), None]),
            ("watch:122:1..2", WriteKind::Watch, Stage::Write, vec![Some(122), Some(122), None]),
            ("watch:28:3..", WriteKind::Watch, Stage::Write, vec![None, None, Some(28), Some(28)]),
            ("lock:13:1", WriteKind::Lock, Stage::Create, vec![Some(13), None]),
            ("reserve:28:all", WriteKind::Reserve, Stage::Create, vec![Some(28), Some(28)]),
            ("watch@create:28:1", WriteKind::Watch, Stage::Create, vec![Some(28), None]),
            ("watch@write:28:1", WriteKind::Watch, Stage::Write, vec![Some(28), None]),
            ("watch@short:28:1", WriteKind::Watch, Stage::ShortWrite, vec![Some(28), None]),
            ("watch@sync:5:2", WriteKind::Watch, Stage::Sync, vec![None, Some(5)]),
            ("watch@rename:122:all", WriteKind::Watch, Stage::Rename, vec![Some(122), Some(122)]),
            ("watch@cleanup:5:all", WriteKind::Watch, Stage::Cleanup, vec![Some(5), Some(5)]),
            ("lock@create:28:all", WriteKind::Lock, Stage::Create, vec![Some(28), Some(28)]),
            ("reserve@create:28:2", WriteKind::Reserve, Stage::Create, vec![None, Some(28)]),
            ("reserve@short:28:1", WriteKind::Reserve, Stage::ShortWrite, vec![Some(28), None]),
            ("watch@rename:28:1..1", WriteKind::Watch, Stage::Rename, vec![Some(28), None]),
        ];
        for (spec, kind, stage, expected) in cases {
            let plan = FaultPlan::parse(spec).unwrap_or_else(|error| panic!("{spec:?} should parse: {error}"));
            assert_eq!(run_at(&plan, kind, stage, "/s/w.json", expected.len()), expected, "{spec:?}");
            let other_stage = ALL_STAGES.into_iter().find(|candidate| *candidate != stage).unwrap();
            let fresh = FaultPlan::parse(spec).unwrap();
            assert_eq!(run_at(&fresh, kind, other_stage, "/s/w.json", 1), vec![None], "{spec:?} must not fire at {other_stage:?}");
        }
        let both = FaultPlan::parse("watch:28:1,reserve@short:5:all").expect("two rules, comma separated");
        assert_eq!(run(&both, WriteKind::Watch, 2), vec![Some(28), None]);
        assert_eq!(run_at(&both, WriteKind::Reserve, Stage::ShortWrite, "/s/.reserve", 2), vec![Some(5), Some(5)]);
        let dir = tempfile::tempdir().unwrap();
        let switch = dir.path().join("full");
        let guarded = FaultPlan::parse(&format!("watch@rename:28:while={}", switch.display())).expect("while=PATH with a stage");
        assert_eq!(run_at(&guarded, WriteKind::Watch, Stage::Rename, "/s/w.json", 1), vec![None]);
        std::fs::write(&switch, "").unwrap();
        assert_eq!(run_at(&guarded, WriteKind::Watch, Stage::Rename, "/s/w.json", 1), vec![Some(28)]);
        let filtered = FaultPlan::parse("watch@sync:5:1:b.json").expect("a FILE field");
        assert_eq!(errno_of(filtered.begin(WriteKind::Watch, Path::new("/s/watches/a.json")).at(Stage::Sync)), None, "a.json is not b.json");
        assert_eq!(errno_of(filtered.begin(WriteKind::Watch, Path::new("/s/watches/b.json")).at(Stage::Sync)), Some(5), "the first b.json write");
        let filtered_default_stage = FaultPlan::parse("lock:28:all:w.lock").expect("a FILE field after the default stage");
        assert_eq!(errno_of(filtered_default_stage.begin(WriteKind::Lock, Path::new("/s/locks/w.lock")).at(Stage::Create)), Some(28));
    }

    #[test]
    fn parse_refuses_a_malformed_plan_instead_of_injecting_nothing() {
        // why: a typo in VIGIL_TEST_FAULT that parsed as "no faults" would make
        //      every e2e disk-full test pass vacuously (fault never fires).
        //      Unknown kind, non-numeric errno, attempt 0 (1-based), a missing
        //      field, an inverted range, an unknown WHEN. Each row below is
        //      refused.
        // proves: CLAIM(DF-seam)
        for bad in ["bogus:28:all", "watch:x:all", "watch:28:0", "watch:28", "watch:28:2..1", "watch:28:sometimes", "watch:28:all,,lock:5:1"] {
            assert!(FaultPlan::parse(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn parse_refuses_a_malformed_stage_loudly_and_names_it() {
        // why: S4 + the same reasoning as above, one level down: `watch@renmae`
        //      that parsed as "never fires" would turn every stage test into a
        //      vacuous pass. Refused: an unknown name, a different case, an
        //      empty stage, a missing kind, two stages, a field count that is
        //      wrong, an empty or path-like FILE. And the error must say what
        //      is wrong: it contains the offending stage name, so the person
        //      who set the variable can fix it.
        // proves: CLAIM(DF-seam), CLAIM(S4)
        for bad in [
            "watch@renmae:28:all",
            "watch@RENAME:28:all",
            "watch@Rename:28:all",
            "watch@:28:all",
            "@rename:28:all",
            "watch@rename@sync:28:all",
            "watch@rename:28",
            "watch@rename:28:all:b.json:extra",
            "watch:28:all:",
            "watch:28:all:dir/b.json",
            "watch@rename:28:all,watch@renmae:28:all",
        ] {
            assert!(FaultPlan::parse(bad).is_err(), "{bad:?} must be refused");
        }
        let error = FaultPlan::parse("watch@renmae:28:all").unwrap_err();
        assert!(error.contains("renmae"), "the message should name the bad stage: {error:?}");
    }

    #[test]
    fn parse_accepts_exactly_the_stages_each_kind_has() {
        // why: S4 lists the stages per kind: a watch file has all six, a lock
        //      file is only ever created, the reserve is created or half
        //      written. A rule at a stage the store never asks for (a
        //      `lock@sync`, a `reserve@rename`) could never fire, so it is a
        //      typo and is refused like one. The 18 kind x stage pairs are
        //      listed by hand (9 accepted, 9 refused) rather than derived from
        //      the code's own table.
        // proves: CLAIM(DF-seam), CLAIM(S4)
        let accepted = [
            "watch@create", "watch@write", "watch@short", "watch@sync", "watch@rename", "watch@cleanup",
            "lock@create",
            "reserve@create", "reserve@short",
        ];
        let refused = [
            "lock@write", "lock@short", "lock@sync", "lock@rename", "lock@cleanup",
            "reserve@write", "reserve@sync", "reserve@rename", "reserve@cleanup",
        ];
        for head in accepted {
            let spec = format!("{head}:28:all");
            assert!(FaultPlan::parse(&spec).is_ok(), "{spec:?} must be accepted");
        }
        for head in refused {
            let spec = format!("{head}:28:all");
            assert!(FaultPlan::parse(&spec).is_err(), "{spec:?} must be refused: the store never asks for that stage");
        }
        assert_eq!(accepted.len() + refused.len(), 18, "3 kinds x 6 stages");
    }

    #[test]
    fn the_credit_kind_has_the_six_watch_stages_and_a_hold_rule_parses() {
        // why: docs/specs/auto-interval.md N1/N4 inject at the `credit-state.json`
        //      write. Every one of the six stages of `write_atomic` must be
        //      addressable (`credit@STAGE`), the bare kind targets the default
        //      `write` stage like a watch file, and `hold=PATH` takes the
        //      place of the errno. A hold with no path, and a credit rule at a
        //      stage the store never asks for, are typos and are refused.
        // proves: CLAIM(DF-seam), CLAIM(N1), CLAIM(N4)
        for stage in ["create", "write", "short", "sync", "rename", "cleanup"] {
            assert!(FaultPlan::parse(&format!("credit@{stage}:5:1")).is_ok(), "credit@{stage}");
            assert!(FaultPlan::parse(&format!("credit@{stage}:hold=/tmp/go:all")).is_ok(), "credit@{stage} hold");
        }
        assert!(FaultPlan::parse("credit:5:all").is_ok());
        for bad in ["credit@nope:5:1", "credit:hold=:all", "credit:hold:all", "credit:0:all"] {
            assert!(FaultPlan::parse(bad).is_err(), "{bad:?} must be refused");
        }
        let plan = FaultPlan::parse("credit:5:2").unwrap();
        let outcomes: Vec<_> = (0..3).map(|_| errno_of(plan.begin(WriteKind::CreditState, Path::new("/s/credit-state.json")).at(Stage::Write))).collect();
        assert_eq!(outcomes, [None, Some(5), None], "attempt numbers count credit writes only");
        assert_eq!(errno_of(plan.begin(WriteKind::Watch, Path::new("/s/w.json")).at(Stage::Write)), None, "a credit rule leaves watch writes alone");
    }

    #[test]
    fn a_hold_rule_parks_the_stage_until_the_file_exists_and_says_it_is_parked() {
        // why: N1 needs a process frozen at an exact point (after the hook,
        //      before the commit of credit-state.json) so a test can look at
        //      the lock from outside. Proven here on the seam itself: the
        //      attempt must (1) not return while the release file is missing,
        //      (2) leave `<release>.waiting` the moment it parks, (3) return
        //      Ok (it does not fail the write) once the file exists, and
        //      (4) park only the stage and attempts the rule selects: another
        //      stage of the same attempt passes at once.
        // proves: CLAIM(N1)
        let dir = tempfile::tempdir().unwrap();
        let release = dir.path().join("go");
        let spec = format!("credit@rename:hold={}:all", release.display());
        let plan = FaultPlan::parse(&spec).unwrap();
        let attempt = plan.begin(WriteKind::CreditState, Path::new("/s/credit-state.json"));
        assert_eq!(errno_of(attempt.at(Stage::Sync)), None);
        assert_eq!(plan.held(WriteKind::CreditState, Stage::Rename), 0, "other stages are not parked");
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = attempt.at(Stage::Rename);
            sender.send(result.is_ok()).unwrap();
        });
        let waiting = dir.path().join("go.waiting");
        let started = std::time::Instant::now();
        while !waiting.exists() && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(waiting.exists(), "the parked attempt must announce itself");
        assert!(receiver.recv_timeout(Duration::from_millis(200)).is_err(), "it must not pass before the release file exists");
        std::fs::write(&release, "").unwrap();
        assert_eq!(receiver.recv_timeout(Duration::from_secs(5)), Ok(true), "released, the write goes on");
        worker.join().unwrap();
        assert_eq!(plan.held(WriteKind::CreditState, Stage::Rename), 1);
        assert_eq!(plan.injected(WriteKind::CreditState, Stage::Rename), 0, "a hold is not an injected failure");
    }
}
