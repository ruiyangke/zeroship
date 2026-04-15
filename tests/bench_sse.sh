#!/usr/bin/env bash
# SSE Streaming Benchmark — measures chunk latency and throughput
# through the full pipeline (V8 → worker → gateway → client).
#
# Uses a mock app that emits N chunks at a fixed interval (no real LLM).
# Measures: time-to-first-byte, chunk-to-chunk latency, total throughput.
#
# Prerequisites:
#   - cargo build --release
#   - docker start pg-test (Postgres on port 5434)
#   - curl installed
#
# Usage:
#   ./tests/bench_sse.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"
CORES=$(nproc)

PIDS=()
cleanup() {
    for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
    wait 2>/dev/null || true
    rm -rf /tmp/zeroship-sse-bench-*
}
trap cleanup EXIT

echo "================================================================="
echo "  zeroship SSE Streaming Benchmark"
echo "  $(date -u +%Y-%m-%d) | $CORES cores | $(uname -m)"
echo "================================================================="
echo ""

# --- Setup ---
for port in 9090 8080 8000; do
    lsof -ti :"$port" 2>/dev/null | xargs kill -9 2>/dev/null || true
done
rm -rf /tmp/zeroship-sse-bench-bundles
docker exec pg-test psql -U postgres -c "DROP TABLE IF EXISTS usage_history, usage, apps CASCADE" > /dev/null 2>&1

# Start platform
"$BIN/zeroship-control" --port 9090 --db "postgres://postgres:test@localhost:5434/postgres" \
    --bundles /tmp/zeroship-sse-bench-bundles --control-key bk --master-key bm > /dev/null 2>&1 &
PIDS+=($!)
sleep 3

"$BIN/zeroship-worker" --port 8080 --workers "$CORES" --control http://localhost:9090 \
    --control-key bk --poll-interval 60 > /dev/null 2>&1 &
PIDS+=($!)
sleep 2

"$BIN/zeroship-gate" --port 8000 --control http://localhost:9090 \
    --control-key bk --workers http://localhost:8080 --poll-interval 60 > /dev/null 2>&1 &
PIDS+=($!)
sleep 3

# Create + deploy SSE test app
APP=$(curl -sf -X POST http://localhost:9090/api/apps \
    -H 'Content-Type: application/json' \
    -H 'Authorization: Bearer bm' \
    -d '{"name":"sse-bench"}')
APP_ID=$(echo "$APP" | jq -r '.id')
API_KEY=$(echo "$APP" | jq -r '.api_key')

mkdir -p /tmp/zeroship-sse-bench-app

# SSE test app: emits 100 chunks with 1ms delay between each
# Simulates a fast LLM that generates ~1000 tokens/second
cat > /tmp/zeroship-sse-bench-app/index.js << 'APPEOF'
export function onRequest(request) {
    const url = new URL(request.url);
    const chunks = parseInt(url.searchParams.get("chunks") || "100");
    const delayMs = parseInt(url.searchParams.get("delay") || "1");

    const stream = new ReadableStream({
        async start(controller) {
            for (let i = 0; i < chunks; i++) {
                const data = `data: {"token":"chunk_${i}","index":${i}}\n\n`;
                controller.enqueue(new TextEncoder().encode(data));
                if (delayMs > 0) {
                    await new Promise(r => setTimeout(r, delayMs));
                }
            }
            controller.enqueue(new TextEncoder().encode("data: [DONE]\n\n"));
            controller.close();
        }
    });

    return new Response(stream, {
        headers: {
            "Content-Type": "text/event-stream",
            "Cache-Control": "no-cache",
            "Connection": "keep-alive",
        }
    });
}
APPEOF

"$BIN/zeroship" deploy /tmp/zeroship-sse-bench-app/index.js --app="$APP_ID" \
    --control=http://localhost:9090 --key=bm > /dev/null 2>&1

# Warmup
for i in $(seq 1 15); do
    if curl -sf http://localhost:8000/apps/sse-bench/health > /dev/null 2>&1; then break; fi
    sleep 1
done

echo "--- Single stream: 100 chunks, 1ms delay ---"
echo ""

# Measure single SSE stream
START=$(date +%s%N)
FIRST_BYTE_TIME=""
CHUNK_COUNT=0

while IFS= read -r line; do
    if [ -z "$FIRST_BYTE_TIME" ]; then
        FIRST_BYTE_TIME=$(date +%s%N)
    fi
    if [[ "$line" == data:* ]]; then
        CHUNK_COUNT=$((CHUNK_COUNT + 1))
    fi
    if [[ "$line" == "data: [DONE]" ]]; then
        break
    fi
done < <(curl -sN "http://localhost:8000/apps/sse-bench/stream?chunks=100&delay=1" \
    -H "X-Api-Key: $API_KEY" 2>/dev/null)

END=$(date +%s%N)
TOTAL_MS=$(( (END - START) / 1000000 ))
TTFB_MS=$(( (FIRST_BYTE_TIME - START) / 1000000 ))

echo "  Chunks received:        $CHUNK_COUNT"
echo "  Time to first byte:     ${TTFB_MS}ms"
echo "  Total stream time:      ${TOTAL_MS}ms"
if [ "$CHUNK_COUNT" -gt 0 ]; then
    AVG_LATENCY=$(( TOTAL_MS / CHUNK_COUNT ))
    echo "  Avg chunk latency:      ${AVG_LATENCY}ms"
    THROUGHPUT=$(( CHUNK_COUNT * 1000 / TOTAL_MS ))
    echo "  Chunk throughput:       ${THROUGHPUT} chunks/sec"
fi

echo ""
echo "--- Single stream: 1000 chunks, 0ms delay (max throughput) ---"
echo ""

START=$(date +%s%N)
FIRST_BYTE_TIME=""
CHUNK_COUNT=0

while IFS= read -r line; do
    if [ -z "$FIRST_BYTE_TIME" ]; then
        FIRST_BYTE_TIME=$(date +%s%N)
    fi
    if [[ "$line" == data:* ]]; then
        CHUNK_COUNT=$((CHUNK_COUNT + 1))
    fi
    if [[ "$line" == "data: [DONE]" ]]; then
        break
    fi
done < <(curl -sN "http://localhost:8000/apps/sse-bench/stream?chunks=1000&delay=0" \
    -H "X-Api-Key: $API_KEY" 2>/dev/null)

END=$(date +%s%N)
TOTAL_MS=$(( (END - START) / 1000000 ))
TTFB_MS=$(( (FIRST_BYTE_TIME - START) / 1000000 ))

echo "  Chunks received:        $CHUNK_COUNT"
echo "  Time to first byte:     ${TTFB_MS}ms"
echo "  Total stream time:      ${TOTAL_MS}ms"
if [ "$CHUNK_COUNT" -gt 0 ] && [ "$TOTAL_MS" -gt 0 ]; then
    THROUGHPUT=$(( CHUNK_COUNT * 1000 / TOTAL_MS ))
    echo "  Chunk throughput:       ${THROUGHPUT} chunks/sec"
fi

echo ""
echo "--- Concurrent streams: 10 simultaneous SSE connections ---"
echo ""

START=$(date +%s%N)
for i in $(seq 1 10); do
    curl -sN "http://localhost:8000/apps/sse-bench/stream?chunks=100&delay=1" \
        -H "X-Api-Key: $API_KEY" > /dev/null 2>&1 &
    PIDS+=($!)
done
wait "${PIDS[@]}" 2>/dev/null || true
END=$(date +%s%N)
TOTAL_MS=$(( (END - START) / 1000000 ))

echo "  10 concurrent streams (100 chunks each)"
echo "  Total wall time:        ${TOTAL_MS}ms"
echo "  Expected (sequential):  ~1000ms"
echo ""

echo "================================================================="
