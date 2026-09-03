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

# ---------------------------------------------------------------------------
# TWO CRATES SINCE 2026-09-03, AND THEY ANSWER THE SAME QUESTION AT TWO STAGES.
# ---------------------------------------------------------------------------
# `SRC` was `crates/zeroship-plugin-db/src` alone. The ENGINE tier left for
# `crates/zeroship-data-engine/src` that day, taking ~22k lines and most of the
# `pub(crate) mod` fences with it - so a census pinned to the first root would
# have reported a small, healthy number about the modules that stayed and said
# nothing at all about the ones whose fence had just BECOME a crate boundary.
# That is exactly the "reports on nothing while printing what a clean tree
# prints" failure this file's own header is a catalogue of.
#
# The two roots are reported SEPARATELY, not merged, because the question means
# something different on each side:
#
#   ADAPTER (`zeroship-plugin-db`)  - items still fenced by a `pub(crate) mod`.
#     This is surface the split has NOT yet published: it becomes public API the
#     day the module becomes a crate root.
#
#   ENGINE (`zeroship-data-engine`) - items behind a `pub mod` at a crate root.
#     This is surface the split HAS published. The fence is the crate boundary
#     now, and the 325 `pub(crate)` markers inside those modules were widened to
#     `pub` in the same commit that moved them, because a `pub(crate)` item in
#     the engine is unreachable from the adapter that calls it.
#
# The combined figure is what the split publishes in total, and it is the number
# `tests/private_interface_gate.sh` rules on the health of.
ADAPTER_SRC=crates/zeroship-plugin-db/src
ENGINE_SRC=crates/zeroship-data-engine/src

for _d in "$ADAPTER_SRC" "$ENGINE_SRC"; do
  [ -f "$_d/lib.rs" ] || {
    echo "pub_fence_census: $_d/lib.rs not found - has a crate moved?" >&2
    exit 1
  }
done

SRC=$ADAPTER_SRC
LIB=$SRC/lib.rs

capped=$(grep -oE "^pub\(crate\) mod [a-z_]+;" "$LIB" | awk '{print $3}' | tr -d ';' | sort -u)
twinned=$(grep -oE "^pub mod [a-z_]+;" "$LIB" | awk '{print $3}' | tr -d ';' | sort -u)

if [ -z "$capped" ]; then
  echo "pub_fence_census: no 'pub(crate) mod' declarations in $LIB." >&2
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

# Count line-start `pub <kind>` declarations OUTSIDE any `#[cfg(test)] mod`
# block, tracking brace depth to find where each block ends.
#
# THE RULE WAS "stop counting at the first #[cfg(test)] mod" UNTIL 2026-09-02,
# which assumes a file's test module is last. Five files in this tree break
# that: plugin-db's `auth/bootstrap.rs` (`mod reserved_table_revoke_tests` at
# :489, real `mod tests` at :555), `backend/postgres.rs` (:1467, then :1671 and
# :1702), `lib.rs` (five blocks from :506), `v8_classes/subscription.rs`
# (:217, :350), and zeroship-schema's `query.rs` (`mod schema_renderer_tests`
# at :594, real `mod tests` at :6618). The old rule discarded every shipped
# item after the first marker - one item here, and 6000 lines of query.rs when
# the same rule was used to survey that file.
count_shipped_pub() {
  awk '
    BEGIN { depth = 0; intest = 0; pending = 0; n = 0 }
    /^[[:space:]]*#\[cfg\(test\)\]/ { pending = 1; next }
    {
      if (pending && NF) {
        if ($0 ~ /^[[:space:]]*(pub )?mod /) {
          intest = 1
          depth = gsub(/\{/, "{") - gsub(/\}/, "}")
          pending = 0
          next
        }
        pending = 0
      }
      if (intest) {
        depth += gsub(/\{/, "{") - gsub(/\}/, "}")
        if (depth <= 0) { intest = 0; depth = 0 }
        next
      }
      if ($0 ~ /^[[:space:]]*(\/\/|\*)/) next
      if ($0 ~ /^[[:space:]]*pub (fn|struct|enum|trait|const|static|type|unsafe fn|async fn) /) n++
    }
    END { print n + 0 }' "$1"
}

echo "== ADAPTER ($ADAPTER_SRC): fenced by a pub(crate) module, NOT yet published =="
printf '%-22s %10s %10s  %s\n' MODULE SHIPPED TEST_GATED 'ALSO PUB UNDER test-helpers'
total=0; gated_total=0; modules=0
for m in $capped; do
  files=$(find "$SRC/$m.rs" "$SRC/$m" -name '*.rs' 2>/dev/null | LC_ALL=C sort)
  [ -n "$files" ] || continue
  modules=$((modules + 1))
  n=0; g=0
  for f in $files; do
    k=$(count_shipped_pub "$f")
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
# The ENGINE half. Same counter, opposite question: these modules are `pub mod`
# at a crate root, so their `pub` items ARE the published surface - the fence
# is cargo's now, not lib.rs's.
# ---------------------------------------------------------------------------
SRC=$ENGINE_SRC
LIB=$SRC/lib.rs
published=$(grep -oE "^pub mod [a-z_]+;" "$LIB" | awk '{print $3}' | tr -d ';' | sort -u)

if [ -z "$published" ]; then
  echo "pub_fence_census: no 'pub mod' declarations in $LIB." >&2
  echo "  The engine crate publishes nothing, which cannot be true while the" >&2
  echo "  adapter compiles against it. This census is reporting on nothing." >&2
  exit 1
fi

echo
echo "== ENGINE ($ENGINE_SRC): published by the crate boundary, ALREADY public =="
printf '%-22s %10s %10s\n' MODULE SHIPPED TEST_GATED
eng_total=0; eng_gated_total=0; eng_modules=0
for m in $published; do
  files=$(find "$SRC/$m.rs" "$SRC/$m" -name '*.rs' 2>/dev/null | LC_ALL=C sort)
  [ -n "$files" ] || continue
  eng_modules=$((eng_modules + 1))
  n=0; g=0
  for f in $files; do
    k=$(count_shipped_pub "$f")
    if module_is_test_gated "$f"; then g=$((g + k)); else n=$((n + k)); fi
  done
  printf '%-22s %10d %10d\n' "$m" "$n" "$g"
  eng_total=$((eng_total + n))
  eng_gated_total=$((eng_gated_total + g))
done

echo
printf 'MODULES RULED ON: %d\n' "$eng_modules"
printf 'pub items published by the engine crate boundary: %d\n' "$eng_total"
printf '  (+ %d more in test-gated submodules, which no shipped binary compiles)\n' "$eng_gated_total"

echo
printf 'COMBINED, both crates: %d modules, %d shipped pub items (+ %d test-gated)\n' \
  "$((modules + eng_modules))" "$((total + eng_total))" "$((gated_total + eng_gated_total))"
echo "  The two halves are NOT interchangeable: the adapter's number is surface"
echo "  the split has yet to publish, the engine's is surface it already has."

SRC=$ADAPTER_SRC

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
report sanitize_app_actor "$ENGINE_SRC/crud/unmask.rs" '^[[:space:]]*pub(\(crate\))? fn sanitize_app_actor'
report "TxRoute::capture"  "$ENGINE_SRC/tx_route.rs"   '^[[:space:]]*pub(\(crate\))? fn capture'
report "context::with_mut" "$ADAPTER_SRC/context.rs"   '^[[:space:]]*pub(\(crate\))? fn with_mut'
report "DbBinding::cold_start" crates/zeroship-data-core/src/binding.rs \
       '^[[:space:]]*pub(\(crate\))? fn cold_start'

echo
echo "Measured 2026-09-02, and TWO OF THE FOUR PREDICTIONS WERE FALSIFIED BY THE"
echo "ENGINE CUT ON 2026-09-03. Corrected here rather than quietly rewritten:"
echo "  sanitize_app_actor  said 'pub(crate) on the ITEM - survives the split'."
echo "    It is plain 'pub' now, at zeroship-data-engine's crate root. It could"
echo "    not have survived as pub(crate): three of its five call sites are in"
echo "    the engine and two are the ADAPTER's masked_value.rs. The DB-3 fence"
echo "    was never this visibility - it is that all five call sites invoke it -"
echo "    so the guarantee holds and the PREDICTION did not."
echo "  TxRoute::capture    the same, for the same reason: 'pub' in the engine,"
echo "    called from the adapter's tx_scope::capture_route. Its real fence is"
echo "    its argument, a '&mut v8::PinScope' the engine crate cannot even name."
echo "  context::with_mut   pub fn, capped by 'pub(crate) mod context'"
echo "                                               - DOES NOT survive"
echo "  DbBinding::cold_start  #[cfg(feature=\"test-helpers\")], and the feature"
echo "    resolves OFF for the shipped worker. Verified with"
echo "      cargo tree -p zeroship-worker -e features -i zeroship-data-core"
echo "    which shows data-core at feature \"default\" only. plugin-db enables"
echo "    data-core/test-helpers in [dev-dependencies], NOT in [dependencies] -"
echo "    a dependency grep alone cannot tell those apart and reads as a live"
echo "    production exposure."
