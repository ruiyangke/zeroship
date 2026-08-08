# shellcheck shell=bash
#
# Tell "the thing under test broke" apart from "the measurement could not run".
#
# A full disk does not announce itself. It surfaces as a wall of
# `error: linking with cc failed` and `could not compile`, mid-log, reading exactly
# like a defect in whatever change is in flight. That has cost real time three
# separate ways in one session: two suites misread as broken code, and once the
# billing Postgres container died replaying WAL because it could not extend a file -
# taking every live-DB gate with it while the logs blamed the compiler.
#
# So a gate that merely reports "FAILED" is not enough. It has to be able to say
# THIS DID NOT MEASURE ANYTHING, because the two demand opposite responses: one
# means read the diff, the other means reclaim space and re-run.
#
# Correctness here is two-directional and the second direction is the dangerous one.
# A detector that fired on any failure would excuse genuine breakage as
# infrastructure - worse than the confusion it replaces, because a real regression
# would get waved through. `tests/lib_measurement_integrity_selftest.sh` asserts both
# directions, including the adjacent cases that tempt a sloppy pattern: out-of-MEMORY
# is a different resource, and `os error 2` (ENOENT) merely shares digits with
# `os error 28` (ENOSPC).

# Spellings a full disk actually produces. Each is here because it was OBSERVED,
# not because it seemed plausible:
#
#   No space left on device   rustc, cc, and PostgreSQL all render ENOSPC this way
#   os error 28               std::io renders errno numerically; the word boundary
#                             keeps `os error 2` (ENOENT) and any longer number out
#   ENOSPC                    the bare symbol, as some tools print it
#
# Deliberately NOT included: "linking with cc failed" and "could not compile". They
# are what a full disk CAUSES, and matching them is what would relabel every real
# link error as a disk problem.
ZS_DISK_FULL_MARKERS='No space left on device|os error 28\b|ENOSPC'

# log_shows_disk_full <logfile>
#
# 0 when the log indicates the run died for want of disk space.
#
# Scans the WHOLE file: the marker is emitted by whichever compilation unit hit the
# wall first and is then buried under everything that kept running, so a
# head/tail-only check misses it. Missing or unreadable file reports "no" rather
# than erroring - a classifier is not the right place to fail a gate.
log_shows_disk_full() {
  local log="${1:-}"
  [ -n "$log" ] && [ -r "$log" ] || return 1
  grep -qE "$ZS_DISK_FULL_MARKERS" "$log" 2>/dev/null
}

# report_measurement_did_not_run <logfile> <what>
#
# Print an unmistakable banner naming the cause and the fix. Loud on purpose: the
# whole failure mode is that the real cause is one line inside thousands.
#
# The reclamation commands are the ones measured on 2026-08-07: removing
# `incremental` took the volume from 42 MB to 12 GB free, and age-pruning `deps`
# took it from 12 GB to 281 GB. Both are safe because cargo rebuilds what it cannot
# find; `cargo clean` is NOT suggested, because it also destroys
# target/debug/gn_out and forces a full V8 rebuild.
report_measurement_did_not_run() {
  local log="${1:-}" what="${2:-the run}"
  {
    echo "=================================================================="
    echo "MEASUREMENT DID NOT RUN: out of disk space during $what."
    echo
    echo "This is NOT a result. Any compile or link errors above are"
    echo "consequences of the full volume, not of the code under test - do not"
    echo "read them as findings and do not attribute them to the current diff."
    echo
    echo "Matching lines:"
    grep -nE "$ZS_DISK_FULL_MARKERS" "$log" 2>/dev/null | head -5 | sed 's/^/  /'
    echo
    echo "Free space, then re-run. Measured reclamation, largest effect first:"
    echo "  find target/debug/deps -type f -mmin +720 -delete   # stale per-feature artifacts"
    echo "  rm -rf target/debug/incremental                     # regenerable"
    echo "Avoid \`cargo clean\`: it also deletes target/debug/gn_out (the V8 build)."
    echo "=================================================================="
  } >&2
}
