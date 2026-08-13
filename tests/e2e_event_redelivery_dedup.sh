#!/usr/bin/env bash
# ============================================================================
# e2e_event_redelivery_dedup.sh — NEW-COVERAGE E2E for the §6 dedup contract:
# a RE-DELIVERED usage event must not create a second billing identity.
#
# The stream is at-least-once (redb WAL + Kafka), so the SAME UsageEvent
# (same `event_id`, the CloudEvents idempotency key — crates/core/src/
# usage_event.rs) can appear on the topic twice. The billing forwarder posts
# each event to the provider under a STABLE per-event key
# (Lago transaction_id = UsageEvent.event_id — crates/control/src/metering/
# provider/adapters/lago.rs), so the provider dedups and zeroship never
# double-bills. The forwarder's own cumulative-commit idempotency
# (event_forwarder.rs, unit-tested) is a second layer.
#
# This exercises it deterministically against a REAL Lago by PRODUCING a real
# duplicate onto the topic (rpk topic produce) — a guaranteed re-delivery, with
# none of the flakiness of consumer-group offset games:
#   1. produce 3 distinct 'requests' events (E1=10, E2=20, E3=30) to the topic;
#      the forwarder forwards them → Lago has 3 events, value-sum 60.
#   2. produce an EXACT DUPLICATE of E2 (same event_id) — a re-delivery.
#   3. assert Lago still has 3 distinct transaction_ids and value-sum 60 (NOT 4 /
#      80): the stable key made the re-delivery idempotent. + 0 dead-letters.
#
# Only control (the forwarder) is needed — no worker/gateway/deploy; events are
# injected straight onto the stream with the creator subject set, so the
# forwarder posts them to the creator's Lago customer directly. Real-traffic
# attribution is covered by e2e_lago_billing.sh + e2e_multi_app_attribution.sh.
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
echo "  zeroship E2E — event re-delivery / dedup (§6, REAL Lago, produce-injected)"
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
for b in zeroship-control zeroship-platform-migrate; do [ -x "$BIN/$b" ] || { echo "missing $BIN/$b"; exit 2; }; done
command -v node >/dev/null && command -v openssl >/dev/null && command -v curl >/dev/null || { echo "need node/openssl/curl"; exit 2; }

ZEROSHIP_CONTROL_PORT=9175; PG_PORT=5475; RP_PORT=19175
LAGO_PORT=3480; LAGO_KEY="lago_key-hooli-1234567890"; LAGO_URL="http://localhost:$LAGO_PORT"
PGC=zs-e2e-dedup-pg; RPC=zs-e2e-dedup-redpanda
DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"; CONTROL_URL="http://localhost:$ZEROSHIP_CONTROL_PORT"
RP_BROKERS="127.0.0.1:$RP_PORT"; USAGE_TOPIC="zeroship-usage-dedup-e2e"; FWD_GROUP="zs-fwd-dedup-e2e"
WORK="$(mktemp -d -t zs-e2e-dedup-XXXXXX)"; mkdir -p "$WORK/blobs"; PIDFILE="$WORK/pids"; : > "$PIDFILE"
jget(){ node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);process.stdout.write(String(o$1??'')+'\n')}catch(e){console.log('')}})"; }
psql_exec(){ docker exec -i "$PGC" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 "$@"; }
lago(){ curl -s -H "Authorization: Bearer $LAGO_KEY" -H "Content-Type: application/json" "$@"; }
lago_txids(){ lago "$LAGO_URL/api/v1/events?external_subscription_id=$1&per_page=500" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const t=new Set((JSON.parse(s).events||[]).filter(e=>e.code==="requests").map(e=>e.transaction_id));process.stdout.write(String(t.size)+"\n")}catch(e){process.stdout.write("0\n")}})'; }
lago_value(){ lago "$LAGO_URL/api/v1/events?external_subscription_id=$1&per_page=500" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const e=(JSON.parse(s).events||[]).filter(x=>x.code==="requests");process.stdout.write(String(e.reduce((a,x)=>a+Number((x.properties||{}).value||0),0))+"\n")}catch(e){process.stdout.write("0\n")}})'; }
fwd_batch_count(){ local c; c=$(grep -ac "event_forwarder batch completed" "$WORK/control.log" 2>/dev/null); echo "${c:-0}"; }
# Produce one UsageEvent JSON record onto the stream. Args: event_id value
produce_event(){
  local eid="$1" val="$2" now; now=$(date +%s)
  local json="{\"event_id\":\"$eid\",\"source\":\"e2e-dedup\",\"subject\":{\"app\":\"$APP_UUID\",\"creator\":\"$CREATOR\"},\"meter\":\"requests\",\"value\":$val,\"event_time\":$now}"
  printf '%s\n' "$json" | docker exec -i "$RPC" rpk topic produce "$USAGE_TOPIC" --brokers "127.0.0.1:$RP_PORT" >/dev/null 2>&1
}

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
lsof -ti :"$ZEROSHIP_CONTROL_PORT" 2>/dev/null | xargs -r kill -9 2>/dev/null || true

echo ""; echo "=== Stage 1: infra (PG + redpanda + REAL Lago) + migrate + seed + control ==="
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
# Create the usage topic up front so the forwarder subscribes cleanly (no
# UnknownTopicOrPartition churn) and produce has a target.
docker exec "$RPC" rpk topic create "$USAGE_TOPIC" -p 1 >/dev/null 2>&1 || true
docker exec "$RPC" rpk topic list 2>/dev/null | grep -q "$USAGE_TOPIC" && pass "created usage topic '$USAGE_TOPIC'" || { fail "topic create"; exit 1; }

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
[ "$(curl -s -o /dev/null -w '%{http_code}' "$LAGO_URL/health")" = "200" ] && pass "Lago api healthy" || { fail "lago api"; exit 1; }
LAGO_CID="$(lago_api_cid)"; [ -n "$LAGO_CID" ] && docker exec "$LAGO_CID" bundle exec rails db:prepare > "$WORK/lago-db-prepare.log" 2>&1 && pass "Lago DB prepared" || { fail "lago db:prepare (container=${LAGO_CID:-<UNRESOLVED: compose ps -q lago-api returned nothing>})"; tail -15 "$WORK/lago-db-prepare.log" 2>/dev/null; exit 1; }
lago -o /dev/null -w '' -X POST "$LAGO_URL/api/v1/billable_metrics" -d '{"billable_metric":{"name":"Requests","code":"requests","aggregation_type":"sum_agg","field_name":"value","recurring":false}}'
BM_ID=$(lago "$LAGO_URL/api/v1/billable_metrics/requests" | jget '.billable_metric.lago_id')
lago -o /dev/null -w '' -X POST "$LAGO_URL/api/v1/plans" -d "{\"plan\":{\"name\":\"E2E\",\"code\":\"e2e_plan\",\"interval\":\"monthly\",\"amount_cents\":0,\"amount_currency\":\"USD\",\"pay_in_advance\":false,\"charges\":[{\"billable_metric_id\":\"$BM_ID\",\"charge_model\":\"standard\",\"properties\":{\"amount\":\"0.01\"}}]}}"
[ -n "$BM_ID" ] && pass "Lago seeded metric + plan" || { fail "lago seed"; exit 1; }

MIG_LOG="$WORK/migrate.log"
"$BIN/zeroship-platform-migrate" \
  --database-url "$DBURL" --migrations-dir "$ROOT/db/migrations-ts" \
  --project-schema zeroship --project-id zeroship > "$MIG_LOG" 2>&1 \
  && pass "zeroship platform migrations applied" || { fail "migrate"; tail -20 "$MIG_LOG"; exit 1; }

# A creator + its Lago customer/subscription (external_id = creator UUID = the
# subject the forwarder posts under). An app UUID for the event subject. The
# events carry a NON-NIL creator, so the forwarder attributes them directly and
# never touches the app→creator resolver — no app_members/apps rows needed.
CREATOR="$(node -e 'console.log(require("crypto").randomUUID())')"
APP_UUID="$(node -e 'console.log(require("crypto").randomUUID())')"
psql_exec >/dev/null 2>&1 <<SQL && pass "seeded creator user" || { fail "seed creator"; exit 1; }
INSERT INTO zeroship.users (id,email,name,email_verified_at) VALUES ('$CREATOR','e2e-dedup-$CREATOR@zeroship.test'::citext,'E2E Dedup',NOW());
SQL
lago -o /dev/null -w '' -X POST "$LAGO_URL/api/v1/customers" -d "{\"customer\":{\"external_id\":\"$CREATOR\",\"name\":\"E2E\",\"currency\":\"USD\"}}"
lago -o /dev/null -w '' -X POST "$LAGO_URL/api/v1/subscriptions" -d "{\"subscription\":{\"external_customer_id\":\"$CREATOR\",\"external_id\":\"$CREATOR\",\"plan_code\":\"e2e_plan\"}}"
pass "Lago customer + subscription for creator $CREATOR"

openssl genpkey -algorithm ed25519 -out "$WORK/sk.pem" 2>/dev/null; chmod 600 "$WORK/sk.pem"
CFG_TOML="$WORK/zeroship.toml"
printf '[metering]\nredpanda_brokers = "%s"\nusage_events_topic = "%s"\n' "$RP_BROKERS" "$USAGE_TOPIC" > "$CFG_TOML"
ZEROSHIP_CONTROL_SIGNING_KEY_FILE="$WORK/sk.pem"
ZEROSHIP_GATEWAY_SIGNING_KEY_FILE="$ZEROSHIP_CONTROL_SIGNING_KEY_FILE"
e2e_export_runtime_secrets "$WORK" || exit 1
e2e_export_database_urls "$DBURL"
"$BIN/zeroship-control" --port "$ZEROSHIP_CONTROL_PORT" --config "$CFG_TOML" \
  --blob-store "$WORK/blobs" --signing-key-file "$WORK/sk.pem" \
  --meter-provider lago --invoicer-provider lago \
  --provider-config "{\"lago\":{\"api_url\":\"$LAGO_URL\",\"api_key\":\"$LAGO_KEY\",\"billable_metric_code\":\"requests\"}}" \
  --billing-forwarder-group-id "$FWD_GROUP" --spend-recompute-interval 5 > "$WORK/control.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$CONTROL_URL/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$CONTROL_URL/readyz" >/dev/null 2>&1 && pass "control healthy (forwarder → REAL Lago)" || { fail "control"; tail -30 "$WORK/control.log"; exit 1; }

echo ""; echo "=== Stage 2: produce 3 distinct 'requests' events → forwarder → REAL Lago ==="
E1="evt-$(node -e 'console.log(require("crypto").randomUUID())')"
E2="evt-$(node -e 'console.log(require("crypto").randomUUID())')"
E3="evt-$(node -e 'console.log(require("crypto").randomUUID())')"
produce_event "$E1" 10; produce_event "$E2" 20; produce_event "$E3" 30
pass "produced 3 distinct events onto the stream (E1=10, E2=20, E3=30; sum 60)"
TXN=0; VAL=0
for _ in $(seq 1 45); do TXN="$(lago_txids "$CREATOR")"; VAL="$(lago_value "$CREATOR")"; [ "$TXN" -ge 3 ] 2>/dev/null && [ "$VAL" -ge 60 ] 2>/dev/null && break; sleep 2; done
echo "    Lago: distinct transaction_ids=$TXN  value-sum=$VAL (want 3 / 60)"
{ [ "$TXN" = "3" ] && [ "$VAL" = "60" ]; } 2>/dev/null \
  && pass "forwarder delivered all 3 distinct events to Lago (3 ids, value 60)" \
  || { fail "first delivery wrong (ids=$TXN val=$VAL, want 3/60)"; grep -a "event_forwarder\|dead" "$WORK/control.log" | tail -10; }
BATCHES_BEFORE="$(fwd_batch_count)"

echo ""; echo "=== Stage 3: RE-DELIVER E2 (produce an exact duplicate) → provider dedups ==="
produce_event "$E2" 20
pass "produced an EXACT DUPLICATE of E2 (same event_id '$E2', value 20)"
# Wait until the forwarder has consumed the duplicate (a new batch appears).
for _ in $(seq 1 40); do [ "$(fwd_batch_count)" -gt "$BATCHES_BEFORE" ] 2>/dev/null && break; sleep 2; done
BATCHES_AFTER="$(fwd_batch_count)"
echo "    forwarder 'batch completed' count: before=$BATCHES_BEFORE after=$BATCHES_AFTER"
[ "$BATCHES_AFTER" -gt "$BATCHES_BEFORE" ] 2>/dev/null \
  && pass "forwarder CONSUMED the re-delivered duplicate ($BATCHES_BEFORE→$BATCHES_AFTER batches) — the dedup path genuinely ran" \
  || fail "forwarder never consumed the duplicate (stuck at $BATCHES_BEFORE)"
# Give any (deduped) provider write a beat to settle, then assert NO double count.
sleep 6
TXN2="$(lago_txids "$CREATOR")"; VAL2="$(lago_value "$CREATOR")"
echo "    Lago after re-delivery: distinct transaction_ids=$TXN2 (was 3)  value-sum=$VAL2 (was 60)"
[ "$TXN2" = "3" ] 2>/dev/null \
  && pass "re-delivery minted NO new billing identity ($TXN2 == 3) — stable transaction_id (event_id) is the idempotency key" \
  || fail "re-delivery created a new transaction_id ($TXN2 != 3) — duplicate event_id was NOT deduped (double-bill)"
[ "$VAL2" = "60" ] 2>/dev/null \
  && pass "re-delivery did NOT inflate the billed value ($VAL2 == 60, not 80) — E2 counted once despite two deliveries" \
  || fail "re-delivery inflated value to $VAL2 (want 60) — the duplicate was double-counted"

DL=$(psql_exec -tA -c "SELECT COUNT(*) FROM zeroship.provider_dead_letter" 2>/dev/null | tr -d '[:space:]')
[ "$DL" = "0" ] && pass "0 provider dead-letters (all events, incl. the duplicate, decoded + attributed)" || fail "provider_dead_letter has $DL rows"

echo ""; echo "============================================"
echo "  Results: $PASS passed, $FAIL failed"
echo "============================================"
[ $FAIL -eq 0 ] && exit 0 || exit 1
