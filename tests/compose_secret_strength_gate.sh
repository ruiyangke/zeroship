#!/usr/bin/env bash
# ============================================================================
# Every secret deploy/compose supplies must satisfy the MINIMUM LENGTH THE
# PRODUCT ITSELF ENFORCES.
#
# THE DEFECT THIS EXISTS FOR, measured 2026-08-11 by bringing the stack up on a
# fresh database (scenario 18). Three of five services could not start, and two
# of the three failures were the same root cause: the compose file ships
# placeholder secrets shorter than the minimum the binaries refuse below.
#
#   worker:   worker: refusing to start with unsafe WORKER_KEY
#             WORKER_KEY is too short (10 bytes); minimum 32 bytes
#   control:  config: WORKER_KEY / --worker-key: secret reference env var
#             'ZEROSHIP_WORKER_KEY' is not set
#
# Both refusals are the PRODUCT BEHAVING CORRECTLY - crates/worker/src/main.rs:460
# and crates/control/src/main.rs:878 both decline to run with a weak key. The
# defect is entirely in the shipped configuration.
#
# WHY THE ENFORCED SET IS DERIVED, NOT LISTED. A hardcoded list of "secrets with
# a minimum" is satisfiable by editing the list, which is the shape this repo has
# been burned by repeatedly. The names and thresholds are parsed out of
# crates/core/src/config/secrets.rs, so a FOURTH secret gaining a minimum is
# picked up here without anyone remembering to update this file.
#
# WHAT BOUNDING FIRST BOUGHT, recorded because it is the reason this gate covers
# more than the one failure that fired: WORKER_KEY (10 bytes) is what actually
# crashed. STASH_SIGNING_KEY ships at 27 bytes and is checked by the same code,
# so fixing only WORKER_KEY would have moved the failure to the gateway on the
# next bring-up rather than clearing it. PAIRWISE_SALT ships at 38 and passes.
#
# WHAT THIS DOES NOT CHECK, so a green is not over-read:
#   - ENTROPY. Length is not strength; a 32-byte run of "a" passes here and is
#     worthless. This gate only enforces what the product enforces.
#   - secrets with NO minimum in the product. CONTROL_KEY (12 bytes) and
#     MASTER_KEY (10 bytes) are also short and are NOT length-checked anywhere,
#     so they pass today. That is a real observation, not a covered case.
#   - WHICH SERVICE gets which secret. control declares it needs WORKER_KEY via
#     deploy/ops/zeroship.toml and the compose file sets it on gateway and worker
#     only; that mapping is a judgement this file cannot derive, and the
#     bring-up is what catches it.
#   - that any of this is safe to ship publicly. These are dev defaults.
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CF="$ROOT/deploy/compose/docker-compose.yml"
SRC="$ROOT/crates/core/src/config/secrets.rs"
PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }

echo "============================================"
echo "  compose secrets meet the product's own minimum length"
echo "============================================"

[ -f "$CF" ]  || { echo "  x REFUSED: $CF not found." >&2; exit 1; }
[ -f "$SRC" ] || { echo "  x REFUSED: $SRC not found - the enforced set is derived from it." >&2; exit 1; }

# NAME -> minimum, parsed from the product's own refusal messages, which read
#   "<NAME> is too short ({} bytes); minimum <N> bytes"
mapfile -t RULES < <(
  grep -oE '"[A-Z_]+ is too short \(\{\} bytes\); minimum [0-9]+ bytes"' "$SRC" |
  sed -E 's/^"([A-Z_]+) is too short.*minimum ([0-9]+) bytes"$/\1 \2/'
)

if [ "${#RULES[@]}" -eq 0 ]; then
  echo "  x REFUSED: parsed ZERO length rules out of $SRC." >&2
  echo "    Either the message shape changed or this parser is broken; a gate" >&2
  echo "    that checks nothing must not report success." >&2
  exit 1
fi

for rule in "${RULES[@]}"; do
  name="${rule%% *}"; min="${rule##* }"
  # Compose supplies these either literally (`NAME: value`) or with a shell
  # default (`NAME: ${NAME:-value}`); take the default in the latter case, since
  # that is what an operator who sets nothing actually gets.
  mapfile -t VALUES < <(
    grep -hoE "^ +(ZEROSHIP_)?${name}: .*$" "$CF" |
    sed -E "s/^ +(ZEROSHIP_)?${name}: //; s/^\\\$\{[A-Z_]+:-(.*)\}$/\1/" |
    sort -u
  )
  if [ "${#VALUES[@]}" -eq 0 ]; then
    pass "$name has a ${min}-byte minimum and compose supplies no value (nothing to check)"
    continue
  fi
  for v in "${VALUES[@]}"; do
    len=$(printf '%s' "$v" | wc -c)
    if [ "$len" -ge "$min" ]; then
      pass "$name = ${len} bytes (minimum $min)"
    else
      fail "$name = ${len} bytes, BELOW the ${min}-byte minimum the product enforces; the service refuses to start"
    fi
  done
done

echo ""
echo "  $PASS passed, $FAIL failed, $((PASS+FAIL)) ran"

# Floor counts assertions that RAN, not that PASSED.
# MEASURED 2026-08-11: 3 enforced secrets, each supplied once by compose.
MIN_RAN="${COMPOSE_SECRETS_MIN_RAN:-3}"
RAN=$((PASS + FAIL))
rc=0
[ "$FAIL" -eq 0 ] || rc=1
if [ "$RAN" -lt "$MIN_RAN" ]; then
  echo "  x FLOOR: only $RAN secrets checked, expected at least $MIN_RAN." >&2
  echo "    Rules or values went missing from the parse - a smaller green is not a pass." >&2
  rc=1
fi
exit $rc
