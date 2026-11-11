# vigil disk-full: green phase report

STATUS: done. Every test in the suite passes, including the 8 added for the final
defaults. Two things are not as clean as they could be and are said plainly in
"Not satisfied / caveats": `cargo clippy -- -D warnings` and `cargo fmt --check`
fail on code this work did not touch.

Spec: `specs/disk-full.md` (DF1-DF7 and every amendment; the last sections win).
Red history: `specs/disk-full-red-report.md`. Branch `feat/vigil-disk-full`.
Crate: ``. Also changed: `agent-integration/skills/vigil/SKILL.md`.

## Commits

| SHA | Message |
|---|---|
| `79804224` | vigil: red tests for final disk-full defaults (DF4d, DF4e, N3, S1) |
| `f0d694b7` | vigil: make the write-fault seam real (parse, staged injection, counters) |
| `294f8d00` | vigil: answer from memory when the disk is full (reserve, persist note, redelivery) |
| `65515693` | vigil: CLI names the state root on stderr when a save failed (DF2) |
| `a3871321` | vigil: document surviving a full disk (SPEC state section, skill) |

A report cannot hold the SHA of the commit that contains it, so the commit of this
file is the final one (the SHA is in the last line of the hand-off message).

## Phase A: red edits for the final defaults

Against the stub (`cargo test`, before any behaviour): lib 241 passed, 67 failed.
66 of those are disk-full tests including the 8 new ones; the 67th is the known
flaky `engine::tests::listing_prunes_too`. Every new test fails on an assertion or
a precondition ("a save leaves a reserve", "no `persist` field"), none hangs, none
fails to compile.

| Final default | Tests | What they pin |
|---|---|---|
| DF4d (N1) | `store`: `a_reserve_is_published_only_after_the_store_wide_lock_file_exists`; the lock-entry test, rewritten for `lock`/`try_lock`. `disk_full_tests`: `without_a_store_wide_lock_file_that_can_be_made_there_is_no_rescue_and_the_reserve_is_left_alone` | no reserve before the lock file exists; a missing store-wide lock file that cannot be made allows no rescue: `disk_full`, reserve untouched. The test that expected a rescue for `lock`/`try_lock` is replaced; the per-watch entries keep the rescue leg (their store-wide lock file exists). Engine twin has a control where the same fault IS rescued once the lock file can be made. |
| DF4e (N2) | `store`: `a_rescue_waits_..._at_most_the_call_budget_and_then_gives_up_untouched` (budget 0, 200 ms, and the 2 s ceiling), `a_lock_released_within_the_budget_lets_the_rescue_through` (positive control). `disk_full_tests`: `a_rescue_blocked_by_a_held_store_wide_lock_gives_up_inside_the_call_budget_with_the_envelope` | wait is `min(2 s, budget left)`; held beyond it: envelope, reserve intact (same inode), ONE attempt. The engine test has a world with 0.5 s of budget left (a 29.5 s virtual read out of a 30 s call) so it fails if the engine does not pass its budget. Adds the stub `Store::save_within`. |
| N3 | `store`: `the_error_a_failed_write_returns_is_the_writes_own_never_the_cleanups`; the cleanup-leftover test now fails the cleanup with EACCES while the write is EIO | different errnos for write and cleanup; the returned error is the write's, both directions (EIO write + ENOSPC cleanup; ENOSPC write + EIO cleanup), at four stages |
| S1 qualifier | `disk_full_tests`: `a_glob_redelivers_its_current_matches_and_never_a_match_that_has_vanished`, `a_file_redelivers_every_append_since_the_saved_baseline_up_to_the_64_kib_capture_cap` | glob is transient: both-stay gives "a | b", a-vanishes gives only "b" (and a appears nowhere), all-vanish is a quiet saved `pending`. File: 20000+20000 bytes redelivered whole; 40000+40000 keeps the newest 65536 bytes with `[truncated]` and loses FIRST's tag. Oracle: the stored `last_digest`, not cut by the 8 KiB envelope limit. |

Note on one test with a range: the `lock`/`try_lock` rescue leg asserts 1 or 2 lock
creation attempts (`(1..=2).contains(..)`). The spec does not say whether the rescue's
own try to create the store-wide lock file counts as a second attempt; my
implementation makes it (2). The range is the honest reading of the spec, and the
reserve/lock-file assertions next to it are exact.

## Phase B: what was built, and the design choices

### Seam (`src/fault.rs`)

Real matching. `begin` counts one attempt per call and fixes each matching rule's
ordinal at that moment (a file-filtered rule counts only its own file), so later
attempts cannot shift it; `Attempt::at` fires the first rule at that stage whose
`When` holds, evaluating `while=PATH` at the stage. `parse` refuses a rule with an
unknown kind or stage, a stage the kind never has (`lock@sync`), errno 0, attempt 0,
an inverted range, a path-like or empty FILE, a wrong field count, or an empty rule;
messages name the offending piece. `WriteKind::stages()` is the table of what each
kind has; the module docs state that `VIGIL_TEST_FAULT` is test-only and assumes a
trusted environment (S6). `main.rs` already read the variable at the edge and
already failed loudly on a bad plan; unset it changes nothing.

### Store (`src/store.rs`, `src/error.rs`)

* **`write_staged`**: create, write, short write, sync, rename (the commit point),
  cleanup. A failed attempt removes only its own `.tmp`, best effort, and returns the
  WRITE's error (N3). A leftover `.tmp` is truncated and consumed by that watch's next
  write; nothing sweeps other `*.tmp` files.
* **One rescue path** (`attempt_with_rescue`) for watch files, lock files and the
  reserve upkeep: run the attempt; on ENOSPC/EDQUOT with a reserve file present, take
  the store-wide lock, re-check the reserve is still there (a racing rescue may have
  spent it: then no retry, DF4a), delete it, retry once, and re-make the reserve on
  success (A5). A failed retry returns the retry's error plus `first_error`.
* **Lock access as a type**: `StoreLockAccess::{Held(&StoreLock), Wait(Duration)}`.
  `Store::save_under(watch, &StoreLock)` is what `create` calls for its placeholder, so
  it reuses the lock it holds and never waits for itself; `save_within(watch, budget)`
  waits `min(2 s, budget)`; `save` waits 2 s.
* **`StoreLock` (store-wide) and `WatchLock` are now different types**, so
  `save_under` cannot be handed a watch lock.
* **DF4d**: `with_store_lock` makes the lock file first (through the seam, with no
  rescue of its own); if it cannot, nothing runs: no rescue, no reserve. Taking
  `lock`/`try_lock` for a missing store-wide lock file that cannot be made therefore
  reports `disk_full` with the reserve untouched.
* **DF4b**: valid reserve = regular file (not a symlink), exactly 262144 bytes,
  `st_blocks * 512 >= 262144`. Replaced when invalid, written in place with
  `create_new` (never through a symlink), real zeros, `sync_all`, half written then the
  `ShortWrite` stage, and removed again if anything fails. Never kept open.
* **Errors**: `StoreError::Io` and `DiskFull` keep their names (tests match them) and
  now carry an `IoFailure { os_error, write: Option<WriteContext { path, state_root,
  first_error }> }`. The message names the state root and says `disk full`
  (lowercase) for ENOSPC/EDQUOT.

### Engine, keepalive, tick, envelope

* A failed write is a value, not an error: `Persisted::{Saved, SavedOnReserve,
  NotSaved(PersistFailure)}` (`engine.rs`). `Vigil::persist` does the save, passes
  the call budget to `save_within`, and keeps the process's memory of which watches
  failed to save (`Session::unsaved`, for S1).
* `TickReport` (now `pub(crate)`) carries the watch as the tick left it, saved or
  not, plus `persisted` and `redelivered`. `sense` accumulates a `CallRecord`
  (changes, risky saves, redelivered ids) and ends the call right after a round with
  an unsaved tick (DF1a). `keepalive_envelope` answers from the in-memory copy for such
  a watch, flags events of redelivering watches, does NOT mark an unsaved terminal
  result returned (DF3), and builds `persist`/`persist_failures`.
* `PersistNote` is an enum (`ReserveSpent`, `NotSaved { failure, redelivery }`) behind
  a private flat wire struct (`#[serde(into, try_from)]`), so the Rust type cannot hold
  `ok: true` together with an `error`. `redelivery` is computed once, from the
  envelope's content: events present, or a terminal status (DF3a).
* `cancel` returns an envelope with `persist` (DF5a); `create` saves its placeholder
  under the held lock and removes it when the baseline cannot be saved (DF7); `wait`
  sleeps one `interval_s` after a pending envelope that could not be saved (DF1a).
* `Event` has `redelivered` (skipped when false, never stored true). `Envelope` has
  `persist` and `persist_failures` (both absent on a healthy call).
* CLI: one stderr line per failed save naming the state root (`keepalive`, `wait`,
  `cancel`); exit codes unchanged.

Decisions I took where the spec leaves room:

* If a covered watch other than the primary spent the reserve and the primary has no
  note of its own, the envelope's `persist` is `ReserveSpent`: the agent should learn
  the disk is full whichever watch noticed. A primary's own note always wins.
* `watch_create` returns no `persist` note when its save used the reserve (the
  `Created` shape has no such field and the spec is silent). Only a failure is
  reported, as an error.
* `wait` prints the stderr line for the final envelope only; intermediate pending
  envelopes whose save failed are silent on stderr.
* A failed `create_dir_all(watches/)` is inside the retried closure, so a full disk
  that fails the directory is rescued like the file.

## Verification

All cargo runs via qex, foreground. Full output of each is under the session
scratchpad (`run*.log`, `final*.log`).

### The three final full runs (`cargo test -j2 --no-fail-fast`, tree = `a3871321`)

| Run | `--lib` | `src/main.rs` | `cli_e2e` | `mcp_e2e` | doc-tests | Failed tests |
|---|---|---|---|---|---|---|
| 1 | 308 passed, 0 failed | 0 | 40 passed, 0 failed | 14 passed, 0 failed | 0 | none |
| 2 | 307 passed, 1 failed | 0 | 40 passed, 0 failed | 14 passed, 0 failed | 0 | `engine::tests::listing_prunes_too` |
| 3 | 308 passed, 0 failed | 0 | 40 passed, 0 failed | 14 passed, 0 failed | 0 | none |

362 tests in the suite (308 + 40 + 14), up from 354 after red round 3 (8 added in
Phase A). The first full run after the green code (before the docs commit) was also
308/40/14, all passing.

### Flaky tests

* `engine::tests::listing_prunes_too` fails sometimes (1 of the 3 final runs). It
  also failed on the Phase A stub run, before any behaviour existed, so it predates
  this work. It also failed in one mutated run together with
  `engine::tests::prune_also_removes_abandoned_pending_watches_and_their_lock_files_but_never_the_global_lock`.
  The red reports (rounds 2 and 3) already list both and give a cause I did not
  re-verify: `prune` only try-locks a watch's lock file, and a child forked by a
  concurrent test and not yet exec'd still holds the descriptor, so the candidate is
  skipped. Not touched.
* `inputs::tests::port_without_a_want_reports_no_want_status_and_want_closed_inverts`
  (flaky per the red report) did not fail in any run of this session.
* `keepalive::tests::a_blocked_call_returns_early_...` (flaky per round 1) did not
  fail in any run of this session.

### clippy and rustfmt

Both are in `TOOLS` (`cargo-clippy`, `cargo-fmt`/`rustfmt`).

* `cargo clippy --all-targets -j2 -- -D warnings`: **fails** (qex reports exit 97 for a failed job), 6 errors in the lib
  (and again in the lib test), all in files and lines this work did not touch:
  `testing.rs` (3 x `new_without_default`/`field_reassign_with_default`,
  `collapsible_if`, and the `read` extern signature lint
  `suspicious_runtime_symbol_definitions`) and `watch.rs:93` (`derivable_impls`). The
  same command also stops the `mcp_e2e` target at `tests/mcp_e2e.rs:252`
  (`type_complexity`), a line from an earlier commit. With exactly those seven lints
  allowed (`-A clippy::new_without_default -A clippy::field_reassign_with_default -A
  clippy::collapsible_if -A clippy::derivable_impls -A
  suspicious_runtime_symbol_definitions -A clippy::type_complexity`) clippy over all
  targets exits **0**: nothing from this work is flagged. I did not fix the old
  lints: they are in test infrastructure and an unrelated impl.
* `cargo fmt --check`: **fails** (exit 1), 911 hunks across 19 files. The crate is not
  rustfmt-formatted at the baseline: untouched files (`clock.rs`, `config.rs`,
  `judge.rs`) fail too. I matched the surrounding style and did not reformat the
  crate, which would bury this change in an unrelated diff.

### Mutation checks (each reverted; nothing committed)

Applied in disjoint batches and attributed by which tests went red.

| Mutation | Killed by |
|---|---|
| proceed without a store-wide lock file that could not be made (DF4d) | `without_a_store_wide_lock_file_...`, `a_reserve_is_published_only_after_...`, the lock-entry test |
| ignore the call budget, always wait 2 s (DF4e) | the store budget test and the engine DF4e test |
| the cleanup's error replaces the write's (N3) | `the_error_a_failed_write_returns_is_the_writes_own_...` and the cleanup-leftover test (the old test, with equal errnos, would not have caught it; this is why it changed) |
| a retry that does not delete the reserve first | 10 store/engine tests (all `WhileExists(reserve)` rescues) |
| reserve validity ignores `st_blocks` | `an_invalid_reserve_is_replaced_by_the_next_successful_write` (the sparse rows) |
| `create` calls `save` instead of `save_under` (self-wait) | `a_create_that_fits_once_the_reserve_is_freed_succeeds` |
| drop the re-check of the reserve under the lock (DF4c/DF4a) | `two_rescues_racing_on_one_root_spend_the_reserve_exactly_once` |
| unsaved results counted as returned (DF3) | `a_no_id_keepalive_...`, `a_cancel_saved_by_another_process_wins_...`, `one_terminal_result_saved_and_another_unsaved_...` |
| `wait` does not sleep (DF1a) | `wait_sleeps_one_interval_...` |
| no stop after an unsaved tick (DF1a) | watchdog failures across the tick tests |
| events not flagged `redelivered` (S1) | the flag tests (`every_event_...`, the glob and file tests, the DF3 tests) |
| create keeps its placeholder (DF7) | `a_create_whose_baseline_save_fails_removes_its_placeholder_...` |

Not mutated: the S1 file/glob tests have no code of their own to break (they pin
existing reader behaviour through the new redelivery path); their sharpness is that
they need `persist`/`redelivered` to exist at all.

### A real ENOSPC, not only the seam

`unshare -Urm` works on this box, so I mounted a 1 MiB tmpfs in a private mount
namespace and ran the built binary against it (scripts in the session scratchpad,
nothing touched the host's filesystems; the tmpfs vanished with the namespace):

* healthy `create` left `lock`, `locks/`, `watches/` and a 262144-byte `.reserve`;
* disk filled to 0 free, input changed, `keepalive`: exit 0, the full fired envelope,
  `persist = {"ok": true, "used_reserve": true, "kind": "disk_full"}`, no stderr
  line, and a new valid reserve appeared again (the replaced watch file freed exactly
  the page the new reserve needed);
* reserve removed first, disk full, `keepalive`: exit 0, full fired envelope,
  `persist = {ok: false, kind: disk_full, error: "No space left on device (os error
  28)", path: <watch file>, redelivery: true}`, ONE stderr line naming the state
  root, the watch file byte-identical, no `.tmp`;
* the same disk, `create`: exit 1, `error: disk full: cannot write .../w4.json (state
  root ...): No space left on device (os error 28)`, no `w4.json` left behind;
* space freed, a fresh process: the same event (`w3:1:0`, "boom2") again, persisted,
  no `persist` field, empty stderr, no `redelivered` flag (a fresh process cannot
  know, as decided), the reserve back.

## Not satisfied / caveats

Nothing in the spec was left unimplemented and no test was weakened. What is not
perfect:

1. **`cargo clippy -- -D warnings` and `cargo fmt --check` do not pass**, for the
   old reasons above. They are the only red gates.
2. **No real quota or inode-exhaustion drill** (EDQUOT, ENOSPC on create from a full
   inode table): only the seam covers those. The ENOSPC drill above is on tmpfs.
3. **A filesystem that compresses or deduplicates zeros** reports fewer blocks for
   the reserve than its length, so it is never "valid" (DF4b, S2) and every healthy
   write finds it invalid and re-writes 256 KiB with an fsync. Correct by the spec's
   definition, but a cost on such filesystems. The skill and SPEC say the rescue can
   fail there; they do not mention the repeated rewrite.
4. **The rescue wait blocks a thread.** `Store` is synchronous, so a rescue that
   waits for the store-wide lock sleeps its runtime thread for up to `min(2 s,
   budget)`. In practice the lock is held for milliseconds by another process;
   the cost is bounded and only paid on a full disk. The DF4e engine test therefore
   takes about 2.5 s of real time.
5. **`lib.rs` still has the old "RED phase: every public item below is a stub"
   header** and `#![allow(dead_code, unused_variables)]` from before this work;
   I left them.
6. **Parent-directory fsync after the rename is not done** (the old code did not
   either); not in the spec.
7. The flaky `listing_prunes_too` family (above) is pre-existing and unfixed.

## Review round 1 fixes (Sol 6/10, bar 9)

Three real bugs, each with a red test committed before its fix. Work order S1, S2, S3.
All cargo runs via qex.

### S1: a failed save did not end the round (`keepalive.rs`)

* Red test: `disk_full_tests::a_quiet_failed_save_ends_the_call_without_waiting_for_a_watch_whose_lock_is_held`,
  plus `a_failed_save_ends_the_call_at_once_but_keeps_the_saves_that_were_ready_with_it`
  (red for the same reason, and its file assertions fail a fix that drops the rest of the pass).
  Commit `59d0f090`.
* How it failed before the fix: watch "a" saves with EIO, watch "b"'s lock is held; the envelope
  came back with `waited_s` 30 (the whole budget) instead of 0.
* Fix: the `for_each_completed` callback in `sense` returns "stop" for any absorbed report whose
  call record has an unsaved save (it was only `Finished` or a finished `Ticked`). Completions already
  ready in the same pass are still collected by the existing stop logic (C12). Commit `eecf368e`.

### S2: redelivery provenance lost (`engine.rs`, `keepalive.rs`)

* Red tests (commit `896f715`; both failed on the literal `redelivered: true` missing from the event):
  * `disk_full_tests::a_changed_event_another_process_saved_under_a_lost_event_id_comes_back_flagged_redelivered`:
    MCP process P cannot save "boom"; a concurrent CLI process saves "boom2" under the same event id
    `w:1:0`; P's next call replayed it with no flag.
  * `disk_full_tests::a_retry_that_saved_keeps_its_redelivered_flag_until_the_envelope_is_really_delivered`:
    P's retry saves, its response is dropped before `commit_delivery` (`keepalive_uncommitted`); `watch_status`
    and the next call then replayed the stored event with no flag, because the first successful save had cleared
    the memory.
* Fix (commit `2713e32`): `Session::unsaved` became `Session::repeat_risk`, "this process may be repeating events
  of this watch". It is set by a failed save, survives a successful save while the saved result is finished and
  has events (the delivery is not committed yet), and is cleared only by `commit_delivery`. The one place that
  hands stored events out, `Vigil::replayed_events`, flags them while the watch is in the set; `envelope_for`
  (so `watch_status` and `cancel`) and `keepalive_envelope` both use it. The in-memory copy of an unsaved result
  is still the first delivery and stays unflagged unless its own tick followed a failed save.
  A deadlock I introduced on the way (the session lock taken twice in one struct literal) hung the first run; fixed
  before the commit.
* Guard (commit `c77541f`, passes before and after the S2 fix, killed by a mutation that never clears the set):
  `the_redelivery_memory_is_released_when_its_envelope_is_delivered`. Decision to flag: after the commit the
  replay of the same stored result is unflagged, as the brief says ("until delivery commits").

### S3: lock waits blocked the executor (`store.rs`, `engine.rs`, `tick.rs`, `main.rs`)

* Red tests (commit `d916f1c`, real time, single-threaded runtime):
  * `disk_full_tests::rescue_waits_of_different_watches_overlap_and_never_block_the_executor`: three watches whose
    rescues all wait for a held store-wide lock took 6.00 s (3 x 2 s) instead of about 2 s, and a heartbeat task
    got no CPU. Bounds: 1.9 s <= call < 4 s, at least 30 heartbeats (about 200 expected).
  * `disk_full_tests::a_cancel_during_a_rescue_wait_is_observed_at_once`: a cancel at 200 ms was only seen after the
    whole 2 s wait (the call returned `Some(envelope)` instead of `None`).
* Fix (commit `a6e69c2`): the waiting is written once, in `store.rs`, generic over `Patience`: `Blocking` (thread
  sleep, for the CLI edge and the store tests, run by `block_on_ready` because each of its awaits is ready in its
  first poll) and `Cooperative` (`tokio::time::sleep` on tokio's clock). The write, the retry and the reserve
  work stay synchronous under the lock: there is no `.await` between taking the store-wide lock and the end of
  `run`, so a dropped future never leaves a rescue half done. New async entry points: `save_async`,
  `save_within_async`, `try_lock_async`, `try_lock_watch_async`, `prune_async` (and `Vigil::prune_async`, used by
  `list` and by `main`). `Vigil::persist`, the engine's two lock-polling loops and `create`'s baseline save use
  them. Every waiting site in the store goes through it: rescue, reserve upkeep, lock-file creation (a rescue in
  disguise) and prune's short waits. The blocking `lock()`/`lock_watch()` keep their blocking `flock` (they are
  sync-only API).
* Two things the async wait forced: (1) a rescue inside a call stops `RESCUE_BUDGET_MARGIN` (100 ms) before the call
  budget, because the call's own `timeout_at` can now fire while the wait runs and would drop the tick's answer
  (DF1) if the wait ended at the same instant; `a_rescue_with_almost_no_budget_left_still_returns_the_answer`
  pins it (mutation: margin 0 turns it red). (2) Upkeep waits `min(limit, 250 ms)`, never longer than the
  caller's limit.
* One existing test changed, and why: `a_rescue_blocked_by_a_held_store_wide_lock_gives_up_inside_the_call_budget_with_the_envelope`
  asserted `>= 1.9 s of REAL time on a blocked thread`, which is the bug. It now measures the wait on the call's
  clock (virtual under the paused test clock): world 1 is `1.9 s <= wait < 2.5 s` (the upper bound is new), world 2
  is `29.8 s <= call < 30 s` (29.5 s read + 0.4 s wait); its other assertions are unchanged. It takes 0.1 s instead
  of 2.5 s.
* Store-level async twins (added with the fix, they could not compile before it):
  `async_rescues_racing_on_one_root_spend_the_reserve_exactly_once` (40 rounds, multi-thread runtime),
  `an_async_rescue_waits_without_parking_its_thread_and_gets_through_when_the_lock_is_released`,
  `dropping_an_async_rescue_during_its_wait_leaves_the_reserve_and_the_file_untouched`.

### Commits of this round

| SHA | Message |
|---|---|
| `59d0f090` | vigil: red test for review S1 |
| `eecf368e` | vigil: a failed save ends the round as soon as it is absorbed (review S1) |
| `896f715` | vigil: red test for review S2 |
| `2713e32` | vigil: keep redelivery provenance in the process until delivery commits (review S2) |
| `d916f1c` | vigil: red test for review S3 |
| `a6e69c2` | vigil: waits for a contended store lock yield to the executor (review S3) |
| `c77541f` | vigil: pin that delivery releases the redelivery memory (review S2 guard) |
| `2e3ea80` | vigil: the S3 overlap test accepts either watch's failure as the reported one |

The report's own commit follows (the SHA is in the last line of the hand-off message).

### Verification of this round (qex, foreground, `cargo test -j2 --no-fail-fast`)

Code tree = `2e3ea80` (the report commit changes only this file). Three clean full runs:

| Run | `--lib` | `src/main.rs` | `cli_e2e` | `mcp_e2e` | doc-tests | Failed |
|---|---|---|---|---|---|---|
| 1 | 319 passed | 0 | 40 passed | 14 passed | 0 | none |
| 2 | 319 passed | 0 | 40 passed | 14 passed | 0 | none |
| 3 | 319 passed | 0 | 40 passed | 14 passed | 0 | none |

373 tests (319 + 40 + 14), up from 362: 11 added in `--lib` (S1 2, S2 3, S3 6: 3 engine + 3 store).
Before these three clean runs, the full run just before `2e3ea80` had one flake, in a test of this round:
`rescue_waits_of_different_watches_overlap_...` asserted that the failure shows as `persist`, but the three
rescues give up microseconds apart and S1 ends the round at the first one absorbed, so it can instead sit in
`persist_failures`. The test was over-specified, not the code; fixed in `2e3ea80` (time and heartbeat assertions
unchanged). The old `engine::tests::listing_prunes_too` flake did not appear in any of the 8 runs of this session.

`cargo clippy --all-targets --no-deps -- -D warnings` shows the same seven old lints (`testing.rs`, `watch.rs`) and
nothing in code from this round (one new `manual_async_fn` in `store.rs` was fixed). `cargo fmt --check` was not
re-run; the crate is still not rustfmt-formatted at the baseline.

### Not done / caveats of this round

* No MCP end-to-end test for S1: the smallest real call is 30 s, and the paused-clock engine test reproduces the
  same code path (the held lock, the `waited_s`, the persist note) exactly.
* S2's decision: replays after the commit are unflagged (see the guard). If the owner wants a replay lost on the
  wire to stay flagged, drop the `remove` in `commit_delivery` (and the guard).
* S3: a tick dropped by the budget during the post-write reserve upkeep wait (at most 250 ms and never longer
  than the margin-trimmed limit) has saved its watch but loses its in-call report; the next call sees the saved
  file, and the memory of a failed save is kept (conservative flagging), so nothing is lost.
* The new real-time tests (S3 overlap about 2 s, cancel about 0.2 s, the store heartbeat test about 0.15 s)
  use generous bounds (4 s against 2 s, 1 s against 0.2 s, 30 against about 200 beats); they were stable in
  all 8 runs apart from the over-specified assertion above.

## Review round 2 fixes (Sol 4/10, bar 9)

STATUS: done. Two findings,
both caused by the async rescue wait added for S3. Work order S4, S5. Cargo runs via
qex, foreground. Deviation from the brief: `--mem 1.1GB` instead of `1.7GB`, because
other sessions outside the queue left the machine 2.2 GB of usable memory and a
1.7 GB claim (plus qex's 1 GB reserve) waited for 19 minutes with no job running; the
measured use of the test run is about 0.8 GB.

### S4: a dropped rescue wait took the sensed answer with it (`store.rs`, `engine.rs`, `tick.rs`, `keepalive.rs`)

* Red tests, commit `711bc6a`. All three fail on the answer that is missing:
  * `disk_full_tests::a_rescue_waiting_for_the_store_lock_keeps_its_answer_when_another_watch_ends_the_round`:
    "a" ENOSPC with the store-wide lock held, "b" EIO at once. Before the fix the envelope was b's
    (`watch_id` b, one event `b:1:0`, `persist` io); a's event and failure were gone.
  * `disk_full_tests::a_rescue_still_waiting_when_another_rescue_gives_up_keeps_its_answer`:
    both ENOSPC, b starts its wait 0.5 s later; a gives up at 2 s and the round drops b. Before the fix
    `events` held only `a:1:0`.
  * `disk_full_tests::a_tick_that_saved_keeps_its_report_when_the_reserve_upkeep_after_its_save_is_dropped`:
    the same class on the success side (a judge watch saved, its reserve upkeep waits for the lock, b's EIO
    ends the round). Before the fix `digest` was empty instead of naming the change `bad`.
* Fix, commit `b28802e`. The instances of the class are the awaits that follow a tick's sensing:
  (1) the rescue's wait for the store-wide lock after the first write failed, (2) the reserve upkeep's wait
  after a write that succeeded. Both are covered by one mechanism, not by one patch each:
  * `store.rs`: `Settled::{FirstAttemptFailed(&StoreError), Written}` is reported by `save_with` to an
    observer BEFORE the wait (the first failure is reported right after the first attempt, before
    `attempt_with_rescue` can await; `Written` right before upkeep waits, and only if upkeep has work).
    Nothing the store does after the report changes the file other than the rescue's delete+retry and the
    reserve fill, which still run under the store-wide lock with no `.await` between taking it and finishing
    (CLAIM(DF4c)): a drop therefore leaves the watch file, the `.tmp` and the reserve exactly as the report says
    (rename stays the commit point; the reserve is deleted only right before the retry, under the held lock).
    `StoreError::write_failed` now borrows the `io::Error`.
  * `engine.rs`: `Vigil::persist_observed` maps the report to a `Persisted` (`NotSaved(failure)` with `kind`
    from the FIRST error, or `Saved`) for a caller's `settled` callback. The process's failed-save memory
    (`remember_save`) is applied at the end of a save and by whoever collects a dropped save; a report alone does
    not touch it, so a rescue that then succeeds does not flag its events as repeats.
  * `tick.rs`: `Retained`, one slot per tick of a round. The tick keeps its `TickReport` (outcome, changed,
    the watch as it is in memory, `Persisted`) in it at each report and clears it when the save returns, so a
    completed tick is never collected twice.
  * `keepalive.rs`: the round owns the slots; after `for_each_completed` it takes in the report of every tick
    that was dropped inside its save (S1's early stop, or the budget's `timeout_at`), through the same
    `take_in` the completed ticks use (progress, `changed`, `saves`). A dropped tick whose save was reported
    `NotSaved` therefore reaches the envelope as `persist` / `persist_failures`, with its events, and the
    call ends as S1 says.
  * The two red tests counted every watch write since the create (6) where they meant the call's (2); that
    assertion was not reached before the fix. Corrected to a difference in the same commit.
* Mutation checks (each reverted): the first-failure report not made: both rescue tests red; no collection of
  retained reports: all three red; no `Written` report: the upkeep test red; slots never cleared: 8 tests red
  (a completed tick was taken in twice).

### S5: the rescue deadline was captured before the write (`engine.rs`, `store.rs`, `fault.rs`)

* Red test, commit `a9a2ab1`: `disk_full_tests::a_slow_failing_write_near_the_deadline_gets_no_rescue_wait_and_the_call_ends_inside_its_budget`.
  A 30 s call whose read takes 29 s, a first write that fails with ENOSPC after 0.95 s, the store-wide lock held. It
  failed on `the rescue must not wait once its cutoff has passed; the call took 30s` (the stale 0.9 s wait ran into the
  outer timeout: `waited_s` 30; the S4 retention kept the answer, which is why the assertion is on the call's end). The
  same test holds a control (lock free, same slow write): the rescue still gets the lock on its one try, spends the
  reserve, `used_reserve`, attempt 2. Its preconditions pin that the read and the slow write really ran (`>= 29.9 s`).
* The seam had no delay stage, so the red commit adds a minimal one (test-only semantics, documented in the `fault.rs`
  module docs): `FaultRule::taking(duration)`. The failing attempt reports it (`Attempt::time_taken`); the store lets that
  time pass on the call's clock right after the FIRST attempt fails and before it decides about a rescue (`Patience::elapse`:
  a `tokio::time::sleep`, virtual under the paused test clock; a thread sleep in the blocking entry points). It applies to
  the first attempt of a save and only when it fails; every plan without `taking`, and every plan parsed from
  `VIGIL_TEST_FAULT` (no syntax for it), delays nothing and awaits nothing. Seam test:
  `fault::tests::a_failure_reports_the_time_it_took_and_only_that_failure_does`.
* Fix, commit `d9c13fe`. `Vigil::persist_observed` no longer turns the deadline into a duration: it hands the store an
  absolute `tokio::time::Instant` cutoff (call deadline less `RESCUE_BUDGET_MARGIN`, 100 ms). The store's `LockWait
  { ceiling, cutoff }` computes `allowed_now()` at the moment a wait is about to start: in `with_store_lock`, after the
  store-wide lock file has been opened, so it covers both the rescue and the reserve upkeep (which caps the ceiling at
  250 ms and keeps the cutoff). A passed cutoff means no waiting, not no rescue: the lock is still tried once, so a free
  lock is taken. The blocking entry points (`save`, `save_within`) keep a plain duration from the start of their wait
  (`cutoff: None`); nothing in production uses them inside a call budget.
* Store-level tests, same commit (new API, so they could not exist before the fix):
  `a_cutoff_that_has_passed_forbids_waiting_for_the_lock_but_not_taking_a_free_one`,
  `the_time_left_to_the_cutoff_is_read_when_the_wait_starts_not_when_the_save_began` (write 300 ms, cutoff 500 ms:
  the wait ends at 500 ms, not 800 ms), `reserve_upkeep_waits_no_longer_than_the_cutoff_allows` (100 ms, then 0),
  and, for the mated S4 interface, `a_save_reports_what_it_settled_before_each_wait_it_makes` (the report precedes the
  wait and survives a drop 100 ms into it; reserve inode and file unchanged; nothing is reported on a healthy save).
* Mutation checks (each reverted): the cutoff ignored in `allowed_now`: 6 tests red (the S5 test, three store tests,
  the two existing near-budget tests); the upkeep's `capped` dropping the cutoff: the upkeep test red; the wait started
  from the ceiling alone (the old arithmetic): 6 red; margin 0 in the engine: the two near-budget tests and the S5 test
  red.

### Commits of this round

| SHA | Message |
|---|---|
| `711bc6a` | vigil: red test for review S4 |
| `b28802e` | vigil: a dropped rescue or upkeep wait keeps the tick's answer (review S4) |
| `a9a2ab1` | vigil: red test for review S5 |
| `d9c13fe` | vigil: a rescue or upkeep wait is bounded by an absolute cutoff read when it starts (review S5) |

The report's own commit follows (the SHA is in the last line of the hand-off message).

### Verification of this round

Code tree = `d9c13fe` (the report commit changes only this file). `cargo test -j2 --no-fail-fast`, qex, foreground:

| Run | `--lib` | `src/main.rs` | `cli_e2e` | `mcp_e2e` | doc-tests | Failed |
|---|---|---|---|---|---|---|
| 1 | 328 passed | 0 | 40 passed | 14 passed | 0 | none |
| 2 | 328 passed | 0 | 40 passed | 14 passed | 0 | none |
| 3 | 328 passed | 0 | 40 passed | 14 passed | 0 | none |

382 tests (328 + 40 + 14), up from 373: 9 added in `--lib` (S4 3, S5 1 engine + 1 seam + 4 store). No existing test
was changed except the attempt counts in the two S4 red tests (see S4). `cargo clippy --all-targets --no-deps -- -D warnings`
with the same seven old lints allowed as last round exits 0: nothing new from this round. `cargo fmt --check` not re-run
(the crate is still not rustfmt-formatted at the baseline).

Flakes seen in this session, none in the three final runs and none in code touched by this round: the port-input tests
`inputs::tests::port_reports_open_and_closed_and_whether_the_want_is_met` (once, in the S5 red run) and
`inputs::tests::port_without_a_want_reports_no_want_status_and_want_closed_inverts` (twice, in mutated runs M1 and M6, which
do not touch inputs). The second is the one already listed as flaky in the red reports; both bind and probe local ports
on a machine that other sessions share. `engine::tests::listing_prunes_too` did not fail in any run of this round.

### Decisions and caveats of this round

* qex claim `--mem 1.1GB` instead of `1.7GB` (see the top of this section); the runs used up to 1 GB.
* A tick dropped inside its save is reported as the store left it: `NotSaved(first error)` with `redelivery: true` for a
  dropped rescue, `Saved` for a dropped upkeep. For a rescue that would have SUCCEEDED had it been allowed to continue,
  the envelope therefore says "not saved" and the next call redelivers: the conservative side of DF3 (nothing is lost, an
  event may come twice, flagged `redelivered`).
* The collected tick also sends its progress notification and counts in `session_ticks` of the call, like a completed one.
* A cancelled call (`keepalive_cancellable`'s cancel future) still drops everything and returns nothing (C2): the retained
  reports are collected by the round, not by the cancel path, by design.
* `Settled::Written` is reported only when the upkeep has something to make, so a healthy save with a valid reserve builds
  no provisional report.
* No MCP end-to-end test for S4/S5 (the smallest real call is 30 s); the paused-clock engine tests drive the same code
  path, as in round 1.
