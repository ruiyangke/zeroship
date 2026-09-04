#!/usr/bin/env bash
#
# source_citation_selftest.sh — prove `source_citation_gate.sh` can still fail.
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
# TWO PROBES, ONE PER CORPUS, and that is not redundancy. The scan reads source
# files for source citations AND doc files for source citations, through
# separate patterns, separate scope rules and separate resolution rules. A
# source-only probe passes with the entire doc half deleted - which is exactly
# the state the tree was in before the doc half existed, and the state it would
# silently return to if the doc pattern, the exclusion filter or the root *.md
# arm ever stopped matching. Each probe is planted in the corpus it controls.
#
# A THIRD PROBE for build-state divergence (direction 5), which the two above
# cannot reach: their citations resolve nowhere, and this one resolves on THIS
# machine and on no fresh checkout.
#
# IT USED TO ASSERT A WARNING, and could only ever assert a warning, because the
# scan resolved citations with `[ -e ]` against the working tree - so a build
# output was resolvable here and unresolvable in CI, and the gate could not call
# that an error without failing on every developer's machine. On 2026-09-04 the
# scan's resolution oracle became `git ls-files`, the same set a fresh checkout
# has, and the divergence stopped existing: an untracked target is now
# unresolvable EVERYWHERE, including here. So direction 5 asserts the exit code
# like the other four, which is a strictly stronger claim than the warning it
# replaces - a warning that stopped printing was indistinguishable from a tree
# with nothing to warn about.
#
# Exits 0 when the gate behaves correctly, 1 otherwise.

set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1

SCAN=tests/source_citation_gate.sh
# Planted inside a real scanned root so the probe exercises the same resolution
# path a genuine citation would. A temp file outside the tree would prove
# nothing: the scan would never look at it.
PROBE=crates/zeroship-core/src/zz_citation_selftest_probe.rs
CITATION="crates/zeroship-core/src/this_file_does_not_exist_zz.rs"

# The doc-side probe. In `docs/reference/` rather than at the docs root on
# purpose: a probe in a directory the exclusion list names would be filtered out
# and the test would then pass by never being scanned, which looks identical to
# passing because the gate works. `reference/` is inside the enforced scope and
# is not date-prefixed, so this probe is subject to every rule a real citation
# is. The citation is written with a `../` prefix so the doc-relative resolution
# arm is what has to reject it - the arm that does not exist on the source side.
DOC_PROBE=docs/reference/zz-citation-selftest-probe.md
DOC_CITATION="../../crates/zeroship-core/src/this_doc_citation_does_not_exist_zz.rs"

# The build-state probe (direction 5). An UNTRACKED file that EXISTS, cited from
# a scanned root. That combination is the whole subject: it resolves here and
# cannot resolve on a fresh checkout, which is what CI has.
#
# A path of its own rather than reusing one of the real `dist/` citations,
# because those are already reported - a probe indistinguishable from the
# standing output would pass whether or not the probe did anything.
BUILD_PROBE=crates/zeroship-core/src/zz_buildstate_selftest_probe.rs
BUILD_DIR=examples/zz-buildstate-selftest/dist
BUILD_TARGET="$BUILD_DIR/probe.js"
# A TRACKED file, cited the same way, as the discrimination control: the report
# must name the untracked one and NOT this, or it is just listing everything
# that resolved.
TRACKED_CITATION="crates/zeroship-core/src/typed_id.rs"

cleanup() {
  rm -f "$PROBE" "$DOC_PROBE" "$BUILD_PROBE"
  rm -rf "${BUILD_DIR%/dist}"
  git reset -- "$PROBE" "$DOC_PROBE" "$BUILD_PROBE" > /dev/null 2>&1
}
trap cleanup EXIT

# The scan's corpus is `git ls-files`, not the filesystem (2026-08-20 fix: a
# filesystem corpus makes the verdict depend on whatever untracked clutter is
# lying around, so two people on the same commit could disagree). Every probe
# below is a scratch file that is never committed, so it needs `git add -N`
# (intent-to-add) to become visible to that corpus - the same state a `git add`
# right before commit would produce, without writing any blob. `git reset --
# <path>` in cleanup drops the index entry again, whether or not the working
# copy still exists, so a probe never lingers as a staged addition of a
# deleted file.
stage() { git add -N -- "$1"; }

fail() { echo "::error::source-citation gate self-test: $1"; exit 1; }

# --- Direction 1: the gate must be GREEN before we plant anything. ------------
# If it is already red the rest of the test proves nothing, and the failure
# belongs to whoever broke a citation rather than to this script.
if ! bash "$SCAN" > /dev/null 2>&1; then
  fail "the gate is already failing on a clean tree; fix the citations first, then re-run this"
fi

# --- Direction 2: a planted bad citation must be DETECTED and must exit non-zero.
printf '\n// Self-test probe: %s\n' "$CITATION" > "$PROBE"
stage "$PROBE"

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
git reset -- "$PROBE" > /dev/null 2>&1
if ! bash "$SCAN" > /dev/null 2>&1; then
  fail "the gate stayed red after the probe was removed; it is not tracking the tree"
fi

# --- Direction 4: the same three assertions for the DOC corpus. --------------
# Run against a clean tree that direction 3 just re-established.
printf '# Self-test probe\n\nSee [probe](%s).\n' "$DOC_CITATION" > "$DOC_PROBE"
stage "$DOC_PROBE"

doc_out="$(bash "$SCAN" 2>&1)"
doc_rc=$?

if [ "$doc_rc" -eq 0 ]; then
  fail "planted an unresolvable citation in a doc and the gate still exited 0 - the doc half detects nothing, or it reports without failing"
fi
case "$doc_out" in
  *"$DOC_CITATION"*) : ;;
  *) fail "the gate exited non-zero but never named the planted doc citation; it may be failing for an unrelated reason" ;;
esac

rm -f "$DOC_PROBE"
git reset -- "$DOC_PROBE" > /dev/null 2>&1
if ! bash "$SCAN" > /dev/null 2>&1; then
  fail "the gate stayed red after the doc probe was removed; it is not tracking the doc tree"
fi

# --- Direction 5: an UNTRACKED-but-existing target must FAIL the gate here, on
# --- a built tree, and a tracked one cited identically must not. ---------------
# This is the CI-divergence class, and it is the one defect this gate exists to
# not have: it spent 2026-08-10 green on every developer's machine and red in
# CI, because `dist/` is a build output that a fresh checkout does not have. The
# fix was to stop asking the filesystem. The proof that the fix holds is below,
# and it has to be run on a BUILT tree to mean anything - which is what every
# machine that runs this has.
#
# TWO ARMS, and the second is the one that carries the meaning. "The gate went
# red" alone is satisfied by a gate that rejects every citation. Only the tracked
# control separates that from a gate that discriminates - same probe file, same
# syntax, same resolution arm, differing in ONE variable: whether git tracks the
# target.
mkdir -p "$BUILD_DIR" || fail "could not create $BUILD_DIR"
printf '// self-test build output\n' > "$BUILD_TARGET"
printf '\n// Self-test probe: %s and %s\n' "$BUILD_TARGET" "$TRACKED_CITATION" > "$BUILD_PROBE"
# BUILD_PROBE is staged so the corpus scan reaches it; BUILD_TARGET is
# deliberately left untracked - that gap is the exact thing this direction
# tests for. It EXISTS on disk, so a gate that resolved against the filesystem
# would pass here; that is the mutation this arm is written to catch.
stage "$BUILD_PROBE"

build_out="$(bash "$SCAN" 2>&1)"
build_rc=$?

if [ "$build_rc" -eq 0 ]; then
  fail "cited an existing but UNTRACKED file and the gate exited 0 - it is resolving against this built working tree, so a green here is not a green in CI"
fi
case "$build_out" in
  *"$BUILD_TARGET"*) : ;;
  *) fail "the gate exited non-zero but never named $BUILD_TARGET; it may be failing for an unrelated reason" ;;
esac
# The one-variable control. The tracked citation sits in the SAME probe file, so
# if it is named too the gate is refusing everything rather than discriminating
# on tracked-ness.
case "$build_out" in
  *"$TRACKED_CITATION"*) fail "the gate also reported $TRACKED_CITATION, which git tracks - it is rejecting every citation rather than the untracked one" ;;
  *) : ;;
esac

rm -f "$BUILD_PROBE"
rm -rf "${BUILD_DIR%/dist}"
git reset -- "$BUILD_PROBE" > /dev/null 2>&1
if ! bash "$SCAN" > /dev/null 2>&1; then
  fail "the gate stayed red after the build-state probe was removed; it is not tracking the tree"
fi

echo "source-citation gate self-test: detects a planted citation in BOTH corpora (source and docs), exits non-zero on each, returns to green, and fails on an untracked-but-existing target while passing a tracked one cited beside it"
