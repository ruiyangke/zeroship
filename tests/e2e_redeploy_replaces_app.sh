#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Redeploy: does a second deploy actually replace the running app, or does a
# worker keep serving a stale isolate?
#
# Made unmissable by redeploying a DIFFERENT app under the same name. A stale
# isolate is then not a subtle content difference but a procedure that should
# no longer exist still answering:
#
#   deploy A (kv-dashboard) -> kv.visit answers, getMessages does not exist
#   deploy B (starter)      -> getMessages answers, kv.visit must NOT exist
#
# Proven load-bearing by deploying A twice instead of A then B, which is what a
# stale isolate looks like from outside: 8 passed 0 failed becomes 6 passed
# 2 failed, and the two that fail are exactly the post-redeploy assertions.
#
# dev-provision reuses an app by name (`--name` is documented "create or reuse",
# and the reuse path calls set_deploy_with_manifest), so no PAT is needed here.
#
# Prereqs (docs/runbooks/local-dev.md):
#   cargo build --release -p zeroship-control -p zeroship-worker -p zeroship-gateway -p zeroship --bins
#   cargo build --release -p zeroship-migrate-adapter --features platform-cli --bin zeroship-platform-migrate
#   docker (Postgres on :5440 as compose-postgres-1, plus an ephemeral Redis)
#   pnpm build in examples/kv-dashboard and examples/starter
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"
WORK="$(mktemp -d)"

PG_CONTAINER="${PG_CONTAINER:-compose-postgres-1}"
PG_USER="${PG_USER:-postgres}"
PG_DB="${PG_DB:-zeroship_redeploy}"
DB_URL="${DATABASE_URL:-postgres://postgres:zeroship@localhost:5440/$PG_DB}"
# Distinct from golden_path.sh and e2e_dev_vs_deployed_kv.sh.
CONTROL_PORT="${CONTROL_PORT:-9393}"
WORKER_PORT="${WORKER_PORT:-8393}"
GATE_PORT="${GATE_PORT:-8303}"
REDIS_PORT="${REDIS_PORT:-6397}"
REDIS_CONTAINER="zs-redeploy-redis"
export ZEROSHIP_DEV_INSECURE=1
export WORKER_KEY="${WORKER_KEY:-redeploy-worker-key-0123456789abcdef}"
APP_NAME="redep"
ZSHIP_A="${ZSHIP_A:-$ROOT/examples/kv-dashboard/dist/app.zship}"
ZSHIP_B="${ZSHIP_B:-$ROOT/examples/starter/dist/app.zship}"

PASS=0; FAIL=0; PIDS=()
ok() { PASS=$((PASS+1)); echo "  ok   $1"; }
no() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  docker rm -f "$REDIS_CONTAINER" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# Are both sides the same BUILD? dev runs vite, which spawns `zeroship serve`;
# the deployed side runs separate server binaries. A partial rebuild leaves
# them at different commits and this harness would report the version skew as
# a dev-vs-deployed divergence. That already happened once, in the storage
# walk, where a worker predating 3e7e5e387 produced an error that read exactly
# like an S3 list defect. See tests/lib/binary_freshness.sh.
# shellcheck source=lib/binary_freshness.sh
source "$ROOT/tests/lib/binary_freshness.sh"
zs_check_binary_freshness "$ROOT" "$BIN" \
  "crates/runtime/src crates/worker/src crates/gateway/src crates/control/src crates/bundle/src" \
  "zeroship zeroship-worker zeroship-gate zeroship-control dev-provision" \
  || { [ "$?" -eq 2 ] && exit 2; }

echo "=== redeploy replaces the running app ==="
for z in "$ZSHIP_A" "$ZSHIP_B"; do
  [ -f "$z" ] || { no "missing $z (run pnpm build in that example)"; exit 1; }
done

for p in $CONTROL_PORT $WORKER_PORT $GATE_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done
docker rm -f "$REDIS_CONTAINER" >/dev/null 2>&1 || true
docker run --name "$REDIS_CONTAINER" -d -p "$REDIS_PORT:6379" redis:7-alpine >/dev/null
for _ in $(seq 1 30); do docker exec "$REDIS_CONTAINER" redis-cli ping 2>/dev/null | grep -q PONG && break; sleep 1; done
docker exec "$REDIS_CONTAINER" redis-cli ping 2>/dev/null | grep -q PONG || { no "Redis never became ready"; exit 1; }

docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "DROP DATABASE IF EXISTS $PG_DB WITH (FORCE)" >/dev/null 2>&1 || true
docker exec "$PG_CONTAINER" psql -U "$PG_USER" -c "CREATE DATABASE $PG_DB" >/dev/null 2>&1 || true
"$BIN/zeroship-platform-migrate" --database-url "$DB_URL" --migrations-dir "$ROOT/db/migrations-ts" \
  --project-schema zeroship --project-id zeroship > "$WORK/migrate.log" 2>&1 \
  || { no "platform migrations failed"; tail -20 "$WORK/migrate.log"; exit 1; }

"$BIN/zeroship-control" --port "$CONTROL_PORT" --db "$DB_URL" --blob-store "$WORK/bundles" \
  --control-key rd-ck --master-key rd-mk > "$WORK/control.log" 2>&1 & PIDS+=($!)
sleep 4
# env.kv is absent without --kv-url by design, and kv-dashboard needs it.
"$BIN/zeroship-worker" --port "$WORKER_PORT" --worker-threads 2 --control "http://localhost:$CONTROL_PORT" \
  --control-key rd-ck --blob-store "$WORK/bundles" --poll-interval 2 \
  --kv-url "redis://127.0.0.1:$REDIS_PORT" > "$WORK/worker.log" 2>&1 & PIDS+=($!)
sleep 3
openssl rand -base64 48 > "$WORK/gate-secret"; chmod 600 "$WORK/gate-secret"
"$BIN/zeroship-gate" --port "$GATE_PORT" --control "http://localhost:$CONTROL_PORT" --control-key rd-ck \
  --workers "http://localhost:$WORKER_PORT" --blob-store "$WORK/bundles" \
  --gateway-broker-secret-file "$WORK/gate-secret" --poll-interval 2 > "$WORK/gate.log" 2>&1 & PIDS+=($!)
sleep 4
curl -sf "http://localhost:$GATE_PORT/health" >/dev/null && ok "stack healthy" \
  || { no "stack did not come up"; tail -20 "$WORK/gate.log"; exit 1; }

rpc() {
  curl -sS -m 15 -X POST -H 'content-type: application/json' -H "X-Api-Key: $KEY" \
    "http://localhost:$GATE_PORT/apps/$APP_NAME/__zeroship/v1/$1" -d '{"json":{}}' 2>&1
}
# An unknown procedure is refused by the GATEWAY at routing off the manifest
# ("no resource matched"), not by the dispatcher, which is what dev returns
# ("Method not found"). Accept both so this reads correctly on either side.
absent() { printf '%s' "$1" | grep -qE "Method not found|no resource matched"; }

echo "=== deploy A"
OUT=$("$BIN/dev-provision" --db "$DB_URL" --blob-store "$WORK/bundles" --name "$APP_NAME" --zship "$ZSHIP_A" 2>&1)
KEY=$(echo "$OUT" | awk -F= '$1=="api_key"{print $2}')
APPID=$(echo "$OUT" | awk -F= '$1=="app_id"{print $2}')
[ -n "$KEY" ] && ok "deployed A (app_id=$APPID)" || { no "provision A: $OUT"; exit 1; }
sleep 6
A_VISIT=$(rpc kv.visit); A_MSGS=$(rpc getMessages)
# Values are printed BEFORE they are asserted on. An assertion whose pattern is
# wrong then shows its own evidence: matching the dev-only "Method not found"
# here once reported a stale isolate against an app that was behaving correctly.
echo "    A kv.visit    -> $(printf '%s' "$A_VISIT" | head -c 90)"
echo "    A getMessages -> $(printf '%s' "$A_MSGS" | head -c 90)"
printf '%s' "$A_VISIT" | grep -q '"visits"' && ok "A: kv.visit answers" || no "A: kv.visit did not answer"
absent "$A_MSGS" && ok "A: getMessages absent, as expected" || no "A: getMessages unexpectedly present"

echo "=== redeploy B under the SAME name"
OUT2=$("$BIN/dev-provision" --db "$DB_URL" --blob-store "$WORK/bundles" --name "$APP_NAME" --zship "$ZSHIP_B" 2>&1)
APPID2=$(echo "$OUT2" | awk -F= '$1=="app_id"{print $2}')
KEY=$(echo "$OUT2" | awk -F= '$1=="api_key"{print $2}')
[ -n "$APPID2" ] && ok "redeployed B (app_id=$APPID2)" || { no "provision B: $OUT2"; exit 1; }
[ "$APPID" = "$APPID2" ] && ok "same app id reused, so this is a redeploy not a create" \
  || no "app id changed $APPID -> $APPID2, which is a create not a redeploy"
sleep 8   # gateway route-sync poll + worker bundle pickup

B_VISIT=$(rpc kv.visit); B_MSGS=$(rpc getMessages)
echo "    B kv.visit    -> $(printf '%s' "$B_VISIT" | head -c 90)"
echo "    B getMessages -> $(printf '%s' "$B_MSGS" | head -c 90)"
printf '%s' "$B_MSGS" | grep -q "Build locally" && ok "B: getMessages answers, the new bundle is live" \
  || no "B: getMessages did not answer, so the new bundle is not serving"
absent "$B_VISIT" && ok "B: kv.visit is gone, no stale isolate" \
  || no "B: kv.visit STILL ANSWERS after redeploy, a stale isolate is serving the old bundle"

echo ""
echo "  redeploy: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
