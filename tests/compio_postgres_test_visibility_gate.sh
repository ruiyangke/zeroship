#!/usr/bin/env bash
#
# Every test declared in libs/compio-postgres/tests/suite/ must be compiled by
# at least one of the feature resolutions this crate is actually verified under.
#
# THE FAILURE THIS EXISTS FOR, 2026-08-27. `tests/suite/temporal_edge_values.rs`
# declares 6 `#[compio::test]` cases. Three sit behind `with-chrono-0_4` /
# `with-time-0_3`, which the then-documented feature set did not enable. A
# `#[cfg(feature = ...)]` test is not reported as skipped or ignored -- it is
# ABSENT FROM THE BINARY -- so the run compiled 3 of the 6 and printed
#
#     test result: ok. 626 passed; 0 failed; 0 ignored; 0 measured
#
# which is indistinguishable from having run them all. The three that vanished
# were exactly the ones proving PostgreSQL `time '24:00'` is refused rather
# than silently aliased to midnight -- the headline of the commit being
# verified. A whole verification pass was published off that green.
#
# Same shape as a filter matching nothing (`0 passed; ... N filtered out`), one
# layer down: the absence is invisible because the only evidence of it is a
# number nobody compares against anything. So this compares it.
#
# WHY THREE RESOLUTIONS, AND NOT ONE. Measured 2026-08-27: there is NO single
# feature resolution that compiles every test in this suite.
#
#     gate feature set   631 of 633   omits the suite-over-tls and
#                                     suite-with-statement-cache cases
#     --all-features     630 of 633   omits prefer_attestation_fallback, whose
#                                     module is `#![cfg(not(suite-over-tls))]`
#     default (none)     625 of 633   omits everything behind tls and friends
#     union              633 of 633
#
# `integration.rs` also carries a `cfg(not(tls))` / `cfg(tls)` either-or PAIR,
# so counting one resolution can never reconcile that file. The union is the
# only honest denominator.
#
# WHY `test_target_census_gate.sh` DOES NOT ALREADY COVER THIS. That gate
# counts the test NAMES the workspace builds against a floor
# (`NAME_FLOOR_DEFAULT=5413`, measured 2026-08-27). It answers "did a suite
# vanish". A `#[cfg]` that hides three cases out of 633 moves that total by
# three against thousands of points of slack, so it cannot fire -- and it is
# not supposed to: a floor with no slack turns every deletion into a failure.
# This gate asks the other question, which needs no slack because it is a
# comparison rather than a threshold: does the source declare a case that no
# binary contains.
#
# WHAT THIS DOES NOT DO. It does not check that a test asserts anything, that
# it ran, or that it passed -- `--list` reports what was COMPILED IN. A test
# that is present and vacuous passes here. It rules on one question: is every
# case a reader can see in the file actually in some binary. It also cannot see
# a case behind a feature none of the three resolutions selects; the
# resolutions arm is what stops that set silently shrinking.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT" || exit 1

# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init compio_postgres_test_visibility

SUITE_DIR="libs/compio-postgres/tests/suite"
# The set the compio-postgres gate RUNS with. Deliberately not `--all-features`:
# that enables `suite-over-tls`, which cfg-replaces `common::test_url()` so the
# suite ignores PG_TEST_URL entirely.
GATE_FEATURES="tls,live-tls-tests,live-unix-socket,with-chrono-0_4,with-time-0_3"

FAIL=0

if [ ! -d "$SUITE_DIR" ]; then
  echo "REFUSED: $SUITE_DIR does not exist; this gate cannot rule on anything" >&2
  exit 1
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# A build failure must be a loud refusal, never an empty listing that every
# later comparison reads as "nothing declared".
list_into() {
  local out="$1"; shift
  if ! cargo test -p compio-postgres "$@" --test suite -- --list > "$out" 2>/dev/null; then
    echo "REFUSED: could not build/list the suite target for: $*" >&2
    return 1
  fi
  if ! grep -qE ': test$' "$out"; then
    echo "REFUSED: listing for '$*' contained no tests; nothing was ruled on" >&2
    return 1
  fi
  return 0
}

n_resolutions=0
list_into "$work/gate.txt" --features "$GATE_FEATURES" || exit 1
n_resolutions=$((n_resolutions + 1))
list_into "$work/all.txt" --all-features || exit 1
n_resolutions=$((n_resolutions + 1))
list_into "$work/def.txt" || exit 1
n_resolutions=$((n_resolutions + 1))

grep -hE ': test$' "$work"/gate.txt "$work"/all.txt "$work"/def.txt | sort -u > "$work/union.txt"
union_total=$(wc -l < "$work/union.txt")
gate_total=$(grep -cE ': test$' "$work/gate.txt")

n_files=0
n_missing=0

for src in "$SUITE_DIR"/*.rs; do
  base="$(basename "$src" .rs)"
  [ "$base" = "main" ] && continue
  # Only modules main.rs registers are compiled at all; an unregistered file is
  # a different defect and not this gate's question.
  grep -qE "^mod $base;" "$SUITE_DIR/main.rs" || continue

  declared=$(grep -cE '^\s*#\[(compio::test|tokio::test|test)\]' "$src")
  [ "$declared" -eq 0 ] && continue

  present=$(grep -cE "^${base}::[A-Za-z0-9_]+: test$" "$work/union.txt")
  n_files=$((n_files + 1))

  if [ "$present" -lt "$declared" ]; then
    n_missing=$((n_missing + 1))
    FAIL=1
    echo "MISSING: $src declares $declared test(s); only $present compile in ANY" \
         "of the three resolutions" >&2
    grep -nE '#!?\[cfg\(' "$src" | sed 's/^/    cfg at /' >&2
  fi
done

# Floors sit well under today's numbers: ordinary deletion must not trip them,
# a collapse of the enumeration must.
gate_arm files_compared "$n_files" 20 || FAIL=1
gate_arm union_tests_listed "$union_total" 300 || FAIL=1
gate_arm resolutions_listed "$n_resolutions" 3 || FAIL=1

if [ "$n_missing" -eq 0 ]; then
  echo "ok: every declared test in $SUITE_DIR compiles in some resolution" \
       "($n_files files, union $union_total)"
fi
# Not a failure, but the number a verifier needs: the set they RUN is not the
# set that EXISTS, and this says by how much.
echo "note: the gate feature set compiles $gate_total of $union_total;" \
     "$((union_total - gate_total)) case(s) only exist under another resolution"

gate_arms_finish || FAIL=1
exit "$FAIL"
