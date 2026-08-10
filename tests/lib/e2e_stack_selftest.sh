#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Self-test for the run-accounting helpers in tests/lib/e2e_stack.sh.
#
# These need no services and no database: they exercise the shipped functions
# directly by sourcing the real file, so a green here is about the code that
# actually runs, not a re-implementation of it beside it.
#
# What it pins (task #256): a run carrying SKIPPED checks must not exit 0. The
# old verdict line was `[ $FAIL -eq 0 ] && exit 0 || exit 1`, and the only
# counter a missing prerequisite touched was KNOWN, which does not feed FAIL at
# the default STRICT=0. So a fresh checkout with no built `dist/*.zship` fired
# every SKIP arm and exited green having exercised nothing.
#
# Each case is run against BOTH verdicts on identical inputs, so the two
# columns show where the behaviour actually changed rather than asserting it.
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/tests/lib/e2e_stack.sh"

T=0; BAD=0
check() { # check <label> <expected> <actual>
  T=$((T+1))
  if [ "$2" = "$3" ]; then
    echo "  ok   $1 (exit $3)"
  else
    echo "  FAIL $1: expected exit $2, got $3"
    BAD=$((BAD+1))
  fi
}

# The verdict line as it stood before this change, for side-by-side comparison.
old_verdict() { [ "${FAIL:-0}" -eq 0 ] && return 0 || return 1; }

run_case() { # run_case <p> <f> <k> <s> <allow_skip>
  PASS=$1 FAIL=$2 KNOWN=$3 SKIPPED=$4 ALLOW_SKIP=$5
  e2e_verdict >/dev/null 2>&1; NEW=$?
  old_verdict; OLD=$?
}

echo "=== e2e_stack accounting self-test ==="

# 1. Clean run: nothing failed, nothing skipped. Both verdicts agree on green.
run_case 5 0 0 0 0
check "clean run is green (new)" 0 "$NEW"
check "clean run is green (old, unchanged)" 0 "$OLD"

# 2. THE FIX, and the one case where the two verdicts disagree: a check that
#    never ran. One variable moved from case 1 -- SKIPPED 0 -> 1.
run_case 5 0 0 1 0
check "a skipped check fails the run (new)" 1 "$NEW"
check "a skipped check passed the run (old) -- the hollow gate" 0 "$OLD"

# 3. Opt-out is available but must be typed. One variable from case 2.
run_case 5 0 0 1 1
check "ALLOW_SKIP=1 accepts the gap (new)" 0 "$NEW"

# 4. A real failure still fails, with nothing skipped, so case 2's red cannot
#    be mistaken for this one. Both verdicts agree.
run_case 5 1 0 0 0
check "a real failure fails (new)" 1 "$NEW"
check "a real failure fails (old, unchanged)" 1 "$OLD"

# 5. KNOWN alone stays non-failing at STRICT=0. This is deliberate and
#    unchanged -- a known defect is evidence, an unrun check is not.
run_case 5 0 3 0 0
check "known-fails alone stay green (unchanged)" 0 "$NEW"

# 6. e2e_skipped increments the counter it claims to.
SKIPPED=0
e2e_skipped "probe" >/dev/null
check "e2e_skipped increments SKIPPED" 1 "$SKIPPED"

echo ""
echo "  $T checks, $BAD failed"
[ "$BAD" -eq 0 ] || exit 1
