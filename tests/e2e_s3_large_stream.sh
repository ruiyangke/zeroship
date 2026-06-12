#!/usr/bin/env bash
# ============================================================================
# e2e_s3_large_stream.sh — ISS-32 stress: a >4 GiB env.storage streaming
# round-trip against MinIO, at the u32::MAX integer-width boundary, proving
# memory-boundedness of the S3 multipart streaming path.
#
# This is the heavy sibling of e2e_s3_storage.sh (which only goes to 20 MiB).
# It exists to cover TWO things the 20 MiB E2E cannot:
#
#   1. The u32::MAX byte-count boundary. We drive gallery.putLarge with
#      sizeBytes = 5 GiB (5368709120), comfortably past 4 GiB = 4294967296
#      (u32::MAX+1) and well under the 32 GiB stream cap. If any length, part
#      offset, or content-length is held in a u32 anywhere on the multipart
#      path, the returned size truncates (5368709120 mod 2^32 = 1073741824) or
#      the upload corrupts. We assert the returned size == 5368709120 exactly,
#      and that the same byte count + rolling checksum come BACK on getLargeHash.
#
#   2. Memory-boundedness. We sample worker RSS once a second across the whole
#      upload and capture the PEAK. A correctly streaming multipart path holds
#      only ~(part size + isolate) live — a few hundred MB — regardless of
#      object size. If peak RSS climbs toward 5 GiB, the "stream" is buffering
#      the whole object = a bug. This is THE boundedness assertion.
#
# KNOWN RISK (the headline thing this harness measures): even on the
# `unlimited` plan (wall_timeout_ms = None), the worker /dispatch path
# (crates/worker/src/handler.rs::wall_limit) FALLS BACK to a hardcoded 30s
# wall limit via `.unwrap_or(Duration::from_secs(30))`. recv_with_timeout
# then trips the cancel flag and returns HTTP 504 "request timed out". A
# multi-GB single-request upload may well exceed 30s and be CUT OFF. We use a
# long curl --max-time (1800s) precisely so curl is NOT the thing that cuts
# it — we want to observe whether the WORKER does. Whatever happens, we report
# it as evidence (elapsed, bytes-to-MinIO, peak RSS), NOT silently bump a knob.
#
# Stack bring-up mirrors e2e_s3_storage.sh verbatim (sources tests/lib/
# e2e_stack.sh for preflight + PAT mint; inlines the same PG + Liquibase + S3
# control/worker/gateway boot) but on its OWN port band + container names so
# the two harnesses never collide.
#
# Skips CLEANLY (exit 0) when docker is unavailable.
#
# Usage:
#   ./tests/e2e_s3_large_stream.sh
#   SIZE_BYTES=8589934592 ./tests/e2e_s3_large_stream.sh   # override (8 GiB)
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"

PASS=0; FAIL=0
declare -a ROWS
gib() { node -e 'process.stdout.write((Number(process.argv[1])/1073741824).toFixed(2))' "$1"; }
pass()  { PASS=$((PASS+1)); echo "  ✓ $1"; ROWS+=("PASS|$1"); }
fail()  { FAIL=$((FAIL+1)); echo "  ✗ $1"; ROWS+=("FAIL|$1"); }
note()  { echo "  • $1"; ROWS+=("NOTE|$1"); }

echo "============================================"
echo "  zeroship E2E — >4 GiB S3 multipart streaming round-trip (MinIO)"
echo "============================================"

# --- docker gate: skip cleanly when unavailable ----------------------------
if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  echo "  ⚠ SKIP: docker unavailable — large-stream S3 E2E needs a MinIO container."
  exit 0
fi

# --- preflight: binaries + built example -----------------------------------
for b in zeroship zeroship-control zeroship-gate zeroship-worker; do
  [ -x "$BIN/$b" ] || { echo "missing $BIN/$b — run: cargo build --release"; exit 2; }
done
ST_ZSHIP="$ROOT/examples/storage-gallery/dist/app.zship"
[ -f "$ST_ZSHIP" ] || { echo "missing $ST_ZSHIP — (cd examples/storage-gallery && pnpm install && pnpm build)"; exit 2; }

# ---------------------------------------------------------------------------
# The object size. 5 GiB by default — past u32::MAX+1 (4 GiB), under 32 GiB.
# ---------------------------------------------------------------------------
SIZE_BYTES="${SIZE_BYTES:-5368709120}"   # 5 GiB
U32_MAX_PLUS1=4294967296                  # 4 GiB = u32::MAX+1
# chunkBytes MUST stay <= the runtime's per-stream backpressure cap
# (crates/runtime/src/core/channel.rs::DEFAULT_STREAM_BUFFER_CAP = 4 MiB):
# a SINGLE pull() chunk larger than that cap trips StreamPushResult::Full on
# the first push and the upload fails instantly with HTTP 500 "upload stream
# exceeded the buffer backpressure cap" — before any S3 part is sent and
# before the 30s wall_limit is even reached. 1 MiB (the storage-gallery
# server default) sits well under the cap and exercises the real multi-wave
# pause/resume backpressure path across the whole 5 GiB. See the FINDING note
# emitted in Stage 5 for why this is a genuine ceiling, not just a knob.
CHUNK_BYTES="${CHUNK_BYTES:-1048576}"     # 1 MiB pull() chunks (<= 4 MiB cap)
SEED=7

# ---------------------------------------------------------------------------
# MinIO + stack ports — OWN band, offset from e2e_s3_storage.sh (920x/91xx/
# 80xx/546x) so the two can run back-to-back without colliding.
# ---------------------------------------------------------------------------
MINIO_CONTAINER="zs-e2e-s3-large-minio"
MINIO_PORT=9211
MINIO_ACCESS="minioadmin"
MINIO_SECRET="minioadmin"
MINIO_BUCKET="zeroship-e2e-large"
MINIO_ENDPOINT="http://127.0.0.1:$MINIO_PORT"

export CONTROL_PORT=9141
export WORKER_PORT=8191
export GATE_PORT=8031
export PG_PORT=5471
export PG_CONTAINER="zs-e2e-s3-large-pg"
export WORKER_THREADS=2

RSS_SAMPLER_PID=""
minio_cleanup() { docker rm -f "$MINIO_CONTAINER" >/dev/null 2>&1 || true; }

cleanup() {
  echo ""
  echo "=== Cleanup ==="
  [ -n "$RSS_SAMPLER_PID" ] && kill "$RSS_SAMPLER_PID" 2>/dev/null || true
  if declare -F stack_down >/dev/null 2>&1; then stack_down; fi
  minio_cleanup
  echo "  stack down, ephemeral PG + MinIO removed"
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 1: MinIO up + bucket created ==="
minio_cleanup
docker run -d --name "$MINIO_CONTAINER" \
  -p "$MINIO_PORT:9000" \
  -e "MINIO_ROOT_USER=$MINIO_ACCESS" \
  -e "MINIO_ROOT_PASSWORD=$MINIO_SECRET" \
  minio/minio server /data >/dev/null 2>&1 \
  || { fail "failed to start MinIO container"; exit 1; }

MINIO_READY=0
for _ in $(seq 1 40); do
  if docker exec "$MINIO_CONTAINER" mc alias set local "http://127.0.0.1:9000" "$MINIO_ACCESS" "$MINIO_SECRET" >/dev/null 2>&1; then
    if docker exec "$MINIO_CONTAINER" mc mb -p "local/$MINIO_BUCKET" >/dev/null 2>&1; then
      MINIO_READY=1; break
    fi
  fi
  sleep 0.5
done
[ "$MINIO_READY" = "1" ] && pass "MinIO ready on :$MINIO_PORT, bucket '$MINIO_BUCKET' created" \
  || { fail "MinIO did not become ready / bucket create failed"; docker logs "$MINIO_CONTAINER" 2>&1 | tail -20; exit 1; }

# ---------------------------------------------------------------------------
# S3 URLs — same shape as e2e_s3_storage.sh (minio provider, path style,
# loopback plaintext, checksum=none), distinct prefixes for deploy vs storage.
# ---------------------------------------------------------------------------
BLOB_S3="s3://$MINIO_BUCKET/deploy?provider=minio&endpoint=$MINIO_ENDPOINT&region=us-east-1&style=path&dev_http=true&checksum=none"
STORAGE_S3="s3://$MINIO_BUCKET/storage?provider=minio&endpoint=$MINIO_ENDPOINT&region=us-east-1&style=path&dev_http=true&checksum=none"

export AWS_ACCESS_KEY_ID="$MINIO_ACCESS"
export AWS_SECRET_ACCESS_KEY="$MINIO_SECRET"
unset AWS_SESSION_TOKEN 2>/dev/null || true

# shellcheck source=tests/lib/e2e_stack.sh
. "$ROOT/tests/lib/e2e_stack.sh"

echo ""
echo "=== Stage 2: ephemeral PG + Liquibase, then control/worker/gateway on s3:// ==="
stack_preflight || { fail "preflight failed"; exit 1; }

WORK="$(mktemp -d -t zs-e2e-s3-large-XXXXXX)"
mkdir -p "$WORK/blob-cache"
PIDFILE="$WORK/pids"; : > "$PIDFILE"
DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"
export WORK PIDFILE DBURL

docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
docker run --name "$PG_CONTAINER" -d -p "$PG_PORT:5432" \
  -e POSTGRES_PASSWORD=zeroship -e POSTGRES_USER=postgres -e POSTGRES_DB=zeroship \
  postgres:16 -c max_connections=300 >/dev/null || { fail "docker run postgres failed"; exit 1; }
for _ in $(seq 1 30); do docker exec "$PG_CONTAINER" pg_isready -U postgres >/dev/null 2>&1 && break; sleep 1; done
docker exec "$PG_CONTAINER" pg_isready -U postgres >/dev/null 2>&1 && pass "ephemeral PG ready on :$PG_PORT" || { fail "PG never became ready"; exit 1; }

[ -f "$ROOT/ops/postgres-init.sql" ] && docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 < "$ROOT/ops/postgres-init.sql" >/dev/null 2>&1 \
  && pass "applied ops/postgres-init.sql" || true

MIG_LOG="$WORK/liquibase.log"
if docker run --rm --network host -v "$ROOT/db/changelog:/liquibase/changelog" \
    liquibase/liquibase:4.31 \
    --url="jdbc:postgresql://localhost:$PG_PORT/zeroship" \
    --username=postgres --password=zeroship \
    --changelog-file=changelog/db.changelog-master.yaml update > "$MIG_LOG" 2>&1; then
  pass "Liquibase changelog applied cleanly from scratch"
else
  fail "Liquibase migration FAILED (see $MIG_LOG)"; tail -20 "$MIG_LOG"; exit 1
fi

openssl genpkey -algorithm ed25519 -out "$WORK/signing-key.pem" 2>/dev/null
chmod 600 "$WORK/signing-key.pem"
for p in $CONTROL_PORT $WORKER_PORT $GATE_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done

# control
"$BIN/zeroship-control" --port "$CONTROL_PORT" --db "$DBURL" \
  --blob-store "$BLOB_S3" --signing-key-file "$WORK/signing-key.pem" \
  --dev-insecure > "$WORK/control.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$CONTROL_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$CONTROL_PORT/health" >/dev/null 2>&1 && pass "control healthy (blob-store=s3)" || { fail "control unhealthy"; tail -30 "$WORK/control.log"; exit 1; }

# worker — capture its PID explicitly for RSS sampling
"$BIN/zeroship-worker" --port "$WORKER_PORT" --worker-threads "$WORKER_THREADS" \
  --control "http://localhost:$CONTROL_PORT" --db "$DBURL" \
  --storage-url "$STORAGE_S3" \
  --blob-store "$BLOB_S3" --poll-interval 2 --dev-insecure > "$WORK/worker.log" 2>&1 &
WORKER_PID=$!
echo $WORKER_PID >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$WORKER_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$WORKER_PORT/health" >/dev/null 2>&1 && pass "worker healthy (pid=$WORKER_PID, env.storage=s3)" || { fail "worker unhealthy"; tail -30 "$WORK/worker.log"; exit 1; }

# gateway
"$BIN/zeroship-gate" --port "$GATE_PORT" --control "http://localhost:$CONTROL_PORT" \
  --workers "http://localhost:$WORKER_PORT" --blob-store "$BLOB_S3" \
  --blob-cache-disk-root "$WORK/blob-cache" --db "$DBURL" --poll-interval 2 \
  --signing-key-file "$WORK/signing-key.pem" --dev-insecure > "$WORK/gate.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$GATE_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$GATE_PORT/health" >/dev/null 2>&1 && pass "gateway healthy (blob-store=s3)" || { fail "gateway unhealthy"; tail -30 "$WORK/gate.log"; exit 1; }

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 3: mint admin PAT (offline) ==="
mint_admin_pat || { fail "PAT mint failed"; exit 1; }

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 4: deploy storage-gallery on the 'unlimited' plan ==="
ST_APP_JSON="$(curl -s -X POST "http://localhost:$CONTROL_PORT/api/apps" \
  -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" \
  -d '{"name":"storage-gallery-large","plan_id":"unlimited"}')"
ST_APP="$(echo "$ST_APP_JSON" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{console.log(JSON.parse(s).id)}catch(e){console.log("")}})')"
if [ -z "$ST_APP" ]; then fail "create-app failed: $ST_APP_JSON"; exit 1; fi
ST_DEP="$("$BIN/zeroship" deploy "$ST_ZSHIP" --app="$ST_APP" --control="http://localhost:$CONTROL_PORT" --token="$PAT" 2>&1)"
if echo "$ST_DEP" | grep -q "deploy_hash"; then
  pass "deployed storage-gallery (plan=unlimited) → app $ST_APP"
else
  fail "deploy failed: $ST_DEP"; tail -30 "$WORK/control.log"; exit 1
fi
sleep 5  # let route + version sync to worker

# ---------------------------------------------------------------------------
# Dispatch helpers — direct to the worker /dispatch/<app> (unauthenticated
# loopback), exactly like e2e_s3_storage.sh. We do NOT go through the gateway.
# ---------------------------------------------------------------------------
envelope() {
  node -e 'process.stdout.write(JSON.stringify({method:"POST",url:"http://x/__zeroship/v1/"+process.argv[1],headers:[["content-type","application/json"]],body:JSON.stringify({json:JSON.parse(process.argv[2])})}))' "$1" "$2"
}
# dispatch_to <out-file> <proc> <json>  — writes body to <out-file>, echoes "<http_code> <elapsed_s>"
dispatch_to() {
  local out="$1" proc="$2" body="$3"
  curl -s -o "$out" -w '%{http_code} %{time_total}' --max-time 1800 \
    -X POST "http://localhost:$WORKER_PORT/dispatch/$ST_APP" \
    -H 'content-type: application/json' -d "$(envelope "$proc" "$body")"
}
jget_json() { node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);console.log((o.json&&o.json'"$1"')??"")}catch(e){console.log("")}})'; }

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 5: drive gallery.putLarge at $SIZE_BYTES bytes ($(gib "$SIZE_BYTES") GiB) ==="
echo "    u32::MAX+1 = $U32_MAX_PLUS1 (4 GiB) — this object is comfortably past it."
echo "    Sampling worker RSS (pid=$WORKER_PID) every 1s → $WORK/rss.log"

SKEY="big/stream-$(date +%s).bin"
RSS_LOG="$WORK/rss.log"; : > "$RSS_LOG"

# Background RSS sampler: VmRSS (kB) from /proc, peak tracked downstream.
(
  while kill -0 "$WORKER_PID" 2>/dev/null; do
    rss="$(awk '/VmRSS/{print $2}' /proc/$WORKER_PID/status 2>/dev/null)"
    [ -n "$rss" ] && echo "$(date +%s) $rss" >> "$RSS_LOG"
    sleep 1
  done
) &
RSS_SAMPLER_PID=$!

PUT_OUT="$WORK/put.json"
echo "    dispatching putLarge (curl --max-time 1800; the WORKER's 30s wall_limit is the thing under test)…"
PUT_META="$(dispatch_to "$PUT_OUT" "gallery.putLarge" "{\"key\":\"$SKEY\",\"sizeBytes\":$SIZE_BYTES,\"seed\":$SEED,\"chunkBytes\":$CHUNK_BYTES}")"
PUT_CODE="${PUT_META%% *}"; PUT_ELAPSED="${PUT_META##* }"

# Stop sampling the upload window; compute peak RSS so far.
kill "$RSS_SAMPLER_PID" 2>/dev/null || true; RSS_SAMPLER_PID=""
PEAK_RSS_KB="$(awk '{if($2>m)m=$2}END{print m+0}' "$RSS_LOG")"
PEAK_RSS_MB=$(( PEAK_RSS_KB / 1024 ))
SAMPLES="$(wc -l < "$RSS_LOG" | tr -d ' ')"
note "putLarge HTTP $PUT_CODE in ${PUT_ELAPSED}s; worker peak RSS over $SAMPLES samples = ${PEAK_RSS_MB} MiB (object = $(gib "$SIZE_BYTES") GiB)"

# How much actually reached MinIO (parts may still be an in-progress multipart;
# count both the finalized object and any multipart staging).
MINIO_OBJ_SIZE="$(docker exec "$MINIO_CONTAINER" mc stat --json "local/$MINIO_BUCKET/storage/$SKEY" 2>/dev/null | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{console.log(JSON.parse(s).size||0)}catch(e){console.log(0)}})')"
MINIO_OBJ_SIZE="${MINIO_OBJ_SIZE:-0}"

# Boundedness verdict — independent of pass/fail of the round-trip.
# A streaming multipart path should hold only ~(part size + isolate) live.
# Treat anything below 25% of the object (and below ~2 GiB absolute) as bounded.
BOUND_THRESHOLD_MB=$(( SIZE_BYTES / 1024 / 1024 / 4 ))
if [ "$PEAK_RSS_MB" -lt "$BOUND_THRESHOLD_MB" ] && [ "$PEAK_RSS_MB" -lt 2048 ]; then
  pass "MEMORY BOUNDED: peak worker RSS ${PEAK_RSS_MB} MiB ≪ object $(gib "$SIZE_BYTES") GiB (streaming did NOT buffer the whole object)"
else
  fail "MEMORY NOT BOUNDED: peak worker RSS ${PEAK_RSS_MB} MiB approaches the $(gib "$SIZE_BYTES") GiB object — streaming appears to be buffering"
fi

# ---------------------------------------------------------------------------
# Interpret the upload outcome.
# ---------------------------------------------------------------------------
PUT_SIZE=""; PUT_SUM=""
if [ "$PUT_CODE" = "200" ]; then
  PUT_SIZE="$(jget_json '.size' < "$PUT_OUT")"
  PUT_SUM="$(jget_json '.checksum' < "$PUT_OUT")"
  if [ "$PUT_SIZE" = "$SIZE_BYTES" ]; then
    pass "NO u32 TRUNCATION: putLarge returned size=$PUT_SIZE == $SIZE_BYTES (5 GiB survived the multipart path intact)"
  else
    fail "u32 TRUNCATION SUSPECTED: putLarge returned size=$PUT_SIZE, expected $SIZE_BYTES (5368709120 mod 2^32 = 1073741824)"
  fi
  [ -n "$PUT_SUM" ] && pass "putLarge returned a non-empty rolling checksum ($PUT_SUM)" || fail "putLarge checksum empty"

  if [ "${MINIO_OBJ_SIZE:-0}" = "$SIZE_BYTES" ]; then
    pass "MinIO holds the finalized object at storage/$SKEY = $MINIO_OBJ_SIZE bytes (>4 GiB, multipart finalized)"
  else
    fail "MinIO object size mismatch: mc stat = ${MINIO_OBJ_SIZE} bytes, expected $SIZE_BYTES"
  fi
else
  # Non-200 — the headline known-risk case. Distinguish the 30s wall cut.
  WORKER_TAIL="$(grep -iE 'timed out|cancel|wall|timeout|504|abort|storage|multipart' "$WORK/worker.log" | tail -8)"
  PUT_BODY="$(head -c 400 "$PUT_OUT" 2>/dev/null)"
  if [ "$PUT_CODE" = "504" ] || echo "$PUT_BODY" | grep -qi "timed out"; then
    ELAPSED_INT="${PUT_ELAPSED%.*}"
    fail "WORKER CUT THE UPLOAD: HTTP $PUT_CODE 'request timed out' after ${PUT_ELAPSED}s — the hardcoded 30s wall_limit fired despite the 'unlimited' plan (wall_timeout=None). ~${MINIO_OBJ_SIZE} bytes reached MinIO before the cut."
    note "FINDING: crates/worker/src/handler.rs::wall_limit() falls back to Duration::from_secs(30) when runtime.wall_timeout() is None. The 'unlimited' plan's None must propagate to a no-wall-cap (or an inactivity/idle timeout) for streaming dispatch; a fixed wall cap makes large single-request streaming uploads impossible."
    echo "    --- worker.log evidence ---"
    echo "$WORKER_TAIL" | sed 's/^/    /'
  elif echo "$WORKER_TAIL$PUT_BODY" | grep -qi "backpressure cap\|exceeded the buffer"; then
    fail "BACKPRESSURE-CAP OVERFLOW: putLarge HTTP $PUT_CODE after ${PUT_ELAPSED}s — a pull() chunk exceeded the runtime's 4 MiB per-stream buffer cap (DEFAULT_STREAM_BUFFER_CAP)."
    note "FINDING: crates/runtime/src/core/channel.rs::DEFAULT_STREAM_BUFFER_CAP = 4 MiB is a HARD per-chunk ceiling for env.storage putStream. Any single ReadableStream chunk > 4 MiB trips StreamPushResult::Full on the first push (HTTP 500, before any S3 part is sent). chunkBytes MUST be <= 4 MiB; this harness uses 1 MiB. Worth documenting the cap in docs/reference/db/storage and/or having the SDK re-slice oversized chunks."
    echo "    --- worker.log evidence ---"
    echo "$WORKER_TAIL" | sed 's/^/    /'
  else
    fail "putLarge failed HTTP $PUT_CODE (not a clean 504). body: $PUT_BODY"
    echo "$WORKER_TAIL" | sed 's/^/    /'
  fi
fi

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 6: drive gallery.getLargeHash — full >4 GiB download round-trip ==="
if [ "$PUT_CODE" = "200" ] && [ "$PUT_SIZE" = "$SIZE_BYTES" ]; then
  GET_OUT="$WORK/get.json"
  GET_META="$(dispatch_to "$GET_OUT" "gallery.getLargeHash" "{\"key\":\"$SKEY\"}")"
  GET_CODE="${GET_META%% *}"; GET_ELAPSED="${GET_META##* }"
  GET_SIZE="$(jget_json '.size' < "$GET_OUT")"
  GET_SUM="$(jget_json '.checksum' < "$GET_OUT")"
  note "getLargeHash HTTP $GET_CODE in ${GET_ELAPSED}s"
  if [ "$GET_CODE" = "200" ] && [ "$GET_SIZE" = "$PUT_SIZE" ] && [ "$GET_SUM" = "$PUT_SUM" ]; then
    pass "ROUND-TRIP OK: download streamed $GET_SIZE bytes; size+checksum BYTE-MATCH the upload (no truncation, full >4 GiB round-trip)"
  elif [ "$GET_CODE" = "504" ] || head -c 400 "$GET_OUT" 2>/dev/null | grep -qi "timed out"; then
    fail "DOWNLOAD CUT: getLargeHash HTTP $GET_CODE after ${GET_ELAPSED}s — same 30s wall_limit fired on the download path."
  else
    fail "round-trip mismatch: up(size=$PUT_SIZE sum=$PUT_SUM) down(code=$GET_CODE size=$GET_SIZE sum=$GET_SUM)"
  fi
else
  note "SKIPPED getLargeHash: upload did not complete a full $SIZE_BYTES-byte object (see Stage 5)."
fi

# ---------------------------------------------------------------------------
echo ""
echo "============================================"
echo "  PASS/FAIL table"
echo "============================================"
printf "  %-6s %s\n" "RESULT" "ASSERTION"
printf "  %-6s %s\n" "------" "---------"
for row in "${ROWS[@]}"; do
  printf "  %-6s %s\n" "${row%%|*}" "${row#*|}"
done
echo "============================================"
echo "  Totals: $PASS passed, $FAIL failed"
echo "  Object: $SIZE_BYTES bytes ($(gib "$SIZE_BYTES") GiB) | peak worker RSS: ${PEAK_RSS_MB:-?} MiB | upload HTTP $PUT_CODE in ${PUT_ELAPSED:-?}s"
echo "============================================"
[ $FAIL -eq 0 ] && exit 0 || exit 1
