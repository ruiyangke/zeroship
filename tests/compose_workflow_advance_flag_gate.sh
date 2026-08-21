#!/usr/bin/env bash
# ============================================================================
# THIS FILE IS A SHIM. The gate is Rust:
#   crates/zeroship-gatekit/src/workflow_advance_flag.rs  the gate, and the WHY
#
# It stays a shell entry point so CI (.github/workflows/ci.yml), any human who
# knows this path, and `gate-arm-census` - which enumerates tests/*_gate.sh and
# would otherwise stop seeing this gate at all - keep working.
#
# The short version of the WHY, which is on the Rust module in full: the flag
# that would unblock durable workflows in the shipped deployment is the same
# flag that arms the unauthenticated gateway workflow-advance edge (task #354).
# The two were tracked as separate problems by people each looking at one side.
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# THE SHIM STILL OWES AN ACCOUNT: gate_arms_delegate refuses if the binary emits
# no `zsgate-arm` line, and re-applies every floor to the numbers it forwards.
# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init compose_workflow_advance_flag

gate_arms_delegate cargo run \
  --quiet \
  --manifest-path "$ROOT/Cargo.toml" \
  --package zeroship-gatekit \
  --bin compose-workflow-advance-flag \
  -- "$ROOT/deploy" "$ROOT/crates/worker/src/handler.rs"
status=$?

gate_arms_finish || status=1
exit "$status"
