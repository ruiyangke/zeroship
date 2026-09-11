#!/usr/bin/env bash
# Self-test for tests/lib/test_config.sh.
#
# The case that matters is the SPLIT BRAIN. A harness reads PG_HOST/PG_PORT for
# its own psql calls; every service it spawns reads the overlay. Point a suite
# at a second cluster and the two halves talk to two different servers, and
# nothing says so - the run completes and reports a plausible number computed
# against two databases. That is not a failure, it is a WRONG MEASUREMENT
# wearing a result's clothes, which is the one outcome no reader can catch.
#
# So it is checked in both directions, and the false-alarm direction is checked
# too: `localhost` and `127.0.0.1` are the same host and CI writes one while the
# generator writes the other. A gate that refuses every CI run is a gate
# somebody deletes.
#
# Run directly: tests/lib_test_config_selftest.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LIB="$ROOT/tests/lib/test_config.sh"
# shellcheck source=tests/lib/test_config.sh
. "$LIB"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail=0; pass=0
ok()  { pass=$((pass + 1)); echo "ok   - $1"; }
bad() { fail=$((fail + 1)); echo "FAIL - $1" >&2; }
check() { if [ "$2" = "$3" ]; then ok "$1 ($3)"; else bad "$1: expected '$2', got '$3'"; fi }

# A fake repo root carrying only the overlay the library reads.
mkdir -p "$TMP/root/deploy/ops"
cat > "$TMP/root/deploy/ops/zeroship.test.toml" <<'EOF'
[control]
database_url = "postgres://postgres:zeroship@127.0.0.1:5440/zeroship"

EOF

run_load() { # run_load <env assignments...>  -> prints exit code, stderr to $TMP/err
  ( for kv in "$@"; do export "${kv?}"; done
    zs_test_config_load "$TMP/root" ) >"$TMP/out" 2>"$TMP/err"
  printf '%s' "$?"
}

echo "=== with nothing asked for, the overlay is simply read ==="
rc="$(run_load PG_HOST= PG_PORT= PG_USER= PG_PASS=)"
check "an unconstrained load succeeds" "0" "$rc"
( unset PG_HOST PG_PORT PG_USER PG_PASS
  zs_test_config_load "$TMP/root" >/dev/null 2>&1
  printf '%s %s %s\n' "$PG_HOST" "$PG_PORT" "$PG_DB" ) > "$TMP/parsed"
read -r h p d < "$TMP/parsed"
check "host parsed from the DSN" "127.0.0.1" "$h"
check "port parsed from the DSN" "5440" "$p"
check "database parsed from the DSN" "zeroship" "$d"

echo
echo "=== a port the overlay does not name is REFUSED, not honoured ==="
rc="$(run_load PG_PORT=5444)"
check "a mismatched PG_PORT refuses" "1" "$rc"
if grep -q "you asked for '5444'" "$TMP/err"; then
  ok "the refusal names both values"
else
  bad "refusal did not name the values: $(cat "$TMP/err")"
fi
if grep -q "provision_test_backends.sh" "$TMP/err"; then
  ok "and names the command that fixes it"
else
  bad "refusal offered no fix"
fi

# CONTROL, one variable changed: the SAME call with the port the overlay names.
rc="$(run_load PG_PORT=5440)"
check "control: the port the overlay names is accepted" "0" "$rc"

echo
echo "=== the same rule for host, user and password ==="
rc="$(run_load PG_HOST=db.example.com)"; check "a mismatched PG_HOST refuses" "1" "$rc"
rc="$(run_load PG_USER=someone_else)";   check "a mismatched PG_USER refuses" "1" "$rc"
rc="$(run_load PG_PASS=not-the-one)";    check "a mismatched PG_PASS refuses" "1" "$rc"
if grep -q "values not shown" "$TMP/err"; then
  ok "and the password refusal prints neither value"
else
  bad "the password refusal may have printed a credential: $(cat "$TMP/err")"
fi

echo
echo "=== localhost and 127.0.0.1 are the same host, and must not refuse ==="
# CI sets PG_HOST=localhost; the generator writes 127.0.0.1. Refusing that would
# fail every CI run for a difference that is not one.
rc="$(run_load PG_HOST=localhost PG_PORT=5440)"
check "localhost against 127.0.0.1 is accepted" "0" "$rc"
rc="$(run_load PG_HOST=::1 PG_PORT=5440)"
check "::1 against 127.0.0.1 is accepted" "0" "$rc"

echo
echo "=== a missing overlay is refused, never defaulted ==="
mkdir -p "$TMP/bare"
( zs_test_config_load "$TMP/bare" ) >/dev/null 2>"$TMP/err"
check "no overlay refuses" "1" "$?"
if grep -q "no test overlay" "$TMP/err"; then
  ok "and says so"
else
  bad "unexpected message: $(cat "$TMP/err")"
fi

echo
echo "=================================================================="
echo "test config selftest: ${pass} passed, ${fail} failed"
[ "$fail" -eq 0 ] || exit 1
