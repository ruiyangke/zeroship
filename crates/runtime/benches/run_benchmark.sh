#!/usr/bin/env bash
# Cross-runtime benchmark: zeroship compio vs Node.js
#
# NUMA-aware: servers pinned to NUMA node 0, wrk client to NUMA node 1.
# This avoids cross-socket memory access for accurate measurements.
#
# Usage:
#   ./crates/runtime/benches/run_benchmark.sh
#   ./crates/runtime/benches/run_benchmark.sh --conns 500    # override connections
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

CONNS=300                   # connections (tunable via --conns)
THREADS=8                   # wrk threads
DURATION=10s
PORT_COMPIO_1=5100
PORT_COMPIO_N=5101
PORT_NODE=4002
PORT_NODE_CLUSTER=4003
PORT_ECHO=8888

# Parse CLI args
for arg in "$@"; do
    case "$arg" in
        --conns=*) CONNS="${arg#*=}" ;;
        --threads=*) THREADS="${arg#*=}" ;;
        --duration=*) DURATION="${arg#*=}" ;;
    esac
done

# ---------------------------------------------------------------------------
# NUMA detection
# ---------------------------------------------------------------------------

NUMA_NODES=$(lscpu 2>/dev/null | grep "NUMA node(s)" | awk '{print $NF}' || echo "1")
HAS_NUMACTL=$(command -v numactl >/dev/null 2>&1 && echo "1" || echo "0")
if [ "$NUMA_NODES" -ge 2 ] && [ "$HAS_NUMACTL" = "1" ]; then
    # 2+ NUMA nodes + numactl available: pin servers to node 0, wrk to node 1
    NUMA_SERVER="numactl --cpunodebind=0 --membind=0"
    NUMA_CLIENT="numactl --cpunodebind=1 --membind=1"
    NUMA_NODE0_CPUS=$(lscpu | grep "NUMA node0" | awk '{print $NF}')
    NUMA_NODE1_CPUS=$(lscpu | grep "NUMA node1" | awk '{print $NF}')
    NUMA_INFO="NUMA split: servers on node 0 ($NUMA_NODE0_CPUS), wrk on node 1 ($NUMA_NODE1_CPUS)"
else
    NUMA_SERVER=""
    NUMA_CLIENT=""
    if [ "$NUMA_NODES" -ge 2 ]; then
        NUMA_INFO="Multi-NUMA detected but numactl not installed (apt install numactl)"
    else
        NUMA_INFO="Single NUMA node"
    fi
fi

# Count physical cores per NUMA node for server workers
TOTAL_CORES=$(nproc)
SERVER_WORKERS=$(( TOTAL_CORES / 2 ))  # half for servers, half for wrk
[ "$SERVER_WORKERS" -lt 1 ] && SERVER_WORKERS=1

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
(cd "$ROOT_DIR" && cargo build --release --bin v8-server-compio --bin echo-server 2>&1 | tail -1) >&2

echo "=== Starting servers (NUMA node 0) ===" >&2
for port in $PORT_COMPIO_1 $PORT_COMPIO_N $PORT_NODE $PORT_NODE_CLUSTER $PORT_ECHO; do
    lsof -ti :"$port" 2>/dev/null | xargs kill -9 2>/dev/null || true
done
sleep 1

$NUMA_SERVER "$ROOT_DIR/target/release/echo-server" $PORT_ECHO &
PIDS+=($!)

$NUMA_SERVER "$ROOT_DIR/target/release/v8-server-compio" --port=$PORT_COMPIO_1 --workers=1 &
PIDS+=($!)

$NUMA_SERVER "$ROOT_DIR/target/release/v8-server-compio" --port=$PORT_COMPIO_N --workers=$SERVER_WORKERS &
PIDS+=($!)

$NUMA_SERVER node "$SCRIPT_DIR/node_server.js" $PORT_NODE &
PIDS+=($!)

$NUMA_SERVER node "$SCRIPT_DIR/node_server_cluster.js" $PORT_NODE_CLUSTER $SERVER_WORKERS &
PIDS+=($!)

sleep 4

# Verify
for port in $PORT_COMPIO_1 $PORT_COMPIO_N $PORT_NODE $PORT_NODE_CLUSTER; do
    curl -sf -X POST "http://localhost:$port/rpc" \
        -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}' > /dev/null 2>&1 || {
        echo "ERROR: port $port not responding" >&2; exit 1
    }
done
echo "All servers ready." >&2

# ---------------------------------------------------------------------------
# Helpers
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

run_test() {
    local name=$1 port=$2 lua=$3
    local result
    result=$($NUMA_CLIENT wrk -t$THREADS -c$CONNS -d"$DURATION" -s "$lua" "http://localhost:$port/rpc" 2>&1)
    local rps=$(echo "$result" | grep 'Requests/sec' | awk '{print $2}')
    local lat=$(echo "$result" | grep 'Latency' | awk '{printf "%s (%s)", $2, $3}')
    printf "  %-42s  %12s req/s  %s\n" "$name" "$rps" "$lat"
}

scenario() {
    local label=$1 method=$2 params=$3
    local lua=$(make_lua "$method" "$params")
    echo ""
    echo "--- $label ---"
    run_test "compio (1 worker)" $PORT_COMPIO_1 "$lua"
    run_test "compio ($SERVER_WORKERS workers)" $PORT_COMPIO_N "$lua"
    run_test "Node.js $(node --version)" $PORT_NODE "$lua"
    run_test "Node.js cluster ($SERVER_WORKERS)" $PORT_NODE_CLUSTER "$lua"
}

run_sse() {
    local label=$1 port=$2 chunks=$3 delay=$4 size=${5:-50}
    local url="http://localhost:$port/sse?chunks=$chunks&delay=$delay&size=$size"

    local start=$(date +%s%N)
    local first_byte=""
    local count=0

    while IFS= read -r line; do
        if [ -z "$first_byte" ]; then
            first_byte=$(date +%s%N)
        fi
        if [[ "$line" == data:* ]]; then
            count=$((count + 1))
        fi
        if [[ "$line" == "data: [DONE]" ]]; then
            break
        fi
    done < <($NUMA_CLIENT curl -sN --max-time 30 "$url" 2>/dev/null)

    local end=$(date +%s%N)
    local total_ms=$(( (end - start) / 1000000 ))
    local ttfb_ms=0
    if [ -n "$first_byte" ]; then
        ttfb_ms=$(( (first_byte - start) / 1000000 ))
    fi
    local tps=0
    if [ "$total_ms" -gt 0 ] && [ "$count" -gt 0 ]; then
        tps=$(( count * 1000 / total_ms ))
    fi

    printf "  %-42s  TTFB %4dms  %5d chunks in %5dms  %6d chunks/s\n" \
        "$label" "$ttfb_ms" "$count" "$total_ms" "$tps"
}

# ---------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------

echo ""
echo "================================================================="
echo "  zeroship Runtime Benchmark"
echo "  $(date -u +%Y-%m-%d) | $(nproc) logical cores | $(uname -m)"
echo "  $NUMA_INFO"
echo "  wrk: $THREADS threads, $CONNS connections, $DURATION per test"
echo "  servers: $SERVER_WORKERS workers"
echo "================================================================="

# --- RPC throughput ---

scenario "1. ping/pong (minimal)" ping "[]"
scenario "2. fib(10) (light CPU)" fib "[10]"
scenario "3. setTimeout(0)" timeout0 "[]"
scenario "4. Promise chain (sync .then)" promiseChain "[]"
scenario "5. Promise chain + 100ms timer" promiseChainTimeout "[]"
scenario "6. fetch() → local echo" fetchExternal "[\"http://localhost:$PORT_ECHO\"]"

echo ""
echo "--- Crypto ---"
scenario "7. randomUUID()" uuid "[]"
scenario "8. SHA-256 digest" sha256 "[]"
scenario "9. HMAC-SHA256 sign (cached)" hmacSign "[]"
scenario "10. AES-GCM encrypt (cached)" aesEncrypt "[]"
scenario "11. ECDSA P-256 sign (cached)" ecdsaSign "[]"

# --- SSE streaming ---

echo ""
echo "--- SSE Streaming ---"
echo ""

run_sse "compio (1 worker) 100×0ms" $PORT_COMPIO_1 100 0
run_sse "compio ($SERVER_WORKERS workers) 100×0ms" $PORT_COMPIO_N 100 0
run_sse "Node.js 100×0ms" $PORT_NODE 100 0
