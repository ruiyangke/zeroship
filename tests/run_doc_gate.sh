#!/usr/bin/env bash
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
