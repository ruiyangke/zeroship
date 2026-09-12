# shellcheck shell=bash
# ============================================================================
# sweep_db.sh - the decisions tests/sweep_test_databases.sh makes, separated
# from the server it makes them against.
#
# THE LOGIC IS NOW IN RUST: xtask/src/platform_db/sweep.rs, reached
# through `zs-testkit sweep`. This file is the shell BINDING - it keeps the
# function names and the two variables the sweeper reads, so that script did
# not change. The Rust module carries the design notes.
#
# They are in their own file because they are the part that can be WRONG
# SILENTLY. The sweeper's other half either connects or does not; these four
# functions produce a VERDICT, and a wrong verdict is a dropped database.
#
# THE SWEEPER MUST NEVER DROP `WITH (FORCE)`, and the asymmetry against
# tests/lib/scratch_db.sh - which uses it, correctly - is the whole safety
# model. FORCE terminates every other backend on the database before dropping.
# scratch_db drops a database ITS OWN run created, where the only connections
# left are its own stragglers and a plain DROP would leak the database on them.
# The sweeper drops databases OTHER runs created; a live connection there is a
# peer agent mid-suite, and FORCE would kill it. A plain DROP failing with "is
# being accessed by other users" is not an inconvenience for the sweeper - it
# is the answer, and the last line of defence behind the liveness scan here.
#
# The one that matters most is `zs_fingerprint_of_ref`. It recomputes
# `zs_schema_fingerprint` from a git tree instead of from a directory, and the
# two MUST agree byte for byte. If they ever disagree, every database on the
# server matches no reachable branch, the sweeper calls the lot dead, and
# `--apply` deletes every agent's work at once. That is the single most
# destructive bug this design admits, so the selftest pins the agreement
# against the real repository rather than against a fixture.
#
# tests/lib_sweep_db_selftest.sh covers all four in both directions.
# ============================================================================

# shellcheck source=tests/lib/testkit.sh
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/testkit.sh"

# The database families the sweeper owns. Deliberately a short explicit list
# rather than a `zeroship%` wildcard: `zeroship` itself is the dev platform
# database on the shared cluster and carries real rows. The list lives in
# xtask/src/platform_db/sweep.rs; this is here so a shell caller can still
# print it.
ZS_SWEEP_FAMILIES=(zeroship_auth_test zeroship_billing_test)

# Print the family a database name belongs to, or fail.
#
# A name matches a family only if it IS the family or the family followed by
# `_`. The prefix test has to be anchored that way: `zeroship` is a prefix of
# every name here, and a sweeper that treated prefixes loosely would put the
# platform's own database on the list.
zs_sweep_family_of() {
  zs_testkit sweep family-of --name "$1"
}

# Recompute zs_schema_fingerprint from a git tree instead of a directory.
#
# Usage: zs_fingerprint_of_ref <repo> <ref>
#
# Byte-for-byte the same construction as the working-tree one - `<basename> `
# then the sha256 of the file's bytes, sorted, hashed, truncated to 12. Read the
# header above before changing either one.
#
# The repository is an ARGUMENT, mirroring `zs_schema_fingerprint <root>`. It
# used to be whatever directory the caller happened to be in, which agreed with
# the `git for-each-ref` that produced the ref only by coincidence - and left
# the selftest no way to ask about a tree it built itself.
zs_fingerprint_of_ref() {
  zs_testkit fingerprint ref --repo "$1" --ref "$2"
}

# Is `pid` this shell or one of its descendants?
#
# Our own subshells inherit our command line and our environment, so a /proc
# scan for a name we are examining will otherwise find US and report every
# candidate live. Walks the ppid chain rather than comparing command lines,
# which is what makes it independent of what the caller was invoked with.
#
# `ZS_SWEEP_SELF_PID` is read HERE and passed as an argument. It is a shell
# variable, not an exported one, and the binary reads no environment at all -
# so the caller sets it in the shell they are testing and the answer follows
# the argument rather than an ambient value the callee happened to inherit.
zs_pid_is_ours() {
  zs_testkit sweep pid-is-ours --self-pid "${ZS_SWEEP_SELF_PID:-$$}" --pid "$1"
}

# Scan /proc ONCE for every name in the file `$1`, filling `ZS_SWEEP_HELD_BY`
# with `<name> <pid>` lines for the names something on this box is holding, and
# setting the `ZS_SWEEP_PROC_*` variables with the pass's account of ITSELF.
#
# The ENVIRONMENT is the half that earns this scan its place. A suite run
# exports PG_TEST_URL=postgres://.../<db>, so the name sits in
# /proc/<pid>/environ from the first line of the script to the last - INCLUDING
# the minutes it spends in cargo with no backend attached at all, which is
# exactly when pg_stat_activity sees nothing and a sweeper would call the
# database dead.
#
# AND THE PASS SAYS WHAT IT COULD NOT SEE. `ZS_SWEEP_PROC_UNREADABLE` used to
# be one number covering four unrelated situations; it read 406 of 495 on an
# ordinary run and was printed and ignored. The variables below separate the
# entries with an explanation - the process exited, or the kernel protects its
# environment - from the ones without, and `zs_sweep_decide` rules on them:
#
#   ZS_SWEEP_PROC_EXAMINED      entries this pass ruled on
#   ZS_SWEEP_PROC_VANISHED      entries whose process had already exited
#   ZS_SWEEP_PROC_ENV_DENIED    entries whose argv was read and environ was not
#   ZS_SWEEP_PROC_PEER_ENV_READ environments read for processes not ours; ZERO
#                               means the environment half of the scan is dead
#   ZS_SWEEP_PROC_UNEXPLAINED   live entries with no readable argv - the gap
#   ZS_SWEEP_PROC_UNLISTABLE    /proc could not be listed at all
#   ZS_SWEEP_PROC_HIDDEN        /proc did not list pid 1
#
# A NON-ZERO RETURN MUST REACH THE CALLER. It sets the variables to a state
# that reads as "nothing is holding anything", which is precisely the answer a
# failed scan must not be allowed to give quietly.
zs_sweep_scan_holders() {
  local assignments status
  assignments="$(zs_testkit sweep scan \
    --self-pid "${ZS_SWEEP_SELF_PID:-$$}" --patterns "$1")"
  status=$?
  if [ "$status" -ne 0 ]; then
    ZS_SWEEP_HELD_BY=""
    ZS_SWEEP_PROC_EXAMINED=0
    ZS_SWEEP_PROC_VANISHED=0
    ZS_SWEEP_PROC_ENV_DENIED=0
    ZS_SWEEP_PROC_PEER_ENV_READ=0
    ZS_SWEEP_PROC_UNEXPLAINED=""
    ZS_SWEEP_PROC_UNLISTABLE=0
    ZS_SWEEP_PROC_HIDDEN=0
    return "$status"
  fi
  eval "$assignments"
}

# The pids holding `$1`, or the empty string.
zs_sweep_holders_of() {
  printf '%s' "${ZS_SWEEP_HELD_BY:-}" | zs_testkit sweep holders-of --name "$1"
}

# May this run drop what it planned to drop?
#
# Usage: zs_sweep_decide <doomed> <scan-ok 0|1> <sessions-ok 0|1> \
#                        <git-ok 0|1> <worktrees-seen> <worktrees-fingerprinted>
#
# Exit 0 to proceed, 3 to REFUSE. The `/proc` half comes from the variables
# zs_sweep_scan_holders set; everything else is the caller's, because it is a
# question about a server and a repository rather than about a process table.
#
# THE POINT IS THE EXIT CODE. "Scanned everything, nothing is dead" and "could
# not scan, so nothing looked alive" were both 0, and the second one dropped
# databases. There is no flag that turns this off and no variable that steers
# it: an ambient opt-out is how gates get silently disabled.
zs_sweep_decide() {
  zs_testkit sweep decide \
    --doomed "$1" \
    --proc-scan-ok "$2" \
    --sessions-ok "$3" \
    --git-listing-ok "$4" \
    --worktrees-seen "$5" \
    --worktrees-fingerprinted "$6" \
    --proc-unlistable "${ZS_SWEEP_PROC_UNLISTABLE:-0}" \
    --proc-hidden "${ZS_SWEEP_PROC_HIDDEN:-0}" \
    --proc-examined "${ZS_SWEEP_PROC_EXAMINED:-0}" \
    --proc-peer-env-read "${ZS_SWEEP_PROC_PEER_ENV_READ:-0}" \
    --proc-unexplained "${ZS_SWEEP_PROC_UNEXPLAINED:-}"
}
