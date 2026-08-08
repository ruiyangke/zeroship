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
# checks that they ACTUALLY RAN. If any test announces that it skipped, the run
# fails - a missing database can never masquerade as a pass. The one exception
# is an allowlist further down that names each tolerated skip and why; it is
# there so a deferred decision reads as a deferred decision rather than as a
# blind spot in the check.
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

# Distinguishes a real failure from a run that could not happen. See the library
# header; `tests/lib_measurement_integrity_selftest.sh` covers both directions.
. "$ROOT/tests/lib/measurement_integrity.sh"

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
# --nocapture is no longer what makes the skip check work - the announcer writes
# straight to the stderr handle, which the harness's capture never touches. It
# stays for everything else a gated test prints on its way to a decision.
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
# tests/run_billing_suite.sh was named here as owning the CONTROL_TEST_DB half -
# it does not. That script invokes zeroship-control and zeroship-migrated and
# never touches the gateway crate, so oidc_rp_e2e is covered by nothing.
#
# TWO BINARIES IN THE LIST BELOW ARE NOT ACTUALLY COVERED EITHER.
# `auth_token_anchors_test` gates on GATEWAY_ANCHORS_DB_URL, which is set
# nowhere in this repo - not here, not in ci.yml, not in deploy/. Measured on a
# full gate run: 13 lines reading "[anchors] skip <name> (no
# GATEWAY_ANCHORS_DB_URL)" sat in this script's own log while it reported "0
# skipped". `browser_auth_test` gates on the same unset variable.
#
# Exporting it is not the fix and that is measured too: with the variable
# pointed at this script's database the suite runs 23 tests and 11 FAIL, the
# first on "initial login must succeed, left: 400". So the coverage was never
# merely switched off - the tests need work, and turning them on turns this gate
# red. Whether to fix them or delete them is an operator decision, filed rather
# than taken here. The skip check below now SEES those 13 lines; it tolerates
# them by an explicit allowlist that names them, so the deferral is stated
# rather than smuggled in as a gap in the search.
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
#
# The search is for a token, not a word. `zeroship_test_support::skip` (and its
# verbatim copy in the standalone libs/ crates) prefixes every announcement with
# SKIP_MARKER, and nothing else in a run log is spelled that way. The word
# "skip" cannot do this job and that is measured: of the 98 lines containing it
# in one full run, 13 were real announcements, 5 were the harness's own
# "test <name> ... ok" for tests whose names contain "skips"/"skipped", and ~80
# were driver debug output echoing an INSERT that names a skip_consent column.
# The marker's hyphens are not legal in a Rust identifier, so no test name can
# forge it.
SKIP_MARKER="ZEROSHIP-TEST-SKIPPED"

# Skips this gate reports but does not fail on. Each entry names a backend this
# script does not provision, and the decision to leave it unprovisioned:
#
#   GATEWAY_ANCHORS_DB_URL - auth_token_anchors_test (13 tests) and
#     browser_auth_test. Pointing this at the gate's own database runs 23 tests
#     of which 11 fail; see the comment above the binary list. Fixing or
#     deleting them is an open operator decision, so the skips stay visible and
#     tolerated rather than silently undetectable.
#   AUTH_TEST_SMTP_SINK - one zeroship-mailer test
#     (smtp_plaintext_sink_delivers_relay_forward) wants a live SMTP sink at a
#     host:port this script has no way to stand up. Its two siblings in the same
#     binary gate only on AUTH_DB_URL and ARE covered; the allowlist matches on
#     the reason rather than the binary precisely so exempting this one does not
#     blind the gate to the rest of the file.
SKIP_ALLOWLIST='GATEWAY_ANCHORS_DB_URL|AUTH_TEST_SMTP_SINK'

skips="$(grep -F "$SKIP_MARKER" "$LOG" | grep -cvE "$SKIP_ALLOWLIST" || true)"
tolerated="$(grep -F "$SKIP_MARKER" "$LOG" | grep -cE "$SKIP_ALLOWLIST" || true)"
if [ "$tolerated" -ne 0 ]; then
  echo "NOTE: ${tolerated} allowlisted skip(s) - reported, not failed:"
  grep -F "$SKIP_MARKER" "$LOG" | grep -E "$SKIP_ALLOWLIST" | sort -u | head -20
fi
if [ "$skips" -ne 0 ]; then
  echo "FAIL: ${skips} test(s) skipped despite a provisioned database." >&2
  echo "A skipped auth test is a silent pass. Offending lines:" >&2
  grep -F "$SKIP_MARKER" "$LOG" | grep -vE "$SKIP_ALLOWLIST" | sort -u | head -20 >&2
  status=1
fi

passed="$(grep -oE '^test result: ok\. [0-9]+ passed' "$LOG" | grep -oE '[0-9]+' | awk '{s+=$1} END {print s+0}')"

# The skip check above counts problems and requires none, so it succeeds when it
# finds nothing - including when there was nothing it COULD find. It only sees a
# test that announces, and 7 of the OIDC suites do not: they gate on
# `let Some(fx) = Fixture::boot(...).await else { return; };` and return in
# silence. Measured with no database: 75 tests across
# oidc_{refresh_token,authorization_code,userinfo,brokered_login,login_consent,
# backchannel_logout}_test and device_grant_test all report "ok" in ~0.00s, and
# the grep above finds zero. The gate would print "0 skipped" and exit 0.
#
# So require a MINIMUM instead of forbidding a maximum. 505 against 543 measured
# on 2026-08-07 is the same ~7 percent headroom the CI test-target floor carries.
# Raise it as the suite grows; a fixed floor gets looser with every test added,
# which is the wrong direction for a guard against coverage loss.
# Checked BEFORE the floor, because a build that died for want of disk space also
# passes zero tests. Without this the gate blames coverage loss and tells you to
# lower AUTH_MIN_PASSED - advice that would permanently weaken the guard in
# response to a full volume.
if log_shows_disk_full "$LOG"; then
  report_measurement_did_not_run "$LOG" "the auth suite"
  exit 90
fi

AUTH_MIN_PASSED="${AUTH_MIN_PASSED:-505}"
if [ "$passed" -lt "$AUTH_MIN_PASSED" ]; then
  echo "FAIL: only ${passed} auth tests passed, fewer than the ${AUTH_MIN_PASSED} this gate expects." >&2
  echo "A suite that silently stopped running is indistinguishable from a suite that passed." >&2
  echo "If the suite really did shrink, lower AUTH_MIN_PASSED deliberately; do not treat the gap as slack." >&2
  status=1
fi

echo "=================================================================="
if [ "$status" -eq 0 ]; then
  echo "AUTH SUITE: ${passed} tests passed, 0 unexpected skips, ${tolerated} allowlisted (floor ${AUTH_MIN_PASSED})"
else
  echo "AUTH SUITE: FAILED"
fi
echo "=================================================================="
exit "$status"
