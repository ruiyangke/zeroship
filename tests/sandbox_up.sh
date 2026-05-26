#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

STATE_DIR="${SANDBOX_HARNESS_STATE_DIR:-$ROOT/.zeroship/sandbox-harness}"
PID_FILE="$STATE_DIR/zeroship-sandbox.pid"
LOG_FILE="$STATE_DIR/zeroship-sandbox.log"
READY_FILE="$STATE_DIR/readyz.json"

PORT="${SANDBOX_PORT:-9091}"
URL="${SANDBOX_URL:-http://localhost:$PORT}"
TOKEN="${SANDBOX_TOKEN:-zeroship-sandbox-local-harness-token-2026-05-26}"
IMAGE="${SANDBOX_IMAGE:-zeroship/sandbox-base:latest}"
NETWORK="${SANDBOX_NETWORK:-zeroship-sandbox-net}"
WORKSPACE_ROOT="${SANDBOX_WORKSPACE_ROOT:-$STATE_DIR/projects}"
PERSIST_DIR="${SANDBOX_PERSIST_DIR:-$STATE_DIR/persist}"
DB_NAME="${SANDBOX_HARNESS_DB_NAME:-zeroship_sandbox}"
DB_URL="${SANDBOX_DATABASE_URL:-postgres://postgres:zeroship@localhost:5440/$DB_NAME}"
HOST_ID="${SANDBOX_HOST_ID:-00000000-0000-7000-8000-000000000091}"

require() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 1
  fi
}

require docker
require cargo
require curl

if (( ${#TOKEN} < 32 )); then
  echo "SANDBOX_TOKEN must be at least 32 bytes; got ${#TOKEN}" >&2
  exit 1
fi

if [[ ! "$DB_NAME" =~ ^[A-Za-z0-9_]+$ ]]; then
  echo "SANDBOX_HARNESS_DB_NAME must contain only letters, numbers, and underscore" >&2
  exit 1
fi

mkdir -p "$STATE_DIR" "$WORKSPACE_ROOT" "$PERSIST_DIR"

ensure_postgres() {
  if ! docker compose up -d postgres; then
    if ! existing_postgres_container >/dev/null; then
      echo "postgres compose start failed and no existing container publishes :5440" >&2
      docker compose logs --tail=80 postgres >&2 || true
      exit 1
    fi
    echo "Using existing Postgres container on :5440 for sandbox harness." >&2
  fi

  local i
  for i in $(seq 1 60); do
    if postgres_ready; then
      break
    fi
    sleep 1
  done

  if ! postgres_ready; then
    echo "postgres did not become ready" >&2
    docker compose logs --tail=80 postgres >&2 || true
    exit 1
  fi

  if ! postgres_db_exists "$DB_NAME"; then
    create_postgres_db "$DB_NAME"
  fi
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

postgres_db_exists() {
  local db="$1"
  if docker compose exec -T postgres psql -U postgres -tAc "SELECT 1 FROM pg_database WHERE datname = '$db'" 2>/dev/null | grep -qx "1"; then
    return 0
  fi
  local c
  c="$(existing_postgres_container || true)"
  [[ -n "$c" ]] && docker exec "$c" psql -U postgres -tAc "SELECT 1 FROM pg_database WHERE datname = '$db'" 2>/dev/null | grep -qx "1"
}

create_postgres_db() {
  local db="$1"
  if docker compose exec -T postgres createdb -U postgres "$db" >/dev/null 2>&1; then
    return 0
  fi
  local c
  c="$(existing_postgres_container || true)"
  if [[ -n "$c" ]]; then
    docker exec "$c" createdb -U postgres "$db"
    return 0
  fi
  echo "could not create postgres database $db" >&2
  exit 1
}

ensure_network() {
  if ! docker network inspect "$NETWORK" >/dev/null 2>&1; then
    docker network create "$NETWORK" >/dev/null
  fi
}

stop_existing_controller() {
  if [[ -f "$PID_FILE" ]]; then
    local pid
    pid="$(cat "$PID_FILE")"
    if [[ -n "$pid" ]] && kill -0 "$pid" >/dev/null 2>&1; then
      kill "$pid" >/dev/null 2>&1 || true
      local i
      for i in $(seq 1 20); do
        if ! kill -0 "$pid" >/dev/null 2>&1; then
          break
        fi
        sleep 0.5
      done
      if kill -0 "$pid" >/dev/null 2>&1; then
        kill -9 "$pid" >/dev/null 2>&1 || true
      fi
    fi
    rm -f "$PID_FILE"
  elif curl -fsS "$URL/livez" >/dev/null 2>&1; then
    echo "$URL already responds, but $PID_FILE is absent; refusing to stop an unknown process" >&2
    exit 1
  fi
}

build_base_image() {
  docker build \
    -f crates/sandbox/docker/Dockerfile.sandbox-base \
    -t "$IMAGE" \
    .
}

build_controller() {
  cargo build -p zeroship-sandbox --bin zeroship-sandbox
}

start_controller() {
  : > "$LOG_FILE"
  local -a cmd
  cmd=(env
    SANDBOX_PORT="$PORT"
    SANDBOX_TOKEN="$TOKEN"
    SANDBOX_BACKEND=docker
    SANDBOX_IMAGE="$IMAGE"
    SANDBOX_WORKSPACE_ROOT="$WORKSPACE_ROOT"
    SANDBOX_NETWORK="$NETWORK"
    SANDBOX_AUTO_PULL=false
    SANDBOX_IDLE_TIMEOUT_SECS="${SANDBOX_IDLE_TIMEOUT_SECS:-1800}"
    SANDBOX_MAX_LIFETIME_SECS="${SANDBOX_MAX_LIFETIME_SECS:-28800}"
    SANDBOX_DATABASE_URL="$DB_URL"
    SANDBOX_PG_RUN_MIGRATIONS=1
    SANDBOX_PG_BOOT_TIMEOUT_SECS="${SANDBOX_PG_BOOT_TIMEOUT_SECS:-60}"
    SANDBOX_PERSIST_DIR="$PERSIST_DIR"
    SANDBOX_HOST_ID="$HOST_ID"
    "$ROOT/target/debug/zeroship-sandbox")

  if command -v setsid >/dev/null 2>&1; then
    setsid "${cmd[@]}" > "$LOG_FILE" 2>&1 < /dev/null &
  else
    nohup "${cmd[@]}" > "$LOG_FILE" 2>&1 < /dev/null &
  fi
  echo "$!" > "$PID_FILE"
}

wait_ready() {
  local pid
  pid="$(cat "$PID_FILE")"

  local i
  for i in $(seq 1 90); do
    if curl -fsS "$URL/readyz" > "$READY_FILE" 2>/dev/null; then
      return 0
    fi
    if ! kill -0 "$pid" >/dev/null 2>&1; then
      echo "zeroship-sandbox exited before readiness" >&2
      tail -120 "$LOG_FILE" >&2
      exit 1
    fi
    sleep 1
  done

  echo "zeroship-sandbox did not become ready at $URL/readyz" >&2
  tail -160 "$LOG_FILE" >&2
  exit 1
}

ensure_postgres
ensure_network
build_base_image
build_controller
stop_existing_controller
start_controller
wait_ready

cat <<EOF
Sandbox controller is ready.
export SANDBOX_URL=$URL
export SANDBOX_TOKEN=$TOKEN
SANDBOX_DATABASE_URL=$DB_URL
SANDBOX_IMAGE=$IMAGE
SANDBOX_NETWORK=$NETWORK
SANDBOX_WORKSPACE_ROOT=$WORKSPACE_ROOT
SANDBOX_HOST_ID=$HOST_ID
PID_FILE=$PID_FILE
LOG_FILE=$LOG_FILE
EOF
