#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Golden path — the "build locally → deploy → run" chain, end-to-end, with a
# REAL vite-built app (examples/starter), not a hand-packed fixture.
#
# This is the canonical creator flow under the build-local strategy: an AI
# coding agent (Claude Code / Codex) builds a zeroship app on the creator's
# machine, `pnpm build` produces dist/app.zship, and `zeroship deploy` ships
# it to the platform — then the gateway serves it.
#
# Proves: vite-plugin build → .zship → control-plane deploy → worker load →
# gateway serve (static assets) + (best-effort) an RPC round-trip.
#
# Prereqs (see docs/runbooks/local-dev.md):
#   - release binaries: cargo build --release -p zeroship-control -p zeroship-worker -p zeroship-gateway -p zeroship
#   - a Postgres reachable at $DATABASE_URL (default: the compose instance on :5440)
#   - examples/starter deps installed (pnpm install) so `pnpm build` works
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"
# Dedicated, freshly-migrated DB per run (isolated from the shared `zeroship`
# db) so the run is self-contained + reproducible and never re-provisions a
# stale app.
PG_CONTAINER="${PG_CONTAINER:-appbase-migrate-postgres-1}"
PG_USER="${PG_USER:-postgres}"
PG_DB="${PG_DB:-zeroship_golden}"
DB_URL="${DATABASE_URL:-postgres://postgres:zeroship@localhost:5440/$PG_DB}"
# Distinct ports so this never clashes with a running dev stack.
CONTROL_PORT="${CONTROL_PORT:-9390}"
WORKER_PORT="${WORKER_PORT:-8390}"
GATE_PORT="${GATE_PORT:-8300}"
CONTROL_KEY="gp-ck"
MASTER_KEY="gp-mk"
# Local dev: run the platform without the production secret set (signing keys,
# worker key, …). NEVER use this outside local dev. Mirrors tests/m0_gate.sh.
export ZEROSHIP_DEV_INSECURE=1
export WORKER_KEY="${WORKER_KEY:-golden-path-worker-key-0123456789abcdef}"
APP_NAME="starter"
STARTER="$ROOT/examples/starter"
ZSHIP="$STARTER/dist/app.zship"

PASS=0; FAIL=0; PIDS=()
pass() { PASS=$((PASS+1)); echo "  ✓ $1"; }
fail() { FAIL=$((FAIL+1)); echo "  ✗ $1"; }
cleanup() { for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done; wait 2>/dev/null || true; }
trap cleanup EXIT

echo "============================================"
echo "  zeroship golden path (build-local → deploy)"
echo "============================================"

# --- 1. Build the starter (real vite-plugin → .zship) ---
echo "=== 1. Build examples/starter (pnpm build → dist/app.zship) ==="
( cd "$STARTER" && pnpm build ) >/tmp/gp-build.log 2>&1
[ -f "$ZSHIP" ] && pass "built $(basename "$ZSHIP") ($(du -k "$ZSHIP" | cut -f1)KB)" || { fail "build produced no app.zship"; tail -20 /tmp/gp-build.log; exit 1; }

# --- 2. Bring up the stack (control + worker + gateway) ---
echo "=== 2. Bring up the stack ==="
for p in $CONTROL_PORT $WORKER_PORT $GATE_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done
rm -rf /tmp/gp-bundles

# Fresh dedicated DB + the full platform schema (db/migrations/*.sql are plain
# SQL, applied in order — no migrate-engine build needed).
echo "  migrating a fresh $PG_DB ..."
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "DROP DATABASE IF EXISTS $PG_DB WITH (FORCE)" >/dev/null 2>&1 || \
  docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "DROP DATABASE IF EXISTS $PG_DB" >/dev/null 2>&1 || true
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "CREATE DATABASE $PG_DB" >/dev/null 2>&1 || true
for f in $(ls "$ROOT"/db/migrations/V*.sql | grep -vE "\.down\." | sort); do
  docker exec -i "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -v ON_ERROR_STOP=1 -q < "$f" >/tmp/gp-migrate.log 2>&1 \
    || { fail "migration $(basename "$f") failed"; tail -5 /tmp/gp-migrate.log; exit 1; }
done
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc "select to_regclass('zeroship.apps')" 2>/dev/null | grep -q apps \
  && pass "schema migrated (fresh $PG_DB)" || { fail "schema missing after migrate"; exit 1; }

"$BIN/zeroship-control" --port "$CONTROL_PORT" --db "$DB_URL" --blob-store /tmp/gp-bundles \
  --control-key "$CONTROL_KEY" --master-key "$MASTER_KEY" >/tmp/gp-control.log 2>&1 & PIDS+=($!)
sleep 3
"$BIN/zeroship-worker" --port "$WORKER_PORT" --worker-threads 2 --control "http://localhost:$CONTROL_PORT" \
  --control-key "$CONTROL_KEY" --blob-store /tmp/gp-bundles --poll-interval 2 >/tmp/gp-worker.log 2>&1 & PIDS+=($!)
sleep 2
"$BIN/zeroship-gate" --port "$GATE_PORT" --control "http://localhost:$CONTROL_PORT" \
  --control-key "$CONTROL_KEY" --workers "http://localhost:$WORKER_PORT" --blob-store /tmp/gp-bundles --poll-interval 2 >/tmp/gp-gate.log 2>&1 & PIDS+=($!)
sleep 3

curl -sf "http://localhost:$CONTROL_PORT/health" >/dev/null && pass "control healthy" || { fail "control down"; tail -20 /tmp/gp-control.log; exit 1; }
curl -sf "http://localhost:$WORKER_PORT/health"  >/dev/null && pass "worker healthy"  || { fail "worker down";  tail -20 /tmp/gp-worker.log; exit 1; }
curl -sf "http://localhost:$GATE_PORT/health"    >/dev/null && pass "gateway healthy" || { fail "gateway down"; tail -20 /tmp/gp-gate.log; exit 1; }

# --- 3. Create app + deploy the real .zship ---
echo "=== 3. Create app + deploy ==="
TOKEN="${ZEROSHIP_TOKEN:-}"
if [ -n "$TOKEN" ]; then
  APP=$(curl -sf -X POST "http://localhost:$CONTROL_PORT/api/apps" -H 'Content-Type: application/json' \
    -H "Authorization: Bearer $TOKEN" -d "{\"name\":\"$APP_NAME\"}")
  APP_ID=$(echo "$APP" | jq -r '.id'); API_KEY=$(echo "$APP" | jq -r '.api_key')
  [ -n "$APP_ID" ] && [ "$APP_ID" != "null" ] && pass "created app ($APP_ID)" || { fail "create app: $APP"; exit 1; }

  DEPLOY=$("$BIN/zeroship" deploy "$ZSHIP" --app="$APP_ID" --control="http://localhost:$CONTROL_PORT" --token="$TOKEN" 2>&1)
  echo "$DEPLOY" | grep -q "deploy_hash" && pass "deployed real vite .zship" || { fail "deploy: $DEPLOY"; exit 1; }
else
  OUT=$("$BIN/dev-provision" --db "$DB_URL" --blob-store /tmp/gp-bundles --name "$APP_NAME" --zship "$ZSHIP")
  APP_ID=$(echo "$OUT" | awk -F= '$1 == "app_id" { print $2 }')
  API_KEY=$(echo "$OUT" | awk -F= '$1 == "api_key" { print $2 }')
  [ -n "$APP_ID" ] && [ -n "$API_KEY" ] && pass "dev-provisioned app ($APP_ID)" || { fail "dev-provision: $OUT"; exit 1; }
fi
sleep 4  # gateway route-sync poll

# --- 4. The chain works: gateway serves the deployed app ---
echo "=== 4. Live: gateway serves the deployed app ==="
INDEX=$(curl -sf "http://localhost:$GATE_PORT/apps/$APP_NAME/" -H "X-Api-Key: $API_KEY" 2>/dev/null || echo "")
echo "$INDEX" | grep -qi "<!doctype html" && pass "GET / serves the app index.html" || fail "index.html not served (got: ${INDEX:0:80})"

# the hashed JS asset referenced by index.html
ASSET=$(echo "$INDEX" | grep -oE '/assets/[A-Za-z0-9._-]+\.js' | head -1)
if [ -n "$ASSET" ]; then
  code=$(curl -s -o /dev/null -w '%{http_code}' "http://localhost:$GATE_PORT/apps/$APP_NAME$ASSET" -H "X-Api-Key: $API_KEY")
  [ "$code" = "200" ] && pass "client JS asset served ($ASSET → 200)" || fail "asset $ASSET → $code"
fi

# --- 5. RPC round-trip: the deployed app's SERVER FUNCTION actually executes ---
# vite-app RPCs are at /__zeroship/v1/<wireId> (GET ?input= for queries), the
# same path the browser client uses; through the path-routed gateway that's
# /apps/<name>/__zeroship/v1/<wireId>. getMessages takes no input.
echo "=== 5. RPC round-trip (server function executes) ==="
RPC=$(curl -s "http://localhost:$GATE_PORT/apps/$APP_NAME/__zeroship/v1/getMessages" \
  -H "X-Api-Key: $API_KEY" 2>/dev/null || echo "")
if echo "$RPC" | grep -q "Build locally"; then
  pass "getMessages RPC executed in the worker and returned the seeded messages"
else
  echo "  RPC response: ${RPC:0:200}"
  fail "RPC round-trip (getMessages did not return the expected data)"
fi

echo ""
echo "============================================"
echo "  golden path: $PASS passed, $FAIL failed"
echo "============================================"
[ "$FAIL" -eq 0 ]
