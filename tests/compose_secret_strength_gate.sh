#!/usr/bin/env bash
# ============================================================================
# THIS FILE IS A SHIM. The gate is Rust:
#   crates/zeroship-gatekit/src/secret_strength.rs   the gate
#   crates/zeroship-gatekit/src/compose.rs           the shared compose model
#   crates/core/src/config/secrets.rs                PLATFORM_SECRETS, the rules
#
# It stays a shell entry point only so CI (.github/workflows/ci.yml) and any
# human who knows this path keep working. Everything it used to do - deriving
# the rule set, parsing the compose YAML, counting, refusing - moved into the
# binary it execs. Read the Rust for the WHY; the short version is:
#
#   The rule set used to be derived by regexing the product's own refusal
#   MESSAGES out of crates/core/src/config/secrets.rs. 2c56e92a3 (2026-08-13)
#   replaced the baked-in variable name in those messages with a `{label}`
#   format parameter - a correct change - and the regex matched nothing from
#   that day on. The gate's anti-vacuity guard fired, so it went RED rather
#   than falsely green, and the invariant went unenforced for seven days.
#
#   The rules are now a const slice the product's own generator and validators
#   read, so a change to them is a change to compiler-checked data. The
#   anti-vacuity guard survives, in zeroship_gatekit::report::Report: there is
#   no path from zero checks to a green.
#
# The binary is built with `cargo run`, so this needs a Rust toolchain where
# the shell version needed only grep. That is the cost of the port and it is
# paid once per CI job, which already compiles this workspace.
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

exec cargo run \
  --quiet \
  --manifest-path "$ROOT/Cargo.toml" \
  --package zeroship-gatekit \
  --bin compose-secret-strength \
  -- "$ROOT/deploy/compose/docker-compose.yml"
