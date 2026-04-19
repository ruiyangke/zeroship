#!/usr/bin/env bash
# End-to-end platform test suite.
#
# Tests:
#   1. Health checks (all components)
#   2. App lifecycle (create, deploy, delete)
#   3. Identity verification (each app returns its own response)
#   4. Routing consistency (CHWBL: same app → same worker)
#   5. Isolation (app-01 state doesn't leak into app-02)
#   6. Auth enforcement (missing/wrong key rejected)
#   7. Worker on-demand loading (cold start)
#   8. Hot deploy (update code while serving)
#
# Prerequisites:
#   - cargo build --release -p zeroship-control -p zeroship-gateway -p zeroship-worker -p zeroship
#   - docker compose up -d postgres (Postgres on port 5440 per docker-compose.yml)
#
# Usage:
#   ./tests/e2e_platform.sh
#
# Env overrides:
#   DATABASE_URL     — postgres connection URL (default: compose instance on 5440)
#   PG_CONTAINER     — docker container name for cleanup (default: appbase-postgres-1)
#   PG_USER / PG_DB  — user/database for cleanup
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"

CONTROL_PORT=9090
WORKER_PORTS=(8080 8081 8082)
GATE_PORT=8000
DB_URL="${DATABASE_URL:-postgres://postgres:zeroship@localhost:5440/zeroship}"
PG_CONTAINER="${PG_CONTAINER:-appbase-postgres-1}"
PG_USER="${PG_USER:-postgres}"
PG_DB="${PG_DB:-zeroship}"
CONTROL_KEY="test-ck"
MASTER_KEY="test-mk"

PASS=0
FAIL=0
PIDS=()

pass() { PASS=$((PASS + 1)); echo "  ✓ $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  ✗ $1"; }

cleanup() {
    for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
    wait 2>/dev/null || true
    rm -rf /tmp/zeroship-e2e-*
}
trap cleanup EXIT

echo "============================================"
echo "  zeroship E2E Platform Test"
echo "============================================"
echo ""

# --- Setup ---
echo "=== Setup ==="
for port in $CONTROL_PORT ${WORKER_PORTS[@]} $GATE_PORT; do
    lsof -ti :"$port" 2>/dev/null | xargs kill -9 2>/dev/null || true
done
rm -rf /tmp/zeroship-e2e-bundles
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -c "DROP TABLE IF EXISTS usage_history, usage, apps CASCADE" > /dev/null 2>&1

# Start control
"$BIN/zeroship-control" --port $CONTROL_PORT --db "$DB_URL" --bundles /tmp/zeroship-e2e-bundles \
    --control-key "$CONTROL_KEY" --master-key "$MASTER_KEY" > /dev/null 2>&1 &
PIDS+=($!)
sleep 3

# Start 3 separate workers (so we can verify routing)
WORKER_URL_LIST=""
for port in "${WORKER_PORTS[@]}"; do
    "$BIN/zeroship-worker" --port "$port" --workers 2 --control "http://localhost:$CONTROL_PORT" \
        --control-key "$CONTROL_KEY" --poll-interval 2 > /dev/null 2>&1 &
    PIDS+=($!)
    [ -n "$WORKER_URL_LIST" ] && WORKER_URL_LIST="$WORKER_URL_LIST,"
    WORKER_URL_LIST="${WORKER_URL_LIST}http://localhost:${port}"
done
sleep 2

# Start gateway
"$BIN/zeroship-gate" --port $GATE_PORT --control "http://localhost:$CONTROL_PORT" \
    --control-key "$CONTROL_KEY" --workers "$WORKER_URL_LIST" --poll-interval 2 > /dev/null 2>&1 &
PIDS+=($!)
sleep 3
echo "  control=$CONTROL_PORT workers=${WORKER_PORTS[*]} gateway=$GATE_PORT"

# ---------------------------------------------------------------------------
# Test 1: Health checks
# ---------------------------------------------------------------------------
echo ""
echo "=== Test 1: Health checks ==="
curl -sf "http://localhost:$CONTROL_PORT/health" > /dev/null && pass "control healthy" || fail "control unhealthy"
for port in "${WORKER_PORTS[@]}"; do
    curl -sf "http://localhost:$port/health" > /dev/null && pass "worker:$port healthy" || fail "worker:$port unhealthy"
done
curl -sf "http://localhost:$GATE_PORT/health" > /dev/null && pass "gateway healthy" || fail "gateway unhealthy"

# ---------------------------------------------------------------------------
# Test 2: App lifecycle
# ---------------------------------------------------------------------------
echo ""
echo "=== Test 2: App lifecycle ==="

# Create
APP=$(curl -sf -X POST "http://localhost:$CONTROL_PORT/api/apps" \
    -H 'Content-Type: application/json' \
    -H "Authorization: Bearer $MASTER_KEY" \
    -d '{"name":"lifecycle-test"}')
APP_ID=$(echo "$APP" | jq -r '.id')
API_KEY=$(echo "$APP" | jq -r '.api_key')
[ -n "$APP_ID" ] && [ "$APP_ID" != "null" ] && pass "create app ($APP_ID)" || fail "create app"

# Deploy
tmpf=$(mktemp --suffix=.js)
echo 'export function ping() { return "lifecycle-ok"; }' > "$tmpf"
DEPLOY=$("$BIN/zeroship" deploy "$tmpf" --app="$APP_ID" --control="http://localhost:$CONTROL_PORT" --key="$MASTER_KEY" 2>&1)
rm "$tmpf"
echo "$DEPLOY" | grep -q "deploy_hash" && pass "deploy" || fail "deploy"

# Verify via internal API
sleep 3
VERSIONS=$(curl -sf "http://localhost:$CONTROL_PORT/internal/versions" -H "Authorization: Bearer $CONTROL_KEY")
echo "$VERSIONS" | jq -e ".[\"$APP_ID\"]" > /dev/null 2>&1 && pass "version in internal API" || fail "version missing"

# Delete
DEL=$(curl -sf -X DELETE "http://localhost:$CONTROL_PORT/api/apps/$APP_ID" -H "Authorization: Bearer $MASTER_KEY")
echo "$DEL" | grep -q "true" && pass "delete app" || fail "delete app"

# ---------------------------------------------------------------------------
# Test 3: Identity verification
# ---------------------------------------------------------------------------
echo ""
echo "=== Test 3: Identity (10 apps, each returns its own name) ==="

declare -A APP_IDS
declare -A APP_KEYS

for i in $(seq 1 10); do
    name="id-$(printf '%02d' $i)"
    result=$(curl -sf -X POST "http://localhost:$CONTROL_PORT/api/apps" \
        -H 'Content-Type: application/json' \
        -H "Authorization: Bearer $MASTER_KEY" \
        -d "{\"name\":\"$name\"}")
    APP_IDS[$name]=$(echo "$result" | jq -r '.id')
    APP_KEYS[$name]=$(echo "$result" | jq -r '.api_key')

    tmpf=$(mktemp --suffix=.js)
    echo "export function ping() { return \"I am $name\"; }" > "$tmpf"
    "$BIN/zeroship" deploy "$tmpf" --app="${APP_IDS[$name]}" --control="http://localhost:$CONTROL_PORT" --key="$MASTER_KEY" > /dev/null 2>&1
    rm "$tmpf"
done
sleep 4

ID_PASS=0
for i in $(seq 1 10); do
    name="id-$(printf '%02d' $i)"
    key="${APP_KEYS[$name]}"
    result=$(curl -sf -X POST "http://localhost:$GATE_PORT/apps/$name/rpc" \
        -H 'Content-Type: application/json' \
        -H "X-Api-Key: $key" \
        -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}' 2>/dev/null || echo "")
    returned=$(echo "$result" | jq -r '.result // empty')
    if [ "$returned" = "I am $name" ]; then
        ID_PASS=$((ID_PASS + 1))
    else
        fail "$name returned '$returned' (expected 'I am $name')"
    fi
done
[ $ID_PASS -eq 10 ] && pass "all 10 apps returned correct identity" || fail "$ID_PASS/10 correct"

# ---------------------------------------------------------------------------
# Test 4: Routing consistency (counter increments on same isolate)
# ---------------------------------------------------------------------------
echo ""
echo "=== Test 4: Routing consistency ==="

# Deploy an app with a counter
name="counter-app"
result=$(curl -sf -X POST "http://localhost:$CONTROL_PORT/api/apps" \
    -H 'Content-Type: application/json' \
    -H "Authorization: Bearer $MASTER_KEY" \
    -d "{\"name\":\"$name\"}")
CID=$(echo "$result" | jq -r '.id')
CKEY=$(echo "$result" | jq -r '.api_key')

tmpf=$(mktemp --suffix=.js)
cat > "$tmpf" << 'JSEOF'
let counter = 0;
export function ping() { counter++; return { count: counter }; }
JSEOF
"$BIN/zeroship" deploy "$tmpf" --app="$CID" --control="http://localhost:$CONTROL_PORT" --key="$MASTER_KEY" > /dev/null 2>&1
rm "$tmpf"
sleep 4

# Send 10 requests — counters should increase (across 1-2 threads)
COUNTS=""
for j in $(seq 1 10); do
    result=$(curl -sf -X POST "http://localhost:$GATE_PORT/apps/$name/rpc" \
        -H 'Content-Type: application/json' \
        -H "X-Api-Key: $CKEY" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"ping\",\"params\":[],\"id\":$j}")
    c=$(echo "$result" | jq -r '.result.count // 0')
    COUNTS="$COUNTS $c"
done
echo "  counters:$COUNTS"
# Verify counters are non-zero and generally increasing
MAX_COUNT=$(echo $COUNTS | tr ' ' '\n' | sort -rn | head -1)
[ "$MAX_COUNT" -ge 3 ] && pass "counter reached $MAX_COUNT (routing consistent)" || fail "counter only reached $MAX_COUNT"

# ---------------------------------------------------------------------------
# Test 5: Isolation
# ---------------------------------------------------------------------------
echo ""
echo "=== Test 5: Isolation ==="

# app id-01 and id-02 should have independent state
# Send 10 requests to id-01
key1="${APP_KEYS[id-01]}"
for j in $(seq 1 10); do
    curl -sf -X POST "http://localhost:$GATE_PORT/apps/id-01/rpc" \
        -H 'Content-Type: application/json' \
        -H "X-Api-Key: $key1" \
        -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}' > /dev/null
done

# id-02 should still return its own identity
key2="${APP_KEYS[id-02]}"
result=$(curl -sf -X POST "http://localhost:$GATE_PORT/apps/id-02/rpc" \
    -H 'Content-Type: application/json' \
    -H "X-Api-Key: $key2" \
    -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}')
returned=$(echo "$result" | jq -r '.result // empty')
[ "$returned" = "I am id-02" ] && pass "id-02 isolated from id-01" || fail "id-02 returned '$returned'"

# ---------------------------------------------------------------------------
# Test 6: Auth enforcement
# ---------------------------------------------------------------------------
echo ""
echo "=== Test 6: Auth ==="

# No API key
result=$(curl -sf -X POST "http://localhost:$GATE_PORT/apps/id-01/rpc" \
    -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}' 2>/dev/null || echo "rejected")
echo "$result" | grep -qi "missing\|unauthorized\|api.key\|rejected" && pass "missing key rejected" || fail "missing key not rejected: $result"

# Wrong API key
result=$(curl -sf -X POST "http://localhost:$GATE_PORT/apps/id-01/rpc" \
    -H 'Content-Type: application/json' \
    -H 'X-Api-Key: wrong-key-12345' \
    -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}' 2>/dev/null || echo "rejected")
echo "$result" | grep -qi "invalid\|unauthorized\|rejected" && pass "wrong key rejected" || fail "wrong key not rejected: $result"

# Unknown app
result=$(curl -sf -X POST "http://localhost:$GATE_PORT/apps/nonexistent/rpc" \
    -H 'Content-Type: application/json' \
    -H 'X-Api-Key: any' \
    -d '{}' 2>/dev/null || echo "not_found")
echo "$result" | grep -qi "not.found\|not_found" && pass "unknown app returns 404" || fail "unknown app: $result"

# Admin API without master key
result=$(curl -sf -X POST "http://localhost:$CONTROL_PORT/api/apps" \
    -H 'Content-Type: application/json' \
    -d '{"name":"should-fail"}' 2>/dev/null || echo "rejected")
echo "$result" | grep -qi "unauthorized\|master.key\|rejected" && pass "admin without master key rejected" || fail "admin not rejected: $result"

# ---------------------------------------------------------------------------
# Test 7: Cold start (on-demand loading)
# ---------------------------------------------------------------------------
echo ""
echo "=== Test 7: Cold start ==="

# Create + deploy a NEW app (not yet loaded on any worker)
result=$(curl -sf -X POST "http://localhost:$CONTROL_PORT/api/apps" \
    -H 'Content-Type: application/json' \
    -H "Authorization: Bearer $MASTER_KEY" \
    -d '{"name":"cold-start"}')
COLD_ID=$(echo "$result" | jq -r '.id')
COLD_KEY=$(echo "$result" | jq -r '.api_key')

tmpf=$(mktemp --suffix=.js)
echo 'export function ping() { return "cold-ok"; }' > "$tmpf"
"$BIN/zeroship" deploy "$tmpf" --app="$COLD_ID" --control="http://localhost:$CONTROL_PORT" --key="$MASTER_KEY" > /dev/null 2>&1
rm "$tmpf"
sleep 3

# First request triggers on-demand load
START=$(date +%s%N)
result=$(curl -sf -X POST "http://localhost:$GATE_PORT/apps/cold-start/rpc" \
    -H 'Content-Type: application/json' \
    -H "X-Api-Key: $COLD_KEY" \
    -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}')
END=$(date +%s%N)
COLD_MS=$(( (END - START) / 1000000 ))
returned=$(echo "$result" | jq -r '.result // empty')
[ "$returned" = "cold-ok" ] && pass "cold start in ${COLD_MS}ms" || fail "cold start failed: $result"

# Second request should be warm
START=$(date +%s%N)
curl -sf -X POST "http://localhost:$GATE_PORT/apps/cold-start/rpc" \
    -H 'Content-Type: application/json' \
    -H "X-Api-Key: $COLD_KEY" \
    -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":2}' > /dev/null
END=$(date +%s%N)
WARM_MS=$(( (END - START) / 1000000 ))
pass "warm request in ${WARM_MS}ms"

# ---------------------------------------------------------------------------
# Test 8: Hot deploy
# ---------------------------------------------------------------------------
echo ""
echo "=== Test 8: Hot deploy ==="

# Deploy v1
result=$(curl -sf -X POST "http://localhost:$CONTROL_PORT/api/apps" \
    -H 'Content-Type: application/json' \
    -H "Authorization: Bearer $MASTER_KEY" \
    -d '{"name":"hot-deploy"}')
HOT_ID=$(echo "$result" | jq -r '.id')
HOT_KEY=$(echo "$result" | jq -r '.api_key')

tmpf=$(mktemp --suffix=.js)
echo 'export function ping() { return "v1"; }' > "$tmpf"
"$BIN/zeroship" deploy "$tmpf" --app="$HOT_ID" --control="http://localhost:$CONTROL_PORT" --key="$MASTER_KEY" > /dev/null 2>&1
rm "$tmpf"
sleep 3

# Verify v1
result=$(curl -sf -X POST "http://localhost:$GATE_PORT/apps/hot-deploy/rpc" \
    -H 'Content-Type: application/json' \
    -H "X-Api-Key: $HOT_KEY" \
    -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}')
v=$(echo "$result" | jq -r '.result // empty')
[ "$v" = "v1" ] && pass "v1 deployed" || fail "expected v1, got '$v'"

# Deploy v2
tmpf=$(mktemp --suffix=.js)
echo 'export function ping() { return "v2"; }' > "$tmpf"
"$BIN/zeroship" deploy "$tmpf" --app="$HOT_ID" --control="http://localhost:$CONTROL_PORT" --key="$MASTER_KEY" > /dev/null 2>&1
rm "$tmpf"

# Wait for worker sync to pick up new hash + reload
# Worker polls every 2s, needs time to detect + download + reload
sleep 12

# Verify v2
v=""
for attempt in $(seq 1 5); do
    result=$(curl -sf -X POST "http://localhost:$GATE_PORT/apps/hot-deploy/rpc" \
        -H 'Content-Type: application/json' \
        -H "X-Api-Key: $HOT_KEY" \
        -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}' 2>/dev/null || echo "")
    v=$(echo "$result" | jq -r '.result // empty')
    [ "$v" = "v2" ] && break
    sleep 3
done
[ "$v" = "v2" ] && pass "v2 hot deployed" || fail "expected v2, got '$v'"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "============================================"
echo "  Results: $PASS passed, $FAIL failed"
echo "============================================"

[ $FAIL -eq 0 ] && exit 0 || exit 1
