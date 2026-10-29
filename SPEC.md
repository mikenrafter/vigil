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
  under `$XDG_STATE_HOME/vigil/`.

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
- `vigil` CLI subcommands (`create`, `status`, `list`, `cancel`, `keepalive`)
  run the same code against the same files. Programmatic monitors, with no
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
   prints the final envelope and exits (0 fired, 3 expired, 4 cancelled, 2 usage,
   1 error). The agent runs it once as a background command; Claude Code
   wakes the agent when it exits.
   Trade-off: no model calls while waiting, so the prompt cache goes cold
   after its TTL. Use `keepalive` to stay warm, `wait` for long waits where
   a cold cache is fine. Codex has no background wake, so it uses
   `keepalive` only.

### Interval and long deadlines

The agent stays "in cache" only if the gap between its requests is under the
prompt-cache TTL, so every `keepalive` call has a fixed window:

- `max_s` is clamped to **30 minimum, 270 maximum**; the default and the
  value vigil enforces as the ceiling is **270**. The ceiling is not
  configurable. Values outside the range are clamped, not rejected, and the
  envelope reports the value used (`waited_s`).
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
  "judge_rule": {"question": "failed", "noul_gte": 0.8}
}
```

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

Input: `{watch_id?: string, max_s?: number}`. With no `watch_id`, it covers
the pending watches this `vigil mcp` process created. Watches from another
session (or from before a restart) need their id.

Blocks until any covered watch fires or expires, or `max_s` (30 to 270,
default 270) elapses. Ticks run in this call, each when the watch's
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
Nothing is written to the working tree, `$XDG_CONFIG_HOME` or any repository.

- `watches/<id>.json`: the watch as created (paths already absolute, see
  V11), plus `status`, `created_at`, `deadline_at` (absolute UTC),
  `finished_at`, `tick`, `last_tick_at`, per-input observation state (V9),
  `judge_strikes`, the pending change (V14, R7), `last_digest`, and the
  terminal events.
- Writes are atomic: write `<id>.json.tmp` in the same directory, fsync,
  rename.
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
   `instructions`, `max_s` clamp to 30-270, progress notifications. Tests: an MCP client
   in the test drives create, then repeated keepalive; blocked call returns
   early on fire; `max_s` of 5 and 9999 are clamped to 30 and 270; a deadline hours away returns `pending` at 270 s; pending returns carry `tick` and `now`; quiet envelope is
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
