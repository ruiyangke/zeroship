#!/usr/bin/env bash
# Every script CI invokes BY BARE PATH must be executable in the index.
#
# WHY THIS EXISTS. On 2026-08-09 commit e2d0cba4c ("share the stale-binary check
# across all four dev-vs-deployed scripts") changed three harnesses from 100755
# to 100644. Nothing in that commit was about file modes. The next day
# 735ac2f30 wired two of them into CI by bare path:
#
#     tests/e2e_dev_vs_deployed_kv.sh 2>&1 | tee "$RUNNER_TEMP/dvd-kv.log"
#
# A bare invocation of a non-executable file is exit 126, so from the moment
# they were gated those two steps could not run a single assertion. Reproduced
# locally in CI's exact shape (bash -e, pipefail, bare path):
#
#     bash: line 1: tests/e2e_dev_vs_deployed_kv.sh: Permission denied
#     rc=126
#
# Restored in ac3bfd2cf. This gate is what stops the next mode change that
# rides along in a commit about something else.
#
# WHAT IT DOES NOT CHECK, deliberately. Not "every .sh is executable" - that is
# false. Most of tests/lib/ is SOURCED and correctly non-executable; sourcing
# needs the read bit, not the exec bit, so a blanket rule would demand the wrong
# thing on those files and teach people to add exec bits that mean nothing.
#
# This paragraph used to NAME the sourced files. The list went stale in both
# directions - it named `skip_census.sh`, deleted with the skip protocol, and
# had never grown to cover the sourced libraries added since - which is the
# reason it is now a property. Read the modes if you want the set:
#   ls -l tests/lib/*.sh
#
# The invariant is narrower and is the one that actually broke: if CI runs it as
# a COMMAND, it must be executable. Files that CI sources, or invokes behind an
# interpreter, are outside this by construction and are not inspected.
#
# NOTE that tests/lib/ is NOT a reliable "library" marker: CI invokes
# tests/lib/dev_ready_selftest.sh bare. Directory is not the discriminator;
# invocation form is. I generalised the other way first and it was wrong.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init ci_invocable

WF="$ROOT/.github/workflows"
fail=0
checked=0

[ -d "$WF" ] || { echo "FAIL: no $WF; this gate derives its list from the workflows and would" >&2
                  echo "      otherwise inspect nothing and exit 0." >&2; exit 1; }

# A BARE INVOCATION is a command whose FIRST token is a path ending in .sh.
# Comments are dropped and the `run:` key is stripped, so a run line naming a
# script is seen as a command. Anything whose first token is an interpreter or
# `source`/`.` is skipped, because those do not need the exec bit.
while IFS= read -r cmd; do
  case "$cmd" in
    ''|'#'*) continue ;;
  esac
  first="${cmd%% *}"
  # Must look like a PATH, not a flag that happens to end in .sh. The first
  # draft of this gate reported two failures on a clean tree, both
  # `--exclude=source_citation_gate.sh` - a grep flag sitting as the first
  # token of a continuation line. Requiring a leading non-dash and an embedded
  # slash removes that class; every real invocation here is under tests/, and a
  # repo-root script would still carry a leading dot-slash, which has a slash.
  case "$first" in
    -*)      continue ;;
    */*.sh)  ;;
    *)       continue ;;
  esac
  checked=$((checked + 1))
  mode="$(cd "$ROOT" && git ls-files -s -- "$first" 2>/dev/null | awk '{print $1}')"
  if [ -z "$mode" ]; then
    echo "FAIL: CI invokes '$first' as a command, but it is not a tracked file."
    fail=1
  elif [ "$mode" != "100755" ]; then
    echo "FAIL: CI invokes '$first' as a command, but it is mode $mode."
    echo "      A bare invocation of a non-executable file is exit 126, so this"
    echo "      step cannot run. Fix with: chmod +x $first"
    fail=1
  fi
done < <(
  sed -E 's/[[:space:]]*#.*$//; s/^[[:space:]]*//; s/^run:[[:space:]]*//; s/^-[[:space:]]+//' \
      "$WF"/*.yml 2>/dev/null
)

# Anti-hollow floor. If the extraction stops matching - the workflow moves, the
# indentation changes, `run:` gains a block scalar this sed does not strip -
# this gate inspects nothing and exits 0 looking exactly like a clean run.
# MEASURED 2026-08-20: 62 bare invocations across .github/workflows/*.yml.
# Floor 10 is reused unchanged from the hand-rolled MIN check this replaces -
# well under 62, far above the single-digit count a broken extractor produces.
echo "bare CI script invocations checked: $checked (floor 10)"
if ! gate_arm bare_invocations "$checked" 10; then
  echo "FAIL: found only $checked bare invocation(s), fewer than the floor expected." >&2
  echo "      CI does not stop invoking scripts by accident: the extractor is" >&2
  echo "      broken. Fix the extractor, do not lower the floor." >&2
  fail=1
fi

gate_arms_finish || fail=1
[ "$fail" -eq 0 ] || { echo "CI INVOCABLE GATE: FAILED" >&2; exit 1; }
echo "CI INVOCABLE GATE: passed"
