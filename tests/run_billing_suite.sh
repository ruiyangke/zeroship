#!/usr/bin/env bash
# ============================================================================
# run_billing_suite.sh — the CI runner for the zeroship-control billing &
# metering integration suite (ISS-31, billing-metering epic).
#
# WHY THIS SCRIPT EXISTS (the constraint)
# ----------------------------------------
# The billing test binaries share one Postgres DB (:5440) and exercise the REAL
# fleet-wide cron sweeps — `billing_reconcile`, `metering_export`,
# `stripe_reconcile`, `billing_notify`, `spend_reconcile`. Each of those sweeps
# is single-flighted FLEET-WIDE in production by a session-scoped
# `pg_try_advisory_lock(<stable key>)`: a second concurrent sweep LOSES the lock
# and returns `Ok(0)` (it does nothing this tick). That advisory lock is a
# production correctness mechanism (multi-instance safety) and is deliberately
# NOT changed for tests.
#
# Consequence: two DIFFERENT test binaries that each drive the SAME sweep cannot
# run CONCURRENTLY. The loser's `tick` returns 0 and its `n == 1` / `billed == 1`
# assertion fails. Several sweeps also bill/exports keyed on the CURRENT or
# PREVIOUS calendar month (`Utc::now()`), so every reconcile-driving binary's
# creators land in the SAME period window — the fleet sweep's count then spans
# both binaries' data even setting the lock aside.
#
# Within ONE binary the collision is already handled: each binary serializes its
# own sweep-driving tests with a process-wide mutex (RECONCILE_LOCK / EXPORT_LOCK
# / …) and gives every test UNIQUE ids (creator/app/customer) + (where it can)
# its OWN far-future period bucket. So a binary run ALONE — even multi-threaded
# internally — is reliable. The ONLY thing that must be serialized is the set of
# test BINARIES relative to each other.
#
# This runner therefore runs each billing test binary ONE AT A TIME (never two
# concurrently). cargo is invoked with `--test <name>` per binary; the default
# multi-threaded in-binary runner is kept (each binary's intra-binary mutexes +
# unique ids make that safe and fast). This is a documented, honest serial
# integration runner — strictly better than silently-flaky full parallelism.
#
# PARALLEL-SAFE vs SERIAL-ONLY (for reference)
# --------------------------------------------
# Parallel-safe in principle (no fleet-cron drive; pure per-entity, unique ids):
#   account_status_test, billing_invoice_payments_test, billing_read_api_test,
#   billing_redesign_regression_test, connect_fee_test, delete_app_billing_fk_test,
#   spend_limit_http_test.
# Serial-only (drive an advisory-locked fleet sweep and/or assert on its count,
# and/or mutate a global singleton — pricing_config 'global' / metric_weights):
#   billing_reconcile_test, billing_proration_test, billing_tax_test,
#   billing_credit_test, billing_refund_void_test, billing_dispute_test,
#   billing_notify_test, stripe_webhook_test, stripe_reconcile_test,
#   spend_reconcile_test, metering_export_test, metering_export_openmeter_test,
#   pricing_config_test, plan_catalog, spend, metering, stripe_store.
# This runner runs ALL of them serially (one binary at a time) so the operator
# never has to remember which bucket a binary is in — it is always correct.
#
# USAGE
# -----
#   tests/run_billing_suite.sh                 # recreate DB + migrate + run all
#   SKIP_DB_RECREATE=1 tests/run_billing_suite.sh   # reuse an already-migrated DB
#   PG_PORT=5440 PG_USER=postgres PG_PASS=zeroship tests/run_billing_suite.sh
#
# ENV (defaults target the dev compose Postgres on :5440)
#   PG_HOST (localhost)  PG_PORT (5440)  PG_USER (postgres)  PG_PASS (zeroship)
#   TEST_DB (zeroship_billing_test)
#   PSQL    (auto-detected; override with an explicit psql path)
#   SKIP_DB_RECREATE (unset)  — set to skip the drop/create/migrate step
#   TEST_THREADS (unset)      — passed to each binary's `--test-threads`
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

export CONTROL_TEST_DB="postgresql://${PG_USER}:${PG_PASS}@${PG_HOST}:${PG_PORT}/${TEST_DB}"

# The billing test binaries (excluding the *_live_test binaries, which require
# real Stripe/OpenMeter credentials — they self-skip without creds, so they are
# safe to include but are left out of the default CI gate). Run ONE AT A TIME.
BILLING_TESTS=(
  # --- serial-only: drive an advisory-locked fleet sweep / global singleton ---
  billing_reconcile_test
  billing_proration_test
  billing_tax_test
  billing_credit_test
  billing_refund_void_test
  billing_dispute_test
  billing_notify_test
  stripe_webhook_test
  stripe_reconcile_test
  spend_reconcile_test
  metering_export_test
  metering_export_openmeter_test
  pricing_config_test
  plan_catalog
  spend
  metering
  stripe_store
  # --- parallel-safe in principle; run here too for one correct gate ---
  account_status_test
  billing_invoice_payments_test
  billing_read_api_test
  billing_redesign_regression_test
  connect_fee_test
  delete_app_billing_fk_test
  spend_limit_http_test
)

run_psql() { PGPASSWORD="$PG_PASS" "$PSQL" -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" "$@"; }

if [ -z "${SKIP_DB_RECREATE:-}" ]; then
  echo "==> Recreating ${TEST_DB} on ${PG_HOST}:${PG_PORT}"
  run_psql -d postgres -v ON_ERROR_STOP=1 \
    -c "DROP DATABASE IF EXISTS ${TEST_DB} WITH (FORCE);" \
    -c "CREATE DATABASE ${TEST_DB};"
  echo "==> Migrating ${TEST_DB} (Liquibase)"
  ZEROSHIP_DB_JDBC="jdbc:postgresql://${PG_HOST}:${PG_PORT}/${TEST_DB}" \
  ZEROSHIP_DB_USER="$PG_USER" ZEROSHIP_DB_PASS="$PG_PASS" \
    ops/db-migrate.sh update >/dev/null
  echo "==> Migration complete"
else
  echo "==> SKIP_DB_RECREATE set; reusing ${TEST_DB}"
fi

# Compile all the binaries first (once), so the per-binary runs are pure execution.
echo "==> Compiling control test binaries"
cargo test -p zeroship-control --no-run >/dev/null 2>&1

THREAD_ARG=()
if [ -n "${TEST_THREADS:-}" ]; then
  THREAD_ARG=(--test-threads "$TEST_THREADS")
fi

echo "==> Running ${#BILLING_TESTS[@]} billing test binaries SERIALLY (one binary at a time)"
fail=0
declare -a failed=()
for t in "${BILLING_TESTS[@]}"; do
  echo "------------------------------------------------------------------"
  echo "==> $t"
  if cargo test -p zeroship-control --test "$t" -- "${THREAD_ARG[@]}"; then
    :
  else
    fail=1
    failed+=("$t")
  fi
done

echo "=================================================================="
if [ "$fail" -ne 0 ]; then
  echo "BILLING SUITE FAILED: ${failed[*]}" >&2
  exit 1
fi
echo "BILLING SUITE: ALL ${#BILLING_TESTS[@]} BINARIES PASSED"
