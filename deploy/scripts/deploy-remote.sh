#!/usr/bin/env bash
#
# Build the platform image here, ship it to a remote host, and roll the stack.
#
#   deploy/scripts/deploy-remote.sh --host root@1.2.3.4 --registry ghcr.io/<owner>/zeroship-platform
#   deploy/scripts/deploy-remote.sh --host ... --registry ... --dry-run
#
# WHY THIS EXISTS, 2026-08-12. Deploying was 25 hand-run commands in
# docs/runbooks/deploy-server.md. Running them by hand once surfaced two
# failures that no amount of care would reliably avoid, and both are silent:
#
#   1. A RENAMED variable. `ZEROSHIP_SCHEME` became `ZEROSHIP_ORIGIN_SCHEME`.
#      The host still had the old name, and the new one defaults to `http`.
#      Compose renders happily; every public URL and the OIDC issuer quietly
#      turn into http://, and the symptom is a login loop with a clean log.
#      So this script REFUSES to deploy when the host is missing a variable
#      the new compose interpolates, rather than letting a default paper over
#      it. A default is the right behaviour for a fresh install and the wrong
#      behaviour for an upgrade, and only the operator knows which this is.
#
#   2. A variable that was never in `.env` at all. `ZEROSHIP_CONTROL_KEY` was
#      a hardcoded literal in four services and `${VAR}`-indirected in a
#      fifth, so setting it moved one service and desynced the stack. Every
#      service must therefore come up together; this script never does a
#      partial or rolling restart.
#
# SECURITY POSTURE
#   - No source ever reaches the host. It receives an image reference, a
#     compose file and a Caddyfile. That is the whole contract.
#   - Secrets are generated ON the host and never traverse this machine, are
#     never passed as arguments, and are never printed. Only names are logged.
#   - Generation is strictly additive: an existing value is KEPT, never
#     rotated. Rotating on deploy would invalidate issued tokens.
#   - Every mutated file is backed up first, and `--rollback` restores the
#     most recent backup of each and re-rolls. It restores all three or none:
#     a half-restored configuration is the failure it exists to undo.
#
# WHAT THIS DOES NOT DO: it does not manage DNS, TLS, the database, or
# migrations, and it does not verify the app beyond liveness. Point
# `--probe` at an app URL to assert real behaviour after the roll.

set -euo pipefail

# SOURCEABLE BY DESIGN. The helpers below are defined at the top level; every
# side effect lives in main(), which the guard at the bottom of the file calls
# only when this file is EXECUTED. `source deploy/scripts/deploy-remote.sh`
# therefore opens no ssh connection and parses no arguments, which is what lets
# tests/deploy_scripts_gate.sh drive compose_vars and rename_suspects directly.
# Those two produced two false results between them, both caught by hand; while
# they were welded to a script whose first act is `ssh`, there was no other way
# to test them than to run a deploy.

usage() {
  # BASH_SOURCE, not $0: when this file is sourced $0 is the SOURCING script,
  # and the help text would come out of whatever file called us.
  sed -n '2,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

say()  { printf '\n== %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

# Two things are NOT compose variables and must not be counted, both of which
# this check reported as missing on its first run against a healthy host:
#   - `$${VAR}` is an ESCAPED reference: compose emits a literal `$VAR` for the
#     container's own shell. `WORKERS` is one, built inside the gateway's
#     command block.
#   - a reference inside a comment. `ZEROSHIP_AUTH_PUBLIC_URL` appears only in prose.
# Counting either turns this guard into a false alarm that blocks a good
# deploy, which is how a safety check gets switched off.
#
# $1 selects the suffix to match ('' = every reference, ':-' = the ones with a
# compiled default). $2 is the compose file, defaulting to the one we ship;
# tests pass fixtures.
compose_vars() {
  grep -vE '^[[:space:]]*#' "${2:-deploy/compose/docker-compose.yml}" \
    | sed 's/\$\$[{]*[A-Za-z_]*[}]*//g' \
    | grep -oE "\\\$\\{[A-Z_]+$1" \
    | sed 's/\${//' | sed 's/:-$//' | sort -u
}

# Pair orphaned host variables ($1) with absent-but-defaulted ones ($2), and
# print one line per suspected rename. Empty output means no rename detected.
#
# Pair them only when the NAMES are actually related. "any orphan plus any
# defaulted variable" was the first attempt and it fired on a healthy host --
# orphans and defaulted variables coexist in normal steady state, so that
# version refused every deploy. A guard that always fails gets removed just as
# fast as one that never does.
#
# Relatedness = a shared underscore-separated token, minus a stoplist of tokens
# so common they carry no signal (the product name, service names, and the
# generic KEY/SECRET/URL suffixes). ZEROSHIP_SCHEME and ZEROSHIP_ORIGIN_SCHEME
# share SCHEME and pair; ZEROSHIP_GATEWAY_BROKER_SECRET_FILE and
# ZEROSHIP_GATEWAY_DATABASE_URL share only GATEWAY and do not.
rename_suspects() {
  local stopwords=" ZEROSHIP AUTH CONTROL GATEWAY WORKER MIGRATED DB DATABASE URL KEY SECRET "
  local o d tok
  for o in $1; do
    for d in $2; do
      for tok in $(echo "$o" | tr '_' ' '); do
        [ "${#tok}" -ge 4 ] || continue
        case "$stopwords" in *" $tok "*) continue ;; esac
        case "_${d}_" in
          *"_${tok}_"*) printf '\n    %s (host, now unused)  <->  %s (needed, would default)' "$o" "$d" ;;
        esac
      done
    done
  done
  return 0
}

main() {
  HOST=""
  REGISTRY=""
  REMOTE_DIR="/opt/zeroship-deploy"
  PROBE_URL=""
  DRY_RUN=0
  DO_ROLLBACK=0
  SKIP_BUILD=0
  IMAGE_OVERRIDE=""

  while [ $# -gt 0 ]; do
    case "$1" in
      --host)      HOST="$2"; shift 2 ;;
      --registry)  REGISTRY="$2"; shift 2 ;;
      --remote-dir) REMOTE_DIR="$2"; shift 2 ;;
      --probe)     PROBE_URL="$2"; shift 2 ;;
      --dry-run)   DRY_RUN=1; shift ;;
      --rollback)  DO_ROLLBACK=1; shift ;;
      --skip-build) SKIP_BUILD=1; shift ;;
      --image)     IMAGE_OVERRIDE="$2"; shift 2 ;;
      -h|--help)   usage 0 ;;
      *) echo "unknown argument: $1" >&2; usage 1 ;;
    esac
  done

  [ -n "$HOST" ] || { echo "--host is required" >&2; exit 2; }
  ROOT="$(git rev-parse --show-toplevel)"
  cd "$ROOT"

  # BatchMode: a deploy must never sit on an interactive prompt. If the agent
  # is not loaded this fails immediately instead of hanging a CI job.
  SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=15 -o ServerAliveInterval=5 -o ServerAliveCountMax=3)
  rsh() { ssh "${SSH_OPTS[@]}" "$HOST" "$@"; }

  # ---------------------------------------------------------------- preflight
  say "preflight"
  rsh true || fail "cannot ssh to $HOST (is the ssh agent loaded? ssh-add -l)"
  rsh "test -d '$REMOTE_DIR'" || fail "$REMOTE_DIR does not exist on $HOST"
  rsh "command -v docker >/dev/null" || fail "docker is not installed on $HOST"
  echo "ok  ssh, remote dir, docker"

  if [ "$DO_ROLLBACK" = 1 ]; then
    say "rollback"
    # PICK BY STAMP, NOT BY MTIME. This was `ls -1t | head -1`, and mtime is
    # the wrong key twice over. `cp -a` PRESERVES mtime, so a backup carries
    # the timestamp of the file's last content change, not the moment the
    # backup was taken; and anything that puts an older mtime back on the live
    # file -- this very rollback, or an operator's `cp -p` / `rsync -a` /
    # `tar -xp` out of an archive -- makes the NEXT backup look older than an
    # earlier one. On a tie (which `cp -a backup live` then a redeploy
    # produces exactly), GNU `ls -1t` breaks by name ascending, so `head -1`
    # returned the OLDEST stamp. The name is the only thing that actually
    # records when a backup was taken, so sort on that.
    #
    # The glob is [0-9]* rather than *: only this script's own STAMPED backups
    # have a position in that ordering. A hand-made `.env.bak.manual` sorts
    # after every digit and would win forever.
    # ALL THREE OR NONE. This used to print `no backup for <f>`, restore the
    # others anyway and restart the stack. An old .env against a new
    # docker-compose.yml is the exact pairing the top of this file exists to
    # prevent: the renamed variable takes its compiled default, compose
    # renders, every service comes up, and only behaviour like the OIDC issuer
    # changes. Doing that during a recovery and reporting success is worse
    # than refusing.
    #
    # "Some restore is better than none in an outage" is the obvious
    # objection, and it does not survive the question of WHEN a backup can be
    # absent: a deploy writes all three in one `set -e` block, so a missing one
    # means no deploy ever completed for that file and there is no coherent
    # state to return to. An operator who really wants one file back can cp it
    # by hand; this script must not do it silently on their behalf. So the
    # scan runs to completion BEFORE anything is copied.
    rsh "set -e
    cd '$REMOTE_DIR'
    files='compose/.env compose/docker-compose.yml ops/Caddyfile'
    newest() { ls -1d \"\$1\".bak.[0-9]* 2>/dev/null | sort | tail -1; }
    missing=''
    for f in \$files; do
      [ -n \"\$(newest \"\$f\")\" ] || missing=\"\$missing \$f\"
    done
    if [ -n \"\$missing\" ]; then
      echo \"no backup for:\$missing\" >&2
      echo 'REFUSING to roll back. Restoring only some of the three would pair an old .env with a new compose file, which is the silent mis-render this script exists to prevent, and then restart the stack on it. Nothing was changed.' >&2
      exit 3
    fi
    for f in \$files; do
      b=\"\$(newest \"\$f\")\"
      cp -a \"\$b\" \"\$f\"; echo \"restored \$f from \$b\"
    done
    cd compose && docker compose up -d --remove-orphans"
    say "rolled back"
    exit 0
  fi

  [ -n "$REGISTRY" ] || fail "--registry is required (e.g. ghcr.io/<owner>/zeroship-platform)"

  # The tag is the commit, so a running container is always traceable to a tree.
  # A dirty tree gets a marker: an image you cannot reproduce must not look like
  # one you can.
  SHA="$(git rev-parse --short=9 HEAD)"
  if ! git diff --quiet || ! git diff --cached --quiet; then
    SHA="${SHA}-dirty"
    echo "WARNING: working tree is dirty; tagging $SHA"
  fi
  IMAGE="$REGISTRY:$SHA"
  # --image pins an EXISTING reference: redeploying a known-good tag, or rolling
  # back to a prior one, must not depend on what this checkout happens to be at.
  if [ -n "$IMAGE_OVERRIDE" ]; then
    IMAGE="$IMAGE_OVERRIDE"
    SKIP_BUILD=1
  fi
  echo "ok  image will be $IMAGE"

  # ------------------------------------------------- required-variable contract
  # Read the variable surface out of the compose file we are about to ship, and
  # compare it against what the host already has. This is the check that would
  # have caught the ZEROSHIP_SCHEME rename.
  say "checking the host has every variable the new compose needs"
  REQUIRED="$(compose_vars '')"
  HOST_HAS="$(rsh "grep -ohE '^[A-Z_]+' '$REMOTE_DIR/compose/.env' 2>/dev/null | sort -u" || true)"

  # A variable with a `:-default` is optional by construction; only `:?` ones
  # and bare `${VAR}` ones can break a render or silently mis-render.
  OPTIONAL="$(compose_vars ':-')"
  GENERATED="$(sed -n 's/^ *"\([A-Z_]*\)",$/\1/p' crates/cli/src/dev.rs 2>/dev/null | sort -u)"

  MISSING=""
  DEFAULTED=""
  for v in $REQUIRED; do
    echo "$HOST_HAS" | grep -qx "$v" && continue
    # Generated secrets are provisioned below; they are not an operator problem.
    echo "$GENERATED" | grep -qx "$v" && continue
    if echo "$OPTIONAL" | grep -qx "$v"; then
      echo "note  $v is absent and will take its compiled default"
      DEFAULTED="$DEFAULTED $v"
    else
      MISSING="$MISSING $v"
    fi
  done

  # THE RENAME CHECK, and the reason the loop above is not sufficient.
  #
  # The first version of this script treated "absent but has a default" as a
  # note. Tested by deleting ZEROSHIP_ORIGIN_SCHEME from a healthy host, it
  # printed `ok no required variable is missing` -- the exact silent breakage
  # this guard exists to prevent, because the renamed variable HAS a default
  # (`:-http`). A guard that cannot fail on its own worked example is theatre.
  #
  # The signal that distinguishes a rename from a fresh install is an ORPHAN: a
  # variable the host sets that the new compose no longer reads. On its own an
  # orphan is harmless; paired with a defaulted variable it is the fingerprint
  # of a rename, and taking the default would silently change behaviour.
  ORPHANS=""
  for h in $HOST_HAS; do
    echo "$REQUIRED" | grep -qx "$h" && continue
    case "$h" in ZEROSHIP_IMAGE) continue ;; esac  # written by this script
    ORPHANS="$ORPHANS $h"
  done
  SUSPECT="$(rename_suspects "$ORPHANS" "$DEFAULTED")"
  if [ -n "$SUSPECT" ]; then
    fail "possible RENAME, refusing to guess:$SUSPECT
  Copy the VALUE across before deploying. Letting the default apply is silent:
  compose renders, the stack boots, and only a behaviour like the OIDC issuer
  or the cookie scheme changes. If they are unrelated, delete the stale name
  from the host .env to clear this."
  fi
  [ -z "$ORPHANS" ] || echo "note  host sets variables the new compose ignores:$ORPHANS"
  if [ -n "$MISSING" ]; then
    fail "host is missing required variables:$MISSING
  Set them in $REMOTE_DIR/compose/.env first. If one of these is a RENAME of a
  variable the host already has, copy the value across rather than letting a
  default apply -- that is the silent-breakage case this check exists for."
  fi
  echo "ok  no required variable is missing"

  if [ "$DRY_RUN" = 1 ]; then
    say "dry run: stopping before build"
    echo "would build and push $IMAGE, sync compose + Caddyfile, provision generated secrets, and roll the stack"
    exit 0
  fi

  # ------------------------------------------------------------------- build
  if [ "$SKIP_BUILD" = 0 ]; then
    say "building $IMAGE"
    # --target runtime: the builder stage carries the whole source tree and must
    # never be what we push.
    docker build --target runtime -t "$IMAGE" -f deploy/Dockerfile . \
      || fail "image build failed"
    echo "ok  built"

    say "pushing $IMAGE"
    docker push "$IMAGE" || fail "push failed (is docker logged in to the registry?)"
    echo "ok  pushed"
  else
    docker image inspect "$IMAGE" >/dev/null 2>&1 || fail "--skip-build but $IMAGE is not present locally"
    echo "ok  reusing local $IMAGE"
  fi

  # ------------------------------------------------------- ship configuration
  say "backing up and syncing host configuration"
  STAMP="$(date +%Y%m%d%H%M%S)"
  rsh "set -e
  cd '$REMOTE_DIR'
  cp -a compose/.env compose/.env.bak.$STAMP
  cp -a compose/docker-compose.yml compose/docker-compose.yml.bak.$STAMP
  cp -a ops/Caddyfile ops/Caddyfile.bak.$STAMP
  echo 'ok  backups tagged $STAMP'"

  scp "${SSH_OPTS[@]}" -q deploy/compose/docker-compose.yml "$HOST:$REMOTE_DIR/compose/docker-compose.yml"
  scp "${SSH_OPTS[@]}" -q deploy/ops/Caddyfile "$HOST:$REMOTE_DIR/ops/Caddyfile"
  echo "ok  compose + Caddyfile synced"

  # --------------------------------------------------------- provision secrets
  # Generated ON the host: the value never exists on this machine, so it cannot
  # leak into a local shell history, a log, or a process listing. Additive only.
  say "provisioning generated secrets (additive; existing values are kept)"
  GEN_LIST="$(echo "$GENERATED" | tr '\n' ' ')"
  [ -n "${GEN_LIST// /}" ] || fail "could not read the generated-secret list from crates/cli/src/dev.rs
  Refusing to continue: with an empty list this step would silently provision
  nothing and the render below would be the only thing standing between you
  and a half-configured stack."
  rsh "set -e
  ENV='$REMOTE_DIR/compose/.env'
  for k in $GEN_LIST; do
    if grep -q \"^\${k}=\" \"\$ENV\"; then
      echo \"  kept    \$k\"
    else
      printf '%s=%s\n' \"\$k\" \"\$(openssl rand -hex 32)\" >> \"\$ENV\"
      echo \"  created \$k\"
    fi
  done"

  # ------------------------------------------------------------ render + roll
  # Render BEFORE touching the running stack. The compose file declares its
  # secrets as \${VAR:?}, so a missing one fails here rather than half-starting.
  say "rendering the new configuration"
  rsh "cd '$REMOTE_DIR/compose' && docker compose config -q" \
    || fail "the new compose does not render on the host; nothing was restarted.
  Re-run with --rollback to restore the $STAMP backups."
  echo "ok  renders"

  say "rolling the stack to $IMAGE"
  # One `up -d` for every service. The control key is shared, so a partial
  # restart splits the stack into two halves that cannot authenticate.
  rsh "set -e
  cd '$REMOTE_DIR/compose'
  sed -i 's|^ZEROSHIP_IMAGE=.*|ZEROSHIP_IMAGE=$IMAGE|' .env
  grep -q '^ZEROSHIP_IMAGE=$IMAGE\$' .env || { echo 'ZEROSHIP_IMAGE was not updated'; exit 1; }
  docker compose pull -q 2>&1 | tail -3 || true
  docker compose up -d --remove-orphans" \
    || fail "the roll failed. Re-run with --rollback to restore the $STAMP backups."

  # ----------------------------------------------------------------- verify
  say "verifying"
  sleep 10

  BAD="$(rsh "cd '$REMOTE_DIR/compose'
  docker compose ps --format '{{.Name}}|{{.State}}' | awk -F'|' '\$2 != \"running\" {print \$1\" \"\$2}'" || true)"
  if [ -n "$BAD" ]; then
    printf 'FAIL: services not running:\n%s\n' "$BAD" >&2
    rsh "cd '$REMOTE_DIR/compose' && docker compose logs --tail=25 2>&1 | tail -40" >&2 || true
    fail "roll produced unhealthy services. Re-run with --rollback to restore $STAMP."
  fi
  echo "ok  every service is running"

  RUNNING_IMAGE="$(rsh "docker inspect \$(cd '$REMOTE_DIR/compose' && docker compose ps -q control | head -1) --format '{{.Config.Image}}'" || true)"
  [ "$RUNNING_IMAGE" = "$IMAGE" ] || fail "control is running '$RUNNING_IMAGE', not '$IMAGE'"
  echo "ok  control is running the image we just pushed"

  if [ -n "$PROBE_URL" ]; then
    say "probing $PROBE_URL"
    node examples/db-todos/scripts/probe-live.mjs "$PROBE_URL" \
      || fail "the app probe failed against the new deploy. Re-run with --rollback to restore $STAMP."
  fi

  say "deployed $IMAGE to $HOST"
  echo "backups tagged $STAMP; roll back with: $0 --host $HOST --rollback"
}

# Only run when EXECUTED. Sourcing this file must have no side effects: the
# gate sources it to reach compose_vars and rename_suspects.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then main "$@"; fi
