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
# Counts the tests that announced they did nothing, so a green tally cannot hide
# them; `tests/lib_skip_census_selftest.sh` covers both directions.
. "$ROOT/tests/lib/skip_census.sh"

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
# `oidc_rp_e2e` used to be excluded BY NAME here, on the stated ground that it
# "also wants CONTROL_TEST_DB, which this script does not provision, so it would
# skip". That reason was wrong, and the file says so: `db_url()` at
# crates/gateway/tests/oidc_rp_e2e.rs:47 is
# `test_env!("AUTH_DB_URL").or_else(|| test_env!("CONTROL_TEST_DB"))` - EITHER
# variable satisfies it, and this script exports the first one. Measured
# 2026-08-16 with CONTROL_TEST_DB explicitly unset and only AUTH_DB_URL set:
# "3 passed in 6.73s", against the "3 passed in 0.00s" the same target reports
# with neither. The exclusion cost real coverage for a provisioning gap that did
# not exist, and this is the branch's strongest new assertion (the projected
# identity check) - which no gate ran.
#
# It is in the list below now. It needs no allowlist entry and gets none: it
# announces through `zeroship_test_support::skip`, so if it ever stops seeing a
# database the census below counts it and this gate goes red, which is the
# required behaviour - a self-skip here is a FAILURE, not a pass.
#
# GATEWAY_ANCHORS_DB_URL used to be set nowhere in this repo, so the 13 gated
# tests in `auth_token_anchors_test` and the 1 in `browser_auth_test` announced
# a skip into this script's own log and never ran. Pointing it at this
# script's database ran 23 tests of which 11 FAILED, the first on "initial
# login must succeed, left: 400", so the deferral was recorded here rather
# than taken.
#
# It is taken now. The 11 failures were one stale fixture, not 11 defects: the
# mock OP minted its ID token and its access token independently and never
# carried `at_hash`, while `session_post` requests access-token binding, which
# makes the claim mandatory. The gateway log named it exactly - "at_hash
# missing while access token binding was requested". The REAL OP does mint it
# (crates/auth/src/oidc/issuer.rs, unconditional in the single mint path), so
# the handler was right and the fixture was wrong. Binding the mock's ID tokens
# to the access token they ship with took it to 23 passed / 0 failed, and
# surfaced a second, real defect on the way (the gateway declined to verify
# at_hash on a ROTATED id_token while holding the access token; see
# crates/gateway/src/auth_token.rs).
#
# So the variable is exported below and both binaries are in the list. This is
# the SAME database the auth tests use: these tests seed their own users and
# key off per-test UUIDs, and TEST_THREADS serializes the run.
export GATEWAY_ANCHORS_DB_URL="$DSN"

# GATEWAY_POOL_SMOKE_URL was set NOWHERE in this repo - the only occurrence of
# the name outside its own test file was docs/reference/env-vars.md:608, so
# `crates/gateway/tests/db_pool_smoke.rs` announced a skip on every run of
# `cargo test --workspace` and its one test had never executed. The target needs
# no schema at all ("any reachable Postgres works", db_pool_smoke.rs:6 - it runs
# `SELECT $1::int4`), so this script's database satisfies it as-is and there is
# nothing to provision beyond naming the variable.
export GATEWAY_POOL_SMOKE_URL="$DSN"

echo "==> Other AUTH_DB_URL-gated binaries (authn, authz, mailer, gateway)"
# `zeroship-authn` is here because it was in NO gate at all. Its one target,
# crates/authn/tests/service_replay_pg_test.rs, gates all six of its tests on
# AUTH_DB_URL and announces a skip for each; the string "zeroship-authn"
# appeared nowhere under tests/ or .github/. It is not feature-gated, so the
# `rust` job DID build and run it - with no database, six announced skips, six
# counted passes, and a census that reports rather than fails.
#
# This list is hand-maintained and that is its weakness: a new AUTH_DB_URL-gated
# binary anywhere in the workspace is covered only if someone remembers to add
# it here. run_billing_suite.sh removed the equivalent list by running a whole
# package with a feature; the same trick does not apply here because these are
# individual targets inside packages whose other targets need no database.
for spec in \
  "zeroship-authn:" \
  "zeroship-authz:" \
  "zeroship-mailer:" \
  "zeroship-gateway:backchannel_logout_test" \
  "zeroship-gateway:auth_token_anchors_test" \
  "zeroship-gateway:browser_auth_test" \
  "zeroship-gateway:identities_relay_test" \
  "zeroship-gateway:sessions_test" \
  "zeroship-gateway:oidc_rp_e2e" \
  "zeroship-gateway:db_pool_smoke" \
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
# The counting itself now lives in tests/lib/skip_census.sh, which this script
# sources at the top, so the same census runs here, in run_billing_suite.sh, and
# over the blanket `cargo test --workspace` in CI. It used to be open-coded here
# and nowhere else, which is why every crate outside the auth suite could
# announce a skip into a log no gate ever read. Moving it did not weaken this
# gate: the allowlist and the failure below are unchanged, and the library adds
# `grep -a`, without which a log carrying a single NUL byte reports its skips as
# one nameless "binary file matches" line instead of naming the backend.
#
# tests/lib_skip_census_selftest.sh covers the library in both directions.

# Skips this gate reports but does not fail on. Each entry names a backend this
# script does not provision, and the decision to leave it unprovisioned:
#
#   GATEWAY_ANCHORS_DB_URL is NO LONGER HERE, deliberately. This script now
#     exports it (see above), so a skip announcing it means the export broke or
#     a test stopped seeing it - a regression, not a tolerated gap. Leaving the
#     entry in place after provisioning the backend would make exactly that
#     regression undetectable, which is the failure this whole allowlist exists
#     to avoid.
#   AUTH_TEST_SMTP_SINK - one zeroship-mailer test
#     (smtp_plaintext_sink_delivers_relay_forward) wants a live SMTP sink at a
#     host:port this script has no way to stand up. Its two siblings in the same
#     binary gate only on AUTH_DB_URL and ARE covered; the allowlist matches on
#     the reason rather than the binary precisely so exempting this one does not
#     blind the gate to the rest of the file.
SKIP_ALLOWLIST='AUTH_TEST_SMTP_SINK'

if ! zs_skip_census "$LOG" "$SKIP_ALLOWLIST"; then
  echo "FAIL: ${ZS_SKIP_COUNT} test(s) skipped despite a provisioned database." >&2
  echo "A skipped auth test is a silent pass. Offending lines:" >&2
  zs_skip_lines "$LOG" "$SKIP_ALLOWLIST" | sort -u | head -20 >&2
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
# So require a MINIMUM instead of forbidding a maximum. The floor tracks the
# measured total at ~7 percent headroom, the same margin the CI test-target floor
# carries. Raise it as the suite grows; a fixed floor gets looser with every test
# added, which is the wrong direction for a guard against coverage loss.
#
# History, so nobody reads the gap between floor and total as slack: 505 was set
# against 543 measured on 2026-08-07. The suite then reached 636 and the floor
# stayed put, leaving 21 percent of the suite free to vanish unnoticed. 595 is
# against 640 measured on 2026-08-16 - the 636 the fixture repairs reached, plus
# oidc_rp_e2e's 3 (newly run by this gate) and the identities rebinding test.
# Checked BEFORE the floor, because a build that died for want of disk space also
# passes zero tests. Without this the gate blames coverage loss and tells you to
# lower AUTH_MIN_PASSED - advice that would permanently weaken the guard in
# response to a full volume.
if log_shows_disk_full "$LOG"; then
  report_measurement_did_not_run "$LOG" "the auth suite"
  exit 90
fi

AUTH_MIN_PASSED="${AUTH_MIN_PASSED:-595}"
if [ "$passed" -lt "$AUTH_MIN_PASSED" ]; then
  echo "FAIL: only ${passed} auth tests passed, fewer than the ${AUTH_MIN_PASSED} this gate expects." >&2
  echo "A suite that silently stopped running is indistinguishable from a suite that passed." >&2
  echo "If the suite really did shrink, lower AUTH_MIN_PASSED deliberately; do not treat the gap as slack." >&2
  status=1
fi

echo "=================================================================="
if [ "$status" -eq 0 ]; then
  # ZS_SKIP_TOLERATED, not `tolerated`: the census lives in
  # tests/lib/skip_census.sh and exports the ZS_-prefixed names. Under `set -u`
  # the unprefixed spelling aborted the script HERE, on the success line, so a
  # fully green suite exited 1 with no verdict printed and the failure looked
  # like a test failure. Only the success branch was affected, which is why it
  # survived: a red run takes the else branch and reports normally.
  echo "AUTH SUITE: ${passed} tests passed, 0 unexpected skips, ${ZS_SKIP_TOLERATED} allowlisted (floor ${AUTH_MIN_PASSED})"
else
  echo "AUTH SUITE: FAILED"
fi
echo "=================================================================="
exit "$status"
