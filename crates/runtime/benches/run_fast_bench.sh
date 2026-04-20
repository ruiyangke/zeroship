#!/usr/bin/env bash
# run_fast_bench.sh — zeroship-only wrk bench with NUMA split.
#
# Three scenarios, two server shapes (1w + 16w), cross-NUMA load gen.
# No Node.js; no cluster. Pure kernel-path throughput.
#
# Usage:
#   ./crates/runtime/benches/run_fast_bench.sh
#   ./crates/runtime/benches/run_fast_bench.sh --conns=1024 --threads=16 --duration=15s
#   PERF=1 ./crates/runtime/benches/run_fast_bench.sh   # attach perf stat to the 16w server

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"
BIN="$ROOT_DIR/target/release"

CONNS="${CONNS:-512}"
THREADS="${THREADS:-16}"
DURATION="${DURATION:-10s}"
WORKERS="${WORKERS:-16}"
PERF="${PERF:-0}"

for arg in "$@"; do
    case "$arg" in
        --conns=*)    CONNS="${arg#*=}" ;;
        --threads=*)  THREADS="${arg#*=}" ;;
        --duration=*) DURATION="${arg#*=}" ;;
        --workers=*)  WORKERS="${arg#*=}" ;;
        --perf)       PERF=1 ;;
    esac
done

# ---------------------------------------------------------------------------
# NUMA split: servers on node 0, wrk client on node 1. Keeps TCP, V8 heap,
# and wrk's socket buffers on their own NUMA memory — removes the cross-
# socket coherence tax that dominates the small-packet hot path.
# ---------------------------------------------------------------------------
NUMA_SERVER=""
NUMA_CLIENT=""
if command -v numactl >/dev/null 2>&1; then
    NUMA_NODES=$(lscpu | awk '/^NUMA node\(s\):/ {print $3}')
    if [ "${NUMA_NODES:-1}" -ge 2 ]; then
        NUMA_SERVER="numactl --cpunodebind=0 --membind=0"
        NUMA_CLIENT="numactl --cpunodebind=1 --membind=1"
    fi
fi

PORT_1W=5100
PORT_NW=5101
PORT_ECHO=8888

# ---------------------------------------------------------------------------
# Kill stragglers on our ports (best-effort)
# ---------------------------------------------------------------------------
for port in $PORT_1W $PORT_NW $PORT_ECHO; do
    lsof -ti :$port 2>/dev/null | xargs -r kill -9 2>/dev/null || true
done
sleep 1

echo "=== Build ==="
(cd "$ROOT_DIR" && cargo build --release --bin v8-server-compio --bin echo-server 2>&1 | tail -1)

echo "=== Start servers ==="
$NUMA_SERVER "$BIN/echo-server" $PORT_ECHO >/tmp/echo.log 2>&1 &
ECHO_PID=$!

$NUMA_SERVER env ZEROSHIP_DEV=1 "$BIN/v8-server-compio" --port=$PORT_1W --workers=1 \
    >/tmp/v8-1w.log 2>&1 &
V8_1_PID=$!

$NUMA_SERVER env ZEROSHIP_DEV=1 "$BIN/v8-server-compio" --port=$PORT_NW --workers=$WORKERS \
    >/tmp/v8-nw.log 2>&1 &
V8_N_PID=$!

PERF_PID=""
if [ "$PERF" = "1" ]; then
    sleep 2
    perf stat -a -p $V8_N_PID -o /tmp/v8-nw.perf.log >/dev/null 2>&1 &
    PERF_PID=$!
fi

cleanup() {
    [ -n "$PERF_PID" ] && kill -INT $PERF_PID 2>/dev/null || true
    kill $ECHO_PID $V8_1_PID $V8_N_PID 2>/dev/null || true
    wait $ECHO_PID $V8_1_PID $V8_N_PID 2>/dev/null || true
}
trap cleanup EXIT

# Ready gate — poll each port up to 10s
sleep 2
for port in $PORT_1W $PORT_NW; do
    for _try in $(seq 1 10); do
        if curl -sf -X POST "http://127.0.0.1:$port/_rpc/ping" -d '[]' \
             -H 'content-type: application/json' --max-time 1 -o /dev/null 2>/dev/null; then
            break
        fi
        sleep 1
    done
done
for _try in $(seq 1 5); do
    if curl -sf "http://127.0.0.1:$PORT_ECHO" --max-time 1 -o /dev/null 2>/dev/null; then
        break
    fi
    sleep 1
done

# ---------------------------------------------------------------------------
# wrk payloads — URL-path RPC wire (POST /_rpc/<method>, body = JSON array)
# ---------------------------------------------------------------------------
cat >/tmp/wrk-rpc-empty.lua <<'EOF'
wrk.method = "POST"
wrk.headers["Content-Type"] = "application/json"
wrk.body = "[]"
EOF

cat >/tmp/wrk-rpc-fetchlocal.lua <<'EOF'
wrk.method = "POST"
wrk.headers["Content-Type"] = "application/json"
wrk.body = '["http://127.0.0.1:8888"]'
EOF

# ---------------------------------------------------------------------------
# Harness
# ---------------------------------------------------------------------------
run_wrk() {
    local label=$1 port=$2 path=$3 lua=$4
    local output rps p50 p99
    output=$($NUMA_CLIENT wrk -t$THREADS -c$CONNS -d$DURATION --latency \
        -s "$lua" "http://127.0.0.1:$port$path" 2>&1)
    rps=$(awk '/Requests\/sec/ {print $2}' <<< "$output")
    p50=$(awk '$1=="50%" {print $2}' <<< "$output")
    p99=$(awk '$1=="99%" {print $2}' <<< "$output")
    # Also pull total requests for sanity
    local total
    total=$(awk '/[Rr]equests in / {print $1}' <<< "$output")
    printf "  %-28s %14s req/s  p50=%-8s p99=%-8s (total %s)\n" \
        "$label" "$rps" "$p50" "$p99" "${total:-?}"
}

echo ""
echo "=================================================================="
echo "  zeroship fast bench — $(date -u +'%Y-%m-%dT%H:%M:%SZ')"
echo "  $(hostname)  |  $(nproc) cores  |  load: $(awk '{print $1, $2, $3}' /proc/loadavg)"
echo "  wrk: -t$THREADS -c$CONNS -d$DURATION  |  workers=$WORKERS"
echo "  NUMA: $([ -n "$NUMA_SERVER" ] && echo "server=node0 / client=node1" || echo 'disabled')"
echo "=================================================================="

echo ""
echo "--- ping ---"
run_wrk "v8-compio 1w" $PORT_1W "/_rpc/ping" /tmp/wrk-rpc-empty.lua
run_wrk "v8-compio ${WORKERS}w" $PORT_NW "/_rpc/ping" /tmp/wrk-rpc-empty.lua

echo ""
echo "--- promiseChain ---"
run_wrk "v8-compio 1w" $PORT_1W "/_rpc/promiseChain" /tmp/wrk-rpc-empty.lua
run_wrk "v8-compio ${WORKERS}w" $PORT_NW "/_rpc/promiseChain" /tmp/wrk-rpc-empty.lua

echo ""
echo "--- fetchLocal (→ echo @ $PORT_ECHO) ---"
run_wrk "v8-compio 1w" $PORT_1W "/_rpc/fetchExternal" /tmp/wrk-rpc-fetchlocal.lua
run_wrk "v8-compio ${WORKERS}w" $PORT_NW "/_rpc/fetchExternal" /tmp/wrk-rpc-fetchlocal.lua

echo ""
echo "=================================================================="

if [ "$PERF" = "1" ]; then
    # Stop perf; wait for it to flush.
    kill -INT $PERF_PID 2>/dev/null || true
    wait $PERF_PID 2>/dev/null || true
    echo ""
    echo "--- perf stat (16w server, aggregate over all bench scenarios) ---"
    cat /tmp/v8-nw.perf.log
fi
