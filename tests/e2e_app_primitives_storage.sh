#!/usr/bin/env bash
# ============================================================================
# e2e_app_primitives_storage.sh - exercise env.storage through the worker.
#
# Creates and deploys storage-gallery through control, then checks put, get,
# list, delete and missing-object reads over authenticated worker dispatch.
# KV deployment coverage lives under examples/kv-dashboard/tests/.
#
# Requires Docker, Node with workspace dependencies, OpenSSL, zstd, built
# platform release binaries, and a built examples/storage-gallery/dist/app.zship.
# Usage: ./tests/e2e_app_primitives_storage.sh
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"
# shellcheck source=tests/lib/runtime_secrets.sh
source "$ROOT/tests/lib/runtime_secrets.sh"
STRICT="${STRICT:-0}"

# --- ports (offset again to avoid colliding with e2e_app_primitives.sh)
ZEROSHIP_CONTROL_PORT=9098
ZEROSHIP_WORKER_PORT=8086
ZEROSHIP_GATEWAY_PORT=8002
PG_PORT=5444
PG_CONTAINER="zs-e2e-storage-pg"

JOSE_JS="$ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"

PASS=0; FAIL=0; KNOWN=0
PIDS=()
WORK=""

pass()  { PASS=$((PASS+1));  echo "  ✓ $1"; }
fail()  { FAIL=$((FAIL+1));  echo "  ✗ $1"; }
known() { KNOWN=$((KNOWN+1)); echo "  ⚠ KNOWN-FAIL: $1"; if [ "$STRICT" = "1" ]; then FAIL=$((FAIL+1)); fi; }

cleanup() {
  echo ""
  echo "=== Cleanup ==="
  for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true
  docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
  [ -n "$WORK" ] && rm -rf "$WORK"
  echo "  stack down, ephemeral PG removed"
}
trap cleanup EXIT

jget() { node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);process.stdout.write(String(o$1??'')+'\n')}catch(e){console.log('')}})"; }

# Build a worker /dispatch request envelope for an RPC procedure `id`,
# carrying `{json: <args>}` as the body. The worker maps the URL path
# /__zeroship/v1/<id> to the procedure by its declared `{ id }`.
# The worker decodes a length-prefixed binary frame, not a JSON envelope with
# the body inline. This used to build the latter, so every dispatch below was
# refused with "dispatch metadata too large" and env.storage was never exercised over the edge.
# shellcheck source=tests/lib/dispatch_frame.sh
. "$ROOT/tests/lib/dispatch_frame.sh"

# POST an envelope to the worker /dispatch for $APP_ID; echo "<body>\n<code>".
dispatch() {
  local app="$1" id="$2" args="$3"
  local frame="$WORK/frame-$$.bin"
  zs_rpc_frame "$frame" "$id" "$args"
  curl -s -w '\n%{http_code}' -X POST "http://localhost:$ZEROSHIP_WORKER_PORT/dispatch/$app" \
    -H "Authorization: Bearer $E2E_STALE_WORKER_BEARER" \
    -H 'content-type: application/octet-stream' --data-binary @"$frame"
}

echo "============================================"
echo "  zeroship E2E — env.storage over the worker"
echo "============================================"

# --- preflight -------------------------------------------------------------
for b in zeroship zeroship-control zeroship-gate zeroship-worker; do
  [ -x "$BIN/$b" ] || { echo "missing $BIN/$b - run cargo build --release"; exit 2; }
done
[ -f "$ROOT/packages/zero-migrate-cli/dist/cli-bin.js" ] || { echo "missing the zero-migrate CLI - run: pnpm install && pnpm build"; exit 2; }
ST_ZSHIP="$ROOT/examples/storage-gallery/dist/app.zship"
[ -f "$ST_ZSHIP" ] || { echo "missing $ST_ZSHIP — (cd examples/storage-gallery && pnpm install && pnpm build)"; exit 2; }
[ -f "$JOSE_JS" ] || { echo "missing jose at $JOSE_JS"; exit 2; }
command -v docker >/dev/null || { echo "docker required"; exit 2; }
command -v openssl >/dev/null || { echo "openssl required"; exit 2; }

WORK="$(mktemp -d -t zs-e2e-storage-XXXXXX)"
mkdir -p "$WORK/blobs" "$WORK/blob-cache" "$WORK/storage"

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 1: ephemeral Postgres + migrations ==="
docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
docker run --name "$PG_CONTAINER" -d -p "$PG_PORT:5432" \
  -e POSTGRES_PASSWORD=zeroship -e POSTGRES_USER=postgres -e POSTGRES_DB=zeroship \
  postgres:16 -c max_connections=300 >/dev/null
# Readiness = three CONSECUTIVE successful queries, not one pg_isready. There
# is a window where pg_isready reports ready and a query still fails, because
# the entrypoint tears down its initdb-phase server and restarts it; a single
# check lets a run through that window and it fails later as "PG never became
# ready". Same fix as stack_pg_up in tests/lib/e2e_stack.sh, which measured it.
PG_OK=0
for i in $(seq 1 60); do
  if docker exec "$PG_CONTAINER" psql -U postgres -d zeroship -tAc 'select 1' >/dev/null 2>&1; then
    PG_OK=$((PG_OK + 1))
    [ "$PG_OK" -ge 3 ] && break
  else
    PG_OK=0
  fi
  sleep 1
done
[ "$PG_OK" -ge 3 ] && pass "ephemeral PG ready on :$PG_PORT" || { fail "PG never became ready"; docker logs "$PG_CONTAINER" 2>&1 | tail -20; exit 1; }

if [ -f "$ROOT/deploy/ops/postgres-init.sql" ]; then
  docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 < "$ROOT/deploy/ops/postgres-init.sql" >/dev/null 2>&1 \
    && pass "applied deploy/ops/postgres-init.sql" || fail "postgres-init.sql failed"
fi

DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"
MIG_LOG="$WORK/migrate.log"
if zs_platform_migrate "$DBURL" \
    --migrations-dir "$ROOT/db/migrations-ts" \
    --project-schema zeroship --project-id zeroship > "$MIG_LOG" 2>&1; then
  pass "platform migrations applied cleanly from scratch (zeroship-platform-migrate)"
else
  fail "zeroship-platform-migrate FAILED (see $MIG_LOG)"; tail -20 "$MIG_LOG"; exit 1
fi

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 2: boot secured stack (worker with --storage-url) ==="

openssl genpkey -algorithm ed25519 -out "$WORK/signing-key.pem" 2>/dev/null
chmod 600 "$WORK/signing-key.pem"

for p in $ZEROSHIP_CONTROL_PORT $ZEROSHIP_WORKER_PORT $ZEROSHIP_GATEWAY_PORT; do lsof -ti :$p 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done

ZEROSHIP_GATEWAY_SIGNING_KEY_FILE="$WORK/signing-key.pem"
# The issuer control verifies the admin bearer against, on the same key the
# gateway signs with. Up BEFORE control: control reads the issuer once at boot.
e2e_platform_op_up "$WORK/signing-key.pem" "$WORK" || exit 1
PIDS+=($E2E_PLATFORM_OP_PID)
ZEROSHIP_GATEWAY_BROKER_SECRET_FILE="$WORK/gate-secret"
e2e_export_runtime_secrets "$WORK" || exit 1
e2e_export_database_urls "$DBURL"
"$BIN/zeroship-control" --port $ZEROSHIP_CONTROL_PORT \
  --blob-store "$WORK/blobs" \
 > "$WORK/control.log" 2>&1 &
PIDS+=($!)
for i in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_CONTROL_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_CONTROL_PORT/readyz" >/dev/null 2>&1 && pass "control healthy" || { fail "control unhealthy"; tail -20 "$WORK/control.log"; exit 1; }

e2e_start_cdc_relay "$BIN/zeroship-data-cdc-server" || exit 1
# Worker storage uses the fixture's local directory.
"$BIN/zeroship-worker" --port $ZEROSHIP_WORKER_PORT --threads 2 \
  --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
 --storage-url "$WORK/storage" \
  --blob-store "$WORK/blobs" --poll-interval 2 > "$WORK/worker.log" 2>&1 &
PIDS+=($!)
for i in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" >/dev/null 2>&1 && pass "worker healthy (storage configured)" || { fail "worker unhealthy"; tail -20 "$WORK/worker.log"; exit 1; }

# Broker master secret. cd54028e7 made the gateway refuse to start without one,
# and this harness was never updated, so it has been unable to get past "gateway
# healthy" since.
openssl rand -base64 48 > "$WORK/gate-secret"
chmod 600 "$WORK/gate-secret"

"$BIN/zeroship-gate" --port $ZEROSHIP_GATEWAY_PORT --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
  --worker-urls "http://localhost:$ZEROSHIP_WORKER_PORT" --blob-store "$WORK/blobs" \
  --blob-cache-disk-root "$WORK/blob-cache" --poll-interval 2 \
  --broker-secret-file "$WORK/gate-secret" \
 > "$WORK/gate.log" 2>&1 &
PIDS+=($!)
for i in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" >/dev/null 2>&1 && pass "gateway healthy" || { fail "gateway unhealthy"; tail -20 "$WORK/gate.log"; exit 1; }

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 3: mint admin platform bearer (offline) ==="
# The scope string is the deleted permission_tokens policy's action list,
# one-for-one: it becomes the token policy control intersects with the
# owner's own authority (TOKEN and USER).
SCOPE="apps:read apps:write apps:deploy apps:archive deployments:read env:read env:write secrets:read secrets:write"

OWNER="$(node -e 'console.log(require("crypto").randomUUID())')"

docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id, email, name, email_verified_at)
VALUES ('$OWNER', 'e2e-$OWNER@zeroship.test'::citext, 'E2E Admin', NOW());
SQL

ADMIN_TOKEN="$(e2e_mint_platform_bearer "$OWNER" "$SCOPE")"
[ "$(echo -n "$ADMIN_TOKEN" | awk -F. '{print NF}')" = "3" ] && pass "minted platform bearer" || { fail "bearer mint failed: $ADMIN_TOKEN"; exit 1; }

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 4: create and deploy storage-gallery ==="
ST_APP_JSON="$(curl -s -X POST "http://localhost:$ZEROSHIP_CONTROL_PORT/api/apps" \
  -H 'Content-Type: application/json' -H "Authorization: Bearer $ADMIN_TOKEN" -d '{"name":"storage-gallery-e2e"}')"
ST_APP="$(echo "$ST_APP_JSON" | jget '.id')"
[ -n "$ST_APP" ] && pass "created app storage-gallery-e2e ($ST_APP)" || { fail "create storage app: $ST_APP_JSON"; exit 1; }

STDEP="$("$BIN/zeroship" deploy "$ST_ZSHIP" --app="$ST_APP" --control="http://localhost:$ZEROSHIP_CONTROL_PORT" --token="$ADMIN_TOKEN" 2>&1)"
echo "$STDEP" | grep -q "deploy_hash" && pass "deployed storage-gallery .zship" || fail "storage deploy failed: $STDEP"
sleep 5   # let route + version sync to gateway + worker

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 5: env.storage over the worker /dispatch ==="
SKEY="gallery/hello-$(date +%s).txt"
STEXT="storage-edge-payload-$(date +%s)"
# put
PUT="$(dispatch "$ST_APP" "gallery.put" "{\"key\":\"$SKEY\",\"text\":\"$STEXT\",\"contentType\":\"text/plain\"}")"
PUTB="$(echo "$PUT" | head -1)"; PUTC="$(echo "$PUT" | tail -1)"
if [ "$PUTC" = "200" ] && echo "$PUTB" | grep -q '"json"'; then
  PUTSZ="$(echo "$PUTB" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{process.stdout.write(String(JSON.parse(s).json.size)+"\n")}catch(e){console.log("")}})')"
  pass "env.storage put: gallery.put stored $PUTSZ bytes at '$SKEY'"

  # get — assert the text round-trips
  GET="$(dispatch "$ST_APP" "gallery.get" "{\"key\":\"$SKEY\"}" | head -1)"
  GOT="$(echo "$GET" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);console.log(o.json.found?o.json.text:"")}catch(e){console.log("")}})')"
  if [ "$GOT" = "$STEXT" ]; then
    pass "env.storage get: gallery.get round-tripped the text payload"
  else
    fail "env.storage get mismatch: wanted '$STEXT' got '$GOT' (body=$GET)"
  fi

  # list — assert the key shows up
  LST="$(dispatch "$ST_APP" "gallery.list" '{"prefix":"gallery/"}' | head -1)"
  if echo "$LST" | grep -q "$SKEY"; then
    pass "env.storage list: gallery.list enumerated '$SKEY'"
  else
    fail "env.storage list missing key '$SKEY' (body=$LST)"
  fi

  # delete — then a get should report found:false
  DEL="$(dispatch "$ST_APP" "gallery.delete" "{\"key\":\"$SKEY\"}" | head -1)"
  DELED="$(echo "$DEL" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{process.stdout.write(String(JSON.parse(s).json.deleted)+"\n")}catch(e){console.log("")}})')"
  GET2="$(dispatch "$ST_APP" "gallery.get" "{\"key\":\"$SKEY\"}" | head -1)"
  GONE="$(echo "$GET2" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{process.stdout.write(String(JSON.parse(s).json.found)+"\n")}catch(e){console.log("")}})')"
  if [ "$DELED" = "true" ] && [ "$GONE" = "false" ]; then
    pass "env.storage delete: gallery.delete removed it (subsequent get found:false)"
  else
    fail "env.storage delete did not remove object (deleted=$DELED found-after=$GONE del=$DEL get=$GET2)"
  fi
else
  ERR="$(grep -iE 'env.storage|StoragePlugin|Cannot find module|storage' "$WORK/worker.log" | tail -1)"
  fail "env.storage gallery.put failed over /dispatch. HTTP $PUTC; err: ${ERR:-$PUTB}"
fi

# ---------------------------------------------------------------------------
echo ""
echo "============================================"
echo "  Results: $PASS passed, $FAIL failed, $KNOWN known-fail"
echo "============================================"
[ $FAIL -eq 0 ] && exit 0 || exit 1
