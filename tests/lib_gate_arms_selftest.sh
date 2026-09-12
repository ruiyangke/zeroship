#!/usr/bin/env bash
# Self-test for tests/lib/gate_arms.sh.
#
# The library's whole claim is that an arm which ruled on nothing cannot report
# success. A detector that refused EVERYTHING would satisfy that claim and be
# useless, so every refusal below is paired with a control that differs in ONE
# variable and must stay green. Without the pair, a positive proves only that
# the code RAN, not that it DISCRIMINATES.
#
# Run directly: tests/lib_gate_arms_selftest.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LIB="$ROOT/tests/lib/gate_arms.sh"
[ -r "$LIB" ] || { echo "FAIL: cannot read $LIB; nothing was tested." >&2; exit 1; }

pass=0
fail=0

# expect <0|1> <label> <body...>
#
# Each case runs in its own subshell with a FRESH source of the library, so one
# case's accumulated refusals cannot leak into the next and make a green look
# red. Status is read directly from the subshell - never through a pipe, which
# would report the pipe's last command instead.
expect() {
  local want="$1" label="$2"
  shift 2
  local out actual
  out="$(
    # shellcheck source=tests/lib/gate_arms.sh
    . "$LIB"
    "$@" 2>&1
  )"
  actual=$?
  if [ "$actual" = "$want" ]; then
    pass=$((pass + 1))
    echo "ok   - $label (status=$actual)"
  else
    fail=$((fail + 1))
    echo "FAIL - $label: expected status=$want, got $actual" >&2
    printf '%s\n' "$out" | sed 's/^/       /' >&2
  fi
}

# --- the founding case: an arm that ruled on nothing ------------------------
#
# This is ws_subscription_stub arm 1, in miniature: the enumeration went to
# zero and the arm still had a verdict to report.
zero_examined() { gate_arms_init g; gate_arm a 0 3; gate_arms_finish; }
expect 1 "an arm that examined 0 against a floor of 3 refuses" zero_examined

# ONE-VARIABLE CONTROL: identical call, identical floor, count above it.
above_floor() { gate_arms_init g; gate_arm a 3 3; gate_arms_finish; }
expect 0 "the same arm at exactly its floor passes" above_floor

# The collapse case a plain zero-check misses: 200 items become 3. The count is
# non-zero, so "did it examine anything" says yes; only the floor sees it.
partial_collapse() { gate_arms_init g; gate_arm a 3 40; gate_arms_finish; }
expect 1 "a count that collapsed from far above the floor to 3 refuses" partial_collapse

# --- a refusal survives being ignored ---------------------------------------
#
# Gates are written with `set -uo pipefail`, not `-e`. If the verdict lived
# only in gate_arm's return value, `gate_arm a 0 3` on a line by itself would
# discard it and the gate would exit 0. The finish call has to carry it.
ignored_return() { gate_arms_init g; gate_arm a 0 3 || true; gate_arms_finish; }
expect 1 "a discarded gate_arm return value still fails the gate" ignored_return

# --- zero arms at all -------------------------------------------------------
#
# A gate whose arms are all skipped by a `case` that stopped matching runs to
# the end and exits 0. Indistinguishable from clean, unless someone counts.
no_arms() { gate_arms_init g; gate_arms_finish; }
expect 1 "a gate that declared no arms refuses" no_arms

one_arm() { gate_arms_init g; gate_arm a 1 1; gate_arms_finish; }
expect 0 "the same gate with one cleared arm passes" one_arm

# --- a floor of zero is a declared vacuity ----------------------------------
floor_zero() { gate_arms_init g; gate_arm a 0 0; gate_arms_finish; }
expect 1 "a floor of 0 is refused rather than honoured" floor_zero

floor_one() { gate_arms_init g; gate_arm a 1 1; gate_arms_finish; }
expect 0 "a floor of 1 with one item passes" floor_one

# --- a count that is not a count --------------------------------------------
#
# `examined=$(grep -c ... )` on a missing file yields an empty string, and
# treating that as zero would be a silent guess about which failure happened.
empty_count() { gate_arms_init g; gate_arm a "" 1; gate_arms_finish; }
expect 1 "an empty examined count refuses instead of reading as zero" empty_count

text_count() { gate_arms_init g; gate_arm a "no such file" 1; gate_arms_finish; }
expect 1 "a command's error text in place of a count refuses" text_count

numeric_count() { gate_arms_init g; gate_arm a "7" 1; gate_arms_finish; }
expect 0 "a well-formed count passes, so the two above are about the value" numeric_count

# --- one arm must not vouch for another -------------------------------------
#
# Arms are written by copy-paste. A duplicated id makes the census report two
# arms where one number was measured twice.
dup_arm() { gate_arms_init g; gate_arm a 5 1; gate_arm a 5 1; gate_arms_finish; }
expect 1 "the same arm id declared twice refuses" dup_arm

distinct_arms() { gate_arms_init g; gate_arm a 5 1; gate_arm b 5 1; gate_arms_finish; }
expect 0 "two distinct arm ids with the same counts pass" distinct_arms

emit_out="$(. "$LIB"; gate_arms_init widget; gate_arm citations 85 40; gate_arms_finish)"
if printf '%s\n' "$emit_out" | grep -qx 'zsgate-arm gate=widget arm=citations examined=85 floor=40'; then
  pass=$((pass + 1))
  echo "ok   - the per-arm census line has its documented shape"
else
  fail=$((fail + 1))
  echo "FAIL - the per-arm census line changed shape; the diagnostic protocol is no longer recognizable:" >&2
  printf '%s\n' "$emit_out" | sed 's/^/       /' >&2
fi

if printf '%s\n' "$emit_out" | grep -qx 'zsgate-arms gate=widget arms=1 refusals=0'; then
  pass=$((pass + 1))
  echo "ok   - the per-gate trailer line has its documented shape"
else
  fail=$((fail + 1))
  echo "FAIL - the per-gate trailer changed shape:" >&2
  printf '%s\n' "$emit_out" | sed 's/^/       /' >&2
fi

# A refusing arm must STILL emit its census line. The consumer's job is to see
# the collapse; a library that went quiet on the failing case would hide the
# very arm the operator needs named.
refuse_out="$(. "$LIB"; gate_arms_init widget; gate_arm citations 0 40; gate_arms_finish)"
if printf '%s\n' "$refuse_out" | grep -qx 'zsgate-arm gate=widget arm=citations examined=0 floor=40'; then
  pass=$((pass + 1))
  echo "ok   - a refusing arm still emits its census line"
else
  fail=$((fail + 1))
  echo "FAIL - a refusing arm emitted no census line; the collapse is invisible" >&2
fi

# --- anti-hollow guard for this file itself ---------------------------------
EXPECTED=16
total=$((pass + fail))
if [ "$total" -ne "$EXPECTED" ]; then
  echo "SELF-TEST DID NOT RUN: expected $EXPECTED assertions, ran $total." >&2
  exit 1
fi

echo "gate_arms selftest: $pass passed, $fail failed ($total assertions)"
[ "$fail" -eq 0 ]
