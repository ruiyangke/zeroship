# shellcheck shell=bash
# ============================================================================
# suite_db.sh - the SHARED database the live-Postgres test GATES run against,
# named after the schema they need rather than after whoever launched them.
#
# NOT tests/lib/scratch_db.sh. That file serves the e2e stack scripts, which
# stand up a whole platform, assert on the contents of an empty registry, and
# want a virgin database that dies with the run. This file serves
# run_auth_suite.sh and run_billing_suite.sh, which run thousands of tests that
# already scope their own fixtures and want a database that OUTLIVES the run.
# Two policies, two files, each name accurate.
#
# WHAT WAS WRONG WITH A NAME PER AGENT
# ------------------------------------
# Both gates used to take a database name from whoever launched them and open
# with `DROP DATABASE IF EXISTS <name> WITH (FORCE); CREATE DATABASE <name>`.
# Cleanup was therefore bounded by a NAME COLLISION, not by the run's lifetime:
# the database survives until something else picks the same name and forcibly
# drops it. That works only if names are reused, and the entire point of handing
# every agent a private suffix is that they are NOT.
#
# MEASURED 2026-08-19 on the shared cluster at :5440 - 84 databases, whose names
# are an archaeology of every agent slot this project has ever had:
# zeroship_auth_test_{s4,s8,s33,s40,s46,s47,s48,s54,s57,s61,s63,s65b,s65c,s65d,
# s67,s69,s71,v8,v33,v46,v47,v48,v53,v54,v57,v59m,v61,v63,v65,v65b,v69,verify,
# main,r6}, the same again under zeroship_billing_test_, plus a dozen one-off
# names from individual investigations. The population grew with AGENT COUNT.
#
# WHAT THE FRESH DATABASE ACTUALLY BUYS
# -------------------------------------
# Not test isolation - the tests already have that; they scope their fixtures
# with per-run UUIDs. What it buys is SCHEMA FRESHNESS: a guarantee that the
# schema in the database matches the migrations in the tree under test.
#
# That is a property of the BRANCH, not of the agent. So the name is derived
# from the migration set itself - a short hash over the exact `*.ts` files
# `zeroship-platform-migrate` will apply. Each consequence is the point:
#
#   - every agent on the same commit SHARES one database, which is most agents
#     most of the time
#   - an agent on a branch that adds or edits a migration gets its own
#     automatically, with nobody deciding and nobody passing a flag
#   - the population stops growing with agent count and starts growing with
#     SCHEMA VERSIONS, a far smaller number
#   - cleanup becomes DECIDABLE: a database whose hash matches no reachable
#     branch is provably dead, which is what tests/sweep_test_databases.sh
#     relies on instead of guessing whether a name is still in use
#
# WHY CONTENTS AND NOT FILENAMES. A branch that edits an existing migration in
# place changes the schema without changing the file list, and the platform
# runner keys its journal on a sha256 of each file's source bytes - so a
# name-only hash would hand that branch a database whose journal refuses every
# later run with ChecksumMismatch. Hashing contents gives it a fresh database
# instead. The set hashed is exactly `discover_ts_files`'s
# (crates/zeroship-migrate-adapter/src/platform.rs:223-250): `*.ts` in
# db/migrations-ts, ordered by filename.
#
# WHAT THIS DOES *NOT* KEY ON, AND THE HOLE THAT LEAVES. Two branches with
# identical migrations but different Rust CODE share one database. That is
# correct - the schema is identical, which is the only thing the database
# carries - but it is not the same as "safe": a branch whose code expects a
# column it has not yet written a migration for gets a database without that
# column, and the failure reads as an ordinary test failure rather than as a
# missing migration. It reads that way on a PRIVATE database too, for exactly
# the same reason, so this is a property the change neither creates nor cures.
#
# AND THE ONE IT DOES CREATE, WHICH NO HASH CAN SEE: a SINGLETON ROW. The
# migration set describes the schema; it says nothing about whether two runs can
# both hold a row the schema allows only one of. The auth suite had exactly one
# - `zeroship.signing_keys` permits a single `active` OP key per database, and
# `publish_active_key` retires every other active row and refuses to reactivate
# a retired one - so two runs sharing a database spent the whole run retiring
# each other. MEASURED before the fixture fix: 168/97 and 168/103, of which 96
# failures were that one message. See `crates/auth/tests/common/mod.rs`.
#
# It is recorded here because it is the class of hazard this design has, not an
# anecdote about one table: a shared database shares DATABASE-SCOPED SINGLETONS
# - advisory-lock keys, single-active-row registries, fleet-wide sweep locks -
# and the fingerprint cannot warn about any of them. The only instrument that
# finds them is running two suites at once and reading what breaks. Do that
# before pointing a new suite at a shared database; a serial pass proves
# nothing, because serial already worked.
#
# CONCURRENT RUNS ON ONE SHARED DATABASE, and the one place they were NOT safe:
#
#   - CREATE: two runs can both find the database absent and both issue
#     CREATE DATABASE. `zs_suite_db_ensure` treats "it exists now" as success,
#     so the loser proceeds instead of aborting on 42P04.
#   - MIGRATE, database ALREADY migrated: safe, and measured.
#     `run_platform_migrations` brackets its apply in
#     `pg_advisory_lock(hashtext('zeroship'))` taken IN THE TARGET DATABASE
#     (third_party/zero-migrate/.../apply/backend/postgres/session.rs:60),
#     which is exactly the lock that was useless while every run had its own
#     database and works now that they share one. MEASURED 2026-08-20 on 5444,
#     two migrates of one migrated database started together, three rounds:
#     0/0, 0/0, 0/0.
#   - MIGRATE, database NEVER migrated: NOT safe on its own, and this is the
#     hole the design had until it was measured. The same experiment against a
#     freshly created database:
#         migrate A exit=0   migrate B exit=1
#         zeroship-platform-migrate: FAILED: provision schema:
#           duplicate key value violates unique constraint
#           "pg_namespace_nspname_index"
#     The project advisory lock is taken around the APPLY. Provisioning the
#     schema happens BEFORE it, so two first-ever runs race on `CREATE SCHEMA`
#     and one dies. This is not a rare window either: it is exactly what two
#     agents starting together on a newly-written migration hit.
#
#     `zs_suite_db_provision` therefore serializes create+migrate under an
#     `flock` keyed on host:port:database. WHAT THAT DOES NOT COVER: agents on
#     DIFFERENT MACHINES sharing one server. Every agent here runs on one box,
#     and CI gives each job its own Postgres service, so the covered case is
#     the one that exists - but a second machine pointed at the same cluster
#     would reopen the same race, and the fix then is in the migrate binary,
#     beside cluster_lock.rs, not here.
#   - CLUSTER-GLOBAL CATALOGS (pg_authid, pg_auth_members, pg_db_role_setting)
#     are handled separately, since they are shared across every database on the
#     cluster no matter which one issued the write:
#     crates/zeroship-migrate-adapter/src/platform/cluster_lock.rs.
#
# NOTHING IS EVER DROPPED HERE. A failed suite's database is the primary
# debugging artifact, and a shared database is by definition not this run's to
# destroy. Reclaiming space is tests/sweep_test_databases.sh's job, which is
# also the only place that knows which hashes are still reachable.
#
# THE OVERRIDE IS A FLAG, NEVER AN ENVIRONMENT VARIABLE. `--database <name>`
# on the suite script. An ambient `TEST_DB=...` is REFUSED rather than ignored:
# a variable that silently redirects a gate is how gates get silently disabled,
# and refusing is the only behaviour that cannot be inherited from a shell the
# caller forgot about.
#
# tests/lib_suite_db_selftest.sh covers both directions of every function here.
# ============================================================================

# Hash the migration set the platform runner would apply from this tree.
#
# Usage: zs_schema_fingerprint <repo-root>
# Prints: 12 lowercase hex characters. Exit 2 if the directory is missing or
#         holds no migrations - never a fingerprint of nothing, which would be
#         a stable name shared by every broken checkout.
zs_schema_fingerprint() {
  local root="$1" dir file count=0 digest

  dir="$root/db/migrations-ts"
  if [ ! -d "$dir" ]; then
    echo "FATAL: no platform migrations directory at $dir" >&2
    echo "       The suite database is named after the migration set; without" >&2
    echo "       one there is nothing to name it after." >&2
    return 2
  fi

  for file in "$dir"/*.ts; do
    [ -f "$file" ] || continue
    count=$((count + 1))
  done
  if [ "$count" -eq 0 ]; then
    echo "FATAL: $dir holds no *.ts migrations" >&2
    echo "       An empty set would hash to a fixed value, so every broken" >&2
    echo "       checkout would share one database and call it fresh." >&2
    return 2
  fi

  # The BASENAME goes into the hash beside the bytes. The runner orders by
  # filename and journals under it, so `20260101_a.ts` and `20260301_a.ts` with
  # identical bytes are two different schemas and must not share a name.
  #
  # `sha256sum < file` rather than `sha256sum file`, so an absolute path (which
  # differs per worktree) cannot reach the digest - two checkouts of the same
  # commit must fingerprint identically or the sharing never happens.
  #
  # LC_ALL=C sort makes the order independent of the caller's locale; the glob
  # above already sorts, and this makes that a guarantee rather than a default.
  digest="$(
    for file in "$dir"/*.ts; do
      [ -f "$file" ] || continue
      printf '%s ' "${file##*/}"
      sha256sum <"$file"
    done | LC_ALL=C sort | sha256sum
  )"
  if [ -z "$digest" ]; then
    echo "FATAL: could not hash the migrations in $dir" >&2
    return 2
  fi

  printf '%s\n' "${digest:0:12}"
}

# Resolve $TEST_DB for a suite gate.
#
# Usage: zs_suite_db_resolve <prefix> <explicit-name-or-empty> <repo-root>
# Sets:  TEST_DB, ZS_SUITE_DB_EXPLICIT (1 = the caller named it on the command
#        line, 0 = derived from the migration set).
zs_suite_db_resolve() {
  local prefix="$1" explicit="${2:-}" root="$3" fingerprint status

  # Refused, not honoured and not ignored. Honouring it reinstates the ambient
  # opt-out; ignoring it silently sends a caller's run somewhere they did not
  # ask for. Both leave a gate pointed at the wrong database with nobody told.
  if [ -n "${TEST_DB:-}" ]; then
    echo "FATAL: TEST_DB is set in this environment ('${TEST_DB}')." >&2
    echo "       The suite database is derived from the migration set now, and" >&2
    echo "       the override is a FLAG so it cannot be inherited from a shell:" >&2
    echo "         <suite> --database ${TEST_DB}" >&2
    echo "       Unset TEST_DB and pass it there if that is what you meant." >&2
    return 2
  fi
  if [ -n "${SKIP_DB_RECREATE:-}" ]; then
    echo "FATAL: SKIP_DB_RECREATE is set, and there is nothing left for it to do." >&2
    echo "       The suite database is no longer recreated per run - it is named" >&2
    echo "       after the migration set and reused by every run that needs the" >&2
    echo "       same schema. Unset it." >&2
    return 2
  fi

  if [ -n "$explicit" ]; then
    zs_suite_db_check_identifier "$explicit" || return 2
    TEST_DB="$explicit"
    ZS_SUITE_DB_EXPLICIT=1
    export TEST_DB ZS_SUITE_DB_EXPLICIT
    return 0
  fi

  fingerprint="$(zs_schema_fingerprint "$root")"
  status=$?
  [ "$status" -eq 0 ] || return "$status"

  TEST_DB="${prefix}_${fingerprint}"
  ZS_SUITE_DB_EXPLICIT=0
  export TEST_DB ZS_SUITE_DB_EXPLICIT
}

# A database name reaches `CREATE DATABASE` unquoted, so it must be a bare
# identifier. This is the only caller-controlled string in that statement.
zs_suite_db_check_identifier() {
  local name="$1"
  if ! printf '%s' "$name" | grep -qE '^[a-z_][a-z0-9_]*$'; then
    echo "FATAL: '${name}' is not a bare PostgreSQL identifier." >&2
    echo "       Use lowercase letters, digits and underscores only." >&2
    return 2
  fi
  if [ "${#name}" -gt 63 ]; then
    echo "FATAL: '${name}' is ${#name} bytes; PostgreSQL truncates past 63," >&2
    echo "       which would silently merge two databases into one." >&2
    return 2
  fi
  return 0
}

# Does a database exist on the server `run_psql` reaches?
#
# Three-valued on purpose. 0 = present, 1 = absent, 2 = COULD NOT TELL. A psql
# that cannot reach the server prints nothing and exits non-zero, which is
# indistinguishable from an empty result set unless the two are separated here -
# and reading "could not tell" as "absent" turns an unreachable server into a
# CREATE DATABASE that fails for reasons nobody can name.
zs_suite_db_exists() {
  local name="$1" found status

  found="$(run_psql -d postgres -v ON_ERROR_STOP=1 -tAc \
    "SELECT 1 FROM pg_database WHERE datname = '${name}'")"
  status=$?
  if [ "$status" -ne 0 ]; then
    echo "FATAL: could not ask the server whether ${name} exists (psql exit ${status})." >&2
    return 2
  fi
  [ "$found" = "1" ]
}

# Create the suite database if it is not there yet. Never drops, never
# recreates, and tolerates losing the create race to a concurrent run.
#
# The caller must define `run_psql`, and must run the migration itself
# afterwards - unconditionally, not only when this function created something.
# A run that dies between CREATE and the end of its migration leaves a
# partially journalled database, and the next run's migrate is what finishes it.
zs_suite_db_ensure() {
  local name="${TEST_DB:?zs_suite_db_ensure before zs_suite_db_resolve}"
  local status output

  if ! declare -F run_psql >/dev/null 2>&1; then
    echo "FATAL: zs_suite_db_ensure needs the caller's run_psql." >&2
    return 2
  fi

  # `if !`, never a bare call followed by `$?`. Both suites run under `set -e`,
  # where a bare call returning 1 - which is this function's ordinary way of
  # saying "absent" - kills the script before the next line runs. MEASURED: the
  # bare form exited the harness silently right after printing the resolved
  # name, with no error and status 1, which reads as a psql that hung.
  status=0
  zs_suite_db_exists "$name" || status=$?
  case "$status" in
    0) echo "==> Reusing ${name} (schema-keyed; shared with every run on this migration set)"
       return 0 ;;
    2) return 2 ;;
  esac

  echo "==> Creating ${name}"
  # stderr is captured rather than discarded: on the losing side of a create
  # race it carries the only evidence of what happened, and it is re-emitted
  # verbatim below when the database still is not there.
  status=0
  output="$(run_psql -d postgres -v ON_ERROR_STOP=1 -c "CREATE DATABASE ${name};" 2>&1)" || status=$?
  if [ "$status" -eq 0 ]; then
    return 0
  fi

  # A peer creating the same database between our probe and our CREATE is the
  # expected outcome of two agents starting together, not an error. What makes
  # it safe to swallow is that we re-ask the server rather than pattern-matching
  # the message: "it exists now" is the condition we actually need.
  status=0
  zs_suite_db_exists "$name" || status=$?
  case "$status" in
    0) echo "==> ${name} appeared concurrently; another run created it first"
       return 0 ;;
    *) printf '%s\n' "$output" >&2
       echo "FATAL: could not create ${name}." >&2
       return 2 ;;
  esac
}

# Create the database if absent, then run the caller's migrate command - with
# both steps serialized against every other run on this machine targeting the
# same database.
#
# Usage: zs_suite_db_provision <migrate command...>
#
# WHY A LOCK AT ALL, when the migrate binary already takes one. Because its
# lock is taken around the APPLY, and the step that races is the one BEFORE it.
# See the header for the measurement: two first-ever migrates of one database,
# started together, give exit 0 and exit 1, the loser dying on
# `provision schema: duplicate key value violates unique constraint
# "pg_namespace_nspname_index"`.
#
# WHY flock AND NOT AN ADVISORY LOCK. A PostgreSQL session advisory lock has to
# be HELD by an open session for the whole bracket, and the bracket here spans a
# separate process. Holding one from bash means keeping a psql alive on a pipe
# and having no reliable way to know the lock was granted before proceeding -
# a lock you cannot confirm you hold is worse than none, because it reads as
# protection. flock's guarantee is exactly the one wanted: the caller does not
# continue until the descriptor is held. The trade is that it is per-machine;
# the header says what that does not cover.
#
# THE LOCK IS HELD OVER PROVISIONING ONLY, never over the tests. A suite run is
# tens of minutes; serializing that would mean two agents never overlap, which
# is the entire property this file exists to deliver.
zs_suite_db_provision() {
  local name="${TEST_DB:?zs_suite_db_provision before zs_suite_db_resolve}"
  local key lock

  # Keyed on the SERVER as well as the database. Two clusters can carry the
  # same database name, and a lock that ignored the port would serialize runs
  # that cannot touch each other.
  key="$(printf '%s' "${PG_HOST:-}:${PG_PORT:-}:${name}" | sha256sum | cut -c1-16)"
  lock="${TMPDIR:-/tmp}/zeroship-suite-db-${key}.lock"

  # The subshell exists so the descriptor closes - and the lock releases - on
  # every path out, including a failing migrate under `set -e`.
  (
    if ! flock --timeout 900 9; then
      echo "FATAL: waited 900s for another run to finish provisioning ${name}." >&2
      echo "       Lock file: ${lock}" >&2
      exit 2
    fi
    zs_suite_db_ensure || exit $?
    "$@" || exit $?
  ) 9>"$lock"
}
