#!/usr/bin/env bash
# End-to-end smoke test for db-chat.
#
# Exercises the db-chat RPC handlers through the dev server. Requires:
#   • A running dev server (`pnpm dev` in another shell)
set -euo pipefail

URL="${ZEROSHIP_URL:-http://localhost:3001}"
RPC="${URL}/__zeroship/v1"
FAILED=0

rpc() {
  local proc="$1"; shift
  local body="${1-}"
  [ -z "$body" ] && body="{}"
  curl -sS -X POST -H 'content-type: application/json' \
    "${RPC}/${proc}" -d "{\"json\":${body}}"
}

check() {
  local name="$1"; shift
  if "$@"; then echo "  ✓ $name"; else echo "  ✗ $name"; FAILED=$((FAILED+1)); fi
}

contains() { echo "$1" | grep -q -- "$2"; }

# ---------------------------------------------------------------------------
# Setup — seed two channels + a user
# ---------------------------------------------------------------------------

echo "[setup] seeding channels + author"

# Unique suffixes so the smoke is re-runnable against a persistent
# Postgres without UNIQUE conflicts.
SUFFIX=$(date +%s%N | head -c10)
GENERAL=$(rpc createChannel "{\"slug\":\"general-${SUFFIX}\",\"name\":\"General\"}")
RANDOM_CH=$(rpc createChannel "{\"slug\":\"random-${SUFFIX}\",\"name\":\"Random\"}")
AUTHOR_RES=$(rpc createUser "{\"handle\":\"user_${SUFFIX}\",\"name\":\"Alice\"}")

GENERAL_ID=$(echo "$GENERAL" | grep -oE '"id":[0-9]+' | head -1 | cut -d: -f2 || echo "")
RANDOM_ID=$(echo "$RANDOM_CH" | grep -oE '"id":[0-9]+' | head -1 | cut -d: -f2 || echo "")
AUTHOR_ID=$(echo "$AUTHOR_RES" | grep -oE '"id":[0-9]+' | head -1 | cut -d: -f2 || echo "")

if [ -z "$GENERAL_ID" ] || [ -z "$RANDOM_ID" ] || [ -z "$AUTHOR_ID" ]; then
  echo "  ✗ setup failed — general=$GENERAL random=$RANDOM_CH author=$AUTHOR_RES"
  exit 1
fi
echo "  seeded general=$GENERAL_ID random=$RANDOM_ID author=$AUTHOR_ID"

# ---------------------------------------------------------------------------
# Check 1: sendMessage to general (mutation capability)
# ---------------------------------------------------------------------------

echo "[check 1] sendMessage — mutation"

M1=$(rpc sendMessage "{\"channelId\":${GENERAL_ID},\"authorId\":${AUTHOR_ID},\"body\":\"hello general\"}")
check "sendMessage returned an id" contains "$M1" '"id":'

# ---------------------------------------------------------------------------
# Check 2: listMessages narrows to channel (B3 query wrapper)
# ---------------------------------------------------------------------------

echo "[check 2] listMessages — query wrapper, returns rows"

LIST_GENERAL=$(rpc listMessages "{\"channelId\":${GENERAL_ID}}")
check "listMessages returned an array shape" contains "$LIST_GENERAL" '\['

# ---------------------------------------------------------------------------
# Check 3: channel isolation — messages in #random don't leak to #general
# ---------------------------------------------------------------------------

echo "[check 3] channel isolation"

rpc sendMessage "{\"channelId\":${RANDOM_ID},\"authorId\":${AUTHOR_ID},\"body\":\"hello random\"}" >/dev/null

# Re-list general — should NOT contain "hello random"
LIST_AFTER=$(rpc listMessages "{\"channelId\":${GENERAL_ID}}")
check "general channel still doesn't see random's messages" \
  bash -c "! echo '$LIST_AFTER' | grep -q 'hello random'"

# ---------------------------------------------------------------------------
# Check 4: FK enforcement — sendMessage with non-existent channel fails
# ---------------------------------------------------------------------------

echo "[check 4] FK enforcement — orphan channel rejected"

BAD=$(rpc sendMessage "{\"channelId\":99999,\"authorId\":${AUTHOR_ID},\"body\":\"orphan\"}")
check "orphan channel insert produced an error" \
  bash -c "echo '$BAD' | grep -qiE 'error|violation|foreign'"

# ---------------------------------------------------------------------------
# Check 5: flagMessage updates row and listMessages filters it out
# ---------------------------------------------------------------------------

echo "[check 5] flagMessage updates row and listMessages filters it out"

M_ID=$(echo "$M1" | grep -oE '"id":[0-9]+' | head -1 | cut -d: -f2 || echo "")
if [ -n "$M_ID" ]; then
  rpc flagMessage "{\"id\":${M_ID}}" >/dev/null
  LIST_AFTER_FLAG=$(rpc listMessages "{\"channelId\":${GENERAL_ID}}")
  check "flagged message no longer in listMessages" \
    bash -c "! echo '$LIST_AFTER_FLAG' | grep -q '\"id\":${M_ID},'"
else
  check "obtained M1 id for flag test" bash -c "false"
fi

# ---------------------------------------------------------------------------
# Check 6: action — runQuery (read) + runMutation (write) round-trip
# ---------------------------------------------------------------------------

echo "[check 6] action — runQuery (read) + runMutation (write) round-trip"

# Spin up a tiny one-shot moderation stub. Returns {"unsafe":true} so
# the action takes the flag-via-runMutation branch.
MOD_PORT=$(node -e 'const s=require("net").createServer(); s.listen(0,"127.0.0.1",()=>{console.log(s.address().port);s.close();})')
node -e "
const http = require('http');
const srv = http.createServer((req, res) => {
  res.writeHead(200, {'content-type': 'application/json'});
  res.end(JSON.stringify({unsafe: true}));
  setImmediate(() => process.exit(0));
});
srv.listen(${MOD_PORT}, '127.0.0.1');
" &
MOD_PID=$!
sleep 1

NEW_MSG=$(rpc sendMessage "{\"channelId\":${GENERAL_ID},\"authorId\":${AUTHOR_ID},\"body\":\"will be flagged\"}")
NEW_ID=$(echo "$NEW_MSG" | grep -oE '"id":[0-9]+' | head -1 | cut -d: -f2 || echo "")

MODERATE_RES=$(rpc moderateMessage "{\"id\":${NEW_ID:-1},\"moderationUrl\":\"http://127.0.0.1:${MOD_PORT}/\"}")
kill $MOD_PID 2>/dev/null || true

check "moderateMessage took the unsafe-branch (action → runQuery + fetch + runMutation)" \
  bash -c "echo '$MODERATE_RES' | grep -q '\"flagged\":true'"

LIST_AFTER_MOD=$(rpc listMessages "{\"channelId\":${GENERAL_ID}}")
check "flagged message via runMutation is filtered from listMessages" \
  bash -c "! echo '$LIST_AFTER_MOD' | grep -q '\"id\":${NEW_ID},'"

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
