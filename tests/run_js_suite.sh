#!/usr/bin/env bash
# ============================================================================
# run_js_suite.sh - the CI runner for every JavaScript/TypeScript package suite.
#
# WHY THIS EXISTS
# ---------------
# `pnpm -r test` reports success for two things that are not success:
#
#   MEASURED, pnpm 9.15.4:
#     pnpm -r --no-bail test --filter '@zeroship/no-such-package-xyz'
#       -> "No projects matched the filters"                       EXIT 0
#     pnpm --filter '@zeroship/types' run test   (no `test` script)
#       -> EXIT 0
#
# The second is the one that bites. `pnpm -r` SKIPS a package with no `test`
# script, in silence: nothing in the output separates "16 packages tested" from
# "15 tested and one whose script was renamed". Measured on this tree, the CI
# filter selects 41 projects and only 16 of them run a test script at all, so a
# package dropping out of that 16 is invisible against 25 that legitimately
# have nothing to run.
#
# That is the same failure as the one that left this repo with NO JavaScript
# test gating a merge at all until recently - a state that persisted long enough
# for the @zeroship/db suite to accumulate over a hundred failures and for
# @zeroship/vite-plugin to ship an undeclared dependency.
#
# `--fail-if-no-match` does NOT close this. Measured: it exists on 9.15.4, fires
# on an empty project match (EXIT 1), does not over-fire on a healthy run
# (EXIT 0), and returns EXIT 0 for a package whose `test` script is missing -
# which is this hole. It is also inert here regardless, because the filters
# below are NEGATIVE and a negative filter in a populated workspace never
# matches nothing. Adding it would imply a protection that does not exist.
#
# So: count, and require a floor.
#
# USAGE
#   tests/run_js_suite.sh
#
# ENV
#   JS_MIN_PACKAGES (16)  packages that must actually RUN a test script
#   JS_MIN_TESTS   (800)  total TAP assertions that must pass
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

LOG="$(mktemp)"
trap 'rm -f "$LOG"' EXIT

# The exclusions are debts with owners, not a permanent allow-list; see the
# comment on this step in .github/workflows/ci.yml.
# FILTERS GO BEFORE THE SCRIPT NAME. Anything after it is forwarded TO the
# script, so `pnpm -r test --filter=!x` passes `--filter=!x` to the test command
# and pnpm never sees it. pnpm 9 tolerated the wrong order; pnpm 11 does not.
# Measured: filters after the name give "Scope: 44 of 45" and all three excluded
# packages run; before the name gives "Scope: 41 of 45", which is correct.
set +e
pnpm -r --no-bail \
  --filter='!zero-migrate' \
  --filter='!@zeroship/migrate' \
  --filter='!@zeroship/vite-plugin' \
  test 2>&1 | tee "$LOG"
# PIPESTATUS, not $?: piping into tee makes $? tee's status, which is 0 whenever
# tee could write - every failing run would look green.
run_status="${PIPESTATUS[0]}"
set -e

# Strip ANSI before parsing. pnpm colours the per-package prefix, so an
# unstripped grep matches nothing and would report a confident zero.
clean="$(sed -e 's/\x1b\[[0-9;]*m//g' "$LOG")"

# Packages that actually invoked a `test` script. pnpm prints one
# "<pkg> test$ <command>" header per package it runs.
packages="$(printf '%s\n' "$clean" | grep -oP '^\S+(?= test\$)' | sort -u | wc -l)"

# TAP assertions. NOTE this covers 12 of the 16 running packages: the other four
# (the vendored zero-migrate-node addon, examples/db-todos, examples/ssr-blog,
# sdks/payments) do not emit a TAP summary line. The package count above is the
# check that covers all sixteen; this one bounds mass deletion inside the
# twelve that report.
tests_passed="$(printf '%s\n' "$clean" | grep -oP '# pass \K[0-9]+' | awk '{s+=$1} END {print s+0}')"
tests_failed="$(printf '%s\n' "$clean" | grep -oP '# fail \K[0-9]+' | awk '{s+=$1} END {print s+0}')"

status=0

if [ "$run_status" -ne 0 ]; then
  echo "FAIL: pnpm reported a non-zero exit (${run_status})." >&2
  status=1
fi

# 16 measured on 2026-08-08. Floor 16, not 15: the package set is discrete and
# changes only when someone adds or removes a suite, so there is no noise to
# absorb - and a package silently dropping out is the exact defect this guards.
# Adding a suite means raising this deliberately, which is the point.
JS_MIN_PACKAGES="${JS_MIN_PACKAGES:-16}"
if [ "$packages" -lt "$JS_MIN_PACKAGES" ]; then
  echo "FAIL: only ${packages} packages ran a test script, fewer than the ${JS_MIN_PACKAGES} expected." >&2
  echo "A package whose 'test' script is renamed or removed is SKIPPED SILENTLY by pnpm -r." >&2
  echo "If a suite was deliberately retired, lower JS_MIN_PACKAGES in the same change." >&2
  status=1
fi

# 800 against 862 measured, the same ~7 percent headroom the auth and billing
# gates carry.
JS_MIN_TESTS="${JS_MIN_TESTS:-800}"
if [ "$tests_passed" -lt "$JS_MIN_TESTS" ]; then
  echo "FAIL: only ${tests_passed} JS tests passed, fewer than the ${JS_MIN_TESTS} expected." >&2
  status=1
fi

if [ "$tests_failed" -ne 0 ]; then
  echo "FAIL: ${tests_failed} JS tests failed." >&2
  status=1
fi

echo "=================================================================="
if [ "$status" -ne 0 ]; then
  echo "JS SUITE: FAILED (${packages} packages, ${tests_passed} passed, ${tests_failed} failed)" >&2
  exit 1
fi
# Printed on SUCCESS too. A count nobody sees until the gate has already failed
# cannot warn anyone, and this is the channel where a false green would differ
# from a true one.
echo "JS SUITE: ${packages} packages ran, ${tests_passed} tests passed, 0 failed" \
     "(floors ${JS_MIN_PACKAGES} packages / ${JS_MIN_TESTS} tests)"
