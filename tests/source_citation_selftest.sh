#!/usr/bin/env bash
#
# source_citation_selftest.sh — prove `source_citation_scan.sh` can still fail.
#
# The gate it guards passes at zero unresolvable citations, which is also what a
# gate that has quietly stopped matching reports. A pattern that no longer fires,
# a root list that drifted, a `grep` that declines to read a file — all of them
# produce the same clean output as a healthy tree. This runs the positive control
# that separates those, on every CI run rather than once by hand.
#
# The same reasoning as `lib_measurement_integrity_selftest.sh` beside it: a check
# that only matters on a bad day is exactly the check that rots unnoticed.
#
# Asserts BOTH directions, because either alone is satisfiable by a broken gate:
#   - a planted unresolvable citation is DETECTED and the script exits non-zero
#   - with it removed the script exits zero
# A gate that always fails is as useless as one that always passes.
#
# Exits 0 when the gate behaves correctly, 1 otherwise.

set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1

SCAN=tests/source_citation_scan.sh
# Planted inside a real scanned root so the probe exercises the same resolution
# path a genuine citation would. A temp file outside the tree would prove
# nothing: the scan would never look at it.
PROBE=crates/core/src/zz_citation_selftest_probe.rs
CITATION="crates/core/src/this_file_does_not_exist_zz.rs"

cleanup() { rm -f "$PROBE"; }
trap cleanup EXIT

fail() { echo "::error::source-citation gate self-test: $1"; exit 1; }

# --- Direction 1: the gate must be GREEN before we plant anything. ------------
# If it is already red the rest of the test proves nothing, and the failure
# belongs to whoever broke a citation rather than to this script.
if ! bash "$SCAN" > /dev/null 2>&1; then
  fail "the gate is already failing on a clean tree; fix the citations first, then re-run this"
fi

# --- Direction 2: a planted bad citation must be DETECTED and must exit non-zero.
printf '\n// Self-test probe: %s\n' "$CITATION" > "$PROBE"

out="$(bash "$SCAN" 2>&1)"
rc=$?

if [ "$rc" -eq 0 ]; then
  fail "planted an unresolvable citation and the gate still exited 0 — it detects nothing, or it reports without failing"
fi
case "$out" in
  *"$CITATION"*) : ;;
  *) fail "the gate exited non-zero but never named the planted citation; it may be failing for an unrelated reason" ;;
esac

# --- Direction 3: removing it must return the gate to green. -----------------
rm -f "$PROBE"
if ! bash "$SCAN" > /dev/null 2>&1; then
  fail "the gate stayed red after the probe was removed; it is not tracking the tree"
fi

echo "source-citation gate self-test: detects a planted citation, exits non-zero on it, and returns to green"
