#!/usr/bin/env bash
# Shared real-relay fixture for platform E2E harnesses.

e2e_start_cdc_relay() {
  local binary="$1" directory dsn relay_port ready pid status
  if [ -n "${E2E_CDC_RELAY_PID:-}" ] && kill -0 "$E2E_CDC_RELAY_PID" 2>/dev/null; then
    return 0
  fi
  [ -n "${ZEROSHIP_WORKER_DATABASE_URL:-}" ] || return 0
  [ -x "$binary" ] || {
    echo "required relay binary missing: cargo build --release -p zeroship-data-cdc-server" >&2
    return 1
  }
  if [ -z "${PIDFILE:-}" ] && ! declare -p PIDS >/dev/null 2>&1; then
    echo "relay fixture requires the harness PIDFILE or PIDS cleanup registry" >&2
    return 1
  fi
  directory="$(dirname "${ZEROSHIP_WORKER_ENROLLER_FILE:?worker enroller credential is required}")/cdc"
  bash "$E2E_ROOT/deploy/ops/init-cdc-tls.sh" "$directory" localhost >"${directory%/cdc}/cdc-tls.log" 2>&1 || return 1
  if ! declare -F zs_ports_reserve >/dev/null; then
    source "$E2E_ROOT/tests/lib/e2e_ports.sh"
  fi
  zs_ports_reserve relay_port || return 1
  dsn="$(_e2e_database_url_for_role "$ZEROSHIP_WORKER_DATABASE_URL" zeroship_cdc zeroship_cdc)" || return 1
  ZEROSHIP_DATA_CDC_SERVER_DATABASE_URL="$dsn" "$binary" --no-config \
    --listen "127.0.0.1:$relay_port" \
    --tls-cert-file "$directory/cdc-cert.pem" --tls-key-file "$directory/cdc-key.pem" \
    > "$directory/relay.log" 2>&1 &
  pid=$!
  E2E_CDC_RELAY_PID="$pid"
  if [ -n "${PIDFILE:-}" ]; then
    printf '%s\n' "$pid" >> "$PIDFILE"
  else
    PIDS+=("$pid")
  fi
  export ZEROSHIP_WORKER_CDC_RELAY_URL="wss://localhost:$relay_port/internal/v1/cdc/subscribe"
  export ZEROSHIP_WORKER_CDC_RELAY_CA_FILE="$directory/cdc-cert.pem"
  for ready in $(seq 1 100); do
    if ! kill -0 "$pid" 2>/dev/null; then
      cat "$directory/relay.log" >&2
      return 1
    fi
    status=$(curl --silent --http1.1 --cacert "$directory/cdc-cert.pem" --max-time 1 \
      --header 'Connection: Upgrade' --header 'Upgrade: websocket' \
      --header 'Sec-WebSocket-Version: 13' \
      --header 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' \
      --output /dev/null --write-out '%{http_code}' \
      "https://localhost:$relay_port/internal/v1/cdc/subscribe" || true)
    if [ "$status" = 101 ]; then
      return 0
    fi
    sleep 0.1
  done
  echo "CDC relay failed to bind its TLS endpoint" >&2
  cat "$directory/relay.log" >&2
  return 1
}
