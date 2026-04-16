#!/usr/bin/env bash
# Cross-runtime benchmark: zeroship compio vs Node.js
#
# Uses `zerobench` — our in-house benchmark tool (wrk superset) that handles
# HTTP, SSE, WebSocket, NUMA pinning, and Lua scripting natively.
#
# Usage:
#   ./crates/runtime/benches/run_benchmark.sh
#   ./crates/runtime/benches/run_benchmark.sh --conns=500 --duration=30s
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

CONNS=300
THREADS=8
DURATION=10s
PORT_COMPIO_1=5100
PORT_COMPIO_N=5101
PORT_NODE=4002
PORT_NODE_CLUSTER=4003
PORT_ECHO=8888

for arg in "$@"; do
    case "$arg" in
        --conns=*) CONNS="${arg#*=}" ;;
        --threads=*) THREADS="${arg#*=}" ;;
        --duration=*) DURATION="${arg#*=}" ;;
    esac
done

# ---------------------------------------------------------------------------
# NUMA detection — zerobench handles pinning natively via --numa
# ---------------------------------------------------------------------------

NUMA_NODES=$(lscpu 2>/dev/null | grep "NUMA node(s)" | awk '{print $NF}' || echo "1")
SERVER_NUMA=""
CLIENT_NUMA=""
NUMA_INFO="Single NUMA node"

if [ "$NUMA_NODES" -ge 2 ]; then
    # Use numactl for server pinning (still needed — zerobench doesn't wrap servers)
    if command -v numactl >/dev/null 2>&1; then
        SERVER_NUMA="numactl --cpunodebind=0 --membind=0"
        CLIENT_NUMA="--numa 1"   # zerobench handles client pinning
        NODE0=$(lscpu | grep "NUMA node0" | awk '{print $NF}')
        NODE1=$(lscpu | grep "NUMA node1" | awk '{print $NF}')
        NUMA_INFO="servers on node 0 ($NODE0), zerobench on node 1 ($NODE1)"
    else
        NUMA_INFO="Multi-NUMA detected but numactl not installed (apt install numactl)"
    fi
fi

TOTAL_CORES=$(nproc)
SERVER_WORKERS=$(( TOTAL_CORES / 2 ))
[ "$SERVER_WORKERS" -lt 1 ] && SERVER_WORKERS=1

ZB="$ROOT_DIR/target/release/zerobench"

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

PIDS=()
cleanup() {
    for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
    wait 2>/dev/null || true
}
trap cleanup EXIT

echo "=== Building ===" >&2
(cd "$ROOT_DIR" && cargo build --release --bin v8-server-compio --bin echo-server --bin zerobench 2>&1 | tail -1) >&2

if [ ! -x "$ZB" ]; then
    echo "ERROR: zerobench not found at $ZB" >&2
    exit 1
fi

echo "=== Starting servers ===" >&2
for port in $PORT_COMPIO_1 $PORT_COMPIO_N $PORT_NODE $PORT_NODE_CLUSTER $PORT_ECHO; do
    lsof -ti :"$port" 2>/dev/null | xargs kill -9 2>/dev/null || true
done
sleep 1

$SERVER_NUMA "$ROOT_DIR/target/release/echo-server" $PORT_ECHO &
PIDS+=($!)

$SERVER_NUMA "$ROOT_DIR/target/release/v8-server-compio" --port=$PORT_COMPIO_1 --workers=1 &
PIDS+=($!)

$SERVER_NUMA "$ROOT_DIR/target/release/v8-server-compio" --port=$PORT_COMPIO_N --workers=$SERVER_WORKERS &
PIDS+=($!)

$SERVER_NUMA node "$SCRIPT_DIR/node_server.js" $PORT_NODE &
PIDS+=($!)

$SERVER_NUMA node "$SCRIPT_DIR/node_server_cluster.js" $PORT_NODE_CLUSTER $SERVER_WORKERS &
PIDS+=($!)

sleep 4

for port in $PORT_COMPIO_1 $PORT_COMPIO_N $PORT_NODE $PORT_NODE_CLUSTER; do
    curl -sf -X POST "http://localhost:$port/rpc" \
        -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}' > /dev/null 2>&1 || {
        echo "ERROR: port $port not responding" >&2; exit 1
    }
done
echo "All servers ready." >&2

# ---------------------------------------------------------------------------
# Lua scripts for zerobench (wrk-compatible)
# ---------------------------------------------------------------------------

make_lua() {
    local method=$1 params=$2
    local file="/tmp/zeroship-bench-${method}.lua"
    cat > "$file" << EOF
wrk.method = "POST"
wrk.body = '{"jsonrpc":"2.0","method":"${method}","params":${params},"id":1}'
wrk.headers["Content-Type"] = "application/json"
EOF
    echo "$file"
}

# ---------------------------------------------------------------------------
# Test runners — all use zerobench
# ---------------------------------------------------------------------------

# Parse "Requests/sec" and "Latency avg" from zerobench output.
parse_rps_lat() {
    local out="$1"
    local rps=$(echo "$out" | grep -E 'Requests/sec|req/s' | head -1 | awk '{print $2}')
    local lat=$(echo "$out" | grep -E '^\s*Latency' | head -1 | awk '{print $2, $3}')
    echo "$rps|$lat"
}

run_rpc() {
    local name=$1 port=$2 lua=$3
    local out
    out=$($ZB -t "$THREADS" -c "$CONNS" -d "$DURATION" $CLIENT_NUMA -s "$lua" "http://localhost:$port/rpc" 2>&1)
    local parsed=$(parse_rps_lat "$out")
    local rps="${parsed%%|*}"
    local lat="${parsed##*|}"
    printf "  %-42s  %12s req/s  %s\n" "$name" "${rps:-ERR}" "${lat:-}"
}

scenario() {
    local label=$1 method=$2 params=$3
    local lua=$(make_lua "$method" "$params")
    echo ""
    echo "--- $label ---"
    run_rpc "compio (1 worker)"                 $PORT_COMPIO_1       "$lua"
    run_rpc "compio ($SERVER_WORKERS workers)"  $PORT_COMPIO_N       "$lua"
    run_rpc "Node.js $(node --version)"         $PORT_NODE           "$lua"
    run_rpc "Node.js cluster ($SERVER_WORKERS)" $PORT_NODE_CLUSTER   "$lua"
}

# SSE — uses zerobench's native --sse mode (concurrent connections, accurate chunk timing).
run_sse() {
    local label=$1 port=$2 chunks=$3 delay=$4 conns=${5:-$CONNS} duration=${6:-5s}
    local size=50
    local url="http://localhost:$port/sse?chunks=$chunks&delay=$delay&size=$size"

    local out
    out=$($ZB --sse -t "$THREADS" -c "$conns" -d "$duration" $CLIENT_NUMA "$url" 2>&1)

    local chunks_per_sec=$(echo "$out" | grep -E 'Chunks/sec' | head -1 | awk '{print $2}')
    local transfer=$(echo "$out" | grep -E 'Transfer/sec' | head -1 | awk '{print $2}')
    local ttfb_p50=$(echo "$out" | grep -A 1 'TTFB' | tail -1 | awk '{print $2}')

    printf "  %-42s  %12s chunks/s  %10s  TTFB p50 %s\n" \
        "$label" "${chunks_per_sec:-ERR}" "${transfer:-}" "${ttfb_p50:-}"
}

# ---------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------

echo ""
echo "================================================================="
echo "  zeroship Runtime Benchmark (zerobench)"
echo "  $(date -u +%Y-%m-%d) | $(nproc) logical cores | $(uname -m)"
echo "  $NUMA_INFO"
echo "  zerobench: $THREADS threads, $CONNS connections, $DURATION per test"
echo "  servers: $SERVER_WORKERS workers"
echo "================================================================="

# --- RPC throughput ---

scenario "1. ping/pong (minimal)"              ping                 "[]"
scenario "2. fib(10) (light CPU)"              fib                  "[10]"
scenario "3. setTimeout(0)"                    timeout0             "[]"
scenario "4. Promise chain (sync .then)"       promiseChain         "[]"
scenario "5. Promise chain + 100ms timer"      promiseChainTimeout  "[]"
scenario "6. fetch() → local echo"             fetchExternal        "[\"http://localhost:$PORT_ECHO\"]"

echo ""
echo "--- Crypto ---"
scenario "7. randomUUID()"                     uuid                 "[]"
scenario "8. SHA-256 digest"                   sha256               "[]"
scenario "9. HMAC-SHA256 sign (cached)"        hmacSign             "[]"
scenario "10. AES-GCM encrypt (cached)"        aesEncrypt           "[]"
scenario "11. ECDSA P-256 sign (cached)"       ecdsaSign            "[]"

# --- SSE streaming ---

echo ""
echo "--- SSE Streaming (throughput) ---"
echo ""

run_sse "compio (1 worker) 100 chunks"                 $PORT_COMPIO_1  100  0
run_sse "compio ($SERVER_WORKERS workers) 100 chunks"  $PORT_COMPIO_N  100  0
run_sse "Node.js 100 chunks"                           $PORT_NODE      100  0

echo ""

run_sse "compio (1 worker) 1000 chunks"                 $PORT_COMPIO_1  1000 0
run_sse "compio ($SERVER_WORKERS workers) 1000 chunks"  $PORT_COMPIO_N  1000 0
run_sse "Node.js 1000 chunks"                           $PORT_NODE      1000 0

echo ""
echo "--- SSE Streaming (delayed — real-time scenarios) ---"
echo ""

run_sse "compio ($SERVER_WORKERS workers) 100×1ms"   $PORT_COMPIO_N  100  1   50
run_sse "compio ($SERVER_WORKERS workers) 100×10ms"  $PORT_COMPIO_N  100  10  50
run_sse "Node.js 100×1ms"                            $PORT_NODE      100  1   50
run_sse "Node.js 100×10ms"                           $PORT_NODE      100  10  50

echo ""
echo "--- SSE Scaling (connections × chunks = 100) ---"
echo ""

run_sse "50 connections"   $PORT_COMPIO_N  100  0  50
run_sse "100 connections"  $PORT_COMPIO_N  100  0  100
run_sse "200 connections"  $PORT_COMPIO_N  100  0  200
run_sse "500 connections"  $PORT_COMPIO_N  100  0  500

echo ""
echo "================================================================="
