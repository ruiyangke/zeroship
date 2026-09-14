#!/usr/bin/env bash
# ============================================================================
# The two deploy scripts had no automated test of any kind. This covers the
# part of them that is PURE TEXT PROCESSING over the compose file, plus the
# argument and credential handling of both, and it does it without ssh, docker
# or the network.
#
# WHY THIS EXISTS. deploy/scripts/deploy-remote.sh decides whether a deploy is
# allowed to proceed by extracting the variable surface out of the compose file
# and diffing it against the host's .env. That extraction produced TWO real
# false results while it was being written, and both were caught by hand:
#
#   1. compose_vars counted `$${VAR}` -- a compose-ESCAPED reference that
#      becomes a literal `$VAR` for the CONTAINER's own shell, never a compose
#      variable. `WORKERS`, built inside the gateway command block, is one. It
#      also counted references inside comment lines. Both were reported as
#      "missing on the host" against a perfectly healthy host, i.e. the guard
#      blocked a good deploy. That is how a safety check gets switched off.
#
#   2. The rename guard swung from useless to useless in the other direction.
#      First it treated "absent but has a :-default" as a note, so deleting
#      ZEROSHIP_ORIGIN_SCHEME printed `ok no required variable is missing` --
#      the exact silent breakage it exists to catch. The fix then fired on a
#      healthy host, because orphans and defaulted variables coexist in normal
#      steady state. It now pairs them only when the NAMES are related by a
#      shared token outside a stoplist.
#
# Both bugs are re-introduced as mutations during review; if you change the
# extraction, do that again rather than trusting this file's green.
#
# HOW IT REACHES THE LOGIC. deploy-remote.sh runs `ssh` a few lines into its
# flow, so it cannot be tested by executing it. It now defines its helpers at
# the top level and calls main() only from a `[ "${BASH_SOURCE[0]}" = "$0" ]`
# guard, so this file sources it and calls compose_vars / rename_suspects
# directly against FIXTURE compose files written here -- not against the real
# one, which is owned by other work and would make this gate a change detector.
#
# WHAT THIS DOES NOT CHECK, so a green is not over-read:
#   - deploy-app.sh past its argument and credential handling. No ssh, no
#     port-forward, no upload. Those need a host and are not faked here.
#   - deploy-remote.sh's docker build, docker push, and the app probe. The rest
#     of the deploy path IS driven now, but through a stub ssh and a stub
#     docker, so what is established is the script's CONTROL FLOW -- which
#     commands it sends, in what order, and where it stops. NOT established:
#     that a real image exists, that a real `zeroship dev init` writes the
#     right files, that a real server accepts or rejects a given
#     configuration, or that the stack comes back up. The one exception is the
#     conditional block at the end, which runs the compiled control binary
#     against a real overlay and is therefore evidence about the SERVER.
#   - whether the five --check-config runs cover every startup guard. They do
#     not: auth's five key-file requirements and migrated's PAT signing key are
#     enforced AFTER their check-config early return, so a dry run passes with
#     those files absent. That gap is why the script ALSO checks the secret
#     files exist, and why both are asserted here rather than one.
#   - deploy-app.sh's tunnel teardown, the ControlMaster=no/ControlPath=none
#     pairing that makes the PID real, and the `kill -0` liveness poll. That
#     was measured by hand with `ss -tlnp` and has no coverage here.
#   - that the compose file we ship is CORRECT. This tests the reader, not the
#     thing read. One conditional block below does look at the real file, and
#     only to confirm the escaped-reference exclusion still holds there.
#   - a KNOWN GAP in compose_vars, deliberately not asserted because asserting
#     it would only encode today's behaviour: the comment filter drops WHOLE
#     comment lines only, so a reference in a TRAILING `# ...` comment on an
#     otherwise live line IS still counted. No line in the shipped compose file
#     has that shape today, so it is latent, not live.
#   - whether the host .env side of the diff is read correctly. That half is
#     one `rsh grep` against a real host and is not reachable without ssh.
#   - that a tree the source-tree check PASSES will then build. That check asks
#     one question - is every declared submodule populated with the manifests
#     the build opens - and this gate asks whether it asks it correctly. A
#     missing npm dependency, a lockfile out of date with a package.json, a
#     compile error, a full disk on the build host: all of them still fail the
#     way they always did, twenty minutes in. Nothing here says the tree builds.
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REMOTE="$ROOT/deploy/scripts/deploy-remote.sh"
APPDEP="$ROOT/deploy/scripts/deploy-app.sh"
REAL_COMPOSE="$ROOT/deploy/compose/docker-compose.yml"
REAL_OVERLAY="$ROOT/deploy/ops/zeroship.toml"
# The whole committed migration corpus, NOT one named file. These two
# assertions are about the END STATE the corpus produces, and pinning them to
# 20260702000900_grants.ts made them assertions about WHICH FILE spells the
# statement. That is a real difference: once a file has been applied to a
# deployed database its bytes are frozen by the runner's checksum guard, so a
# statement can only ever MOVE to a later file, and a gate that reads one
# filename then reports the invariant as broken when it is intact.
MIGRATIONS_DIR="$ROOT/db/migrations-ts"

echo "============================================"
echo "  deploy scripts: extraction, pairing, arguments"
echo "============================================"

[ -f "$REMOTE" ] || { echo "  x REFUSED: $REMOTE not found." >&2; exit 1; }
[ -f "$APPDEP" ] || { echo "  x REFUSED: $APPDEP not found." >&2; exit 1; }

# Sourcing must come BEFORE pass/fail are defined: deploy-remote.sh defines its
# own fail(), which exits. Ours is defined after so it wins. If you move this,
# the first failing assertion will abort the run instead of being counted (it
# still exits non-zero, so it cannot fake a green -- but the count will lie).
# shellcheck source=/dev/null
source "$REMOTE"
# deploy-remote.sh sets -e for its own flow. We evaluate failures ourselves and
# must not die on the first non-zero command.
set +e

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }

# Per-arm anti-vacuity accounting.
#
# THIS GATE IS ONE OF THE FOUR THE LIBRARY EXISTS FOR. Its argv-secret scan
# matched exactly ONE `--<name>-file` row in the shipped compose, on the single
# service its own filter excludes, so it examined nothing and printed the same
# clean line a compose file with no flags would - underneath a gate-level floor
# on the ASSERTION count that was green throughout, because the assertion did
# run. It just ran over an empty set. Everything the floor at the bottom of this
# file counts is assertions; everything declared below is ITEMS.
#
# The arms are the places this file enumerates something DERIVED - from
# the sourced deploy-remote.sh, from the shipped compose, from .gitmodules - as
# opposed to the fixtures it writes itself, which cannot silently shrink.
# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init deploy_scripts

if ! declare -F compose_vars >/dev/null || ! declare -F rename_suspects >/dev/null; then
  echo "  x REFUSED: sourcing $REMOTE did not define compose_vars/rename_suspects." >&2
  exit 1
fi

# ---------------------------------------- every repo path the roll reads exists
#
# THE ARM THAT WOULD HAVE CAUGHT THE 2026-08 REORG, and the reason it is written
# generically rather than as two `test -f` lines. The crate directories were all
# renamed `crates/<name>` -> `crates/zeroship-<name>`. deploy-remote.sh scrapes
# two Rust files with `sed`, and both paths went stale. Under its own
# `set -euo pipefail` that does not produce a wrong answer, it produces a roll
# that DIES at the assignment with a bare `sed: can't read ...` -- before the
# carefully written "read ZERO names, refusing" arm on the next line can run.
# The platform could not be deployed at all, and every one of this gate's own
# checks on that path failed with the same opaque capture.
#
# What no check in the tree was doing was the simplest possible thing: opening
# the files the script names. The Rust twins pinned the sed PATTERNS (and one of
# them pinned the stale PATH as a literal, so it required the broken spelling);
# this gate's edge arms sat behind `[ -f "$EDGE_RS" ]` and stopped being
# declared. A path is not verified by being spelled.
#
# So: take every repo-relative path deploy-remote.sh hands to a command, and
# open it. One verdict per path.
ROLL_SOURCE_PATHS="$(
  grep -oE '(crates|sdks|libs|db|deploy|examples)/[A-Za-z0-9_./-]+\.(rs|toml|tsv|json|yml|yaml)' "$REMOTE" \
    | sort -u
)"
N_ROLL_PATHS=0
[ -n "$ROLL_SOURCE_PATHS" ] && N_ROLL_PATHS="$(printf '%s\n' "$ROLL_SOURCE_PATHS" | grep -c .)"

# MEASURED 2026-08-28: 10 distinct paths. Floor 5 -- far below that, so ordinary
# editing of the script does not reach it, while the failure this guards (the
# grep stops matching the script's shape and rules on nothing) lands on 0.
# NOTE ON WHAT THIS ARM CANNOT SEE: it enumerates by SPELLING, so a path built
# at runtime from a variable is invisible to it, and a path that appears only
# inside a comment is counted as though the script read it. The first is the
# blind spot; the second only ever over-fires, which prints a refusal a reader
# resolves rather than shipping a name.
if ! gate_arm roll_source_paths "$N_ROLL_PATHS" 5; then
  fail "no repo-relative source path was found in $REMOTE at all; the enumeration below rules on nothing"
else
  roll_missing=""
  for p in $ROLL_SOURCE_PATHS; do
    [ -e "$ROOT/$p" ] || roll_missing="$roll_missing $p"
  done
  [ -z "$roll_missing" ] \
    && pass "every repo path deploy-remote.sh names resolves ($N_ROLL_PATHS checked)" \
    || fail "deploy-remote.sh names repo paths that are not in this tree:$roll_missing. Anything it feeds to sed/cat/test -f there reads nothing, and under its set -euo pipefail the roll aborts with a bare shell error instead of a diagnosis"
  # CONTROL: the loop must be able to say no. Without it, "all resolve" could
  # mean the existence test passes on everything.
  [ -e "$ROOT/crates/zeroship-control/src/zz_not_a_real_file.rs" ] \
    && fail "CONTROL: an invented path resolved; the existence check discriminates nothing" \
    || pass "CONTROL: an invented crate path does NOT resolve, so the check above can fail"
fi

# ------------------------------------------------------------ set comparison
#
# ANTI-VACUITY. An extraction that returns NOTHING compares equal to an empty
# expectation, and every set assertion below would pass while asserting
# nothing. That is the failure mode that makes a whole gate meaningless, so
# "empty" is a distinct verdict from "equal" and is always a failure.
usable() { [ -n "${1//[[:space:]]/}" ]; }
set_verdict() {
  usable "$1" || { echo empty; return; }
  # shellcheck disable=SC2086
  if [ "$(printf '%s\n' $1 | sort -u)" = "$(printf '%s\n' $2 | sort -u)" ]; then
    echo ok
  else
    echo differ
  fi
}
expect_set() { # $1 actual, $2 expected, $3 label
  case "$(set_verdict "$1" "$2")" in
    ok)     pass "$3" ;;
    empty)  fail "$3: the extraction returned NOTHING. An empty result is not a match; it means the parser broke and every comparison after it is vacuous" ;;
    differ) fail "$3: got [$(echo $1)] want [$(echo $2)]" ;;
  esac
}

FIX="$(mktemp -d)"
trap 'rm -rf "$FIX"' EXIT

# The base fixture carries one of every shape the extraction must tell apart.
cat >"$FIX/base.yml" <<'FIXTURE'
#      COMMENTED: ${COMMENT_ONLY_VAR}
services:
  demo:
    environment:
      PLAIN: ${PLAIN_VAR}
      STRICT: ${STRICT_VAR:?set this or the render fails}
      DEFAULTED: ${DEFAULTED_VAR:-fallback}
    command: |
      WORKERS="$${ESCAPED_VAR}http://host:8080"
FIXTURE

# CONTROLS THAT DIFFER IN ONE CHARACTER. Without them, "ESCAPED_VAR is absent"
# only proves the parser missed it -- it does not prove the parser EXCLUDED it
# for being escaped. Each control changes exactly one thing and the name must
# then appear.
sed 's/\$\${ESCAPED_VAR}/${ESCAPED_VAR}/' "$FIX/base.yml" >"$FIX/escape_control.yml"
sed 's/^#\( *COMMENTED:\)/ \1/'           "$FIX/base.yml" >"$FIX/comment_control.yml"
: >"$FIX/empty.yml"

echo ""
echo "-- compose_vars extraction (fixtures)"

BASE_ALL="$(compose_vars '' "$FIX/base.yml")"
BASE_OPT="$(compose_vars ':-' "$FIX/base.yml")"
expect_set "$BASE_ALL" "DEFAULTED_VAR PLAIN_VAR STRICT_VAR" \
  "a plain \${VAR}, a \${VAR:?msg} and a \${VAR:-default} are all counted; \$\${VAR} and a commented \${VAR} are not"
expect_set "$BASE_OPT" "DEFAULTED_VAR" \
  "only the :- variant is classified optional"

# Required = counted minus optional. This is the set the script actually
# refuses on, so assert it directly rather than leaving it to be inferred.
BASE_REQ="$(printf '%s\n' $BASE_ALL | grep -vxF -e "$BASE_OPT")"
expect_set "$BASE_REQ" "PLAIN_VAR STRICT_VAR" \
  "\${VAR} and \${VAR:?msg} are required, the defaulted one is not"

ESC_ALL="$(compose_vars '' "$FIX/escape_control.yml")"
expect_set "$ESC_ALL" "DEFAULTED_VAR ESCAPED_VAR PLAIN_VAR STRICT_VAR" \
  "CONTROL: the same name with ONE \$ instead of two IS counted (the exclusion is about the escape, not the name)"

CMT_ALL="$(compose_vars '' "$FIX/comment_control.yml")"
expect_set "$CMT_ALL" "COMMENT_ONLY_VAR DEFAULTED_VAR PLAIN_VAR STRICT_VAR" \
  "CONTROL: the same line uncommented IS counted (the exclusion is about the #, not the name)"

# ------------------------------------------------ secret_files extraction
#
# WHY THIS FUNCTION EXISTS. Provisioning read its list out of `ENV_KEYS` in
# crates/zeroship-cli/src/dev.rs (that const is gone since 2026-08-20; the list is
# `zeroship_core::config::PLATFORM_SECRETS`), which is env-shaped by
# construction, so the seven
# secret FILES the servers open were in nobody's list and nothing created them.
# The compose file is the only artefact that states which files this
# deployment's binaries will be handed, so it is the thing to ask.
echo ""
echo "-- secret_files extraction (fixtures)"

cat >"$FIX/secrets.yml" <<'FIXTURE'
#      COMMENTED: ZEROSHIP_X_FILE: /etc/zeroship/secrets/commented-only
services:
  demo:
    environment:
      ZEROSHIP_A_FILE: /etc/zeroship/secrets/alpha-signing.pem
      ZEROSHIP_B_FILE: /etc/zeroship/secrets/bravo-secret
    command: >
      zeroship-demo
        --signing-key-file /etc/zeroship/secrets/alpha-signing.pem
    volumes:
      - ${ZEROSHIP_SECRETS_DIR:-./secrets}:/etc/zeroship/secrets:ro
FIXTURE
sed 's/^#\( *COMMENTED:\)/ \1/' "$FIX/secrets.yml" >"$FIX/secrets_comment_control.yml"

expect_set "$(secret_files "$FIX/secrets.yml")" "alpha-signing.pem bravo-secret" \
  "a path in an env value and the same path in a command are counted once; a commented one is not"
expect_set "$(secret_files "$FIX/secrets_comment_control.yml")" \
  "alpha-signing.pem bravo-secret commented-only" \
  "CONTROL: the same line uncommented IS counted (the exclusion is about the #, not the name)"

# THE MOUNT LINE, which names the directory and no file. If it were counted the
# provisioning check would demand a file called `secrets` forever.
if secret_files "$FIX/secrets.yml" | grep -qx 'secrets'; then
  fail "the read-only mount of the secrets DIRECTORY was counted as a secret file"
else
  pass "the \${ZEROSHIP_SECRETS_DIR}:/etc/zeroship/secrets:ro mount is not counted as a file"
fi

EMPTY_SECRETS="$(secret_files "$FIX/empty.yml")"
if usable "$EMPTY_SECRETS"; then
  fail "an empty compose file produced secret files [$EMPTY_SECRETS]"
else
  pass "an empty compose file references no secret file (the extraction is not matching on nothing)"
fi

# NO SECRET PATH MAY ARRIVE AS A COMMAND FLAG. The pre-roll dry run is
# `docker compose run --entrypoint <bin> <svc> --check-config`, and passing a
# command to `run` REPLACES the service's own command. Anything supplied there
# is invisible to the dry run, so a key path passed as `--x-file` makes
# check-config report a required secret missing on a host where it is present
# -- a false refusal, which is how a gate gets switched off. Control's
# `--signing-key-file` was the last one; it is deleted, along with the personal
# access tokens whose issuer was its only consumer.
#
# SCOPED TO THE SERVICES THE PRE-ROLL ACTUALLY DRY-RUNS, which is the rule
# matching its own reason rather than an exception carved out of it. The set is
# CHECK_SERVICES, read from the sourced deploy-remote.sh, so it cannot drift
# from the loop that consumes it and nothing here carries its own list. A
# service the pre-roll never runs cannot have a flag hidden from a dry run that
# does not happen: `migrate` is absent from CHECK_SERVICES on purpose
# (deploy/scripts/deploy-remote.sh: "zeroship-platform-migrate has no
# --check-config and mounts no overlay"), and its DSN path is a flag precisely
# so the credential stays out of both argv and the container environment.
#
# WHAT THE ANTI-HOLLOW GUARD BELOW DID NOT COVER UNTIL 2026-08-20, and it is
# the instructive part. The scan matched exactly ONE `--<name>-file` row in the
# shipped compose -- `migrate --database-url-file` -- and `migrate` is the one
# service the filter excludes, so the set it examined was EMPTY and the pass
# below was reporting on nothing. The guard that was here proves the service
# WALK still sees every dry-run service, which is a real check and stays; it
# says nothing about whether the flag MATCHER still matches, and a matcher that
# matches nothing prints the same clean line as a compose file with no flags.
# So the flag-row count is now floored and the rows are named in the pass, which
# is the only thing that tells a reader the filter had work to do.
if [ -f "$REAL_COMPOSE" ]; then
  DRY_RUN_SERVICES=""
  for svc in $CHECK_SERVICES; do DRY_RUN_SERVICES="$DRY_RUN_SERVICES ${svc%%:*}"; done

  # `<service>\t<flag>` for every `--<name>-file` on a live line, attributed to
  # the service block it sits in. NOT yet filtered to the dry-run set.
  ALL_FLAG_FILES="$(
    awk '
      /^  [a-z][a-z0-9_-]*:[[:space:]]*$/ { svc = $1; sub(/:$/, "", svc); next }
      /^[[:space:]]*#/ { next }
      svc != "" && match($0, /--[a-z-]+-file/) {
        print svc "\t" substr($0, RSTART, RLENGTH)
      }
    ' "$REAL_COMPOSE" | sort -u
  )"
  N_ALL_FLAGS=0
  [ -n "$ALL_FLAG_FILES" ] && N_ALL_FLAGS="$(printf '%s\n' "$ALL_FLAG_FILES" | wc -l | tr -d ' ')"

  FLAG_FILES="$(
    printf '%s' "$ALL_FLAG_FILES" | while IFS=$'\t' read -r s f; do
      [ -n "$s" ] || continue
      case " $DRY_RUN_SERVICES " in *" $s "*) echo "$s $f" ;; esac
    done
  )"

  # Anti-hollow, part one: the walk is worthless if it stops attributing lines
  # to services, and that looks exactly like compliance. Every dry-run service
  # must be visible to it.
  SEEN_SERVICES="$(awk '/^  [a-z][a-z0-9_-]*:[[:space:]]*$/ { s=$1; sub(/:$/,"",s); print s }' "$REAL_COMPOSE" | sort -u)"
  MISSING_SEEN=""
  N_DRY_RUN=0
  for s in $DRY_RUN_SERVICES; do
    N_DRY_RUN=$((N_DRY_RUN + 1))
    echo "$SEEN_SERVICES" | grep -qx "$s" || MISSING_SEEN="$MISSING_SEEN $s"
  done

  # BOTH ARMS ARE DECLARED BEFORE THE BRANCH, not inside it. Written as `elif`
  # they would stop being declared the moment an earlier branch was taken, and a
  # declaration that sits behind a condition that stopped matching is the same
  # defect one level up - the meta-gate would see the source and be content.
  #
  # The services the pre-roll dry-runs, taken from CHECK_SERVICES in the sourced
  # deploy-remote.sh. MEASURED 2026-08-20: 5 (control, migrated, gateway, worker,
  # auth). Floor 3, well under that: the number moves when a platform service is
  # added or retired, which is ordinary, but the failure this guards - the sourced
  # script stops defining the list, or its shape changes - takes it to 0.
  gate_arm dry_run_services "$N_DRY_RUN" 3 || true

  # THE ARM THAT WAS VACUOUS, and the count it declares is deliberately the
  # PRE-FILTER one. `FLAG_FILES` - the rows left after the dry-run filter - is
  # the VIOLATION set, and its correct value is 0; flooring that would demand a
  # credential in argv to pass. What may never collapse is the number of rows the
  # matcher handed to the filter, because every one of those got a verdict.
  # MEASURED 2026-08-20: 1, `migrate --database-url-file`, which is exactly the
  # row the filter excludes. So this arm rules on one item and the floor is 1;
  # that is as much as this enumeration can honestly claim, and it is the
  # difference between "the filter excluded the only row" and "the matcher found
  # no rows", which printed identically until now.
  argv_rows_ok=0
  gate_arm argv_flag_rows "$N_ALL_FLAGS" 1 || argv_rows_ok=1

  if [ -n "$MISSING_SEEN" ]; then
    fail "the service walk did not see$MISSING_SEEN, so a clean result below would mean nothing"
  elif [ "$argv_rows_ok" -ne 0 ]; then
    fail "the --<name>-file matcher found no row anywhere in $REAL_COMPOSE, not even on the services the pre-roll does not dry-run. It matched 1 on 2026-08-20; a matcher that matches nothing reports the same clean result as a compose file with no flags"
  elif [ -z "$FLAG_FILES" ]; then
    pass "no dry-run service passes a secret path as a command flag, so --check-config sees every one of them ($N_ALL_FLAGS flag row(s) in the file, $(printf '%s\n' "$ALL_FLAG_FILES" | tr '\t' ' ' | tr '\n' ';') -- all outside the dry-run set $(echo $DRY_RUN_SERVICES))"
  else
    fail "these secret paths are passed as command flags on a service the pre-roll dry-runs, and are therefore invisible to --check-config: $(echo $FLAG_FILES). Move them to their canonical ZEROSHIP_*_FILE environment name"
  fi
fi

# The worker handles attacker-controlled app code. Its default DSN must name
# the constrained worker role, never the provisioning principal that owns the
# platform schema. Scope the extraction to the worker service so a safe DSN on
# another service cannot make this assertion pass.
if [ -f "$REAL_COMPOSE" ]; then
  WORKER_DSN_LINE="$(awk '
    /^  worker:/ { in_worker=1; next }
    /^  [a-zA-Z0-9_-]+:/ { in_worker=0 }
    in_worker && /ZEROSHIP_WORKER_DATABASE_URL:/ { print; exit }
  ' "$REAL_COMPOSE")"
  if [ -z "$WORKER_DSN_LINE" ]; then
    fail "the worker service has no ZEROSHIP_WORKER_DATABASE_URL default to inspect"
  elif [[ "$WORKER_DSN_LINE" == *"postgres://zeroship_worker:"* ]]; then
    pass "the worker default DSN uses the constrained zeroship_worker identity"
  else
    fail "the worker default DSN is not constrained to zeroship_worker: $WORKER_DSN_LINE"
  fi
fi

if [ -d "$MIGRATIONS_DIR" ]; then
  grep -rqF 'REVOKE ALL PRIVILEGES ON ALL TABLES IN SCHEMA zeroship FROM zeroship_worker' "$MIGRATIONS_DIR" \
    && pass "the platform migrations revoke worker authority from every current zeroship table" \
    || fail "the platform migrations do not revoke worker authority from every current zeroship table"
  grep -rqF 'ALTER DEFAULT PRIVILEGES IN SCHEMA zeroship REVOKE ALL PRIVILEGES ON TABLES FROM zeroship_worker' "$MIGRATIONS_DIR" \
    && pass "future zeroship tables remain denied to the worker by default" \
    || fail "future zeroship tables are not denied to the worker by default"
else
  fail "the platform grants migration is missing: $GRANTS_MIGRATION"
fi

# THE DRIFT CHECK, and the actual defect this whole area is about: two lists,
# one of which nobody updated. Every file the SHIPPED compose hands a binary
# must be a file the provisioner (`secret_specs()` + the pairwise-salt case in
# crates/zeroship-cli/src/dev.rs) knows how to create. This is asserted against the real
# compose deliberately -- a fixture cannot go stale in the way that matters.
DEV_RS="$ROOT/crates/zeroship-cli/src/dev.rs"
if [ -f "$REAL_COMPOSE" ] && [ -f "$DEV_RS" ]; then
  REAL_SECRETS="$(secret_files "$REAL_COMPOSE")"
  N_REAL_SECRETS=0
  usable "$REAL_SECRETS" && N_REAL_SECRETS="$(printf '%s\n' $REAL_SECRETS | grep -c .)"
  # One verdict per file: each must be named in dev.rs. MEASURED 2026-08-20: 7
  # (auth-signing.pem, broker-secret, gateway-signing.pem, migrate-dsn,
  # pairwise-salt, refresh-hash-key, refresh-idem-key). Floor 4, which a deploy
  # that legitimately retires a key or two still clears, while the failure this
  # guards - the extraction stops matching the compose file's shape - lands on 0.
  # The `usable` test it replaces was a floor of 1.
  if ! gate_arm real_secret_files "$N_REAL_SECRETS" 4; then
    fail "secret_files extracted NOTHING (or almost nothing: $N_REAL_SECRETS) from the shipped compose file"
  else
    unprovisioned=""
    for n in $REAL_SECRETS; do
      grep -qF "\"$n\"" "$DEV_RS" || unprovisioned="$unprovisioned $n"
    done
    [ -z "$unprovisioned" ] \
      && pass "every secret file the shipped compose references is named in dev.rs ($(echo $REAL_SECRETS))" \
      || fail "the shipped compose hands the binaries files the provisioner never creates:$unprovisioned"
    # CONTROL: without it, "all named" could mean the grep matches anything.
    grep -qF '"zs-not-a-real-secret"' "$DEV_RS" \
      && fail "CONTROL: dev.rs appears to name an invented secret; the drift check discriminates nothing" \
      || pass "CONTROL: an invented secret name is NOT found in dev.rs, so the check above can fail"
  fi
else
  # AN ABSENT INPUT IS A FAILURE, NOT A SKIP. See the census block near the top:
  # `gate_arm real_secret_files` lives inside the arm above, so a false
  # condition here does not produce an arm that ruled on zero - it produces NO
  # ARM AT ALL, and `gate_arms_finish` only refuses when the gate declares zero
  # arms in total. Four arms of this gate vanished exactly that way during the
  # crate reorg while the trailer printed `arms=5 refusals=0`.
  fail "the drift check did not run: missing $( [ -f "$REAL_COMPOSE" ] || echo "$REAL_COMPOSE" ) $( [ -f "$DEV_RS" ] || echo "$DEV_RS" ). It rules on nothing and declares no arm, so this gate would otherwise report a clean census while examining none of it."
fi

# ------------------------------------- the edge's host claims vs the reserved list
#
# WHY THIS EXISTS. An app's name IS its hostname label, so every host the edge
# claims is a name no creator app may hold. `RESERVED_APP_NAMES` refuses those
# names at app-create time and a cargo test binds it to the adapted edge config
# in both directions -- but deploy-remote.sh is not gated on cargo tests. It
# runs from an operator's checkout and `scp`s deploy/ops/Caddyfile to
# production, so before this it could ship an edge claiming a name the registry
# still hands out. Both halves of the new refusal are exercised here, and the
# fixture pair that makes the failing half one-variable is
# crates/zeroship-control/testdata/edge_{control,matcher_claim}.json -- REAL `caddy
# adapt` output for the live edge and for the live edge plus one matcher.
echo "-- the edge config the roll ships (host claims vs RESERVED_APP_NAMES)"

EDGE_ART="$ROOT/deploy/ops/caddy-claimed-hosts.json"
EDGE_RS="$ROOT/crates/zeroship-control/src/reserved_names.rs"
FIX_CONTROL="$ROOT/crates/zeroship-control/testdata/edge_control.json"
FIX_MATCHER="$ROOT/crates/zeroship-control/testdata/edge_matcher_claim.json"

# ANTI-VACUITY FIRST, and it is not a formality here: both extractions are one
# `sed` over a file this gate does not own, so both can go to zero silently, and
# an empty claimed set compares clean against ANY reserved list while an empty
# reserved list makes EVERY claim look uncovered. Neither empty may print green.
REAL_RESERVED=""
if [ -f "$EDGE_ART" ] && [ -f "$EDGE_RS" ]; then
  REAL_LABELS="$(edge_claimed_labels "$EDGE_ART")"
  N_REAL_LABELS=0
  usable "$REAL_LABELS" && N_REAL_LABELS="$(printf '%s\n' $REAL_LABELS | grep -c .)"
  REAL_RESERVED="$(reserved_app_names "$EDGE_RS")"
  N_REAL_RESERVED=0
  usable "$REAL_RESERVED" && N_REAL_RESERVED="$(printf '%s\n' $REAL_RESERVED | grep -c .)"

  # One verdict per hostname label the SHIPPED artifact names. MEASURED
  # 2026-08-21: 4 (api, auth, console, control). Floor 3, which an edge that
  # legitimately retires a host still clears, while the failure this guards --
  # the artifact's shape moves and the grep matches nothing -- lands on 0.
  if ! gate_arm edge_claimed_labels "$N_REAL_LABELS" 3; then
    fail "edge_claimed_labels extracted NOTHING (or almost nothing: $N_REAL_LABELS) from the shipped $EDGE_ART"
  fi
  # One verdict per name in the const. MEASURED 2026-08-21: 4. Same floor, same
  # reason; this is the extraction whose silent emptying the Rust twin test
  # `the_deploy_scripts_sed_still_yields_reserved_app_names` also pins.
  if ! gate_arm reserved_app_names "$N_REAL_RESERVED" 3; then
    fail "reserved_app_names extracted NOTHING (or almost nothing: $N_REAL_RESERVED) from $EDGE_RS"
  fi

  # THE AGREEMENT the deploy path now enforces, asserted against the two real
  # files rather than fixtures: a fixture cannot go stale in the way that
  # matters. This is the same conclusion `reserved_set_matches_the_edge` reaches
  # in Rust, reached by the coarse text route the deploy path actually uses --
  # so if the two ever disagree, one of them is wrong and this says so.
  edge_uncovered=""
  for l in $REAL_LABELS; do
    printf '%s\n' "$REAL_RESERVED" | grep -qx "$l" || edge_uncovered="$edge_uncovered $l"
  done
  [ -z "$edge_uncovered" ] \
    && pass "every host the shipped edge artifact claims is in RESERVED_APP_NAMES ($(echo $REAL_LABELS))" \
    || fail "the shipped edge claims hostnames RESERVED_APP_NAMES does not cover:$edge_uncovered"

  # The wildcard is the creator-app catch-all and claims no name. Without this,
  # a `*` leaking through would make the check above fail on every tree and get
  # switched off.
  printf '%s\n' "$REAL_LABELS" | grep -qxF '*' \
    && fail "the wildcard host leaked into the claimed-label set; the deploy path would refuse every tree" \
    || pass "the creator-app wildcard is excluded from the claimed-label set"
else
  # AN ABSENT INPUT IS A FAILURE, NOT A SKIP -- and this is the block where that
  # cost the most. Both `gate_arm edge_claimed_labels` and `gate_arm
  # reserved_app_names` are inside it, so when the reorg moved
  # reserved_names.rs this condition went false and BOTH arms stopped being
  # declared. Not "declared as zero", which gate_arms.sh refuses: not declared,
  # which it cannot see. The gate went on printing `arms=5 refusals=0` while the
  # deploy path it guards could not complete a single roll.
  fail "the edge-vs-reserved check did not run: missing $( [ -f "$EDGE_ART" ] || echo "$EDGE_ART" ) $( [ -f "$EDGE_RS" ] || echo "$EDGE_RS" ). Two arms are declared inside it, so a skip here removes them from the census rather than showing them as empty."
fi

# The control host is path-split at the edge: /v1/* never reaches control. A
# control route under that prefix would compile and test clean while
# being unreachable in every deployment that ships the Caddy rule.
#
# This is a registration STYLE boundary as well as a prefix search. Every
# resource must carry one full absolute literal, except the two audited constants
# pinned below, and scopes are forbidden. Without those restrictions a source
# grep misses `scope("/v1") + resource("/databases/x")`, an imported constant, or
# a dynamic catch-all while still reporting that it inspected every route.
CONTROL_SRC="$ROOT/crates/zeroship-control/src"
MIGRATE_V1_EDGE_HANDLERS="$(
  jq '[
        .config.apps.http.servers[].routes[]
        | select(any(.match[]?.host[]?; . == "control.zsdomain.invalid"))
        | .handle[]?.routes[]?
        | select(any(.match[]?.path[]?; . == "/v1/*"))
        | select(any(.. | objects | .upstreams?[]?.dial?; . == "migrate-server:9091"))
      ] | length' "$EDGE_ART" 2>/dev/null \
    || printf '0\n'
)"
MIGRATE_V1_EDGE_HANDLERS="${MIGRATE_V1_EDGE_HANDLERS:-0}"
if gate_arm migrate_v1_edge_handler "$MIGRATE_V1_EDGE_HANDLERS" 1 \
  && [ "$MIGRATE_V1_EDGE_HANDLERS" -eq 1 ]; then
  pass "the control host sends the complete /v1/* namespace directly to migrate-server"
else
  fail "the adapted control host must declare exactly one /v1/* handler for migrate-server; found $MIGRATE_V1_EDGE_HANDLERS"
fi

# THE INTERNAL-ROUTE BOUNDARY AT THE EDGE.
#
# Control's /internal/* routes are service-to-service. The catch-all used to
# forward them from the public edge, and `/internal/workers/enrol` in particular
# then observed CADDY as the peer - so control derived the proxy's address for
# every enrolment, while the `ProxyFronted` arm stayed silent because its input
# is control's own `trust_proxy` declaration rather than an observation.
#
# This is EDGE CONFIGURATION, so nothing in Rust notices it being deleted. That
# is the whole reason for this arm.
#
# It rules on the ADAPTED artifact, not the Caddyfile text, for the reason
# reserved_names.rs records: host and path claims are not statically decidable
# from the source grammar, and the adapted JSON is what Caddy actually does.
#
# THREE FACTS, and the third is the one a reader would omit. Stripe delivers to
# `/internal/webhooks/stripe` FROM THE INTERNET, so that one path must stay
# reachable - and because `handle` blocks match in order, a reorder putting the
# refusal first would swallow it and break webhook delivery while both other
# facts still held.
INTERNAL_EDGE_CHECKS=0
INTERNAL_EDGE_REFUSED="$(
  jq '[.config.apps.http.servers[].routes[]
       | select(any(.match[]?.host[]?; . == "control.zsdomain.invalid"))
       | .handle[]?.routes[]?
       | select(any(.match[]?.path[]?; . == "/internal/*"))
       | select([.. | objects | .upstreams?] | flatten | map(select(. != null)) | length == 0)
       | select(any(.. | objects | .handler?; . == "static_response"))] | length' "$EDGE_ART" 2>/dev/null \
    || printf '0\n'
)"
INTERNAL_EDGE_WEBHOOK="$(
  jq '[.config.apps.http.servers[].routes[]
       | select(any(.match[]?.host[]?; . == "control.zsdomain.invalid"))
       | .handle[]?.routes[]?
       | select(any(.match[]?.path[]?; . == "/internal/webhooks/*"))
       | select(any(.. | objects | .upstreams?[]?.dial?; . == "control:9090"))] | length' "$EDGE_ART" 2>/dev/null \
    || printf '0\n'
)"
INTERNAL_EDGE_ORDER="$(
  jq -r '[.config.apps.http.servers[].routes[]
          | select(any(.match[]?.host[]?; . == "control.zsdomain.invalid"))
          | .handle[]?.routes[]? | .match[]?.path[]?]
         | if (index("/internal/webhooks/*") // -1) < (index("/internal/*") // -1)
           then "ok" else "bad" end' "$EDGE_ART" 2>/dev/null \
    || printf 'bad\n'
)"
[ "${INTERNAL_EDGE_REFUSED:-0}" -eq 1 ] && INTERNAL_EDGE_CHECKS=$((INTERNAL_EDGE_CHECKS + 1))
[ "${INTERNAL_EDGE_WEBHOOK:-0}" -eq 1 ] && INTERNAL_EDGE_CHECKS=$((INTERNAL_EDGE_CHECKS + 1))
[ "$INTERNAL_EDGE_ORDER" = "ok" ] && INTERNAL_EDGE_CHECKS=$((INTERNAL_EDGE_CHECKS + 1))
if gate_arm internal_edge_boundary "$INTERNAL_EDGE_CHECKS" 3 \
  && [ "$INTERNAL_EDGE_CHECKS" -eq 3 ]; then
  pass "the edge refuses control's /internal/* and still delivers the Stripe webhook, in that order"
else
  fail "the control host must refuse /internal/* with no upstream, keep /internal/webhooks/* proxied to control, and order the webhook FIRST; refused=$INTERNAL_EDGE_REFUSED webhook=$INTERNAL_EDGE_WEBHOOK order=$INTERNAL_EDGE_ORDER"
fi

N_CONTROL_RESOURCES="$(
  rg -U --pcre2 -o 'web::resource\s*\(' "$CONTROL_SRC" 2>/dev/null | wc -l
)"
N_CONTROL_SCOPES="$(
  rg -U --pcre2 -o 'web::scope\s*\(' "$CONTROL_SRC" 2>/dev/null | wc -l
)"
N_CONTROL_ROUTE_DECLS=$((N_CONTROL_RESOURCES + N_CONTROL_SCOPES))

# MEASURED 2026-08-30 after deleting control's migration forward: 56 source
# constructors. Two are test-only workflow resources, leaving 54 production
# resources: 26 /api, 23 /internal, two /me, health, readiness, and the OAuth
# protected-resource metadata route. Floor 40 leaves room for real endpoint
# deletion while refusing the extractor-collapse failure where a clean tree and
# zero inspected routes would otherwise print the same result.
if ! gate_arm control_v1_route_collision "$N_CONTROL_ROUTE_DECLS" 40; then
  fail "control route enumeration found only $N_CONTROL_ROUTE_DECLS declaration(s); it cannot prove the edge prefix is collision-free"
fi

CONTROL_SCOPES="$(
  rg -n -U --pcre2 'web::scope\s*\(' "$CONTROL_SRC" 2>/dev/null || true
)"
if [ -n "$CONTROL_SCOPES" ]; then
  fail "control route scope(s) defeat the full-path collision check:"
  printf '%s\n' "$CONTROL_SCOPES" | sed 's/^/    /'
else
  pass "control declares no route scopes, so every resource carries its full path"
fi

CONTROL_ADMITTED_RESOURCES="$(
  rg -U --pcre2 -o \
    'web::resource\s*\(\s*(?:"/[^"\\]*"|device_grant::PROTECTED_RESOURCE_METADATA_PATH|WORKFLOW_ADVANCE_PATH)(?=\s*\))' \
    "$CONTROL_SRC" 2>/dev/null \
    | wc -l
)"
if [ "$CONTROL_ADMITTED_RESOURCES" = "$N_CONTROL_RESOURCES" ]; then
  pass "all $N_CONTROL_RESOURCES control resources use a full literal or an audited constant"
else
  fail "only $CONTROL_ADMITTED_RESOURCES of $N_CONTROL_RESOURCES control resources use an auditable full path:"
  rg -n -U --pcre2 \
    'web::resource\s*\(\s*(?!(?:"/[^"\\]*"|device_grant::PROTECTED_RESOURCE_METADATA_PATH|WORKFLOW_ADVANCE_PATH)(?=\s*\)))[^\n)]*' \
    "$CONTROL_SRC" 2>/dev/null \
    | sed 's/^/    /'
fi

grep -Fqx \
  'pub const PROTECTED_RESOURCE_METADATA_PATH: &str = "/.well-known/oauth-protected-resource";' \
  "$ROOT/crates/zeroship-core/src/device_grant.rs" \
  && pass "the allowed device-grant route constant is pinned outside /v1" \
  || fail "the allowed device-grant route constant changed; audit it before admitting the new path"
grep -Fqx \
  'pub const WORKFLOW_ADVANCE_PATH: &str = "/__zeroship/internal/workflow-advance";' \
  "$ROOT/crates/zeroship-workflow-scheduler/src/lib.rs" \
  && pass "the allowed workflow route constant is pinned outside /v1" \
  || fail "the allowed workflow route constant changed; audit it before admitting the new path"

CONTROL_V1_ROUTES="$(
  rg -n -U --pcre2 \
    'web::resource\s*\(\s*"/v1(?:/[^"\\]*)?"' \
    "$CONTROL_SRC" 2>/dev/null \
    || true
)"
if [ -z "$CONTROL_V1_ROUTES" ]; then
  pass "control declares no route shadowed by the edge's /v1/* handler"
else
  fail "control route(s) collide with the edge's /v1/* handler:"
  printf '%s\n' "$CONTROL_V1_ROUTES" | sed 's/^/    /'
fi

# THE FAILING ARM, one variable from its control. Both fixtures are real `caddy
# adapt` output generated by deploy/ops/caddy-claimed-hosts.sh; the matcher one
# is the live edge plus `@status host status.{$D}` and nothing else. The old
# text parser this replaced answered the unchanged four labels for exactly that
# file, so `status` is the shape a host claim takes when nothing sees it.
if [ -f "$FIX_CONTROL" ] && [ -f "$FIX_MATCHER" ]; then
  CTRL_LABELS="$(edge_claimed_labels "$FIX_CONTROL")"
  MATCH_LABELS="$(edge_claimed_labels "$FIX_MATCHER")"
  [ "$CTRL_LABELS" != "$MATCH_LABELS" ] \
    && pass "the fixture pair differs in the claimed-label set, so the two arms below are one variable apart" \
    || fail "both fixtures yield the same labels ($CTRL_LABELS); the failing arm is a copy of the passing one"
  printf '%s\n' "$MATCH_LABELS" | grep -qx status \
    && pass "a host claimed by a MATCHER rather than a site block is still seen (status)" \
    || fail "the matcher-claimed host is invisible to edge_claimed_labels: $MATCH_LABELS"
  printf '%s\n' "$REAL_RESERVED" | grep -qx status \
    && fail "CONTROL: 'status' is apparently reserved, so the failing arm below proves nothing" \
    || pass "CONTROL: 'status' is NOT in RESERVED_APP_NAMES, so a claim on it is genuinely uncovered"
  # The control arm must pass the property the other fails, or "refused" here
  # would be evidence about a check that refuses everything.
  ctrl_uncovered=""
  for l in $CTRL_LABELS; do
    printf '%s\n' "$REAL_RESERVED" | grep -qx "$l" || ctrl_uncovered="$ctrl_uncovered $l"
  done
  [ -z "$ctrl_uncovered" ] \
    && pass "CONTROL: the same edge WITHOUT the matcher claims only reserved names ($(echo $CTRL_LABELS))" \
    || fail "CONTROL: the unmutated edge fixture already claims uncovered names:$ctrl_uncovered"
fi

# A sentinel-less artifact must return NON-ZERO with no output, not "no hosts".
# An artifact whose shape moved answering "nothing is claimed" is the vacuous
# green the whole arm contract exists for, and it would be indistinguishable
# from an edge that claims nothing.
printf '{\n  "caddyfile_sha256": "deadbeef",\n  "config": {}\n}\n' >"$FIX/no-sentinel.json"
NOSENT="$(edge_claimed_labels "$FIX/no-sentinel.json")"; NOSENT_RC=$?
[ "$NOSENT_RC" != 0 ] && [ -z "$NOSENT" ] \
  && pass "an artifact with no sentinel_domain returns non-zero and no labels, so it cannot read as 'claims nothing'" \
  || fail "a sentinel-less artifact answered '$NOSENT' with rc=$NOSENT_RC"

# ------------------------------------------ the SOURCE tree, not the host
#
# WHY THIS EXISTS, measured 2026-08-19 on a real production deploy. Every other
# check in deploy-remote.sh is about the machine we deploy TO. Nothing was about
# the tree we BUILD, and the deploy was run from a git worktree - `git worktree
# add` DOES NOT POPULATE SUBMODULES, so third_party/zero-migrate was an empty
# directory. deploy/Dockerfile COPYs third_party/ into both stages, the image
# built for about twenty minutes, and then:
#
#   src/gen-types/addon.ts(66,8): error TS2307:
#       Cannot find module 'zeroship-migrate-node'
#   [ERR_PNPM_RECURSIVE_RUN_FIRST_FAIL] @zeroship/vite-plugin build
#
echo ""
echo "-- submodule_paths / submodule_manifests (fixtures)"

cat >"$FIX/gitmodules" <<'FIXTURE'
[submodule "third_party/zero-migrate"]
	path = third_party/zero-migrate
	url = https://example.invalid/zero-migrate.git
[submodule "vendor/other"]
	path = vendor/other/
	url = https://example.invalid/other.git
FIXTURE
expect_set "$(submodule_paths "$FIX/gitmodules")" "third_party/zero-migrate vendor/other" \
  "both declared paths are read, and a trailing slash is normalised away"

# A .gitmodules with no `path =` at all. The script REFUSES on this rather than
# looping zero times, because "no submodule was empty" is exactly what a check
# that has stopped looking reports.
printf '[submodule "x"]\n\turl = https://example.invalid/x.git\n' >"$FIX/gitmodules.nopath"
if usable "$(submodule_paths "$FIX/gitmodules.nopath")"; then
  fail "submodule_paths invented a path from a stanza that declares none"
else
  pass "a .gitmodules with no path line yields nothing, which the script turns into a refusal"
fi

# The two manifest producers, on fixtures shaped like the real files. The
# DECOY entries are the point: a prefix match would scoop `zero-migrate-other`,
# and that path is not a submodule member at all.
cat >"$FIX/lock.yaml" <<'FIXTURE'
importers:

  .: {}

  sdks/vite-plugin:
    dependencies: {}

  third_party/zero-migrate/crates/zeroship-migrate-node:
    dependencies: {}

  third_party/zero-migrate/packages/zero-migrate:
    dependencies: {}

  third_party/zero-migrate-other/packages/decoy:
    dependencies: {}
FIXTURE
cat >"$FIX/cargo.toml" <<'FIXTURE'
[workspace]
members = [
    "crates/*",
    "third_party/zero-migrate",
]
[workspace.dependencies]
zero-migrate = { path = "third_party/zero-migrate/crates/zero-migrate" }
decoy = { path = "third_party/zero-migrate-other/crates/decoy" }
FIXTURE
expect_set "$(submodule_manifests third_party/zero-migrate "$FIX/lock.yaml" "$FIX/cargo.toml" | sort -u)" \
  "third_party/zero-migrate/crates/zeroship-migrate-node/package.json
   third_party/zero-migrate/packages/zero-migrate/package.json
   third_party/zero-migrate/Cargo.toml
   third_party/zero-migrate/crates/zero-migrate/Cargo.toml" \
  "lockfile importers become package.json and cargo paths become Cargo.toml, under the submodule only"

if submodule_manifests third_party/zero-migrate "$FIX/lock.yaml" "$FIX/cargo.toml" | grep -q 'zero-migrate-other'; then
  fail "a sibling path that merely shares the submodule's PREFIX was scooped in; the check would demand files from a directory that is not a submodule"
else
  pass "third_party/zero-migrate-other is NOT matched by the third_party/zero-migrate prefix"
fi

# Against the REAL files, because the fixtures above only prove the parser and
# the whole point is what THIS repo's build opens. Both modules named in the
# 2026-08-19 TS2307 output must appear, or the check would not have caught it.
if [ -f "$ROOT/pnpm-lock.yaml" ] && [ -f "$ROOT/Cargo.toml" ] && [ -f "$ROOT/.gitmodules" ]; then
  REAL_SUB="$(submodule_paths "$ROOT/.gitmodules")"
  REAL_MAN="$(for s in $REAL_SUB; do
    submodule_manifests "$s" "$ROOT/pnpm-lock.yaml" "$ROOT/Cargo.toml"
  done | sort -u)"
  N_REAL_MAN=0
  usable "$REAL_MAN" && N_REAL_MAN="$(printf '%s\n' "$REAL_MAN" | grep -c .)"
  # The manifests the deploy preflight would open, one verdict each. MEASURED
  # 2026-08-20: 7, across the single declared submodule. Floor 3: the block below
  # names TWO manifests by hand, so a floor of 2 could be met by an enumeration
  # that had collapsed to exactly those two and nothing else.
  if ! gate_arm submodule_manifests "$N_REAL_MAN" 3; then
    fail "no manifest was derived for any declared submodule of this repo ($N_REAL_MAN); the preflight would inspect nothing"
  else
    miss=""
    for m in third_party/zero-migrate/crates/zeroship-migrate-node/package.json \
             third_party/zero-migrate/Cargo.toml; do
      printf '%s\n' "$REAL_MAN" | grep -qx "$m" || miss="$miss $m"
    done
    [ -z "$miss" ] \
      && pass "the real derivation names the addon package and the engine workspace manifest" \
      || fail "the real derivation missed:$miss"
  fi
fi

# ------------------------------------------------------- anti-vacuity itself
echo ""
echo "-- the comparator refuses an empty extraction"
EMPTY_OUT="$(compose_vars '' "$FIX/empty.yml")"
if usable "$EMPTY_OUT"; then
  fail "an empty compose file produced [$EMPTY_OUT]; the fixture or the parser is not what this gate thinks it is"
else
  pass "an empty compose file extracts nothing (the input for the check below)"
fi
[ "$(set_verdict "$EMPTY_OUT" "")" = "empty" ] \
  && pass "empty-vs-empty is reported EMPTY, not equal, so a broken parser cannot pass every set assertion" \
  || fail "empty-vs-empty compared EQUAL; every set assertion in this file would pass vacuously"
[ "$(set_verdict "" "ANY_VAR")" = "empty" ] \
  && pass "empty-vs-nonempty is reported EMPTY" \
  || fail "an empty extraction was not reported as empty"
[ "$(set_verdict "A_VAR B_VAR" "B_VAR A_VAR")" = "ok" ] \
  && pass "CONTROL: the comparator is order-insensitive and does report equal sets as equal" \
  || fail "the comparator failed to match two equal sets; it discriminates nothing"
[ "$(set_verdict "A_VAR" "B_VAR")" = "differ" ] \
  && pass "CONTROL: the comparator reports different sets as different" \
  || fail "the comparator matched two different sets"

# ---------------------------------------------------------- rename pairing
echo ""
echo "-- rename_suspects pairing"

# The bug that shipped and was caught by hand.
SUS="$(rename_suspects "ZEROSHIP_SCHEME" "ZEROSHIP_ORIGIN_SCHEME")"
if [ -n "$SUS" ]; then
  pass "ZEROSHIP_SCHEME (orphan) pairs with ZEROSHIP_ORIGIN_SCHEME (absent, would default) and refuses the deploy"
else
  fail "the historical rename was NOT detected; deleting ZEROSHIP_ORIGIN_SCHEME would silently take the http default"
fi

# Steady state on a healthy host: orphans and defaulted variables coexist.
SUS="$(rename_suspects "LEGACY_PORT STALE_TIMEOUT" "ZEROSHIP_DOMAIN OPENAI_API_KEY")"
if [ -z "$SUS" ]; then
  pass "unrelated orphans plus unrelated defaulted variables do NOT pair (this is normal steady state)"
else
  fail "unrelated names paired, so the guard fires on a healthy host and gets removed:$SUS"
fi

# Shared token, but the token is in the stoplist. Two variables of the same
# service are not a rename of each other.
SUS="$(rename_suspects "ZEROSHIP_GATEWAY_BROKER_SECRET_FILE" "ZEROSHIP_GATEWAY_DATABASE_URL")"
if [ -z "$SUS" ]; then
  pass "the two gateway names do not pair (GATEWAY, SECRET, DATABASE and URL are stoplisted)"
else
  fail "a stoplisted token paired two unrelated variables:$SUS"
fi

# A short shared token must not pair either: ABC is 3 characters.
SUS="$(rename_suspects "OLD_ABC_HOST" "NEW_ABC_TARGET")"
if [ -z "$SUS" ]; then
  pass "a shared token shorter than 4 characters does not pair"
else
  fail "a 3-character token paired two variables:$SUS"
fi

# CONTROL for the two negatives above: the same shape with a long, non-stoplist
# shared token MUST pair. Otherwise "did not pair" could mean the function is
# simply dead.
SUS="$(rename_suspects "OLD_TELEMETRY_HOST" "NEW_TELEMETRY_TARGET")"
if [ -n "$SUS" ]; then
  pass "CONTROL: a shared 9-character non-stoplisted token DOES pair"
else
  fail "no pairing at all; the negatives above prove nothing"
fi

# --- rule 2, canonical re-scoping ---------------------------------------
# The eight compose aliases renamed on 2026-08-13 to satisfy the alias-equality
# rule. Rule 1 was SILENT on six of these because every token they own is
# stoplisted, and every one carries a `:-default`, so a host still setting only
# the old name would have rendered green on the built-in default. Each of these
# assertions FAILS against the pre-2026-08-13 rename_suspects.
for pair in \
  "CONTROL_DATABASE_URL ZEROSHIP_CONTROL_DATABASE_URL" \
  "GATEWAY_DATABASE_URL ZEROSHIP_GATEWAY_DATABASE_URL" \
  "WORKER_DATABASE_URL ZEROSHIP_WORKER_DATABASE_URL" \
  "MIGRATE_SERVER_DATABASE_URL ZEROSHIP_MIGRATE_SERVER_DATABASE_URL" \
  "WORKER_KV_URL ZEROSHIP_WORKER_KV_URL" \
  "PROVISION_DATABASE_URL ZEROSHIP_MIGRATE_SERVER_PROVISION_DATABASE_URL" \
  "STRIPE_WEBHOOK_SECRET ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET"; do
  set -- $pair
  SUS="$(rename_suspects "$1" "$2")"
  if [ -n "$SUS" ]; then
    pass "$1 pairs with its canonical $2"
  else
    fail "$1 -> $2 was NOT detected; a host keeping the old name would silently take the compose default"
  fi
done

# The eighth is not a pure re-scoping: DB abbreviates to DATABASE as well.
SUS="$(rename_suspects "AUTH_DB_URL" "ZEROSHIP_AUTH_DATABASE_URL")"
[ -n "$SUS" ] && pass "AUTH_DB_URL pairs with ZEROSHIP_AUTH_DATABASE_URL (DB expands to DATABASE)" \
              || fail "the DB/DATABASE abbreviation was not resolved; AUTH_DB_URL -> ZEROSHIP_AUTH_DATABASE_URL is silent"

# NEGATIVE for rule 2: a bare name that is NOT a suffix of the needed one must
# still not pair, or rule 2 has simply replaced "never fires" with "always
# fires". Same shape as the positives, one variable changed: the scope.
SUS="$(rename_suspects "WORKER_KV_URL" "ZEROSHIP_CONTROL_DATABASE_URL")"
[ -z "$SUS" ] && pass "a bare name that is not a suffix of the needed name does not pair" \
              || fail "rule 2 paired two unrelated names:$SUS"

# NEGATIVE for rule 2: a single generic token must not pair with everything that
# happens to end in it.
SUS="$(rename_suspects "URL" "ZEROSHIP_CONTROL_DATABASE_URL")"
[ -z "$SUS" ] && pass "a single-token orphan does not pair by suffix alone" \
              || fail "a bare generic token paired by suffix:$SUS"

# Either side empty is the fresh-install case and must never refuse.
SUS="$(rename_suspects "" "ZEROSHIP_ORIGIN_SCHEME")"
[ -z "$SUS" ] && pass "no orphans at all cannot be a rename (fresh install)" \
              || fail "pairing fired with no orphans:$SUS"
SUS="$(rename_suspects "ZEROSHIP_SCHEME" "")"
[ -z "$SUS" ] && pass "an orphan with nothing defaulted is only a note, not a refusal" \
              || fail "pairing fired with nothing defaulted:$SUS"

# ------------------------------------------- the real compose, escaped refs
# Conditional on purpose: the shipped compose file is owned by other work, and
# a gate that goes red when someone legitimately edits it is a gate that gets
# deleted. This asserts only that whatever escaped references exist there stay
# out of the extraction.
echo ""
echo "-- the shipped compose file (escaped references only)"
if [ -f "$REAL_COMPOSE" ]; then
  # Uppercase only: the extraction can only ever emit [A-Z_]+, so a lowercase
  # escaped name (`$${ip}`) could not leak into it and asserting about one
  # would be a free pass.
  ESCAPED_NAMES="$(grep -oE '\$\$\{?[A-Z_]+' "$REAL_COMPOSE" | sed -E 's/^\$\$\{?//' | sort -u)"
  if [ -z "$ESCAPED_NAMES" ]; then
    echo "  note the shipped compose has no \$\${VAR} reference today; nothing to exclude"
  else
    REAL_ALL="$(compose_vars '' "$REAL_COMPOSE")"
    if ! usable "$REAL_ALL"; then
      fail "compose_vars extracted NOTHING from the shipped compose file"
    else
      leaked=""
      for n in $ESCAPED_NAMES; do
        printf '%s\n' $REAL_ALL | grep -qxF "$n" && leaked="$leaked $n"
      done
      [ -z "$leaked" ] \
        && pass "escaped references in the shipped compose stay out of the extraction ($(echo $ESCAPED_NAMES))" \
        || fail "escaped reference(s) counted as compose variables:$leaked"
    fi
  fi
fi

# ------------------------------------------------------- argument handling
#
# Both scripts are RUN here, not sourced. Every case below exits before the
# first ssh, which is what makes them runnable with no host: deploy-remote.sh
# validates --host before it connects, and deploy-app.sh validates its
# arguments and its token before it connects.
echo ""
echo "-- argument handling (scripts executed, all paths exit before ssh)"

run_case() { # $1 label, $2 expected-exit ('nonzero' or a number), $3 expected substring, rest: argv
  local label="$1" want_rc="$2" want_txt="$3"; shift 3
  local out rc
  out="$("$@" 2>&1)"; rc=$?
  local rc_ok=0
  if [ "$want_rc" = "nonzero" ]; then [ "$rc" -ne 0 ] && rc_ok=1; else [ "$rc" = "$want_rc" ] && rc_ok=1; fi
  local txt_ok=0
  [[ "$out" == *"$want_txt"* ]] && txt_ok=1
  # Report WHICH half failed. "exit N and text missing" reads as two problems
  # when it is usually one, and the wrong one gets investigated.
  if [ "$rc_ok" = 1 ] && [ "$txt_ok" = 1 ]; then
    pass "$label"
  elif [ "$rc_ok" != 1 ] && [ "$txt_ok" = 1 ]; then
    fail "$label: exit was $rc, wanted $want_rc (the message was right)"
  elif [ "$rc_ok" = 1 ]; then
    fail "$label: exit $rc was right but the output never said '$want_txt'"
  else
    fail "$label: exit was $rc, wanted $want_rc, and the output never said '$want_txt'"
  fi
}

run_case "deploy-remote.sh with no --host exits non-zero and says so" \
  2 "--host is required" env -u ZEROSHIP_TOKEN "$REMOTE"
run_case "deploy-remote.sh rejects an unknown argument" \
  nonzero "unknown argument: --nope" "$REMOTE" --host h --nope
run_case "deploy-remote.sh --help prints usage and exits 0" \
  0 "Build the platform image here" "$REMOTE" --help

run_case "deploy-app.sh with no --host exits non-zero and says so" \
  nonzero "--host is required" "$APPDEP"
run_case "deploy-app.sh with no --app exits non-zero and says so" \
  nonzero "--app is required" "$APPDEP" --host h
run_case "deploy-app.sh with neither --dir nor --zship exits non-zero and says so" \
  nonzero "one of --dir or --zship is required" "$APPDEP" --host h --app a
run_case "deploy-app.sh rejects an unknown argument" \
  nonzero "unknown argument: --nope" "$APPDEP" --host h --app a --nope
run_case "deploy-app.sh --help prints usage and exits 0" \
  0 "Deploy a creator app" "$APPDEP" --help

# The token is read from the environment or a file and never from argv, because
# argv is world-readable in `ps`. The refusal has to SAY that, or the next
# person adds --token and the property is gone.
run_case "deploy-app.sh refuses with no token available" \
  nonzero "no token. Set ZEROSHIP_TOKEN" env -u ZEROSHIP_TOKEN "$APPDEP" --host h --app a --dir d
run_case "the refusal explains why a token argument is not accepted" \
  nonzero "deliberately not accepted as an argument" env -u ZEROSHIP_TOKEN "$APPDEP" --host h --app a --dir d
run_case "deploy-app.sh refuses an unreadable --token-file" \
  nonzero "cannot read token file" env -u ZEROSHIP_TOKEN "$APPDEP" --host h --app a --dir d --token-file "$FIX/nope"

# ----------------------------------------------------- sourcing is inert
echo ""
echo "-- sourcing deploy-remote.sh has no side effects"
SRC_OUT="$(bash -c 'source "$1" >/dev/null; echo SOURCED' _ "$REMOTE" 2>&1)"; SRC_RC=$?
if [ "$SRC_RC" = 0 ] && [ "$SRC_OUT" = "SOURCED" ]; then
  pass "sourcing exits 0 and prints nothing of its own (no preflight, no ssh, no argument parsing)"
else
  fail "sourcing produced rc=$SRC_RC output [$SRC_OUT]; main() ran or something else escaped the guard"
fi

# ============================================================================
# --rollback, driven through a STUB ssh
# ============================================================================
#
# WHY THIS SECTION EXISTS. --rollback had never been executed, by anyone, in
# any form. It is the thing you reach for during an outage, and it ends in
# `docker compose up -d` on a live stack, so a defect in it is not a failed
# test, it is a failed recovery.
#
# HOW IT IS REACHED WITHOUT A HOST. A stub `ssh` earlier on PATH records every
# command the script sends AND runs the remote body locally with `bash -c`,
# with the script pointed at a sandbox directory via --remote-dir. `docker`,
# `scp` and `openssl` are stubbed to record-and-succeed. So two different
# things are under test at once, and they must not be confused:
#
#   - the COMMAND STREAM (what the script decided to send). This is real
#     evidence about the script's control flow.
#   - the remote body's own LOGIC (which backup it picks, what it copies),
#     executed here against seeded files. Real evidence about the snippet,
#     under THIS machine's bash and GNU coreutils.
#
# WHAT NONE OF IT PROVES: that a real host does the right thing with the
# stream. ssh transport, the remote login shell, remote `docker compose`,
# whether the stack actually comes back up, and every quoting hazard that only
# appears when the body crosses ssh's own shell are all untested here and
# cannot be tested without a host.
echo ""
echo "-- --rollback (stub ssh; the remote body is executed locally)"

STUB="$FIX/stub"; mkdir -p "$STUB"

cat >"$STUB/ssh" <<'STUBEOF'
#!/usr/bin/env bash
# Record the command, then run it locally. Options come as `-o X=Y` pairs, so
# drop pairs, then the host, and the single remaining argument is the body.
while [ $# -gt 0 ]; do
  case "$1" in
    -o) shift 2 ;;
    -*) shift ;;
    *)  break ;;
  esac
done
host="$1"; shift
{ printf '### ssh %s\n' "$host"; printf '%s\n' "$*"; } >>"$ZS_GATE_CAPTURE"
bash -c "$*"
STUBEOF

cat >"$STUB/scp" <<'STUBEOF'
#!/usr/bin/env bash
printf '### scp %s\n' "$*" >>"$ZS_GATE_CAPTURE"
exit 0
STUBEOF
# The docker stub can be told to FAIL for one command shape. That is how the
# deploy-path cases below reach "a server rejected the configuration" and "the
# restored configuration does not render" without a host, an image or a server.
# It proves what the SCRIPT does with a refusal, not that any particular
# configuration would be refused; the second half is measured separately by
# running the real binaries (see the conditional block near the end).
cat >"$STUB/docker" <<'STUBEOF'
#!/usr/bin/env bash
printf '### docker %s\n' "$*" >>"$ZS_GATE_CAPTURE"
if [ -n "${ZS_GATE_DOCKER_FAIL:-}" ]; then
  case "$*" in *"$ZS_GATE_DOCKER_FAIL"*) exit 1 ;; esac
fi
# Two questions the script asks docker that need a real ANSWER, not just an
# exit status: which images the compose file resolves to, and which image
# control is actually running. A stub that says nothing makes the happy path
# fail for reasons unrelated to anything under test, and a control that cannot
# succeed is not a control.
# The frozen-migration check asks two more, and they are ANSWERS too. The
# existence probe must say `t` or `f` and nothing else, because the script
# refuses any third reply on purpose -- a down database and an empty deployment
# both produce no output, and folding them together would report "nothing is
# frozen" for a database it simply could not reach. ZS_GATE_JOURNAL_EXISTS
# drives that arm; ZS_GATE_STATUS names a file holding a `zero-migrate status
# --json` document.
case "$*" in
  *"config --images"*) printf '%s\n' "${ZS_GATE_RENDERED_IMAGE-${ZS_GATE_RUNNING_IMAGE:-}}" ;;
  inspect*)            printf '%s\n' "${ZS_GATE_RUNNING_IMAGE:-}" ;;
  *to_regclass*)       printf '%s\n' "${ZS_GATE_JOURNAL_EXISTS-t}" ;;
  *"--verb status"*)
    [ -n "${ZS_GATE_STATUS:-}" ] && [ -r "${ZS_GATE_STATUS:-}" ] && cat "$ZS_GATE_STATUS"
    ;;
esac
exit 0
STUBEOF
# Must print something: the real call is \$(openssl rand -hex 32).
cat >"$STUB/openssl" <<'STUBEOF'
#!/usr/bin/env bash
printf '### openssl %s\n' "$*" >>"$ZS_GATE_CAPTURE"
echo deadbeef
STUBEOF
chmod +x "$STUB"/*

# The three files the rollback contract names, and two backup generations of
# each. GEN2 has the LATER stamp in its name and the EARLIER mtime; GEN1 the
# reverse. That inversion is the whole point of the fixture: without it, a
# selector that reads mtime and a selector that reads the stamp agree, and
# "restored GEN2" would prove nothing about which one the script used.
#
# The inversion is not hypothetical. `cp -a` preserves mtime, so a backup
# carries the mtime of the file's LAST CONTENT CHANGE, not the moment the
# backup was taken. Anything that puts an older mtime back on the live file --
# this script's own rollback, an operator's `cp -p`, `rsync -a` or `tar -xp`
# from an archive, all normal outage moves -- makes the next backup's mtime
# older than an earlier backup's.
#
# THE MEMBER LIST COMES FROM THE SCRIPT, not from a copy here. A member added
# there and forgotten here would leave every case below seeding an incomplete
# generation, which the script correctly refuses -- and the whole section would
# go red for the wrong reason, or worse, a member could be REMOVED there and
# this file would never notice.
ROLL_FILES="$(printf '%s\n' $SNAPSHOT_MEMBERS | grep -v '^secrets\.tar$' | tr '\n' ' ')"

# Two arms, because these two derived lists drive the rest of this section and
# every loop over them is a `for ... done` whose empty case is silent - the
# restore-content loop, the secret-comparison loop and the snapshot-coverage
# loop all report success on an empty list. They are declared here, where the
# lists are built, rather than at each loop.
#
# MEASURED 2026-08-20 against the sourced deploy-remote.sh and the shipped
# compose: SNAPSHOT_MEMBERS is 5 (compose/.env, compose/docker-compose.yml,
# ops/Caddyfile, ops/zeroship.toml, secrets.tar) and ROLL_FILES is those minus
# secrets.tar, so 4. Floor 3 each: a member being added or removed is a real
# change to the deploy contract and moves the number by one, while the failure
# guarded here - the sourced script renames the variable, or its shape stops
# splitting on whitespace - takes it to 0 or 1.
gate_arm snapshot_members "$(printf '%s\n' $SNAPSHOT_MEMBERS | grep -c .)" 3 || true
gate_arm rollback_files "$(printf '%s\n' $ROLL_FILES | grep -c .)" 3 || true

# The secret files the shipped compose references. Derived, for the same
# reason: this gate must not carry its own copy of a list the script reads.
SECRET_NAMES="$(secret_files "$REAL_COMPOSE" | tr '\n' ' ')"
# The SAME extraction as `real_secret_files` above, declared again because it is
# a second, independent read feeding different consumers: that arm guards the
# dev.rs drift check, this one guards seed_sandbox and the two rollback loops
# that compare restored key material. The dev.rs block is also conditional on
# crates/zeroship-cli/src/dev.rs existing, so its arm can be absent from a run this one
# is present in. MEASURED 2026-08-20: 7. Floor 4, as above.
gate_arm rollback_secret_files "$(printf '%s\n' $SECRET_NAMES | grep -c .)" 4 || true

seed_secrets() { # $1 sandbox. Writes the referenced secret files, non-empty.
  local d="$1" n
  mkdir -p "$d/secrets"
  for n in $SECRET_NAMES; do printf 'SECRET-%s\n' "$n" >"$d/secrets/$n"; done
}

seed_sandbox() { # $1 dir, $2 body of the current (post-deploy) files
  local d="$1" body="$2" f
  mkdir -p "$d/compose" "$d/ops"
  for f in $ROLL_FILES; do
    printf '%s\n' "$body" >"$d/$f"
    printf 'GEN1\n' >"$d/$f.bak.20260101000000"
    printf 'GEN2\n' >"$d/$f.bak.20260812010101"
    touch -d '2026-08-20 00:00:00' "$d/$f.bak.20260101000000"   # oldest stamp, NEWEST mtime
    touch -d '2026-01-01 00:00:00' "$d/$f.bak.20260812010101"   # newest stamp, OLDEST mtime
  done
  # The secrets member of each generation. GEN1 and GEN2 hold DIFFERENT bytes
  # for the same file, so "which generation was restored" is answerable from
  # the key material and not only from the config files. Each archive is built
  # from the real directory at its real path with `tar -cpP`, which is exactly
  # how the script builds it -- an archive assembled some other way would be
  # testing this file's idea of the format rather than the script's.
  local n gen
  for gen in 20260101000000:GEN1 20260812010101:GEN2; do
    mkdir -p "$d/secrets"
    for n in $SECRET_NAMES; do printf '%s-%s\n' "${gen#*:}" "$n" >"$d/secrets/$n"; done
    tar -cpPf "$d/secrets.tar.bak.${gen%%:*}" "$d/secrets"
  done
  # Leave the live directory holding NEITHER generation, so a restore is
  # visible as a change rather than as a coincidence.
  for n in $SECRET_NAMES; do printf 'CURRENT-%s\n' "$n" >"$d/secrets/$n"; done
  printf 'ZEROSHIP_SECRETS_DIR=%s\n' "$d/secrets" >>"$d/compose/.env"
}

CAP_N=0
run_rollback() { # $1 sandbox, rest: extra argv. Sets ROLL_RC and CAP.
  CAP_N=$((CAP_N+1))
  CAP="$FIX/capture.$CAP_N"
  : >"$CAP"
  local sb="$1"; shift
  ROLL_OUT="$(env PATH="$STUB:$PATH" ZS_GATE_CAPTURE="$CAP" \
    "$REMOTE" --host fakehost --remote-dir "$sb" --rollback "$@" 2>&1)"
  ROLL_RC=$?
}

# The scanner. `seen` is asserted BOTH ways below so that "not seen" is a
# statement about the stream and not about a scanner that never matches.
#
# `### <prog>` lines are RECORDED INVOCATIONS: that program's stub actually
# ran. Everything else in the capture is the TEXT of a command sent over ssh,
# which is a far weaker fact -- the rollback body contains the literal string
# `docker compose up -d` inside a branch it may never reach, so matching that
# text proves the body was composed, not that anything ran. The first draft of
# this section asserted on the unprefixed string and duly reported a stack
# restart in the one case where the script had refused and restarted nothing.
# Prefix the token with `### ` unless you really do mean the sent text.
seen() { grep -qF -- "$2" "$1"; }

SB="$FIX/sb_main"; seed_sandbox "$SB" CURRENT

# FIXTURE SELF-CHECK. If the mtimes are not inverted, a selector that reads
# mtime and one that reads the stamp agree, and every assertion below passes
# without discriminating between them.
if [ "$(stat -c %Y "$SB/compose/.env.bak.20260812010101")" \
   -lt "$(stat -c %Y "$SB/compose/.env.bak.20260101000000")" ]; then
  pass "FIXTURE: the newest-stamp backup carries the OLDEST mtime, so stamp-order and mtime-order disagree"
else
  fail "FIXTURE: the two backups do not have inverted mtimes; the selection assertions below cannot discriminate"
fi

run_rollback "$SB"

[ "$ROLL_RC" = 0 ] \
  && pass "--rollback exits 0" \
  || fail "--rollback exited $ROLL_RC; output: $ROLL_OUT"

# One assertion per file: a restore loop that drops one file must redden
# exactly one line, not collapse three into a single unreadable failure.
for f in $ROLL_FILES; do
  got="$(cat "$SB/$f" 2>/dev/null)"
  if [ "$got" = "GEN2" ]; then
    pass "$f restored from the MOST RECENT backup by stamp (.bak.20260812010101)"
  elif [ "$got" = "GEN1" ]; then
    fail "$f was restored from .bak.20260101000000 -- the OLDEST backup. The selector is reading mtime, which cp -a preserves from the source file, not the backup stamp"
  else
    fail "$f was not restored at all (content [$got], wanted GEN2)"
  fi
done

# THE SECRET FILES, which no rollback touched before 2026-08-13. Provisioning
# writes into that directory during a deploy, so it is a deploy INPUT and has to
# come back with the rest. The live copies were seeded to CURRENT-<name>, so
# GEN2-<name> can only have come from the archive.
roll_secret_bad=""
for n in $SECRET_NAMES; do
  [ "$(cat "$SB/secrets/$n" 2>/dev/null)" = "GEN2-$n" ] || roll_secret_bad="$roll_secret_bad $n"
done
[ -z "$roll_secret_bad" ] \
  && pass "the secret files are restored from the same generation as the config ($(echo $SECRET_NAMES))" \
  || fail "--rollback left these secret files un-restored:$roll_secret_bad. A deploy provisions into that directory, so a rollback that skips it does not return the host to its pre-deploy state"

# The render check on the RESTORED files, which is what stops a rollback from
# restarting the stack onto a state that was already broken before the deploy.
seen "$CAP" '### docker compose config' \
  && pass "--rollback verifies the restored configuration renders" \
  || fail "--rollback restarted the stack without checking that what it restored renders"

seen "$CAP" '### docker compose up -d --remove-orphans' \
  && pass "--rollback restarts the whole stack (docker compose up -d --remove-orphans RAN)" \
  || fail "--rollback never reached 'docker compose up -d --remove-orphans'; it restored files and left the stack on the old ones"

# THE SHORT-CIRCUIT. This is what makes --rollback safe to run in a panic: it
# must not build, must not push, must not overwrite host config, and must not
# provision anything.
#
# `### docker compose config` USED TO BE ON THIS LIST and is not any more. It
# was a proxy for "did not fall through to the deploy path", and the rollback
# now runs it deliberately, on the RESTORED files, to refuse to restart onto a
# configuration that does not render. The two uses are indistinguishable in the
# capture, so the token moved from the negatives to the positives below rather
# than being asserted both ways. The fall-through it was standing in for is
# still covered, by `### docker build` / `### docker push` / `### scp`.
FORBIDDEN="### docker build|### docker push|### scp|### openssl|ZEROSHIP_IMAGE="
IFS='|' read -r -a FORBIDDEN_TOKENS <<<"$FORBIDDEN"
for tok in "${FORBIDDEN_TOKENS[@]}"; do
  seen "$CAP" "$tok" \
    && fail "--rollback reached '$tok'; it is not a short-circuit and is not safe to run blind" \
    || pass "--rollback never reaches '$tok'"
done

# CONTROL for the negatives above. Without it, a scanner that matches nothing
# would report all of them as clean.
printf '### docker build --target runtime\n### docker push x\n### scp a b\n### openssl rand -hex 32\n### docker compose config -q\nZEROSHIP_IMAGE=x\n' >"$FIX/synthetic"
ctl_missed=""
for tok in "${FORBIDDEN_TOKENS[@]}"; do
  seen "$FIX/synthetic" "$tok" || ctl_missed="$ctl_missed [$tok]"
done
[ -z "$ctl_missed" ] \
  && pass "CONTROL: the scanner finds every forbidden command in a capture that contains them" \
  || fail "CONTROL: the scanner missed$ctl_missed in a capture that contains them; the negatives above prove nothing"

# --------------------------------------------------------- argument contract
run_case "--rollback still requires --host" \
  2 "--host is required" env PATH="$STUB:$PATH" ZS_GATE_CAPTURE=/dev/null "$REMOTE" --rollback

case "$ROLL_OUT" in
  *"--registry is required"*) fail "--rollback demanded --registry; there is nothing to build" ;;
  *) pass "--rollback does not require --registry" ;;
esac

# A --registry that IS supplied must not switch the build back on. The host
# .env here is generated from the shipped compose file's own variable surface,
# so the required-variable contract PASSES and nothing earlier than the build
# would stop a fall-through. Verified by mutation: with the `--rollback` branch
# disabled this case reaches `### docker build`, `### docker push`, `### scp`,
# `### openssl` and `### docker compose config`, so the negatives here are
# about a reachable path rather than one that dies of unrelated causes.
#
# Two assertions, not one: exit status and command stream fail for different
# reasons and a compound verdict sends you to investigate the wrong half.
SB_REG="$FIX/sb_registry"; seed_sandbox "$SB_REG" CURRENT
compose_vars '' "$REAL_COMPOSE" | sed 's/$/=x/' >"$SB_REG/compose/.env"
cp -a "$SB_REG/compose/.env" "$SB_REG/compose/.env.bak.20260812010101"
run_rollback "$SB_REG" --registry ghcr.io/example/zeroship-platform
[ "$ROLL_RC" = 0 ] \
  && pass "--rollback with --registry supplied exits 0" \
  || fail "--rollback with --registry exited $ROLL_RC: $ROLL_OUT"
reg_hit=""
for tok in '### docker build' '### docker push' '### scp'; do
  seen "$CAP" "$tok" && reg_hit="$reg_hit [$tok]"
done
[ -z "$reg_hit" ] \
  && pass "--rollback with --registry supplied and a passing variable contract still builds, pushes and syncs nothing" \
  || fail "--rollback fell through to the deploy path and reached$reg_hit"

# ------------------------------------------------------ preflight still runs
run_rollback "$FIX/no_such_dir"
[ "$ROLL_RC" != 0 ] && [[ "$ROLL_OUT" == *"does not exist on"* ]] \
  && pass "--rollback runs preflight first and refuses a remote dir that is not there" \
  || fail "--rollback did not preflight the remote dir (rc=$ROLL_RC): $ROLL_OUT"

# ---------------------------------------------------------- a missing backup
#
# The script REFUSES rather than restoring what it can. The argument, and the
# counter-argument, are in the comment above the scan in deploy-remote.sh; the
# assertions here are what stop it quietly reverting to a partial restore.
SB_MISS="$FIX/sb_missing"; seed_sandbox "$SB_MISS" CURRENT
rm -f "$SB_MISS"/ops/Caddyfile.bak.*
run_rollback "$SB_MISS"

# EXIT 3 EXACTLY, not merely non-zero. Measured by mutation: deleting the scan
# leaves `cp -a "" ops/Caddyfile` to fail under set -e, which also exits
# non-zero, so "non-zero" cannot tell a deliberate refusal from a crash that
# happens to abort in roughly the right place. 3 is reached only by the scan.
[ "$ROLL_RC" = 3 ] \
  && pass "a missing backup for one file makes --rollback exit 3 (the refusal, not an incidental failure)" \
  || fail "--rollback exited $ROLL_RC with no backup for ops/Caddyfile; 3 is the refusal, 0 would be a partial recovery reported as success"
[[ "$ROLL_OUT" == *"ops/Caddyfile"* && "$ROLL_OUT" == *"REFUSING to roll back"* ]] \
  && pass "the refusal says it is refusing and names the file that has no backup" \
  || fail "the refusal did not both say REFUSING and name ops/Caddyfile: $ROLL_OUT"
if [ "$(head -1 "$SB_MISS/compose/.env")" = "CURRENT" ] \
   && [ "$(cat "$SB_MISS/compose/docker-compose.yml")" = "CURRENT" ]; then
  pass "the two files that DO have backups are left untouched (no half-restored configuration)"
else
  fail "--rollback restored some files and not others; the host is now on a mixed configuration"
fi
seen "$CAP" '### docker compose up -d' \
  && fail "--rollback restarted the stack on a half-restored configuration" \
  || pass "--rollback does not restart the stack when it refuses"

# --------------------------------------------- selection edge cases, pinned
#
# TIE. Two backups with byte-identical mtimes is not exotic: this script's own
# rollback does `cp -a backup live`, so the NEXT deploy's backup inherits the
# earlier backup's mtime exactly. MEASURED on GNU coreutils 9.10: `ls -1t`
# breaks a tie by name ASCENDING, so `head -1` on a tie returned the OLDEST
# stamp. Selecting on the stamp removes the tie from the question entirely.
SB_TIE="$FIX/sb_tie"; seed_sandbox "$SB_TIE" CURRENT
for f in $ROLL_FILES; do touch -d '2026-03-03 03:03:03' "$SB_TIE/$f".bak.*; done
run_rollback "$SB_TIE"
[ "$(cat "$SB_TIE/compose/.env")" = "GEN2" ] \
  && pass "backups with IDENTICAL mtimes resolve to the newest stamp, not to ls -1t's name tie-break" \
  || fail "a tie resolved to [$(cat "$SB_TIE/compose/.env")], wanted GEN2"

# A hand-made backup has no stamp and therefore no position in the ordering,
# so it is not a candidate. Under a plain name sort it would sort after every
# digit and win forever; it carries the newest mtime here so it would also
# have won under the old selector.
SB_MAN="$FIX/sb_manual"; seed_sandbox "$SB_MAN" CURRENT
for f in $ROLL_FILES; do
  printf 'MANUAL\n' >"$SB_MAN/$f.bak.manual"
  touch -d '2026-09-09 09:09:09' "$SB_MAN/$f.bak.manual"
done
run_rollback "$SB_MAN"
[ "$(cat "$SB_MAN/compose/.env")" = "GEN2" ] \
  && pass "a hand-made .bak.manual is not a rollback candidate (no stamp, no position in the ordering)" \
  || fail "restored [$(cat "$SB_MAN/compose/.env")] instead of GEN2; an unstamped file was treated as a backup"

# ------------------------------------- ONE GENERATION, NEVER A MIX (D3)
#
# The selection used to be per file: each member independently took its own
# newest backup. Give ONE member an extra stamp the others do not have -- which
# is what a deploy that died mid-backup, a hand-taken backup, or a member added
# by a later version of this script all leave behind -- and the per-file
# selector stitches that member's newer content onto everyone else's older
# content. The result is a .env from one moment against a compose file from
# another: exactly the mismatched pair the whole script exists to prevent,
# assembled by the recovery path itself.
#
# This case FAILS against the pre-2026-08-13 script, which restores GEN3 for
# .env and GEN2 for the rest.
SB_MIX="$FIX/sb_mixed"; seed_sandbox "$SB_MIX" CURRENT
printf 'GEN3\n' >"$SB_MIX/compose/.env.bak.20260813020202"
run_rollback "$SB_MIX"
[ "$ROLL_RC" = 0 ] \
  && pass "an extra stamp on ONE member does not make --rollback refuse; it falls back to the newest COMPLETE generation" \
  || fail "--rollback exited $ROLL_RC when one member had an extra stamp: $ROLL_OUT"
mix_bad=""
for f in $ROLL_FILES; do
  [ "$(head -1 "$SB_MIX/$f")" = "GEN2" ] || mix_bad="$mix_bad $f=[$(head -1 "$SB_MIX/$f")]"
done
[ -z "$mix_bad" ] \
  && pass "every member is restored from the SAME stamp; the lone newer .env backup is not stitched onto the older compose file" \
  || fail "the restore mixed generations:$mix_bad (wanted GEN2 everywhere). Selecting the newest backup PER FILE pairs a .env from one moment with a compose file from another"

# CONTROL for the case above. Without it, "GEN2 everywhere" could mean the
# script simply cannot see 20260813020202 at all. Complete that generation and
# it must be the one chosen.
SB_MIX2="$FIX/sb_mixed_complete"; seed_sandbox "$SB_MIX2" CURRENT
for f in $ROLL_FILES; do printf 'GEN3\n' >"$SB_MIX2/$f.bak.20260813020202"; done
cp -a "$SB_MIX2/secrets.tar.bak.20260812010101" "$SB_MIX2/secrets.tar.bak.20260813020202"
run_rollback "$SB_MIX2"
[ "$(head -1 "$SB_MIX2/compose/.env")" = "GEN3" ] \
  && pass "CONTROL: when 20260813020202 is COMPLETE it is the generation chosen, so the case above is about completeness and not about visibility" \
  || fail "CONTROL: a complete newer generation was not chosen; restored [$(head -1 "$SB_MIX2/compose/.env")]"

# ---------------------------- restoring into a state that does not render
#
# The state a rollback returns to is only as good as the state before the
# deploy, and that state can itself be broken: migrate renamed names in .env by
# hand, deploy, and the snapshot faithfully records a .env and a compose file
# that do not agree. Restarting onto that turns one outage into two. The files
# must still be restored -- that IS the requested state -- but the restart is
# withheld and the exit code says which of the two things happened.
SB_NR="$FIX/sb_norender"; seed_sandbox "$SB_NR" CURRENT
CAP_N=$((CAP_N+1)); CAP="$FIX/capture.$CAP_N"; : >"$CAP"
NR_OUT="$(env PATH="$STUB:$PATH" ZS_GATE_CAPTURE="$CAP" ZS_GATE_DOCKER_FAIL='compose config' \
  "$REMOTE" --host fakehost --remote-dir "$SB_NR" --rollback 2>&1)"; NR_RC=$?
[ "$NR_RC" = 4 ] \
  && pass "a restored configuration that does not render exits 4 (distinct from 3, which is the refusal to restore at all)" \
  || fail "--rollback exited $NR_RC when the restored configuration did not render; 4 is 'restored but not restarted': $NR_OUT"
[ "$(head -1 "$SB_NR/compose/.env")" = "GEN2" ] \
  && pass "the files stay restored even though the stack was not restarted (the operator asked for that state)" \
  || fail "the restore was rolled back as well; the host is now on neither state"
seen "$CAP" '### docker compose up -d' \
  && fail "--rollback restarted the stack onto a configuration it had just proved does not render" \
  || pass "--rollback does not restart the stack when the restored configuration does not render"

# ------------------------------------------------------------ --rollback-to
#
# The recovery this replaces was "restore from backups I happened to take by
# hand". An operator who lands on an older-but-good generation needs to be able
# to name it.
SB_TO="$FIX/sb_rollback_to"; seed_sandbox "$SB_TO" CURRENT
run_rollback "$SB_TO" >/dev/null 2>&1
CAP_N=$((CAP_N+1)); CAP="$FIX/capture.$CAP_N"; : >"$CAP"
SB_TO2="$FIX/sb_rollback_to2"; seed_sandbox "$SB_TO2" CURRENT
TO_OUT="$(env PATH="$STUB:$PATH" ZS_GATE_CAPTURE="$CAP" \
  "$REMOTE" --host fakehost --remote-dir "$SB_TO2" --rollback-to 20260101000000 2>&1)"; TO_RC=$?
[ "$TO_RC" = 0 ] && [ "$(head -1 "$SB_TO2/compose/.env")" = "GEN1" ] \
  && pass "--rollback-to restores the NAMED generation, not the newest one" \
  || fail "--rollback-to 20260101000000 exited $TO_RC and left [$(head -1 "$SB_TO2/compose/.env")], wanted GEN1: $TO_OUT"

SB_TO3="$FIX/sb_rollback_to3"; seed_sandbox "$SB_TO3" CURRENT
rm -f "$SB_TO3"/ops/zeroship.toml.bak.20260101000000
CAP_N=$((CAP_N+1)); CAP="$FIX/capture.$CAP_N"; : >"$CAP"
TO_OUT="$(env PATH="$STUB:$PATH" ZS_GATE_CAPTURE="$CAP" \
  "$REMOTE" --host fakehost --remote-dir "$SB_TO3" --rollback-to 20260101000000 2>&1)"; TO_RC=$?
[ "$TO_RC" = 3 ] \
  && pass "--rollback-to refuses an INCOMPLETE named generation rather than silently using a different one" \
  || fail "--rollback-to an incomplete generation exited $TO_RC, wanted 3: $TO_OUT"
[ "$(head -1 "$SB_TO3/compose/.env")" = "CURRENT" ] \
  && pass "nothing is restored when --rollback-to refuses" \
  || fail "--rollback-to refused and restored anyway; the host is on a mixed configuration"

# ----------------------------------------------------------- --list-backups
SB_LS="$FIX/sb_list"; seed_sandbox "$SB_LS" CURRENT
rm -f "$SB_LS"/ops/Caddyfile.bak.20260101000000
CAP_N=$((CAP_N+1)); CAP="$FIX/capture.$CAP_N"; : >"$CAP"
LS_OUT="$(env PATH="$STUB:$PATH" ZS_GATE_CAPTURE="$CAP" \
  "$REMOTE" --host fakehost --remote-dir "$SB_LS" --list-backups 2>&1)"; LS_RC=$?
[ "$LS_RC" = 0 ] && [[ "$LS_OUT" == *"20260812010101  complete"* ]] \
  && pass "--list-backups reports a complete generation as complete" \
  || fail "--list-backups (rc=$LS_RC) did not report 20260812010101 complete: $LS_OUT"
[[ "$LS_OUT" == *"20260101000000  INCOMPLETE"* && "$LS_OUT" == *"ops/Caddyfile"* ]] \
  && pass "--list-backups names the members an INCOMPLETE generation is missing" \
  || fail "--list-backups did not flag 20260101000000 incomplete and name ops/Caddyfile: $LS_OUT"
seen "$CAP" '### docker compose up -d' \
  && fail "--list-backups restarted the stack; it is a read-only question" \
  || pass "--list-backups restarts nothing"

# ============================================================================
# The DEPLOY path, driven through the same stub ssh
# ============================================================================
#
# WHY THIS SECTION EXISTS. On 2026-08-13 a roll of a new image took the site
# down three times over, and every one of the three defects was on this path,
# which had no coverage at all: the ops overlay was never synced, the secret
# FILES were never provisioned, and nothing asked the servers whether they
# accepted the configuration before restarting them.
#
# WHAT A STUB PROVES HERE, and it is narrower than it looks. `docker` and `scp`
# record and succeed, so what is under test is the SCRIPT'S CONTROL FLOW: which
# commands it decides to send, in what order, and where it stops. It is real
# evidence about the script.
#
# WHAT IT DOES NOT PROVE: that a real server rejects a stale overlay, that a
# real `zeroship dev init` creates the right files, that the image exists, or
# that the stack comes back. The first of those is measured separately against
# the compiled binaries in the conditional block below; the rest need a host.
echo ""
# ------------------------------------ the source-tree refusal, end to end
#
# Sits here, far from the submodule_paths/submodule_manifests fixtures it
# belongs with, because it needs the stub ssh built above. Those test the
# PARSERS; this runs the real script against a throwaway git repo standing in
# for the source tree, which works because main() takes its root from `git
# rev-parse --show-toplevel`. Nothing here touches this repo's own
# third_party/, which is the vendored engine and is not ours to move.
echo "-- an unpopulated submodule stops the deploy before the build"

SRC="$FIX/srctree"
mkdir -p "$SRC/third_party/zero-migrate" "$SRC/deploy/compose"
( cd "$SRC"
  git init -q .
  git config user.email gate@example.invalid
  git config user.name gate
  printf '[submodule "third_party/zero-migrate"]\n\tpath = third_party/zero-migrate\n\turl = https://example.invalid/x.git\n' >.gitmodules
  printf 'importers:\n\n  third_party/zero-migrate/packages/zero-migrate:\n    dependencies: {}\n' >pnpm-lock.yaml
  printf '[workspace]\nmembers = [\n    "third_party/zero-migrate",\n]\n' >Cargo.toml
  git add -A >/dev/null
  git commit -qm "gate fixture" >/dev/null )

SRC_SANDBOX="$FIX/srcsandbox"; mkdir -p "$SRC_SANDBOX"
run_src() { # rest: extra argv to deploy-remote.sh, run with $SRC as the repo root
  ( cd "$SRC" && env PATH="$STUB:$PATH" ZS_GATE_CAPTURE=/dev/null \
      "$REMOTE" --host fakehost --registry ghcr.io/gate/img \
      --remote-dir "$SRC_SANDBOX" --dry-run "$@" 2>&1 )
}

# `git init` leaves the directory empty, exactly as `git worktree add` does.
SRC_OUT="$(run_src)"; SRC_RC=$?
[ "$SRC_RC" != 0 ] && [[ "$SRC_OUT" == *"third_party/zero-migrate"* ]] \
  && pass "an empty submodule directory refuses the deploy (exit $SRC_RC) and names the path" \
  || fail "an empty submodule directory did not stop the deploy (exit $SRC_RC): $SRC_OUT"
[[ "$SRC_OUT" == *"submodule update --init"* ]] \
  && pass "the refusal names the command that fixes it" \
  || fail "the refusal does not say how to fix it: $SRC_OUT"
[[ "$SRC_OUT" != *"would build and push"* ]] \
  && pass "it stops BEFORE the build, which is the twenty minutes this exists to save" \
  || fail "the dry run reported it would build from a tree that cannot build"

# PARTIALLY populated: the directory is not empty, but a manifest the build
# opens is absent. The empty-directory arm above cannot see this.
mkdir -p "$SRC/third_party/zero-migrate/packages/zero-migrate"
printf '[package]\nname = "zero-migrate"\n' >"$SRC/third_party/zero-migrate/Cargo.toml"
SRC_OUT="$(run_src)"; SRC_RC=$?
[ "$SRC_RC" != 0 ] && [[ "$SRC_OUT" == *"packages/zero-migrate/package.json"* ]] \
  && pass "a PARTIAL copy is refused by the name of the manifest it is missing (exit $SRC_RC)" \
  || fail "a partial submodule copy was accepted (exit $SRC_RC): $SRC_OUT"

# CONTROL, and the gate is worthless without it: complete the tree and the same
# invocation must get past this check. A guard that always refuses is removed
# as fast as one that never does.
printf '{ "name": "zero-migrate" }\n' >"$SRC/third_party/zero-migrate/packages/zero-migrate/package.json"
SRC_OUT="$(run_src)"
[[ "$SRC_OUT" == *"every declared submodule is populated"* ]] \
  && pass "CONTROL: with the manifests present the same command passes the source-tree check" \
  || fail "CONTROL: a complete source tree was still refused: $SRC_OUT"

# --skip-build and --image deploy a reference that already exists, so the source
# tree is not an input and must not gate them. Re-emptied to prove the bypass is
# real and not just a tree that happens to pass.
rm -rf "$SRC/third_party/zero-migrate"; mkdir -p "$SRC/third_party/zero-migrate"
for bypass in --skip-build "--image ghcr.io/gate/img:pinned"; do
  # shellcheck disable=SC2086
  SRC_OUT="$(run_src $bypass)"
  [[ "$SRC_OUT" != *"submodule directories are EMPTY"* ]] \
    && pass "$bypass does not run the source-tree check (no image is built from this tree)" \
    || fail "$bypass was blocked by a source-tree check that does not apply to it"
done


echo "-- the deploy path (stub ssh, stub docker; nothing is built or pushed)"

FAKE_IMAGE="ghcr.io/example/zeroship-platform:testonly"

seed_deploy() { # $1 sandbox. A host that would pass every contract.
  local d="$1"
  mkdir -p "$d/compose" "$d/ops" "$d/secrets"
  compose_vars '' "$REAL_COMPOSE" | sed 's/$/=x/' >"$d/compose/.env"
  printf 'ZEROSHIP_SECRETS_DIR=%s\n' "$d/secrets" >>"$d/compose/.env"
  printf 'ZEROSHIP_IMAGE=%s\n' "ghcr.io/example/zeroship-platform:previous" >>"$d/compose/.env"
  printf 'HOSTCOMPOSE\n' >"$d/compose/docker-compose.yml"
  printf 'HOSTCADDY\n'   >"$d/ops/Caddyfile"
  printf 'HOSTOVERLAY\n' >"$d/ops/zeroship.toml"
  seed_secrets "$d"
}

# The status the stub `migrate` service reports.
#
# A REAL CAPTURE, not a hand-written document: `deploy/testdata/platform_status.json`
# is `zero-migrate status --json --env platform` run against a PostgreSQL the
# corpus had just been applied to, with each plan's `steps` array truncated to
# two entries so the file is 25 KB instead of 144 KB. Nothing else is edited.
#
# THE FIDELITY THAT MATTERS IS THE NESTING, so the steps are truncated rather than
# removed: a plan header is `version,name,state` and a STEP is
# `version,name,kind,state`, and each plan also carries `"steps":[...],
# "missingDependencies":[],"touchedTables":[...]`. A fixture without those would
# pass an extractor that cannot tell a plan from a step, or one that slices the
# plans array at the first `],"<key>":` -- which is exactly the bug the first
# draft of `platform_status_plans` had (36 plans in, 1 out).
#
# RE-CAPTURE IT if the CLI's status shape changes. There is no live database in
# this gate, so nothing here can notice on its own.
GATE_STATUS_FIXTURE="$ROOT/deploy/testdata/platform_status.json"
[ -r "$GATE_STATUS_FIXTURE" ] || {
  echo "missing $GATE_STATUS_FIXTURE; the migration-freeze arms cannot run" >&2
  exit 1
}
GATE_STATUS="$FIX/status.json"
cp "$GATE_STATUS_FIXTURE" "$GATE_STATUS"

run_deploy() { # $1 sandbox, rest: extra argv. Sets DEP_RC, DEP_OUT and CAP.
  CAP_N=$((CAP_N+1))
  CAP="$FIX/capture.$CAP_N"
  : >"$CAP"
  local sb="$1"; shift
  DEP_OUT="$(env PATH="$STUB:$PATH" ZS_GATE_CAPTURE="$CAP" ZS_GATE_RUNNING_IMAGE="$FAKE_IMAGE" \
    ZS_GATE_STATUS="$GATE_STATUS" "$@" \
    "$REMOTE" --host fakehost --remote-dir "$sb" \
      --registry ghcr.io/example/zeroship-platform --image "$FAKE_IMAGE" 2>&1)"
  DEP_RC=$?
}

# --- the happy path, which is the CONTROL for every refusal below ----------
#
# Without it, "the deploy stopped before restarting" could mean the deploy stops
# before restarting for reasons that have nothing to do with the check under
# test. This run must reach the roll.
SB_DEP="$FIX/sb_deploy"; seed_deploy "$SB_DEP"
# Snapshot the pre-deploy state so the end-to-end restore below has something
# to compare against that was captured before the script ran, not after.
DEP_BEFORE="$FIX/before"; rm -rf "$DEP_BEFORE"; cp -a "$SB_DEP" "$DEP_BEFORE"
run_deploy "$SB_DEP"

[ "$DEP_RC" = 0 ] \
  && pass "CONTROL: a host that passes every contract deploys to completion (exit 0)" \
  || fail "CONTROL: the happy path exited $DEP_RC, so every refusal below could be an unrelated stop: $DEP_OUT"
seen "$CAP" '### scp' \
  && pass "CONTROL: a host that passes every contract reaches the config sync" \
  || fail "CONTROL: the deploy never reached scp (rc=$DEP_RC): $DEP_OUT"
seen "$CAP" '### docker compose up -d --remove-orphans' \
  && pass "CONTROL: the same run reaches 'docker compose up -d --remove-orphans', so the refusals below are about the checks and not about an unrelated stop" \
  || fail "CONTROL: the deploy never reached the roll (rc=$DEP_RC): $DEP_OUT"

# --- D1: the ops overlay is SYNCED ----------------------------------------
#
# It was not, and the host kept an overlay written for older binaries. The new
# ones parse it with deny_unknown_fields, so every service exited at parse time.
# This assertion FAILS against the pre-fix script, which scp'd two files.
# Match the `### scp` INVOCATION, not the bare name. `ops/zeroship.toml` also
# appears in the snapshot body that this same run sends over ssh, so a scan for
# the name alone stays green with the sync deleted -- measured, by deleting the
# scp line and watching this assertion pass.
dep_synced=""
for f in deploy/compose/docker-compose.yml deploy/ops/Caddyfile deploy/ops/zeroship.toml; do
  grep -q "### scp .*$f " "$CAP" || dep_synced="$dep_synced $f"
done
[ -z "$dep_synced" ] \
  && pass "all three tracked config files are scp'd to the host, ops/zeroship.toml included (D1: a host copy written for an older binary made every service refuse to start)" \
  || fail "these tracked config files are never sent to the host:$dep_synced. A stale one survives the roll, and the overlay is parsed with deny_unknown_fields"
# CONTROL: a repo file the deploy does NOT sync must not match, or the loop
# above would report success against any capture at all.
grep -q "### scp .*deploy/ops/postgres-init.sql " "$CAP" \
  && fail "CONTROL: the matcher found a file the deploy does not sync; the loop above discriminates nothing" \
  || pass "CONTROL: deploy/ops/postgres-init.sql is NOT matched (it is host-layout config, shipped once by hand)"

# --- D1: --check-config runs, for every server, before the roll ------------
check_missing=""
for svc in $CHECK_SERVICES; do
  seen "$CAP" "--entrypoint ${svc#*:} ${svc%%:*} --check-config" || check_missing="$check_missing ${svc%%:*}"
done
[ -z "$check_missing" ] \
  && pass "--check-config is run for every server ($(for s in $CHECK_SERVICES; do printf '%s ' "${s%%:*}"; done))" \
  || fail "no --check-config run for:$check_missing. A server that rejects the new configuration is then discovered by restarting it"

# ORDER MATTERS MORE THAN PRESENCE. A check that runs after the roll is a
# post-mortem, not a gate.
CHK_LINE="$(grep -n -- '--check-config' "$CAP" | head -1 | cut -d: -f1)"
UP_LINE="$(grep -n -- '### docker compose up -d --remove-orphans' "$CAP" | head -1 | cut -d: -f1)"
if [ -n "$CHK_LINE" ] && [ -n "$UP_LINE" ] && [ "$CHK_LINE" -lt "$UP_LINE" ]; then
  pass "every --check-config run precedes 'docker compose up -d' in the command stream"
else
  fail "--check-config at line ${CHK_LINE:-none} does not precede the roll at line ${UP_LINE:-none}; the check is a post-mortem"
fi

# --- D4: an APPLIED migration that was edited refuses BEFORE the roll -------
#
# The defect: the migrate runner refuses every later run against a database whose
# journal recorded a different checksum for a migration it already applied,
# permanently. Editing an applied file bricks that database, and the only signal
# `docker compose up` gives is `service "migrate" didn't complete successfully:
# exit 2` with the container's own stderr never surfaced. It cost a full day on
# 2026-08-19.
#
# THE MUTATION IS IN THE STATUS, NOT IN THE TREE, and that is the change from the
# version of this arm that appended a line to a real `db/migrations-ts` file. The
# check no longer compares file bytes against a checked-in snapshot -- it asks the
# deployment's own runner, and the runner's word for "an applied migration's
# content moved" is `"state": "drifted"`. Editing a file in this repo would
# therefore produce no signal at all through a stub that cannot run the CLI, and
# the arm would go green over nothing.
#
# MEASURED, NOT ASSUMED: the fixture below is what `zero-migrate status --json`
# actually printed on 2026-08-28 after one applied migration's source was edited
# (`control_workflow_journal_access`) -- `"state":"drifted"`, with `blocked` and
# `unexpectedJournal` both empty. So the state string is the whole signal, and a
# check that only read `blocked` would have seen nothing.
#
# The CONTROL for this pair is the happy path above, which ran with the same
# fixture unmutated and DID reach the roll -- so a refusal here is about the
# drifted plan and not about the check existing at all.
# Every arm from D2 down reads the HAPPY PATH's capture. These arms each run
# their own deploy, which repoints $CAP, so put it back before leaving.
CAP_HAPPY="$CAP"
# THE *LAST* PLAN, and picking the first instead is how this arm proved nothing.
# A drifted plan is also not-applied, so if it sits anywhere but last the
# ORDERING check fires on it too and refuses the deploy on its own. Measured
# 2026-08-28: with the first plan mutated, deleting the drift refusal from
# deploy-remote.sh left this gate at 158 passed / 0 failed - the ordering arm was
# answering for both. Mutating the last plan leaves nothing applied after it, so
# ordering stays silent and only the drift check can refuse.
#
# Plan headers are at six spaces of indent and STEP names at ten, which is what
# separates them here; a bare `"name":` grep would return a step.
DRIFT_VICTIM="$(sed -n 's/^      "name": "\([a-z0-9_]*\)",$/\1/p' "$GATE_STATUS_FIXTURE" | tail -1)"
[ -n "$DRIFT_VICTIM" ] || fail "could not read a plan name out of $GATE_STATUS_FIXTURE; the drift arm would rule on nothing"
GATE_STATUS_DRIFT="$FIX/status.drifted.json"
awk -v victim="$DRIFT_VICTIM" '
  $0 ~ "\"name\": \""victim"\"," { seen = 1 }
  seen && /"state": "applied"/ { sub(/"applied"/, "\"drifted\""); seen = 0 }
  { print }' "$GATE_STATUS_FIXTURE" >"$GATE_STATUS_DRIFT"
grep -q '"drifted"' "$GATE_STATUS_DRIFT" \
  || fail "the drift mutation changed nothing in $GATE_STATUS_FIXTURE; this arm would rule on a clean status"
SB_LEDGER="$FIX/sb_ledger"; seed_deploy "$SB_LEDGER"
GATE_STATUS_SAVE="$GATE_STATUS"; GATE_STATUS="$GATE_STATUS_DRIFT"
run_deploy "$SB_LEDGER"
GATE_STATUS="$GATE_STATUS_SAVE"

[ "$DEP_RC" != 0 ] \
  && pass "a migration the host applied whose content drifted makes the deploy exit non-zero" \
  || fail "a drifted applied migration deployed cleanly (rc=$DEP_RC); the roll will stop at 'migrate' with a bare exit 2"
case "$DEP_OUT" in
  *"$DRIFT_VICTIM"*) pass "the refusal names the drifted migration ($DRIFT_VICTIM)" ;;
  *) fail "the refusal never names the migration, which is the whole diagnosis compose does not give: $DEP_OUT" ;;
esac
seen "$CAP" '### docker compose up -d --remove-orphans' \
  && fail "the stack was ROLLED anyway; the check has to precede the roll or 'migrate' has already refused" \
  || pass "NOTHING IS RESTARTED when an applied migration drifted"

# --- D4: a migration that sorts before an applied one refuses the roll ------
#
# Disjoint from the arm above and invisible to it: the file at fault is NEW, so
# no journal row covers it and its own content is fine. On a host that already
# applied everything sorting after it the file lands LAST, while on a fresh
# database it lands in position, so the two schemas agree only if it commutes
# with everything it jumped. The retired runner derived versions from the file
# ordinal and aborted on the collision; the CLI derives them from the migration
# name, so the insert now applies with NO error at all. Measured on 2026-08-20
# with 20260819000000_app_egress_rules.ts against a journal ending at
# 20260820000000_control_workflow_journal_access.ts.
#
# The planted plan is PREPENDED to the plans array in the CLI's own field order,
# which is what a file with an early date produces: measured 2026-08-28, a
# `20260101000000_*.ts` planted into an otherwise fully applied corpus came back
# as `plans[0]` with `"state": "pending"`.
GATE_STATUS_ORDER="$FIX/status.misordered.json"
awk '
  /^  "plans": \[$/ && !done {
    print
    print "    {"
    print "      \"version\": \"mig_gateMisorderedProbe0001\","
    print "      \"name\": \"gate_misordered\","
    print "      \"state\": \"pending\","
    print "      \"steps\": ["
    print "        {"
    print "          \"version\": \"mig_gateMisorderedProbeStep\","
    print "          \"name\": \"create_table_gate_misordered\","
    print "          \"kind\": \"ddl\","
    print "          \"state\": \"pending\""
    print "        }"
    print "      ],"
    print "      \"missingDependencies\": [],"
    print "      \"touchedTables\": ["
    print "        \"gate_misordered\""
    print "      ]"
    print "    },"
    done = 1
    next
  }
  { print }' "$GATE_STATUS_FIXTURE" >"$GATE_STATUS_ORDER"
grep -q 'gate_misordered' "$GATE_STATUS_ORDER" \
  || fail "the misorder mutation planted nothing in $GATE_STATUS_FIXTURE; this arm would rule on a clean status"
SB_ORDER="$FIX/sb_order"; seed_deploy "$SB_ORDER"
GATE_STATUS_SAVE="$GATE_STATUS"; GATE_STATUS="$GATE_STATUS_ORDER"
run_deploy "$SB_ORDER"
GATE_STATUS="$GATE_STATUS_SAVE"

[ "$DEP_RC" != 0 ] \
  && pass "a migration sorting before one the host has applied makes the deploy exit non-zero" \
  || fail "a mid-corpus migration deployed cleanly (rc=$DEP_RC); on this host it would apply after every file that sorts later than it"
case "$DEP_OUT" in
  *gate_misordered*) pass "the refusal names the misordered migration, which the engine itself never does" ;;
  *) fail "the refusal does not name the misordered migration: $DEP_OUT" ;;
esac
seen "$CAP" '### docker compose up -d --remove-orphans' \
  && fail "the stack was ROLLED anyway; this has to precede the roll or 'migrate' has already applied it out of order" \
  || pass "NOTHING IS RESTARTED when a migration sorts before an applied one"

# CONTROL: the happy path above ran the same check against the same fixture with
# no mutation and DID reach the roll, so these arms are about the mutation and
# not about the check refusing everything.

# --- the edge claim refusal, end to end ------------------------------------
#
# The two arms above rule on the EXTRACTION. This runs the real script and
# checks it STOPS -- the distinction that matters, because a check can read the
# right set and then not act on it.
#
# The mutation is the committed fixture PAIR swapped over the shipped edge:
# edge_matcher_claim.{Caddyfile,json} is the live edge plus one `@status host`
# matcher, generated together by deploy/ops/caddy-claimed-hosts.sh, so the
# artifact's caddyfile_sha256 still describes the Caddyfile beside it. That is
# what makes this the FAILED arm and not the REFUSED one: the check can rule,
# and what it rules is that `status` is not covered.
#
# The CONTROL is the happy path above, which ran with the real edge and reached
# the roll. Run under --image, so it also establishes the check is NOT skipped
# on the paths that build no image but still scp the Caddyfile.
if [ -f "$FIX_MATCHER" ] && [ -f "$ROOT/crates/zeroship-control/testdata/edge_matcher_claim.Caddyfile" ]; then
  EDGE_CADDY="$ROOT/deploy/ops/Caddyfile"
  cp "$EDGE_CADDY" "$FIX/edge.Caddyfile.orig"
  cp "$EDGE_ART"   "$FIX/edge.json.orig"
  cp "$ROOT/crates/zeroship-control/testdata/edge_matcher_claim.Caddyfile" "$EDGE_CADDY"
  cp "$FIX_MATCHER" "$EDGE_ART"
  SB_EDGE="$FIX/sb_edge"; seed_deploy "$SB_EDGE"
  run_deploy "$SB_EDGE"
  cp "$FIX/edge.Caddyfile.orig" "$EDGE_CADDY"
  cp "$FIX/edge.json.orig"      "$EDGE_ART"

  [ "$DEP_RC" != 0 ] \
    && pass "an edge claiming a host RESERVED_APP_NAMES does not cover makes the deploy exit non-zero" \
    || fail "an edge claiming an unreserved host deployed cleanly (rc=$DEP_RC); the roll would scp a Caddyfile that routes a registrable name away from creator apps"
  case "$DEP_OUT" in
    *"RESERVED_APP_NAMES does not cover"*status*) pass "the refusal names the host (status) and the list that does not cover it" ;;
    *) fail "the refusal does not name the uncovered host: $DEP_OUT" ;;
  esac
  case "$DEP_OUT" in
    *deploy/ops/Caddyfile*) pass "the refusal names the file the roll would ship (deploy/ops/Caddyfile)" ;;
    *) fail "the refusal never names deploy/ops/Caddyfile, so an operator cannot tell what to edit: $DEP_OUT" ;;
  esac
  case "$DEP_OUT" in
    FAIL:*|*$'\n'FAIL:*) pass "it is reported as FAILED, not REFUSED: the check ruled and said no" ;;
    *) fail "an uncovered claim was not reported as a FAIL: $DEP_OUT" ;;
  esac
  seen "$CAP" '### scp' \
    && fail "the Caddyfile was SHIPPED anyway; the check has to precede the scp or the edge is already in production" \
    || pass "NOTHING IS SHIPPED when the edge claims an uncovered host"

  # THE OTHER HALF: the artifact does not describe the Caddyfile. Nothing then
  # knows what the edge claims, so the check cannot RULE -- and an unchecked
  # edge must not print what a checked one prints. One variable from the arm
  # above: the same fixture Caddyfile, with the SHIPPED artifact left in place.
  cp "$ROOT/crates/zeroship-control/testdata/edge_matcher_claim.Caddyfile" "$EDGE_CADDY"
  SB_STALE="$FIX/sb_edge_stale"; seed_deploy "$SB_STALE"
  run_deploy "$SB_STALE"
  cp "$FIX/edge.Caddyfile.orig" "$EDGE_CADDY"

  [ "$DEP_RC" != 0 ] \
    && pass "a Caddyfile the committed artifact does not describe stops the deploy" \
    || fail "an edge config nothing in the tree describes was deployed (rc=$DEP_RC)"
  case "$DEP_OUT" in
    REFUSED:*|*$'\n'REFUSED:*) pass "it is reported as REFUSED, not FAILED: the check could not rule at all" ;;
    *) fail "a stale artifact was not reported as a REFUSED: $DEP_OUT" ;;
  esac
  case "$DEP_OUT" in
    *"caddy-claimed-hosts.sh --write"*) pass "the refusal names the command that regenerates the artifact" ;;
    *) fail "the refusal does not say how to fix it: $DEP_OUT" ;;
  esac

  # CONTROL: with both files back as they ship, the same invocation gets past
  # this check. Without it, the two refusals above could be a check that refuses
  # every tree.
  SB_EDGE_OK="$FIX/sb_edge_ok"; seed_deploy "$SB_EDGE_OK"
  run_deploy "$SB_EDGE_OK"
  [ "$DEP_RC" = 0 ] && [[ "$DEP_OUT" == *"every host the edge claims is reserved"* ]] \
    && pass "CONTROL: the edge this repo ships passes the same check and reaches the roll (exit $DEP_RC)" \
    || fail "CONTROL: the shipped edge did not pass the check and reach the roll (rc=$DEP_RC; either it was refused, or the check never ran), so the two refusals above prove nothing: $DEP_OUT"
  CAP="$CAP_HAPPY"
else
  # AN ABSENT FIXTURE IS A FAILURE, NOT A SKIP. This block holds the ONE arm
  # that proves the roll refuses an edge claiming an unreserved host - the
  # failure the whole reserved-name apparatus exists for. Its fixtures moved
  # with the crate directory, so it silently stopped running and took that
  # proof with it.
  fail "the uncovered-host arm did not run: the edge fixtures are missing ($FIX_MATCHER, ${FIX_MATCHER%.json}.Caddyfile). Regenerate with deploy/ops/caddy-claimed-hosts.sh --write. Nothing here then proves the roll refuses an edge that claims a registrable name."
fi

# --- D4: a journal probe that answers neither t nor f is refused ------------
#
# A down database, a wrong role and an empty deployment all produce no output
# here. Folding them together would report "nothing is frozen" for the first
# two -- a failure and a legitimate empty result printing identically, which is
# the exact silence this whole check exists to remove.
SB_PROBE="$FIX/sb_probe"; seed_deploy "$SB_PROBE"
run_deploy "$SB_PROBE" ZS_GATE_JOURNAL_EXISTS=''
[ "$DEP_RC" != 0 ] \
  && pass "a migration-journal probe that answers neither t nor f refuses the deploy" \
  || fail "an unreadable migration journal was treated as 'nothing is frozen' (rc=$DEP_RC)"

# CONTROL for the pair above: `f` is a real answer and means a host with no
# journal yet, which must deploy. Without this the two arms above would both
# pass for a check that refuses unconditionally.
SB_FRESH="$FIX/sb_fresh"; seed_deploy "$SB_FRESH"
run_deploy "$SB_FRESH" ZS_GATE_JOURNAL_EXISTS='f'
[ "$DEP_RC" = 0 ] \
  && pass "CONTROL: a host with NO migration journal yet still deploys (nothing is frozen there)" \
  || fail "CONTROL: a fresh host with no journal was refused (rc=$DEP_RC), so the two refusals above prove nothing: $DEP_OUT"

# --- D4: the repo keeps NO snapshot of what the deployment has applied ------
#
# THERE IS NOTHING LEFT TO GO STALE, and this arm is what says so rather than a
# comment claiming it. `db/released_migrations.tsv` was a checked-in copy of one
# deployment's journal; it covered 21 files while that journal held 34 and
# reported the identical green either way, and the deploy carried a post-roll
# step that rewrote it and exited 1 asking for a commit. Both are gone: the
# pre-roll check asks the deployment's own runner instead, so no copy exists to
# drift.
#
# The property under test is therefore an ABSENCE, and an absence is exactly the
# kind of claim that rots into prose. Two spellings are checked because a rename
# would defeat either one alone, and the deploy script is checked for the
# post-roll writer as well as the tree for the file: a script that still writes
# it would recreate the drift the moment someone committed the output.
[ -e "$ROOT/db/released_migrations.tsv" ] \
  && fail "db/released_migrations.tsv is back. It is a snapshot of ONE deployment's journal, it went 13 files stale while reporting green, and nothing writes it now - so it can only mislead." \
  || pass "the repo keeps no checked-in snapshot of any deployment's migration journal"
# NON-COMMENT LINES ONLY. The prose above the pre-roll check names the deleted
# snapshot on purpose - that is the record of why it is gone - and a grep over
# the whole file would refuse the very explanation it is protecting.
#
# `grep -c`, NOT `| grep -q`, and this file is the one place in this gate where
# the difference can bite: every other piped `grep -q` here filters a shell
# variable a few hundred bytes long, while this one filters a SHIPPED SCRIPT
# that grows.
#
# THE HAZARD. `grep -q` exits on its FIRST match. If the upstream `grep -v` is
# still writing at that moment it takes SIGPIPE (141), `set -o pipefail`
# promotes 141 to the pipeline's status, the `&&` arm is skipped and `pass`
# runs - EXACTLY WHEN THE VIOLATION IS PRESENT. Whether the upstream is still
# writing depends only on input size, so the check is fail-open above some
# threshold and correct below it.
#
# MEASURED 2026-09-04, same pipeline, the match planted on line 2, only the
# non-comment body size varying:
#     8 KB -> FAIL    61 KB -> FAIL    91 KB -> FAIL    112 KB -> FAIL
#    31 KB -> FAIL    71 KB -> FAIL   101 KB -> pass    119 KB -> pass
#                                     131 KB -> pass    198 KB -> pass
# It is not a clean threshold, it is a RACE: 101 KB inverted while 112 KB did
# not, in the same run. deploy-remote.sh's non-comment body is 31 KB today, so
# this was latent and roughly 3x from live. `grep -c` reads to EOF and cannot
# race; the same corpus reports FAIL at every size.
#
# IT IS A FUNCTION so the controls below can drive the SAME code this line does.
# A probe that only ever runs against one shipped file is a probe whose
# behaviour on any other input is a claim in a comment.
journal_snapshot_hits() {   # <script> -> non-comment lines naming the snapshot
  grep -v '^[[:space:]]*#' "$1" | grep -c 'released_migrations\|released_ledger'
}
ledger_hits="$(journal_snapshot_hits "$REMOTE")"
[ "${ledger_hits:-0}" -gt 0 ] \
  && fail "deploy-remote.sh still reads or writes the released-migrations snapshot ($ledger_hits non-comment line(s)); the pre-roll check is supposed to be the only thing that consults a deployment's journal" \
  || pass "the deploy script keeps no copy of the journal and writes none back"

# --- the probe's own controls, and the one that BINDS the grep -c -----------
#
# UNTIL 2026-09-04 THE FIX ABOVE WAS CODE PLUS A COMMENT. Reverting `grep -c` to
# the `| grep -q` pipeline turned nothing in this tree red: the shipped script
# is 31 KB of non-comment body, three times under the size where the race
# begins, so the live assertion passes either way. A correct check nobody ever
# watched fail is the shape this repository keeps finding, and these four cases
# are what end it here.
#
# Case 4 is the regression. It is case 3 with ONE variable changed - the size of
# the body the match is planted in - which is the whole of the defect.
#
# RE-MEASURED 2026-09-04 on this exact pipeline, match on line 2, 30 trials per
# size: at 67 KB the `| grep -q` form found the match 30/30; at 134 KB, 270 KB,
# 1.1 MB, 4.4 MB and 17.8 MB it INVERTED 30/30, reporting "no match" under
# pipefail exactly when the match was there. `grep -c` reported 1 at every size.
# The corpus below is ~1.2 MB deliberately: 18x the 65,536-byte pipe capacity
# (`getconf PAGESIZE` x 16) and 9x the smallest size measured to invert every
# time, so this case does not depend on winning a race.
JP="$FIX/journal_probe"
mkdir -p "$JP"
printf 'echo nothing here names the deleted snapshot\n' >"$JP/clean"
printf '# a comment naming released_migrations, as this gate does above\necho ordinary line\n' >"$JP/commented"
printf '# a comment naming released_migrations, as this gate does above\nledger_sync released_migrations.tsv\n' >"$JP/small"
{
  printf '# a comment naming released_migrations, as this gate does above\n'
  printf 'ledger_sync released_migrations.tsv\n'
  awk 'BEGIN { for (i = 0; i < 20000; i++) print "echo padding line " i " of an ordinary deploy script body" }'
} >"$JP/large"

JP_CASES=0
jp_case() {   # <label> <expected hits> <file>
  local label="$1" want="$2" path="$3" got
  JP_CASES=$((JP_CASES + 1))
  got="$(journal_snapshot_hits "$path")"
  [ "${got:-x}" = "$want" ] \
    && pass "journal probe: $label (hits=$got)" \
    || fail "journal probe: $label - expected hits=$want, got hits=$got"
}
jp_case "a body naming the snapshot nowhere reports none"        0 "$JP/clean"
jp_case "a COMMENT naming the snapshot does not count"           0 "$JP/commented"
jp_case "a live line naming the snapshot counts"                 1 "$JP/small"
jp_case "the SAME live line counts in a 1.2 MB body ($(wc -c <"$JP/large") bytes)" \
                                                                 1 "$JP/large"

# FLOOR 4 AND EXACTLY 4 CASES, which is deliberate here and would be wrong for
# an arm that enumerates the tree. These four are literals twenty lines up, not
# a population that shrinks on its own: deleting one is a decision, and the
# floor is what makes that decision arrive as a refusal rather than as a quietly
# smaller control set. Add a case, raise the floor in the same commit.
if ! gate_arm journal_probe_controls "$JP_CASES" 4; then
  fail "the journal-probe controls ruled on $JP_CASES case(s), under their floor
       of 4. The cases stopped running, so the assertion above is back to being
       a claim in a comment."
fi

CAP="$CAP_HAPPY"

# --- D2: provisioning uses the IMAGE'S OWN dev init ------------------------
seen "$CAP" "--entrypoint zeroship $FAKE_IMAGE dev init" \
  && pass "secrets are provisioned by the deployed image's own 'zeroship dev init', which owns BOTH the env list and the file list" \
  || fail "provisioning does not run the image's zeroship dev init; it is keeping a second list that cannot cover the secret FILES"
seen "$CAP" '### openssl' \
  && fail "provisioning still generates values with 'openssl rand'; that path only ever produced env-shaped secrets and no key files" \
  || pass "provisioning no longer hand-rolls values with openssl"

# --- D3: the snapshot covers every member, under ONE stamp -----------------
snap_missing=""
for m in $SNAPSHOT_MEMBERS; do
  ls "$SB_DEP/$m".bak.[0-9]* >/dev/null 2>&1 || snap_missing="$snap_missing $m"
done
[ -z "$snap_missing" ] \
  && pass "the deploy snapshots every member --rollback needs ($SNAPSHOT_MEMBERS)" \
  || fail "the deploy never backs up:$snap_missing, so --rollback can never restore them"
SNAP_STAMPS="$(for m in $SNAPSHOT_MEMBERS; do ls -1d "$SB_DEP/$m".bak.[0-9]* 2>/dev/null; done | sed 's|.*\.bak\.||' | sort -u | wc -l)"
[ "$SNAP_STAMPS" = 1 ] \
  && pass "every member is snapshotted under ONE stamp, which is what lets --rollback name a generation" \
  || fail "the members carry $SNAP_STAMPS different stamps; there is no single generation to restore"

# END TO END: deploy, then roll back, and the host is byte-identical to what it
# was before the deploy. Reading the restore code is not the same as running it.
#
# The stub cannot mutate the host the way a real deploy does (scp and dev init
# record and succeed), so the mutations a real deploy WOULD make are applied
# here by hand, after the snapshot the real script took.
VICTIM="$(printf '%s\n' $SECRET_NAMES | head -1)"
for f in compose/docker-compose.yml ops/Caddyfile ops/zeroship.toml; do printf 'NEWLY-DEPLOYED\n' >"$SB_DEP/$f"; done
printf 'MUTATED\n' >>"$SB_DEP/compose/.env"
printf 'CHANGED\n' >"$SB_DEP/secrets/$VICTIM"
printf 'EXTRA\n'   >"$SB_DEP/secrets/added-after-the-snapshot"
run_rollback "$SB_DEP"
[ "$ROLL_RC" = 0 ] \
  && pass "END TO END: --rollback of a snapshot taken by an actual deploy run exits 0" \
  || fail "END TO END: --rollback exited $ROLL_RC: $ROLL_OUT"
e2e_bad=""
for f in compose/.env compose/docker-compose.yml ops/Caddyfile ops/zeroship.toml; do
  cmp -s "$SB_DEP/$f" "$DEP_BEFORE/$f" || e2e_bad="$e2e_bad $f"
done
for n in $SECRET_NAMES; do
  cmp -s "$SB_DEP/secrets/$n" "$DEP_BEFORE/secrets/$n" || e2e_bad="$e2e_bad secrets/$n"
done
[ -z "$e2e_bad" ] \
  && pass "END TO END: every deploy input is byte-identical to its pre-deploy content after --rollback (.env, compose, Caddyfile, ops/zeroship.toml and all $(printf '%s\n' $SECRET_NAMES | wc -l) secret files)" \
  || fail "END TO END: --rollback did not restore:$e2e_bad"
[ -f "$SB_DEP/secrets/added-after-the-snapshot" ] \
  && pass "a secret file created AFTER the snapshot is left in place, not deleted (an inert extra beats unrecoverable key loss)" \
  || fail "--rollback deleted a secret file that was not in the snapshot; deleting key material something may already be signing with is unrecoverable"

# --- D1 refusal: a server that rejects the configuration stops the deploy ---
#
# This is the assertion the outage is about. It FAILS against the pre-fix
# script, which has no check-config step at all and goes straight to the roll.
SB_CHK="$FIX/sb_deploy_checkfail"; seed_deploy "$SB_CHK"
run_deploy "$SB_CHK" ZS_GATE_DOCKER_FAIL='--check-config'
[ "$DEP_RC" != 0 ] \
  && pass "a server rejecting the new configuration makes the deploy exit non-zero" \
  || fail "the deploy exited 0 even though a server rejected the configuration"
[[ "$DEP_OUT" == *"reject the new configuration"* ]] \
  && pass "the refusal says which servers rejected the configuration" \
  || fail "the refusal did not name the failing servers: $DEP_OUT"
seen "$CAP" '### docker compose up -d' \
  && fail "the deploy restarted the stack after a server had already refused the configuration; the check bought nothing" \
  || pass "NOTHING IS RESTARTED when a server rejects the configuration (the old stack keeps serving)"

# --- the dry run must be of the NEW image ----------------------------------
#
# ZEROSHIP_IMAGE is referenced by the server-only docker-compose.override.yml,
# not by the tracked compose file. A host without that override renders the
# tracked file's literal tag, so all five --check-config runs would dry-run the
# OLD binaries and pass -- a green that means the opposite of what it says.
SB_IMG="$FIX/sb_deploy_wrongimage"; seed_deploy "$SB_IMG"
run_deploy "$SB_IMG" ZS_GATE_RENDERED_IMAGE=zeroship-platform:dev
[ "$DEP_RC" != 0 ] && [[ "$DEP_OUT" == *"does not resolve to"* ]] \
  && pass "a host whose compose does not resolve to the new image is refused, so the dry run can never be of the old binaries" \
  || fail "the deploy proceeded with the compose file rendering a different image (rc=$DEP_RC): $DEP_OUT"
seen "$CAP" '--check-config' \
  && fail "the --check-config runs went ahead against zeroship-platform:dev; every one of them would have passed on the old binaries" \
  || pass "no --check-config run happens once the rendered image is known to be wrong"

# --- D2 refusal: a missing secret FILE stops the deploy --------------------
#
# The compose file hands each binary an absolute path under
# /etc/zeroship/secrets. Provisioning is supposed to create every one; this asks
# independently, because the whole defect was two lists that disagreed. FAILS
# against the pre-fix script, which never looks at the secrets directory.
SB_SEC="$FIX/sb_deploy_nosecret"; seed_deploy "$SB_SEC"
VICTIM="$(printf '%s\n' $SECRET_NAMES | head -1)"
rm -f "$SB_SEC/secrets/$VICTIM"
run_deploy "$SB_SEC"
[ "$DEP_RC" != 0 ] \
  && pass "a missing secret file makes the deploy exit non-zero" \
  || fail "the deploy exited 0 with $VICTIM absent; the servers would have found out by crash-looping"
[[ "$DEP_OUT" == *"$VICTIM"* ]] \
  && pass "the refusal names the missing secret file ($VICTIM)" \
  || fail "the refusal did not name $VICTIM: $DEP_OUT"
seen "$CAP" '### docker compose up -d' \
  && fail "the deploy restarted the stack with $VICTIM absent" \
  || pass "NOTHING IS RESTARTED when a secret file the new compose references is missing"

# An EMPTY file is not a provisioned file. This is the shape a bind mount of a
# missing source leaves behind, and it fails at boot rather than at deploy.
SB_EMP="$FIX/sb_deploy_emptysecret"; seed_deploy "$SB_EMP"
: >"$SB_EMP/secrets/$VICTIM"
run_deploy "$SB_EMP"
[ "$DEP_RC" != 0 ] && [[ "$DEP_OUT" == *"$VICTIM"* ]] \
  && pass "an EMPTY secret file is treated as missing" \
  || fail "an empty $VICTIM was accepted (rc=$DEP_RC): $DEP_OUT"

# --- the rename trap: a GENERATED secret that was renamed ------------------
#
# THE MOST EXPENSIVE HOLE IN THE SCRIPT. Provisioning keeps a value only when
# THAT EXACT NAME is already in .env. The classification loop used to skip every
# generated secret before the rename pairing ran, on the grounds that
# provisioning would create it -- true on a fresh host, and catastrophic across
# a rename: the old value is orphaned, a new one is generated, and for
# ZEROSHIP_CONTROL_MASTER_KEY that means existing encrypted data can never be
# decrypted. Both `created` and `kept` read identically in the output.
#
# FAILS against the pre-fix script, which deploys quietly.
SB_REN="$FIX/sb_deploy_rename"; seed_deploy "$SB_REN"
grep -v '^ZEROSHIP_CONTROL_MASTER_KEY=' "$SB_REN/compose/.env" >"$SB_REN/compose/.env.tmp"
mv "$SB_REN/compose/.env.tmp" "$SB_REN/compose/.env"
printf 'CONTROL_MASTER_KEY=the-real-one\n' >>"$SB_REN/compose/.env"
run_deploy "$SB_REN"
[ "$DEP_RC" != 0 ] \
  && pass "a renamed GENERATED secret (CONTROL_MASTER_KEY -> ZEROSHIP_CONTROL_MASTER_KEY) refuses the deploy" \
  || fail "the deploy proceeded with the old name orphaned; provisioning would have generated a NEW master key and the existing ciphertext would be undecryptable"
[[ "$DEP_OUT" == *"RENAME OF A GENERATED SECRET"* && "$DEP_OUT" == *"ZEROSHIP_CONTROL_MASTER_KEY"* ]] \
  && pass "the refusal says it is a generated secret and names both spellings" \
  || fail "the refusal did not identify the generated-secret rename: $DEP_OUT"
seen "$CAP" "dev init" \
  && fail "provisioning ran anyway; the new master key has already been written" \
  || pass "provisioning never runs when a generated-secret rename is suspected"

# CONTROL: a FRESH host has no orphans, so absent generated secrets are exactly
# the case provisioning exists for and must NOT be refused. Without this, the
# guard above could simply be "refuse whenever a generated secret is absent",
# which would block every first deploy.
SB_FRESH="$FIX/sb_deploy_fresh"; seed_deploy "$SB_FRESH"
grep -v '^ZEROSHIP_CONTROL_MASTER_KEY=' "$SB_FRESH/compose/.env" >"$SB_FRESH/compose/.env.tmp"
mv "$SB_FRESH/compose/.env.tmp" "$SB_FRESH/compose/.env"
run_deploy "$SB_FRESH"
seen "$CAP" "dev init" \
  && pass "CONTROL: an absent generated secret with NO orphan on the host is provisioned, not refused (the fresh-install case)" \
  || fail "CONTROL: a fresh host was refused (rc=$DEP_RC); the rename guard has become 'refuse whenever a generated secret is absent': $DEP_OUT"

# --- the host layout must be complete before anything is backed up ---------
SB_LAY="$FIX/sb_deploy_layout"; seed_deploy "$SB_LAY"
rm -f "$SB_LAY/ops/zeroship.toml"
run_deploy "$SB_LAY"
[ "$DEP_RC" != 0 ] && [[ "$DEP_OUT" == *"ops/zeroship.toml"* ]] \
  && pass "a host missing ops/zeroship.toml is refused by name before anything is backed up or synced" \
  || fail "a missing ops/zeroship.toml was not refused (rc=$DEP_RC): $DEP_OUT"
seen "$CAP" '### scp' \
  && fail "the deploy synced config to a host whose layout it could not back up" \
  || pass "nothing is synced when the host layout is incomplete"

# ============================================================================
# The other half of D1, measured against the REAL binaries
# ============================================================================
#
# Everything above proves what the SCRIPT does with a refusal. It cannot prove
# that a stale overlay IS refused -- with stubbed docker, every configuration
# passes. That half is a fact about the servers, and the only honest way to
# establish it is to run one.
#
# CONDITIONAL, and not counted in the floor, for the same reason the shipped
# compose block is: this gate must stay runnable with no build. Build with
# `cargo build --bin zeroship-control` to turn it on.
echo ""
echo "-- a stale overlay field is REFUSED by the real binary (needs target/debug)"
CTL_BIN="$ROOT/target/debug/zeroship-control"
if [ ! -x "$CTL_BIN" ]; then
  echo "  note $CTL_BIN is not built; skipping (cargo build --bin zeroship-control)"
else
  ovl_env=(
    ZEROSHIP_CONTROL_DATABASE_URL=postgres://u:p@postgres:5432/z
    ZEROSHIP_CONTROL_KEY=1111111111111111111111111111111111111111111111111111111111111111
    ZEROSHIP_PAIRWISE_SALT=4444444444444444444444444444444444444444444444444444444444444444
    ZEROSHIP_CONTROL_MASTER_KEY=5555555555555555555555555555555555555555555555555555555555555555
    ZEROSHIP_AUTH_PLATFORM_ISSUER=https://auth.example.com/oauth2
  )
  # The GOOD overlay is the one this repo ships, so the control below is not a
  # hand-written minimum that happens to parse.
  env -i PATH=/usr/bin:/bin HOME=/tmp "${ovl_env[@]}" \
    ZEROSHIP_CONFIG="$ROOT/deploy/ops/zeroship.toml" "$CTL_BIN" --check-config >/dev/null 2>&1
  good_rc=$?
  # ONE VARIABLE: the same file with the ONE key spelled the way the host still
  # spelled it. Appending a second [observability] table instead would also be
  # refused -- as a duplicate table header, which is a different defect and
  # would let this assertion pass while proving nothing about the rename.
  sed 's/^log_filter =/rust_log =/' "$ROOT/deploy/ops/zeroship.toml" >"$FIX/stale.toml"
  cmp -s "$FIX/stale.toml" "$ROOT/deploy/ops/zeroship.toml" \
    && fail "the stale fixture is identical to the shipped overlay; the sed did not apply and the case below is vacuous"
  STALE_OUT="$(env -i PATH=/usr/bin:/bin HOME=/tmp "${ovl_env[@]}" \
    ZEROSHIP_CONFIG="$FIX/stale.toml" "$CTL_BIN" --check-config 2>&1)"
  stale_rc=$?
  [ "$good_rc" = 0 ] \
    && pass "CONTROL: --check-config accepts the overlay this repo ships (exit 0)" \
    || fail "CONTROL: --check-config rejected the SHIPPED overlay (exit $good_rc); the stale case below would prove nothing"
  [ "$stale_rc" != 0 ] && [[ "$STALE_OUT" == *"rust_log"* ]] \
    && pass "the same overlay plus the host's stale 'rust_log' is REFUSED by name (exit $stale_rc), so --check-config would have caught this before the roll" \
    || fail "a stale rust_log was accepted (exit $stale_rc): $STALE_OUT"
fi

echo ""
echo "  $PASS passed, $FAIL failed, $((PASS+FAIL)) ran"

# Floor counts assertions that RAN, not that PASSED: a mutation moves an
# outcome BETWEEN those columns, so only a LOST assertion drops the sum. Blocks
# that depend on something outside this file are conditional and deliberately
# NOT counted in the floor.
# MEASURED 2026-08-12: 53 unconditional assertions (54 ran with the shipped
# compose present). RE-MEASURED 2026-08-13 after the rule-2 rename block: 63
# unconditional (64 with the shipped compose present).
# RE-MEASURED 2026-08-13 after the deploy-path, secret_files and snapshot
# blocks: 108 unconditional; 112 with the shipped compose + dev.rs present;
# 114 with target/debug/zeroship-control built as well.
# 2026-08-19: the overlay check for the platform mint credential is deleted, so
# every count above drops by one. MEASURED both sides on one host with
# target/debug/zeroship-control absent: 118 ran before, 117 after. That
# credential is no longer a declared setting, so an overlay naming it is an
# unknown field, which the --check-config case at the bottom already covers.
# 2026-08-20: the source-tree block adds 11 unconditional assertions (4 parser
# fixtures, 7 end-to-end through the stub ssh) plus 1 conditional one that needs
# pnpm-lock.yaml, Cargo.toml and .gitmodules. MEASURED both sides on one host
# with target/debug/zeroship-control absent: 117 ran before, 129 after.
# 2026-08-21: the edge-host-claim block adds 16 assertions, of which exactly ONE
# (the sentinel-less artifact) is unconditional; the other 15 are guarded on
# tracked repo files and so are not counted here, by the same rule as the
# shipped-compose and source-tree blocks above. MEASURED both sides on one host
# with target/debug/zeroship-control absent: 141 ran before, 157 after; arms 7
# before, 9 after. Those 15 are not left unguarded by that: what protects them
# is the pair of ARMS, which refuse when either extraction goes empty, and that
# is the stronger guard of the two.
MIN_RAN=119
RAN=$((PASS + FAIL))
rc=0
[ "$FAIL" -eq 0 ] || rc=1
if [ "$RAN" -lt "$MIN_RAN" ]; then
  echo "  x FLOOR: only $RAN assertions ran, expected at least $MIN_RAN." >&2
  echo "    Assertions went missing - a smaller green is not a pass." >&2
  rc=1
fi
# The floor above counts ASSERTIONS and the trailer below counts ITEMS, and the
# 2026-08-20 failure is exactly the gap between them: 141 assertions ran, one of
# them over an empty set, and the floor was green.
gate_arms_finish || rc=1
exit $rc
