#!/usr/bin/env bash
# PILOT verification for quiet holds. Run from the vigil crate, on a build host
# or through qex; this deliberately avoids a workspace-wide test run.
set -euo pipefail
for suite in hold keepalive::tests items::tests envelope::tests attention::tests auto_interval_tests orchestrator::tests disk_full_tests peers_tests; do
  cargo test -j2 --lib "$suite" -- --test-threads=2
done
cargo test -j2 --test quiet_hold_e2e --test cli_e2e --test mcp_e2e -- --test-threads=2
