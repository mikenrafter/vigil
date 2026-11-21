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
