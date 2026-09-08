#!/usr/bin/env bash
#
# The app-id sweep's frontier cannot go backwards.
#
# WHAT THIS GUARDS. `AppId::from_uuid` is a dated bridge. Its own module header
# says so: it "wraps the `Uuid` the `zeroship.apps.id` column still holds,
# producing a NON-CANONICAL id whose printed form is the hyphenated uuid... and
# it is deleted in the slice that flips the column." Every call site is a place
# where a typed id degrades back to a uuid, and the sweep that removes them is
# long enough that a NEW one can be added in the middle of it without anyone
# noticing - a `Uuid` in hand and a function wanting an `AppId` makes
# `AppId::from_uuid` the path of least resistance, and it compiles.
#
# So this gate is a RATCHET, not a threshold. The count may fall and may not
# rise. Lowering CEILING is part of the commit that removes a caller; nothing
# here lowers it automatically, because a ceiling that follows the code cannot
# refuse anything.
#
# WHY A GATE AND NOT A LINT. `#[deprecated]` is the obvious alternative and it
# is banned: AGENTS.md's pre-launch section forbids deprecation aliases, and a
# warning is not a refusal in a tree that carries warnings. A count is a
# refusal.
#
# WHAT THE COUNT DELIBERATELY INCLUDES. Every non-comment mention outside the
# two defining modules and outside `tests/` directories - INCLUDING mentions
# inside in-file `#[cfg(test)]` modules. Excluding those would need a brace
# tracker, and getting it wrong UNDER-counts, which is the direction that lets a
# new caller through. Over-counting only costs a ceiling that is one higher than
# the production frontier, and the composition below says which is which.
#
# THE COMMENT FILTER IS THE PART THAT WAS WRONG FIRST. Written as
# `grep -vE ':[0-9]+: *(//|...)'` it silently matched nothing, because single-file
# `grep -n` emits `205:  // ...` with no leading colon. The gate then counted
# three prose mentions in `zeroship-gateway/src/sync.rs` as call sites and would
# have baked them into the ceiling. Arm 2 exists so a filter that stops
# filtering cannot pass unnoticed.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# shellcheck source=tests/lib/gate_arms.sh
. "$(dirname "$0")/lib/gate_arms.sh"

gate_arms_init app_id_frontier

FAIL=0

# The ratchet. Composition, measured by this script:
#   3 production call sites  - plugin-workflow store/pg.rs, migrate-server
#     apply.rs, plugin-db replication.rs
#   1 in-file test mention   - gateway sync.rs, inside its `#[cfg(test)] mod`
# When the sweep finishes, the production half is zero and `AppId::from_uuid` is
# deleted outright, taking the last one with it - so the end state is CEILING=0
# and this gate is deleted with the bridge it was written to police.
#
# IT WAS 8, AND PART OF THE DROP IS NOT PROGRESS OF THE KIND THE NUMBER SUGGESTS.
# Five of the removed sites were the app-lifecycle lock's holders, and they did
# not stop converting - they now share ONE conversion inside
# `app_derivation::lifecycle_lock_seed_for_stored_uuid`, in a file this gate
# EXCLUDES as a defining module. So one degradation moved out of the count
# rather than out of the tree. That is the intended shape (the derivation seam
# is where a transitional conversion belongs, and one conversion cannot half-move
# the way five could) but a reader comparing 8 to 4 should know that four of the
# four are real removals and one is a relocation.
CEILING=4

# ONE definition, used by the real scan and by the anti-vacuity control alike.
# It was two copies for the length of one edit, and they had already drifted on
# the comment filter before either ran.
count_from_uuid() {
  local root="$1" n=0 f
  for f in $(grep -rl 'AppId::from_uuid' --include='*.rs' "$root" 2>/dev/null \
    | grep -v 'app_id\.rs' \
    | grep -v 'app_derivation\.rs' \
    | grep -v '/tests/'); do
    local hits
    hits=$(grep -n 'AppId::from_uuid' "$f" | grep -vcE '^[0-9]+: *(//|///|//!|\*)' || true)
    n=$((n + hits))
  done
  printf '%s' "$n"
}

# ---------------------------------------------------------------------------
# Arm 1: the frontier itself.
#
# `examined` is the number of Rust source files the scan covered, not the number
# of call sites found. A gate whose examined-count IS its finding reports zero
# examined on a clean tree, which is the shape this contract exists to refuse.
# ---------------------------------------------------------------------------
n_files=$(find crates -path '*/src/*' -name '*.rs' | wc -l)
n_callers=$(count_from_uuid crates)

echo "==> app-id frontier: $n_callers mentions across $n_files source files (ceiling $CEILING)"

if [ "$n_callers" -gt "$CEILING" ]; then
  echo "FAIL: the app-id frontier went backwards: $n_callers > $CEILING" >&2
  echo "  A new AppId::from_uuid call site was added while the sweep is in flight." >&2
  echo "  Every one of these degrades a typed id back to a hyphenated uuid, and the" >&2
  echo "  derived names built from it - the schema, the per-app role, the HKDF salt -" >&2
  echo "  change shape silently when the id does. Take the AppId through instead." >&2
  echo "  Sites:" >&2
  for f in $(grep -rl 'AppId::from_uuid' --include='*.rs' crates \
    | grep -v 'app_id\.rs' | grep -v 'app_derivation\.rs' | grep -v '/tests/'); do
    grep -n 'AppId::from_uuid' "$f" | grep -vE '^[0-9]+: *(//|///|//!|\*)' \
      | sed "s|^|    $f:|" >&2
  done
  FAIL=$((FAIL + 1))
elif [ "$n_callers" -lt "$CEILING" ]; then
  echo "FAIL: the frontier moved forward to $n_callers but CEILING is still $CEILING" >&2
  echo "  Lower CEILING in the same commit that removed the caller. A ceiling left" >&2
  echo "  above the count is slack a later commit can spend without any arm firing," >&2
  echo "  which is how a ratchet stops ratcheting." >&2
  FAIL=$((FAIL + 1))
fi

if ! gate_arm frontier "$n_files" 500; then
  FAIL=$((FAIL + 1))
fi

# ---------------------------------------------------------------------------
# Arm 2: the control. Can the scanner still SEE a call site, and does the
# comment filter still filter?
#
# Both halves are needed and they fail in opposite directions. A scanner that
# finds nothing reports a frontier of zero and passes arm 1's upper bound; a
# comment filter that stops filtering inflates the count and fails arm 1 for the
# wrong reason. The synthetic file carries one real call and one prose mention,
# so a correct predicate answers exactly 1.
# ---------------------------------------------------------------------------
probe_dir="$(mktemp -d)"
trap 'rm -rf "$probe_dir"' EXIT
mkdir -p "$probe_dir/probe/src"
cat > "$probe_dir/probe/src/lib.rs" <<'PROBE'
// A prose mention of AppId::from_uuid that must NOT be counted.
/// Nor this doc-comment one: AppId::from_uuid.
fn real() {
    let _ = AppId::from_uuid(&id);
}
PROBE

n_probe=$(count_from_uuid "$probe_dir")
if [ "$n_probe" -ne 1 ]; then
  echo "FAIL: the control found $n_probe call sites in a file carrying exactly 1" >&2
  if [ "$n_probe" -eq 0 ]; then
    echo "  The scanner matched nothing. Arm 1's frontier of $n_callers is then a" >&2
    echo "  statement about the instrument, not about the tree." >&2
  else
    echo "  The comment filter stopped filtering, so prose mentions are being" >&2
    echo "  counted as call sites. This exact defect shipped once already." >&2
  fi
  FAIL=$((FAIL + 1))
fi

if ! gate_arm scanner_control "$n_probe" 1; then
  FAIL=$((FAIL + 1))
fi

gate_arms_finish || FAIL=$((FAIL + 1))

if [ "$FAIL" -ne 0 ]; then
  echo "app-id frontier gate: FAILED" >&2
  exit 1
fi
echo "app-id frontier gate: ok"
