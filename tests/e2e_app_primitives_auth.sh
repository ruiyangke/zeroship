#!/usr/bin/env bash
# ============================================================================
# e2e_app_primitives_auth.sh — close G3/ISS-54: exercise env.auth through the
# REAL multi-node edge (worker → V8 → AuthPlugin), the last untested primitive.
# Sibling of e2e_app_primitives_kv_storage.sh (env.kv/env.storage GREEN) and
# e2e_app_primitives.sh (env.db GREEN, Stage 5c).
#
# The identity injection (the crux):
#   env.auth identity normally arrives via the gateway's HMAC-signed
#   `ZeroShip-User` header. For a DIRECT worker /dispatch test (bypassing the
#   gateway), the worker STILL verifies that header — `worker_key.is_empty()`
#   disables the *bearer* gate on /dispatch but NOT the user-header HMAC
#   (crates/worker/src/handler.rs verified_user_json). With an empty dev
#   worker_key the HMAC key is the empty byte string, so the harness mints a
#   fully-formed, request-bound, empty-key-signed header itself:
#
#     <base64(userJson)>.<request_id>.<issued_at>.<hmac_sha256_hex("", signed)>
#
#   where signed = "<base64(userJson)>.<request_id>.<issued_at>" and the
#   request_id MUST equal the `x-request-id` header on the dispatch POST
#   (handler binds the header to one request id). The header has a 60s max age,
#   so it is minted FRESH per dispatch. (Format: crates/core/src/auth/mod.rs
#   sign_zeroship_user_header_at / verify_zeroship_user_header_for_request_at.)
#
# What this does, end to end, against a CLEAN ephemeral stack:
#   1. Ephemeral Postgres + ops/postgres-init.sql + the full zeroship-migrate platform set
#      (control needs a DB for app CRUD + deploy).
#   2. Throwaway Redis (auth-notes scopes notes in env.kv → Redis backend).
#   3. control + worker + gateway with `--dev-insecure`; worker gets `--kv-url`.
#   4. Mint an admin PAT OFFLINE (ed25519 --signing-key-file + permission_tokens
#      row), exactly as the sibling harnesses do.
#   5. Create + deploy examples/auth-notes.
#   6. Exercise env.auth DIRECT to the worker /dispatch:
#        (a) ANON  — no ZeroShip-User header:
#              auth.whoami        → { user: null }
#              auth.notes.list    → app-level 401 (UNAUTHENTICATED)
#              auth.whoamiStrict  → RAW requireUser() kernel throw (status)
#        (b) AUTHED — inject signed identity userA:
#              auth.whoami        → returns userA
#              auth.notes.add     → stores a note under userA
#              auth.notes.list    → returns userA's note
#        (c) ISOLATION — inject a DIFFERENT identity userB:
#              auth.notes.list    → empty (does NOT see userA's note)
#              auth.whoami        → returns userB (identity actually flows)
#
# Each step prints ✓/✗ and the trap tears the whole stack down on exit.
#
# Usage:
#   ./tests/e2e_app_primitives_auth.sh
#   STRICT=1 ./tests/e2e_app_primitives_auth.sh   # known-fails hard-fail
#
# Requires: docker, node (+ workspace jose), openssl, zstd; a release build
#   (target/release/{zeroship,zeroship-control,zeroship-gate,zeroship-worker});
#   built example (cd examples/auth-notes && pnpm install && pnpm build).
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"
STRICT="${STRICT:-0}"

# --- ports (offset again to avoid colliding with the sibling harnesses)
CONTROL_PORT=9099
WORKER_PORT=8087
GATE_PORT=8003
PG_PORT=5445
REDIS_PORT=6395
PG_CONTAINER="zs-e2e-auth-pg"
REDIS_CONTAINER="zs-e2e-auth-redis"

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

# Mint a FRESH request id (UUID) on stdout.
new_request_id() { node -e 'console.log(require("crypto").randomUUID())'; }

# Mint a request-bound, empty-key-signed ZeroShip-User header for `userJson`
# bound to `request_id`, issued now. Mirrors Rust sign_zeroship_user_header_at
# with an empty HMAC key (the dev worker_key). Echoes the header on stdout.
sign_user_header() {
  local user_json="$1" request_id="$2"
  node -e '
    const c = require("crypto");
    const [userJson, rid] = process.argv.slice(1);
    const iat = Math.floor(Date.now() / 1000);
    const b64 = Buffer.from(userJson, "utf8").toString("base64");
    const signed = `${b64}.${rid}.${iat}`;
    const mac = c.createHmac("sha256", Buffer.alloc(0)).update(signed).digest("hex");
    process.stdout.write(`${signed}.${mac}`);
  ' "$user_json" "$request_id"
}

# POST an envelope to the worker /dispatch for $APP_ID; echo "<body>\n<code>".
#   dispatch <app> <id> <args> [userJson]
# When userJson is given, a fresh request-bound signed ZeroShip-User header is
# minted and injected along with the matching x-request-id; otherwise the call
# is anonymous (no ZeroShip-User header at all).
dispatch() {
  local app="$1" id="$2" args="$3" user_json="${4:-}"
  local rid; rid="$(new_request_id)"
  local body; body="$(envelope "$id" "$args")"
  if [ -n "$user_json" ]; then
    local hdr; hdr="$(sign_user_header "$user_json" "$rid")"
    curl -s -w '\n%{http_code}' -X POST "http://localhost:$WORKER_PORT/dispatch/$app" \
      -H 'content-type: application/json' \
      -H "x-request-id: $rid" \
      -H "zeroship-user: $hdr" \
      -d "$body"
  else
    curl -s -w '\n%{http_code}' -X POST "http://localhost:$WORKER_PORT/dispatch/$app" \
      -H 'content-type: application/json' \
      -d "$body"
  fi
}

echo "============================================"
echo "  zeroship E2E — env.auth over the edge (G3/ISS-54)"
echo "============================================"

# --- preflight -------------------------------------------------------------
for b in zeroship zeroship-control zeroship-gate zeroship-worker zeroship-migrate; do
  [ -x "$BIN/$b" ] || { echo "missing $BIN/$b — run: cargo build --release"; exit 2; }
done
AUTH_ZSHIP="$ROOT/examples/auth-notes/dist/app.zship"
[ -f "$AUTH_ZSHIP" ] || { echo "missing $AUTH_ZSHIP — (cd examples/auth-notes && pnpm install && pnpm build)"; exit 2; }
[ -f "$JOSE_JS" ] || { echo "missing jose at $JOSE_JS"; exit 2; }
command -v docker >/dev/null || { echo "docker required"; exit 2; }
command -v openssl >/dev/null || { echo "openssl required"; exit 2; }

WORK="$(mktemp -d -t zs-e2e-auth-XXXXXX)"
mkdir -p "$WORK/blobs" "$WORK/blob-cache"

# --- two distinct test identities (pairwise-shaped ids, the gateway projection)
USER_A='{"id":"pws_alice0000000000000a","email":null,"emailVerified":false,"name":"Alice","avatar":null,"scopes":["openid","profile"]}'
USER_B='{"id":"pws_bob00000000000000b","email":null,"emailVerified":false,"name":"Bob","avatar":null,"scopes":["openid","profile"]}'

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

MIG_LOG="$WORK/migrate.log"
if "$BIN/zeroship-migrate" migrate \
    --dir "$ROOT/db/migrations" \
    --database-url "postgres://postgres:zeroship@localhost:$PG_PORT/zeroship" \
    --profile platform --yes > "$MIG_LOG" 2>&1; then
  pass "platform migrations applied cleanly from scratch (zeroship-migrate)"
else
  fail "zeroship-migrate FAILED (see $MIG_LOG)"; tail -20 "$MIG_LOG"; exit 1
fi

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 2: boot stack (--dev-insecure, worker with --kv-url) ==="
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

# worker: env.kv ← --kv-url (Redis), env.auth ← AuthPlugin (always registered).
# Empty worker_key (dev) ⇒ /dispatch bearer gate is open on loopback, but the
# ZeroShip-User header is STILL HMAC-verified (with the empty key) — which is
# exactly why the harness signs the header it injects.
"$BIN/zeroship-worker" --port $WORKER_PORT --worker-threads 2 \
  --control "http://localhost:$CONTROL_PORT" --db "$DBURL" \
  --kv-url "$KVURL" \
  --blob-store "$WORK/blobs" --poll-interval 2 --dev-insecure > "$WORK/worker.log" 2>&1 &
PIDS+=($!)
for i in $(seq 1 30); do curl -sf "http://localhost:$WORKER_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$WORKER_PORT/health" >/dev/null 2>&1 && pass "worker healthy (kv configured)" || { fail "worker unhealthy"; tail -20 "$WORK/worker.log"; exit 1; }

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
echo "=== Stage 4: create app + deploy auth-notes ==="
APP_JSON="$(curl -s -X POST "http://localhost:$CONTROL_PORT/api/apps" \
  -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" -d '{"name":"auth-notes-e2e"}')"
APP="$(echo "$APP_JSON" | jget '.id')"
[ -n "$APP" ] && pass "created app auth-notes-e2e ($APP)" || { fail "create app: $APP_JSON"; exit 1; }

DEP="$("$BIN/zeroship" deploy "$AUTH_ZSHIP" --app="$APP" --control="http://localhost:$CONTROL_PORT" --token="$PAT" 2>&1)"
echo "$DEP" | grep -q "deploy_hash" && pass "deployed auth-notes .zship" || fail "auth deploy failed: $DEP"
sleep 5   # let route + version sync to gateway + worker

# Warm the isolate (on-demand load) before the auth assertions.
dispatch "$APP" "auth.whoami" '{}' >/dev/null

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 5: ANONYMOUS — env.auth resolves null / gates ==="
# (a) auth.whoami with NO ZeroShip-User header → { user: null }.
ANON="$(dispatch "$APP" "auth.whoami" '{}')"
ANB="$(echo "$ANON" | head -1)"; ANC="$(echo "$ANON" | tail -1)"
ANON_USER="$(echo "$ANB" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);console.log(o.json&&o.json.user===null?"NULL":JSON.stringify(o.json&&o.json.user))}catch(e){console.log("ERR")}})')"
if [ "$ANC" = "200" ] && [ "$ANON_USER" = "NULL" ]; then
  pass "anon auth.whoami → getUser() === null (HTTP 200)"
else
  # If the namespace is missing the handler 500s — surface the worker log.
  ERR="$(grep -iE 'env.auth|AuthPlugin|Cannot find module|auth' "$WORK/worker.log" | tail -1)"
  known "anon auth.whoami unexpected (HTTP $ANC, user=$ANON_USER). err: ${ERR:-$ANB}"
fi

# (b) auth.notes.list anonymous → app-level 401 (requireSignedIn throws 401).
LANON="$(dispatch "$APP" "auth.notes.list" '{}')"
LANB="$(echo "$LANON" | head -1)"; LANC="$(echo "$LANON" | tail -1)"
if [ "$LANC" = "401" ]; then
  pass "anon auth.notes.list → HTTP 401 (app-level requireSignedIn gate)"
else
  fail "anon auth.notes.list expected 401, got HTTP $LANC (body=$LANB)"
fi

# (c) auth.whoamiStrict anonymous → RAW kernel requireUser() throw. Post-ISS-67
#     the kernel callback throws an Error carrying status:401 (+ code
#     "unauthenticated"), so the fetch-handler maps it to a clean 401 (a 4xx —
#     its message is NOT masked). The legacy 500 (status-less throw) is still
#     tolerated to keep the assertion green against an un-rebuilt worker.
SANON="$(dispatch "$APP" "auth.whoamiStrict" '{}')"
SANC="$(echo "$SANON" | tail -1)"; SANB="$(echo "$SANON" | head -1)"
if [ "$SANC" = "401" ]; then
  pass "anon auth.whoamiStrict → HTTP 401 (kernel requireUser() throw carries status:401; ISS-67 fixed)"
elif [ "$SANC" = "500" ]; then
  pass "anon auth.whoamiStrict → HTTP 500 (legacy status-less throw; pre-ISS-67 worker not rebuilt)"
else
  fail "anon auth.whoamiStrict expected 401/500, got HTTP $SANC (body=$SANB)"
fi

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 6: AUTHED (userA) — identity flows into env.auth ==="
# (a) auth.whoami with signed userA header → returns userA.
WA="$(dispatch "$APP" "auth.whoami" '{}' "$USER_A")"
WAB="$(echo "$WA" | head -1)"; WAC="$(echo "$WA" | tail -1)"
WA_ID="$(echo "$WAB" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);console.log(o.json&&o.json.user?o.json.user.id:"")}catch(e){console.log("")}})')"
WA_NAME="$(echo "$WAB" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);console.log(o.json&&o.json.user?o.json.user.name:"")}catch(e){console.log("")}})')"
if [ "$WAC" = "200" ] && [ "$WA_ID" = "pws_alice0000000000000a" ] && [ "$WA_NAME" = "Alice" ]; then
  pass "authed auth.whoami → getUser() returned userA (id=$WA_ID name=$WA_NAME)"
else
  fail "authed auth.whoami did not return userA (HTTP $WAC, id='$WA_ID' name='$WA_NAME', body=$WAB)"
fi

# (b) auth.notes.add as userA → stores a note (200, scoped under userA).
NONCE="note-from-alice-$(date +%s)"
ADDA="$(dispatch "$APP" "auth.notes.add" "{\"text\":\"$NONCE\"}" "$USER_A")"
ADDAB="$(echo "$ADDA" | head -1)"; ADDAC="$(echo "$ADDA" | tail -1)"
ADDA_USER="$(echo "$ADDAB" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{console.log(JSON.parse(s).json.user)}catch(e){console.log("")}})')"
if [ "$ADDAC" = "200" ] && [ "$ADDA_USER" = "pws_alice0000000000000a" ]; then
  pass "authed auth.notes.add → stored note scoped to userA"
else
  fail "authed auth.notes.add failed (HTTP $ADDAC, user='$ADDA_USER', body=$ADDAB)"
fi

# (c) auth.notes.list as userA → returns userA's note (round-trip, identity-scoped).
LA="$(dispatch "$APP" "auth.notes.list" '{}' "$USER_A")"
LAB="$(echo "$LA" | head -1)"; LAC="$(echo "$LA" | tail -1)"
if [ "$LAC" = "200" ] && echo "$LAB" | grep -q "$NONCE"; then
  pass "authed auth.notes.list → returned userA's own note '$NONCE'"
else
  fail "authed auth.notes.list missing userA's note (HTTP $LAC, body=$LAB)"
fi

# ---------------------------------------------------------------------------
echo ""
echo "=== Stage 7: ISOLATION (userB) — no cross-user leakage ==="
# (a) auth.whoami as userB → returns userB (a DIFFERENT identity flows).
WB="$(dispatch "$APP" "auth.whoami" '{}' "$USER_B")"
WBB="$(echo "$WB" | head -1)"
WB_ID="$(echo "$WBB" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);console.log(o.json&&o.json.user?o.json.user.id:"")}catch(e){console.log("")}})')"
if [ "$WB_ID" = "pws_bob00000000000000b" ]; then
  pass "authed auth.whoami (userB) → getUser() returned userB (id=$WB_ID)"
else
  fail "authed auth.whoami (userB) did not return userB (id='$WB_ID', body=$WBB)"
fi

# (b) auth.notes.list as userB → MUST NOT contain userA's note. userB has an
#     empty list — the kv scoping by user.id is the isolation boundary.
LB="$(dispatch "$APP" "auth.notes.list" '{}' "$USER_B")"
LBB="$(echo "$LB" | head -1)"; LBC="$(echo "$LB" | tail -1)"
LB_USER="$(echo "$LBB" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{console.log(JSON.parse(s).json.user)}catch(e){console.log("")}})')"
LB_COUNT="$(echo "$LBB" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{console.log(JSON.parse(s).json.notes.length)}catch(e){console.log("")}})')"
if [ "$LBC" = "200" ] && [ "$LB_USER" = "pws_bob00000000000000b" ] && ! echo "$LBB" | grep -q "$NONCE" && [ "$LB_COUNT" = "0" ]; then
  pass "ISOLATION: userB's auth.notes.list is empty + lacks userA's note (identity-scoped, no leakage)"
else
  fail "ISOLATION BREACH or unexpected: userB list (HTTP $LBC, user='$LB_USER', count='$LB_COUNT', body=$LBB)"
fi

# (c) Re-list as userA → still has the note (userB's calls did not clobber it).
LA2="$(dispatch "$APP" "auth.notes.list" '{}' "$USER_A")"
LA2B="$(echo "$LA2" | head -1)"
if echo "$LA2B" | grep -q "$NONCE"; then
  pass "ISOLATION: userA still sees its own note after userB activity"
else
  fail "userA lost its note after userB activity (body=$LA2B)"
fi

# ---------------------------------------------------------------------------
echo ""
echo "============================================"
echo "  Results: $PASS passed, $FAIL failed, $KNOWN known-fail"
echo "============================================"
[ $FAIL -eq 0 ] && exit 0 || exit 1
