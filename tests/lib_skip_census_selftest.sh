#!/usr/bin/env bash
# Self-test for tests/lib/skip_census.sh.
#
# The library exists so a suite's green tally cannot hide a test that exercised
# nothing. That is only worth anything if it is right in BOTH directions:
#
#   a real announcement MUST be counted   - else the census reports "0 skipped"
#                                           over a log that contains them, which
#                                           is worse than no census at all
#                                           because it actively asserts safety
#   incidental text MUST NOT be counted   - the word "skip" occurs in test names
#                                           (`pg_or_skip`) and in SQL the drivers
#                                           echo (`skip_consent`); a census that
#                                           counted those would cry wolf until
#                                           people stopped reading it
#
# It also pins the two traps that would each silently turn the census into a
# constant zero or a constant pass. Both are cases where the WRONG behaviour
# still looks like a working gate, which is why they are asserted rather than
# left to inspection.
#
# Run directly: tests/lib_skip_census_selftest.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
. "$ROOT/tests/lib/skip_census.sh"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail=0
pass=0

# expect_count <expected_offenders> <label> <allowlist> <log body>
expect_count() {
  local expected="$1" label="$2" allow="$3" body="$4"
  local log="$TMP/log"
  printf '%s\n' "$body" > "$log"
  zs_skip_census "$log" "$allow" >/dev/null 2>&1
  if [ "$ZS_SKIP_COUNT" = "$expected" ]; then
    pass=$((pass + 1))
    echo "ok   - $label (offenders=$ZS_SKIP_COUNT)"
  else
    fail=$((fail + 1))
    echo "FAIL - $label: expected offenders=$expected, got $ZS_SKIP_COUNT" >&2
  fi
}

# expect_status <expected: 0|1> <label> <allowlist> <log body>
expect_status() {
  local expected="$1" label="$2" allow="$3" body="$4"
  local log="$TMP/log" actual
  printf '%s\n' "$body" > "$log"
  if zs_skip_census "$log" "$allow" >/dev/null 2>&1; then actual=0; else actual=1; fi
  if [ "$actual" = "$expected" ]; then
    pass=$((pass + 1))
    echo "ok   - $label (status=$actual)"
  else
    fail=$((fail + 1))
    echo "FAIL - $label: expected status=$expected, got $actual" >&2
  fi
}

echo "=== must COUNT: real announcements ==="
expect_count 1 "single announcement" '' \
  'ZEROSHIP-TEST-SKIPPED: e2e_dragonfly: ZEROSHIP_WORKER_KV_URL unset'
expect_count 3 "several announcements" '' \
  'ZEROSHIP-TEST-SKIPPED: a
ZEROSHIP-TEST-SKIPPED: b
ZEROSHIP-TEST-SKIPPED: c'
# The announcement is interleaved with harness output, which is where it always
# is. A head/tail-only check would miss this.
expect_count 1 "buried between passing tests" '' \
  'test alpha ... ok
ZEROSHIP-TEST-SKIPPED: e2e_dragonfly: ZEROSHIP_WORKER_KV_URL unset
test beta ... ok
test result: ok. 2 passed; 0 failed; 0 ignored'

echo
echo "=== must NOT count: incidental text (one variable from the cases above) ==="
# These are the exact shapes measured in a real auth run. Each differs from a
# true announcement ONLY in that it lacks the marker, which is the single
# variable that must decide the outcome.
expect_count 0 "harness line for a test NAMED ...skips..." '' \
  'test dunning_tick_skips_when_advisory_lock_held ... ok'
expect_count 0 "SQL echoing a skip_consent column" '' \
  'INSERT INTO zeroship.oauth_clients (client_id, skip_consent, created_at) VALUES ($1, $2, $3)'
expect_count 0 "the bare word in prose" '' \
  'note: skipping the full-apply proof because the DSN is unset'
expect_count 0 "a wholly clean run" '' \
  'test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out'

echo
echo "=== allowlist: tolerate a NAMED backend, keep seeing the rest ==="
expect_count 0 "allowlisted reason is tolerated" 'GATEWAY_ANCHORS_DB_URL' \
  'ZEROSHIP-TEST-SKIPPED: [anchors] skip auth_token (no GATEWAY_ANCHORS_DB_URL)'
# The allowlist must exempt the entry it names and NOTHING else. An allowlist
# that swallowed its siblings would blind the gate to the whole binary while
# reading as a narrow, deliberate exemption.
expect_count 1 "a sibling skip is still reported" 'GATEWAY_ANCHORS_DB_URL' \
  'ZEROSHIP-TEST-SKIPPED: [anchors] skip auth_token (no GATEWAY_ANCHORS_DB_URL)
ZEROSHIP-TEST-SKIPPED: e2e_dragonfly: ZEROSHIP_WORKER_KV_URL unset'

echo
echo "=== trap 1: an EMPTY allowlist must match NOTHING ==="
# `grep -E ''` matches every line. Passing an unset allowlist straight into the
# pipeline would therefore tolerate every skip in the run and print a clean
# census - a gate that is broken in the one direction nobody checks, because it
# fails open and its output looks correct.
expect_count 2 "empty allowlist tolerates nothing" '' \
  'ZEROSHIP-TEST-SKIPPED: one
ZEROSHIP-TEST-SKIPPED: two'
expect_status 1 "empty allowlist still reports failure status" '' \
  'ZEROSHIP-TEST-SKIPPED: one'

echo
echo "=== trap 2: a log containing a NUL must still be counted, BY NAME ==="
# The byte that matters is NUL, and that is measured rather than assumed. On GNU
# grep 3.12 - the grep these gates actually run under - a log holding one NUL
# makes LINE mode print "grep: <file>: binary file matches" INSTEAD of the
# matching lines, while count mode still returns the right number. This library
# reads in line mode, because the census has to name which backend was missing,
# so dropping `-a` turns two named skips into one nameless banner. A cargo log
# can pick up a NUL from a panic payload or a captured subprocess.
#
# An earlier version of this case used \xff\xfe instead. GNU grep treats that as
# ordinary text and the case passed with `-a` REMOVED - green by construction,
# testing nothing. Verified by perturbation: with NUL, removing `-a` fails this
# case; with \xff\xfe, it did not.
printf 'ZEROSHIP-TEST-SKIPPED: before the NUL\nNUL->\000<-here\nZEROSHIP-TEST-SKIPPED: after it\n' > "$TMP/binlog"
zs_skip_census "$TMP/binlog" '' >/dev/null 2>&1
binlines="$(zs_skip_lines "$TMP/binlog")"
if [ "$ZS_SKIP_COUNT" = "2" ] && printf '%s' "$binlines" | grep -q 'after it'; then
  pass=$((pass + 1)); echo "ok   - NUL-bearing log counted and still names the reason (offenders=2)"
else
  fail=$((fail + 1))
  echo "FAIL - NUL-bearing log: expected 2 named offenders, got $ZS_SKIP_COUNT: $binlines" >&2
fi
# One-variable control: byte-for-byte the same log with the NUL replaced by
# printable text. This separates "the census cannot count" from "the NUL is what
# breaks it".
expect_count 2 "control: same log, NUL replaced by text" '' \
  'ZEROSHIP-TEST-SKIPPED: before the NUL
NUL-><-here
ZEROSHIP-TEST-SKIPPED: after it'

echo
echo "=== status contract ==="
expect_status 0 "clean run returns 0" '' 'test result: ok. 5 passed'
expect_status 0 "fully allowlisted run returns 0" 'AUTH_TEST_SMTP_SINK' \
  'ZEROSHIP-TEST-SKIPPED: smtp sink absent (AUTH_TEST_SMTP_SINK)'
expect_status 1 "an unexplained skip returns 1" 'AUTH_TEST_SMTP_SINK' \
  'ZEROSHIP-TEST-SKIPPED: e2e_dragonfly: ZEROSHIP_WORKER_KV_URL unset'
# A missing or unreadable log is not a pass in disguise: it reports zero
# offenders, and the caller's own ran-something / floor check is what catches a
# run that produced no log at all. Pinned so the behaviour is stated, not
# assumed.
zs_skip_census "$TMP/does_not_exist" '' >/dev/null 2>&1
if [ "$ZS_SKIP_COUNT" = "0" ]; then
  pass=$((pass + 1)); echo "ok   - missing log reports 0 (caller's floor is what catches it)"
else
  fail=$((fail + 1)); echo "FAIL - missing log: expected 0, got $ZS_SKIP_COUNT" >&2
fi

echo
echo "=================================================================="
echo "skip census selftest: ${pass} passed, ${fail} failed"
[ "$fail" -eq 0 ] || exit 1
