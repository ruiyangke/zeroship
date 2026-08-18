#!/usr/bin/env bash
# Self-test for tests/lib/scratch_db.sh.
#
# The library exists so two concurrent suite runs cannot drop each other's
# database. That is only worth anything if it is right in BOTH directions:
#
#   a generated name MUST be unique and MUST be dropped   - else the fix trades
#                                                           a collision for a
#                                                           server slowly
#                                                           filling with orphans
#   a caller-named database MUST NEVER be dropped         - `TEST_DB=...` is how
#                                                           a run is kept for
#                                                           inspection, and how
#                                                           SKIP_DB_RECREATE
#                                                           reuses one; dropping
#                                                           it would destroy
#                                                           exactly the data the
#                                                           caller asked to keep
#
# The cleanup cases run against a FAKE run_psql that records the SQL it is
# handed, so they assert what the trap actually issues rather than that some
# database vanished. The failing-exit case runs a real subshell with the real
# trap installed, because "drops on success" and "drops on every exit" produce
# identical output on a green run.
#
# Run directly: tests/lib_scratch_db_selftest.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LIB="$ROOT/tests/lib/scratch_db.sh"
. "$LIB"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail=0
pass=0

ok()   { pass=$((pass + 1)); echo "ok   - $1"; }
bad()  { fail=$((fail + 1)); echo "FAIL - $1" >&2; }

check() { # check <label> <expected> <actual>
  if [ "$2" = "$3" ]; then ok "$1 ($3)"; else bad "$1: expected '$2', got '$3'"; fi
}

echo "=== an explicit TEST_DB is honoured verbatim and is NOT owned by the run ==="
( unset ZS_SCRATCH_DB_GENERATED
  TEST_DB=zeroship_auth_test_byhand
  zs_scratch_db_resolve zeroship_auth_test
  printf '%s %s\n' "$TEST_DB" "$ZS_SCRATCH_DB_GENERATED" ) > "$TMP/explicit"
read -r name gen < "$TMP/explicit"
check "explicit name passes through" "zeroship_auth_test_byhand" "$name"
check "explicit name is not run-owned" "0" "$gen"

echo
echo "=== an unset TEST_DB yields a run-owned, prefixed, legal identifier ==="
( unset TEST_DB ZS_SCRATCH_DB_GENERATED SKIP_DB_RECREATE
  zs_scratch_db_resolve zeroship_auth_test
  printf '%s %s\n' "$TEST_DB" "$ZS_SCRATCH_DB_GENERATED" ) > "$TMP/gen"
read -r gname ggen < "$TMP/gen"
check "generated name is run-owned" "1" "$ggen"
case "$gname" in
  zeroship_auth_test_*) ok "generated name carries the prefix ($gname)" ;;
  *) bad "generated name lost the prefix: $gname" ;;
esac
# Postgres truncates an identifier past 63 bytes, which would turn two distinct
# per-run names into one shared name - the exact collision this library removes.
if [ "${#gname}" -le 63 ]; then
  ok "generated name fits Postgres's 63-byte identifier limit (${#gname})"
else
  bad "generated name is ${#gname} bytes; Postgres would truncate it to 63"
fi
if printf '%s' "$gname" | grep -qE '^[a-z][a-z0-9_]*$'; then
  ok "generated name needs no quoting"
else
  bad "generated name is not a bare identifier: $gname"
fi

echo
echo "=== uniqueness, which is the whole claim ==="
# Two separate PROCESSES is the case that matters: that is what two concurrent
# suite runs are. Distinct pids alone would satisfy it, so the same-process case
# below is the one that proves the clock token is doing work too.
a="$( unset TEST_DB SKIP_DB_RECREATE; zs_scratch_db_resolve p; printf '%s' "$TEST_DB" )"
b="$( unset TEST_DB SKIP_DB_RECREATE; zs_scratch_db_resolve p; printf '%s' "$TEST_DB" )"
if [ "$a" != "$b" ]; then ok "two processes get different names"; else bad "two processes collided on $a"; fi
c="$( unset TEST_DB SKIP_DB_RECREATE
      zs_scratch_db_resolve p; first="$TEST_DB"
      unset TEST_DB; zs_scratch_db_resolve p
      [ "$first" != "$TEST_DB" ] && printf 'differ' || printf 'same' )"
check "two calls in ONE process still differ (pid alone is not enough)" "differ" "$c"

echo
echo "=== SKIP_DB_RECREATE has nothing to reuse unless the caller names it ==="
( unset TEST_DB; SKIP_DB_RECREATE=1; zs_scratch_db_resolve zeroship_auth_test ) >"$TMP/skip.out" 2>"$TMP/skip.err"
check "refuses SKIP_DB_RECREATE with no TEST_DB" "2" "$?"
if grep -q 'FATAL: SKIP_DB_RECREATE needs an explicit TEST_DB' "$TMP/skip.err"; then
  ok "refusal names the variable the caller must set"
else
  bad "refusal did not name TEST_DB: $(cat "$TMP/skip.err")"
fi
# One variable changed from the case above.
( TEST_DB=zeroship_auth_test_reuse SKIP_DB_RECREATE=1
  zs_scratch_db_resolve zeroship_auth_test
  printf '%s %s\n' "$TEST_DB" "$ZS_SCRATCH_DB_GENERATED" ) > "$TMP/reuse" 2>/dev/null
reuse_rc=$?
read -r rname rgen < "$TMP/reuse"
check "control: SKIP_DB_RECREATE WITH a named TEST_DB is accepted" "0" "$reuse_rc"
check "control: the named database is reused" "zeroship_auth_test_reuse" "$rname"
check "control: and is not run-owned" "0" "$rgen"

echo
echo "=== cleanup drops what the run created, and only that ==="
# A fake run_psql that records its SQL. This asserts the statement the trap
# issues; a check that "the database is gone" would pass just as well if the
# database had never been created.
run_psql() { printf '%s\n' "$*" >> "$TMP/psql.log"; }

: > "$TMP/psql.log"
( ZS_SCRATCH_DB_GENERATED=1 TEST_DB=zeroship_auth_test_9_1 zs_scratch_db_cleanup )
if grep -q 'DROP DATABASE IF EXISTS zeroship_auth_test_9_1 WITH (FORCE)' "$TMP/psql.log"; then
  ok "run-owned database is dropped WITH (FORCE)"
else
  bad "no forced drop issued for a run-owned database: $(cat "$TMP/psql.log")"
fi

: > "$TMP/psql.log"
( ZS_SCRATCH_DB_GENERATED=0 TEST_DB=zeroship_auth_test_byhand zs_scratch_db_cleanup )
if [ ! -s "$TMP/psql.log" ]; then
  ok "a caller-named database is left alone"
else
  bad "cleanup touched a caller-named database: $(cat "$TMP/psql.log")"
fi

echo
echo "=== cleanup must not rewrite the run's exit status ==="
# A trap whose last command fails takes the script's exit status with it, so a
# green suite would exit non-zero the day the drop failed. Both arms checked:
( ZS_SCRATCH_DB_GENERATED=1 TEST_DB=x zs_scratch_db_cleanup ) >/dev/null 2>&1
check "cleanup returns 0 after a successful drop" "0" "$?"
( run_psql() { return 1; }
  ZS_SCRATCH_DB_GENERATED=1 TEST_DB=x zs_scratch_db_cleanup ) >/dev/null 2>&1
check "cleanup returns 0 even when the drop FAILS" "0" "$?"
( unset -f run_psql
  ZS_SCRATCH_DB_GENERATED=1 TEST_DB=x zs_scratch_db_cleanup ) >/dev/null 2>&1
check "cleanup returns 0 when no run_psql exists" "0" "$?"

echo
echo "=== the case that matters: a FAILING run still drops its database ==="
# Run a real subshell script with the real trap installed. "drops on success"
# and "drops on every exit" are indistinguishable on a green run, which is why
# this case exits non-zero - and why it also asserts the status survives.
cat > "$TMP/failing_run.sh" <<EOF
set -euo pipefail
. "$LIB"
run_psql() { printf '%s\n' "\$*" >> "$TMP/psql.log"; }
zs_scratch_db_resolve zeroship_auth_test
trap 'zs_scratch_db_cleanup' EXIT
printf '%s\n' "\$TEST_DB" > "$TMP/failing_name"
false   # the suite dies here, mid-run, exactly as a red suite does
echo "UNREACHABLE"
EOF
: > "$TMP/psql.log"
bash "$TMP/failing_run.sh" >/dev/null 2>&1
check "the failing run keeps its non-zero status" "1" "$?"
failed_name="$(cat "$TMP/failing_name")"
if grep -qF "DROP DATABASE IF EXISTS ${failed_name} WITH (FORCE)" "$TMP/psql.log"; then
  ok "a run that died mid-suite still dropped ${failed_name}"
else
  bad "a failing run leaked ${failed_name}: $(cat "$TMP/psql.log")"
fi

echo
echo "=================================================================="
echo "scratch db selftest: ${pass} passed, ${fail} failed"
[ "$fail" -eq 0 ] || exit 1
