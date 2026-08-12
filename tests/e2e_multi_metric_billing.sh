#!/usr/bin/env bash
# ============================================================================
# e2e_spend_state_transitions.sh — NEW-COVERAGE E2E for the spend-enforcement
# STATE MACHINE end to end: Allow → Warn → Degrade → Block.
#
# The other billing e2es only exercise the Block (402) leg. This walks ALL four
# spend states and asserts both the control-derived state AND the gateway's
# per-state behavior:
#
#   spend engine (crates/control/src/spend.rs derive_state):
#     pct = spend*100/limit ; Warn>=80, Degrade>=95, Block>=100
#   gateway enforcement (crates/gateway/src/enforce.rs + router/dispatch.rs):
#     Allow   -> pass, no header
#     Warn    -> pass + `x-zs-spend-warn: 1` response header
#     Degrade -> pass (throttled 1/DEGRADE_FACTOR, NOT blocked)
#     Block   -> 402 SPEND_LIMIT
#
# DETERMINISTIC design: drive a fixed usage ONCE, read its projected charge C
# (cents), then move through the bands by changing ONLY the spend_limit and
# re-reconciling (a limit change re-derives the state immediately, bypassing the
# anti-flap deadband). Enforcement is provider-independent, so this uses the
# `lite` provider (no external billing account).
#
# REFUSES (exit 1) when docker is unavailable - a run that asserted nothing
# is not a passing run. KEEP_WORK=1 preserves logs.
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; BIN="$ROOT/target/release"
# shellcheck source=tests/lib/runtime_secrets.sh
source "$ROOT/tests/lib/runtime_secrets.sh"
PASS=0; FAIL=0
pass(){ PASS=$((PASS+1)); echo "  ✓ $1"; }
fail(){ FAIL=$((FAIL+1)); echo "  ✗ $1"; }

echo "============================================"
echo "  zeroship E2E — multi-metric billing (charge sums Σ over weighted metrics)"
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
for b in zeroship zeroship-control zeroship-gate zeroship-worker zeroship-platform-migrate; do [ -x "$BIN/$b" ] || { echo "missing $BIN/$b"; exit 2; }; done
command -v node >/dev/null && command -v openssl >/dev/null && command -v curl >/dev/null || { echo "need node/openssl/curl"; exit 2; }
PROBE="$ROOT/examples/metering-probe/dist/app.zship"; [ -f "$PROBE" ] || { echo "missing $PROBE"; exit 2; }
JOSE="$ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"; [ -f "$JOSE" ] || { echo "missing jose"; exit 2; }

CONTROL_PORT=9178; WORKER_PORT=8078; GATE_PORT=8068; PG_PORT=5478; RP_PORT=19178
PGC=zs-e2e-mm-pg; RPC=zs-e2e-mm-redpanda
DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"; CONTROL_URL="http://localhost:$CONTROL_PORT"
RP_BROKERS="127.0.0.1:$RP_PORT"; USAGE_TOPIC="zeroship-usage-mm-e2e"
WORK="$(mktemp -d -t zs-e2e-mm-XXXXXX)"; mkdir -p "$WORK/blobs" "$WORK/blob-cache"; PIDFILE="$WORK/pids"; : > "$PIDFILE"
jget(){ node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);process.stdout.write(String(o$1??'')+'\n')}catch(e){console.log('')}})"; }
psql_exec(){ docker exec -i "$PGC" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 "$@"; }
spend_state(){ psql_exec -tA -c "SELECT state::text FROM zeroship.app_spend_state WHERE app_id='$1'" 2>/dev/null | tr -d '[:space:]'; }
set_limit(){ curl -s -o /dev/null -X PUT "$CONTROL_URL/api/apps/$1/spend-limit" -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" -d "{\"cents\":$2}"; curl -s -o /dev/null -X POST -H "Authorization: Bearer $CONTROL_KEY" "$CONTROL_URL/internal/spend/reconcile"; }
# GET one probe request; echo "<status> <warnheader:0|1>"
probe_req(){ local out; out="$(curl -s -D - -o /dev/null -H 'Host: mm-probe.localhost' "http://localhost:$GATE_PORT/probe/$1" 2>/dev/null)"; local code warn; code="$(printf '%s' "$out" | awk 'NR==1{print $2}')"; warn=0; printf '%s' "$out" | grep -qiE '^x-zs-spend-warn:' && warn=1; echo "${code:-000} $warn"; }

cleanup(){
  echo ""; echo "=== Cleanup ==="
  [ -f "$PIDFILE" ] && while read -r pid; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done < "$PIDFILE"
  wait 2>/dev/null || true
  if [ "${KEEP_WORK:-0}" = "1" ]; then echo "  KEEP_WORK=1 → $PGC/$RPC + $WORK preserved";
  else docker rm -f "$PGC" "$RPC" >/dev/null 2>&1 || true; rm -rf "$WORK"; echo "  stack down, $WORK cleaned"; fi
}
trap cleanup EXIT
for p in $CONTROL_PORT $WORKER_PORT $GATE_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done

echo ""; echo "=== Stage 1: infra + migrate + seed + stack (lite provider) ==="
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

PLAN_ID="pln_mm_e2e"
# FX 1e12 pico-cents/unit = 1 cent per CU; requests weight 1 CU/op → 1 cent/request.
psql_exec >/dev/null 2>&1 <<SQL && pass "seeded plan + pricing (1 cent/request) + weights" || { fail "plan seed"; exit 1; }
INSERT INTO zeroship.plans (id,name,base_fee_cents,included_units,fx_pico_cents_per_unit,runtime_limits_json,spend_limit_default_cents)
VALUES ('$PLAN_ID','spend-e2e',0,0,1000000000000,'{"cpu_limit_ms":5000,"wall_timeout_ms":30000,"heap_limit_mb":256}',100000000) ON CONFLICT (id) DO NOTHING;
INSERT INTO zeroship.pricing_config (id,fx_pico_cents_per_unit) VALUES ('global',1000000000000) ON CONFLICT (id) DO UPDATE SET fx_pico_cents_per_unit=EXCLUDED.fx_pico_cents_per_unit;
INSERT INTO zeroship.billing_metrics (metric,kind,unit) VALUES ('requests','platform','op') ON CONFLICT (metric) DO UPDATE SET kind='platform';
INSERT INTO zeroship.metric_weights (metric,units_per_op,per_units) VALUES ('requests',1,1) ON CONFLICT (metric) DO UPDATE SET units_per_op=1,per_units=1;
INSERT INTO zeroship.billing_metrics (metric,kind,unit) VALUES ('egress_bytes','platform','byte') ON CONFLICT (metric) DO UPDATE SET kind='platform';
INSERT INTO zeroship.metric_weights (metric,units_per_op,per_units) VALUES ('egress_bytes',1,100) ON CONFLICT (metric) DO UPDATE SET units_per_op=1,per_units=100;
SQL

openssl genpkey -algorithm ed25519 -out "$WORK/sk.pem" 2>/dev/null; chmod 600 "$WORK/sk.pem"
openssl rand -base64 48 > "$WORK/gate-broker-secret"; chmod 600 "$WORK/gate-broker-secret"
CFG_TOML="$WORK/zeroship.toml"
printf '[metering]\nredpanda_brokers = "%s"\nusage_events_topic = "%s"\n' "$RP_BROKERS" "$USAGE_TOPIC" > "$CFG_TOML"

SIGNING_KEY_FILE="$WORK/sk.pem"
GATEWAY_SIGNING_KEY_FILE="$SIGNING_KEY_FILE"
GATEWAY_BROKER_SECRET_FILE="$WORK/gate-broker-secret"
e2e_export_runtime_secrets "$WORK" || exit 1
"$BIN/zeroship-control" --port "$CONTROL_PORT" --db "$DBURL" --config "$CFG_TOML" \
  --blob-store "$WORK/blobs" --signing-key-file "$WORK/sk.pem" \
  --stripe-base-url "http://127.0.0.1:1" --stripe-secret-key "sk_test_unused" \
  --meter-provider lite --invoicer-provider lite --allow-unsupported-billing \
  --spend-recompute-interval 2 > "$WORK/control.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$CONTROL_URL/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$CONTROL_URL/health" >/dev/null 2>&1 && pass "control healthy (lite provider, stream=redpanda)" || { fail "control"; tail -30 "$WORK/control.log"; exit 1; }

USAGE_OUTBOX_WAL_PATH="$WORK/worker-outbox.redb" "$BIN/zeroship-worker" --port "$WORKER_PORT" --worker-threads 2 \
  --config "$CFG_TOML" --control "$CONTROL_URL" --db "$DBURL" --blob-store "$WORK/blobs" --poll-interval 2 > "$WORK/worker.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$WORKER_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$WORKER_PORT/health" >/dev/null 2>&1 && pass "worker healthy" || { fail "worker"; tail -30 "$WORK/worker.log"; exit 1; }

USAGE_OUTBOX_WAL_PATH="$WORK/gate-outbox.redb" "$BIN/zeroship-gate" --port "$GATE_PORT" --control "$CONTROL_URL" \
  --config "$CFG_TOML" --workers "http://localhost:$WORKER_PORT" --blob-store "$WORK/blobs" --blob-cache-disk-root "$WORK/blob-cache" \
  --db "$DBURL" --poll-interval 2 --signing-key-file "$WORK/sk.pem" --gateway-broker-secret-file "$WORK/gate-broker-secret" > "$WORK/gate.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$GATE_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$GATE_PORT/health" >/dev/null 2>&1 && pass "gateway healthy" || { fail "gateway"; tail -30 "$WORK/gate.log"; exit 1; }

echo ""; echo "=== Stage 2: PAT + creator + app + deploy ==="
POLICY_JSON='{"name":"e2e-mm","statements":[{"effect":"allow","actions":["apps:read","apps:write","apps:deploy","billing:read","billing:write"],"resources":[{"type":"any"}],"conditions":[]}]}'
POLICY_HASH="$(node -e 'const{createHash}=require("crypto");function c(v){if(v===null||typeof v==="number"||typeof v==="boolean")return JSON.stringify(v);if(typeof v==="string")return JSON.stringify(v);if(Array.isArray(v))return "["+v.map(c).join(",")+"]";return "{"+Object.keys(v).sort().map(k=>JSON.stringify(k)+":"+c(v[k])).join(",")+"}";}process.stdout.write(createHash("sha256").update(c(JSON.parse(process.argv[1]))).digest("hex"));' "$POLICY_JSON")"
CREATOR="$(node -e 'console.log(require("crypto").randomUUID())')"; TOKID="$(node -e 'console.log(require("crypto").randomUUID())')"; EXP=$(( $(date +%s) + 86400 ))
psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id,email,name,email_verified_at) VALUES ('$CREATOR','e2e-mm-$CREATOR@zeroship.test'::citext,'E2E MM',NOW());
INSERT INTO zeroship.platform_admin_roles (user_id,role,granted_by) VALUES ('$CREATOR','admin','$CREATOR');
INSERT INTO zeroship.permission_tokens (id,owner_id,kind,name,policies,policy_hash,expires_at) VALUES ('$TOKID','$CREATOR','pat','e2e spend','$POLICY_JSON'::jsonb,'$POLICY_HASH',to_timestamp($EXP));
SQL
PAT="$(node --input-type=module -e 'import{readFileSync}from "node:fs";import{createHash,randomBytes}from "node:crypto";import{importPKCS8,exportJWK,SignJWT}from "file://'"$JOSE"'";const[pem,owner,tid,phash,exp]=process.argv.slice(1);const key=await importPKCS8(readFileSync(pem,"utf8"),"EdDSA",{extractable:true});const x=(await exportJWK(key)).x;const kid=createHash("sha256").update(`{"crv":"Ed25519","kty":"OKP","x":"${x}"}`).digest("base64url");const jwt=await new SignJWT({sub:owner,owner,tid,jti:tid,scope:"pat",policy_hash:phash,nonce:randomBytes(32).toString("base64url")}).setProtectedHeader({alg:"EdDSA",typ:"pat+jwt",kid}).setIssuer("https://api.zeroship.ai").setAudience("control.zeroship.ai").setIssuedAt(Math.floor(Date.now()/1000)).setExpirationTime(Number(exp)).sign(key);process.stdout.write(jwt);' "$WORK/sk.pem" "$CREATOR" "$TOKID" "$POLICY_HASH" "$EXP")"
[ "$(echo -n "$PAT" | awk -F. '{print NF}')" = "3" ] && pass "minted PAT (creator=$CREATOR)" || { fail "PAT"; exit 1; }
APP="$(curl -s -X POST "$CONTROL_URL/api/apps" -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" -d "{\"name\":\"mm-probe\",\"plan_id\":\"$PLAN_ID\"}" | jget '.id')"
[ -n "$APP" ] && pass "created app $APP" || { fail "create app"; exit 1; }
psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.app_members (app_id,user_id,role) VALUES ('$APP','$CREATOR','owner') ON CONFLICT (app_id,user_id) DO UPDATE SET role='owner';
SQL
"$BIN/zeroship" deploy "$PROBE" --app="$APP" --control="$CONTROL_URL" --token="$PAT" 2>&1 | grep -q deploy_hash && pass "deployed probe" || { fail "deploy"; exit 1; }
sleep 5

echo ""; echo "=== Stage 3: drive traffic → both 'requests' AND 'egress_bytes' flow to usage_aggregates ==="
N_REQ=100; BODY='{"hello":"multi-metric","pad":"........................................................"}'
READY=0; for _ in $(seq 1 30); do [ "$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: mm-probe.localhost' -H 'content-type: application/json' --data "$BODY" "http://localhost:$GATE_PORT/probe/ready")" = "200" ] && { READY=1; break; }; sleep 1; done
[ "$READY" = "1" ] && pass "app reachable" || { fail "app never reachable"; tail -15 "$WORK/gate.log"; exit 1; }
GW_OK=0; for i in $(seq 1 $N_REQ); do for _ in 1 2 3; do [ "$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: mm-probe.localhost' -H 'content-type: application/json' --data "$BODY" "http://localhost:$GATE_PORT/probe/$i")" = "200" ] && { GW_OK=$((GW_OK+1)); break; }; sleep 0.2; done; done
[ "$GW_OK" = "$N_REQ" ] && pass "drove $GW_OK/$N_REQ requests" || { fail "traffic $GW_OK/$N_REQ"; exit 1; }

usage_of(){ psql_exec -tA -c "SELECT COALESCE(SUM(total),0) FROM zeroship.usage_aggregates WHERE app_id='$APP' AND metric='$1'" 2>/dev/null | tr -d '[:space:]'; }
# Poll until requests + egress_bytes are present AND STABLE (recompute replaces the
# full snapshot each cycle, so once traffic stops + the outbox drains it is fixed).
R=0; EGR=0; pR=-1; pE=-1; metrics=0
for _ in $(seq 1 45); do
  R="$(usage_of requests)"; EGR="$(usage_of egress_bytes)"
  { [ "$R" -ge "$N_REQ" ] && [ "$EGR" -ge 100 ] && [ "$R" = "$pR" ] && [ "$EGR" = "$pE" ]; } 2>/dev/null && break
  pR="$R"; pE="$EGR"; sleep 2
done
metrics="$(psql_exec -tA -c "SELECT COUNT(DISTINCT metric) FROM zeroship.usage_aggregates WHERE app_id='$APP' AND total>0" 2>/dev/null | tr -d '[:space:]')"
echo "    usage_aggregates (stable): requests=$R  egress_bytes=$EGR  (distinct metrics with usage=$metrics)"
{ [ "$R" -ge "$N_REQ" ] && [ "$EGR" -ge 100 ] && [ "$metrics" -ge 2 ]; } 2>/dev/null \
  && pass "multiple platform metrics flow to usage_aggregates (requests=$R, egress_bytes=$EGR, ≥2 metrics with usage)" \
  || { fail "multi-metric usage missing (requests=$R egress=$EGR metrics=$metrics)"; tail -15 "$WORK/control.log"; exit 1; }

echo ""; echo "=== Stage 4: projected charge = Σ over weighted metrics (requests + egress_bytes) ==="
# CU: requests=floor(R*1/1)=R ; egress=floor(EGR*1/100). FX 1e12 → 1 cent/CU.
# Multi-metric charge must be R + floor(EGR/100) — strictly MORE than requests-only.
EGR_CU=$(( EGR / 100 )); EXPECT=$(( R + EGR_CU ))
[ "$EGR_CU" -ge 1 ] 2>/dev/null && pass "egress contributes $EGR_CU CU (non-zero → a genuine 2nd priced metric)" || { fail "egress CU is 0 (EGR=$EGR); can't prove multi-metric"; exit 1; }
C=""
for _ in $(seq 1 20); do C="$(curl -s "$CONTROL_URL/api/apps/$APP/projected-charge" -H "Authorization: Bearer $PAT" | jget '.projected_charge_cents')"; [ -n "$C" ] && [ "$C" -ge "$R" ] 2>/dev/null && break; sleep 2; done
echo "    projected charge C = $C cents ; expected R + floor(EGR/100) = $R + $EGR_CU = $EXPECT"
[ "$C" = "$EXPECT" ] 2>/dev/null \
  && pass "projected charge = $C == requests($R) + egress($EGR_CU) CU — multi-metric pricing sums correctly" \
  || fail "projected charge $C != expected $EXPECT (multi-metric sum wrong)"
[ -n "$C" ] && [ "$C" -gt "$R" ] 2>/dev/null \
  && pass "multi-metric charge ($C) strictly exceeds the requests-only charge ($R) — the 2nd metric IS billed" \
  || fail "charge $C did not exceed requests-only $R (egress not billed)"

echo ""; echo "============================================"
echo "  Results: $PASS passed, $FAIL failed"
echo "============================================"
[ $FAIL -eq 0 ] && exit 0 || exit 1
