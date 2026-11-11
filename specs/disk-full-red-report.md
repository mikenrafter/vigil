# vigil disk-full: red phase report

Spec: `specs/disk-full.md` (claims DF1-DF6). Branch `feat/vigil-disk-full`.
Red commit: `fb2cdea5` (message `vigil: red tests for disk-full resilience (DF1-DF6)`).

## Result

41 new tests fail on assertions. No compile errors. Every test that existed
before still passes.

| Target | passed | failed | new tests | new tests that fail |
|---|---|---|---|---|
| `--lib` | 243 | 31 | 35 | 31 |
| `--test cli_e2e` | 28 | 7 | 8 | 7 |
| `--test mcp_e2e` | 10 | 3 | 3 | 3 |
| total | 281 | 41 | 46 | 41 |

Before this change the suite had 239 + 27 + 10 = 276 tests, all green. The 5
new tests that pass are controls, on purpose (they must stay green after the
green phase): see "Controls" below. The failures were checked by name against
the list of added functions: every failing test is new.

Two pre-existing tests flake on this shared machine; neither touches code I
changed:

* `keepalive::tests::a_blocked_call_returns_early_when_the_watch_fires_and_waited_s_is_the_real_wait`
  failed once in a prototype run under heavy load (`60.146s != 60s`, real time
  leaking into paused tokio time).
* `inputs::tests::port_without_a_want_reports_no_want_status_and_want_closed_inverts`
  failed in the last two `cargo test` runs (`Some(false) != Some(true)`: it
  drops a listener and expects the port closed, so another process on the box
  that binds a loopback port in that window breaks it). The same test binary
  passed it 8 of 8 times when run directly, and the earlier full runs passed
  it. When it hits, the lib line reads 242 passed, 32 failed.

Command used (cargo is not on PATH on host-b; these store paths exist today):

```
TOOLS=/nix/store/ix3zdmwb5yds2zjfy4zcdgx4191yr36j-rust-default-1.99.1/bin:/nix/store/g098s364ad1fpbj068sg6y3sv643b8al-gcc-wrapper-15.2.0/bin
cd  && qex submit --wait --wait-timeout 9m --cpu 2 --mem 1.7GB --name vigil-red \
  --id-file ~/.qex-vigil-red.id -- env PATH=$TOOLS:$PATH cargo test -j2 --no-fail-fast
qex logs "$(cat ~/.qex-vigil-red.id)" --all     # the full output; the submit output is cut
```

`--mem 3GB` never started: the machine reports ~2.8 GB usable and qex keeps 1
GB in reserve, so a claim above ~1.8 GB waits forever. The job really uses
about 750 MB.

## The seam (stub): `src/fault.rs`

What exists now, all behaviour-neutral:

* `FaultPlan` (cheap to clone, clones share counters), `FaultRule {kind, errno,
  when}`, `WriteKind {Watch, Lock, Reserve}`, `When {Every, Nth(n), Range{first,
  last}, WhileExists(path)}`.
* `FaultPlan::{none, from_rules, parse, check, attempts}`. `check` always
  returns `Ok`, `attempts` returns 0, `parse` returns an empty plan. The green
  phase makes them real.
* `Store::with_faults`, `Store::faults`, `Vigil::with_write_faults`; the store
  keeps a plan and ignores it.
* `Store::save` now returns `Result<WriteOutcome, StoreError>` (`WriteOutcome
  {used_reserve: bool}`, default false). Callers that ignored the unit value
  compile unchanged.
* `StoreError::DiskFull(String)`, never constructed yet. Display is
  `disk full: <text>`.
* `main.rs` reads `VIGIL_TEST_FAULT` at the edge and hands the plan to
  `Vigil`. Unset: `FaultPlan::none()`. Set to garbage: exit 1 naming the
  variable (stub parse accepts everything, so the test for this is red).

Contract of the seam, written in the `fault.rs` module docs and pinned by 7
tests there:

* `check(kind)` is called once per attempt, so the retry is attempt 2.
  `Watch`: inside `write_atomic` after `<path>.tmp` exists and before any byte
  is written (so the `.tmp` cleanup is on the line and the test can watch it
  being deleted). `Lock`: only when the lock file has to be created.
  `Reserve`: before creating `<root>/.reserve`.
* The injected error is `io::Error::from_raw_os_error(errno)`.
* Counters are per kind, 1-based.
* Env syntax: `KIND:ERRNO:WHEN[,...]`, `KIND` in `watch|lock|reserve`, `WHEN`
  in `all | N | N..M | N.. | while=PATH`.

Why this shape:

* **Runtime plan in an env var, not `cfg(test)` or a cargo feature.** The e2e
  suites spawn the built binary; `cfg(test)` is invisible to it. A feature
  needs `required-features` on the e2e targets, and then a plain `cargo test`
  silently skips them (the "skipped fixtures" failure mode). Cost: a release
  binary honours `VIGIL_TEST_FAULT`. Unset it changes nothing; a malformed
  value is a hard error, so a typo cannot turn a test into a no-op. `testing.rs`
  already ships in the library, so test support in the lib is the local
  convention.
* **`while=PATH` (not in the spec).** A spawned binary cannot be called, so
  the test needs a switch both sides see. A path is that switch, and it models
  the incident exactly: `while=<root>/.reserve` means "the disk is full until
  the reserve is deleted", so a retry can only succeed if the store deleted
  the reserve first. That is how DF4's order (delete, then retry) is proven
  and not assumed. A sentinel file also lets one MCP server create a watch on
  a healthy disk, fail a tick, and recover, without counting writes.
* **Per-kind rules.** "Fail the Nth write" is ambiguous once reserve creation
  and lock creation are also writes. A `Reserve` rule lets a test model "no
  room to recreate the reserve" (a full disk) and "creating the reserve failed"
  (DF4 best effort).
* **A real, seam-free ENOSPC.** `store.rs` also has a test that symlinks
  `w.json.tmp` to `/dev/full`, so the kernel itself returns errno 28. That is
  the independent check that classification works on a real error.

## Test list

CLAIM ids are the spec's (DF1-DF6) plus `DF-seam` for the seam's own tests.

### `src/fault.rs` (7, the seam)

| Test | Claim | Red? |
|---|---|---|
| `an_empty_plan_never_fails_anything` | DF-seam-inert | control, green |
| `every_fails_each_attempt_of_its_kind_...` | DF-seam | red |
| `nth_and_range_fail_exactly_the_named_attempts_...` | DF-seam | red |
| `while_exists_follows_a_path_...` | DF-seam | red |
| `clones_of_a_plan_share_one_attempt_counter` | DF-seam | red |
| `parse_builds_the_plan_the_documented_syntax_names` | DF-seam | red |
| `parse_refuses_a_malformed_plan_instead_of_injecting_nothing` | DF-seam | red |

### `src/store.rs` (12)

| Test | Claim | Red? |
|---|---|---|
| `a_successful_save_leaves_a_256_kib_reserve_...` | DF4 | red: no reserve |
| `an_existing_reserve_is_not_rewritten` | DF4 | control, green |
| `failing_to_create_the_reserve_is_never_an_error` | DF4 | red: creation never tried |
| `disk_full_on_the_first_attempt_frees_the_reserve_retries_once_...` (28, 122) | DF4, DF6 | red: no retry |
| `after_spending_the_reserve_a_new_one_is_made_...` | DF4 | red |
| `a_disk_that_stays_full_fails_after_exactly_one_retry_...` (28, 122) | DF4, DF5, DF6 | red |
| `errors_that_are_not_a_full_disk_neither_retry_nor_touch_the_reserve` (5, 13, 30) | DF4, DF2 | red: precondition, no reserve |
| `a_reserve_that_cannot_be_deleted_does_not_turn_the_retry_into_an_error` | DF4 | red: precondition |
| `a_failed_write_removes_its_tmp_file_and_keeps_the_previous_content` (28, 5) | DF6 | red: write does not fail |
| `a_real_enospc_from_the_kernel_is_a_disk_full_error_and_still_atomic` | DF2, DF6 | red on the last assertion only: error is `Io`, not `DiskFull`. The atomicity half passes today. |
| `creating_a_lock_file_on_a_full_disk_goes_through_the_reserve_and_retry_...` (4 entry points) | DF5 | red |
| `taking_a_lock_whose_file_exists_needs_no_disk_space_...` | DF-seam, DF5 | control, green |

### `src/disk_full_tests.rs` (16, engine and keepalive level)

Run on paused tokio time with a real-time watchdog (20 s). A call that
re-ticks in a hot loop never lets virtual time advance, so a normal timeout
cannot stop it; the watchdog turns a spin into a test failure. Checked against
a prototype: with the "stop after an unsaved tick" guard removed, 8 tests fail
with "the call is spinning" after 20 s instead of hanging.

| Test | Claim | Red? |
|---|---|---|
| `a_tick_whose_save_fails_returns_the_same_envelope_a_healthy_tick_returns` (healthy twin plus hand-derived `w:1:0`, "boom") | DF1 | red |
| `the_failed_save_is_described_in_persist_with_the_kind_the_errno_decides` (28, 122, 5, 13: exact key set, OS text, path) | DF2, DF3 | red |
| `a_clean_write_adds_no_persist_field_to_any_envelope` (fired, pending, cancelled) | DF2 | control, green |
| `a_quiet_tick_that_cannot_save_returns_pending_with_persist_and_does_not_spin` | DF1, DF2 | red |
| `a_tick_that_could_not_save_does_not_advance_the_baseline_and_its_events_come_again` | DF3, DF6 | red |
| `a_separate_process_reports_the_events_a_failed_save_lost` | DF3 | red |
| `a_no_id_keepalive_does_not_count_an_unsaved_result_as_returned` | DF3 | red |
| `a_tick_that_only_fits_after_the_reserve_is_deleted_is_saved_and_says_so` (28, 122) | DF4, DF2 | red |
| `an_error_that_is_not_a_full_disk_leaves_the_reserve_alone_end_to_end` | DF4, DF2 | red |
| `a_create_on_a_full_disk_fails_with_a_message_that_names_the_root_...` | DF5 | red |
| `a_create_that_fits_once_the_reserve_is_freed_succeeds` | DF5, DF4 | red |
| `a_cancel_that_fits_once_the_reserve_is_freed_is_saved_and_reports_it` | DF5, DF4 | red |
| `a_cancel_that_cannot_be_saved_is_never_a_silent_success` | DF5, DF6 | red |
| `a_missing_lock_file_is_created_through_the_reserve_and_the_tick_still_answers` | DF5, DF1 | red: precondition, no reserve |
| `a_lock_file_that_can_never_be_created_fails_the_call_with_a_message_that_names_the_root` | DF5 | red |
| `a_failed_tick_save_leaves_the_watch_file_byte_identical_and_no_tmp_anywhere` | DF6, DF1 | red |

### End to end

CLI, appended to `tests/cli_e2e.rs` (8):

| Test | Claim | Red? |
|---|---|---|
| `keepalive_on_a_full_disk_prints_the_envelope_exits_0_and_names_the_state_root_once_on_stderr` (28 and 5) | DF1, DF2, DF6 | red |
| `the_next_process_on_a_healthy_disk_reports_the_events_the_full_disk_run_could_not_save` | DF3 | red |
| `wait_keeps_its_exit_code_when_the_final_save_fails` (fired 0, expired 3) | DF1, DF2 | red |
| `the_reserve_exists_after_a_healthy_run_and_saves_the_tick_when_the_disk_is_full` | DF4 | red |
| `a_healthy_run_prints_no_persist_field_and_nothing_on_stderr` | DF2 | control, green |
| `create_on_a_full_disk_exits_1_with_a_line_that_names_the_root_and_says_disk_full` | DF5 | red |
| `cancel_on_a_disk_that_only_has_the_reserve_to_give_is_saved_and_says_so` | DF5, DF4 | red |
| `a_malformed_fault_plan_is_refused_loudly_instead_of_injecting_nothing` | DF-seam | red |

MCP, appended to `tests/mcp_e2e.rs` (3):

| Test | Claim | Red? |
|---|---|---|
| `keepalive_over_mcp_on_a_full_disk_returns_the_envelope_with_persist_not_a_tool_error` | DF1, DF2, DF6 | red |
| `the_same_mcp_server_delivers_an_unsaved_result_again_to_a_no_id_keepalive` (the agent's real call shape) | DF3 | red |
| `watch_create_over_mcp_on_a_full_disk_is_an_error_that_names_the_root_and_says_disk_full` | DF5 | red |

Shared helpers are in `tests/common/mod.rs` (`FAULT_ENV`, `faulted_command`,
`assert_failed_persist`, `reserve_used_note`, `only_the_reserve_to_give`).

### Controls (5 new tests that pass today, and must stay green)

`an_empty_plan_never_fails_anything`, `an_existing_reserve_is_not_rewritten`,
`taking_a_lock_whose_file_exists_needs_no_disk_space_...`,
`a_clean_write_adds_no_persist_field_to_any_envelope`,
`a_healthy_run_prints_no_persist_field_and_nothing_on_stderr`. The atomicity
half of the `/dev/full` test also passes today, inside a test that is red on
its last assertion. Two controls (`an_existing_reserve_is_not_rewritten`, the
existing-lock one) are only trivially green against the stub; they gain teeth
once the reserve and the lock hook exist.

## Existing tests I had to touch (DF4 forces it)

DF4 puts a 256 KiB `.reserve` in the state root. Three existing tests listed
the root exactly. I made each tolerate `.reserve` *only as a regular file of
exactly 262144 bytes*, and only if present, so they pass now and after green
and still reject any other stray file. No assertion was loosened beyond that.

* `store.rs` `lock_files_exist_only_where_a_lock_was_taken_and_saving_creates_none`:
  its root listing skips `.reserve`.
* `tick.rs` `a_full_lifecycle_writes_only_lock_and_json_files_under_the_state_root`:
  `.reserve` of 262144 bytes is allowed.
* `tests/cli_e2e.rs` `nothing_is_written_outside_the_state_root`: same.

Without this the green phase would have hit two red existing tests and been
tempted to loosen them.

## Evidence the tests are satisfiable and sharp (not committed)

I wrote a throwaway green implementation in a scratch copy
(`/tmp/claude-1001/-home-mini-Documents-repos-host-config/e2feadb3-8992-4225-adb8-3e94a6e8c8f6/scratchpad/green`,
not in the repo, ~250 lines: seam, reserve, retry, classification, `persist`
note, in-memory result of an unsaved tick, stop-on-unsaved, stderr line). With
it, all 46 new tests pass and the whole suite is green once the three existing tests
above tolerate `.reserve`. It is a check on the tests, not a design to copy.

Manual mutations on that prototype, one at a time:

* Retry without deleting the reserve first: killed by the store, tick, create,
  cancel and CLI rescue tests (8 tests).
* Count an unsaved result as "returned": killed by the engine and the MCP
  no-id tests (2 tests).
* Remove the stop-after-unsaved-tick guard: 8 tests fail with the watchdog
  message. Nothing hangs.

Not mutated: the per-errno kind table and the stderr-line count; they are
asserted field by field, so I expect them to bite.

## Things in the spec I found ambiguous or wrong (push back)

* **A1. A failed save on a quiet tick can spin.** DF1 says the envelope is the
  one a successful tick would return. A successful quiet tick keeps ticking to
  `max_s`. With a failed save the persisted `last_tick_at` never moves, so the
  watch is due again at once and a naive loop re-ticks forever and hammers a
  full disk. Propose: a tick whose save failed ends the call and returns the
  envelope (pending is fine). The tests allow either that or ticking in memory,
  but forbid a spin and forbid more reads than a healthy call makes
  (`a_quiet_tick_...`). Related: `vigil wait` loops `keepalive` until the watch
  finishes; with an early return it would call again in a tight loop until
  the disk has room or the deadline passes. Propose a short sleep (one
  interval) in `wait` after a pending envelope with `persist.ok = false`.
* **A2. `redelivery: true` on a tick with nothing to redeliver.** The spec says
  every failure note carries it. For a quiet tick or an expiry there are no
  events at risk. I kept the literal reading (always true when `ok` is false).
  If you want it only when events are at stake, the five-key assertion in
  `assert_failed_persist` changes.
* **A3. Cancel (and an unrecoverable lock) are not specified.** Only a failing
  *create* has a stated error. For cancel I assert the rescue path (saved,
  reserve spent, and, inferred from DF4's "the envelope carries", the cancel
  envelope carries the `used_reserve` note), and for an unrecoverable cancel I
  accept either an error naming the root and "disk full", or an envelope with
  `persist.ok = false`; a silent success fails. Please pick one in the spec
  (I would pick the envelope, like a tick) and I will tighten the test.
* **A4. Retry when there is no reserve.** The spec says delete `.reserve` and
  retry once. If there is nothing to delete, is a retry still made? It is
  harmless, but the attempt count and `used_reserve` differ. The tests always
  have a reserve when they count attempts and never pin this case.
* **A5. "Created after any successful write", including the retry.** Read
  literally, a successful retry is a successful write, so the reserve is made
  again at once. On a truly full disk that fails silently, which is fine. One
  store test pins it for a disk with room (`after_spending_the_reserve_...`);
  say so if you meant "not until the next write".
* **A6. "Says the disk is full".** I fixed the contract as: the message
  contains the state root path and the lowercase text `disk full`. `persist.error`
  contains the OS text (`No space left on device`, `Disk quota exceeded`,
  `Input/output error`, `Permission denied`, taken from glibc, not from the
  code). Put the exact words in the spec.
* **A7. A failed `create` burns the id.** `create` writes a placeholder, then
  the baseline. If the baseline write fails, the error is returned but the
  placeholder stays, so the agent's retry gets `DuplicateId`. Deleting a file
  works on a full disk, so the fix is cheap: remove the placeholder when the
  baseline save fails. The spec is silent, so no test pins it. I recommend
  adding it as DF7.
* **A8. `watch_status` after a failed save says `pending`** while `keepalive`
  answered `fired` (the file is stale by design, DF3). Worth one sentence in
  the skill doc (D4), next to "keep calling".
* **A9. Unsaved events reappear with later content.** DF3 says the same events
  are reported again. If the input kept changing, the re-tick also includes the
  later content (the baseline did not move). Fine, but the "same events"
  wording only holds for an unchanged input. The tests keep the input still.
* **A10. Idea, not in scope:** on a disk_full failure, `prune` of finished
  watches older than 7 days would free space before the retry. Deleting works
  on a full disk. Not tested.

## Notes for the green phase

* `Envelope` needs a `persist` field (skip when `None`); the red phase did not
  add it, so the tests read the envelope as JSON.
* The three existing tests that now tolerate `.reserve` are the only existing assertions that
  changed.
* `specs/auto-interval.md` is untracked in this worktree
  and is not part of this commit.

## Commit

Red commit `fb2cdea5` on `feat/vigil-disk-full`. A report cannot contain the SHA of the commit that
holds it, so this line was added in a one-file follow-up commit.
