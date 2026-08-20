#!/usr/bin/env bash
# ============================================================================
# e2e_openmeter_export.sh — FAITHFUL multi-node E2E of the forwarder->PROVIDER
# rail against a REAL, locally-running OpenMeter (kafka + clickhouse + sink).
# OpenMeter is a forwarder-fed METER provider (like Lago), so this drives the
# SAME stream path the lite e2e cannot:
#
#   real traffic -> gateway -> worker (Meter) -> redpanda
#     -> control event_forwarder (resolves app->creator) -> REAL OpenMeter
#        POST /api/v1/events (CloudEvents, subject=creator)
#     -> OpenMeter /api/v1/meters/requests/query aggregates it   (provider rail)
#     -> control spend_recompute -> usage_aggregates -> 402       (enforcement)
#
# OpenMeter is METER-only (never invoices), so the stack pairs it with `lite` as
# the (unused-here) invoicer. The `requests` meter is in deploy/ops/openmeter-config.yaml.
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
echo "  zeroship E2E — forwarder -> REAL OpenMeter provider rail"
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

ZEROSHIP_CONTROL_PORT=9173; ZEROSHIP_WORKER_PORT=8073; ZEROSHIP_GATEWAY_PORT=8063; PG_PORT=5473; RP_PORT=19173
OM_URL="http://127.0.0.1:48888"
PGC=zs-e2e-om-pg; RPC=zs-e2e-om-redpanda
DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"; CONTROL_URL="http://localhost:$ZEROSHIP_CONTROL_PORT"
RP_BROKERS="127.0.0.1:$RP_PORT"; USAGE_TOPIC="zeroship-usage-om-e2e"
WORK="$(mktemp -d -t zs-e2e-om-XXXXXX)"; mkdir -p "$WORK/blobs" "$WORK/blob-cache"; PIDFILE="$WORK/pids"; : > "$PIDFILE"
jget(){ node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);process.stdout.write(String(o$1??'')+'\n')}catch(e){console.log('')}})"; }
psql_exec(){ docker exec -i "$PGC" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 "$@"; }

cleanup(){
  echo ""; echo "=== Cleanup ==="
  [ -f "$PIDFILE" ] && while read -r pid; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done < "$PIDFILE"
  wait 2>/dev/null || true
  if [ "${KEEP_WORK:-0}" = "1" ]; then
    echo "  KEEP_WORK=1 → PG/$PGC redpanda/$RPC OpenMeter(compose) + $WORK preserved"
  else
    docker rm -f "$PGC" "$RPC" >/dev/null 2>&1 || true
    docker compose -f "$ROOT/deploy/compose/openmeter.yml" down -v >/dev/null 2>&1 || true
    rm -rf "$WORK"; echo "  stack down, OpenMeter down, $WORK cleaned"
  fi
}
trap cleanup EXIT
for p in $ZEROSHIP_CONTROL_PORT $ZEROSHIP_WORKER_PORT $ZEROSHIP_GATEWAY_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done

echo ""; echo "=== Stage 1: infra (PG + redpanda + REAL OpenMeter) + migrate + seed + stack ==="
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

# Real OpenMeter (kafka + clickhouse + sink-worker). Slow to become ready.
docker compose -f "$ROOT/deploy/compose/openmeter.yml" up -d >/dev/null 2>&1 || { fail "openmeter compose up"; exit 1; }
OM_READY=0
for _ in $(seq 1 60); do
  if curl -sf "$OM_URL/api/v1/meters" 2>/dev/null | grep -q '"slug":"requests"'; then OM_READY=1; break; fi
  sleep 3
done
[ "$OM_READY" = "1" ] && pass "OpenMeter healthy on $OM_URL (requests meter present)" || { fail "openmeter never ready"; docker compose -f "$ROOT/deploy/compose/openmeter.yml" logs openmeter 2>&1 | tail -20; exit 1; }

MIG_LOG="$WORK/migrate.log"
"$BIN/zeroship-platform-migrate" \
  --database-url "$DBURL" --migrations-dir "$ROOT/db/migrations-ts" \
  --project-schema zeroship --project-id zeroship > "$MIG_LOG" 2>&1 \
  && pass "zeroship platform migrations applied" || { fail "migrate"; tail -20 "$MIG_LOG"; exit 1; }

PLAN_ID="pln_om_e2e"
psql_exec >/dev/null 2>&1 <<SQL && pass "seeded plan + pricing_config + metric_weights (requests=1 CU/op × 1c/CU)" || { fail "plan seed"; exit 1; }
INSERT INTO zeroship.plans (id,name,base_fee_cents,included_units,fx_pico_cents_per_unit,runtime_limits_json,spend_limit_default_cents)
VALUES ('$PLAN_ID','om-e2e',0,0,1000000000000,'{"cpu_limit_ms":5000,"wall_timeout_ms":30000,"heap_limit_mb":256}',1000000) ON CONFLICT (id) DO NOTHING;
INSERT INTO zeroship.pricing_config (id,fx_pico_cents_per_unit) VALUES ('global',1000000000000) ON CONFLICT (id) DO UPDATE SET fx_pico_cents_per_unit=EXCLUDED.fx_pico_cents_per_unit;
INSERT INTO zeroship.billing_metrics (metric,kind,unit) VALUES ('requests','platform','op') ON CONFLICT (metric) DO UPDATE SET kind='platform';
INSERT INTO zeroship.metric_weights (metric,units_per_op,per_units) VALUES ('requests',1,1) ON CONFLICT (metric) DO UPDATE SET units_per_op=1,per_units=1;
SQL

openssl genpkey -algorithm ed25519 -out "$WORK/sk.pem" 2>/dev/null; chmod 600 "$WORK/sk.pem"
openssl rand -base64 48 > "$WORK/gate-broker-secret"; chmod 600 "$WORK/gate-broker-secret"
CFG_TOML="$WORK/zeroship.toml"
printf '[metering]\nredpanda_brokers = "%s"\nusage_events_topic = "%s"\n' "$RP_BROKERS" "$USAGE_TOPIC" > "$CFG_TOML"

# control: OpenMeter meter (forwarder-fed) + lite invoicer (unused here, needs
# --allow-unsupported-billing + a stripe base for lite's store) + the stream.
ZEROSHIP_GATEWAY_SIGNING_KEY_FILE="$WORK/sk.pem"
ZEROSHIP_GATEWAY_BROKER_SECRET_FILE="$WORK/gate-broker-secret"
# The issuer control verifies the admin bearer against, on the same key the
# gateway signs with. Up BEFORE control: control reads the issuer once at boot.
e2e_platform_op_up "$WORK/sk.pem" "$WORK" || exit 1
e2e_export_runtime_secrets "$WORK" || exit 1
e2e_export_database_urls "$DBURL"
# The token is a shell variable, NOT an exported one: the provider config
# carries the material itself. It used to say `env:OPENMETER_TOKEN`, an arm the
# resolver stopped having in 94c7ba7dd, so control refused to start and every
# assertion below it was unreachable. The local OpenMeter this drives has no
# account to have issued a key, so the token is a literal by nature.
OM_TOKEN="$(openssl rand -hex 32)"
ZEROSHIP_CONTROL_STRIPE_SECRET_KEY="sk_test_unused" \
"$BIN/zeroship-control" --port "$ZEROSHIP_CONTROL_PORT" --config "$CFG_TOML" \
  --blob-store "$WORK/blobs" \
  --stripe-base-url "http://127.0.0.1:1" \
  --meter-provider openmeter --invoicer-provider lite --allow-unsupported-billing \
  --provider-config "{\"openmeter\":{\"base_url\":\"$OM_URL\",\"token\":\"$OM_TOKEN\"}}" \
  --spend-recompute-interval 2 > "$WORK/control.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$CONTROL_URL/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$CONTROL_URL/readyz" >/dev/null 2>&1 && pass "control healthy (meter=openmeter, invoicer=lite, stream=redpanda)" || { fail "control"; tail -30 "$WORK/control.log"; exit 1; }

# The worker takes NO `--config`. 9b205f6ed (2026-08-16) removed its TOML
# overlay source on purpose - "the worker deliberately has no TOML overlay
# source", a credential boundary - so `--config` here is an unknown argument and
# clap exited before the worker did anything, making every assertion below it
# unreachable. The stream producer settings that used to arrive in the file's
# [metering] table now have exactly one channel left, the four
# UsageStreamSettings::from_env names; USAGE_OUTBOX_WAL_PATH was already one of
# them. Without the brokers the worker still boots but drains and DROPS every
# usage event, so the forwarder rail below would assert against silence.
REDPANDA_BROKERS="$RP_BROKERS" USAGE_EVENTS_TOPIC="$USAGE_TOPIC" \
USAGE_OUTBOX_WAL_PATH="$WORK/worker-outbox.redb" "$BIN/zeroship-worker" --port "$ZEROSHIP_WORKER_PORT" --threads 2 \
  --control-url "$CONTROL_URL" --blob-store "$WORK/blobs" --poll-interval 2 > "$WORK/worker.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" >/dev/null 2>&1 && pass "worker healthy (outbox → redpanda)" || { fail "worker"; tail -30 "$WORK/worker.log"; exit 1; }

USAGE_OUTBOX_WAL_PATH="$WORK/gate-outbox.redb" "$BIN/zeroship-gate" --port "$ZEROSHIP_GATEWAY_PORT" --control-url "$CONTROL_URL" \
  --config "$CFG_TOML" --worker-urls "http://localhost:$ZEROSHIP_WORKER_PORT" --blob-store "$WORK/blobs" --blob-cache-disk-root "$WORK/blob-cache" \
 --poll-interval 2 --signing-key-file "$WORK/sk.pem" --broker-secret-file "$WORK/gate-broker-secret" > "$WORK/gate.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" >/dev/null 2>&1 && pass "gateway healthy" || { fail "gateway"; tail -30 "$WORK/gate.log"; exit 1; }

echo ""; echo "=== Stage 2: bearer + creator + app + deploy ==="
# The scope string is the action list the deleted permission_tokens policy
# carried, one scope per Cedar action.
SCOPE="apps:read apps:write apps:deploy billing:read billing:write"
CREATOR="$(node -e 'console.log(require("crypto").randomUUID())')"
psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id,email,name,email_verified_at) VALUES ('$CREATOR','e2e-om-$CREATOR@zeroship.test'::citext,'E2E OM',NOW());
SQL
ADMIN_TOKEN="$(e2e_mint_platform_bearer "$CREATOR" "$SCOPE")"
[ "$(echo -n "$ADMIN_TOKEN" | awk -F. '{print NF}')" = "3" ] && pass "minted platform bearer (creator=$CREATOR)" || { fail "bearer mint"; exit 1; }
APP="$(curl -s -X POST "$CONTROL_URL/api/apps" -H 'Content-Type: application/json' -H "Authorization: Bearer $ADMIN_TOKEN" -d "{\"name\":\"om-probe\",\"plan_id\":\"$PLAN_ID\"}" | jget '.id')"
[ -n "$APP" ] && pass "created app $APP" || { fail "create app"; exit 1; }
psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.app_members (app_id,user_id,role) VALUES ('$APP','$CREATOR','owner') ON CONFLICT (app_id,user_id) DO UPDATE SET role='owner';
SQL
"$BIN/zeroship" deploy "$PROBE" --app="$APP" --control="$CONTROL_URL" --token="$ADMIN_TOKEN" 2>&1 | grep -q deploy_hash && pass "deployed probe" || { fail "deploy"; exit 1; }
sleep 5

echo ""; echo "=== Stage 3: gateway traffic ==="
N_REQ=100; BODY='{"hello":"om","n":1}'
READY=0; for _ in $(seq 1 30); do [ "$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: om-probe.localhost' -H 'content-type: application/json' --data "$BODY" "http://localhost:$ZEROSHIP_GATEWAY_PORT/probe/ready")" = "200" ] && { READY=1; break; }; sleep 1; done
[ "$READY" = "1" ] && pass "app reachable via gateway" || { fail "app never reachable"; tail -15 "$WORK/gate.log"; exit 1; }
for _ in 1 2 3 4 5 6; do curl -s -o /dev/null -H 'Host: om-probe.localhost' -H 'content-type: application/json' --data "$BODY" "http://localhost:$ZEROSHIP_GATEWAY_PORT/probe/warm" || true; done
GW_OK=0; for i in $(seq 1 $N_REQ); do for _ in 1 2 3; do [ "$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: om-probe.localhost' -H 'content-type: application/json' --data "$BODY" "http://localhost:$ZEROSHIP_GATEWAY_PORT/probe/$i")" = "200" ] && { GW_OK=$((GW_OK+1)); break; }; sleep 0.2; done; done
[ "$GW_OK" = "$N_REQ" ] && pass "drove $GW_OK/$N_REQ requests (HTTP 200)" || { fail "traffic $GW_OK/$N_REQ"; tail -20 "$WORK/worker.log"; exit 1; }

echo ""; echo "=== Stage 4: forwarder -> REAL OpenMeter (meter query) + enforcement ==="
FROM="$(date -u -d '1 hour ago' +%Y-%m-%dT%H:%M:%SZ)"; TO="$(date -u -d '1 hour' +%Y-%m-%dT%H:%M:%SZ)"
OM_UNITS=0
for _ in $(seq 1 30); do
  OM_UNITS=$(curl -s "$OM_URL/api/v1/meters/requests/query?subject=$CREATOR&from=$FROM&to=$TO" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const j=JSON.parse(s);const rows=j.data||j.rows||[];process.stdout.write(String(Math.round(rows.reduce((a,r)=>a+Number(r.value||0),0)))+"\n")}catch(e){process.stdout.write("0\n")}})')
  [ -n "$OM_UNITS" ] && [ "$OM_UNITS" -ge "$N_REQ" ] 2>/dev/null && break
  sleep 2
done
echo "    OpenMeter meter[requests] SUM for subject $CREATOR = $OM_UNITS"
[ -n "$OM_UNITS" ] && [ "$OM_UNITS" -ge "$N_REQ" ] 2>/dev/null \
  && pass "REAL OpenMeter aggregated the forwarded usage ($OM_UNITS ≥ $N_REQ requests) for the resolved creator — forwarder→OpenMeter + app→creator work" \
  || fail "OpenMeter did not aggregate the forwarded usage (got '$OM_UNITS', want ≥ $N_REQ)"

DL=$(psql_exec -tA -c "SELECT COUNT(*) FROM zeroship.provider_dead_letter" 2>/dev/null | tr -d '[:space:]')
[ "$DL" = "0" ] && pass "0 provider dead-letters (every event attributed to a real creator)" || fail "provider_dead_letter has $DL rows"

REQS=""; for _ in $(seq 1 20); do REQS="$(curl -s "$CONTROL_URL/api/apps/$APP/usage" -H "Authorization: Bearer $ADMIN_TOKEN" | jget '.requests')"; [ -n "$REQS" ] && [ "$REQS" != "0" ] && break; sleep 2; done
[ -n "$REQS" ] && [ "$REQS" -ge "$N_REQ" ] 2>/dev/null && pass "enforcement recompute aggregated requests ($REQS ≥ $N_REQ)" || fail "usage_aggregates not populated (got '$REQS')"

echo ""; echo "=== Stage 5: spend enforcement — low cap → 402 Block ==="
curl -s -o /dev/null -X PUT "$CONTROL_URL/api/apps/$APP/spend-limit" -H 'Content-Type: application/json' -H "Authorization: Bearer $ADMIN_TOKEN" -d '{"cents":1}'
curl -s -o /dev/null -X POST -H "Authorization: Bearer $ZEROSHIP_CONTROL_KEY" "$CONTROL_URL/internal/spend/reconcile"
STATE="$(psql_exec -tA -c "SELECT state FROM zeroship.app_spend_state WHERE app_id='$APP'" 2>/dev/null | tr -d '[:space:]')"
[ "$STATE" = "block" ] && pass "control derived spend state = block" || fail "expected block, got '$STATE'"
GW402=0; for _ in $(seq 1 15); do [ "$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: om-probe.localhost' "http://localhost:$ZEROSHIP_GATEWAY_PORT/probe/blocked")" = "402" ] && { GW402=1; break; }; sleep 1; done
[ "$GW402" = "1" ] && pass "gateway returns 402 for the over-limit app" || fail "gateway never returned 402"

echo ""; echo "============================================"
echo "  Results: $PASS passed, $FAIL failed"
echo "============================================"
[ $FAIL -eq 0 ] && exit 0 || exit 1
