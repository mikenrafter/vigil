# Integrations

vigil runs on its own: `change`, `condition` and `hold` watches need nothing
but the `vigil` binary. Each tool below adds one feature. When a tool is
missing, vigil either works without that feature or says so at the moment you
ask for it.

| Tool | What it adds | Without it |
|---|---|---|
| A judge endpoint | `fire = "judge"` watches | `watch_create` with `fire = "judge"` fails and names `VIGIL_JUDGE_URL` |
| `uw` (usagewindow) | The credit check reads usage windows directly | vigil reads Claude Code transcripts instead, silently |
| `boat` | A `boat-budget` advisory seeded on first use | The advisories file starts empty, with a commented example |
| `qex` | `qex:JOB` and `idle:DIR` hold items | Creating such a hold fails and says where to get qex |
| T3 Code | Nothing in vigil itself; hold on its delegate tasks with `none` items | Nothing changes |
| BabelTele | Nothing in vigil itself; `VIGIL_COLD_SUMMARY` can ask for it | `cold` asks for a short plain summary |

Harness settings for Claude Code and Codex are at the end. Set them in any
case.

## The judge

The judge is an HTTP endpoint that answers typed questions about text. vigil
calls it "the judge, a Jev-style typed-decision endpoint". Laya is one
implementation.

**What vigil does with it.** A watch with `fire = "judge"` sends the changed
input text to `POST $VIGIL_JUDGE_URL/v1/predict` and fires when the answer
matches the watch's `judge_rule`. The request body is
`{"state": "...", "questions": {ID: {"type": "noul" | "choice" | "score", "instructions": "...", "criteria": ...}}}`.
The answer per question is `noul` (the probability of yes) with `confidence`,
`choice` with `probabilities`, or `score`. `docs/findings.md` section 1 has the
full schema. vigil calls `GET $VIGIL_JUDGE_URL/health` when you create a judge
watch and in `watch_status`.

**Without it.** Watches with `fire = "change"`, `"condition"` or `"hold"`
never call the judge. Creating a `fire = "judge"` watch fails at once:

```text
error: fire = judge needs the judge (a Jev-style typed-decision endpoint, such as Laya), and none answered at http://127.0.0.1:4010: start one, or set VIGIL_JUDGE_URL to where it listens. fire = change, condition and hold do not need a judge
```

If the judge goes down after the watch exists, a change still reaches the
agent within three ticks, marked as not judged.

**How to enable it.** Run a judge and point vigil at it. The default is
`http://127.0.0.1:4010`.

```sh
export VIGIL_JUDGE_URL=http://127.0.0.1:4010
```

Set the variable where the agent harness starts `vigil mcp`, for example in
the `env` of the MCP server entry.

## uw (usagewindow)

usagewindow is a local daemon that tracks usage limits. Its CLI is `uw`.

**What vigil does with it.** A main-session ("orchestrator") call checks every
4 minutes whether the Claude account has run past a usage window and is
spending usage credits. It runs `uw --json status` and counts only the
`ClaudeCode` windows: any window with `exceeded: true` means on credits. When
the state changes, the call returns a `credit_state_changed` event and runs the
hook in `VIGIL_ON_CREDIT_CHANGE`, if set, with the new state as JSON on stdin.

**Without it.** A missing `uw` is handled exactly like a `uw` that fails or
answers too slowly. vigil prints nothing about it and reads the newest Claude
Code transcripts instead. If neither source can answer, the state is "unknown"
and vigil keeps the last known state.

**How to enable it.** Install usagewindow so `uw` is on the `PATH` that
`vigil` runs with, or set `VIGIL_UW_COMMAND` to its full path.

**Suggested advisory.** This advisory tells every agent when the Claude
account is on usage credits. It needs `jq`.

```toml
[advisories.claude-credits]
probe = { kind = "cmd", argv = ["sh", "-c", "uw --json status | jq '[.usage[] | select(.provider == \"ClaudeCode\") | .windows[] | select(.exceeded)] | length'"], timeout_s = 10 }
extract = { name = "exceeded", regex = "(\\d+)" }
condition = "exceeded >= 1"
message_when_true = "Claude is past a usage window ({exceeded} exceeded) and spends usage credits: keep reviews and subagents small."
message_when_false = "Claude usage windows have headroom."
cache_s = 300
```

## boat

Boat is a cloud sandbox service. Its CLI is `boat`.

**What vigil does with it.** The first time vigil reads the advisories file
(`~/.config/vigil/advisories.toml`, or `VIGIL_ADVISORIES_FILE`) and finds none,
it creates one. If `boat` is on `PATH` at that moment, the new file holds a
`boat-budget` advisory. That advisory runs `env BOAT_NO_UPDATE=1 <path to boat>
limits --json`, adds the plan's remaining hours to the prepaid credit hours,
and tells agents to prefer Boat for heavy work until 10 hours or fewer are
left. vigil stores the full
path it found, so a hook started with a shorter `PATH` still runs the same
`boat`.

**Without it.** The new file has no active advisory. It holds a header that
says `boat` was not found and the same advisory as a commented example.
`vigil advice` prints nothing.

**How to enable it.** Install `boat` before vigil first runs. If the file
already exists, vigil never adds the advisory by itself. Uncomment the
example, or add it with `vigil advisory add`:

```toml
[advisories.boat-budget]
probe = { kind = "cmd", argv = ["env", "BOAT_NO_UPDATE=1", "boat", "limits", "--json"], timeout_s = 15 }
extract = { name = "hours", json = "planIncluded.vmHoursRemaining+creditBalanceHours" }
condition = "hours <= threshold"
message_when_true = "Boat has {hours} hours left, plan and credit together (at or below {threshold}): run heavy jobs (builds, test suites) on this host, through a queue such as qex, instead of Boat."
message_when_false = "Boat has {hours} hours left, plan and credit together (above {threshold}): prefer Boat for heavy work (builds, test runs); run only one or two quick jobs locally."
cache_s = 600

[advisories.boat-budget.params]
threshold = 10
```

Change the threshold at runtime with `vigil advisory set boat-budget
threshold=15`.

## qex

qex is a local job queue: https://github.com/stephenc/qex.

**What vigil does with it.** Two hold items read qex:

- `qex:JOB` ends when the qex job ends. vigil runs `qex status JOB --json
  --no-logs` and stores the full job id.
- `idle:DIR` (or `--until-idle`, which means `idle:` the current directory)
  ends when no qex job at or under `DIR` is queued or running. vigil runs
  `qex list --under DIR --state active --json`.

**Without it.** Creating a hold with either item fails at once and writes
nothing. The same check covers `vigil hold add`, the `hold` and `hold_add`
tools, and `watch_create` with a `qex` or `qex_idle` input:

```text
error: qex:abc123 needs qex, a local job queue, and `qex` is not on PATH: install it (https://github.com/stephenc/qex), or wait on the work another way (pid:N, file:PATH, until:RFC3339, none)
```

The other items (`pid:N`, `file:PATH`, `until:RFC3339`, `none`) do not need
qex.

**How to enable it.** Install qex so it is on the `PATH` that `vigil` runs
with (`cargo install qex`, or a release binary).

```sh
qex submit --name build --cpu 2 --mem 2GB -- cargo build
vigil hold --for qex:build --expect-s 600
```

## T3 Code

T3 Code runs agent threads and delegate tasks. vigil has no T3 Code code, and
the state of a delegate task lives in the T3 Code server, where vigil cannot
see it.

**What vigil does with it.** Nothing directly. To keep a session's cache warm
while a delegate task runs, hold on a `none` item: a delegated run with no pid
or file to watch. It ends only when you say so.

```sh
vigil hold --id work --for none --label review --expect-s 900
# later, when the delegate reports back:
vigil hold done work review
```

`--expect-s` gives a quiet overdue note at 1.5 times the estimate (at most 9
minutes over) and one interruption at twice the estimate. If the delegated
run writes a file when it finishes, `file:PATH` ends the item by itself.

**Without it.** Nothing changes.

## BabelTele

BabelTele is a compact session-summary format. vigil does not produce or read
it.

**What vigil does with it.** When an agent's work is done it calls `cold`.
`cold` cancels the session's watches and answers with a `next` instruction:

```text
Going cold: post a short summary of this session (task, progress, in-flight ids, tools, the user's standing rules), then end your turn. Nothing will wake you.
```

`VIGIL_COLD_SUMMARY` replaces the part between "Going cold: " and ", then end
your turn". The MCP server instructions use the same text.

**Without it.** Agents post a short plain summary.

**How to enable it.** Set the variable where the harness starts vigil:

```sh
export VIGIL_COLD_SUMMARY="post an inline BabelTele summary"
```

Keep the phrase short. Codex reads only the first 512 characters of the
server instructions, and the default instructions use about 490 of them.

## Claude Code and Codex settings

`keepalive` blocks for up to 270 seconds by default (up to 3000 seconds for a
main session on a 1-hour prompt cache). Both harnesses need a setting so that
one call stays one round trip.

**Claude Code.** Claude Code moves any MCP call that runs longer than 2
minutes to a background task, so a 270-second `keepalive` costs two model
round trips. Raise that limit in `~/.claude/settings.json`:

```json
{
  "env": { "CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS": "280000" }
}
```

Register the server:

```sh
claude mcp add vigil -- vigil mcp
```

**Codex.** Codex stops a tool call after `tool_timeout_sec`, and progress
notifications do not reset that timer. Give vigil 330 seconds in
`~/.codex/config.toml`:

```toml
[mcp_servers.vigil]
command = "vigil"
args = ["mcp"]
tool_timeout_sec = 330
```

**Advice in a hook.** `vigil advice` prints one `id: message` line per
advisory and exits 0 even when a probe fails. A PreToolUse hook in either
harness can run it before heavy shell commands (builds, test suites) and pass
the lines to the agent as extra context. Make the hook do nothing when
`vigil advice` fails or prints nothing, so a broken probe never blocks a
command.
