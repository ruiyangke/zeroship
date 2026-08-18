# shellcheck shell=bash
# ============================================================================
# scratch_db.sh - a per-run database name, so two suites cannot destroy each
# other.
#
# WHY THIS EXISTS
# ---------------
# run_auth_suite.sh and run_billing_suite.sh each begin by running
# `DROP DATABASE IF EXISTS <name> WITH (FORCE); CREATE DATABASE <name>;`
# against a FIXED name. WITH (FORCE) terminates every other backend on that
# database first, so the drop always succeeds - including when the other
# backend is a second suite run that is fifteen minutes into its own work.
#
# MEASURED 2026-08-16: one agent's auth run dropped `zeroship_auth_test` out
# from under another agent's auth run. The victim did not error on the drop; it
# reported ordinary-looking test failures, and three of them naming a
# deterministic signing key were investigated as product defects before the
# collision was found. A fixed name plus a warning comment does not fix that,
# because the warning is read after the run, by the person holding the wrong
# failures.
#
# So the name carries a per-run token and the run drops what it created. Two
# concurrent runs then touch disjoint databases by construction: pid is unique
# among live processes, and the nanosecond clock separates a reused pid from
# the run that held it before.
#
# HONOURING AN EXPLICIT $TEST_DB is not a convenience - it is the workaround
# everyone has been using (the dev server on :5440 carries a dozen
# `zeroship_auth_test_*` databases named by hand), and it is how a caller
# inspects a failed run's data afterwards. An explicitly named database is
# therefore also NEVER dropped on exit: the caller owns its lifetime.
#
# WHAT THIS DOES NOT BUY YOU. Per-run names make concurrent runs SAFE, not
# unconditionally green: they still share one server. MEASURED 2026-08-17, two
# auth suites started together against the dev Postgres on :5440
# (max_connections 100, nothing else connected): the pair peaked at 96 of 100
# backends, one finished 637 passed / 0 failed and the other lost 4 tests to
# `SqlState 53300 sorry, too many clients already`. Both ran all 77 test
# binaries and neither lost its database, which is the property this file is
# about; the ceiling is the server's, and a third concurrent suite needs a
# bigger max_connections rather than a change here.
#
# tests/lib_scratch_db_selftest.sh covers both directions of both functions.
# ============================================================================

# Resolve $TEST_DB for a suite, and record whether this run owns it.
#
# Usage: zs_scratch_db_resolve <prefix>
# Sets:  TEST_DB, ZS_SCRATCH_DB_GENERATED (1 = this run created it and must
#        drop it, 0 = the caller named it and owns it).
zs_scratch_db_resolve() {
  local prefix="$1"

  if [ -n "${TEST_DB:-}" ]; then
    ZS_SCRATCH_DB_GENERATED=0
    export TEST_DB ZS_SCRATCH_DB_GENERATED
    return 0
  fi

  # SKIP_DB_RECREATE means "reuse the database from an earlier run". With a
  # per-run name there is nothing to reuse - the earlier run dropped its own
  # database and no fixed name exists to fall back to. Refusing is the honest
  # answer; silently generating a fresh name would migrate nothing and then
  # fail every test on a missing schema, which reads as a product regression.
  if [ -n "${SKIP_DB_RECREATE:-}" ]; then
    echo "FATAL: SKIP_DB_RECREATE needs an explicit TEST_DB." >&2
    echo "       Suite databases are now named per run, so there is no fixed" >&2
    echo "       name to reuse. Re-run naming the database you want:" >&2
    echo "         TEST_DB=${prefix}_reuse SKIP_DB_RECREATE=1 <suite>" >&2
    echo "       An explicitly named database is never dropped on exit." >&2
    return 2
  fi

  # pid + nanoseconds, the same shape the migrate-adapter tests use for their
  # token-suffixed schemas (crates/zeroship-migrate-adapter/tests/
  # smoke_apply_pg.rs:80). Identifier length is not a concern: the longest
  # prefix here is 22 characters, leaving 41 of Postgres's 63-byte limit for a
  # 7-digit pid and a 19-digit nanosecond count.
  TEST_DB="${prefix}_$$_$(date +%s%N)"
  ZS_SCRATCH_DB_GENERATED=1
  export TEST_DB ZS_SCRATCH_DB_GENERATED
}

# Drop the database this run created. A no-op when the caller named it.
#
# Called from an EXIT trap, so it runs on success, on failure, and on the
# `set -e` path. It must never change the exit status: `return 0` at the end is
# load-bearing, since a trap whose last command fails would rewrite a green
# run's status.
#
# The caller must define `run_psql` (both suites already do) before installing
# the trap. Absent it there is no way to reach the server and the database
# would be leaked silently, so say so instead.
zs_scratch_db_cleanup() {
  [ "${ZS_SCRATCH_DB_GENERATED:-0}" = "1" ] || return 0
  [ -n "${TEST_DB:-}" ] || return 0

  if ! declare -F run_psql >/dev/null 2>&1; then
    echo "WARN: no run_psql; leaving ${TEST_DB} behind." >&2
    return 0
  fi

  # WITH (FORCE) for the same reason the create path uses it: this run's own
  # connections may outlive the test process by a moment, and a plain DROP
  # would fail on them and leak the database.
  run_psql -d postgres -c "DROP DATABASE IF EXISTS ${TEST_DB} WITH (FORCE);" \
    >/dev/null 2>&1 \
    || echo "WARN: could not drop ${TEST_DB}; drop it by hand." >&2
  return 0
}
