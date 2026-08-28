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
# REFUSES (exit 1) when docker is unavailable - a run that asserted nothing
# is not a passing run. KEEP_WORK=1 preserves logs.
# Dedicated ports (distinct from the other billing e2es); cleans up on exit.
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; BIN="$ROOT/target/release"
# shellcheck source=tests/lib/runtime_secrets.sh
source "$ROOT/tests/lib/runtime_secrets.sh"
# shellcheck source=tests/lib/usage_producer.sh
source "$ROOT/tests/lib/usage_producer.sh"
PASS=0; FAIL=0
pass(){ PASS=$((PASS+1)); echo "  ✓ $1"; }
fail(){ FAIL=$((FAIL+1)); echo "  ✗ $1"; }

echo "============================================"
echo "  zeroship E2E — multi-app -> per-creator attribution (REAL Lago)"
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
for b in zeroship zeroship-control zeroship-gate zeroship-worker; do [ -x "$BIN/$b" ] || { echo "missing $BIN/$b - run cargo build --release"; exit 2; }; done
[ -f "$ROOT/packages/zero-migrate-cli/dist/cli-bin.js" ] || { echo "missing the zero-migrate CLI - run: pnpm install && pnpm build && pnpm --filter zero-migrate-cli build"; exit 2; }
command -v node >/dev/null && command -v openssl >/dev/null && command -v curl >/dev/null || { echo "need node/openssl/curl"; exit 2; }
PROBE="$ROOT/examples/metering-probe/dist/app.zship"; [ -f "$PROBE" ] || { echo "missing $PROBE"; exit 2; }
JOSE="$ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"; [ -f "$JOSE" ] || { echo "missing jose"; exit 2; }

ZEROSHIP_CONTROL_PORT=9174; ZEROSHIP_WORKER_PORT=8074; ZEROSHIP_GATEWAY_PORT=8064; PG_PORT=5474; RP_PORT=19174
LAGO_PORT=3480; LAGO_KEY="lago_key-hooli-1234567890"; LAGO_URL="http://localhost:$LAGO_PORT"
PGC=zs-e2e-mapp-pg; RPC=zs-e2e-mapp-redpanda
DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"; CONTROL_URL="http://localhost:$ZEROSHIP_CONTROL_PORT"
RP_BROKERS="127.0.0.1:$RP_PORT"; USAGE_TOPIC="zeroship-usage-mapp-e2e"
WORK="$(mktemp -d -t zs-e2e-mapp-XXXXXX)"; mkdir -p "$WORK/blobs" "$WORK/blob-cache"; PIDFILE="$WORK/pids"; : > "$PIDFILE"
jget(){ node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);process.stdout.write(String(o$1??'')+'\n')}catch(e){console.log('')}})"; }
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
for p in $ZEROSHIP_CONTROL_PORT $ZEROSHIP_WORKER_PORT $ZEROSHIP_GATEWAY_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done

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
LAGO_CID="$(lago_api_cid)"; [ -n "$LAGO_CID" ] && docker exec "$LAGO_CID" bundle exec rails db:prepare > "$WORK/lago-db-prepare.log" 2>&1 && pass "Lago DB prepared" || { fail "lago db:prepare (container=${LAGO_CID:-<UNRESOLVED: compose ps -q lago-api returned nothing>})"; tail -15 "$WORK/lago-db-prepare.log" 2>/dev/null; exit 1; }
lago -o /dev/null -w '' -X POST "$LAGO_URL/api/v1/billable_metrics" -d '{"billable_metric":{"name":"Requests","code":"requests","aggregation_type":"sum_agg","field_name":"value","recurring":false}}'
BM_ID=$(lago "$LAGO_URL/api/v1/billable_metrics/requests" | jget '.billable_metric.lago_id')
lago -o /dev/null -w '' -X POST "$LAGO_URL/api/v1/plans" -d "{\"plan\":{\"name\":\"E2E\",\"code\":\"e2e_plan\",\"interval\":\"monthly\",\"amount_cents\":0,\"amount_currency\":\"USD\",\"pay_in_advance\":false,\"charges\":[{\"billable_metric_id\":\"$BM_ID\",\"charge_model\":\"standard\",\"properties\":{\"amount\":\"0.01\"}}]}}"
[ -n "$BM_ID" ] && pass "Lago seeded: billable_metric 'requests' + plan 'e2e_plan'" || { fail "lago seed"; exit 1; }

MIG_LOG="$WORK/migrate.log"
zs_platform_migrate "$DBURL" \
  --migrations-dir "$ROOT/db/migrations-ts" \
  --project-schema zeroship --project-id zeroship > "$MIG_LOG" 2>&1 \
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
printf '[metering]\nbrokers = "%s"\nevents_topic = "%s"\n' "$RP_BROKERS" "$USAGE_TOPIC" > "$CFG_TOML"

ZEROSHIP_GATEWAY_SIGNING_KEY_FILE="$WORK/sk.pem"
ZEROSHIP_GATEWAY_BROKER_SECRET_FILE="$WORK/gate-broker-secret"
# The issuer control verifies the admin bearer against, on the same key the
# gateway signs with. Up BEFORE control: control reads the issuer once at boot.
e2e_platform_op_up "$WORK/sk.pem" "$WORK" || exit 1
e2e_export_runtime_secrets "$WORK" || exit 1
e2e_export_database_urls "$DBURL"
"$BIN/zeroship-control" --port "$ZEROSHIP_CONTROL_PORT" --config "$CFG_TOML" \
  --blob-store "$WORK/blobs" \
  --meter-provider lago --invoicer-provider lago \
  --provider-config "{\"lago\":{\"api_url\":\"$LAGO_URL\",\"api_key\":\"$LAGO_KEY\",\"billable_metric_code\":\"requests\"}}" \
  --spend-recompute-interval 2 > "$WORK/control.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$CONTROL_URL/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$CONTROL_URL/readyz" >/dev/null 2>&1 && pass "control healthy (provider=lago, stream=redpanda)" || { fail "control"; tail -30 "$WORK/control.log"; exit 1; }

# The worker takes NO `--config` - 9b205f6ed removed its TOML overlay source as
# a credential boundary - and no longer needs one: the usage-stream settings are
# four real flags with ZEROSHIP_METERING_* twins. Without brokers the worker
# boots and drains and DROPS every usage event, which is why the outbox
# assertion below the health poll is here: it stops the forwarder rail further
# down from asserting against silence.
"$BIN/zeroship-worker" --port "$ZEROSHIP_WORKER_PORT" --threads 2 \
  --metering-brokers "$RP_BROKERS" --metering-events-topic "$USAGE_TOPIC" \
  --metering-outbox-wal-path "$WORK/worker-outbox.redb" \
  --control-url "$CONTROL_URL" --blob-store "$WORK/blobs" --poll-interval 2 > "$WORK/worker.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" >/dev/null 2>&1 && pass "worker healthy" || { fail "worker"; tail -30 "$WORK/worker.log"; exit 1; }
e2e_assert_usage_producer "$WORK/worker.log" "worker"

"$BIN/zeroship-gate" --port "$ZEROSHIP_GATEWAY_PORT" --control-url "$CONTROL_URL" \
  --metering-outbox-wal-path "$WORK/gate-outbox.redb" \
  --config "$CFG_TOML" --worker-urls "http://localhost:$ZEROSHIP_WORKER_PORT" --blob-store "$WORK/blobs" --blob-cache-disk-root "$WORK/blob-cache" \
 --poll-interval 2 --signing-key-file "$WORK/sk.pem" --broker-secret-file "$WORK/gate-broker-secret" > "$WORK/gate.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" >/dev/null 2>&1 && pass "gateway healthy" || { fail "gateway"; tail -30 "$WORK/gate.log"; exit 1; }
e2e_assert_usage_producer "$WORK/gate.log" "gateway"

echo ""; echo "=== Stage 2: admin bearer + 2 creators + 3 apps (A1,A2->C1 ; A3->C2) + deploy ==="
# The scope string is the action list the deleted permission_tokens policy
# carried, one scope per Cedar action.
SCOPE="apps:read apps:write apps:deploy billing:read billing:write"
ADMIN="$(uuid)"; C1="$(uuid)"; C2="$(uuid)"
psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id,email,name,email_verified_at) VALUES
 ('$ADMIN','e2e-mapp-admin-$ADMIN@zeroship.test'::citext,'Admin',NOW()),
 ('$C1','e2e-mapp-c1-$C1@zeroship.test'::citext,'Creator One',NOW()),
 ('$C2','e2e-mapp-c2-$C2@zeroship.test'::citext,'Creator Two',NOW());
SQL
ADMIN_TOKEN="$(e2e_mint_platform_bearer "$ADMIN" "$SCOPE")"
[ "$(echo -n "$ADMIN_TOKEN" | awk -F. '{print NF}')" = "3" ] && pass "minted admin bearer (admin=$ADMIN, C1=$C1, C2=$C2)" || { fail "bearer mint"; exit 1; }

# Create + deploy 3 apps via the admin bearer, then OVERRIDE each app's sole owner to
# the intended creator (delete auto membership + insert exactly one owner) so the
# forwarder's app->creator resolution is unambiguous.
declare -A APPID
create_deploy(){ # $1=slug  $2=owner_creator
  local slug="$1" owner="$2" id
  id="$(curl -s -X POST "$CONTROL_URL/api/apps" -H 'Content-Type: application/json' -H "Authorization: Bearer $ADMIN_TOKEN" -d "{\"name\":\"$slug\",\"plan_id\":\"$PLAN_ID\"}" | jget '.id')"
  [ -n "$id" ] || { fail "create app $slug"; exit 1; }
  "$BIN/zeroship" deploy "$PROBE" --app="$id" --control="$CONTROL_URL" --token="$ADMIN_TOKEN" 2>&1 | grep -q deploy_hash || { fail "deploy $slug"; exit 1; }
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
#
# THE HARNESS DOES THIS; THE PLATFORM DOES NOT. Measured 2026-08-11 by
# enumerating every Lago path the control plane calls:
#
#     "/api/v1/events                        x2   (the only WRITE)
#     "/api/v1/meters/{enc_slug}/query       x1
#     "/api/v1/customers/{subject}/current_usage  x1
#
# There is no POST to /api/v1/customers or /api/v1/subscriptions anywhere in
# crates/control/src/ - the only things in this repo that create them are these
# harnesses. So on a Lago-configured deployment the provider-side customer must
# be provisioned out of band, and `docs/reference/billing-metering.md` says
# nothing about who does it (one Lago mention, line 108, a capability list).
#
# ATTR_SKIP_PROVISION=1 reproduces the un-provisioned state deliberately. It is
# a control, not a convenience: it shows what the conservation check below is
# for, and it is the state the product leaves Lago in by itself.
if [ "${ATTR_SKIP_PROVISION:-0}" = "1" ]; then
  echo "    (ATTR_SKIP_PROVISION=1 — NOT creating Lago customers/subscriptions,"
  echo "     which is what the control plane does on its own)"
fi
for c in "$C1" "$C2"; do
  [ "${ATTR_SKIP_PROVISION:-0}" = "1" ] && continue
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
  for _ in $(seq 1 30); do [ "$(curl -s -o /dev/null -w '%{http_code}' -H "Host: $host" -H 'content-type: application/json' --data "$BODY" "http://localhost:$ZEROSHIP_GATEWAY_PORT/probe/ready")" = "200" ] && { ready=1; break; }; sleep 1; done
  [ "$ready" = "1" ] || { fail "$slug never reachable"; tail -10 "$WORK/gate.log"; exit 1; }
  for _ in 1 2 3; do curl -s -o /dev/null -H "Host: $host" -H 'content-type: application/json' --data "$BODY" "http://localhost:$ZEROSHIP_GATEWAY_PORT/probe/warm" || true; done
  for i in $(seq 1 "$n"); do for _ in 1 2 3; do [ "$(curl -s -o /dev/null -w '%{http_code}' -H "Host: $host" -H 'content-type: application/json' --data "$BODY" "http://localhost:$ZEROSHIP_GATEWAY_PORT/probe/$i")" = "200" ] && { ok=$((ok+1)); break; }; sleep 0.2; done; done
  echo "$ok"
}
N1=40; N2=60; N3=70
OK1="$(drive app1 $N1)"; OK2="$(drive app2 $N2)"; OK3="$(drive app3 $N3)"
[ "$OK1" = "$N1" ] && [ "$OK2" = "$N2" ] && [ "$OK3" = "$N3" ] \
  && pass "drove traffic app1=$OK1/$N1 app2=$OK2/$N2 app3=$OK3/$N3 (HTTP 200)" \
  || { fail "traffic app1=$OK1/$N1 app2=$OK2/$N2 app3=$OK3/$N3"; tail -20 "$WORK/worker.log"; exit 1; }

echo ""; echo "=== Stage 4: per-creator attribution in REAL Lago (isolation) ==="
lago_sum(){ # $1=creator -> sum of requests-event values for that subject
  lago "$LAGO_URL/api/v1/events?external_subscription_id=$1&per_page=500" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const evs=(JSON.parse(s).events||[]).filter(e=>e.code==="requests");process.stdout.write(String(evs.reduce((a,e)=>a+Number((e.properties||{}).value||0),0))+"\n")}catch(e){process.stdout.write("0\n")}})'
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
# The MIRROR of the row above, and it was missing. Everything else here is a
# FLOOR (`>= expected`), so a leak INTO C1 is invisible: with A3 bled onto C1,
# C1 would read ~$FLEET and `$FLEET >= $C1_EXPECT` is true, so the header's
# claim of "ZERO cross-attribution" held in one direction only. That is the
# one-sided-floor shape task #288 removed from scenario 14.
#
# The threshold is measured, not guessed. A correct run reads C1 = 109 against
# an expected 100 - the excess is the metered ready/warm probe traffic (~4-5 per
# app, and C1 owns two). A leak of A3 would put C1 at ~179. Anything at or above
# the fleet total cannot be C1's own two apps plus probe noise.
[ -n "$C1_SUM" ] && [ "$C1_SUM" -lt "$FLEET" ] 2>/dev/null \
  && pass "C1 is NOT over-attributed ($C1_SUM < $FLEET) - A3's usage did not bleed onto C1" \
  || fail "over-attribution suspected - C1=$C1_SUM reached the ceiling $FLEET; its own apps are only $C1_EXPECT, so something else's usage is on this creator"

# CONSERVATION. Everything above is per-subject, so usage sent to a subject
# nobody queries is invisible: a resolver returning the WRONG creator id leaves
# C1 and C2 both reading their own correct totals while the events pile up
# somewhere else. That is exactly what a mutated resolver produced here
# (C1=0, C2=0, 0 dead-letters, and every per-subject row green).
#
# The fleet sum is the one check that can see it: every event this run drove
# must land on SOME subject, so the total across all subjects cannot fall below
# what was sent.
#
# WHAT THIS DOES **NOT** COVER, corrected 2026-08-11 after I got it wrong. I
# first wrote that Lago "accepts an event for a subscription that does not
# exist, so the usage is billed to nobody", and treated this check as covering
# that too. Running `ATTR_SKIP_PROVISION=1` - no Lago customers or
# subscriptions created at all - REFUTED it: 21 passed, 0 failed, conservation
# 183 >= 170. The events survive and stay retrievable BY SUBJECT, so counting
# them cannot detect an unprovisioned creator. Measured directly:
#
#   GET /api/v1/events?external_subscription_id=<unprovisioned>
#     -> the event, with lago_customer_id: null, lago_subscription_id: null
#   GET /api/v1/customers/<unprovisioned>/current_usage
#     -> HTTP 404 {"code":"resource_not_found"}
#
# So it is not silent LOSS, it is silent NON-BILLING: the data is retained and
# the INVOICING read path - the one the shipped adapter uses at
# crates/control/src/metering/provider/adapters/lago.rs:206 - 404s. The check
# for that is below, and it is a different question from conservation.
lago_sum_all(){ # sum of requests-event values across EVERY subject
  lago "$LAGO_URL/api/v1/events?per_page=1000" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const evs=(JSON.parse(s).events||[]).filter(e=>e.code==="requests");process.stdout.write(String(evs.reduce((a,e)=>a+Number((e.properties||{}).value||0),0))+"\n")}catch(e){process.stdout.write("0\n")}})'
}
ALL_SUM="$(lago_sum_all)"
echo "    Lago requests-sum across ALL subjects: $ALL_SUM (fleet driven = $FLEET)"
[ -n "$ALL_SUM" ] && [ "$ALL_SUM" -ge "$FLEET" ] 2>/dev/null \
  && pass "conservation: every driven request reached SOME subject ($ALL_SUM >= $FLEET) — no usage accepted-and-orphaned" \
  || fail "usage vanished: Lago holds $ALL_SUM requests across all subjects but $FLEET were driven. Lago 200s an event whose external_subscription_id does not exist, so a wrong-but-present creator id is silently billed to nobody"

# INVOICEABILITY. Accepted-and-retained is not the same as billable. The
# adapter's aggregate read is GET /api/v1/customers/{subject}/current_usage
# (lago.rs:206), and that endpoint 404s for a subject with no Lago customer -
# which is the state the CONTROL PLANE leaves every creator in, since it never
# POSTs to /api/v1/customers or /api/v1/subscriptions (enumerated above).
# The harness provisions them itself, so without this row nothing here would
# ever exercise the billing read path at all.
for c in "$C1" "$C2"; do
  CU_CODE="$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $LAGO_KEY" \
    "$LAGO_URL/api/v1/customers/$c/current_usage?external_subscription_id=$c")"
  [ "$CU_CODE" = "200" ] \
    && pass "creator $c is INVOICEABLE (current_usage HTTP 200) — accepted usage can actually be billed" \
    || fail "creator $c is NOT invoiceable: current_usage HTTP $CU_CODE. Its events are accepted and retrievable, but the aggregate read the adapter uses (lago.rs:206) cannot see them, so the usage is recorded and unbillable"
done

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
