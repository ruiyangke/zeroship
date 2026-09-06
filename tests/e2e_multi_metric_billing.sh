#!/usr/bin/env bash
# ============================================================================
# e2e_multi_metric_billing.sh — NEW-COVERAGE E2E for the charge being a SUM over
# weighted metrics, not the requests count wearing a price.
#
# Every other billing e2e drives one priced metric, so a pricing bug that ignored
# the metric weights entirely would still show the right number. This drives a
# padded body so `requests` AND `egress_bytes` both land in usage_aggregates with
# different weights, then asserts the projected charge is the SUM:
#
#   pricing (crates/zeroship-control/src/pricing.rs):
#     CU(metric) = floor(total * units_per_op / per_units)
#     requests     weight 1 CU / 1 op    -> CU = R
#     egress_bytes weight 1 CU / 100 b   -> CU = floor(EGR/100)
#   FX 1e12 pico-cents/unit = 1 cent per CU, so charge = R + floor(EGR/100)
#
# The second metric must be strictly non-zero, and the charge strictly greater
# than the requests-only charge: without both, a summation that dropped every
# metric after the first would still pass. Pricing is provider-independent, so
# this uses the `lite` provider (no external billing account).
#
# UNTIL 2026-08-20 THIS HEADER DESCRIBED e2e_spend_state_transitions.sh, which is
# a DIFFERENT harness that still exists - this file was branched from it and the
# comment block came along. Three of its helpers came along too and are deleted.
#
# REFUSES (exit 1) when docker is unavailable - a run that asserted nothing
# is not a passing run. KEEP_WORK=1 preserves logs.
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; BIN="$ROOT/target/release"
# shellcheck source=tests/lib/runtime_secrets.sh
source "$ROOT/tests/lib/runtime_secrets.sh"
# `zeroship.app_members` is deleted; an app reaches the people who answer for it
# through its project's organization. `seat_app_owner_sql` emits that join AND a
# check that raises when it matches nothing - an INSERT ... SELECT over no rows
# is a SUCCESSFUL statement that seats nobody, and the 403 it later produces
# surfaces far from here.
source "$ROOT/tests/lib/organization_fixture.sh"
# shellcheck source=tests/lib/usage_producer.sh
source "$ROOT/tests/lib/usage_producer.sh"
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
for b in zeroship zeroship-control zeroship-gate zeroship-worker zeroship-migrate-server; do [ -x "$BIN/$b" ] || { echo "missing $BIN/$b"; exit 2; }; done
[ -f "$ROOT/packages/zero-migrate-cli/dist/cli-bin.js" ] || { echo "missing the zero-migrate CLI - run: pnpm install && pnpm build"; exit 2; }
command -v node >/dev/null && command -v openssl >/dev/null && command -v curl >/dev/null || { echo "need node/openssl/curl"; exit 2; }
PROBE="$ROOT/examples/metering-probe/dist/app.zship"; [ -f "$PROBE" ] || { echo "missing $PROBE"; exit 2; }
PROBE_IR="$ROOT/examples/metering-probe/generated/zeroship/migrations.ir.json"; [ -s "$PROBE_IR" ] || { echo "missing $PROBE_IR"; exit 2; }
JOSE="$ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"; [ -f "$JOSE" ] || { echo "missing jose"; exit 2; }

ZEROSHIP_CONTROL_PORT=9178; ZEROSHIP_WORKER_PORT=8078; ZEROSHIP_GATEWAY_PORT=8068; ZEROSHIP_MIGRATE_SERVER_PORT=9078; PG_PORT=5478; RP_PORT=19178
PGC=zs-e2e-mm-pg; RPC=zs-e2e-mm-redpanda
DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"; CONTROL_URL="http://localhost:$ZEROSHIP_CONTROL_PORT"
MIGRATE_SERVER_URL="http://localhost:$ZEROSHIP_MIGRATE_SERVER_PORT"
RP_BROKERS="127.0.0.1:$RP_PORT"; USAGE_TOPIC="zeroship-usage-mm-e2e"
WORK="$(mktemp -d -t zs-e2e-mm-XXXXXX)"; mkdir -p "$WORK/blobs" "$WORK/blob-cache"; PIDFILE="$WORK/pids"; : > "$PIDFILE"
jget(){ node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);process.stdout.write(String(o$1??'')+'\n')}catch(e){console.log('')}})"; }
psql_exec(){ docker exec -i "$PGC" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 "$@"; }

cleanup(){
  echo ""; echo "=== Cleanup ==="
  [ -f "$PIDFILE" ] && while read -r pid; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done < "$PIDFILE"
  wait 2>/dev/null || true
  if [ "${KEEP_WORK:-0}" = "1" ]; then echo "  KEEP_WORK=1 → $PGC/$RPC + $WORK preserved";
  else docker rm -f "$PGC" "$RPC" >/dev/null 2>&1 || true; rm -rf "$WORK"; echo "  stack down, $WORK cleaned"; fi
}
trap cleanup EXIT
for p in $ZEROSHIP_CONTROL_PORT $ZEROSHIP_WORKER_PORT $ZEROSHIP_GATEWAY_PORT $ZEROSHIP_MIGRATE_SERVER_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done

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
zs_platform_migrate "$DBURL" \
  --migrations-dir "$ROOT/db/migrations-ts" \
  --project-schema zeroship --project-id zeroship > "$MIG_LOG" 2>&1 \
  && pass "zeroship platform migrations applied" || { fail "migrate"; tail -20 "$MIG_LOG"; exit 1; }

PLAN_ID="pln_mm_e2e"
# FX 1e12 pico-cents/unit = 1 cent per CU; requests weight 1 CU/op → 1 cent/request.
psql_exec >/dev/null 2>&1 <<SQL && pass "seeded plan + pricing (1 cent/request) + weights" || { fail "plan seed"; exit 1; }
INSERT INTO zeroship.plans (id,name,base_fee_cents,included_units,fx_pico_cents_per_unit,runtime_limits_json,spend_limit_default_cents)
VALUES ('$PLAN_ID','mm-e2e',0,0,1000000000000,'{"cpu_limit_ms":5000,"wall_timeout_ms":30000,"heap_limit_mb":256}',100000000) ON CONFLICT (id) DO NOTHING;
INSERT INTO zeroship.pricing_config (id,fx_pico_cents_per_unit) VALUES ('global',1000000000000) ON CONFLICT (id) DO UPDATE SET fx_pico_cents_per_unit=EXCLUDED.fx_pico_cents_per_unit;
INSERT INTO zeroship.billing_metrics (metric,kind,unit) VALUES ('requests','platform','op') ON CONFLICT (metric) DO UPDATE SET kind='platform';
INSERT INTO zeroship.metric_weights (metric,units_per_op,per_units) VALUES ('requests',1,1) ON CONFLICT (metric) DO UPDATE SET units_per_op=1,per_units=1;
INSERT INTO zeroship.billing_metrics (metric,kind,unit) VALUES ('egress_bytes','platform','byte') ON CONFLICT (metric) DO UPDATE SET kind='platform';
INSERT INTO zeroship.metric_weights (metric,units_per_op,per_units) VALUES ('egress_bytes',1,100) ON CONFLICT (metric) DO UPDATE SET units_per_op=1,per_units=100;
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
ZEROSHIP_CONTROL_STRIPE_SECRET_KEY="sk_test_unused" \
"$BIN/zeroship-control" --port "$ZEROSHIP_CONTROL_PORT" --config "$CFG_TOML" \
  --blob-store "$WORK/blobs" \
  --stripe-base-url "http://127.0.0.1:1" \
  --meter-provider lite --invoicer-provider lite --allow-unsupported-billing \
  --spend-recompute-interval 2 > "$WORK/control.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$CONTROL_URL/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$CONTROL_URL/readyz" >/dev/null 2>&1 && pass "control healthy (lite provider, stream=redpanda)" || { fail "control"; tail -30 "$WORK/control.log"; exit 1; }

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

"$BIN/zeroship-migrate-server" --port "$ZEROSHIP_MIGRATE_SERVER_PORT" \
  --tmp-dir "$WORK/migrated-tmp" > "$WORK/migrated.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$MIGRATE_SERVER_URL/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$MIGRATE_SERVER_URL/readyz" >/dev/null 2>&1 \
  && pass "zeroship-migrate-server healthy" \
  || { fail "migrate-server"; tail -30 "$WORK/migrated.log"; exit 1; }

echo ""; echo "=== Stage 2: bearer + creator + app + deploy ==="
# The scope string is the action list the deleted permission_tokens policy
# carried, one scope per Cedar action.
SCOPE="apps:read apps:write apps:deploy billing:read billing:write"
CREATOR="$(node -e 'console.log(require("crypto").randomUUID())')"
psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id,email,name,email_verified_at) VALUES ('$CREATOR','e2e-mm-$CREATOR@zeroship.test'::citext,'E2E MM',NOW());
SQL
ADMIN_TOKEN="$(e2e_mint_platform_bearer "$CREATOR" "$SCOPE")"
[ "$(echo -n "$ADMIN_TOKEN" | awk -F. '{print NF}')" = "3" ] && pass "minted platform bearer (creator=$CREATOR)" || { fail "bearer mint"; exit 1; }
APP="$(curl -s -X POST "$CONTROL_URL/api/apps" -H 'Content-Type: application/json' -H "Authorization: Bearer $ADMIN_TOKEN" -d "{\"name\":\"mm-probe\",\"plan_id\":\"$PLAN_ID\"}" | jget '.id')"
[ -n "$APP" ] && pass "created app $APP" || { fail "create app"; exit 1; }
psql_exec >/dev/null 2>&1 <<SQL
$(seat_app_owner_sql "$APP" "$CREATOR")
SQL
CREATE_CODE="$(curl -sS -o "$WORK/create-database-response.json" -w '%{http_code}' \
  -X POST "$MIGRATE_SERVER_URL/v1/databases/$APP" \
  -H "Authorization: Bearer $ADMIN_TOKEN")"
if [[ "$CREATE_CODE" != 2?? ]]; then
  fail "database create failed (http=$CREATE_CODE): $(cat "$WORK/create-database-response.json")"
  tail -30 "$WORK/migrated.log"; exit 1
fi
APPLY_CODE="$(curl -sS -o "$WORK/apply-response.json" -w '%{http_code}' \
  -X POST "$MIGRATE_SERVER_URL/v1/apps/$APP/migrations/apply" \
  -H 'Content-Type: application/json' -H "Authorization: Bearer $ADMIN_TOKEN" \
  --data-binary @"$PROBE_IR")"
APPLIED="$(jget '.applied.length' < "$WORK/apply-response.json")"
if [ "$APPLY_CODE" = "200" ] && [ -n "$APPLIED" ] && [ "$APPLIED" -ge 1 ] 2>/dev/null; then
  pass "applied probe migrations (applied=$APPLIED)"
else
  fail "migration apply failed (http=$APPLY_CODE): $(cat "$WORK/apply-response.json")"
  tail -30 "$WORK/migrated.log"; exit 1
fi
"$BIN/zeroship" deploy "$PROBE" --app="$APP" --control="$CONTROL_URL" --token="$ADMIN_TOKEN" 2>&1 | grep -q deploy_hash && pass "deployed probe" || { fail "deploy"; exit 1; }
sleep 5

echo ""; echo "=== Stage 3: drive traffic → both 'requests' AND 'egress_bytes' flow to usage_aggregates ==="
N_REQ=100; BODY='{"hello":"multi-metric","pad":"........................................................"}'
READY=0; for _ in $(seq 1 30); do [ "$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: mm-probe.localhost' -H 'content-type: application/json' --data "$BODY" "http://localhost:$ZEROSHIP_GATEWAY_PORT/probe/ready")" = "200" ] && { READY=1; break; }; sleep 1; done
[ "$READY" = "1" ] && pass "app reachable" || { fail "app never reachable"; tail -15 "$WORK/gate.log"; exit 1; }
GW_OK=0; LAST_BODY=""
for i in $(seq 1 $N_REQ); do
  for _ in 1 2 3; do
    RESPONSE="$(curl -s -w '\n%{http_code}' -H 'Host: mm-probe.localhost' -H 'content-type: application/json' --data "$BODY" "http://localhost:$ZEROSHIP_GATEWAY_PORT/probe/$i")"
    CODE="$(printf '%s' "$RESPONSE" | tail -1)"; PROBE_BODY="$(printf '%s' "$RESPONSE" | sed '$d')"
    [ "$CODE" = "200" ] && { GW_OK=$((GW_OK+1)); LAST_BODY="$PROBE_BODY"; break; }
    sleep 0.2
  done
done
WROTE="$(printf '%s' "$LAST_BODY" | jget '.wrote')"
READ_BACK="$(printf '%s' "$LAST_BODY" | jget '.readBack')"
DB_ERROR_NULL="$(printf '%s' "$LAST_BODY" | jget '.dbError === null')"
if [ "$GW_OK" = "$N_REQ" ] && [ "$WROTE" = "true" ] && \
   [ -n "$READ_BACK" ] && [ "$READ_BACK" -ge 1 ] 2>/dev/null && \
   [ "$DB_ERROR_NULL" = "true" ]; then
  pass "drove $GW_OK/$N_REQ requests with probe db write/read (wrote=$WROTE readBack=$READ_BACK dbError=null)"
else
  fail "traffic $GW_OK/$N_REQ; wrote=$WROTE readBack=$READ_BACK dbError-null=$DB_ERROR_NULL; body: ${LAST_BODY:0:300}"
  tail -20 "$WORK/worker.log"; exit 1
fi

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
for _ in $(seq 1 20); do C="$(curl -s "$CONTROL_URL/api/apps/$APP/projected-charge" -H "Authorization: Bearer $ADMIN_TOKEN" | jget '.projected_charge_cents')"; [ -n "$C" ] && [ "$C" -ge "$R" ] 2>/dev/null && break; sleep 2; done
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
