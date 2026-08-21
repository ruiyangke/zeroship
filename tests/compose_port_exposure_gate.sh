#!/usr/bin/env bash
# ============================================================================
# THIS FILE IS A SHIM. The gate is Rust:
#   crates/zeroship-gatekit/src/port_exposure.rs   the gate, and the WHY
#   crates/zeroship-gatekit/src/compose.rs         the shared compose model
#
# It stays a shell entry point so CI (.github/workflows/ci.yml), any human who
# knows this path, and `gate-arm-census` - which enumerates tests/*_gate.sh and
# would otherwise stop seeing this gate at all - keep working. Everything it
# used to do moved into the binary it execs: the block-aware awk that read
# `ports:` blocks is now a YAML parse, and the two Caddyfile greps are now
# anchored matchers with their own one-variable tests.
#
# The binary is built with `cargo run`, so this needs a Rust toolchain where the
# shell version needed only awk and grep. That is the cost of the port, paid
# once per CI job, which already compiles this workspace.
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# THE SHIM STILL OWES AN ACCOUNT. It has no counts of its own - the binary has
# them - so it delegates, and gate_arms_delegate refuses if the binary emits no
# `zsgate-arm` line and re-applies every floor to the numbers it forwards.
# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init compose_port_exposure

gate_arms_delegate cargo run \
  --quiet \
  --manifest-path "$ROOT/Cargo.toml" \
  --package zeroship-gatekit \
  --bin compose-port-exposure \
  -- "$ROOT/deploy/compose/docker-compose.yml" "$ROOT/deploy/ops/Caddyfile"
status=$?

gate_arms_finish || status=1
exit "$status"
