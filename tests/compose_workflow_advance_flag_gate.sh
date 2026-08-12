#!/usr/bin/env bash
# ============================================================================
# `deploy/` must not pass --workflow-advance-unsigned while the gateway's
# internal workflow-advance edge is still unauthenticated.
#
# WHY THIS EXISTS, found 2026-08-12 by running
# tests/e2e_gateway_workflow_advance_authz.sh end to end. Two facts were on
# record in two different places, tracked as two different problems, and they
# are one fact:
#
#   PRODUCT  (docs/pilot spine, "the blocker that has no creator workaround"):
#            deploy/ passes --workflow-advance-unsigned nowhere and the worker
#            403s without it, so a deployment as shipped in deploy/compose
#            cannot advance a workflow at all.
#
#   SECURITY (tasks #199 / #300 / #338): the gateway's
#            /__zeroship/internal/workflow-advance edge has no authorization
#            beyond a Host check, and takes app_id from the caller's JSON body,
#            so the caller selects WHICH APP's workflow to advance.
#
# Measured, both arms of the same switch, in one run:
#
#   worker WITH the flag     -> HTTP 200 {"ack":true}, run queued -> completed,
#                               step-rows 0 -> 1   (the #199 gap, reproduced)
#   worker WITHOUT the flag  -> HTTP 403 {"error":"workflow advance unsigned
#                               disabled"}, run stayed queued
#
# So the 403 that makes durable workflows unusable in the shipped deployment is
# exactly what makes the #199 exploit unreachable in the shipped deployment.
# THE OBVIOUS ONE-LINE FIX FOR THE BLOCKER ARMS THE EXPLOIT IN THE SAME CHANGE.
# Nobody had written that down, because each side was recorded by someone
# looking at only one of the two. See task #354.
#
# This gate exists so that coupling cannot be crossed silently. It does not
# decide the fix - that is an operator call, and the recommendation on #354 is
# to build the signed-advance path the flag's own doc says "replaces it in a
# later durable-workflows task", which closes both at once.
#
# WHAT THIS DOES NOT CHECK, so a green is not over-read:
#   - it CANNOT tell whether the gateway edge has since gained authorization.
#     Detecting "this handler authenticates now" by grep is the kind of
#     spelling-for-behaviour substitution that has produced wrong answers in
#     this repo before. So if the edge is fixed, this gate must be retired or
#     rewritten BY A HUMAN, in the same commit as the fix. Arm A failing is a
#     prompt to think, not proof that the change is wrong.
#   - it does not run docker, does not boot a stack, and probes nothing. The
#     behavioural claims above come from e2e_gateway_workflow_advance_authz.sh,
#     which is where they stay measured.
#   - arm B matches an error-message STRING. If the backstop is reworded but
#     kept, arm B false-alarms; if it is deleted and the same string is written
#     somewhere inert, arm B false-passes. It is a tripwire on the thing the
#     probe asserts, not a proof the refusal still executes.
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

FLAG="workflow-advance-unsigned"
BACKSTOP_STRING="workflow advance unsigned disabled"
BACKSTOP_FILE="crates/worker/src/handler.rs"

pass=0
fail=0
ok()   { echo "  PASS $*"; pass=$((pass + 1)); }
bad()  { echo "  FAIL $*"; fail=$((fail + 1)); }

echo "=== arm A: deploy/ must not arm the unsigned advance path ==="
if [ ! -d deploy ]; then
  bad "deploy/ does not exist - this gate is pointed at a path that moved"
else
  # -r over the whole directory: the flag has no env binding today, so the CLI
  # spelling in a compose `command:` is the only known vector, but scanning the
  # whole tree costs nothing and catches a spelling nobody predicted.
  hits="$(grep -rn -- "$FLAG" deploy/ 2>/dev/null)"
  if [ -z "$hits" ]; then
    ok "no deploy/ file passes --$FLAG"
  else
    bad "deploy/ arms the unsigned workflow-advance path:"
    echo "$hits" | sed 's/^/       /'
    echo "       -> This ALSO makes the #199 gateway authz gap live on the"
    echo "          public listener. The two are the same flag (task #354)."
    echo "          If the gateway edge now authenticates, retire this gate in"
    echo "          the same commit and say so; it cannot detect that itself."
  fi
fi

echo "=== arm B: the worker backstop that makes arm A protective still exists ==="
# Arm A's absence only protects while the worker actually refuses. If the
# backstop is deleted, "deploy/ has no flag" stops meaning anything, and arm A
# would keep printing green over a live edge.
if [ ! -f "$BACKSTOP_FILE" ]; then
  bad "$BACKSTOP_FILE not found - the backstop cannot be checked"
elif grep -qF -- "$BACKSTOP_STRING" "$BACKSTOP_FILE"; then
  ok "worker still refuses unsigned advance (\"$BACKSTOP_STRING\")"
else
  bad "the worker's unsigned-advance refusal is GONE from $BACKSTOP_FILE"
  echo "       -> arm A above is now vacuous: with no backstop, the absence of"
  echo "          the flag in deploy/ protects nothing. Re-measure with"
  echo "          tests/e2e_gateway_workflow_advance_authz.sh before shipping."
fi

echo
echo "=== summary ==="
echo "  passed: $pass   failed: $fail"
# Anti-hollow guard: a run that asserted nothing must not read as success.
if [ "$((pass + fail))" -lt 2 ]; then
  echo "  FAIL harness ran fewer than its 2 arms - treating as failure"
  exit 1
fi
[ "$fail" -eq 0 ] || exit 1
echo "OK"
