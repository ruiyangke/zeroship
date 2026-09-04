#!/usr/bin/env bash
# ============================================================================
# e2e_db_app_end_to_end.sh — migration-first creator app DB path, end to end.
#
# Proves the real deployed app path for env.db:
#   1. committed op.* migrations -> committed generated descriptor artifacts
#   2. vite builds a .zship whose manifest carries runtime_descriptor
#   3. zeroship deploy uploads that .zship
#   4. zeroship-migrate-server applies recorded IR to the app's Postgres schema
#   5. gateway -> worker -> env.db insert/find succeeds on the deployed app
#   6. db_reads/db_writes reach usage_aggregates and are included in charge
#
# REFUSES (exit 1) when docker is unavailable. KEEP_WORK=1 preserves logs/containers.
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"
# shellcheck source=tests/lib/runtime_secrets.sh
source "$ROOT/tests/lib/runtime_secrets.sh"
# shellcheck source=tests/lib/usage_producer.sh
source "$ROOT/tests/lib/usage_producer.sh"
PASS=0
FAIL=0
pass(){ PASS=$((PASS+1)); echo "  ✓ $1"; }
fail(){ FAIL=$((FAIL+1)); echo "  ✗ $1"; }

echo "============================================"
echo "  zeroship E2E — deployed migration-first env.db app"
echo "============================================"

# Docker unavailable is a REFUSAL, not a skip. Everything below runs against an
# ephemeral PG and redpanda this harness starts itself, so without docker NOTHING
# here runs - and until 2026-08-20 that printed a warning and exited 0, which any
# caller reads as "the deployed env.db path passed". The eleven harnesses
# 7ff94acb0 converted on 2026-08-11 did not include this one.
if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  echo "  x REFUSED: docker unavailable, so NOTHING in this harness ran." >&2
  echo "    Exiting non-zero: a run that asserted nothing is not a passing run." >&2
  echo "    Start docker and re-run." >&2
  exit 1
fi

for b in zeroship zeroship-control zeroship-gate zeroship-worker zeroship-migrate-server; do
  [ -x "$BIN/$b" ] || { echo "missing $BIN/$b - run cargo build --release"; exit 2; }
done
[ -f "$ROOT/packages/zero-migrate-cli/dist/cli-bin.js" ] || { echo "missing the zero-migrate CLI - run: pnpm install && pnpm build"; exit 2; }
command -v node >/dev/null && command -v openssl >/dev/null && command -v curl >/dev/null && command -v pnpm >/dev/null || {
  echo "need node/openssl/curl/pnpm"
  exit 2
}

APP_EXAMPLE="$ROOT/examples/db-hitcounter"
[ -d "$APP_EXAMPLE" ] || { echo "missing examples/db-hitcounter"; exit 2; }
JOSE="$ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"
[ -f "$JOSE" ] || { echo "missing jose"; exit 2; }

ZEROSHIP_CONTROL_PORT=9186
ZEROSHIP_WORKER_PORT=8086
ZEROSHIP_GATEWAY_PORT=8076
ZEROSHIP_MIGRATE_SERVER_PORT=9086
PG_PORT=5486
RP_PORT=19186
PGC=zs-e2e-dbapp-pg
RPC=zs-e2e-dbapp-redpanda
DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"
CONTROL_URL="http://localhost:$ZEROSHIP_CONTROL_PORT"
MIGRATE_SERVER_URL="http://localhost:$ZEROSHIP_MIGRATE_SERVER_PORT"
RP_BROKERS="127.0.0.1:$RP_PORT"
USAGE_TOPIC="zeroship-usage-dbapp-e2e"
APP_HOST="db-hitcounter.localhost"
PLAN_ID="pln_dbapp_e2e"
N_REQ=30

WORK="$(mktemp -d -t zs-e2e-dbapp-XXXXXX)"
mkdir -p "$WORK/blobs" "$WORK/blob-cache"
PIDFILE="$WORK/pids"
: > "$PIDFILE"

jget(){ node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);process.stdout.write(String(o$1??'')+'\n')}catch(e){console.log('')}})"; }
psql_exec(){ docker exec -i "$PGC" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 "$@"; }
gw(){ curl -s -H "Host: $APP_HOST" "$@"; }
usage_of(){ psql_exec -tA -c "SELECT COALESCE(SUM(total),0) FROM zeroship.usage_aggregates WHERE app_id='$APP' AND metric='$1'" 2>/dev/null | tr -d '[:space:]'; }

manifest_descriptor_hash(){
  node --input-type=module - "$1" <<'NODE'
import { readFileSync } from "node:fs";
import { zstdDecompressSync } from "node:zlib";
const file = process.argv[2];
const archive = zstdDecompressSync(readFileSync(file));
let offset = 0;
let manifest = null;
while (offset + 512 <= archive.length) {
  const header = archive.subarray(offset, offset + 512);
  if (header.every((b) => b === 0)) break;
  const name = header.subarray(0, 100).toString("utf8").replace(/\0.*$/, "");
  const sizeRaw = header.subarray(124, 136).toString("utf8").replace(/\0.*$/, "").trim();
  const size = Number.parseInt(sizeRaw || "0", 8);
  const start = offset + 512;
  const end = start + size;
  if (name === "manifest.json") {
    manifest = JSON.parse(archive.subarray(start, end).toString("utf8"));
    break;
  }
  offset = start + Math.ceil(size / 512) * 512;
}
if (!manifest) throw new Error("manifest.json missing");
const hash = manifest.runtime_descriptor?.hash;
if (!hash) throw new Error("runtime_descriptor missing");
console.log(hash);
NODE
}

# The inline migration recorder that used to live here is GONE. It re-derived
# the apply body from `migrations/*.ts` with a copy of the build's own recorder
# call, which meant this harness could pass with a build that emitted nothing a
# creator could use. Stage 3 now posts the artifact the build committed, through
# `zeroship migrate`, which is the path a creator has.

cleanup(){
  echo ""
  echo "=== Cleanup ==="
  [ -f "$PIDFILE" ] && while read -r pid; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done < "$PIDFILE"
  wait 2>/dev/null || true
  if [ "${KEEP_WORK:-0}" = "1" ]; then
    echo "  KEEP_WORK=1 → $PGC/$RPC + $WORK preserved"
  else
    docker rm -f "$PGC" "$RPC" >/dev/null 2>&1 || true
    rm -rf "$WORK"
    echo "  stack down, $WORK cleaned"
  fi
}
trap cleanup EXIT

for p in $ZEROSHIP_CONTROL_PORT $ZEROSHIP_WORKER_PORT $ZEROSHIP_GATEWAY_PORT $ZEROSHIP_MIGRATE_SERVER_PORT; do
  lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
done

echo ""
echo "=== Stage 1: build migration-first db-hitcounter .zship ==="
(
  cd "$APP_EXAMPLE" &&
  pnpm typecheck &&
  pnpm build
) > "$WORK/appbuild.log" 2>&1
ZSHIP="$APP_EXAMPLE/dist/app.zship"
if [ -f "$ZSHIP" ]; then
  pass "built db-hitcounter app.zship ($(du -k "$ZSHIP" | cut -f1)KB)"
else
  fail "app build produced no .zship"
  tail -50 "$WORK/appbuild.log"
  exit 1
fi
RUNTIME_DESCRIPTOR_HASH="$(manifest_descriptor_hash "$ZSHIP" 2>"$WORK/manifest-check.err")"
if [ -n "$RUNTIME_DESCRIPTOR_HASH" ]; then
  pass "manifest carries runtime_descriptor hash $RUNTIME_DESCRIPTOR_HASH"
else
  fail "manifest runtime_descriptor missing"
  cat "$WORK/manifest-check.err"
  exit 1
fi

echo ""
echo "=== Stage 2: infra + platform migrations + billing seed + services ==="
# An infra wait that times out must say WHY. Measured 2026-08-09: this stage
# failed once with the single line "✗ PG" and exit 1, an immediate re-run
# passed, and nothing survived to diagnose the failure — both probes sent
# stdout AND stderr to /dev/null, the failure arm printed a two-character
# label, and cleanup then destroyed the container. Reproducing this same
# `docker run` by hand gave a healthy database in under 8 seconds, so the
# recipe is sound and the cause is still unknown. It is unknown BECAUSE the
# evidence was discarded, on precisely the run that had already gone wrong.
#
# This runs only on the failure path, so a green run costs nothing.
infra_diag() {  # <container> <probe-description> <probe...>
  local c="$1" what="$2"; shift 2
  echo "  --- diagnostics for $c ($what) ---"
  docker ps -a --filter "name=^${c}$" --format '  status: {{.Status}}  image: {{.Image}}' 2>&1 | sed 's/^/  /'
  echo "  --- last 20 log lines ---"
  docker logs --tail 20 "$c" 2>&1 | sed 's/^/  /'
  echo "  --- final probe, unredirected ---"
  "$@" 2>&1 | sed 's/^/  /'
  echo "  --- end diagnostics ---"
}

docker rm -f "$PGC" >/dev/null 2>&1 || true
docker run --name "$PGC" -d -p "$PG_PORT:5432" -e POSTGRES_PASSWORD=zeroship -e POSTGRES_USER=postgres -e POSTGRES_DB=zeroship postgres:16 -c max_connections=200 >/dev/null || { fail "pg run"; exit 1; }
for _ in $(seq 1 40); do docker exec "$PGC" pg_isready -U postgres >/dev/null 2>&1 && break; sleep 1; done
docker exec "$PGC" pg_isready -U postgres >/dev/null 2>&1 && pass "ephemeral PG on :$PG_PORT" || {
  fail "PG did not become ready in 40s"
  infra_diag "$PGC" "pg_isready" docker exec "$PGC" pg_isready -U postgres
  exit 1
}

docker rm -f "$RPC" >/dev/null 2>&1 || true
docker run --name "$RPC" -d -p "$RP_PORT:$RP_PORT" docker.redpanda.com/redpandadata/redpanda:latest \
  redpanda start --overprovisioned --smp 1 --memory 512M --reserve-memory 0M --node-id 0 --check=false \
  --kafka-addr "external://0.0.0.0:$RP_PORT" --advertise-kafka-addr "external://127.0.0.1:$RP_PORT" \
  --set redpanda.auto_create_topics_enabled=true >/dev/null || { fail "redpanda run"; exit 1; }
for _ in $(seq 1 40); do docker exec "$RPC" rpk cluster health --exit-when-healthy >/dev/null 2>&1 && break; sleep 1.5; done
docker exec "$RPC" rpk cluster health --exit-when-healthy >/dev/null 2>&1 && pass "redpanda on $RP_BROKERS" || {
  fail "redpanda did not become healthy in 60s"
  infra_diag "$RPC" "rpk cluster health" docker exec "$RPC" rpk cluster health --exit-when-healthy
  exit 1
}

MIG_LOG="$WORK/platform-migrate.log"
# Post-extraction: platform schema is applied by zeroship-platform-migrate
# (adapter, platform-cli). It authors via its own built-in V8 (no recorder child)
# and drives the published zero-migrate engine over CompioPgSession.
zs_platform_migrate "$DBURL" \
  --migrations-dir "$ROOT/db/migrations-ts" \
  --project-schema zeroship --project-id zeroship > "$MIG_LOG" 2>&1 \
  && pass "zeroship platform migrations applied" || { fail "platform migrate"; tail -30 "$MIG_LOG"; exit 1; }

psql_exec >/dev/null 2>&1 <<SQL && pass "seeded plan + metric pricing for requests, db_reads, db_writes, and platform counters" || { fail "billing seed"; exit 1; }
INSERT INTO zeroship.plans (id,name,base_fee_cents,included_units,fx_pico_cents_per_unit,runtime_limits_json,spend_limit_default_cents)
VALUES ('$PLAN_ID','db-app-e2e',0,0,1000000000000,'{"cpu_limit_ms":5000,"wall_timeout_ms":30000,"heap_limit_mb":256}',100000000)
ON CONFLICT (id) DO NOTHING;
INSERT INTO zeroship.pricing_config (id,fx_pico_cents_per_unit)
VALUES ('global',1000000000000)
ON CONFLICT (id) DO UPDATE SET fx_pico_cents_per_unit=EXCLUDED.fx_pico_cents_per_unit;
INSERT INTO zeroship.billing_metrics (metric,kind,unit) VALUES
  ('requests','platform','op'),
  ('cpu_us','platform','us'),
  ('wall_us','platform','us'),
  ('ingress_bytes','platform','byte'),
  ('egress_bytes','platform','byte'),
  ('db_reads','primitive','op'),
  ('db_writes','primitive','op'),
  ('db_rows_written','primitive','row')
ON CONFLICT (metric) DO UPDATE SET kind=EXCLUDED.kind, unit=EXCLUDED.unit;
INSERT INTO zeroship.metric_weights (metric,units_per_op,per_units) VALUES
  ('requests',1,1),
  ('cpu_us',0,1),
  ('wall_us',0,1),
  ('ingress_bytes',0,1),
  ('egress_bytes',0,1),
  ('db_reads',1,1),
  ('db_writes',1,1),
  ('db_rows_written',0,1)
ON CONFLICT (metric) DO UPDATE SET units_per_op=EXCLUDED.units_per_op, per_units=EXCLUDED.per_units;
SQL

openssl genpkey -algorithm ed25519 -out "$WORK/sk.pem" 2>/dev/null
chmod 600 "$WORK/sk.pem"
openssl rand -base64 48 > "$WORK/gate-broker-secret"
chmod 600 "$WORK/gate-broker-secret"
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
curl -sf "$CONTROL_URL/readyz" >/dev/null 2>&1 && pass "control healthy" || { fail "control"; tail -40 "$WORK/control.log"; exit 1; }

"$BIN/zeroship-migrate-server" --port "$ZEROSHIP_MIGRATE_SERVER_PORT" \
  --mutation-rate-limit-burst 3 \
  --tmp-dir "$WORK/migrated-tmp" > "$WORK/migrated.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$MIGRATE_SERVER_URL/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$MIGRATE_SERVER_URL/readyz" >/dev/null 2>&1 && pass "zeroship-migrate-server healthy" || { fail "migrated"; tail -40 "$WORK/migrated.log"; exit 1; }

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
curl -sf "http://localhost:$ZEROSHIP_WORKER_PORT/readyz" >/dev/null 2>&1 && pass "worker healthy" || { fail "worker"; tail -40 "$WORK/worker.log"; exit 1; }
e2e_assert_usage_producer "$WORK/worker.log" "worker"

"$BIN/zeroship-gate" --port "$ZEROSHIP_GATEWAY_PORT" --control-url "$CONTROL_URL" \
  --metering-outbox-wal-path "$WORK/gate-outbox.redb" \
  --config "$CFG_TOML" --worker-urls "http://localhost:$ZEROSHIP_WORKER_PORT" --blob-store "$WORK/blobs" --blob-cache-disk-root "$WORK/blob-cache" \
 --poll-interval 2 --signing-key-file "$WORK/sk.pem" --broker-secret-file "$WORK/gate-broker-secret" > "$WORK/gate.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$ZEROSHIP_GATEWAY_PORT/readyz" >/dev/null 2>&1 && pass "gateway healthy" || { fail "gateway"; tail -40 "$WORK/gate.log"; exit 1; }
e2e_assert_usage_producer "$WORK/gate.log" "gateway"

echo ""
echo "=== Stage 3: bearer + creator + app + deploy + migrated apply ==="
# The scope string is the action list the deleted permission_tokens policy
# carried, one scope per Cedar action: control turns `scope` into the token
# policy and intersects it with the owner's own authority.
SCOPE="apps:read apps:write apps:deploy billing:read billing:write"
CREATOR="$(node -e 'console.log(require("crypto").randomUUID())')"
psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id,email,name,email_verified_at) VALUES ('$CREATOR','e2e-dbapp-$CREATOR@zeroship.test'::citext,'E2E DB App',NOW());
SQL
ADMIN_TOKEN="$(e2e_mint_platform_bearer "$CREATOR" "$SCOPE")"
[ "$(echo -n "$ADMIN_TOKEN" | awk -F. '{print NF}')" = "3" ] && pass "minted platform bearer (creator=$CREATOR)" || { fail "bearer mint"; exit 1; }

APP="$(curl -s -X POST "$CONTROL_URL/api/apps" -H 'Content-Type: application/json' -H "Authorization: Bearer $ADMIN_TOKEN" -d "{\"name\":\"db-hitcounter\",\"plan_id\":\"$PLAN_ID\"}" | jget '.id')"
[ -n "$APP" ] && pass "created app db-hitcounter ($APP)" || { fail "create app"; exit 1; }
psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.app_members (app_id,user_id,role) VALUES ('$APP','$CREATOR','owner') ON CONFLICT (app_id,user_id) DO UPDATE SET role='owner';
SQL

"$BIN/zeroship" deploy "$ZSHIP" --app="$APP" --control="$CONTROL_URL" --token="$ADMIN_TOKEN" 2>&1 | tee "$WORK/deploy.log" | grep -q deploy_hash \
  && pass "deployed db-hitcounter .zship" || { fail "deploy"; cat "$WORK/deploy.log"; exit 1; }

# --- THE PRE-MIGRATE CONTROL ------------------------------------------------
#
# The app is deployed and serving, and its database schema does not exist yet.
# That is the state a creator reaches by following the documented chain up to
# `zeroship deploy`, and this arm pins the thing that is missing: the per-app
# role. Every `env.db` call opens a transaction and issues
# `SET LOCAL ROLE "app_<id>_role"` (crates/zeroship-data-engine/src/auth/bootstrap.rs), so
# an absent role fails the first database call and reaches the end user as an
# opaque `internal error`.
#
# READ FROM pg_roles, NOT BY CALLING THE APP. The obvious control -- send one
# request and watch it fail -- was written first and MEASURED to break stage 5:
# with it, `requests` came back 32 against 33 sent, because a dispatch that
# fails this way did not produce a `requests` row. That is its own question and
# this harness is not the place to answer it; what matters here is that the
# control must not perturb the counters stage 5 asserts exactly. Reading the
# catalog costs no request and states the fact more directly than a 500 does.
#
# It is a CONTROL, not a spec: it pins that stage 4's green is caused by the
# migrate step. Without it, a `zeroship migrate` that silently did nothing would
# leave every later assertion passing.
APP_ROLE="app_${APP}_role"
ROLE_BEFORE="$(psql_exec -tA -c "SELECT count(*) FROM pg_roles WHERE rolname='$APP_ROLE'" 2>/dev/null | tr -d '[:space:]')"
[ "$ROLE_BEFORE" = "0" ] \
  && pass "the per-app role $APP_ROLE does NOT exist on the deployed, unmigrated app" \
  || fail "$APP_ROLE already existed before migrate (count=$ROLE_BEFORE) - the control proves nothing"

# Create the database only after the absent-database control above has observed
# the precondition. Create, first apply, and the immediate idempotent re-apply
# consume three mutation tokens, so this harness raises only its test-local burst
# to three at server startup.
CREATE_CODE="$(curl -sS -o "$WORK/create-database-response.json" -w '%{http_code}' \
  -X POST "$MIGRATE_SERVER_URL/v1/databases/$APP" \
  -H "Authorization: Bearer $ADMIN_TOKEN")"
if [[ "$CREATE_CODE" != 2?? ]]; then
  fail "database create failed (http=$CREATE_CODE): $(cat "$WORK/create-database-response.json")"
  tail -50 "$WORK/migrated.log"; exit 1
fi

# --- APPLY THROUGH THE CLI, DIRECT TO MIGRATE-SERVER ------------------------
#
# This harness has no edge process, so its `--control` value is the migration
# service's loopback URL. Production uses the public control URL and the edge
# selects this same upstream. The committed build artifact, app id, and bearer
# are otherwise the creator path: no extra scope and no operator credential.
#
# The body is the file the BUILD wrote (`generated/zeroship/migrations.ir.json`),
# not one this harness records inline. That is deliberate: it makes the
# committed artifact load-bearing, so a build that stops emitting it fails here
# rather than in a creator's terminal.
IR_FILE="$APP_EXAMPLE/generated/zeroship/migrations.ir.json"
[ -f "$IR_FILE" ] \
  && pass "the build emitted $(basename "$IR_FILE")" \
  || { fail "missing $IR_FILE - run pnpm gen-types"; exit 1; }

MIGRATE_OUT="$("$BIN/zeroship" migrate "$IR_FILE" --app="$APP" --control="$MIGRATE_SERVER_URL" --token="$ADMIN_TOKEN" 2>&1)"
MIGRATE_RC=$?
echo "$MIGRATE_OUT" > "$WORK/migrate.log"
if [ "$MIGRATE_RC" = "0" ] && grep -qE 'Applied [1-9][0-9]* migration op' <<<"$MIGRATE_OUT"; then
  pass "zeroship migrate applied the app's migrations through migrate-server ($(head -c 80 <<<"$MIGRATE_OUT"))"
else
  fail "zeroship migrate failed (rc=$MIGRATE_RC): $MIGRATE_OUT"
  tail -50 "$WORK/migrated.log"
  tail -20 "$WORK/control.log"
  exit 1
fi

# Re-running must be a no-op, not a second apply. A creator runs `deploy` then
# `migrate` on every push; if the second run re-applied, every push after the
# first would fail on an already-existing table.
MIGRATE_AGAIN="$("$BIN/zeroship" migrate "$IR_FILE" --app="$APP" --control="$MIGRATE_SERVER_URL" --token="$ADMIN_TOKEN" 2>&1)"
grep -qE 'Applied 0 migration op' <<<"$MIGRATE_AGAIN" \
  && pass "a second zeroship migrate is a no-op (idempotent)" \
  || fail "re-running zeroship migrate was not a no-op: $MIGRATE_AGAIN"

# The other half of the control: the role the apply path is the sole producer of
# now exists. Paired with the count of 0 above, this is one variable changed --
# the migrate step -- and one observation changed.
ROLE_AFTER="$(psql_exec -tA -c "SELECT count(*) FROM pg_roles WHERE rolname='$APP_ROLE'" 2>/dev/null | tr -d '[:space:]')"
[ "$ROLE_AFTER" = "1" ] \
  && pass "$APP_ROLE exists after zeroship migrate (0 -> 1 across the one step)" \
  || fail "$APP_ROLE still missing after migrate (count=$ROLE_AFTER)"
sleep 5

echo ""
echo "=== Stage 4: deployed env.db CRUD through gateway ==="
READY=0
READY_RESP=""
# COUNTED, because stage 5 bills every one of these. The readiness loop is the
# only variable-length source of app requests in this harness -- everything else
# is exactly N_REQ -- so without this counter the expected `requests` total is
# unknowable and the only assertable thing is a floor. It is also why the label
# below used to be wrong: READY_RESP holds the LAST attempt, and calling it
# "first" hid every retry that ever happened here.
READY_TRIES=0
for _ in $(seq 1 30); do
  READY_TRIES=$((READY_TRIES+1))
  READY_RESP="$(gw "http://localhost:$ZEROSHIP_GATEWAY_PORT/hit/ready")"
  WROTE="$(printf '%s' "$READY_RESP" | jget '.wrote')"
  READBACK="$(printf '%s' "$READY_RESP" | jget '.readBack')"
  if [ "$WROTE" = "true" ] && [ -n "$READBACK" ] && [ "$READBACK" -gt 0 ] 2>/dev/null; then
    READY=1
    break
  fi
  sleep 1
done
APP_REQUESTS=$((READY_TRIES + N_REQ))
echo "    env.db response on readiness attempt $READY_TRIES of 30: $READY_RESP"
[ "$READY" = "1" ] && pass "probe response has wrote=true and readBack=$READBACK" || { fail "env.db probe did not succeed"; tail -50 "$WORK/worker.log"; exit 1; }

OK=0
LAST_RESP=""
for i in $(seq 1 "$N_REQ"); do
  LAST_RESP="$(gw "http://localhost:$ZEROSHIP_GATEWAY_PORT/hit/$i")"
  WROTE="$(printf '%s' "$LAST_RESP" | jget '.wrote')"
  READBACK="$(printf '%s' "$LAST_RESP" | jget '.readBack')"
  if [ "$WROTE" = "true" ] && [ -n "$READBACK" ] && [ "$READBACK" -gt 0 ] 2>/dev/null; then
    OK=$((OK+1))
  else
    echo "    failed response[$i]: $LAST_RESP"
  fi
done
[ "$OK" = "$N_REQ" ] && pass "drove $OK/$N_REQ gateway requests with env.db insert+find success" || { fail "only $OK/$N_REQ env.db requests succeeded"; exit 1; }

# MUTATION CONTROL for the stage-5 equality below. Deleting one row makes the
# billing tables claim one more write than the database can account for -- the
# exact shape of over-counting, which is the direction the old floor could not
# see. It mutates the OBSERVABLE, not the product, so on its own it proves only
# that the ASSERTION discriminates.
#
# THE PRODUCT ARM IS PROVEN TOO, separately and by hand (2026-08-11, not
# automated here because it needs a rebuild). `crates/zeroship-data-engine/src/exec.rs`
# Postgres write path, `emit_db_metric(app_id, DB_WRITES, 1)` -> `2`, one line,
# then `cargo build --release -p zeroship-worker` and this harness unmutated:
#   db_writes=62 db_rows_written=31 rows=31   -> RED, "does not match Postgres"
# Reverted, rebuilt, re-run: 31/31/31, 21 passed 0 failed. So the equality does
# observe the runtime's own counter travelling worker -> metering -> outbox ->
# Redpanda -> control -> usage_aggregates, not merely this script's arithmetic.
# The OLD predicate on those same mutated numbers is 62>=30 -> GREEN: a
# platform billing every creator twice, reported as a pass.
#
# EXPECTATION, stated as the observable rather than a pass/fail total: the
# `db_writes and db_rows_written EQUAL the Postgres row count` line must go RED
# and name a rows= one lower than db_writes=. Measured 2026-08-11:
#   MUTATE=none        db_writes=31 db_rows_written=31 rows=31  -> pass
#   MUTATE=drop-a-row  db_writes=31 db_rows_written=31 rows=30  -> fail
if [ "${MUTATE:-none}" = "drop-a-row" ]; then
  psql_exec -tA -c "DELETE FROM \"$APP\".hits WHERE ctid IN (SELECT ctid FROM \"$APP\".hits LIMIT 1)" >/dev/null 2>&1
  echo "    MUTATED (drop-a-row): one row deleted from \"$APP\".hits AFTER the writes were metered"
fi

ROW_COUNT="$(psql_exec -tA -c "SELECT count(*)::bigint FROM \"$APP\".hits" 2>/dev/null | tr -d '[:space:]')"
echo "    Postgres row count in schema \"$APP\".hits = $ROW_COUNT"
[ -n "$ROW_COUNT" ] && [ "$ROW_COUNT" -ge "$N_REQ" ] 2>/dev/null && pass "per-app Postgres table contains env.db-created rows" || { fail "missing rows in per-app schema"; exit 1; }

echo ""
echo "=== Stage 5: db_reads/db_writes usage and projected charge ==="
REQ=0
DBR=0
DBW=0
DBROWS=0
pREQ=-1
pDBR=-1
pDBW=-1
pDBROWS=-1
for _ in $(seq 1 45); do
  REQ="$(usage_of requests)"
  DBR="$(usage_of db_reads)"
  DBW="$(usage_of db_writes)"
  DBROWS="$(usage_of db_rows_written)"
  {
    [ "$REQ" -ge "$APP_REQUESTS" ] &&
    [ "$DBR" -ge "$ROW_COUNT" ] &&
    [ "$DBW" -ge "$ROW_COUNT" ] &&
    [ "$REQ" = "$pREQ" ] &&
    [ "$DBR" = "$pDBR" ] &&
    [ "$DBW" = "$pDBW" ] &&
    [ "$DBROWS" = "$pDBROWS" ]
  } 2>/dev/null && break
  pREQ="$REQ"
  pDBR="$DBR"
  pDBW="$DBW"
  pDBROWS="$DBROWS"
  sleep 2
done
echo "    usage_aggregates: requests=$REQ db_reads=$DBR db_writes=$DBW db_rows_written=$DBROWS"
echo "    driver-side truth:  app requests=$APP_REQUESTS ($READY_TRIES readiness + $N_REQ), Postgres rows=$ROW_COUNT"

# EVERY metric that reached the table, not only the four this stage names.
# The billing seed above declares eight, five of them platform counters, and
# nothing here had ever looked at whether the other four ARRIVE. A counter that
# never lands cannot be billed no matter what the plan prices it at, and the
# seed deliberately weights cpu_us/wall_us/ingress_bytes/egress_bytes at 0
# units_per_op to keep the charge arithmetic simple -- which means a zero
# charge is exactly what a missing counter also produces.
echo "    all metrics that landed for this app:"
psql_exec -tA -F' ' -c "SELECT metric, SUM(total) FROM zeroship.usage_aggregates WHERE app_id='$APP' GROUP BY metric ORDER BY metric" 2>/dev/null | sed 's/^/      /'

# A COUNTER THAT STOPS ARRIVING IS INVISIBLE HERE, which is why this exists.
# The seed weights cpu_us/wall_us/egress_bytes at 0 units_per_op to keep the
# charge arithmetic simple, so a vanished counter moves neither the charge nor
# any assertion above it. In production those weights are not zero.
#
# ingress_bytes is EXCLUDED, and this is measured rather than assumed. It is
# request BODY bytes (`crates/zeroship-worker/src/handler.rs:336`,
# `ingress_bytes = request_body.len()`), this harness drives only bodyless
# GETs, and `crates/zeroship-metering/src/meter.rs` `push_metric` skips a zero value, so
# no row is ever written. Absent is CORRECT here, not a defect -- I checked
# before filing it as one. `tests/e2e_metering_billing.sh:445` already asserts
# it present and non-zero on traffic that carries a body.
#
# MEASURED 2026-08-11, one run: cpu_us=15090 wall_us=5175687 egress_bytes=3129
# requests=32, and no ingress_bytes row at all.
PLATFORM_MISSING=""
for m in requests cpu_us wall_us egress_bytes; do
  v="$(usage_of "$m")"
  { [ -n "$v" ] && [ "$v" -gt 0 ]; } 2>/dev/null || PLATFORM_MISSING="$PLATFORM_MISSING $m"
done
[ -z "$PLATFORM_MISSING" ] \
  && pass "all four body-independent platform counters reached usage_aggregates (requests, cpu_us, wall_us, egress_bytes)" \
  || { fail "platform counters missing or zero:$PLATFORM_MISSING"; tail -50 "$WORK/control.log"; exit 1; }

# TWO-SIDED, AND AGAINST AN INDEPENDENT SOURCE. What stood here was
# `DBR >= N_REQ && DBW >= N_REQ`: a floor, and one whose ceiling nothing
# supplied. A worker that counted every db op TWICE satisfied it, and so did
# the projected-charge check below, because that charge is computed FROM these
# same numbers. For a billing signal that is the wrong direction to be blind
# in -- under-counting costs the platform, OVER-counting bills creators for
# traffic they never generated, and only the second was silent.
#
# `ROW_COUNT` is the discriminator: it comes from Postgres, not from
# usage_aggregates, so equality is a claim ACROSS the seam rather than within
# one table. Every row in "$APP".hits was created by exactly one metered
# insert, and per the metering contract the primitives emit in the SUCCESS ARM
# ONLY, so a failed insert adds neither a row nor a count. If that equality
# ever breaks, that IS the finding.
#
# MEASURED 2026-08-11 at HEAD, one full run: requests=32 db_reads=31
# db_writes=31 db_rows_written=31, rows=31, N_REQ=30. The +1 on requests is
# the readiness attempt; the harness now counts those instead of leaving the
# expected total unknowable, which is what forced the floor in the first place.
{ [ "$DBW" = "$ROW_COUNT" ] && [ "$DBROWS" = "$ROW_COUNT" ]; } 2>/dev/null \
  && pass "db_writes and db_rows_written EQUAL the Postgres row count ($ROW_COUNT) -- not over-counted, not under-counted" \
  || { fail "write metering does not match Postgres: db_writes=$DBW db_rows_written=$DBROWS rows=$ROW_COUNT"; tail -50 "$WORK/control.log"; exit 1; }

# db_reads gets a BOUND, not an equality, and the asymmetry is deliberate. A
# request can fail AFTER its insert -- readBack=0 is a find that ran and
# returned nothing -- so a read without a row is legitimate and would make an
# equality flake. The bound still refuses both directions that matter: fewer
# reads than rows means reads went unmetered, more reads than requests means
# reads were counted that no request made.
{ [ "$DBR" -ge "$ROW_COUNT" ] && [ "$DBR" -le "$APP_REQUESTS" ]; } 2>/dev/null \
  && pass "db_reads=$DBR lies within [rows=$ROW_COUNT, app requests=$APP_REQUESTS]" \
  || { fail "db_reads=$DBR outside [$ROW_COUNT, $APP_REQUESTS]"; tail -50 "$WORK/control.log"; exit 1; }

# The one counter the harness knows exactly, because it made every request.
[ "$REQ" = "$APP_REQUESTS" ] 2>/dev/null \
  && pass "requests=$REQ EQUALS the $APP_REQUESTS requests this harness sent" \
  || { fail "requests=$REQ but this harness sent $APP_REQUESTS ($READY_TRIES readiness + $N_REQ)"; tail -50 "$WORK/control.log"; exit 1; }

# NOT AN AMOUNT CHECK, and the pass line says so. EXPECTED_CHARGE is computed
# from the very rows this queries, so it can only establish that db_reads and
# db_writes are in the PRICED SET and that the pricing arithmetic agrees with
# itself. The amount question is answered above, against Postgres.
EXPECTED_CHARGE=$(( REQ + DBR + DBW ))
CHARGE=""
for _ in $(seq 1 20); do
  CHARGE="$(curl -s "$CONTROL_URL/api/apps/$APP/projected-charge" -H "Authorization: Bearer $ADMIN_TOKEN" | jget '.projected_charge_cents')"
  [ -n "$CHARGE" ] && [ "$CHARGE" = "$EXPECTED_CHARGE" ] 2>/dev/null && break
  sleep 2
done
echo "    projected charge = $CHARGE cents ; expected requests + db_reads + db_writes = $REQ + $DBR + $DBW = $EXPECTED_CHARGE"
[ "$CHARGE" = "$EXPECTED_CHARGE" ] 2>/dev/null \
  && pass "projected charge prices db_reads/db_writes (consistency within the billing tables, NOT an amount check)" \
  || { fail "projected charge did not include DB metrics as priced"; exit 1; }

echo ""
echo "============================================"
echo "  Results: $PASS passed, $FAIL failed"
echo "  Evidence: wrote=true readBack=$READBACK rows=$ROW_COUNT db_reads=$DBR db_writes=$DBW projected_charge_cents=$CHARGE"
echo "============================================"
[ $FAIL -eq 0 ] && exit 0 || exit 1
