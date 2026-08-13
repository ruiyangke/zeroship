#!/usr/bin/env bash
# Generated inputs for shell harnesses that boot the multi-service platform.
#
# Source this file and call `e2e_export_runtime_secrets "$WORK"` after the
# harness has assigned its own key variables. Existing strong values are kept;
# missing or weak fixture values are replaced. The function exports the real
# service inputs, so every normal startup guard remains active.
#
# EVERY name below is the canonical `ZEROSHIP_` projection of a declared
# setting, and there is no second spelling. The bare middle names this file
# used to export - the unprefixed control-key, master-key and signing-key-file
# spellings - are not read by any binary any more: Step 5 of
# docs/proposals/2026-08-11-config-name-alignment.md deleted them along with
# the value flags that shared them. Adding one back here would not restore a
# fallback, it would just set a variable nothing reads, and the harness would
# fail at the startup guard several hundred lines later.
#
# The names are DISTINCT per service by construction, so exporting the whole
# set into one shell is safe: each binary reads only its own scope, and a
# gateway process ignores `ZEROSHIP_AUTH_*` entirely. That is what makes a
# single `e2e_export_runtime_secrets` call able to configure a five-service
# stack without the harness restating anything.

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

# e2e_export_database_urls <dsn>
#
# One DSN, five canonical names. The services' database roles are DISTINCT
# settings - control writes the registry, the gateway only reads sessions, the
# worker runs creator SQL - so each has its own identity and there is no shared
# middle name to point them all at. A harness that runs the whole stack against
# one ephemeral Postgres says so once, here, instead of restating a `--db` on
# every launch line. Pass a per-service DSN by setting that one name after this
# call; the assignments below are plain and unconditional so a caller can see
# what it is overriding.
e2e_export_database_urls() {
  local dsn="${1:-}"
  [ -n "$dsn" ] || {
    echo "e2e_export_database_urls: a DSN is required" >&2
    return 1
  }
  ZEROSHIP_CONTROL_DATABASE_URL="$dsn"
  ZEROSHIP_GATEWAY_DATABASE_URL="$dsn"
  ZEROSHIP_WORKER_DATABASE_URL="$dsn"
  ZEROSHIP_AUTH_DATABASE_URL="$dsn"
  ZEROSHIP_MIGRATED_DATABASE_URL="$dsn"
  ZEROSHIP_MIGRATED_PROVISION_DATABASE_URL="$dsn"
  ZEROSHIP_WORKFLOW_SCHEDULER_DATABASE_URL="$dsn"
  export ZEROSHIP_CONTROL_DATABASE_URL ZEROSHIP_GATEWAY_DATABASE_URL
  export ZEROSHIP_WORKER_DATABASE_URL ZEROSHIP_AUTH_DATABASE_URL
  export ZEROSHIP_MIGRATED_DATABASE_URL ZEROSHIP_MIGRATED_PROVISION_DATABASE_URL
  export ZEROSHIP_WORKFLOW_SCHEDULER_DATABASE_URL
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

  _e2e_keep_or_generate ZEROSHIP_CONTROL_KEY "${ZEROSHIP_CONTROL_KEY:-}" || return 1
  _e2e_keep_or_generate_hex_key ZEROSHIP_CONTROL_MASTER_KEY \
    "${ZEROSHIP_CONTROL_MASTER_KEY:-}" || return 1
  _e2e_keep_or_generate ZEROSHIP_WORKER_KEY "${ZEROSHIP_WORKER_KEY:-}" || return 1
  _e2e_keep_or_generate ZEROSHIP_PAIRWISE_SALT "${ZEROSHIP_PAIRWISE_SALT:-}" || return 1
  _e2e_keep_or_generate ZEROSHIP_MIGRATED_POLICY_SEAL_KEY \
    "${ZEROSHIP_MIGRATED_POLICY_SEAL_KEY:-}" || return 1
  _e2e_keep_or_generate_hex_key ZEROSHIP_AUTH_TOTP_ENC_KEY \
    "${ZEROSHIP_AUTH_TOTP_ENC_KEY:-}" || return 1

  # The stash HMAC is one deployment fact with two canonical consumers: the
  # gateway mints the cookie and the OP verifies it, so the two names carry
  # identical material. They are separate settings because the two services
  # can be rolled independently, not because the value may differ.
  _e2e_keep_or_generate ZEROSHIP_GATEWAY_STASH_SIGNING_KEY \
    "${ZEROSHIP_GATEWAY_STASH_SIGNING_KEY:-}" || return 1
  ZEROSHIP_AUTH_STASH_SIGNING_KEY="${ZEROSHIP_AUTH_STASH_SIGNING_KEY:-$ZEROSHIP_GATEWAY_STASH_SIGNING_KEY}"
  export ZEROSHIP_AUTH_STASH_SIGNING_KEY

  if ! _e2e_strong_value "${ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET:-}"; then
    ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET="whsec_$(openssl rand -hex 32)" || return 1
  fi
  export ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET

  ZEROSHIP_AUTH_PLATFORM_ISSUER="${ZEROSHIP_AUTH_PLATFORM_ISSUER:-http://localhost:${AUTH_PORT:-9092}/oauth2}"
  ZEROSHIP_ORIGIN_SCHEME="${ZEROSHIP_ORIGIN_SCHEME:-http}"
  ZEROSHIP_CONTROL_ALLOW_UNSUPPORTED_BILLING="${ZEROSHIP_CONTROL_ALLOW_UNSUPPORTED_BILLING:-true}"
  export ZEROSHIP_AUTH_PLATFORM_ISSUER ZEROSHIP_ORIGIN_SCHEME ZEROSHIP_CONTROL_ALLOW_UNSUPPORTED_BILLING

  # Control mints the PAT/session token the gateway verifies, so both services
  # get the same Ed25519 file. `signing_key_file` is Operational<PathBuf> on
  # every consumer: a path to key material is not itself key material.
  ZEROSHIP_CONTROL_SIGNING_KEY_FILE="${ZEROSHIP_CONTROL_SIGNING_KEY_FILE:-$secret_dir/platform-signing.pem}"
  if [ ! -s "$ZEROSHIP_CONTROL_SIGNING_KEY_FILE" ]; then
    openssl genpkey -algorithm ed25519 -out "$ZEROSHIP_CONTROL_SIGNING_KEY_FILE" 2>/dev/null || return 1
  fi
  chmod 600 "$ZEROSHIP_CONTROL_SIGNING_KEY_FILE"
  ZEROSHIP_GATEWAY_SIGNING_KEY_FILE="${ZEROSHIP_GATEWAY_SIGNING_KEY_FILE:-$ZEROSHIP_CONTROL_SIGNING_KEY_FILE}"
  ZEROSHIP_MIGRATED_SIGNING_KEY_FILE="${ZEROSHIP_MIGRATED_SIGNING_KEY_FILE:-$ZEROSHIP_CONTROL_SIGNING_KEY_FILE}"
  export ZEROSHIP_CONTROL_SIGNING_KEY_FILE ZEROSHIP_GATEWAY_SIGNING_KEY_FILE
  export ZEROSHIP_MIGRATED_SIGNING_KEY_FILE

  ZEROSHIP_GATEWAY_BROKER_SECRET_FILE="${ZEROSHIP_GATEWAY_BROKER_SECRET_FILE:-$secret_dir/gateway-broker-secret}"
  _e2e_secret_file "$ZEROSHIP_GATEWAY_BROKER_SECRET_FILE" || return 1
  export ZEROSHIP_GATEWAY_BROKER_SECRET_FILE

  ZEROSHIP_AUTH_SIGNING_KEY_FILE="${ZEROSHIP_AUTH_SIGNING_KEY_FILE:-$secret_dir/auth-signing.pem}"
  if [ ! -s "$ZEROSHIP_AUTH_SIGNING_KEY_FILE" ]; then
    openssl genpkey -algorithm ed25519 -out "$ZEROSHIP_AUTH_SIGNING_KEY_FILE" 2>/dev/null || return 1
  fi
  chmod 600 "$ZEROSHIP_AUTH_SIGNING_KEY_FILE"
  export ZEROSHIP_AUTH_SIGNING_KEY_FILE

  ZEROSHIP_AUTH_PAIRWISE_SALT_FILE="${ZEROSHIP_AUTH_PAIRWISE_SALT_FILE:-$secret_dir/auth-pairwise-salt}"
  ZEROSHIP_AUTH_BROKER_SECRET_FILE="${ZEROSHIP_AUTH_BROKER_SECRET_FILE:-$ZEROSHIP_GATEWAY_BROKER_SECRET_FILE}"
  ZEROSHIP_AUTH_REFRESH_HASH_KEY_FILE="${ZEROSHIP_AUTH_REFRESH_HASH_KEY_FILE:-$secret_dir/refresh-hash-key}"
  ZEROSHIP_AUTH_REFRESH_IDEM_KEY_FILE="${ZEROSHIP_AUTH_REFRESH_IDEM_KEY_FILE:-$secret_dir/refresh-idem-key}"
  printf '%s' "$ZEROSHIP_PAIRWISE_SALT" > "$ZEROSHIP_AUTH_PAIRWISE_SALT_FILE" || return 1
  chmod 600 "$ZEROSHIP_AUTH_PAIRWISE_SALT_FILE"
  # The OP and gateway derive per-client broker secrets from identical bytes.
  # Keep an explicit auth override when a harness supplied one; otherwise both
  # services consume the gateway file generated above.
  _e2e_secret_file "$ZEROSHIP_AUTH_BROKER_SECRET_FILE" || return 1
  if ! grep -Eq '^[1-9][0-9]*:[A-Za-z0-9_-]{43,}$' "$ZEROSHIP_AUTH_REFRESH_HASH_KEY_FILE" 2>/dev/null; then
    printf '1:%s' "$(openssl rand -hex 32)" > "$ZEROSHIP_AUTH_REFRESH_HASH_KEY_FILE" || return 1
  fi
  chmod 600 "$ZEROSHIP_AUTH_REFRESH_HASH_KEY_FILE"
  _e2e_secret_file "$ZEROSHIP_AUTH_REFRESH_IDEM_KEY_FILE" || return 1
  export ZEROSHIP_AUTH_PAIRWISE_SALT_FILE ZEROSHIP_AUTH_BROKER_SECRET_FILE
  export ZEROSHIP_AUTH_REFRESH_HASH_KEY_FILE ZEROSHIP_AUTH_REFRESH_IDEM_KEY_FILE
}
