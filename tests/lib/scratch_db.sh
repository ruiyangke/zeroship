# shellcheck shell=bash
# ============================================================================
# scratch_db.sh - a per-run database name, so two suites cannot destroy each
# other.
#
# Reusing a fixed database name and dropping it with FORCE can terminate
# another suite's connections and turn fixture collisions into apparent product
# failures. Each run must own the database it creates and removes.
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
# THE SECOND WAY A PRIVATE DATABASE IS NOT ISOLATION, and the reason the
# paragraph above was not the whole story: a per-run database does not isolate
# CLUSTER-GLOBAL objects, because they are not in any database. The platform
# migrations create roles, role memberships and role-level `search_path`
# settings, which live in the shared catalogs `pg_authid`, `pg_auth_members`
# and `pg_db_role_setting`. Two suites migrating two private databases on one
# cluster write those same rows, and PostgreSQL aborts one of them with
# `tuple concurrently updated` or a duplicate key on `pg_authid_rolname_index`
# -- an infrastructure error with NO test name attached, which reads like
# flakiness or like the reader's own change.
#
# NOTHING HANDLES THAT ANY MORE, AND THIS PARAGRAPH USED TO SAY SOMETHING DID.
#
# Until 2026-08-28 it read: "That is now handled INSIDE
# `zeroship-platform-migrate`, which takes a cluster-wide advisory lock in a
# coordination database around exactly the migrations whose SQL writes a shared
# catalog." That was true, and the binary carrying the lock
# (`crates/zeroship-migrate-adapter/src/platform/cluster_lock.rs`, 451 lines) was
# deleted with it when the platform schema moved to the `zero-migrate` CLI.
#
# WHAT THE CLI HAS INSTEAD, and why it is not the same thing. It brackets each
# apply in a two-`int4` advisory lock keyed by `hashtextextended(project schema, 0)`
# (crates/zeroship-migrate-postgres/src/backend/session.rs:60-77). A PostgreSQL
# advisory lock tag carries MyDatabaseId, so that lock is DATABASE-SCOPED: two
# suites holding the same key in two scratch databases do not exclude each other
# at all. It is also acquired and released PER MIGRATION FILE rather than once
# per run (crates/zeroship-migrate-node/src/verbs.rs:332 and :453, driven by the
# per-file loop in packages/zero-migrate-cli/src/cli.ts:1420), so even
# same-database runs interleave at file boundaries.
#
# So the race described above is BACK. It is probabilistic rather than certain -
# the 2026-08-17 two-auth-suite measurement did not trigger it - and the symptom
# is an infrastructure error with no test name attached:
#   ERROR:  tuple concurrently updated
#   ERROR:  duplicate key value violates unique constraint "pg_authid_rolname_index"
#   ERROR:  duplicate key ... "pg_db_role_setting_databaseid_rol_index"
# If you see one of those while two suites are running, this is why. Nothing in
# this file can prevent it; the fix belongs wherever the cluster-wide lock is
# rebuilt, and today it is nowhere.
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

  # pid + nanoseconds, the same shape the migrate-server PG tests use for their
  # token-suffixed schemas (crates/zeroship-migrate-server/tests/
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

# THE OTHER LIFETIME POLICY: drop on SUCCESS ONLY.
#
# Usage, from an EXIT trap, with the run's status as the argument:
#   cleanup() { local rc=$?; ...; zs_scratch_db_cleanup_on_success "$rc"; }
#
# WHY BOTH POLICIES EXIST, because one of them being wrong everywhere would be
# simpler and is the first thing a reader will suspect. `zs_scratch_db_cleanup`
# above drops unconditionally, and its callers argue for that in their own
# comments: golden_path.sh and the dev-vs-deployed harnesses run unattended, on
# a per-run name nothing will ever reuse, so a database kept for a failure
# nobody is watching is a leak with no reader. The harnesses that use THIS
# function are the ones a human runs by hand and then investigates - the Stripe
# rails, where the failure is usually "what did control actually write", and the
# answer is rows.
#
# THE STATUS IS AN ARGUMENT AND HAS NO DEFAULT. A default of 0 would drop a
# failed run's evidence the day someone installed the trap without it; a
# default of 1 would leak on every green run. Called with nothing, this
# REFUSES: it keeps the database and says the call site is wrong, which is
# recoverable in a way that either silent default is not.
zs_scratch_db_cleanup_on_success() {
  local status="${1-}"

  case "$status" in
    ''|*[!0-9]*)
      echo "WARN: zs_scratch_db_cleanup_on_success needs the run's exit status" >&2
      echo "      as its argument; got '${status}'. Keeping ${TEST_DB:-<unset>}." >&2
      return 0
      ;;
  esac

  [ "${ZS_SCRATCH_DB_GENERATED:-0}" = "1" ] || return 0
  [ -n "${TEST_DB:-}" ] || return 0

  if [ "$status" -ne 0 ]; then
    echo "  DB KEPT for inspection: ${TEST_DB} (the run exited ${status})." >&2
    echo "    drop it with: psql -d postgres -c 'DROP DATABASE ${TEST_DB}'" >&2
    return 0
  fi

  if ! declare -F run_psql >/dev/null 2>&1; then
    echo "WARN: no run_psql; leaving ${TEST_DB} behind." >&2
    return 0
  fi

  # WITH (FORCE) is correct HERE and nowhere near a sweeper: this run created
  # this database, so the only backends left on it are its own stragglers, and
  # a plain DROP would fail on them and leak it. tests/lib/sweep_db.sh carries
  # the other half of that asymmetry.
  run_psql -d postgres -c "DROP DATABASE IF EXISTS ${TEST_DB} WITH (FORCE);" \
    >/dev/null 2>&1 \
    || echo "WARN: could not drop ${TEST_DB}; drop it by hand." >&2
  return 0
}
