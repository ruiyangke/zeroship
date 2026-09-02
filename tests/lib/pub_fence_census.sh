#!/usr/bin/env bash
# Phase 0.5 audit 1: what stops being private when a module becomes a crate?
#
# `docs/proposals/2026-08-31-data-crate-shape.md` lists three audits that "must
# precede ANY crate boundary". The first is every symbol going
# `pub(crate) -> pub`, "and whether the fence was load-bearing", noting that
# "this repository has already shipped this mistake once and written a comment
# claiming it had not."
#
# THE MISTAKE DOES NOT LOOK LIKE AN EDIT TO THE SYMBOL. `lib.rs` today declares
# `pub(crate) mod backend;`, `pub(crate) mod context;` and friends, so a
# `pub fn` inside them is crate-private however it is spelled - Rust caps an
# item's effective visibility by its module path. The split promotes those
# modules to crate roots, and a crate root is public. Every one of those
# `pub fn`s becomes reachable by every dependent, in a commit whose diff is
# `git mv` and a Cargo.toml.
#
# So the reviewable question is not "which symbols did this PR widen" - none of
# them - but "how many were only ever fenced by the module, and is each one
# meant to be public API of its new crate". This census answers the first half
# and sizes the second.
#
# NOT A GATE. There is no pass/fail number here: a large count is a statement
# about how much API surface the split publishes, not a defect. It sits beside
# tier_signature_census.sh for the same reason - a verdict would have to encode
# a decision nobody has made yet.
#
# WHAT IT CANNOT SEE. It counts declarations by their `pub` keyword and the
# file's own test-module boundary. It does not resolve re-exports, so a symbol
# already reachable through a `pub use` elsewhere is counted here as fenced
# when it is not. Treat the total as an UPPER bound on newly-public items and
# the per-module rows as the worklist.
#
# ---------------------------------------------------------------------------
# HOW TO WORK A ROW, and the two ways of deciding that DO NOT work
# ---------------------------------------------------------------------------
#
# 1. COUNTING REFERENCES BY BARE NAME IS USELESS. Measured 2026-09-02 while
#    working `encryption`: `new` returned 140990 hits across the workspace,
#    `insert` 3455, `resolve` 16048, and `Row` 51 - every same-named item
#    anywhere. Only the zeros carried information.
#
# 2. COUNTING BY QUALIFIED PATH (`aead::decrypt`, `session::TypedRows`) fixes
#    the collisions AND IS BLIND TO METHODS, because a method call is
#    `receiver.method()` and never `module::method`. Every method row in such a
#    measurement is meaningless rather than merely noisy.
#
# 3. THE COMPILER IS THE ORACLE. Narrow the candidates, build with
#    `--features test-helpers --all-targets`, read the errors. It is the only
#    instrument here that sees methods, `pub use` re-exports and inference.
#
# AND THE RULE THAT COST THREE ROUND TRIPS TO LEARN:
#
#   A ZERO READER COUNT IS NOT A LICENCE TO NARROW.
#
# An item in a PUBLIC SIGNATURE must stay public however few callers name it,
# because callers obtain it by inference. Three items hit this in one day, each
# found only by the compiler refusing the narrowing:
#
#   encryption::aead::AeadKey            return of `KeyStore::resolve`
#   backend::sqlite::session::TypedRows  return of `query_typed`     (39 errors)
#   backend::sqlite::reservation::Reservation
#                                        return of
#                                        `spent_autocommit_reservation_for_tests`
#
# So ask two SEPARATE questions per item: "does anything name it?" picks the
# candidate; "is it in a public signature?" decides. Only the second is
# authoritative, and only the compiler answers it reliably.
#
# ---------------------------------------------------------------------------
# THE ORACLE IS ONE-SIDED: IT CATCHES NARROWING AND IS BLIND TO WIDENING.
# ---------------------------------------------------------------------------
#
# The batch-narrow-then-let-the-compiler-adjudicate loop ends when the build is
# green. Green proves nothing was narrowed too far. It says NOTHING about an
# item left wider than it needs to be, because widening never fails to compile.
# So the restore half of the loop needs its own check, and on 2026-09-02 it did
# not have one: a `sed` restoring the 17 items the compiler had demanded matched
# by NAME, and `parse_args` / `parse_bulk_args` each exist TWICE -
#
#     #[cfg(any(test, feature = "test-helpers"))]  pub fn parse_args(..)
#     #[cfg(not(any(test, feature = "test-helpers")))]
#                                          pub(crate) fn parse_args(..)
#
# - so it widened the PRODUCTION arm of the DB-3 argument parser to `pub` and
# every config still compiled clean. The tell was not a build failure but
# `git diff` on a file that should have had none.
#
# Two consequences worth carrying:
#   - After a restore pass, DIFF the files. A restore that touched a line the
#     narrowing pass never touched is a widening, not a restoration.
#   - Before restoring an item by name, check whether the name is forked by
#     cfg. `grep -A2 'cfg(not(any(test' <file>` lists the production arms.
#
# And the numeric tell, which is why this census is worth re-running on both
# sides of an edit: the total went UP by 2 while the pass was supposedly only
# narrowing. A count that moves the wrong way is the cheapest available signal.
set -uo pipefail
cd "$(dirname "$0")/../.."

SRC=crates/zeroship-plugin-db/src
LIB=$SRC/lib.rs

if [ ! -f "$LIB" ]; then
  echo "pub_fence_census: $LIB not found - has the crate moved?" >&2
  exit 1
fi

capped=$(grep -oE "^pub\(crate\) mod [a-z_]+;" "$LIB" | awk '{print $3}' | tr -d ';' | sort -u)
twinned=$(grep -oE "^pub mod [a-z_]+;" "$LIB" | awk '{print $3}' | tr -d ';' | sort -u)

if [ -z "$capped" ]; then
  echo "pub_fence_census: no 'pub(crate) mod' declarations in lib.rs." >&2
  echo "  Either the split has landed, or the declaration style changed and" >&2
  echo "  this census is now reporting on nothing. It does NOT mean zero." >&2
  exit 1
fi

# A submodule whose own `mod` declaration is behind a test cfg is not compiled
# into any shipped binary, so its items are not surface the split publishes.
# They were counted as such until 2026-09-02, which put 22 of `transaction`'s
# 107 in the worklist: `transaction/probe.rs`, declared at
# `transaction/mod.rs:144` as `#[cfg(any(test, feature = "test-helpers"))]
# pub mod probe;` and described in its own doc comment as "Not compiled into a
# production build". Same rule as `module_is_test_gated` in
# tests/vendor_embedding_gate.sh; kept separate so a rename there cannot
# silently change this census's numbers.
module_is_test_gated() {  # $1 = a .rs path under $SRC
  local file="$1" name decls gated decl_file decl_line prev i
  name=$(basename "$file" .rs)
  [ "$name" = "mod" ] && name=$(basename "$(dirname "$file")")
  [ -z "$name" ] && return 1
  decls=0; gated=0
  while IFS= read -r hit; do
    decl_file="${hit%%:*}"; decl_line="${hit#*:}"; decl_line="${decl_line%%:*}"
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
          esac ;;
        "#["*|*"//"*|"") i=$((i - 1)) ;;
        *) break ;;
      esac
    done
  done < <(grep -rn -E "^[[:space:]]*(pub([[:space:]]*\([^)]*\))?[[:space:]]+)?mod[[:space:]]+${name}[[:space:]]*;" "$SRC" 2>/dev/null)
  [ "$decls" -gt 0 ] && [ "$decls" -eq "$gated" ]
}

printf '%-22s %10s %10s  %s\n' MODULE SHIPPED TEST_GATED 'ALSO PUB UNDER test-helpers'
total=0; gated_total=0; modules=0
for m in $capped; do
  files=$(find "$SRC/$m.rs" "$SRC/$m" -name '*.rs' 2>/dev/null | LC_ALL=C sort)
  [ -n "$files" ] || continue
  modules=$((modules + 1))
  n=0; g=0
  for f in $files; do
    b=$(awk '/^#\[cfg\(test\)\]/{c=NR;next} c&&NF{if($0~/^(pub )?mod /){print c;exit} c=0}' "$f")
    [ -n "$b" ] || b=999999
    k=$(awk -v b="$b" '
      NR >= b { exit }
      /^[[:space:]]*(\/\/|\*)/ { next }
      /^[[:space:]]*pub (fn|struct|enum|trait|const|static|type|unsafe fn|async fn) / { n++ }
      END { print n+0 }' "$f")
    if module_is_test_gated "$f"; then g=$((g + k)); else n=$((n + k)); fi
  done
  note=""
  printf '%s\n' "$twinned" | grep -qx "$m" && note="yes"
  printf '%-22s %10d %10d  %s\n' "$m" "$n" "$g" "$note"
  total=$((total + n))
  gated_total=$((gated_total + g))
done

echo
printf 'MODULES RULED ON: %d\n' "$modules"
printf 'pub items fenced ONLY by a pub(crate) module: %d\n' "$total"
printf '  (+ %d more in test-gated submodules, which no shipped binary compiles)\n' "$gated_total"

# ---------------------------------------------------------------------------
# The four controls the proposal names by hand. Reported individually because
# a count cannot say whether the RIGHT things are fenced, and because these are
# the ones whose failure is a security bug rather than an API-surface question.
# ---------------------------------------------------------------------------
echo
echo "== the four named controls =="
report() { # name, file, pattern
  local name="$1" file="$2" pat="$3" hit
  if [ ! -f "$file" ]; then
    printf '  MISSING  %-24s %s does not exist\n' "$name" "$file"
    return
  fi
  hit=$(grep -nE "$pat" "$file" | head -1)
  if [ -z "$hit" ]; then
    printf '  MISSING  %-24s no match in %s\n' "$name" "$file"
    return
  fi
  printf '  %-24s %s:%s\n' "$name" "$file" "${hit%%:*}"
  printf '           %s\n' "$(printf '%s' "$hit" | cut -d: -f2- | sed 's/^[[:space:]]*//')"
}
report sanitize_app_actor "$SRC/crud/unmask.rs"  '^[[:space:]]*pub(\(crate\))? fn sanitize_app_actor'
report "TxRoute::capture"  "$SRC/tx_route.rs"     '^[[:space:]]*pub(\(crate\))? fn capture'
report "context::with_mut" "$SRC/context.rs"      '^[[:space:]]*pub(\(crate\))? fn with_mut'
report "DbBinding::cold_start" crates/zeroship-data-core/src/binding.rs \
       '^[[:space:]]*pub(\(crate\))? fn cold_start'

echo
echo "Measured 2026-09-02: all four hold TODAY, but three of them hold for"
echo "DIFFERENT reasons, and only one of those survives becoming a crate:"
echo "  sanitize_app_actor  pub(crate) on the ITEM   - survives the split"
echo "  TxRoute::capture    pub(crate) on the ITEM   - survives the split"
echo "  context::with_mut   pub fn, capped by 'pub(crate) mod context'"
echo "                                               - DOES NOT survive"
echo "  DbBinding::cold_start  #[cfg(feature=\"test-helpers\")], and the feature"
echo "    resolves OFF for the shipped worker. Verified with"
echo "      cargo tree -p zeroship-worker -e features -i zeroship-data-core"
echo "    which shows data-core at feature \"default\" only. plugin-db enables"
echo "    data-core/test-helpers in [dev-dependencies], NOT in [dependencies] -"
echo "    a dependency grep alone cannot tell those apart and reads as a live"
echo "    production exposure."
