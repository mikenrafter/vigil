# vigil — surviving a full disk (spec draft, 2026-11-10)

## Why

On 2026-11-10 host-b's root filesystem hit 100%. `vigil keepalive` then exited
1 with `io error: No space left on device (os error 28)` and printed no
envelope. The agent lost the tick's result (events, digest, `next`) and had to
guess whether its watch still existed. Sensing worked; only the state write
failed. A watcher that dies when the disk fills is useless at exactly the
moment disks fill (big builds).

Intent: a state-write failure must never cost the agent the tick's answer,
and a full disk must be named as such.

Out of scope: moving the state root, judge (Laya) behaviour, any new input
kind.

## Claims

- **DF1 — answer survives a failed write.** When sensing succeeds and the
  watch-file write fails, `keepalive` (MCP and CLI) still returns the full
  envelope for that tick: same `events`, `digest`, `status`, `next` as a
  successful tick would have. The CLI exits as it would have (0 for
  keepalive; `wait` keeps its outcome code).
- **DF2 — failure is visible.** That envelope carries
  `"persist": {"ok": false, "kind": "disk_full" | "io", "error": "<message>", "path": "<file>"}`.
  `kind` is `disk_full` for ENOSPC (28) and EDQUOT (122), `io` otherwise. A
  successful write carries no `persist` field (wire format unchanged when all
  is well). The CLI also writes one line to stderr naming the state root.
- **DF3 — nothing is lost, delivery is at-least-once.** A tick whose write
  failed does not advance the persisted baseline, so its events are reported
  again on the next successful tick (in the MCP server and across separate CLI
  processes alike). The envelope says so: `"persist": {..., "redelivery": true}`.
  Events are never silently dropped.
- **DF4 — reserve ("ballast") file.** The store keeps `<root>/.reserve`, 256
  KiB, created best-effort after any successful write when absent. On a
  disk_full write failure the store deletes `.reserve` and retries the same
  write once. If the retry succeeds, the tick is persisted and the envelope
  carries `"persist": {"ok": true, "used_reserve": true, "kind": "disk_full"}`
  so the agent still learns the disk is full. Failure to create or delete the
  reserve is never itself an error.
- **DF5 — same path for every write.** Watch create, tick save, cancel, and
  per-watch lock-file creation all go through the reserve-and-retry path. A
  create that still fails returns an error whose message names the state root
  and says the disk is full (not a bare `io error`).
- **DF6 — atomicity kept.** The existing guarantee stands: on any failure the
  previous watch file is intact and no `.tmp` remains (store.rs `write_atomic`).

## Test seam

Tests cannot fill a real disk. The store gets a narrow fault-injection seam
(a test-only writer hook or trait) that makes the Nth write, or every write,
fail with a chosen `io::Error` (raw os error 28 / 122 / other). The hook must
not change production behaviour; red tests drive DF1–DF6 through it, at the
store level and end to end (`tests/cli_e2e.rs`, `tests/mcp_e2e.rs`).

## Defaults taken (owner may override)

- D1: reserve size 256 KiB, path `<root>/.reserve`.
- D2: no new CLI exit code; the envelope's `persist` field is the signal.
- D3: state root unchanged (`~/.local/state/vigil`); moving it to
  `$XDG_RUNTIME_DIR` was considered (cheaper, survives `/` filling) and
  rejected for now because it loses watches on reboot and tmpfs can fill too.
- D4: skill doc (`agent-integration/skills/vigil/SKILL.md`) is updated in the
  same change: glob inputs take `pattern`; a `change` watch ends after it
  fires; what `persist` means and what to do (free space, keep calling).

## Amendments after red (orchestrator, 2026-11-10; answers red report A1–A10)

- **A1 → DF1a.** A tick whose save failed ends the call and returns its
  envelope (no in-memory re-ticking, no spin). `vigil wait`, after a pending
  envelope with `persist.ok = false`, sleeps one interval before calling again.
- **A2 → DF3a.** `redelivery: true` only when something is at risk: the
  envelope has events, or a terminal status (fired/expired/cancelled) that
  was not saved. Otherwise the field is absent.
- **A3 → DF5a.** A cancel that cannot be saved even after the reserve returns
  an envelope with `persist.ok = false` (like a tick), never a silent success.
- **A4 → DF4a.** No reserve file, no retry: the first failure is reported
  (`used_reserve` absent).
- **A5.** Literal reading kept: the reserve is recreated after any successful
  write, including right after the retry when there is room.
- **A6 → DF5b.** Error contract: the message contains the state root path and
  the lowercase words `disk full`; `persist.error` carries the OS text.
- **A7 → DF7.** A `create` whose baseline save fails removes its placeholder
  file, so the agent can retry with the same id.
- **A8.** Skill doc (D4): after a failed save `status` may still say pending
  while keepalive answered fired; trust the envelope, keep calling.
- **A9 → DF3b.** Redelivered events are those since the last saved baseline:
  the unsaved ones plus anything newer.
- **A10.** Out of scope (pruning old watches to free space).
- **Seam.** `VIGIL_TEST_FAULT` stays an env var read in `main.rs`; it is
  documented as test-only, a malformed value is a hard error, unset changes
  nothing.

## Amendments after spec review round 1 (Sol 6/10, bar 9; orchestrator, 2026-11-10)

- **S1 → DF3 narrowed.** Guaranteed: this call's answer (DF1). Best effort
  after a failed save: the next tick re-senses from the last saved baseline.
  Inputs with durable history (`file` appends, `glob`) redeliver their content;
  transient inputs (`cmd`, `url`, `port`, `process`) report their state at the
  next sensing and may not repeat a vanished change; an expiry or a
  cancellation saved by another process wins over redelivery. Event identity:
  every event produced by a tick that follows a failed save carries
  `"redelivered": true`; agents must not deduplicate by `event_id` across a
  persist failure (the tick number did not advance, so ids can repeat with
  different content). Tests assert literal event content (both lines in the
  DF3b case), and cover a transient input that reverts, expiry, and a
  concurrent saved cancel.
- **S2 → DF4b.** A valid reserve is a regular file of 262144 bytes with at
  least that much allocated (`st_blocks * 512 >= 262144`), written with real
  zeros and fsynced. A short, sparse or unreadable reserve is replaced on the
  next successful write (the ten-byte "preserve" control is reversed). The
  rescue is best effort and the docs say so: a write larger than the reserve,
  inode exhaustion, quotas, compression or a different filesystem can still
  fail, and then DF1/DF2 apply.
- **S3 → DF4c.** Reserve create, delete and the retry run under the
  store-wide lock (taken after any watch lock, matching SPEC.md's lock order),
  so two processes cannot both spend or re-create it mid-rescue. vigil never
  keeps the reserve open. One threaded test races two rescues on one root:
  exactly one consumes the reserve, the other reports `disk_full`.
- **S4 → seam stages and DF6 precision.** The seam injects at each stage of
  `write_atomic`: create (incl. ENOSPC on inode exhaustion), short write,
  `sync_all`, `rename`; and for the reserve: create, short write. The rename
  is the commit point: before it the old file is intact; a failed cleanup of
  `.tmp` is best effort and the leftover is removed at the next successful
  write. Retry-error precedence: the retry's error is reported, the first is
  kept in `persist.first_error`. A crash after deleting the reserve and before
  the retry leaves no reserve; the next successful write re-creates it.
- **S5 → DF2a (several watches).** A keepalive covering several watches
  reports `persist` for the watch whose envelope it returns, plus
  `persist_failures: [{watch_id, kind, error, path}]` listing every other
  covered watch whose save failed in that call (absent when empty). A quiet
  failed save on a secondary watch is therefore visible. Tests: two watches,
  one saved terminal and one unsaved; a secondary quiet failure; mixed kinds.
- **S6 → doc only.** SPEC/skill state that `VIGIL_TEST_FAULT` is for tests,
  assumes a trusted process environment, and that inheriting it disables
  writes or (if malformed) stops vigil from starting.

## Decisions after red round 3 (orchestrator, 2026-11-10)

- `redelivered: true` is set by the process that failed to save (MCP server,
  or `vigil wait`). A fresh CLI process cannot know, so its events are
  unflagged; the skill tells agents to treat any event after an envelope with
  `persist.ok = false` as possibly repeated.
- `persist.first_error` is present whenever a retry was made and failed, even
  if both errors match.
- `create` holds the store-wide lock while saving its placeholder; the rescue
  reuses the held lock (no self-deadlock).
- A rescue waits for the store-wide lock, bounded by vigil's existing lock
  timeout; on timeout it reports `disk_full` without retrying.
- `.tmp` cleanup after a successful write covers only that watch's own file;
  never sweep other `*.tmp` files (another process may be mid-write).

## Defaults after spec review round 2 (final round; Sol 6/10; orchestrator, 2026-11-10)

- **S1 qualified.** Redelivery covers `file` appends only, within the existing
  64 KiB pending-change cap (SPEC W7); `glob` reports current matches, like
  other transient inputs.
- **N1 → DF4d (lock bootstrap).** The store-wide lock file exists before any
  reserve is published: the reserve is created only after the lock file has
  been created successfully. If the lock file is missing and cannot be
  created, no rescue is attempted: the write reports `disk_full`, the reserve
  is untouched. (Changes the test that expected a rescue in that case.)
- **N2 → DF4e (rescue deadline).** A rescue waits for the store-wide lock at
  most `min(2 s, remaining call budget)`; on timeout it reports `disk_full`,
  leaves the reserve untouched and does not retry. Test: lock held beyond the
  deadline → envelope returned, reserve intact, one attempt.
- **N3.** The cleanup-precedence test injects different errnos for the write
  and the cleanup and asserts the returned error is the write's.

## Amendment after tier-2 review (orchestrator, 2026-11-10)

- **DF1a revised (T1).** When the returned envelope is pending (no events, no
  terminal status) and its save failed, the call is held for
  `min(interval_s, budget left)` without re-ticking, then returns. Envelopes
  with events or a terminal status still return at once. The same pacing
  serves MCP, CLI `keepalive` and `wait` (one shared path), so a disk that
  stays full never turns into back-to-back agent turns.
