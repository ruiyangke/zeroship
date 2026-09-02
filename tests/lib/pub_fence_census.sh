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

printf '%-22s %10s  %s\n' MODULE PUB_ITEMS 'ALSO PUB UNDER test-helpers'
total=0; modules=0
for m in $capped; do
  files=$(find "$SRC/$m.rs" "$SRC/$m" -name '*.rs' 2>/dev/null | LC_ALL=C sort)
  [ -n "$files" ] || continue
  modules=$((modules + 1))
  n=0
  for f in $files; do
    b=$(awk '/^#\[cfg\(test\)\]/{c=NR;next} c&&NF{if($0~/^(pub )?mod /){print c;exit} c=0}' "$f")
    [ -n "$b" ] || b=999999
    k=$(awk -v b="$b" '
      NR >= b { exit }
      /^[[:space:]]*(\/\/|\*)/ { next }
      /^[[:space:]]*pub (fn|struct|enum|trait|const|static|type|unsafe fn|async fn) / { n++ }
      END { print n+0 }' "$f")
    n=$((n + k))
  done
  note=""
  printf '%s\n' "$twinned" | grep -qx "$m" && note="yes"
  printf '%-22s %10d  %s\n' "$m" "$n" "$note"
  total=$((total + n))
done

echo
printf 'MODULES RULED ON: %d\n' "$modules"
printf 'pub items fenced ONLY by a pub(crate) module: %d\n' "$total"

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
