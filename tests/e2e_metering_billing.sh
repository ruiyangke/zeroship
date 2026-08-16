#!/usr/bin/env bash
# ============================================================================
# e2e_metering_billing.sh — full multi-node E2E for the billing & metering
# pipeline (ISS-31, Stream-1), STREAM-TO-PROVIDER architecture. Proves the
# CROSS-SERVICE path the per-crate integration tests cover only in isolation:
#
#   real traffic ─► gateway ─► worker (platform counters + env.db metrics)
#                                 │ Meter.drain ─► UsageOutbox.publish (~10s)
#                                 ▼
#                            REAL REDPANDA (durable usage-event stream)
#                                 │  (two independent consumer groups)
#         ┌───────────────────────┴────────────────────────┐
#         ▼ spend-recompute-witness                          ▼ (billing rail)
#   spend_recompute: stream ─► usage_aggregates              provider forwarder
#     ─► spend_reconcile (price+state) ─► app_spend_state       (lite is fed by
#     ─► gateway route-pull ─► 402 Block                         the recompute
#                                 │                              snapshot, not
#                                 ▼                              the forwarder —
#   billing_reconcile (closed month) ─► lite close_period ─►      so no forwarder
#     mock-Stripe invoice items + invoice                          is spawned)
#
# PROVIDER: `lite` (the evaluation-grade, locally-runnable billing provider —
# no external metering service). Its billing still bills the creator's infra
# usage through control's REAL cyper StripeClient against the mock-Stripe.
#
# FAITHFUL by construction — NO stubbing of the components under test:
#   * Real zeroship-control / zeroship-worker / zeroship-gate binaries.
#   * Real ephemeral Postgres + a real Redpanda broker (docker) + the full
#     platform migration set.
#   * A real deployed app (examples/metering-probe .zship) hit through the
#     gateway with real HTTP traffic; usage flows worker ─► redpanda ─►
#     control recompute (NOT a POST — the old /internal/usage path is gone).
#   * A real (local) Stripe endpoint: the STANDALONE `zeroship-mock-stripe`
#     server, hit over the wire by control's REAL cyper-based StripeClient.
#
# On-demand reconcile: POST /internal/billing/reconcile?period=<unix> and POST
# /internal/spend/reconcile force one sweep each (same /internal/* gate) so the
# billing + spend stages are deterministic instead of waiting on the cron. Usage
# aggregation is driven by a SHORT --spend-recompute-interval so the recompute
# consumes the stream into usage_aggregates within a couple seconds.
#
# DEDICATED port band + DB/dirs + redpanda container. Cleans up on exit.
# REFUSES (exit 1) when docker is unavailable - a run that asserted nothing
# is not a passing run.
#
# Usage:
#   ./tests/e2e_metering_billing.sh
#   STRICT=1 ./tests/e2e_metering_billing.sh    # known-fails hard-fail
#
# Requires (when docker IS available): docker, node (+ workspace jose),
#   openssl, curl; a release build of
#   target/release/{zeroship,zeroship-control,zeroship-gate,zeroship-worker,
#   zeroship-mock-stripe}; and a built metering-probe example
#   (cd examples/metering-probe && pnpm install && pnpm build).
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"
# shellcheck source=tests/lib/runtime_secrets.sh
source "$ROOT/tests/lib/runtime_secrets.sh"
STRICT="${STRICT:-0}"

PASS=0; FAIL=0; KNOWN=0
pass()  { PASS=$((PASS+1));  echo "  ✓ $1"; }
fail()  { FAIL=$((FAIL+1));  echo "  ✗ $1"; }
known() { KNOWN=$((KNOWN+1)); echo "  ⚠ KNOWN-FAIL: $1"; if [ "$STRICT" = "1" ]; then FAIL=$((FAIL+1)); fi; }

echo "============================================"
echo "  zeroship E2E — billing & metering pipeline (multi-node, real Stripe-mock)"
echo "============================================"

# --- docker gate: skip cleanly when unavailable ----------------------------
# Docker unavailable is a REFUSAL, not a skip. This block used to `exit 0`
# after a warning, so on a machine (or a CI runner) without docker the
# harness reported success having asserted nothing. RED-PROVEN before the
# change: a stub `docker` returning 1 on PATH gave rc 0 here.
if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  echo "  x REFUSED: docker unavailable - billing/metering E2E needs an ephemeral Postgres." >&2
  echo "    NOTHING ran. Exiting non-zero: a run that asserted nothing is not" >&2
  echo "    a passing run. Start docker and re-run." >&2
  exit 1
fi

# --- preflight: binaries + tooling + built example -------------------------
for b in zeroship zeroship-control zeroship-gate zeroship-worker zeroship-mock-stripe zeroship-platform-migrate zeroship-migrated; do
  [ -x "$BIN/$b" ] || { echo "missing $BIN/$b — run cargo build --release, then cargo build --release -p zeroship-migrate-adapter --features platform-cli --bin zeroship-platform-migrate"; exit 2; }
done
command -v node    >/dev/null 2>&1 || { echo "node required"; exit 2; }
command -v openssl >/dev/null 2>&1 || { echo "openssl required"; exit 2; }
PROBE_ZSHIP="$ROOT/examples/metering-probe/dist/app.zship"
[ -f "$PROBE_ZSHIP" ] || { echo "missing $PROBE_ZSHIP — (cd examples/metering-probe && pnpm install && pnpm build)"; exit 2; }
JOSE_JS="$ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"
[ -f "$JOSE_JS" ] || { echo "missing jose at $JOSE_JS"; exit 2; }

# --- DEDICATED ports + containers (distinct from every other harness) ------
ZEROSHIP_CONTROL_PORT=9171
ZEROSHIP_WORKER_PORT=8071
ZEROSHIP_GATEWAY_PORT=8061
PG_PORT=5471
MOCK_PORT=9571
# zeroship-migrated applies the probe's committed migrations to its per-app
# schema. Without it env.db has no table and every insert fails -- which is
# exactly the state this harness shipped in until 2026-08-11, undetected
# because both env.db guards were unfailable (eda51b973).
ZEROSHIP_MIGRATED_PORT=9071
REDPANDA_PORT=19171
PG_CONTAINER="zs-e2e-billing-pg"
RP_CONTAINER="zs-e2e-billing-redpanda"
ZEROSHIP_WORKER_THREADS=2
DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"
CONTROL_URL="http://localhost:$ZEROSHIP_CONTROL_PORT"
MIGRATED_URL="http://localhost:$ZEROSHIP_MIGRATED_PORT"
MOCK_URL="http://127.0.0.1:$MOCK_PORT"
# The worker producer and control's forwarder/recompute consumers share ONE topic.
RP_BROKERS="127.0.0.1:$REDPANDA_PORT"
USAGE_TOPIC="zeroship-usage-e2e"

WORK="$(mktemp -d -t zs-e2e-billing-XXXXXX)"
mkdir -p "$WORK/blobs" "$WORK/blob-cache"
PIDFILE="$WORK/pids"; : > "$PIDFILE"

# Custom metering-test plan: 1 cent / request, NO included quota, generous
# runtime caps, and a high default spend limit (so a LOW per-app override is
# allowed — set_spend_limit enforces override <= plan default). Seeded by SQL
# below so both the spend stage (low override) and the billing stage (priced
# closed-period usage) have a deterministic price model.
PLAN_ID="pln_metering_test_e2e"

# A node JSON field reader: `... | jget '.id'`.
jget() { node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);process.stdout.write(String(o$1??'')+'\n')}catch(e){console.log('')}})"; }

psql_exec() { docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 "$@"; }

# Record the probe's committed migrations as IR for zeroship-migrated. Same
# helper as tests/e2e_db_app_end_to_end.sh; the recorder lives in the built
# vite-plugin, so this reads the SAME migration files the .zship was built from.
write_apply_request() {
  node --input-type=module - "$ROOT/sdks/vite-plugin/dist/gen-types/recorder.js" \
    "$ROOT/examples/metering-probe/migrations" > "$WORK/apply-migrations.json" <<'NODE'
import { pathToFileURL } from "node:url";
const [recorderPath, dir] = process.argv.slice(2);
const { discoverMigrations, recordMigration } = await import(pathToFileURL(recorderPath).href);
const migrations = await discoverMigrations(dir);
const documents = [];
for (const migration of migrations) {
  documents.push({
    filename: migration.stem + ".ir.json",
    body: await recordMigration(migration.path),
  });
}
console.log(JSON.stringify({ kind: "ir", documents }));
NODE
}

cleanup() {
  echo ""
  echo "=== Cleanup ==="
  if [ -f "$PIDFILE" ]; then
    while read -r pid; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done < "$PIDFILE"
  fi
  wait 2>/dev/null || true
  if [ "${KEEP_WORK:-0}" = "1" ]; then
    echo "  stack down (procs killed); KEEP_WORK=1 → PG $PG_CONTAINER + redpanda $RP_CONTAINER + logs in $WORK PRESERVED"
  else
    docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
    docker rm -f "$RP_CONTAINER" >/dev/null 2>&1 || true
    [ -n "${WORK:-}" ] && rm -rf "$WORK"
    echo "  stack down, ephemeral PG + redpanda removed, $WORK cleaned"
  fi
}
trap cleanup EXIT

# Free the ports (a prior aborted run may have left a listener).
for p in $ZEROSHIP_CONTROL_PORT $ZEROSHIP_WORKER_PORT $ZEROSHIP_GATEWAY_PORT $MOCK_PORT; do
  lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
done

# ===========================================================================
echo ""
echo "=== Stage 1: ephemeral Postgres + platform migrations + plan seed + mock-Stripe + stack ==="
# ===========================================================================

docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
docker run --name "$PG_CONTAINER" -d -p "$PG_PORT:5432" \
  -e POSTGRES_PASSWORD=zeroship -e POSTGRES_USER=postgres -e POSTGRES_DB=zeroship \
  postgres:16 -c max_connections=300 >/dev/null || { fail "docker run postgres failed"; exit 1; }
# Readiness = three CONSECUTIVE successful queries, not one pg_isready. This
# file had diverged from tests/lib/e2e_stack.sh `stack_pg_up`, which already
# polls exactly this way and records the measurement: there is a window in
# which pg_isready reports ready and `psql -c 'select 1'` still fails, because
# the entrypoint tears down its initdb-phase server and restarts it. Third time
# a harness has drifted from that library (see #274).
#
# MEASURED 2026-08-11, and this is why it changed: under 12 busy cores the
# pg_isready form let the run through that window, the init.sql apply below
# failed, and its `|| true` made the assertion VANISH. The run reported
# "38 passed, 0 failed" against 39 idle -- a lost assertion reads exactly like
# a healthy pass unless someone counts.
PG_READY=0
for _ in $(seq 1 90); do
  if docker exec "$PG_CONTAINER" psql -U postgres -d zeroship -tAc 'select 1' >/dev/null 2>&1; then
    PG_READY=$((PG_READY + 1))
    [ "$PG_READY" -ge 3 ] && break
  else
    PG_READY=0
  fi
  sleep 1
done
[ "$PG_READY" -ge 3 ] \
  && pass "ephemeral PG query-able on :$PG_PORT (3 consecutive selects, dedicated)" \
  || { fail "PG never became query-able on :$PG_PORT"; docker logs --tail 20 "$PG_CONTAINER" 2>&1 | sed 's/^/      /'; exit 1; }

# A real failure, not `|| true`. init.sql sets `search_path = zeroship, public`
# for the postgres role, and several control queries name tables UNQUALIFIED
# (`UPDATE apps ...`), so a silent skip here leaves a database that works until
# it suddenly does not. The old form also sent both streams to /dev/null, so
# there was nothing to diagnose either.
if [ -f "$ROOT/deploy/ops/postgres-init.sql" ]; then
  if INIT_OUT="$(psql_exec < "$ROOT/deploy/ops/postgres-init.sql" 2>&1)"; then
    pass "applied deploy/ops/postgres-init.sql"
  else
    fail "postgres-init.sql failed -- search_path is unset, unqualified queries will break"
    printf '%s\n' "$INIT_OUT" | tail -10 | sed 's/^/      /'
  fi
fi

# Redpanda — the durable usage-event stream. Needs an explicit advertised
# listener so the worker producer + control consumers reach it at $RP_BROKERS.
docker rm -f "$RP_CONTAINER" >/dev/null 2>&1 || true
docker run --name "$RP_CONTAINER" -d -p "$REDPANDA_PORT:$REDPANDA_PORT" \
  docker.redpanda.com/redpandadata/redpanda:latest \
  redpanda start --overprovisioned --smp 1 --memory 512M --reserve-memory 0M \
  --node-id 0 --check=false \
  --kafka-addr "external://0.0.0.0:$REDPANDA_PORT" \
  --advertise-kafka-addr "external://127.0.0.1:$REDPANDA_PORT" \
  --set redpanda.auto_create_topics_enabled=true >/dev/null \
  || { fail "docker run redpanda failed"; exit 1; }
for _ in $(seq 1 40); do docker exec "$RP_CONTAINER" rpk cluster health --exit-when-healthy >/dev/null 2>&1 && break; sleep 1.5; done
if docker exec "$RP_CONTAINER" rpk cluster health --exit-when-healthy >/dev/null 2>&1; then
  pass "redpanda broker healthy on $RP_BROKERS (usage-event stream)"
else
  fail "redpanda never became healthy"; docker logs "$RP_CONTAINER" 2>&1 | tail -20; exit 1
fi

MIG_LOG="$WORK/migrate.log"
if "$BIN/zeroship-platform-migrate" \
    --database-url "$DBURL" \
    --migrations-dir "$ROOT/db/migrations-ts" \
    --project-schema zeroship --project-id zeroship > "$MIG_LOG" 2>&1; then
  pass "platform migrations applied cleanly from scratch (zeroship-platform-migrate)"
else
  fail "zeroship-platform-migrate FAILED (see $MIG_LOG)"; tail -20 "$MIG_LOG"; exit 1
fi

# Seed the metering-test plan (compute-unit pricing, Refactor B scalar schema).
# The global metric_weights (changeset 0041) weight `requests` at 1 CU/op, so
# N_REQ requests ⇒ ≥ N_REQ CU. This plan pins an explicit FX of 1 cent/CU
# (1e12 pico-cents/CU) so ~100 requests ⇒ ~$1.00+ priced spend — i.e. the
# historical "≈1 cent per request" used by Stage 5's low-cap → Block check
# (the tiny global default FX would price 100 CU to ~$0). `included_units = 0`
# so all CU are billable; high default cap so Stage 5's override drives Block.
if psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.plans
  (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit,
   runtime_limits_json, spend_limit_default_cents)
VALUES ('$PLAN_ID', 'metering-test', 0, 0, 1000000000000,
        '{"cpu_limit_ms":5000,"wall_timeout_ms":30000,"heap_limit_mb":256}', 1000000)
ON CONFLICT (id) DO NOTHING;
-- The global default FX (pricing_config.id='global') MUST exist or every spend +
-- billing sweep fails closed ("global default FX row is MISSING"). Match the
-- plan's 1 cent/CU so pricing is deterministic.
INSERT INTO zeroship.pricing_config (id, fx_pico_cents_per_unit)
VALUES ('global', 1000000000000)
ON CONFLICT (id) DO UPDATE SET fx_pico_cents_per_unit = EXCLUDED.fx_pico_cents_per_unit;
-- Register the platform metrics + WEIGHT 'requests' at 1 CU/op. Without a
-- metric_weights row the compute-unit pricing weights the metric at 0, so priced
-- spend is \$0 (no Block, no invoice). Only 'requests' is weighted so pricing is a
-- deterministic 1 cent/request; the other counters stay unpriced.
INSERT INTO zeroship.billing_metrics (metric, kind, unit) VALUES
  ('requests','platform','op'), ('cpu_us','platform','us'), ('wall_us','platform','us'),
  ('egress_bytes','platform','byte'), ('ingress_bytes','platform','byte')
ON CONFLICT (metric) DO UPDATE SET kind='platform';
INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units)
VALUES ('requests', 1, 1)
ON CONFLICT (metric) DO UPDATE SET units_per_op=1, per_units=1;
SQL
then pass "seeded plan ($PLAN_ID) + pricing_config global FX + metric_weights (requests=1 CU/op)"; else fail "plan seed failed"; exit 1; fi

# mock-Stripe (standalone, fixed port) — control's REAL cyper client targets it.
"$BIN/zeroship-mock-stripe" --port "$MOCK_PORT" > "$WORK/mock-stripe.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 20); do grep -q "mock-stripe listening" "$WORK/mock-stripe.log" 2>/dev/null && break; sleep 0.3; done
if curl -sf "$MOCK_URL/__mock/requests" >/dev/null 2>&1; then
  pass "mock-Stripe up on :$MOCK_PORT (introspection /__mock/requests live)"
else
  fail "mock-Stripe did not become ready"; tail -5 "$WORK/mock-stripe.log"; exit 1
fi

# signing key for PAT issuance + gateway/worker JWT.
openssl genpkey -algorithm ed25519 -out "$WORK/signing-key.pem" 2>/dev/null
chmod 600 "$WORK/signing-key.pem"

# Gateway broker secret (≥32 bytes) — required to start the gateway (it backs the
# OIDC RP broker). A per-run random secret is fine for the e2e.
openssl rand -base64 48 > "$WORK/gateway-broker-secret"
chmod 600 "$WORK/gateway-broker-secret"

# Shared config overlay (zeroship.toml). The [metering] section configures the
# usage-event stream from the config FILE — for BOTH the producers (worker +
# gateway) AND the control-plane consumers (forwarder + recompute) — instead of
# env vars. Each binary loads it via --config. (Per-process outbox WAL paths are
# passed separately since two producers on one host must not share one redb file.)
CFG_TOML="$WORK/zeroship.toml"
cat > "$CFG_TOML" <<TOML
[metering]
redpanda_brokers = "$RP_BROKERS"
usage_events_topic = "$USAGE_TOPIC"
TOML
pass "wrote shared config overlay $CFG_TOML ([metering] stream config, not env)"

# control — mock-Stripe base URL + the `lite` billing provider + the redpanda
# usage stream. `lite` is evaluation-grade (production_ready()=false) so it is
# boot-gated behind --allow-unsupported-billing. The forwarder + recompute
# consumers get distinct group ids from the base --stream-config (control injects
# group.id per role). A SHORT --spend-recompute-interval makes usage aggregation
# deterministic (the recompute drains the stream every 2s). lite is recompute-fed
# so no forwarder is spawned for it (Meter::accepts_forwarded_events=false).
ZEROSHIP_CONTROL_SIGNING_KEY_FILE="$WORK/signing-key.pem"
ZEROSHIP_GATEWAY_SIGNING_KEY_FILE="$ZEROSHIP_CONTROL_SIGNING_KEY_FILE"
ZEROSHIP_GATEWAY_BROKER_SECRET_FILE="$WORK/gateway-broker-secret"
e2e_export_runtime_secrets "$WORK" || exit 1
e2e_export_database_urls "$DBURL"
ZEROSHIP_CONTROL_STRIPE_SECRET_KEY="sk_test_e2e_billing" \
e2e_with_platform_mint_key "$BIN/zeroship-control" --port "$ZEROSHIP_CONTROL_PORT" \
  --config "$CFG_TOML" \
  --blob-store "$WORK/blobs" --signing-key-file "$WORK/signing-key.pem" \
  --stripe-base-url "$MOCK_URL" \
  --meter-provider lite --invoicer-provider lite --allow-unsupported-billing \
  --spend-recompute-interval 2 \
 > "$WORK/control.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$CONTROL_URL/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$CONTROL_URL/readyz" >/dev/null 2>&1 \
  && pass "control healthy (lite provider, stream=redpanda, stripe→mock :$MOCK_PORT)" || { fail "control unhealthy"; tail -30 "$WORK/control.log"; exit 1; }

# worker — publishes drained usage events to redpanda. The [metering] stream
# config comes from --config; only the per-process outbox WAL path is passed
# separately (two producers on one host must not share one redb file).
USAGE_OUTBOX_WAL_PATH="$WORK/worker-outbox.redb" \
"$BIN/zeroship-worker" --port "$ZEROSHIP_WORKER_PORT" --threads "$ZEROSHIP_WORKER_THREADS" \
  --config "$CFG_TOML" \
  --control-url "$CONTROL_URL" \
  --blob-store "$WORK/blobs" --poll-interval 2 > "$WORK/worker.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" >/dev/null 2>&1 \
  && pass "worker healthy (usage outbox → redpanda $USAGE_TOPIC)" || { fail "worker unhealthy"; tail -30 "$WORK/worker.log"; exit 1; }

# gateway — pulls routes (incl. spend_state) every 2s; ALSO a usage producer
# (gateway_egress_bytes etc.), publishing to the same stream via --config.
USAGE_OUTBOX_WAL_PATH="$WORK/gate-outbox.redb" \
"$BIN/zeroship-gate" --port "$ZEROSHIP_GATEWAY_PORT" --control-url "$CONTROL_URL" \
  --config "$CFG_TOML" \
  --worker-urls "http://localhost:$ZEROSHIP_WORKER_PORT" --blob-store "$WORK/blobs" \
  --blob-cache-disk-root "$WORK/blob-cache" --poll-interval 2 \
  --signing-key-file "$WORK/signing-key.pem" \
  --broker-secret-file "$WORK/gateway-broker-secret" \
 > "$WORK/gate.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" >/dev/null 2>&1 \
  && pass "gateway healthy (route-pull poll-interval 2s)" || { fail "gateway unhealthy"; tail -30 "$WORK/gate.log"; exit 1; }

# The migration service. Same invocation as tests/e2e_db_app_end_to_end.sh.
"$BIN/zeroship-migrated" --port "$ZEROSHIP_MIGRATED_PORT" \
  --signing-key-file "$WORK/signing-key.pem" --tmp-dir "$WORK/migrated-tmp" \
  > "$WORK/migrated.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$MIGRATED_URL/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$MIGRATED_URL/readyz" >/dev/null 2>&1 \
  && pass "zeroship-migrated healthy (applies the probe's committed migrations)" \
  || { fail "zeroship-migrated unhealthy"; tail -30 "$WORK/migrated.log"; exit 1; }

# ===========================================================================
echo ""
echo "=== Stage 2: mint admin PAT (offline) + create creator + deploy metering-probe ==="
# ===========================================================================

# Offline-mint a platform-admin PAT (signs an EdDSA pat+jwt the PatIssuer
# verifies; seeds users + platform_admin_roles + permission_tokens).
POLICY_JSON='{"name":"e2e-bill-admin","statements":[{"effect":"allow","actions":["apps:read","apps:write","apps:deploy","apps:delete","deployments:read","deployments:rollback","env:read","env:write","secrets:read","secrets:write","billing:read","billing:write"],"resources":[{"type":"any"}],"conditions":[]}]}'
POLICY_HASH="$(node -e '
const {createHash}=require("crypto");
function c(v){if(v===null||typeof v==="number"||typeof v==="boolean")return JSON.stringify(v);
if(typeof v==="string")return JSON.stringify(v);
if(Array.isArray(v))return "["+v.map(c).join(",")+"]";
return "{"+Object.keys(v).sort().map(k=>JSON.stringify(k)+":"+c(v[k])).join(",")+"}";}
process.stdout.write(createHash("sha256").update(c(JSON.parse(process.argv[1]))).digest("hex"));
' "$POLICY_JSON")"
# The PAT owner is ALSO the creator that owns the deployed app (so the billing
# reconciler groups the app under this creator).
CREATOR="$(node -e 'console.log(require("crypto").randomUUID())')"
TOKID="$(node -e 'console.log(require("crypto").randomUUID())')"
EXP=$(( $(date +%s) + 86400 ))
psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id, email, name, email_verified_at)
VALUES ('$CREATOR', 'e2e-bill-$CREATOR@zeroship.test'::citext, 'E2E Billing Admin', NOW());
INSERT INTO zeroship.platform_admin_roles (user_id, role, granted_by)
VALUES ('$CREATOR', 'admin', '$CREATOR');
INSERT INTO zeroship.permission_tokens (id, owner_id, kind, name, policies, policy_hash, expires_at)
VALUES ('$TOKID', '$CREATOR', 'pat', 'e2e billing harness', '$POLICY_JSON'::jsonb, '$POLICY_HASH', to_timestamp($EXP));
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
' "$WORK/signing-key.pem" "$CREATOR" "$TOKID" "$POLICY_HASH" "$EXP")"
[ "$(echo -n "$PAT" | awk -F. '{print NF}')" = "3" ] && pass "minted pat+jwt (creator=$CREATOR)" || { fail "PAT mint failed: $PAT"; exit 1; }

# Create the app ON the metering-test plan.
APP_JSON="$(curl -s -X POST "$CONTROL_URL/api/apps" -H 'Content-Type: application/json' \
  -H "Authorization: Bearer $PAT" -d "{\"name\":\"metering-probe\",\"plan_id\":\"$PLAN_ID\"}")"
APP="$(echo "$APP_JSON" | jget '.id')"
[ -n "$APP" ] && pass "created app metering-probe → $APP (plan=$PLAN_ID)" || { fail "create-app failed: $APP_JSON"; exit 1; }

# The PAT user owns the app (app_members role='owner') so the billing
# reconciler groups it under this creator. control's create-app may already do
# this for the PAT principal; make it explicit + idempotent.
psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.app_members (app_id, user_id, role) VALUES ('$APP', '$CREATOR', 'owner')
ON CONFLICT (app_id, user_id) DO UPDATE SET role='owner';
SQL

DEP="$("$BIN/zeroship" deploy "$PROBE_ZSHIP" --app="$APP" --control="$CONTROL_URL" --token="$PAT" 2>&1)"
echo "$DEP" | grep -q "deploy_hash" && pass "deployed metering-probe .zship → $APP" || { fail "deploy failed: $DEP"; tail -20 "$WORK/control.log"; exit 1; }

# Apply the probe's migrations to its per-app schema. Deploy uploads the bundle;
# it does NOT create tables. Without this the probe's env.db insert has nothing
# to write to, which is the state this harness ran in until 2026-08-11.
write_apply_request || { fail "could not record migration IR"; exit 1; }
APPLY_CODE="$(curl -s -o "$WORK/apply-response.json" -w '%{http_code}' \
  -X POST "$MIGRATED_URL/v1/apps/$APP/migrations/apply" \
  -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" \
  --data-binary @"$WORK/apply-migrations.json")"
APPLIED="$(jget '.applied.length' < "$WORK/apply-response.json")"
if [ "$APPLY_CODE" = "200" ] && [ -n "$APPLIED" ] && [ "$APPLIED" -ge 1 ] 2>/dev/null; then
  pass "zeroship-migrated applied the probe's migrations (applied=$APPLIED)"
else
  fail "migration apply failed (http=$APPLY_CODE): $(cat "$WORK/apply-response.json")"
  tail -30 "$WORK/migrated.log"; exit 1
fi

sleep 5  # let route + version sync to gateway + worker

# ===========================================================================
echo ""
echo "=== Stage 3: real gateway traffic → worker (platform-measured metering) ==="
# ===========================================================================
# Metering is infrastructure: the probe drives an env.db write+read per
# request; the worker emits db_writes/db_reads + the five platform counters.
# There is NO env.meter — app code cannot self-report.
# 100 requests = 100 priced cents — large enough that the spend-Degrade band
# (95–99% of the cap) is reachable with an integer cap in Stage 5.
N_REQ=100
REQ_BODY='{"hello":"metering","n":1}'   # measurable ingress body
# Readiness gate: the gateway must have synced the app's route from control AND
# the worker must be able to load the app on-demand before the counted loop —
# otherwise early requests miss (route not yet pulled / cold worker thread). Poll
# until a probe request returns 200 (bounded), then warm both worker threads.
READY=0
for _ in $(seq 1 30); do
  C="$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: metering-probe.localhost' \
        -H 'content-type: application/json' --data "$REQ_BODY" "http://localhost:$ZEROSHIP_GATEWAY_PORT/probe/ready")"
  if [ "$C" = "200" ]; then READY=1; break; fi
  sleep 1
done
[ "$READY" = "1" ] && pass "gateway route synced + app loadable (probe returns 200)" || { fail "app never became reachable via the gateway"; tail -15 "$WORK/gate.log"; exit 1; }
# Warm both worker threads so the counted loop doesn't race a cold on-demand load.
for _ in $(seq 1 $((ZEROSHIP_WORKER_THREADS * 3))); do
  curl -s -o /dev/null -H 'Host: metering-probe.localhost' -H 'content-type: application/json' \
    --data "$REQ_BODY" "http://localhost:$ZEROSHIP_GATEWAY_PORT/probe/warmup" || true
done
GW_OK=0
LAST_BODY=""
for i in $(seq 1 $N_REQ); do
  # Retry a cold-start/transient miss so all N_REQ are counted (the Stage 4/5
  # assertions depend on exactly N_REQ priced requests landing).
  for _ in 1 2 3; do
    R="$(curl -s -w '\n%{http_code}' -H 'Host: metering-probe.localhost' \
          -H 'content-type: application/json' \
          --data "$REQ_BODY" "http://localhost:$ZEROSHIP_GATEWAY_PORT/probe/$i")"
    CODE="$(echo "$R" | tail -1)"; BODY="$(echo "$R" | head -n -1)"
    [ "$CODE" = "200" ] && { GW_OK=$((GW_OK+1)); LAST_BODY="$BODY"; break; }
    sleep 0.2
  done
done
# The second clause used to be `grep -q '"metric":"db_writes"'` -- a match on the
# STRING db_writes anywhere in the response body. The probe prints that name in
# its own payload whether or not the write happened, so the clause was satisfied
# by the app describing itself. Measured 2026-08-11 at HEAD: this pass line
# printed `probe db write ok=false` on all 100 requests and stayed green.
# `.wrote` is the observable; assert on it.
WROTE="$(echo "$LAST_BODY" | jget '.wrote')"
if [ "$GW_OK" = "$N_REQ" ] && [ "$WROTE" = "true" ]; then
  pass "drove $GW_OK/$N_REQ requests through the gateway (HTTP 200; probe db write ok=$WROTE)"
else
  fail "gateway traffic: $GW_OK/$N_REQ HTTP 200, probe db write ok=$WROTE (want true) -- body: ${LAST_BODY:0:300}"
  tail -20 "$WORK/worker.log"; exit 1
fi

# ===========================================================================
echo ""
echo "=== Stage 4: metering → aggregation (worker → redpanda → recompute → usage_aggregates) ==="
# ===========================================================================
# The worker outbox drains the Meter + publishes UsageEvents to redpanda every
# ~10s; control's spend_recompute consumer drains the stream into
# usage_aggregates every ~2s (--spend-recompute-interval). Poll the creator usage
# endpoint until the aggregates appear (bounded wait), then assert the platform
# counters AND the custom metric — proving the whole stream path, not a POST.
# The probe drives one env.db write + read per request, so the platform
# emits `db_writes`/`db_reads` (≥ N_REQ) alongside the five platform counters.
PRIMARY_METRIC="db_writes"
USAGE_JSON=""
for _ in $(seq 1 20); do
  USAGE_JSON="$(curl -s "$CONTROL_URL/api/apps/$APP/usage" -H "Authorization: Bearer $PAT")"
  REQS="$(echo "$USAGE_JSON" | jget '.requests')"
  if [ -n "$REQS" ] && [ "$REQS" != "0" ]; then break; fi
  sleep 2
done
echo "    usage_aggregates (current period): $USAGE_JSON"

REQS="$(echo "$USAGE_JSON" | jget '.requests')"
WALL="$(echo "$USAGE_JSON" | jget '.wall_us')"
EGRESS="$(echo "$USAGE_JSON" | jget '.egress_bytes')"
INGRESS="$(echo "$USAGE_JSON" | jget '.ingress_bytes')"
CPU="$(echo "$USAGE_JSON" | jget '.cpu_us')"
DBW="$(echo "$USAGE_JSON" | jget ".$PRIMARY_METRIC")"

[ -n "$REQS" ]   && [ "$REQS"   -ge "$N_REQ" ] 2>/dev/null && pass "requests aggregated ($REQS ≥ $N_REQ) for the current period" || fail "requests not aggregated (got '$REQS', want ≥ $N_REQ)"
[ -n "$WALL" ]   && [ "$WALL"   -gt 0 ]       2>/dev/null && pass "wall_us present and non-zero ($WALL)"        || fail "wall_us missing/zero (got '$WALL')"
[ -n "$EGRESS" ] && [ "$EGRESS" -gt 0 ]       2>/dev/null && pass "egress_bytes present and non-zero ($EGRESS)" || fail "egress_bytes missing/zero (got '$EGRESS')"
[ -n "$INGRESS" ] && [ "$INGRESS" -gt 0 ]     2>/dev/null && pass "ingress_bytes present and non-zero ($INGRESS)" || fail "ingress_bytes missing/zero (got '$INGRESS')"
# cpu_us is a documented sync lower-bound — assert it's present/>=0, don't over-assert.
if [ -n "$CPU" ] && [ "$CPU" -ge 0 ] 2>/dev/null; then pass "cpu_us present (sync lower-bound, $CPU ≥ 0)"; else fail "cpu_us absent (got '$CPU')"; fi
# db_writes is platform-measured (emitted by the trusted env.db primitive, NOT
# self-reported by app code). One write per request => >= N_REQ.
#
# THIS USED TO BE A SOFT NOTE, and the note was hiding a live failure. The else
# arm was a bare `echo`, so the strongest claim in the whole billing story --
# that a PRIMITIVE the app cannot forge fed the meter -- could not fail. Its
# stated reason ("the platform counters above already prove the pipeline")
# answers a different question: the platform counters prove FLUSH -> AGGREGATE;
# only this metric proves the primitive EMITS.
#
# MEASURED 2026-08-11 at HEAD, and it is why this is now a hard fail:
#     usage_aggregates (current period):
#       {"cpu_us":23466,"requests":108,"ingress_bytes":2808,"egress_bytes":41708,"wall_us":5024497}
#     NOTE: 'db_writes' got '' (< 100).
# Seven platform counters, no db_writes at all, and the harness reported a pass.
#
# ROOT CAUSE, found rather than worked around, and it is a mechanism this repo
# has already named (#209): examples/metering-probe has NO migrations/ directory
# and declares its schema INLINE via schema()/t.* in src/index.ts. An inline
# schema builds a servable .zship whose manifest carries no runtime_descriptor,
# so nothing installs on env.db and every insert fails -- which is exactly what
# `probe db write ok=false` above was reporting. This harness also never invokes
# zeroship-migrated (zero references in the file), unlike
# tests/e2e_db_app_end_to_end.sh, whose db-hitcounter has a committed
# migrations/20260711000000_create_hits.ts and an apply step.
#
# So this gate is RED at HEAD BY DESIGN until the probe gets migrations. That is
# the honest state: the property was never true, and the guard was what made it
# look true.
if [ -n "$DBW" ] && [ "$DBW" -ge "$N_REQ" ] 2>/dev/null; then
  pass "platform-measured metric '$PRIMARY_METRIC' aggregated ($DBW >= $N_REQ) -- env.db primitive fed the pipeline (unforgeable; no env.meter)"
else
  fail "'$PRIMARY_METRIC' got '$DBW' (want >= $N_REQ) -- the env.db primitive did NOT feed the meter, so nothing here proves platform-measured billing"
fi

# ===========================================================================
echo ""
echo "=== Stage 5: spend enforcement — set a LOW cap, cross it, force sweep → 402 Block ==="
# ===========================================================================
# The app has >= N_REQ priced requests (1 cent each) in the current period, so
# priced spend is >= N_REQ cents. Set the override cap to 1 cent so spend is
# WAY over 100% ⇒ derive_state = Block.
SL_JSON="$(curl -s -w '\n%{http_code}' -X PUT "$CONTROL_URL/api/apps/$APP/spend-limit" \
  -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" -d '{"cents":1}')"
SL_CODE="$(echo "$SL_JSON" | tail -1)"
[ "$SL_CODE" = "200" ] && pass "set spend-limit override to 1 cent (PUT /api/apps/:id/spend-limit → 200)" || { fail "set spend-limit failed (HTTP $SL_CODE): $(echo "$SL_JSON" | head -n -1)"; }

# Force ONE spend sweep on demand (operator-gated internal endpoint) so we
# don't wait on the 60s cron. This prices the app's current-period usage,
# derives Block, and persists app_spend_state.
SR="$(curl -s -X POST -H "Authorization: Bearer $ZEROSHIP_CONTROL_KEY" "$CONTROL_URL/internal/spend/reconcile")"
echo "    spend sweep result: $SR"
pass "forced spend sweep via POST /internal/spend/reconcile (operator-gated)"

# Confirm control persisted Block.
STATE="$(psql_exec -tA -c "SELECT state FROM zeroship.app_spend_state WHERE app_id='$APP'" 2>/dev/null | tr -d '[:space:]')"
[ "$STATE" = "block" ] && pass "control derived spend state = block (app_spend_state)" || fail "expected block, got '$STATE'"

# The gateway must pick the new state up via the route pull (poll-interval 2s).
# Poll the gateway until it returns 402 for the app (bounded wait).
GW402=0
for _ in $(seq 1 15); do
  C="$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: metering-probe.localhost' "http://localhost:$ZEROSHIP_GATEWAY_PORT/probe/blocked")"
  if [ "$C" = "402" ]; then GW402=1; break; fi
  sleep 1
done
if [ "$GW402" = "1" ]; then
  # Confirm the 402 carries the SPEND_LIMIT code (the gateway's check_spend body).
  BLK_BODY="$(curl -s -H 'Host: metering-probe.localhost' "http://localhost:$ZEROSHIP_GATEWAY_PORT/probe/blocked")"
  echo "$BLK_BODY" | grep -q 'SPEND_LIMIT' \
    && pass "gateway returns 402 SPEND_LIMIT for the over-limit app (route-pull → check_spend Block)" \
    || fail "gateway 402 but body lacked SPEND_LIMIT code: $BLK_BODY"
else
  fail "gateway never returned 402 for the blocked app within the poll window"; tail -10 "$WORK/gate.log"
fi

# --- intermediate threshold: Degrade tightens throughput -------------------
# Raise the cap so priced spend lands in the Degrade band (95–99% of the cap).
# With ~$REQS cents of spend, a cap of ceil(spend/0.96) puts pct ≈ 96% (Degrade,
# below the 100% Block boundary). Best-effort: assert the derived state if we
# can land it; otherwise note it.
SPEND_CENTS="$REQS"
if [ -n "$SPEND_CENTS" ] && [ "$SPEND_CENTS" -ge 50 ] 2>/dev/null; then
  # Choose cap so pct = spend*100/cap lands ≈ 97 (inside the 95–99 Degrade
  # band, below the 100% Block boundary). Integer cap = floor(spend*100/97).
  DEG_CAP=$(( SPEND_CENTS * 100 / 97 ))
  curl -s -o /dev/null -X PUT "$CONTROL_URL/api/apps/$APP/spend-limit" \
    -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" -d "{\"cents\":$DEG_CAP}"
  curl -s -o /dev/null -X POST -H "Authorization: Bearer $ZEROSHIP_CONTROL_KEY" "$CONTROL_URL/internal/spend/reconcile"
  DEG_STATE="$(psql_exec -tA -c "SELECT state FROM zeroship.app_spend_state WHERE app_id='$APP'" 2>/dev/null | tr -d '[:space:]')"
  if [ "$DEG_STATE" = "degrade" ]; then
    pass "intermediate threshold: cap=\$$DEG_CAP cents puts spend ($SPEND_CENTS cents) in Degrade (throttle, not Block)"
  else
    known "intermediate Degrade not landed (cap=$DEG_CAP → state=$DEG_STATE); Block path already proven above"
  fi
else
  known "skipped Degrade staging (priced spend $SPEND_CENTS too small to band)"
fi

# ===========================================================================
echo ""
echo "=== Stage 6: Stripe reconcile — closed (previous-month) period, on demand ==="
# ===========================================================================
# Seed a SEPARATE creator + owned app on the priced plan, give it a
# creator_billing Customer (set up via the mock client path), and seed usage in
# the CLOSED (previous-month) period. Then trigger the operator-gated billing
# reconcile for that period and assert the mock recorded the invoice-item +
# invoice calls exactly once; a second trigger is idempotent.

# Reset the mock's recorded set so the closed-period assertions are isolated.
curl -s -o /dev/null -X POST "$MOCK_URL/__mock/reset"

CLOSED_CREATOR="$(node -e 'console.log(require("crypto").randomUUID())')"
CLOSED_APP="$(node -e 'console.log(require("crypto").randomUUID())')"
# previous_period_start_unix(now) — the closed month the reconciler bills.
NOW_UNIX="$(date +%s)"
PERIOD_START="$(node -e '
const now=new Date(Number(process.argv[1])*1000);
let y=now.getUTCFullYear(), m=now.getUTCMonth(); // 0-based; prev month
if(m===0){y-=1;m=11;}else{m-=1;}
process.stdout.write(String(Math.floor(Date.UTC(y,m,1,0,0,0)/1000)));
' "$NOW_UNIX")"

if psql_exec >/dev/null <<SQL
INSERT INTO zeroship.users (id, email, name, email_verified_at)
VALUES ('$CLOSED_CREATOR', 'e2e-closed-$CLOSED_CREATOR@zeroship.test'::citext, 'Closed-Period Creator', NOW());
INSERT INTO zeroship.apps (id, name, plan_id, api_key, api_key_hash)
VALUES ('$CLOSED_APP', 'closed-period-app-$CLOSED_APP', '$PLAN_ID', '$CLOSED_APP', '');
INSERT INTO zeroship.app_members (app_id, user_id, role) VALUES ('$CLOSED_APP', '$CLOSED_CREATOR', 'owner');
-- The creator must have a saved platform Stripe Customer or the reconciler skips
-- them. The customer lives in billing_customer_refs (provider='stripe'); the
-- creator_billing identity row backs the notify-cron FK.
INSERT INTO zeroship.creator_billing (creator_id) VALUES ('$CLOSED_CREATOR')
ON CONFLICT (creator_id) DO NOTHING;
INSERT INTO zeroship.billing_customer_refs (creator_id, provider, external_id)
VALUES ('$CLOSED_CREATOR', 'stripe', 'cus_e2e_closed')
ON CONFLICT (creator_id, provider) DO UPDATE SET external_id = EXCLUDED.external_id;
-- Seed 750 priced requests (= 750 cents) in the CLOSED period. usage_aggregates
-- is keyed by the period DATE (the month bucket), not a timestamp.
INSERT INTO zeroship.usage_aggregates (app_id, period, metric, total)
VALUES ('$CLOSED_APP', to_timestamp($PERIOD_START)::date, 'requests', 750)
ON CONFLICT (app_id, period, metric) DO UPDATE SET total = 750;
SQL
then pass "seeded closed-period creator+app (period=$(date -u -d @$PERIOD_START +%Y-%m-%d), 750 priced requests, Customer cus_e2e_closed)"; else fail "closed-period seed failed"; fi

# Trigger the on-demand reconcile for this period (operator-gated internal).
# We pass `now`=$NOW_UNIX; the endpoint bills previous_period_start_unix(now).
RECON="$(curl -s -X POST -H "Authorization: Bearer $ZEROSHIP_CONTROL_KEY" "$CONTROL_URL/internal/billing/reconcile?period=$NOW_UNIX")"
echo "    reconcile result: $RECON"
BILLED="$(echo "$RECON" | jget '.billed')"
RPERIOD="$(echo "$RECON" | jget '.period_start')"
[ "$BILLED" = "1" ] && pass "billing reconcile billed 1 creator for the closed period" || fail "expected billed=1, got '$BILLED' ($RECON)"
[ "$RPERIOD" = "$PERIOD_START" ] && pass "reconciler resolved the expected closed period_start ($RPERIOD)" || fail "period mismatch: endpoint=$RPERIOD seeded=$PERIOD_START"

# Assert the mock recorded EXACTLY ONE invoice-item create for this creator,
# plus the invoice create + finalize (both POST /v1/invoices…).
MOCK_REQS="$(curl -s "$MOCK_URL/__mock/requests")"
# process.stdout.write(String(n)), NOT console.log(n). console.log routes ANY
# NON-STRING through util.inspect, which colours it when colour is forced -- and
# FORCE_COLOR is set in plenty of developer shells. The comparisons below are
# string equality, so `1` arrives as ESC[33m1ESC[39m and every one of them
# fails while the platform is behaving. MEASURED 2026-08-11 on this machine
# (FORCE_COLOR=3): `node -e 'console.log(1)'` into a pipe emits
# 033 [ 3 3 m 1 033 [ 3 9 m; with FORCE_COLOR unset it emits a bare `1`.
# Same fix as cee243ec4 (#211), which cleared the JSON-extractor sites and did
# not reach the shell harnesses. `jget` in this same file already writes raw,
# which is why .billed and .period_start were never affected.
#
# THIS COMMENT SAID "a NUMBER" UNTIL 2026-08-12, and that wording cost a whole
# extra round. Booleans colourise identically: e2e_app_primitives_kv_storage.sh
# compared `.deleted` and `.found` and got ESC[33mtrueESC[39m /
# ESC[33mfalseESC[39m, so the storage-delete assertion reported a failure over
# correct behaviour. I had swept for the class the day before and declared it
# closed -- but I grepped for NUMERIC shapes, because this comment framed the
# rule numerically. The rule is ANY non-string; strings are the only safe arg.
# Also worth knowing for the next sweep: every one of these extractors carries a
# `console.log("")` in its catch arm, so a LINE-BASED `grep -v 'console.log("'`
# deletes exactly the lines you are hunting. Match `console.log(JSON.parse`
# positively instead of excluding.
count_path() { echo "$MOCK_REQS" | node -e '
let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{
  const arr=JSON.parse(s);
  const [method,prefix]=process.argv.slice(1);
  process.stdout.write(String(arr.filter(r=>r.method===method && r.path.startsWith(prefix) && !r.replayed).length)+"\n");
});' "$1" "$2"; }
ITEMS="$(count_path POST /v1/invoiceitems)"
INVOICES="$(count_path POST /v1/invoices)"
[ "$ITEMS" = "1" ]    && pass "mock-Stripe recorded EXACTLY ONE invoice-item create for the closed period" || fail "expected 1 invoice-item create, got '$ITEMS'"
[ "$INVOICES" = "2" ] && pass "mock-Stripe recorded invoice create + finalize (2 POST /v1/invoices)"        || fail "expected 2 /v1/invoices (create+finalize), got '$INVOICES'"

# Prove faithfulness: the recorded item carries the deterministic Idempotency-Key
# and the Bearer auth our control client sent.
HAS_KEY="$(echo "$MOCK_REQS" | node -e '
let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{
  const arr=JSON.parse(s);
  const it=arr.find(r=>r.path.startsWith("/v1/invoiceitems"));
  console.log(it && /^billitem:/.test(it.idempotency_key||"") && /^Bearer sk_test_e2e_billing/.test(it.authorization||"") ? "yes":"no");
});')"
[ "$HAS_KEY" = "yes" ] && pass "recorded item carries deterministic Idempotency-Key (billitem:…) + Bearer auth (real cyper wire path)" || fail "invoice-item lacked the expected Idempotency-Key/auth (not the real wire path?)"

# The billing rail records exactly one FINALIZED invoice for (creator, period),
# priced at 750 cents (750 requests × 1 cent). (The legacy billing_runs table is
# gone; the invoice is the system of record.)
INV_N="$(psql_exec -tA -c "SELECT COUNT(*) FROM zeroship.invoices WHERE creator_id='$CLOSED_CREATOR' AND status='finalized'" 2>/dev/null | tr -d '[:space:]')"
INV_TOTAL="$(psql_exec -tA -c "SELECT COALESCE(total_cents,0) FROM zeroship.invoices WHERE creator_id='$CLOSED_CREATOR' AND status='finalized' LIMIT 1" 2>/dev/null | tr -d '[:space:]')"
[ "$INV_N" = "1" ] && pass "invoices has exactly one finalized invoice for the creator/period" || fail "expected 1 finalized invoice, got '$INV_N'"
[ "$INV_TOTAL" = "750" ] && pass "finalized invoice total = 750 cents (750 requests × 1¢)" || fail "expected invoice total 750, got '$INV_TOTAL'"

# --- idempotency: a SECOND trigger creates NO new items ---------------------
RECON2="$(curl -s -X POST -H "Authorization: Bearer $ZEROSHIP_CONTROL_KEY" "$CONTROL_URL/internal/billing/reconcile?period=$NOW_UNIX")"
BILLED2="$(echo "$RECON2" | jget '.billed')"
[ "$BILLED2" = "0" ] && pass "second reconcile is a no-op (billed=0) — per-period idempotency" || fail "expected billed=0 on re-trigger, got '$BILLED2' ($RECON2)"
MOCK_REQS2="$(curl -s "$MOCK_URL/__mock/requests")"
MOCK_REQS="$MOCK_REQS2"
ITEMS2="$(count_path POST /v1/invoiceitems)"
[ "$ITEMS2" = "1" ] && pass "no double-bill: invoice-item create count still 1 after the second trigger" || fail "double-bill! invoice-item creates = '$ITEMS2' after re-trigger (expected 1)"

# --- gate check: the internal endpoint is not an unauthenticated bypass -----
UNAUTH_RECON_CODE="$(curl -s -o /dev/null -w '%{http_code}' -X POST "$CONTROL_URL/internal/billing/reconcile?period=$NOW_UNIX")"
[ "$UNAUTH_RECON_CODE" = "401" ] \
  && pass "force-reconcile rejects a request without the control bearer" \
  || fail "force-reconcile without control bearer returned $UNAUTH_RECON_CODE (expected 401)"

# ===========================================================================
echo ""
echo "============================================"
echo "  Results: $PASS passed, $FAIL failed, $KNOWN known-fail"
echo "============================================"

# A FLOOR ON ASSERTIONS THAT RAN, not on assertions that passed. `FAIL -eq 0`
# below already catches a red; this catches the other failure, and this harness
# has now demonstrated it rather than theorised it.
#
# MEASURED 2026-08-11, all four runs mine:
#   idle,   before a70280f4c   39 passed, 0 failed
#   loaded, before a70280f4c   38 passed, 0 failed   <- an assertion VANISHED
#   loaded, after              39 passed, 0 failed
#   idle,   after              39 passed, 0 failed
# The 38 was `applied deploy/ops/postgres-init.sql` disappearing behind a
# `|| true` when a loaded host widened the Postgres readiness window. It
# printed "0 failed" and read as healthy. Nothing here could tell the two
# apart, which is exactly what a floor is for.
#
# PASS+FAIL, because a mutation or a genuine red moves an outcome between the
# columns without removing it; only a lost assertion drops the sum. Same
# invariant as the five dev-vs-deployed harnesses (b41665ea8 and after).
#
# NO HEADROOM: 39 is the count this file reaches, fixed by the source, not
# discovered at run time. Adding an assertion passes untouched; removing one
# costs a deliberate edit here.
#
# WHAT IT DOES NOT CATCH: substitution. Swapping one assertion for an easier
# one keeps the total at 39. Nothing here can see that; review can.
BILLING_MIN_RAN="${BILLING_MIN_RAN:-39}"
RAN=$((PASS + FAIL))
rc=0
[ "$FAIL" -eq 0 ] || rc=1
if [ "$RAN" -lt "$BILLING_MIN_RAN" ]; then
  echo "FAIL: only $RAN assertions RAN, fewer than the $BILLING_MIN_RAN this gate expects." >&2
  echo "      (passed $PASS, failed $FAIL -- the floor counts both, because a failing" >&2
  echo "      assertion still ran and is already caught above.)" >&2
  echo "      An assertion that DISAPPEARS reports 0 failed and reads as healthy;" >&2
  echo "      this harness lost one exactly that way under load on 2026-08-11." >&2
  rc=1
fi
exit "$rc"
