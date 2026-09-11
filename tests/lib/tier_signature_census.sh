#!/usr/bin/env bash
set -uo pipefail

# shellcheck source=tests/lib/module_gating.sh
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/module_gating.sh"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# TWO SOURCE ROOTS SINCE 2026-09-03, for the reason spelled out at the head of
# tier_direction_census.sh: the ENGINE tier left `zeroship-data-v8/src` for
# `zeroship-data-orm/src`, and a census pinned to the first would rule on
# what stayed while printing a clean verdict about what went. A tier is not a
# crate; both trees are scanned as one region under one `tier()` map, so the
# arms below rule on the same files they ruled on before.
SRC_ROOTS=(
  "$ROOT/crates/zeroship-data-v8/src"
  "$ROOT/crates/zeroship-data-orm/src"
)
SHOW_ALL=0
TEST_REGION=0
case "${1:-}" in
  --all)   SHOW_ALL=1 ;;
  --tests) TEST_REGION=1 ;;
  "")      ;;
  *)       echo "tier_signature_census: unknown option '$1'" >&2; exit 2 ;;
esac

for _root in "${SRC_ROOTS[@]}"; do
  [ -d "$_root" ] || { echo "tier_signature_census: no such tree: $_root" >&2; exit 1; }
done

# Every `.rs` under every source root, as `<root>\t<./-relative path>`. The
# relative half is what `tier()` and `module_is_test_gated()` are keyed on; the
# root half is what the loop `cd`s into so a file's parent `mod` declaration
# resolves in its own crate.
all_sources() {
  local p
  for p in "${SRC_ROOTS[@]}"; do
    ( cd "$p" && find . -name '*.rs' | LC_ALL=C sort | sed "s|^|$p\t|" )
  done
}

# `plugin-db/context.rs` rather than `./context.rs`: with two crates in one
# region a bare relative path no longer says which tree a row came from.
label() { printf '%s/%s\n' "$(basename "$(dirname "$1")" | sed 's/^zeroship-//')" "$2"; }

# Emit the PRODUCTION lines of a file, comments dropped, test-module regions
# excised by brace depth.
#
# DEFECT 5 (found after the first four were fixed): truncating at the first
# cfg(test)-gated `mod` is still wrong, because a file may hold a gated module
# in the MIDDLE and resume production code after it. Three do:
#   lib.rs                    gated mod at :432, production resumes at :559
#   auth/bootstrap.rs         gated mod at :511, production resumes at :564
#   v8_classes/subscription.rs gated mod at :216, production resumes at :279
# The lib.rs case is the one that stings: :559 `row_to_json_for_bench(row:
# &compio_postgres::Row)` and :588 are the row-to-JSON adapter surface that the
# ADAPTER:compio_postgres rule below was added specifically to expose. The rule
# was correct and could never fire, because the region cut removed its subject.
# Excising regions instead of truncating fixes all three.
# DEFECT 8, and the reason this is a COLUMN rule rather than a brace rule:
# counting braces is defeated by braces inside string literals. auth/bootstrap.rs
# has a nested `mod live_reserved_sweep_tests` at :702 inside `mod tests` (:577);
# brace-depth tracking closed the outer region early on a literal and let the
# nested module's `use compio_postgres::{Client, NoTls};` reach the production
# scan. A reviewer hit the same trap from the other side, getting a false alarm
# on backend/postgres.rs from string-literal braces.
#
# In rustfmt'd code a top-level test module opens at column 0 and closes with a
# line that is exactly `}` at column 0. That is unambiguous regardless of what
# the body contains. Nested modules never reach column 0, so they are excised
# with their parent for free.
# DEFECT 12: a module gated where it is DECLARED, not where it is defined.
#
# `region_filter` below reads one file and finds every cfg inside it. It cannot
# see the attribute that decides whether the file is compiled AT ALL, because
# that attribute is in the PARENT:
#
#     // lib.rs:259
#     #[cfg(any(test, feature = "test-helpers"))]
#     pub mod drop_namespace;
#
# `drop_namespace.rs` contains no cfg of its own, so every line read as
# production and the census reported its `use compio_postgres::Pool` as a live
# ENGINE violation. Measured: one declaration, gated, zero callers in src/, and
# its own header says "real code in a build nobody ships".
#
# THE RULE IS "EVERY DECLARATION IS GATED", NOT "SOME DECLARATION IS", and the
# difference is the whole fix. This crate declares most modules through a
# two-arm visibility ladder:
#
#     #[cfg(not(feature = "test-helpers"))] pub(crate) mod exec;
#     #[cfg(feature = "test-helpers")]      pub mod exec;
#
# Both lines carry a cfg. Requiring ALL of them to be gated keeps `exec` (one
# gated arm, one shipped arm) out of the exclusion, where "any" would have
# excluded it.
#
# AND `not(...)` HAS TO BE READ, NOT PATTERN-MATCHED. The first attempt tested
# the attribute for the substring `test`. `"test-helpers"` CONTAINS `test`, so
# `#[cfg(not(feature = "test-helpers"))]` - the SHIPPED arm - counted as gated,
# every laddered module was excluded, and the census went to ZERO rows. It
# printed that as calmly as it prints a real number. A `not(` wrapper is the
# production arm by construction and is skipped before the substring test.
#
# THE PREDICATE ITSELF MOVED TO tests/lib/module_gating.sh ON 2026-09-04, from
# four hand-copies down to one. Three of the four - this one included - tested
# the `#[cfg(` substring ANYWHERE in the line and tested it BEFORE the comment
# arm, so a COMMENT that merely QUOTED `#[cfg(test)]` above a `mod x;`
# declaration read as a gate and the module dropped out of the census entirely.
# Two files were in that state here: `backend/mod.rs` and `crud/unmask.rs`, each
# switched off by prose written to explain a visibility decision. Everything
# above still describes the rule; only the implementation left.
#
# The search root is passed EXPLICITLY. This census `cd`s into each crate's src
# before scanning, so its root is `.` - which is exactly why it could not share
# a helper that hard-coded somebody else's.

region_filter() {   # $1 = file, $2 = "prod" | "test"
  awk -v WANT="$2" '
    /^[[:space:]]*(\/\/\/|\/\/!|\/\/)/ { next }
    # DEFECT 9: match COMPOUND cfgs, not just the bare `#[cfg(test)]` string.
    # auth/bootstrap.rs:701 is `#[cfg(all(test, feature = "live-db-tests"))]`, so
    # a bare matcher read its whole test module as production and admitted
    # `use compio_postgres::{Client, NoTls}` into the production import set -
    # which then matched every mention of `Client`, including the SqlExecutor
    # ASSOCIATED TYPE, reporting 11 where the answer is 3.
    !intest && !initem && /^#\[cfg\(/ && /(^|[^A-Za-z_])test([^A-Za-z_]|$)/ { pend = 1; next }
    pend && /^(pub )?mod [A-Za-z_]+ \{/ { pend = 0; intest = 1; if (WANT=="test") print; next }
    # DEFECT 10: a test cfg on a single ITEM was read as production. Defect 9
    # taught this filter to match compound cfgs, but the only thing it would
    # SKIP was a `mod X {`; a gated `fn` fell through to `{ pend = 0 }` and its
    # signature was scanned as shipped code.
    #
    # That is not hypothetical. On 2026-09-02 six `auth/bootstrap.rs` role
    # provisioners - all with zero production callers, measured across every
    # crate - were put behind `#[cfg(any(test, feature = "test-helpers"))]`, and
    # this census reported the identical 16 afterwards. `test-helpers` is
    # enabled ONLY by `[[test]]` targets via `required-features`; no shipped
    # binary turns it on (zeroship-worker and zeroship-cli both take plugin-db
    # with no features), so that code is in no production build and the tier
    # question does not apply to it - exactly the reasoning that already
    # excludes `#[cfg(test)]`.
    #
    # A single-line item (`use ...;`) ends on its own line; a block item ends at
    # the first column-0 `}`, the same terminator the signature scanner uses.
    pend && /^(pub([[:space:]]*\([^)]*\))?[[:space:]]+)?(async[[:space:]]+)?(fn|struct|enum|trait|impl|use|const|static|type|mod)[[:space:]]/ {
      pend = 0
      if (WANT == "test") print
      if ($0 ~ /;[[:space:]]*$/) next   # single-line item: done
      initem = 1
      next
    }
    # DEFECT 11 is defect 10 own blind spot, found the next cycle by acting on
    # it. `pend` survived a blank line but not a second ATTRIBUTE, so
    #     #[cfg(any(test, feature = "test-helpers"))]
    #     #[doc(hidden)]
    #     pub fn set_postgres_pool_for_tests(pool: Rc<compio_postgres::Pool>, ..)
    # dropped out of the gate at the `#[doc(hidden)]` line and was scanned as
    # shipped code. Both remaining `compio_postgres` signatures in lib.rs have
    # exactly that shape, which is why #109 still read as 2 open violations
    # after its bench half had already been fixed on 2026-09-01.
    #
    # NOTE FOR EDITORS: this awk program is a SINGLE-QUOTED shell string. An
    # apostrophe anywhere in it - including in a comment - ends the string and
    # the file stops parsing. The first draft of this block said "defect 10s"
    # with an apostrophe and the whole census died with a bash syntax error
    # that printed NOTHING through the usual `| grep VIOLATION`, which reads
    # exactly like a clean run.
    pend && /^#\[/ { next }
    pend && /^[[:space:]]*$/ { next }
    { pend = 0 }
    intest {
      if (WANT == "test") print
      if ($0 == "}") intest = 0      # column-0 close ends the module
      next
    }
    initem {
      if (WANT == "test") print
      if ($0 ~ /^}/) initem = 0      # column-0 close ends the item
      next
    }
    WANT == "prod" { print }
  ' "$1"
}
# A file whose module is gated at its declaration has NO production region -
# not "a production region that happens to be empty". Returning nothing is what
# makes the tier question stop applying, exactly as it stops applying inside a
# `#[cfg(test)] mod`.
prod_lines() {
  module_is_test_gated "$1" . && return 0
  region_filter "$1" prod
}
test_lines() { region_filter "$1" test; }

# Does this file have any test region at all?
has_tests() { grep -qP '^\s*#\[cfg\(test\)\]' "$1"; }

# DEFECT 6: defect 4's bare-name fix was applied to ONE marker of four. The
# runtime marker learned to see unqualified imports; compio_postgres, rusqlite
# and v8 still demanded the qualified spelling. So
#   use compio_postgres::Pool;  ...  fn f(pool: &Pool)
# was invisible - costing auth/bootstrap.rs:267/:564, context.rs:541/:554 and
# service.rs:395. Resolve each file's own imports into the pattern.
#
# DEFECT 7, introduced BY the defect-6 fix and caught before publishing: reading
# `use` lines from the whole FILE pulls in test-only imports and applies them to
# the production scan. auth/bootstrap.rs:704 is `use compio_postgres::{Client,
# NoTls};` inside a test module; admitting `Client` as a bare alternative matched
# every production mention of the word - including the `SqlExecutor::Client`
# ASSOCIATED TYPE, which is not a vendor reference at all - and reported 11 where
# the answer is 3. Reads the same region being scanned, on stdin.
imported_names() {
  grep -oP "^\s*use\s+${1}::\{?\K[^;]+" 2>/dev/null \
    | tr -d '{}' | tr ',' '\n' \
    | sed 's/.*:://; s/\s\+as\s\+/ /; s/^\s*//; s/\s*$//' \
    | awk 'NF && $0 ~ /^[A-Z]/ { print $NF }' | sort -u | tr '\n' '|' | sed 's/|$//'
}

# Destination crate per module, from the proposal's assignment table.
tier() {
  case "$1" in
    # tx_scope.rs is ADAPTER: all SEVEN of its production functions take a
    # `&mut v8::PinScope` and do V8 context-map manipulation, so the proposal
    # moves the file whole. It has no test module, so production == the file.
    #
    # This comment said SIX until 2026-09-02, and was correct when written
    # (e81ff8783, 08-31 22:11). `capture_route` landed 85 minutes later in
    # ced6daf1c and nobody re-ran the count. The tier map below was never
    # affected - it classifies the FILE - so the census kept behaving
    # correctly while its own comment described a file that no longer existed.
    # Re-derive with `grep -c 'scope: &mut v8::PinScope' tx_scope.rs` rather
    # than trusting this number.
    ./v8_classes/*|./v8_bridge.rs|./lib.rs|./tx_scope.rs)  echo "ADAPTER" ;;
    # `./broker.rs` and `./read_set.rs` are NOT here, and left on 2026-09-03
    # for `zeroship-data-core` - the broker because the ENGINE and CDC both
    # publish into it, `read_set` because the broker names its `ReadSetEntry`.
    # Same treatment the CORE note below describes: this census scans only
    # `zeroship-data-v8/src`, so an arm for a file that moved out is a pattern
    # matching nothing, a map claiming coverage it does not have. Kept in step
    # with tier_direction_census.sh, where the two censuses judging one file
    # differently is defect 1.
    ./crud/*|./transaction/*|./exec.rs|./backend_selection.rs|./tx_route.rs) echo "ENGINE" ;;
    ./auth/bootstrap.rs)                                 echo "ENGINE" ;;
    # NO ARMS for ./backend/postgres.rs, ./backend/pg_*.rs, ./backend/sqlite/*,
    # ./encryption/* or ./lock_policy.rs. Every one of those was extracted into
    # a dependency crate before 2026-09-03 and the arms were patterns matching
    # nothing - a map claiming coverage it does not have. Deleted rather than
    # kept, exactly as the `broker`/`read_set` note above describes.
    # CORE is now HALF EXTRACTED. `error.rs` and `binding.rs` left for
    # `zeroship-data-core`; this census scans only `zeroship-data-v8/src`, so
    # naming them here would be two patterns that match nothing - a map claiming
    # coverage it does not have. The extracted half needs no census row: Cargo
    # enforces its dependency direction, and its vendor-freedom is ruled on by
    # tests/vendor_embedding_gate.sh, which now lists the crate as a root.
    # What remains below is the CORE-destined code still inside the plugin.
    # Settled by docs/proposals/2026-09-02-thread-context-ownership.md. Kept
    # byte-identical in intent to tier_direction_census.sh: the two censuses
    # judging one file differently is defect 1 in that file.
    ./context.rs|./service.rs|./op_error.rs)              echo "ADAPTER" ;;
    ./tx_lanes.rs|./backend_handle.rs|./backend/cancel.rs|./assignments.rs|./metrics.rs) echo "ENGINE" ;;
    # `descriptor.rs` was CORE here and data-engine in the proposal; SETTLED as
    # ENGINE on 2026-09-03 by the cut, and changed in the same commit as
    # tier_direction_census.sh - the two censuses judging one file differently
    # is defect 1 in that file. `budgets.rs` has no arm: it left for
    # `zeroship-data-core` and an arm for a file this region does not hold is a
    # pattern matching nothing.
    ./descriptor.rs)                                     echo "ENGINE" ;;
    # `backend/mod.rs` was CONTESTED here too. It is ENGINE now: the one CDC
    # name in it - a `#[cfg(test)]` `PgChangeStream` conformance assertion -
    # moved to `change_stream_pg.rs` with the engine cut, and what is left is a
    # prelude over data-core, both vendors and zeroship-schema plus this tier's
    # own `BackendHandle`. Issue #170.
    ./backend/mod.rs)                                    echo "ENGINE" ;;
    *)                                                   echo "CONTESTED" ;;
  esac
}

# May tier $1 name marker $2? ADAPTER sits above everything; CONTESTED modules
# have no destination yet, so they cannot be in violation of one.
#
# NOTE the deliberate exception: ADAPTER is NOT blanket-allowed for
# `upward` - nothing is above the adapter, so that combination is impossible
# rather than permitted - and it is NOT allowed to name both vendors, because
# v8_bridge.rs doing so is the proposal's own row-to-JSON finding. An earlier
# `ADAPTER:*` blanket meant this census could never report that.
allowed() {
  case "$1:$2" in
    ADAPTER:upward)        return 0 ;;
    ADAPTER:compio_postgres|ADAPTER:rusqlite) return 1 ;;   # row-to-JSON finding
    ADAPTER:*)             return 0 ;;
    CONTESTED:*)           return 0 ;;
    PG:compio_postgres)    return 0 ;;
    SQLITE:rusqlite)       return 0 ;;
    CDC:compio_postgres)   return 0 ;;   # the relay reads WAL over the pg protocol
    *)                     return 1 ;;
  esac
}

# Regex per marker. `upward` is a module reference, not a crate: an engine module
# naming crate::v8_bridge or crate::v8_classes points INTO the adapter, which is
# the direction a layered split exists to forbid. No crate-name marker can
# express it.
# $1 marker, $2 the region text being scanned. Defect 6: bare-name alternatives
# are resolved from the imports OF THAT REGION, per marker, so
# `use compio_postgres::Pool;` makes a later `&Pool` visible. Previously only the
# runtime marker did this, and it read the whole file (see defect 7).
marker_re() {
  local extra
  extra=$(printf '%s\n' "$2" | imported_names "$1")
  case "$1" in
    v8)               echo "(^|[^A-Za-z0-9_])(v8::${extra:+|$extra})" ;;
    zeroship_runtime) echo "(^|[^A-Za-z0-9_])(zeroship_runtime::|OpError|OpResult|ResolveValue|SharedState${extra:+|$extra})" ;;
    compio_postgres)  echo "(^|[^A-Za-z0-9_])(compio_postgres::${extra:+|$extra})" ;;
    rusqlite)         echo "(^|[^A-Za-z0-9_])(rusqlite::${extra:+|$extra})" ;;
    upward)           echo 'crate::(v8_bridge|v8_classes)' ;;
  esac
}

COL=$([ "$TEST_REGION" -eq 1 ] && echo TESTREFS || echo SIGS)
printf '%-10s %-46s %-17s %5s   %s\n' TIER FILE MARKER "$COL" VERDICT
echo "----------------------------------------------------------------------------------------"
viol=0; rows=0
while IFS=$'\t' read -r croot f; do
  cd "$croot" || continue
  disp="$(label "$croot" "${f#./}")"
  t=$(tier "$f")
  if [ "$TEST_REGION" -eq 1 ]; then
    has_tests "$f" || continue        # no test region: nothing to say in this mode
    region=$(test_lines "$f")
  else
    region=$(prod_lines "$f")
  fi
  [ -z "$region" ] && continue
  for m in v8 zeroship_runtime compio_postgres rusqlite upward; do
    re=$(marker_re "$m" "$region")
    if [ "$TEST_REGION" -eq 1 ]; then
      # Occurrences, not signatures: a test body has no signature to inspect.
      n=$(printf '%s\n' "$region" | grep -cP "$re")
    elif [ "$m" = upward ]; then
      # `upward` is ALWAYS an occurrence scan, never a signature scan. A module
      # reaches up through `use crate::v8_bridge::...` and through expressions
      # like `transform: crate::v8_classes::masked_value::rehydrate_masked_values`
      # - neither is a function signature, so scanning signatures for it finds
      # nothing and reports a clean tier that is not clean. Scanning signatures
      # for this marker returned 0 across the whole crate while 12 real upward
      # references existed.
      n=$(printf '%s\n' "$region" | grep -cP "$re")
    else
      n=$(printf '%s\n' "$region" | awk -v RE="$re" '
        function flush(  ) { if (c && sig ~ RE) k++; c = 0 }
        # Defect 2: test the fn line itself before moving on - a one-line
        # signature terminates immediately and used to be skipped by `next`.
        /^[[:space:]]*(pub([[:space:]]*\([^)]*\))?[[:space:]]+)?(async[[:space:]]+)?fn[[:space:]]/ {
          flush(); s = NR; sig = $0; c = 1
          if ($0 ~ /\{[[:space:]]*$/ || $0 ~ /;[[:space:]]*$/) flush()
          next
        }
        c { sig = sig " " $0
            if (NR - s > 8 || /\{[[:space:]]*$/ || /;[[:space:]]*$/) flush() }
        # Defect 3: flush the final function, which used to die unchecked at EOF.
        END { flush(); print k+0 }')
    fi
    [ "$n" -eq 0 ] && continue
    if allowed "$t" "$m"; then
      [ "$SHOW_ALL" -eq 1 ] || continue
      v="ok"
    else
      v="** VIOLATION **"; viol=$((viol+n))
    fi
    rows=$((rows+1))
    printf '%-10s %-46s %-17s %5s   %s\n' "$t" "$disp" "$m" "$n" "$v"
  done
done < <(all_sources)
echo "----------------------------------------------------------------------------------------"
echo "rows reported: $rows"
if [ "$TEST_REGION" -eq 1 ]; then
  echo "test-region marker refs whose tier forbids them: $viol"
  echo
  echo "These are dev-dependency crossings, not shipped ones - and they still bind,"
  echo "because a test build must compile. Anything moved by Phase 0 has a test tail"
  echo "that must move with it or the vacated crate keeps the dependency."
else
  echo "signature-position violations: $viol"
fi
echo
echo "A LOWER number is not automatically progress - check that tier() still"
echo "matches the proposal before reading any movement as a fix. The 2026-08-31"
echo "headline of 49 was right by CANCELLATION: an under-reading instrument and a"
echo "stale tier map erred in opposite directions by the same amount."
