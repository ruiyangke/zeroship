#!/usr/bin/env bash
# ============================================================================
# run_billing_suite.sh - the CI runner for every zeroship-control /
# zeroship-migrate-server test that needs a live, migrated PostgreSQL.
#
# Database tests run in ordinary cargo test. This runner creates and migrates
# their database, then runs each package without a target list or feature flag.
# Missing infrastructure or failing tests must fail the run.
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
#   tests/run_billing_suite.sh                      # per-run DB + migrate + run
#   TEST_DB=mine tests/run_billing_suite.sh         # name it, and keep it after
#   TEST_DB=mine SKIP_DB_RECREATE=1 tests/run_billing_suite.sh  # reuse that one
#   PG_PORT=5440 PG_USER=postgres PG_PASS=zeroship tests/run_billing_suite.sh
#
# PROVISION FIRST. This script creates and migrates a DATABASE; it does not
# create a SERVER. Stand one up with `tests/provision_test_backends.sh`, which
# brings up deploy/compose's postgres on the port below.
#
# ENV (defaults target deploy/compose's postgres service, published on :5440)
#   The comment here read "the dev compose Postgres on :5440" for months while
#   the server actually answering was `zs-auth-pg-5440`, started by hand and
#   owned by no file in this tree. The address was right, the provenance was
#   not, and the wrong half is the one that reads as though somebody maintains
#   that server.
#   PG_HOST PG_PORT PG_USER PG_PASS - INPUTS to
#     tests/provision_test_backends.sh, which writes the coordinates into
#     deploy/ops/zeroship.test.toml; this script reads them back from there
#     rather than carrying a second copy of the defaults.
#   TEST_DB (per run: zeroship_billing_test_<pid>_<nanos>, dropped on exit)
#   PSQL    (auto-detected; override with an explicit psql path)
#   SKIP_DB_RECREATE (unset)  - reuse a TEST_DB you named; needs one
#   TEST_THREADS (1)          - passed to each binary's `--test-threads`
#   REDPANDA_BROKERS (unset)  - set to gate the real Kafka-wire stream path
#
# The tests never skip for want of a database: PG_TEST_URL is exported below,
# and with it unset the targets fall back to the generated overlay and fail
# loudly if that server is unreachable. A missing database can
# never masquerade as a pass.
# ============================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Distinguishes a real failure from a run that could not happen. See the library
# header; `tests/lib_measurement_integrity_selftest.sh` covers both directions.
. "$ROOT/tests/lib/measurement_integrity.sh"
# Names the database per run. This script drops its database WITH (FORCE) at
# the top, which terminates every other backend on it first - so a fixed name
# means a second run of this script, or of the auth suite pointed at the same
# name, destroys the first one's database mid-run and its failures read as
# product defects. `tests/lib_scratch_db_selftest.sh` covers both directions.
. "$ROOT/tests/lib/scratch_db.sh"

# Workflow acceptance owns its database and service fleet through Testcontainers.
# It also has a dedicated runner: `cargo xtask test workflow`.

# The server coordinates come from the generated overlay. See
# tests/lib/test_config.sh; `tests/provision_test_backends.sh` writes the file,
# and the PG_* names are its INPUTS, so setting one still points a run wherever
# you like.
. "$ROOT/tests/lib/test_config.sh"
zs_test_config_load "$ROOT" || exit 2

zs_scratch_db_resolve zeroship_billing_test || exit $?

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
# Every PostgreSQL fixture, including workflow database cloning, reads this
# shared override. It points at the migrated database owned by this run.
export PG_TEST_URL="$DSN"

run_psql() { PGPASSWORD="$PG_PASS" "$PSQL" -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" "$@"; }

# Armed BEFORE the database is created: a migration that fails leaves one behind
# exactly as a failing test does, and a per-run name is never reused, so nothing
# would ever clean it up. INT and TERM route through `exit` so a cancelled run
# reaches this trap too - bash runs the EXIT trap on a signal only if the
# handler exits.
SUITE_LOG=""
cleanup() {
  if [ -n "$SUITE_LOG" ]; then rm -f "$SUITE_LOG"; fi
  zs_scratch_db_cleanup
  return 0
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Reachability BEFORE the create, so "no server" reads as "no server" rather
# than as a CREATE DATABASE that failed for reasons unknown. The auth gate has
# carried this probe for a while; the difference here is that both now name the
# command that fixes it, which is the whole point of deleting the flag - a
# developer should never have to guess which server was missing or how to get
# one.
run_psql -d postgres -v ON_ERROR_STOP=1 -tAc "select 1" >/dev/null 2>&1 \
  || { echo "FATAL: no PostgreSQL answering at ${PG_HOST}:${PG_PORT}" >&2
       echo "       Provision a server first: tests/provision_test_backends.sh" >&2
       echo "       (or set PG_HOST/PG_PORT/PG_USER/PG_PASS to reach your own)" >&2
       exit 2; }

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

# Serialize this suite’s tests against its shared billing fixture.
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

  # A full disk surfaces as compile and link errors, mid-log, indistinguishable
  # from a defect in the change under test. Say so and STOP: once the volume is
  # full every later group produces the same garbage, so continuing spends an hour
  # manufacturing more misleading output. Checked before the ran-count below,
  # because a build that died for want of space also ran zero tests and would
  # otherwise be reported as a filter that matched nothing.
  if [ "$status" -ne 0 ] && log_shows_disk_full "$group_log"; then
    report_measurement_did_not_run "$group_log" "$1"
    rm -f "$group_log"
    exit 90
  fi

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
echo "==> zeroship-control live-database suite"
if run_group cargo test -p zeroship-control --no-fail-fast \
     -- "${THREAD_ARG[@]}"; then
  :
else
  fail=1
  failed+=("zeroship-control::database")
fi

echo "------------------------------------------------------------------"
echo "==> zeroship-migrate-server live-database suite"
# This package holds the service's own targets AND the two live-PG session proofs
# `smoke_apply_pg` / `author_and_apply_pg`. The invocation below names no targets,
# so a target added to the package is covered here the moment it is added.
#
# No `env PG_TEST_URL=` prefix is needed: `PG_TEST_URL` is EXPORTED near the top of
# this script to `$DSN`, the database THIS SCRIPT ALREADY CREATED. Nothing new is
# provisioned - the two PG proofs create and drop their own token-suffixed
# `proj_*` / `meta_*` schemas.
#
# WHY THE DSN MATTERS, measured on `author_and_apply_pg`, one variable changed:
#   unset -> "test result: ok. 2 passed ... finished in 0.04s", one skip line
#   set   -> "test result: ok. 2 passed ... finished in 0.34s", no skip line
# The result lines are IDENTICAL. Only the clock and the announcement differ,
# which is exactly why being named in a script is not evidence of coverage.
if run_group cargo test -p zeroship-migrate-server --no-fail-fast \
     -- "${THREAD_ARG[@]}"; then
  :
else
  fail=1
  failed+=("zeroship-migrate-server::database")
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
# ran in the control invocation above, where it REFUSES without a broker. Only
# the zeroship-stream half needs naming here.
#
# IT IS NAMED UNCONDITIONALLY, and the `if [ -n "${REDPANDA_BROKERS:-}" ]` that
# stood here is gone with the rest of the skip apparatus. That branch printed
# "==> SKIP real-broker tests" and ran nothing, which is the same escape hatch
# the tests themselves lost, one level up - and it was already contradicted by
# the control-side half three groups above, which had turned the same missing
# broker into a failure. A run without a broker was therefore red either way;
# the branch only decided whether the stream half was ALSO measured. Both halves
# now name the broker as missing, which is what the header above promises.
echo "------------------------------------------------------------------"
echo "==> real-broker: zeroship-stream::redpanda_roundtrip (REDPANDA_BROKERS=${REDPANDA_BROKERS:-unset})"
if run_group cargo test -p zeroship-stream --test redpanda_roundtrip -- "${THREAD_ARG[@]}"; then :; else
  fail=1; failed+=("zeroship-stream::redpanda_roundtrip")
fi

echo "=================================================================="
# THE SKIP CENSUS THAT STOOD HERE IS GONE. The problem it solved is worth
# stating, because the fix moved rather than disappeared: a test that returned
# early because its backend was absent still counted as PASSED, so it sat inside
# the ${passed} total below and inside "ALL GROUPS PASSED", and the per-group
# ran-count above could not see it either - that check catches a filter matching
# nothing (`running 0 tests`), while a skipping test genuinely runs and simply
# tests nothing.
#
# The census distinguished them by counting an announcement. That worked only
# for tests that announced, and only when somebody ran the suite script rather
# than cargo directly. Backend guards now REFUSE instead: the test fails, naming
# what was missing and the command that provisions it, so `run_group` reports it
# like any other failure and there is nothing left for a census to add.
#
# The history that motivated the census stays worth knowing. This check was once
# `|| true`, with a comment saying it would become a gate "once that decision is
# made" and offering ZEROSHIP_REQUIRE_LIVE_BACKENDS=1 as the alternative - so
# the same announcement failed the auth suite and was merely printed by this
# one, and which of two gates you happened to run decided whether a missing
# backend counted. Asymmetries like that are what a single hard failure removes.

if [ "$fail" -ne 0 ]; then
  echo "LIVE-DATABASE SUITE FAILED: ${failed[*]}" >&2
  exit 1
fi

# This runner retains a passed-test floor to detect missing test groups.
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
# 670 -> 758, MEASURED 2026-08-20 on the first run of this script that ever
# reached its end: 816 passed, 0 failed, WITHOUT REDPANDA_BROKERS, so 816 is
# still the lower of the two legitimate configurations. 758 is ~7 percent under
# it, the margin this file and the auth gate both carry.
#
#   control  264 + 16 + 358 + 19 + 1 + 56     = 714
#   migrated 30 + 5 + 25 + 3 + 3              =  66
#   adapter  6 + 5 + 2 + 15 + 1               =  29
#   metering 7                                =   7
#
# The per-group split above is one 2026-08-20 reading; the SUM is what this floor
# sits under. Do not lower it because the groups were rearranged - a run under 758
# means coverage was lost, not that the accounting drifted.
#
# TWO REASONS THE GAP WAS 146 AND NOT SLACK, and the note that stood here
# asserted the second one is why it is worth spelling out:
#
#   - `platform_migrate` was RED on `PLATFORM_MIGRATION_FILES = 22`, so this
#     note said the adapter group contributed 10 and told the next reader to
#     "raise this floor by a further 8" once that was fixed. The constant was
#     DELETED (the file asserts by name now, see its header), the target is
#     green, and the group contributes 29 - so the instruction was for a number
#     that no longer describes anything. A comment that names a deleted constant
#     reads as a live measurement.
#   - The gate itself had never run to its end. It aborted inside
#     `zeroship-control --test main` on a stack overflow, so no run since
#     2026-08-08 produced a total to compare 670 against.
#
# WHAT THIS FLOOR DOES NOT CATCH, measured on the runs above rather than
# reasoned: it did not catch that abort and 758 would not have either. The
# aborting run tallied 791, because a binary that dies prints no
# `test result: ok.` line and its tests are simply absent from a total that is
# still large. What caught it was `run_group`'s exit status. A floor bounds how
# much can go missing QUIETLY; it is not a second opinion on a group that fails
# loudly, and 758 tolerates the loss of any group up to 58 tests.
BILLING_MIN_PASSED=758
if [ "$passed" -lt "$BILLING_MIN_PASSED" ]; then
  echo "FAIL: only ${passed} billing tests passed, fewer than the ${BILLING_MIN_PASSED} this gate expects." >&2
  echo "A group that silently stopped running is indistinguishable from a group that passed." >&2
  echo "If the suite really did shrink, lower BILLING_MIN_PASSED deliberately; do not treat the gap as slack." >&2
  exit 1
fi

# Printed on SUCCESS, not only inside a failure message: a count nobody sees
# until the gate has already failed cannot warn anyone. The floor rides in the
# same line for that reason - "ALL GROUPS PASSED (713 tests)" on its own is the
# sentence that let a suite shrink unnoticed, so the qualifier belongs where
# that sentence is read, not 40 lines earlier in the scrollback.
#
# The two skip counts that used to ride here are gone with the census. Reporting
# "0 unexpected skips" now would measure an empty set and print it as a finding:
# nothing in the workspace skips, so the number cannot be anything else.
echo "LIVE-DATABASE SUITE: ALL GROUPS PASSED (${passed} tests, floor ${BILLING_MIN_PASSED})"
