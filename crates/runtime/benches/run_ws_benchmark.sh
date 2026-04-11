#!/usr/bin/env bash
# WebSocket benchmark: appbase compio vs Node.js
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"

PORT_COMPIO_1=5100
PORT_COMPIO_N=5101
PORT_NODE=4010
CLIENTS=50
DURATION=10

PIDS=()
cleanup() { for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done; wait 2>/dev/null || true; }
trap cleanup EXIT

echo "=== Building ===" >&2
(cd "$ROOT_DIR" && cargo build --release --bin v8-server-compio 2>&1 | tail -1) >&2

echo "=== Starting servers ===" >&2
for port in $PORT_COMPIO_1 $PORT_COMPIO_N $PORT_NODE; do
    lsof -ti :"$port" 2>/dev/null | xargs kill -9 2>/dev/null || true
done
sleep 1

"$ROOT_DIR/target/release/v8-server-compio" --port=$PORT_COMPIO_1 --workers=1 &
PIDS+=($!)

"$ROOT_DIR/target/release/v8-server-compio" --port=$PORT_COMPIO_N --workers=$(nproc) &
PIDS+=($!)

node "$SCRIPT_DIR/node_ws_server.js" $PORT_NODE &
PIDS+=($!)

sleep 4
echo "All servers ready." >&2

run_ws() {
    local label=$1 url=$2 clients=$3
    local result
    result=$(node "$SCRIPT_DIR/ws_benchmark.js" "$url" --clients="$clients" --duration="$DURATION" --payload=64 2>&1)
    local msgs=$(echo "$result" | grep 'Messages/sec' | awk '{print $2}')
    local avg=$(echo "$result" | grep 'Avg latency' | awk '{print $3, $4}')
    local p99=$(echo "$result" | grep 'p99 latency' | awk '{print $3, $4}')
    printf "  %-40s  %12s msg/s  avg=%s  p99=%s\n" "$label" "$msgs" "$avg" "$p99"
}

echo ""
echo "WebSocket Benchmark (echo, 64-byte payload)"
echo "Date: $(date -u +%Y-%m-%d)"
echo "Machine: $(uname -s) ($(nproc) cores)"
echo "Clients: $CLIENTS, Duration: ${DURATION}s"
echo ""
echo "========================================================="

echo ""
echo "--- 50 clients ---"
run_ws "appbase compio (1 worker)" "ws://localhost:$PORT_COMPIO_1" 50
run_ws "appbase compio ($(nproc) workers)" "ws://localhost:$PORT_COMPIO_N" 50
run_ws "Node.js $(node --version)" "ws://localhost:$PORT_NODE" 50

echo ""
echo "--- 100 clients ---"
run_ws "appbase compio (1 worker)" "ws://localhost:$PORT_COMPIO_1" 100
run_ws "appbase compio ($(nproc) workers)" "ws://localhost:$PORT_COMPIO_N" 100
run_ws "Node.js $(node --version)" "ws://localhost:$PORT_NODE" 100

echo ""
echo "========================================================="
