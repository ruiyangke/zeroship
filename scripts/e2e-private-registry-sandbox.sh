#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

STATE_DIR="${PRIVATE_REGISTRY_E2E_STATE_DIR:-$ROOT/.zeroship/private-registry-e2e}"
LOG_DIR="$STATE_DIR/logs"
RUN_LOG="$LOG_DIR/private-registry-sandbox.log"
REGISTRY_PUBLIC="${ZEROSHIP_NPM_REGISTRY:-http://localhost:4873}"
REGISTRY_SANDBOX="${ZEROSHIP_SDK_REGISTRY:-http://host.docker.internal:4873}"
SANDBOX_E2E_TOKEN="${SANDBOX_TOKEN:-zeroship-sandbox-private-registry-e2e-token-2026-05-26}"
SANDBOX_E2E_PORT="${SANDBOX_PORT:-9091}"
SANDBOX_E2E_URL="${SANDBOX_URL:-http://localhost:$SANDBOX_E2E_PORT}"
SANDBOX_E2E_IMAGE="${SANDBOX_IMAGE:-zeroship/sandbox-base:private-registry-e2e}"
SANDBOX_E2E_STATE="$STATE_DIR/sandbox-harness"
NPMRC="$(mktemp)"
STARTED_VERDACCIO=0
STARTED_SANDBOX=0

require() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 1
  fi
}

cleanup() {
  local status=$?
  if [[ "$STARTED_SANDBOX" == "1" ]]; then
    SANDBOX_HARNESS_STATE_DIR="$SANDBOX_E2E_STATE" \
      SANDBOX_NETWORK="${SANDBOX_NETWORK:-zeroship-sandbox-net}" \
      tests/sandbox_down.sh >/dev/null 2>&1 || true
  fi
  if [[ "$STARTED_VERDACCIO" == "1" ]]; then
    docker compose stop verdaccio >/dev/null 2>&1 || true
  fi
  rm -f "$NPMRC"
  exit "$status"
}
trap cleanup EXIT

mkdir -p "$LOG_DIR"
: > "$RUN_LOG"

require curl
require docker
require node
require npm
require pnpm

compose_service_running() {
  docker compose ps --status running --services 2>/dev/null | grep -qx "$1"
}

wait_for_registry() {
  local i
  for i in $(seq 1 90); do
    if curl -fsS "$REGISTRY_PUBLIC/-/ping" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "Verdaccio did not become ready at $REGISTRY_PUBLIC" >&2
  docker compose logs --tail=120 verdaccio >&2 || true
  exit 1
}

create_publish_user() {
  local token
  local user="zeroship-publisher-$(date +%s)-$$"
  token="$(
    curl -fsS -X PUT "$REGISTRY_PUBLIC/-/user/org.couchdb.user:$user" \
      -H 'content-type: application/json' \
      --data "{\"name\":\"$user\",\"password\":\"zeroship-publisher-password\",\"email\":\"zeroship@example.com\",\"type\":\"user\",\"roles\":[]}" |
      node -e 'const fs = require("node:fs"); process.stdout.write(JSON.parse(fs.readFileSync(0, "utf8")).token)'
  )"
  printf 'registry=%s/\n@zeroship:registry=%s/\n//localhost:4873/:_authToken=%s\nalways-auth=true\n' \
    "$REGISTRY_PUBLIC" "$REGISTRY_PUBLIC" "$token" > "$NPMRC"
}

run_logged() {
  echo "\$ $*" | tee -a "$RUN_LOG"
  "$@" 2>&1 | tee -a "$RUN_LOG"
}

echo "== private registry sandbox e2e ==" | tee -a "$RUN_LOG"
echo "host publish registry: $REGISTRY_PUBLIC" | tee -a "$RUN_LOG"
echo "sandbox scoped registry: $REGISTRY_SANDBOX" | tee -a "$RUN_LOG"

if compose_service_running verdaccio; then
  echo "Verdaccio compose service already running; reusing it." | tee -a "$RUN_LOG"
else
  run_logged docker compose up -d verdaccio
  STARTED_VERDACCIO=1
fi
wait_for_registry
create_publish_user

echo "Publishing SDKs to Verdaccio..." | tee -a "$RUN_LOG"
run_logged env \
  ZEROSHIP_NPM_REGISTRY="$REGISTRY_PUBLIC" \
  NPM_CONFIG_USERCONFIG="$NPMRC" \
  pnpm publish:sdks

echo "Starting real docker sandbox controller..." | tee -a "$RUN_LOG"
run_logged env \
  SANDBOX_HARNESS_STATE_DIR="$SANDBOX_E2E_STATE" \
  SANDBOX_PORT="$SANDBOX_E2E_PORT" \
  SANDBOX_URL="$SANDBOX_E2E_URL" \
  SANDBOX_TOKEN="$SANDBOX_E2E_TOKEN" \
  SANDBOX_IMAGE="$SANDBOX_E2E_IMAGE" \
  SANDBOX_NETWORK="${SANDBOX_NETWORK:-zeroship-sandbox-net}" \
  tests/sandbox_up.sh
STARTED_SANDBOX=1

echo "Running Builder e2e against the real sandbox install/build path..." | tee -a "$RUN_LOG"
run_logged env \
  PLAYWRIGHT_NO_WEBSERVER=1 \
  SANDBOX_URL="$SANDBOX_E2E_URL" \
  SANDBOX_TOKEN="$SANDBOX_E2E_TOKEN" \
  ZEROSHIP_SDK_REGISTRY="$REGISTRY_SANDBOX" \
  pnpm --filter zeroship-builder exec playwright test e2e/private-registry-sandbox.spec.ts --reporter=line

echo "Acceptance log: $RUN_LOG" | tee -a "$RUN_LOG"
