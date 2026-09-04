# shellcheck shell=bash
#
# Count the tests that announced they did nothing, so a green tally cannot hide
# them.
#
# The failure this exists for: a test gated on a live backend is written as
#
#     let Ok(url) = std::env::var("ZEROSHIP_KV_URL") else { ...announce...; return; };
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
# THE THIRD DIRECTION, added 2026-08-21: the census REFUSES (status 2) rather
# than reporting zero when its log is missing, unreadable, not a regular file or
# empty. This is an anti-vacuity instrument, so "no input" must not spell
# "nothing to report" - see `zs_skip_log_problem` for the run that proved it.
#
# `tests/lib_skip_census_selftest.sh` covers all three directions.

# The token every announcement carries. Kept byte-identical to
# `crates/zeroship-test-support/src/lib.rs` and to the verbatim `SKIP_MARKER` consts in
# `libs/compio-s3` and `libs/compio-redis`; one search over a run log has to
# find every skip in the workspace, whichever side of that line it came from.
# `libs/compio-postgres` deliberately has neither -- it replaced its announcer
# with a panic, and its `libs/compio-postgres/tests/common/mod.rs` header says
# so. This said "the three verbatim copies in libs/*/tests/common/mod.rs",
# which was one too many and named a file that asserts the opposite.
#
# "Kept byte-identical" was an instruction, not a check, until 2026-08-20:
# `tests/skip_marker_gate.sh` now compares every token in the marker's family
# against this one, because a single wrong character removes every skip
# announced through that copy from this census without any number moving.
#
# It is deliberately not a word. Searching for "skip" cannot do this job and
# that is measured: of the 98 lines containing it in one full auth run, 13 were
# real announcements, 5 were the harness's own `test <name> ... ok` for tests
# whose names contain "skips"/"skipped", and ~80 were driver debug output
# echoing an INSERT naming a `skip_consent` column. The marker's hyphens are not
# legal in a Rust identifier, so no test name can forge it.
ZS_SKIP_MARKER='ZEROSHIP-TEST-SKIPPED'

# The status a census returns when it COULD NOT RULE, as opposed to ruling and
# finding skips. `zs_check_binary_freshness` in tests/lib/binary_freshness.sh
# already uses this split (0 fresh / 1 stale-and-strict / 2 cannot answer) and
# `zeroship_testkit::live_db::REFUSED_EXIT_CODE` is the same 2 for a cargo test
# binary, so a caller reading a status here reads the same vocabulary it reads
# everywhere else in this tree.
#
# WHY IT IS A SEPARATE STATUS AND NOT JUST 1. "This suite skipped tests" and
# "there is no evidence about what this suite did" demand opposite responses:
# the first means provision a backend or allowlist the gap, the second means the
# run never happened and nothing above it can be believed. Collapsing them into
# one non-zero would let a caller's "skips are tolerated here" arm - which is a
# legitimate arm, see the report-only census in ci.yml - swallow a void run.
ZS_SKIP_REFUSED_STATUS=2

# zs_skip_log_problem <log>
#
# Echo the reason <log> cannot be censused at all, and return 0. Return 1, with
# no output, when it is a usable source.
#
# THE FAILURE THIS EXISTS FOR, hit on 2026-08-21 by an acceptance harness whose
# run log went to a path that failed to open. The harness therefore never ran,
# and `zs_skip_census /that/path` printed
#
#     ==> skip census: 0 test(s) announced they did nothing (0 allowlisted, 0 not)
#
# and exited 0. A clean green over a file that does not exist. This library is
# the repo's ANTI-VACUITY instrument - it exists to prove a suite did not
# silently skip its work - so returning success on no input at all is precisely
# the failure it exists to prevent, one level out. Note that the counting arm
# below was already careful about the ADJACENT vacuity (an empty string still
# has one line, so `wc -l` cannot be trusted unguarded); nobody had asked the
# same question about the SOURCE.
#
# The five arms, and why each is a refusal rather than a zero:
#
#   no path         the caller passed nothing; there is no log to be right about
#   does not exist  the run wrote somewhere else, or did not run
#   not a regular   this library reads the log TWICE (once for the total, once
#     file          for the offenders), so a fifo or a directory cannot serve it
#                   even when readable - a fifo yields its bytes to the first
#                   pass and nothing to the second, and grep over a directory
#                   prints "Is a directory" and matches nothing
#   unreadable      permissions; grep would print zero lines and rc 2
#   empty           0 bytes. This is the case the broken redirect ABOVE would
#                   have produced on a writable path, and it is not the same as
#                   a clean run: every producer in this tree pipes cargo through
#                   `tee`, and cargo cannot emit zero bytes and still have run.
zs_skip_log_problem() {
  local log="${1:-}"
  if [ -z "$log" ]; then
    echo "no log path was given"
  elif [ ! -e "$log" ]; then
    echo "no such file: $log"
  elif [ ! -f "$log" ]; then
    echo "not a regular file (this census reads it twice): $log"
  elif [ ! -r "$log" ]; then
    echo "not readable: $log"
  elif [ ! -s "$log" ]; then
    echo "empty, 0 bytes: $log"
  else
    return 1
  fi
  return 0
}

# zs_skip_lines <log> [allowlist_regex]
#
# Print the unique announcement lines, optionally restricted to those NOT
# matching the allowlist. Returns $ZS_SKIP_REFUSED_STATUS, having printed one
# line naming the path on stderr, when the log is not a source it can read;
# it used to `return 0` there, which is the same false green the census had.
# `zs_skip_census` checks the source itself and returns BEFORE calling this, so
# the banner is printed once, not twice.
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
  local log="${1:-}" allow="${2:-}" problem
  if problem="$(zs_skip_log_problem "$log")"; then
    echo "  x REFUSED: skip census log unusable - ${problem}" >&2
    return "$ZS_SKIP_REFUSED_STATUS"
  fi
  if [ -n "$allow" ]; then
    grep -aF "$ZS_SKIP_MARKER" "$log" 2>/dev/null | grep -avE "$allow" || true
  else
    grep -aF "$ZS_SKIP_MARKER" "$log" 2>/dev/null || true
  fi
}

# zs_skip_census <log> [allowlist_regex]
#
# Print the census and set ZS_SKIP_COUNT / ZS_SKIP_TOLERATED / ZS_SKIP_REFUSED.
#
#   0  ruled: no non-allowlisted skip is present
#   1  ruled: there are skips this allowlist does not cover
#   2  REFUSED (= $ZS_SKIP_REFUSED_STATUS): no verdict was reachable, because
#      <log> is missing, unreadable, not a regular file, or empty
#
# A REFUSAL IS NOT A FAILURE, and the distinction is one this tree already
# draws in two places rather than one this function invents.
# `crates/zeroship-testkit/src/live_db.rs` states it directly - "a failure is a
# verdict about the code; a refusal is the statement that no verdict was
# reachable" - and `tests/project_config_gate.sh` acts on it, printing
# `FAIL: no zeroship binary at <path>` and emitting ZERO arm lines rather than
# ruling on nothing. So a refusal here prints NO `==> skip census:` line at all.
# That line is the artefact of the bug: its "0 test(s) announced they did
# nothing" is a positive claim about a run, and a reader who saw it over a
# phantom log would have no way to know it was made of nothing.
#
# ZS_SKIP_COUNT and ZS_SKIP_TOLERATED are set to the EMPTY STRING on a refusal,
# not to 0. They are assigned, so `set -u` callers still work; they are not a
# number, so a caller that reads the count while ignoring the status - the shape
# ci.yml's report-only census had - gets a loud "integer expression expected"
# instead of a quiet zero. ZS_SKIP_REFUSED is the flag to branch on.
#
# STATUS 1 is advisory ON PURPOSE and the caller decides what to do with it.
# Whether a given suite SHOULD have its backend provisioned is an operator
# decision; whether the absence is visible is not. A gate that has provisioned
# the backend treats a 1 as failure (see tests/run_auth_suite.sh); a gate that
# has not yet still prints the census, so the gap is reported rather than
# silent.
#
# STATUS 2 IS NOT ADVISORY, and that asymmetry is the point. Tolerating skips is
# an operator's call about provisioning; tolerating a census that never ran is
# nobody's call, because there is no fact to tolerate. Every caller in this tree
# treats a 2 as a hard failure, including the report-only one - a caller that
# folded 2 back into "|| true" would restore the original bug wearing a
# different hat.
#
# An EMPTY allowlist must match nothing. This is the trap the whole function has
# to get right: `grep -E ''` matches every line, so passing an unset allowlist
# straight through would tolerate every skip in the run and report a clean
# census - the exact silence this file exists to remove. Hence the branch in
# zs_skip_lines rather than one unconditional pipeline.
zs_skip_census() {
  local log="${1:-}" allow="${2:-}"
  local all offenders problem

  # Checked BEFORE anything is counted, so no census line is ever printed over
  # a source that could not be read.
  if problem="$(zs_skip_log_problem "$log")"; then
    ZS_SKIP_REFUSED=1
    ZS_SKIP_COUNT=""
    ZS_SKIP_TOLERATED=""
    {
      echo "=================================================================="
      echo "  x REFUSED: the skip census could not read its log."
      echo "             ${problem}"
      echo
      echo "This is NOT a result. No census was taken, so nothing here says"
      echo "whether the run skipped tests - or whether the run happened at all."
      echo "Do not read the absence of offenders as an absence of skips."
      echo
      echo "Usual cause: the producer's redirect went somewhere it could not"
      echo "write, or died before it wrote, so the suite never ran. Check that"
      echo "the log path is the one the suite tees to, then re-run the suite."
      echo "=================================================================="
    } >&2
    return "$ZS_SKIP_REFUSED_STATUS"
  fi
  ZS_SKIP_REFUSED=0

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
