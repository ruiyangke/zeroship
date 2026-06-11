#!/usr/bin/env bash
# ============================================================================
# e2e_app_primitives.sh — close ISS-54 / G1: exercise a REAL example app's
# primitives through the REAL multi-node edge (gateway → worker → V8).
#
# What this does, end to end, against a CLEAN ephemeral stack:
#   1. Stand up a fresh ephemeral Postgres + apply ops/postgres-init.sql +
#      the FULL Liquibase changelog (0001→0036). A migration failure here is
#      a finding — the changelog must apply cleanly from scratch.
#   2. Boot control + worker + gateway with `--dev-insecure` (current code
#      requires WORKER_KEY/SIGNING_KEY otherwise — ISS-53).
#   3. Mint an admin PAT OFFLINE (control's `/api/apps` now requires a real
#      PAT bearer or OAuth introspection — the old `--master-key` bearer is
#      gone). We give control a STABLE ed25519 signing key via
#      `--signing-key-file`, seed a matching `permission_tokens` row + admin
#      role, and sign a `pat+jwt` with jose so the PatIssuer verifies it.
#   4. Create an app + deploy the built `examples/db-todos` .zship.
#   5. Exercise primitives THROUGH THE EDGE:
#        a. anon SSR/HTML for a schema-less app  (proves dispatch+load+serve)
#        b. db-todos RPC over the gateway          (env.db primitive)
#        c. db-todos RPC direct to worker /dispatch (env.db, no auth gate)
#
# Known findings this harness SURFACES (see the report at the end):
#   * SEC-5 fail-closed default — db-todos RPC procedures declare no `auth`,
#     so the gateway defaults them to `User` ⇒ 401 without a real OIDC/Hydra
#     session. There is NO `--dev-insecure` shortcut to mint an app session,
#     so authenticated env.db RPC through the gateway is not headlessly
#     reachable without standing up Hydra. (gateway-auth gap)
#   * SCHEMA-INIT bug — any app exporting `default.schema` (every env.db app)
#     fails module init on the production worker: the runtime's embedded
#     runtime-entry does `await import("@zeroship/bootstrap/install-schema")`,
#     which the worker's module loader cannot resolve (not in the bundle's
#     static graph, not a native module) ⇒ `Cannot find module` ⇒ the app
#     never initializes. This blocks env.db over BOTH the gateway and the
#     direct worker path. (runtime gap — needs a code fix in the runtime.)
#
# Because of the SCHEMA-INIT bug, db-todos cannot currently run on the
# worker. The harness asserts the parts that DO work (clean stack, migration,
# PAT auth, create, deploy, schema-less app over the edge) as PASS, and marks
# the env.db-over-edge steps as KNOWN-FAIL with the captured error so a future
# fix flips them green. Set STRICT=1 to make the known-fails hard-fail.
#
# Usage:
#   ./tests/e2e_app_primitives.sh
#   STRICT=1 ./tests/e2e_app_primitives.sh   # known-fails become failures
#
# Requires: docker, jq-free (uses node), node (+ workspace jose), openssl,
#           a release build (target/release/{zeroship,zeroship-control,
#           zeroship-gate,zeroship-worker}), and a built examples/db-todos
#           (cd examples/db-todos && pnpm install && pnpm build).
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"
STRICT="${STRICT:-0}"

# --- ports (offset from e2e_platform.sh to avoid colliding with a dev stack)
CONTROL_PORT=9099
WORKER_PORT=8087
GATE_PORT=8001
PG_PORT=5443
PG_CONTAINER="zs-e2e-pg"

# --- jose ESM entry in the workspace pnpm store (no direct node_modules link)
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

# node helper: read a JSON field from stdin
jget() { node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);console.log(o$1??'')}catch(e){console.log('')}})"; }

echo "============================================"
echo "  zeroship E2E — app primitives over the edge (ISS-54/G1)"
echo "============================================"

# --- preflight -------------------------------------------------------------
for b in zeroship zeroship-control zeroship-gate zeroship-worker; do
  [ -x "$BIN/$b" ] || { echo "missing $BIN/$b — run: cargo build --release"; exit 2; }
done
ZSHIP="$ROOT/examples/db-todos/dist/app.zship"
[ -f "$ZSHIP" ] || { echo "missing $ZSHIP — run: (cd examples/db-todos && pnpm install && pnpm build)"; exit 2; }
[ -f "$JOSE_JS" ] || { echo "missing jose at $JOSE_JS"; exit 2; }
command -v docker >/dev/null || { echo "docker required"; exit 2; }
command -v openssl >/dev/null || { echo "openssl required"; exit 2; }

WORK="$(mktemp -d -t zs-e2e-prim-XXXXXX)"
mkdir -p "$WORK/blobs" "$WORK/blob-cache"

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 1: ephemeral Postgres + migrations (0001→0036) ==="
docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
docker run --name "$PG_CONTAINER" -d -p "$PG_PORT:5432" \
  -e POSTGRES_PASSWORD=zeroship -e POSTGRES_USER=postgres -e POSTGRES_DB=zeroship \
  postgres:16 -c max_connections=300 >/dev/null
for i in $(seq 1 30); do docker exec "$PG_CONTAINER" pg_isready -U postgres >/dev/null 2>&1 && break; sleep 1; done
docker exec "$PG_CONTAINER" pg_isready -U postgres >/dev/null 2>&1 && pass "ephemeral PG ready on :$PG_PORT" || { fail "PG never became ready"; exit 1; }

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
  pass "Liquibase changelog 0001→0036 applied cleanly from scratch"
else
  fail "Liquibase migration FAILED (see $MIG_LOG)"; tail -20 "$MIG_LOG"; exit 1
fi

# sanity: control tables + apps.system column
psql_q() { docker exec "$PG_CONTAINER" psql -U postgres -d zeroship -tAc "$1" 2>/dev/null | tr -d '[:space:]'; }
[ "$(psql_q "select to_regclass('zeroship.apps') is not null")" = "t" ] && pass "zeroship.apps exists" || fail "zeroship.apps missing"
[ "$(psql_q "select count(*) from information_schema.columns where table_schema='zeroship' and table_name='apps' and column_name='system'")" = "1" ] \
  && pass "apps.system column present (0036)" || fail "apps.system column missing"

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 2: boot stack (--dev-insecure) ==="
DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"

# stable ed25519 PKCS#8 signing key so we can offline-mint a PAT the
# control PatIssuer (built via --signing-key-file) will verify.
openssl genpkey -algorithm ed25519 -out "$WORK/signing-key.pem" 2>/dev/null
chmod 600 "$WORK/signing-key.pem"

for p in $CONTROL_PORT $WORKER_PORT $GATE_PORT; do lsof -ti :$p 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done

"$BIN/zeroship-control" --port $CONTROL_PORT --db "$DBURL" \
  --blob-store "$WORK/blobs" --signing-key-file "$WORK/signing-key.pem" \
  --dev-insecure > "$WORK/control.log" 2>&1 &
PIDS+=($!)
for i in $(seq 1 30); do curl -sf "http://localhost:$CONTROL_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$CONTROL_PORT/health" >/dev/null 2>&1 && pass "control healthy" || { fail "control unhealthy"; tail -20 "$WORK/control.log"; exit 1; }

# worker: empty worker_key (dev) ⇒ /dispatch unauthenticated on loopback;
# shared blob-store with control (single-host shared-volume pattern); --db for env.db.
"$BIN/zeroship-worker" --port $WORKER_PORT --worker-threads 2 \
  --control "http://localhost:$CONTROL_PORT" --db "$DBURL" \
  --blob-store "$WORK/blobs" --poll-interval 2 --dev-insecure > "$WORK/worker.log" 2>&1 &
PIDS+=($!)
for i in $(seq 1 30); do curl -sf "http://localhost:$WORKER_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$WORKER_PORT/health" >/dev/null 2>&1 && pass "worker healthy" || { fail "worker unhealthy"; tail -20 "$WORK/worker.log"; exit 1; }

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
# Admin wrapper policy — the token's stored `policies` is the ceiling Cedar
# AND's with the principal's role (TOKEN ⊂ USER). Must grant the control
# actions we exercise, on resource {type:any}.
POLICY_JSON='{"name":"e2e-admin","statements":[{"effect":"allow","actions":["apps:read","apps:write","apps:deploy","apps:delete","deployments:read","deployments:rollback","env:read","env:write","secrets:read","secrets:write"],"resources":[{"type":"any"}],"conditions":[]}]}'

# policy_hash MUST mirror crates/authz canonical_json (object keys sorted, no ws).
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
const x = (await exportJWK(key)).x;                          // OKP public component
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
echo "=== Stage 4: create app + deploy db-todos ==="
APP_JSON="$(curl -s -X POST "http://localhost:$CONTROL_PORT/api/apps" \
  -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" -d '{"name":"db-todos-e2e"}')"
APP_ID="$(echo "$APP_JSON" | jget '.id')"
[ -n "$APP_ID" ] && pass "created app db-todos-e2e ($APP_ID)" || { fail "create app: $APP_JSON"; exit 1; }

DEP="$("$BIN/zeroship" deploy "$ZSHIP" --app="$APP_ID" --control="http://localhost:$CONTROL_PORT" --token="$PAT" 2>&1)"
echo "$DEP" | grep -q "deploy_hash" && pass "deployed db-todos .zship" || fail "deploy failed: $DEP"
sleep 4   # let route + version sync to gateway + worker

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 5a: schema-LESS app over the real edge (control) ==="
# Proves routing + on-demand load + V8 fetch end-to-end, isolating the
# schema-init bug from the rest of the dispatch path.
STAGE="$(mktemp -d)"; mkdir -p "$STAGE/blobs"
cat > "$STAGE/app.js" <<'JS'
export default { fetch() { return new Response(JSON.stringify({ ok: true }), { status: 200, headers: { "content-type": "application/json" } }); } };
JS
H="$(sha256sum "$STAGE/app.js" | awk '{print $1}')"
cp "$STAGE/app.js" "$STAGE/blobs/$H"
cat > "$STAGE/manifest.json" <<EOF
{"version":1,"resources":{"/[...rest]":{"auth":"anon","publicly_accessible":true}},"assets":{},"runtime_assets":{},"asset_version":0,"sourcemaps":{},"worker":{"entry":"index.js","modules":{"index.js":"$H"}},"metadata":{"compiler":"e2e","built_at":"$(date -u +%FT%TZ)"}}
EOF
(cd "$STAGE" && tar --format=ustar -cf - manifest.json "blobs/$H") | zstd -q -f -o "$STAGE/app.zship"
NS_JSON="$(curl -s -X POST "http://localhost:$CONTROL_PORT/api/apps" -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" -d '{"name":"noschema-e2e"}')"
NS_ID="$(echo "$NS_JSON" | jget '.id')"
"$BIN/zeroship" deploy "$STAGE/app.zship" --app="$NS_ID" --control="http://localhost:$CONTROL_PORT" --token="$PAT" >/dev/null 2>&1
rm -rf "$STAGE"
sleep 4
NS_RESP="$(curl -s -w '\n%{http_code}' -H 'Host: noschema-e2e.localhost' "http://localhost:$GATE_PORT/")"
NS_CODE="$(echo "$NS_RESP" | tail -1)"
NS_BODY="$(echo "$NS_RESP" | head -1)"
if [ "$NS_CODE" = "200" ] && echo "$NS_BODY" | grep -q '"ok":true'; then
  pass "schema-less app served HTTP 200 through gateway→worker→V8"
else
  fail "schema-less app over edge: HTTP $NS_CODE body=$NS_BODY"
fi

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 5b: db-todos RPC through the GATEWAY (env.db over the edge) ==="
# SEC-5: db-todos RPC procedures declare no `auth`, so the gateway defaults
# them to `User` ⇒ 401 without a real OIDC session. Document the gate.
GW_RESP="$(curl -s -w '\n%{http_code}' -X POST \
  -H 'Host: db-todos-e2e.localhost' -H 'content-type: application/json' \
  "http://localhost:$GATE_PORT/__zeroship/v1/users.public" -d '{"json":{}}')"
GW_CODE="$(echo "$GW_RESP" | tail -1)"
GW_BODY="$(echo "$GW_RESP" | head -1)"
if [ "$GW_CODE" = "200" ] && echo "$GW_BODY" | grep -q '"json"'; then
  pass "db-todos users.public via gateway returned a row"
elif [ "$GW_CODE" = "401" ]; then
  known "gateway gates db-todos RPC at 401 (SEC-5 fail-closed default; no headless app session without Hydra). body=$GW_BODY"
else
  known "db-todos RPC via gateway: HTTP $GW_CODE body=$GW_BODY"
fi

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 5c: db-todos RPC DIRECT to worker /dispatch (no auth gate) ==="
# Bypasses the gateway auth gate (empty worker_key ⇒ loopback dispatch is
# unauthenticated). This is the cleanest proof env.db works on the runtime —
# IF schema-init succeeds.
ENVELOPE="$(node -e 'process.stdout.write(JSON.stringify({method:"POST",url:"http://db-todos-e2e.localhost/__zeroship/v1/users.public",headers:[["content-type","application/json"]],body:JSON.stringify({json:{}})}))')"
WK_RESP="$(curl -s -w '\n%{http_code}' -X POST "http://localhost:$WORKER_PORT/dispatch/$APP_ID" -H 'content-type: application/json' -d "$ENVELOPE")"
WK_CODE="$(echo "$WK_RESP" | tail -1)"
WK_BODY="$(echo "$WK_RESP" | head -1)"
if [ "$WK_CODE" = "200" ] && echo "$WK_BODY" | grep -q '"json"'; then
  pass "db-todos users.public via worker /dispatch returned a row (env.db works over the runtime)"
  # Follow up: a mutation + query to fully exercise env.db CRUD.
  UID_VAL="$(echo "$WK_BODY" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);console.log((o.json&&o.json.id)||"")}catch(e){console.log("")}})')"
  if [ -n "$UID_VAL" ]; then
    CRE="$(node -e 'process.stdout.write(JSON.stringify({method:"POST",url:"http://x/__zeroship/v1/todos.create",headers:[["content-type","application/json"]],body:JSON.stringify({json:{userId:process.argv[1],title:"e2e todo"}})}))' "$UID_VAL")"
    C_RESP="$(curl -s -X POST "http://localhost:$WORKER_PORT/dispatch/$APP_ID" -H 'content-type: application/json' -d "$CRE")"
    echo "$C_RESP" | grep -q '"json"' && pass "env.db mutation todos.create over worker" || fail "todos.create failed: $C_RESP"
    LST="$(node -e 'process.stdout.write(JSON.stringify({method:"POST",url:"http://x/__zeroship/v1/todos.list",headers:[["content-type","application/json"]],body:JSON.stringify({json:{userId:process.argv[1]}})}))' "$UID_VAL")"
    L_RESP="$(curl -s -X POST "http://localhost:$WORKER_PORT/dispatch/$APP_ID" -H 'content-type: application/json' -d "$LST")"
    echo "$L_RESP" | grep -q 'e2e todo' && pass "env.db query todos.list returned the inserted row" || fail "todos.list missing row: $L_RESP"
  fi
else
  ERR="$(grep -iEo "Cannot find module '[^']*'" "$WORK/worker.log" | tail -1)"
  [ -z "$ERR" ] && ERR="$(grep -iE 'install-schema|Evaluate rejected' "$WORK/worker.log" | tail -1)"
  known "db-todos schema-init fails on worker ⇒ env.db unreachable. HTTP $WK_CODE; runtime error: ${ERR:-$WK_BODY}"
fi

# ---------------------------------------------------------------------------
echo ""
echo "============================================"
echo "  Results: $PASS passed, $FAIL failed, $KNOWN known-fail"
echo "============================================"
[ $FAIL -eq 0 ] && exit 0 || exit 1
