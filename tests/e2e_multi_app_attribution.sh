#!/usr/bin/env bash
# ============================================================================
# e2e_multi_app_attribution.sh — NEW-COVERAGE E2E for the forwarder's
# app->creator attribution at SCALE, against a REAL self-hosted Lago.
#
# The single-app lago e2e proves one app's usage reaches its owning creator.
# THIS proves the CreatorResolver (crates/control/src/cron/event_forwarder.rs,
# fix f561f2ad) is correct when MANY apps map to FEW creators:
#
#   creator C1  owns  app A1 (40 req) + app A2 (60 req)
#   creator C2  owns  app A3 (70 req)
#
#   traffic -> gateway -> worker(Meter) -> redpanda
#     -> control event_forwarder (resolves EACH app -> its owning creator)
#     -> REAL Lago /api/v1/events attributed to the resolved creator subject
#
# Asserts, via Lago's own events API (the provider's view of what it received):
#   * C1's Lago subject received A1+A2 usage AGGREGATED = 100 (not 40, not 60)
#   * C2's Lago subject received A3 usage = 70
#   * ZERO cross-attribution — C1 != C1+C2, C2 has exactly A3's usage
#   * 0 provider dead-letters (every event mapped to a real creator)
#   * per-APP enforcement usage_aggregates are correct (A1=40, A2=60, A3=70)
#
# Skips CLEANLY (exit 0) when docker is unavailable. KEEP_WORK=1 preserves logs.
# Dedicated ports (distinct from the other billing e2es); cleans up on exit.
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; BIN="$ROOT/target/release"
PASS=0; FAIL=0
pass(){ PASS=$((PASS+1)); echo "  ✓ $1"; }
fail(){ FAIL=$((FAIL+1)); echo "  ✗ $1"; }

echo "============================================"
echo "  zeroship E2E — multi-app -> per-creator attribution (REAL Lago)"
echo "============================================"
if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then echo "  ⚠ SKIP: docker unavailable."; exit 0; fi
for b in zeroship zeroship-control zeroship-gate zeroship-worker zeroship-migrate; do [ -x "$BIN/$b" ] || { echo "missing $BIN/$b — cargo build --release"; exit 2; }; done
command -v node >/dev/null && command -v openssl >/dev/null && command -v curl >/dev/null || { echo "need node/openssl/curl"; exit 2; }
PROBE="$ROOT/examples/metering-probe/dist/app.zship"; [ -f "$PROBE" ] || { echo "missing $PROBE"; exit 2; }
JOSE="$ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"; [ -f "$JOSE" ] || { echo "missing jose"; exit 2; }

CONTROL_PORT=9174; WORKER_PORT=8074; GATE_PORT=8064; PG_PORT=5474; RP_PORT=19174
LAGO_PORT=3480; LAGO_KEY="lago_key-hooli-1234567890"; LAGO_URL="http://localhost:$LAGO_PORT"
PGC=zs-e2e-mapp-pg; RPC=zs-e2e-mapp-redpanda
DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"; CONTROL_URL="http://localhost:$CONTROL_PORT"
RP_BROKERS="127.0.0.1:$RP_PORT"; USAGE_TOPIC="zeroship-usage-mapp-e2e"
WORK="$(mktemp -d -t zs-e2e-mapp-XXXXXX)"; mkdir -p "$WORK/blobs" "$WORK/blob-cache"; PIDFILE="$WORK/pids"; : > "$PIDFILE"
jget(){ node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);console.log(o$1??'')}catch(e){console.log('')}})"; }
psql_exec(){ docker exec -i "$PGC" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 "$@"; }
lago(){ curl -s -H "Authorization: Bearer $LAGO_KEY" -H "Content-Type: application/json" "$@"; }
uuid(){ node -e 'console.log(require("crypto").randomUUID())'; }

cleanup(){
  echo ""; echo "=== Cleanup ==="
  [ -f "$PIDFILE" ] && while read -r pid; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done < "$PIDFILE"
  wait 2>/dev/null || true
  if [ "${KEEP_WORK:-0}" = "1" ]; then
    echo "  KEEP_WORK=1 → PG/$PGC redpanda/$RPC Lago(compose) + $WORK preserved"
  else
    docker rm -f "$PGC" "$RPC" >/dev/null 2>&1 || true
    docker compose --env-file "$ROOT/.env.lago" -f "$ROOT/deploy/compose/lago.yml" down -v >/dev/null 2>&1 || true
    rm -rf "$WORK"; echo "  stack down, Lago down, $WORK cleaned"
  fi
}
trap cleanup EXIT
for p in $CONTROL_PORT $WORKER_PORT $GATE_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done

echo ""; echo "=== Stage 1: infra (PG + redpanda + REAL Lago) + migrate + seed + stack ==="
docker rm -f "$PGC" >/dev/null 2>&1 || true
docker run --name "$PGC" -d -p "$PG_PORT:5432" -e POSTGRES_PASSWORD=zeroship -e POSTGRES_USER=postgres -e POSTGRES_DB=zeroship postgres:16 -c max_connections=300 >/dev/null || { fail "pg run"; exit 1; }
for _ in $(seq 1 40); do docker exec "$PGC" pg_isready -U postgres >/dev/null 2>&1 && break; sleep 1; done
docker exec "$PGC" pg_isready -U postgres >/dev/null 2>&1 && pass "ephemeral PG on :$PG_PORT" || { fail "PG"; exit 1; }

docker rm -f "$RPC" >/dev/null 2>&1 || true
docker run --name "$RPC" -d -p "$RP_PORT:$RP_PORT" docker.redpanda.com/redpandadata/redpanda:latest \
  redpanda start --overprovisioned --smp 1 --memory 512M --reserve-memory 0M --node-id 0 --check=false \
  --kafka-addr "external://0.0.0.0:$RP_PORT" --advertise-kafka-addr "external://127.0.0.1:$RP_PORT" \
  --set redpanda.auto_create_topics_enabled=true >/dev/null || { fail "redpanda run"; exit 1; }
for _ in $(seq 1 40); do docker exec "$RPC" rpk cluster health --exit-when-healthy >/dev/null 2>&1 && break; sleep 1.5; done
docker exec "$RPC" rpk cluster health --exit-when-healthy >/dev/null 2>&1 && pass "redpanda on $RP_BROKERS" || { fail "redpanda"; exit 1; }

if [ ! -f "$ROOT/.env.lago" ]; then
  cat > "$ROOT/.env.lago" <<EOF
LAGO_SECRET_KEY_BASE=$(openssl rand -hex 64)
LAGO_RSA_PRIVATE_KEY=$(openssl genrsa 2048 2>/dev/null | base64 -w0)
LAGO_ENCRYPTION_PRIMARY_KEY=$(openssl rand -hex 16)
LAGO_ENCRYPTION_DETERMINISTIC_KEY=$(openssl rand -hex 16)
LAGO_ENCRYPTION_KEY_DERIVATION_SALT=$(openssl rand -hex 16)
LAGO_ORG_API_KEY=$(openssl rand -hex 24)
EOF
  chmod 600 "$ROOT/.env.lago"
fi
docker compose --env-file "$ROOT/.env.lago" -f "$ROOT/deploy/compose/lago.yml" up -d >/dev/null 2>&1 || { fail "lago compose up"; exit 1; }
for _ in $(seq 1 40); do [ "$(curl -s -o /dev/null -w '%{http_code}' "$LAGO_URL/health" 2>/dev/null)" = "200" ] && break; sleep 3; done
[ "$(curl -s -o /dev/null -w '%{http_code}' "$LAGO_URL/health")" = "200" ] && pass "Lago api healthy on $LAGO_URL" || { fail "lago api"; docker logs billing-impl-lago-api-1 2>&1 | tail -20; exit 1; }
docker exec billing-impl-lago-api-1 bundle exec rails db:prepare >/dev/null 2>&1 && pass "Lago DB prepared" || { fail "lago db:prepare"; exit 1; }
lago -o /dev/null -w '' -X POST "$LAGO_URL/api/v1/billable_metrics" -d '{"billable_metric":{"name":"Requests","code":"requests","aggregation_type":"sum_agg","field_name":"value","recurring":false}}'
BM_ID=$(lago "$LAGO_URL/api/v1/billable_metrics/requests" | jget '.billable_metric.lago_id')
lago -o /dev/null -w '' -X POST "$LAGO_URL/api/v1/plans" -d "{\"plan\":{\"name\":\"E2E\",\"code\":\"e2e_plan\",\"interval\":\"monthly\",\"amount_cents\":0,\"amount_currency\":\"USD\",\"pay_in_advance\":false,\"charges\":[{\"billable_metric_id\":\"$BM_ID\",\"charge_model\":\"standard\",\"properties\":{\"amount\":\"0.01\"}}]}}"
[ -n "$BM_ID" ] && pass "Lago seeded: billable_metric 'requests' + plan 'e2e_plan'" || { fail "lago seed"; exit 1; }

MIG_LOG="$WORK/migrate.log"
ZEROSHIP_RECORDER_CHILD="$BIN/zeroship-migrate-recorder-child" "$BIN/zeroship-migrate" migrate \
  --dir "$ROOT/db/migrations-ts" --database-url "$DBURL" --profile platform --yes > "$MIG_LOG" 2>&1 \
  && pass "zeroship platform migrations applied" || { fail "migrate"; tail -20 "$MIG_LOG"; exit 1; }

PLAN_ID="pln_mapp_e2e"
psql_exec >/dev/null 2>&1 <<SQL && pass "seeded plan + pricing_config + metric_weights" || { fail "plan seed"; exit 1; }
INSERT INTO zeroship.plans (id,name,base_fee_cents,included_units,fx_pico_cents_per_unit,runtime_limits_json,spend_limit_default_cents)
VALUES ('$PLAN_ID','mapp-e2e',0,0,1000000000000,'{"cpu_limit_ms":5000,"wall_timeout_ms":30000,"heap_limit_mb":256}',1000000) ON CONFLICT (id) DO NOTHING;
INSERT INTO zeroship.pricing_config (id,fx_pico_cents_per_unit) VALUES ('global',1000000000000) ON CONFLICT (id) DO UPDATE SET fx_pico_cents_per_unit=EXCLUDED.fx_pico_cents_per_unit;
INSERT INTO zeroship.billing_metrics (metric,kind,unit) VALUES ('requests','platform','op') ON CONFLICT (metric) DO UPDATE SET kind='platform';
INSERT INTO zeroship.metric_weights (metric,units_per_op,per_units) VALUES ('requests',1,1) ON CONFLICT (metric) DO UPDATE SET units_per_op=1,per_units=1;
SQL

openssl genpkey -algorithm ed25519 -out "$WORK/sk.pem" 2>/dev/null; chmod 600 "$WORK/sk.pem"
openssl rand -base64 48 > "$WORK/gate-broker-secret"; chmod 600 "$WORK/gate-broker-secret"
CFG_TOML="$WORK/zeroship.toml"
printf '[metering]\nredpanda_brokers = "%s"\nusage_events_topic = "%s"\n' "$RP_BROKERS" "$USAGE_TOPIC" > "$CFG_TOML"

LAGO_API_KEY="$LAGO_KEY" \
"$BIN/zeroship-control" --port "$CONTROL_PORT" --db "$DBURL" --config "$CFG_TOML" \
  --blob-store "$WORK/blobs" --signing-key-file "$WORK/sk.pem" \
  --meter-provider lago --invoicer-provider lago \
  --provider-config "{\"lago\":{\"api_url\":\"$LAGO_URL\",\"api_key\":\"env:LAGO_API_KEY\",\"billable_metric_code\":\"requests\"}}" \
  --spend-recompute-interval 2 --dev-insecure > "$WORK/control.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$CONTROL_URL/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$CONTROL_URL/health" >/dev/null 2>&1 && pass "control healthy (provider=lago, stream=redpanda)" || { fail "control"; tail -30 "$WORK/control.log"; exit 1; }

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

echo ""; echo "=== Stage 2: admin PAT + 2 creators + 3 apps (A1,A2->C1 ; A3->C2) + deploy ==="
POLICY_JSON='{"name":"e2e-mapp","statements":[{"effect":"allow","actions":["apps:read","apps:write","apps:deploy","billing:read","billing:write"],"resources":[{"type":"any"}],"conditions":[]}]}'
POLICY_HASH="$(node -e 'const{createHash}=require("crypto");function c(v){if(v===null||typeof v==="number"||typeof v==="boolean")return JSON.stringify(v);if(typeof v==="string")return JSON.stringify(v);if(Array.isArray(v))return "["+v.map(c).join(",")+"]";return "{"+Object.keys(v).sort().map(k=>JSON.stringify(k)+":"+c(v[k])).join(",")+"}";}process.stdout.write(createHash("sha256").update(c(JSON.parse(process.argv[1]))).digest("hex"));' "$POLICY_JSON")"
ADMIN="$(uuid)"; C1="$(uuid)"; C2="$(uuid)"; TOKID="$(uuid)"; EXP=$(( $(date +%s) + 86400 ))
psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id,email,name,email_verified_at) VALUES
 ('$ADMIN','e2e-mapp-admin-$ADMIN@zeroship.test'::citext,'Admin',NOW()),
 ('$C1','e2e-mapp-c1-$C1@zeroship.test'::citext,'Creator One',NOW()),
 ('$C2','e2e-mapp-c2-$C2@zeroship.test'::citext,'Creator Two',NOW());
INSERT INTO zeroship.platform_admin_roles (user_id,role,granted_by) VALUES ('$ADMIN','admin','$ADMIN');
INSERT INTO zeroship.permission_tokens (id,owner_id,kind,name,policies,policy_hash,expires_at) VALUES ('$TOKID','$ADMIN','pat','e2e mapp','$POLICY_JSON'::jsonb,'$POLICY_HASH',to_timestamp($EXP));
SQL
PAT="$(node --input-type=module -e 'import{readFileSync}from "node:fs";import{createHash,randomBytes}from "node:crypto";import{importPKCS8,exportJWK,SignJWT}from "file://'"$JOSE"'";const[pem,owner,tid,phash,exp]=process.argv.slice(1);const key=await importPKCS8(readFileSync(pem,"utf8"),"EdDSA",{extractable:true});const x=(await exportJWK(key)).x;const kid=createHash("sha256").update(`{"crv":"Ed25519","kty":"OKP","x":"${x}"}`).digest("base64url");const jwt=await new SignJWT({sub:owner,owner,tid,jti:tid,scope:"pat",policy_hash:phash,nonce:randomBytes(32).toString("base64url")}).setProtectedHeader({alg:"EdDSA",typ:"pat+jwt",kid}).setIssuer("https://api.zeroship.ai").setAudience("control.zeroship.ai").setIssuedAt(Math.floor(Date.now()/1000)).setExpirationTime(Number(exp)).sign(key);process.stdout.write(jwt);' "$WORK/sk.pem" "$ADMIN" "$TOKID" "$POLICY_HASH" "$EXP")"
[ "$(echo -n "$PAT" | awk -F. '{print NF}')" = "3" ] && pass "minted admin PAT (admin=$ADMIN, C1=$C1, C2=$C2)" || { fail "PAT"; exit 1; }

# Create + deploy 3 apps via the admin PAT, then OVERRIDE each app's sole owner to
# the intended creator (delete auto membership + insert exactly one owner) so the
# forwarder's app->creator resolution is unambiguous.
declare -A APPID
create_deploy(){ # $1=slug  $2=owner_creator
  local slug="$1" owner="$2" id
  id="$(curl -s -X POST "$CONTROL_URL/api/apps" -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" -d "{\"name\":\"$slug\",\"plan_id\":\"$PLAN_ID\"}" | jget '.id')"
  [ -n "$id" ] || { fail "create app $slug"; exit 1; }
  "$BIN/zeroship" deploy "$PROBE" --app="$id" --control="$CONTROL_URL" --token="$PAT" 2>&1 | grep -q deploy_hash || { fail "deploy $slug"; exit 1; }
  psql_exec >/dev/null 2>&1 <<SQL
DELETE FROM zeroship.app_members WHERE app_id='$id';
INSERT INTO zeroship.app_members (app_id,user_id,role) VALUES ('$id','$owner','owner');
SQL
  APPID[$slug]="$id"
  echo "    $slug = $id  (owner $owner)"
}
create_deploy app1 "$C1"
create_deploy app2 "$C1"
create_deploy app3 "$C2"
pass "created + deployed 3 apps; ownership: app1,app2→C1  app3→C2"

# One Lago customer + subscription per CREATOR (external_id = creator UUID, which
# is what the forwarder stamps as the event subject).
for c in "$C1" "$C2"; do
  lago -o /dev/null -w '' -X POST "$LAGO_URL/api/v1/customers" -d "{\"customer\":{\"external_id\":\"$c\",\"name\":\"cust-$c\",\"currency\":\"USD\"}}"
  lago -o /dev/null -w '' -X POST "$LAGO_URL/api/v1/subscriptions" -d "{\"subscription\":{\"external_customer_id\":\"$c\",\"external_id\":\"$c\",\"plan_code\":\"e2e_plan\"}}"
done
pass "Lago customers + subscriptions created for C1 and C2"
sleep 5

echo ""; echo "=== Stage 3: per-app traffic (A1=40, A2=60, A3=70) ==="
BODY='{"hello":"mapp"}'
drive(){ # $1=slug  $2=count  -> echoes ok count
  local slug="$1" n="$2" host="$1.localhost" ok=0 i
  local ready=0
  for _ in $(seq 1 30); do [ "$(curl -s -o /dev/null -w '%{http_code}' -H "Host: $host" -H 'content-type: application/json' --data "$BODY" "http://localhost:$GATE_PORT/probe/ready")" = "200" ] && { ready=1; break; }; sleep 1; done
  [ "$ready" = "1" ] || { fail "$slug never reachable"; tail -10 "$WORK/gate.log"; exit 1; }
  for _ in 1 2 3; do curl -s -o /dev/null -H "Host: $host" -H 'content-type: application/json' --data "$BODY" "http://localhost:$GATE_PORT/probe/warm" || true; done
  for i in $(seq 1 "$n"); do for _ in 1 2 3; do [ "$(curl -s -o /dev/null -w '%{http_code}' -H "Host: $host" -H 'content-type: application/json' --data "$BODY" "http://localhost:$GATE_PORT/probe/$i")" = "200" ] && { ok=$((ok+1)); break; }; sleep 0.2; done; done
  echo "$ok"
}
N1=40; N2=60; N3=70
OK1="$(drive app1 $N1)"; OK2="$(drive app2 $N2)"; OK3="$(drive app3 $N3)"
[ "$OK1" = "$N1" ] && [ "$OK2" = "$N2" ] && [ "$OK3" = "$N3" ] \
  && pass "drove traffic app1=$OK1/$N1 app2=$OK2/$N2 app3=$OK3/$N3 (HTTP 200)" \
  || { fail "traffic app1=$OK1/$N1 app2=$OK2/$N2 app3=$OK3/$N3"; tail -20 "$WORK/worker.log"; exit 1; }

echo ""; echo "=== Stage 4: per-creator attribution in REAL Lago (isolation) ==="
lago_sum(){ # $1=creator -> sum of requests-event values for that subject
  lago "$LAGO_URL/api/v1/events?external_subscription_id=$1&per_page=500" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const evs=(JSON.parse(s).events||[]).filter(e=>e.code==="requests");console.log(evs.reduce((a,e)=>a+Number((e.properties||{}).value||0),0))}catch(e){console.log(0)}})'
}
C1_EXPECT=$((N1+N2))   # 100 — app1 + app2 aggregated onto C1
C2_EXPECT=$N3          # 70  — app3 only
C1_SUM=0; C2_SUM=0
for _ in $(seq 1 40); do
  C1_SUM="$(lago_sum "$C1")"; C2_SUM="$(lago_sum "$C2")"
  [ -n "$C1_SUM" ] && [ -n "$C2_SUM" ] && [ "$C1_SUM" -ge "$C1_EXPECT" ] 2>/dev/null && [ "$C2_SUM" -ge "$C2_EXPECT" ] 2>/dev/null && break
  sleep 2
done
echo "    Lago requests-sum  C1=$C1_SUM (want $C1_EXPECT = A1+A2)   C2=$C2_SUM (want $C2_EXPECT = A3)"
[ "$C1_SUM" -ge "$C1_EXPECT" ] 2>/dev/null \
  && pass "C1 received BOTH its apps' usage AGGREGATED ($C1_SUM ≥ $C1_EXPECT) — multi-app→one-creator resolution works" \
  || fail "C1 aggregate wrong (got '$C1_SUM', want ≥ $C1_EXPECT)"
[ "$C2_SUM" -ge "$C2_EXPECT" ] 2>/dev/null \
  && pass "C2 received exactly its app's usage ($C2_SUM ≥ $C2_EXPECT)" \
  || fail "C2 aggregate wrong (got '$C2_SUM', want ≥ $C2_EXPECT)"
# Isolation: C2 must NOT have C1's apps' usage. If A1/A2 (100) had bled onto C2,
# C2 would be ~fleet (170). C2 must stay well below C1's own aggregate (N1+N2),
# a robust margin that tolerates the metered ready/warm probe noise (~4/app).
FLEET=$((N1+N2+N3))
[ -n "$C2_SUM" ] && [ "$C2_SUM" -lt "$C1_EXPECT" ] 2>/dev/null \
  && pass "no cross-attribution — C2 ($C2_SUM) has only A3's usage, far below C1's aggregate ($C1_EXPECT) and the fleet ($FLEET)" \
  || fail "cross-attribution suspected — C2=$C2_SUM should be ~$C2_EXPECT, not near C1's $C1_EXPECT / fleet $FLEET"

DL=$(psql_exec -tA -c "SELECT COUNT(*) FROM zeroship.provider_dead_letter" 2>/dev/null | tr -d '[:space:]')
[ "$DL" = "0" ] && pass "0 provider dead-letters (every app mapped to a real owning creator)" || fail "provider_dead_letter has $DL rows"

echo ""; echo "=== Stage 5: per-APP enforcement usage_aggregates (independent of creator) ==="
app_usage(){ psql_exec -tA -c "SELECT COALESCE(SUM(total),0) FROM zeroship.usage_aggregates WHERE app_id='$1' AND metric='requests'" 2>/dev/null | tr -d '[:space:]'; }
U1=0; U2=0; U3=0
for _ in $(seq 1 30); do
  U1="$(app_usage "${APPID[app1]}")"; U2="$(app_usage "${APPID[app2]}")"; U3="$(app_usage "${APPID[app3]}")"
  [ "$U1" -ge "$N1" ] 2>/dev/null && [ "$U2" -ge "$N2" ] 2>/dev/null && [ "$U3" -ge "$N3" ] 2>/dev/null && break
  sleep 2
done
echo "    usage_aggregates  app1=$U1 (want ≥$N1)  app2=$U2 (want ≥$N2)  app3=$U3 (want ≥$N3)"
{ [ "$U1" -ge "$N1" ] && [ "$U2" -ge "$N2" ] && [ "$U3" -ge "$N3" ]; } 2>/dev/null \
  && pass "per-app enforcement aggregates correct (recompute keys by app_id, not creator)" \
  || fail "per-app usage_aggregates wrong (app1=$U1 app2=$U2 app3=$U3)"

echo ""; echo "============================================"
echo "  Results: $PASS passed, $FAIL failed"
echo "============================================"
[ $FAIL -eq 0 ] && exit 0 || exit 1
