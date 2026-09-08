# shellcheck shell=bash
#
# Make "this arm examined nothing" impossible to spell as a pass.
#
# THE FAILURE THIS EXISTS FOR, four separate instances found on 2026-08-20, each
# by a human reading rather than by any test:
#
#   ws_subscription_stub_gate.sh arm 1  watched ONE identifier that the commit
#                                       it was written for had just deleted. It
#                                       examined 0 names and printed green,
#                                       while THREE live phantom identifiers sat
#                                       in the file it guards.
#   skip_marker_gate.sh                 8 raw hits, 8 excused by its own
#                                       allowlist, 0 ruled on. Green. That gate
#                                       is DELETED - the skip protocol it
#                                       policed is gone - but it remains the
#                                       clearest instance of the shape.
#   deploy_scripts_gate.sh argv scan    1 pre-filter row, on the single service
#                                       the filter excludes. 0 examined. Green.
#   compose_secret_strength_gate.sh     derived its rule set by regexing another
#                                       file's refusal MESSAGE; the message
#                                       improved and the rule set went to 0.
#                                       Its anti-vacuity guard fired, so it went
#                                       RED - the only one of the four that
#                                       announced itself, and the only reason
#                                       anyone went looking for the others.
#
# One shape: A CHECK THAT EXAMINES NOTHING AND A CLEAN TREE PRINT THE SAME
# THING. The last case is the counter-example that shows the fix works, so the
# fix is to make every arm carry what that one carried.
#
# WHY PER-ARM AND NOT PER-GATE. ws_subscription_stub_gate.sh ALREADY had a
# gate-level guard (`if [ "$RAN" -lt 3 ]`) and it was green throughout, because
# three arms did run - one of them over an empty set. A gate-level count cannot
# see inside a gate. That is the measurement, not a preference.
#
# THE CONTRACT. A gate declares each arm with the number of ITEMS THAT ARM
# RULED ON - not files opened, not lines read: the things whose verdict the arm
# actually decided - and a floor that number must clear. The floor lives HERE,
# in the gate, next to the code that produces the number, where the person
# changing that code will see it. There is deliberately no central table of
# expected counts: that would be a census, and a census going stale is the
# fourth of the failures above.
#
# WHAT A FLOOR IS FOR, and it is not a target. It separates "clean" from "did
# not look". Set it well under today's number - far enough that ordinary
# editing does not reach it, close enough that a collapse does. A floor equal
# to today's count turns every deletion into a gate failure and gets lowered
# until it means nothing.
#
# WHAT THIS DOES NOT DO. It does not check that an arm's verdict is CORRECT, or
# that the items it counted are the right items. An arm that enumerates 400 of
# the wrong things passes here. It rules on one question only: did this arm
# have anything to rule on.
#
# Machine-readable emission, one line per arm, on stdout:
#
#     zsgate-arm gate=<gate-id> arm=<arm-id> examined=<n> floor=<m>
#
# That line is a WIRE FORMAT, not prose to be scraped. `tests/gate_arm_census.sh`
# consumes it; changing the shape means changing both files in the same patch.
# The whole reason this is fixed is that the compose-secret failure above was
# caused by one program regexing another program's human-readable message.
#
# Usage:
#
#     . "$ROOT/tests/lib/gate_arms.sh"
#     gate_arms_init my_thing
#     ...
#     gate_arm citations "$n_checked" 40
#     gate_arm doc_501   "$n_doc"     1
#     gate_arms_finish || exit 1
#
# Self-test, with the one-variable controls: tests/lib_gate_arms_selftest.sh

# All state is shell-global with a reserved prefix. NOT environment variables:
# a gate whose accounting depends on how the process was launched is a gate
# whose result cannot be reproduced from its invocation (operator rule,
# 2026-08-20). Nothing here exports, and nothing here reads the environment.
ZS_GATE_ARMS_ID=""
ZS_GATE_ARMS_COUNT=0
ZS_GATE_ARMS_SEEN=""
ZS_GATE_ARMS_REFUSALS=0
ZS_GATE_ARMS_STARTED=0

# gate_arms_init <gate-id>
#
# <gate-id> names the gate in every emitted line and every refusal. Use the
# script's basename without `_gate.sh`, so a CI log line points at a file.
gate_arms_init() {
  ZS_GATE_ARMS_ID="${1:-}"
  ZS_GATE_ARMS_COUNT=0
  ZS_GATE_ARMS_SEEN=""
  ZS_GATE_ARMS_REFUSALS=0
  ZS_GATE_ARMS_STARTED=1
  if [ -z "$ZS_GATE_ARMS_ID" ]; then
    echo "GATE ARM CONTRACT: gate_arms_init called with no gate id." >&2
    ZS_GATE_ARMS_ID="<unnamed>"
    ZS_GATE_ARMS_REFUSALS=1
  fi
}

# _gate_arms_is_uint <string> - a bare non-negative decimal integer.
#
# Strict on purpose. `examined` is nearly always a command substitution, and a
# command that FAILED and a command that found nothing both tend to produce
# something that is not a number - empty, or a wrapped error. Accepting those
# as "probably zero" would reintroduce the exact ambiguity this file exists to
# remove, so a malformed count is a refusal and says which arm produced it.
_gate_arms_is_uint() {
  case "${1:-}" in
    ''|*[!0-9]*) return 1 ;;
    *) return 0 ;;
  esac
}

# gate_arm <arm-id> <examined> <floor>
#
# Records one arm, emits its census line, and refuses when the arm ruled on
# fewer items than its floor. Returns 0 when the arm cleared its floor and 1
# when it did not, so a caller may branch, but the verdict is carried to
# `gate_arms_finish` either way - an ignored return value cannot lose it.
gate_arm() {
  local arm="${1:-}" examined="${2:-}" floor="${3:-}"

  if [ "$ZS_GATE_ARMS_STARTED" -ne 1 ]; then
    echo "GATE ARM CONTRACT: gate_arm '$arm' called before gate_arms_init." >&2
    ZS_GATE_ARMS_REFUSALS=$((ZS_GATE_ARMS_REFUSALS + 1))
    return 1
  fi

  if [ -z "$arm" ]; then
    echo "GATE ARM CONTRACT [$ZS_GATE_ARMS_ID]: an arm was declared with no name." >&2
    ZS_GATE_ARMS_REFUSALS=$((ZS_GATE_ARMS_REFUSALS + 1))
    return 1
  fi

  # A repeated arm id lets one arm's count vouch for another's. That is not
  # hypothetical for a file whose arms are written by copy-paste.
  case " $ZS_GATE_ARMS_SEEN " in
    *" $arm "*)
      echo "GATE ARM CONTRACT [$ZS_GATE_ARMS_ID]: arm '$arm' declared twice." >&2
      echo "  Two arms sharing an id means one of them is reporting the other's" >&2
      echo "  count, so neither number can be trusted. Give them distinct ids." >&2
      ZS_GATE_ARMS_REFUSALS=$((ZS_GATE_ARMS_REFUSALS + 1))
      return 1
      ;;
  esac
  ZS_GATE_ARMS_SEEN="$ZS_GATE_ARMS_SEEN $arm"
  ZS_GATE_ARMS_COUNT=$((ZS_GATE_ARMS_COUNT + 1))

  if ! _gate_arms_is_uint "$examined"; then
    echo "GATE ARM CONTRACT [$ZS_GATE_ARMS_ID/$arm]: examined='$examined' is not a" >&2
    echo "  non-negative integer. The command that produced it failed, or it" >&2
    echo "  produced text. Either way this arm has no count and cannot pass." >&2
    ZS_GATE_ARMS_REFUSALS=$((ZS_GATE_ARMS_REFUSALS + 1))
    return 1
  fi

  # A floor of zero is a DECLARED VACUITY: it says out loud that this arm may
  # rule on nothing and still pass, which is the defect wearing the uniform of
  # the fix. `tests/gate_arm_census.sh` refuses a literal 0 in the source as
  # well, so this cannot be smuggled past by a gate CI happens not to run.
  if ! _gate_arms_is_uint "$floor" || [ "$floor" -lt 1 ]; then
    echo "GATE ARM CONTRACT [$ZS_GATE_ARMS_ID/$arm]: floor='$floor' must be an" >&2
    echo "  integer of at least 1. A floor of 0 permits an arm that examines" >&2
    echo "  nothing to report success, which is what this contract exists to" >&2
    echo "  stop. If the arm genuinely has nothing to enumerate, it is not an" >&2
    echo "  arm - delete it." >&2
    ZS_GATE_ARMS_REFUSALS=$((ZS_GATE_ARMS_REFUSALS + 1))
    return 1
  fi

  echo "zsgate-arm gate=$ZS_GATE_ARMS_ID arm=$arm examined=$examined floor=$floor"

  if [ "$examined" -lt "$floor" ]; then
    {
      echo "GATE ARM EXAMINED TOO LITTLE: $ZS_GATE_ARMS_ID/$arm ruled on" \
           "$examined item(s), floor $floor."
      echo "  THIS IS NOT A PASS AND IT IS NOT A FINDING ABOUT THE TREE. The arm"
      echo "  had (almost) nothing to rule on, so its clean result says nothing:"
      echo "  a check that examines nothing and a clean tree print the same"
      echo "  thing. Something the arm enumerates stopped matching - a renamed"
      echo "  symbol, a moved file, a filter that now excludes everything."
      echo "  Fix the enumeration. Do not lower the floor."
    } >&2
    ZS_GATE_ARMS_REFUSALS=$((ZS_GATE_ARMS_REFUSALS + 1))
    return 1
  fi
  return 0
}

# gate_arms_finish
#
# Prints the trailer and returns non-zero if any arm refused, or if the gate
# declared no arms at all. THE ZERO-ARM CASE IS THE POINT: a gate whose arms
# were all skipped by a `case` that stopped matching runs to completion and
# exits 0 with nothing to show for it, which is indistinguishable from a clean
# run unless someone counts. Here, it is a refusal.
gate_arms_finish() {
  if [ "$ZS_GATE_ARMS_STARTED" -ne 1 ]; then
    echo "GATE ARM CONTRACT: gate_arms_finish called before gate_arms_init." >&2
    return 1
  fi
  if [ "$ZS_GATE_ARMS_COUNT" -eq 0 ]; then
    echo "GATE ARM CONTRACT [$ZS_GATE_ARMS_ID]: the gate declared ZERO arms." >&2
    echo "  It ran to the end having ruled on nothing. That is a refusal, not" >&2
    echo "  a pass." >&2
    return 1
  fi
  echo "zsgate-arms gate=$ZS_GATE_ARMS_ID arms=$ZS_GATE_ARMS_COUNT refusals=$ZS_GATE_ARMS_REFUSALS"
  [ "$ZS_GATE_ARMS_REFUSALS" -eq 0 ]
}
