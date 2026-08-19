#!/usr/bin/env bash
# ============================================================================
# run_doc_gate.sh - fail the build when a doc comment cites something that
# does not exist.
#
# WHY THIS EXISTS
# ---------------
# Nothing in CI ran `cargo doc`, so an intra-doc link naming a deleted or
# renamed item was invisible. Two that survived that way, both found 2026-08-07:
#
#   crates/control/src/refund.rs explained the native refund rail by pointing at
#   `metering::provider::native::NativeProvider` - deleted in f975eae8b, whose
#   own commit message records the successor ("reshape native into
#   lite/openmeter/stripe_meters").
#
#   crates/control/src/billing_read.rs promised that a pricing failure maps to
#   `RegistryError::Pricing`. No such variant exists and none ever did; the
#   function returns `FxUnresolved`.
#
# Neither is a formatting slip. Both are documentation that describes a system
# which changed underneath it, and rustdoc knew about both the whole time.
#
# WHAT THIS GATE ASSERTS, AND WHY EACH PIECE IS LOAD-BEARING
# ----------------------------------------------------------
# The count alone is NOT a sufficient check. Every line below exists because
# measuring this specific number went wrong in a specific way:
#
# 1. `cargo clean --doc` first. A warm doc cache does not re-emit warnings for
#    crates it does not rebuild, so an incremental run under-reports - measured
#    three times during the sweep that produced this file.
#
# 2. Warn-mode, NOT `-D rustdoc::broken_intra_doc_links`. `-D` aborts the build
#    at the first offending crate, so the count is truncated to however far it
#    got. One tree reported 26, 99, and 143 depending on how the build stopped.
#
# 3. THE DOCUMENTED-CRATE COUNT IS THE REAL CONTROL. A failed build emits zero
#    warnings, so `grep -c 'unresolved link'` returns 0 - identical to a clean
#    pass. That is not hypothetical: while building this gate, `-p $P` with an
#    unquoted variable under zsh collapsed the whole package list into one
#    bogus package name, cargo exited 101, and the unresolved count read a
#    confident 0. Only the crate count (0, expected 26) caught it.
#    A gate that cannot tell "nothing broken" from "nothing ran" is not a gate.
#
# 4. Both feature configurations. The default and --all-features counts differ,
#    because items behind `#[cfg(feature = ...)]` do not exist to link to when
#    the feature is off. Asserting only one config would silently bless the
#    other.
#
# THE ASYMMETRIC FLOORS ARE DELIBERATE
# ------------------------------------
# --all-features must be 0. Default is allowed 1, and that 1 is a known,
# named residue: crates/plugin-db/src/cross_app_fk.rs cites
# `register_model::bootstrap::build_ctx`, whose module is
# `#[cfg(any(test, feature = "test-helpers"))]`. It cannot be edited into
# correctness - whether it should be a link or a code span depends on which
# configuration plugin-db's docs are built for, which is an open operator
# decision (see the note at crates/plugin-db/src/backend/mod.rs and the
# 2026-08-07 entry in docs/pilot/2026-08-06-pilot-decision-log.md).
#
# Tighten DOC_MAX_DEFAULT to 0 in the same change that answers it.
#
# WHAT THIS DOES NOT CATCH
# ------------------------
# Only links rustdoc can prove wrong. A link that resolves to the WRONG
# existing item is still green here - `[`Self::take`]` pointing at a real
# method that does the opposite of what the sentence claims reads as correct.
# It also says nothing about prose accuracy, only about names. And it does not
# cover `--document-private-items`, which asks a different question and
# reports much larger numbers.
#
# USAGE
#   tests/run_doc_gate.sh
#
# THRESHOLDS (constants below, not environment - a bound its caller can move
# is not a bound)
#   DOC_MAX_DEFAULT 1   max unresolved links with default features
#   DOC_MAX_ALL     0   max unresolved links with --all-features
#   DOC_MIN_CRATES  26  workspace members that must actually be documented
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Names a full volume as the cause when the crate-count control fires. See the
# library header; `tests/lib_measurement_integrity_selftest.sh` covers both
# directions and is itself gated in CI.
. "$ROOT/tests/lib/measurement_integrity.sh"

DOC_MAX_DEFAULT=1
DOC_MAX_ALL=0
DOC_MIN_CRATES=26

LOG="$(mktemp)"
trap 'rm -f "$LOG"' EXIT

status=0

# $1 = human label, $2 = max allowed unresolved, $3.. = extra cargo flags
run_config() {
  local label="$1" max="$2"
  shift 2

  # Cold, every time. See note 1.
  cargo clean --doc >/dev/null 2>&1

  # No -D: warn-mode keeps the build going so the count is the whole tree,
  # not the prefix that built before the first abort. See note 2.
  cargo doc --no-deps --workspace "$@" >"$LOG" 2>&1
  local rc=$?

  local unresolved crates
  unresolved="$(grep -c 'unresolved link' "$LOG")"
  crates="$(grep -oP '^\s+Documenting \K\S+' "$LOG" | sort -u | wc -l)"

  echo "--- ${label}: exit=${rc} documented=${crates} unresolved=${unresolved} (max ${max}) ---"

  # Checked BEFORE the unresolved count, because a build that did not run
  # produces the most reassuring number in this whole script. See note 3.
  if [ "$crates" -lt "$DOC_MIN_CRATES" ]; then
    # Name the cause when it is knowable. The crate-count control already turns a
    # dead build into a FAIL rather than a false green, so the verdict was never
    # wrong - but "documented only 3 crates" sends the reader looking for a doc
    # problem, and a full volume is the one cause that is both common here and
    # invisible in the message. Checked first so the diagnosis leads.
    if log_shows_disk_full "$LOG"; then
      report_measurement_did_not_run "$LOG" "${label} cargo doc"
      status=1
      return
    fi
    echo "FAIL: ${label} documented only ${crates} crates, expected ${DOC_MIN_CRATES}." >&2
    echo "      A doc build that did not run reports 0 unresolved links, which is" >&2
    echo "      indistinguishable from a clean pass by that count alone. Treat this" >&2
    echo "      as a broken measurement, NOT as a doc-link result." >&2
    if [ "$rc" -ne 0 ]; then
      echo "      cargo exited ${rc}; last 15 lines:" >&2
      tail -15 "$LOG" >&2
    fi
    status=1
    return
  fi

  if [ "$rc" -ne 0 ]; then
    echo "FAIL: ${label} cargo doc exited ${rc}." >&2
    tail -15 "$LOG" >&2
    status=1
  fi

  if [ "$unresolved" -gt "$max" ]; then
    echo "FAIL: ${label} has ${unresolved} unresolved doc links, more than the ${max} allowed." >&2
    echo "      A link naming a deleted or renamed item is the defect this gate exists for;" >&2
    echo "      resolve the name rather than raising the ceiling. Offenders:" >&2
    awk '/unresolved link/{msg=$0; getline; while ($0 !~ /-->/ && NF) getline; print "        " $2 "  " msg}' "$LOG" \
      | sed 's/warning: //' >&2
    status=1
  fi
}

run_config "default features " "$DOC_MAX_DEFAULT"
run_config "--all-features   " "$DOC_MAX_ALL" --all-features

echo "=================================================================="
if [ "$status" -ne 0 ]; then
  echo "DOC GATE: FAILED" >&2
  exit 1
fi
# Printed on success too: a number nobody sees until the gate has already
# failed cannot warn anyone, and this is the channel where a false green
# would look different from a true one.
echo "DOC GATE: passed (<= ${DOC_MAX_DEFAULT} default, <= ${DOC_MAX_ALL} all-features," \
     "${DOC_MIN_CRATES} crates documented in each)"
