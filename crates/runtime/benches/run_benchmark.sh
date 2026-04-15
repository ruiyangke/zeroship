#!/usr/bin/env bash
# Cross-runtime benchmark: zeroship compio vs Node.js
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"

CONNS=50
THREADS=4
DURATION=10s
PORT_COMPIO_1=5100
PORT_COMPIO_16=5101
PORT_NODE=4002
PORT_NODE_CLUSTER=4003
PORT_ECHO=8888

PIDS=()
cleanup() { for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done; wait 2>/dev/null || true; }
trap cleanup EXIT

echo "=== Building ===" >&2
(cd "$ROOT_DIR" && cargo build --release --bin v8-server-compio --bin echo-server 2>&1 | tail -1) >&2

echo "=== Starting servers ===" >&2
for port in $PORT_COMPIO_1 $PORT_COMPIO_16 $PORT_NODE $PORT_NODE_CLUSTER $PORT_ECHO; do
    lsof -ti :"$port" 2>/dev/null | xargs kill -9 2>/dev/null || true
done
sleep 1

"$ROOT_DIR/target/release/echo-server" $PORT_ECHO &
PIDS+=($!)

"$ROOT_DIR/target/release/v8-server-compio" --port=$PORT_COMPIO_1 --workers=1 &
PIDS+=($!)

"$ROOT_DIR/target/release/v8-server-compio" --port=$PORT_COMPIO_16 --workers=$(nproc) &
PIDS+=($!)

node "$SCRIPT_DIR/node_server.js" $PORT_NODE &
PIDS+=($!)

node "$SCRIPT_DIR/node_server_cluster.js" $PORT_NODE_CLUSTER $(nproc) &
PIDS+=($!)

sleep 4

# Verify
for port in $PORT_COMPIO_1 $PORT_COMPIO_16 $PORT_NODE $PORT_NODE_CLUSTER; do
    curl -sf -X POST "http://localhost:$port/rpc" \
        -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}' > /dev/null 2>&1 || {
        echo "ERROR: port $port not responding" >&2; exit 1
    }
done
echo "All servers ready." >&2

# Helpers
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
    result=$(wrk -t$THREADS -c$CONNS -d"$DURATION" -s "$lua" "http://localhost:$port/rpc" 2>&1)
    local rps=$(echo "$result" | grep 'Requests/sec' | awk '{print $2}')
    local lat=$(echo "$result" | grep 'Latency' | awk '{printf "%s (%s)", $2, $3}')
    printf "  %-40s  %12s req/s  %s\n" "$name" "$rps" "$lat"
}

scenario() {
    local label=$1 method=$2 params=$3
    local lua=$(make_lua "$method" "$params")
    echo ""
    echo "--- $label ---"
    run_test "compio (1 worker)" $PORT_COMPIO_1 "$lua"
    run_test "compio ($(nproc) workers)" $PORT_COMPIO_16 "$lua"
    run_test "Node.js $(node --version)" $PORT_NODE "$lua"
    run_test "Node.js cluster ($(nproc))" $PORT_NODE_CLUSTER "$lua"
}

# Output
echo "zeroship Runtime Benchmark"
echo "Date: $(date -u +%Y-%m-%d)"
echo "Machine: $(uname -s) ($(nproc) cores)"
echo "Tool: wrk, ${THREADS} threads, ${CONNS} connections, ${DURATION} per test"
echo ""
echo "========================================================="

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

echo ""
echo "--- SSE Streaming ---"
echo ""

# SSE benchmarks use curl (wrk doesn't support streaming responses).
# We measure: time-to-first-byte (TTFB), total stream time, and throughput.
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
    done < <(curl -sN "$url" 2>/dev/null)

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

    printf "  %-40s  TTFB %4dms  %5d chunks in %5dms  %6d chunks/s\n" \
        "$label" "$ttfb_ms" "$count" "$total_ms" "$tps"
}

# SSE: 100 chunks, no delay (max throughput)
run_sse "compio (1 worker) 100×0ms" $PORT_COMPIO_1 100 0
run_sse "compio ($(nproc) workers) 100×0ms" $PORT_COMPIO_16 100 0
run_sse "Node.js 100×0ms" $PORT_NODE 100 0

echo ""

# SSE: 1000 chunks, no delay (sustained throughput)
run_sse "compio (1 worker) 1000×0ms" $PORT_COMPIO_1 1000 0
run_sse "compio ($(nproc) workers) 1000×0ms" $PORT_COMPIO_16 1000 0
run_sse "Node.js 1000×0ms" $PORT_NODE 1000 0

echo ""

# SSE: 100 chunks, 1ms delay (simulates fast LLM ~1000 tok/s)
run_sse "compio (1 worker) 100×1ms" $PORT_COMPIO_1 100 1
run_sse "Node.js 100×1ms" $PORT_NODE 100 1

echo ""

# SSE: 50 chunks, 10ms delay (simulates slower LLM ~100 tok/s)
run_sse "compio (1 worker) 50×10ms" $PORT_COMPIO_1 50 10
run_sse "Node.js 50×10ms" $PORT_NODE 50 10

echo ""

# SSE: concurrent streams
echo "  Concurrent SSE (10 streams × 100 chunks × 0ms):"
start_conc=$(date +%s%N)
for i in $(seq 1 10); do
    curl -sN "http://localhost:$PORT_COMPIO_16/sse?chunks=100&delay=0" > /dev/null 2>&1 &
done
wait
end_conc=$(date +%s%N)
conc_ms=$(( (end_conc - start_conc) / 1000000 ))
printf "  %-40s  %5dms (10 parallel)\n" "compio ($(nproc) workers)" "$conc_ms"

start_conc=$(date +%s%N)
for i in $(seq 1 10); do
    curl -sN "http://localhost:$PORT_NODE/sse?chunks=100&delay=0" > /dev/null 2>&1 &
done
wait
end_conc=$(date +%s%N)
conc_ms=$(( (end_conc - start_conc) / 1000000 ))
printf "  %-40s  %5dms (10 parallel)\n" "Node.js" "$conc_ms"

echo ""
echo "========================================================="
