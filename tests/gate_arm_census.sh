#!/usr/bin/env bash
# ============================================================================
# THE META-GATE: every gate must be able to say how much it examined.
#
# WHAT WENT WRONG, and why a meta-gate rather than more gates. Four repository
# gates were found vacuous within a few hours on 2026-08-20, each by a human
# reading rather than by any test:
#
#   ws_subscription_stub_gate.sh   arm 1 watched ONE identifier that the commit
#                                  it was written for had just deleted. It
#                                  examined 0 names and printed green, while
#                                  three live phantom identifiers sat in the
#                                  file it guards.
#   skip_marker_gate.sh            8 raw hits, 8 excused by its own allowlist,
#                                  0 ruled on. Green. (That gate is DELETED -
#                                  the skip protocol it policed no longer
#                                  exists. The failure it demonstrates does not
#                                  depend on the gate still being here.)
#   deploy_scripts_gate.sh         the argv scan had one pre-filter row, on the
#                                  single service the filter excludes. 0
#                                  examined. Green.
#   compose_secret_strength_gate.sh derived its rule set by regexing another
#                                  file's refusal MESSAGE; the message improved
#                                  and the rule set went to 0. ITS ANTI-VACUITY
#                                  GUARD FIRED, so it went RED rather than
#                                  falsely green - the counter-example that
#                                  proves the fix, and the only reason anyone
#                                  went looking for the other three.
#
# One shape: A CHECK THAT EXAMINES NOTHING AND A CLEAN TREE PRINT THE SAME
# THING. So every gate declares, per arm, the number of items THAT ARM RULED ON
# and a floor that number must clear (tests/lib/gate_arms.sh). This gate rules
# on whether they do.
#
# WHY THIS IS NOT ITSELF A CENSUS. The obvious meta-gate holds a table of
# expected per-gate counts and fails when one drifts. That table BECOMES THE
# DEFECT IT IS FIXING: it goes stale, and four gates in this repo went red in
# one week purely because a new crate landed and each held a list a new crate is
# by construction absent from. So the floor for each arm lives IN THAT ARM'S
# GATE, beside the code that produces the number. This script checks the
# PROPERTY, never the VALUES. There is exactly ONE number here - the enumeration
# floor - and it counts gate FILES, which are added and deleted deliberately.
#
# TWO MODES, answering two different questions:
#
#   <tests-dir>                STATIC. Does every gate under <tests-dir>
#                              PARTICIPATE in the contract? A property of the
#                              source; costs milliseconds; this is what CI runs.
#                              It catches the gate that never declared an arm.
#   <tests-dir> --run G...     DYNAMIC. Additionally execute the named gates and
#                              rule on the counts they emit. This catches an arm
#                              whose declaration sits behind a branch that
#                              stopped being taken, and an arm whose enumeration
#                              collapsed. The gates are NAMED ARGUMENTS rather
#                              than a list held here: a built-in "these gates are
#                              cheap enough to run" table would be a census.
#
# Paths are ARGUMENTS, never environment variables: a gate whose target depends
# on how the process was launched cannot be reasoned about from its invocation.
#
# WHAT THIS MISSES, stated so nobody reads it as complete:
#   - whether an arm's declared count is the RIGHT count. An arm that declares
#     its pre-filter total instead of the number it ruled on passes here, and
#     that is exactly the skip_marker_gate failure.
#   - whether a floor is well chosen. A floor of 1 on an arm that should see 400
#     passes here.
#   - the static mode is static. An arm whose declaration sits behind a `case`
#     that stopped matching never executes; --run closes that for the gates it
#     is given, and CI closes it for the rest by running each gate as its own
#     step, where gate_arms_finish refuses on zero declared arms.
#
# THIS WAS 959 LINES OF RUST (crates/zeroship-gatekit) until 2026-08-21. It read
# shell scripts to enforce a shell convention, and the crate existed for nothing
# else once its five compose gates were deleted.
# ============================================================================
set -uo pipefail

usage() {
  echo "usage: $0 <tests-dir> [--run <gate.sh>...]" >&2
}

# THE ONLY NUMBER HERE, and it counts FILES, not findings.
#
# THE LADDER OF PAST READINGS THAT STOOD HERE IS DELETED, and its own state is
# the argument. It recorded every value this pin had held and the gate that
# moved it, each entry written by hand by whoever noticed - and its most recent
# entry, dated four days before the pin below last moved, was more than ten
# behind the pin it was annotating. A history of a number, maintained by the
# same people who forget to maintain the number, rots faster than the number
# does and reads with more authority.
#
# What that history was really saying is kept, because it justifies the equality
# check further down: this pin has repeatedly been found sitting BEHIND the
# tree, back when it was a lower bound and every stale reading passed.
#
# Do not restate the count in prose. Re-measure it:
#   find tests -maxdepth 1 -name '*_gate.sh' | wc -l
#
# Gates are added and deleted by hand, so a drop is a decision somebody made and
# must be recorded here in the same commit, not an accident to be absorbed. Set
# at the observed count on purpose: a slack floor here would let the glob
# half-break unnoticed, which is the precise failure this script exists to catch
# one level down. RAISE THIS WHEN YOU ADD A GATE - leaving it behind the real
# count is how a floor stops meaning anything without ever going red.
#
# THIS IS THE ONLY EXACT PIN ON THIS GLOB. `tests/ci_wiring_gate.sh` enumerates
# the identical population and deliberately takes a SLACK floor instead, so
# adding a gate moves one number and not two - the second of two pins is the
# one that gets forgotten, and a forgotten pin is the stale census both scripts
# warn about.
#
# THE PIN IS NOW EXACT, AND THE DRIFT IS WHY. This was a `-lt` floor, and the
# comment it replaces had grown into a tally of the times it went stale - each
# entry added by the person who noticed, none of them by the mechanism. A
# lower bound cannot notice that it is behind: every stale reading passed, which
# is the failure signature of a pin nobody is forced to move.
#
# So the check below is EQUALITY, and a mismatch prints the number to write. A
# gate added without touching this line now fails loudly and tells you the
# one-line fix, instead of leaving a pin that silently means less every time the
# directory grows. Re-measure, never reason:
#   find tests -maxdepth 1 -name '*_gate.sh' | wc -l
#
# The count itself is deliberately the ONLY magnitude here. The history of how
# often it drifted was prose that had to be maintained by hand and rotted the
# same way the pin did.
GATE_FILE_COUNT=56

DIR=""
RUN=()
if [ "$#" -lt 1 ]; then usage; exit 1; fi
DIR="$1"; shift
if [ "$#" -gt 0 ]; then
  if [ "$1" != "--run" ]; then usage; exit 1; fi
  shift
  if [ "$#" -lt 1 ]; then
    echo "  x REFUSED: --run needs at least one gate script." >&2
    exit 1
  fi
  RUN=("$@")
fi
[ -d "$DIR" ] || { echo "  x REFUSED: cannot read $DIR" >&2; exit 1; }

CHECKS=0
FAILURES=0
REFUSAL=""

pass() { CHECKS=$((CHECKS + 1)); echo "  ok   $1"; }
note_fail() { CHECKS=$((CHECKS + 1)); FAILURES=$((FAILURES + 1)); echo "  FAIL $1"; }
refuse() { [ -n "$REFUSAL" ] || REFUSAL="$1"; }

# ---------------------------------------------------------------------------
# Extract the `gate_arm` CALLS from a gate's source.
#
# A call counts only when `gate_arm` is the command being run: first token of
# the line, or immediately behind `if`, `elif`, `!`, `&&`, `||` or `then`. A
# gate's own prose about gate_arm is not a call, and neither is `gate_arms_init`
# or `gate_arms_finish`, which share the prefix.
#
# Emits one `<arm-id><TAB><floor>` row per call; the floor is empty when it is
# written as a variable this cannot evaluate. Shell punctuation is stripped off
# the floor token FIRST: `gate_arm x "$n" 3 || exit 1` and `... 3; then` both put
# it against the number, and reading that as "a variable I cannot evaluate"
# would silently stop the floor-of-zero check applying to every call written in
# the `if !` form - a filter that excuses exactly the cases it was built to rule
# on, which is this repo's founding bug.
# ---------------------------------------------------------------------------
# The gate's source with its FULL-LINE comments removed.
#
# Every participation check below reads this rather than the file, for the
# reason arm_calls already encodes one function down: a gate's own prose about
# gate_arms is not a call. Measured 2026-09-04 against all 41 gates, deleting
# the real `. "$ROOT/tests/lib/gate_arms.sh"` line from each in turn: the
# whole-file substring this replaces read 40 of the 41 mutants as CLEAN, because
# `# shellcheck source=tests/lib/gate_arms.sh` sits directly above the real one.
# The init and finish checks were satisfied by a comment in 1 and 2 gates. On
# the comment-stripped text, 0 of the 123 mutants get through.
#
# WHAT IT STILL CANNOT DO: a TRAILING comment on a code line is code to this
# filter, so `foo # gate_arms_init` would satisfy the init check. Stripping
# those needs a shell tokenizer to know a `#` inside quotes is not a comment,
# and a wrong strip would fail correct gates. Full-line comments are where the
# headers live and are the whole of the measured gap.
code_lines() { grep -vE '^[[:space:]]*#' "$1"; }

arm_calls() {
  awk '
    {
      line = $0
      sub(/^[ \t]+/, "", line)
      if (line ~ /^#/) next
      changed = 1
      while (changed) {
        changed = 0
        split("if elif ! && || then", pre, " ")
        for (i = 1; i <= 6; i++) {
          p = pre[i] " "
          if (index(line, p) == 1) {
            line = substr(line, length(p) + 1)
            sub(/^[ \t]+/, "", line)
            changed = 1
          }
        }
      }
      if (index(line, "gate_arm ") != 1) next
      rest = substr(line, 10)
      n = split(rest, f, /[ \t]+/)
      if (n < 1) next
      arm = f[1]
      gsub(/^["\047]+|["\047]+$/, "", arm)
      if (arm == "") next
      floor = ""
      if (n >= 3) {
        t = f[3]
        gsub(/["\047;]/, "", t)
        if (t ~ /^-?[0-9]+$/) floor = t
      }
      print arm "\t" floor
    }
  ' "$1"
}

# ---------------------------------------------------------------------------
# STATIC: does every gate participate?
# ---------------------------------------------------------------------------
GATES=()
while IFS= read -r g; do
  [ -n "$g" ] && GATES+=("$g")
done < <(find "$DIR" -maxdepth 1 -name '*_gate.sh' -type f | LC_ALL=C sort)

echo "  gates enumerated: ${#GATES[@]} under $DIR (pinned $GATE_FILE_COUNT)"
if [ "${#GATES[@]}" -ne "$GATE_FILE_COUNT" ]; then
  refuse "enumerated ${#GATES[@]} gate script(s) under $DIR, but this script pins \
$GATE_FILE_COUNT. FEWER means the glob stopped matching or gates were deleted - a meta-gate \
that checks zero gates is the joke version of this check, so that is a refusal and not a \
pass. MORE means a gate landed without moving the pin, which is how this line went stale \
repeatedly while a lower bound kept passing. Either way the fix is one line: set \
GATE_FILE_COUNT=${#GATES[@]} in the same commit that changed the set."
fi

if [ -z "$REFUSAL" ]; then
  for path in "${GATES[@]}"; do
    name="$(basename "$path")"
    src="$(code_lines "$path")"
    calls="$(arm_calls "$path")"
    n_calls=0
    [ -n "$calls" ] && n_calls="$(printf '%s\n' "$calls" | grep -c .)"

    if [ "$n_calls" -eq 0 ]; then
      note_fail "$name: declares no arms. Every gate must state, per arm, how many items that arm ruled on and the floor that number must clear - see tests/lib/gate_arms.sh. Without it, an arm whose enumeration collapses to zero prints the same green as a clean tree."
      continue
    fi

    problems=""
    # The SOURCE check wants a `.`/`source` COMMAND, not the path appearing
    # somewhere. Every gate carries `# shellcheck source=tests/lib/gate_arms.sh`
    # directly above the real line, so a bare substring is satisfied by the
    # directive alone - see the header note on what this used to miss.
    case "$src" in
      *". "*"lib/gate_arms.sh"*|*"source "*"lib/gate_arms.sh"*) ;;
      *) problems="does not source tests/lib/gate_arms.sh" ;;
    esac
    case "$src" in *"gate_arms_init"*) ;; *) problems="${problems:+$problems; }never calls gate_arms_init" ;; esac
    case "$src" in
      *"gate_arms_finish"*) ;;
      *) problems="${problems:+$problems; }never calls gate_arms_finish, so its arms' refusals reach no exit code" ;;
    esac

    # A floor of zero is a DECLARED VACUITY: it says out loud that this arm may
    # rule on nothing and still pass. tests/lib/gate_arms.sh refuses it at
    # runtime too; checking it here as well means a gate CI happens not to run
    # cannot carry one.
    while IFS="$(printf '\t')" read -r arm floor; do
      [ -n "$floor" ] || continue
      if [ "$floor" -lt 1 ]; then
        problems="${problems:+$problems; }arm '$arm' declares a floor of $floor; a floor under 1 permits an arm that examines nothing to pass"
      fi
    done <<< "$calls"

    # A repeated arm id means one arm is reporting another's count.
    #
    # LITERAL IDS ONLY. A gate that loops over subjects writes
    # `gate_arm "$subject_arm" ...`, and two such calls in a helper are two
    # DIFFERENT arms at runtime with the same spelling in the source. Reporting
    # that as a duplicate would fail a correct gate, and a check that cries wolf
    # gets weakened until it means nothing. Runtime duplicates are caught where
    # the values exist: gate_arm refuses an id it has already seen in that run.
    dupes="$(printf '%s\n' "$calls" | cut -f1 | grep -v '\$' | sort | uniq -d)"
    while IFS= read -r d; do
      [ -n "$d" ] || continue
      problems="${problems:+$problems; }arm '$d' is declared twice; two arms sharing an id means one is vouching for the other's count"
    done <<< "$dupes"

    if [ -z "$problems" ]; then
      pass "$name: $n_calls arm(s) declared with a floor each"
    else
      note_fail "$name: $problems"
    fi
  done
fi

# ---------------------------------------------------------------------------
# DYNAMIC: did the arms a gate RAN have anything to rule on?
#
# The gate's own exit code is deliberately IGNORED. This mode asks one question,
# and a gate that is red about the tree still answers it. Conflating the two
# would report every ordinary gate failure a second time.
# ---------------------------------------------------------------------------
for path in "${RUN[@]:-}"; do
  [ -n "$path" ] || continue
  name="$(basename "$path")"
  out="$(bash "$path" 2>&1)"
  emitted="$(printf '%s\n' "$out" | grep '^[[:space:]]*zsgate-arm ' || true)"
  if [ -z "$emitted" ]; then
    refuse "$name emitted no zsgate-arm line. Either it does not participate in the arm contract, or the run did not reach any arm - both mean this tells you nothing about the tree."
    continue
  fi
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    g=""; a=""; e=""; f=""
    for field in $line; do
      case "$field" in
        gate=*) g="${field#gate=}" ;;
        arm=*) a="${field#arm=}" ;;
        examined=*) e="${field#examined=}" ;;
        floor=*) f="${field#floor=}" ;;
      esac
    done
    case "$g$a$e$f" in "") continue ;; esac
    [ -n "$g" ] && [ -n "$a" ] && [ -n "$e" ] && [ -n "$f" ] || continue
    case "$e$f" in *[!0-9-]*) continue ;; esac
    if [ "$e" -lt "$f" ]; then
      note_fail "$g/$a ruled on $e item(s), floor $f. The arm's enumeration collapsed; its clean result says nothing."
    else
      pass "$g/$a ruled on $e item(s), floor $f"
    fi
  done <<< "$emitted"
done

# ---------------------------------------------------------------------------
# THE VERDICT. Three outcomes, not two: green, red, and REFUSED. A gate that
# checked nothing must not report success, so zero checks is a refusal and there
# is no way to ask for it to be anything else.
# ---------------------------------------------------------------------------
echo "  $((CHECKS - FAILURES)) passed, $FAILURES failed, $CHECKS ran"
if [ -n "$REFUSAL" ]; then
  echo "  x REFUSED: $REFUSAL" >&2
  exit 1
fi
if [ "$CHECKS" -eq 0 ]; then
  echo "  x REFUSED: this ran and checked nothing; a gate that checks nothing must not report success" >&2
  exit 1
fi
if [ "$FAILURES" -ne 0 ]; then
  echo "GATE ARM CENSUS: FAILED" >&2
  exit 1
fi
echo "GATE ARM CENSUS: passed"
