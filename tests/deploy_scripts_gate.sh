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
#   - NOTHING past the argument and credential handling of either script. No
#     ssh, no port-forward, no docker build/push/roll, no backup, no rollback,
#     no probe. Those need a host and are not faked here.
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
SUS="$(rename_suspects "GATEWAY_OIDC_SECRET" "GATEWAY_DATABASE_URL")"
if [ -z "$SUS" ]; then
  pass "GATEWAY_OIDC_SECRET and GATEWAY_DATABASE_URL do not pair (GATEWAY and SECRET are stoplisted)"
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

echo ""
echo "  $PASS passed, $FAIL failed, $((PASS+FAIL)) ran"

# Floor counts assertions that RAN, not that PASSED: a mutation moves an
# outcome BETWEEN those columns, so only a LOST assertion drops the sum. The
# real-compose block is conditional and deliberately NOT counted in the floor.
# MEASURED 2026-08-12: 29 unconditional assertions (30 ran with the shipped
# compose present).
MIN_RAN="${DEPLOY_SCRIPTS_MIN_RAN:-29}"
RAN=$((PASS + FAIL))
rc=0
[ "$FAIL" -eq 0 ] || rc=1
if [ "$RAN" -lt "$MIN_RAN" ]; then
  echo "  x FLOOR: only $RAN assertions ran, expected at least $MIN_RAN." >&2
  echo "    Assertions went missing - a smaller green is not a pass." >&2
  rc=1
fi
exit $rc
