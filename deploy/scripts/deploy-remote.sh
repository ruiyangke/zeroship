#!/usr/bin/env bash
#
# Build the platform image here, ship it to a remote host, and roll the stack.
#
#   deploy/scripts/deploy-remote.sh --host root@1.2.3.4 --registry ghcr.io/<owner>/zeroship-platform
#   deploy/scripts/deploy-remote.sh --host ... --registry ... --dry-run
#   deploy/scripts/deploy-remote.sh --host ... --list-backups
#   deploy/scripts/deploy-remote.sh --host ... --rollback
#   deploy/scripts/deploy-remote.sh --host ... --rollback-to 20260813120000
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
# AND THREE MORE, 2026-08-13, each of which took the site down on its own
# during one roll of a new image:
#
#   3. `ops/zeroship.toml` was never synced. The host kept an overlay written
#      for an older binary; the new ones parse it with `deny_unknown_fields`
#      and every service refused to start on `unknown field rust_log`. The
#      overlay is now shipped like the compose file and the Caddyfile, AND
#      every server's `--check-config` is run against the rendered
#      configuration BEFORE the stack is touched.
#
#   4. Provisioning only ever created ENV-shaped secrets, because it read its
#      list out of `crates/cli/src/dev.rs` `ENV_KEYS`. The binaries also need
#      seven secret FILES, and nothing created them, so control refused to
#      start on a missing `--signing-key-file`. Provisioning now runs the
#      deployed image's own `zeroship dev init`, which owns BOTH lists, and
#      the file set the new compose actually references is verified after.
#
#   5. `--rollback` did not roll back. It restored three files, never touched
#      the overlay or the secrets, and picked each file's newest backup
#      INDEPENDENTLY, so a partial generation could be stitched together. A
#      failed deploy therefore became an outage. It now snapshots every input
#      a deploy mutates under ONE stamp, restores a COMPLETE generation or
#      none, and verifies the restored configuration renders before it
#      restarts anything.
#
# SECURITY POSTURE
#   - No source ever reaches the host. It receives an image reference, a
#     compose file, a Caddyfile and the ops overlay. That is the whole
#     contract.
#   - Secrets are generated ON the host and never traverse this machine, are
#     never passed as arguments, and are never printed. Only names are logged.
#   - Generation is strictly additive: an existing value is KEPT, never
#     rotated. Rotating a signing key invalidates issued tokens; rotating the
#     master key destroys encrypted data.
#   - Every mutated input is backed up first under one stamp, and `--rollback`
#     restores a complete generation or refuses: a half-restored
#     configuration is the failure it exists to undo.
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
  sed -n '2,70p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
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
# Two independent rules; a pair matching EITHER is reported.
#
# Rule 1, shared token. Relatedness = a shared underscore-separated token, minus
# a stoplist of tokens so common they carry no signal (the product name, service
# names, and the generic KEY/SECRET/URL suffixes). ZEROSHIP_SCHEME and
# ZEROSHIP_ORIGIN_SCHEME share SCHEME and pair; ZEROSHIP_GATEWAY_BROKER_SECRET_FILE
# and ZEROSHIP_GATEWAY_DATABASE_URL share only GATEWAY and do not.
#
# Rule 2, canonical re-scoping. Rule 1 alone was BLIND to the exact rename this
# guard is most likely to meet: giving a bare deployment name its canonical
# ZEROSHIP_<scope>_ prefix. Measured 2026-08-13 over the eight compose aliases
# that were renamed to satisfy the alias-equality rule, rule 1 fired on two and
# was SILENT on six -- CONTROL_DATABASE_URL -> ZEROSHIP_CONTROL_DATABASE_URL
# among them, because every token it owns (CONTROL, DATABASE, URL) is
# stoplisted. Silence there is the expensive direction: the value has a compose
# default, so the stack boots green with the control plane pointed at the
# built-in database instead of the operator's.
#
# So: strip a leading ZEROSHIP_ from both, expand the DB abbreviation to
# DATABASE, and pair when the needed name equals the orphan or ends with
# _<orphan>. Requiring the orphan to hold at least one underscore keeps a single
# generic token (URL, KEY) from pairing with everything.
#
# Over-firing is cheap and under-firing is not: a false pair prints a refusal an
# operator clears by deleting one stale line, while a missed pair silently
# changes what the stack points at.
# The secret FILES the new compose actually references, as bare names.
#
# WHY THIS IS DERIVED FROM THE COMPOSE FILE and not from a list kept here.
# Defect 4 above is exactly what a second, hand-maintained list produces: the
# provisioning step read `ENV_KEYS` out of the CLI source, that list is
# env-shaped by construction, and the seven secret FILES the servers require
# were in nobody's list at all. The compose file is the only artefact that
# states which files this deployment's binaries will open, because it is the
# thing that passes the paths, so it is the thing to ask.
#
# The container path is fixed: every service that needs key material mounts
# the host secrets directory at /etc/zeroship/secrets, so a reference is any
# `/etc/zeroship/secrets/<name>` in a live (non-comment) line. The mount line
# itself ends the path at `secrets:` and so cannot match -- the pattern needs
# a following slash and at least one name character.
#
# $1 is the compose file, defaulting to the one we ship; tests pass fixtures.
secret_files() {
  grep -vE '^[[:space:]]*#' "${1:-deploy/compose/docker-compose.yml}" \
    | grep -oE '/etc/zeroship/secrets/[A-Za-z0-9._-]+' \
    | sed 's|.*/||' | sort -u
}

rename_norm() {
  local n="${1#ZEROSHIP_}"
  case "$n" in
    *_DB_*) n="${n%%_DB_*}_DATABASE_${n#*_DB_}" ;;
    *_DB) n="${n%_DB}_DATABASE" ;;
  esac
  printf '%s' "$n"
}

rename_suspects() {
  local stopwords=" ZEROSHIP AUTH CONTROL GATEWAY WORKER MIGRATED DB DATABASE URL KEY SECRET "
  local o d tok on dn hit
  for o in $1; do
    on="$(rename_norm "$o")"
    for d in $2; do
      hit=""
      for tok in $(echo "$o" | tr '_' ' '); do
        [ "${#tok}" -ge 4 ] || continue
        case "$stopwords" in *" $tok "*) continue ;; esac
        case "_${d}_" in
          *"_${tok}_"*) hit=1 ;;
        esac
      done
      if [ -z "$hit" ] && [ "${on#*_}" != "$on" ]; then
        dn="$(rename_norm "$d")"
        case "$dn" in
          "$on"|*"_$on") hit=1 ;;
        esac
      fi
      [ -n "$hit" ] && printf '\n    %s (host, now unused)  <->  %s (needed, would default)' "$o" "$d"
    done
  done
  return 0
}

# THE SNAPSHOT SET: every input a deploy mutates, and therefore every input a
# rollback must put back. `secrets.tar` is a GNU tar taken with absolute paths
# (-P) so it restores to wherever ZEROSHIP_SECRETS_DIR pointed when it was
# taken, without the rollback having to re-derive that path from a .env it is
# in the middle of replacing.
#
# ops/zeroship.toml is on this list because a deploy now SYNCS it. It was
# absent from both halves before, which is how a stale overlay survived a roll
# and then survived the rollback too.
SNAPSHOT_MEMBERS="compose/.env compose/docker-compose.yml ops/Caddyfile ops/zeroship.toml secrets.tar"

# The four members that are plain host files and must exist before a deploy can
# back them up. secrets.tar is generated, so it is not in this list.
SNAPSHOT_FILES="compose/.env compose/docker-compose.yml ops/Caddyfile ops/zeroship.toml"

# The five long-running servers, as `<compose service>:<binary>`. Each one's
# `--check-config` is run against the rendered environment and the mounted
# overlay before the stack is touched. The `migrate` one-shot is absent because
# `zeroship-platform-migrate` has no --check-config and mounts no overlay.
CHECK_SERVICES="control:zeroship-control migrated:zeroship-migrated gateway:zeroship-gate worker:zeroship-worker auth:zeroship-auth"

# A remote-shell fragment that sets $SEC to the host's secrets directory, run
# from $REMOTE_DIR. Three separate ssh bodies need it -- the snapshot, the
# provisioner and the file check -- and three copies of a path derivation is
# three chances for two of them to point somewhere the third does not.
#
# Where the secrets live is the compose file's business: it bind-mounts
# ${ZEROSHIP_SECRETS_DIR:-./secrets} relative to the compose directory, and a
# relative value resolves from there, not from $REMOTE_DIR. Reading the .env
# rather than assuming the default matters: on a host that sets it (the runbook
# tells operators to) the assumption would archive an empty directory while the
# real key material sat somewhere else, and report success.
SEC_RESOLVE='SEC=$(sed -n "s/^ZEROSHIP_SECRETS_DIR=//p" compose/.env | tail -1)
  [ -n "$SEC" ] || SEC=./secrets
  case "$SEC" in /*) : ;; *) SEC="$(cd compose && pwd)/$SEC" ;; esac'

main() {
  HOST=""
  REGISTRY=""
  REMOTE_DIR="/opt/zeroship-deploy"
  PROBE_URL=""
  DRY_RUN=0
  DO_ROLLBACK=0
  ROLLBACK_TO=""
  LIST_BACKUPS=0
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
      --rollback-to) DO_ROLLBACK=1; ROLLBACK_TO="$2"; shift 2 ;;
      --list-backups) LIST_BACKUPS=1; shift ;;
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

  if [ "$LIST_BACKUPS" = 1 ]; then
    say "snapshots on $HOST"
    # Printed newest first, with the members each stamp is MISSING, because an
    # incomplete generation is exactly what an operator needs to know about
    # before they reach for it in an outage.
    rsh "cd '$REMOTE_DIR'
    members='$SNAPSHOT_MEMBERS'
    stamps=\$(for f in \$members; do ls -1d \"\$f\".bak.[0-9]* 2>/dev/null; done | sed 's|.*\\.bak\\.||' | sort -ru)
    [ -n \"\$stamps\" ] || { echo 'no snapshots'; exit 0; }
    for s in \$stamps; do
      miss=''
      for f in \$members; do [ -e \"\$f.bak.\$s\" ] || miss=\"\$miss \$f\"; done
      if [ -z \"\$miss\" ]; then echo \"\$s  complete\"; else echo \"\$s  INCOMPLETE, missing:\$miss\"; fi
    done"
    exit 0
  fi

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
    #
    # ONE GENERATION, ALL OF IT, OR NONE. Two separate defects live here.
    #
    # The first is scope. The list used to be three files; the ops overlay and
    # the secret files were mutated by a deploy and restored by nothing. A roll
    # that stopped on a stale overlay therefore could not be undone by the tool
    # that caused it, which is how a failed deploy turned into an outage.
    #
    # The second is that the newest backup was chosen PER FILE. That silently
    # stitches generations together: if one member has an extra stamp the
    # others do not -- a partial run, a hand-taken backup, a member added to
    # this list by a later version of this script -- the restore mixes a .env
    # from one moment with a compose file from another. That is precisely the
    # mismatched pair the top of this file exists to prevent, assembled by the
    # recovery path itself. So the stamp is chosen ONCE, as the newest stamp
    # for which EVERY member is present, and every member is restored from it.
    #
    # "Some restore is better than none in an outage" is the obvious objection.
    # It does not survive the question of WHEN a generation can be incomplete:
    # a deploy writes every member in one `set -e` block, so an incomplete one
    # means that deploy died mid-backup and there is no coherent state to
    # return to. An operator who really wants one file back can cp it by hand,
    # after `--list-backups` has told them what is actually there; this script
    # must not guess on their behalf. The scan therefore runs to completion
    # BEFORE anything is copied.
    rsh "set -e
    cd '$REMOTE_DIR'
    members='$SNAPSHOT_MEMBERS'
    want='$ROLLBACK_TO'
    stamps=\$(for f in \$members; do ls -1d \"\$f\".bak.[0-9]* 2>/dev/null; done | sed 's|.*\\.bak\\.||' | sort -ru)
    [ -n \"\$want\" ] && stamps=\"\$want\"
    chosen=''
    report=''
    for s in \$stamps; do
      miss=''
      for f in \$members; do [ -e \"\$f.bak.\$s\" ] || miss=\"\$miss \$f\"; done
      if [ -z \"\$miss\" ]; then chosen=\"\$s\"; break; fi
      [ -z \"\$report\" ] && report=\"\$s is missing:\$miss\"
    done
    if [ -z \"\$chosen\" ]; then
      if [ -n \"\$want\" ]; then
        echo \"no complete snapshot at \$want (\$report)\" >&2
      else
        echo \"no complete snapshot; newest incomplete generation: \${report:-there are no snapshots at all}\" >&2
      fi
      echo 'REFUSING to roll back. Restoring only some members would pair an old .env with a new compose file or a new overlay, which is the silent mis-render this script exists to prevent, and then restart the stack on it. Nothing was changed. Run --list-backups to see what is here.' >&2
      exit 3
    fi
    for f in \$members; do
      case \"\$f\" in
        # RESTORE NEVER DELETES KEY MATERIAL. The tar is extracted over the
        # secrets directory, so a file the deploy changed or removed comes
        # back; a file the deploy ADDED is left in place. That asymmetry is
        # deliberate: provisioning is additive, so an extra file is inert,
        # while deleting one that something has since started signing with is
        # unrecoverable. -P because the archive carries absolute paths.
        secrets.tar) tar -xpPf \"\$f.bak.\$chosen\"; echo \"restored secret files from \$f.bak.\$chosen (extra files, if any, were kept)\" ;;
        *) cp -a \"\$f.bak.\$chosen\" \"\$f\"; echo \"restored \$f from \$f.bak.\$chosen\" ;;
      esac
    done
    echo \"restored generation \$chosen\"
    # VERIFY BEFORE RESTARTING. The state a rollback returns to is only as good
    # as the state that existed before the deploy, and that state can itself be
    # broken -- an operator who migrates renamed names in .env by hand and only
    # then deploys leaves a .env and a compose file that do not agree, and the
    # snapshot records exactly that. Restarting onto it turns one outage into
    # two. The files stay restored either way; only the restart is withheld.
    cd compose
    if ! docker compose config -q; then
      echo \"ROLLED BACK TO \$chosen, BUT THAT CONFIGURATION DOES NOT RENDER. The stack was NOT restarted and is still running whatever it was running.\" >&2
      echo 'The state before the failed deploy was already inconsistent. Run --list-backups and retry with --rollback-to <older stamp>.' >&2
      exit 4
    fi
    docker compose up -d --remove-orphans"
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

  # Every file a deploy overwrites has to already be there, because every one is
  # backed up first and a backup of an absent file cannot be taken or restored.
  # This is also a real trap in its own right for the ones compose bind-mounts:
  # docker CREATES A DIRECTORY at a missing bind source, so a stack whose
  # ops/zeroship.toml went missing does not fail loudly, it starts every service
  # with a directory where its overlay should be.
  MISSING_LAYOUT="$(rsh "cd '$REMOTE_DIR' 2>/dev/null && for f in $SNAPSHOT_FILES; do [ -f \"\$f\" ] || printf ' %s' \"\$f\"; done" || true)"
  [ -z "$MISSING_LAYOUT" ] || fail "the host layout is incomplete under $REMOTE_DIR:$MISSING_LAYOUT
  Ship the missing config first (docs/runbooks/deploy-server.md, \"One-time:
  prepare the host\"). This script backs up every file it overwrites and cannot
  do that for one that is not there."
  echo "ok  host layout is complete"

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
  GEN_ABSENT=""
  for v in $REQUIRED; do
    echo "$HOST_HAS" | grep -qx "$v" && continue
    # A GENERATED secret that the host does not have is not an operator problem
    # ON A FRESH HOST -- provisioning creates it below. ACROSS A RENAME it is
    # the most dangerous case in the whole script, so it is not simply skipped:
    # it goes to the rename pairing along with the defaulted names.
    #
    # This arm used to `continue` outright, and that is a hole with a very
    # expensive shape. Provisioning keeps a value only when THAT EXACT NAME is
    # already in .env, so a generated secret that was renamed looks absent, gets
    # a fresh random value, and the old one is gone. For a signing key that
    # invalidates every issued token; for ZEROSHIP_CONTROL_MASTER_KEY it means
    # existing ciphertext can never be decrypted again. Nothing about that is
    # visible in the deploy output -- `created ZEROSHIP_CONTROL_MASTER_KEY`
    # reads identically on a fresh host and on a rename.
    if echo "$GENERATED" | grep -qx "$v"; then
      GEN_ABSENT="$GEN_ABSENT $v"
      continue
    fi
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
  GEN_SUSPECT="$(rename_suspects "$ORPHANS" "$GEN_ABSENT")"
  if [ -n "$GEN_SUSPECT" ]; then
    fail "possible RENAME OF A GENERATED SECRET, refusing to guess:$GEN_SUSPECT
  Copy the VALUE across to the new name in $REMOTE_DIR/compose/.env before
  deploying. Provisioning keeps a value only under the name it already has, so
  if this is a rename it will generate a NEW secret and the old one is gone:
  a new signing key invalidates every issued token, and a new master key means
  existing encrypted data can never be decrypted. If they are unrelated, delete
  the stale name from the host .env to clear this."
  fi
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
    echo "would build and push $IMAGE, snapshot every deploy input, sync compose + Caddyfile + ops/zeroship.toml, provision generated secrets and secret files, run --check-config for every server, and roll the stack"
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
  #
  # The snapshot is taken BEFORE the first mutation and every member gets the
  # SAME stamp, which is what makes `--rollback` able to name one generation and
  # restore all of it. `secrets.tar` is a whole-directory archive rather than a
  # per-file backup because the file set itself changes across versions: a
  # deploy that starts requiring an eighth secret file must still be undoable by
  # a rollback that predates it.
  say "snapshotting every deploy input"
  STAMP="$(date +%Y%m%d%H%M%S)"
  rsh "set -e
  cd '$REMOTE_DIR'
  $SEC_RESOLVE
  mkdir -p \"\$SEC\" && chmod 700 \"\$SEC\"
  for f in $SNAPSHOT_FILES; do cp -a \"\$f\" \"\$f.bak.$STAMP\"; done
  # -P keeps the absolute path in the archive, so the restore lands where this
  # came from without re-deriving it from a .env it is busy replacing.
  tar -cpPf secrets.tar.bak.$STAMP \"\$SEC\"
  echo \"ok  snapshot $STAMP covers $SNAPSHOT_MEMBERS (secrets from \$SEC)\""

  scp "${SSH_OPTS[@]}" -q deploy/compose/docker-compose.yml "$HOST:$REMOTE_DIR/compose/docker-compose.yml"
  scp "${SSH_OPTS[@]}" -q deploy/ops/Caddyfile "$HOST:$REMOTE_DIR/ops/Caddyfile"
  # THE OVERLAY, which nothing synced until 2026-08-13. It is mounted read-only
  # into control, migrated, gateway, worker and auth at the well-known path, and
  # the binaries parse it with `deny_unknown_fields`, so a host copy written for
  # an older binary does not degrade -- every service refuses to start. It is
  # tracked config exactly like the compose file and the Caddyfile, and it now
  # ships exactly like them.
  scp "${SSH_OPTS[@]}" -q deploy/ops/zeroship.toml "$HOST:$REMOTE_DIR/ops/zeroship.toml"
  echo "ok  compose + Caddyfile + ops/zeroship.toml synced"

  # --------------------------------------------------------- provision secrets
  #
  # RUN THE IMAGE'S OWN PROVISIONER. This used to be a shell loop over a list
  # scraped out of `crates/cli/src/dev.rs` with sed, and that list is
  # `ENV_KEYS` -- env-shaped names. The servers also require seven secret
  # FILES, which appear in `secret_specs()` a hundred lines further down the
  # same file and were in nobody's list, so nothing ever created them and
  # control refused to start on a missing --signing-key-file.
  #
  # A second list is the defect, not the eight names it happened to hold, so
  # the fix is to stop keeping one. `zeroship dev init` owns both halves, and
  # the copy that runs is the one INSIDE THE IMAGE BEING DEPLOYED, so it can
  # never describe a different version of the platform than the binaries do.
  # It is additive by construction (existing material is validated and kept,
  # never rotated) and refuses rather than overwriting on a conflict.
  #
  # Generated ON the host: the value never exists on this machine, so it cannot
  # leak into a local shell history, a log, or a process listing.
  say "provisioning secrets with the image's own zeroship dev init (additive)"
  rsh "set -e
  cd '$REMOTE_DIR'
  $SEC_RESOLVE
  docker run --rm \
    -v '$REMOTE_DIR/compose':'$REMOTE_DIR/compose' \
    -v \"\$SEC\":\"\$SEC\" \
    --entrypoint zeroship '$IMAGE' dev init \
      --secrets-dir=\"\$SEC\" \
      --env-file='$REMOTE_DIR/compose/.env'" \
    || fail "secret provisioning failed; nothing was restarted.
  It refuses rather than overwriting, so this usually means existing material
  on the host disagrees with itself (most often pairwise-salt against
  ZEROSHIP_PAIRWISE_SALT). Fix the named file or value by hand.
  Re-run with --rollback to restore the $STAMP snapshot."

  # THE INDEPENDENT CHECK, and it is independent on purpose. The step above
  # provisions what the IMAGE thinks is needed; this asks what the COMPOSE FILE
  # about to be rendered actually references, and requires each of those files
  # to exist. The two lists agree today. When they stop agreeing -- a service
  # gains a key file before the provisioner learns about it -- this is what
  # says so, and it says it while the old stack is still up.
  #
  # It also covers the binaries --check-config cannot: auth's five key-file
  # requirements and migrated's PAT signing key are enforced AFTER their
  # check-config early return, so a dry run passes with those files absent.
  say "checking every secret file the new compose references exists"
  WANT_FILES="$(secret_files | tr '\n' ' ')"
  [ -n "${WANT_FILES// /}" ] || fail "could not read any secret file reference out of deploy/compose/docker-compose.yml
  Refusing to continue: an empty list would make this check vacuous, which is
  the shape of the gap it exists to close."
  ABSENT="$(rsh "cd '$REMOTE_DIR'
  $SEC_RESOLVE
  for f in $WANT_FILES; do [ -s \"\$SEC/\$f\" ] || printf ' %s' \"\$f\"; done" || true)"
  [ -z "$ABSENT" ] || fail "secret files the new compose requires are missing or empty:$ABSENT
  Nothing was restarted. The provisioner did not create them, so the two lists
  have diverged: add them to secret_specs() in crates/cli/src/dev.rs, rebuild
  the image, and deploy that. Re-run with --rollback to restore the $STAMP snapshot."
  echo "ok  every referenced secret file is present ($(echo $WANT_FILES))"

  # ------------------------------------------------------------ render + roll
  # Render BEFORE touching the running stack. The compose file declares its
  # secrets as \${VAR:?}, so a missing one fails here rather than half-starting.
  say "rendering the new configuration"
  rsh "cd '$REMOTE_DIR/compose' && docker compose config -q" \
    || fail "the new compose does not render on the host; nothing was restarted.
  Re-run with --rollback to restore the $STAMP snapshot."
  echo "ok  renders"

  # ------------------------------------------------------------ check-config
  #
  # NGINX -T PARITY, and the check that would have turned this deploy's outage
  # into a refusal. `docker compose config -q` above proves the YAML renders; it
  # says nothing about whether the BINARIES accept what it renders. The overlay
  # is the clearest case: a host copy carrying a field the new servers deleted
  # makes every one of them exit at parse time, and compose renders it happily
  # because to compose it is just a bind mount.
  #
  # `--check-config` is the servers' own answer to that question. It resolves
  # the overlay, the environment and the flags, runs the presence and strength
  # guards, and exits before it opens a socket, connects to a database or
  # mutates the filesystem. Run against the NEW image with the NEW configuration
  # while the OLD stack is still serving, a refusal here costs nothing.
  #
  # `run --rm --no-deps` and not `up`: it gets the service's real environment,
  # its real volumes (including the overlay mount) and its real image without
  # starting postgres, without joining the running stack, and without leaving a
  # container behind. --entrypoint is required because the gateway's service
  # entrypoint is a shell wrapper.
  say "running --check-config for every server against the new image"
  # The whole check is worthless if the host's compose does not actually resolve
  # to the image we are about to roll -- it would dry-run the OLD binaries and
  # pass. That happens for one concrete reason: ZEROSHIP_IMAGE is referenced by
  # the server-side docker-compose.override.yml, not by the tracked compose
  # file, so a host without that override silently pins whatever literal tag the
  # tracked file carries.
  RENDERED="$(rsh "cd '$REMOTE_DIR/compose' && ZEROSHIP_IMAGE='$IMAGE' docker compose config --images 2>/dev/null | sort -u" || true)"
  echo "$RENDERED" | grep -qx "$IMAGE" || fail "the host's compose does not resolve to $IMAGE; it renders:
$RENDERED
  Nothing was restarted. The image reference comes from ZEROSHIP_IMAGE in the
  server-only docker-compose.override.yml (docs/runbooks/deploy-server.md,
  \"Control plane access\"); without it every check below would dry-run the
  OLD binaries and pass."

  # Pull first, so the check runs against the image the roll will run and not
  # against whatever tag happens to be cached on the host.
  rsh "cd '$REMOTE_DIR/compose' && ZEROSHIP_IMAGE='$IMAGE' docker compose pull -q 2>&1 | tail -3" \
    || fail "could not pull $IMAGE on the host; nothing was restarted."
  CHECK_BAD=""
  # -T, and it is load-bearing. `docker compose run` is interactive by default;
  # over `ssh host "..."` there is no pseudo-terminal, so without it the run can
  # fail on the TTY rather than on the configuration -- a refusal that looks
  # exactly like the one this check exists to produce.
  CHECK_RUN="docker compose run --rm -T --no-deps --entrypoint"
  for svc in $CHECK_SERVICES; do
    if rsh "cd '$REMOTE_DIR/compose' && ZEROSHIP_IMAGE='$IMAGE' $CHECK_RUN ${svc#*:} ${svc%%:*} --check-config" >/dev/null 2>&1 </dev/null; then
      echo "ok  ${svc%%:*}"
    else
      echo "FAIL: ${svc%%:*}" >&2
      rsh "cd '$REMOTE_DIR/compose' && ZEROSHIP_IMAGE='$IMAGE' $CHECK_RUN ${svc#*:} ${svc%%:*} --check-config 2>&1 | tail -20" >&2 </dev/null || true
      CHECK_BAD="$CHECK_BAD ${svc%%:*}"
    fi
  done
  [ -z "$CHECK_BAD" ] || fail "these servers reject the new configuration:$CHECK_BAD
  NOTHING WAS RESTARTED and the old stack is still serving.
  It is NOT unchanged, though: the new compose file, Caddyfile and ops overlay
  are already on disk, and the running containers bind-mount the last two. They
  will not notice until they restart -- and `restart: on-failure` or a host
  reboot can do that for you, on the new config with the OLD image. So either
  deploy a fixed image promptly, or --rollback to the $STAMP snapshot and take
  your time."
  echo "ok  every server accepts the new configuration"

  say "rolling the stack to $IMAGE"
  # One `up -d` for every service. The control key is shared, so a partial
  # restart splits the stack into two halves that cannot authenticate.
  rsh "set -e
  cd '$REMOTE_DIR/compose'
  sed -i 's|^ZEROSHIP_IMAGE=.*|ZEROSHIP_IMAGE=$IMAGE|' .env
  grep -q '^ZEROSHIP_IMAGE=$IMAGE\$' .env || { echo 'ZEROSHIP_IMAGE was not updated'; exit 1; }
  docker compose up -d --remove-orphans" \
    || fail "the roll failed. Re-run with --rollback to restore the $STAMP snapshot."

  # ----------------------------------------------------------------- verify
  say "verifying"
  sleep 10

  BAD="$(rsh "cd '$REMOTE_DIR/compose'
  docker compose ps --format '{{.Name}}|{{.State}}' | awk -F'|' '\$2 != \"running\" {print \$1\" \"\$2}'" || true)"
  if [ -n "$BAD" ]; then
    printf 'FAIL: services not running:\n%s\n' "$BAD" >&2
    rsh "cd '$REMOTE_DIR/compose' && docker compose logs --tail=25 2>&1 | tail -40" >&2 || true
    fail "roll produced unhealthy services. Re-run with --rollback to restore the $STAMP snapshot."
  fi
  echo "ok  every service is running"

  RUNNING_IMAGE="$(rsh "docker inspect \$(cd '$REMOTE_DIR/compose' && docker compose ps -q control | head -1) --format '{{.Config.Image}}'" || true)"
  [ "$RUNNING_IMAGE" = "$IMAGE" ] || fail "control is running '$RUNNING_IMAGE', not '$IMAGE'"
  echo "ok  control is running the image we just pushed"

  if [ -n "$PROBE_URL" ]; then
    say "probing $PROBE_URL"
    node examples/db-todos/scripts/probe-live.mjs "$PROBE_URL" \
      || fail "the app probe failed against the new deploy. Re-run with --rollback to restore the $STAMP snapshot."
  fi

  say "deployed $IMAGE to $HOST"
  echo "snapshot $STAMP covers $SNAPSHOT_MEMBERS"
  echo "roll back with: $0 --host $HOST --rollback"
}

# Only run when EXECUTED. Sourcing this file must have no side effects: the
# gate sources it to reach compose_vars, secret_files and rename_suspects.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then main "$@"; fi
