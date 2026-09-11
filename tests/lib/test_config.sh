# shellcheck shell=bash
# ============================================================================
# test_config.sh - the shell half of the test overlay.
#
# THE LOGIC IS NOW IN RUST: crates/zeroship-testkit/src/overlay.rs, reached
# through `zs-testkit overlay`. This file is the shell BINDING - it keeps the
# function names and the exported-variable contract that tests/run_auth_suite.sh
# and tests/run_billing_suite.sh already source, so neither of them changed.
# Read the Rust module for what the reader parses and why it is not the real
# config parser.
#
# WHAT THIS REPLACES (unchanged, and still the reason the overlay exists).
# Every suite carried its own copy of the test backends' coordinates:
#
#   tests/run_auth_suite.sh:65-68       PG_HOST/PG_PORT/PG_USER/PG_PASS
#   tests/run_billing_suite.sh:124-127  the same four lines again
#
# and then exported the resulting DSN under whichever names the crates it ran
# happened to read - AUTH_DB_URL, CONTROL_TEST_DB, MIGRATE_SERVER_TEST_DB,
# GATEWAY_ANCHORS_DB_URL, GATEWAY_POOL_SMOKE_URL. Two copies of the address
# drift; five names for one value means a crate whose name nobody exported runs
# against nothing and reports passes.
#
# `tests/provision_test_backends.sh` writes `deploy/ops/zeroship.test.toml` in
# the platform's own config schema, and this reads it. Rust SERVICE code reads
# the same document through `zeroship_core::config::test_overlay`, which parses
# it with `FileConfig` under `deny_unknown_fields`, so a typo cannot survive
# `cargo test -p zeroship-core`.
#
# WHAT IT DOES NOT DO. It does not invent a DSN when the file is missing. A
# suite that silently falls back to a compiled default when its configuration
# is absent is the deleted ZEROSHIP_REQUIRE_LIVE_BACKENDS flag in another
# costume: it converts "there is no configuration" into "the run passed".
#
# WHY THE FOUR WANTED VALUES GO IN ON STDIN. `PG_PASS` on a command line is
# readable by every user on the box through `ps` and through /proc/<pid>/cmdline
# - which tests/lib/sweep_db.sh's own scanner reads by design. They travel as a
# key=value block instead. Nothing about them is read by the Rust side from the
# environment: this file reads its own environment and passes what it found,
# which is what keeps the binary free of ambient configuration.
# ============================================================================

# shellcheck source=tests/lib/testkit.sh
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/testkit.sh"

# Read one `section.key` out of the generated test overlay.
#
# Usage: zs_test_config_get <section> <key>
# Prints the value, or nothing when the key is absent.
zs_test_config_get() {
  local section="$1" key="$2"
  local file="${ZS_TEST_OVERLAY:?zs_test_config_get before zs_test_config_load}"
  zs_testkit overlay get --file "$file" --section "$section" --key "$key"
}

# Locate and validate the generated test overlay, then export the values every
# harness needs.
#
# Sets, all exported:
#   ZS_TEST_OVERLAY  path to the overlay that was read
#   PG_HOST PG_PORT PG_USER PG_PASS PG_DB   parsed back out of the DSN
#   ZS_TEST_PG_DSN   the server DSN as written (database = PG_DB)
#
# The PG_* names are set for the harnesses' own database calls. They are also
# still honoured as INPUTS by the provisioner, which is the override tier: set
# one there and the overlay this reads is written to match. Setting one HERE
# without regenerating the overlay is refused - see below.
#
# ON A REFUSAL NOTHING IS SET, which the shell version did not manage: it
# exported PG_* and then ran the agreement check, so a refused load left the
# caller's own PG_PORT overwritten by the overlay's. No consumer read them
# afterwards - all four do `|| exit 2` - so this is a difference nobody can
# observe today, recorded because it is a difference.
zs_test_config_load() {
  local root assignments status

  root="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"

  # The command substitution swallows stdout only; the refusal text goes to
  # stderr and reaches the caller's terminal unchanged.
  assignments="$(
    printf 'host=%s\nport=%s\nuser=%s\npass=%s\n' \
      "${PG_HOST:-}" "${PG_PORT:-}" "${PG_USER:-}" "${PG_PASS:-}" \
      | zs_testkit overlay load --root "$root"
  )"
  status=$?
  [ "$status" -eq 0 ] || return "$status"

  eval "$assignments"
  export ZS_TEST_OVERLAY PG_HOST PG_PORT PG_USER PG_PASS PG_DB \
    ZS_TEST_PG_DSN
}

# Refuse when the caller asked for one server and the overlay names another.
#
# WHY A REFUSAL AND NOT A WARNING. A harness reads PG_HOST/PG_PORT for its OWN
# database calls; every SERVICE it spawns reads the overlay through
# `zeroship_core::config::test_overlay`. Point a suite at a second cluster with
# `PG_PORT=5444` and the two halves connect to two different servers - the
# provisioning and the probes on one, the code under test on the other. Nothing
# announces that. The run completes and reports a plausible number computed
# against two databases, which is the worst available outcome: not a failure, a
# WRONG MEASUREMENT that reads like a result.
#
# `zs_test_config_load` already applies this to the values it was given; this
# entry point stays because it is a separately meaningful question - "would
# THESE coordinates be accepted?" - and answering it needs no second copy of
# the rule, only a second load.
zs_test_config_assert_agrees() {
  local want_host="$1" want_port="$2" want_user="$3" want_pass="$4"
  local root
  root="$(cd "$(dirname "${ZS_TEST_OVERLAY:?zs_test_config_assert_agrees before zs_test_config_load}")/../.." && pwd)"
  printf 'host=%s\nport=%s\nuser=%s\npass=%s\n' \
    "$want_host" "$want_port" "$want_user" "$want_pass" \
    | zs_testkit overlay load --root "$root" >/dev/null
}
