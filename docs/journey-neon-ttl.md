# Neon cache lifetime

The 2026-11-21 pilot, `8d30697`, added Antigravity detection and a
180-second orchestrator window using the measured 4-5 minute conversation
cache lifetime. It left explicit Antigravity requests at the Unknown
3000-second ceiling and did not update the specs.

Opus review found that the wire enum, window rules and mixed identity rule
no longer described the implementation. Fix round 1 updates those specs and
records that the markers were observed in an agy shell child; the MCP child
is still unverified.

The owner requested a 270-second explicit Antigravity ceiling because the
MCP timeout is unknown and the cache lasts 4-5 minutes. The old test's
3000-second expectation changed to 270. A new boundary test covers explicit
0, 180, 269, 270, 271 and 9999 for both caller roles. Both tests failed on
the old implementation on Boat before moving Antigravity into the existing
Codex/Ambiguous ceiling arm. No new timeout mechanism was needed.

Evidence and final checks are in
`mstack.research/neon-trial/report-neon-ttl-fix1.md` beside the original brief
and pilot report. Live MCP marker propagation and timeout remain unverified.

## Live check, defect, fix (2026-11-21)

Live check (04:14-04:21 MST, 11:14-11:21 UTC): `agy --print --model
gemini-4-neon` creating a hold and calling keepalive once over MCP.
Both runs failed after exactly 3m0s with `MCP tool call to server "vigil" timed
out after 3m0s`: agy's MCP tool timeout is 180 s and has no config key.
Run 2 (`env -i`) showed the `vigil mcp` child carries no `ANTIGRAVITY_*`
variable; its parent process name is `agy`. So vigil saw Neon as Unknown and
gave it 270 s, and even the 180 s Antigravity window from the earlier pilot
would have equalled the timeout.

Dead end: raising nothing. The fix is on vigil's side only. The window for
every Antigravity call is 150 s (auto, non-orchestrator default, explicit
ceiling), leaving room for the post-window advisory tail, which is now capped
at 15 s in total. Antigravity is also detected from the process tree (any
ancestor named `agy`, names and parent pids only), and any Antigravity signal
mixed with Claude or Codex keeps the stricter bound instead of the old 270 s
ambiguous ceiling (live run 1 had Claude env inherited plus an `agy` parent).
Red tests were committed first; the previous round's 180 s and 270 s
expectations for Antigravity changed to 150 s.
