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
# WHERE THE CREATE/ENSURE CASES WENT, and why they are not here any more.
#
# They used to inject a FAKE `run_psql` shell function that recorded the SQL it
# was handed and was scripted to answer - the only way to reach the
# lost-create-race arm, which needs two runs interleaved at a point no test can
# schedule. That worked because the library was shell, running in THIS process.
# The logic is a separate binary now (crates/zeroship-testkit), so:
#
#   - a shell function defined here is unreachable from it, and exporting the
#     function would not help: the fake closed over this script's non-exported
#     $TMP, which no child inherits;
#   - worse, run unchanged the cases would reach the REAL server the overlay
#     names. MEASURED 2026-08-20 during the port: the unmodified selftest
#     created `zeroship_auth_test_abc` on the shared cluster at :5440 and then
#     reported 36/43 - with several of the 36 passing only because a real
#     database happened to answer the way the fake was meant to. A vacuous green
#     is worse than a red.
#
# So they moved, and split by what they can prove:
#
#   the DECISIONS (reuse / create-once / lost race / genuine failure /
#   could-not-tell) -> `cargo test -p zeroship-testkit`, suite_db::tests, over a
#   scripted DbAdmin - the same technique, expressed as data instead of as a
#   shell function;
#
#   the DRIVER AND THE SERVER (create really works outside a transaction, a
#   duplicate create really returns 42P04, the provisioning lock really
#   serializes two PROCESSES) -> crates/zeroship-testkit/tests/live_suite_db.rs,
#   against a real PostgreSQL, with the race FORCED by a decorator rather than
#   waited for.
#
# What is left here is what only a shell test can answer: that the shim wires
# its arguments through and hands back the exit codes the suites switch on.
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
echo "=== nothing in the suite-database path can issue a DROP ==="
# The whole point of the schema-keyed name. A drop here would destroy a peer's
# run mid-suite and would destroy the failed-run data a caller kept
# deliberately, so the property is STRUCTURAL rather than conditional: the
# DbAdmin trait has exactly `exists` and `create`, and there is no arm to reach.
#
# Two instruments, because they answer different questions. First, the SHAPE:
# a trait with a third method is a drop waiting for a caller.
methods="$(awk '/^pub trait DbAdmin/,/^\}/' \
  "$ROOT/crates/zeroship-testkit/src/admin.rs" \
  | sed -n 's/^ *fn \([a-z_]*\).*/\1/p' | tr '\n' ' ')"
check "the server trait offers exactly these operations" "exists create " "$methods"

# Second, the STATEMENT. The pattern is anchored on the opening quote of a Rust
# string literal, so it matches SQL this code would send and not the same words
# in a comment or in a test's hostile-input fixture - both of which exist in
# these files and are meant to.
if grep -rn '"DROP DATABASE' "$ROOT/crates/zeroship-testkit/src/" >"$TMP/drops" 2>&1; then
  bad "the suite-database path can issue a DROP: $(cat "$TMP/drops")"
else
  ok "no DROP statement anywhere under crates/zeroship-testkit/src"
fi
# THE POSITIVE CONTROL, one variable changed: a file that DOES issue one. Without
# it, a grep that matched nothing and a grep whose pattern was broken read the
# same - and this pattern is deliberately narrow, which is exactly the way a
# pattern goes silently blind. `live_suite_db.rs` drops its own scratch database
# WITH (FORCE), correct there and wrong in the sweeper: see that file and
# tests/lib/sweep_db.sh for whose connections FORCE terminates.
if grep -q '"DROP DATABASE IF EXISTS {name} WITH (FORCE)"' \
     "$ROOT/crates/zeroship-testkit/tests/live_suite_db.rs"; then
  ok "control: the same pattern finds the scratch drop it is meant to find"
else
  bad "control: the pattern found nothing even where a DROP exists"
fi

echo
echo "=== check_identifier accepts what the resolver produces ==="
# The control for the refusal loop above: a checker that refused everything
# would pass every case there and take the suites down on their real name.
zs_suite_db_check_identifier zeroship_auth_test_a1eec1e1c30f >/dev/null 2>&1
check "a derived name is accepted" "0" "$?"
sixtythree="$(printf 'a%.0s' $(seq 1 63))"
zs_suite_db_check_identifier "$sixtythree" >/dev/null 2>&1
check "63 bytes is accepted, and 64 was refused above" "0" "$?"

echo
echo "=== a helper called before resolve refuses instead of guessing ==="
# `zs_suite_db_ensure` with no TEST_DB has no database to reason about. Guessing
# one would create something nobody asked for, on a shared cluster.
( unset TEST_DB; zs_suite_db_ensure ) >/dev/null 2>"$TMP/noname.err"
check "ensure before resolve fails" "1" "$?"
if grep -q 'zs_suite_db_ensure before zs_suite_db_resolve' "$TMP/noname.err"; then
  ok "and says which call was missing"
else
  bad "unexpected message: $(cat "$TMP/noname.err")"
fi
( unset TEST_DB; zs_suite_db_provision true ) >/dev/null 2>&1
check "provision before resolve fails" "1" "$?"

echo
echo "=== every function survives 'set -e', which is how both suites run it ==="
# The case this file did NOT have, and the bug it did not catch. Both suite
# scripts open with `set -euo pipefail`, and a helper that returns non-zero as
# an ORDINARY answer takes the whole harness down on the line that calls it
# unless the helper itself is written for it.
#
# MEASURED before the original fix, on a real 5444 database that did not exist
# yet: the harness printed the resolved name and then stopped. No error, no
# output, exit 1. It reads exactly like a database call that hung, which is why
# the selftest passing at the time was not evidence of anything - it runs under
# `set -uo pipefail` with no `-e`, so it exercised the one mode the suites never
# use.
cat > "$TMP/under_set_e.sh" <<EOF
set -euo pipefail
. "$LIB"
zs_suite_db_resolve zeroship_auth_test "" "$TMP/a"
zs_suite_db_check_identifier "\$TEST_DB"
if zs_sweep_never_defined 2>/dev/null; then :; fi
echo "REACHED THE END with \$TEST_DB"
EOF
out="$(bash "$TMP/under_set_e.sh" 2>&1)"
rc=$?
check "a resolve-and-check run under set -e finishes" "0" "$rc"
if printf '%s' "$out" | grep -q "REACHED THE END with zeroship_auth_test_${fa}"; then
  ok "and reaches the line after, with TEST_DB set"
else
  bad "it died inside the library: $out"
fi
echo
echo "=================================================================="
echo "suite db selftest: ${pass} passed, ${fail} failed"
[ "$fail" -eq 0 ] || exit 1
