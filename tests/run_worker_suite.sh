#!/usr/bin/env bash
# ============================================================================
# run_worker_suite.sh - the live-PostgreSQL gate for zeroship-worker.
#
# WHAT IT GATES
# -------------
# `crates/zeroship-worker/src/handler.rs` holds seven tests that drive
# `workflow_advance_unsigned` end to end. Their claim path joins
#
#     zeroship.apps   zeroship.plans   zeroship.app_deploys
#
# (crates/zeroship-plugin-workflow/src/claim.rs:89-91) - PLATFORM tables, which
# exist only after `db/migrations-ts/` has been applied. No fixture creates
# them and none should: they are the same tables production reads.
# `db_posture` has one full boot test behind the same feature because its role
# posture query reads those three platform tables after checking the cluster's
# `max_slot_wal_keep_size`.
#
# THE FAILURE THIS EXISTS TO END. Until 2026-08-28 those seven were ungated and
# nothing provisioned that database, so they ran against the shared, UNMIGRATED
# `zeroship` and died on
#
#     relation "zeroship.plans" does not exist
#
# which `cargo test` prints as seven ordinary FAILED lines. That is a VOID RUN -
# the code under test never executed - wearing the costume of a regression, and
# a suite that is red for environmental reasons trains people to ignore it.
# They now carry `--features live-db-tests` and this script is what runs them.
# `zeroship-control` hit the identical shape on 2026-08-20; see the
# `live-db-tests` block in crates/zeroship-control/Cargo.toml.
#
# WHY BOTH HALVES ARE NEEDED. Gating alone would leave the tests runnable
# nowhere, which is barely better than deleting them. Self-provisioning alone
# would leave the default `cargo test -p zeroship-worker` red on every machine
# without a migrated database. Together: the default command is honest about
# what it ruled on, and this script makes the gated set actually run.
#
# IT DOES NOT SKIP, AND IT DOES NOT CREATE A SERVER
# -------------------------------------------------
# It creates and migrates a DATABASE on a server that must already be
# listening. When there is no overlay, no server, or no `zero-migrate` CLI, it
# exits non-zero naming the command that fixes it and NEVER reaches `cargo
# test` - so no tally is printed that could be mistaken for a pass. Concretely,
# with Docker absent (hence no Postgres):
#
#   - `zs_test_config_load` refuses, naming tests/provision_test_backends.sh;
#   - failing that, `zs_suite_db_provision` fails to reach the server;
#   - failing that, `deploy/ops/db-migrate.sh` exits 2 naming `pnpm build`.
#
# THE DATABASE is `zeroship_worker_test_<hash of db/migrations-ts/*.ts>`,
# created if absent, migrated, and never dropped - the same shape
# tests/run_auth_suite.sh uses, and for the same reasons (two agents on one
# commit share it; a branch that edits a migration gets its own automatically;
# a database no branch can name is provably reclaimable). See tests/lib/
# suite_db.sh.
#
# WHAT IT RULES ON, in the order that matters:
#
#   corpus applied   every file in db/migrations-ts/ was reported by the
#                    applier for THIS database. Reachable is not migrated, and
#                    that distinction is the entire bug above. Counted rather
#                    than assumed, and compared against the files on disk, so a
#                    silently-skipped file cannot pass for a clean apply.
#   live tests ran   at least WORKER_LIVE_MIN of the `workflow_advance_*` tests
#                    and POSTURE_LIVE_MIN full boot tests reported `ok`. THE
#                    LOAD-BEARING ARMS: a total-passes floor cannot tell a
#                    gated-out live set from a shrunken one, because a feature
#                    typo and a deleted test both just print a smaller number.
#   floor            the total pass count did not fall.
#
# There is no "no skips" arm here any more, and the row that named one was the
# last thing in this file describing an apparatus that is gone. A test which
# cannot reach its backend FAILS, so a run that did nothing cannot report a
# tally at all - see the note above the floor below.
#
# USAGE
#   tests/run_worker_suite.sh                    # the shared, schema-keyed DB
#   tests/run_worker_suite.sh --database mine    # one you name and own
#   tests/run_worker_suite.sh --dsn <url>        # a server you control outright
#   TEST_THREADS=1 tests/run_worker_suite.sh     # serialise (see below)
#
# `TEST_DB=... tests/run_worker_suite.sh` is REFUSED, not honoured, by
# `zs_suite_db_resolve`: the override is a flag so a gate cannot be redirected
# by a variable left in a shell nobody remembers exporting it in.
#
# `--dsn` is the same escape hatch tests/platform_migration_corpus_gate.sh
# carries, and it exists for the case that machine has: the overlay names a
# SHARED server, and someone working on this gate needs to migrate a database
# without writing to one. It bypasses the overlay and the derived name; it
# bypasses NO arm - the corpus reconciliation still runs, so pointing it at an
# unmigrated database fails rather than passes.
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 2

# Distinguishes a real failure from a run that could not happen.
# shellcheck source=tests/lib/measurement_integrity.sh
. "$ROOT/tests/lib/measurement_integrity.sh"
# The database, named after the migration set.
# shellcheck source=tests/lib/suite_db.sh
. "$ROOT/tests/lib/suite_db.sh"
# The server's coordinates, from the generated overlay and nowhere else.
# shellcheck source=tests/lib/test_config.sh
. "$ROOT/tests/lib/test_config.sh"

# Default parallel. Each of the seven seeds a fresh `Uuid::new_v4()` app, so it
# owns its `zeroship.apps` row and its own `app_<uuid>` journal schema, and the
# worker's isolate cache is a `thread_local!` that each test initialises on its
# own libtest thread (crates/zeroship-worker/src/cache.rs:56). Measured
# 2026-08-28 against a freshly migrated PostgreSQL 17: 7 passed, 0 failed, at
# the default thread count. TEST_THREADS is here for bisecting a suspected
# race, not because one is known.
TEST_THREADS="${TEST_THREADS:-}"

DB_OVERRIDE=""
DSN_OVERRIDE=""
usage() {
  echo "usage: tests/run_worker_suite.sh [--database <name> | --dsn <url>]" >&2
  echo "  --database <name>  run against a database you name and own on the" >&2
  echo "                     server the overlay points at. It is created if" >&2
  echo "                     absent and never dropped. Omit it to use the" >&2
  echo "                     shared database named after this tree's" >&2
  echo "                     migration set." >&2
  echo "  --dsn <url>        run against a server you control outright. The" >&2
  echo "                     overlay and the derived name are bypassed; the" >&2
  echo "                     database is migrated and every arm still runs." >&2
}
while [ "$#" -gt 0 ]; do
  case "$1" in
    --database)
      [ "$#" -ge 2 ] || { echo "FATAL: --database needs a name" >&2; usage; exit 2; }
      DB_OVERRIDE="$2"; shift 2 ;;
    --database=*) DB_OVERRIDE="${1#--database=}"; shift ;;
    --dsn)
      [ "$#" -ge 2 ] || { echo "FATAL: --dsn needs a url" >&2; usage; exit 2; }
      DSN_OVERRIDE="$2"; shift 2 ;;
    --dsn=*) DSN_OVERRIDE="${1#--dsn=}"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "FATAL: unknown argument '$1'" >&2; usage; exit 2 ;;
  esac
done

if [ -n "$DSN_OVERRIDE" ] && [ -n "$DB_OVERRIDE" ]; then
  echo "FATAL: --dsn names a whole server; --database names one on the overlay's." >&2
  echo "       They select different servers, so passing both cannot mean one thing." >&2
  usage
  exit 2
fi

# The gated tests resolve their DSN through
# `zeroship_core::config::test_database_url`, whose override tier is
# PG_TEST_URL. The suite database name is derived from this tree's migration
# set and so cannot live in the shared overlay; this is exactly what that tier
# is for.
if [ -n "$DSN_OVERRIDE" ]; then
  DSN="$DSN_OVERRIDE"
  # Enough to name the target in the banner and in failures. Not a URL parser -
  # it is a label, and the DSN itself is what every command below is handed.
  TEST_DB="${DSN##*/}"
  TEST_DB="${TEST_DB%%\?*}"
  WHERE="the database you named with --dsn"
else
  zs_test_config_load "$ROOT" || exit 2
  zs_suite_db_resolve zeroship_worker_test "$DB_OVERRIDE" "$ROOT" || exit $?
  DSN="postgres://${PG_USER}:${PG_PASS}@${PG_HOST}:${PG_PORT}/${TEST_DB}"
  WHERE="${PG_HOST}:${PG_PORT}"
fi
export PG_TEST_URL="$DSN"

LOG=""
MIGRATE_LOG=""
cleanup() {
  [ -n "$LOG" ] && rm -f "$LOG"
  [ -n "$MIGRATE_LOG" ] && rm -f "$MIGRATE_LOG"
  return 0
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

MIGRATE_LOG="$(mktemp -t zeroship-worker-migrate.XXXXXX.log)"

# Create-if-absent and migrate, both under a lock so two agents starting
# together cannot race. Migrate UNCONDITIONALLY, whether this run created the
# database or found it: a run that dies between CREATE and the end of its
# migration leaves a partially journalled database, and the next run's migrate
# is what finishes it. The apply is idempotent.
echo "==> Provisioning ${TEST_DB} (create if absent, then migrate)"
if [ -n "$DSN_OVERRIDE" ]; then
  # The caller owns this database's existence and its lifetime, so there is
  # nothing to create and nothing to serialise against - only the migrate.
  env ZEROSHIP_MIGRATE_DSN="$DSN" deploy/ops/db-migrate.sh >"$MIGRATE_LOG" 2>&1
else
  zs_suite_db_provision \
    env ZEROSHIP_MIGRATE_DSN="$DSN" deploy/ops/db-migrate.sh >"$MIGRATE_LOG" 2>&1
fi
provision_status=$?
if [ "$provision_status" -ne 0 ]; then
  echo "FATAL: could not provision ${TEST_DB} at ${WHERE}." >&2
  echo "       NO TEST RAN. This is not a failure; it is the absence of a verdict." >&2
  echo "       Stand up the server:  tests/provision_test_backends.sh" >&2
  echo "       Build the applier:    pnpm install && pnpm build" >&2
  tail -20 "$MIGRATE_LOG" >&2
  exit 2
fi

# REACHABLE IS NOT MIGRATED, and telling them apart is the whole point. The
# applier names every corpus file it considers, on a first apply and on a
# re-apply alike (the second reports `"applied":[]` for each), so the count of
# its report lines is the count of files it ruled on for THIS database.
# Reconciled against the files on disk, because an applier that exits 0 having
# quietly skipped a file is exactly the shape
# tests/platform_migration_corpus_gate.sh was written for.
corpus_files="$(find "$ROOT/db/migrations-ts" -maxdepth 1 -name '*.ts' | wc -l | tr -d ' ')"
applied_files="$(grep -c '^apply ' "$MIGRATE_LOG")"
if [ "$corpus_files" -lt 1 ]; then
  echo "FATAL: db/migrations-ts holds no migrations; there is nothing to apply." >&2
  exit 2
fi
if [ "$applied_files" -ne "$corpus_files" ]; then
  echo "FATAL: the applier reported ${applied_files} of ${corpus_files} corpus files for ${TEST_DB}." >&2
  echo "       A file it never mentioned was skipped, not applied, so this database" >&2
  echo "       is not the schema the worker reads. NO TEST RAN." >&2
  tail -20 "$MIGRATE_LOG" >&2
  exit 2
fi
echo "==> Provisioning complete (${applied_files}/${corpus_files} corpus files applied)"

LOG="$(mktemp -t zeroship-worker-suite.XXXXXX.log)"
status=0

# ONE invocation, and it is by construction rather than a name list: the
# feature satisfies `#[cfg(all(test, feature = "live-db-tests"))]` wherever it
# appears in the crate, so a live test added tomorrow is covered the moment it
# is written. Nothing here enumerates test names.
#
# --no-fail-fast because cargo otherwise STOPS at the first failing target and
# never runs the ones after it, and "not run" is indistinguishable from
# "passed" in the tally below. This crate has two targets and the lib is first.
echo "==> Running zeroship-worker against ${TEST_DB}"
THREAD_ARGS=()
[ -n "$TEST_THREADS" ] && THREAD_ARGS=(--test-threads "$TEST_THREADS")
cargo test -p zeroship-worker --features live-db-tests --no-fail-fast -- \
  "${THREAD_ARGS[@]}" --nocapture 2>&1 | tee "$LOG" || status=1

echo "------------------------------------------------------------------"

# Checked BEFORE any count, because a build that died for want of disk space
# also passes zero tests, and blaming coverage loss for a full volume is advice
# that permanently weakens the guard.
if log_shows_disk_full "$LOG"; then
  report_measurement_did_not_run "$LOG" "the worker suite"
  exit 90
fi

# THE ARM THIS GATE IS FOR. Everything else here would stay green if the seven
# live tests silently stopped being compiled - a misspelled feature name, a
# `#[cfg]` that stopped matching, a moved module - because a gated-out test and
# a deleted one both just make the total smaller.
#
# KEYED ON THE MODULE PATH, not on the test names, and that is not a stylistic
# choice. The first version of this arm matched `workflow_advance_[a-z0-9_]+`
# anywhere in the line and reported EIGHT on a run that has seven, because
# `config::tests::unsigned_workflow_advance_has_no_environment_or_overlay_source`
# is a database-free lib test whose name contains the phrase. An arm that counts
# a neighbouring test is an arm that would stay above its floor with one of the
# seven deleted. `handler::workflow_live_tests` is the module the feature gates,
# so it is the thing to count; a live test added tomorrow under a different name
# is counted for free, and no other test can drift into the set.
WORKER_LIVE_MIN=7
live_ran="$(grep -cE '^test handler::workflow_live_tests::[a-z0-9_]+ \.\.\. ok$' "$LOG")"
if [ "$live_ran" -lt "$WORKER_LIVE_MIN" ]; then
  echo "FAIL: only ${live_ran} workflow_advance test(s) passed, fewer than the ${WORKER_LIVE_MIN} this gate exists to run." >&2
  echo "This gate's entire purpose is that those tests execute against a migrated" >&2
  echo "database. A smaller number means they were gated out, renamed or deleted -" >&2
  echo "not that the suite got faster. Check that the crate still declares" >&2
  echo "\`live-db-tests\` and that handler::workflow_live_tests is still behind it." >&2
  status=1
fi

# The full boot posture test needs the migrated platform projection above, so
# it is feature-gated for the same reason as the workflow tests. Count its exact
# module path separately: the broad pass floor stays green if one gated test
# disappears, and the workflow arm cannot see a test outside its module.
POSTURE_LIVE_MIN=1
posture_live_ran="$(grep -cE '^test db_posture::tests::worker_boot_refuses_unlimited_replication_slot_wal_retention \.\.\. ok$' "$LOG")"
if [ "$posture_live_ran" -lt "$POSTURE_LIVE_MIN" ]; then
  echo "FAIL: only ${posture_live_ran} full boot-posture test(s) passed, fewer than the ${POSTURE_LIVE_MIN} this gate requires." >&2
  echo "The max_slot_wal_keep_size regression was gated out, renamed, deleted or" >&2
  echo "did not reach its migrated database. This is no verdict on worker boot." >&2
  status=1
fi

# THE SKIP CENSUS THAT STOOD HERE IS GONE. It carried no allowlist, because
# nothing in this crate announced a skip - so it was the arm of this script that
# ruled on the emptiest set, and it is the one whose deletion changes least.
# Workspace-wide, a test that cannot reach its backend now FAILS rather than
# announcing, so there is no marker to count in this log.
#
# The blunt instrument below is unaffected, and it is what the named count could
# never see anyway: the
# other 108 tests quietly disappearing.
#
# MEASURED 2026-08-29 against a migrated PostgreSQL 18: 116 passed
# (35 lib + 81 bin + 0 doctests), 0 failed. The floor carries ~9 percent
# headroom, the same margin tests/run_auth_suite.sh and the CI test-target
# floor use. Raise it as the crate grows; a fixed floor gets looser with every
# test added, which is the wrong direction for a guard against coverage loss.
#
# IT DOES NOT COVER EITHER NAMED LIVE SET AND MUST NOT BE READ AS DOING SO.
# Misspelling the feature in the `#[cfg]` would report 108 passed - ABOVE this
# floor - while the two named arms report 0 of 7 and 0 of 1. The three arms are
# complementary, not redundant. A named set that loses one pass is a change no
# percentage floor can distinguish from a Tuesday.
WORKER_MIN_PASSED=106
passed="$(grep -oE '^test result: ok\. [0-9]+ passed' "$LOG" | grep -oE '[0-9]+' | awk '{s+=$1} END {print s+0}')"
if [ "$passed" -lt "$WORKER_MIN_PASSED" ]; then
  echo "FAIL: only ${passed} worker tests passed, fewer than the ${WORKER_MIN_PASSED} this gate expects." >&2
  echo "A suite that silently stopped running is indistinguishable from one that passed." >&2
  echo "If the crate really did shrink, lower WORKER_MIN_PASSED deliberately; the gap is not slack." >&2
  status=1
fi

echo "=================================================================="
if [ "$status" -eq 0 ]; then
  # No skip count on this line. The census that produced one was deleted from
  # this file; a constant "0 unexpected skips" printed beside three measured
  # numbers reads as a fourth measurement and is a claim about an empty set.
  echo "WORKER SUITE: ${passed} tests passed (floor ${WORKER_MIN_PASSED}), ${live_ran} live workflow-advance (floor ${WORKER_LIVE_MIN}), ${posture_live_ran} live boot-posture (floor ${POSTURE_LIVE_MIN})"
  echo "              against ${TEST_DB} at ${WHERE}"
else
  echo "WORKER SUITE: FAILED"
fi
echo "=================================================================="
exit "$status"
