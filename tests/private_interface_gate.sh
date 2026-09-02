#!/usr/bin/env bash
# Refuse a type narrowed below the signature that carries it.
#
# THE DEFECT THIS EXISTS FOR, introduced by the very audit it now protects.
# Phase 0.5 audit 1 walks every `pub` item in zeroship-plugin-db asking whether
# its `pub` is real API or an accident of `pub(crate) mod` capping it. The
# method is to narrow a candidate and let the compiler adjudicate. That method
# has a blind spot: narrowing a TYPE while the STRUCT, ENUM or METHOD naming it
# stays `pub` compiles clean and only warns. On 2026-09-02 a reducer pass
# narrowed 23 types and left `TxEvent`, `Action` and `TxReducer` public, which
# produced 66 `private_interfaces` warnings and shipped, because
#
#   nothing in this tree reads rustc's warning stream.
#
# `cargo test` prints them and exits 0. `clippy_gate.sh` lints under
# `--all-features`, where the reducer's own targets resolve differently, and it
# rules on clippy's deny set, not on rustc's warn-by-default lints. So the
# 66 were visible in every build log for a day and caught by a human reading
# one.
#
# WHY IT MATTERS BEYOND TIDINESS. `private_interfaces` is the compiler saying an
# item is reachable at a visibility its own signature cannot honour. During a
# crate split that is precisely the question under audit: when `pub(crate) mod
# transaction` becomes a crate root, a `pub enum TxEvent` whose fields are
# `pub(crate)` becomes a public type nobody outside can construct, destructure
# or match. The warning is the split's own acceptance criterion, already
# computed, already free.
#
# ---------------------------------------------------------------------------
# THE TRAP THIS GATE IS BUILT AROUND: CARGO DOES NOT RE-EMIT CACHED WARNINGS.
# ---------------------------------------------------------------------------
#
# A warm `cargo check` reports zero warnings for a crate it did not rebuild -
# including a crate whose source is dirty with all 66. A gate that just greps a
# warm build's stderr passes on the exact tree it exists to refuse, and prints
# what a clean tree prints. So this gate FORCES re-emission by touching the
# crate root before checking, and arm 1 rules on the number of compilation
# units that actually came back. If the touch stops working, or the feature
# set stops resolving, or the package is renamed, arm 1 collapses to near zero
# and the gate refuses instead of reporting a green it never measured.
#
# `touch` and not `cargo clean -p`: measured 2026-09-02, cleaning the package
# removed 8313 files / 11.1 GiB and rebuilt every dependency-facing artifact.
# Touching the crate root re-runs rustc for this crate and its dependents only,
# which is all that is needed for this crate's warnings to be re-emitted.
#
# WHAT IT DOES NOT RULE ON. One feature resolution only - the widest single one,
# `--features test-helpers --all-targets`, which is where the crate's test
# targets compile and therefore where the reducer's public surface is widest. A
# type reachable only under `live-db-tests` is outside this gate's sight.
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init private_interface

PKG=zeroship-plugin-db
SRC=crates/$PKG/src
LIB=$SRC/lib.rs
OUT=$(mktemp)
trap 'rm -f "$OUT"' EXIT

if [ ! -f "$LIB" ]; then
  echo "private_interface_gate: $LIB not found - has the crate moved?" >&2
  exit 1
fi

# Force re-emission. See the header: a warm build's silence is not evidence.
touch "$LIB"
CARGO_INCREMENTAL=0 cargo check -p "$PKG" --features test-helpers --all-targets \
  --message-format=json > "$OUT" 2>/dev/null
check_status=$?

units=$(grep '"reason":"compiler-artifact"' "$OUT" | grep -c "$PKG")

echo "== compilation units re-emitted for $PKG =="
echo "  $units unit(s) rebuilt under --features test-helpers --all-targets"
echo
gate_arm compiled_units "$units" 6

# A build that failed outright tells us nothing about visibility; say so rather
# than reporting zero warnings from a compile that never produced any.
if [ "$check_status" -ne 0 ]; then
  echo "private_interface_gate: cargo check exited $check_status." >&2
  echo "  The crate does not compile, so its warning stream is not evidence" >&2
  echo "  about visibility. Fix the build first." >&2
  gate_arms_finish
  exit 1
fi

# ---------------------------------------------------------------------------
# Arm 2: the population the lint can fire inside - every `pub` item whose only
# fence is a `pub(crate) mod` in lib.rs. Counted here rather than imported from
# tests/lib/pub_fence_census.sh so this gate does not go quiet if that script is
# renamed.
#
# THE TWO NUMBERS DIFFER ON PURPOSE, and did not until 2026-09-02. The census
# reports SHIPPED surface and now excludes items in test-gated submodules
# (transaction/probe.rs, crud/mask_drift.rs, auth/util.rs - 35 items), because
# a module no shipped binary compiles is not API the split publishes. This gate
# counts them, because it runs UNDER `--features test-helpers`, where those
# modules very much do compile and the lint very much can fire inside them.
# Census 147 shipped + 35 gated = 182 here. A divergence of any other size is
# worth reading rather than reconciling automatically.
# ---------------------------------------------------------------------------
# Brace-tracking, not "stop at the first #[cfg(test)] mod". That older rule
# assumed a file's test module is last; five files here declare an early named
# one and keep shipping items after it (see the note in
# tests/lib/pub_fence_census.sh, which carries the same function).
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

capped=$(grep -oE "^pub\(crate\) mod [a-z_]+;" "$LIB" | awk '{print $3}' | tr -d ';' | sort -u)
fenced=0
for m in $capped; do
  files=$(find "$SRC/$m.rs" "$SRC/$m" -name '*.rs' 2>/dev/null)
  for f in $files; do
    fenced=$((fenced + $(count_shipped_pub "$f")))
  done
done

echo "== items the lint can fire inside =="
echo "  $fenced pub item(s) fenced only by a pub(crate) module"
echo
gate_arm fenced_pub_items "$fenced" 100

# ---------------------------------------------------------------------------
# The verdict. Both lints, because they are the same defect in two positions:
# private_interfaces is a private type in a public signature, private_bounds a
# private type in a public bound.
# ---------------------------------------------------------------------------
# Counted by LINE, not by a quoted-field regex: cargo emits one JSON object per
# line, and `rendered` embeds escaped quotes, so `"[^"]*"` truncates mid-value
# and miscounts. One diagnostic is one line.
leaks=$(grep -c 'more private than the item' "$OUT")

echo "== verdict =="
if [ "$leaks" -eq 0 ]; then
  echo "  ok   no item is reachable at a visibility its signature cannot honour"
  gate_arms_finish || exit 1
  echo
  echo "private_interface_gate: passed"
  exit 0
fi

{
  echo "PRIVATE TYPE IN A PUBLIC SIGNATURE: $leaks diagnostic(s)."
  echo
  echo "  Each names an item reachable at \`pub\` whose signature carries a type"
  echo "  that is not. During the crate split this is not cosmetic: when the"
  echo "  fencing module becomes a crate root, that item becomes public API no"
  echo "  dependent can actually use."
  echo
  echo "  THE FIX IS NOT TO WIDEN THE TYPE BY REFLEX. Ask which of the two is"
  echo "  right, and make the other agree:"
  echo "    - nothing outside names the CONTAINER  -> narrow the container"
  echo "    - something outside needs it           -> the type is real API,"
  echo "                                              restore it to pub"
  echo "  Settle it with the compiler, not by counting readers: a caller"
  echo "  obtains a return type by inference and never names it."
  echo
} >&2
# The `message` field is safe to slice on quotes - rustc renders these with
# backticks around the type names, never quotes - unlike `rendered`.
grep -oE '"message":"type [^"]*more private than the item[^"]*"' "$OUT" \
  | sed -E 's/^"message":"//; s/"$//' | sort -u | sed 's/^/  /' >&2

gate_arms_finish
exit 1
