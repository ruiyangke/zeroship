#!/usr/bin/env bash
# Generated inputs for shell harnesses that boot the multi-service platform.
#
# Source this file and call `e2e_export_runtime_secrets "$WORK"` after the
# harness has assigned its own key variables. Existing strong values are kept;
# missing or weak fixture values are replaced. The function exports the real
# service inputs, so every normal startup guard remains active.

_e2e_strong_value() {
  local value="${1:-}"
  [ "${#value}" -ge 32 ] || return 1
  case "$value" in
    platform-key|dev-secret|dev-stash-key-please-rotate|\
    dev-stash-signing-key-not-for-production|\
    dev-only-stash-signing-key-not-for-production-use\!\!|\
    dev-pairwise-salt-never-rotate-in-prod|\
    dev-worker-key-not-for-production-use)
      return 1
      ;;
  esac
  return 0
}

_e2e_keep_or_generate() {
  local name="$1" current="${2:-}" generated
  if _e2e_strong_value "$current"; then
    printf -v "$name" '%s' "$current"
  else
    generated="$(openssl rand -hex 32)" || return 1
    printf -v "$name" '%s' "$generated"
  fi
  export "$name"
}

_e2e_keep_or_generate_hex_key() {
  local name="$1" current="${2:-}" generated
  if [[ "$current" =~ ^[0-9a-fA-F]{64}$ ]] &&
     [ "$current" != "00000000000000000000000000000000000000000000000000000000000000ff" ]; then
    printf -v "$name" '%s' "$current"
  else
    generated="$(openssl rand -hex 32)" || return 1
    printf -v "$name" '%s' "$generated"
  fi
  export "$name"
}

_e2e_secret_file() {
  local path="$1" current_hex sentinel_hex
  current_hex="$(od -An -tx1 -v "$path" 2>/dev/null | tr -d ' \n')"
  sentinel_hex="$(printf '%s' 'dev-broker-master-secret-never-use-prod' | od -An -tx1 -v | tr -d ' \n')"
  if [ ! -s "$path" ] ||
     [ "$(wc -c < "$path" 2>/dev/null || echo 0)" -lt 32 ] ||
     [ "$current_hex" = "$sentinel_hex" ] ||
     [ -z "${current_hex//0/}" ]; then
    openssl rand -base64 48 > "$path" || return 1
  fi
  chmod 600 "$path"
}

e2e_export_runtime_secrets() {
  local secret_dir="$1"
  [ -n "$secret_dir" ] || {
    echo "e2e_export_runtime_secrets: a workspace directory is required" >&2
    return 1
  }
  command -v openssl >/dev/null 2>&1 || {
    echo "e2e_export_runtime_secrets: openssl is required" >&2
    return 1
  }
  mkdir -p "$secret_dir"

  _e2e_keep_or_generate CONTROL_KEY "${CONTROL_KEY:-}" || return 1
  _e2e_keep_or_generate_hex_key MASTER_KEY "${MASTER_KEY:-}" || return 1
  _e2e_keep_or_generate WORKER_KEY "${WORKER_KEY:-}" || return 1
  _e2e_keep_or_generate STASH_SIGNING_KEY "${STASH_SIGNING_KEY:-}" || return 1
  _e2e_keep_or_generate PAIRWISE_SALT "${PAIRWISE_SALT:-}" || return 1
  _e2e_keep_or_generate MIGRATED_POLICY_SEAL_KEY "${MIGRATED_POLICY_SEAL_KEY:-}" || return 1
  _e2e_keep_or_generate_hex_key AUTH_TOTP_ENC_KEY "${AUTH_TOTP_ENC_KEY:-}" || return 1

  if ! _e2e_strong_value "${STRIPE_WEBHOOK_SECRET:-}"; then
    STRIPE_WEBHOOK_SECRET="whsec_$(openssl rand -hex 32)" || return 1
  fi
  export STRIPE_WEBHOOK_SECRET

  AUTH_PLATFORM_ISSUER="${AUTH_PLATFORM_ISSUER:-http://localhost:${AUTH_PORT:-9092}/oauth2}"
  ZEROSHIP_ORIGIN_SCHEME="${ZEROSHIP_ORIGIN_SCHEME:-http}"
  ZEROSHIP_CONTROL_ALLOW_UNSUPPORTED_BILLING="${ZEROSHIP_CONTROL_ALLOW_UNSUPPORTED_BILLING:-true}"
  export AUTH_PLATFORM_ISSUER ZEROSHIP_ORIGIN_SCHEME ZEROSHIP_CONTROL_ALLOW_UNSUPPORTED_BILLING

  SIGNING_KEY_FILE="${SIGNING_KEY_FILE:-$secret_dir/platform-signing.pem}"
  if [ ! -s "$SIGNING_KEY_FILE" ]; then
    openssl genpkey -algorithm ed25519 -out "$SIGNING_KEY_FILE" 2>/dev/null || return 1
  fi
  chmod 600 "$SIGNING_KEY_FILE"
  GATEWAY_SIGNING_KEY_FILE="${GATEWAY_SIGNING_KEY_FILE:-$SIGNING_KEY_FILE}"
  export SIGNING_KEY_FILE GATEWAY_SIGNING_KEY_FILE

  GATEWAY_BROKER_SECRET_FILE="${GATEWAY_BROKER_SECRET_FILE:-$secret_dir/gateway-broker-secret}"
  _e2e_secret_file "$GATEWAY_BROKER_SECRET_FILE" || return 1
  export GATEWAY_BROKER_SECRET_FILE

  AUTH_SIGNING_KEY_FILE="${AUTH_SIGNING_KEY_FILE:-$secret_dir/auth-signing.pem}"
  if [ ! -s "$AUTH_SIGNING_KEY_FILE" ]; then
    openssl genpkey -algorithm ed25519 -out "$AUTH_SIGNING_KEY_FILE" 2>/dev/null || return 1
  fi
  chmod 600 "$AUTH_SIGNING_KEY_FILE"
  export AUTH_SIGNING_KEY_FILE

  AUTH_PAIRWISE_SALT_FILE="${AUTH_PAIRWISE_SALT_FILE:-$secret_dir/auth-pairwise-salt}"
  AUTH_BROKER_SECRET_FILE="${AUTH_BROKER_SECRET_FILE:-$GATEWAY_BROKER_SECRET_FILE}"
  REFRESH_HASH_KEY_FILE="${REFRESH_HASH_KEY_FILE:-$secret_dir/refresh-hash-key}"
  REFRESH_IDEM_KEY_FILE="${REFRESH_IDEM_KEY_FILE:-$secret_dir/refresh-idem-key}"
  printf '%s' "$PAIRWISE_SALT" > "$AUTH_PAIRWISE_SALT_FILE" || return 1
  chmod 600 "$AUTH_PAIRWISE_SALT_FILE"
  # The OP and gateway derive per-client broker secrets from identical bytes.
  # Keep an explicit auth override when a harness supplied one; otherwise both
  # services consume the gateway file generated above.
  _e2e_secret_file "$AUTH_BROKER_SECRET_FILE" || return 1
  if ! grep -Eq '^[1-9][0-9]*:[A-Za-z0-9_-]{43,}$' "$REFRESH_HASH_KEY_FILE" 2>/dev/null; then
    printf '1:%s' "$(openssl rand -hex 32)" > "$REFRESH_HASH_KEY_FILE" || return 1
  fi
  chmod 600 "$REFRESH_HASH_KEY_FILE"
  _e2e_secret_file "$REFRESH_IDEM_KEY_FILE" || return 1
  export AUTH_PAIRWISE_SALT_FILE AUTH_BROKER_SECRET_FILE
  export REFRESH_HASH_KEY_FILE REFRESH_IDEM_KEY_FILE

  AUTH_STASH_SIGNING_KEY="${AUTH_STASH_SIGNING_KEY:-$STASH_SIGNING_KEY}"
  export AUTH_STASH_SIGNING_KEY
}
