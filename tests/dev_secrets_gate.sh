#!/usr/bin/env bash
# ============================================================================
# tests/lib/dev_secrets.sh decides whether a harness may run at all, and it had
# no test of any kind.
#
# THE DEFECT THIS EXISTS FOR, found 2026-08-20. `_dev_secrets_complete` carried
# a hand-written list of secret files, and `control-signing.pem` was on it.
# 8e365f478 (2026-08-17) deleted control's PAT signing key outright - nothing
# but a PatIssuer read it - and removed the file from `secret_specs()` in
# crates/cli/src/dev.rs, from both compose mounts, and from
# crates/cli/tests/dev_init_test.rs. It did not remove it from here.
#
# `zeroship dev init` therefore stopped writing a file this function demanded,
# so the function returned 1 on EVERY machine, ALWAYS, and `ensure_dev_secrets`
# failed both its callers (tests/e2e_docker.sh, tests/external_chain.sh) before
# they started. Neither is in the auth or billing suites, so no number anyone
# watches ever moved. The same list was ALSO missing `migrate-dsn`, which the
# compose file does mount - wrong in both directions at once.
#
# The list is now derived from the compose file. This gate exists so that a
# check which CANNOT PASS is loud rather than silent, and it asserts the
# property directly: every file the function demands is a file `dev init`
# writes, and every file the shipped compose names is demanded.
#
# WHAT IT DOES NOT CHECK, so a green is not over-read:
#   - that a real `zeroship dev init` succeeds. It runs no binary and builds
#     nothing; it drives the pure function against fixture directories. The
#     real end-to-end (`dev init` on an empty tree, then this function) was run
#     by hand on 2026-08-20 and has no coverage here.
#   - the CONTENT of any secret. This is presence and non-emptiness, exactly as
#     the function is; a file holding the wrong kind of material passes both.
#   - the two callers. Whether e2e_docker.sh and external_chain.sh then work is
#     a docker question and is not asked here.
#   - the environment-variable half against any source of truth. The producer
#     is `zeroship_core::config::PLATFORM_SECRETS`, which dev.rs drives (it
#     held its own `ENV_KEYS` copy until 2026-08-20), but the names in that
#     const are not distinguished in the SOURCE TEXT from any other quoted
#     uppercase constant, so pairing them here would assert this file's parse
#     rather than the contract. Reading it as typed data needs a Rust consumer,
#     and the one that did (a crates/zeroship-gatekit compose gate) was deleted
#     on 2026-08-21; nothing reads the table against compose today.
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LIB="$ROOT/tests/lib/dev_secrets.sh"
REAL_COMPOSE="$ROOT/deploy/compose/docker-compose.yml"
DEV_RS="$ROOT/crates/cli/src/dev.rs"

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }

echo "============================================"
echo "  tests/lib/dev_secrets.sh"
echo "============================================"

[ -f "$LIB" ] || { echo "  x REFUSED: $LIB not found." >&2; exit 1; }
# shellcheck source=/dev/null
. "$LIB"
declare -F _dev_secrets_missing >/dev/null || {
  echo "  x REFUSED: sourcing $LIB did not define _dev_secrets_missing." >&2; exit 1; }

# Per-arm anti-vacuity accounting (tests/lib/gate_arms.sh). The file half and
# the variable half of the demanded set are two independent enumerations
# inside _dev_secrets_missing (one reads compose `/etc/zeroship/secrets/...`
# mounts, the other walks a fixed name list) - either can collapse to zero on
# its own without the other noticing.
# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init dev_secrets

FIX="$(mktemp -d)"
trap 'rm -rf "$FIX"' EXIT
SEC="$FIX/secrets"
ENVF="$FIX/.env"
mkdir -p "$SEC"

# The REAL compose file, because the file half of the contract is derived from
# it and a fixture would only test this gate's idea of the shape. It is copied
# rather than read in place: _dev_secrets_missing looks for the compose file
# beside the secrets directory it is given.
[ -f "$REAL_COMPOSE" ] || { echo "  x REFUSED: $REAL_COMPOSE not found." >&2; exit 1; }
cp "$REAL_COMPOSE" "$FIX/docker-compose.yml"

# Start from an empty secrets directory and NO .env, then build up. The first
# report is the whole demanded set, which is how the required list below is
# obtained without this file keeping a copy of it.
MISSING_NO_ENV="$(_dev_secrets_missing "$ENVF" "$SEC")"
case "$MISSING_NO_ENV" in
  *"no environment overlay at all"*) pass "an absent .env is reported as such, not as a missing variable" ;;
  *) fail "an absent .env produced [$MISSING_NO_ENV]" ;;
esac

# An .env holding every variable the function asks for. Derived by asking the
# function itself, so a variable added to it is covered without an edit here.
: >"$ENVF"
VARS_DEMANDED="$(_dev_secrets_missing "$ENVF" "$SEC" | sed -n 's/ (variable)$//p')"
N_VARS_DEMANDED=$(printf '%s\n' "$VARS_DEMANDED" | grep -c .)
# MEASURED 2026-08-20: 8 platform-secret variable names in the fixed list
# inside _dev_secrets_missing. Floor well under that: this list moves by ones
# as secrets are added or retired, while the failure this guards against - the
# fixed `for name in ...` list emptied out, or _dev_secrets_missing stopped
# being sourced correctly - drops it to zero.
gate_arm required_env_vars "$N_VARS_DEMANDED" 3 || true
for v in $VARS_DEMANDED; do
  printf '%s=x\n' "$v" >>"$ENVF"
done
VARS_LEFT="$(_dev_secrets_missing "$ENVF" "$SEC" | grep -c '(variable)$')"
[ "$VARS_LEFT" = 0 ] \
  && pass "with every named variable present, none is reported missing ($(grep -c . "$ENVF") variables)" \
  || fail "$VARS_LEFT variables are still reported missing after writing all of them"

REQUIRED="$(_dev_secrets_missing "$ENVF" "$SEC" | sed -n 's/ (secret file)$//p' | sort -u)"
if [ -z "${REQUIRED//[[:space:]]/}" ]; then
  fail "the function demands NO secret file at all against the shipped compose; every case below would be vacuous"
else
  pass "the shipped compose yields a non-empty demanded file set ($(echo $REQUIRED))"
fi
N_REQUIRED=$(printf '%s\n' "$REQUIRED" | grep -c .)
# MEASURED 2026-08-20: 7 secret files named in the shipped compose's
# `/etc/zeroship/secrets/...` mounts. Floor well under that: this is the SAME
# set the two loops below (dev.rs pairing, discrimination) iterate over, so
# one arm covers all three call sites - if the compose scan loses its anchor
# they all silently iterate zero times together.
gate_arm required_secret_files "$N_REQUIRED" 3 || true

# ------------------------------------------------------------- THE REGRESSION
#
# Every file this function demands must be one `zeroship dev init` writes.
# control-signing.pem failed exactly this and nothing asked.
#
# `pairwise-salt` is written by ensure_pairwise_file(), not by secret_specs(),
# so it is matched against the whole of dev.rs rather than against a spec
# tuple - the question is whether the provisioner writes the name at all.
if [ -f "$DEV_RS" ]; then
  UNWRITTEN=""
  for n in $REQUIRED; do
    grep -qF "\"$n\"" "$DEV_RS" || UNWRITTEN="$UNWRITTEN $n"
  done
  [ -z "$UNWRITTEN" ] \
    && pass "every demanded file is a name crates/cli/src/dev.rs writes" \
    || fail "these are demanded but 'zeroship dev init' never writes them, so this check CANNOT PASS on any machine:$UNWRITTEN"

  # CONTROL. Without it, "all found" could mean the grep matches anything.
  grep -qF '"control-signing.pem"' "$DEV_RS" \
    && fail "CONTROL: dev.rs still names control-signing.pem; the pairing above discriminates nothing" \
    || pass "CONTROL: the deleted control-signing.pem is absent from dev.rs, so the pairing above can fail"
fi

# ------------------------------------------------------------ discrimination
#
# A check that passes is worthless if it would pass for anything. Each required
# file is removed in turn and must be reported BY NAME.
mkfull() { local n; rm -rf "$SEC"; mkdir -p "$SEC"; for n in $REQUIRED; do printf 'X\n' >"$SEC/$n"; done; }

mkfull
COMPLETE="$(_dev_secrets_missing "$ENVF" "$SEC")"
[ -z "$COMPLETE" ] \
  && pass "a complete directory reports nothing missing" \
  || fail "a complete directory reported [$COMPLETE]"
_dev_secrets_complete "$ENVF" "$SEC" \
  && pass "_dev_secrets_complete returns 0 on a complete directory" \
  || fail "_dev_secrets_complete returned non-zero on a complete directory"

for n in $REQUIRED; do
  mkfull; rm -f "$SEC/$n"
  OUT="$(_dev_secrets_missing "$ENVF" "$SEC")"
  if [ "$OUT" = "$n (secret file)" ]; then
    pass "removing $n is reported, by name, and nothing else is"
  else
    fail "removing $n reported [$OUT]"
  fi
done

# EMPTY, not absent. `dev init` creates the file before it writes, and a
# zero-byte key would boot a service with no key material rather than refuse.
mkfull
FIRST="$(printf '%s\n' $REQUIRED | head -1)"
: >"$SEC/$FIRST"
OUT="$(_dev_secrets_missing "$ENVF" "$SEC")"
[ "$OUT" = "$FIRST (secret file)" ] \
  && pass "a zero-byte $FIRST is treated as missing, not as present" \
  || fail "a zero-byte $FIRST reported [$OUT]"

mkfull
VAR="$(grep -m1 -o '^[A-Z_]*' "$ENVF")"
grep -v "^${VAR}=" "$ENVF" >"$FIX/.env.short"
OUT="$(_dev_secrets_missing "$FIX/.env.short" "$SEC")"
[ "$OUT" = "$VAR (variable)" ] \
  && pass "dropping $VAR from the overlay is reported, by name" \
  || fail "dropping $VAR reported [$OUT]"

# ------------------------------------------------------------- anti-vacuity
#
# The file half is DERIVED, so a compose file it cannot read makes the loop
# iterate zero times - and "no file is missing" is precisely what a check that
# has stopped looking reports. It must refuse instead.
mkfull
mkdir -p "$FIX/blind"
cp "$ENVF" "$FIX/blind/.env"
mkdir -p "$FIX/blind/secrets"
printf 'services:\n  control:\n    image: x\n' >"$FIX/blind/docker-compose.yml"
OUT="$(_dev_secrets_missing "$FIX/blind/.env" "$FIX/blind/secrets")"
case "$OUT" in
  *"no secret file references could be read out of it"*)
    pass "a compose file naming no secret refuses rather than reporting a complete tree" ;;
  *) fail "a compose file naming no secret reported [$OUT]" ;;
esac

rm -f "$FIX/blind/docker-compose.yml"
OUT="$(_dev_secrets_missing "$FIX/blind/.env" "$FIX/blind/secrets" 2>/dev/null)"
case "$OUT" in
  *"no secret file references could be read out of it"*)
    pass "an ABSENT compose file refuses too (an unreadable source is not an empty one)" ;;
  *) fail "an absent compose file reported [$OUT]" ;;
esac

echo ""
echo "  $PASS passed, $FAIL failed, $((PASS+FAIL)) ran"

# Floor counts assertions that RAN, not that PASSED: a mutation moves an
# outcome BETWEEN those columns, so only a LOST assertion drops the sum.
#
# MEASURED 2026-08-20: 18 with the shipped compose and dev.rs both present -
# 11 fixed assertions plus one per secret file the compose names, of which
# there are 7. A floor rather than an equality because that loop scales with
# the compose file; set one below the measurement so a single file legitimately
# leaving compose does not trip it, while a LOST assertion block still does.
MIN_RAN=17
RAN=$((PASS + FAIL))
rc=0
[ "$FAIL" -eq 0 ] || rc=1
if [ "$RAN" -lt "$MIN_RAN" ]; then
  echo "  x FLOOR: only $RAN assertions ran, expected at least $MIN_RAN." >&2
  echo "    Assertions went missing - a smaller green is not a pass." >&2
  rc=1
fi

gate_arms_finish || rc=1
exit $rc
