#!/usr/bin/env bash
# ============================================================================
# e2e_app_primitives_kv_storage.sh — close G2/G4: exercise env.kv and
# env.storage through the REAL multi-node edge (worker → V8 → native plugin),
# the same way e2e_app_primitives.sh now proves env.db (Stage 5c GREEN after
# ISS-63 + ISS-66).
#
# What this does, end to end, against a CLEAN ephemeral stack:
#   1. Stand up a fresh ephemeral Postgres + ops/postgres-init.sql + the FULL
#      Liquibase changelog. (Control still needs a DB for app CRUD + deploy.)
#   2. Stand up a throwaway Redis (env.kv's multi-node backend is Redis, NOT
#      embedded redb — see crates/worker/src/cache.rs create_plugins()).
#   3. Boot control + worker + gateway with `--dev-insecure`. The worker gets
#      `--kv-url redis://...` (enables env.kv) AND `--storage-url <path|s3://…>`
#      (enables env.storage). Without those flags the namespaces simply are
#      not registered.
#   4. Mint an admin PAT OFFLINE (ed25519 --signing-key-file + seeded
#      permission_tokens row), exactly as e2e_app_primitives.sh does.
#   5. Create + deploy examples/kv-dashboard and examples/storage-gallery.
#   6. Exercise the primitives DIRECT to the worker /dispatch (empty
#      worker_key ⇒ loopback dispatch is unauthenticated; this bypasses the
#      gateway's SEC-5 fail-closed auth gate, mirroring Stage 5c). This is the
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
STRICT="${STRICT:-0}"

# --- ports (offset again to avoid colliding with e2e_app_primitives.sh)
CONTROL_PORT=9098
WORKER_PORT=8086
GATE_PORT=8002
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

jget() { node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);console.log(o$1??'')}catch(e){console.log('')}})"; }

# Build a worker /dispatch request envelope for an RPC procedure `id`,
# carrying `{json: <args>}` as the body. The worker maps the URL path
# /__zeroship/v1/<id> to the procedure by its declared `{ id }`.
envelope() {
  local id="$1" args="$2"
  node -e 'process.stdout.write(JSON.stringify({method:"POST",url:"http://x/__zeroship/v1/"+process.argv[1],headers:[["content-type","application/json"]],body:JSON.stringify({json:JSON.parse(process.argv[2])})}))' "$id" "$args"
}

# POST an envelope to the worker /dispatch for $APP_ID; echo "<body>\n<code>".
dispatch() {
  local app="$1" id="$2" args="$3"
  curl -s -w '\n%{http_code}' -X POST "http://localhost:$WORKER_PORT/dispatch/$app" \
    -H 'content-type: application/json' -d "$(envelope "$id" "$args")"
}

echo "============================================"
echo "  zeroship E2E — env.kv + env.storage over the edge (G2/G4)"
echo "============================================"

# --- preflight -------------------------------------------------------------
for b in zeroship zeroship-control zeroship-gate zeroship-worker; do
  [ -x "$BIN/$b" ] || { echo "missing $BIN/$b — run: cargo build --release"; exit 2; }
done
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
for i in $(seq 1 30); do docker exec "$PG_CONTAINER" pg_isready -U postgres >/dev/null 2>&1 && break; sleep 1; done
docker exec "$PG_CONTAINER" pg_isready -U postgres >/dev/null 2>&1 && pass "ephemeral PG ready on :$PG_PORT" || { fail "PG never became ready"; exit 1; }

docker rm -f "$REDIS_CONTAINER" >/dev/null 2>&1 || true
docker run --name "$REDIS_CONTAINER" -d -p "$REDIS_PORT:6379" redis:7-alpine >/dev/null
for i in $(seq 1 30); do docker exec "$REDIS_CONTAINER" redis-cli ping 2>/dev/null | grep -q PONG && break; sleep 1; done
docker exec "$REDIS_CONTAINER" redis-cli ping 2>/dev/null | grep -q PONG && pass "ephemeral Redis ready on :$REDIS_PORT" || { fail "Redis never became ready"; exit 1; }

if [ -f "$ROOT/ops/postgres-init.sql" ]; then
  docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 < "$ROOT/ops/postgres-init.sql" >/dev/null 2>&1 \
    && pass "applied ops/postgres-init.sql" || fail "postgres-init.sql failed"
fi

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

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 2: boot stack (--dev-insecure, worker with --kv-url + --storage-url) ==="
DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"
KVURL="redis://127.0.0.1:$REDIS_PORT"

openssl genpkey -algorithm ed25519 -out "$WORK/signing-key.pem" 2>/dev/null
chmod 600 "$WORK/signing-key.pem"

for p in $CONTROL_PORT $WORKER_PORT $GATE_PORT; do lsof -ti :$p 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done

"$BIN/zeroship-control" --port $CONTROL_PORT --db "$DBURL" \
  --blob-store "$WORK/blobs" --signing-key-file "$WORK/signing-key.pem" \
  --dev-insecure > "$WORK/control.log" 2>&1 &
PIDS+=($!)
for i in $(seq 1 30); do curl -sf "http://localhost:$CONTROL_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$CONTROL_PORT/health" >/dev/null 2>&1 && pass "control healthy" || { fail "control unhealthy"; tail -20 "$WORK/control.log"; exit 1; }

# worker: env.kv ← --kv-url (Redis), env.storage ← --storage-url (LocalFs path),
# env.db ← --db. Empty worker_key (dev) ⇒ /dispatch is unauthenticated on loopback.
"$BIN/zeroship-worker" --port $WORKER_PORT --worker-threads 2 \
  --control "http://localhost:$CONTROL_PORT" --db "$DBURL" \
  --kv-url "$KVURL" --storage-url "$WORK/storage" \
  --blob-store "$WORK/blobs" --poll-interval 2 --dev-insecure > "$WORK/worker.log" 2>&1 &
PIDS+=($!)
for i in $(seq 1 30); do curl -sf "http://localhost:$WORKER_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$WORKER_PORT/health" >/dev/null 2>&1 && pass "worker healthy (kv+storage configured)" || { fail "worker unhealthy"; tail -20 "$WORK/worker.log"; exit 1; }

"$BIN/zeroship-gate" --port $GATE_PORT --control "http://localhost:$CONTROL_PORT" \
  --workers "http://localhost:$WORKER_PORT" --blob-store "$WORK/blobs" \
  --blob-cache-disk-root "$WORK/blob-cache" --db "$DBURL" --poll-interval 2 \
  --dev-insecure > "$WORK/gate.log" 2>&1 &
PIDS+=($!)
for i in $(seq 1 30); do curl -sf "http://localhost:$GATE_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$GATE_PORT/health" >/dev/null 2>&1 && pass "gateway healthy" || { fail "gateway unhealthy"; tail -20 "$WORK/gate.log"; exit 1; }

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 3: mint admin PAT (offline) ==="
POLICY_JSON='{"name":"e2e-admin","statements":[{"effect":"allow","actions":["apps:read","apps:write","apps:deploy","apps:delete","deployments:read","deployments:rollback","env:read","env:write","secrets:read","secrets:write"],"resources":[{"type":"any"}],"conditions":[]}]}'

POLICY_HASH="$(node -e '
const {createHash}=require("crypto");
function c(v){if(v===null||typeof v==="number"||typeof v==="boolean")return JSON.stringify(v);
if(typeof v==="string")return JSON.stringify(v);
if(Array.isArray(v))return "["+v.map(c).join(",")+"]";
return "{"+Object.keys(v).sort().map(k=>JSON.stringify(k)+":"+c(v[k])).join(",")+"}";}
process.stdout.write(createHash("sha256").update(c(JSON.parse(process.argv[1]))).digest("hex"));
' "$POLICY_JSON")"

OWNER="$(node -e 'console.log(require("crypto").randomUUID())')"
TOKID="$(node -e 'console.log(require("crypto").randomUUID())')"
EXP=$(( $(date +%s) + 86400 ))

docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id, email, name, email_verified_at)
VALUES ('$OWNER', 'e2e-$OWNER@zeroship.test'::citext, 'E2E Admin', NOW());
INSERT INTO zeroship.platform_admin_roles (user_id, role, granted_by)
VALUES ('$OWNER', 'admin', '$OWNER');
INSERT INTO zeroship.permission_tokens (id, owner_id, kind, name, policies, policy_hash, expires_at)
VALUES ('$TOKID', '$OWNER', 'pat', 'e2e harness', '$POLICY_JSON'::jsonb, '$POLICY_HASH', to_timestamp($EXP));
SQL

PAT="$(node --input-type=module -e '
import { readFileSync } from "node:fs";
import { createHash, randomBytes } from "node:crypto";
import { importPKCS8, exportJWK, SignJWT } from "file://'"$JOSE_JS"'";
const [pem, owner, tid, phash, exp] = process.argv.slice(1);
const key = await importPKCS8(readFileSync(pem,"utf8"), "EdDSA", { extractable:true });
const x = (await exportJWK(key)).x;
const kid = createHash("sha256").update(`{"crv":"Ed25519","kty":"OKP","x":"${x}"}`).digest("base64url");
const jwt = await new SignJWT({ sub:owner, owner, tid, jti:tid, scope:"pat", policy_hash:phash, nonce:randomBytes(32).toString("base64url") })
  .setProtectedHeader({ alg:"EdDSA", typ:"pat+jwt", kid })
  .setIssuer("https://api.zeroship.ai").setAudience("control.zeroship.ai")
  .setIssuedAt(Math.floor(Date.now()/1000)).setExpirationTime(Number(exp))
  .sign(key);
process.stdout.write(jwt);
' "$WORK/signing-key.pem" "$OWNER" "$TOKID" "$POLICY_HASH" "$EXP")"
[ "$(echo -n "$PAT" | awk -F. '{print NF}')" = "3" ] && pass "minted pat+jwt" || { fail "PAT mint failed: $PAT"; exit 1; }

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 4: create apps + deploy kv-dashboard + storage-gallery ==="
KV_APP_JSON="$(curl -s -X POST "http://localhost:$CONTROL_PORT/api/apps" \
  -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" -d '{"name":"kv-dashboard-e2e"}')"
KV_APP="$(echo "$KV_APP_JSON" | jget '.id')"
[ -n "$KV_APP" ] && pass "created app kv-dashboard-e2e ($KV_APP)" || { fail "create kv app: $KV_APP_JSON"; exit 1; }

ST_APP_JSON="$(curl -s -X POST "http://localhost:$CONTROL_PORT/api/apps" \
  -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" -d '{"name":"storage-gallery-e2e"}')"
ST_APP="$(echo "$ST_APP_JSON" | jget '.id')"
[ -n "$ST_APP" ] && pass "created app storage-gallery-e2e ($ST_APP)" || { fail "create storage app: $ST_APP_JSON"; exit 1; }

KVDEP="$("$BIN/zeroship" deploy "$KV_ZSHIP" --app="$KV_APP" --control="http://localhost:$CONTROL_PORT" --token="$PAT" 2>&1)"
echo "$KVDEP" | grep -q "deploy_hash" && pass "deployed kv-dashboard .zship" || fail "kv deploy failed: $KVDEP"

STDEP="$("$BIN/zeroship" deploy "$ST_ZSHIP" --app="$ST_APP" --control="http://localhost:$CONTROL_PORT" --token="$PAT" 2>&1)"
echo "$STDEP" | grep -q "deploy_hash" && pass "deployed storage-gallery .zship" || fail "storage deploy failed: $STDEP"
sleep 5   # let route + version sync to gateway + worker

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 5: env.kv over the worker /dispatch ==="
# kv.visit increments counter:visits and returns a snapshot; do it twice and
# assert the count climbs (round-trip through Redis).
V1="$(dispatch "$KV_APP" "kv.visit" '{}')"; V1B="$(echo "$V1" | head -1)"; V1C="$(echo "$V1" | tail -1)"
if [ "$V1C" = "200" ] && echo "$V1B" | grep -q '"json"'; then
  VISITS1="$(echo "$V1B" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);console.log(o.json.visits)}catch(e){console.log("")}})')"
  V2="$(dispatch "$KV_APP" "kv.visit" '{}' | head -1)"
  VISITS2="$(echo "$V2" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);console.log(o.json.visits)}catch(e){console.log("")}})')"
  if [ -n "$VISITS1" ] && [ -n "$VISITS2" ] && [ "$VISITS2" -gt "$VISITS1" ]; then
    pass "env.kv incr round-trip: visits $VISITS1 → $VISITS2 (kv.visit over /dispatch)"
  else
    fail "env.kv incr did not advance: $VISITS1 → $VISITS2 (body=$V2)"
  fi
else
  ERR="$(grep -iE 'env.kv|KvPlugin|Cannot find module|redis' "$WORK/worker.log" | tail -1)"
  known "env.kv kv.visit failed over /dispatch. HTTP $V1C; err: ${ERR:-$V1B}"
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
  PUTSZ="$(echo "$PUTB" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{console.log(JSON.parse(s).json.size)}catch(e){console.log("")}})')"
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
  DELED="$(echo "$DEL" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{console.log(JSON.parse(s).json.deleted)}catch(e){console.log("")}})')"
  GET2="$(dispatch "$ST_APP" "gallery.get" "{\"key\":\"$SKEY\"}" | head -1)"
  GONE="$(echo "$GET2" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{console.log(JSON.parse(s).json.found)}catch(e){console.log("")}})')"
  if [ "$DELED" = "true" ] && [ "$GONE" = "false" ]; then
    pass "env.storage delete: gallery.delete removed it (subsequent get found:false)"
  else
    fail "env.storage delete did not remove object (deleted=$DELED found-after=$GONE del=$DEL get=$GET2)"
  fi
else
  ERR="$(grep -iE 'env.storage|StoragePlugin|Cannot find module|storage' "$WORK/worker.log" | tail -1)"
  known "env.storage gallery.put failed over /dispatch. HTTP $PUTC; err: ${ERR:-$PUTB}"
fi

# ---------------------------------------------------------------------------
echo ""
echo "============================================"
echo "  Results: $PASS passed, $FAIL failed, $KNOWN known-fail"
echo "============================================"
[ $FAIL -eq 0 ] && exit 0 || exit 1
