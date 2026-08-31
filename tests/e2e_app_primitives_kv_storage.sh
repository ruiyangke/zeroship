#!/usr/bin/env bash
# ============================================================================
# e2e_app_primitives_kv_storage.sh — close G2/G4: exercise env.kv and
# env.storage through the REAL multi-node edge (worker → V8 → native plugin),
# the same way e2e_app_primitives.sh now proves env.db (Stage 5c GREEN after
# ISS-63 + ISS-66).
#
# What this does, end to end, against a CLEAN ephemeral stack:
#   1. Stand up a fresh ephemeral Postgres + deploy/ops/postgres-init.sql + the FULL
#      platform migration set. (Control still needs a DB for app CRUD + deploy.)
#   2. Stand up a throwaway Redis (env.kv's multi-node backend is Redis, NOT
#      embedded redb — see crates/worker/src/cache.rs create_plugins()).
#   3. Boot control + worker + gateway with generated keys. The worker gets
#      `ZEROSHIP_WORKER_KV_URL=redis://...` (enables env.kv) AND `--storage-url <path|s3://…>`
#      (enables env.storage). Without those inputs the namespaces simply are
#      not registered.
#   4. Mint an admin platform bearer OFFLINE (a one-key JWKS served on the
#      harness's own ed25519 key, named as control's trusted issuer), exactly
#      as e2e_app_primitives.sh does.
#   5. Create + deploy examples/kv-dashboard and examples/storage-gallery.
#   6. Exercise the primitives DIRECT to the worker /dispatch with the
#      generated worker bearer, bypassing only the gateway's SEC-5 app-auth
#      policy while retaining transport authentication. This is the
#      cleanest proof the primitive works on the runtime over the real edge:
#        env.kv      — kv.visit (incr) → kv.snapshot → kv.string.set →
#                      kv.keys.list → kv.string.delete   (round-trips)
#        env.storage — gallery.put → gallery.get (text round-trip) →
#                      gallery.list → gallery.delete → gallery.get (absent)
#
# Each step prints ✓/✗ and the trap tears the whole stack (procs + ephemeral
# PG + ephemeral Redis) down on exit.
#
# Usage:
#   ./tests/e2e_app_primitives_kv_storage.sh
#   STRICT=1 ./tests/e2e_app_primitives_kv_storage.sh   # known-fails hard-fail
#
# Requires: docker, node (+ workspace jose), openssl, zstd; a release build
#   (target/release/{zeroship,zeroship-control,zeroship-gate,zeroship-worker});
#   built examples (cd examples/kv-dashboard && pnpm install && pnpm build;
#   cd examples/storage-gallery && pnpm install && pnpm build).
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
REDIS_PORT=6394
PG_CONTAINER="zs-e2e-kvst-pg"
REDIS_CONTAINER="zs-e2e-kvst-redis"

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
  docker rm -f "$REDIS_CONTAINER" >/dev/null 2>&1 || true
  [ -n "$WORK" ] && rm -rf "$WORK"
  echo "  stack down, ephemeral PG + Redis removed"
}
trap cleanup EXIT

jget() { node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);process.stdout.write(String(o$1??'')+'\n')}catch(e){console.log('')}})"; }

# Build a worker /dispatch request envelope for an RPC procedure `id`,
# carrying `{json: <args>}` as the body. The worker maps the URL path
# /__zeroship/v1/<id> to the procedure by its declared `{ id }`.
# The worker decodes a length-prefixed binary frame, not a JSON envelope with
# the body inline. This used to build the latter, so every dispatch below was
# refused with "dispatch metadata too large" and neither env.kv nor
# env.storage was ever exercised over the edge.
# shellcheck source=tests/lib/dispatch_frame.sh
. "$ROOT/tests/lib/dispatch_frame.sh"

# POST an envelope to the worker /dispatch for $APP_ID; echo "<body>\n<code>".
dispatch() {
  local app="$1" id="$2" args="$3"
  local frame="$WORK/frame-$$.bin"
  zs_rpc_frame "$frame" "$id" "$args"
  curl -s -w '\n%{http_code}' -X POST "http://localhost:$ZEROSHIP_WORKER_PORT/dispatch/$app" \
    -H "Authorization: Bearer $ZEROSHIP_WORKER_KEY" \
    -H 'content-type: application/octet-stream' --data-binary @"$frame"
}

echo "============================================"
echo "  zeroship E2E — env.kv + env.storage over the edge (G2/G4)"
echo "============================================"

# --- preflight -------------------------------------------------------------
for b in zeroship zeroship-control zeroship-gate zeroship-worker; do
  [ -x "$BIN/$b" ] || { echo "missing $BIN/$b - run cargo build --release"; exit 2; }
done
[ -f "$ROOT/packages/zero-migrate-cli/dist/cli-bin.js" ] || { echo "missing the zero-migrate CLI - run: pnpm install && pnpm build && pnpm --filter zero-migrate-cli build"; exit 2; }
KV_ZSHIP="$ROOT/examples/kv-dashboard/dist/app.zship"
ST_ZSHIP="$ROOT/examples/storage-gallery/dist/app.zship"
[ -f "$KV_ZSHIP" ] || { echo "missing $KV_ZSHIP — (cd examples/kv-dashboard && pnpm install && pnpm build)"; exit 2; }
[ -f "$ST_ZSHIP" ] || { echo "missing $ST_ZSHIP — (cd examples/storage-gallery && pnpm install && pnpm build)"; exit 2; }
[ -f "$JOSE_JS" ] || { echo "missing jose at $JOSE_JS"; exit 2; }
command -v docker >/dev/null || { echo "docker required"; exit 2; }
command -v openssl >/dev/null || { echo "openssl required"; exit 2; }

WORK="$(mktemp -d -t zs-e2e-kvst-XXXXXX)"
mkdir -p "$WORK/blobs" "$WORK/blob-cache" "$WORK/storage"

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 1: ephemeral Postgres + Redis + migrations ==="
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

docker rm -f "$REDIS_CONTAINER" >/dev/null 2>&1 || true
docker run --name "$REDIS_CONTAINER" -d -p "$REDIS_PORT:6379" redis:7-alpine >/dev/null
for i in $(seq 1 30); do docker exec "$REDIS_CONTAINER" redis-cli ping 2>/dev/null | grep -q PONG && break; sleep 1; done
docker exec "$REDIS_CONTAINER" redis-cli ping 2>/dev/null | grep -q PONG && pass "ephemeral Redis ready on :$REDIS_PORT" || { fail "Redis never became ready"; exit 1; }

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
KVURL="redis://127.0.0.1:$REDIS_PORT"

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

# worker: env.kv <- ZEROSHIP_WORKER_KV_URL (Redis), env.storage <- --storage-url (LocalFs path),
# env.db comes from --db; direct /dispatch uses the generated worker bearer.
ZEROSHIP_WORKER_KV_URL="$KVURL" \
"$BIN/zeroship-worker" --port $ZEROSHIP_WORKER_PORT --threads 2 \
  --control-url "http://localhost:$ZEROSHIP_CONTROL_PORT" \
 --storage-url "$WORK/storage" \
  --blob-store "$WORK/blobs" --poll-interval 2 > "$WORK/worker.log" 2>&1 &
PIDS+=($!)
for i in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" >/dev/null 2>&1 && pass "worker healthy (kv+storage configured)" || { fail "worker unhealthy"; tail -20 "$WORK/worker.log"; exit 1; }

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
SCOPE="apps:read apps:write apps:deploy apps:archive deployments:read deployments:rollback env:read env:write secrets:read secrets:write"

OWNER="$(node -e 'console.log(require("crypto").randomUUID())')"

docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id, email, name, email_verified_at)
VALUES ('$OWNER', 'e2e-$OWNER@zeroship.test'::citext, 'E2E Admin', NOW());
SQL

ADMIN_TOKEN="$(e2e_mint_platform_bearer "$OWNER" "$SCOPE")"
[ "$(echo -n "$ADMIN_TOKEN" | awk -F. '{print NF}')" = "3" ] && pass "minted platform bearer" || { fail "bearer mint failed: $ADMIN_TOKEN"; exit 1; }

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 4: create apps + deploy kv-dashboard + storage-gallery ==="
KV_APP_JSON="$(curl -s -X POST "http://localhost:$ZEROSHIP_CONTROL_PORT/api/apps" \
  -H 'Content-Type: application/json' -H "Authorization: Bearer $ADMIN_TOKEN" -d '{"name":"kv-dashboard-e2e"}')"
KV_APP="$(echo "$KV_APP_JSON" | jget '.id')"
[ -n "$KV_APP" ] && pass "created app kv-dashboard-e2e ($KV_APP)" || { fail "create kv app: $KV_APP_JSON"; exit 1; }

ST_APP_JSON="$(curl -s -X POST "http://localhost:$ZEROSHIP_CONTROL_PORT/api/apps" \
  -H 'Content-Type: application/json' -H "Authorization: Bearer $ADMIN_TOKEN" -d '{"name":"storage-gallery-e2e"}')"
ST_APP="$(echo "$ST_APP_JSON" | jget '.id')"
[ -n "$ST_APP" ] && pass "created app storage-gallery-e2e ($ST_APP)" || { fail "create storage app: $ST_APP_JSON"; exit 1; }

KVDEP="$("$BIN/zeroship" deploy "$KV_ZSHIP" --app="$KV_APP" --control="http://localhost:$ZEROSHIP_CONTROL_PORT" --token="$ADMIN_TOKEN" 2>&1)"
echo "$KVDEP" | grep -q "deploy_hash" && pass "deployed kv-dashboard .zship" || fail "kv deploy failed: $KVDEP"

STDEP="$("$BIN/zeroship" deploy "$ST_ZSHIP" --app="$ST_APP" --control="http://localhost:$ZEROSHIP_CONTROL_PORT" --token="$ADMIN_TOKEN" 2>&1)"
echo "$STDEP" | grep -q "deploy_hash" && pass "deployed storage-gallery .zship" || fail "storage deploy failed: $STDEP"
sleep 5   # let route + version sync to gateway + worker

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 5: env.kv over the worker /dispatch ==="
# kv.visit increments counter:visits and returns a snapshot; do it twice and
# assert the count climbs (round-trip through Redis).
V1="$(dispatch "$KV_APP" "kv.visit" '{}')"; V1B="$(echo "$V1" | head -1)"; V1C="$(echo "$V1" | tail -1)"
if [ "$V1C" = "200" ] && echo "$V1B" | grep -q '"json"'; then
  VISITS1="$(echo "$V1B" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);process.stdout.write(String(o.json.visits)+"\n")}catch(e){console.log("")}})')"
  V2="$(dispatch "$KV_APP" "kv.visit" '{}' | head -1)"
  VISITS2="$(echo "$V2" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);process.stdout.write(String(o.json.visits)+"\n")}catch(e){console.log("")}})')"
  if [ -n "$VISITS1" ] && [ -n "$VISITS2" ] && [ "$VISITS2" -gt "$VISITS1" ]; then
    pass "env.kv incr round-trip: visits $VISITS1 → $VISITS2 (kv.visit over /dispatch)"
  else
    fail "env.kv incr did not advance: $VISITS1 → $VISITS2 (body=$V2)"
  fi
else
  ERR="$(grep -iE 'env.kv|KvPlugin|Cannot find module|redis' "$WORK/worker.log" | tail -1)"
  fail "env.kv kv.visit failed over /dispatch. HTTP $V1C; err: ${ERR:-$V1B}"
fi

# kv.string.set stores a string; kv.snapshot reads it back (round-trip).
NONCE="kv-edge-$(date +%s)"
SS="$(dispatch "$KV_APP" "kv.string.set" "{\"value\":\"$NONCE\"}" | head -1)"
SNAP="$(dispatch "$KV_APP" "kv.snapshot" '{}' | head -1)"
if echo "$SNAP" | grep -q "$NONCE"; then
  pass "env.kv string round-trip: kv.string.set → kv.snapshot returned '$NONCE'"
else
  fail "env.kv string round-trip missing value '$NONCE' (set=$SS snap=$SNAP)"
fi

# kv.keys.list enumerates keys; expect the visits counter key present.
KL="$(dispatch "$KV_APP" "kv.keys.list" '{"limit":50}' | head -1)"
if echo "$KL" | grep -q 'counter:visits'; then
  pass "env.kv list: kv.keys.list enumerated 'counter:visits'"
else
  fail "env.kv list missing counter:visits (body=$KL)"
fi

# kv.string.delete removes it; kv.snapshot should no longer carry the nonce.
dispatch "$KV_APP" "kv.string.delete" '{}' >/dev/null
SNAP2="$(dispatch "$KV_APP" "kv.snapshot" '{}' | head -1)"
if ! echo "$SNAP2" | grep -q "$NONCE"; then
  pass "env.kv delete: kv.string.delete removed the value (gone from snapshot)"
else
  fail "env.kv delete did not remove value (snapshot still has '$NONCE')"
fi

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 6: env.storage over the worker /dispatch ==="
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
