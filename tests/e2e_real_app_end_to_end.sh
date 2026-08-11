#!/usr/bin/env bash
# ============================================================================
# e2e_real_app_end_to_end.sh — a REAL creator app, built + deployed + verified
# end to end INCLUDING metering/billing.
#
# Builds examples/starter (a real React + @zeroship/rpc message-board app) with
# the real vite plugin → dist/app.zship, deploys it to a full platform stack
# (control + worker + gateway) that has METERING enabled, then verifies the whole
# chain a real deployed app exercises:
#   1. build local .zship (vite-plugin)                 (the creator's `pnpm build`)
#   2. deploy the .zship                                (`zeroship deploy`)
#   3. gateway serves the app index.html + hashed JS    (static asset serving)
#   4. getMessages QUERY executes in the worker         (server function runs)
#   5. addMessage MUTATION executes                     (input schema, real dispatch)
#      (the in-isolate round-trip is REPORTED, not asserted - see stage 5)
#   6. the app's requests are METERED → usage_aggregates → a projected CHARGE
#
# Host-routed (starter.localhost) — how a deployed app is really hit. lite billing
# provider. REFUSES (exit 1) when docker is unavailable - a run that asserted nothing
# is not a passing run. KEEP_WORK=1 keeps logs.
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; BIN="$ROOT/target/release"
PASS=0; FAIL=0
pass(){ PASS=$((PASS+1)); echo "  ✓ $1"; }
fail(){ FAIL=$((FAIL+1)); echo "  ✗ $1"; }

echo "============================================"
echo "  zeroship E2E — REAL app (starter) build → deploy → serve → RPC → BILLED"
echo "============================================"
# Docker unavailable is a REFUSAL, not a skip. This used to `exit 0` after a
# warning, so on any machine without docker the harness reported success having
# asserted nothing - "0 failed over 0 assertions", the shape of tasks
# #102/#103/#279. RED-PROVEN across all six harnesses that carried this arm:
# with a stub `docker` returning 1 on PATH, every one of them printed
# "SKIP: docker unavailable." and exited 0.
if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  echo "  x REFUSED: docker unavailable, so NOTHING in this harness ran." >&2
  echo "    Exiting non-zero: a run that asserted nothing is not a passing run." >&2
  echo "    Start docker and re-run." >&2
  exit 1
fi
for b in zeroship zeroship-control zeroship-gate zeroship-worker zeroship-platform-migrate; do [ -x "$BIN/$b" ] || { echo "missing $BIN/$b — run cargo build --release, then cargo build --release -p zeroship-migrate-adapter --features platform-cli --bin zeroship-platform-migrate"; exit 2; }; done
command -v node >/dev/null && command -v openssl >/dev/null && command -v curl >/dev/null && command -v pnpm >/dev/null || { echo "need node/openssl/curl/pnpm"; exit 2; }
JOSE="$ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"; [ -f "$JOSE" ] || { echo "missing jose"; exit 2; }
STARTER="$ROOT/examples/starter"; [ -d "$STARTER" ] || { echo "missing examples/starter"; exit 2; }

CONTROL_PORT=9181; WORKER_PORT=8081; GATE_PORT=8071; PG_PORT=5481; RP_PORT=19181
PGC=zs-e2e-app-pg; RPC=zs-e2e-app-redpanda
DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"; CONTROL_URL="http://localhost:$CONTROL_PORT"
RP_BROKERS="127.0.0.1:$RP_PORT"; USAGE_TOPIC="zeroship-usage-app-e2e"; APP_HOST="starter.localhost"
WORK="$(mktemp -d -t zs-e2e-app-XXXXXX)"; mkdir -p "$WORK/blobs" "$WORK/blob-cache"; PIDFILE="$WORK/pids"; : > "$PIDFILE"
jget(){ node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);process.stdout.write(String(o$1??'')+'\n')}catch(e){console.log('')}})"; }
psql_exec(){ docker exec -i "$PGC" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 "$@"; }
gw(){ curl -s -H "Host: $APP_HOST" "$@"; }   # host-routed request to the deployed app

cleanup(){
  echo ""; echo "=== Cleanup ==="
  [ -f "$PIDFILE" ] && while read -r pid; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done < "$PIDFILE"
  wait 2>/dev/null || true
  if [ "${KEEP_WORK:-0}" = "1" ]; then echo "  KEEP_WORK=1 → $PGC/$RPC + $WORK preserved";
  else docker rm -f "$PGC" "$RPC" >/dev/null 2>&1 || true; rm -rf "$WORK"; echo "  stack down, $WORK cleaned"; fi
}
trap cleanup EXIT
for p in $CONTROL_PORT $WORKER_PORT $GATE_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done

echo ""; echo "=== Stage 1: BUILD the real app (examples/starter → dist/app.zship via vite) ==="
( cd "$STARTER" && pnpm build ) > "$WORK/appbuild.log" 2>&1
ZSHIP="$STARTER/dist/app.zship"
[ -f "$ZSHIP" ] && pass "built starter app.zship ($(du -k "$ZSHIP" | cut -f1)KB) with the real vite plugin" || { fail "app build produced no .zship"; tail -25 "$WORK/appbuild.log"; exit 1; }

echo ""; echo "=== Stage 2: infra + migrate + seed + stack (metering enabled, lite provider) ==="
docker rm -f "$PGC" >/dev/null 2>&1 || true
docker run --name "$PGC" -d -p "$PG_PORT:5432" -e POSTGRES_PASSWORD=zeroship -e POSTGRES_USER=postgres -e POSTGRES_DB=zeroship postgres:16 -c max_connections=200 >/dev/null || { fail "pg run"; exit 1; }
for _ in $(seq 1 40); do docker exec "$PGC" pg_isready -U postgres >/dev/null 2>&1 && break; sleep 1; done
docker exec "$PGC" pg_isready -U postgres >/dev/null 2>&1 && pass "ephemeral PG on :$PG_PORT" || { fail "PG"; exit 1; }
docker rm -f "$RPC" >/dev/null 2>&1 || true
docker run --name "$RPC" -d -p "$RP_PORT:$RP_PORT" docker.redpanda.com/redpandadata/redpanda:latest \
  redpanda start --overprovisioned --smp 1 --memory 512M --reserve-memory 0M --node-id 0 --check=false \
  --kafka-addr "external://0.0.0.0:$RP_PORT" --advertise-kafka-addr "external://127.0.0.1:$RP_PORT" \
  --set redpanda.auto_create_topics_enabled=true >/dev/null || { fail "redpanda run"; exit 1; }
for _ in $(seq 1 40); do docker exec "$RPC" rpk cluster health --exit-when-healthy >/dev/null 2>&1 && break; sleep 1.5; done
docker exec "$RPC" rpk cluster health --exit-when-healthy >/dev/null 2>&1 && pass "redpanda on $RP_BROKERS" || { fail "redpanda"; exit 1; }

MIG_LOG="$WORK/migrate.log"
"$BIN/zeroship-platform-migrate" \
  --database-url "$DBURL" --migrations-dir "$ROOT/db/migrations-ts" \
  --project-schema zeroship --project-id zeroship > "$MIG_LOG" 2>&1 \
  && pass "zeroship platform migrations applied" || { fail "migrate"; tail -20 "$MIG_LOG"; exit 1; }

PLAN_ID="pln_app_e2e"
psql_exec >/dev/null 2>&1 <<SQL && pass "seeded plan + pricing (1c/request)" || { fail "plan seed"; exit 1; }
INSERT INTO zeroship.plans (id,name,base_fee_cents,included_units,fx_pico_cents_per_unit,runtime_limits_json,spend_limit_default_cents)
VALUES ('$PLAN_ID','app-e2e',0,0,1000000000000,'{"cpu_limit_ms":5000,"wall_timeout_ms":30000,"heap_limit_mb":256}',100000000) ON CONFLICT (id) DO NOTHING;
INSERT INTO zeroship.pricing_config (id,fx_pico_cents_per_unit) VALUES ('global',1000000000000) ON CONFLICT (id) DO UPDATE SET fx_pico_cents_per_unit=EXCLUDED.fx_pico_cents_per_unit;
INSERT INTO zeroship.billing_metrics (metric,kind,unit) VALUES ('requests','platform','op') ON CONFLICT (metric) DO UPDATE SET kind='platform';
INSERT INTO zeroship.metric_weights (metric,units_per_op,per_units) VALUES ('requests',1,1) ON CONFLICT (metric) DO UPDATE SET units_per_op=1,per_units=1;
SQL

openssl genpkey -algorithm ed25519 -out "$WORK/sk.pem" 2>/dev/null; chmod 600 "$WORK/sk.pem"
openssl rand -base64 48 > "$WORK/gate-broker-secret"; chmod 600 "$WORK/gate-broker-secret"
CFG_TOML="$WORK/zeroship.toml"; printf '[metering]\nredpanda_brokers = "%s"\nusage_events_topic = "%s"\n' "$RP_BROKERS" "$USAGE_TOPIC" > "$CFG_TOML"
"$BIN/zeroship-control" --port "$CONTROL_PORT" --db "$DBURL" --config "$CFG_TOML" \
  --blob-store "$WORK/blobs" --signing-key-file "$WORK/sk.pem" \
  --stripe-base-url "http://127.0.0.1:1" --stripe-secret-key "sk_test_unused" \
  --meter-provider lite --invoicer-provider lite --allow-unsupported-billing \
  --spend-recompute-interval 2 --dev-insecure > "$WORK/control.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$CONTROL_URL/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$CONTROL_URL/health" >/dev/null 2>&1 && pass "control healthy" || { fail "control"; tail -30 "$WORK/control.log"; exit 1; }
USAGE_OUTBOX_WAL_PATH="$WORK/worker-outbox.redb" "$BIN/zeroship-worker" --port "$WORKER_PORT" --worker-threads 2 \
  --config "$CFG_TOML" --control "$CONTROL_URL" --db "$DBURL" --blob-store "$WORK/blobs" --poll-interval 2 --dev-insecure > "$WORK/worker.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$WORKER_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$WORKER_PORT/health" >/dev/null 2>&1 && pass "worker healthy" || { fail "worker"; tail -30 "$WORK/worker.log"; exit 1; }
USAGE_OUTBOX_WAL_PATH="$WORK/gate-outbox.redb" "$BIN/zeroship-gate" --port "$GATE_PORT" --control "$CONTROL_URL" \
  --config "$CFG_TOML" --workers "http://localhost:$WORKER_PORT" --blob-store "$WORK/blobs" --blob-cache-disk-root "$WORK/blob-cache" \
  --db "$DBURL" --poll-interval 2 --signing-key-file "$WORK/sk.pem" --gateway-broker-secret-file "$WORK/gate-broker-secret" --dev-insecure > "$WORK/gate.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$GATE_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$GATE_PORT/health" >/dev/null 2>&1 && pass "gateway healthy" || { fail "gateway"; tail -30 "$WORK/gate.log"; exit 1; }

echo ""; echo "=== Stage 3: PAT + creator + app '$APP_HOST' + DEPLOY the real .zship ==="
POLICY_JSON='{"name":"e2e-app","statements":[{"effect":"allow","actions":["apps:read","apps:write","apps:deploy","billing:read","billing:write"],"resources":[{"type":"any"}],"conditions":[]}]}'
POLICY_HASH="$(node -e 'const{createHash}=require("crypto");function c(v){if(v===null||typeof v==="number"||typeof v==="boolean")return JSON.stringify(v);if(typeof v==="string")return JSON.stringify(v);if(Array.isArray(v))return "["+v.map(c).join(",")+"]";return "{"+Object.keys(v).sort().map(k=>JSON.stringify(k)+":"+c(v[k])).join(",")+"}";}process.stdout.write(createHash("sha256").update(c(JSON.parse(process.argv[1]))).digest("hex"));' "$POLICY_JSON")"
CREATOR="$(node -e 'console.log(require("crypto").randomUUID())')"; TOKID="$(node -e 'console.log(require("crypto").randomUUID())')"; EXP=$(( $(date +%s) + 86400 ))
psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id,email,name,email_verified_at) VALUES ('$CREATOR','e2e-app-$CREATOR@zeroship.test'::citext,'E2E App',NOW());
INSERT INTO zeroship.platform_admin_roles (user_id,role,granted_by) VALUES ('$CREATOR','admin','$CREATOR');
INSERT INTO zeroship.permission_tokens (id,owner_id,kind,name,policies,policy_hash,expires_at) VALUES ('$TOKID','$CREATOR','pat','e2e app','$POLICY_JSON'::jsonb,'$POLICY_HASH',to_timestamp($EXP));
SQL
PAT="$(node --input-type=module -e 'import{readFileSync}from "node:fs";import{createHash,randomBytes}from "node:crypto";import{importPKCS8,exportJWK,SignJWT}from "file://'"$JOSE"'";const[pem,owner,tid,phash,exp]=process.argv.slice(1);const key=await importPKCS8(readFileSync(pem,"utf8"),"EdDSA",{extractable:true});const x=(await exportJWK(key)).x;const kid=createHash("sha256").update(`{"crv":"Ed25519","kty":"OKP","x":"${x}"}`).digest("base64url");const jwt=await new SignJWT({sub:owner,owner,tid,jti:tid,scope:"pat",policy_hash:phash,nonce:randomBytes(32).toString("base64url")}).setProtectedHeader({alg:"EdDSA",typ:"pat+jwt",kid}).setIssuer("https://api.zeroship.ai").setAudience("control.zeroship.ai").setIssuedAt(Math.floor(Date.now()/1000)).setExpirationTime(Number(exp)).sign(key);process.stdout.write(jwt);' "$WORK/sk.pem" "$CREATOR" "$TOKID" "$POLICY_HASH" "$EXP")"
[ "$(echo -n "$PAT" | awk -F. '{print NF}')" = "3" ] && pass "minted PAT (creator=$CREATOR)" || { fail "PAT"; exit 1; }
APP="$(curl -s -X POST "$CONTROL_URL/api/apps" -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" -d "{\"name\":\"starter\",\"plan_id\":\"$PLAN_ID\"}" | jget '.id')"
[ -n "$APP" ] && pass "created app 'starter' ($APP)" || { fail "create app"; exit 1; }
psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.app_members (app_id,user_id,role) VALUES ('$APP','$CREATOR','owner') ON CONFLICT (app_id,user_id) DO UPDATE SET role='owner';
SQL
"$BIN/zeroship" deploy "$ZSHIP" --app="$APP" --control="$CONTROL_URL" --token="$PAT" 2>&1 | grep -q deploy_hash && pass "deployed the real starter .zship" || { fail "deploy"; exit 1; }
sleep 5

echo ""; echo "=== Stage 4: gateway SERVES the deployed app (static assets) ==="
READY=0; for _ in $(seq 1 30); do echo "$(gw "http://localhost:$GATE_PORT/")" | grep -qi "<!doctype html" && { READY=1; break; }; sleep 1; done
INDEX="$(gw "http://localhost:$GATE_PORT/")"
[ "$READY" = "1" ] && pass "GET / serves the app index.html (real React shell)" || { fail "index.html not served"; tail -15 "$WORK/gate.log"; exit 1; }
ASSET="$(printf '%s' "$INDEX" | grep -oE '/assets/[A-Za-z0-9._-]+\.js' | head -1)"
if [ -n "$ASSET" ]; then
  CODE="$(gw -o /dev/null -w '%{http_code}' "http://localhost:$GATE_PORT$ASSET")"
  [ "$CODE" = "200" ] && pass "hashed client JS asset served ($ASSET → 200)" || fail "asset $ASSET → $CODE"
else fail "no /assets/*.js referenced in index.html"; fi

echo ""; echo "=== Stage 5: RPC — the deployed app's SERVER FUNCTIONS execute ==="
# getMessages (query, no input): GET /__zeroship/v1/getMessages
QMSG="$(gw "http://localhost:$GATE_PORT/__zeroship/v1/getMessages")"
echo "$QMSG" | grep -q "Build locally" && pass "getMessages QUERY executed in the worker → returned the seeded messages" || { fail "getMessages: ${QMSG:0:160}"; }
# addMessage (mutation, input {text}): POST superjson body {"json":{"text":...}}
NEWTXT="deployed-and-metered-$(date +%s)"
AMSG="$(gw -X POST "http://localhost:$GATE_PORT/__zeroship/v1/addMessage" -H 'content-type: application/json' --data "{\"json\":{\"text\":\"$NEWTXT\"}}")"
# A failed mutation is a FAILURE, not a note. This arm used to be `|| echo`,
# so a broken addMessage cost one pass and zero failures. RED-PROVEN by sending
# a body the input schema must reject ({"WRONGFIELD":...}): the response was
# INVALID_ARGUMENT and the run still reported "16 passed, 0 failed", rc 0.
echo "$AMSG" | grep -q "$NEWTXT" \
  && pass "addMessage MUTATION executed (input schema parsed, returned the new message)" \
  || fail "addMessage MUTATION did not execute: ${AMSG:0:200}"
# getMessages again → the mutation persisted in the worker instance
QMSG2="$(gw "http://localhost:$GATE_PORT/__zeroship/v1/getMessages")"
# NOT AN ASSERTION, and now labelled as such. The starter's store is a
# MODULE-LEVEL array (examples/starter/src/server.ts:28, pushed at :45), so this
# only holds when both calls land in the SAME isolate - which the platform does
# not promise: the worker runs one isolate per (app, live deploy) per thread with
# LRU eviction. Measured 2026-08-11: the pass has NEVER fired, in a clean run or
# a mutated one, and because its else-arm was an `echo` carrying a pre-written
# excuse, nothing surfaced that. Reported, not asserted - promoting it would pin
# a guarantee the platform does not make.
if echo "$QMSG2" | grep -q "$NEWTXT"; then
  echo "    note: the added message was visible on the next getMessages (same isolate served both)"
else
  echo "    note: the added message was NOT visible on the next getMessages - expected"
  echo "          whenever the two calls land in different isolates. In-isolate state is"
  echo "          not a platform guarantee, so this is REPORTED, never asserted."
fi

echo ""; echo "=== Stage 6: the real app's traffic is METERED → usage_aggregates → a projected CHARGE ==="
# Drive a batch of real app requests (index + RPC), all metered dispatches.
for i in $(seq 1 60); do gw -o /dev/null "http://localhost:$GATE_PORT/__zeroship/v1/getMessages"; done
REQ=0; for _ in $(seq 1 30); do REQ="$(psql_exec -tA -c "SELECT COALESCE(SUM(total),0) FROM zeroship.usage_aggregates WHERE app_id='$APP' AND metric='requests'" 2>/dev/null | tr -d '[:space:]')"; [ -n "$REQ" ] && [ "$REQ" -ge 50 ] 2>/dev/null && break; sleep 2; done
echo "    usage_aggregates.requests for the deployed app = $REQ"
[ -n "$REQ" ] && [ "$REQ" -ge 50 ] 2>/dev/null && pass "the app's requests were METERED into usage_aggregates ($REQ ≥ 50)" || fail "app usage not metered (got '$REQ')"
CH=""; for _ in $(seq 1 20); do CH="$(curl -s "$CONTROL_URL/api/apps/$APP/projected-charge" -H "Authorization: Bearer $PAT" | jget '.projected_charge_cents')"; [ -n "$CH" ] && [ "$CH" -ge 1 ] 2>/dev/null && break; sleep 2; done
echo "    projected charge for the app = $CH cents (1c/request)"
[ -n "$CH" ] && [ "$CH" -ge 1 ] 2>/dev/null && pass "billing priced the real app's usage → projected charge $CH cents" || fail "no projected charge for the app (got '$CH')"

echo ""; echo "============================================"
echo "  Results: $PASS passed, $FAIL failed"
echo "  (real app: built with vite → deployed → served → RPC executed → billed)"
echo "============================================"
[ $FAIL -eq 0 ] && exit 0 || exit 1
