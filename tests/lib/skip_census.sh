# shellcheck shell=bash
#
# Count the tests that announced they did nothing, so a green tally cannot hide
# them.
#
# The failure this exists for: a test gated on a live backend is written as
#
#     let Ok(url) = std::env::var("ZEROSHIP_WORKER_KV_URL") else { ...announce...; return; };
#
# which COMPILES, RUNS, RETURNS EARLY, AND COUNTS AS PASSED. Measured on
# 2026-08-09, `cargo test -p zeroship-plugin-kv --test e2e_runtime` with the var
# unset prints `test e2e_dragonfly ... ok` and closes with
# `10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out`. Two of those ten
# exercised nothing. Nothing in the tally says so, because Rust has no way to
# mark a test ignored at RUNTIME - by the time the test body knows the backend
# is missing, the harness has already committed to counting it.
#
# So the count has to come from the log, and the announcement has to survive the
# harness. `zeroship_test_support::skip` handles the second half: it writes
# straight to the stderr handle, because the harness swaps the thread-local
# target `println!`/`eprintln!` write through and replays that buffer only for a
# FAILING test - an announcement made through a macro is invisible on a pass,
# which is exactly the run where it matters. This file handles the first half.
#
# `tests/lib_skip_census_selftest.sh` covers both directions.

# The token every announcement carries. Kept byte-identical to
# `crates/test-support/src/lib.rs` and to the three verbatim copies in
# `libs/*/tests/common/mod.rs`; one search over a run log has to find every skip
# in the workspace, whichever side of that line it came from.
#
# It is deliberately not a word. Searching for "skip" cannot do this job and
# that is measured: of the 98 lines containing it in one full auth run, 13 were
# real announcements, 5 were the harness's own `test <name> ... ok` for tests
# whose names contain "skips"/"skipped", and ~80 were driver debug output
# echoing an INSERT naming a `skip_consent` column. The marker's hyphens are not
# legal in a Rust identifier, so no test name can forge it.
ZS_SKIP_MARKER='ZEROSHIP-TEST-SKIPPED'

# zs_skip_lines <log> [allowlist_regex]
#
# Print the unique announcement lines, optionally restricted to those NOT
# matching the allowlist.
#
# `grep -a` is load-bearing, not decoration, and the measurement says exactly
# how. A cargo run log is not guaranteed to be text: a panic payload, a driver
# hex dump, or a captured subprocess can put a NUL in it. Measured 2026-08-09 on
# GNU grep 3.12 (what these gates run under) against a log holding one NUL and
# two real announcements:
#
#   grep -F  <marker> log   ->  "grep: log: binary file matches", rc 0
#   grep -cF <marker> log   ->  2
#   grep -aF <marker> log   ->  both announcement lines
#
# So LINE mode - the mode this file uses, because the census has to PRINT which
# backend was missing - silently replaces the findings with a one-line banner.
# Without `-a` the census would report one nameless "skip" instead of two named
# ones, on precisely the run that went strange. Count mode happens to survive on
# GNU grep, which is what makes this worth pinning rather than trusting: the
# same omission is harmless in one mode and lossy in the other. ugrep 7.5.0,
# which is `grep` on an interactive shell in this repo, is stricter still and
# goes silent in EVERY mode including `-c`.
#
# `-a` is applied to the allowlist pass too. That pipeline reads grep's own
# output rather than the file, so it is not at risk today, but a mixed pair
# invites someone to "tidy" the surviving flag away.
zs_skip_lines() {
  local log="${1:-}" allow="${2:-}"
  [ -n "$log" ] && [ -r "$log" ] || return 0
  if [ -n "$allow" ]; then
    grep -aF "$ZS_SKIP_MARKER" "$log" 2>/dev/null | grep -avE "$allow" || true
  else
    grep -aF "$ZS_SKIP_MARKER" "$log" 2>/dev/null || true
  fi
}

# zs_skip_census <log> [allowlist_regex]
#
# Print the census and set ZS_SKIP_COUNT / ZS_SKIP_TOLERATED. Returns 0 when no
# non-allowlisted skip is present, 1 otherwise.
#
# The return value is advisory ON PURPOSE and the caller decides what to do with
# it. Whether a given suite SHOULD have its backend provisioned is an operator
# decision; whether the absence is visible is not. A gate that has provisioned
# the backend treats a non-zero return as failure (see tests/run_auth_suite.sh);
# a gate that has not yet still prints the census, so the gap is reported rather
# than silent.
#
# An EMPTY allowlist must match nothing. This is the trap the whole function has
# to get right: `grep -E ''` matches every line, so passing an unset allowlist
# straight through would tolerate every skip in the run and report a clean
# census - the exact silence this file exists to remove. Hence the branch in
# zs_skip_lines rather than one unconditional pipeline.
zs_skip_census() {
  local log="${1:-}" allow="${2:-}"
  local all offenders

  all="$(zs_skip_lines "$log")"
  offenders="$(zs_skip_lines "$log" "$allow")"

  # `grep -c` counts MATCHING LINES, but an empty string still has one line, so
  # `printf '%s' "" | wc -l` is the honest zero here. Guard both.
  ZS_SKIP_COUNT=0
  ZS_SKIP_TOLERATED=0
  [ -n "$offenders" ] && ZS_SKIP_COUNT="$(printf '%s\n' "$offenders" | wc -l | tr -d ' ')"
  local total=0
  [ -n "$all" ] && total="$(printf '%s\n' "$all" | wc -l | tr -d ' ')"
  ZS_SKIP_TOLERATED=$((total - ZS_SKIP_COUNT))

  echo "==> skip census: ${total} test(s) announced they did nothing (${ZS_SKIP_TOLERATED} allowlisted, ${ZS_SKIP_COUNT} not)"
  if [ "$total" -gt 0 ]; then
    printf '%s\n' "$all" | sed 's/^/    /' | sort -u | head -40
  fi

  [ "$ZS_SKIP_COUNT" -eq 0 ]
}
