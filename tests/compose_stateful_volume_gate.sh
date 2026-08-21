#!/usr/bin/env bash
# ============================================================================
# THIS FILE IS A SHIM. The gate is Rust:
#   crates/zeroship-gatekit/src/stateful_volume.rs  the gate, and the WHY
#   crates/zeroship-gatekit/src/compose.rs          the shared compose model
#
# It stays a shell entry point so CI (.github/workflows/ci.yml), any human who
# knows this path, and `gate-arm-census` - which enumerates tests/*_gate.sh and
# would otherwise stop seeing this gate at all - keep working.
#
# The awk this file used to carry came with a warning about rebuilding `$0` and
# losing the block anchor after exactly one volume; that whole class of hazard
# is gone with the hand-rolled parser.
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# THE SHIM STILL OWES AN ACCOUNT: gate_arms_delegate refuses if the binary emits
# no `zsgate-arm` line, and re-applies every floor to the numbers it forwards.
# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init compose_stateful_volume

gate_arms_delegate cargo run \
  --quiet \
  --manifest-path "$ROOT/Cargo.toml" \
  --package zeroship-gatekit \
  --bin compose-stateful-volume \
  -- "$ROOT/deploy/compose/docker-compose.yml"
status=$?

gate_arms_finish || status=1
exit "$status"
