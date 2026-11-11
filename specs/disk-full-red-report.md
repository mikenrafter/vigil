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

---

# Round 2

Spec: `specs/disk-full.md`, section "Amendments after red" (commit
`610c3ad4`), which answers A1-A10 above. This round only changes tests. No
behaviour was added; the stub is as in round 1.

STATUS: done. Red commit for this round: see "Commit" at the end of this section.

## Amendments and the tests that pin them

| Amendment | What is pinned | Tests (changed = C, new = N) |
|---|---|---|
| DF1a (A1) | a tick whose save failed ends the call; no in-memory re-tick; `wait` sleeps one interval after a pending envelope with `persist.ok = false` | C `a_quiet_tick_that_cannot_save_...` (exactly 2 reads, `waited_s` 0, healthy twin reads 3); N `wait_sleeps_one_interval_...`; N CLI quiet keepalive returns at once |
| DF3a (A2) | `redelivery: true` only with events or an unsaved terminal status | C `assert_failed_persist` in `disk_full_tests.rs` and `tests/common/mod.rs` (new `redelivery` argument, key set checked both ways); C fired/expired callers say `true`; C quiet case says absent |
| DF3b (A9) | redelivered events are those since the last saved baseline | N `a_redelivery_covers_everything_since_the_last_saved_baseline_...` |
| DF4a (A4) | no reserve file, no retry | N store test (attempt count 1, a one-shot fault is not retried); C lock entry test (no-reserve leg); N engine test (`used_reserve` absent) |
| DF5a (A3) | an unrecoverable cancel is an envelope with `persist.ok = false` | C `a_cancel_that_cannot_be_saved_...`; N CLI cancel test |
| DF5b (A6) | message holds the state root and lowercase `disk full`; `persist.error` has the OS text | C every "names the root" assertion (no `to_lowercase()` any more) |
| DF7 (A7) | a failed baseline save removes the placeholder, so the same id can be created again | N engine test; N CLI test |

## Result

50 tests fail on assertions, none on compile errors, none by hanging. Every
test that passed before still passes.

| Target | passed | failed | new tests (round 1 + round 2) | new tests that fail |
|---|---|---|---|---|
| `--lib` | 243 | 37 | 35 + 6 = 41 | 37 |
| `--test cli_e2e` | 28 | 10 | 8 + 3 = 11 | 10 |
| `--test mcp_e2e` | 10 | 3 | 3 + 0 = 3 | 3 |
| total | 281 | 50 | 46 + 9 = 55 | 50 |

Round 1 was 281 passed, 41 failed. The 9 new tests are the 9 new failures; the
same 5 controls still pass (they must stay green after green). Changed tests
stay red for the same reason as before, now on a sharper assertion. Run on
the stub with the command at the top of this report (qex job `vigil-red2`,
about 16 s, 750 MB).

Intermittent failures seen this round, none of them in tests I touched:

* `engine::tests::prune_also_removes_abandoned_pending_watches_and_their_lock_files_but_never_the_global_lock`
  and `engine::tests::listing_prunes_too` failed in some of the 11 prototype
  runs (2 and 3 of them) and passed alone and on the next run. They did not
  fail on the committed tree (the 3 stub runs of this round). Not investigated; they are about pruning old watches and
  have nothing to do with disk full. The known flaky
  `inputs::tests::port_without_a_want_...` did not fail this round.

## Changed and added tests

CLAIM ids: the spec's DF1-DF7 and the amendments DF1a, DF3a, DF3b, DF4a, DF5a,
DF5b (grep `CLAIM(DF1a)` etc. finds the proof). `x` = the file the test is in.

### `src/disk_full_tests.rs`

| Test | Claim | Change | Fails today on |
|---|---|---|---|
| `a_quiet_tick_that_cannot_save_ends_the_call_at_once_and_is_not_ticked_again` | DF1, DF1a, DF2, DF3a | rewritten. Healthy twin of a 30 s call reads 3 times (baseline + t=0 + t=20); the faulted call reads exactly 2, has `waited_s` 0, and its persist has exactly `error, kind, ok, path` | no `persist` field (stub never fails the save) |
| `wait_sleeps_one_interval_after_a_pending_envelope_that_could_not_be_saved` | DF1a | new. `wait` on a quiet watch (interval 20 s), every write fails, stopped after 50 s of virtual time: still running; scripted reads start at offsets `[0, 0, 20, 40]` s; at least 3 failed-save attempts. A `wait` without the sleep spins, which the real-time watchdog turns into a failure | precondition `attempts >= 3` (stub: 0) |
| `redelivery_is_flagged_exactly_when_events_or_a_terminal_status_are_at_risk` | DF3a | new. Four unsaved envelopes: fired (event) true, expired (no events, terminal) true, cancelled (terminal) true, quiet pending ABSENT | no `persist` field |
| `a_redelivery_covers_everything_since_the_last_saved_baseline_when_the_input_kept_changing` | DF3b, DF3 | new. Call 1 loses event `w:1:0` ("boom"); the input grows ("bang"); call 2 must equal a healthy twin world in which both lines were appended before one tick (event `w:1:0`, summary "bang", digest has both lines, `tick` 1), and differ from the lost event | no `persist` field |
| `a_tick_on_a_disk_without_a_reserve_reports_the_first_failure_and_does_not_retry` | DF4a, DF4 | new. A fault on the first write only: with a reserve the retry rescues it (control, 2 attempts); with the reserve deleted the note is `ok:false`, no `used_reserve`, 1 attempt, file unchanged | precondition: no reserve (stub never makes one) |
| `a_cancel_that_cannot_be_saved_returns_an_envelope_with_persist_not_a_silent_success` | DF5a, DF3a, DF6 | tightened. It used to accept an error or an envelope; now only an `Ok` cancelled envelope with `ok:false`, `kind` disk_full, `redelivery` true, 2 attempts, file byte-identical, and a later healthy cancel really cancels | no `persist` field |
| `a_create_whose_baseline_save_fails_removes_its_placeholder_so_the_id_can_be_reused` | DF7, DF5b | new. Writes 2 and 3 fail (baseline and its retry); the error names the root and says `disk full`; 3 attempts prove the placeholder was written; no `dup.json`, `list` empty, no `.tmp`; a healthy process creates the same id | `create` succeeds on the stub |
| `a_create_on_a_full_disk_...`, `a_lock_file_that_can_never_be_created_...` | DF5, DF5b | `to_lowercase()` removed: the text must contain the root and the lowercase `disk full` | unchanged |
| `the_failed_save_is_described_in_persist_...`, `assert_failed_persist` | DF2, DF3a | helper has a `redelivery: bool` argument and checks the key set both ways (five keys, or four with the key absent); the errno table fires an event, so `true` | unchanged |

### `src/store.rs`

| Test | Claim | Change |
|---|---|---|
| `with_no_reserve_file_a_full_disk_is_reported_at_the_first_failure_with_no_retry` | DF4a, DF5b | new. For 28 and 122: with a reserve, a first-write-only fault is rescued (control, 2 attempts, `used_reserve`); with the reserve deleted, `DiskFull` naming the root after exactly 1 attempt, old file intact, no `.tmp` |
| `creating_a_lock_file_on_a_full_disk_goes_through_the_reserve_and_retry_on_every_entry_point` | DF5, DF4a | gained a third leg per entry point (4 of them): no reserve, a first-creation-only fault, `DiskFull` after 1 attempt (the "left open" comment is gone) |
| `a_disk_that_stays_full_...`, `says_disk_full_naming` | DF4, DF5b | exact lowercase `disk full` |

### `tests/cli_e2e.rs`, `tests/mcp_e2e.rs`, `tests/common/mod.rs`

| Test | Claim | Change |
|---|---|---|
| `a_quiet_keepalive_on_a_full_disk_returns_pending_at_once_without_a_redelivery_flag` (x cli) | DF1a, DF3a, DF2 | new. Must exit within 5 s (a healthy call lasts 270 s): exit 0, pending, four-key persist, one stderr line naming the root. On the stub it is killed after 5 s, so the red run waits 5 s for it |
| `cancel_on_a_full_disk_prints_the_envelope_with_persist_and_leaves_the_watch_pending` (cli) | DF5a, DF3a, DF6 | new. Exit 0, `cancelled` envelope with the five-key persist, one stderr line naming the root, file unchanged and still `pending` |
| `a_create_whose_baseline_cannot_be_saved_leaves_no_placeholder_so_the_same_id_can_be_created_again` (cli) | DF7, DF5b | new. `watch:28:2..`: exit 1, root and `disk full` on stderr, no watch file, no `.tmp`; then a healthy process creates the same id (exit 0) |
| `keepalive_on_a_full_disk_...`, `wait_keeps_its_exit_code_when_the_final_save_fails` (cli), `keepalive_over_mcp_on_a_full_disk_...` (mcp) | DF3a, DF1a | pass `redelivery = true` (events or a terminal status at risk). The `wait` expired case now also relies on DF1a: one pending envelope, one sleep of 1 s, then one expired envelope, one JSON on stdout |
| `create_on_a_full_disk_...` (cli), `watch_create_over_mcp_on_a_full_disk_...` (mcp) | DF5b | exact lowercase `disk full` |
| `assert_failed_persist` (common) | DF2, DF3a | new `redelivery: bool` argument, same two key sets as the engine helper |

## Decisions I took where the amendments leave room

* **Expiry says `redelivery: true`.** DF3a counts "expired" as a terminal
  status that was not saved. The brief for this round said "update the quiet/
  expiry cases"; I read it as quiet = absent, expired = true. If you meant
  expiry to be absent, change one row in
  `redelivery_is_flagged_exactly_when_events_or_a_terminal_status_are_at_risk`
  and the `wait` expired case in `cli_e2e.rs`.
* **Cancel on a full disk exits 0 in the CLI** and writes one stderr line
  naming the root, like a keepalive. DF5a only says "an envelope".
* **DF4a with a reserve that exists but cannot be deleted still retries.**
  The round 1 test `a_reserve_that_cannot_be_deleted_...` stays: "no reserve
  file" means the file is absent, not that deleting failed.
* **DF7 test models the placeholder with `Range{first: 2}`**: placeholder is
  watch write 1, the baseline is write 2, so the placeholder write is what
  succeeds. It does not fault `Reserve`, otherwise there is no reserve after
  write 1 and DF4a (correctly) skips the retry; I made that mistake first and
  the prototype showed it.
* **The `wait` test counts reads, not envelopes**, because `wait` only returns
  on a finished watch. The 50 s window and the offsets `[0, 0, 20, 40]` are
  derived above, not read from output.

## Evidence the tests are satisfiable and sharp (not committed)

Throwaway green prototype: round 1's prototype plus the amendments (stop after
an unsaved tick, `wait` sleeps one interval, `redelivery` only when something
is at risk, no retry without a reserve file for both watch writes and lock
creation, placeholder removed on a failed baseline). In a scratch copy under
the session scratchpad, not in the repo. With it all 331 tests pass (280 lib,
38 cli, 13 mcp), on the exact test files in the commit.

One mutation at a time on that prototype, tests that went red:

| Mutation | Killed by |
|---|---|
| no stop after an unsaved tick | 12 engine tests (most with the watchdog spin message), the quiet CLI test and 5 other e2e tests (cli and mcp) |
| `wait` does not sleep | `wait_sleeps_one_interval_...` |
| `wait` sleeps 1 s instead of the interval | `wait_sleeps_one_interval_...` |
| `redelivery` always on | the quiet engine test, the DF3a table, the quiet CLI test |
| `redelivery` never on | the DF3a table, the errno table, cancel (engine and CLI), fired CLI and MCP tests, `wait` CLI |
| retry even without a reserve (watch write) | the DF4a engine test and the DF4a store test |
| retry even without a reserve (lock creation) | the lock entry-point store test |
| placeholder not removed | the DF7 engine and CLI tests |

## Notes for the green phase

* `redelivery` is decided from the envelope's content, so the same rule serves
  tick, expiry and cancel: events non-empty or status terminal.
* `Vigil::wait` must sleep the watch's `interval_s` (not a constant), and only
  after a pending envelope whose `persist.ok` is false.
* DF4a distinguishes "no `.reserve`" (no retry) from "`.reserve` could not be
  deleted" (retry). Both lock creation and watch writes follow it.
* A tick whose save failed must not mark the watch returned (round 1, still
  pinned) and must end the call.

## Commit

Tests and this section: `vigil: tighten red tests for disk-full amendments
(DF1a-DF7)`. The SHA is in the last line of the final message and, as in
round 1, is added to this file in a one-file follow-up commit.
