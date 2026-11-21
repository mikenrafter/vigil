# vigil notices: one-time announcements, and where topic snippets would grow from (design note, 2026-11-21)

Status: notices are built (`src/notice.rs`, `src/notice_ops.rs`, SPEC
"Notices"). Topic snippets with cadence, sketched in the second half, are
design only: no code, no schema is committed to.

## Why notices

On 2026-11-21 the owner wanted every agent session to hear once that Gemini
Neon is a build-loop reviewer and which skill files to reread. Neither existing
channel fits:

- An **advisory** is probe-backed state shown on every return. As an
  announcement it nags forever.
- A **reminder** is scoped and scheduled, and `done` / `snooze` are shared by
  everyone: the first agent to mark it done silences it for all sessions. The
  stand-in (`neon-onboarding`, a reminder gated by a date probe, told to "not
  mark this done") is a workaround for exactly this.

What an announcement needs is the one thing neither has: **memory per agent
session**. That per-session record is the part to reuse.

## Model

```
<state root>/notices/<id>.json            what to say (operator-written)
<state root>/notice-delivery/<hash>.json  what this session has been told
```

A delivery record maps notice id to the creation time of the notice that was
shown. A pending notice is one that is not expired, is in scope for the
caller's directory and is not in the record. `claim` records and returns the
pending set under the record's lock; the caller prints and keeps the claim, or
releases it when printing failed. Details and claims: SPEC "Notices".

Delivery is at most once on a crash between the durable claim and the print,
or on a transport failure after the MCP handler returns. That delivery can be
lost for the session; it is never automatically repeated. A failed print or a cancel
before commit releases the claim for retry. Release is best effort if the state
cannot be locked or written. A crash can lose the claimed batch, at most one
delivery, rather than replaying notices already claimed.

Design choices worth keeping when this grows:

- **One record per session name, one directory lock, durable multi-record tx.**
  Different sessions keep separate files so corruption stays isolated to one
  name, while all claims, releases and maintenance serialize on
  `notice-delivery/.lock` (never unlinked). Multi-name updates write a durable
  undo journal (`notice-delivery/.tx`) before touching target records so a
  crash never leaves half the names claimed or released.
- **The session id is text from outside** (a hook payload field), so it only
  ever names a file through a hash.
- **Silence writes nothing.** The common claim finds nothing pending and
  neither writes nor locks. This is a requirement, not a micro-optimization:
  every keepalive claims, and `cli_e2e::nothing_is_written_outside_the_state_root`
  pins the state root's contents.
- **Generation, not just id.** The record stores the creation time it
  delivered, so remove-and-re-add is a new announcement.
- **Claim, then print, then keep or release.** The same shape as the keepalive
  `Delivery` (N3): at most once under concurrency, released on a failed print or cancellation before commit, subject to
  the crash guarantee above.

## Delivery targets (hook wiring happens in phoe-nix)

| Harness | Where to call `vigil notices --from-hook` | What reaches the model |
|---|---|---|
| Claude Code | `SessionStart` and `UserPromptSubmit` hooks | `hookSpecificOutput.additionalContext` |
| Codex | `SessionStart`, and `PreToolUse` next to `vigil-advice-hook` | `additionalContext` (as the advice hook does today) |
| Antigravity `agy` | `~/.gemini/config/hooks.json`; a `PreInvocation` event exists | only a `deny` reason on a tool hook is known to reach the model (`report-agy-integration.md`); `PreInvocation` `injectSteps` with a `systemMessage` is reported feasible but untried. A first-pass design: deny the first tool call of a session with the notice text, allow the identical retry, as the advice hook already does |
| T3 Code | no hooks | the keepalive envelope and the MCP `notices` tool, through the vigil MCP server |

Session identifiers are tabulated, with how sure we are, in
`docs/integrations.md` ("Notices in a hook"). `--session` takes any text, and
`--from-hook` reads `session_id`, `conversationId` and the thread spellings, so
a harness whose field differs from the table is a one-line fix in
`hook_session_id`.

## Open questions

1. **Set-of-names identity.** Notices collect every session name a call sees:
   all present identity environment variables (`T3CODE_THREAD_ID`,
   `CLAUDE_CODE_SESSION_ID`, `CODEX_SESSION_ID`, `CODEX_THREAD_ID`,
   `ANTIGRAVITY_CONVERSATION_ID`), plus hook payload ids (`--from-hook`).
   Ancestry is used last and only when neither environment nor payload provides
   a name. Explicit `--session` selects only that named session. This is
   separate from peers and hold ownership.
2. **Compaction.** After `/compact` the agent has lost the notice text but the
   record says it was shown. A SessionStart hook with source `compact` could
   re-deliver unexpired notices (a `--again` flag that ignores the record for
   this call and does not claim). Not built: it needs a decision on whether a
   notice is knowledge (once is enough) or an instruction (repeat after
   compaction).
3. **Wake-up.** A notice waits for the next return. A long orchestrator
   keepalive (up to 50 minutes) could end early with a `kind: "notice"` event,
   as `reminder_due` does. Not built: an announcement is rarely urgent, and the
   event claim would need its own once-per-session machinery.
4. **Retention.** The sweep runs on notice removal and on an operator list
   that finds expired notices, never between claim and print. A record is
   removed only when every delivered generation is removed or expired. All
   writers, releasers and cleanup share one delivery-directory lock
   (`notice-delivery/.lock`), which is never unlinked; cleanup also retires any
   legacy per-record `.lock` files.
5. **Codex child environments.** Fable's round-2 live check found
   `CODEX_SESSION_ID`, `CODEX_THREAD_ID` and `T3CODE_THREAD_ID` in shell
   children, and only `T3CODE_THREAD_ID` in MCP children. Both CODEX names
   exist in the 0.170 binary. The hook environment remains unverified.
   One app-server hosts several threads, so ancestry cannot distinguish them.

## Identity as a set of names (implemented)

Implemented in `src/notice.rs` and `src/notice_ops.rs` (`CLAIM(notice-tx)`).
Every call collects all present identity environment variables and hook payload
ids, using ancestry only when none exist. A notice is pending only when no
record under any of those names contains its generation. Every delivery—and any
live delivered generation already present on another name in the set—is
recorded under every name the call sees, so a bridging call links names without
printing again. Reads and updates serialize on `notice-delivery/.lock`; writes
touching more than one record use a durable undo journal
(`notice-delivery/.tx`) recovered before any later claim, release or cleanup,
so a crash never leaves half the names claimed or released. Release removes
only generations claimed by that call across those names.

**Known limit:** disjoint name sets cannot be joined until some call sees both
names. Outside T3, a Codex MCP child with no session environment variable or
payload still falls back to its shared ancestor record and remains disjoint
from named threads until a call sees both.

## Topic snippets with cadence (residual-cli's guru pattern), later

residual-cli's `skills/guru` holds topic-keyed guidance snippets, attached per
skill (`topics_for_skill`, `block_for_topic`), and `skills/hooks.rs` decides
when a suggestion may resurface: a per-topic last-fired turn and a minimum
number of turns (`MIN_TURNS_BETWEEN_SAME_TOPIC = 10`), plus a dedup of the last
suggestion, with the rule "staying silent when irrelevant matters as much as
firing". Two limits make it a poor fit for several agents at once, and both are
what vigil's session record fixes: its state is one global file
(`residual-hook-state.json`, one `last_suggestion` for everyone), and the turn
number comes from the caller.

A notice is the degenerate case of that pattern: a snippet with no relevance
test and a cadence of "once". The extension keeps one machinery and widens the
unit of delivery.

### Shape

Same per-session file, extended (the field names are a sketch):

```
SessionRecord {
  session,
  delivered: { notice id -> created_at },      // exists today
  turn: u64,                                    // new: bumped by each hook call that asks for topics
  topics: { topic id -> { last_turn, last_at, count } },   // new
}
```

and a second operator-written store beside `notices/`:

```
<state root>/topics/<topic id>.json  { snippet, skills?: [...], scope?: [...],
                                       match?: [keywords], min_turns, max_per_session? }
```

`vigil topics --session S [--text T | --topic ID...] --from-hook` would, under
the record's lock: increment `turn`; pick the topics that are in scope and
relevant (either named by the caller, or matched by the keyword rule guru uses:
case-insensitive substring, false negatives are the safe failure); drop any
whose `turn - last_turn < min_turns` or whose `count` reached `max_per_session`;
record the survivors; and print them. A notice is `{min_turns: none,
max_per_session: 1, match: always}`, so `claim` becomes the one-topic special
case rather than a second mechanism.

### What carries over unchanged

- the claim / print / keep-or-release protocol and the lock (once per session, subject to
  the crash guarantee above);
- hashed session file names and the set-of-names notice identity (explicit,
  else all present identity env vars plus hook payload ids, else ancestor,
  else process run);
- "silence writes nothing": a turn with no relevant topic must not rewrite the
  record. The turn counter is the one wrinkle: bumping it every prompt writes
  every prompt. Options: derive `turn` from the harness (the hook payload has a
  turn id on Codex), or count turns only on calls that matched a topic, or
  keep the counter in memory-mapped scratch and persist it only when a topic
  fires. This is the main design decision before building;
- scopes by path component, expiry by exclusive instant;
- the hook targets above, with `PreToolUse` the natural Codex point and
  `UserPromptSubmit` the Claude point.

### What to test when it is built (test-quality)

The cadence rule is a pure function `(record, topic, turn) -> fire?`; give it a
model-based property test (random sequences of turns and topic sets against a
simple model), with a witness that the generator reaches the "fired, then
suppressed, then fired again" path (a conditional property with a vacuous
antecedent otherwise). Mutations that must go red: `<` to `<=` on
`min_turns`, forgetting to record a fired topic, recording a topic that was
suppressed, and sharing `last_turn` across sessions (guru's global-state bug).

### Not part of this

No code, config schema or CLI surface for topics exists. Ranking, learning
which topics help, and any model call in the hook are out of scope: guru's own
classifier is deliberately substring matching.
