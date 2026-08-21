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
# BOTH CEILINGS ARE 0, AND THE ASYMMETRY IS GONE
# ----------------------------------------------
# Default was allowed 1 until 2026-08-20. That 1 was a named residue:
# crates/plugin-db/src/cross_app_fk.rs cited
# `register_model::bootstrap::build_ctx`, whose module is
# `#[cfg(any(test, feature = "test-helpers"))]`, so the link resolved under
# --all-features and not under default. The header here said it "cannot be
# edited into correctness" because link-vs-span depended on an open decision
# about which configuration plugin-db's docs are built for.
#
# Two things were wrong with that.
#
# First, the decision was not open, it was already made HERE: this gate builds
# both configurations and demands zero in each, so neither is privileged, and
# a cfg-gated internal must be a code span because that is the only construct
# correct in both. The note at crates/plugin-db/src/backend/mod.rs that framed
# it as undecided predates this file and said so explicitly ("zeroship has no
# doc gate today, so nothing currently encodes either answer").
#
# Second, and worse, the link was FACTUALLY WRONG independent of any of that:
# `build_ctx` does not call `reject_cross_app_fk` and never did. `bootstrap`
# does, at bootstrap.rs:133, on the other side of the advisory lock the
# sentence described. So the allowance was not tolerating an unresolvable
# link, it was tolerating a false statement about the code - exactly the
# defect in the two cases above. An allowance sized to fit one known item
# admits any new item that also fits, and this one had been sitting under it.
#
# There is now no standing allowance. If a legitimately unresolvable link ever
# appears, write it as a code span; if you believe it must be a link, that is a
# change to what this gate asserts and belongs in this header, not in a number.
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
# The residue retired above is the worked example of that limit cutting BOTH
# ways. rustdoc flagged that link, but only because the module was cfg-gated -
# it had no opinion on the sentence being false. Had `build_ctx` been public,
# the same false sentence would have resolved cleanly and this gate would have
# reported zero. So a green run means "every name exists", never "the docs are
# right", and the surrounding claim still has to be read by a person.
#
# USAGE
#   tests/run_doc_gate.sh
#
# THRESHOLDS (constants below, not environment - a bound its caller can move
# is not a bound)
#   DOC_MAX_DEFAULT 0   max unresolved links with default features
#   DOC_MAX_ALL     0   max unresolved links with --all-features
#   DOC_MIN_CRATES      DERIVED from `cargo metadata`, not written down - see
#                       below for why the constant that was here went stale
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Names a full volume as the cause when the crate-count control fires. See the
# library header; `tests/lib_measurement_integrity_selftest.sh` covers both
# directions and is itself gated in CI.
. "$ROOT/tests/lib/measurement_integrity.sh"

# Per-arm anti-vacuity accounting. This gate already HAD the right instinct -
# note 3 above is exactly "a check that examines nothing prints the same as a
# clean tree" - it just enforced it ad hoc per run_config() call instead of
# through the shared contract. Wiring it through names the arm (default vs.
# all-features) in the refusal instead of just the gate.
# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init run_doc

DOC_MAX_DEFAULT=0
DOC_MAX_ALL=0

# HOW MANY CRATES A REAL RUN DOCUMENTS, asked of the workspace manifest rather
# than written down. This was `DOC_MIN_CRATES=26` against 30 real members
# (measured 2026-08-20 by a cold `cargo clean --doc && cargo doc --no-deps
# --workspace`: 30 `Documenting` lines, exit 0). A build that died after the
# 26th crate therefore read as a clean pass, and the gap only ever widens,
# because every new crate loosens a constant nobody re-runs. Two crates landed
# on 2026-08-20 alone.
#
# Deriving it does not violate the "not environment" rule in the header above:
# the number comes from the workspace's own membership, which the caller cannot
# move without editing a Cargo.toml, and it is exact rather than slack.
DOC_MIN_CRATES="$(cargo metadata --no-deps --format-version 1 2>/dev/null \
  | jq '.packages | length')"

# jq missing, cargo failing, or a manifest error all yield an empty or tiny
# number, and a floor of 0 passes every dead build. Refuse instead.
if ! [ "${DOC_MIN_CRATES:-0}" -ge 10 ] 2>/dev/null; then
  echo "FAIL: could not read the workspace member count (got '${DOC_MIN_CRATES:-}')." >&2
  echo "      That number IS the control that separates a clean doc build from" >&2
  echo "      one that never ran, so the gate refuses rather than defaulting." >&2
  exit 2
fi

LOG="$(mktemp)"
trap 'rm -f "$LOG"' EXIT

status=0

# $1 = arm id, $2 = human label, $3 = max allowed unresolved, $4.. = extra cargo flags
run_config() {
  local arm="$1" label="$2" max="$3"
  shift 3

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
  #
  # This IS the arm: `crates` is the number of crates this config's build
  # actually documented, i.e. ruled on for unresolved links, and DOC_MIN_CRATES
  # (derived above from the workspace's own membership) is the floor a real run
  # clears. Reusing that existing floor here rather than picking a fresh
  # "well under" number, because DOC_MIN_CRATES is already exact - it is not a
  # slack bound, it is what a live build produces - and the whole point of note
  # 3 was that a build which did not run must not pass as if it examined
  # everything.
  if ! gate_arm "$arm" "$crates" "$DOC_MIN_CRATES"; then
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

run_config doc_default_features "default features " "$DOC_MAX_DEFAULT"
run_config doc_all_features     "--all-features   " "$DOC_MAX_ALL" --all-features

gate_arms_finish || status=1

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
