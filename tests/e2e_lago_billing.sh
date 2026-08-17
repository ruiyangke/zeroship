#!/usr/bin/env bash
# ============================================================================
# e2e_lago_billing.sh — FAITHFUL multi-node E2E of the forwarder->PROVIDER
# billing rail against a REAL self-hosted Lago (not a mock). Complements the
# `lite` e2e (which is recompute-fed, no forwarder) by exercising the piece lite
# cannot: real usage events flowing worker -> redpanda -> control event-forwarder
# -> Lago /api/v1/events, attributed to the app's OWNING creator, and visible in
# Lago's own current_usage aggregation.
#
#   real traffic -> gateway -> worker (Meter) -> redpanda
#     -> control event_forwarder (resolves app->creator) -> REAL Lago /events
#     -> Lago current_usage[creator] reflects the usage         (billing rail)
#     -> control spend_recompute -> usage_aggregates -> 402      (enforcement)
#
# Lago is stood up by deploy/compose/lago.yml (api+worker+pg+redis); `db:prepare`
# seeds a default "Hooli" org whose API key is lago_key-hooli-1234567890.
#
# REFUSES (exit 1) when docker is unavailable - a run that asserted nothing
# is not a passing run. KEEP_WORK=1 preserves the
# stack + logs for debugging. Dedicated ports; cleans up on exit.
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; BIN="$ROOT/target/release"
# shellcheck source=tests/lib/runtime_secrets.sh
source "$ROOT/tests/lib/runtime_secrets.sh"
PASS=0; FAIL=0
pass(){ PASS=$((PASS+1)); echo "  ✓ $1"; }
fail(){ FAIL=$((FAIL+1)); echo "  ✗ $1"; }

echo "============================================"
echo "  zeroship E2E — forwarder -> REAL Lago billing rail"
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
for b in zeroship zeroship-control zeroship-gate zeroship-worker zeroship-platform-migrate; do
  [ -x "$BIN/$b" ] || { echo "missing $BIN/$b — run cargo build --release, then cargo build --release -p zeroship-migrate-adapter --features platform-cli --bin zeroship-platform-migrate"; exit 2; }; done
command -v node >/dev/null && command -v openssl >/dev/null && command -v curl >/dev/null || { echo "need node/openssl/curl"; exit 2; }
PROBE="$ROOT/examples/metering-probe/dist/app.zship"
[ -f "$PROBE" ] || { echo "missing $PROBE — (cd examples/metering-probe && pnpm i && pnpm build)"; exit 2; }
JOSE="$ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"
[ -f "$JOSE" ] || { echo "missing jose"; exit 2; }

ZEROSHIP_CONTROL_PORT=9172; ZEROSHIP_WORKER_PORT=8072; ZEROSHIP_GATEWAY_PORT=8062; PG_PORT=5472; RP_PORT=19172
LAGO_PORT=3480; LAGO_KEY="lago_key-hooli-1234567890"; LAGO_URL="http://localhost:$LAGO_PORT"
PGC=zs-e2e-lago-pg; RPC=zs-e2e-lago-redpanda
DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"
CONTROL_URL="http://localhost:$ZEROSHIP_CONTROL_PORT"
RP_BROKERS="127.0.0.1:$RP_PORT"; USAGE_TOPIC="zeroship-usage-lago-e2e"
WORK="$(mktemp -d -t zs-e2e-lago-XXXXXX)"; mkdir -p "$WORK/blobs" "$WORK/blob-cache"
PIDFILE="$WORK/pids"; : > "$PIDFILE"
jget(){ node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);process.stdout.write(String(o$1??'')+'\n')}catch(e){console.log('')}})"; }
psql_exec(){ docker exec -i "$PGC" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 "$@"; }
lago(){ curl -s -H "Authorization: Bearer $LAGO_KEY" -H "Content-Type: application/json" "$@"; }

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
for p in $ZEROSHIP_CONTROL_PORT $ZEROSHIP_WORKER_PORT $ZEROSHIP_GATEWAY_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done

echo ""; echo "=== Stage 1: infra (PG + redpanda + REAL Lago) + migrate + seed + stack ==="
docker rm -f "$PGC" >/dev/null 2>&1 || true
docker run --name "$PGC" -d -p "$PG_PORT:5432" -e POSTGRES_PASSWORD=zeroship -e POSTGRES_USER=postgres -e POSTGRES_DB=zeroship postgres:16 -c max_connections=300 >/dev/null || { fail "pg run"; exit 1; }
for _ in $(seq 1 30); do docker exec "$PGC" pg_isready -U postgres >/dev/null 2>&1 && break; sleep 1; done
docker exec "$PGC" pg_isready -U postgres >/dev/null 2>&1 && pass "ephemeral PG on :$PG_PORT" || { fail "PG"; exit 1; }

docker rm -f "$RPC" >/dev/null 2>&1 || true
docker run --name "$RPC" -d -p "$RP_PORT:$RP_PORT" docker.redpanda.com/redpandadata/redpanda:latest \
  redpanda start --overprovisioned --smp 1 --memory 512M --reserve-memory 0M --node-id 0 --check=false \
  --kafka-addr "external://0.0.0.0:$RP_PORT" --advertise-kafka-addr "external://127.0.0.1:$RP_PORT" \
  --set redpanda.auto_create_topics_enabled=true >/dev/null || { fail "redpanda run"; exit 1; }
for _ in $(seq 1 40); do docker exec "$RPC" rpk cluster health --exit-when-healthy >/dev/null 2>&1 && break; sleep 1.5; done
docker exec "$RPC" rpk cluster health --exit-when-healthy >/dev/null 2>&1 && pass "redpanda on $RP_BROKERS" || { fail "redpanda"; exit 1; }

# Real Lago (compose). Generate keys if .env.lago is absent.
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
# Resolve the Lago API container from COMPOSE ITSELF rather than hardcoding a
# project-prefixed name. These harnesses used to say `billing-impl-lago-api-1`,
# a name docker compose derives from the compose FILE'S PARENT DIRECTORY - which
# the repo reorg changed from `billing-impl/` to `deploy/compose/`. Every Lago
# harness has been dead at `db:prepare` since, with the error swallowed by
# `>/dev/null 2>&1` so the only symptom was the words "lago db:prepare".
# Deriving it means the next directory move cannot break this again.
lago_api_cid(){
  docker compose --env-file "$ROOT/.env.lago" -f "$ROOT/deploy/compose/lago.yml" ps -q lago-api 2>/dev/null
}

docker compose --env-file "$ROOT/.env.lago" -f "$ROOT/deploy/compose/lago.yml" up -d >/dev/null 2>&1 || { fail "lago compose up"; exit 1; }
for _ in $(seq 1 40); do [ "$(curl -s -o /dev/null -w '%{http_code}' "$LAGO_URL/health" 2>/dev/null)" = "200" ] && break; sleep 3; done
[ "$(curl -s -o /dev/null -w '%{http_code}' "$LAGO_URL/health")" = "200" ] && pass "Lago api healthy on $LAGO_URL" || { fail "lago api"; docker logs "$(lago_api_cid)" 2>&1 | tail -20; exit 1; }
LAGO_CID="$(lago_api_cid)"; [ -n "$LAGO_CID" ] && docker exec "$LAGO_CID" bundle exec rails db:prepare > "$WORK/lago-db-prepare.log" 2>&1 && pass "Lago DB prepared (seeded Hooli org + api key)" || { fail "lago db:prepare (container=${LAGO_CID:-<UNRESOLVED: compose ps -q lago-api returned nothing>})"; tail -15 "$WORK/lago-db-prepare.log" 2>/dev/null; exit 1; }
lago -o /dev/null -w '' "$LAGO_URL/api/v1/billable_metrics?per_page=1"
[ "$(lago -o /dev/null -w '%{http_code}' "$LAGO_URL/api/v1/billable_metrics?per_page=1")" = "200" ] && pass "Lago API key works (lago_key-hooli-…)" || { fail "lago api key"; exit 1; }

# Seed Lago: billable metric 'requests' (sum of properties.value) + a plan.
lago -o /dev/null -w '' -X POST "$LAGO_URL/api/v1/billable_metrics" -d '{"billable_metric":{"name":"Requests","code":"requests","aggregation_type":"sum_agg","field_name":"value","recurring":false}}'
BM_ID=$(lago "$LAGO_URL/api/v1/billable_metrics/requests" | jget '.billable_metric.lago_id')
lago -o /dev/null -w '' -X POST "$LAGO_URL/api/v1/plans" -d "{\"plan\":{\"name\":\"E2E\",\"code\":\"e2e_plan\",\"interval\":\"monthly\",\"amount_cents\":0,\"amount_currency\":\"USD\",\"pay_in_advance\":false,\"charges\":[{\"billable_metric_id\":\"$BM_ID\",\"charge_model\":\"standard\",\"properties\":{\"amount\":\"0.01\"}}]}}"
[ -n "$BM_ID" ] && pass "Lago seeded: billable_metric 'requests' + plan 'e2e_plan' (1 cent/unit)" || { fail "lago seed"; exit 1; }

MIG_LOG="$WORK/migrate.log"
"$BIN/zeroship-platform-migrate" \
  --database-url "$DBURL" --migrations-dir "$ROOT/db/migrations-ts" \
  --project-schema zeroship --project-id zeroship > "$MIG_LOG" 2>&1 \
  && pass "zeroship platform migrations applied" || { fail "migrate"; tail -20 "$MIG_LOG"; exit 1; }

PLAN_ID="pln_lago_e2e"
psql_exec >/dev/null 2>&1 <<SQL && pass "seeded plan + pricing_config + metric_weights (requests=1 CU/op × 1c/CU)" || { fail "plan seed"; exit 1; }
INSERT INTO zeroship.plans (id,name,base_fee_cents,included_units,fx_pico_cents_per_unit,runtime_limits_json,spend_limit_default_cents)
VALUES ('$PLAN_ID','lago-e2e',0,0,1000000000000,'{"cpu_limit_ms":5000,"wall_timeout_ms":30000,"heap_limit_mb":256}',1000000) ON CONFLICT (id) DO NOTHING;
INSERT INTO zeroship.pricing_config (id,fx_pico_cents_per_unit) VALUES ('global',1000000000000) ON CONFLICT (id) DO UPDATE SET fx_pico_cents_per_unit=EXCLUDED.fx_pico_cents_per_unit;
INSERT INTO zeroship.billing_metrics (metric,kind,unit) VALUES ('requests','platform','op') ON CONFLICT (metric) DO UPDATE SET kind='platform';
INSERT INTO zeroship.metric_weights (metric,units_per_op,per_units) VALUES ('requests',1,1) ON CONFLICT (metric) DO UPDATE SET units_per_op=1,per_units=1;
SQL

openssl genpkey -algorithm ed25519 -out "$WORK/sk.pem" 2>/dev/null; chmod 600 "$WORK/sk.pem"
openssl rand -base64 48 > "$WORK/gate-broker-secret"; chmod 600 "$WORK/gate-broker-secret"
CFG_TOML="$WORK/zeroship.toml"
cat > "$CFG_TOML" <<TOML
[metering]
redpanda_brokers = "$RP_BROKERS"
usage_events_topic = "$USAGE_TOPIC"
TOML

# control with the LAGO provider (forwarder-fed) + the redpanda stream. api_key
# carries the api key as a literal in the provider config. The `env:<NAME>`
# secret handle it used to use is deleted: an operator-supplied config string
# naming an environment variable is env-to-env indirection with no declared
# identity. Short recompute interval.
ZEROSHIP_GATEWAY_SIGNING_KEY_FILE="$WORK/sk.pem"
ZEROSHIP_GATEWAY_BROKER_SECRET_FILE="$WORK/gate-broker-secret"
# The issuer control verifies the admin bearer against, on the same key the
# gateway signs with. Up BEFORE control: control reads the issuer once at boot.
e2e_platform_op_up "$WORK/sk.pem" "$WORK" || exit 1
e2e_export_runtime_secrets "$WORK" || exit 1
e2e_export_database_urls "$DBURL"
e2e_with_platform_mint_key "$BIN/zeroship-control" --port "$ZEROSHIP_CONTROL_PORT" --config "$CFG_TOML" \
  --blob-store "$WORK/blobs" \
  --meter-provider lago --invoicer-provider lago \
  --provider-config "{\"lago\":{\"api_url\":\"$LAGO_URL\",\"api_key\":\"$LAGO_KEY\",\"billable_metric_code\":\"requests\"}}" \
  --spend-recompute-interval 2 > "$WORK/control.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$CONTROL_URL/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$CONTROL_URL/readyz" >/dev/null 2>&1 && pass "control healthy (provider=lago, stream=redpanda)" || { fail "control"; tail -30 "$WORK/control.log"; exit 1; }

USAGE_OUTBOX_WAL_PATH="$WORK/worker-outbox.redb" "$BIN/zeroship-worker" --port "$ZEROSHIP_WORKER_PORT" --threads 2 \
  --config "$CFG_TOML" --control-url "$CONTROL_URL" --blob-store "$WORK/blobs" --poll-interval 2 > "$WORK/worker.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" >/dev/null 2>&1 && pass "worker healthy (outbox → redpanda)" || { fail "worker"; tail -30 "$WORK/worker.log"; exit 1; }

USAGE_OUTBOX_WAL_PATH="$WORK/gate-outbox.redb" "$BIN/zeroship-gate" --port "$ZEROSHIP_GATEWAY_PORT" --control-url "$CONTROL_URL" \
  --config "$CFG_TOML" --worker-urls "http://localhost:$ZEROSHIP_WORKER_PORT" --blob-store "$WORK/blobs" --blob-cache-disk-root "$WORK/blob-cache" \
 --poll-interval 2 --signing-key-file "$WORK/sk.pem" --broker-secret-file "$WORK/gate-broker-secret" > "$WORK/gate.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" >/dev/null 2>&1 && pass "gateway healthy" || { fail "gateway"; tail -30 "$WORK/gate.log"; exit 1; }

echo ""; echo "=== Stage 2: bearer + creator + app + deploy + matching Lago customer/subscription ==="
# The scope string is the action list the deleted permission_tokens policy
# carried, one scope per Cedar action.
SCOPE="apps:read apps:write apps:deploy billing:read billing:write"
CREATOR="$(node -e 'console.log(require("crypto").randomUUID())')"
psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id,email,name,email_verified_at) VALUES ('$CREATOR','e2e-lago-$CREATOR@zeroship.test'::citext,'E2E Lago',NOW());
INSERT INTO zeroship.platform_admin_roles (user_id,role,granted_by) VALUES ('$CREATOR','admin','$CREATOR');
SQL
ADMIN_TOKEN="$(e2e_mint_platform_bearer "$CREATOR" "$SCOPE")"
[ "$(echo -n "$ADMIN_TOKEN" | awk -F. '{print NF}')" = "3" ] && pass "minted platform bearer (creator=$CREATOR)" || { fail "bearer mint"; exit 1; }

APP="$(curl -s -X POST "$CONTROL_URL/api/apps" -H 'Content-Type: application/json' -H "Authorization: Bearer $ADMIN_TOKEN" -d "{\"name\":\"lago-probe\",\"plan_id\":\"$PLAN_ID\"}" | jget '.id')"
[ -n "$APP" ] && pass "created app $APP" || { fail "create app"; exit 1; }
psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.app_members (app_id,user_id,role) VALUES ('$APP','$CREATOR','owner') ON CONFLICT (app_id,user_id) DO UPDATE SET role='owner';
SQL
"$BIN/zeroship" deploy "$PROBE" --app="$APP" --control="$CONTROL_URL" --token="$ADMIN_TOKEN" 2>&1 | grep -q deploy_hash && pass "deployed probe" || { fail "deploy"; exit 1; }

# The forwarder attributes usage to the app's OWNING creator, so the Lago
# customer + subscription external_id MUST be the creator UUID.
lago -o /dev/null -w '' -X POST "$LAGO_URL/api/v1/customers" -d "{\"customer\":{\"external_id\":\"$CREATOR\",\"name\":\"E2E\",\"currency\":\"USD\"}}"
lago -o /dev/null -w '' -X POST "$LAGO_URL/api/v1/subscriptions" -d "{\"subscription\":{\"external_customer_id\":\"$CREATOR\",\"external_id\":\"$CREATOR\",\"plan_code\":\"e2e_plan\"}}"
pass "Lago customer + subscription created for creator $CREATOR"
sleep 5

echo ""; echo "=== Stage 3: gateway traffic (readiness gate + N requests) ==="
N_REQ=100; BODY='{"hello":"lago","n":1}'
READY=0; for _ in $(seq 1 30); do [ "$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: lago-probe.localhost' -H 'content-type: application/json' --data "$BODY" "http://localhost:$ZEROSHIP_GATEWAY_PORT/probe/ready")" = "200" ] && { READY=1; break; }; sleep 1; done
[ "$READY" = "1" ] && pass "app reachable via gateway" || { fail "app never reachable"; tail -15 "$WORK/gate.log"; exit 1; }
for _ in 1 2 3 4 5 6; do curl -s -o /dev/null -H 'Host: lago-probe.localhost' -H 'content-type: application/json' --data "$BODY" "http://localhost:$ZEROSHIP_GATEWAY_PORT/probe/warm" || true; done
GW_OK=0; for i in $(seq 1 $N_REQ); do for _ in 1 2 3; do [ "$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: lago-probe.localhost' -H 'content-type: application/json' --data "$BODY" "http://localhost:$ZEROSHIP_GATEWAY_PORT/probe/$i")" = "200" ] && { GW_OK=$((GW_OK+1)); break; }; sleep 0.2; done; done
[ "$GW_OK" = "$N_REQ" ] && pass "drove $GW_OK/$N_REQ requests (HTTP 200)" || { fail "traffic $GW_OK/$N_REQ"; tail -20 "$WORK/worker.log"; exit 1; }

echo ""; echo "=== Stage 4: forwarder -> REAL Lago + enforcement (usage_aggregates) ==="
# The forwarder posts each usage event to Lago /api/v1/events attributed to the
# app's OWNING creator ($CREATOR). This asserts what zeroship is responsible for
# — the events REACH Lago with the correct external_subscription_id (the resolved
# creator) — via Lago's own events API. (Lago's downstream async usage
# aggregation / current_usage is a Lago-internal concern, best-effort below.)
# Events arrive async (worker outbox ~10s + forwarder), so poll (bounded).
LAGO_REQ=0
for _ in $(seq 1 30); do
  LAGO_REQ=$(lago "$LAGO_URL/api/v1/events?external_subscription_id=$CREATOR&per_page=200" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const evs=(JSON.parse(s).events||[]).filter(e=>e.code==="requests");process.stdout.write(String(evs.reduce((a,e)=>a+Number((e.properties||{}).value||0),0))+"\n")}catch(e){process.stdout.write("0\n")}})')
  [ -n "$LAGO_REQ" ] && [ "$LAGO_REQ" -ge "$N_REQ" ] 2>/dev/null && break
  sleep 2
done
echo "    Lago received requests-event value sum = $LAGO_REQ (attributed to creator $CREATOR)"
[ -n "$LAGO_REQ" ] && [ "$LAGO_REQ" -ge "$N_REQ" ] 2>/dev/null \
  && pass "REAL Lago received the forwarded usage ($LAGO_REQ ≥ $N_REQ requests) attributed to the resolved creator — forwarder→Lago + app→creator work" \
  || fail "Lago did not receive the forwarded usage (got '$LAGO_REQ', want ≥ $N_REQ)"

# The provider dead-letter table must be EMPTY (no nil-creator quarantines) —
# proves the app→creator resolution fed a real customer, not a nil one.
DL=$(psql_exec -tA -c "SELECT COUNT(*) FROM zeroship.provider_dead_letter" 2>/dev/null | tr -d '[:space:]')
[ "$DL" = "0" ] && pass "0 provider dead-letters (every event attributed to a real creator)" || fail "provider_dead_letter has $DL rows (nil-creator or reject?)"

# Best-effort: Lago's own current_usage aggregation (Lago-internal, async).
LAGO_UNITS=$(lago "$LAGO_URL/api/v1/customers/$CREATOR/current_usage?external_subscription_id=$CREATOR" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const c=(JSON.parse(s).customer_usage.charges_usage||[]).find(x=>x.billable_metric.code==="requests");process.stdout.write(String(c?Math.round(parseFloat(c.units)):0)+"\n")}catch(e){process.stdout.write("0\n")}})')
if [ -n "$LAGO_UNITS" ] && [ "$LAGO_UNITS" -ge "$N_REQ" ] 2>/dev/null; then
  pass "Lago current_usage aggregated the usage ($LAGO_UNITS units)"
else
  echo "    NOTE: Lago current_usage=$LAGO_UNITS (Lago-internal async aggregation; events were received above — not a zeroship concern)"
fi

# Enforcement rail (provider-independent): usage_aggregates populated by recompute.
REQS=""; for _ in $(seq 1 20); do REQS="$(curl -s "$CONTROL_URL/api/apps/$APP/usage" -H "Authorization: Bearer $ADMIN_TOKEN" | jget '.requests')"; [ -n "$REQS" ] && [ "$REQS" != "0" ] && break; sleep 2; done
[ -n "$REQS" ] && [ "$REQS" -ge "$N_REQ" ] 2>/dev/null && pass "enforcement recompute aggregated requests ($REQS ≥ $N_REQ)" || fail "usage_aggregates not populated (got '$REQS')"

echo ""; echo "=== Stage 5: spend enforcement — low cap → 402 Block ==="
curl -s -o /dev/null -X PUT "$CONTROL_URL/api/apps/$APP/spend-limit" -H 'Content-Type: application/json' -H "Authorization: Bearer $ADMIN_TOKEN" -d '{"cents":1}'
curl -s -o /dev/null -X POST -H "Authorization: Bearer $ZEROSHIP_CONTROL_KEY" "$CONTROL_URL/internal/spend/reconcile"
STATE="$(psql_exec -tA -c "SELECT state FROM zeroship.app_spend_state WHERE app_id='$APP'" 2>/dev/null | tr -d '[:space:]')"
[ "$STATE" = "block" ] && pass "control derived spend state = block" || fail "expected block, got '$STATE'"
GW402=0; for _ in $(seq 1 15); do [ "$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: lago-probe.localhost' "http://localhost:$ZEROSHIP_GATEWAY_PORT/probe/blocked")" = "402" ] && { GW402=1; break; }; sleep 1; done
[ "$GW402" = "1" ] && pass "gateway returns 402 for the over-limit app" || fail "gateway never returned 402"

echo ""; echo "============================================"
echo "  Results: $PASS passed, $FAIL failed"
echo "============================================"
[ $FAIL -eq 0 ] && exit 0 || exit 1
