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
# TWO PROBES, ONE PER CORPUS, and that is not redundancy. The scan reads source
# files for source citations AND doc files for source citations, through
# separate patterns, separate scope rules and separate resolution rules. A
# source-only probe passes with the entire doc half deleted - which is exactly
# the state the tree was in before the doc half existed, and the state it would
# silently return to if the doc pattern, the exclusion filter or the root *.md
# arm ever stopped matching. Each probe is planted in the corpus it controls.
#
# A THIRD PROBE for the build-state report (direction 5), which the two above
# cannot reach: it is a WARNING, so it never moves the exit code that directions
# 1-4 assert on. It could stop printing entirely and every assertion here would
# still pass.
#
# Exits 0 when the gate behaves correctly, 1 otherwise.

set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1

SCAN=tests/source_citation_scan.sh
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

# --- Direction 5: the build-state report must name an UNTRACKED resolved target,
# --- and must NOT name a tracked one. -----------------------------------------
# The gate reads the working tree; CI reads a fresh checkout. A citation to a
# build output therefore resolves on every developer's machine and on nobody
# else's, which is how this gate spent two days green locally and red in CI
# (2026-08-10). The report that separates those two states is a WARNING, so it
# cannot be checked by exit code - which is exactly why it needs a self-test:
# a warning that stopped printing looks identical to a tree with nothing to warn
# about.
#
# TWO ARMS, and the second is the one that carries the meaning. "Names the
# untracked path" alone is satisfied by a report that lists every path that
# resolved. Only the tracked control separates that from a report that
# discriminates - same probe, same syntax, same resolution arm, differing in one
# variable: whether git tracks the target.
mkdir -p "$BUILD_DIR" || fail "could not create $BUILD_DIR"
printf '// self-test build output\n' > "$BUILD_TARGET"
printf '\n// Self-test probe: %s and %s\n' "$BUILD_TARGET" "$TRACKED_CITATION" > "$BUILD_PROBE"
# BUILD_PROBE is staged so the corpus scan reaches it; BUILD_TARGET is
# deliberately left untracked - that gap is the exact thing this direction
# tests for.
stage "$BUILD_PROBE"

build_out="$(bash "$SCAN" 2>&1)"
build_rc=$?

# Checked BEFORE the naming assertions, and they depend on it: on a non-zero
# exit the planted paths would be named by an ::error:: line instead, and the
# grep below could not tell the two apart.
if [ "$build_rc" -ne 0 ]; then
  fail "both planted citations resolve, so the gate should have exited 0; it exited $build_rc - the build-state arm cannot be read from a failing run"
fi
case "$build_out" in
  *"$BUILD_TARGET"*) : ;;
  *) fail "cited an existing but UNTRACKED file and the build-state report never named it; a local run can no longer tell itself apart from a CI run" ;;
esac
case "$build_out" in
  *"$TRACKED_CITATION"*) fail "the build-state report named $TRACKED_CITATION, which git tracks - it is listing everything that resolved rather than what a fresh checkout would lose" ;;
  *) : ;;
esac

rm -f "$BUILD_PROBE"
rm -rf "${BUILD_DIR%/dist}"
git reset -- "$BUILD_PROBE" > /dev/null 2>&1
gone_out="$(bash "$SCAN" 2>&1)"
if [ $? -ne 0 ]; then
  fail "the gate stayed red after the build-state probe was removed; it is not tracking the tree"
fi
case "$gone_out" in
  *"$BUILD_TARGET"*) fail "the build-state report still names $BUILD_TARGET after the probe was removed; it is not reading the current tree" ;;
  *) : ;;
esac

echo "source-citation gate self-test: detects a planted citation in BOTH corpora (source and docs), exits non-zero on each, returns to green, and names an untracked-but-resolved target without naming a tracked one"
