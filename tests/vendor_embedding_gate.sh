#!/usr/bin/env bash
# Decision 5: the core and every other non-vendor crate NEVER embed a vendor.
#
# OPERATOR DECISION, 2026-09-01. Not "should avoid" - never. This gate exists
# because the mechanism the design relies on cannot enforce it, and because the
# crate split will otherwise LAUNDER the violations rather than surface them.
#
# WHY CARGO CANNOT DO THIS FOR US, which is the whole point:
#
#   The manifest fence (a crate that does not declare `compio-postgres` cannot
#   write `compio_postgres::Error`, E0433) stops the SPELLING of a vendor path.
#   It does not stop POSSESSION of a vendor VALUE arriving by public field or by
#   type inference - no token appears at the call site to be fenced. That is
#   exactly how `SchemaError.source: compio_postgres::Error` (#97) got in.
#
#   Worse for the split: `git mv`-ing a file that NAMES a vendor into a new crate
#   does not fail. Cargo simply wants the dependency declared, you declare it,
#   and the build goes GREEN having made the violation permanent and official.
#   Structural edges fail loudly; vendor embedding fails silently. So this gate
#   must run BEFORE each move, not after.
#
# WHAT IT RULES ON. Two arms, deliberately different questions:
#
#   1. NAMED   - a non-vendor production file writes `compio_postgres` or
#                `rusqlite`. Cheap, exact, and the only arm that can be complete.
#   2. BASELINE- every entry in the known-violation list below is still real. A
#                gate whose excuse list outlives the thing it excuses is how a
#                census goes stale, and this project has four recorded instances.
#
# The value-flow question - who HOLDS a vendor value without naming it - is
# genuinely harder and lives in `tests/lib/vendor_value_flow_census.sh`, which
# this gate deliberately does not duplicate. Run it too; it is a FLOOR, not a
# verdict.
#
# KNOWN LIMIT, and it bit this gate on its first run. A vendor token inside a
# STRING LITERAL is counted as an embedding. `exec.rs:76` is a `hint:` message
# reading "The SQLite backend does not expose a compio_postgres::Pool...", which
# is prose, not a type reference. The comment filter below strips `//` lines but
# nothing strips string contents, and telling the two apart needs a parser rather
# than a regex. The same class produced two false hits when the engine's raw-SQL
# surface was first measured (`crud/system_fields_pass.rs:399`, `:428` are error
# messages beginning "UPDATE patch attempted to overwrite..."). So the per-file
# COUNTS here are an upper bound; the file LIST is exact, because every file
# named also has at least one real mention. Open the lines before acting on a
# count.
#
# Run the detector's own positive/control pair: this script --self-test.

set -uo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=tests/lib/gate_arms.sh
. "$(dirname "$0")/lib/gate_arms.sh"
gate_arms_init vendor_embedding

PASS=0
FAIL=0
ok()  { printf '  ok   %s\n' "$1"; PASS=$((PASS + 1)); }
bad() { printf '  FAIL %s\n' "$1"; FAIL=$((FAIL + 1)); }

VENDORS='compio_postgres|rusqlite'

# ROOTS. Decision 5 says "the core and every OTHER non-vendor crate", and the
# first version of this gate scanned exactly one crate - which reported green
# while `zeroship-schema/src/error.rs:31` held `pub source: compio_postgres::Error`
# one crate BELOW the one being scanned. That is #97, and it is the floor the
# proposal places `data-core` on (#101), so a vendor in it is the violation at
# its most load-bearing point.
#
# Twenty workspace crates name a vendor. MOST OF THEM LEGITIMATELY DO - the
# migrate dialect backends are dialect implementations, and the services talk to
# their own driver. Scanning all twenty would report noise and train people to
# ignore this gate. The roots below are the crates the data-plane split requires
# to be vendor-free, and adding one is a design decision, not housekeeping.
ROOTS="
crates/zeroship-plugin-db/src
crates/zeroship-schema/src
crates/zeroship-data-core/src
"

# Files that are ALLOWED to name a vendor: the vendor tiers themselves, plus the
# CDC tier, which the target explicitly gives its own `compio-postgres`
# dependency (it is a service peer of the migration server, not a data-plane
# layer). Everything else in the crate is non-vendor and is ruled on.
#
# The KEY for a file is `<crate>/<path after src/>` - e.g.
# `zeroship-plugin-db/error.rs`. Qualifying by crate is not decoration: with two
# roots, `error.rs` names a file in BOTH, and an unqualified key would let one
# crate's baseline entry silently excuse the other's violation.
file_key() {
  local p="${1#crates/}"
  printf '%s/%s\n' "${p%%/src/*}" "${p#*/src/}"
}

is_vendor_tier() {
  case "$(file_key "$1")" in
    zeroship-plugin-db/backend/postgres.rs) return 0 ;;
    zeroship-plugin-db/backend/pg_*.rs|zeroship-plugin-db/backend/sqlite/*) return 0 ;;
    zeroship-plugin-db/replication.rs|zeroship-plugin-db/slot_reaper.rs) return 0 ;;
    zeroship-plugin-db/wal_consumer.rs|zeroship-plugin-db/change_stream_pg.rs) return 0 ;;
    *) return 1 ;;
  esac
}

# Map a baseline key back to a path, across roots.
key_to_path() {
  local crate="${1%%/*}" rest="${1#*/}"
  printf 'crates/%s/src/%s\n' "$crate" "$rest"
}

# --------------------------------------------------------------------------
# KNOWN VIOLATIONS, dated. This list may only SHRINK.
#
# Adding an entry is an operator decision and needs a reason on the line. Every
# entry is re-checked by arm 2: if the violation is gone, the gate FAILS and
# tells you to delete the line, so the list cannot quietly outlive the defect.
# --------------------------------------------------------------------------
# Measured at ea529977c: ELEVEN files, 21 production mentions. Decision 5's
# scope is an order of magnitude wider than #97 alone, which named exactly one
# public field. Every entry carries the task that DELETES it - an entry with no
# owner is a permanent exception, and this list does not have those.
#
# RE-MEASURED 2026-09-02 from this gate's own arm-2 output: SEVEN files, 11
# production mentions.
#
# The notes below CARRIED A PER-FILE COUNT COLUMN until 2026-09-02, and it had
# already rotted: `exec.rs` said 5 while the gate reported 1. The column is
# deleted rather than corrected. Arm 2 prints the live count for every entry on
# every run, so a number written here can only ever duplicate that output or
# contradict it - and a stale one reads exactly like a measured one. What a
# comment CAN say that the gate cannot is why the entry exists and what deletes
# it, so that is all these lines say now.
BASELINE_FILES="
zeroship-plugin-db/exec.rs
zeroship-plugin-db/backend/mod.rs
zeroship-plugin-db/backend/cancel.rs
zeroship-plugin-db/auth/bootstrap.rs
zeroship-plugin-db/context.rs
zeroship-plugin-db/lib.rs
zeroship-plugin-db/service.rs
"
# exec.rs                 Pool + Vec<Row> - the unsettled row vocabulary. Blocked on
#                         the neutral-row decision; see roled_rows in pg_autocommit.
# backend/mod.rs          BackendHandle names BOTH vendors, which is why this file
#                         has no tier at all. #119.
# backend/cancel.rs       CancelToken, Pool. Was transaction/cancel.rs; moved into
#                         the vendor tier by #122. See the note below on why a
#                         move does not clear an entry, only relocates it.
# auth/bootstrap.rs       session setup reaching the driver. Follows #110's cut.
# context.rs              holds a live Pool in a field. #100 - placement unsettled.
# drop_namespace.rs       ENTRY DELETED 2026-09-02, and NOT because the code
# backend/lock_guard.rs   changed. Each module is gated at its single
#                         declaration - lib.rs:259 and backend/mod.rs:82 - so
#                         neither is in a shipped binary, and neither was ever a
#                         file this gate should have ruled on.
#                         `module_is_test_gated` skips them in arm 1 now, which
#                         is what makes the entries wrong: an entry arm 1 cannot
#                         reach can never be RETIRED by fixing the file, only
#                         held green forever by arm 2. Both files still contain
#                         `use compio_postgres::...` - do not read either
#                         deletion as the coupling having gone. Arm 2 now
#                         refuses such an entry outright, so this pairing cannot
#                         be reintroduced by hand.
# lib.rs                  the thin adapter still links the driver. #109.
# service.rs              unjudged file; no destination decided yet.
#
# **THE ENTRIES RETIRE TWO DIFFERENT WAYS, and conflating them reads the list
# as more alarming than it is.** Three came off on 2026-09-02 the first way:
# transaction/driver.rs, transaction/mod.rs and transaction/cancel.rs, when #122
# separated the SC-1 protocol from the vendor lane. driver.rs and mod.rs stopped
# naming a vendor at all; cancel.rs MOVED, so its entry did not disappear, it
# became backend/cancel.rs above.
#
# The rest come off the second way: by the crate split itself. backend/mod.rs,
# backend/cancel.rs and backend/lock_guard.rs are already in the tier that is
# ALLOWED to name a driver - they are listed only because plugin-db is still one
# crate, and this gate's rule is about crates. They need no code move; they need
# `backend/` to become zeroship-data-postgres. Do not chase them as defects.

in_baseline() {
  printf '%s\n' "$BASELINE_FILES" | grep -qx -- "$1"
}

# A module can be gated where it is DECLARED rather than where it is defined,
# and the production-region filter below cannot see that: the attribute is in
# the PARENT file.
#
#     // lib.rs:259
#     #[cfg(any(test, feature = "test-helpers"))]
#     pub mod drop_namespace;
#
# `drop_namespace.rs` carries no cfg of its own, so this gate counted its
# `use compio_postgres::Pool` and carried a baseline entry for a module that is
# in no shipped binary. tests/lib/tier_signature_census.sh had the identical
# blind spot and was fixed the same day; this is the same rule, so the two
# instruments agree about which files exist to rule on.
#
# TWO THINGS MAKE IT CORRECT, and both were learned by getting them wrong in the
# census first:
#
#   * EVERY declaration must be gated, not merely one. This crate declares most
#     modules through a two-arm visibility ladder (`#[cfg(not(feature =
#     "test-helpers"))] pub(crate) mod exec;` plus its `pub` twin), so "any
#     gated declaration" would exclude nearly the whole crate.
#   * A `not(...)` wrapper is the SHIPPED arm and must be skipped before the
#     substring test - `"test-helpers"` CONTAINS `test`, and matching it
#     naively took the census to zero rows while printing that as calmly as a
#     real number.
module_is_test_gated() {   # $1 = a path under one of $ROOTS
  local file="$1" name decls gated
  name=$(basename "$file" .rs)
  if [ "$name" = "mod" ]; then
    name=$(basename "$(dirname "$file")")
  fi
  [ -z "$name" ] && return 1

  decls=0
  gated=0
  while IFS= read -r hit; do
    local decl_file decl_line prev i
    decl_file="${hit%%:*}"
    decl_line="${hit#*:}"
    decl_line="${decl_line%%:*}"
    decls=$((decls + 1))
    i=$((decl_line - 1))
    while [ "$i" -ge 1 ]; do
      prev=$(sed -n "${i}p" "$decl_file")
      case "$prev" in
        *"#[cfg("*)
          case "$prev" in
            *"not("*) break ;;
            *test*) gated=$((gated + 1)); break ;;
            *) break ;;
          esac
          ;;
        "#["*|*"//"*|"") i=$((i - 1)) ;;
        *) break ;;
      esac
    done
  done < <(grep -rn -E "^[[:space:]]*(pub([[:space:]]*\([^)]*\))?[[:space:]]+)?mod[[:space:]]+${name}[[:space:]]*;" $ROOTS 2>/dev/null)

  [ "$decls" -gt 0 ] && [ "$decls" -eq "$gated" ]
}

# --------------------------------------------------------------------------
# Arm 1: no non-vendor production file names a vendor.
# --------------------------------------------------------------------------
echo "== non-vendor files must not name a vendor =="

n_ruled=0
n_new=0
while IFS= read -r f; do
  is_vendor_tier "$f" && continue
  # A module that is not compiled into any shipped binary cannot embed a vendor
  # into one. Skipped BEFORE n_ruled so the arm's count stays honest: it reports
  # what it decided, and it decided nothing about this file.
  module_is_test_gated "$f" && continue
  rel="$(file_key "$f")"
  n_ruled=$((n_ruled + 1))

  # Production region only: everything before the file's first column-0
  # #[cfg(test)]. A vendor named inside a test module is not an embedding.
  # The production region ends at the file's test MODULE, not at its first
  # item-level `#[cfg(test)]`. Taking the first cfg of ANY kind is silently
  # defeated by one innocuous line: a `#[cfg(test)] use crate::x;` near the top
  # hides every vendor mention below it.
  #
  # Measured 2026-09-01 on exec.rs: two lines at the top took its five mentions
  # to zero and this arm went from naming them to printing `ok`. Only the
  # baseline-liveness arm noticed, and only because that file is IN the
  # baseline - a file outside it would be blinded in total silence. Every crate
  # move adds exactly such imports, so this had to be fixed before the moves.
  boundary=$(awk '
    /^#\[cfg\(test\)\]/ { candidate = NR; next }
    candidate && NF {
      if ($0 ~ /^(pub )?mod /) { print candidate; exit }
      candidate = 0
    }
  ' "$f")
  [ -n "$boundary" ] || boundary=999999
  hits=$(awk -v b="$boundary" -v pat="$VENDORS" \
           'NR < b && $0 ~ pat && $0 !~ /^[[:space:]]*\/\// { n++ } END { print n+0 }' "$f")
  [ "$hits" -eq 0 ] && continue

  if in_baseline "$rel"; then
    printf '  note %s names a vendor %d time(s) - KNOWN, in baseline\n' "$rel" "$hits"
    continue
  fi
  n_new=$((n_new + 1))
  bad "$rel names a vendor $hits time(s) in production and is NOT in the baseline"
  awk -v b="$boundary" -v pat="$VENDORS" \
      'NR < b && $0 ~ pat && $0 !~ /^[[:space:]]*\/\// { printf "       %d: %s\n", NR, $0 }' "$f" \
    | head -5
done < <(find $ROOTS -name '*.rs' 2>/dev/null | LC_ALL=C sort)

[ "$n_new" -eq 0 ] && ok "no new vendor embedding across $n_ruled non-vendor file(s)"

# FLOOR. 60+ non-vendor files today. Set well under it: deleting a few modules
# must not trip this, but a change that stops the walk finding files - a moved
# SRC root, a broken find - must.
if ! gate_arm named_vendor "$n_ruled" 25; then
  FAIL=$((FAIL + 1))
fi

# --------------------------------------------------------------------------
# Arm 2: every baseline entry is still a real violation.
# --------------------------------------------------------------------------
echo
echo "== baseline entries must still describe live violations =="

n_baseline=0
while IFS= read -r rel; do
  [ -n "$rel" ] || continue
  n_baseline=$((n_baseline + 1))
  f="$(key_to_path "$rel")"
  if [ ! -f "$f" ]; then
    bad "baseline names $rel, which does not exist - delete the entry"
    continue
  fi
  # An entry arm 1 SKIPS is unreachable, not satisfied. Arm 1 declines to rule
  # on a test-gated module, so nothing about that file can ever retire its
  # entry; the check below would hold it green off the file's own text forever,
  # and the two arms would disagree about which files exist to rule on. This is
  # not hypothetical - lock_guard.rs and drop_namespace.rs were both in that
  # state on 2026-09-02, and the gate was GREEN throughout, because each arm was
  # individually self-consistent. Keep the arms agreeing by construction.
  if module_is_test_gated "$f"; then
    bad "baseline names $rel, but it is test-gated at its declaration so arm 1 never rules on it - delete the entry"
    continue
  fi
  # The production region ends at the file's test MODULE, not at its first
  # item-level `#[cfg(test)]`. Taking the first cfg of ANY kind is silently
  # defeated by one innocuous line: a `#[cfg(test)] use crate::x;` near the top
  # hides every vendor mention below it.
  #
  # Measured 2026-09-01 on exec.rs: two lines at the top took its five mentions
  # to zero and this arm went from naming them to printing `ok`. Only the
  # baseline-liveness arm noticed, and only because that file is IN the
  # baseline - a file outside it would be blinded in total silence. Every crate
  # move adds exactly such imports, so this had to be fixed before the moves.
  boundary=$(awk '
    /^#\[cfg\(test\)\]/ { candidate = NR; next }
    candidate && NF {
      if ($0 ~ /^(pub )?mod /) { print candidate; exit }
      candidate = 0
    }
  ' "$f")
  [ -n "$boundary" ] || boundary=999999
  hits=$(awk -v b="$boundary" -v pat="$VENDORS" \
           'NR < b && $0 ~ pat && $0 !~ /^[[:space:]]*\/\// { n++ } END { print n+0 }' "$f")
  if [ "$hits" -eq 0 ]; then
    bad "baseline still excuses $rel, but it names no vendor any more - DELETE the entry"
  else
    ok "$rel still violates ($hits occurrence(s)) - entry earns its place"
  fi
done <<EOF
$(printf '%s\n' "$BASELINE_FILES" | grep -v '^[[:space:]]*$')
EOF

if ! gate_arm baseline_liveness "$n_baseline" 1; then
  FAIL=$((FAIL + 1))
fi

# --------------------------------------------------------------------------
# Self-test: prove the detector fires, and prove it does not fire on a control.
# --------------------------------------------------------------------------
if [ "${1:-}" = "--self-test" ]; then
  echo
  echo "== self-test =="
  probe=$(mktemp -d)
  trap 'rm -rf "$probe"' EXIT

  # POSITIVE: a non-vendor-shaped file naming a vendor in production.
  printf 'fn f(e: &compio_postgres::Error) {}\n' > "$probe/positive.rs"
  b=$(grep -c '^#\[cfg(test)\]' "$probe/positive.rs")
  h=$(awk -v pat="$VENDORS" '$0 ~ pat && $0 !~ /^[[:space:]]*\/\// { n++ } END { print n+0 }' "$probe/positive.rs")
  if [ "$h" -ge 1 ]; then ok "positive control: detector sees a production vendor mention"
  else bad "positive control FAILED: detector blind to an obvious mention"; fi

  # NEGATIVE: same token, but inside a test region and behind a comment. One
  # variable changed at a time is the whole point of a control.
  printf '#[cfg(test)]\nmod t { fn f(e: &compio_postgres::Error) {} }\n' > "$probe/negative.rs"
  nb=$(grep -n '^#\[cfg(test)\]' "$probe/negative.rs" | head -1 | cut -d: -f1)
  nh=$(awk -v b="$nb" -v pat="$VENDORS" \
         'NR < b && $0 ~ pat && $0 !~ /^[[:space:]]*\/\// { n++ } END { print n+0 }' "$probe/negative.rs")
  if [ "$nh" -eq 0 ]; then ok "negative control: a mention inside a test region is not an embedding"
  else bad "negative control FAILED: test-region mention counted as production"; fi

  printf '// compio_postgres::Error in a comment\n' > "$probe/comment.rs"
  ch=$(awk -v pat="$VENDORS" '$0 ~ pat && $0 !~ /^[[:space:]]*\/\// { n++ } END { print n+0 }' "$probe/comment.rs")
  if [ "$ch" -eq 0 ]; then ok "negative control: a mention in a comment is not an embedding"
  else bad "negative control FAILED: comment counted as production"; fi
fi

echo
gate_arms_finish || FAIL=$((FAIL + 1))

echo
if [ "$FAIL" -gt 0 ]; then
  printf 'vendor_embedding_gate: %d passed, %d FAILED\n' "$PASS" "$FAIL"
  exit 1
fi
printf 'vendor_embedding_gate: %d passed\n' "$PASS"
