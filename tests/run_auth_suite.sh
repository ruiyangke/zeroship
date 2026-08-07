#!/usr/bin/env bash
# ============================================================================
# run_auth_suite.sh - the auth live-database gate
#
# WHY THIS EXISTS
# ---------------
# The auth tests resolve their database from AUTH_DB_URL / PG_TEST_URL and
# return early when neither is set. Cargo captures test output by default, so a
# skipped test is indistinguishable from a passing one: `cargo test -p
# zeroship-auth` reports success while test bodies do nothing at all. A suite
# that passes because it never ran is worse than a red one, because it is
# trusted.
#
# This script reports the real passed and skipped counts on every run, so no
# figure is written down here to go stale.
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
# Every other AUTH_DB_URL-gated binary in the workspace. These self-skip exactly
# like the auth crate's, and until they were listed here nothing ever ran them
# with a database: `cargo test --workspace` provisions none, and no other gate
# names them. Measured on zeroship-authz before adding it - "ok. 1 passed" in
# 0.00s without a DSN against the same "ok. 1 passed" in 0.22s with one. Same
# count, same exit code, only the clock differed.
#
# `oidc_rp_e2e` is deliberately absent: it also wants CONTROL_TEST_DB, which
# this script does not provision, so it would skip and fail the check below.
# tests/run_billing_suite.sh owns the CONTROL_TEST_DB half.
echo "==> Other AUTH_DB_URL-gated binaries (authz, mailer, gateway)"
for spec in \
  "zeroship-authz:" \
  "zeroship-mailer:" \
  "zeroship-gateway:backchannel_logout_test" \
  "zeroship-gateway:auth_token_anchors_test" \
  "zeroship-gateway:identities_relay_test" \
  "zeroship-gateway:sessions_test" \
; do
  pkg="${spec%%:*}"
  bin="${spec#*:}"
  if [ -n "$bin" ]; then
    cargo test -p "$pkg" --test "$bin" -- \
      --test-threads "$TEST_THREADS" --nocapture 2>&1 | tee -a "$LOG" || status=1
  else
    cargo test -p "$pkg" -- \
      --test-threads "$TEST_THREADS" --nocapture 2>&1 | tee -a "$LOG" || status=1
  fi
done

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

# The skip check above counts problems and requires none, so it succeeds when it
# finds nothing - including when there was nothing it COULD find. It only sees a
# test that prints the word "skipping", and 7 of the OIDC suites do not: they
# gate on `let Some(fx) = Fixture::boot(...).await else { return; };` and return
# in silence. Measured with no database: 75 tests across
# oidc_{refresh_token,authorization_code,userinfo,brokered_login,login_consent,
# backchannel_logout}_test and device_grant_test all report "ok" in ~0.00s, and
# the grep above finds zero. The gate would print "0 skipped" and exit 0.
#
# So require a MINIMUM instead of forbidding a maximum. 505 against 543 measured
# on 2026-08-07 is the same ~7 percent headroom the CI test-target floor carries.
# Raise it as the suite grows; a fixed floor gets looser with every test added,
# which is the wrong direction for a guard against coverage loss.
AUTH_MIN_PASSED="${AUTH_MIN_PASSED:-505}"
if [ "$passed" -lt "$AUTH_MIN_PASSED" ]; then
  echo "FAIL: only ${passed} auth tests passed, fewer than the ${AUTH_MIN_PASSED} this gate expects." >&2
  echo "A suite that silently stopped running is indistinguishable from a suite that passed." >&2
  echo "If the suite really did shrink, lower AUTH_MIN_PASSED deliberately; do not treat the gap as slack." >&2
  status=1
fi

echo "=================================================================="
if [ "$status" -eq 0 ]; then
  echo "AUTH SUITE: ${passed} tests passed, 0 skipped (floor ${AUTH_MIN_PASSED})"
else
  echo "AUTH SUITE: FAILED"
fi
echo "=================================================================="
exit "$status"
