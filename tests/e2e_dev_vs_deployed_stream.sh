#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Streaming, dev vs deployed: do SSE chunk BOUNDARIES survive the gateway?
#
# The failure this guards is a proxy that buffers the response body. Such a
# proxy still delivers every chunk, in order, with identical bytes -- so a test
# that compared only the assembled result would pass while streaming was
# completely broken. Nothing else in this repo would notice.
#
# So the fixture (examples/stream-probe) paces its emission: 5 chunks 200ms
# apart. That makes TIME-TO-FIRST-CHUNK the discriminator. Streaming delivers
# chunk 1 at ~200ms and chunk 5 at ~1000ms; buffering delivers all five at
# ~1000ms. The assertion is ttfc < total/2, which the streaming case clears by
# 2.5x and the buffering case fails outright -- not a tight timing margin.
#
# Proven load-bearing by dropping curl's -N (unbuffered) flag, which makes the
# client aggregate exactly as a buffering proxy would: both sides then report
# ttfc == total and the run fails. See the MUTATION note at the bottom.
#
# Prereqs (docs/runbooks/local-dev.md):
#   cargo build --release -p zeroship-control -p zeroship-worker -p zeroship-gateway -p zeroship --bins
#   cargo build --release -p zeroship-migrate-adapter --features platform-cli --bin zeroship-platform-migrate
#   docker (Postgres on :5440 as compose-postgres-1)
#   pnpm install in examples/stream-probe
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"
APP_DIR="$ROOT/examples/stream-probe"
ZSHIP="$APP_DIR/dist/app.zship"
WORK="$(mktemp -d)"

PG_CONTAINER="${PG_CONTAINER:-compose-postgres-1}"
PG_USER="${PG_USER:-postgres}"
PG_DB="${PG_DB:-zeroship_stream}"
DB_URL="${DATABASE_URL:-postgres://postgres:zeroship@localhost:5440/$PG_DB}"
CONTROL_PORT="${CONTROL_PORT:-9394}"
WORKER_PORT="${WORKER_PORT:-8394}"
GATE_PORT="${GATE_PORT:-8304}"
DEV_PORT="${DEV_PORT:-3061}"
export ZEROSHIP_DEV_INSECURE=1
export WORKER_KEY="${WORKER_KEY:-stream-worker-key-0123456789abcdefgh}"
APP_NAME="streamp"
# Set to 1 to run the buffering mutation described in the header.
MUTATE_BUFFERED="${MUTATE_BUFFERED:-0}"

PASS=0; FAIL=0; PIDS=()
ok() { PASS=$((PASS+1)); echo "  ok   $1"; }
no() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  lsof -ti :"$DEV_PORT" 2>/dev/null | xargs -r kill 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# Are both sides the same BUILD? dev runs vite, which spawns `zeroship serve`;
# the deployed side runs separate server binaries. A partial rebuild leaves
# them at different commits and this harness would report the version skew as
# a dev-vs-deployed divergence. That already happened once, in the storage
# walk, where a worker predating 3e7e5e387 produced an error that read exactly
# like an S3 list defect. See tests/lib/binary_freshness.sh.
# shellcheck source=lib/binary_freshness.sh
source "$ROOT/tests/lib/binary_freshness.sh"
zs_check_binary_freshness "$ROOT" "$BIN" \
  "crates/runtime/src crates/worker/src crates/gateway/src crates/control/src sdks/rpc/src" \
  "zeroship zeroship-worker zeroship-gate zeroship-control dev-provision" \
  || { [ "$?" -eq 2 ] && exit 2; }

# Drive the paced stream and print "<ms> <line>" per chunk. -N is what makes
# curl emit each chunk as it arrives; without it curl aggregates and every
# line appears at the end, which is the mutation.
drive() {
  local url="$1" key="${2:-}" out="$3"
  local hdr=() nflag=(-N)
  [ -n "$key" ] && hdr=(-H "X-Api-Key: $key")
  [ "$MUTATE_BUFFERED" = "1" ] && nflag=()
  local start; start=$(date +%s%N)
  curl -sS "${nflag[@]}" -m 25 -X POST -H 'content-type: application/json' \
    -H 'accept: text/event-stream' "${hdr[@]}" "$url" -d '{"json":{}}' 2>&1 \
  | while IFS= read -r line; do
      local now; now=$(date +%s%N)
      printf '%s %s\n' $(( (now - start) / 1000000 )) "$line"
    done > "$out"
}

# ttfc < total/2 means chunks arrived spread out; ttfc == total means they all
# landed together, which is what buffering looks like from the client.
judge() {
  local label="$1" f="$2"
  local ticks ttfc total
  ticks=$(grep -c '"mark":"TICK"' "$f" || true)
  [ "$ticks" -eq 5 ] && ok "$label: 5 chunks arrived" || { no "$label: got $ticks chunks, want 5"; return; }
  grep -q '"i":1' "$f" && grep -q '"i":5' "$f" && ok "$label: first and last chunk present" \
    || no "$label: chunk numbering incomplete"
  ttfc=$(grep '"mark":"TICK"' "$f" | head -1 | cut -d' ' -f1)
  total=$(grep '"mark":"TICK"' "$f" | tail -1 | cut -d' ' -f1)
  echo "      ttfc=${ttfc}ms total=${total}ms"
  if [ "$total" -gt 0 ] && [ "$((ttfc * 2))" -lt "$total" ]; then
    ok "$label: chunks arrived INCREMENTALLY (ttfc well under half of total)"
  else
    no "$label: chunks arrived TOGETHER (ttfc=${ttfc}ms vs total=${total}ms) -- buffered, not streamed"
  fi
}

echo "=== streaming: dev vs deployed ==="
[ "$MUTATE_BUFFERED" = "1" ] && echo "  (MUTATION ACTIVE: curl -N dropped, expect both sides to fail)"

( cd "$APP_DIR" && pnpm build ) > "$WORK/build.log" 2>&1
[ -f "$ZSHIP" ] && ok "built stream-probe .zship" || { no "build produced no .zship"; tail -20 "$WORK/build.log"; exit 1; }
# A missing "use server" builds clean and yields zero procedures (see #167), so
# check the bundle actually carries them rather than trusting the exit code.
grep -q "2 server functions" "$WORK/build.log" && ok "server bundle carries both procedures" \
  || no "server bundle did not report 2 server functions (missing \"use server\"?)"

echo "=== dev side"
lsof -ti :"$DEV_PORT" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
( cd "$APP_DIR" && ./node_modules/.bin/vite > "$WORK/dev.log" 2>&1 ) & PIDS+=($!)
for _ in $(seq 1 20); do
  curl -sf -o /dev/null -m 2 -X POST -H 'content-type: application/json' \
    "http://localhost:$DEV_PORT/__zeroship/v1/probe.ping" -d '{"json":{}}' && break
  sleep 2
done
curl -sf -o /dev/null -m 5 -X POST -H 'content-type: application/json' \
  "http://localhost:$DEV_PORT/__zeroship/v1/probe.ping" -d '{"json":{}}' \
  && ok "dev app reachable" || { no "dev app never came up"; tail -20 "$WORK/dev.log"; exit 1; }
drive "http://localhost:$DEV_PORT/__zeroship/v1/probe.ticks" "" "$WORK/dev.txt"
judge "dev" "$WORK/dev.txt"

echo "=== deployed side"
for p in $CONTROL_PORT $WORKER_PORT $GATE_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "DROP DATABASE IF EXISTS $PG_DB WITH (FORCE)" >/dev/null 2>&1 || true
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "CREATE DATABASE $PG_DB" >/dev/null 2>&1 || true
"$BIN/zeroship-platform-migrate" --database-url "$DB_URL" --migrations-dir "$ROOT/db/migrations-ts" \
  --project-schema zeroship --project-id zeroship > "$WORK/migrate.log" 2>&1 \
  || { no "platform migrations failed"; tail -20 "$WORK/migrate.log"; exit 1; }
"$BIN/zeroship-control" --port "$CONTROL_PORT" --db "$DB_URL" --blob-store "$WORK/bundles" \
  --control-key st-ck --master-key st-mk > "$WORK/control.log" 2>&1 & PIDS+=($!)
sleep 4
"$BIN/zeroship-worker" --port "$WORKER_PORT" --worker-threads 2 --control "http://localhost:$CONTROL_PORT" \
  --control-key st-ck --blob-store "$WORK/bundles" --poll-interval 2 > "$WORK/worker.log" 2>&1 & PIDS+=($!)
sleep 3
openssl rand -base64 48 > "$WORK/gate-secret"; chmod 600 "$WORK/gate-secret"
"$BIN/zeroship-gate" --port "$GATE_PORT" --control "http://localhost:$CONTROL_PORT" --control-key st-ck \
  --workers "http://localhost:$WORKER_PORT" --blob-store "$WORK/bundles" \
  --gateway-broker-secret-file "$WORK/gate-secret" --poll-interval 2 > "$WORK/gate.log" 2>&1 & PIDS+=($!)
sleep 4
curl -sf "http://localhost:$GATE_PORT/health" >/dev/null && ok "stack healthy" \
  || { no "stack did not come up"; tail -20 "$WORK/gate.log"; exit 1; }

OUT=$("$BIN/dev-provision" --db "$DB_URL" --blob-store "$WORK/bundles" --name "$APP_NAME" --zship "$ZSHIP" 2>&1)
KEY=$(echo "$OUT" | awk -F= '$1=="api_key"{print $2}')
[ -n "$KEY" ] && ok "deployed stream-probe" || { no "provision: $OUT"; exit 1; }
sleep 6
curl -sf -o /dev/null -m 10 -X POST -H 'content-type: application/json' -H "X-Api-Key: $KEY" \
  "http://localhost:$GATE_PORT/apps/$APP_NAME/__zeroship/v1/probe.ping" -d '{"json":{}}' \
  && ok "deployed app reachable" || no "deployed app did not answer ping"
drive "http://localhost:$GATE_PORT/apps/$APP_NAME/__zeroship/v1/probe.ticks" "$KEY" "$WORK/deployed.txt"
judge "deployed" "$WORK/deployed.txt"

echo ""
echo "  streaming: $PASS passed, $FAIL failed"
echo "  MUTATION: re-run with MUTATE_BUFFERED=1 to drop curl -N; both sides must then FAIL"
[ "$FAIL" -eq 0 ]
