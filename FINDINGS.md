# vigil — phase 0 findings (2026-10-29)

Toolchain: rustc/cargo 1.95.0 (nixos-26.05). Clients installed on host-a:
Claude Code 2.1.301, codex-cli 0.163.0. Spike files: `/tmp/vigil-spike/`
(probe source `rmcp-probe/src/main.rs`, binary `bin/rmcp-probe`; not kept in
the repo).

A minimal rmcp stdio server compiled and completed a real round trip: the
rmcp test client spawned it as a child process, read `instructions`, and got a
tool's text result. The rmcp client attaches a `progressToken` by itself.

## 1. Laya request and response schema

Read from the laya 0.3.6 wheel (`agent.py`, `common.py`).

`questions` is an object keyed by question id, not a list. Three types; there
is no bool, number or text type.

| type | request fields | answer fields |
|---|---|---|
| `noul` (yes/no) | `instructions`, optional `criteria: {"true": "...", "false": "..."}` | `noul` (P(true)), `confidence` |
| `choice` | `instructions`, `criteria`: `{label: description}` or list of labels | `choice`, `probabilities` per label, `confidence` |
| `score` (ordinal) | `instructions`, `criteria`: list of levels | `score` (expected level, float), `legend`, `probabilities`, `confidence` |

Every answer also has `type` and `action: {act_probability}`. Response:
`{"model":"laya-rl-agent","answers":{qid:{...}},"usage":{"input_tokens":N,"output_tokens":0}}`.
Errors: HTTP 400 or 500 with `{"error": "..."}`.

```json
{"state": "...", "questions": {"failed": {"type": "noul", "instructions": "Did the build fail?"}}}
```
```json
{"answers": {"failed": {"type": "noul", "noul": 0.93, "confidence": 0.93, "action": {"act_probability": 0.71}}}}
```

- Laya truncates `state` from the end: it keeps the first tokens (about 768
  on typed-decisions). vigil must send about 2-3 KB, newest lines first.
- Laya runs only on host-c, bound to 127.0.0.1 unless `exposeOnTailscale` is
  set. Unreachable from host-a and host-b (port 4010 refused / timed out).
- Laya's README uses "TypeSafe Jev" for a competing model, so the spec now
  calls the judge "judge" (Laya).

## 2. Claude Code channels: not building

Docs: code.claude.com/docs/en/channels-reference, /channels,
/mcp#mcp-client-runtimes. Capability `experimental["claude/channel"] = {}`;
notification `notifications/claude/channel` with
`{content: string, meta?: Record<string,string>}`; idle sessions are woken.

Not worth it: a custom server needs `--dangerously-load-development-channels
server:vigil` (a confirmation dialog at every start, ignored with `-p` and the
Agent SDK); refused events are dropped with no acknowledgement; and Claude
Code will not register a channel server on protocol 2026-07-28 (rmcp 3.8's
default). Moot under the stateless design anyway.

## 3. Blocking limits

Claude Code (stdio):
- Wall-clock limit about 28 h (`MCP_TOOL_TIMEOUT` or per-server `timeout`);
  progress does not extend it. 30 min with no response or progress aborts the
  call; progress resets that idle timer.
- A main-conversation tool call still running after 2 minutes is moved to a
  background task: the model gets a turn at once and the result arrives later
  as a task notification. A 270 s `keepalive` then costs two model round
  trips. Controlled by `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS` (0 disables); the
  harness's setting, not the server's.
- Server instructions are cut at 4,096 characters. Tool output warns at 10k
  tokens and is capped at 25k.

Codex 0.163.0:
- Docs say `tool_timeout_sec` defaults to 60 s; the source sets
  `DEFAULT_TOOL_TIMEOUT` to 300 s. Hard limit; progress is only logged.

## 4. Codex MCP client

Reads `instructions` from initialize and uses them as the server's tool
description (docs: keep the first 512 characters self-contained). Text tool
results work. Progress is logged only. No channels.

## Pinned crates

All fit rustc 1.95 (rmcp 3.8.0 needs 1.88). Compiled in the probe: rmcp,
tokio, serde, serde_json, schemars, anyhow. File locks use
`std::fs::File::lock` (stable since 1.89); no crate. reqwest has default
features off: no TLS, because its default rustls pulls aws-lc-rs (cmake + C).
URL inputs are http only.

```toml
[dependencies]
rmcp = { version = "=3.8.0", default-features = false, features = ["server", "macros", "schemars", "transport-io"] }
tokio = { version = "1.54.1", features = ["rt-multi-thread", "macros", "time", "io-std", "process", "net", "sync", "fs", "signal"] }
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.154"
schemars = "1.2.3"
reqwest = { version = "0.13.5", default-features = false, features = ["json"] }
sha2 = "0.11.1"
glob = "0.3.4"
chrono = { version = "0.4.45", default-features = false, features = ["std", "clock", "serde"] }
clap = { version = "4.6.7", features = ["derive"] }
tempfile = "3.27.0"
anyhow = "1.0.104"
thiserror = "2.0.21"

[dev-dependencies]
rmcp = { version = "=3.8.0", features = ["client", "transport-child-process"] }
```

## rmcp 3.8 API notes (confirmed by the probe)

- `#[tool_router]` on the impl, `#[tool(description = ...)]` per tool,
  `#[tool_handler]` on `impl ServerHandler`.
- Arguments: `Parameters<T>` with `T: Deserialize + schemars::JsonSchema`. A
  tool can also take `RequestMetaObject` (`get_progress_token()`) and
  `Peer<RoleServer>`.
- Progress: `peer.notify_progress(ProgressNotificationParam::new(tok, n).with_message(..))`.
- Result: `CallToolResult::success(vec![ContentBlock::text(s)])` (the type is
  `ContentBlock`, not `Content`).
- `get_info`: `ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(..)`
  (`ServerInfo` is deprecated).
- Test client: `().serve(TokioChildProcess::new(cmd)?)`, then `peer_info()`
  and `call_tool(...)`.

## Still unverified (needs a live session)

- Claude: `claude mcp add -s local probe -e PROBE_SLEEP_S=270 -- /tmp/vigil-spike/bin/rmcp-probe server`,
  then have it call `mcp__probe__keepalive` once. Repeat with
  `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS=0`, and with `-e PROBE_PROGRESS=0`.
- Codex: `codex mcp add probe --env PROBE_SLEEP_S=270 -- /tmp/vigil-spike/bin/rmcp-probe server`,
  call the tool; then `PROBE_SLEEP_S=320` to confirm the 300 s cutoff.
- Laya: on host-c, POST the example request to `localhost:4010/v1/predict`.
