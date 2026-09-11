#!/usr/bin/env bash
#
# The live-Postgres gate for zeroship-data-v8.
#
# WHY THIS EXISTS. `crates/zeroship-data-v8/tests/integration.rs` holds 111 tests that
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
#   1. `-c wal_level=logical`. IT IS STILL NOT OPTIONAL, but the way a missing
#      setting presents has changed, and the old measurement is why the change
#      was made. Ten tests USED TO SKIP AND STILL REPORT PASSED. Measured on two
#      servers differing in nothing else:
#        replica  test result: ok. 11 passed; 0 failed; 1 ignored;  3.31s
#        logical  test result: ok. 11 passed; 0 failed; 1 ignored; 11.25s
#      IDENTICAL result lines, distinguishable only by elapsed time. On replica
#      all of them skipped; on logical all of them executed. The cargo tally
#      could not see the difference, so a census over the run log had to.
#
#      THE TESTS THEMSELVES NOW REFUSE. A server without logical WAL fails them,
#      naming the setting, that it is postmaster-level, and that `ALTER SYSTEM
#      SET` needs a RESTART and not a reload. Two result lines that differ only
#      in duration is no longer a state this suite can reach, which is what
#      retired the census that used to be the only thing telling them apart.
#
#      TEN, not the eleven this said until 2026-08-18. Re-measured that day on
#      the FULL target against two servers differing only in wal_level: the
#      replica announced one skip per wal-gated test plus one for postgis, the
#      logical server only the postgis one, and `pg_has_logical_wal` has exactly
#      10 call sites. The older figure was taken on a filtered run and copied
#      into three files; the re-measurement differs from it by one and I did not
#      reproduce the original setup, so treat ten as "what the tree did then"
#      rather than as a correction of what it did before that.
#      NOTE this cannot be expressed in a GitHub `services:` block, which takes
#      no command arguments - hence the explicit `docker run` in ci.yml, the
#      same pattern golden-path and dev-vs-deployed already use.
#
#   2. pgvector, AND the extension actually created in the target database.
#      Available is NOT enough: a fresh database with the extension merely
#      available fails with `type "vector" does not exist`.
#
#   3. postgis, and it is a REQUIREMENT of this suite rather than a tolerated
#      absence. `spatial_near_runs_under_per_app_role_via_rls` used to announce
#      a skip on a server without the extension and be excused by name in a
#      census here; both the announcement and the census are gone, so it now
#      FAILS, naming the extension and how to get it. Nothing in this file
#      excuses it and there is no variable that turns it back into a pass:
#      point this suite at a server carrying postgis, or the run is red.
#      Its `#[ignore]`d twin `near_returns_within_radius` is not in that count -
#      cargo never builds it into a default run - so the extension being
#      missing costs exactly one failure, not two.
#      ci.yml's `data-v8-live-gate` installs the postgis package into the
#      pgvector container for exactly this reason; the two images are disjoint
#      and no published one carries both.
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

. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init data_database_required

metadata=$(cargo metadata --no-deps --format-version 1) || exit 1
checked=0
while IFS=$'\t' read -r package gated; do
  checked=$((checked + 1))
  if [ "$gated" = true ]; then
    echo "FAIL: $package must include database tests without feature gates" >&2
    exit 1
  fi
done < <(printf '%s' "$metadata" | jq -r '
  .packages[] | select(.name == "zeroship-data-orm" or .name == "zeroship-data-v8") |
  [.name, ((.features | has("live-db-tests")) or
    any(.targets[]; ((.["required-features"] // []) | length) > 0))] | @tsv')
gate_arm ordinary_database_targets "$checked" 2 || exit 1

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

SUITE_LOG="${SUITE_LOG:-${TMPDIR:-/tmp}/data-v8-live.log}"

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
# WHAT IS IGNORED TODAY IS NOT WHAT THIS PASSAGE SAID, and the correction is the
# point rather than the number: it named a pgvector group among the ignored, and
# no pgvector test in that file carries the attribute. Count them where they are
# declared instead of reading a tally here -
#   grep -c '^#\[ignore' crates/zeroship-data-v8/tests/integration.rs
# - and read the reasons on the attributes for which prerequisite each wants. A
# test whose prerequisite is NOT statically ignored fails when the server lacks
# it; that is the postgis case in requirement 3 above.
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
#       per-collection schema boot cutover until someone read it. Removing the attribute is
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
#   -15  the platform system-schema deletion (`390f4b97b`) removed 15 `integration`
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
# MOVED 100 -> 114 on 2026-08-29 after re-running every target in this loop on
# a fresh dedicated `pgvector/pgvector:pg16` cluster with logical WAL:
#   integration         81  (0 failed, 6 ignored)
#   native_transaction  24  (0 failed)
#   distributed_live     1  (0 failed)
#   missing_role         3  (0 failed)
#   column_grants        5  (0 failed)
#   SUM                114
# The supplied `zs-dbbind-pg-5490` image has no pgvector package. The same run
# there measured 112 passed / 2 failed: both failures were the known missing-
# pgvector arms. `column_grants` itself passed 5 / 0 on that server.
#
# WHY THE INSTRUMENT CHANGED TOO: roles are cluster-scoped, so a fresh database
# on a shared server is not isolation. A leftover `p6a_unmask_login` produced
# `CREATE ROLE ... 42710` and masked a real defect behind a collision error.
# Re-measure on a dedicated cluster or this number will not reproduce.
PLUGIN_DB_MIN_PASSED=114

echo "==> zeroship-data-v8 live-database suite"
echo "    PG_TEST_URL=${PG_TEST_URL%%\?*}"

# Run all targets; database fixtures within each target run serially.
suite_rc=0
: > "$SUITE_LOG"
# Build the real relay used by distributed V8 subscriptions.
cargo build -p zeroship-data-cdc-server || exit 1
# Ordinary package tests include every database target and enable their helpers.
cargo test -p zeroship-data-orm -p zeroship-data-v8 --no-fail-fast \
  -- --nocapture --test-threads=1 2>&1 | tee -a "$SUITE_LOG"
suite_rc=${PIPESTATUS[0]}

passed=$(grep -a '^test result:' "$SUITE_LOG" | sed 's/.*ok\. \([0-9]*\) passed.*/\1/;t;s/.*FAILED\. \([0-9]*\) passed.*/\1/;t;d' \
  | awk '{s+=$1} END {print s+0}')
failed=$(grep -a '^test result:' "$SUITE_LOG" | sed 's/.*; \([0-9]*\) failed.*/\1/;t;d' \
  | awk '{s+=$1} END {print s+0}')

echo "==> data-v8 live suite: ${passed} passed, ${failed} failed (floor ${PLUGIN_DB_MIN_PASSED})"

rc=0
[ "$suite_rc" -ne 0 ] && rc=1

if [ "$passed" -lt "$PLUGIN_DB_MIN_PASSED" ]; then
  echo "FAIL: ${passed} passed is below the floor of ${PLUGIN_DB_MIN_PASSED}." >&2
  echo "      Either a test target stopped running, or the database is missing" >&2
  echo "      a prerequisite. Read the FAILURES above before assuming a product" >&2
  echo "      regression: a missing wal_level, pgvector or PostGIS now names" >&2
  echo "      itself in the failing test rather than lowering this count" >&2
  echo "      silently." >&2
  rc=1
fi

gate_arm database_tests "$passed" "$PLUGIN_DB_MIN_PASSED" || rc=1
gate_arms_finish || rc=1

# THE SKIP CENSUS THAT STOOD HERE IS GONE, along with the `postgis` entry that
# was the only thing it excused. It searched this log for the announcements the
# wal- and extension-gated tests wrote and failed the run on any it did not
# tolerate.
#
# The wal_level failure it was built for is now caught by the tests themselves.
# Those guards REFUSE: on a server started without `-c wal_level=logical` the
# replication tests FAIL, naming the setting, that it is postmaster-level, and
# that `ALTER SYSTEM SET` needs a restart rather than a reload. The identical
# result lines this file's header records - the same "11 passed; 0 failed"
# printed by a server where all eleven ran and by one where ten did nothing -
# cannot happen any more, because the second server does not print a pass.
#
# So the diagnosis moved from this script to the failing test, which is where a
# reader who ran cargo directly can also see it. The floor above still guards
# the other direction: a target that stops running entirely.
exit "$rc"
