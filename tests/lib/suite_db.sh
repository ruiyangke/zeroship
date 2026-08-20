# shellcheck shell=bash
# ============================================================================
# suite_db.sh - the SHARED database the live-Postgres test GATES run against,
# named after the schema they need rather than after whoever launched them.
#
# THE LOGIC IS NOW IN RUST: crates/zeroship-testkit/src/suite_db.rs (the
# decisions), src/admin.rs (the server), src/fingerprint.rs (the name), reached
# through `zs-testkit suite-db`. This file is the shell BINDING - it keeps the
# function names and the exported-variable contract tests/run_auth_suite.sh and
# tests/run_billing_suite.sh already source, so neither of them changed. Those
# modules carry the design notes: why the name is a hash of the migration set,
# what a shared database does and does not isolate, and the measurements behind
# the provisioning lock. What follows is only what a SHELL caller needs.
#
# NOT tests/lib/scratch_db.sh. That file serves the e2e stack scripts, which
# stand up a whole platform and want a virgin database that dies with the run.
# This file serves the suite gates, which run thousands of tests that already
# scope their own fixtures and want a database that OUTLIVES the run.
#
# NOTHING IS EVER DROPPED HERE, AND `WITH (FORCE)` APPEARS NOWHERE. A failed
# suite's database is the primary debugging artifact, and a shared database is
# by definition not this run's to destroy. scratch_db.sh DOES use WITH (FORCE),
# correctly - it drops a database this run created, where the only connections
# left are its own. Reclaiming space here is tests/sweep_test_databases.sh's
# job, and the sweeper must never use FORCE either: a live connection to a
# database it is considering is a peer agent mid-suite, and FORCE would
# terminate it.
#
# THE OVERRIDE IS A FLAG, NEVER AN ENVIRONMENT VARIABLE. `--database <name>` on
# the suite script. An ambient `TEST_DB=...` is REFUSED rather than ignored: a
# variable that silently redirects a gate is how gates get silently disabled,
# and refusing is the only behaviour that cannot be inherited from a shell the
# caller forgot about. This file reads TEST_DB out of its OWN environment and
# passes what it found as an argument - the binary never reads it, so the thing
# doing the refusing is not itself steerable by the variable it refuses.
#
# THERE IS NO `run_psql` SEAM ANY MORE. It existed because shell has no
# database client; the Rust side connects with compio-postgres. A caller that
# still defines `run_psql` for its own probes (run_auth_suite.sh does) is
# unaffected - nothing here consults it.
#
# tests/lib_suite_db_selftest.sh covers this file. The cases that scripted a
# fake `run_psql` cannot drive a separate process and are covered instead by
# `cargo test -p zeroship-testkit` and crates/zeroship-testkit/tests/.
# ============================================================================

# shellcheck source=tests/lib/testkit.sh
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/testkit.sh"

# The repository this library was sourced from.
zs_suite_db_root() {
  (cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
}

# Hash the migration set the platform runner would apply from this tree.
#
# Usage: zs_schema_fingerprint <repo-root>
# Prints: 12 lowercase hex characters. Exit 2 if the directory is missing or
#         holds no migrations - never a fingerprint of nothing, which would be
#         a stable name shared by every broken checkout.
zs_schema_fingerprint() {
  zs_testkit fingerprint dir --root "$1"
}

# Resolve $TEST_DB for a suite gate.
#
# Usage: zs_suite_db_resolve <prefix> <explicit-name-or-empty> <repo-root>
# Sets:  TEST_DB, ZS_SUITE_DB_EXPLICIT (1 = the caller named it on the command
#        line, 0 = derived from the migration set).
zs_suite_db_resolve() {
  local prefix="$1" explicit="${2:-}" root="$3" assignments status

  assignments="$(zs_testkit suite-db resolve \
    --prefix "$prefix" \
    --explicit "$explicit" \
    --root "$root" \
    --ambient-test-db "${TEST_DB:-}" \
    --ambient-skip-db-recreate "${SKIP_DB_RECREATE:-}")"
  status=$?
  [ "$status" -eq 0 ] || return "$status"

  eval "$assignments"
  export TEST_DB ZS_SUITE_DB_EXPLICIT
}

# A database name reaches `CREATE DATABASE` unquoted, so it must be a bare
# identifier. This is the only caller-controlled string in that statement.
zs_suite_db_check_identifier() {
  zs_testkit suite-db check-identifier --name "$1"
}

# Does a database exist on the server the overlay names?
#
# Three-valued on purpose. 0 = present, 1 = absent, 2 = COULD NOT TELL. Under
# psql the last two were the same two bytes of nothing and had to be separated
# by hand; the driver distinguishes them, and reading "could not tell" as
# "absent" would turn an unreachable server into a CREATE DATABASE that fails
# for reasons nobody can name.
zs_suite_db_exists() {
  zs_testkit suite-db exists --root "$(zs_suite_db_root)" --name "$1"
}

# Create the suite database if it is not there yet. Never drops, never
# recreates, and tolerates losing the create race to a concurrent run.
#
# The caller must run the migration itself afterwards - unconditionally, not
# only when this created something. A run that dies between CREATE and the end
# of its migration leaves a partially journalled database, and the next run's
# migrate is what finishes it.
zs_suite_db_ensure() {
  local name="${TEST_DB:?zs_suite_db_ensure before zs_suite_db_resolve}"
  zs_testkit suite-db ensure --root "$(zs_suite_db_root)" --name "$name"
}

# Create the database if absent, then run the caller's migrate command - with
# both steps serialized against every other run on this machine targeting the
# same database.
#
# Usage: zs_suite_db_provision <migrate command...>
#
# The lock covers PROVISIONING ONLY and is released before the caller's tests
# start. A suite run is tens of minutes; serializing that would mean two agents
# never overlap, which is the entire property this file exists to deliver.
zs_suite_db_provision() {
  local name="${TEST_DB:?zs_suite_db_provision before zs_suite_db_resolve}"
  zs_testkit suite-db provision \
    --root "$(zs_suite_db_root)" \
    --name "$name" \
    --lock-dir "${TMPDIR:-/tmp}" \
    -- "$@"
}
