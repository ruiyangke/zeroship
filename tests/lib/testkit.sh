# shellcheck shell=bash
# ============================================================================
# testkit.sh - find `zs-testkit`, the binary the other tests/lib files call.
#
# WHY THERE IS A BINARY AT ALL. The harness's database logic moved to Rust
# (crates/zeroship-testkit) so `compio-postgres` could replace `psql`. The
# CONSUMERS are still shell - tests/run_auth_suite.sh sources tests/lib/
# suite_db.sh and calls zs_suite_db_provision - so the shell functions kept
# their names and their contracts, and their bodies became one call each. No
# consumer changed.
#
# WHY IT BUILDS RATHER THAN REQUIRING A BUILD. A harness that fails with
# "no such file" the first time somebody runs it on a fresh clone is a harness
# people stop running. `cargo build` on an up-to-date crate is fast; the guard
# below is about not paying even that.
#
# WHY THE MTIME GUARD IS NOT AN OPTIMISATION. Two failure modes it exists for,
# and both have happened in this repository:
#
#   - A STALE BINARY REPORTS WHATEVER WAS BUILT LAST. Picking an artifact by
#     existence measures the last build, not this tree. That has produced a
#     "reproduced" bug that was fixed hours earlier, and a green gate that was
#     testing deleted code.
#   - `cargo build` TAKES THE TARGET-DIRECTORY LOCK. Several agents share this
#     checkout's target/; an unconditional build in a fast selftest would block
#     behind whichever of them is mid-compile. Skipping the build when the
#     binary is already newer than every source file keeps the common path off
#     that lock entirely.
#
# WHAT THE GUARD DOES NOT COVER: a change in a DEPENDENCY (compio-postgres,
# zeroship-schema) with this crate's own sources untouched. Cargo would rebuild;
# this will not. Run `cargo build -p zeroship-testkit` by hand after such a
# change, or touch a file here. It is a real hole and it is stated rather than
# papered over, because the alternative is paying the lock on every selftest.
# ============================================================================

# Print the path to a `zs-testkit` that is at least as new as its sources.
#
# Usage: zs_testkit_bin
# The result is memoised in this shell, so a script that calls forty helpers
# pays the check once.
zs_testkit_bin() {
  if [ -n "${ZS_TESTKIT_BIN_CACHED:-}" ]; then
    printf '%s' "$ZS_TESTKIT_BIN_CACHED"
    return 0
  fi

  local root src bin newer
  root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
  src="$root/crates/zeroship-testkit"
  bin="$root/target/debug/zs-testkit"

  newer=1
  if [ -x "$bin" ]; then
    # Any source file newer than the binary means the binary is not this tree's.
    newer="$(find "$src/src" "$src/Cargo.toml" -newer "$bin" -print -quit 2>/dev/null)"
    [ -z "$newer" ] && newer=0 || newer=1
  fi

  if [ "$newer" != "0" ]; then
    if ! (cd "$root" && cargo build -q -p zeroship-testkit >&2); then
      echo "FATAL: could not build zs-testkit (cargo build -p zeroship-testkit)." >&2
      echo "       Every tests/lib helper below the database line calls it." >&2
      return 2
    fi
  fi

  ZS_TESTKIT_BIN_CACHED="$bin"
  printf '%s' "$bin"
}

# Run a `zs-testkit` subcommand, forwarding its stdout, stderr and exit status.
#
# The `|| return $?` shape is deliberate: both suite scripts run under
# `set -euo pipefail`, where a helper's ordinary "no" answer - exit 1 - would
# otherwise take the whole harness down before the caller's own `case` ran.
zs_testkit() {
  local bin
  bin="$(zs_testkit_bin)" || return $?
  "$bin" "$@"
}
