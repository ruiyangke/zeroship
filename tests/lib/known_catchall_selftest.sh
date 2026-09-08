#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# A catch-all `else known "..."` arm cannot fail, so a harness carrying one
# reports exit 0 over a totally broken platform.
#
# THE SHAPE, from tests/e2e_app_primitives.sh before this landed:
#
#     if   [ "$CODE" = "200" ] && ...; then pass "..."
#     elif [ "$CODE" = "401" ];        then known "documented fail-closed gate"
#     else                                  known "RPC via gateway: HTTP $CODE"
#     fi
#
# The middle arm is legitimate: a specific, named, expected outcome. The LAST
# arm is not. It catches 500, 502, 404, a connection refused that leaves $CODE
# empty - every way the platform can be broken - and records all of them as a
# warning. `known()` only increments FAIL under STRICT=1, which no gate script
# sets, and the harness exits on FAIL alone:
#
#     known() { KNOWN=$((KNOWN+1)); ...; if [ "$STRICT" = "1" ]; then FAIL=...; fi; }
#     [ $FAIL -eq 0 ] && exit 0 || exit 1
#
# So the harness announces the breakage and passes anyway. That is the same
# defect class as a Rust test that returns early because its backend is absent
# and still counts as passed, arriving by a different route. The Rust half of
# that family is closed: those tests now FAIL, naming what was missing, and the
# census that used to count their announcements is deleted. This one is not - a
# catch-all recording known() still passes on a real 500 - which is why this
# selftest pins the exit rule rather than trusting the announcement.
#
# WHAT THIS SELFTEST PINS, and what it does not. It drives the REAL pass/fail/
# known definitions and the REAL exit rule, so it proves the SEMANTICS: an
# unexpected outcome must move the exit code. It does NOT drive a live stack,
# so it cannot prove that any particular harness reaches its catch-all arm on a
# real 500 - that needs the four-service run. The per-arm behavioural red is
# therefore NOT established here and must not be read as established.
#
# The counterpart selftest for the skip-counted-as-pass shape is
# tests/lib/e2e_stack_selftest.sh.
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

checks=0
failed=0

check() { # <label> <expected> <actual>
  checks=$((checks + 1))
  if [ "$2" = "$3" ]; then
    echo "  ok   $1 (= $3)"
  else
    failed=$((failed + 1))
    echo "  FAIL $1: expected $2, got $3" >&2
  fi
}

# The real counters and the real exit rule, lifted verbatim in shape from
# tests/e2e_app_primitives.sh:82-88 and :375. Kept here rather than sourced
# because the harness runs a whole platform at import time.
arm() { # <http-code> <catchall-verdict: known|fail> -> echoes the exit code
  (
    PASS=0; FAIL=0; KNOWN=0; STRICT="${STRICT:-0}"
    pass()  { PASS=$((PASS+1)); }
    fail()  { FAIL=$((FAIL+1)); }
    known() { KNOWN=$((KNOWN+1)); if [ "$STRICT" = "1" ]; then FAIL=$((FAIL+1)); fi; }

    CODE="$1"
    if   [ "$CODE" = "200" ]; then pass  "served"
    elif [ "$CODE" = "401" ]; then known "documented fail-closed gate"
    else                           "$2"  "unexpected HTTP $CODE"
    fi

    [ $FAIL -eq 0 ] && echo 0 || echo 1
  )
}

echo "=== the defect: a catch-all that records known ==="
check "200 exits 0"                      0 "$(arm 200 known)"
check "401 (documented) exits 0"         0 "$(arm 401 known)"
check "500 exits 0 -- THE DEFECT"        0 "$(arm 500 known)"
check "empty code exits 0 -- THE DEFECT" 0 "$(arm ''  known)"

echo "=== the fix: the catch-all records fail, the documented arm still known ==="
check "200 still exits 0"                0 "$(arm 200 fail)"
check "401 still exits 0 (not swept up)" 0 "$(arm 401 fail)"
check "500 now exits 1"                  1 "$(arm 500 fail)"
check "empty code now exits 1"           1 "$(arm ''  fail)"

# THE ONE-VARIABLE CONTROL. Without this pair, "500 now exits 1" is consistent
# with a fix that simply fails everything, which would be a different bug with
# the same green. The 401 rows above and this row differ in exactly one input
# and must land on opposite verdicts.
echo "=== STRICT=1 escalates known, which is the pre-existing opt-in ==="
check "401 under STRICT=1 exits 1" 1 "$(STRICT=1 arm 401 fail)"
check "200 under STRICT=1 exits 0" 0 "$(STRICT=1 arm 200 fail)"

echo ""
echo "known-catchall selftest: $checks checks, $failed failed"
[ "$failed" -eq 0 ] || exit 1
