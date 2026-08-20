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

# Print one line per thing the stack needs and does not have. Empty output means
# provisioned. It PRINTS rather than returning a bare status because the callers
# below have to say WHICH name is absent: "did not provision the complete set"
# named nothing, so a wrong list and a stale binary produced identical output.
#
# THE FILE HALF IS DERIVED FROM THE COMPOSE FILE, not listed here. A hand-kept
# copy is what broke this: `control-signing.pem` was on it until 2026-08-20,
# five days after 8e365f478 deleted control's PAT signing key outright (nothing
# but a PatIssuer read it) and dropped the file from `secret_specs()`, from both
# compose mounts and from crates/cli/tests/dev_init_test.rs. `dev init` stopped
# writing it, nothing here noticed, and this function returned 1 on every
# machine, always -- failing both callers before they started. The same list was
# ALSO missing `migrate-dsn`, which compose does mount, so it was wrong in both
# directions at once. The compose file is the artefact that states which files
# this stack will open, so it is the thing to ask; `secret_files()` in
# deploy/scripts/deploy-remote.sh asks it the same way for the same reason.
#
# WHAT THIS DOES NOT CATCH: presence and non-emptiness only. A file holding the
# wrong KIND of material passes here -- only `dev init`'s own per-secret
# validator rejects that. It also cannot see a secret a BINARY requires that
# compose never names; that gap is the deploy script's --check-config pass.
_dev_secrets_missing() {
  local env_file="$1" secrets_dir="$2" compose name file wanted
  compose="$(dirname "$secrets_dir")/docker-compose.yml"

  if [ ! -f "$env_file" ]; then
    printf '%s\n' "$env_file (no environment overlay at all)"
    return 0
  fi

  # ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET is NOT in this set: only Stripe issues a value that
  # verifies, so `dev init` does not generate one and compose defaults it to
  # empty. Control still refuses every webhook while it is empty.
  for name in \
    ZEROSHIP_CONTROL_KEY ZEROSHIP_CONTROL_MASTER_KEY ZEROSHIP_WORKER_KEY \
    ZEROSHIP_MIGRATED_POLICY_SEAL_KEY ZEROSHIP_GATEWAY_STASH_SIGNING_KEY \
    ZEROSHIP_PAIRWISE_SALT ZEROSHIP_AUTH_STASH_SIGNING_KEY ZEROSHIP_AUTH_TOTP_ENC_KEY; do
    grep -q "^${name}=" "$env_file" || printf '%s\n' "$name (variable)"
  done

  # Every `/etc/zeroship/secrets/<name>` in a live (non-comment) compose line.
  # The mount line itself ends the path at `secrets:` and cannot match: the
  # pattern needs a following slash and at least one name character.
  wanted="$(grep -vE '^[[:space:]]*#' "$compose" \
    | grep -oE '/etc/zeroship/secrets/[A-Za-z0-9._-]+' \
    | sed 's|.*/||' | sort -u)"
  if [ -z "$wanted" ]; then
    # Refuse to pass vacuously. An unreadable or restructured compose file makes
    # the loop below iterate zero times, and "no missing files" is exactly what
    # a check that has stopped looking reports.
    printf '%s\n' "$compose (no secret file references could be read out of it)"
    return 0
  fi
  for file in $wanted; do
    [ -s "$secrets_dir/$file" ] || printf '%s\n' "$file (secret file)"
  done
}

_dev_secrets_complete() {
  [ -z "$(_dev_secrets_missing "$1" "$2")" ]
}

ensure_dev_secrets() {
  local root env_file secrets_dir bin missing
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
      if "$bin" dev init; then
        missing="$(_dev_secrets_missing "$env_file" "$secrets_dir")"
        [ -n "$missing" ] || return 0
        echo "ensure_dev_secrets: $bin ran but these are still absent:" >&2
        echo "$missing" | sed 's/^/  /' >&2
      else
        echo "ensure_dev_secrets: $bin dev init failed" >&2
      fi
    fi
  done

  if command -v cargo >/dev/null 2>&1; then
    if ( cd "$root" && cargo run -q -p zeroship --bin zeroship -- dev init ); then
      missing="$(_dev_secrets_missing "$env_file" "$secrets_dir")"
      [ -n "$missing" ] || return 0
      echo "ensure_dev_secrets: cargo run dev init left these absent:" >&2
      echo "$missing" | sed 's/^/  /' >&2
    fi
  fi

  echo "ensure_dev_secrets: could not provision what the compose stack requires" >&2
  return 1
}
