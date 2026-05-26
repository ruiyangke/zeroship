#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

STATE_DIR="${SANDBOX_HARNESS_STATE_DIR:-$ROOT/.zeroship/sandbox-harness}"
PID_FILE="$STATE_DIR/zeroship-sandbox.pid"
NETWORK="${SANDBOX_NETWORK:-zeroship-sandbox-net}"

if [[ -f "$PID_FILE" ]]; then
  pid="$(cat "$PID_FILE")"
  if [[ -n "$pid" ]] && kill -0 "$pid" >/dev/null 2>&1; then
    kill "$pid" >/dev/null 2>&1 || true
    for _ in $(seq 1 20); do
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
fi

mapfile -t containers < <(docker ps -aq --filter "label=zeroship.sandbox")
if (( ${#containers[@]} > 0 )); then
  docker rm -f "${containers[@]}" >/dev/null
fi

if [[ "${1:-}" == "--postgres" ]]; then
  docker compose stop postgres
fi

if [[ "${SANDBOX_REMOVE_NETWORK:-0}" == "1" ]]; then
  docker network rm "$NETWORK" >/dev/null 2>&1 || true
fi

echo "Sandbox controller stopped; docker sandbox containers removed."
