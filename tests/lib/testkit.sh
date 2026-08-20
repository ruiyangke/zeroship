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

# Set ZS_TESTKIT_BIN to a `zs-testkit` at least as new as its sources.
#
# NOT a function that PRINTS the path. That is the shape this started as, and it
# does not memoise: `$(zs_testkit_bin)` runs the body in a subshell, so the
# variable it sets dies with the substitution and every call re-runs the `find`.
# The sweeper calls a helper once per git ref, so "once per shell" and "once per
# call" are 1 and 21.
#
# THERE IS NO OVERRIDE. The path is DERIVED from this file's own location every
# time, never taken from a variable, so no exported name can point the harness
# at somebody else's binary - the same reason `--database` is a flag and an
# ambient TEST_DB is refused. `ZS_TESTKIT_CHECKED` only records that the
# freshness check already ran, and it is keyed on the derived path, so a value
# inherited from another checkout does not match and is simply recomputed.
zs_testkit_resolve_bin() {
  local root src newer
  root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
  src="$root/crates/zeroship-testkit"
  ZS_TESTKIT_BIN="$root/target/debug/zs-testkit"
  [ "${ZS_TESTKIT_CHECKED:-}" = "$ZS_TESTKIT_BIN" ] && return 0
  local bin="$ZS_TESTKIT_BIN"

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

  ZS_TESTKIT_CHECKED="$ZS_TESTKIT_BIN"
}

# Run a `zs-testkit` subcommand, forwarding its stdout, stderr and exit status.
#
# The `|| return $?` on the resolve is deliberate: both suite scripts run under
# `set -euo pipefail`, and a build failure has to reach the caller as a status
# rather than as a missing file two lines later.
zs_testkit() {
  zs_testkit_resolve_bin || return $?
  "$ZS_TESTKIT_BIN" "$@"
}
