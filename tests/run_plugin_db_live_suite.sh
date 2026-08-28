#!/usr/bin/env bash
#
# The live-Postgres gate for zeroship-plugin-db.
#
# WHY THIS EXISTS. `crates/plugin-db/tests/integration.rs` holds 111 tests that
# dial a real Postgres, and until 2026-08-12 NOT ONE OF THEM RAN ANYWHERE.
# Measured, three independent greps, all empty: no workflow sets PG_TEST_URL, no
# workflow invokes `--test integration`, and no `pg-test` image exists in the
# tree. The `rust` job omits the target deliberately, deferring it to "the other
# live-database gates", but that gate runs
# `--features zeroship-control/live-db-tests,zeroship-migrate-server/live-db-tests`
# and this crate declares no `live-db-tests` feature, so the deferral named a
# destination that could not accept it. The binary fell between two jobs.
#
# What that cost, concretely: `b2_ref_creates_foreign_key` asserted the FK
# default contract the project decided AGAINST (#44), and sat red for however
# long, because nothing ran it. Fixed at a182cc22a.
#
# ---------------------------------------------------------------------------
# THE PROVISIONING REQUIREMENTS ARE NOT OPTIONAL, and each was measured rather
# than assumed. Progression, one variable per step:
#
#   plain postgres:16, fresh DB            100 passed /  3 failed
#   + the b2 FK assertion corrected        101 passed /  2 failed
#   + pgvector image, wal_level=logical    102 passed /  1 failed
#   + `CREATE EXTENSION vector` in the DB  103 passed /  0 failed, 8 ignored
#
#   1. `-c wal_level=logical`. WITHOUT IT, TEN TESTS SKIP AND STILL REPORT
#      PASSED. Measured on two servers differing in nothing else:
#        replica  test result: ok. 11 passed; 0 failed; 1 ignored;  3.31s
#        logical  test result: ok. 11 passed; 0 failed; 1 ignored; 11.25s
#      IDENTICAL result lines. On replica all of them skipped; on logical all
#      of them executed. That is the entire reason the census below is a hard
#      failure rather than a report: the cargo tally cannot see the difference.
#
#      TEN, not the eleven this said until 2026-08-18. Re-measured that day on
#      the FULL target against two servers differing only in wal_level:
#        replica  11 ZEROSHIP-TEST-SKIPPED markers (10 wal-worded + 1 postgis)
#        logical   1 marker (postgis)
#      and `pg_has_logical_wal` has exactly 10 call sites. The older figure was
#      taken on a filtered run and copied into three files; the re-measurement
#      differs from it by one and I did not reproduce the original setup, so
#      treat ten as "what the tree does today" rather than as a correction of
#      what it did then.
#      NOTE this cannot be expressed in a GitHub `services:` block, which takes
#      no command arguments - hence the explicit `docker run` in ci.yml, the
#      same pattern golden-path and dev-vs-deployed already use.
#
#   2. pgvector, AND the extension actually created in the target database.
#      Available is NOT enough: a fresh database with the extension merely
#      available fails with `type "vector" does not exist`.
#
#   3. postgis is NOT provisioned, so exactly one test announces a skip
#      (`spatial_near_runs_under_per_app_role_via_rls`). It is allowlisted
#      BY NAME below rather than by raising a tolerance, so the day postgis
#      arrives the allowlist stops matching nothing and someone has to look.
#
# A TRAP FOR WHOEVER DEBUGS A FAILURE HERE: several tests cannot be run in
# isolation on a fresh database. `p4_round_trip_encrypted_masked_vector_via_
# introspected_metadata` needs both the vector extension AND an admin bootstrap
# an EARLIER test performs; run alone it fails with `type "vector" does not
# exist`, and run alone WITH the extension it fails differently, with
# `column_key_not_configured`. Both are artefacts of running it alone. The full
# suite in order is green. Do not chase either error as a product defect
# without first reproducing it in a full run.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# The server may come from the generated overlay, so this no longer demands
# PG_TEST_URL be exported - it demands that SOMETHING names a Postgres. The
# overlay is written by tests/provision_test_backends.sh; PG_TEST_URL overrides
# it, and this gate additionally needs logical WAL and pgvector, which the
# compose server provides and an arbitrary one may not.
. "$ROOT/tests/lib/test_config.sh"
if [ -z "${PG_TEST_URL:-}" ]; then
  zs_test_config_load "$ROOT" || exit 2
  export PG_TEST_URL="$ZS_TEST_PG_DSN"
fi

# LIVE_DB_TEST_URL is GONE. `distributed_live` predated this gate and read its
# own name for the same server, so this script re-exported one variable as the
# other to keep them on one database - a workaround for the sprawl rather than a
# fix for it. That target now resolves through
# zeroship_core::config::test_database_url_opt like every other, so there is
# nothing left to bridge.

SUITE_LOG="${SUITE_LOG:-${TMPDIR:-/tmp}/plugin-db-live.log}"

# MEASURED on a fresh database provisioned per the requirements above:
#   integration         103 passed / 0 failed / 8 ignored / 0 skips
#   native_transaction    8 passed / 0 failed / 0 ignored / 0 skips
# Those two lines are the 2026-08-18 reading and are left as read. `integration`
# now reports 6 ignored, not 8, and only ONE of the two is this change: removing
# `#[ignore]` from `parity_matrix_pg_matches_sqlite_projection` (accounted at the
# floor below). The other had already gone by 2026-08-21 -- the tree holds seven
# `#[ignore]` in this file before that removal, counted by grep, so the target
# lost one between the two dates and nothing recorded it. Treat the passed/failed
# columns of the old reading the same way: re-measure rather than adjust.
# The 6 that remain are the pgvector (3), PostGIS (1) and pg_dump (2)
# prerequisites named above.
# The floor is the SUM, 111, and it is the measured number rather than a round
# one below it: these binaries have a fixed test count, so any shortfall means a
# target stopped running rather than a test getting faster. Raise it
# deliberately when tests are added; do not lower it to match a red run.
# MOVED 111 -> 110 on 2026-08-12, and NOT to match a red run. The accounting,
# one line per test, all three verified against `git show` of the prior commit:
#   -1  p8a2_auto_spawn_is_idempotent_via_registry was VACUOUS. It cleared the
#       registry, asserted not-registered, and ended `let _ = app;` without ever
#       registering anything. Restoring it would raise this number and test
#       nothing, which is the failure this floor exists to prevent.
#   -1  p8a2_auto_spawn_via_callback_short_circuits proved WAL delivery reached a
#       broker subscription in ONE isolate. `distributed_live` now proves the
#       same delivery ACROSS isolates, which is strictly stronger.
#   -1  p8a2_supervised_consumer_exits_on_slot_invalidated drove `WalConsumer` +
#       `run_supervised` directly. Both became `pub(crate)`, so no out-of-crate
#       test can construct them. The property it guarded is now fail-loud
#       startup (cdc_lifecycle: a startup failure rejects the stream instead of
#       leaving it healthy-looking and static) and is STILL UNTESTED - the one
#       real debt from that change.
#   +1  distributed_live, now listed above.
# Net -2 removed +1 added against the previous 111.
# The three `missing_role` tests cover contextual session setup, generic pool
# reconnect classification, and the fixed wire message. Counting them in the
# floor ensures all three run in CI rather than only when invoked by hand.
# The three real-Runtime unmigrated-path tests added to `native_transaction`
# raise the measured full-suite census from 113 to 116.
#   +1  parity_matrix_pg_matches_sqlite_projection, 2026-08-21. It is not a NEW
#       test - it has been in `integration` all along, `#[ignore]`d with the
#       reason "default gate runs the sqlite leg only", which named THIS script
#       and was wrong about it: nothing here passes `--ignored`, so the one job
#       that could run it never did, and it sat broken from the 2026-08-10
#       registerModel cutover until someone read it. Removing the attribute is
#       what puts it in this count.
#   +1  bytes_column_stores_raw_bytes_on_postgres. A genuinely new test in
#       `integration`, and the only one of the two written for the t.bytes()
#       double-encode that this script can see: its SQLite twin
#       (`bytes_column_stores_a_raw_blob_on_sqlite`) lives in
#       `sqlite_integration`, which is not a target below and contributes
#       nothing to this floor.
# MOVED 118 -> 100 on 2026-08-27, and NOT to match a red run: the run that
# produced this number is 100 passed / **0 failed** across all four targets. The
# floor was the only thing red.
#
# Measured composition, on a fresh database on a DEDICATED cluster
# (`zs-adminfix-pg-5471`) rather than the shared 5463:
#   integration         82  (0 failed, 6 ignored)
#   native_transaction  14  (0 failed)
#   distributed_live     1  (0 failed)
#   missing_role         3  (0 failed)
#   SUM                100
#
# Accounting for -18, every line verified rather than inferred:
#   -15  the `__zeroship_admin` deletion (`390f4b97b`) removed 15 `integration`
#        tests, all named in that commit's review: the 13 `b8c_*` admin-schema
#        arms plus `pg_admin_table_key_source_reads_bytea_directly` and
#        `pitr_pg_records_target`. Their subject is gone, not their coverage.
#    +1  `pg_declared_mask_policy_authorizes_unmask_without_durable_store`
#        (`integration.rs:6001`), ADDED by `b801bf12b` because the behaviour it
#        changed had no live-PG coverage at all.
#    +1  `distributed_live` now PASSES. It was counted in the old floor while
#        failing (`replication_publication_missing`), so the 118 was only ever
#        reachable on a tree where this target was red -- an unreachable floor
#        is not a floor. Fixed in `a0074e154`.
#   The residual -5 against 118 is NOT explained by this change and is not
#   invented here: 118 was last re-derived on 2026-08-21 and the targets have
#   moved since without the ledger being re-measured. That is exactly the drift
#   this comment block exists to stop, so the number above is a fresh
#   measurement rather than 118 minus arithmetic.
#
# WHY THE INSTRUMENT CHANGED TOO: roles are cluster-scoped, so a fresh database
# on a shared server is not isolation. A leftover `p6a_unmask_login` produced
# `CREATE ROLE ... 42710` and masked a real defect behind a collision error.
# Re-measure on a dedicated cluster or this number will not reproduce.
PLUGIN_DB_MIN_PASSED=100

# Only postgis. An EMPTY allowlist would be wrong in the other direction:
# `grep -E ''` matches every line, so zs_skip_lines branches on empty rather
# than passing it through - see the note in tests/lib/skip_census.sh.
PLUGIN_DB_SKIP_ALLOWLIST="postgis"

. "$ROOT/tests/lib/skip_census.sh"

echo "==> zeroship-plugin-db live-database suite"
echo "    PG_TEST_URL=${PG_TEST_URL%%\?*}"

# ALL live-Postgres targets. `integration` and `native_transaction` are the two
# siblings ci.yml names together as belonging "with the other live-database
# gates"; running only the first would leave the second in exactly the limbo
# this script exists to end. `distributed_live` joined them on 2026-08-12 for
# the same reason: it dials the same server, needs the same wal_level=logical,
# and was reachable only by hand until it was listed here.
#
# `--test-threads=1` is required, not tidiness: these tests share one database
# and create identically-named schemas, which is the same hazard #78 fixed for
# compio-postgres.
suite_rc=0
: > "$SUITE_LOG"
# `live-db-tests` NOT `test-helpers`: it is a superset
# (live-db-tests = ["test-helpers"]), `distributed_live` declares
# `required-features = ["live-db-tests"]`, and `missing_role` declares
# `required-features = ["test-helpers"]`. Passing the narrower feature makes
# cargo REFUSE the distributed target with "requires the features", while the
# superset lets all four targets build.
for target in integration native_transaction distributed_live missing_role; do
  echo "--- cargo test --test ${target} ---" | tee -a "$SUITE_LOG"
  cargo test -p zeroship-plugin-db --features live-db-tests --test "$target" \
    -- --test-threads=1 2>&1 | tee -a "$SUITE_LOG"
  [ "${PIPESTATUS[0]}" -ne 0 ] && suite_rc=1
done

# Sum EVERY `test result:` line rather than reading the last one. One binary
# emits one line today, but a tail would silently start lying the moment a
# second target is added here.
passed=$(grep -a '^test result:' "$SUITE_LOG" | sed 's/.*ok\. \([0-9]*\) passed.*/\1/;t;s/.*FAILED\. \([0-9]*\) passed.*/\1/;t;d' \
  | awk '{s+=$1} END {print s+0}')
failed=$(grep -a '^test result:' "$SUITE_LOG" | sed 's/.*; \([0-9]*\) failed.*/\1/;t;d' \
  | awk '{s+=$1} END {print s+0}')

echo "==> plugin-db live suite: ${passed} passed, ${failed} failed (floor ${PLUGIN_DB_MIN_PASSED})"

rc=0
[ "$suite_rc" -ne 0 ] && rc=1

if [ "$passed" -lt "$PLUGIN_DB_MIN_PASSED" ]; then
  echo "FAIL: ${passed} passed is below the floor of ${PLUGIN_DB_MIN_PASSED}." >&2
  echo "      Either a test target stopped running, or the database is missing" >&2
  echo "      a prerequisite. Check the census below before assuming a product" >&2
  echo "      regression: a suite that cannot reach Postgres reports few passes," >&2
  echo "      and one that reaches a REPLICA server reports the full count with" >&2
  echo "      ten of them hollow." >&2
  rc=1
fi

# A non-zero return is a FAILURE here, not a report, and the distinction is the
# one tests/lib/skip_census.sh draws itself: a gate that has provisioned the
# backend treats a skip as a break, a gate that has not still prints the census
# so the gap is visible. This job provisions it, so it is the former.
#
# A REFUSAL (status 2) is a third outcome, distinct from both: the log is
# missing or empty, so no census happened and the diagnosis below - which blames
# a Postgres started without wal_level=logical - would be a guess about a run
# that produced no evidence at all.
census_rc=0
zs_skip_census "$SUITE_LOG" "$PLUGIN_DB_SKIP_ALLOWLIST" || census_rc=$?
if [ "$census_rc" -eq "$ZS_SKIP_REFUSED_STATUS" ]; then
  echo "FAIL: the skip census refused ${SUITE_LOG}, so this run proved nothing" >&2
  echo "      about skips. Do not read the ${passed} passes above as coverage" >&2
  echo "      until the log the suite tees to is the log censused here." >&2
  rc=1
elif [ "$census_rc" -ne 0 ]; then
  echo "FAIL: ${ZS_SKIP_COUNT} test(s) announced they exercised nothing." >&2
  echo "      The likeliest cause is a Postgres that came up WITHOUT" >&2
  echo "      -c wal_level=logical, which makes ten replication tests skip" >&2
  echo "      while the cargo tally still reads '11 passed; 0 failed'." >&2
  rc=1
fi

exit "$rc"
