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
#   tests/run_auth_suite.sh                    # shared schema-keyed DB, run all
#   tests/run_auth_suite.sh --database mine    # a private one you name and own
#
# THE DATABASE IS SHARED, AND ITS NAME IS DERIVED. It is
# `zeroship_auth_test_<hash of db/migrations-ts/*.ts>`, so every run that needs
# the same schema lands in the same database and two agents on one commit
# neither collide nor multiply. It is created if absent, migrated, and NEVER
# dropped - by this script or any other. Why that is the right axis, why
# concurrent runs are safe on it, and what it does not cover: the header of
# tests/lib/suite_db.sh. Reclaiming space is tests/sweep_test_databases.sh.
#
# `TEST_DB=...` in the environment is REFUSED, not honoured. The override is a
# flag so that it cannot be inherited from a shell nobody remembers exporting
# it in; an ambient variable that silently redirects a gate is how gates get
# silently disabled.
#
# TWO RUNS AT ONCE ARE GREEN, AND HERE IS WHAT IT TOOK.
# A shared database shares DATABASE-SCOPED SINGLETONS, which no migration hash
# can see (tests/lib/suite_db.sh says why). Running two suites together is the
# only instrument that finds them, and it found these:
#
#   the active OP signing key   3 fixtures published a constant-seed key per
#                               test; a peer run retired it and the republish
#                               died. 96 failures -> 0.
#   5 rate-limit bucket keys    a shared client ip, or none at all, so a peer
#                               drained the bucket and a 429 arrived where the
#                               test asserts 401 / 303 / 200 / 302. The last of
#                               them was `reset_ip:0.0.0.0`, shared by the three
#                               /reset POSTs in password_reset_test.
#   4 globally-named DDL        triggers and a CHECK constraint installed on
#     objects on shared tables  zeroship.{users,magic_links,magic_completions,
#                               email_verifications}. Per-run NAMES and a WHEN
#                               clause (or predicate) naming the run's own row;
#                               the model is signing_key_retention_test.rs:643,
#                               which has done both since it was written.
#
# MEASURED 2026-08-20 on this cluster, THREE pairs run one after another, each
# pair two whole gates started together on the ONE shared database:
#
#   pair 1   641/0 and 641/0    277s of 279s overlapping
#   pair 2   641/0 and 641/0    286s of 303s
#   pair 3   641/0 and 641/0    304s of 316s
#
# and the one-variable control, the same two runs against a database EACH:
#
#   641/0 and 641/0             265s of 281s
#
# so the concurrency cost on a shared database is now zero tests, not "a few".
#
# WHAT THE INSTRUMENT LOOKS LIKE WHEN IT IS WORKING, because a green whole-gate
# pair is a weak signal: 641 tests dilute a handful of colliding ones, and a
# pre-fix pair of whole gates ALSO reported 641/0 twice on this cluster. Run
# only the four colliding modules in both processes instead -
#
#   cargo test -p zeroship-auth --test main --no-fail-fast -- --test-threads 1 \
#     magic_link_test:: password_reset_test:: verification_test:: \
#     signup_forgot_ratelimit_test::
#
# - twice at once, and the collisions concentrate. Five such pairs before the
# fixes: 9 of 10 runs red. Five after: 0 of 10.
#
# AND THE PART THAT WAS WORSE THAN "CONCURRENT RUNS GO RED": one of those tests
# POISONED THE SHARED DATABASE FOR EVERY LATER RUN, INCLUDING SINGLE ONES.
# `signup_forgot_ratelimit_test` inserts a user named `M3_FAIL` and used to add
# `CHECK (name <> 'M3_FAIL')` to zeroship.users under a fixed name. Lose the
# race, panic between the insert and the cleanup, and the row stays - and
# because nothing ever drops this database, it stayed forever:
#     add test constraint: ... check constraint
#     "auth_users_signup_m3_name_check" of relation "users" is violated by
#     some row      (SqlState 23514)
# on ONE row left by a concurrent run that died mid-test. No concurrency was
# involved in that failure; the residue was. The constraint now names one
# email, so a leaked one can never match another row.
#
# RECOVERY IS ONE COMMAND, and it is the thing to reach for whenever this gate
# fails in a way that looks like state rather than code:
#
#     psql -c 'DROP DATABASE zeroship_auth_test_<hash>'
#
# The next run recreates and re-migrates it in about a minute. That is the
# whole point of deriving the name - the database is reproducible, so throwing
# it away costs nothing and no one has to decide whether it was still wanted.
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
#   PSQL    (auto-detected; override with an explicit psql path)
#   TEST_THREADS (1)         - live-database tests share rows, so serialize
#
# TEST_DB and SKIP_DB_RECREATE are no longer read here, and a run that finds
# either set in its environment stops rather than quietly going elsewhere.
# ============================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Distinguishes a real failure from a run that could not happen. See the library
# header; `tests/lib_measurement_integrity_selftest.sh` covers both directions.
. "$ROOT/tests/lib/measurement_integrity.sh"
# Names the database after the MIGRATION SET, so every run needing this schema
# shares one and a branch that changes the schema gets its own without being
# told to. See that file's header; `tests/lib_suite_db_selftest.sh` covers both
# directions.
. "$ROOT/tests/lib/suite_db.sh"

# The server's coordinates come from the generated overlay, not from four
# `${PG_x:-...}` lines here and four more in run_billing_suite.sh. See that
# file's header; `tests/provision_test_backends.sh` writes it, and the PG_*
# names are its INPUTS, so setting one still points a run wherever you like.
. "$ROOT/tests/lib/test_config.sh"
zs_test_config_load "$ROOT" || exit 2

TEST_THREADS="${TEST_THREADS:-1}"

# The override is an ARGUMENT. Nothing here reads a database name out of the
# environment; see the header.
DB_OVERRIDE=""
usage() {
  echo "usage: tests/run_auth_suite.sh [--database <name>]" >&2
  echo "  --database <name>  run against a database you name and own. It is" >&2
  echo "                     created if absent and never dropped. Omit it to" >&2
  echo "                     use the shared database named after this tree's" >&2
  echo "                     migration set." >&2
}
while [ "$#" -gt 0 ]; do
  case "$1" in
    --database)
      [ "$#" -ge 2 ] || { echo "FATAL: --database needs a name" >&2; usage; exit 2; }
      DB_OVERRIDE="$2"; shift 2 ;;
    --database=*) DB_OVERRIDE="${1#--database=}"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "FATAL: unknown argument '$1'" >&2; usage; exit 2 ;;
  esac
done

zs_suite_db_resolve zeroship_auth_test "$DB_OVERRIDE" "$ROOT" || exit $?

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
# The suite database name is derived from this tree's migration set and cannot
# live in the shared file, so PG_TEST_URL is exactly the override tier the
# overlay is designed for.
DSN="postgres://${PG_USER}:${PG_PASS}@${PG_HOST}:${PG_PORT}/${TEST_DB}"
export PG_TEST_URL="$DSN"

run_psql() { PGPASSWORD="$PG_PASS" "$PSQL" -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" "$@"; }

# The trap disposes of this run's LOG and nothing else. The database is not
# this run's to drop: it is shared with every concurrent run on the same schema,
# and on a red run it is the primary debugging artifact - the tables a failing
# test left behind are usually the only way to tell a product defect from a
# fixture one. INT and TERM route through `exit` so a cancelled run still
# removes its log; bash runs an EXIT trap on a signal only if the handler exits.
LOG=""
cleanup() {
  if [ -n "$LOG" ]; then rm -f "$LOG"; fi
  return 0
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Create-if-absent and migrate, both under a lock so two agents starting
# together cannot race. Migrate UNCONDITIONALLY, whether this run created the
# database or found it: a run that dies between CREATE and the end of its
# migration leaves a partially journalled database, and the next run's migrate
# is what finishes it. Skipping it on "it already existed" would hand that run
# a half-built schema and blame the tests. The apply is idempotent - it
# re-derives the journal and skips applied files.
#
# The lock covers PROVISIONING ONLY and is released before cargo starts; see
# tests/lib/suite_db.sh for the measurement that put it there and for what it
# does not cover.
echo "==> Provisioning ${TEST_DB} (create if absent, then migrate)"
zs_suite_db_provision \
  env ZEROSHIP_MIGRATE_DSN="$DSN" deploy/ops/db-migrate.sh >/dev/null || exit $?
echo "==> Provisioning complete"

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
# crates/zeroship-gateway/tests/oidc_rp_e2e.rs:46 is now
# `zeroship_core::config::test_database_url_opt()`, one source for every target
# in the workspace. The measurement above is why the collapse is safe here - the
# target was already satisfied by whichever name happened to be exported, which
# is another way of saying the two names never meant different things.
#
# It is in the list below now, and it needs nothing to make a lost database
# visible: the target REFUSES when it cannot resolve one, so this gate goes red
# on the run itself rather than on a marker counted afterwards.
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
# (crates/zeroship-auth/src/oidc/issuer.rs, unconditional in the single mint path), so
# the handler was right and the fixture was wrong. Binding the mock's ID tokens
# to the access token they ship with took it to 23 passed / 0 failed, and
# surfaced a second, real defect on the way (the gateway declined to verify
# at_hash on a ROTATED id_token while holding the access token; see
# crates/zeroship-gateway/src/auth_token.rs).
#
# So both binaries are in the list. This is the SAME database the auth tests
# use: these tests seed their own users and key off per-test UUIDs, and
# TEST_THREADS serializes the run.
#
# GATEWAY_ANCHORS_DB_URL and GATEWAY_POOL_SMOKE_URL were exported here, and
# there is nothing left to export - both now read the single test DSN above.
# Their history is the argument for that collapse rather than a footnote to it:
# GATEWAY_POOL_SMOKE_URL was set NOWHERE in this repository outside its own
# test file and one docs line, so `crates/zeroship-gateway/tests/db_pool_smoke.rs`
# announced a skip on every run of `cargo test --workspace` and its one test had
# never executed. A private name for a value that already exists is a test that
# does not run, and it looks exactly like a test that passes.

echo "==> Other database-gated binaries (authn, authz, mailer, gateway)"
# `zeroship-authn` is here because it was in NO gate at all. Its PostgreSQL
# integration targets announce a skip for every test that cannot reach their
# database; the string "zeroship-authn" appeared nowhere under tests/ or
# .github/. It is not feature-gated, so the `rust` job DID build and run the
# original target with no database, six announced skips, six counted passes,
# and a census that reports rather than fails.
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
# THE SKIP CENSUS THAT STOOD HERE IS GONE, and so is the allowlist it consulted.
#
# It searched this log for a marker every skipping test wrote to stderr, failed
# the run on any occurrence not named in `SKIP_ALLOWLIST`, and carried one
# standing entry (`AUTH_TEST_SMTP_SINK`, a live SMTP sink this script cannot
# stand up). An operator decision removed skipping from the workspace outright:
# every backend guard now REFUSES - it fails the test, naming what was missing
# and the command that provisions it - so there is no marker to count, nothing
# for an allowlist to excuse, and a missing SMTP sink reddens this suite like
# any other absent backend.
#
# WHY COUNTING WAS THE WEAKER DESIGN, kept because it is the argument for what
# replaced it. A census only sees a test that ANNOUNCES. The paragraph below
# records the measurement that made that concrete here: seven OIDC targets
# gated on `let Some(fx) = Fixture::boot(...) else { return; }` and returned in
# silence, so 75 tests reported "ok" in ~0.00s against no database while the
# census printed "0 skipped" and exited 0. Widening the marker cannot fix that;
# only the test failing can.
#
# The floor below is what survives, and it now guards a narrower gap than it
# used to - not because the floor changed, but because the silent-return arm it
# was compensating for no longer exists.

passed="$(grep -oE '^test result: ok\. [0-9]+ passed' "$LOG" | grep -oE '[0-9]+' | awk '{s+=$1} END {print s+0}')"

# A count of failures requires none, so it succeeds when it finds nothing -
# including when there was nothing it COULD find. That was the census's blind
# spot and the reason this floor exists: measured with no database, 75 tests
# across oidc_{refresh_token,authorization_code,userinfo,brokered_login,
# login_consent,backchannel_logout}_test and device_grant_test all reported
# "ok" in ~0.00s, having asserted nothing.
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
AUTH_MIN_PASSED=604
if [ "$passed" -lt "$AUTH_MIN_PASSED" ]; then
  echo "FAIL: only ${passed} auth tests passed, fewer than the ${AUTH_MIN_PASSED} this gate expects." >&2
  echo "A suite that silently stopped running is indistinguishable from a suite that passed." >&2
  echo "If the suite really did shrink, lower AUTH_MIN_PASSED deliberately; do not treat the gap as slack." >&2
  status=1
fi

echo "=================================================================="
if [ "$status" -eq 0 ]; then
  # No skip counts here any more: nothing in the workspace skips, so a line
  # reporting "0 unexpected skips" would be a measurement of an empty set
  # printed as if it were a finding.
  #
  # The bug this line once carried is worth keeping in view, because only the
  # SUCCESS branch reported the totals: it named a census variable without its
  # exported `ZS_` prefix, and under `set -u` a fully green suite therefore
  # aborted HERE, exiting non-zero with no verdict printed, which read as a test
  # failure. A red run took the else branch and reported normally, which is why
  # it survived. Anything added to this branch is reached only when everything
  # else passed, so it is the least exercised line in the script.
  echo "AUTH SUITE: ${passed} tests passed (floor ${AUTH_MIN_PASSED})"
else
  echo "AUTH SUITE: FAILED"
fi
echo "=================================================================="
exit "$status"
