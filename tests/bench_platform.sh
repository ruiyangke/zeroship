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
    --blob-store /tmp/zeroship-bench-bundles --control-key bk --master-key bm > /dev/null 2>&1 &
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

# Warmup (retry until gateway syncs).
#
# THIS LOOP USED TO FALL THROUGH SILENTLY. It had no failure branch: 15 misses
# and it simply continued, so wrk went on to benchmark whatever the endpoint
# actually returns and printed throughput and latency for it. A benchmark that
# cannot tell "the app answered" from "the app 404'd" reports the 404 as a
# result, and a 404 is CHEAP - so the broken leg looks FAST, flattering the full
# pipeline against the raw-runtime baseline it is being compared to.
#
# That is not hypothetical here. MEASURED 2026-08-10, serving the very artifact
# this harness benchmarks (examples/bench/dist/server/index.js) under
# `zeroship serve`, one variable apart:
#
#   POST /rpc                 -> 404 "Not Found"      <- what this harness sends
#   POST /__zeroship/v1/ping  -> 200 {"json":"pong"}  <- what the app serves
#
# examples/bench/src/server.ts exports only query/stream procedures and no
# `fetch`, so the dispatcher's non-/__zeroship/v1/ fallback is a 404 by
# construction. Whether the GATEWAY rewrites /apps/bench/rpc onto a wireId
# before it reaches the app is NOT established - bench has no config.ts, so no
# rewrite is authored, but that leg was not measured.
#
# Fail loudly either way. An unreachable warmup means the numbers below measure
# nothing, and that must not be reported as a benchmark.
warmed=0
for i in $(seq 1 15); do
    if curl -sf -X POST http://localhost:8000/apps/bench/rpc \
        -H 'Content-Type: application/json' \
        -H "X-Api-Key: $API_KEY" \
        -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}' > /dev/null 2>&1; then
        warmed=1
        break
    fi
    sleep 1
done
if [ "$warmed" -ne 1 ]; then
    code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -X POST \
        http://localhost:8000/apps/bench/rpc \
        -H 'Content-Type: application/json' \
        -H "X-Api-Key: $API_KEY" \
        -d '{"jsonrpc":"2.0","method":"ping","params":[],"id":1}' 2>/dev/null || echo 000)
    echo "FAIL: warmup never succeeded against /apps/bench/rpc (last status ${code})." >&2
    echo "      The gateway leg is NOT serving this request, so any throughput" >&2
    echo "      printed below would measure the failure path, not app dispatch." >&2
    echo "      404 => the path does not reach a procedure; the app serves" >&2
    echo "      /__zeroship/v1/<id>, not /rpc. 000 => nothing is listening." >&2
    exit 1
fi

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

# wrk ALREADY TELLS YOU when it is timing failures; this harness used to throw
# that away. It grepped Requests/sec and printed it, so a leg answering 404 on
# every request reported a throughput figure indistinguishable from a working
# one -- and higher, because a 404 is cheaper than a dispatch into V8.
#
# wrk prints `Non-2xx or 3xx responses: N` whenever any request failed, and
# `unable to connect` when nothing is listening. Both are read here. Nothing is
# probed that wrk does not already measure; the defect was purely that the
# instrument's own error reporting was discarded.
#
# Measured 2026-08-10 on the artifact this harness benchmarks: POST /rpc gives
# 404 while POST /__zeroship/v1/ping gives 200, so at least one leg was timing
# the failure path. See the warmup block above.
run_bench() {
    local label=$1 url=$2 lua=$3
    local result
    result=$(wrk -t4 -c50 -d10s -s "$lua" "$url" 2>&1)
    local rps=$(echo "$result" | grep 'Requests/sec' | awk '{print $2}')
    local lat=$(echo "$result" | grep 'Latency' | awk '{print $2}')
    local bad=$(echo "$result" | grep -oE 'Non-2xx or 3xx responses: [0-9]+' | grep -oE '[0-9]+$')
    if echo "$result" | grep -qi 'unable to connect\|connection refused'; then
        printf "  %-45s %12s\n" "$label" "UNREACHABLE"
        echo "FAIL: $label -- wrk could not connect to $url; nothing was measured." >&2
        BENCH_BAD=$((${BENCH_BAD:-0} + 1))
        return
    fi
    if [ -n "$bad" ] && [ "$bad" -gt 0 ]; then
        printf "  %-45s %12s req/s  %8s avg   <- %s NON-2xx\n" "$label" "$rps" "$lat" "$bad"
        echo "FAIL: $label -- $bad non-2xx responses. This number times the FAILURE" >&2
        echo "      path, not app dispatch, and a failure is cheaper so it reads FAST." >&2
        BENCH_BAD=$((${BENCH_BAD:-0} + 1))
        return
    fi
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

# A counter nobody reads is the defect this file just fixed, arriving one level
# up. run_bench increments BENCH_BAD for every leg that timed failures or could
# not connect; without this the FAIL lines would go to stderr and the script
# would still exit 0, so a run where every leg 404'd would look like a
# successful benchmark to anything checking the exit status.
if [ "${BENCH_BAD:-0}" -gt 0 ]; then
    echo "BENCHMARK INVALID: ${BENCH_BAD} leg(s) timed a failure path or were unreachable." >&2
    echo "                   The figures above do not measure app dispatch. Do not quote them." >&2
    exit 1
fi
