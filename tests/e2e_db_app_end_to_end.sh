#!/usr/bin/env bash
# ============================================================================
# e2e_db_app_end_to_end.sh — migration-first creator app DB path, end to end.
#
# Proves the real deployed app path for env.db:
#   1. committed op.* migrations -> committed generated descriptor artifacts
#   2. vite builds a .zship whose manifest carries runtime_descriptor
#   3. zeroship deploy uploads that .zship
#   4. zeroship-migrated applies recorded IR to the app's Postgres schema
#   5. gateway -> worker -> env.db insert/find succeeds on the deployed app
#   6. db_reads/db_writes reach usage_aggregates and are included in charge
#
# Skips cleanly when docker is unavailable. KEEP_WORK=1 preserves logs/containers.
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"
PASS=0
FAIL=0
pass(){ PASS=$((PASS+1)); echo "  ✓ $1"; }
fail(){ FAIL=$((FAIL+1)); echo "  ✗ $1"; }

echo "============================================"
echo "  zeroship E2E — deployed migration-first env.db app"
echo "============================================"

if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  echo "  ⚠ SKIP: docker unavailable."
  exit 0
fi

for b in zeroship zeroship-control zeroship-gate zeroship-worker zeroship-platform-migrate zeroship-migrated; do
  [ -x "$BIN/$b" ] || { echo "missing $BIN/$b — run cargo build --release, then cargo build --release -p zeroship-migrate-adapter --features platform-cli --bin zeroship-platform-migrate"; exit 2; }
done
command -v node >/dev/null && command -v openssl >/dev/null && command -v curl >/dev/null && command -v pnpm >/dev/null || {
  echo "need node/openssl/curl/pnpm"
  exit 2
}

APP_EXAMPLE="$ROOT/examples/db-hitcounter"
[ -d "$APP_EXAMPLE" ] || { echo "missing examples/db-hitcounter"; exit 2; }
JOSE="$ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"
[ -f "$JOSE" ] || { echo "missing jose"; exit 2; }

CONTROL_PORT=9186
WORKER_PORT=8086
GATE_PORT=8076
MIGRATED_PORT=9086
PG_PORT=5486
RP_PORT=19186
PGC=zs-e2e-dbapp-pg
RPC=zs-e2e-dbapp-redpanda
DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"
CONTROL_URL="http://localhost:$CONTROL_PORT"
MIGRATED_URL="http://localhost:$MIGRATED_PORT"
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

write_apply_request(){
  node --input-type=module - "$ROOT/sdks/vite-plugin/dist/gen-types/recorder.js" "$APP_EXAMPLE/migrations" > "$WORK/apply-migrations.json" <<'NODE'
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

for p in $CONTROL_PORT $WORKER_PORT $GATE_PORT $MIGRATED_PORT; do
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
"$BIN/zeroship-platform-migrate" \
  --database-url "$DBURL" --migrations-dir "$ROOT/db/migrations-ts" \
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
printf '[metering]\nredpanda_brokers = "%s"\nusage_events_topic = "%s"\n' "$RP_BROKERS" "$USAGE_TOPIC" > "$CFG_TOML"

"$BIN/zeroship-control" --port "$CONTROL_PORT" --db "$DBURL" --config "$CFG_TOML" \
  --blob-store "$WORK/blobs" --signing-key-file "$WORK/sk.pem" \
  --stripe-base-url "http://127.0.0.1:1" --stripe-secret-key "sk_test_unused" \
  --meter-provider lite --invoicer-provider lite --allow-unsupported-billing \
  --spend-recompute-interval 2 --dev-insecure > "$WORK/control.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$CONTROL_URL/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$CONTROL_URL/health" >/dev/null 2>&1 && pass "control healthy" || { fail "control"; tail -40 "$WORK/control.log"; exit 1; }

"$BIN/zeroship-migrated" --port "$MIGRATED_PORT" --db "$DBURL" --provision-db "$DBURL" \
  --signing-key-file "$WORK/sk.pem" --tmp-dir "$WORK/migrated-tmp" --dev-insecure > "$WORK/migrated.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$MIGRATED_URL/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$MIGRATED_URL/health" >/dev/null 2>&1 && pass "zeroship-migrated healthy" || { fail "migrated"; tail -40 "$WORK/migrated.log"; exit 1; }

USAGE_OUTBOX_WAL_PATH="$WORK/worker-outbox.redb" "$BIN/zeroship-worker" --port "$WORKER_PORT" --worker-threads 2 \
  --config "$CFG_TOML" --control "$CONTROL_URL" --db "$DBURL" --blob-store "$WORK/blobs" --poll-interval 2 --dev-insecure > "$WORK/worker.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$WORKER_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$WORKER_PORT/health" >/dev/null 2>&1 && pass "worker healthy" || { fail "worker"; tail -40 "$WORK/worker.log"; exit 1; }

USAGE_OUTBOX_WAL_PATH="$WORK/gate-outbox.redb" "$BIN/zeroship-gate" --port "$GATE_PORT" --control "$CONTROL_URL" \
  --config "$CFG_TOML" --workers "http://localhost:$WORKER_PORT" --blob-store "$WORK/blobs" --blob-cache-disk-root "$WORK/blob-cache" \
  --db "$DBURL" --poll-interval 2 --signing-key-file "$WORK/sk.pem" --gateway-broker-secret-file "$WORK/gate-broker-secret" --dev-insecure > "$WORK/gate.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$GATE_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$GATE_PORT/health" >/dev/null 2>&1 && pass "gateway healthy" || { fail "gateway"; tail -40 "$WORK/gate.log"; exit 1; }

echo ""
echo "=== Stage 3: PAT + creator + app + deploy + migrated apply ==="
POLICY_JSON='{"name":"e2e-dbapp","statements":[{"effect":"allow","actions":["apps:read","apps:write","apps:deploy","billing:read","billing:write"],"resources":[{"type":"any"}],"conditions":[]}]}'
POLICY_HASH="$(node -e 'const{createHash}=require("crypto");function c(v){if(v===null||typeof v==="number"||typeof v==="boolean")return JSON.stringify(v);if(typeof v==="string")return JSON.stringify(v);if(Array.isArray(v))return "["+v.map(c).join(",")+"]";return "{"+Object.keys(v).sort().map(k=>JSON.stringify(k)+":"+c(v[k])).join(",")+"}";}process.stdout.write(createHash("sha256").update(c(JSON.parse(process.argv[1]))).digest("hex"));' "$POLICY_JSON")"
CREATOR="$(node -e 'console.log(require("crypto").randomUUID())')"
TOKID="$(node -e 'console.log(require("crypto").randomUUID())')"
EXP=$(( $(date +%s) + 86400 ))
psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id,email,name,email_verified_at) VALUES ('$CREATOR','e2e-dbapp-$CREATOR@zeroship.test'::citext,'E2E DB App',NOW());
INSERT INTO zeroship.platform_admin_roles (user_id,role,granted_by) VALUES ('$CREATOR','admin','$CREATOR');
INSERT INTO zeroship.permission_tokens (id,owner_id,kind,name,policies,policy_hash,expires_at) VALUES ('$TOKID','$CREATOR','pat','e2e dbapp','$POLICY_JSON'::jsonb,'$POLICY_HASH',to_timestamp($EXP));
SQL
PAT="$(node --input-type=module -e 'import{readFileSync}from "node:fs";import{createHash,randomBytes}from "node:crypto";import{importPKCS8,exportJWK,SignJWT}from "file://'"$JOSE"'";const[pem,owner,tid,phash,exp]=process.argv.slice(1);const key=await importPKCS8(readFileSync(pem,"utf8"),"EdDSA",{extractable:true});const x=(await exportJWK(key)).x;const kid=createHash("sha256").update(`{"crv":"Ed25519","kty":"OKP","x":"${x}"}`).digest("base64url");const jwt=await new SignJWT({sub:owner,owner,tid,jti:tid,scope:"pat",policy_hash:phash,nonce:randomBytes(32).toString("base64url")}).setProtectedHeader({alg:"EdDSA",typ:"pat+jwt",kid}).setIssuer("https://api.zeroship.ai").setAudience("control.zeroship.ai").setIssuedAt(Math.floor(Date.now()/1000)).setExpirationTime(Number(exp)).sign(key);process.stdout.write(jwt);' "$WORK/sk.pem" "$CREATOR" "$TOKID" "$POLICY_HASH" "$EXP")"
[ "$(echo -n "$PAT" | awk -F. '{print NF}')" = "3" ] && pass "minted PAT (creator=$CREATOR)" || { fail "PAT"; exit 1; }

APP="$(curl -s -X POST "$CONTROL_URL/api/apps" -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" -d "{\"name\":\"db-hitcounter\",\"plan_id\":\"$PLAN_ID\"}" | jget '.id')"
[ -n "$APP" ] && pass "created app db-hitcounter ($APP)" || { fail "create app"; exit 1; }
psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.app_members (app_id,user_id,role) VALUES ('$APP','$CREATOR','owner') ON CONFLICT (app_id,user_id) DO UPDATE SET role='owner';
SQL

"$BIN/zeroship" deploy "$ZSHIP" --app="$APP" --control="$CONTROL_URL" --token="$PAT" 2>&1 | tee "$WORK/deploy.log" | grep -q deploy_hash \
  && pass "deployed db-hitcounter .zship" || { fail "deploy"; cat "$WORK/deploy.log"; exit 1; }

write_apply_request || { fail "record migration IR request"; exit 1; }
APPLY_CODE="$(curl -s -o "$WORK/apply-response.json" -w '%{http_code}' -X POST "$MIGRATED_URL/v1/apps/$APP/migrations/apply" \
  -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" --data-binary @"$WORK/apply-migrations.json")"
APPLIED="$(cat "$WORK/apply-response.json" | jget '.applied.length')"
SKIPPED="$(cat "$WORK/apply-response.json" | jget '.skipped.length')"
if [ "$APPLY_CODE" = "200" ] && [ -n "$APPLIED" ] && [ "$APPLIED" -ge 1 ] 2>/dev/null; then
  pass "zeroship-migrated applied app IR migrations (applied=$APPLIED skipped=${SKIPPED:-0})"
else
  fail "migrated apply failed (http=$APPLY_CODE)"
  cat "$WORK/apply-response.json"
  tail -50 "$WORK/migrated.log"
  exit 1
fi
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
  READY_RESP="$(gw "http://localhost:$GATE_PORT/hit/ready")"
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
  LAST_RESP="$(gw "http://localhost:$GATE_PORT/hit/$i")"
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
# automated here because it needs a rebuild). `crates/plugin-db/src/exec.rs`
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
  CHARGE="$(curl -s "$CONTROL_URL/api/apps/$APP/projected-charge" -H "Authorization: Bearer $PAT" | jget '.projected_charge_cents')"
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
