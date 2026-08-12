#!/usr/bin/env bash
# Keep the WebSocket-subscription STUB and the things that describe it in sync.
#
# WHY THIS EXISTS. `crates/gateway/src/router/dispatch.rs` carried a comment
# saying the transparent WS proxy "is wired in `proxy::forward_subscription`".
# It is not. That function was in no file in this repo -- the `proxy` module is
# real, the function never existed -- and the comment sat 760 lines from
# `handle_subscription_dispatch`, which says plainly that the gateway does NOT
# proxy an upgraded connection and returns 501.
#
# That is the expensive shape: a comment that ANSWERS the auditor's question
# ("is WS proxying handled?") before anyone goes and checks. Nothing in the
# build could see it, because a `//` comment is not a doc comment, so neither
# rustdoc's intra-doc link gate nor the repo's citation gates look at it.
#
# WHAT THIS GATE DOES NOT DO: it does not decide whether the proxy should be
# built. It only makes the stub and its descriptions move together.

set -uo pipefail
cd "$(dirname "$0")/.."

PASS=0
FAIL=0
RAN=0
pass() { PASS=$((PASS + 1)); RAN=$((RAN + 1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL + 1)); RAN=$((RAN + 1)); echo "  FAIL $1"; }

DISPATCH="crates/gateway/src/router/dispatch.rs"
WSDOC="docs/reference/websocket-design.md"

for f in "$DISPATCH" "$WSDOC"; do
  [ -f "$f" ] || { echo "gate cannot run: $f is missing"; exit 1; }
done

echo "ws subscription stub gate"

# --- Arm 1: no phantom function ------------------------------------------
# The regression test for the comment that started this. If any file mentions
# `forward_subscription`, some file must DEFINE it. Before the 2026-08-12 fix
# this was 1 mention / 0 definitions and this arm is RED; after it, 0 / 0.
# It also stays green the honest way if the proxy is ever really built.
mentions=$(grep -rl "forward_subscription" --include="*.rs" crates/ 2>/dev/null | wc -l)
defs=$(grep -rl "fn forward_subscription" --include="*.rs" crates/ 2>/dev/null | wc -l)
if [ "$mentions" -eq 0 ] || [ "$defs" -ge 1 ]; then
  pass "forward_subscription: $mentions mention(s), $defs definition(s) -- no phantom"
else
  fail "forward_subscription is NAMED in $mentions file(s) but DEFINED in none.
       A comment or doc is telling readers the transparent WS proxy is wired
       somewhere it is not. Either build it, or say what actually happens --
       see handle_subscription_dispatch, which returns 501."
fi

# --- Arm 2: the stub and the creator-facing doc agree ---------------------
# `docs/reference/websocket-design.md` is what a creator reads before building
# on sockets. While the gateway answers subscriptions with 501, that doc has to
# say so. When the proxy lands and the stub goes, this arm flips and demands
# the doc stop saying it -- which is the point: the doc cannot drift either way.
stub=$(grep -c "StatusCode::NOT_IMPLEMENTED" "$DISPATCH" 2>/dev/null)
doc501=$(grep -c "501" "$WSDOC" 2>/dev/null)
if [ "$stub" -ge 1 ] && [ "$doc501" -ge 1 ]; then
  pass "stub present ($stub site) and $WSDOC states the 501"
elif [ "$stub" -eq 0 ] && [ "$doc501" -eq 0 ]; then
  pass "stub gone and the doc no longer claims a 501 -- consistent"
elif [ "$stub" -ge 1 ]; then
  fail "the gateway still answers subscriptions with 501 ($stub site) but
       $WSDOC never mentions it. A creator reading that doc has no way to
       learn their socket works in \`zeroship serve\` and not deployed."
else
  fail "the 501 stub is GONE from $DISPATCH but $WSDOC still tells creators
       subscriptions return 501. If the proxy landed, update the doc."
fi

# --- anti-hollow guard ----------------------------------------------------
# A filtered-green and a real green print the same tally without this.
if [ "$RAN" -lt 2 ]; then
  echo "GATE DID NOT RUN: expected 2 arms, ran $RAN"
  exit 1
fi

echo "  ws subscription stub gate: $PASS passed, $FAIL failed ($RAN arms ran)"
[ "$FAIL" -eq 0 ] || exit 1
