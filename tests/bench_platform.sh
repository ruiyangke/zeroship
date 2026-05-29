#!/usr/bin/env bash
# Platform benchmark — measures throughput and latency across all layers.
#
# Prerequisites:
#   - cargo build --release (all binaries)
#   - docker start pg-test (Postgres on port 5434)
#   - wrk installed
#
# Usage:
#   ./tests/bench_platform.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"
CORES=$(nproc)

PIDS=()
cleanup() {
    for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
    wait 2>/dev/null || true
    rm -rf /tmp/zeroship-bench-*
}
trap cleanup EXIT

echo "================================================================="
echo "  zeroship Platform Benchmark"
echo "  $(date -u +%Y-%m-%d) | $CORES cores | $(uname -m)"
echo "================================================================="
echo ""

# --- Setup ---
for port in 9090 8080 8000 5100 5101; do
    lsof -ti :"$port" 2>/dev/null | xargs kill -9 2>/dev/null || true
done
rm -rf /tmp/zeroship-bench-bundles
docker exec pg-test psql -U postgres -c "DROP TABLE IF EXISTS usage_history, usage, apps CASCADE" > /dev/null 2>&1

# Start platform
"$BIN/zeroship-control" --port 9090 --db "postgres://postgres:test@localhost:5434/postgres" \
    --bundles /tmp/zeroship-bench-bundles --control-key bk --master-key bm > /dev/null 2>&1 &
PIDS+=($!)
sleep 3

"$BIN/zeroship-worker" --port 8080 --worker-threads $CORES --control http://localhost:9090 \
    --control-key bk --poll-interval 60 > /dev/null 2>&1 &
PIDS+=($!)
sleep 2

"$BIN/zeroship-gate" --port 8000 --control http://localhost:9090 \
    --control-key bk --workers http://localhost:8080 --poll-interval 60 > /dev/null 2>&1 &
PIDS+=($!)
sleep 3

# Create + deploy
APP=$(curl -sf -X POST http://localhost:9090/api/apps \
    -H 'Content-Type: application/json' \
    -H 'Authorization: Bearer bm' \
    -d '{"name":"bench"}')
APP_ID=$(echo "$APP" | jq -r '.id')
API_KEY=$(echo "$APP" | jq -r '.api_key')

mkdir -p /tmp/zeroship-bench-app
echo 'export function ping() { return "pong"; }' > /tmp/zeroship-bench-app/index.js
"$BIN/zeroship" deploy /tmp/zeroship-bench-app/index.js --app="$APP_ID" \
    --control=http://localhost:9090 --key=bm > /dev/null 2>&1

# Baseline
"$BIN/zeroship-bench-server" --port=5100 --workers=1 > /dev/null 2>&1 &
PIDS+=($!)
"$BIN/zeroship-bench-server" --port=5101 --workers=$CORES > /dev/null 2>&1 &
PIDS+=($!)
sleep 4

# Warmup (retry until gateway syncs)
for i in $(seq 1 15); do
    if curl -sf -X POST http://localhost:8000/apps/bench/rpc \
        -H 'Content-Type: application/json' \
        -H "X-Api-Key: $API_KEY" \
        -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}' > /dev/null 2>&1; then
        break
    fi
    sleep 1
done

# wrk scripts
LUA_RPC=$(mktemp)
cat > "$LUA_RPC" << EOF
wrk.method = "POST"
wrk.body = '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}'
wrk.headers["Content-Type"] = "application/json"
EOF

LUA_GATE=$(mktemp)
cat > "$LUA_GATE" << EOF
wrk.method = "POST"
wrk.body = '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}'
wrk.headers["Content-Type"] = "application/json"
wrk.headers["X-Api-Key"] = "$API_KEY"
EOF

run_bench() {
    local label=$1 url=$2 lua=$3
    local result
    result=$(wrk -t4 -c50 -d10s -s "$lua" "$url" 2>&1)
    local rps=$(echo "$result" | grep 'Requests/sec' | awk '{print $2}')
    local lat=$(echo "$result" | grep 'Latency' | awk '{print $2}')
    printf "  %-45s %12s req/s  %8s avg\n" "$label" "$rps" "$lat"
}

# --- Benchmark ---
echo "Throughput & Latency (wrk 4t/50c/10s):"
echo ""
echo "  Layer                                         Throughput       Latency"
echo "  -------------------------------------------  ------------  ----------"
run_bench "Raw runtime (1 worker)" "http://localhost:5100/rpc" "$LUA_RPC"
run_bench "Raw runtime ($CORES workers)" "http://localhost:5101/rpc" "$LUA_RPC"
run_bench "Worker direct ($CORES threads)" "http://localhost:8080/dispatch/$APP_ID" "$LUA_RPC"
run_bench "Full pipeline (gate→worker→V8)" "http://localhost:8000/apps/bench/rpc" "$LUA_GATE"
run_bench "Gateway health (no V8)" "http://localhost:8000/health" "$LUA_RPC"

echo ""
echo "  Detailed latency (full pipeline):"
wrk -t4 -c50 -d10s -s "$LUA_GATE" http://localhost:8000/apps/bench/rpc 2>&1 | grep "Latency"

rm "$LUA_RPC" "$LUA_GATE"
echo ""
echo "================================================================="
