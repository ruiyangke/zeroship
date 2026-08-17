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
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REMOTE="$ROOT/deploy/scripts/deploy-remote.sh"
APPDEP="$ROOT/deploy/scripts/deploy-app.sh"
REAL_COMPOSE="$ROOT/deploy/compose/docker-compose.yml"
REAL_OVERLAY="$ROOT/deploy/ops/zeroship.toml"
GRANTS_MIGRATION="$ROOT/db/migrations-ts/20260702000900_grants.ts"

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

if ! declare -F compose_vars >/dev/null || ! declare -F rename_suspects >/dev/null; then
  echo "  x REFUSED: sourcing $REMOTE did not define compose_vars/rename_suspects." >&2
  exit 1
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
# crates/cli/src/dev.rs, which is env-shaped by construction, so the seven
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
if [ -f "$REAL_COMPOSE" ]; then
  DRY_RUN_SERVICES=""
  for svc in $CHECK_SERVICES; do DRY_RUN_SERVICES="$DRY_RUN_SERVICES ${svc%%:*}"; done
  # `<service>\t<flag>` for every `--<name>-file` on a live line, attributed to
  # the service block it sits in.
  FLAG_FILES="$(
    awk '
      /^  [a-z][a-z0-9_-]*:[[:space:]]*$/ { svc = $1; sub(/:$/, "", svc); next }
      /^[[:space:]]*#/ { next }
      svc != "" && match($0, /--[a-z-]+-file/) {
        print svc "\t" substr($0, RSTART, RLENGTH)
      }
    ' "$REAL_COMPOSE" | sort -u | while IFS=$'\t' read -r s f; do
      case " $DRY_RUN_SERVICES " in *" $s "*) echo "$s $f" ;; esac
    done
  )"
  # Anti-hollow: the walk is worthless if it stops attributing lines to
  # services, and that looks exactly like compliance. Every dry-run service
  # must be visible to it.
  SEEN_SERVICES="$(awk '/^  [a-z][a-z0-9_-]*:[[:space:]]*$/ { s=$1; sub(/:$/,"",s); print s }' "$REAL_COMPOSE" | sort -u)"
  MISSING_SEEN=""
  for s in $DRY_RUN_SERVICES; do
    echo "$SEEN_SERVICES" | grep -qx "$s" || MISSING_SEEN="$MISSING_SEEN $s"
  done
  if [ -n "$MISSING_SEEN" ]; then
    fail "the service walk did not see$MISSING_SEEN, so a clean result below would mean nothing"
  elif [ -z "$FLAG_FILES" ]; then
    pass "no dry-run service passes a secret path as a command flag, so --check-config sees every one of them"
  else
    fail "these secret paths are passed as command flags on a service the pre-roll dry-runs, and are therefore invisible to --check-config: $(echo $FLAG_FILES). Move them to their canonical ZEROSHIP_*_FILE environment name"
  fi
fi

# The operator overlay is shared with services that must never possess the
# platform mint credential. A file reference here leaks the raw key through
# their shared config or secret-directory mounts even if the binary ignores
# the setting.
if [ -f "$REAL_OVERLAY" ]; then
  if grep -Eq '^[[:space:]]*platform_mint_key[[:space:]]*=' "$REAL_OVERLAY"; then
    fail "the shared operator overlay contains auth.platform_mint_key"
  else
    pass "the shared operator overlay contains no platform mint credential"
  fi
else
  fail "the shipped operator overlay is missing: $REAL_OVERLAY"
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

if [ -f "$GRANTS_MIGRATION" ]; then
  grep -qF 'REVOKE ALL PRIVILEGES ON ALL TABLES IN SCHEMA zeroship FROM zeroship_worker' "$GRANTS_MIGRATION" \
    && pass "the platform grants revoke worker authority from every current zeroship table" \
    || fail "the platform grants do not revoke worker authority from every current zeroship table"
  grep -qF 'ALTER DEFAULT PRIVILEGES IN SCHEMA zeroship REVOKE ALL PRIVILEGES ON TABLES FROM zeroship_worker' "$GRANTS_MIGRATION" \
    && pass "future zeroship tables remain denied to the worker by default" \
    || fail "future zeroship tables are not denied to the worker by default"
else
  fail "the platform grants migration is missing: $GRANTS_MIGRATION"
fi

# THE DRIFT CHECK, and the actual defect this whole area is about: two lists,
# one of which nobody updated. Every file the SHIPPED compose hands a binary
# must be a file the provisioner (`secret_specs()` + the pairwise-salt case in
# crates/cli/src/dev.rs) knows how to create. This is asserted against the real
# compose deliberately -- a fixture cannot go stale in the way that matters.
DEV_RS="$ROOT/crates/cli/src/dev.rs"
if [ -f "$REAL_COMPOSE" ] && [ -f "$DEV_RS" ]; then
  REAL_SECRETS="$(secret_files "$REAL_COMPOSE")"
  if ! usable "$REAL_SECRETS"; then
    fail "secret_files extracted NOTHING from the shipped compose file"
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
  "MIGRATED_DATABASE_URL ZEROSHIP_MIGRATED_DATABASE_URL" \
  "WORKER_KV_URL ZEROSHIP_WORKER_KV_URL" \
  "PROVISION_DATABASE_URL ZEROSHIP_MIGRATED_PROVISION_DATABASE_URL" \
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
case "$*" in
  *"config --images"*) printf '%s\n' "${ZS_GATE_RENDERED_IMAGE-${ZS_GATE_RUNNING_IMAGE:-}}" ;;
  inspect*)            printf '%s\n' "${ZS_GATE_RUNNING_IMAGE:-}" ;;
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

# The secret files the shipped compose references. Derived, for the same
# reason: this gate must not carry its own copy of a list the script reads.
SECRET_NAMES="$(secret_files "$REAL_COMPOSE" | tr '\n' ' ')"

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

run_deploy() { # $1 sandbox, rest: extra argv. Sets DEP_RC, DEP_OUT and CAP.
  CAP_N=$((CAP_N+1))
  CAP="$FIX/capture.$CAP_N"
  : >"$CAP"
  local sb="$1"; shift
  DEP_OUT="$(env PATH="$STUB:$PATH" ZS_GATE_CAPTURE="$CAP" ZS_GATE_RUNNING_IMAGE="$FAKE_IMAGE" "$@" \
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
    ZEROSHIP_AUTH_PLATFORM_MINT_KEY=3333333333333333333333333333333333333333333333333333333333333333
    ZEROSHIP_CONTROL_DATABASE_URL=postgres://u:p@postgres:5432/z
    ZEROSHIP_CONTROL_KEY=1111111111111111111111111111111111111111111111111111111111111111
    ZEROSHIP_WORKER_KEY=2222222222222222222222222222222222222222222222222222222222222222
    ZEROSHIP_PAIRWISE_SALT=4444444444444444444444444444444444444444444444444444444444444444
    ZEROSHIP_CONTROL_MASTER_KEY=5555555555555555555555555555555555555555555555555555555555555555
    ZEROSHIP_AUTH_PLATFORM_ISSUER=https://auth.example.com/oauth2
    # The issuer above is the PUBLIC name a token's `iss` carries; this is the
    # address control dials to mint one. Two settings on purpose, and control
    # refuses to start with only the first.
    ZEROSHIP_AUTH_PLATFORM_MINT_URL=http://auth:9092
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
MIN_RAN="${DEPLOY_SCRIPTS_MIN_RAN:-108}"
RAN=$((PASS + FAIL))
rc=0
[ "$FAIL" -eq 0 ] || rc=1
if [ "$RAN" -lt "$MIN_RAN" ]; then
  echo "  x FLOOR: only $RAN assertions ran, expected at least $MIN_RAN." >&2
  echo "    Assertions went missing - a smaller green is not a pass." >&2
  rc=1
fi
exit $rc
