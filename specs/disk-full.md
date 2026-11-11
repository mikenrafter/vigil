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
