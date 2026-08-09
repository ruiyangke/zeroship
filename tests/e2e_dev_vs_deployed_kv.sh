#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Dev vs deployed: run ONE identical operation sequence against `pnpm dev` and
# against the same app deployed behind the gateway, then diff the RESULTS.
#
# This is the only test in the repo that compares the two sides. Every other
# e2e script proves one side answers; none proves both answer the SAME, and
# that gap is where this project's seam bugs have lived (LocalFs dropping
# contentType while S3 kept it; the SQLite dev tier diverging from Postgres).
#
# What it caught on first run (2026-08-09, docs/pilot/e2e-scenarios.md):
#   - examples/kv-dashboard had no src/server/config.ts, so all 16 procedures
#     resolved to `auth: user` and the gateway refused every one. The example
#     was green in dev and completely unreachable deployed.
#   - `kv.keys.list` ORDER differs by backend: redb iterates sorted, Redis SCAN
#     does not, and docs/reference/kv.md specifies neither.
#
# Prereqs (docs/runbooks/local-dev.md):
#   cargo build --release -p zeroship-control -p zeroship-worker -p zeroship-gateway -p zeroship --bins
#   cargo build --release -p zeroship-migrate-adapter --features platform-cli --bin zeroship-platform-migrate
#   docker (Postgres on :5440 as compose-postgres-1, plus an ephemeral Redis)
#   pnpm install in examples/kv-dashboard
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"
APP="$ROOT/examples/kv-dashboard"
ZSHIP="$APP/dist/app.zship"
WORK="$(mktemp -d)"

PG_CONTAINER="${PG_CONTAINER:-compose-postgres-1}"
PG_USER="${PG_USER:-postgres}"
PG_DB="${PG_DB:-zeroship_devdeploy}"
DB_URL="${DATABASE_URL:-postgres://postgres:zeroship@localhost:5440/$PG_DB}"
# Distinct from golden_path.sh (9390/8390/8300) so both can run at once.
CONTROL_PORT="${CONTROL_PORT:-9392}"
WORKER_PORT="${WORKER_PORT:-8392}"
GATE_PORT="${GATE_PORT:-8302}"
DEV_PORT="${DEV_PORT:-3011}"
REDIS_PORT="${REDIS_PORT:-6396}"
REDIS_CONTAINER="zs-devdeploy-redis"
CONTROL_KEY="dd-ck"; MASTER_KEY="dd-mk"
export ZEROSHIP_DEV_INSECURE=1
export WORKER_KEY="${WORKER_KEY:-devdeploy-worker-key-0123456789abcd}"
APP_NAME="kvdash"

PASS=0; FAIL=0; PIDS=()
pass() { PASS=$((PASS+1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  lsof -ti :"$DEV_PORT" 2>/dev/null | xargs -r kill 2>/dev/null || true
  docker rm -f "$REDIS_CONTAINER" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "=== dev vs deployed (kv-dashboard) ==="

# --- the probe: one deterministic sequence, printed as RESULTS ---
# Volatile fields are blanked so two runs are comparable. Anything NOT blanked
# here is being asserted identical across redb and Redis.
probe() {
  local url="$1" hdr="${2:-}" rpc="$1/__zeroship/v1"
  local h=(); [ -n "$hdr" ] && h=(-H "$hdr")
  call() {
    curl -sS -m 20 -X POST -H 'content-type: application/json' "${h[@]}" \
      "$rpc/$1" -d "{\"json\":${2:-{\}}}" 2>&1 | sed -E \
      -e 's/"(resetMs|ttlMs|expiresAt|generatedAt|builtAt|createdAt|acquiredAt)":[0-9-]+/"\1":<V>/g' \
      -e 's/"(nonce|token|leaseId)":"[^"]*"/"\1":"<V>"/g' \
      -e 's/"price":[0-9.]+/"price":<V>/g' \
      -e 's/kv-demo:session:[a-z0-9-]+/kv-demo:session:<V>/g'
  }
  call kv.clear >/dev/null                       # start from a known state
  echo "visit1   $(call kv.visit)"
  echo "visit2   $(call kv.visit)"
  for i in 1 2 3 4 5 6; do echo "rate$i    $(call kv.rate.hit '{"actor":"probe"}')"; done
  echo "cache1   $(call kv.cache.quote '{"sku":"probe-sku"}')"
  echo "cache2   $(call kv.cache.quote '{"sku":"probe-sku"}')"
  echo "memo1    $(call kv.memo.get '{"label":"probe-memo"}')"
  echo "memo2    $(call kv.memo.get '{"label":"probe-memo"}')"
  echo "lease1   $(call kv.lease.acquire '{"owner":"owner-a"}')"
  echo "lease2   $(call kv.lease.acquire '{"owner":"owner-b"}')"
  echo "leaseC   $(call kv.lease.clear)"
  echo "strSet   $(call kv.string.set '{"value":"hello probe","ttlMs":60000}')"
  echo "strExp   $(call kv.string.expire '{"ttlMs":120000}')"
  echo "strPer   $(call kv.string.persist)"
  echo "strDel   $(call kv.string.delete)"
  # keys.list is compared as a SET, not a sequence: redb iterates sorted and
  # Redis SCAN does not, and docs/reference/kv.md specifies neither order, so
  # asserting sequence would fail on an unspecified property rather than a
  # defect. The SET still has to match, which is the part the contract implies.
  # When kv.md states an order, delete the sort and compare verbatim -- see
  # docs/pilot/e2e-scenarios.md, scenario 11 KV leg.
  echo "keys     $(call kv.keys.list '{"prefix":"kv-demo:","limit":50}' \
    | tr ',' '\n' | sort | tr '\n' ',')"
}

# --- 1. real build ---
( cd "$APP" && pnpm build ) > "$WORK/build.log" 2>&1
[ -f "$ZSHIP" ] && pass "built app.zship ($(du -k "$ZSHIP" | cut -f1)KB)" \
  || { fail "build produced no app.zship"; tail -20 "$WORK/build.log"; exit 1; }

# Every RPC resource must declare an auth posture. Without it the procedure
# resolves to `auth: user` and the gateway refuses it -- green in dev, 401
# deployed. That is exactly how kv-dashboard shipped.
d="$WORK/unpack"; mkdir -p "$d"; tar -xf "$ZSHIP" -C "$d"
total=$(grep -oE '"rpc:[^"]+":' "$d/manifest.json" | wc -l)
authed=$(grep -oE '"rpc:[^"]+":\{[^}]*"auth":' "$d/manifest.json" | wc -l)
[ "$total" -gt 0 ] && [ "$authed" -eq "$total" ] \
  && pass "all $total rpc resources declare an auth posture" \
  || fail "only $authed of $total rpc resources declare auth (missing src/server/config.ts?)"

# --- 2. dev side ---
lsof -ti :"$DEV_PORT" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
( cd "$APP" && ./node_modules/.bin/vite > "$WORK/dev.log" 2>&1 ) & PIDS+=($!)
for _ in $(seq 1 20); do
  curl -sf -o /dev/null -m 2 -X POST -H 'content-type: application/json' \
    "http://localhost:$DEV_PORT/__zeroship/v1/kv.visit" -d '{"json":{}}' && break
  sleep 2
done
probe "http://localhost:$DEV_PORT" > "$WORK/dev.txt" 2>&1
grep -q '"visits":1' "$WORK/dev.txt" && pass "dev server answered the probe" \
  || { fail "dev server never answered"; tail -20 "$WORK/dev.log"; exit 1; }

# --- 3. deployed side ---
docker rm -f "$REDIS_CONTAINER" >/dev/null 2>&1 || true
docker run --name "$REDIS_CONTAINER" -d -p "$REDIS_PORT:6379" redis:7-alpine >/dev/null
for _ in $(seq 1 30); do docker exec "$REDIS_CONTAINER" redis-cli ping 2>/dev/null | grep -q PONG && break; sleep 1; done
docker exec "$REDIS_CONTAINER" redis-cli ping 2>/dev/null | grep -q PONG \
  && pass "ephemeral Redis ready on :$REDIS_PORT" || { fail "Redis never became ready"; exit 1; }

for p in $CONTROL_PORT $WORKER_PORT $GATE_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "DROP DATABASE IF EXISTS $PG_DB WITH (FORCE)" >/dev/null 2>&1 || true
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "CREATE DATABASE $PG_DB" >/dev/null 2>&1 || true
"$BIN/zeroship-platform-migrate" --database-url "$DB_URL" --migrations-dir "$ROOT/db/migrations-ts" \
  --project-schema zeroship --project-id zeroship > "$WORK/migrate.log" 2>&1 \
  || { fail "platform migrations failed"; tail -20 "$WORK/migrate.log"; exit 1; }

"$BIN/zeroship-control" --port "$CONTROL_PORT" --db "$DB_URL" --blob-store "$WORK/bundles" \
  --control-key "$CONTROL_KEY" --master-key "$MASTER_KEY" > "$WORK/control.log" 2>&1 & PIDS+=($!)
sleep 4
# Without --kv-url the env.kv namespace is absent BY DESIGN and every handler
# fails loudly (crates/worker/src/main.rs). Omitting it here would look like an
# app bug, not a harness bug.
"$BIN/zeroship-worker" --port "$WORKER_PORT" --worker-threads 2 --control "http://localhost:$CONTROL_PORT" \
  --control-key "$CONTROL_KEY" --blob-store "$WORK/bundles" --poll-interval 2 \
  --kv-url "redis://127.0.0.1:$REDIS_PORT" > "$WORK/worker.log" 2>&1 & PIDS+=($!)
sleep 3
openssl rand -base64 48 > "$WORK/gate-secret"; chmod 600 "$WORK/gate-secret"
"$BIN/zeroship-gate" --port "$GATE_PORT" --control "http://localhost:$CONTROL_PORT" \
  --control-key "$CONTROL_KEY" --workers "http://localhost:$WORKER_PORT" --blob-store "$WORK/bundles" \
  --gateway-broker-secret-file "$WORK/gate-secret" --poll-interval 2 > "$WORK/gate.log" 2>&1 & PIDS+=($!)
sleep 4
for svc in "control:$CONTROL_PORT" "worker:$WORKER_PORT" "gateway:$GATE_PORT"; do
  curl -sf "http://localhost:${svc##*:}/health" >/dev/null \
    || { fail "${svc%%:*} did not come up"; tail -20 "$WORK/${svc%%:*}.log"; exit 1; }
done
pass "control + worker + gateway healthy"

OUT=$("$BIN/dev-provision" --db "$DB_URL" --blob-store "$WORK/bundles" --name "$APP_NAME" --zship "$ZSHIP" 2>&1)
API_KEY=$(echo "$OUT" | awk -F= '$1 == "api_key" { print $2 }')
[ -n "$API_KEY" ] && pass "deployed $APP_NAME" || { fail "provision: $OUT"; exit 1; }
sleep 5   # gateway route-sync poll

probe "http://localhost:$GATE_PORT/apps/$APP_NAME" "X-Api-Key: $API_KEY" > "$WORK/deployed.txt" 2>&1
grep -q '"visits":1' "$WORK/deployed.txt" && pass "deployed app answered the probe" \
  || { fail "deployed app never answered"; head -5 "$WORK/deployed.txt"; tail -5 "$WORK/worker.log"; }

# --- 4. THE POINT: identical operations must produce identical results ---
if diff -q "$WORK/dev.txt" "$WORK/deployed.txt" >/dev/null 2>&1; then
  pass "dev and deployed results are identical across every probed operation"
else
  fail "dev and deployed DIVERGE -- results below (< dev, > deployed)"
  diff "$WORK/dev.txt" "$WORK/deployed.txt" | head -40
  echo ""
  echo "  A divergence here is the finding, not a flaky test. Both backends are"
  echo "  individually correct; disagreeing is the defect. See"
  echo "  docs/pilot/e2e-scenarios.md before weakening anything above."
fi

echo ""
echo "  dev vs deployed: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
