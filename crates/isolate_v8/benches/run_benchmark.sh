#!/usr/bin/env bash
# Cross-runtime RPC benchmark — appbase raw V8 vs Node.js baseline.
#
# Usage:
#   ./run_benchmark.sh                  # run all, write results to stdout
#   ./run_benchmark.sh > results.txt    # capture to file
#
# Prerequisites: wrk, node, cargo (release build of v8-server)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CRATE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
ROOT_DIR="$(cd "$CRATE_DIR/../.." && pwd)"

CONNS=50
THREADS=4
DURATION=10s

# Ports
PORT_CONCURRENT=4000
PORT_POOL=4001
PORT_NODE=4002

PIDS=()

cleanup() {
    for pid in "${PIDS[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------
echo "Building v8-server (release)..." >&2
(cd "$ROOT_DIR" && cargo build --release --bin v8-server 2>&1 | tail -1) >&2

# ---------------------------------------------------------------------------
# Start servers
# ---------------------------------------------------------------------------
echo "Starting servers..." >&2

# Kill any existing servers on our ports
for port in $PORT_CONCURRENT $PORT_POOL $PORT_NODE; do
    lsof -ti :"$port" 2>/dev/null | xargs kill -9 2>/dev/null || true
done
sleep 1

"$ROOT_DIR/target/release/v8-server" --mode=concurrent --port=$PORT_CONCURRENT &
PIDS+=($!)

"$ROOT_DIR/target/release/v8-server" --mode=concurrent-pool --port=$PORT_POOL &
PIDS+=($!)

node "$SCRIPT_DIR/node_server.js" $PORT_NODE &
PIDS+=($!)

sleep 2

# Verify all servers are up
for port in $PORT_CONCURRENT $PORT_POOL $PORT_NODE; do
    if ! curl -sf -X POST "http://localhost:$port/rpc" \
        -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}' > /dev/null 2>&1; then
        echo "ERROR: server on port $port not responding" >&2
        exit 1
    fi
done
echo "All servers ready." >&2

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
make_lua() {
    local method=$1 params=$2
    local file="/tmp/appbase-bench-${method}.lua"
    cat > "$file" << EOF
wrk.method = "POST"
wrk.body = '{"jsonrpc":"2.0","method":"${method}","params":${params},"id":1}'
wrk.headers["Content-Type"] = "application/json"
EOF
    echo "$file"
}

run_test() {
    local name=$1 port=$2 lua=$3 duration=${4:-$DURATION}
    curl -sf -X POST "http://localhost:$port/rpc" \
        -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}' > /dev/null 2>&1 || {
        printf "  %-35s  SKIPPED\n" "$name"
        return
    }
    local result
    result=$(wrk -t$THREADS -c$CONNS -d"$duration" -s "$lua" "http://localhost:$port/rpc" 2>&1)
    local rps lat avg_lat
    rps=$(echo "$result" | grep 'Requests/sec' | awk '{print $2}')
    lat=$(echo "$result" | grep 'Latency' | awk '{printf "%s %s", $2, $3}')
    printf "  %-35s  %12s req/s   %s\n" "$name" "$rps" "$lat"
}

scenario() {
    local label=$1 method=$2 params=$3 dur=${4:-$DURATION}
    local lua
    lua=$(make_lua "$method" "$params")

    echo ""
    echo "--- $label ---"
    run_test "raw V8 concurrent (1 thread)"    $PORT_CONCURRENT  "$lua" "$dur"
    run_test "raw V8 conc-pool ($(nproc) threads)" $PORT_POOL    "$lua" "$dur"
    run_test "Node.js $(node --version)"       $PORT_NODE        "$lua" "$dur"
}

# ---------------------------------------------------------------------------
# Run benchmarks
# ---------------------------------------------------------------------------
echo "Cross-Runtime RPC Benchmark"
echo "Date: $(date -u +%Y-%m-%d)"
echo "Machine: $(uname -s) ($(nproc) cores)"
echo "Tool: wrk, ${THREADS} threads, ${CONNS} connections, ${DURATION} per test"
echo ""
echo "Servers:"
echo "  raw V8 concurrent:   1 V8 thread, serial JS + concurrent I/O"
echo "  raw V8 conc-pool:    $(nproc) V8 threads, each with concurrent I/O"
echo "  Node.js:             $(node --version), http module, single thread"
echo ""
echo "========================================================="

scenario "1. ping/pong (minimal)"           ping              "[]"
scenario "2. fib(10) (light CPU)"           fib               "[10]"
scenario "3. fib(35) (heavy CPU)"           fib               "[35]"  5s
scenario "4. setTimeout(0)"                 timeout0          "[]"
scenario "5. Promise chain (sync .then)"    promiseChain      "[]"
scenario "6. Promise chain + 100ms timer"   promiseChainTimeout "[]"
scenario "7. fetch() → external API"       fetchExternal       "[\"https://httpbin.org/get\"]"

echo ""
echo "========================================================="
echo ""
echo "Notes:"
echo "  raw V8 concurrent:   single V8 thread with serial JS + concurrent I/O."
echo "                       Requests overlap during I/O waits."
echo "  raw V8 conc-pool:    N concurrent isolate threads with round-robin dispatch."
echo "                       Best of both: concurrent I/O + multi-core scaling."
echo "  Node.js:             single-threaded event loop via http module."
