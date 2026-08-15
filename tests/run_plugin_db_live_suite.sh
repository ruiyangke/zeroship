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
# `--features zeroship-control/live-db-tests,zeroship-migrated/live-db-tests`
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
#   1. `-c wal_level=logical`. WITHOUT IT, ELEVEN TESTS SKIP AND STILL REPORT
#      PASSED. Measured on two servers differing in nothing else:
#        replica  test result: ok. 11 passed; 0 failed; 1 ignored;  3.31s
#        logical  test result: ok. 11 passed; 0 failed; 1 ignored; 11.25s
#      IDENTICAL result lines. On replica all eleven skipped; on logical all
#      eleven executed. That is the entire reason the census below is a hard
#      failure rather than a report: the cargo tally cannot see the difference.
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

: "${PG_TEST_URL:?PG_TEST_URL must point at a logical-WAL Postgres with pgvector}"

# `distributed_live` predates this gate and reads `LIVE_DB_TEST_URL`; keep it
# on the same provisioned server as every target unless the caller explicitly
# supplies a separate URL.
export LIVE_DB_TEST_URL="${LIVE_DB_TEST_URL:-$PG_TEST_URL}"

SUITE_LOG="${SUITE_LOG:-${TMPDIR:-/tmp}/plugin-db-live.log}"

# MEASURED on a fresh database provisioned per the requirements above:
#   integration         103 passed / 0 failed / 8 ignored / 0 skips
#   native_transaction    8 passed / 0 failed / 0 ignored / 0 skips
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
PLUGIN_DB_MIN_PASSED="${PLUGIN_DB_MIN_PASSED:-116}"

# Only postgis. An EMPTY allowlist would be wrong in the other direction:
# `grep -E ''` matches every line, so zs_skip_lines branches on empty rather
# than passing it through - see the note in tests/lib/skip_census.sh.
PLUGIN_DB_SKIP_ALLOWLIST="${PLUGIN_DB_SKIP_ALLOWLIST:-postgis}"

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
  echo "      eleven of them hollow." >&2
  rc=1
fi

# A non-zero return is a FAILURE here, not a report, and the distinction is the
# one tests/lib/skip_census.sh draws itself: a gate that has provisioned the
# backend treats a skip as a break, a gate that has not still prints the census
# so the gap is visible. This job provisions it, so it is the former.
if ! zs_skip_census "$SUITE_LOG" "$PLUGIN_DB_SKIP_ALLOWLIST"; then
  echo "FAIL: ${ZS_SKIP_COUNT} test(s) announced they exercised nothing." >&2
  echo "      The likeliest cause is a Postgres that came up WITHOUT" >&2
  echo "      -c wal_level=logical, which makes eleven replication tests skip" >&2
  echo "      while the cargo tally still reads '11 passed; 0 failed'." >&2
  rc=1
fi

exit "$rc"
