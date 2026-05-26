#!/usr/bin/env bash
# M0 exit-gate harness: real Builder chat agent -> real sandbox -> real deploy
# tool -> real control plane -> live gateway fetch.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_DIR="$ROOT/apps/zeroship-builder"
PROMPTS_JSON="$APP_DIR/e2e/m0-prompts.json"
STATE_DIR="$ROOT/.zeroship/m0-gate"
LOG_DIR="$STATE_DIR/logs"

CONTROL_PORT="${CONTROL_PORT:-9090}"
WORKER_PORT="${WORKER_PORT:-8080}"
GATEWAY_PORT="${GATEWAY_PORT:-8000}"
BUILDER_PORT="${BUILDER_PORT:-3001}"
BUILDER_API_PORT="${BUILDER_API_PORT:-3002}"
DATABASE_URL="${DATABASE_URL:-postgres://postgres:zeroship@127.0.0.1:5440/zeroship}"
CONTROL_URL="${CONTROL_URL:-http://localhost:$CONTROL_PORT}"
SANDBOX_URL="${SANDBOX_URL:-http://localhost:9091}"
SANDBOX_TOKEN="${SANDBOX_TOKEN:-zeroship-sandbox-local-harness-token-2026-05-26}"
GATEWAY_URL="${GATEWAY_URL:-http://localhost:$GATEWAY_PORT}"
BUILDER_URL="${BUILDER_URL:-http://localhost:$BUILDER_PORT}"
MASTER_KEY="${MASTER_KEY:-dev-master-key}"
CONTROL_KEY="${CONTROL_KEY:-$MASTER_KEY}"
M0_PROMPT_TIMEOUT_MS="${M0_PROMPT_TIMEOUT_MS:-330000}"

PIDS=()

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 1
  fi
}

kill_numeric_pid() {
  local pid="$1"
  local label="$2"
  if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" >/dev/null 2>&1; then
    echo "[m0] stopping $label pid=$pid"
    kill "$pid" >/dev/null 2>&1 || true
    for _ in $(seq 1 30); do
      if ! kill -0 "$pid" >/dev/null 2>&1; then
        return 0
      fi
      sleep 0.5
    done
    kill -9 "$pid" >/dev/null 2>&1 || true
  fi
}

kill_port_processes() {
  local port="$1"
  local label="$2"
  local pids
  pids="$(lsof -ti :"$port" 2>/dev/null || true)"
  if [[ -z "$pids" ]]; then
    return 0
  fi
  while IFS= read -r pid; do
    [[ -n "$pid" ]] && kill_numeric_pid "$pid" "$label on :$port"
  done <<< "$pids"
}

existing_postgres_container() {
  local c
  c="$(docker ps --format '{{.Names}} {{.Ports}}' \
    | awk '$0 ~ /0\.0\.0\.0:5440->5432\/tcp|127\.0\.0\.1:5440->5432\/tcp|:::5440->5432\/tcp/ { print $1; exit }')"
  [[ -n "$c" ]] || return 1
  printf '%s\n' "$c"
}

postgres_ready() {
  if docker compose exec -T postgres pg_isready -U postgres >/dev/null 2>&1; then
    return 0
  fi
  local c
  c="$(existing_postgres_container || true)"
  [[ -n "$c" ]] && docker exec "$c" pg_isready -U postgres >/dev/null 2>&1
}

ensure_postgres_db() {
  local db="$1"
  if docker compose exec -T postgres psql -U postgres -tAc "SELECT 1 FROM pg_database WHERE datname = '$db'" 2>/dev/null | grep -qx "1"; then
    return 0
  fi
  local c
  c="$(existing_postgres_container || true)"
  if [[ -n "$c" ]]; then
    if ! docker exec "$c" psql -U postgres -tAc "SELECT 1 FROM pg_database WHERE datname = '$db'" 2>/dev/null | grep -qx "1"; then
      docker exec "$c" createdb -U postgres "$db"
    fi
    return 0
  fi
  docker compose exec -T postgres createdb -U postgres "$db"
}

cleanup() {
  local pid
  for pid in "${PIDS[@]:-}"; do
    kill_numeric_pid "$pid" "m0 child"
  done
  wait 2>/dev/null || true
  if [[ "${M0_KEEP_SANDBOX:-0}" != "1" ]]; then
    SANDBOX_URL="$SANDBOX_URL" SANDBOX_TOKEN="$SANDBOX_TOKEN" ./tests/sandbox_down.sh >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

need docker
need cargo
need curl
need jq
need lsof
need npm
need pnpm

mkdir -p "$LOG_DIR"

if [[ -f "$APP_DIR/.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "$APP_DIR/.env"
  set +a
fi

if [[ -z "${OPENAI_API_KEY:-}" ]]; then
  echo "OPENAI_API_KEY is not set; expected it from $APP_DIR/.env" >&2
  exit 1
fi

cat <<EOF
============================================
  zeroship M0 exit gate
============================================

Prompt set and criteria:
EOF
jq -r '.[] | "  - \(.id): \(.prompt)\n    criteria: \(.criteria)"' "$PROMPTS_JSON"

cat <<EOF

Bring-up commands used by tests/m0_gate.sh:
  docker compose up -d postgres
  ./tests/sandbox_up.sh
  cargo build --release -p zeroship-control -p zeroship-worker -p zeroship-gateway
  target/release/zeroship-control --port $CONTROL_PORT --db "$DATABASE_URL" --bundles "$STATE_DIR/bundles" --control-key "$CONTROL_KEY" --master-key "$MASTER_KEY"
  target/release/zeroship-worker --port $WORKER_PORT --workers 2 --control "$CONTROL_URL" --control-key "$CONTROL_KEY" --poll-interval 2
  target/release/zeroship-gate --port $GATEWAY_PORT --control "$CONTROL_URL" --control-key "$CONTROL_KEY" --workers "http://localhost:$WORKER_PORT" --poll-interval 2
  cd apps/zeroship-builder && ZEROSHIP_BUILDER_API_PORT=$BUILDER_API_PORT npm run dev -- --host 127.0.0.1 --port $BUILDER_PORT --strictPort
  cd apps/zeroship-builder && npx playwright test --config=playwright.m0.config.ts e2e/deploy-tool.spec.ts e2e/live-preview-proxy.spec.ts e2e/m0-gate.spec.ts

EOF

cd "$ROOT"

echo "[m0] starting postgres"
if ! docker compose up -d postgres; then
  if ! existing_postgres_container >/dev/null; then
    echo "postgres compose start failed and no existing container publishes :5440" >&2
    exit 1
  fi
  echo "[m0] using existing Postgres container on :5440"
fi
for _ in $(seq 1 60); do
  if postgres_ready; then
    break
  fi
  sleep 1
done
postgres_ready
ensure_postgres_db zeroship

echo "[m0] starting sandbox controller"
kill_port_processes 9091 "sandbox controller"
SANDBOX_URL="$SANDBOX_URL" SANDBOX_TOKEN="$SANDBOX_TOKEN" ./tests/sandbox_up.sh

if [[ "${M0_SKIP_BUILD:-0}" != "1" ]]; then
  echo "[m0] building SDKs and release platform binaries"
  pnpm build
  cargo build --release -p zeroship-control -p zeroship-worker -p zeroship-gateway
fi

kill_port_processes "$CONTROL_PORT" "control plane"
kill_port_processes "$WORKER_PORT" "worker"
kill_port_processes "$GATEWAY_PORT" "gateway"
kill_port_processes "$BUILDER_PORT" "builder dev server"
kill_port_processes "$BUILDER_API_PORT" "builder zeroship API"

mkdir -p "$STATE_DIR/bundles"

echo "[m0] starting control plane"
ZEROSHIP_DEV_INSECURE=1 \
DATABASE_URL="$DATABASE_URL" \
MASTER_KEY="$MASTER_KEY" \
CONTROL_KEY="$CONTROL_KEY" \
"$ROOT/target/release/zeroship-control" \
  --port "$CONTROL_PORT" \
  --db "$DATABASE_URL" \
  --bundles "$STATE_DIR/bundles" \
  --control-key "$CONTROL_KEY" \
  --master-key "$MASTER_KEY" \
  > "$LOG_DIR/control.log" 2>&1 &
PIDS+=("$!")

echo "[m0] starting worker"
"$ROOT/target/release/zeroship-worker" \
  --port "$WORKER_PORT" \
  --workers 2 \
  --control "$CONTROL_URL" \
  --control-key "$CONTROL_KEY" \
  --blob-store "$STATE_DIR/bundles" \
  --poll-interval 2 \
  > "$LOG_DIR/worker.log" 2>&1 &
PIDS+=("$!")

echo "[m0] starting gateway"
"$ROOT/target/release/zeroship-gate" \
  --port "$GATEWAY_PORT" \
  --control "$CONTROL_URL" \
  --control-key "$CONTROL_KEY" \
  --workers "http://localhost:$WORKER_PORT" \
  --blob-store "$STATE_DIR/bundles" \
  --poll-interval 2 \
  > "$LOG_DIR/gateway.log" 2>&1 &
PIDS+=("$!")

for url in "$CONTROL_URL/health" "http://localhost:$WORKER_PORT/health" "$GATEWAY_URL/health"; do
  echo "[m0] waiting for $url"
  for _ in $(seq 1 90); do
    if curl -fsS "$url" >/dev/null 2>&1; then
      break
    fi
    sleep 1
  done
  curl -fsS "$url" >/dev/null
done

echo "[m0] starting one builder dev server on :$BUILDER_PORT"
(
  cd "$APP_DIR"
  CONTROL_URL="$CONTROL_URL" \
  CONTROL_KEY="$CONTROL_KEY" \
  SANDBOX_URL="$SANDBOX_URL" \
  SANDBOX_TOKEN="$SANDBOX_TOKEN" \
  OPENAI_API_KEY="$OPENAI_API_KEY" \
  ZEROSHIP_BUILDER_API_PORT="$BUILDER_API_PORT" \
  npm run dev -- --host 127.0.0.1 --port "$BUILDER_PORT" --strictPort
) > "$LOG_DIR/builder.log" 2>&1 &
PIDS+=("$!")

for _ in $(seq 1 90); do
  if curl -fsS "$BUILDER_URL" >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "${PIDS[-1]}" >/dev/null 2>&1; then
    echo "builder dev server exited before readiness" >&2
    tail -160 "$LOG_DIR/builder.log" >&2
    exit 1
  fi
  sleep 1
done
curl -fsS "$BUILDER_URL" >/dev/null
for _ in $(seq 1 90); do
  if curl -fsS "http://localhost:$BUILDER_API_PORT/health" >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "${PIDS[-1]}" >/dev/null 2>&1; then
    echo "builder dev server exited before API readiness" >&2
    tail -160 "$LOG_DIR/builder.log" >&2
    exit 1
  fi
  sleep 1
done
curl -fsS "http://localhost:$BUILDER_API_PORT/health" >/dev/null

echo "[m0] running real-path Playwright gate"
(
  cd "$APP_DIR"
  PLAYWRIGHT_NO_WEBSERVER=1 \
  BUILDER_URL="$BUILDER_URL" \
  CONTROL_URL="$CONTROL_URL" \
  CONTROL_KEY="$CONTROL_KEY" \
  SANDBOX_URL="$SANDBOX_URL" \
  SANDBOX_TOKEN="$SANDBOX_TOKEN" \
  GATEWAY_URL="$GATEWAY_URL" \
  OPENAI_API_KEY="$OPENAI_API_KEY" \
  M0_PROMPT_TIMEOUT_MS="$M0_PROMPT_TIMEOUT_MS" \
  npx playwright test \
    --config=playwright.m0.config.ts \
    e2e/deploy-tool.spec.ts \
    e2e/live-preview-proxy.spec.ts \
    e2e/m0-gate.spec.ts
)
