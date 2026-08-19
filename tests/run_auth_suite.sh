#!/usr/bin/env bash
# ============================================================================
# run_auth_suite.sh - the auth live-database gate
#
# WHY THIS EXISTS
# ---------------
# The auth tests resolve their database from the generated test overlay
# (deploy/ops/zeroship.test.toml, or PG_TEST_URL overriding it) and return
# early when neither supplies one. Cargo captures test output by default, so a
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
#   tests/run_auth_suite.sh                    # per-run DB + migrate + run all
#   TEST_DB=mine tests/run_auth_suite.sh       # name it, and keep it afterwards
#   TEST_DB=mine SKIP_DB_RECREATE=1 tests/run_auth_suite.sh   # reuse that one
#   PG_PORT=5440 tests/run_auth_suite.sh
#
# PROVISION FIRST. This script creates and migrates a DATABASE; it does not
# create a SERVER, and it fails at line ~110 if none is listening. Stand one up
# with `tests/provision_test_backends.sh`, which brings up deploy/compose's
# postgres on the port below.
#
# ENV (defaults target deploy/compose's postgres service, published on :5440)
#   The comment here read "the dev compose Postgres on :5440" for months while
#   the server actually answering was `zs-auth-pg-5440`, started by hand, owned
#   by no file in this tree, and running the postgres:16 default wal_level
#   instead of compose's `logical`. The address was right and the provenance was
#   wrong, which is the worse of the two failures: it read as though something
#   maintained that server.
#   PG_HOST PG_PORT PG_USER PG_PASS - INPUTS to
#     tests/provision_test_backends.sh, which writes the coordinates into
#     deploy/ops/zeroship.test.toml; this script reads them back from there
#     rather than carrying a second copy of the defaults.
#   TEST_DB (per run: zeroship_auth_test_<pid>_<nanos>, dropped on exit)
#   PSQL    (auto-detected; override with an explicit psql path)
#   SKIP_DB_RECREATE (unset) - reuse a TEST_DB you named; needs one
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
# Names the database per run, so a second run of this script cannot drop this
# one's out from under it. See that file's header for the measured collision;
# `tests/lib_scratch_db_selftest.sh` covers both directions.
. "$ROOT/tests/lib/scratch_db.sh"

# The server's coordinates come from the generated overlay, not from four
# `${PG_x:-...}` lines here and four more in run_billing_suite.sh. See that
# file's header; `tests/provision_test_backends.sh` writes it, and the PG_*
# names are its INPUTS, so setting one still points a run wherever you like.
. "$ROOT/tests/lib/test_config.sh"
zs_test_config_load "$ROOT" || exit 2

TEST_THREADS="${TEST_THREADS:-1}"

zs_scratch_db_resolve zeroship_auth_test || exit $?

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

# ONE name for the test database, and it is the one the overlay's override tier
# already uses. This block exported AUTH_DB_URL and PG_TEST_URL, and further
# down it exported GATEWAY_ANCHORS_DB_URL and GATEWAY_POOL_SMOKE_URL as well -
# four names for one DSN, each read by one crate, so a crate whose name nobody
# remembered to export ran against nothing and counted as passing. That is not
# hypothetical: GATEWAY_ANCHORS_DB_URL was set NOWHERE in the repository, and
# thirteen tests behind it announced skips for months.
#
# The scratch database name is per run and cannot live in the shared file, so
# PG_TEST_URL is exactly the override tier the overlay is designed for.
DSN="postgres://${PG_USER}:${PG_PASS}@${PG_HOST}:${PG_PORT}/${TEST_DB}"
export PG_TEST_URL="$DSN"

run_psql() { PGPASSWORD="$PG_PASS" "$PSQL" -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" "$@"; }

# Armed BEFORE the database is created, not after the tests start: a migration
# that fails leaves a database behind exactly like a failing test does, and the
# per-run name means nothing would ever reuse it. INT and TERM are routed
# through `exit` so they reach this trap too - bash runs an EXIT trap on a
# signal only if the handler exits, and a suite this long is cancelled by hand
# often enough for that to be the common case rather than the exotic one.
LOG=""
cleanup() {
  if [ -n "$LOG" ]; then rm -f "$LOG"; fi
  zs_scratch_db_cleanup
  return 0
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

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
  || { echo "FATAL: ${TEST_DB} unreachable at ${PG_HOST}:${PG_PORT}" >&2
       echo "       Provision a server first: tests/provision_test_backends.sh" >&2
       echo "       (or set PG_HOST/PG_PORT/PG_USER/PG_PASS to reach your own)" >&2
       exit 2; }

LOG="$(mktemp -t zeroship-auth-suite.XXXXXX.log)"

echo "==> Running the auth suite against ${TEST_DB}"
status=0
# --nocapture is no longer what makes the skip check work - the announcer writes
# straight to the stderr handle, which the harness's capture never touches. It
# stays for everything else a gated test prints on its way to a decision.
#
# --no-fail-fast because cargo otherwise STOPS at the first failing binary, and
# the tally this gate checks against its floor is a sum over binaries. This
# crate has 55 of them and the LIB is the first cargo runs, so a single red lib
# test used to discard the other 54.
#
# MEASURED 2026-08-17 by forcing one lib test red (csrf.rs `matches_exact`) and
# changing NOTHING ELSE but this flag:
#   clean, with the flag       55 binaries, 475 auth passes, gate tally 637
#   red lib, WITHOUT the flag   1 binary,   217 auth passes, gate tally 108
#   red lib, WITH the flag     55 binaries, 474 auth passes, gate tally 419
# The mutation costs exactly one test (475 -> 474). The flag costs 54 binaries.
#
# The truncated run ALSO invented 36 failures further down this script, in
# zeroship-authz and zeroship-gateway, every one of them `Key (plan_id)=(free)
# is not present in table "plans"` - rows an auth integration binary seeds and
# the truncated invocation never reached. So the missing flag does not merely
# understate the count; it manufactures failures in other crates that look
# exactly like real ones.
#
# The floor caught the 108 only because it is so far under it. A red in a LATE
# binary truncates by a handful instead of by 54, lands ABOVE the floor, and
# nobody learns the number was truncated - which is the failure this whole gate
# exists to prevent, arriving through the gate's own instrument.
cargo test -p zeroship-auth --no-fail-fast -- --test-threads "$TEST_THREADS" --nocapture 2>&1 | tee "$LOG" || status=1

echo "------------------------------------------------------------------"
# Every other database-gated binary in the workspace. These self-skip exactly
# like the auth crate's, and until they were listed here nothing ever ran them
# with a database: `cargo test --workspace` provisions none, and no other gate
# names them. Measured on zeroship-authz before adding it - "ok. 1 passed" in
# 0.00s without a DSN against the same "ok. 1 passed" in 0.22s with one. Same
# count, same exit code, only the clock differed.
#
# `oidc_rp_e2e` used to be excluded BY NAME here, on the stated ground that it
# "also wants CONTROL_TEST_DB, which this script does not provision, so it would
# skip". That reason was wrong even then - its `db_url()` read
# `test_env!("AUTH_DB_URL").or_else(|| test_env!("CONTROL_TEST_DB"))`, so EITHER
# variable satisfied it and this script exported the first. Measured 2026-08-16
# with CONTROL_TEST_DB explicitly unset and only AUTH_DB_URL set: "3 passed in
# 6.73s", against the "3 passed in 0.00s" the same target reports with neither.
# The exclusion cost real coverage for a provisioning gap that did not exist.
#
# The two-variable `or_else` is gone: `db_url()` at
# crates/gateway/tests/oidc_rp_e2e.rs:46 is now
# `zeroship_core::config::test_database_url_opt()`, one source for every target
# in the workspace. The measurement above is why the collapse is safe here - the
# target was already satisfied by whichever name happened to be exported, which
# is another way of saying the two names never meant different things.
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
# So both binaries are in the list. This is the SAME database the auth tests
# use: these tests seed their own users and key off per-test UUIDs, and
# TEST_THREADS serializes the run.
#
# GATEWAY_ANCHORS_DB_URL and GATEWAY_POOL_SMOKE_URL were exported here, and
# there is nothing left to export - both now read the single test DSN above.
# Their history is the argument for that collapse rather than a footnote to it:
# GATEWAY_POOL_SMOKE_URL was set NOWHERE in this repository outside its own
# test file and one docs line, so `crates/gateway/tests/db_pool_smoke.rs`
# announced a skip on every run of `cargo test --workspace` and its one test had
# never executed. A private name for a value that already exists is a test that
# does not run, and it looks exactly like a test that passes.

echo "==> Other database-gated binaries (authn, authz, mailer, gateway)"
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
  # --no-fail-fast on both arms, but it earns its place only on the second: a
  # bare `-p <pkg>` runs every target in the package (lib, each integration
  # binary, doctests) and cargo stops at the first that fails, so a red
  # zeroship-authn lib test would drop its six service_replay_pg_test results
  # from the tally below. `--test <bin>` selects ONE binary, where the flag
  # changes nothing today; it is there so that stays true if a second target is
  # ever added to that arm.
  if [ -n "$bin" ]; then
    cargo test -p "$pkg" --test "$bin" --no-fail-fast -- \
      --test-threads "$TEST_THREADS" --nocapture 2>&1 | tee -a "$LOG" || status=1
  else
    cargo test -p "$pkg" --no-fail-fast -- \
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

# 595 -> 604 for the three targets added above. The +9 is the measured delta and
# nothing more: 647 before, 656 after, on the same database provisioning, and
# the added tests account for all nine - zeroship-authn's lib (2), its
# service_replay_pg_test (6, previously six announced skips at 0.00s, now
# 1.10s of real work) and zeroship-gateway's db_pool_smoke (1, 0.52s).
AUTH_MIN_PASSED="${AUTH_MIN_PASSED:-604}"
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
