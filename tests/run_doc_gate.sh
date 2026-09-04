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
#   crates/zeroship-control/src/refund.rs explained the native refund rail by pointing at
#   `metering::provider::native::NativeProvider` - deleted in f975eae8b, whose
#   own commit message records the successor ("reshape native into
#   lite/openmeter/stripe_meters").
#
#   crates/zeroship-control/src/billing_read.rs promised that a pricing failure maps to
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
# 1. `cargo clean --doc` first, so every `target/doc/<crate>/index.html` this
#    run is counted against is a product of THIS run rather than of some
#    earlier one.
#
# 2. Warn-mode, NOT `-D rustdoc::broken_intra_doc_links`. `-D` aborts the build
#    at the first offending crate, so the count is truncated to however far it
#    got. One tree reported 26, 99, and 143 depending on how the build stopped.
#
# 3. THE DOCUMENTED-CRATE COUNT IS THE REAL CONTROL. A failed build emits zero
#    warnings, so counting unresolved links returns 0 - identical to a clean
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
# 5. THE COUNT COMES FROM CARGO'S ARTIFACT STREAM AND THE FILES ON DISK, NOT
#    FROM `Documenting` LINES. This is note 3's own failure mode one level in,
#    and it is what the count was BOUND to until 2026-08-20 - see the next
#    section.
#
# WHAT THE CRATE COUNT IS BOUND TO, AND WHAT IT USED TO BE BOUND TO
# ------------------------------------------------------------------
# The DENOMINATOR was already right: `cargo metadata` reads Cargo.toml, so 30 is
# a property of the workspace and reproduces exactly. The NUMERATOR was not. It
# was
#
#     grep -oP '^\s+Documenting \K\S+' "$LOG" | sort -u | wc -l
#
# and `Documenting` is printed only for a doc unit cargo decides to RUN. Two
# things therefore went uncounted, in opposite directions:
#
#   - A unit cargo considers FRESH prints nothing at all. MEASURED on a
#     two-crate scratch workspace: a warm re-run printed 0 `Documenting` lines
#     while cargo's json stream reported both crates, `fresh=true`, each with
#     its index.html. The gate's `cargo clean --doc` is what normally prevents
#     this, and it is not atomic with the doc run - `cargo clean` takes no
#     build-directory lock (checked by holding `target/debug/.cargo-lock` with
#     flock: `cargo clean --doc` removed its 60 files immediately anyway), so
#     anything that re-documents into the same target/ in between makes units
#     fresh again.
#   - A unit that cargo STARTS and that then fails prints `Documenting` and
#     produces no docs, so the old numerator counted it.
#
# MEASURED 2026-08-20, two doc-gate-shaped loops against ONE target directory -
# which is main's situation, several agents sharing a worktree. Six runs, each
# reporting `Documenting`-count vs. packages cargo emitted a doc artifact for:
#
#     30/27 (rc=101)   18/18   19/19   21/22   18/20   12/30
#
# The last row is the whole defect in one line: cargo accounted for all 30
# crates - 12 rebuilt, 18 fresh with their diagnostics replayed - and the old
# instrument said 12. The first is the other direction: three crates cargo could
# not document, counted as documented. Neither run differed from a clean one in
# anything the gate printed except that number. A third arrangement, one loop of
# `cargo clean --doc; cargo doc` against a single gate run, reproduced the
# reported 23-of-30 exactly.
#
# So the numerator is now the set of WORKSPACE PACKAGES that produced a doc-unit
# artifact in `cargo doc --message-format=json` whose `target/doc/<crate>/index.html`
# EXISTS when the run ends. Cargo emits that artifact for a fresh unit as well as
# a rebuilt one, and emits nothing for a unit it never scheduled, so the count no
# longer moves with cache warmth; and requiring the file to be on disk means a
# unit cargo called fresh whose output somebody deleted mid-run does not count
# either - cargo reports exactly that case as an artifact with an EMPTY filename
# list, which is what the ten missing crates in the `18/20` run above looked
# like. Same instrument as tests/clippy_gate.sh arm 3, for the same reason.
#
# BOTH CEILINGS ARE 0, AND THE ASYMMETRY IS GONE
# ----------------------------------------------
# Default was allowed 1 until 2026-08-20. That 1 was a named residue:
# crates/plugin-db/src/cross_app_fk.rs cited a bootstrap helper whose module was
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
# correct in both. The note at crates/zeroship-data-engine/src/backend/mod.rs that framed
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
# One more, about the count rather than the links: a crate counted as documented
# was not necessarily re-run by rustdoc in THIS process. Cargo replays a fresh
# unit's cached diagnostics - measured on a two-crate scratch workspace, where a
# planted `unresolved link` was reported again by a warm re-run that printed no
# `Documenting` line at all - so a fresh unit is still ruled on, just not
# re-examined. The `cargo clean --doc` above means that cannot happen to a run
# with the target directory to itself; it can happen to one sharing it, which is
# the case the refusal path names.
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
JSON="$(mktemp)"
trap 'rm -f "$LOG" "$JSON"' EXIT

status=0

# Every doc-unit artifact cargo reported, as `<package-id><TAB><index.html>`.
# The filter is what makes a DOC unit distinguishable from the `check` unit
# cargo also builds for the same package: the check unit's filename is an
# .rmeta under target/debug/deps, the doc unit's is target/doc/<crate>/index.html.
doc_artifacts() {
  jq -r '
    select(.reason == "compiler-artifact")
    | . as $a
    | .filenames[]?
    | select(test("/doc/[^/]+/index\\.html$"))
    | $a.package_id + "\t" + .
  ' "$JSON"
}

# $1 = arm id, $2 = human label, $3 = max allowed unresolved, $4.. = extra cargo flags
run_config() {
  local arm="$1" label="$2" max="$3"
  shift 3

  # Cold, every time. See note 1.
  cargo clean --doc >/dev/null 2>&1

  # No -D: warn-mode keeps the build going so the count is the whole tree,
  # not the prefix that built before the first abort. See note 2.
  #
  # json on stdout, cargo's own status lines on stderr. The diagnostics move
  # into the json stream with `--message-format=json`, so $LOG is the human
  # narration and $JSON is what anything below counts. See note 5.
  cargo doc --no-deps --workspace "$@" --message-format=json >"$JSON" 2>"$LOG"
  local rc=$?

  local unresolved crates
  # One DIAGNOSTIC per offending link, selected by LINT CODE.
  #
  # Two narrowings have been walked back here, each one a subset that read like
  # the whole set:
  #
  #   1. `grep -c 'unresolved link'` counted rendered LINES, and rustdoc renders
  #      the offending source line underneath the message - so a doc comment
  #      quoting the phrase counted twice. Ceilings are 0, so it never changed a
  #      verdict; it did make the number in the failure message wrong.
  #   2. `startswith("unresolved link")` keyed on rustdoc's PROSE. Measured
  #      2026-09-04: it caught 89 of the 97 `rustdoc::broken_intra_doc_links`
  #      diagnostics under default features and 83 of 91 under --all-features.
  #      The eight it missed carry the SAME lint code and different wording -
  #      "`env` is both a module and a macro", "unknown disambiguator ``".
  #      Both totals exceeded 0 at the time, so the verdict was again unchanged
  #      - but a tree reduced to only those eight would have printed
  #      `unresolved=0` and PASSED, which is this gate's own founding defect
  #      (see the header: a check that examines nothing prints what a clean tree
  #      prints).
  #
  # The lint code is the thing rustdoc promises; its sentence is not. Key on the
  # code, and a future rustdoc rewording cannot silently shrink the population.
  unresolved="$(jq -r '
    select(.reason == "compiler-message")
    | select(.message.code.code == "rustdoc::broken_intra_doc_links")
    | .message.message
  ' "$JSON" | wc -l | tr -d ' ')"
  crates="$(doc_artifacts \
    | while IFS=$'\t' read -r pkg html; do
        [ -f "$html" ] && printf '%s\n' "$pkg"
      done | sort -u | wc -l | tr -d ' ')"

  echo "--- ${label}: exit=${rc} documented=${crates} unresolved=${unresolved} (max ${max}) ---"

  # Checked BEFORE the unresolved count, because a build that did not run
  # produces the most reassuring number in this whole script. See note 3.
  #
  # This IS the arm: `crates` is the number of workspace packages this config's
  # build actually documented - cargo reported a doc artifact for them AND the
  # docs are on disk - i.e. the packages it ruled on for unresolved links, and
  # DOC_MIN_CRATES (derived above from the workspace's own membership) is the
  # floor a real run clears. Reusing that existing floor here rather than picking
  # a fresh "well under" number, because DOC_MIN_CRATES is already exact - it is
  # not a slack bound, it is what a live build produces - and the whole point of
  # note 3 was that a build which did not run must not pass as if it examined
  # everything. Both sides of that comparison are now per-PACKAGE and come from
  # a source other than this run's console output; see note 5.
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
    # Name the OTHER knowable cause. A unit cargo found fresh but whose output
    # file is gone is reported as an artifact with an empty filename list, and
    # that only happens when something else removed target/doc while this run
    # was planning - another `cargo doc` or `cargo clean --doc` in the same
    # target directory, which several agents sharing a worktree produce
    # routinely. It says "this run was not representative", not "the tree has a
    # doc problem", and the two want opposite responses from the reader.
    local vanished
    vanished="$(jq -r '
      select(.reason == "compiler-artifact")
      | select(.filenames | length == 0)
      | .target.name
    ' "$JSON" | sort -u)"
    if [ -n "$vanished" ]; then
      echo "      NOT REPRESENTATIVE: cargo found these units fresh and their output" >&2
      echo "      already gone, so another cargo emptied target/doc under this run:" >&2
      printf '%s\n' "$vanished" | sed 's/^/        /' >&2
      echo "      Re-run with nothing else building into the same target directory." >&2
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
    jq -r '
      select(.reason == "compiler-message")
      | select(.message.code.code == "rustdoc::broken_intra_doc_links")
      | (.message.spans[0] // {}) as $s
      | "        "
        + ($s.file_name // "?") + ":" + (($s.line_start // 0) | tostring)
        + "  " + .message.message
    ' "$JSON" >&2
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
