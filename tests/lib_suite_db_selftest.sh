#!/usr/bin/env bash
# Self-test for tests/lib/suite_db.sh.
#
# The library decides WHICH database two concurrent suite runs land in, so it is
# only worth anything if it is right in BOTH directions:
#
#   same migration set  MUST give the same name  - else nothing is ever shared
#                                                  and the population keeps
#                                                  growing with agent count
#   changed migrations  MUST give a DIFFERENT one - else a branch that edits the
#                                                  schema runs its tests against
#                                                  the previous branch's schema,
#                                                  and the failures read as
#                                                  product defects
#
# Both halves matter equally and only the pair distinguishes a real fingerprint
# from a constant: a function returning "abc" forever passes the first check.
#
# The create/ensure cases run against a FAKE run_psql that records its SQL and
# is scripted to answer, so they assert what the library ISSUES rather than that
# some database exists. That is the only way to exercise the lost-create-race
# arm at all - it needs two runs interleaved at a point no test can schedule.
#
# Run directly: tests/lib_suite_db_selftest.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LIB="$ROOT/tests/lib/suite_db.sh"
# shellcheck source=tests/lib/suite_db.sh
. "$LIB"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail=0
pass=0
ok()  { pass=$((pass + 1)); echo "ok   - $1"; }
bad() { fail=$((fail + 1)); echo "FAIL - $1" >&2; }
check() { # check <label> <expected> <actual>
  if [ "$2" = "$3" ]; then ok "$1 ($3)"; else bad "$1: expected '$2', got '$3'"; fi
}

# A throwaway tree that looks like a repo root to the library: only
# db/migrations-ts/*.ts is read.
mk_tree() { # mk_tree <dir> <file>...
  local dir="$1"; shift
  mkdir -p "$dir/db/migrations-ts"
  local f
  for f in "$@"; do printf 'export const up = "%s";\n' "$f" > "$dir/db/migrations-ts/$f"; done
}

echo "=== the fingerprint is a function of the migration set, and only of it ==="
mk_tree "$TMP/a" 20260101_one.ts 20260202_two.ts
mk_tree "$TMP/b" 20260101_one.ts 20260202_two.ts
fa="$(zs_schema_fingerprint "$TMP/a")"
fb="$(zs_schema_fingerprint "$TMP/b")"
check "two trees with identical migrations agree" "$fa" "$fb"
# The path differs between those two trees, so this also pins that the absolute
# path stays OUT of the digest - two worktrees of one commit must agree or no
# two agents ever share a database.
if [ -n "$fa" ]; then ok "fingerprint is non-empty ($fa)"; else bad "fingerprint was empty"; fi
if printf '%s' "$fa" | grep -qE '^[0-9a-f]{12}$'; then
  ok "fingerprint is 12 hex characters"
else
  bad "fingerprint is not 12 hex characters: '$fa'"
fi

echo
echo "=== ...and it MOVES when the schema does. Three ways it can move. ==="
# 1. A file ADDED. This is the acceptance case: a branch that writes a migration
#    gets its own database without being told to.
mk_tree "$TMP/added" 20260101_one.ts 20260202_two.ts
printf 'export const up = "three";\n' > "$TMP/added/db/migrations-ts/20260303_three.ts"
f_added="$(zs_schema_fingerprint "$TMP/added")"
if [ "$f_added" != "$fa" ]; then ok "an added migration changes the fingerprint"; else bad "an added migration did not change it"; fi

# 2. A file's CONTENT edited, with the file list unchanged. The platform journal
#    keys on a sha256 of each file's bytes, so an in-place edit against the old
#    database fails every later run with ChecksumMismatch; it has to land in a
#    fresh one.
mk_tree "$TMP/edited" 20260101_one.ts 20260202_two.ts
printf 'export const up = "one, but different";\n' > "$TMP/edited/db/migrations-ts/20260101_one.ts"
f_edited="$(zs_schema_fingerprint "$TMP/edited")"
if [ "$f_edited" != "$fa" ]; then ok "an edited migration changes the fingerprint"; else bad "an edited migration did not change it"; fi

# 3. A file RENAMED with its bytes untouched. The runner orders by filename and
#    journals under it, so this is a different schema even though every byte is
#    accounted for. A digest over contents alone would miss it.
mk_tree "$TMP/renamed" 20260101_one.ts
printf 'export const up = "20260202_two.ts";\n' > "$TMP/renamed/db/migrations-ts/20269999_two.ts"
f_renamed="$(zs_schema_fingerprint "$TMP/renamed")"
if [ "$f_renamed" != "$fa" ]; then ok "a renamed migration changes the fingerprint"; else bad "a rename with identical bytes did not change it"; fi

echo
echo "=== an absent or empty migration set is refused, never hashed ==="
# A fingerprint of nothing is a FIXED value, so every broken checkout would
# share one database and call it schema-fresh.
mkdir -p "$TMP/empty/db/migrations-ts"
( zs_schema_fingerprint "$TMP/empty" ) >"$TMP/empty.out" 2>"$TMP/empty.err"
check "an empty migrations dir is refused" "2" "$?"
if grep -q 'no \*\.ts migrations' "$TMP/empty.err"; then ok "the refusal says what is missing"; else bad "refusal text: $(cat "$TMP/empty.err")"; fi
if [ ! -s "$TMP/empty.out" ]; then ok "and prints no fingerprint"; else bad "it printed '$(cat "$TMP/empty.out")'"; fi
mkdir -p "$TMP/nodir"
( zs_schema_fingerprint "$TMP/nodir" ) >/dev/null 2>"$TMP/nodir.err"
check "a missing migrations dir is refused" "2" "$?"

echo
echo "=== resolve: the name is the prefix plus the fingerprint ==="
( unset TEST_DB SKIP_DB_RECREATE
  zs_suite_db_resolve zeroship_auth_test "" "$TMP/a"
  printf '%s %s\n' "$TEST_DB" "$ZS_SUITE_DB_EXPLICIT" ) > "$TMP/res"
read -r rname rexp < "$TMP/res"
check "derived name carries prefix and fingerprint" "zeroship_auth_test_${fa}" "$rname"
check "derived name is not caller-owned" "0" "$rexp"
if [ "${#rname}" -le 63 ]; then ok "derived name fits Postgres's 63-byte limit (${#rname})"; else bad "derived name is ${#rname} bytes"; fi

echo
echo "=== the override is a FLAG, and an ambient variable is REFUSED ==="
# Refused rather than honoured: an environment variable that silently redirects
# a gate is how gates get silently disabled. Refused rather than IGNORED: a
# caller who set it deliberately must be told their run went elsewhere.
( TEST_DB=zeroship_auth_test_byhand
  zs_suite_db_resolve zeroship_auth_test "" "$TMP/a" ) >/dev/null 2>"$TMP/amb.err"
check "an ambient TEST_DB is refused" "2" "$?"
if grep -q -- '--database' "$TMP/amb.err"; then ok "the refusal names the flag to use instead"; else bad "refusal text: $(cat "$TMP/amb.err")"; fi
( SKIP_DB_RECREATE=1
  zs_suite_db_resolve zeroship_auth_test "" "$TMP/a" ) >/dev/null 2>"$TMP/skip.err"
check "an ambient SKIP_DB_RECREATE is refused" "2" "$?"

# One variable changed from the refusal above: the same name, passed as the
# explicit argument instead of through the environment.
( unset TEST_DB SKIP_DB_RECREATE
  zs_suite_db_resolve zeroship_auth_test zeroship_auth_test_byhand "$TMP/a"
  printf '%s %s\n' "$TEST_DB" "$ZS_SUITE_DB_EXPLICIT" ) > "$TMP/expl" 2>"$TMP/expl.err"
expl_rc=$?
read -r ename eexp < "$TMP/expl"
check "control: the same name passed as the flag argument is accepted" "0" "$expl_rc"
check "control: and is used verbatim" "zeroship_auth_test_byhand" "$ename"
check "control: and is marked caller-owned" "1" "$eexp"

echo
echo "=== an override that is not a bare identifier is refused ==="
# The name reaches CREATE DATABASE unquoted; it is the only caller-controlled
# string in that statement.
for evil in 'foo; DROP DATABASE zeroship' 'Foo' 'foo-bar' '1foo' '"foo"' ''"'"'x'"'"''; do
  ( unset TEST_DB SKIP_DB_RECREATE
    zs_suite_db_resolve p "$evil" "$TMP/a" ) >/dev/null 2>&1
  if [ "$?" -eq 2 ]; then ok "refused: '$evil'"; else bad "ACCEPTED an illegal identifier: '$evil'"; fi
done
long="$(printf 'a%.0s' $(seq 1 64))"
( unset TEST_DB SKIP_DB_RECREATE; zs_suite_db_resolve p "$long" "$TMP/a" ) >/dev/null 2>&1
check "refused: a 64-byte name Postgres would truncate" "2" "$?"

echo
echo "=== ensure: an existing database is REUSED, never dropped ==="
# The whole point. A drop here would destroy a peer's run mid-suite, and would
# destroy the failed-run data a caller kept deliberately.
: > "$TMP/psql.log"
run_psql() {
  printf '%s\n' "$*" >> "$TMP/psql.log"
  case "$*" in *"FROM pg_database"*) printf '1\n' ;; esac
}
( TEST_DB=zeroship_auth_test_abc zs_suite_db_ensure ) >/dev/null 2>&1
check "ensure succeeds against an existing database" "0" "$?"
if grep -qi 'DROP DATABASE' "$TMP/psql.log"; then
  bad "ensure issued a DROP: $(grep -i 'DROP DATABASE' "$TMP/psql.log")"
else
  ok "ensure issued no DROP"
fi
if grep -qi 'CREATE DATABASE' "$TMP/psql.log"; then
  bad "ensure re-created an existing database"
else
  ok "ensure re-created nothing"
fi

echo
echo "=== ensure: an absent database is created exactly once ==="
: > "$TMP/psql.log"
run_psql() {
  printf '%s\n' "$*" >> "$TMP/psql.log"
  case "$*" in *"FROM pg_database"*) : ;; esac   # prints nothing: absent
}
( TEST_DB=zeroship_auth_test_abc zs_suite_db_ensure ) >/dev/null 2>&1
check "ensure succeeds when it has to create" "0" "$?"
if grep -qF 'CREATE DATABASE zeroship_auth_test_abc;' "$TMP/psql.log"; then
  ok "ensure issued the CREATE"
else
  bad "no CREATE issued: $(cat "$TMP/psql.log")"
fi
if grep -qi 'DROP DATABASE' "$TMP/psql.log"; then bad "ensure issued a DROP"; else ok "and still no DROP"; fi

echo
echo "=== ensure: LOSING the create race is success, not failure ==="
# Two agents starting together both find the database absent and both issue
# CREATE. The loser must proceed. This is scripted because no test can schedule
# the interleaving, and it is asserted through a second pg_database probe rather
# than by matching the error text - "it exists now" is the condition that
# actually makes it safe to continue.
: > "$TMP/psql.log"
: > "$TMP/probes"
run_psql() {
  printf '%s\n' "$*" >> "$TMP/psql.log"
  case "$*" in
    *"FROM pg_database"*)
      # The counter lives in a FILE, not a variable: the library reads this
      # function's stdout through `$(...)`, so every probe runs in its own
      # subshell and a shell variable would be back at its old value each time.
      printf 'x' >> "$TMP/probes"
      # First probe: absent. Second (after the failed CREATE): a peer made it.
      [ "$(wc -c < "$TMP/probes")" -ge 2 ] && printf '1\n'
      return 0 ;;
    *"CREATE DATABASE"*)
      echo 'ERROR:  database "zeroship_auth_test_abc" already exists' >&2
      return 1 ;;
  esac
}
( TEST_DB=zeroship_auth_test_abc zs_suite_db_ensure ) >"$TMP/race.out" 2>&1
check "a lost create race exits 0" "0" "$?"
if grep -q 'appeared concurrently' "$TMP/race.out"; then
  ok "and says the peer won rather than reporting an error"
else
  bad "output did not explain the race: $(cat "$TMP/race.out")"
fi

echo
echo "=== ensure: a create that fails for ANY OTHER reason still fails ==="
# One variable changed from the case above: the database is still absent on the
# second probe. Without this the arm above would swallow every create failure.
: > "$TMP/psql.log"
run_psql() {
  printf '%s\n' "$*" >> "$TMP/psql.log"
  case "$*" in
    *"FROM pg_database"*) return 0 ;;                       # absent, both times
    *"CREATE DATABASE"*) echo 'ERROR:  permission denied to create database' >&2; return 1 ;;
  esac
}
( TEST_DB=zeroship_auth_test_abc zs_suite_db_ensure ) >"$TMP/denied.out" 2>&1
check "a genuine create failure exits 2" "2" "$?"
if grep -q 'permission denied to create database' "$TMP/denied.out"; then
  ok "and re-emits the server's own reason"
else
  bad "the server's error was swallowed: $(cat "$TMP/denied.out")"
fi

echo
echo "=== ensure: 'cannot tell' is not 'absent' ==="
# psql exiting non-zero prints nothing, exactly like an empty result set. Read
# as absent, an unreachable server becomes a CREATE DATABASE failing for reasons
# nobody can name.
run_psql() { return 2; }
( TEST_DB=zeroship_auth_test_abc zs_suite_db_ensure ) >"$TMP/unreach.out" 2>&1
check "an unreachable server exits 2" "2" "$?"
if grep -q 'could not ask the server' "$TMP/unreach.out"; then
  ok "and says it could not tell, rather than 'absent'"
else
  bad "output: $(cat "$TMP/unreach.out")"
fi

echo
echo "=== ensure without run_psql refuses instead of silently doing nothing ==="
( unset -f run_psql
  TEST_DB=zeroship_auth_test_abc zs_suite_db_ensure ) >/dev/null 2>&1
check "missing run_psql exits 2" "2" "$?"

echo
echo "=================================================================="
echo "suite db selftest: ${pass} passed, ${fail} failed"
[ "$fail" -eq 0 ] || exit 1
