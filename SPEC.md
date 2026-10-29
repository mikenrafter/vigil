# vigil — spec

An MCP server that keeps an agent's turn open while a cheap model watches
things for it. The agent says what to watch and for how long. After that it
calls one tool, `keepalive`, over and over. Each call returns the tick count,
elapsed time, what changed, and what the watcher decided. The big model
spends almost no inference while it waits, and its prompt cache stays warm.

Written in Rust. Status: draft, not built. Nothing below has been run.

## Why this exists

Research (see an earlier design discussion) found parts but no whole:

- wait-style MCP servers (mcp-await, herdr-mcp, mcp-timer) block until a
  fixed condition or timeout, with no judgement about relevance;
- time MCPs (at-mcp, temporal-mcp) count elapsed time but cannot wake or hold
  an agent;
- Claude Code's Monitor tool and OpenClaw's heartbeat wake an agent, but each
  wake is a full-model turn and is tied to one harness.

vigil joins them: wait primitives, a tick/elapsed counter, a cheap judge
(Jev, the Laya typed-decision service), and a delivery path that does not
need the agent to re-arm anything.

The manual `manual-keepalive` skill does about half of this. vigil replaces
the `sleep 270 && echo keepalive` command with a tool whose result carries the
context the agent expects on wake.

## Non-goals

- Not a general scheduler, and no interaction with usagewindow or `uw-agent`.
  A long watch is just many short `keepalive` calls; vigil never resumes a
  session.
- Targets Claude Code and Codex only.
- No new model. Jev is the existing `laya` flakelet on `127.0.0.1:4010`.
- No config UI. A watch is one JSON object.

## Terms

- **watch**: one sensing job. Has a prompt for Jev, a list of inputs, an
  interval, a deadline, and a fire rule.
- **tick**: one evaluation round of a watch (read inputs, maybe ask Jev).
- **keepalive**: the agent-facing blocking tool call. One call can span many
  ticks. It is the thing the agent repeats.
- **Jev**: the cheap judge. A POST to Laya's `/v1/predict`.

## Architecture

```
agent harness ──stdio MCP──▶ vigil mcp ──unix socket──▶ vigil daemon ──▶ Jev (laya :4010)
                                                           │
                                                           ├─ SQLite (watches, ticks, events)
                                                           └─ input readers (file, glob, cmd, url, port, process)
```

Two processes from one binary, because the harness kills a stdio server when
the session ends, and a watch should outlive that.

- `vigil mcp`: thin stdio MCP front. Holds no state. Starts the daemon if its
  socket is absent.
- `vigil daemon`: owns watches and state. Runs ticks on its own clock, so no
  harness runtime limit applies to the sensing.
- `vigil` CLI subcommands (`create`, `status`, `list`, `cancel`, `keepalive`)
  hit the same socket. Programmatic monitors, with no agent involved, use
  these.

Crates: `rmcp` (official Rust MCP SDK) for the front, `tokio`, `rusqlite`,
`serde`, `reqwest` (Jev and URL inputs). Pin versions in the spike (phase 0).

## Delivery: how the agent gets context without re-arming

Three paths. The first is required; the rest are additions.

1. **Blocking `keepalive` result (required).** The call blocks in the daemon
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
3. **Channel push (optional, Claude Code only).** Declare the
   `claude/channel` experimental capability and send
   `notifications/claude/channel` when a watch fires while the agent is idle
   between `keepalive` calls. This is the only path that wakes an idle
   session with no call at all. It needs the session started with
   `--channels` and is a research preview, so treat it as a probe: the
   daemon records at initialize whether the client accepted the capability
   and reports it in `watch_status`.

### Interval and long deadlines

The agent stays "in cache" only if the gap between its requests is under the
prompt-cache TTL, so every `keepalive` call has a fixed window:

- `max_s` is clamped to **30 minimum, 270 maximum**; the default and the
  value the daemon enforces as the ceiling is **270**. The ceiling is not
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
  "fire": "jev",
  "jev_questions": [{"id": "failed", "type": "bool", "text": "Did the build fail?"}]
}
```

- `fire` is `"jev"` (fire when Jev's decision is true), `"change"` (fire on
  any input change, no model call), or `"condition"` (fire when every
  deterministic input meets its `want`, no model call).
- Inputs are the wait primitives from mcp-await (file, URL, port, process),
  re-implemented in the daemon, plus `glob` and `cmd`. The daemon does not
  proxy to a separate mcp-await server: an MCP server cannot easily call
  another one, and the primitives are about 200 lines.
- Returns `{watch_id, status: "pending", created_at, deadline_at}`.

### `keepalive`

Input: `{watch_id?: string, max_s?: number}`. With no `watch_id`, it covers
all pending watches.

Blocks until any covered watch fires or expires, or `max_s` (30 to 270,
default 270) elapses.

### `watch_status`, `watch_list`, `watch_cancel`

Non-blocking reads and cancel. `watch_status` also reports daemon health,
Jev reachability, and whether the channel path was accepted.

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
     "summary": "ERROR: linking failed", "jev": {"failed": true, "confidence": 0.93}}
  ],
  "digest": "tail of /tmp/build.log since last call: ...",
  "jev_status": "ok | down | skipped",
  "next": "act on events"
}
```

- `tick` counts rounds of this watch; `session_ticks` counts `keepalive`
  returns for the whole session. The agent can quote either.
- `now` is the daemon clock. This is the "legibility of time" piece: the agent
  sees real time on every return.
- `digest` is the file/command reading the agent would otherwise do itself,
  already trimmed to what is new since the last return. Bounded by
  `VIGIL_DIGEST_MAX_BYTES` (default 8192). If cut, say so.
- `events` is empty on a pure timeout. Then `status` is `pending`, `next` is
  `"call keepalive again"`, and the envelope is short, so a quiet hour costs
  a few hundred tokens per tick, not thousands.

## Tick logic (daemon)

For each watch, every `interval_s`:

1. Read all inputs. Hash each result.
2. If nothing changed since the last tick, stop. **No Jev call.** Most ticks
   end here, which is where the cost saving comes from.
3. If `fire` is `change` or `condition`, decide deterministically and stop.
4. Otherwise send Jev: `{state: <prompt + changed inputs, trimmed>,
   questions: jev_questions}` to `POST /v1/predict`. Record the answer.
5. Fire if the rule matches. Write an event row. Wake any blocked
   `keepalive`.

Failure rules:

- Jev down or timed out (default 10 s): record `jev_status: "down"`, **do not
  fire and do not drop the change**. Re-ask next tick. After 3 consecutive
  failures, fire with `events[].summary = "jev unavailable; raw change
  attached"` so a human-grade model decides. Silent failure is the worst
  outcome here.
- Input read error (missing file, refused port): report it as a change with
  the error text; it is not a crash.
- Deadline: checked against the wall clock at each tick and at each
  `keepalive` entry and exit. Once passed, `status: "expired"` with the last
  digest, so the agent knows the final state. A daemon that was down past the
  deadline reports `expired` on the first call after restart.

## State

SQLite at `$XDG_STATE_HOME/vigil/vigil.db` (`~/.local/state/vigil/` as the
daemon's default). Tables: `watches`, `ticks` (hash per input per tick),
`events`, `sessions`. Survives daemon restart: on start the daemon reloads
pending watches and keeps their original deadlines, so elapsed time is true
across restarts (the gap at-mcp has). Prune ticks older than 7 days.

## Open questions (resolve in phase 0)

1. **Laya request schema.** The flakelet shows `POST /v1/predict` with
   `{state, questions}` but not the question types or result shape. Read the
   Laya SDK and fix `jev_questions` to match before phase 2.
2. **Channels.** Confirm the exact capability name, payload shape, and whether
   a channel event starts a turn in an idle session. Confirm with a throwaway
   server before building path 3.
3. **Blocking limit.** Confirm a 270 s blocking call completes in both Claude
   Code and Codex over stdio, with and without progress notifications. If
   either client gives up earlier, record the real limit; the 270 s ceiling
   stays locked unless that finding forces a change, which is then the user's
   decision.
4. **Codex.** Confirm the same tools and `instructions` text work in Codex's
   MCP client (it will not use the channel path).

## Packaging

Scout module at ``, following `modules/apk-builder/`:

- `scout` facet: the `vigil` binary and an agent skill (`skills/vigil/SKILL.md`)
  to every host.
- `flakelet`: the daemon as a user service, enabled where wanted. The
  `laya` flakelet is a runtime dependency (default `127.0.0.1:4010`);
  where it is absent, `fire = "jev"` returns an error at `watch_create`
  instead of failing later.
- Wire the MCP server into the agent configs through `agent-integration`.
- Per `AGENTS.md`: build and evaluate only the host you are on. On host-a,
  use `nix eval .#nixosConfigurations.host-a.config.system.build.toplevel.drvPath`,
  not `nix flake check`. Cargo builds on host-a or host-b (8 GB RAM) need
  `low-ram-hosts`; use `-j2`.

## Build plan (for rg-implement)

Each phase ends with passing tests. Tests that need Jev use a fake HTTP
server that returns canned answers.

0. **Spike (no production code).** Answer the four open questions above.
   Output: a short `FINDINGS.md` and pinned crate versions.
1. **Core daemon.** SQLite state, input readers, tick loop, deterministic
   fire rules, restart recovery. CLI: `create`, `status`, `list`, `cancel`.
   Tests: file/glob/cmd/port/process readers; unchanged input makes no model
   call (assert on the fake Jev's request count); deadline expiry; restart
   keeps the original deadline.
2. **Jev.** `fire = "jev"`, trimming of `state`, failure rules (down,
   timeout, three strikes). Tests: each failure path, including that a
   change is never dropped.
3. **MCP front and `keepalive`.** stdio server, the five tools, envelope,
   `instructions`, `max_s` clamp to 30-270, progress notifications. Tests: an MCP client
   in the test drives create, then repeated keepalive; blocked call returns
   early on fire; `max_s` of 5 and 9999 are clamped to 30 and 270; a deadline hours away returns `pending` at 270 s; pending returns carry `tick` and `now`; quiet envelope is
   small.
4. **Channel push.** Only if phase 0 says channels work.
   Test the fallback: with the capability refused, nothing breaks and
   `watch_status` says so.
5. **Nix packaging and skill.** Scout module, flakelet, agent skill, MCP
   wiring. Verify per `AGENTS.md`: the affected output evaluates, then
   `nix flake check --no-build` (or the host-a-safe eval).

## Acceptance

- An agent creates a watch on a growing log, then calls only `keepalive`.
  It sees ticks counting up with real timestamps, no tool calls other than
  `keepalive`, and receives the failure line plus Jev's verdict when it
  appears, or an `expired` envelope with the final digest at the deadline.
- 100 quiet ticks produce zero Jev requests.
- Killing and restarting the daemon mid-watch does not reset elapsed time.
- With Jev stopped, a change still reaches the agent within three ticks and
  is labelled as unjudged.
