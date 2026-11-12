# vigil — spec

An MCP server that keeps an agent's turn open while a cheap model watches
things for it. The agent says what to watch and for how long. After that it
calls one tool, `keepalive`, over and over. Each call returns the tick count,
elapsed time, what changed, and what the watcher decided. The big model
spends almost no inference while it waits, and its prompt cache stays warm.

Written in Rust. Status: spec reviewed (round 1); phase 0 research in
`FINDINGS.md`, its live-client checks still open (V28); not built.

## Why this exists

Research (see an earlier design discussion) found parts but no whole:

- wait-style MCP servers (mcp-await, herdr-mcp, mcp-timer) block until a
  fixed condition or timeout, with no judgement about relevance;
- time MCPs (at-mcp, temporal-mcp) count elapsed time but cannot wake or hold
  an agent;
- Claude Code's Monitor tool and OpenClaw's heartbeat wake an agent, but each
  wake is a full-model turn and is tied to one harness.

vigil joins them: wait primitives, a tick/elapsed counter, a cheap judge
(Laya, a typed-decision service), and a delivery path that does not
need the agent to re-arm anything.

The manual `manual-keepalive` skill does about half of this. vigil replaces
the `sleep 270 && echo keepalive` command with a tool whose result carries the
context the agent expects on wake.

## Non-goals

- Not a general scheduler, and no interaction with usagewindow or `uw-agent`.
  A long watch is just many short `keepalive` calls; vigil never resumes a
  session.
- Targets Claude Code and Codex only.
- No new model. The judge is the existing `laya` flakelet on `127.0.0.1:4010`.
- No config UI. A watch is one JSON object.
- No daemon, no database, no background sensing. Inputs are read only while
  a `keepalive` call is running. Between calls nothing watches; that is
  acceptable because the agent calls `keepalive` again at once.
- Never writes inside the working tree or any repository. All files live
  under `$XDG_STATE_HOME/vigil/`, with one exception: the advisories file
  under `$XDG_CONFIG_HOME` (see "Advisories"), which only `vigil advisory` and
  the first-use seed write.

## Terms

- **watch**: one sensing job. Has a prompt for the judge, a list of inputs, an
  interval, a deadline, and a fire rule.
- **tick**: one evaluation round of a watch (read inputs, maybe ask the judge).
- **keepalive**: the agent-facing blocking tool call. One call can span many
  ticks. It is the thing the agent repeats.
- **judge**: the cheap model. A POST to Laya's `/v1/predict`. (Not "Jev":
  Laya's README uses that name for a competing model.)

## Architecture

```
agent harness ──stdio MCP──▶ vigil mcp ──▶ judge (laya :4010)
                               │
                               ├─ watch files ($XDG_STATE_HOME/vigil/watches/<id>.json)
                               └─ input readers (file, glob, cmd, url, port, process)
```

One process, one binary, no daemon. Stateless here means the process keeps
nothing that matters in memory: every fact needed to resume a watch is in its
watch file, rewritten at the end of each tick. A killed or restarted `vigil
mcp` (new session, harness restart) picks up where the file says.

- `vigil mcp`: stdio MCP server. Runs ticks only inside a `keepalive` call,
  in-process.
- `vigil` CLI subcommands (`create`, `status`, `list`, `cancel`, `keepalive`,
  `wait`, `advice`, `advisory`) run the same code against the same files. Programmatic monitors, with no
  agent involved, use these; `vigil keepalive` blocks in the shell the same
  way.

Crates: `rmcp` (official Rust MCP SDK), `tokio`, `serde`, `serde_json`,
`reqwest` (judge and URL inputs), plus a file-lock crate. Pin versions in the
spike (phase 0).

## Delivery: how the agent gets context without re-arming

Path 1 is required. Channel push (an MCP server starting a turn on its
own) is out of scope: Claude Code loads custom channel servers only behind
`--dangerously-load-development-channels`, drops refused events silently,
and Codex has no equivalent (`FINDINGS.md` §2).

1. **Blocking `keepalive` result (required).** The call ticks in-process
   up to `max_s` or until the watch fires, then returns the envelope below.
   The envelope ends with a `next` field telling the agent exactly what to do
   (`"call keepalive again"` or `"act on events"`). Server `instructions`
   (sent at MCP initialize, so they land in the system prompt) say the same,
   so the agent needs no skill to behave correctly. This is the
   "keepalive keepalive keepalive" loop; the agent never writes a monitor
   command.
2. **Progress notifications (best effort).** While blocked, emit MCP
   `notifications/progress` with the tick count. Clients that honour
   `resetTimeoutOnProgress` stay alive longer. Claude Code's behaviour here
   is disputed in the sources found, so never depend on it.
3. **`vigil wait` (Claude Code, no re-trigger).** `vigil wait <id>` blocks
   with no 270 s ceiling until the watch fires, expires or is cancelled,
   prints the final envelope and exits (0 fired, 3 expired, 4 cancelled,
   5 attention, 2 usage, 1 error). Exit 5 means an `attention` or
   `credit_state_changed` event ended the wait while the watch is still live;
   `--orchestrator` gives `wait` the orchestrator's window and credit checks
   (see "Cache-aware window, time and early attention"). The agent runs it
   once as a background command; Claude Code wakes the agent when it exits.
   Trade-off: no model calls while waiting, so the prompt cache goes cold
   after its TTL. Use `keepalive` to stay warm, `wait` for long waits where
   a cold cache is fine. Codex has no background wake, so it uses
   `keepalive` only.

### Interval and long deadlines

The agent stays "in cache" only if the gap between its requests is under the
prompt-cache TTL, so every `keepalive` call has a fixed window:

- The window is chosen by vigil, not by the agent: **270 s** by default, and
  up to **3300 s** for the orchestrator on a 1-hour prompt cache (see
  "Cache-aware window, time and early attention"). An explicit `max_s` still
  wins, clamped to 30..=3300 (30..=270 for Codex and for an ambiguous
  caller). Values outside the range are clamped, not rejected, and the
  envelope reports the value used (`max_s`, `waited_s`).
- A deadline of any length (hours, days) needs no special path. Each call
  checks the wall clock against the deadline: has it passed, yes or no. If
  not, the call returns `pending` after at most 270 s and the agent calls
  again. The tool keeps firing, so the agent stays warm for as long as the
  watch lives.
- Between calls the only inference is the one tool-call round trip.

## MCP tools

All tools return a JSON text block (the envelope or a subset) and set
`isError` only for real failures. A timeout is a normal result.

### `watch_create`

```json
{
  "id": "optional-stable-id",
  "prompt": "Tell me if the build log shows a failure or finishes.",
  "inputs": [
    {"kind": "file",    "path": "/tmp/build.log", "tail_bytes": 4096},
    {"kind": "glob",    "pattern": "result*/", "report": "created"},
    {"kind": "cmd",     "argv": ["systemctl", "is-active", "foo"], "timeout_s": 5},
    {"kind": "url",     "url": "http://localhost:8080/health"},
    {"kind": "port",    "host": "127.0.0.1", "port": 5432, "want": "open"},
    {"kind": "process", "pid": 1234, "want": "exited"}
  ],
  "interval_s": 20,
  "deadline_s": 3600,
  "fire": "judge",
  "judge_questions": {"failed": {"type": "noul", "instructions": "Did the build fail?"}},
  "judge_rule": {"question": "failed", "noul_gte": 0.8},
  "expect_s": 1200,
  "stall_s": 300,
  "orchestrator": true
}
```

- `expect_s`, `stall_s` (both 1..=604800, the longest a watch can live: a
  larger limit could never fire and is refused naming the field) and
  `orchestrator` are optional; see "Cache-aware window, time and early
  attention".
- `fire` is `"judge"` (fire when `judge_rule` matches the judge's answer),
  `"change"` (fire on any input change, no model call), or `"condition"`
  (fire when every input that has a `want` meets it, no model call).
- `judge_questions` is passed to Laya unchanged, in Laya's own schema
  (`FINDINGS.md` §1): an object keyed by question id, types `noul`, `choice`,
  `score`. vigil checks only that it is a non-empty object and that the
  rule's `question` is a key in it.
- `judge_rule` is exactly one of `{"question", "noul_gte": p}` (p in 0..1),
  `{"question", "choice_in": [labels]}`, `{"question", "score_gte": n}`,
  plus optional `"min_confidence"` (default 0). The rule's kind must match
  the question's `type`; a mismatch is an error at `watch_create`.
- `judge_questions` and `judge_rule` are required with `fire = "judge"` and
  rejected otherwise. `condition` requires at least one input with `want`.
- Inputs are the wait primitives from mcp-await (file, URL, port, process),
  re-implemented in vigil, plus `glob` and `cmd`. vigil does not
  proxy to a separate mcp-await server: an MCP server cannot easily call
  another one, and the primitives are about 200 lines.
- Writes the watch file and returns
  `{watch_id, status: "pending", created_at, deadline_at}`. No tick runs
  until the first `keepalive`.
- An `id` that already exists for a pending watch is an error. Ids are
  `[A-Za-z0-9_-]{1,64}`; a generated id is used when absent.

### `keepalive`

Input: `{watch_id?: string, max_s?: number, orchestrator?: boolean}`. With no
`watch_id`, it covers the pending watches this `vigil mcp` process created.
Watches from another session (or from before a restart) need their id. Leave
`max_s` out: vigil chooses the window. `orchestrator: true` is for the main
session only.

Blocks until any covered watch fires, expires or needs attention, or the
window (270 s by default, see below) elapses. Ticks run in this call, each when the watch's
`last_tick_at + interval_s` comes due, so the cadence holds across calls and
an immediate re-call does not cause an extra tick.

### `watch_status`, `watch_list`, `watch_cancel`

Non-blocking reads and cancel. `watch_status` also reports judge reachability
(a `/health` GET with a 2 s timeout).

## Envelope (the injected context)

```json
{
  "status": "pending | fired | expired | cancelled",
  "now": "2026-10-29T14:03:11Z",
  "tick": 17,
  "session_ticks": 41,
  "elapsed_s": 340,
  "deadline_in_s": 3260,
  "waited_s": 240,
  "events": [
    {"tick": 17, "watch_id": "build", "input": "file:/tmp/build.log",
     "summary": "ERROR: linking failed", "judge": {"failed": {"type": "noul", "noul": 0.93, "confidence": 0.93}}}
  ],
  "digest": "tail of /tmp/build.log since last call: ...",
  "judge_status": "ok | down | skipped",
  "next": "act on events"
}
```

- `tick` counts rounds of this watch; `session_ticks` counts `keepalive`
  returns for this `vigil mcp` process (in memory; it restarts at 0 with the
  process). The agent can quote either.
- `now` is the system clock (UTC). This is the "legibility of time" piece: the agent
  sees real time on every return.
- `digest` is the file/command reading the agent would otherwise do itself,
  already trimmed to what is new since the last return. Bounded by
  `VIGIL_DIGEST_MAX_BYTES` (default 8192). If cut, say so.
- `events` is empty on a pure timeout. Then `status` is `pending`, `next` is
  `"call keepalive again"`, and the envelope is short, so a quiet hour costs
  a few hundred tokens per tick, not thousands.
- `cache` and `clock` ride on every blocking return (a `keepalive`, and the
  final envelope of a `wait`), never on status or cancel:
  `"cache": {"ttl": "1h"|"5m"|"unknown", "source": "claude-transcript"|"codex-default"|"default"|"explicit", "interval_s": 270}`
  (plus `credit_check` after an orchestrator's credit check) and
  `"clock": {"waited_s", "watch_age_s", "since_last_call_s" (null on the first call), "summary"}`.
  `summary` is one plain line, e.g. `waited 55m 0s this call; watching for
  2h 10m; last call 55m ago`.
- `advice` rides on every blocking return (with `cache` and `clock`) when
  advisories are on: a list of `{id, kind: "advice"|"unavailable", message,
  checked_at, value?}`; see "Advisories". Absent when advisories are off or on
  status and cancel; `[]` when on with none configured.
- An event with a `kind` key is not about an input: `attention` (with
  `reason` `overdue` | `stalled`) or `credit_state_changed` (`on_credits`,
  `source`, `windows`, optional `hook`). Either one ends the call with
  `status: "pending"` and `next: "act on events"`.
- `persist` and `persist_failures` appear only when a state write failed or had
  to spend the reserve (see "Surviving a full disk"); an event carries
  `"redelivered": true` only when it follows a failed save. A healthy call has
  none of these keys.

## Tick logic (inside `keepalive`)

For each covered watch, each time a tick comes due:

1. Read all inputs. Hash each result.
2. If nothing changed since the last tick, stop. **No judge call.** Most ticks
   end here, which is where the cost saving comes from.
3. If `fire` is `change` or `condition`, decide deterministically and stop.
4. Otherwise send the judge `{state, questions: judge_questions}` at
   `POST $VIGIL_JUDGE_URL/v1/predict` (default `http://127.0.0.1:4010`).
   `state` is the prompt, then the changed inputs' new content with the
   newest lines first, cut to `VIGIL_JUDGE_STATE_BYTES` (default 2560) on a
   UTF-8 boundary. Laya keeps only the start of `state`, so newest-first puts
   the lines that matter where it reads. Record the answers.
5. Fire if the rule matches: append an event, set `status: "fired"`, and
   return from `keepalive`.
6. Rewrite the watch file (hashes, tick count, `last_tick_at`, file
   offsets, judge strike count, events).

A fired watch is finished. To keep watching, the agent creates a new watch.

Failure rules:

- Judge down, timed out (default 10 s), non-2xx, or an answer missing the
  rule's question: record `judge_status: "down"`, **do not fire and do not
  drop the change**. Re-ask next tick with the same pending change, even if
  inputs did not change again (a pending change counts as a change). After 3
  consecutive failures, fire with `events[].summary = "judge unavailable; raw
  change attached"` and `events[].judge` absent, so a human-grade model
  decides. Silent failure is the worst
  outcome here.
- Input read error (missing file, refused port): report it as a change with
  the error text; it is not a crash.
- Deadline: checked against the wall clock at each tick and at each
  `keepalive` entry and exit. Once passed, `status: "expired"` with the last
  digest, so the agent knows the final state. Because the deadline is an
  absolute time in the watch file, a watch whose deadline passed while no
  `keepalive` ran reports `expired` on the next call.

## State

Files only, under `$XDG_STATE_HOME/vigil/` (default `~/.local/state/vigil/`).
Nothing is written to the working tree or any repository, and nothing to
`$XDG_CONFIG_HOME` except the advisories file (see "Advisories").

- `watches/<id>.json`: the watch as created (paths already absolute, see
  V11), plus `status`, `created_at`, `deadline_at` (absolute UTC),
  `finished_at`, `tick`, `last_tick_at`, per-input observation state (V9),
  `judge_strikes`, the pending change (V14, R7), `last_digest`, and the
  terminal events (in front of them, any attention that was never
  acknowledged when the watch finished, flagged `redelivered`).
- Also under the root: `credit.lock` (an `flock` held around a credit probe,
  its hook and the commit of the state file) and `credit-state.json`
  (`{"on_credits": true|false|null, "checked_at": ts}`: the last known credit
  state and the time of the newest check, which paces the checks of every
  process). While a watch is pending its file may hold, in `events`, attention
  events not yet acknowledged, plus `attention_raised`, `activity` and
  `last_call_at`.
- Writes are atomic: write `<id>.json.tmp` in the same directory, fsync,
  rename. A failed write is survived, see "Surviving a full disk" below.
- **One lock for everything** (V4, V5): every mutation (create, tick,
  cancel, prune) holds an exclusive `flock` on `$XDG_STATE_HOME/vigil/lock`
  for its read-modify-write, and re-reads the watch after taking it. The lock
  file is never deleted. Waiting for it counts against the call budget (V2).
- Terminal states (fired, expired, cancelled) are immutable. The first one
  written under the lock wins; a later cancel or tick on a finished watch
  returns the stored result unchanged.
- Finished watches are deleted 7 days after `finished_at`. A pending watch
  past its deadline counts as expired with `finished_at = deadline_at` (V16).
- Elapsed time survives restarts for free: `created_at` and `deadline_at` are
  in the file.

### Surviving a full disk

Full contract and decisions: `specs/disk-full.md` (claims DF1-DF7 and the
amendments S1-S6, N1-N3); proof: `src/disk_full_tests.rs`, the `store.rs` and
`fault.rs` tests, `tests/cli_e2e.rs`, `tests/mcp_e2e.rs`. The intent: a state
write that fails must never cost the agent the tick's answer, and a full disk
must be named as such.

- **The answer survives (DF1).** If sensing works and the watch-file write
  fails, `keepalive` (MCP and CLI) still returns the full envelope for the tick,
  built from memory: the same `events`, `digest`, `status`, `next` as a
  successful tick. The CLI exits as it would have (`keepalive` 0; `wait` keeps
  its outcome code). A tick whose save failed stops the call's sensing and is
  answered as is (DF1a): no in-memory re-ticking, so a full disk is not hammered.
  An envelope with events or a terminal status is returned at once. A pending
  one (nothing to act on) is first held for `min(interval_s, budget left)`
  without ticking again, so a disk that stays full never turns into back-to-back
  agent turns; the hold is part of the call (cancellable, counted in
  `waited_s`) and the same for the MCP server, CLI `keepalive` and `wait`. It
  uses the shortest `interval_s` of the watches whose save failed.
- **The failure is visible (DF2).** The envelope carries
  `"persist": {"ok": false, "kind": "disk_full" | "io", "error": "<OS text>", "path": "<file>"}`.
  `kind` is `disk_full` for ENOSPC (28) and EDQUOT (122), `io` otherwise.
  `"redelivery": true` is added only when something is at risk: the envelope has
  events or a terminal status (fired, expired, cancelled) that was not saved
  (DF3a). `"first_error"` is added whenever a retry was made and failed, even if
  both errors match; `error` and `kind` are then the retry's (S4). With several
  covered watches, `persist` describes the watch whose envelope is returned and
  `"persist_failures": [{"watch_id", "kind", "error", "path"}]` lists every other
  covered watch whose save failed in that call (absent when empty, S5). A
  healthy write has no `persist` key. The CLI writes one line to stderr naming
  the state root for each failed save.
- **At least once, best effort (DF3, S1).** A failed save does not advance the
  persisted baseline, so the next tick senses the change again, from the last
  saved baseline (DF3b: the unsaved changes plus anything newer). The call's own
  answer is the guarantee; redelivery is best effort:
  - `file` appends are redelivered (the newest `tail_bytes` of the appends since
    the saved offset, within the 64 KiB capture cap of V12 and W7: older bytes
    are skipped and `[truncated]` is said);
  - `glob` reports its CURRENT matches, like the transient inputs `cmd`, `url`,
    `port` and `process`, which report their state at the next sensing and may
    not repeat a change that has reverted;
  - an expiry or a cancellation saved by another process wins over redelivery.
  A result that could not be saved is not marked returned (V7), so the next
  no-id call covers it again. `redelivered: true` is set on every event of a tick
  that follows a failed save, by the process that failed to save (the MCP
  server, or `vigil wait`); a fresh CLI process cannot know, so its events are
  unflagged. Agents must not deduplicate by `event_id` across a persist failure
  (the tick number did not advance, so ids can repeat with other content) and
  treat any event after an envelope with `persist.ok = false` as possibly
  repeated. `watch_status` may lag: after a failed save it can still say
  `pending` while `keepalive` answered `fired`.
- **The reserve (DF4).** The store keeps `<root>/.reserve`: 256 KiB (262144
  bytes) of real zeros, fsynced, never kept open. It is valid only as a regular
  file of exactly that length with at least that much allocated
  (`st_blocks * 512`), so a short, sparse or non-regular reserve is replaced
  (DF4b). After any successful watch write that finds it invalid or absent it is
  (re)made, best effort: failing to make or delete it is never an error.
  A write that fails with `disk_full` while the file exists causes the store to
  delete it and retry the same write once. If the retry succeeds the tick is
  persisted and the envelope carries
  `"persist": {"ok": true, "used_reserve": true, "kind": "disk_full"}`. With no
  reserve file there is no retry (DF4a). The rescue is best effort: a write
  larger than the reserve, inode exhaustion, quotas, compression or a different
  filesystem can still fail, and then DF1 and DF2 apply. On a compressing
  filesystem (ZFS with compression, btrfs with `compress`) the zeros take no
  blocks, so the reserve never validates and every healthy save rewrites it
  (cost: the store-wide lock, a 256 KiB write and an fsync per save); it also
  protects nothing there, because freeing it frees no space.
- **Locks (DF4c-DF4e).** Reserve creation, deletion and the retry run under the
  store-wide lock `<root>/lock`, taken after any watch lock (lock order is
  unchanged). A caller that holds it (`create`) reuses it. The lock FILE exists
  before any reserve is published; a missing store-wide lock file that cannot be
  created allows no rescue and publishes no reserve (DF4d). A rescue waits for
  the lock at most `min(2 s, remaining call budget)`; on timeout it reports the
  first failure, leaves the reserve untouched and does not retry (DF4e).
- **Every write, one path (DF5).** Watch create, tick save, cancel and lock-file
  creation all use the reserve-and-retry path. A failure that survives it is an
  error whose message names the state root and contains the words `disk full`
  (`watch_create`, and any call that cannot even take its lock), except a tick
  or a cancel, whose answer is returned with `persist.ok = false` (DF5a). A
  `create` whose baseline cannot be saved removes its placeholder, so the same
  id can be used again (DF7).
- **Atomicity (DF6).** The rename of `<id>.json.tmp` over `<id>.json` is the
  commit point: on any failure before it the previous file is intact and the
  attempt's own `.tmp` is removed (best effort; the write's error, never the
  cleanup's, is the one reported, N3). A leftover `.tmp` is overwritten by that
  watch's next write; other `*.tmp` files are never swept, because another
  process may be mid-write. A crash between deleting the reserve and the retry
  leaves no reserve; the next successful write makes one.
- **Test seam (S6).** `VIGIL_TEST_FAULT` makes the store's writes fail on
  purpose (`KIND[@STAGE]:ERRNO:WHEN[:FILE]`, grammar in `src/fault.rs`). It is
  for vigil's own tests and assumes a trusted process environment: whoever can
  set it can disable writes, inheriting it does so too, and a malformed value
  stops vigil from starting (it is never read as "no faults"). `main.rs` reads
  it at the edge; unset it changes nothing.

## Open questions (resolve in phase 0)

1. **Laya request schema.** The flakelet shows `POST /v1/predict` with
   `{state, questions}` but not the question types or result shape. Read the
   Laya SDK and fix `judge_questions` to match before phase 2 (done: `FINDINGS.md` §1).
2. **Channels.** Out of scope since the stateless design; findings kept for
   reference only.
3. **Blocking limit.** Confirm a 270 s blocking call completes in both Claude
   Code and Codex over stdio, with and without progress notifications. If
   either client gives up earlier, record the real limit; the 270 s ceiling
   stays locked unless that finding forces a change, which is then the user's
   decision.
4. **Codex.** Confirm the same tools and `instructions` text work in Codex's
   MCP client.

## Packaging

Scout module at ``, following `modules/apk-builder/`:

- `scout` facet: the `vigil` binary and an agent skill (`skills/vigil/SKILL.md`)
  to every host.
- No `flakelet` and no service: `vigil mcp` is started by the harness. The
  `laya` flakelet is an optional runtime dependency (default
  `127.0.0.1:4010`, override with `VIGIL_JUDGE_URL`); where it is unreachable,
  `fire = "judge"` returns an error at `watch_create` instead of failing later.
- Wire the MCP server into the agent configs through `agent-integration`:
  - Codex: `[mcp_servers.vigil]` with `tool_timeout_sec = 330`. Codex's
    docs say the default is 60 s, its source says 300 s, and it is a hard
    limit that progress does not reset (`FINDINGS.md` §3).
  - Claude Code: set `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS=280000` in the
    settings `env`. Otherwise any call over 2 minutes is moved to a
    background task and a 270 s `keepalive` costs two model round trips.
- Server `instructions`: under 512 characters, so neither client truncates
  them (Codex reads the first 512, Claude Code cuts at 4,096). They must say:
  create a watch, then call `keepalive` repeatedly until `next` says
  "act on events".
- Per `AGENTS.md`: build and evaluate only the host you are on. On host-a,
  use `nix eval .#nixosConfigurations.host-a.config.system.build.toplevel.drvPath`,
  not `nix flake check`. Cargo builds on host-a or host-b (8 GB RAM) need
  `low-ram-hosts`; use `-j2`.

## Build plan (for build-loop)

Each phase ends with passing tests. Tests that need the judge use a fake HTTP
server that returns canned answers.

0. **Spike (no production code).** Answer the four open questions above.
   Output: a short `FINDINGS.md` and pinned crate versions.
1. **Core.** Watch files, input readers, tick logic, deterministic fire
   rules, locking, pruning. CLI: `create`, `status`, `list`, `cancel`,
   `keepalive`. Tests: file/glob/cmd/url/port/process readers; unchanged
   input makes no model call (assert on the fake judge's request count);
   deadline expiry; a new process keeps the original deadline and tick count;
   two concurrent ticks on one watch produce one tick; nothing is written
   outside `$XDG_STATE_HOME/vigil/`.
2. **Judge.** `fire = "judge"`, trimming of `state`, failure rules (down,
   timeout, three strikes). Tests: each failure path, including that a
   change is never dropped.
3. **MCP front and `keepalive`.** stdio server, the five tools, envelope,
   `instructions`, `max_s` clamp to 30-3300 (originally 30-270), progress notifications. Tests: an MCP client
   in the test drives create, then repeated keepalive; blocked call returns
   early on fire; `max_s` of 5 and 9999 are clamped to 30 and 3300 (270 for Codex; originally 270, see "Cache-aware window"); a deadline hours away returns `pending` at 270 s (the default window); pending returns carry `tick` and `now`; quiet envelope is
   small.
4. **Nix packaging and skill.** Scout module, flakelet, agent skill, MCP
   wiring. Verify per `AGENTS.md`: the affected output evaluates, then
   `nix flake check --no-build` (or the host-a-safe eval).

## Acceptance

- An agent creates a watch on a growing log, then calls only `keepalive`.
  It sees ticks counting up with real timestamps, no tool calls other than
  `keepalive`, and receives the failure line plus the judge's verdict when it
  appears, or an `expired` envelope with the final digest at the deadline.
- 100 quiet ticks produce zero judge requests.
- Killing `vigil mcp` mid-watch and calling `keepalive` with the watch id
  from a new process does not reset elapsed time or the tick count.
- With the judge stopped, a change still reaches the agent within three ticks and
  is labelled as unjudged.
- Live run on host-b: a cheap Codex agent (GPT Luna, low effort) watches a
  few long-running commands for about an hour using only `keepalive`. host-b
  has no Laya, so this run uses `change` and `condition`; the judge path is
  covered by the fake-judge tests and a separate live check on host-c.

## Defaults taken (raise at next check-in)

- D1: a fired watch is finished; the agent creates a new one to continue.
- D2: `keepalive` with no id covers only watches this process created.
- D3: ticks keep the watch's cadence across calls (`last_tick_at + interval_s`).
- D4: finished watch files are deleted 7 days after they finish.
- D5: judge URL from `VIGIL_JUDGE_URL`, default `http://127.0.0.1:4010`.
- D6: Claude Code gets `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS=280000` through
  agent-integration, so `keepalive` stays one round trip.
- D7: URL inputs are http only (no TLS stack, to avoid aws-lc-rs on 8 GB hosts).
- D11: `vigil wait` added (delivery path 3) so Claude Code agents can wait with no re-trigger.
- D8: the live hour-long test uses `change`/`condition`; Laya stays on host-c.

## Resolved details (from the red phase, 2026-10-29)

- R1 baseline: the first tick is due at once, counts as tick 1, and only
  records hashes; it is never a change. `fire = "condition"` is evaluated on
  every tick, including the first, changed or not (no model cost).
- R2 `want`: only `port` (`open`/`closed`) and `process` (`running`/`exited`)
  inputs take `want`.
- R3 readings: `cmd` reads `exit=<code>\n<output>`; `url` reads
  `HTTP <status>\n<body>`; `file` `tail_bytes` defaults to 4096; `timeout_s`
  defaults to 10. A repeated identical read error is not a new change.
- R4 keepalive with no id and no watches created by this process: a tool
  error saying to pass `watch_id` or create a watch. Covering several
  watches: one envelope whose watch fields (`status`, `tick`, `elapsed_s`,
  `deadline_in_s`) describe the watch that ended the call, else the one with
  the nearest deadline; `events` holds new events from all covered watches.
- R5 `next`: `"call keepalive again"` (pending), `"act on events"` (fired),
  `"watch ended; create a new watch if still needed"` (expired, cancelled).
  Expiry runs no final tick and adds no event; `digest` is the last digest.
- R6 digest cut keeps the newest end and appends `[digest cut]`.
  `watch_status` does not mark events delivered.
- R7 judge down across several changes: the pending change accumulates; the
  next request carries all content changed since the last judged tick,
  newest first, cut to `VIGIL_JUDGE_STATE_BYTES`.
- R8 `vigil wait` has no `--stream`: under D1 a watch fires once, so a stream
  would only ever print one event. Removed.

## Spec review round 1 (Sol, 2026-10-29)

These override anything above that disagrees.

- V1 (was R1): the baseline is recorded at `watch_create`, not at the first
  tick. A change between create and the first `keepalive` is a change.
- V2 one call budget: a `keepalive` call ends by `max_s` measured on a
  monotonic clock. Lock wait, every input read (`cmd`/`url`/`port` timeouts)
  and the judge request (10 s) are each capped at the budget left. A tick that
  cannot finish inside the budget writes nothing, so its change is not
  consumed; the next call sees it again.
- V3 (replaces `delivered_through`): under D1 the only events are terminal.
  The terminal result is stored once and replayed unchanged on every later
  `keepalive`, `wait` or `watch_status` for that id. Events carry a stable
  `event_id` (`<watch_id>:<tick>:<n>`, W1). Delivery is at least once.
- V4/V5: see State (one global lock, never deleted; terminal immutable).
- V6: any id that still has a file (pending or finished) cannot be reused.
- V7/V2-scope: a no-id `keepalive` covers this process's pending watches plus
  its finished watches whose terminal result this process has not yet
  returned (an in-memory set, so a restart may replay once).
- V8: the envelope has `watch_id`. Its watch fields all describe that watch:
  the earliest-finished covered watch if any finished, else the one with the
  nearest deadline (ties: lowest id).
- V9 file changes are by position, not hash: the file input stores
  `(dev, inode, size, mtime, offset)`. New bytes past `offset` are a change
  (two identical appends are two changes). A different `(dev, inode)`, a
  smaller size, or the same size with a new mtime means the file was replaced
  or rewritten: reset `offset` to 0 and treat the last `tail_bytes` as new.
  Other inputs keep hashing their reading.
- V11 relative paths, glob patterns and `cmd` working directory are resolved
  against the creating process's cwd at `watch_create` and stored absolute.
- V12 capture limits: each input reads at most 64 KiB per tick (`cmd`
  stdout+stderr combined, `url` body, `file` new bytes; the excess is
  skipped and the reading says `[truncated]`); a glob reports at most 1000
  matches; a watch has at most 16 inputs; `prompt` at most 1024 bytes.
- V13 `cmd` runs in its own process group (setsid). On timeout, or when the
  call is cancelled or dropped, the whole group gets SIGKILL and is reaped.
- V14 digests: a pending envelope's `digest` is the content that changed
  during this call (accumulated in memory, cut to `VIGIL_DIGEST_MAX_BYTES`),
  empty if nothing changed. A terminal envelope's `digest` is `last_digest`.
  Quiet returns never repeat earlier content.
- V15 judge state: prompt first (at most 1024 bytes, V12), then the
  observation budget (`VIGIL_JUDGE_STATE_BYTES` minus the prompt, at least
  1536) split evenly across the changed inputs, each newest lines first.
- V16: see State (overdue pending watches expire for pruning).
- V17 process input: `watch_create` records the start time from
  `/proc/<pid>/stat`. `exited` = pid gone, zombie (`Z`), or start time
  differs (pid reused).
- V18 MCP progress uses a call-local counter that only increases; per-watch
  tick counts go in the progress message.
- V19 create-time validation (errors, not clamps): judge probabilities and
  `min_confidence` in 0..=1; `deadline_s` 1..=604800 (7 days);
  `interval_s` clamped 1..=3600 (D10); `cmd.argv` non-empty;
  `url` scheme http only (https and redirects to https are errors, D7);
  `timeout_s` 0.1..=60.
- V20 `next` has three exact wire values: `"call keepalive again"`,
  `"act on events"`, `"watch ended; create a new watch if still needed"`.
  Server instructions say: stop calling when `next` is not
  `"call keepalive again"`.
- V21: R8 applies; `--stream` tests are removed.
- V26 D9: the envelope has `max_s` (the clamped ceiling used) and `waited_s`
  (time actually waited). D10: `interval_s` default 20, clamped 1..=3600.
- V27: `VIGIL_DIGEST_MAX_BYTES` bounds the whole digest including the
  `[digest cut]` marker.
- V28: live-client checks (FINDINGS "Still unverified") are an acceptance
  gate, run on host-b before calling the clients compatible.

## Red-phase follow-ups (after round 1)

- W1 `event_id` is always `<watch_id>:<tick>:<n>`, n counting from 0 within
  the tick, so two inputs changing in one tick get distinct ids.
- W2 `VIGIL_JUDGE_STATE_BYTES` is clamped to at least 2560 (1024 prompt +
  1536 observations), so total judge state never exceeds the setting.
- W3 `VIGIL_DIGEST_MAX_BYTES` is clamped to at least 64, so the
  `[digest cut]` marker always fits.
- W4 when a capture limit cuts a file's new bytes, the oldest are skipped
  and the newest kept; the offset moves to the end.
- W5 a cancel that arrives after `deadline_at` finds the watch expired
  (`finished_at = deadline_at`) and returns that.
- W6 `vigil wait` exit codes: 0 fired, 1 error, 2 usage (clap), 3 expired,
  4 cancelled.
- W7 the persisted pending change is capped at 64 KiB per input, newest
  kept, with `[truncated]` when cut.

## Spec review round 2 (Sol) — final decisions

Round 2 was the last spec round; these are decisions, not open questions.

- X1 (S8) a no-id envelope also lists every other covered watch that
  finished in this return as `others: [{watch_id, status, event_ids}]`, so
  an expired or cancelled secondary watch is visible. Their events are in
  `events`; their digests are not repeated.
- X2 (S12) bounds: persisted glob membership at most 1000 paths; an event
  `summary` at most 200 bytes; so an envelope is at most digest limit +
  16 inputs × 200 bytes per covered watch, plus fixed fields.
- X3 (T4, supersedes the single-lock part of V4/V5) locks:
  - `locks/<id>.lock` per watch, held across a tick's sensing and write and
    across cancel. Lock files are never deleted while their watch file
    exists; prune deletes a lock file only after deleting its watch file,
    both while holding that watch's lock and the global lock.
  - `$XDG_STATE_HOME/vigil/lock` (global) is held only for short namespace
    operations: reserving an id at create, and prune. Never during sensing.
  - create: take the watch lock, then the global lock to check the id and
    write a placeholder, release the global lock, then read the baseline
    under the watch lock with each input's timeout.
- X4 (T5) no livelock: `timeout_s` is 0.1..=20 (supersedes 60 in V19);
  inputs of one tick are read concurrently; a tick starts only if the call
  has at least `max(input timeouts) + judge timeout + 1 s` left, so a tick
  due at the start of any call (budget >= 30 s) always finishes. A due tick
  runs first in each call.
- X5 (S24, S25) test rules: the confinement test compares exact bytes of
  everything outside `state-home/vigil`; the cross-process test counts real
  evaluations (a `cmd` input that appends to a counter file) and expects 1.
- X6 (T1-T3, T6, T7) the red tests follow W1 (`:0` suffix), W6 exit codes,
  await MCP progress asynchronously, and test the W2/W3 floors and the W7
  cap through the engine.

## Green-gate fixes (from the final red run)

- X7 (X4 arithmetic): `timeout_s` is 0.1..=19, so the worst tick reserve is
  19 + 10 (judge) + 1 = 30 s and fits the smallest call. Supersedes 20 in X4.
  The tick reserve counts the judge timeout only for `fire = "judge"`;
  `file`, `glob`, `process` inputs count 0, `url`/`port` count their timeout.
- X8 (lock inode split): after taking a watch lock, compare the locked fd's
  `(dev, inode)` with the lock path's; if they differ or the path is gone,
  release and retry (bounded by the call budget).

## Cache-aware window, time and early attention

Full claims and decisions: `specs/auto-interval.md` (AI1-AI8, S1-S10, N1-N4).
This section is the behaviour as built.

**Why.** An agent should not pick a tick length. A Claude main session with a
1-hour prompt cache needs a call only every ~55 minutes; a 270 s tick costs
about 11 times as many model calls. A long wait hides time, so a long window
needs a clock in the envelope and an early wake when a job looks stuck.

**Window (AI1', AI3, S4).** Per call, in this order:
1. An explicit `max_s` / `--max-s` wins, clamped to 30..=3300, or 30..=270
   when the caller is Codex or ambiguous. `cache` is `unknown` / `explicit`.
2. A caller that is not the orchestrator gets 270 s (`unknown` / `default`).
   Subagents therefore need no special instruction.
3. The orchestrator is a call with `orchestrator: true` (MCP), `--orchestrator`
   (CLI), or any covered pending watch created with `orchestrator: true`; an
   explicit `false` overrides the stored flag. Its window comes from the
   session's own transcript (`CLAUDE_CODE_SESSION_ID`,
   `~/.claude/projects/*/<id>.jsonl`, or `VIGIL_CLAUDE_PROJECTS_DIR`, or
   `$CLAUDE_CONFIG_DIR/projects`): the newest main-session assistant record
   with a nonzero `usage.cache_creation` write; `ephemeral_1h_input_tokens > 0`
   gives 1 h and 3300 s, else a 5m write gives 5m and 270 s; nothing usable
   gives `unknown` and 270 s. Codex (`AGENT=codex` or any `CODEX_*` variable)
   is `5m` / `codex-default` / 270 s. Claude and Codex signals together are
   ambiguous (a Claude started from a Codex shell or the reverse): 270 s,
   `unknown`.
4. The window is decided again at every call.

Bounds (S8): only the last 512 KiB of a transcript is read (a seek, at most
512 KiB + 1 byte consumed), lines over 64 KiB are skipped, only regular files
are opened (a symlink or FIFO is never followed or opened), the session id is
restricted to `[A-Za-z0-9_-]{1,64}`.

**Clock (AI4).** `clock.since_last_call_s` is the time since the previous
envelope this watch returned, kept in the watch file as `last_call_at` (best
effort: a failed save of it is silent and never holds or shortens a call).
Finishing a watch keeps the value, so the envelope that reports a fire or an
expiry still says how long it had been since the call before.
`wait` measures the whole wait and records only its own final return.

**Credit check (AI2', AI8, S2, S5, S7, S9).** Every orchestrator call checks
the account's credit state when the last check is at least 240 s old
(`credit-state.json`), after its watches had their first pass, and on every way
out of the call: a call that returns carried attention, an existing terminal
result, a watch that just finished or attention it just raised makes its due
check before it returns (a credit change then rides along as an event beside
the terminal one). A window over 270 s also re-checks every 240 s inside the
call. One check runs under
`credit.lock`: re-read the state file (another process may just have checked),
probe, compare with the recorded KNOWN state, run the hook on a change, commit
`credit-state.json`, release. Probe: `uw --json status` (`VIGIL_UW_COMMAND`;
only `ClaudeCode` windows count; output capped at 64 KiB; own process group),
else any main-session transcript modified in the last 10 minutes (20 newest)
whose newest cache write is 5m after an earlier 1h write. The probe has one
5 s deadline: `uw` may use four fifths of it (4 s), so a hung `uw` still leaves
the transcript fallback its last second. The verdict is
tri-state: on, off, unknown (no source could say). Unknown keeps the last
known state, never makes an event or a hook, and still counts as a check. The
first known verdict is a baseline. A change ends the call with one
`credit_state_changed` event and runs the hook `VIGIL_ON_CREDIT_CHANGE` (an
`sh -c` command, empty means none) with `{on_credits, source, at, windows}` as
one JSON line (at most 4096 bytes, `windows` cut as a prefix) on stdin, a 10 s
timeout, output cut to 2048 bytes. The hook is delivered AT LEAST ONCE: the
state file is written after the hook, so a crash or a failed write makes the
next check run it again; hooks must be idempotent. Probe and hook are bounded
by the call budget minus a 100 ms margin (a check that cannot finish is skipped
and left to the next call), and cancelling a call kills their process groups.

**Attention (AI5, S6).** A watch may declare `expect_s` and `stall_s`. vigil
raises one `attention` event per reason per watch: `overdue` when the watch has
run `min(1.5 x expect_s, expect_s + 540)` seconds (the wake is scheduled for
that moment, however rarely the watch ticks), and `stalled` when no input
changed for `stall_s` while a `process` input still runs (evaluated at ticks).
With neither field set, a window over 270 s applies a default `stall_s` of 900
to changes of `file`, `glob` and `cmd` inputs of a watch that also has a running
`process` input. Attention events are ordinary events: distinct W1 ids
(`<watch>:<tick>:<n>`), saved with the watch before they are delivered,
acknowledged by a committed delivery, and returned again with
`redelivered: true` after a failed save or an uncommitted delivery. A retry
after a failed save is the same event (same id and text) even if a tick landed
in between: the process remembers what it could not save. A finished watch
raises nothing; with several watches the terminal result is the primary and an
attention event of another watch rides along. A watch that finishes (fire,
expiry, cancel) while attention is unacknowledged keeps it, flagged
`redelivered`, in front of its terminal events.

A waiting call looks at the covered watches' outboxes at every pass and sleeps
at most 5 s between passes, so attention another process saved but could not
hand over is returned within about 5 s. Raising attention waits for the
watch's lock only until the call's budget ends and saves nothing after it; an
attention not raised in time is raised by the next call.

**Delivery (N3).** The CLI prints the envelope with a checked write to
descriptor 1 and only then commits the delivery; a failed write (EPIPE,
ENOSPC) exits 1 and leaves the events to be replayed by the next call.

**Environment.** `CLAUDE_CODE_SESSION_ID`, `AGENT`, `CODEX_*` (identity);
`VIGIL_CLAUDE_PROJECTS_DIR`, `CLAUDE_CONFIG_DIR`, `HOME` (transcripts);
`VIGIL_UW_COMMAND`; `VIGIL_ON_CREDIT_CHANGE`. `VIGIL_TEST_FAULT` gains the write
kind `credit` and `hold=PATH` rules (test-only, trusted environment).


## Holds: keep an orchestrator in cache by watching its own work

Claims `H1`-`H12`. Built 2026-11-12 at the owner's request (PLAN W36). Proof:
`src/hold.rs` and `src/watch.rs` (unit), `src/tick.rs` and `src/keepalive.rs`
(engine, virtual time), `tests/cli_e2e.rs` and `tests/mcp_e2e.rs` (binary).

**Why.** An orchestrator holds its prompt cache by always having a vigil call
in flight. A hand-made "hold" (a dummy watch on pid 1, a 24 h deadline) was
cancelled when some other event arrived and not re-armed, and the session fell
out of cache (one lost 1h35). A hold now has the work itself as its inputs, so
it ends exactly when the work ends, and it is one object the agent never has to
re-arm while work is live. Every keepalive also returns `advice` and checks
usage credits, so an orchestrator holds with vigil even when nothing else is
watchable (only a subagent running): `vigil hold` with no work.

**A hold is a watch (H1).** `fire = "hold"` is a fourth fire rule beside
`judge`, `change` and `condition`. Its inputs are *work* inputs; the watch fires
when every one of them has ended. `watch_create` accepts it directly; `vigil
hold` and the MCP tool `hold` are the front doors that build it. Existing
watches (`judge`, `change`, `condition`) read and behave exactly as before
(CLAIM(H12): old watch files, and the three old fire rules, round-trip).

**Work inputs (H2).** New input kinds, each with an implicit "ended" want so
`fire = "condition"` accepts them too:

| front-door spec | input | ended when | reading text |
|---|---|---|---|
| `qex:<job-id>` | `{"kind":"qex","job":ID}` | `qex status ID --json --no-logs` shows a state other than `queued`, `starting`, `running`; or qex no longer knows the job (`gone`) | the state name, `gone`, or `error: ...` |
| `pid:<pid>` | `{"kind":"process","pid":N,"want":"exited"}` | the pid is gone, a zombie, or reused (V17) | `running` / `exited` |
| `file:<path>` | `{"kind":"path_exists","path":P}` | the path exists (a done-marker another program writes) | `missing` / `present` |
| `until:<RFC3339>` | `{"kind":"until","at":T}` | the wall clock is at or past `T` | `waiting` / `reached` |
| `idle:<dir>` | `{"kind":"qex_idle","dir":D}` | no qex job under `D` is `queued`, `starting` or `running` | `idle` / `busy: N active` |

- A read that fails (qex missing, a timeout, unparsable output) reads
  `error: <why>` and is **not ended**: a hold errs towards staying live, because
  the cost of an early release is the lost cache. The one exception is qex's own
  "there is no job" answer, which is `gone` and ended (qex prunes old jobs), so
  a hold does not outlive its job's record. At creation `gone` is refused
  (`qex job X not found`), so a typo is an error at once, not a hold that lives
  7 days (CLAIM(H3)).
- qex states: ended = `completed failed killed timeout expired oom cancelled
  skipped`; live = `queued starting running`. A job that fails and is queued for
  a retry reads live again; the hold is evaluated afresh at every tick.
- Not senseable from here, said plainly: a **T3 task / delegate** (its state is
  in the T3 server, not on this host) and a **`boat exec` run** as a Boat job.
  Hold on the local `boat` process with `pid:<pid>`, or have the work write a
  marker and hold on `file:<path>`. vigil does not guess.
- `qex` is found on `PATH`.

**Hold rules (H4).** Validation at create, errors not clamps:
- every input is a work input (the five above; a `process` input must say
  `want: exited`); `file`, `glob`, `cmd`, `url`, `port` are refused ("a hold
  waits on work, use fire = condition for those");
- `stall_s` is refused (a hold is quiet by definition; the default stall rule
  of "AI5" never applies to a hold, CLAIM(H8)); `expect_s` is allowed and
  raises `overdue` attention as for any watch;
- at most 16 inputs.

**No re-arming, bounded (H5).** A hold's `deadline_s` defaults to 604800 s (7
days, the longest any watch lives) and may be lowered, never raised. This is the
whole "renewal": the deadline is already the sane maximum, so a hold cannot lapse
on a short fixed deadline while its work is live, and no sliding renewal exists
to go wrong. A hold that outlives 7 days expires like any watch
(`next: "watch ended; create a new watch if still needed"`); re-arm it then.
A hold with **no work** (a bare `vigil hold`) never fires: it stays live until
cancelled or the 7 days are up.

**Release (H6).** When all work has ended the hold fires once with one event:
`input: "hold"`, `summary: "hold released: <why>"` where `<why>` lists each
input and what it read (`qex:ab12 completed; process:4242 exited`; each input's label, then its reading), cut to 200
bytes. `next` is `"act on events"`; `vigil wait` exits 0. While any work is live
the envelope is `pending` with `next: "call keepalive again"` and no events
(CLAIM(H7)). A hold that expires or is cancelled gives the ordinary terminal
envelope, with no release event.

**Orchestrator window (H9).** A hold defaults to `orchestrator: true`: holds are
for main sessions, so keepalive over a hold uses the 55 minute window on a 1 hour
prompt cache (and the credit checks). `"orchestrator": false` at creation, or
`orchestrator: false` on the call, overrides. A subagent must pass its own
`watch_id` to `keepalive`: a no-id call covers every watch this process
created, holds included.

**`vigil hold --until-idle` (H10).** `idle:<dir>` with `<dir>` the creating cwd:
live until no qex job whose cwd is that directory or below is `queued`,
`starting` or `running` (`qex list --under DIR --all --json`). Scope decision:
by *directory*, because qex records each job's cwd and the queue is the user's
own. Consequences, stated: a session that runs jobs from a worktree outside its
cwd must hold `--for idle:<worktree>` too; with no active job under the
directory the hold releases at its first tick ("hold released: ... idle"), which
is the correct answer to "is anything running?". Deferred: scoping by the
submitting process tree (qex records `submitter` pids), which would follow a
session into its worktrees; the data is there, the rule is not decided.

**CLI (H11).** `vigil hold [--id ID] [--for SPEC]... [--until-idle]
[--deadline-s N] [--expect-s N] [--orchestrator BOOL]`. `--for` repeats, one spec per use. Prints the created watch like `vigil
create` (`watch_id`, `status`, `created_at`, `deadline_at`); then `keepalive
<id>` / `wait <id>` as for any watch. `--orchestrator` on `keepalive` and
`wait` is the existing flag (AI1'): main session only, selects the 55 minute
window on a 1 hour cache. MCP: tool `hold {id?, for?: [spec], until_idle?,
deadline_s?, expect_s?, orchestrator?}`, same errors as `watch_create`.
`vigil hold` with neither `--for` nor `--until-idle` is the bare hold (H5).

**Wire compatibility (H12).** `fire: "hold"` and the new `kind`s are additions.
A new binary reads every existing watch file unchanged; an old binary does not
understand a watch file with `fire: "hold"` (it reports the file as unreadable),
so run the new binary on every host that shares a state root.

## Advisories

Rules the operator programs at runtime that tell agents what to do, delivered
wherever vigil talks to an agent. Built 2026-11-12 at the owner's request. Proof:
`src/advisory*.rs` (unit), `src/advisory_tests.rs` (engine), `tests/advisory_e2e.rs`
(binary, CLI and MCP). Claim ids below are the `CLAIM(...)` ids in code and tests.

**Why.** An agent cannot see Boat's remaining hours or the host's memory; the
operator can write down once what to do about them, and every agent reads it
where it already looks: the keepalive result. The first rule: prefer Boat
(cloud sandboxes) for all heavy work; when Boat's remaining hours are at or below
a threshold (default 10%), use `qex` on the local host.

**An advisory** is `{probe, extract, params, condition, message_when_true,
message_when_false, cache_s}` under an id `[A-Za-z0-9_-]{1,64}`:

- `probe`: a `cmd` (argv), `url` (http) or `file` input, the same shapes as a
  watch input (cmd `timeout_s` 0.1..=19). A `cmd` must exit 0 and a `url` answer
  2xx, else the probe failed. `port`, `process` and `glob` cannot give text and
  are refused.
- `extract`: how to get one number from the probe's output. Either
  `json = "a.b.0.c"` (dotted path; array indices; a numeric string counts), with
  optional `of = "a.d"` (divide by that path; a zero denominator is a failure)
  and `scale = 100` (multiply last); or `regex = "..."` (group 1, else the whole
  match; at most 256 bytes). `name` (default `value`) is what the number is
  called in the condition and messages; `value` always works as an alias.
- `params`: named finite numbers (at most 16; letters, digits, `_`; not a setting
  name; not the extract name).
- `condition`: `<operand> <op> <operand>`, op one of `<= < >= > == !=`, an operand
  a known name or a number literal, at least one a name (at most 128 bytes).
- `message_when_true` / `message_when_false`: one line, at most 512 bytes;
  `{name}` is the extracted number or a param, printed with at most one decimal
  and no trailing `.0` (`7.3`, `10`); `{{` and `}}` are literal braces. An unknown
  name is an error when written, never printed.
- `cache_s`: how long a probe result is reused (default 600; 0 = probe every
  call; at most 86400).

Everything is checked once, in `Advisory::new`, whichever door it came through
(CLAIM(advisory-valid)). At most 32 advisories. Unknown fields are errors.

**The file (CLAIM(advisory-file), advisory-reread, advisory-seed, advisory-lock).**
`$VIGIL_ADVISORIES_FILE`, else `$XDG_CONFIG_HOME/vigil/advisories.toml`, else
`~/.config/vigil/advisories.toml`. `VIGIL_ADVISORIES_FILE=off` turns advisories
off (no `advice` key anywhere; the test sandboxes use it). One table per
advisory:

```toml
[advisories.boat-budget]
probe = { kind = "cmd", argv = ["env", "BOAT_NO_UPDATE=1", "/home/user/.local/bin/boat", "limits", "--json"], timeout_s = 15 }
extract = { name = "pct", json = "planIncluded.vmHoursRemaining", of = "planIncluded.vmHours", scale = 100 }
condition = "pct <= threshold"
message_when_true = "Boat has {pct}% of its hours left (at or below the {threshold}% threshold): ..."
message_when_false = "Boat has {pct}% ... prefer Boat ... via edge.research/bin/boat-run; ..."
cache_s = 600
[advisories.boat-budget.params]
threshold = 10
```

The file is read on every call, so a hand edit takes effect with no restart.
`vigil advisory ...` edits it in place under `<file>.lock` (an `flock` held across
read, check and rename), keeping the operator's comments, and writes
tmp + fsync + rename; a refused edit leaves the bytes untouched. A file that does
not exist is created with the default `boat-budget`; a file that exists is never
given defaults back (so `rm boat-budget` sticks). A table that no longer
validates is reported as its own `unavailable` entry and does not hide the others;
a file that is not TOML is an error naming it and is never rewritten.

**Evaluation (CLAIM(advisory-cache), advisory-immediate, advisory-unavailable).**
Per advisory: use the cached probe result if it is younger than `cache_s` (a
failed probe: at most 30 s), it was taken for the same probe and extraction
(a hash), and its time is not in the future; else run the probe through the
engine's input reader (deadline 20 s), take the number, and cache the outcome in
`<state root>/advisory-cache/<id>.json` (atomic, best effort), so separate
`vigil` processes share it. The condition and message are applied to the cached
number on every call, so `advisory set ... threshold=15` shows on the very next
return with no new probe. Every failure (probe exit code, timeout, HTTP status,
missing field, not JSON, regex no match, bad file) becomes the entry
`advisory <id> unavailable: <reason>`; a value older than its cache time is never
shown, and nothing is silent. Probes run one after another, so many slow
advisories add up (at most 32 x 20 s; keep probes cheap).

**Delivery (CLAIM(advisory-delivery)).** `advice` is on the final envelope of
every `keepalive` (CLI and MCP) and of `wait`, evaluated when the call returns
(a probe adds at most its own time, only on a cache miss). `vigil advice` prints
one `id: message` line per advisory (`--json`: `{"advice": [...]}`) for hooks
and scripts; it exits 0 even when a probe failed (the failure is a line), and 1
only when advisories are off. The MCP server `instructions` say results carry
`advice`.

**CLI.** `vigil advice [--json]`; `vigil advisory list|show ID|add ID [JSON]
[--replace]|set ID NAME=VALUE...|rm ID`. Output is JSON (`list`: an array;
`show`/`add`/`set`: `{id, spec}`, `show` plus the current `advice`; `rm`:
`{removed}`); errors are one line on stderr, exit 1. `set` takes a parameter name
or one of `cache_s`, `condition`, `message_when_true`, `message_when_false`; a
name that is none of those is an error listing the parameters, and all
assignments apply together or not at all (CLAIM(advisory-set)). Settings can
change values, not add parameters: use `add --replace` for that.

**MCP tools.** `advice`, `advisory_list`, `advisory_show {id}`,
`advisory_add {id, spec, replace?}`, `advisory_set {id, settings: {name: number|string}}`,
`advisory_rm {id}`: the same operations as the CLI (`src/advisory_ops.rs`), errors
as `isError` with the CLI's message.

**Default advisory `boat-budget` (CLAIM(advisory-boat-budget)).** Probe `env
BOAT_NO_UPDATE=1 <home>/.local/bin/boat limits --json` (read-only; `env` keeps the
CLI from self-updating, and the absolute path survives a PATH without
`~/.local/bin`; `boat` alone when HOME is unknown). `pct =
planIncluded.vmHoursRemaining / planIncluded.vmHours * 100`, `threshold = 10`,
true when `pct <= threshold` (exactly 10% counts): use `qex` on this host for
heavy jobs; false: prefer Boat via `edge.research/bin/boat-run` for builds and
test runs, `qex` locally only for one or two quick runs. To change the threshold:
`vigil advisory set boat-budget threshold=15` (or the MCP tool `advisory_set`, or
edit the file).

**Trust.** The model is an untrusted caller of `advisory_add` and `advisory_set`,
but `watch_create` already lets it run any `cmd` input, so an advisory probe adds
no new power; it adds persistence (the probe reruns, cached, on later calls
until removed). Text read from a probe never reaches an agent: only the extracted
number, formatted, is placed in the operator-written message. Bounds above cap
what a caller can make vigil store or run.

**Hook.** agent-integration's Claude/Codex PreToolUse hook runs `vigil advice`
before heavy Bash commands (flutter test, edge-test, cargo build/test, gradle,
nix build, qex submit) and injects the lines as additional context; it fails open.
