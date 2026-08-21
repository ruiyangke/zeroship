#!/usr/bin/env bash
# Self-test for tests/lib/sweep_db.sh.
#
# This library decides which databases a `--apply` run destroys, so the cases
# below are chosen by BLAST RADIUS rather than by coverage:
#
#   the two fingerprint computations MUST agree   - they are the same hash
#                                                   derived two ways, from a
#                                                   directory and from a git
#                                                   tree. Disagree, and every
#                                                   database matches no
#                                                   reachable branch, the
#                                                   sweeper calls all of them
#                                                   dead, and one --apply takes
#                                                   out every agent at once
#   the family test MUST NOT claim `zeroship`     - `zeroship` is the dev
#                                                   platform database with real
#                                                   rows in it, and `zeroship`
#                                                   is a prefix of every name
#                                                   the sweeper does own
#   the /proc scan MUST NOT see itself            - our own subshells inherit
#                                                   our environment, so a scan
#                                                   that found them would call
#                                                   every candidate live and
#                                                   the sweeper would never
#                                                   reclaim anything
#   ...and it MUST see a real holder              - the partner of the case
#                                                   above. Only the pair tells
#                                                   a working scan apart from
#                                                   one that always says "no"
#
# Run directly: tests/lib_sweep_db_selftest.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
# shellcheck source=tests/lib/suite_db.sh
. "$ROOT/tests/lib/suite_db.sh"
# shellcheck source=tests/lib/sweep_db.sh
. "$ROOT/tests/lib/sweep_db.sh"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail=0; pass=0
ok()  { pass=$((pass + 1)); echo "ok   - $1"; }
bad() { fail=$((fail + 1)); echo "FAIL - $1" >&2; }
check() { if [ "$2" = "$3" ]; then ok "$1 ($3)"; else bad "$1: expected '$2', got '$3'"; fi }

echo "=== the fingerprint: both computations agree, and it discriminates ==="
# THE AGREEMENT is pinned against the real tree, not a fixture. A fixture would
# pin the two implementations to each other and prove nothing about the 34 files
# the sweeper actually reasons over - and it is those files, at their real sizes
# and real names, that either agree or delete everything. The control after it
# is the opposite case and is built rather than found; see there for why.
from_dir="$(zs_schema_fingerprint "$ROOT")"
from_ref="$(zs_fingerprint_of_ref "$ROOT" HEAD)"
check "working tree and HEAD agree" "$from_dir" "$from_ref"
if [ -n "$from_dir" ]; then ok "and the value is non-empty ($from_dir)"; else bad "empty fingerprint"; fi

# One variable changed: a migration set that differs by exactly one file. If the
# two computations above agreed because BOTH return a constant, that case passes
# anyway - so it needs this one beside it, and this one has to be the thing that
# would notice.
#
# CONSTRUCTED, NOT SAMPLED. This used to take a commit 40 back on the branch and
# assume its migrations differed. That bound the control to the repository's
# recent ACTIVITY rather than to the variable under test: no migration had
# changed in the last 108 commits, both sides hashed to the same value, and the
# case could not pass however well the fingerprint worked. The two trees below
# differ by one edited migration whatever the log looks like.
#
# TREES RATHER THAN COMMITS: `git write-tree` needs no user identity, runs no
# hooks and signs nothing, so this cannot go red for a reason unrelated to the
# fingerprint. `zs_fingerprint_of_ref` takes any tree-ish - the no-migrations
# case below already hands it a bare tree sha.
#
# `add -A -f`: the force is against a GLOBAL excludes file. Nothing in this
# throwaway tree is ignorable, and a `*.ts` line in somebody's ~/.gitignore
# would otherwise stage an empty set and take the case down with it.
CTRL="$TMP/control-repo"
mkdir -p "$CTRL/db/migrations-ts"
printf 'one' > "$CTRL/db/migrations-ts/20260101_one.ts"
printf 'two' > "$CTRL/db/migrations-ts/20260202_two.ts"
git -C "$CTRL" init -q . >/dev/null 2>&1
git -C "$CTRL" add -A -f
ctrl_before="$(git -C "$CTRL" write-tree 2>/dev/null)"
printf 'one, but different' > "$CTRL/db/migrations-ts/20260101_one.ts"
git -C "$CTRL" add -A -f
ctrl_after="$(git -C "$CTRL" write-tree 2>/dev/null)"
fp_before="$(zs_fingerprint_of_ref "$CTRL" "$ctrl_before")"
fp_after="$(zs_fingerprint_of_ref "$CTRL" "$ctrl_after")"
if [ -z "$ctrl_before" ] || [ -z "$ctrl_after" ]; then
  bad "control: could not build the throwaway repository at $CTRL"
elif [ "$ctrl_before" = "$ctrl_after" ]; then
  bad "control: the two constructed trees are the same object ($ctrl_before)"
elif [ -z "$fp_before" ] || [ -z "$fp_after" ]; then
  bad "control: a constructed tree produced no fingerprint ('$fp_before' / '$fp_after')"
elif [ "$fp_before" = "$fp_after" ]; then
  bad "control: editing one migration did not move the fingerprint ($fp_before)"
else
  ok "control: editing one migration moves the fingerprint ($fp_before then $fp_after)"
fi

echo
echo "=== a ref with no migrations directory FAILS rather than returning a value ==="
# A fingerprint for a tree with no migrations would be a legitimate-looking
# hash that no working tree can ever produce, so every database keyed to it
# would look reachable - or, worse, the empty digest would be shared by every
# such ref.
empty_tree="$(git hash-object -t tree /dev/null)"
( zs_fingerprint_of_ref "$ROOT" "$empty_tree" ) >"$TMP/empty.out" 2>/dev/null
check "an empty tree is refused" "1" "$?"
if [ ! -s "$TMP/empty.out" ]; then ok "and prints nothing"; else bad "printed '$(cat "$TMP/empty.out")'"; fi

echo
echo "=== the family test claims the suite databases and NOTHING else ==="
for name in zeroship_auth_test zeroship_auth_test_s46 zeroship_auth_test_314701e327c4 \
            zeroship_billing_test zeroship_billing_test_v65v; do
  if zs_sweep_family_of "$name" >/dev/null; then ok "owns $name"; else bad "did not own $name"; fi
done
# The ones a loose prefix test would swallow. `zeroship` is the dev platform
# database and carries real rows; the rest belong to other tools on the same
# cluster.
for name in zeroship postgres template0 template1 zeroship_control_test_s8 \
            compio_pg_s8 zeroship_billing_v41 zs_wf_engine_restart_f897474e \
            zeroship_auth_r9_jwks; do
  if zs_sweep_family_of "$name" >/dev/null; then
    bad "CLAIMED $name, which it does not own"
  else
    ok "leaves $name alone"
  fi
done

echo
echo "=== zs_pid_is_ours: this shell and its children, and nothing else ==="
if zs_pid_is_ours "$$"; then ok "recognises this shell"; else bad "did not recognise \$\$"; fi
# Backgrounded from THIS shell, not from inside `$( )`. A `&` inside a command
# substitution is a child of the substitution subshell, which exits at once and
# leaves the process reparented to init - so it is genuinely not our descendant
# and the function was right to say so.
sleep 30 >/dev/null 2>&1 &
child=$!
if zs_pid_is_ours "$child"; then ok "recognises a child ($child)"; else bad "missed child $child"; fi
kill "$child" 2>/dev/null
if zs_pid_is_ours 1; then bad "claimed pid 1 as ours"; else ok "does not claim pid 1"; fi

# A self-pid of 0 or 1 must not swallow the whole process table. Every ancestry
# walk ends at 1 and then 0, so a naive order would match on the last hop of
# EVERY walk and report every process as ours - a sweeper that reclaims nothing
# and says so in the shape of success.
ZS_SWEEP_SELF_PID=1
if zs_pid_is_ours 1; then bad "self-pid 1 makes pid 1 'ours'"; else ok "self-pid 1 does not swallow the walk"; fi
ZS_SWEEP_SELF_PID=0
if zs_pid_is_ours 1; then bad "self-pid 0 makes every walk terminate as 'ours'"; else ok "self-pid 0 does not either"; fi
ZS_SWEEP_SELF_PID="$$"

echo
echo "=== the /proc scan finds a real holder, and not itself ==="
# The holder is a process whose ENVIRONMENT carries the name and whose command
# line does not - which is exactly the shape of a suite run sitting in cargo
# with no database session open, the case pg_stat_activity cannot see.
NEEDLE="zeroship_auth_test_ffffffffffff"
printf '%s\n' "$NEEDLE" > "$TMP/patterns"

ZS_SWEEP_SELF_PID="$$"
zs_sweep_scan_holders "$TMP/patterns"
before="$(zs_sweep_holders_of "$NEEDLE")"
if [ -z "$before" ]; then
  ok "nothing holds the name before the holder starts"
else
  bad "something already held it: $before"
fi

# The fake holder has to be backgrounded from this shell, which makes it a
# DESCENDANT of this shell - the very thing the scan excludes. So the two cases
# below differ in exactly one variable: which pid the scan is told is its own.
# A pid that is in no ancestry chain stands in for "the sweeper, run from
# somewhere else"; `$$` is the real thing.
NOT_AN_ANCESTOR=2147483647

env "PG_TEST_URL=postgres://u:p@h:5432/${NEEDLE}" sleep 60 >/dev/null 2>&1 &
holder=$!
sleep 1
ZS_SWEEP_SELF_PID="$NOT_AN_ANCESTOR"
zs_sweep_scan_holders "$TMP/patterns"
found="$(zs_sweep_holders_of "$NEEDLE")"
ZS_SWEEP_SELF_PID="$$"
zs_sweep_scan_holders "$TMP/patterns"
excluded="$(zs_sweep_holders_of "$NEEDLE")"
kill "$holder" 2>/dev/null
case " $found " in
  *" $holder "*) ok "found the holder ($holder) through its ENVIRONMENT alone" ;;
  *) bad "missed the holder $holder; scan reported '$found'" ;;
esac
case " $excluded " in
  *" $holder "*) bad "a descendant of the scanning shell was reported as a holder" ;;
  *) ok "and the same process is excluded when it IS the scanner's descendant" ;;
esac

echo
echo "=== substring is not membership ==="
# `zeroship_auth_test` is a substring of `zeroship_auth_test_s46`. A holder of
# the long name must not make the short one look held, or the bare family names
# could never be reclaimed while anything at all was running.
printf 'zeroship_auth_test\nzeroship_auth_test_s46\n' > "$TMP/patterns2"
env "PG_TEST_URL=postgres://u:p@h:5432/zeroship_auth_test_s46" sleep 60 >/dev/null 2>&1 &
holder3=$!
sleep 1
ZS_SWEEP_SELF_PID="$NOT_AN_ANCESTOR"   # so the fake holder is not excluded
zs_sweep_scan_holders "$TMP/patterns2"
ZS_SWEEP_SELF_PID="$$"
long="$(zs_sweep_holders_of zeroship_auth_test_s46)"
short="$(zs_sweep_holders_of zeroship_auth_test)"
kill "$holder3" 2>/dev/null
case " $long " in *" $holder3 "*) ok "the held name is reported" ;; *) bad "missed the long name: '$long'" ;; esac
if [ -z "$short" ]; then
  ok "and its PREFIX is not"
else
  bad "the prefix zeroship_auth_test was reported held by '$short'"
fi

echo
echo "=== the verdict: an empty answer and an absent check are not the same ==="
# THE CASE THIS LIBRARY GOT WRONG. Every check above answers with a SET, and an
# empty set has two unrelated causes - nothing is alive, or the check could not
# look. Those printed the same table and the same exit 0, and the second one
# dropped a peer agent's database. The pair below is what separates them, and it
# is a PAIR on purpose: the green case is the control, because a sweeper that
# refuses when it successfully found nothing is a sweeper nobody runs.
#
# The `/proc` half is STATED here rather than taken from the live scan above.
# What is under test below is the DECISION; the scan that produces these
# numbers is covered by its own cases and by `cargo test -p zeroship-testkit`.
# Reading them from a real pass would make every case here a function of what
# else happens to be running on the box.
ZS_SWEEP_PROC_UNLISTABLE=0
ZS_SWEEP_PROC_HIDDEN=0
ZS_SWEEP_PROC_EXAMINED=495
ZS_SWEEP_PROC_PEER_ENV_READ=77
ZS_SWEEP_PROC_UNEXPLAINED=""

decide_green() { zs_sweep_decide "$1" 1 1 1 9 9; }

decide_green 3 >"$TMP/decide.out" 2>&1
check "a complete scan with 3 to drop proceeds" "0" "$?"
if [ ! -s "$TMP/decide.out" ]; then ok "and says nothing"; else bad "printed '$(cat "$TMP/decide.out")'"; fi

# CONTROL: one variable against every red case below - the population is empty
# rather than the evidence incomplete. This must stay 0, or the tool becomes one
# nobody runs and the leak it was built for comes back.
decide_green 0 >"$TMP/decide.out" 2>&1
check "a complete scan with nothing to drop also proceeds" "0" "$?"

# One variable each, all with the same 3 on the drop list.
zs_sweep_decide 3 0 1 1 9 9 >"$TMP/decide.out" 2>&1
check "a /proc scan that did not run REFUSES" "3" "$?"
if grep -q 'proc_scan' "$TMP/decide.out"; then ok "and names the scan"; else bad "did not name it: $(cat "$TMP/decide.out")"; fi

zs_sweep_decide 3 1 0 1 9 9 >"$TMP/decide.out" 2>&1
check "a pg_stat_activity query that did not return REFUSES" "3" "$?"
if grep -q 'sessions' "$TMP/decide.out"; then ok "and names the query"; else bad "did not name it: $(cat "$TMP/decide.out")"; fi

zs_sweep_decide 3 1 1 1 9 8 >"$TMP/decide.out" 2>&1
check "a working tree with no fingerprint REFUSES" "3" "$?"
if grep -q 'worktrees' "$TMP/decide.out"; then ok "and names the trees"; else bad "did not name it: $(cat "$TMP/decide.out")"; fi

# Incomplete evidence with an EMPTY drop list warns and proceeds: every gap can
# only ADD to that list, so an empty one is a conclusion no further evidence
# could overturn. It is still said out loud - a silent count is the defect.
zs_sweep_decide 0 1 0 1 9 9 >"$TMP/decide.out" 2>&1
check "a gap with nothing to drop proceeds" "0" "$?"
if grep -q 'WARNING' "$TMP/decide.out"; then ok "and warns out loud"; else bad "said nothing: $(cat "$TMP/decide.out")"; fi

# The environment half of the scan going dead is its own case, and it is the one
# the scan exists for: a peer's suite sitting in cargo holds no backend and
# appears only in PG_TEST_URL.
ZS_SWEEP_PROC_PEER_ENV_READ=0
zs_sweep_decide 3 1 1 1 9 9 >"$TMP/decide.out" 2>&1
env_blind_status=$?
ZS_SWEEP_PROC_PEER_ENV_READ=77
check "no peer environment read REFUSES" "3" "$env_blind_status"
if grep -q 'env_blind' "$TMP/decide.out"; then ok "and names the environment half"; else bad "did not name it: $(cat "$TMP/decide.out")"; fi

echo
echo "=================================================================="
echo "sweep db selftest: ${pass} passed, ${fail} failed"
[ "$fail" -eq 0 ] || exit 1
