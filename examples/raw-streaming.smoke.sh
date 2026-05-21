#!/usr/bin/env bash
# Smoke test for examples/raw-streaming.js — exercises the four ZS-standard
# dispatcher surfaces this demo demonstrates over `zeroship serve` (no Vite):
#   1. query + input.parse (valid + invalid)
#   2. mutation (kind frame, no auto-tx without plugin-db)
#   3. action-like (no kind set; calls outbound fetch)
#   4. AsyncIterator return — verifies the dispatcher accepts the shape;
#      raw deploys can't encode unary streams over POST, so the wire
#      surface for streams is WS subscription (out of curl's reach;
#      documented in the file header).
#
# Usage:
#   target/release/zeroship serve examples/raw-streaming.js --port 3000 &
#   bash examples/raw-streaming.smoke.sh

set -euo pipefail

URL="${ZS_URL:-http://localhost:3000}"
RPC="${URL}/_zs/v1"
FAILED=0

rpc() {
  local proc="$1"; shift
  local body="${1-}"
  [ -z "$body" ] && body="null"
  curl -sS -X POST -H 'content-type: application/json' "${RPC}/${proc}" -d "{\"json\":${body}}"
}

status_of() {
  local proc="$1"; shift
  local body="${1-}"
  [ -z "$body" ] && body="null"
  curl -sS -o /dev/null -w '%{http_code}' -X POST -H 'content-type: application/json' \
    "${RPC}/${proc}" -d "{\"json\":${body}}"
}

check() {
  local name="$1"; shift
  if "$@"; then echo "  ✓ $name"; else echo "  ✗ $name"; FAILED=$((FAILED+1)); fi
}

contains() { echo "$1" | grep -q -- "$2"; }

echo "[check 1] search — query + input.parse"
R=$(rpc search '{"q":"hello","limit":3}')
check "search returned hits" contains "$R" '"hits":\['
check "search echoed meta" contains "$R" '"limit":3'

echo "[check 2] search — invalid input → 400 INVALID_ARGUMENT"
S=$(status_of search '{"limit":3}')
R=$(rpc search '{"limit":3}')
check "status is 400" bash -c "[ '$S' = '400' ]"
check "body surfaces code=INVALID_ARGUMENT" contains "$R" '"code":"INVALID_ARGUMENT"'
check "body surfaces issues path" contains "$R" '"path":\["q"\]'

echo "[check 3] recordNote — mutation (no auto-tx, no plugin-db loaded)"
R=$(rpc recordNote '{"text":"first"}')
check "recordNote echoed text" contains "$R" '"text":"first"'
check "recordNote assigned an id" contains "$R" '"id":"'

echo "[check 3b] recordNote — input.parse-style throw → 400"
S=$(status_of recordNote '{}')
R=$(rpc recordNote '{}')
check "status is 400" bash -c "[ '$S' = '400' ]"
check "body surfaces code=INVALID_ARGUMENT" contains "$R" '"code":"INVALID_ARGUMENT"'

echo "[check 4] echoHeaders — action-like (no kind; outbound fetch)"
R=$(rpc echoHeaders '"https://www.google.com/"')
check "echoHeaders returned status" contains "$R" '"status":'
check "echoHeaders returned contentType" contains "$R" '"contentType":'

# Stream check — the dispatcher accepts the AsyncIterator return.
# Raw-JS deploys can't encode unary streams over POST (kernel falls
# through to default.fetch, which this demo doesn't expose). We
# verify the dispatcher SEES the stream procedure by hitting it once
# and expecting the documented 404 (fallback) — NOT a 500
# (dispatcher broke). If 500, the dict-shape contract regressed.
echo "[check 5] tick — AsyncIterator dispatch (POST falls through to fallback)"
S=$(status_of tick '{"count":2}')
check "POST stream falls through (404 fallback, not 500 broken)" bash -c "[ '$S' = '404' ]"

echo
if [ "$FAILED" -eq 0 ]; then
  echo "smoke: all checks passed"
  exit 0
else
  echo "smoke: $FAILED check(s) failed"
  exit 1
fi
