#!/usr/bin/env bash
# ============================================================================
# run_billing_suite.sh - the CI runner for every zeroship-control /
# zeroship-migrated test that needs a live, migrated PostgreSQL.
#
# WHAT THIS SCRIPT GATES
# ----------------------
# `cargo test --workspace` provisions no database, so any target that dials one
# either fails there or (worse) skips and reports a pass. Those targets carry
# `required-features = ["live-db-tests"]` in crates/control/Cargo.toml and
# crates/migrated/Cargo.toml, which removes them from the default build. This
# script is what runs them, against a database it creates and migrates itself.
#
# NO NAME LIST. Earlier revisions enumerated the binaries here by hand, which
# meant the set that RAN was maintained separately from the set that was GATED,
# and the two drifted: the list held the billing binaries only, so roughly
# fifteen non-billing control suites (deploy, oauth, device, token, admin,
# bootstrap, workflow, ...) were gated out of `cargo test --workspace` and
# picked up by nothing. The invocation below is by construction instead:
#
#     cargo test -p zeroship-control --features live-db-tests
#
# builds and runs EVERY target in the crate whose `required-features` are
# satisfied - the gated ones plus the handful that were never gated. Adding a
# `[[test]]` block with the feature is therefore sufficient to be covered here;
# there is nothing to remember to update, and nothing that can fall out of step.
# The crate's LIB target is built with the feature too, so the in-crate
# `#[cfg(all(test, feature = "live-db-tests"))]` modules (cron::spend_recompute,
# http_util) run in the same invocation.
#
# SERIALISATION (the original constraint, unchanged)
# --------------------------------------------------
# The billing binaries share one database and drive the REAL fleet-wide cron
# sweeps - `billing_reconcile`, `stripe_reconcile`, `billing_notify`,
# `spend_reconcile`, plus the stream forwarder/recompute rail. Each sweep is
# single-flighted FLEET-WIDE in production by a session-scoped
# `pg_try_advisory_lock(<stable key>)`: a second concurrent sweep LOSES the lock
# and returns `Ok(0)`. That lock is a production correctness mechanism
# (multi-instance safety) and is deliberately NOT relaxed for tests.
#
# Consequence: two different test binaries that drive the SAME sweep cannot run
# CONCURRENTLY. Within ONE binary the collision is already handled - each
# serialises its own sweep-driving tests with a process-wide mutex
# (RECONCILE_LOCK / EXPORT_LOCK / ...) and gives every test unique ids and (where
# it can) its own far-future period bucket - so a binary run alone, even
# multi-threaded internally, is reliable.
#
# Cargo runs test binaries ONE AT A TIME (it parallelises within a binary, never
# across them), which is exactly the property the old per-binary loop was
# hand-rolling. `--no-fail-fast` is required so a failure in one binary does not
# stop cargo before it has run the rest: without it the remaining binaries are
# not run at all, and "not run" is indistinguishable from "passed" in the output.
#
# USAGE
# -----
#   tests/run_billing_suite.sh                      # recreate DB + migrate + run
#   SKIP_DB_RECREATE=1 tests/run_billing_suite.sh   # reuse an already-migrated DB
#   PG_PORT=5440 PG_USER=postgres PG_PASS=zeroship tests/run_billing_suite.sh
#
# ENV (defaults target the dev compose Postgres on :5440)
#   PG_HOST (localhost)  PG_PORT (5440)  PG_USER (postgres)  PG_PASS (zeroship)
#   TEST_DB (zeroship_billing_test)
#   PSQL    (auto-detected; override with an explicit psql path)
#   SKIP_DB_RECREATE (unset)  - set to skip the drop/create/migrate step
#   TEST_THREADS (1)          - passed to each binary's `--test-threads`
#   REDPANDA_BROKERS (unset)  - set to gate the real Kafka-wire stream path
#
# The tests never skip for want of a database: CONTROL_TEST_DB / AUTH_DB_URL /
# MIGRATED_TEST_DB are exported below, and with them unset the targets fall back
# to the dev DSN and fail loudly if it is unreachable. A missing database can
# never masquerade as a pass.
# ============================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PG_HOST="${PG_HOST:-localhost}"
PG_PORT="${PG_PORT:-5440}"
PG_USER="${PG_USER:-postgres}"
PG_PASS="${PG_PASS:-zeroship}"
TEST_DB="${TEST_DB:-zeroship_billing_test}"

# psql: prefer an explicit $PSQL, else PATH, else the pinned Nix store path used
# in this repo's runbooks.
PSQL="${PSQL:-}"
if [ -z "$PSQL" ]; then
  if command -v psql >/dev/null 2>&1; then
    PSQL="$(command -v psql)"
  elif [ -x /nix/store/0hzvyg4lmry0cv8pgl1fw9j1rddyqqbj-postgresql-17.7/bin/psql ]; then
    PSQL=/nix/store/0hzvyg4lmry0cv8pgl1fw9j1rddyqqbj-postgresql-17.7/bin/psql
  else
    echo "FATAL: no psql found; set \$PSQL" >&2
    exit 2
  fi
fi

DSN="postgresql://${PG_USER}:${PG_PASS}@${PG_HOST}:${PG_PORT}/${TEST_DB}"
# Three names for one database. The control suite is not consistent about which
# it reads - the billing and registry targets take CONTROL_TEST_DB, the
# auth-adjacent ones (admin / oauth / token / device / bootstrap handlers) take
# AUTH_DB_URL, and zeroship-migrated takes MIGRATED_TEST_DB. Exporting all three
# is what lets the single cargo invocation below cover all of them.
export CONTROL_TEST_DB="$DSN"
export AUTH_DB_URL="$DSN"
export MIGRATED_TEST_DB="$DSN"

run_psql() { PGPASSWORD="$PG_PASS" "$PSQL" -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" "$@"; }

if [ -z "${SKIP_DB_RECREATE:-}" ]; then
  echo "==> Recreating ${TEST_DB} on ${PG_HOST}:${PG_PORT}"
  run_psql -d postgres -v ON_ERROR_STOP=1 \
    -c "DROP DATABASE IF EXISTS ${TEST_DB} WITH (FORCE);" \
    -c "CREATE DATABASE ${TEST_DB};"
  echo "==> Migrating ${TEST_DB} (zeroship-platform-migrate)"
  # Apply the full committed platform history to the newly created database.
  ZEROSHIP_MIGRATE_DSN="postgres://${PG_USER}:${PG_PASS}@${PG_HOST}:${PG_PORT}/${TEST_DB}" \
    deploy/ops/db-migrate.sh >/dev/null
  echo "==> Migration complete"
else
  echo "==> SKIP_DB_RECREATE set; reusing ${TEST_DB}"
fi

# Default to ONE test thread per binary. The old name list ran only the billing
# binaries, which carry their own intra-binary mutexes and tolerate the default
# thread pool. The set now includes `workflow_engine_test` and
# `workflow_instance_api_test`, which do not: they drive advisory-locked engine
# ticks and assert on claim counts, so under the default pool sibling tests steal
# each other's claims and ten of forty cases fail nondeterministically.
# `tests/e2e_durable_workflows.sh` has always run those two with
# `--test-threads=1` for the same reason. Determinism is worth the wall clock
# here; override TEST_THREADS to trade it back.
THREAD_ARG=(--test-threads "${TEST_THREADS:-1}")

# Capture every group's output so the run can be COUNTED, not just exit-checked.
#
# `cargo test` with a filter matching nothing runs zero tests and exits 0.
# Measured: `cargo test -p zeroship-metering --lib no_such_filter_xyz` reports
# "running 0 tests ... 0 passed ... 12 filtered out" and returns 0. The
# metering group below uses exactly that shape (`--lib outbox`), so renaming
# those tests would cover nothing while this script printed ALL GROUPS PASSED.
#
# `--test <target>` is NOT exposed to this: an unmet required-feature or a
# missing target both exit 101, checked. Only filters degrade silently.
SUITE_LOG="$(mktemp)"
trap 'rm -f "$SUITE_LOG"' EXIT

# Run a group, tee its output into SUITE_LOG, return the CARGO exit status.
#
# PIPESTATUS is load-bearing. Piping into tee makes `$?` tee's status, which is
# 0 whenever tee could write - so every group would look green regardless of
# what cargo did.
#
# Also fails a group that ran NOTHING. The total-passed floor at the bottom of
# this script cannot see a small group vanish: the metering group is ~12 tests
# inside a ~713 total, so renaming its tests drops the total by under 2 percent
# and the floor still passes. The per-group check catches it immediately,
# because a filter matching nothing produces `running 0 tests` and exit 0.
#
# Summed ACROSS the group, not per target. One cargo invocation prints one
# `running N tests` line per target, and a target with genuinely no tests
# (doctests, an empty integration file) legitimately prints 0 - so a per-line
# check would fail honest groups. The group total is the quantity that is only
# zero when nothing ran.
run_group() {
  # Captured to its OWN file first. Counting `running N tests` out of the
  # shared SUITE_LOG would sum every earlier group too, so the second group
  # onwards would inherit a non-zero count and the check would pass for a
  # group that ran nothing - the exact failure it exists to catch.
  local group_log status ran
  group_log="$(mktemp)"
  "$@" 2>&1 | tee "$group_log"
  status="${PIPESTATUS[0]}"
  cat "$group_log" >> "$SUITE_LOG"

  ran=$(grep -oP '^running \K[0-9]+' "$group_log" | awk '{s+=$1} END {print s+0}')
  rm -f "$group_log"

  if [ "$status" -eq 0 ] && [ "$ran" -eq 0 ]; then
    echo "FAIL: group ran 0 tests and still exited 0 - a filter matched nothing." >&2
    echo "      cargo exits 0 when a test-name filter selects no tests, so this" >&2
    echo "      group covered nothing while reporting success." >&2
    return 1
  fi
  return "$status"
}

fail=0
declare -a failed=()

echo "------------------------------------------------------------------"
echo "==> zeroship-control live-database suite (--features live-db-tests)"
if run_group cargo test -p zeroship-control --features live-db-tests --no-fail-fast \
     -- "${THREAD_ARG[@]}"; then
  :
else
  fail=1
  failed+=("zeroship-control::live-db-tests")
fi

echo "------------------------------------------------------------------"
echo "==> zeroship-migrated live-database suite (--features live-db-tests)"
if run_group cargo test -p zeroship-migrated --features live-db-tests --no-fail-fast \
     -- "${THREAD_ARG[@]}"; then
  :
else
  fail=1
  failed+=("zeroship-migrated::live-db-tests")
fi

echo "------------------------------------------------------------------"
echo "==> zeroship-metering outbox WAL unit tests"
# Not a zeroship-control target and not feature-gated, so the invocations above
# do not reach it; the metering outbox is the producer half of the money path.
if run_group cargo test -p zeroship-metering --lib outbox -- "${THREAD_ARG[@]}"; then
  :
else
  fail=1
  failed+=("zeroship-metering::outbox")
fi

# Real-broker path. Rewind/seek and the wired producer->stream->recompute->spend
# flow behave differently on a real Kafka-wire broker than on the in-process
# memory transport (a real rewind cold-start seek bug once shipped precisely
# because this path had no CI). The control-side half
# (`billing_pipeline_redpanda_e2e`) is an ungated target and therefore already
# ran in the control invocation above, self-skipping unless REDPANDA_BROKERS is
# set. Only the zeroship-stream half needs naming here.
if [ -n "${REDPANDA_BROKERS:-}" ]; then
  echo "------------------------------------------------------------------"
  echo "==> real-broker: zeroship-stream::redpanda_roundtrip (REDPANDA_BROKERS=$REDPANDA_BROKERS)"
  if run_group cargo test -p zeroship-stream --test redpanda_roundtrip -- "${THREAD_ARG[@]}"; then :; else
    fail=1; failed+=("zeroship-stream::redpanda_roundtrip")
  fi
else
  echo "------------------------------------------------------------------"
  echo "==> SKIP real-broker tests: REDPANDA_BROKERS unset (set it + run a redpanda broker to gate the real stream path)"
fi

echo "=================================================================="
if [ "$fail" -ne 0 ]; then
  echo "LIVE-DATABASE SUITE FAILED: ${failed[*]}" >&2
  exit 1
fi

# Exit codes alone cannot distinguish "every group passed" from "a group ran
# nothing and said so politely". Require a MINIMUM, the same shape as
# run_auth_suite.sh.
passed="$(grep -oE '^test result: ok\. [0-9]+ passed' "$SUITE_LOG" \
  | grep -oE '[0-9]+' | awk '{s+=$1} END {print s+0}')"

# 660 against 713 measured on 2026-08-08 (61 groups, 0 failures), the same ~7
# percent headroom the auth gate carries.
#
# The measurement was taken WITHOUT REDPANDA_BROKERS, so it excludes the
# real-broker group that CI runs. That makes 713 the LOWER of the two legitimate
# configurations, which is the one a floor has to sit under - a floor derived
# from the richer CI run would fail every local invocation.
#
# Raise it deliberately when the suite grows. A fixed floor gets looser with
# every test added, which is the wrong direction for a guard against coverage
# loss.
BILLING_MIN_PASSED="${BILLING_MIN_PASSED:-660}"
if [ "$passed" -lt "$BILLING_MIN_PASSED" ]; then
  echo "FAIL: only ${passed} billing tests passed, fewer than the ${BILLING_MIN_PASSED} this gate expects." >&2
  echo "A group that silently stopped running is indistinguishable from a group that passed." >&2
  echo "If the suite really did shrink, lower BILLING_MIN_PASSED deliberately; do not treat the gap as slack." >&2
  exit 1
fi

# Printed on SUCCESS, not only inside a failure message: a count nobody sees
# until the gate has already failed cannot warn anyone.
echo "LIVE-DATABASE SUITE: ALL GROUPS PASSED (${passed} tests, floor ${BILLING_MIN_PASSED})"
