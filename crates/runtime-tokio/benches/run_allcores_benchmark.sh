#!/usr/bin/env bash
# All-cores benchmark comparison: compio, tokio, Node.js
# Usage: ./run_allcores_benchmark.sh [scenario] [duration] [connections]
#
# Example:
#   ./run_allcores_benchmark.sh ping 10s 64
#   ./run_allcores_benchmark.sh compute_hash 10s 32

set -euo pipefail

SCENARIO="${1:-ping}"
DURATION="${2:-10s}"
CONNECTIONS="${3:-64}"
THREADS="${4:-4}"
NCPU="$(nproc)"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

# Build release binaries
echo "=== Building release binaries ==="
cargo build --release --bin v8-server-compio --bin v8-server 2>&1 | tail -3

COMPIO_BIN="$PROJECT_ROOT/target/release/v8-server-compio"
TOKIO_BIN="$PROJECT_ROOT/target/release/v8-server"
NODE_SINGLE="$SCRIPT_DIR/node_server.js"
NODE_CLUSTER="$SCRIPT_DIR/node_server_cluster.js"

# RPC body for the scenario
RPC_BODY='{"jsonrpc":"2.0","method":"'"$SCENARIO"'","params":[],"id":1}'

# Port assignments
PORT_COMPIO_1=5100
PORT_COMPIO_N=5101
PORT_TOKIO_1=5102
PORT_TOKIO_N=5103
PORT_NODE_1=5104
PORT_NODE_N=5105

PIDS=()

cleanup() {
    echo ""
    echo "=== Cleaning up ==="
    for pid in "${PIDS[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
}
trap cleanup EXIT

wait_for_port() {
    local port=$1
    local name=$2
    for i in $(seq 1 30); do
        if curl -sf "http://127.0.0.1:$port/health" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.2
    done
    echo "WARN: $name on port $port did not become ready"
    return 1
}

# Start all servers
echo ""
echo "=== Starting servers ($(date)) ==="
echo "  CPU cores: $NCPU"
echo "  Scenario:  $SCENARIO"
echo "  Duration:  $DURATION"
echo "  Connections: $CONNECTIONS"
echo ""

# 1. compio single worker
$COMPIO_BIN --port=$PORT_COMPIO_1 --workers=1 &
PIDS+=($!)

# 2. compio all cores
$COMPIO_BIN --port=$PORT_COMPIO_N --workers=$NCPU &
PIDS+=($!)

# 3. tokio single V8 thread
$TOKIO_BIN --port=$PORT_TOKIO_1 --mode=concurrent &
PIDS+=($!)

# 4. tokio pool (all cores)
$TOKIO_BIN --port=$PORT_TOKIO_N --mode=concurrent-pool &
PIDS+=($!)

# 5. Node.js single
node "$NODE_SINGLE" $PORT_NODE_1 &
PIDS+=($!)

# 6. Node.js cluster (all cores)
node "$NODE_CLUSTER" $PORT_NODE_N $NCPU &
PIDS+=($!)

# Wait for all servers to be ready
echo "Waiting for servers..."
wait_for_port $PORT_COMPIO_1 "compio-1"
wait_for_port $PORT_COMPIO_N "compio-$NCPU"
wait_for_port $PORT_TOKIO_1 "tokio-1"
wait_for_port $PORT_TOKIO_N "tokio-pool"
wait_for_port $PORT_NODE_1 "node-1"
wait_for_port $PORT_NODE_N "node-cluster"
echo "All servers ready."
echo ""

# Warmup: one request to each
for port in $PORT_COMPIO_1 $PORT_COMPIO_N $PORT_TOKIO_1 $PORT_TOKIO_N $PORT_NODE_1 $PORT_NODE_N; do
    curl -sf -X POST "http://127.0.0.1:$port/rpc" \
        -H "Content-Type: application/json" \
        -d "$RPC_BODY" >/dev/null 2>&1 || true
done

# wrk lua script for POST requests
WRK_SCRIPT=$(mktemp /tmp/wrk_rpc_XXXXXX.lua)
cat > "$WRK_SCRIPT" <<'WRKEOF'
wrk.method = "POST"
wrk.headers["Content-Type"] = "application/json"
WRKEOF

echo "=== Benchmark: $SCENARIO (duration=$DURATION, connections=$CONNECTIONS, threads=$THREADS) ==="
echo ""
printf "%-20s %12s %12s %12s\n" "SERVER" "Req/sec" "Avg Lat" "p99 Lat"
printf "%-20s %12s %12s %12s\n" "------" "-------" "-------" "-------"

run_wrk() {
    local name=$1
    local port=$2

    local output
    output=$(wrk -t"$THREADS" -c"$CONNECTIONS" -d"$DURATION" \
        -s "$WRK_SCRIPT" \
        --latency \
        "http://127.0.0.1:$port/rpc" \
        -- "$RPC_BODY" 2>&1)

    # Parse results
    local rps avg_lat p99_lat
    rps=$(echo "$output" | grep "Requests/sec:" | awk '{print $2}')
    avg_lat=$(echo "$output" | grep "Latency" | head -1 | awk '{print $2}')
    p99_lat=$(echo "$output" | grep "99%" | awk '{print $2}')

    printf "%-20s %12s %12s %12s\n" "$name" "$rps" "$avg_lat" "$p99_lat"
}

# Note: wrk needs the body passed differently. Let's use a proper lua script.
cat > "$WRK_SCRIPT" <<WRKEOF
wrk.method = "POST"
wrk.headers["Content-Type"] = "application/json"
wrk.body = '$RPC_BODY'
WRKEOF

run_wrk "compio-1"         $PORT_COMPIO_1
run_wrk "compio-${NCPU}"   $PORT_COMPIO_N
run_wrk "tokio-1"          $PORT_TOKIO_1
run_wrk "tokio-pool"       $PORT_TOKIO_N
run_wrk "node-1"           $PORT_NODE_1
run_wrk "node-cluster"     $PORT_NODE_N

echo ""
echo "=== Done ==="

rm -f "$WRK_SCRIPT"
