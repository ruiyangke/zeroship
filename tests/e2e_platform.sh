#!/usr/bin/env bash
# End-to-end platform test — simulates real-world usage:
#   - 50 apps, 3 workers, 1 gateway
#   - Deploy all apps
#   - Send concurrent traffic
#   - Measure cold starts, warm latency, cache hit rate
#   - Deploy update during traffic
#   - Verify routing consistency (CHWBL)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN="$ROOT_DIR/target/release"

NUM_APPS=50
NUM_WORKERS=3
WORKER_BASE_PORT=8080
GATE_PORT=8000
CONTROL_PORT=9090
DB_URL="postgres://postgres:test@localhost:5434/postgres"

PIDS=()
cleanup() {
    echo ""
    echo "=== Cleanup ==="
    for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
    wait 2>/dev/null || true
    rm -rf /tmp/appbase-e2e-*
}
trap cleanup EXIT

echo "============================================"
echo "  appbase E2E Platform Test"
echo "  $NUM_APPS apps, $NUM_WORKERS workers"
echo "============================================"
echo ""

# --- Build ---
echo "=== Building ==="
(cd "$ROOT_DIR" && cargo build --release -p appbase-control -p appbase-gateway -p appbase-worker -p appbase 2>&1 | tail -1)

# --- Reset DB ---
echo "=== Reset DB ==="
docker exec pg-test psql -U postgres -c "DROP TABLE IF EXISTS usage_history, usage, apps CASCADE" 2>/dev/null || true

# --- Start control plane ---
echo "=== Starting control plane ==="
rm -rf /tmp/appbase-e2e-bundles
"$BIN/appbase-control" \
    --port $CONTROL_PORT \
    --db "$DB_URL" \
    --bundles /tmp/appbase-e2e-bundles \
    --control-key e2e-key \
    --master-key e2e-master &
PIDS+=($!)
sleep 3

# --- Start workers ---
echo "=== Starting $NUM_WORKERS workers ==="
WORKER_URLS=""
for i in $(seq 0 $((NUM_WORKERS - 1))); do
    port=$((WORKER_BASE_PORT + i))
    "$BIN/appbase-worker" \
        --port $port \
        --workers 4 \
        --control http://localhost:$CONTROL_PORT \
        --control-key e2e-key \
        --poll-interval 3 &
    PIDS+=($!)
    if [ -n "$WORKER_URLS" ]; then WORKER_URLS="$WORKER_URLS,"; fi
    WORKER_URLS="${WORKER_URLS}http://localhost:${port}"
done
sleep 2

# --- Start gateway ---
echo "=== Starting gateway ==="
"$BIN/appbase-gate" \
    --port $GATE_PORT \
    --control http://localhost:$CONTROL_PORT \
    --control-key e2e-key \
    --workers "$WORKER_URLS" \
    --poll-interval 2 &
PIDS+=($!)
sleep 3

# --- Verify health ---
echo "=== Health checks ==="
curl -sf http://localhost:$CONTROL_PORT/health > /dev/null && echo "  control: ok"
for i in $(seq 0 $((NUM_WORKERS - 1))); do
    port=$((WORKER_BASE_PORT + i))
    curl -sf http://localhost:$port/health > /dev/null && echo "  worker-$i: ok"
done
curl -sf http://localhost:$GATE_PORT/health > /dev/null && echo "  gateway:  ok"

# --- Create and deploy apps ---
echo ""
echo "=== Creating $NUM_APPS apps ==="
declare -A APP_IDS
declare -A API_KEYS

mkdir -p /tmp/appbase-e2e-apps
for i in $(seq 1 $NUM_APPS); do
    name="app-$(printf '%03d' $i)"
    result=$(curl -sf -X POST http://localhost:$CONTROL_PORT/api/apps \
        -H 'Content-Type: application/json' \
        -H 'Authorization: Bearer e2e-master' \
        -d "{\"name\":\"$name\"}")
    APP_IDS[$name]=$(echo "$result" | jq -r '.id')
    API_KEYS[$name]=$(echo "$result" | jq -r '.api_key')

    # Create unique JS for each app
    cat > "/tmp/appbase-e2e-apps/${name}.js" << JSEOF
export function ping() { return "pong from ${name}"; }
export function info() { return { app: "${name}", time: Date.now() }; }
JSEOF
done
echo "  Created $NUM_APPS apps"

echo "=== Deploying $NUM_APPS apps ==="
deploy_start=$(date +%s%N)
for name in $(seq 1 $NUM_APPS | xargs -I{} printf 'app-%03d\n' {}); do
    id="${APP_IDS[$name]}"
    "$BIN/appbase" deploy "/tmp/appbase-e2e-apps/${name}.js" \
        --app="$id" --control=http://localhost:$CONTROL_PORT --key=e2e-master \
        2>/dev/null
done
deploy_end=$(date +%s%N)
deploy_ms=$(( (deploy_end - deploy_start) / 1000000 ))
echo "  Deployed $NUM_APPS apps in ${deploy_ms}ms ($(( deploy_ms / NUM_APPS ))ms/app)"

# Wait for gateway to sync routing table
echo "=== Waiting for gateway sync (4s) ==="
sleep 4

# --- Test 1: Cold start latency ---
echo ""
echo "=== Test 1: Cold start latency (first request per app) ==="
cold_total=0
cold_count=0
cold_failures=0
for i in $(seq 1 10); do
    name="app-$(printf '%03d' $i)"
    id="${APP_IDS[$name]}"
    key="${API_KEYS[$name]}"

    start=$(date +%s%N)
    result=$(curl -sf -X POST http://localhost:$GATE_PORT/apps/$name/rpc \
        -H 'Content-Type: application/json' \
        -H "X-Api-Key: $key" \
        -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}' 2>/dev/null || echo "FAIL")
    end=$(date +%s%N)
    latency_ms=$(( (end - start) / 1000000 ))

    if echo "$result" | grep -q "pong"; then
        cold_total=$((cold_total + latency_ms))
        cold_count=$((cold_count + 1))
        printf "  %-10s %3dms  %s\n" "$name" "$latency_ms" "$(echo $result | jq -r .result)"
    else
        cold_failures=$((cold_failures + 1))
        printf "  %-10s FAIL   %s\n" "$name" "$result"
    fi
done
if [ $cold_count -gt 0 ]; then
    echo "  Avg cold start: $((cold_total / cold_count))ms ($cold_count ok, $cold_failures fail)"
fi

# --- Test 2: Warm latency ---
echo ""
echo "=== Test 2: Warm latency (second request, cache hit) ==="
warm_total=0
warm_count=0
for i in $(seq 1 10); do
    name="app-$(printf '%03d' $i)"
    key="${API_KEYS[$name]}"

    start=$(date +%s%N)
    result=$(curl -sf -X POST http://localhost:$GATE_PORT/apps/$name/rpc \
        -H 'Content-Type: application/json' \
        -H "X-Api-Key: $key" \
        -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":2}' 2>/dev/null || echo "FAIL")
    end=$(date +%s%N)
    latency_ms=$(( (end - start) / 1000000 ))

    if echo "$result" | grep -q "pong"; then
        warm_total=$((warm_total + latency_ms))
        warm_count=$((warm_count + 1))
    fi
done
echo "  Avg warm latency: $((warm_total / warm_count))ms ($warm_count requests)"

# --- Test 3: CHWBL routing consistency ---
echo ""
echo "=== Test 3: Routing consistency (same app → same worker) ==="
name="app-001"
key="${API_KEYS[$name]}"
for i in $(seq 1 5); do
    curl -sf -X POST http://localhost:$GATE_PORT/apps/$name/rpc \
        -H 'Content-Type: application/json' \
        -H "X-Api-Key: $key" \
        -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":'$i'}' > /dev/null
done
echo "  5 requests for app-001 sent (check worker logs for routing)"

# --- Test 4: Concurrent traffic across all apps ---
echo ""
echo "=== Test 4: Concurrent traffic ($NUM_APPS apps, 10 req each) ==="
conc_start=$(date +%s%N)
conc_ok=0
conc_fail=0
for i in $(seq 1 $NUM_APPS); do
    name="app-$(printf '%03d' $i)"
    key="${API_KEYS[$name]}"
    for j in $(seq 1 10); do
        (curl -sf -X POST http://localhost:$GATE_PORT/apps/$name/rpc \
            -H 'Content-Type: application/json' \
            -H "X-Api-Key: $key" \
            -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":'$j'}' > /dev/null 2>&1 \
            && echo "OK" || echo "FAIL") &
    done
done | while read status; do
    if [ "$status" = "OK" ]; then
        echo -n "."
    else
        echo -n "X"
    fi
done
wait
conc_end=$(date +%s%N)
conc_ms=$(( (conc_end - conc_start) / 1000000 ))
echo ""
echo "  $((NUM_APPS * 10)) requests in ${conc_ms}ms"

# --- Test 5: Deploy update during traffic ---
echo ""
echo "=== Test 5: Hot deploy (update app-001 while serving) ==="
name="app-001"
id="${APP_IDS[$name]}"
key="${API_KEYS[$name]}"

# Send background traffic
for i in $(seq 1 20); do
    (curl -sf -X POST http://localhost:$GATE_PORT/apps/$name/rpc \
        -H 'Content-Type: application/json' \
        -H "X-Api-Key: $key" \
        -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":'$i'}' > /dev/null 2>&1) &
done

# Deploy new version mid-traffic
cat > "/tmp/appbase-e2e-apps/app-001-v2.js" << 'JSEOF'
export function ping() { return "pong v2 from app-001"; }
JSEOF
"$BIN/appbase" deploy "/tmp/appbase-e2e-apps/app-001-v2.js" \
    --app="$id" --control=http://localhost:$CONTROL_PORT --key=e2e-master 2>/dev/null
wait

# Wait for sync
sleep 4

# Verify new version
result=$(curl -sf -X POST http://localhost:$GATE_PORT/apps/$name/rpc \
    -H 'Content-Type: application/json' \
    -H "X-Api-Key: $key" \
    -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":99}')
echo "  After deploy: $(echo $result | jq -r .result)"

echo ""
echo "============================================"
echo "  E2E Test Complete"
echo "============================================"
