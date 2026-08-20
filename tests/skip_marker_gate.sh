#!/usr/bin/env bash
# A test that skips must say so in the one string the census can find.
#
# WHY THIS EXISTS. `tests/lib/skip_census.sh` counts skips by searching a run
# log for a single token:
#
#     ZS_SKIP_MARKER='ZEROSHIP-TEST-SKIPPED'
#
# That token is a good one -- its hyphens are not legal in a Rust identifier, so
# no test name can forge it, and the alternatives were measured and rejected in
# that file's header. But the census enumerates by it, and a test that never
# prints it is invisible to the census however good the census is at its job. It
# skips AND goes uncounted, by a gate whose entire purpose is to stop a suite
# quietly covering less than it claims.
#
# On 2026-08-19 sixteen files across crates/ and libs/ carried a skip-shaped
# `eprintln!` that never printed the marker. Most were live self-skips on an
# absent Postgres or an absent compio runtime; the worst case was seven
# `zeroship-worker` `handler::tests::workflow_advance_*` tests, which were
# silent in two independent ways at once -- uncounted when they skipped, and in
# NEITHER gate suite (`zeroship-worker` is run by neither run_auth_suite.sh nor
# run_billing_suite.sh) when they failed.
#
# WHY A GATE AND NOT JUST ADDING THE MARKER. Adding it to those sixteen files
# fixes today and nothing else. The census's blindness is structural: its
# enumeration source is a string that a new file simply may not contain, and
# nothing anywhere connects "this code skips" to "the census knows". This gate
# is that connection. Without it the seventeenth file arrives next week and the
# census reports the same green.
#
# WHY NOT REPLACE THE MARKER WITH LIBTEST'S OWN COUNTS, which cannot be escaped
# by omission. Because libtest has no count that moves. A runtime self-skip --
#
#     let Ok(url) = std::env::var("PG_TEST_URL") else { ...; return; };
#
# -- COMPILES, RUNS, RETURNS EARLY AND IS COUNTED AS PASSED. Measured 2026-08-09
# and recorded in tests/lib/skip_census.sh: `cargo test -p zeroship-plugin-kv
# --test e2e_runtime` with the var unset closes with `10 passed; 0 failed; 0
# ignored; 0 measured`, and two of those ten exercised nothing. `0 ignored` is
# for `#[ignore]`, a COMPILE-TIME decision; by the time a test body knows its
# backend is missing the harness has already committed to counting it. So
# reconciling the harness's numbers is not a stronger version of this gate, it
# is blind to the whole class -- the distinction between "never ran" and "ran a
# no-op" is exactly what the marker was invented to carry, and libtest does not
# carry it.
#
# WHAT THIS GATE DOES NOT CATCH, and it is the important limit:
#
#   - A test that skips SILENTLY. This gate finds skip-shaped PRINTS; a body
#     that returns early with no output at all matches nothing here and is
#     invisible to the census too. Nothing short of reconciling an expected test
#     list against a run finds that one, and see above for why the harness's own
#     numbers cannot supply it.
#   - Whether a skip SHOULD be a skip. Most of the sixteen were converted to
#     hard failures rather than given a marker, because Postgres and Redis are
#     not optional for this workspace (crates/test-support/src/lib.rs states
#     that policy). This gate does not make that judgement; it only refuses the
#     third option, of skipping without saying so.
#   - Test code outside crates/ and libs/. sdks/ is TypeScript and has its own
#     runner; tests/ is shell.
#   - The ALLOWLIST below is a hole by construction. Two entries today, each
#     with a reason. A third needs one too.

set -uo pipefail
cd "$(dirname "$0")/.."

MARKER='ZEROSHIP-TEST-SKIPPED'

# Print, as `path:line:text`, every skip-shaped print macro in $1.. that does
# not carry the marker.
#
# The shape is a `println!`/`eprintln!` whose format string CONTAINS a word
# starting "skip". Deliberately loose: a narrower pattern anchored at the start
# of the string would have missed
# `eprintln!("skip suppressed_recipient_returns_200... (no test database)")`
# and `eprintln!("Skipping autocommit-leak test - Postgres not reachable: {e}")`,
# both of which were real.
#
# NO `2>/dev/null` ANYWHERE IN THIS FUNCTION. Its output feeds a count, and a
# grep that failed and a grep that found nothing print identically.
skip_shaped_lines() {
  grep -rnE '(eprintln|println)!\("[^"]*[Ss]kip' --include='*.rs' "$@" \
    | grep -v "$MARKER" \
    || true
}

# Files that legitimately print a skip-shaped line that is NOT a test skipping.
# Each entry states why, because an allowlist without reasons becomes a list of
# things nobody remembers refusing to check.
#
#   zeroship-platform-migrate.rs  a SHIPPING BINARY's progress output. It prints
#     `  skipped  <name> (already applied)` per migration the journal already
#     holds, on stdout, to an operator watching a deploy. There is no test and
#     nothing to count.
#
#   crates/runtime/tests/wpt_*.rs  the WPT runners print their own `=== Skipped
#     ===` REPORT HEADER, then list the upstream subtests they declined. That is
#     a census of its own, already visible in the run output; marking the header
#     would make the census count one skip per runner instead of the real
#     number, which is worse than not counting it.
ALLOWLIST='crates/zeroship-migrate-adapter/src/bin/zeroship-platform-migrate\.rs|crates/runtime/tests/wpt_'

self_test() {
  echo "skip marker gate self-test"
  local tmp status=0
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  # POSITIVE: a skip-shaped line with no marker must be NAMED.
  mkdir -p "$tmp/crates/scratch/src"
  cat > "$tmp/crates/scratch/src/lib.rs" <<'RS'
#[cfg(test)]
mod tests {
    #[test]
    fn needs_a_backend() {
        let Some(url) = std::env::var("PG_TEST_URL").ok() else {
            eprintln!("skipping (no test database)");
            return;
        };
        let _ = url;
    }
}
RS
  local found
  found="$(cd "$tmp" && skip_shaped_lines crates)"
  if printf '%s' "$found" | grep -q 'scratch/src/lib.rs'; then
    echo "  ok   a marker-less skip is named: ${found#*:}"
  else
    echo "  FAIL a marker-less skip was NOT named; the gate detects nothing"
    status=1
  fi

  # NEGATIVE CONTROL, differing in ONE variable: the same line, marker added.
  # Without this the positive above only proves the grep RAN, not that it
  # DISCRIMINATES -- a pattern matching every line would pass the positive.
  sed -i "s/skipping (no test database)/$MARKER: no test database/" \
    "$tmp/crates/scratch/src/lib.rs"
  found="$(cd "$tmp" && skip_shaped_lines crates)"
  if [ -z "$found" ]; then
    echo "  ok   the same line WITH the marker is not reported"
  else
    echo "  FAIL the marker did not silence it: $found"
    status=1
  fi

  return "$status"
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi

echo "skip marker gate"

OFFENDERS="$(skip_shaped_lines crates libs | grep -vE "$ALLOWLIST" || true)"

# `printf | wc -l` and not `grep -c`: an empty string still has one line, and
# `grep -c` exits 1 on zero matches, which an `&&` chain would swallow into a
# missing legitimate zero.
COUNT=0
[ -n "$OFFENDERS" ] && COUNT="$(printf '%s\n' "$OFFENDERS" | wc -l | tr -d ' ')"

if [ "$COUNT" -eq 0 ]; then
  echo "  ok   no test announces a skip the census cannot see"
  exit 0
fi

echo "  FAIL $COUNT skip-shaped line(s) do not carry $MARKER:"
printf '%s\n' "$OFFENDERS" | sed 's/^/       /'
cat <<EOF

Each of these announces that a test did nothing, in a string
tests/lib/skip_census.sh cannot find. The suite reports it as PASSED and no
number anybody watches moves.

Three fixes, in order of preference:

  1. Make it FAIL. Postgres and Redis are not optional for this workspace --
     see the policy in crates/test-support/src/lib.rs. If the test cannot run
     without a backend, panic with the address it dialled and
     tests/provision_test_backends.sh, do not skip. This is what most of the
     original sixteen became.

  2. If the absence really is tolerable (no docker for a MinIO container, a
     pgvector extension that is not installed), announce it through
     zeroship_test_support::skip(reason), which writes the marker straight to
     the stderr HANDLE. A println!/eprintln! will not do: the harness buffers
     those and replays them only for a FAILING test, so the announcement is
     invisible on a pass, which is the run where it matters.

  3. If it is not a test skip at all, add it to ALLOWLIST at the top of this
     file WITH A REASON.
EOF
exit 1
