#!/usr/bin/env bash
# DW-07 durable-workflows M1 keystone faithful e2e.
#
# Boots disposable Postgres on :5440, applies the real platform migrations,
# boots real control/gateway/worker processes, deploys a real workflow .zship,
# then drives the real control workflow engine from the Rust integration test.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release"

PG_PORT="${PG_PORT:-5440}"
PG_CONTAINER="${PG_CONTAINER:-zs-dw07-pg}"
PG_USER="${PG_USER:-postgres}"
PG_PASS="${PG_PASS:-zeroship}"
PG_DB="${PG_DB:-zeroship_dw07_$(date +%s)_$$}"
CONTROL_PORT="${CONTROL_PORT:-9130}"
WORKER_PORT="${WORKER_PORT:-9131}"
GATE_PORT="${GATE_PORT:-9132}"
SIDE_PORT="${SIDE_PORT:-9133}"
APP_NAME="dw07-$(date +%s)-$$"
DBURL="postgres://$PG_USER:$PG_PASS@localhost:$PG_PORT/$PG_DB"

WORK="$(mktemp -d -t zs-dw07-XXXXXX)"
PIDFILE="$WORK/pids"
: > "$PIDFILE"
PG_ADMIN_CONTAINER=""
OWNED_PG_CONTAINER=0
CREATED_PG_DB=0

pass() { echo "  ✓ $1"; }
fail() { echo "  ✗ $1"; }

cleanup() {
  if [ -f "$PIDFILE" ]; then
    while read -r pid; do
      [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
    done < "$PIDFILE"
  fi
  wait 2>/dev/null || true
  if [ "$CREATED_PG_DB" = "1" ] && [ -n "$PG_ADMIN_CONTAINER" ]; then
    docker exec "$PG_ADMIN_CONTAINER" psql -U "$PG_USER" -d postgres -v ON_ERROR_STOP=1 \
      -c "DROP DATABASE IF EXISTS \"$PG_DB\" WITH (FORCE);" >/dev/null 2>&1 || true
  fi
  if [ "$OWNED_PG_CONTAINER" = "1" ]; then
    docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

stop_services() {
  if [ -f "$PIDFILE" ]; then
    while read -r pid; do
      [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
    done < "$PIDFILE"
    : > "$PIDFILE"
  fi
  wait 2>/dev/null || true
}

terminate_pg_db_connections() {
  docker exec "$PG_ADMIN_CONTAINER" psql -U "$PG_USER" -d postgres -v ON_ERROR_STOP=1 \
    -c "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '$PG_DB' AND pid <> pg_backend_pid();" \
    >/dev/null
}

require_cmd() {
  command -v "$1" >/dev/null || { fail "$1 required"; exit 2; }
}

build_zship() {
  local js_file="$1"
  local out_path="$2"
  local stage hash now
  stage="$(mktemp -d -t zs-dw07-zship-XXXXXX)"
  mkdir -p "$stage/blobs"
  hash="$(sha256sum "$js_file" | awk '{print $1}')"
  cp "$js_file" "$stage/blobs/$hash"
  now="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  cat > "$stage/manifest.json" <<EOF
{"version":1,"resources":{"/[...rest]":{"auth":"anon","publicly_accessible":true}},"assets":{},"runtime_assets":{},"asset_version":0,"sourcemaps":{},"worker":{"entry":"index.js","modules":{"index.js":"$hash"}},"metadata":{"compiler":"dw07-e2e","built_at":"$now"}}
EOF
  (cd "$stage" && tar --format=ustar -cf - manifest.json "blobs/$hash") \
    | zstd -q -f -o "$out_path"
  rm -rf "$stage"
}

wait_health() {
  local name="$1" url="$2" log="$3"
  local i
  for i in $(seq 1 45); do
    curl -sf "$url" >/dev/null 2>&1 && { pass "$name healthy"; return 0; }
    sleep 1
  done
  fail "$name unhealthy"
  tail -80 "$log" || true
  exit 1
}

require_cmd docker
require_cmd curl
require_cmd sha256sum
require_cmd tar
require_cmd zstd
require_cmd pnpm
require_cmd lsof

echo "=== DW-07 build ==="
pnpm build
cargo build --release -p zeroship-control -p zeroship-gateway -p zeroship-worker
cargo build --release -p zeroship-migrate --features standalone-cli --bins
for b in zeroship-control zeroship-gate zeroship-worker zeroship-migrate dev-provision; do
  [ -x "$BIN/$b" ] || { fail "missing $BIN/$b"; exit 2; }
done
pass "release binaries built"

echo "=== DW-07 database :$PG_PORT ==="
case "$PG_DB" in
  *[!A-Za-z0-9_]*)
    fail "PG_DB must contain only letters, digits, and underscores: $PG_DB"
    exit 2
    ;;
esac

PG_ADMIN_CONTAINER="$(docker ps --format '{{.Names}} {{.Ports}}' \
  | awk -v port="$PG_PORT" 'index($0, ":" port "->5432/tcp") { print $1; exit }')"
if [ -n "$PG_ADMIN_CONTAINER" ]; then
  pass "using existing Postgres container $PG_ADMIN_CONTAINER on :$PG_PORT"
else
  docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
  docker run --name "$PG_CONTAINER" -d -p "$PG_PORT:5432" \
    -e POSTGRES_PASSWORD="$PG_PASS" -e POSTGRES_USER="$PG_USER" -e POSTGRES_DB=postgres \
    postgres:16 -c max_connections=300 >/dev/null
  PG_ADMIN_CONTAINER="$PG_CONTAINER"
  OWNED_PG_CONTAINER=1
fi

for _ in $(seq 1 45); do
  docker exec "$PG_ADMIN_CONTAINER" pg_isready -U "$PG_USER" >/dev/null 2>&1 && break
  sleep 1
done
docker exec "$PG_ADMIN_CONTAINER" pg_isready -U "$PG_USER" >/dev/null 2>&1 || {
  fail "postgres never became ready"
  exit 1
}
pass "PG ready on :$PG_PORT"

docker exec "$PG_ADMIN_CONTAINER" psql -U "$PG_USER" -d postgres -v ON_ERROR_STOP=1 \
  -c "DROP DATABASE IF EXISTS \"$PG_DB\" WITH (FORCE);" \
  -c "CREATE DATABASE \"$PG_DB\";" >/dev/null
CREATED_PG_DB=1
pass "created disposable database $PG_DB on :$PG_PORT"

if [ -f "$ROOT/ops/postgres-init.sql" ]; then
  docker exec -i "$PG_ADMIN_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -v ON_ERROR_STOP=1 \
    < "$ROOT/ops/postgres-init.sql" >/dev/null
  pass "applied ops/postgres-init.sql"
fi

ZEROSHIP_RECORDER_CHILD="$BIN/zeroship-migrate-recorder-child" \
  "$BIN/zeroship-migrate" migrate \
  --profile platform \
  --dir "$ROOT/db/migrations-ts" \
  --database-url "$DBURL" \
  --yes > "$WORK/migrate.log" 2>&1 || {
    fail "platform migrations failed"
    tail -80 "$WORK/migrate.log"
    exit 1
  }
pass "platform migrations applied with zeroship-migrate"

mkdir -p "$WORK/blobs" "$WORK/blob-cache"

echo "=== DW-07 workflow .zship ==="
cat > "$WORK/workflow.js" <<EOF
const SIDE_EFFECT_URL = "http://127.0.0.1:$SIDE_PORT";

async function bump(runId, stepName) {
  const response = await fetch(
    \`\${SIDE_EFFECT_URL}/bump?run=\${encodeURIComponent(runId)}&step=\${encodeURIComponent(stepName)}\`,
    { method: "POST" },
  );
  if (!response.ok) {
    throw new Error(\`side effect \${stepName} failed: \${response.status}\`);
  }
  return await response.json();
}

export class KeystoneWorkflow {
  async run(trigger, step) {
    const a = await step.run("a", () => bump(trigger.runId, "a"));
    await step.sleep("sleep", "PT1S");
    const b = await step.run("b", () => bump(trigger.runId, "b"));
    return { a, b };
  }
}

export class SignalWorkflow {
  async run(trigger, step) {
    const a = await step.run("a", () => bump(trigger.runId, "a"));
    try {
      const signal = await step.waitForSignal("go", {
        type: "go",
        timeout: trigger.input.timeout,
        maxSignalAge: trigger.input.maxSignalAge,
      });
      const b = await step.run("b", () => bump(trigger.runId, "b"));
      return { state: "signaled", a, signal, b };
    } catch (error) {
      if (!error || error.name !== "WorkflowTimeoutError") {
        throw error;
      }
      const timeout = await step.run("timeout", () => bump(trigger.runId, "timeout"));
      return { state: "timeout", errorName: error.name, a, timeout };
    }
  }
}

export class ConcurrentWorkflow {
  async run(trigger, step) {
    const [a, b, c] = await Promise.all([
      step.run("a", () => bump(trigger.runId, "a")),
      step.run("b", () => bump(trigger.runId, "b")),
      step.run("c", () => bump(trigger.runId, "c")),
    ]);
    const final = await step.run("final", () => bump(trigger.runId, "final"));
    return { a, b, c, final };
  }
}

export default {
  async fetch() {
    return new Response("dw07-ok");
  },
  workflows: { KeystoneWorkflow, SignalWorkflow, ConcurrentWorkflow },
};
EOF
build_zship "$WORK/workflow.js" "$WORK/workflow.zship"
pass "built real workflow .zship"

echo "=== DW-07 services ==="
for port in "$CONTROL_PORT" "$WORKER_PORT" "$GATE_PORT" "$SIDE_PORT"; do
  lsof -ti :"$port" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
done
GATEWAY_BROKER_SECRET_FILE="$WORK/gateway-broker-secret"
printf '%s' "dw07-gateway-broker-secret-32-bytes-minimum-ok" > "$GATEWAY_BROKER_SECRET_FILE"
chmod 0600 "$GATEWAY_BROKER_SECRET_FILE"

"$BIN/zeroship-control" \
  --port "$CONTROL_PORT" \
  --db "$DBURL" \
  --blob-store "$WORK/blobs" \
  --gateway-url "http://localhost:$GATE_PORT" \
  --dev-insecure \
  --disable-workflow-engine > "$WORK/control.log" 2>&1 &
echo $! >> "$PIDFILE"
wait_health control "http://localhost:$CONTROL_PORT/health" "$WORK/control.log"

ZEROSHIP_DEV=1 "$BIN/zeroship-worker" \
  --port "$WORKER_PORT" \
  --worker-threads 1 \
  --control "http://localhost:$CONTROL_PORT" \
  --db "$DBURL" \
  --blob-store "$WORK/blobs" \
  --poll-interval 1 \
  --dev-insecure \
  --workflow-dispatch-unsigned > "$WORK/worker.log" 2>&1 &
echo $! >> "$PIDFILE"
wait_health worker "http://localhost:$WORKER_PORT/health" "$WORK/worker.log"

"$BIN/zeroship-gate" \
  --port "$GATE_PORT" \
  --control "http://localhost:$CONTROL_PORT" \
  --workers "http://localhost:$WORKER_PORT" \
  --blob-store "$WORK/blobs" \
  --blob-cache-disk-root "$WORK/blob-cache" \
  --gateway-broker-secret-file "$GATEWAY_BROKER_SECRET_FILE" \
  --db "$DBURL" \
  --poll-interval 1 \
  --dev-insecure > "$WORK/gate.log" 2>&1 &
echo $! >> "$PIDFILE"
wait_health gateway "http://localhost:$GATE_PORT/health" "$WORK/gate.log"

echo "=== DW-07 deploy ==="
ZEROSHIP_DEV_INSECURE=1 "$BIN/dev-provision" \
  --db "$DBURL" \
  --blob-store "$WORK/blobs" \
  --name "$APP_NAME" \
  --zship "$WORK/workflow.zship" > "$WORK/provision.out"
APP_ID="$(awk -F= '/^app_id=/{print $2}' "$WORK/provision.out")"
API_KEY="$(awk -F= '/^api_key=/{print $2}' "$WORK/provision.out")"
[ -n "$APP_ID" ] || { fail "dev-provision did not return app_id"; cat "$WORK/provision.out"; exit 1; }
DEPLOY_ID="dep_dw07_$(echo "$APP_ID" | tr -d '-')"

docker exec -i "$PG_ADMIN_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -v ON_ERROR_STOP=1 >/dev/null <<SQL
INSERT INTO zeroship.app_net_grants (app_id, host, port, granted_by, note)
VALUES ('$APP_ID', '127.0.0.1', $SIDE_PORT, 'dw07-e2e', 'DW-07 side-effect counter')
ON CONFLICT (app_id, host, port) DO UPDATE SET granted_at = now(), note = EXCLUDED.note;

INSERT INTO zeroship.app_deploys (id, app_id, deploy_hash, manifest_json, activated_at)
SELECT '$DEPLOY_ID', id, deploy_hash, manifest_json, now()
  FROM zeroship.apps
 WHERE id = '$APP_ID'
ON CONFLICT (id) DO UPDATE SET
  deploy_hash = EXCLUDED.deploy_hash,
  manifest_json = EXCLUDED.manifest_json,
  activated_at = EXCLUDED.activated_at;
SQL
pass "deployed app $APP_ID and pinned deploy $DEPLOY_ID"

sleep 3
curl -sf "http://localhost:$GATE_PORT/apps/$APP_NAME/" -H "X-Api-Key: $API_KEY" >/dev/null || {
  fail "warmup request failed"
  tail -80 "$WORK/worker.log" || true
  tail -80 "$WORK/gate.log" || true
  exit 1
}
pass "gateway/worker warmed real deployed app"

echo "=== DW-07 keystone assertions ==="
ZEROSHIP_DW_E2E=1 \
CONTROL_TEST_DB="$DBURL" \
ZEROSHIP_DW_E2E_CONTROL_URL="http://localhost:$CONTROL_PORT" \
ZEROSHIP_DW_E2E_GATEWAY_URL="http://localhost:$GATE_PORT" \
ZEROSHIP_DW_E2E_APP_ID="$APP_ID" \
ZEROSHIP_DW_E2E_DEPLOY_ID="$DEPLOY_ID" \
ZEROSHIP_DW_E2E_SIDE_PORT="$SIDE_PORT" \
ZEROSHIP_DW_E2E_PG_CONTAINER="$PG_ADMIN_CONTAINER" \
ZEROSHIP_DW_E2E_PG_USER="$PG_USER" \
ZEROSHIP_DW_E2E_PG_DB="$PG_DB" \
  cargo test -p zeroship-control --test durable_workflows_keystone_e2e -- --nocapture || {
    fail "DW-07 keystone assertions failed"
    echo "--- control.log ---"
    tail -120 "$WORK/control.log" || true
    echo "--- worker.log ---"
    tail -160 "$WORK/worker.log" || true
    echo "--- gate.log ---"
    tail -120 "$WORK/gate.log" || true
    exit 1
  }
pass "DW-07 keystone e2e passed"

echo "=== DW-07 stop services before regression ==="
stop_services
terminate_pg_db_connections
pass "stopped real services; workflow engine regression runs alone"

echo "=== DW-07 workflow engine regression ==="
CONTROL_TEST_DB="$DBURL" \
  cargo test -p zeroship-control --test workflow_engine_test -- --nocapture --test-threads=1 || {
    fail "DW-07 workflow engine regression failed"
    echo "--- control.log ---"
    tail -120 "$WORK/control.log" || true
    echo "--- worker.log ---"
    tail -160 "$WORK/worker.log" || true
    echo "--- gate.log ---"
    tail -120 "$WORK/gate.log" || true
    exit 1
  }
pass "DW-07 workflow engine regression passed"
