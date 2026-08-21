#!/usr/bin/env bash
# ============================================================================
# THIS FILE IS A SHIM. The gate is Rust:
#   crates/zeroship-gatekit/src/backing_service_reach.rs  the gate, and the WHY
#   crates/zeroship-gatekit/src/compose.rs                the shared model
#
# IT IS RED ON THE TRACKED TREE, deliberately, and it is deliberately NOT a step
# in .github/workflows/ci.yml. Redpanda runs in deploy/compose and nothing is
# configured to reach it, so a correct gate fails here today; softening it to
# land green would have made it a gate that passed for the whole 43 days the gap
# existed (docs/proposals/2026-08-20-metering-transport-not-configured.md).
# WIRE THIS INTO ci.yml IN THE SAME COMMIT THAT CONFIGURES THE BROKER, and not
# before: a red required step on unrelated work teaches people to ignore it.
#
# The redness is pinned by crates/zeroship-gatekit/tests/real_compose_gates.rs,
# so a change that makes this gate green without configuring the broker fails a
# cargo test rather than passing quietly.
#
# A compose path may be passed as argv to point the gate at a scratch copy; CI
# passes none.
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CF="${1:-$ROOT/deploy/compose/docker-compose.yml}"

# THE SHIM STILL OWES AN ACCOUNT: gate_arms_delegate refuses if the binary emits
# no `zsgate-arm` line, and re-applies every floor to the numbers it forwards.
# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init compose_backing_service_reach

gate_arms_delegate cargo run \
  --quiet \
  --manifest-path "$ROOT/Cargo.toml" \
  --package zeroship-gatekit \
  --bin compose-backing-service-reach \
  -- "$CF"
status=$?

gate_arms_finish || status=1
exit "$status"
