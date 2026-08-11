#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Self-test for the dev-server readiness helpers in tests/lib/e2e_stack.sh.
#
# Needs no services and no database: it sources the real file and drives the
# shipped functions, so a green here is about the code that actually runs.
#
# WHAT THIS IS FOR (#273, and the two harnesses CI will not run).
#
# Every dev-vs-deployed harness waited for its dev server with a fixed
# iteration count -- `for _ in $(seq 1 25); do probe && break; sleep 2; done` --
# sized on an idle machine, and reported a single string when the count ran
# out: "dev app never came up". That string is a conclusion, not an
# observation, and it is wrong in at least two measured situations:
#
#   1. Cold dependency cache. Measured 2026-08-11 on the storage harness: run 1
#      RED, run 2 green, same code. vite bound its port and was "ready in
#      806 ms"; the runtime came up and registered all four plugins. What
#      expired was the 40 s budget, while vite logged "Re-optimizing
#      dependencies because lockfile has changed" -- twice.
#
#   2. Host contention. The CI file records this from the other direction:
#      e2e_dev_vs_deployed_workflows.sh is 3/3 green idle and 3/3 RED on 12
#      busy cores, one mode being "the runtime missed the 25 x 2s readiness
#      window". Its own comment concludes "every one is a timing budget", and
#      that is the stated reason it and the storage harness are NOT wired into
#      CI.
#
# So the fix is not a bigger number -- a bigger fixed number just moves the
# cliff. It is (a) a deadline in SECONDS that a loaded host can be given
# explicitly, and (b) a timeout message that reports WHICH of the failure modes
# actually happened, read off the dev log rather than assumed.
#
# The classifier is the part worth pinning, because it is the part that can be
# silently wrong: a wrong classification still prints, still looks confident,
# and sends the next reader at the wrong layer. Each case below is a synthetic
# log -- no timing, no services, deterministic -- and each asserts the SPECIFIC
# diagnosis, never merely that some text appeared.
#
# What this does NOT test: that the poll loop honours the deadline in real
# time (that would be a sleep-bound test asserting the OS scheduler), or that
# any particular harness now passes cold. The first is not worth the wall
# clock; the second is a run, recorded in the commit, not a unit test.
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/tests/lib/e2e_stack.sh"

T=0; BAD=0
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# expect_diag <label> <logfile-content> <substring the diagnosis MUST contain>
expect_diag() {
  local label="$1" content="$2" want="$3"
  local f="$WORK/log.$T"
  T=$((T+1))
  printf '%s' "$content" > "$f"
  local got
  got="$(stack_dev_diagnosis "$f" 2>&1)"
  if [[ "$got" == *"$want"* ]]; then
    echo "  ok   $label"
  else
    echo "  FAIL $label"
    echo "         wanted substring: $want"
    echo "         got:              $got"
    BAD=$((BAD+1))
  fi
}

# reject_diag <label> <logfile-content> <substring the diagnosis MUST NOT contain>
# The negative half matters as much as the positive one: a classifier that
# answers "still optimising dependencies" for EVERY input would pass all four
# positive cases above and be useless.
reject_diag() {
  local label="$1" content="$2" unwanted="$3"
  local f="$WORK/log.$T"
  T=$((T+1))
  printf '%s' "$content" > "$f"
  local got
  got="$(stack_dev_diagnosis "$f" 2>&1)"
  # A negative assertion passes vacuously when the function is MISSING: the
  # shell's "command not found" contains no unwanted substring either. That is
  # not hypothetical -- on this file's first red run, all three reject_ cases
  # reported ok against a classifier that did not exist yet, while every
  # positive case failed. So require a real diagnosis first, and only then that
  # it withholds the wrong claim.
  if [ -z "$got" ] || [[ "$got" == *"command not found"* ]]; then
    echo "  FAIL $label: no diagnosis was produced at all (got: '$got')"
    BAD=$((BAD+1))
  elif [[ "$got" != *"$unwanted"* ]]; then
    echo "  ok   $label"
  else
    echo "  FAIL $label: diagnosis wrongly claimed '$unwanted'"
    echo "         got: $got"
    BAD=$((BAD+1))
  fi
}

echo "=== dev-server readiness diagnosis ==="

# The measured case 1: vite is up and ready, but was re-optimising deps. This
# is the one that cost a full cycle on 2026-08-11 by reading as a defect in an
# unrelated change.
REOPT=$'(node:1) ExperimentalWarning: SQLite is an experimental feature\n[vite] (zeroship) Re-optimizing dependencies because lockfile has changed\n[vite] (client) Re-optimizing dependencies because lockfile has changed\n[zeroship] API server starting on :3081\n  VITE v8.0.10  ready in 806 ms\n[zeroship] storage plugin registered (backend=local)\n'
expect_diag "re-optimising deps is named as such" "$REOPT" "re-optimising dependencies"

# vite never even reached "ready": a genuinely stuck or crashed dev server.
NEVER=$'(node:1) ExperimentalWarning: SQLite is an experimental feature\n[vite] (zeroship) Re-optimizing dependencies because lockfile has changed\n'
expect_diag "no ready line is reported as never-ready" "$NEVER" "never reported ready"

# vite ready, no dep work, app still silent -> the app is the suspect, and the
# message must NOT blame dependency optimisation.
APPFAIL=$'  VITE v8.0.10  ready in 512 ms\n[zeroship] API server starting on :3081\n[zeroship] db plugin registered (DATABASE_URL set)\n'
expect_diag "ready-but-silent points at the app" "$APPFAIL" "the app never answered"
reject_diag "ready-but-silent does NOT blame deps" "$APPFAIL" "re-optimising dependencies"

# A port conflict under --strictPort: the harness must not call this "the app
# never answered", because the app was never given a port to answer on. This
# is the #272 failure mode, and it now has a named arm.
PORTBUSY=$'error when starting dev server:\nError: Port 5081 is already in use\n'
expect_diag "strictPort conflict is named" "$PORTBUSY" "port was already in use"
reject_diag "port conflict does NOT blame the app" "$PORTBUSY" "the app never answered"

# The runtime's own blocked-port refusal (bad_ports.rs). Measured on the stream
# harness with VITE_PORT=5061: the app really never came up, but the cause is
# the port number, and "dev app never came up" sent me looking at the app.
BLOCKED=$'  VITE v8.0.10  ready in 700 ms\n[zeroship] API server starting on :3061\n{"level":"ERROR","fields":{"message":"creator app dispatch error","error.message":"Network request failed: network error: blocked port 5061"}}\n'
expect_diag "blocked port is named" "$BLOCKED" "blocked port"
reject_diag "blocked port does NOT blame deps" "$BLOCKED" "re-optimising dependencies"

# Empty log: the process produced nothing at all. Must be distinguishable from
# "ready but silent", because it means the launch itself failed.
expect_diag "empty log is reported as no output" "" "produced no output"

# A log the classifier has no rule for must still say so, rather than falling
# through to whichever arm happens to be last.
UNKNOWN=$'some completely unrelated output\nnothing recognisable here\n'
expect_diag "unrecognised log says so" "$UNKNOWN" "no recognised"

echo
echo "  dev-readiness selftest: $((T-BAD)) passed, $BAD failed  (floor $T)"
[ "$T" -ge 10 ] || { echo "  FAIL selftest asserted fewer cases than it declares"; exit 1; }
[ "$BAD" -eq 0 ] || exit 1
exit 0
