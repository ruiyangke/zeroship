#!/usr/bin/env bash
# ============================================================================
# tests_do_not_create_databases_gate.sh - the harness creates the database; a
# test uses the one it is handed.
#
# WHY
# ---
# A test that provisions its own database decides its own isolation, and the
# decision is invisible from the outside. Three things follow, all of them
# measured on this tree rather than supposed:
#
#   - the harness cannot know how many databases a run will leave behind, so it
#     cannot clean up after one that dies; the cluster at :5440 reached 84
#   - a test that clones the SHARED database cannot run while any other run is
#     connected to it. `CREATE DATABASE x WITH TEMPLATE y` requires exclusive
#     access to y and fails with `source database "y" is being accessed by
#     other users`. That is not a race the test can lose gracefully - it is a
#     panic in setup, attributed to whichever agent happened to start second
#   - a per-test database hides fixture collisions that would otherwise be
#     found early, so they surface later, in CI, as flakes
#
# WHAT THIS GATE CHECKS
# ---------------------
# No Rust test code issues `CREATE DATABASE`, except the files named below,
# each with the reason it is exempt.
#
# BOTH DIRECTIONS. An allowlist entry that no longer matches anything FAILS
# this gate too. An exemption nobody removed after the code changed is how the
# next violation gets waved through: the reader sees a name on the list and
# assumes somebody still thinks about it.
#
# Run directly: tests/tests_do_not_create_databases_gate.sh
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Each entry is `<path>|<reason>`. A reason is required, and it has to say what
# makes the file's own subject impossible to test any other way - "it needs
# isolation" is a description of every test and exempts nothing.
#
# RULED 2026-08-20, one file at a time, from what each test is FOR:
ALLOW=(
  # The subject under test IS the platform migration runner applied to a
  # database that has never been migrated - including
  # `concurrent_migrates_of_two_databases_on_one_cluster_both_succeed`, which
  # exists to prove the cluster-global advisory lock in
  # crates/zeroship-migrate-adapter/src/platform/cluster_lock.rs works, and
  # cannot be written without two databases on one cluster. The databases are
  # per-run named and dropped by the same helper that makes them.
  "crates/zeroship-migrate-adapter/tests/platform_migrate.rs|the runner under test applies to a virgin database, and the cluster-lock test needs two"

  # ONE test: `a_template_clone_is_not_blocked_by_the_previous_runtime`. Its
  # subject IS `CREATE DATABASE ... WITH TEMPLATE` - specifically that a
  # connection dropped by an earlier compio runtime does not leave a backend
  # holding the template. It provisions its own template and its own clone and
  # drops both; every other test in that 2400-line file works inside a private
  # SCHEMA of the harness-provided database.
  "libs/compio-postgres/tests/integration.rs|the statement under test is CREATE DATABASE WITH TEMPLATE itself"

  # NOT an exemption on the merits - a TRACKED VIOLATION, listed so the gate
  # reports the rest of the tree rather than staying unwritten until this is
  # fixed. `build_isolated_fixture_with_gateway` clones a whole database per
  # test from whatever CONTROL_TEST_DB names, ~40 times per run.
  #
  # It has a real reason and an unreal implementation. The real reason: the
  # workflow engine single-flights its fleet-wide sweeps with
  # `pg_try_advisory_lock`, and a PostgreSQL advisory lock is DATABASE-scoped,
  # so two "fleets" cannot coexist in one database - the isolation these tests
  # need genuinely is a database. The unreal part is the SOURCE: it clones the
  # live suite database, and `CREATE DATABASE ... WITH TEMPLATE` requires
  # exclusive access to the source. `DB_CLONE_GATE` at :261 is a
  # `std::sync::Mutex`, which serializes this process and says nothing about a
  # second agent's connections.
  #
  # That is why tests/run_billing_suite.sh still takes a private database while
  # tests/run_auth_suite.sh shares one: converting billing today would make
  # this file fail whenever two agents overlapped. The fix is to clone from a
  # quiescent template the harness provisions, not from the database the suite
  # is running against.
  "crates/control/tests/workflow_engine_test.rs|TRACKED VIOLATION: clones the live suite database per test; blocks sharing the billing database"

  # Not database creation at all: a `const OBJECT_MARKERS` entry and the unit
  # test asserting the classifier matches it. `sql_touches_cluster_global`
  # decides whether a rendered migration statement writes a shared catalog; the
  # string never reaches a server, and this file opens no connection.
  "crates/zeroship-migrate-adapter/src/platform/cluster_lock.rs|a string CLASSIFIED, never executed - the marker table for the cluster-lock router"

  # THIS GATE'S OWN PREMISE, in code. "The harness creates the database and
  # passes the DSN in" - zeroship-testkit IS that harness, and `admin.rs` holds
  # the one statement that does it, behind a trait whose whole point is that
  # every suite goes through it instead of writing its own. It is caught only
  # because the candidate filter counts a `src` file with an inline
  # `#[cfg(test)]` module as test code, which is the right rule meeting the one
  # file it should not apply to. Landed 2026-08-20 and made this gate RED on
  # main until ruled on here.
  "crates/zeroship-testkit/src/admin.rs|the harness's own CREATE DATABASE - the statement every other file is told to use instead of its own"

  # A MATCHER LIMIT, not an exemption on the merits. This file issues no such
  # statement; it asserts on the server's ERROR TEXT, "permission denied to
  # create database", which the case-insensitive match above reads as a
  # statement. Ruled on rather than fixed by dropping `-i`, because a lowercase
  # `create database` in test code is a thing this gate should still catch.
  "crates/zeroship-testkit/src/suite_db.rs|an error MESSAGE asserted on in a unit test, matched only because the search is case-insensitive"
)

echo "==> Scanning Rust test code for CREATE DATABASE"

# Candidates: everything under a `tests/` directory, plus any `src` file
# carrying a `#[cfg(test)]` module. The second half matters - `cluster_lock.rs`
# is a src file, and an inline test module is still test code.
#
# No `-not -path './target/*'`: it was here and could not match. The roots are
# `crates` and `libs`, so every path `find` emits starts `crates/` or `libs/`
# and none can start `./`, and build output does not live under either root
# anyway. Measured 2026-08-20: 1081 candidates with the clause, 1081 without.
# A prune that prunes nothing makes the scan read as broader than it is.
candidates() {
  find crates libs -type f -name '*.rs' \
    \( -path '*/tests/*' -o -path '*/src/*' \) 2>/dev/null | LC_ALL=C sort
}

offenders=""
while IFS= read -r file; do
  [ -f "$file" ] || continue
  case "$file" in
    */tests/*) ;;
    *) grep -q '#\[cfg(test)\]' "$file" || continue ;;
  esac
  # Strip line comments before matching. A doc comment describing the statement
  # is prose, and prose that trips a gate teaches people to write the gate off.
  # `[[:space:]]` rather than a literal space so a wrapped `CREATE\n DATABASE`
  # in a format! string is still caught.
  if sed 's|//.*||' "$file" | tr '\n' ' ' | grep -qiE 'CREATE[[:space:]]+DATABASE'; then
    offenders="${offenders}${file}
"
  fi
done < <(candidates)

status=0

# Direction 1: an offender nobody has ruled on.
while IFS= read -r file; do
  [ -n "$file" ] || continue
  found=0
  for entry in "${ALLOW[@]}"; do
    [ "${entry%%|*}" = "$file" ] && { found=1; break; }
  done
  if [ "$found" -eq 0 ]; then
    echo "FAIL: ${file} issues CREATE DATABASE." >&2
    echo "      The harness creates the database and passes the DSN in; a test" >&2
    echo "      that makes its own decides its own isolation invisibly, and" >&2
    echo "      cannot run against a database anybody else is connected to." >&2
    echo "      If it genuinely cannot be written otherwise, add it to ALLOW in" >&2
    echo "      this file WITH the reason." >&2
    status=1
  fi
done < <(printf '%s' "$offenders")

# Direction 2: a ruling that has outlived its subject. An exemption nobody
# removed is how the next violation gets waved through.
for entry in "${ALLOW[@]}"; do
  file="${entry%%|*}"
  reason="${entry#*|}"
  if [ ! -f "$file" ]; then
    echo "FAIL: ALLOW names ${file}, which does not exist." >&2
    status=1
    continue
  fi
  case "
$offenders" in
    *"
$file
"*) printf '    exempt  %s\n            %s\n' "$file" "$reason" ;;
    *)  echo "FAIL: ${file} is exempt but no longer issues CREATE DATABASE." >&2
        echo "      Remove the entry. A stale exemption reads as a decision" >&2
        echo "      somebody is still making." >&2
        status=1 ;;
  esac
done

n_off="$(printf '%s' "$offenders" | grep -c . )"
echo "==> ${n_off} file(s) issue CREATE DATABASE, ${#ALLOW[@]} ruled on"
if [ "$status" -eq 0 ]; then
  echo "TESTS-CREATE-DATABASES GATE: PASS"
else
  echo "TESTS-CREATE-DATABASES GATE: FAILED" >&2
fi
exit "$status"
