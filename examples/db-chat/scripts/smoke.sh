#!/usr/bin/env bash
# End-to-end smoke test for db-chat.
#
# Exercises the db-chat RPC handlers through the dev server.
#
# SELF-STARTING BY DEFAULT. Run it with nothing else up and it migrates, boots
# its own dev server on a FREE port, runs, and reaps the server on exit. It used
# to require `pnpm dev` in another shell and would otherwise die with
# `curl: (7) Failed to connect to localhost port 3001` -- which meant it had
# never run unattended, and so could not be gated.
#
# Set ZEROSHIP_URL to point at a server you started yourself; the self-start is
# then skipped entirely and nothing is spawned or killed.
#
# WHY A FREE PORT AND NOT 3001: examples that do not set `devServerPort` all
# share the 3001 default and collide OPAQUELY -- the loser hangs rather than
# reporting a bound port. Picking a free port and passing it through
# DB_CHAT_API_PORT (see vite.config.ts) makes this harness independent of
# whatever else is running.
#
# WHY MIGRATE FIRST: schema comes from committed migrations, and the dev runtime
# only READS it. Without `pnpm migrate` the tables do not exist and every check
# fails with `no such table`.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_ROOT="$(cd "$HERE/.." && pwd)"

SERVER_PID=""
cleanup() {
  # Kill the whole process group: `pnpm dev` spawns vite which spawns the
  # zeroship runtime, and killing only the pnpm pid strands both children.
  if [ -n "$SERVER_PID" ]; then
    kill -- "-$SERVER_PID" 2>/dev/null || kill "$SERVER_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

if [ -n "${ZEROSHIP_URL:-}" ]; then
  URL="$ZEROSHIP_URL"
  echo "[setup] using ZEROSHIP_URL=$URL (not starting a server)"
else
  PORT="$(node -e 'const s=require("net").createServer(); s.listen(0,"127.0.0.1",()=>{process.stdout.write(String(s.address().port)+"\n");s.close();})')"
  URL="http://127.0.0.1:${PORT}"
  echo "[setup] no ZEROSHIP_URL; starting our own dev server on ${URL}"

  ( cd "$APP_ROOT" && DB_CHAT_API_PORT="$PORT" pnpm migrate >/dev/null ) \
    || { echo "  x pnpm migrate failed - the schema would be missing" >&2; exit 1; }

  # `set -m` (job control) puts the background job in its OWN process group with
  # the subshell as leader, which is what makes `kill -- -$SERVER_PID` in the
  # trap reach vite AND the zeroship runtime it spawns.
  #
  # DO NOT put `setsid` here. It was the first thing I tried and it LEAKS: setsid
  # moves the server into a brand-new session, so `$!` captures the short-lived
  # subshell instead and the trap signals a group the server is no longer in.
  # Measured -- that version exited 0 with all checks green and left a live
  # listener behind, which is the worst combination because the run looks clean.
  set -m
  ( cd "$APP_ROOT" && DB_CHAT_API_PORT="$PORT" exec pnpm dev >/dev/null 2>&1 ) &
  SERVER_PID=$!
  set +m

  # Wait on the DISPATCHER answering, not on the port being open: the port is
  # bound before the runtime finishes registering procedures, so a port check
  # would let the first RPC race the boot.
  READY=0
  for _ in $(seq 1 60); do
    if curl -sS -m 2 -X POST -H 'content-type: application/json' \
         "${URL}/__zeroship/v1/listMessages" -d '{"json":{"channelId":"chan_probe"}}' >/dev/null 2>&1; then
      READY=1; break
    fi
    sleep 1
  done
  [ "$READY" -eq 1 ] || { echo "  x dev server never answered on ${URL} within 60s" >&2; exit 1; }
fi

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

GENERAL_ID=$(echo "$GENERAL" | grep -oE '"id":"[^"]+"' | head -1 | cut -d: -f2 | tr -d '"' || echo "")
RANDOM_ID=$(echo "$RANDOM_CH" | grep -oE '"id":"[^"]+"' | head -1 | cut -d: -f2 | tr -d '"' || echo "")
AUTHOR_ID=$(echo "$AUTHOR_RES" | grep -oE '"id":"[^"]+"' | head -1 | cut -d: -f2 | tr -d '"' || echo "")

if [ -z "$GENERAL_ID" ] || [ -z "$RANDOM_ID" ] || [ -z "$AUTHOR_ID" ]; then
  echo "  ✗ setup failed — general=$GENERAL random=$RANDOM_CH author=$AUTHOR_RES"
  exit 1
fi
echo "  seeded general=$GENERAL_ID random=$RANDOM_ID author=$AUTHOR_ID"

# ---------------------------------------------------------------------------
# Check 1: sendMessage to general (mutation capability)
# ---------------------------------------------------------------------------

echo "[check 1] sendMessage — mutation"

M1=$(rpc sendMessage "{\"channelId\":\"${GENERAL_ID}\",\"authorId\":\"${AUTHOR_ID}\",\"body\":\"hello general\"}")
check "sendMessage returned an id" contains "$M1" '"id":'

# ---------------------------------------------------------------------------
# Check 2: listMessages narrows to channel (B3 query wrapper)
# ---------------------------------------------------------------------------

echo "[check 2] listMessages — query wrapper, returns rows"

LIST_GENERAL=$(rpc listMessages "{\"channelId\":\"${GENERAL_ID}\"}")
check "listMessages returned an array shape" contains "$LIST_GENERAL" '\['

# ---------------------------------------------------------------------------
# Check 3: channel isolation — messages in #random don't leak to #general
# ---------------------------------------------------------------------------

echo "[check 3] channel isolation"

rpc sendMessage "{\"channelId\":\"${RANDOM_ID}\",\"authorId\":\"${AUTHOR_ID}\",\"body\":\"hello random\"}" >/dev/null

# Re-list general — should NOT contain "hello random"
LIST_AFTER=$(rpc listMessages "{\"channelId\":\"${GENERAL_ID}\"}")
check "general channel still doesn't see random's messages" \
  bash -c "! echo '$LIST_AFTER' | grep -q 'hello random'"

# ---------------------------------------------------------------------------
# Check 4: FK enforcement — sendMessage with non-existent channel fails
# ---------------------------------------------------------------------------

echo "[check 4] FK enforcement — orphan channel rejected"

BAD=$(rpc sendMessage "{\"channelId\":\"chan_thisiddoesnotexist\",\"authorId\":\"${AUTHOR_ID}\",\"body\":\"orphan\"}")
check "orphan channel insert produced an error" \
  bash -c "echo '$BAD' | grep -qiE 'error|violation|foreign'"

# ---------------------------------------------------------------------------
# Check 5: flagMessage updates row and listMessages filters it out
# ---------------------------------------------------------------------------

echo "[check 5] flagMessage updates row and listMessages filters it out"

M_ID=$(echo "$M1" | grep -oE '"id":"[^"]+"' | head -1 | cut -d: -f2 | tr -d '"' || echo "")
if [ -n "$M_ID" ]; then
  rpc flagMessage "{\"id\":\"${M_ID}\"}" >/dev/null
  LIST_AFTER_FLAG=$(rpc listMessages "{\"channelId\":\"${GENERAL_ID}\"}")
  check "flagged message no longer in listMessages" \
    bash -c "! echo '$LIST_AFTER_FLAG' | grep -q '\"id\":\"${M_ID}\",'"
else
  check "obtained M1 id for flag test" bash -c "false"
fi

# ---------------------------------------------------------------------------
# Check 6: action — runQuery (read) + runMutation (write) round-trip
# ---------------------------------------------------------------------------

echo "[check 6] action — runQuery (read) + runMutation (write) round-trip"

# Spin up a tiny one-shot moderation stub. Returns {"unsafe":true} so
# the action takes the flag-via-runMutation branch.
MOD_PORT=$(node -e 'const s=require("net").createServer(); s.listen(0,"127.0.0.1",()=>{process.stdout.write(String(s.address().port)+"\n");s.close();})')
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

NEW_MSG=$(rpc sendMessage "{\"channelId\":\"${GENERAL_ID}\",\"authorId\":\"${AUTHOR_ID}\",\"body\":\"will be flagged\"}")
NEW_ID=$(echo "$NEW_MSG" | grep -oE '"id":"[^"]+"' | head -1 | cut -d: -f2 | tr -d '"' || echo "")

MODERATE_RES=$(rpc moderateMessage "{\"id\":\"${NEW_ID}\",\"moderationUrl\":\"http://127.0.0.1:${MOD_PORT}/\"}")
kill $MOD_PID 2>/dev/null || true

check "moderateMessage took the unsafe-branch (action → runQuery + fetch + runMutation)" \
  bash -c "echo '$MODERATE_RES' | grep -q '\"flagged\":true'"

LIST_AFTER_MOD=$(rpc listMessages "{\"channelId\":\"${GENERAL_ID}\"}")
check "flagged message via runMutation is filtered from listMessages" \
  bash -c "! echo '$LIST_AFTER_MOD' | grep -q '\"id\":\"${NEW_ID}\"'"

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
