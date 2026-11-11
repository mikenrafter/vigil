# vigil — cache-aware tick length, time visibility, early attention (spec draft, 2026-11-10)

Second build-loop cycle on `feat/vigil-disk-full`, after disk-full (DF) is green.

## Why

The agent should not decide how long a keepalive tick is. Today it passes
`--max-s` (30..=270) or the manual skill hardcodes `sleep 270`. On a Claude
main session with a 1-hour prompt cache, a 270 s tick costs ~11x more model
calls than the cache needs (research 06: tick-started turns ≈ 25% of spend).
Longer ticks then hide time from the agent: a job can hang for 50 minutes
before anyone looks. So longer ticks need two things the manual tick gave for
free: a clear sense of elapsed time, and an early wake when a watched job looks
stuck or overdue.

Owner decisions (2026-11-10): keep the cache warm; read the real TTL cheaply;
270 s when unknown; Codex assumed 5 minutes; vigil is the default wait with a
manual tick as backup; the agent should not think about tick length.

## Claims

- **AI1 — interval from the session's cache TTL.** `keepalive` with no
  `max_s` (MCP) / no `--max-s` (CLI) picks the interval itself:
  - Claude Code (`CLAUDE_CODE_SESSION_ID` set): find
    `~/.claude/projects/*/<id>.jsonl`, read only its tail (≤ 512 KiB, newest
    first), take the most recent assistant `usage.cache_creation` with a
    nonzero write: `ephemeral_1h_input_tokens > 0` → TTL 1 h; else
    `ephemeral_5m_input_tokens > 0` → 5 min. Nothing found → unknown.
  - Codex (`CODEX_*` / codex env, or `AGENT=codex`): 5 min (no TTL is
    recorded; owner decision).
  - Anything else, unreadable file, parse error: unknown.
  - Interval: 1 h → 3300 s; 5 min or unknown → 270 s. An explicit
    `max_s`/`--max-s` still wins, clamped to the transport limit.
- **AI2 — transport limits respected.** MCP calls from Codex never exceed
  270 s (Codex tool timeout 300 s). Detection runs at every call, so a session
  that drops from 1 h to 5 min (subscription on usage credits) is caught at the
  next tick.
- **AI3 — the agent sees what was chosen and why.** Every envelope carries
  `"cache": {"ttl": "1h"|"5m"|"unknown", "source": "claude-transcript"|"codex-default"|"default"|"explicit", "interval_s": n}`.
- **AI4 — time visibility.** Every envelope carries
  `"clock": {"waited_s", "watch_age_s", "since_last_call_s", "summary"}` where
  `summary` is one plain line, e.g. `"waited 55m 0s this call; watching for
  2h 10m; last call 55m ago"`. (`since_last_call_s` is from the watch file's
  last-call timestamp; null on the first call.)
- **AI5 — early attention.** A watch may declare `expect_s` (how long the job
  should take) and `stall_s` (how long its inputs may stay unchanged while it
  is still running). vigil emits one `attention` event, which ends the current
  keepalive call early (`next: "act on events"`) without ending the watch, when:
  - elapsed since create passes `min(1.5 × expect_s, expect_s + 540)`
    (build-loop budget rule: 1.5x or 9 minutes over, whichever comes
    first); or
  - no input changed for `stall_s` while a `process` input is still running.
  Each attention reason fires once per watch (re-armed only by a new watch).
  When neither field is set, a long tick (interval > 270 s) applies a default
  `stall_s` of 900 s to `file`/`glob`/`cmd` inputs of a watch that also has a
  running `process` input, so a hung build is noticed within ~15 minutes.
- **AI6 — no change for short ticks.** With a 270 s interval and no
  `expect_s`/`stall_s`, envelopes differ from today's only by the added
  `cache` and `clock` fields.

## Known limit

Claude subagents share the parent's `CLAUDE_CODE_SESSION_ID` and environment
(checked 2026-11-10), but use a 5-minute cache. vigil cannot tell them apart,
so a subagent would get the parent's 1 h interval. Keepalive is an
orchestrator duty; subagent briefs say not to run long waits (and, if they
must, to pass `--max-s 270`). Recorded in the vigil skill.

## Defaults taken (owner may override)

- D1: 1 h → 3300 s (5 minute margin).
- D2: attention threshold = the build-loop rule (1.5x or +9 min, first).
- D3: default stall 900 s only for long ticks with a running process input.
- D4: transcript tail cap 512 KiB.

## Revision 2 (owner, 2026-11-10) — supersedes AI1, AI2 and "Known limit"

- **AI1' — 270 s unless the caller is the orchestrator.** The default interval
  is always 270 s. Only a call that marks itself as the main orchestrator
  session (CLI `--orchestrator`; MCP `orchestrator: true`; the watch can also
  store it at create) gets the TTL-derived interval (1 h → 3300 s). Subagents
  therefore stay at 270 s with no special instruction, which removes the
  subagent limit above.
- **AI2' — credit check every 4 minutes, account-wide.** Usage credits can
  start because of any session on the account, so a long call re-checks every
  240 s, independent of its own transcript:
  1. `uw --json status` (usagewindow, local, no auth): any `ClaudeCode`
     window with `exceeded: true` → treat the TTL as 5 min.
  2. Fallback when `uw` is missing or fails: any main-session transcript under
     `~/.claude/projects/*/*.jsonl` (not `subagents/`) modified in the last
     10 minutes whose newest cache write is 5 min after earlier 1 h writes.
  On detection the call ends early with an event of kind `cache_ttl_changed`
  (`from`, `to`, `source`), and later calls use 270 s until the check clears.
  Each check is cheap (one subprocess or a bounded tail read) and its result
  is in `cache.credit_check: {at, source, on_credits}`.
- **AI7 — going cold.** When `since_last_call_s` exceeds the TTL in force (the
  cache has certainly expired), or AI2' just fired, the envelope carries
  `"cold": true` and `next` tells the agent, before anything else, to post an
  inline BabelTele summary of the session to the user, collapsed as
  `<details><summary>Session state (BabelTele)</summary> … </details>`,
  covering: the task and its progress, work in flight (ids, paths), the tools
  in use (MCP servers, CLIs such as vigil and qex, watch ids), the user's
  standing rules for this session, and the skills loaded. The same instruction
  is also delivered by a hook (agent-integration W4: UserPromptSubmit when the
  gap since the last assistant turn exceeds the TTL), so it works without vigil.

Interplay to note: setting `promptCacheTtl: "1h"` in Claude settings would
keep main sessions at 1 h even on usage credits (2x-input writes billed to
credits) and make AI2' moot. The plan therefore does NOT set it (PLAN W5
dropped) unless the owner prefers that trade.

## Revision 3 (owner, 2026-11-10) — supersedes AI2' and AI7

- **AI7 removed.** vigil never wakes or re-activates a session to summarize
  it. "Going cold" is the agent's own step when it is told to end the session
  (or its keepalive duty ends): it posts the collapsed BabelTele summary
  (`<details><summary>Session state (BabelTele)</summary> … </details>`: task
  and progress, work in flight, tools in use, standing rules, skills loaded),
  then stops calling vigil. This lives in the manual-keepalive and vigil
  skills (PLAN W3), not in vigil's code. `watch_cancel` needs no change.
- **AI2' removed.** `promptCacheTtl: "1h"` is pinned in the managed Claude
  settings (PLAN W5 restored), so usage credits do not shorten the main
  session's cache. Credit state is usagewindow's job (its MCP and hooks), not
  vigil's. vigil still re-reads its own transcript's TTL at every call (AI1),
  which costs one bounded tail read.

## Revision 4 (owner, 2026-11-10) — supersedes Revision 3's removal of AI2'

The 1 h pin (PLAN W5) and the 4-minute credit check are separate things; keep
both.

- **AI2' restored, as written in Revision 2**, minus "later calls use 270 s":
  under the pin the main cache stays 1 h on credits, so the interval follows
  the TTL actually observed in the session's own transcript (AI1).
- **AI8 — credit-change injection point.** When the 4-minute check sees the
  credit state change (either direction), vigil:
  1. records it in `cache.credit_check` and emits one `credit_state_changed`
     event (`on_credits`, `source`, `windows`), which ends the current call so
     the orchestrator learns at once;
  2. runs an optional hook command, `VIGIL_ON_CREDIT_CHANGE` (env) or
     `on_credit_change` in vigil's config, with the new state as JSON on
     stdin, a 10 s timeout, output and exit code captured in the event. Empty
     by default. This is where managing the session cache setting by usage
     limits plugs in (e.g. switching `promptCacheTtl`), if and when Claude
     Code honours a mid-session change. Until then it can log or notify.
- AI7 stays removed (Revision 3).

### AI8 — verified mechanism (2026-11-10)

Tested on a live Claude Code 2.1.301 session: adding `"promptCacheTtl": "5m"`
to a settings file switched the session's next cache write from
`ephemeral_1h` to `ephemeral_5m` with no restart; removing it switched back
after one more request. `~/.claude/settings.json` is a plain writable file
(Nix activation copies it), so the hook can manage the TTL live.

Default hook (agent-integration, PLAN W5): `claude-cache-ttl-policy` reads
the JSON state on stdin and sets `promptCacheTtl` in `~/.claude/settings.json`
with an atomic jq merge that touches only that key: `"5m"` while on usage
credits (writes cost 1.25x input instead of 2x), `"1h"` otherwise (the pinned
default). vigil then reads the new TTL from the transcript at its next call
and shortens its interval to 270 s by itself. A rebuild resets the file to the
pinned 1 h, which is the correct state off credits.

## Defaults after red (orchestrator, 2026-11-11)

The red report's A1–A22 (`host-config.research/vigil-auto-interval/red-report.md`)
are accepted as written, except:
- **A12 amended.** `vigil wait`'s final envelope also carries `clock` (and
  `cache`), since a background `vigil wait` is the main way Claude sessions
  use vigil; status and cancel envelopes do not.

## Amendments after spec review round 1 (Sol 6/10, bar 9; orchestrator, 2026-11-11)

- **S1.** `wait`'s final envelope carries `cache` (from its last internal call)
  and `clock` measuring the whole `wait` (`waited_s` = total). Fix the tests
  that assert absence.
- **S2 (blocker) → AI2''.** Credit monitoring belongs to the orchestrator, not
  to long windows. Every orchestrator call runs the credit check when the
  last check (recorded per state root, see S9) is ≥ 240 s old, whatever its
  window; long windows also re-check every 240 s inside the call. Test the
  full automatic sequence 1 h → credits → 5 min (270 s calls keep checking)
  → credits end → hook restores 1 h → 3300 s.
- **S3.** `wait --orchestrator` uses the TTL window and the credit checks.
  `wait` returns on an `attention` or `credit_state_changed` event with a new
  exit code **5** ("attention; the watch is still live"); 0/3/4 unchanged.
  CLI tests prove both reach the agent.
- **S4 (reverses red default A3).** Identity is ambiguous when both Claude and
  Codex signals are present; ambiguous → 270 s. Test both nesting directions.
- **S5.** Credit state is tri-state: on / off / unknown. Unknown keeps the last
  known state and never emits an event or runs the hook.
- **S6.** `attention` events are ordinary events: generated with distinct ids,
  persisted with the tick (the one-shot flag is saved with them), delivered
  and acknowledged like other events (commit_delivery), and redelivered with
  `redelivered: true` after a failed save or a cancellation before delivery.
  Tests: failed-save retry, cancel-before-delivery, distinct ids.
- **S7.** Probes (≤ 5 s) and the hook (≤ 10 s) are bounded by the remaining
  call budget minus the 100 ms margin; a check is skipped when less than its
  timeout remains (it runs on the next call). Cancelling the call kills the
  hook's process group. Tests: deadline-edge skip, cancel during probe and
  during hook (descendants gone).
- **S8.** Bounds: transcript read by seeking to the last 512 KiB (assert bytes
  read via the seam, not just bytes returned); lines over 64 KiB skipped;
  regular files only, symlinks not followed; fallback scans at most 20
  newest main-session transcripts; `uw` output capped at 64 KiB; hook stdin
  ≤ 4 KiB; captured hook output ≤ 2048 bytes (test against the literal).
- **S9.** Hooks are serialized across processes: `<root>/credit.lock`
  (flock) around probe + hook, and `<root>/credit-state.json` records the
  last applied state and check time; a process runs the hook only when its
  observed known state differs from the recorded one, then records it. Two
  processes detecting the same change run the hook once. Test with two
  processes.
- **S10.** Add no-id (multi-watch) tests: mixed orchestrator flags (any covered
  orchestrator watch makes the call an orchestrator call), attention on a
  secondary watch, attention vs terminal result in the same call (terminal is
  primary; attention still delivered, as a secondary event).

## Defaults after spec review round 2 (final round; Sol 6/10; orchestrator, 2026-11-11)

Red-report defaults R2-A1..A8 accepted (A7 offsets: 60, 300, 540 s). Remaining:
- **N1.** A cross-process test races two real vigil processes (barrier via a
  file/FIFO) and proves `credit.lock` is held through both the hook and the
  `credit-state.json` update: the hook runs once.
- **N2.** Production read paths (transcript tail, fallback scan, `uw` output)
  go through the counting seam; tests assert bytes consumed on those paths,
  not only content.
- **N3.** The CLI prints the envelope before committing delivery (print, then
  `commit_delivery`); a failed stdout write leaves it uncommitted, so the next
  call replays it (`redelivered: true`). Test with a closed stdout.
- **N4.** Hook delivery is at-least-once: deduplication counts only after the
  state write commits; hooks must be idempotent (documented; the default
  `claude-cache-ttl-policy` is, since it sets a value). If the state write
  fails or the process dies after the hook, the next check re-runs it. Test
  with an injected failure after the hook's side effect.
