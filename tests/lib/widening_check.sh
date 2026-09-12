#!/usr/bin/env bash
# Did this working tree WIDEN a visibility? The half of audit 1 the compiler
# cannot do.
#
# WHY IT IS NEEDED. Phase 0.5 audit 1 narrows candidates and lets the compiler
# name what breaks. That loop is ONE-SIDED: it ends when the build is green,
# and green proves only that nothing was narrowed too far. An item left WIDER
# than it needs to be compiles perfectly, so the restore half of the loop has
# no oracle at all.
#
# TWO DISTINCT MECHANISMS HAVE ALREADY PRODUCED THIS, both from restoring by
# NAME what the compiler demanded, when the name matched more than one
# declaration:
#
#   1. CFG-FORKED TWINS (crud/unmask.rs, 2026-09-02). `parse_args` and
#      `parse_bulk_args` each exist twice - `pub` under
#      #[cfg(any(test, feature="test-helpers"))] and `pub(crate)` under the
#      negation. The restore widened the PRODUCTION arm of the DB-3 argument
#      parser, and all four feature configs stayed green.
#
#   2. SAME NAME, DIFFERENT IMPL (backend/sqlite/session.rs, same day).
#      `query` and `query_typed` are declared on two impls: pub(crate) at
#      :852/:875 and pub at :1297/:1305. The compiler demanded the second pair;
#      the restore widened the first.
#
# WHY NOT COMPARE NAME SETS. The first version of this did, and false-positived
# on case 2: `query` is in "was pub(crate)" and in "is pub now" for reasons
# that have nothing to do with each other. Names are not identities here. This
# compares the DIFF, pairing each removed line with the added line that
# replaced it, so a declaration is only judged against itself.
#
# NOT A GATE. It rules on uncommitted work against a ref, so it has nothing to
# say on a clean tree - which is exactly the state CI runs in. It sits beside
# pub_fence_census.sh as a measurement you run DURING a pass.
#
# Usage:  tests/lib/widening_check.sh [ref] [-- path...]
#         tests/lib/widening_check.sh                    # working tree vs HEAD
#         tests/lib/widening_check.sh HEAD~1             # last commit included
#         tests/lib/widening_check.sh HEAD -- crates/zeroship-data-v8
set -uo pipefail
cd "$(dirname "$0")/../.."

REF=HEAD
if [ $# -gt 0 ] && [ "$1" != "--" ]; then REF="$1"; shift; fi
[ "${1:-}" = "--" ] && shift

diff_out=$(git diff -U0 "$REF" -- "$@" 2>/dev/null)
if [ -z "$diff_out" ]; then
  echo "widening_check: no diff against $REF${*:+ for $*}."
  echo "  Nothing to rule on. On a clean tree this is not a pass - it means"
  echo "  the pass you wanted to check is already committed. Pass a ref."
  exit 0
fi

# Pair each removed line with the line that replaced it. `git diff -U0` emits a
# hunk's removals then its additions, so an N-line hunk pairs positionally.
report=$(printf '%s\n' "$diff_out" | awk '
  /^\+\+\+ b\// { file = substr($0, 7); nm = 0; na = 0; next }
  /^@@/ {
    for (i = 1; i <= nm; i++) if (i <= na) {
      if (minus[i] ~ /^[[:space:]]*pub\(crate\) [a-z]/ && plus[i] ~ /^[[:space:]]*pub [a-z]/) {
        sub(/^[[:space:]]+/, "", plus[i])
        printf "  %s\n      was: %s\n      now: %s\n", file, trim(minus[i]), plus[i]
      }
    }
    nm = 0; na = 0; next
  }
  /^-/ && !/^---/ { minus[++nm] = substr($0, 2); next }
  /^\+/ && !/^\+\+\+/ { plus[++na] = substr($0, 2); next }
  END {
    for (i = 1; i <= nm; i++) if (i <= na) {
      if (minus[i] ~ /^[[:space:]]*pub\(crate\) [a-z]/ && plus[i] ~ /^[[:space:]]*pub [a-z]/) {
        sub(/^[[:space:]]+/, "", plus[i])
        printf "  %s\n      was: %s\n      now: %s\n", file, trim(minus[i]), plus[i]
      }
    }
  }
  function trim(s) { sub(/^[[:space:]]+/, "", s); return s }
')

changed=$(printf '%s\n' "$diff_out" | grep -c '^+++ b/')
echo "widening_check: $changed file(s) changed against $REF"

if [ -n "$report" ]; then
  echo
  echo "WIDENED - these lines went pub(crate) -> pub:"
  printf '%s\n' "$report"
  echo
  echo "  A green build does not rule on this. Decide per item whether the"
  echo "  widening was intended; if it came from restoring by name, check"
  echo "  whether the name has a cfg-forked twin or a same-named sibling on"
  echo "  another impl, and put the other one back."
  exit 1
fi

echo "ok  no line went pub(crate) -> pub"
exit 0
