#!/usr/bin/env bash
# ============================================================================
# run_auth_suite.sh - the auth live-database gate
#
# WHY THIS EXISTS
# ---------------
# The auth tests resolve their database from AUTH_DB_URL / PG_TEST_URL and
# return early when neither is set. Cargo captures test output by default, so a
# skipped test is indistinguishable from a passing one: `cargo test -p
# zeroship-auth` reports success while 66 test bodies do nothing at all. A suite
# that passes because it never ran is worse than a red one, because it is
# trusted.
#
# This script provisions an isolated database, points the tests at it, and then
# checks that they ACTUALLY RAN. If any test reports that it skipped, the run
# fails - a missing database can never masquerade as a pass.
#
# USAGE
# -----
#   tests/run_auth_suite.sh                    # recreate DB + migrate + run all
#   SKIP_DB_RECREATE=1 tests/run_auth_suite.sh # reuse an already-migrated DB
#   PG_PORT=5440 tests/run_auth_suite.sh
#
# ENV (defaults target the dev compose Postgres on :5440)
#   PG_HOST (localhost)  PG_PORT (5440)  PG_USER (postgres)  PG_PASS (zeroship)
#   TEST_DB (zeroship_auth_test)
#   PSQL    (auto-detected; override with an explicit psql path)
#   SKIP_DB_RECREATE (unset) - skip the drop/create/migrate step
#   TEST_THREADS (1)         - live-database tests share rows, so serialize
# ============================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PG_HOST="${PG_HOST:-localhost}"
PG_PORT="${PG_PORT:-5440}"
PG_USER="${PG_USER:-postgres}"
PG_PASS="${PG_PASS:-zeroship}"
TEST_DB="${TEST_DB:-zeroship_auth_test}"
TEST_THREADS="${TEST_THREADS:-1}"

PSQL="${PSQL:-}"
if [ -z "$PSQL" ]; then
  if command -v psql >/dev/null 2>&1; then
    PSQL="$(command -v psql)"
  else
    # The dev shell does not always put psql on PATH; fall back to whatever the
    # nix store has rather than failing on a detail the caller cannot guess.
    PSQL="$(ls -d /nix/store/*postgresql*/bin/psql 2>/dev/null | head -1 || true)"
  fi
fi
[ -n "$PSQL" ] && [ -x "$PSQL" ] || { echo "FATAL: no psql found; set \$PSQL" >&2; exit 2; }

DSN="postgres://${PG_USER}:${PG_PASS}@${PG_HOST}:${PG_PORT}/${TEST_DB}"
export AUTH_DB_URL="$DSN"
export PG_TEST_URL="$DSN"

run_psql() { PGPASSWORD="$PG_PASS" "$PSQL" -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" "$@"; }

if [ -z "${SKIP_DB_RECREATE:-}" ]; then
  echo "==> Recreating ${TEST_DB} on ${PG_HOST}:${PG_PORT}"
  run_psql -d postgres -v ON_ERROR_STOP=1 \
    -c "DROP DATABASE IF EXISTS ${TEST_DB} WITH (FORCE);" \
    -c "CREATE DATABASE ${TEST_DB};"
  echo "==> Migrating ${TEST_DB}"
  ZEROSHIP_MIGRATE_DSN="$DSN" deploy/ops/db-migrate.sh >/dev/null
  echo "==> Migration complete"
else
  echo "==> SKIP_DB_RECREATE set; reusing ${TEST_DB}"
fi

# Prove the database is actually reachable before trusting any result below.
run_psql -d "$TEST_DB" -v ON_ERROR_STOP=1 -tAc "select 1" >/dev/null \
  || { echo "FATAL: ${TEST_DB} unreachable at ${PG_HOST}:${PG_PORT}" >&2; exit 2; }

LOG="$(mktemp -t zeroship-auth-suite.XXXXXX.log)"
trap 'rm -f "$LOG"' EXIT

echo "==> Running the auth suite against ${TEST_DB}"
status=0
# --nocapture is REQUIRED, not cosmetic: without it cargo swallows the "skipping"
# lines the tests print, and the skip check below would pass vacuously.
cargo test -p zeroship-auth -- --test-threads "$TEST_THREADS" --nocapture 2>&1 | tee "$LOG" || status=1

echo "------------------------------------------------------------------"
echo "==> Gateway backchannel-logout tests (also AUTH_DB_URL-gated)"
cargo test -p zeroship-gateway --test backchannel_logout_test -- \
  --test-threads "$TEST_THREADS" --nocapture 2>&1 | tee -a "$LOG" || status=1

echo "------------------------------------------------------------------"
# The point of the whole script: a test that skipped is not a test that passed.
skips="$(grep -ci 'skipping' "$LOG" || true)"
if [ "$skips" -ne 0 ]; then
  echo "FAIL: ${skips} test(s) skipped despite a provisioned database." >&2
  echo "A skipped auth test is a silent pass. Offending lines:" >&2
  grep -i 'skipping' "$LOG" | sort -u | head -20 >&2
  status=1
fi

passed="$(grep -oE '^test result: ok\. [0-9]+ passed' "$LOG" | grep -oE '[0-9]+' | awk '{s+=$1} END {print s+0}')"

echo "=================================================================="
if [ "$status" -eq 0 ]; then
  echo "AUTH SUITE: ${passed} tests passed, 0 skipped"
else
  echo "AUTH SUITE: FAILED"
fi
echo "=================================================================="
exit "$status"
