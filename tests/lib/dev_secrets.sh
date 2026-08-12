#!/usr/bin/env bash
#
# Ensure the compose stack has the generated secrets it now REQUIRES.
#
# WHY THIS EXISTS, found 2026-08-12 immediately after merging the provisioning
# and topology tracks. `deploy/compose/docker-compose.yml` stopped shipping weak
# literals and now declares its secrets as required interpolations:
#
#     ZEROSHIP_CONTROL_KEY: ${ZEROSHIP_CONTROL_KEY:?run zeroship dev init}
#
# That is the point of the change - a stack with no real secrets must refuse to
# render rather than boot on `platform-key`. The cost is that EVERY harness
# which renders compose now fails on a clean checkout:
#
#     error while interpolating services.worker.environment.ZEROSHIP_CONTROL_KEY:
#       required variable ZEROSHIP_CONTROL_KEY is missing a value: run zeroship dev init
#
# MEASURED on a worktree with no `deploy/compose/.env`: `docker compose config`
# exits non-zero. Two harnesses reach `docker compose up` and would have failed
# in CI the same way (`tests/e2e_docker.sh`, `tests/external_chain.sh`).
#
# Neither implementation track owned this seam: one made the secrets required,
# the other never rendered compose, and the break only exists once both are on
# the same branch. That is the shape to expect from parallel tracks, so the fix
# belongs in a shared helper rather than in one harness.
#
# WHAT THIS DOES NOT DO: it never rotates. `zeroship dev init` is idempotent and
# reports `0 created, N kept`, so an operator's existing local secrets survive a
# test run. It also does not provision anything for a REMOTE deployment; that is
# the operator's own secrets directory (docs/runbooks/deploy-server.md).

_dev_secrets_complete() {
  local env_file="$1" secrets_dir="$2" name file
  [ -f "$env_file" ] || return 1
  # STRIPE_WEBHOOK_SECRET is NOT in this set: only Stripe issues a value that
  # verifies, so `dev init` does not generate one and compose defaults it to
  # empty. Control still refuses every webhook while it is empty.
  for name in \
    ZEROSHIP_CONTROL_KEY ZEROSHIP_MASTER_KEY ZEROSHIP_WORKER_KEY \
    GATEWAY_OIDC_SECRET MIGRATED_POLICY_SEAL_KEY STASH_SIGNING_KEY \
    PAIRWISE_SALT AUTH_STASH_SIGNING_KEY AUTH_TOTP_ENC_KEY; do
    grep -q "^${name}=" "$env_file" 2>/dev/null || return 1
  done
  for file in \
    auth-signing.pem gateway-signing.pem control-signing.pem broker-secret \
    refresh-hash-key refresh-idem-key pairwise-salt; do
    [ -s "$secrets_dir/$file" ] || return 1
  done
}

ensure_dev_secrets() {
  local root env_file secrets_dir bin
  # Resolve the repo root from git, not from path arithmetic on BASH_SOURCE:
  # inside a function that expands to the DEFINING file, which differs by how
  # the caller sourced this, and a wrong root silently provisions nothing.
  root="$(git rev-parse --show-toplevel 2>/dev/null)"
  [ -n "$root" ] || root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
  env_file="$root/deploy/compose/.env"
  secrets_dir="$root/deploy/compose/secrets"

  # Already provisioned: do nothing, so a run cannot disturb local state.
  if _dev_secrets_complete "$env_file" "$secrets_dir"; then
    return 0
  fi

  # Prefer an already-built binary over a cargo build, but verify its output
  # against the complete current contract before trusting a possibly stale bin.
  for bin in "$root/target/debug/zeroship" "$root/target/release/zeroship"; do
    if [ -x "$bin" ]; then
      if "$bin" dev init && _dev_secrets_complete "$env_file" "$secrets_dir"; then
        return 0
      fi
      echo "ensure_dev_secrets: $bin did not provision the complete current secret set" >&2
    fi
  done

  if command -v cargo >/dev/null 2>&1; then
    if ( cd "$root" && cargo run -q -p zeroship --bin zeroship -- dev init ) &&
       _dev_secrets_complete "$env_file" "$secrets_dir"; then
      return 0
    fi
  fi

  echo "ensure_dev_secrets: could not provision the complete current secret set" >&2
  return 1
}
