#!/usr/bin/env bash
# zeroship vs Node.js — cross-runtime benchmark with zerobench.
#
# Boots every target (v8-compio 1w, v8-compio Nw, node single, node cluster)
# and runs the same unified Rhai plan against each. Output is scenario-first:
# every RPC method shows all four targets side by side (matches the layout
# of the old wrk-based `run_benchmark.sh`).
#
# Usage:
#   ./crates/runtime/benches/run_zerobench.sh
#   ./crates/runtime/benches/run_zerobench.sh --duration=30s --conns=500
#   ./crates/runtime/benches/run_zerobench.sh --rate=200k
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"
BIN="$ROOT_DIR/target/release"
ZB="${ZEROBENCH:-$HOME/Projects/zerobench/target/release/zerobench}"
RHAI="$SCRIPT_DIR/zeroship-bench.rhai"

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------

DURATION="${DURATION:-10s}"
CONNS="${CONNS:-300}"
WORKERS="${WORKERS:-16}"
# Client-side zerobench worker threads — separate knob from the
# server's WORKERS so asymmetric loads (e.g. 16-core bench vs 4-core
# server) can be modelled without editing this script.
CLIENT_THREADS="${CLIENT_THREADS:-$WORKERS}"
RATE="${RATE:-}"
MODE="${MODE:-saturate}"
PORT_V8_1=5100
PORT_V8_N=5101
PORT_NODE=4002
PORT_NODE_CLUSTER=4003
PORT_ECHO=8888

for arg in "$@"; do
    case "$arg" in
        --duration=*) DURATION="${arg#*=}" ;;
        --conns=*)    CONNS="${arg#*=}" ;;
        --workers=*)  WORKERS="${arg#*=}" ;;
        --rate=*)     RATE="${arg#*=}"; MODE=rate ;;
        --saturate)   MODE=saturate ;;
    esac
done

# ---------------------------------------------------------------------------
# NUMA isolation
# ---------------------------------------------------------------------------

NUMA_NODES=$(lscpu 2>/dev/null | grep "NUMA node(s)" | awk '{print $NF}' || echo "1")
HAS_NUMACTL=$(command -v numactl >/dev/null 2>&1 && echo "1" || echo "0")
if [ "$NUMA_NODES" -ge 2 ] && [ "$HAS_NUMACTL" = "1" ]; then
    NUMA_SERVER="numactl --cpunodebind=0 --membind=0"
    NUMA_CLIENT="numactl --cpunodebind=1 --membind=1"
    NUMA_INFO="NUMA split: servers on node 0, zerobench on node 1"
else
    NUMA_SERVER=""
    NUMA_CLIENT=""
    NUMA_INFO=$([ "$NUMA_NODES" -ge 2 ] \
        && echo "Multi-NUMA detected but numactl not installed" \
        || echo "Single NUMA node")
fi

# ---------------------------------------------------------------------------
# Server lifecycle
# ---------------------------------------------------------------------------

PIDS=()
cleanup() {
    for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
    # nginx forks worker processes; stop them via the master pidfile.
    if [ -n "${NGINX_DIR:-}" ] && [ -f "$NGINX_DIR/nginx.pid" ]; then
        kill "$(cat "$NGINX_DIR/nginx.pid")" 2>/dev/null || true
    fi
    [ -n "${NGINX_DIR:-}" ] && rm -rf "$NGINX_DIR"
    wait 2>/dev/null || true
}
trap cleanup EXIT

echo "=== Building ==="
cargo build --release -p zeroship-runtime --bin v8-server-compio 2>&1 | tail -2

# Ensure nginx is available (stable, industry-standard echo server — removes
# our own HTTP impl from the fetch() benchmark loop).
echo "=== Fetching nginx via nix ==="
NGINX_BIN=$(nix build --no-link --print-out-paths nixpkgs#nginx 2>/dev/null)/bin/nginx
if [ ! -x "$NGINX_BIN" ]; then
    echo "FATAL: could not resolve nginx via nix"; exit 1
fi
echo "[ok] $NGINX_BIN"

# Minimal nginx config: return a constant JSON body on any request.
NGINX_DIR="$(mktemp -d)"
cat > "$NGINX_DIR/nginx.conf" << EOF
worker_processes auto;
error_log /dev/null crit;
pid $NGINX_DIR/nginx.pid;
events { worker_connections 8192; use epoll; multi_accept on; }
http {
    access_log off;
    keepalive_timeout 60s;
    keepalive_requests 1000000;
    tcp_nodelay on;
    server {
        listen $PORT_ECHO default_server reuseport backlog=8192;
        location / {
            default_type application/json;
            return 200 '{"status":"ok"}';
        }
    }
}
EOF

echo "=== Starting servers ==="
for port in $PORT_V8_1 $PORT_V8_N $PORT_NODE $PORT_NODE_CLUSTER $PORT_ECHO; do
    lsof -ti :"$port" 2>/dev/null | xargs kill -9 2>/dev/null || true
done
sleep 1

# nginx — echo target used by the fetchEcho scenario.
$NUMA_SERVER "$NGINX_BIN" -c "$NGINX_DIR/nginx.conf" -p "$NGINX_DIR" > /dev/null 2>&1 &
PIDS+=($!)
# ZEROSHIP_DEV=1 disables the runtime's SSRF protection so the
# fetchEcho scenario can hit the in-NUMA nginx on 127.0.0.1 —
# otherwise the runtime returns 500 "Blocked request to
# private/internal IP" on every fetchExternal call.
ZEROSHIP_DEV=1 $NUMA_SERVER "$BIN/v8-server-compio" --port="$PORT_V8_1" --workers=1 > /dev/null 2>&1 &
PIDS+=($!)
ZEROSHIP_DEV=1 $NUMA_SERVER "$BIN/v8-server-compio" --port="$PORT_V8_N" --workers="$WORKERS" > /dev/null 2>&1 &
PIDS+=($!)
$NUMA_SERVER node "$SCRIPT_DIR/node_server.js" $PORT_NODE > /dev/null 2>&1 &
PIDS+=($!)
$NUMA_SERVER node "$SCRIPT_DIR/node_server_cluster.js" $PORT_NODE_CLUSTER "$WORKERS" > /dev/null 2>&1 &
PIDS+=($!)
sleep 4

for port in $PORT_V8_1 $PORT_V8_N $PORT_NODE $PORT_NODE_CLUSTER; do
    # URL-path RPC wire — POST /_rpc/ping with body=[] (appbase ee2fd5f).
    if ! curl -sf -X POST -H 'Content-Type: application/json' \
        -d '[]' "http://127.0.0.1:$port/_rpc/ping" > /dev/null 2>&1; then
        echo "FATAL: port $port not responding"; exit 1
    fi
done
# Verify nginx echo is up too.
if ! curl -sf "http://127.0.0.1:$PORT_ECHO/" > /dev/null 2>&1; then
    echo "FATAL: nginx echo not responding on port $PORT_ECHO"; exit 1
fi
NODE_VERSION=$(node --version 2>/dev/null || echo "unknown")
NGINX_VERSION=$("$NGINX_BIN" -v 2>&1 | sed 's|.*/||')
echo "[ok] v8-compio (1w @ $PORT_V8_1, ${WORKERS}w @ $PORT_V8_N)"
echo "[ok] node $NODE_VERSION (single @ $PORT_NODE, cluster ${WORKERS}w @ $PORT_NODE_CLUSTER)"
echo "[ok] $NGINX_VERSION echo @ $PORT_ECHO"

# ---------------------------------------------------------------------------
# Header
# ---------------------------------------------------------------------------

if [ "$MODE" = "saturate" ]; then
    MODE_LABEL="saturate ($CONNS conns)"
    MODE_ARGS=(--saturate)
else
    MODE_LABEL="open-loop ${RATE:-<script>}/s"
    MODE_ARGS=()
    [ -n "$RATE" ] && MODE_ARGS+=(--rate "$RATE")
fi

echo
echo "================================================================="
echo "  zeroship vs Node.js Runtime Benchmark (zerobench)"
echo "  $(date -u +%Y-%m-%d) | $(nproc) logical cores | $(uname -m)"
echo "  $NUMA_INFO"
echo "  $DURATION per scenario · $MODE_LABEL · serial per-scenario"
echo "================================================================="

# ---------------------------------------------------------------------------
# Pass 1: run zerobench against each target, capture all per-scenario stats.
# ---------------------------------------------------------------------------

RESULTS_DIR="$(mktemp -d)"

bench_target() {
    local label=$1 port=$2 include_streaming=${3:-0}
    local slot=$4
    local out_file="$RESULTS_DIR/$slot.raw"
    printf "  [%d/4] %-30s" "$slot" "$label"
    local env_vars=(BENCH_HOST=127.0.0.1 BENCH_PORT="$port" BENCH_ECHO_PORT="$PORT_ECHO")
    if [ "$include_streaming" = "0" ]; then
        env_vars+=(BENCH_SKIP_STREAMING=1 BENCH_HTTP_GET=0)
    fi
    local start=$(date +%s)
    # zerobench's exit policy now gates on hard transport errors only
    # (connect / read / write / timeout / keepup) — 4xx / 5xx / assertion
    # failures are part of the benchmark signal and exit 0. That means
    # a non-zero exit here genuinely indicates a transport-layer failure
    # worth surfacing in the raw output, so we no longer swallow it.
    env "${env_vars[@]}" $NUMA_CLIENT "$ZB" run "$RHAI" \
        -c "$CONNS" -t "$CLIENT_THREADS" --duration "$DURATION" "${MODE_ARGS[@]}" \
        --color never > "$out_file" 2>&1 || {
        echo "  (zerobench exit $? — transport errors in $out_file)"
    }
    local dur=$(( $(date +%s) - start ))
    printf " done (%ds)\n" "$dur"
    # Save the label for the summary.
    echo "$label" > "$RESULTS_DIR/$slot.label"
}

echo
echo "=== Benchmarking targets ==="
bench_target "v8-compio (1w)"           $PORT_V8_1         0  1
bench_target "v8-compio (${WORKERS}w)"  $PORT_V8_N         1  2
bench_target "node single"              $PORT_NODE         0  3
bench_target "node cluster (${WORKERS}w)" $PORT_NODE_CLUSTER 0  4

# ---------------------------------------------------------------------------
# Pass 2: parse each result file into per-(target,scenario) pairs.
# Emits lines: "slot<TAB>scenario<TAB>rps<TAB>p50<TAB>p99"
# ---------------------------------------------------------------------------

parse_results() {
    local slot=$1
    local file=$2
    local scenario=""
    local rps=""
    local p50=""
    local p99=""
    while IFS= read -r line; do
        if [[ "$line" =~ scenario\ [0-9]+/[0-9]+:\ ([a-zA-Z0-9_-]+) ]]; then
            scenario="${BASH_REMATCH[1]}"
        elif [[ "$line" =~ throughput[[:space:]]+([0-9,]+)[[:space:]]+(req|ops)/s ]]; then
            rps="${BASH_REMATCH[1]//,/}"
        elif [[ "$line" =~ (latency|chunk-gap|rtt|broadcast-rtt)[[:space:]]+p50=([^[:space:]]+) ]]; then
            # BASH_REMATCH[2] is the p50 value (group 1 is the label).
            p50="${BASH_REMATCH[2]}"
            if [[ "$line" =~ p99=([^[:space:]]+) ]]; then
                p99="${BASH_REMATCH[1]}"
            fi
            # Full metric row — emit and reset.
            if [ -n "$scenario" ] && [ -n "$rps" ]; then
                printf '%s\t%s\t%s\t%s\t%s\n' "$slot" "$scenario" "$rps" "$p50" "$p99"
            fi
            scenario="" rps="" p50="" p99=""
        fi
    done < "$file"
}

ALL="$RESULTS_DIR/all.tsv"
> "$ALL"
for slot in 1 2 3 4; do
    parse_results "$slot" "$RESULTS_DIR/$slot.raw" >> "$ALL"
done

# ---------------------------------------------------------------------------
# Pass 3: scenario-first layout — each scenario shows all 4 targets.
# ---------------------------------------------------------------------------

# Scenarios in declaration order (take the first target's ordering).
SCENARIOS=$(awk -F'\t' '$1=="2" {print $2}' "$ALL")
[ -z "$SCENARIOS" ] && SCENARIOS=$(awk -F'\t' '$1=="1" {print $2}' "$ALL")

echo
echo "================================================================="
echo "  Per-scenario comparison (req/s · p50 · p99)"
echo "================================================================="

for sc in $SCENARIOS; do
    echo
    echo "--- $sc ---"
    for slot in 1 2 3 4; do
        label=$(cat "$RESULTS_DIR/$slot.label" 2>/dev/null)
        row=$(awk -F'\t' -v s="$slot" -v sc="$sc" '$1==s && $2==sc {print; exit}' "$ALL")
        if [ -z "$row" ]; then
            printf "  %-30s %14s\n" "$label" "n/a"
            continue
        fi
        rps=$(echo "$row" | awk -F'\t' '{print $3}')
        p50=$(echo "$row" | awk -F'\t' '{print $4}')
        p99=$(echo "$row" | awk -F'\t' '{print $5}')
        printf "  %-30s %'14d req/s  p50=%s  p99=%s\n" "$label" "$rps" "$p50" "$p99"
    done
done

echo
echo "================================================================="
echo "  Raw per-target reports in: $RESULTS_DIR"
echo "================================================================="
