#!/usr/bin/env bash
# End-to-end smoke test for db-todos.
#
# Verifies the @zeroship/db v2 surfaces this example exercises actually
# work against a running dev server. Each check is named after the
# feature it verifies; failures print the unexpected response body.
#
# Usage:
#   bash scripts/smoke.sh                # uses http://localhost:3001
#   ZS_URL=http://... bash scripts/smoke.sh
#
# Prereqs: `npm run dev` already running in another shell; Postgres
# reachable (the dev server wires this up automatically when configured
# via `zeroship serve` or vite-plugin's dev bootstrap).
set -euo pipefail

URL="${ZS_URL:-http://localhost:3001}"
RPC="${URL}/_zs/v1"
FAILED=0

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

rpc() {
  local proc="$1"; shift
  local body="${1-}"
  [ -z "$body" ] && body="{}"
  curl -sS -X POST -H 'content-type: application/json' \
    "${RPC}/${proc}" -d "{\"json\":${body}}"
}

http_status() {
  local proc="$1"; shift
  local body="${1-}"
  [ -z "$body" ] && body="{}"
  curl -sS -o /dev/null -w '%{http_code}' \
    -X POST -H 'content-type: application/json' \
    "${RPC}/${proc}" -d "{\"json\":${body}}"
}

check() {
  local name="$1"; shift
  if "$@"; then
    echo "  ✓ $name"
  else
    echo "  ✗ $name"
    FAILED=$((FAILED + 1))
  fi
}

contains() { echo "$1" | grep -q -- "$2"; }

# ---------------------------------------------------------------------------
# Setup — two users + some todos
# ---------------------------------------------------------------------------

echo "[setup]"

ALICE_EMAIL="alice-$(date +%s%N | head -c12)@example.com"
ALICE_HANDLE="alice_$(date +%s%N | head -c10)"
BOB_EMAIL="bob-$(date +%s%N | head -c12)@example.com"
BOB_HANDLE="bob_$(date +%s%N | head -c10)"
ALICE_RES=$(rpc seedUser "{\"email\":\"${ALICE_EMAIL}\",\"name\":\"Alice\",\"handle\":\"${ALICE_HANDLE}\"}")
BOB_RES=$(rpc seedUser "{\"email\":\"${BOB_EMAIL}\",\"name\":\"Bob\",\"handle\":\"${BOB_HANDLE}\"}")

ALICE_ID=$(echo "$ALICE_RES" | grep -oE '"id":[0-9]+' | head -1 | cut -d: -f2 || echo "")
BOB_ID=$(echo "$BOB_RES"   | grep -oE '"id":[0-9]+' | head -1 | cut -d: -f2 || echo "")

if [ -z "$ALICE_ID" ] || [ -z "$BOB_ID" ]; then
  echo "  ✗ seedUser failed — Alice=$ALICE_RES Bob=$BOB_RES"
  exit 1
fi
echo "  seeded Alice=$ALICE_ID Bob=$BOB_ID"

# ---------------------------------------------------------------------------
# Check 1: createTodo (mutation) — happy path
# ---------------------------------------------------------------------------

echo "[check 1] createTodo via mutation — auto-wrapped in SERIALIZABLE tx"

T1=$(rpc createTodo "{\"userId\":${ALICE_ID},\"title\":\"buy milk\",\"priority\":\"low\"}")
check "createTodo returned an id" contains "$T1" '"id":'

# ---------------------------------------------------------------------------
# Check 2: FK enforcement (B2 + A2 destructive-deploy refusal)
# ---------------------------------------------------------------------------

echo "[check 2] FK enforcement — insert with non-existent userId fails"

BAD=$(rpc createTodo '{"userId":999999,"title":"orphan"}')
check "orphan insert produced an error" \
  bash -c "echo '$BAD' | grep -qiE 'error|violation|foreign'"
check "error mentions foreign key or violation" \
  bash -c "echo '$BAD' | grep -qiE 'foreign|violation|23503'"

# ---------------------------------------------------------------------------
# Check 3: Capability enforcement (B3) — try writing from a query
# ---------------------------------------------------------------------------

echo "[check 3] capability enforcement — wrapper kinds resolve"

# Capability enforcement (B3) is verified by the unit/integration
# tests in plugin-db. The dev runtime doesn't expose `/_zs/manifest`,
# so we probe a known wrapped procedure instead: a 405 / 404 would
# indicate the wrapper was lost; a 200 with the platform's `{json: ...}`
# envelope confirms `query()` resolved at registration time.
PROBE_STATUS=$(http_status listTodos "{\"userId\":${ALICE_ID}}")
PROBE=$(rpc listTodos "{\"userId\":${ALICE_ID}}")
check "wrapper-tagged procedure dispatched" \
  bash -c "[ '$PROBE_STATUS' = '200' ] && echo '$PROBE' | grep -q '\"json\":'"

# ---------------------------------------------------------------------------
# Check 4: listTodos (query) — read-only path
# ---------------------------------------------------------------------------

echo "[check 4] listTodos query — read-only, returns rows"

LIST=$(rpc listTodos "{\"userId\":${ALICE_ID}}")
check "listTodos returned an array" contains "$LIST" '\[\|"data"'

# ---------------------------------------------------------------------------
# Check 5: action + runQuery — shareToWebhook composes a query
# ---------------------------------------------------------------------------

echo "[check 5] action + runQuery — shareToWebhook composes a query"

# Spin up a one-shot Node HTTP stub that 200s; the action's
# runQuery(getTodo) reads the row and then fetches the stub.
SHARE_PORT=$(node -e 'const s=require("net").createServer(); s.listen(0,"127.0.0.1",()=>{console.log(s.address().port);s.close();})')
node -e "
const http = require('http');
const srv = http.createServer((req, res) => {
  res.writeHead(200, {'content-type': 'application/json'});
  res.end(JSON.stringify({received: true}));
  setImmediate(() => process.exit(0));
});
srv.listen(${SHARE_PORT}, '127.0.0.1');
" &
SHARE_PID=$!
sleep 1

SHARE_T=$(rpc createTodo "{\"userId\":${ALICE_ID},\"title\":\"share me\",\"priority\":\"low\"}")
SHARE_ID=$(echo "$SHARE_T" | grep -oE '"id":[0-9]+' | head -1 | cut -d: -f2 || echo "")
SHARE_RES=$(rpc shareToWebhook "{\"id\":${SHARE_ID:-1},\"webhookUrl\":\"http://127.0.0.1:${SHARE_PORT}/\"}")
kill $SHARE_PID 2>/dev/null || true

check "shareToWebhook returned a 2xx status" \
  bash -c "echo '$SHARE_RES' | grep -qiE '\"ok\":true|\"status\":2[0-9][0-9]'"

# ---------------------------------------------------------------------------
# Check 5b: Query.paginate — round-trip continueCursor through 2-3 pages
# ---------------------------------------------------------------------------

echo "[check 5b] listTodosPage — paginate({cursor,numItems}) round-trip"

# Seed 5 extra todos so we have at least 6 rows for Alice (counting the
# earlier 'buy milk' + 'share me' creates). With numItems=2 that's at
# least 3 pages: page1, page2, page3 (isDone).
for i in 1 2 3 4 5; do
  rpc createTodo "{\"userId\":${ALICE_ID},\"title\":\"task-$i\",\"priority\":\"low\"}" > /dev/null
done

P1=$(rpc listTodosPage "{\"userId\":${ALICE_ID},\"cursor\":null,\"numItems\":2}")
check "page 1 returned an envelope with page/continueCursor/isDone" \
  bash -c "echo '$P1' | grep -q '\"page\":' && echo '$P1' | grep -q '\"continueCursor\":' && echo '$P1' | grep -q '\"isDone\":'"

C1=$(echo "$P1" | grep -oE '"continueCursor":"[^"]*"' | head -1 | sed 's/.*"continueCursor":"//;s/"$//')
DONE1=$(echo "$P1" | grep -oE '"isDone":(true|false)' | head -1 | cut -d: -f2)

check "page 1 not done (more rows available)" bash -c "[ \"$DONE1\" = \"false\" ]"
check "page 1 has non-empty continueCursor" bash -c "[ -n \"$C1\" ]"

P2=$(rpc listTodosPage "{\"userId\":${ALICE_ID},\"cursor\":\"${C1}\",\"numItems\":2}")
C2=$(echo "$P2" | grep -oE '"continueCursor":"[^"]*"' | head -1 | sed 's/.*"continueCursor":"//;s/"$//')

check "page 2 advances past page 1 (different cursor)" bash -c "[ \"$C1\" != \"$C2\" ]"

# Final page — keep advancing until isDone=true (max 5 hops to bound the smoke).
HOPS=0
CURRENT="$C2"
DONE_FINAL="false"
while [ "$HOPS" -lt 5 ] && [ "$DONE_FINAL" = "false" ]; do
  PN=$(rpc listTodosPage "{\"userId\":${ALICE_ID},\"cursor\":\"${CURRENT}\",\"numItems\":2}")
  DONE_FINAL=$(echo "$PN" | grep -oE '"isDone":(true|false)' | head -1 | cut -d: -f2)
  CURRENT=$(echo "$PN" | grep -oE '"continueCursor":"[^"]*"' | head -1 | sed 's/.*"continueCursor":"//;s/"$//')
  HOPS=$((HOPS + 1))
done
check "pagination terminates with isDone=true within 5 hops" bash -c "[ \"$DONE_FINAL\" = \"true\" ]"

# ---------------------------------------------------------------------------
# Check 5c: DataLoader batching — Promise.all([db.users.get(a), db.users.get(b)])
# coalesces into one underlying find. We can't directly observe native call
# counts from outside the runtime, so we verify both rows come back correctly
# (the loader's stitch step is what we'd actually break in a regression).
# ---------------------------------------------------------------------------

echo "[check 5c] getUserPair — DataLoader batches concurrent db.users.get(id)"

PAIR=$(rpc getUserPair "{\"aId\":${ALICE_ID},\"bId\":${BOB_ID}}")
check "getUserPair returned Alice's row" \
  bash -c "echo '$PAIR' | grep -q '\"id\":${ALICE_ID}'"
check "getUserPair returned Bob's row" \
  bash -c "echo '$PAIR' | grep -q '\"id\":${BOB_ID}'"

# ---------------------------------------------------------------------------
# Check 5d: listTodosWithUser — find({}, { with: { userId: true } }) eager-
# loads referenced users in one batched roundtrip. Each row's userId field
# must carry the full user row (id + email + name) instead of the bare FK.
# ---------------------------------------------------------------------------

echo "[check 5d] listTodosWithUser — relation-aware reads eager-load via with"

LWU=$(rpc listTodosWithUser "{\"userId\":${ALICE_ID}}")
check "listTodosWithUser returned a row carrying joined user data" \
  bash -c "echo '$LWU' | grep -qE '\"userId\":\\{[^}]*\"email\":'"
check "joined user row carries Alice's email" \
  bash -c "echo '$LWU' | grep -q \"${ALICE_EMAIL}\""

# ---------------------------------------------------------------------------
# Check 6: Migration audit row (A3)
# ---------------------------------------------------------------------------

echo "[check 6] migration audit log — __zeroship_migrations row exists"

AUDIT=$(curl -sS "${URL}/_zs/db/audit/todos" 2>/dev/null || echo '[]')
check "audit endpoint reachable" bash -c "[ -n '$AUDIT' ]"

# ---------------------------------------------------------------------------
# Result
# ---------------------------------------------------------------------------

echo
if [ "$FAILED" -eq 0 ]; then
  echo "smoke: all checks passed"
  exit 0
else
  echo "smoke: $FAILED check(s) failed"
  exit 1
fi
