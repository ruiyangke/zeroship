#!/usr/bin/env bash
# Profile the v8-1w bench server under fetchEcho saturation.
#
# Usage:
#   bench/flamegraphs/profile.sh <output_label> [duration_seconds]
#
# Outputs:
#   bench/flamegraphs/<label>.svg
#   bench/flamegraphs/<label>.bench.txt   (zerobench summary)
#   bench/flamegraphs/<label>.top.txt     (perf report top frames)
set -euo pipefail

LABEL="${1:-baseline}"
DUR="${2:-15}"

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT_DIR="$ROOT/bench/flamegraphs"          # committed (small text artifacts)
TMP_DIR="${TMPDIR:-/tmp}/zeroship-flamegraphs"  # large binaries (perf.data, script)
mkdir -p "$TMP_DIR"
NGINX_BIN="$(nix build --no-link --print-out-paths nixpkgs#nginx 2>/dev/null)/bin/nginx"
BIN="$ROOT/target/release/zeroship-bench-server"
ZB="$HOME/Projects/zerobench/target/release/zerobench"
RHAI="$ROOT/crates/runtime/benches/zeroship-bench.rhai"
FLAMEGRAPH_BIN=/nix/store/g9s1xp5rlq4csh9yrsbab3ii24cwvp8c-cargo-flamegraph-0.6.11/bin/flamegraph
PORT_V8=5100
PORT_ECHO=8888

# Kill any leftover servers from prior runs
for p in $PORT_V8 $PORT_ECHO; do
    lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
done
sleep 1

# nginx config (same as run_zerobench.sh)
NGINX_DIR=$(mktemp -d)
trap 'kill_all' EXIT
kill_all() {
    [ -n "${SERVER_PID:-}" ] && kill "$SERVER_PID" 2>/dev/null || true
    [ -n "${ZB_PID:-}"     ] && kill "$ZB_PID"     2>/dev/null || true
    [ -f "$NGINX_DIR/nginx.pid" ] && kill "$(cat "$NGINX_DIR/nginx.pid")" 2>/dev/null || true
    rm -rf "$NGINX_DIR"
    wait 2>/dev/null || true
}

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
"$NGINX_BIN" -c "$NGINX_DIR/nginx.conf" -p "$NGINX_DIR" > /dev/null 2>&1
sleep 1

# Bench server (single worker, dev mode for SSRF bypass)
ZEROSHIP_DEV=1 "$BIN" --port=$PORT_V8 --workers=1 > /dev/null 2>&1 &
SERVER_PID=$!
sleep 2

# Sanity check
if ! curl -sf -X POST -H 'Content-Type: application/json' \
    -d '{"json":null}' "http://127.0.0.1:$PORT_V8/_zs/v1/ping" >/dev/null; then
    echo "FATAL: bench-server not responding on $PORT_V8"
    exit 1
fi
if ! curl -sf "http://127.0.0.1:$PORT_ECHO/" >/dev/null; then
    echo "FATAL: nginx echo not responding on $PORT_ECHO"
    exit 1
fi

# Start zerobench (open-loop saturate, fetchEcho only) — runs longer than perf.
BENCH_HOST=127.0.0.1 BENCH_PORT=$PORT_V8 BENCH_ECHO_PORT=$PORT_ECHO \
BENCH_SCENARIO=fetchEcho \
"$ZB" run "$RHAI" -c 300 -t 8 --duration "${DUR}s" --saturate --color never \
    > "$OUT_DIR/$LABEL.bench.txt" 2>&1 &
ZB_PID=$!

# Let zerobench warm up the server (~1s) before perf starts.
sleep 1

# perf record on the server PID, for slightly less than zerobench duration so
# perf finishes first.
PERF_DUR=$((DUR - 2))
PERF_DATA="$TMP_DIR/$LABEL.perf.data"
perf record -F 999 --call-graph fp -p "$SERVER_PID" -o "$PERF_DATA" \
    -- sleep "$PERF_DUR" > /dev/null 2>&1

wait "$ZB_PID" || true

# Render flamegraph: perf script → folded → svg
FOLDED="$TMP_DIR/$LABEL.folded"
perf script -i "$PERF_DATA" --no-inline 2>/dev/null > "$TMP_DIR/$LABEL.script"

# Use the flamegraph binary's --post-process or directly use perf-folded format.
# cargo-flamegraph internally invokes inferno-collapse-perf. We don't have a
# standalone collapser — but flamegraph has a perf-script-mode invocation.
# Easiest path: pipe perf script into flamegraph stdin (it accepts folded *or*
# raw perf-script and detects). Actually it doesn't — let's use stackcollapse.
# Fallback: write a python-free awk collapser sufficient for fp call-graphs.
awk '
BEGIN { stack=""; }
/^[[:space:]]*$/ {
    if (stack != "") {
        # Reverse the stack (perf-script outputs leaf first, flamegraph wants root first)
        n = split(stack, a, ";");
        out = a[n];
        for (i=n-1; i>=1; i--) out = out ";" a[i];
        counts[out]++;
        stack = "";
    }
    next;
}
/^[a-zA-Z0-9_]/ {
    # Header line: "process pid [cpu] timestamp: <freq> cycles:":
    # just skip; new sample begins.
    next;
}
/^[[:space:]]+[0-9a-fA-F]+/ {
    # Stack frame line: "    addr symbol+offset (dso)"
    # Extract symbol name only (col 2, before "+offset" if present).
    sym = $2;
    sub(/\+0x[0-9a-fA-F]+$/, "", sym);
    if (sym == "" || sym == "[unknown]") sym = "[unknown]";
    if (stack == "") stack = sym;
    else stack = stack ";" sym;
}
END {
    # Last sample
    if (stack != "") {
        n = split(stack, a, ";");
        out = a[n];
        for (i=n-1; i>=1; i--) out = out ";" a[i];
        counts[out]++;
    }
    for (k in counts) printf "%s %d\n", k, counts[k];
}' "$TMP_DIR/$LABEL.script" > "$FOLDED"

"$FLAMEGRAPH_BIN" --title "fetchEcho v8-1w: $LABEL" \
    -o "$OUT_DIR/$LABEL.svg" -- "$FOLDED" 2>/dev/null \
  || cat "$FOLDED" | "$FLAMEGRAPH_BIN" --title "fetchEcho v8-1w: $LABEL" \
        -o "$OUT_DIR/$LABEL.svg" 2>/dev/null \
  || true

# perf report top frames
perf report -i "$PERF_DATA" --stdio --no-children --percent-limit 0.5 2>/dev/null \
    | head -100 > "$OUT_DIR/$LABEL.top.txt"

echo "wrote: $OUT_DIR/$LABEL.{svg,top.txt,bench.txt}"
echo "       $TMP_DIR/$LABEL.{perf.data,script,folded}"
echo
echo "=== Bench summary ==="
grep -E "throughput|status|latency" "$OUT_DIR/$LABEL.bench.txt" | head -10
echo
echo "=== Top frames (>0.5%) ==="
head -40 "$OUT_DIR/$LABEL.top.txt"
