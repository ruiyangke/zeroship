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
#   - The ALLOW list below is a hole by construction. Two entries today, each
#     with a reason. A third needs one too.
#   - COMPLIANCE. This is a violation lint and it can only ever see violations:
#     the remedy it recommends, `zeroship_test_support::skip`, writes to the
#     stderr HANDLE and so matches no print-macro pattern, and the other remedy
#     is a panic. That is not a defect - a lint that found conformance would be
#     a different tool - but it means the ONLY evidence this gate looked at
#     anything is the pre-filter row count and the ALLOW list, so both are now
#     asserted below rather than assumed. 238 call sites use the remedy
#     (measured 2026-08-20), so it is adopted, not merely advised.
#
# WHY THE COUNTS ARE PRINTED. Until 2026-08-20 this ran 8 raw rows through a
# file-path ALLOW list that excused all 8, then printed "ok no test announces a
# skip" - the same line a tree with no rows at all would print, and the same
# line a broken `grep` would print. It examined nothing and said so nowhere.

set -uo pipefail
cd "$(dirname "$0")/.."

# Per-arm anti-vacuity accounting. This gate is the WORST measured instance the
# library exists for: 8 raw hits, 8 excused by ALLOW, 0 ruled on - and it printed
# the same "ok" a tree with no skip-shaped prints at all would print.
# shellcheck source=tests/lib/gate_arms.sh
. "$(dirname "$0")/lib/gate_arms.sh"
gate_arms_init skip_marker

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
#   crates/runtime/tests/wpt_*.rs  the WPT runners print their own `=== Skipped
#     ===` REPORT HEADER, then list the upstream subtests they declined. That is
#     a census of its own, already visible in the run output; marking the header
#     would make the census count one skip per runner instead of the real
#     number, which is worse than not counting it.
#
# EACH PATTERN IS AS NARROW AS THE THING IT EXCUSES. The wpt entry used to be
# the bare path prefix `crates/runtime/tests/wpt_`, which excused every line in
# seven files, so a genuine marker-less skip written inside a WPT runner would
# have been waved through with the report header. It now matches the header
# SHAPE as well as the path.
ALLOW=(
  'crates/zeroship-runtime/tests/wpt_[a-z_]+\.rs:[0-9]+:.*=== Skipped|the WPT runners own report header, which precedes their own census'
)

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

STATUS=0

# --- Arm: every copy of the marker is byte-identical ----------------------
#
# THE OTHER WAY A SKIP GOES UNCOUNTED, and the one arm here whose subject is
# non-empty by construction. The census enumerates a run log by ONE token, and
# that token is written out by hand in several places: the authority
# (`crates/test-support/src/lib.rs`), a verbatim `SKIP_MARKER` const in each
# standalone driver that keeps its own announcer, an inlined copy in a
# `format!`, and `ZS_SKIP_MARKER` in the census itself. One character wrong in
# any of them and every skip announced through that copy leaves the census
# silently - which is this gate's whole subject, arriving by a different door.
#
# `tests/lib/skip_census.sh` says these copies are "kept byte-identical". That
# was an instruction to a reader, not a check; nothing verified it. It also said
# there were three copies under libs/, and there are two -- compio-postgres
# deliberately has none and its header says so.
#
# The search pattern is the marker with its last segment dropped, so a typo in
# that segment still MATCHES and is reported as a mismatch instead of vanishing
# from the search. A typo further left leaves the family entirely, and the floor
# below catches that instead. Derived from $MARKER rather than written out,
# because a second literal in this file would be one more copy to drift -- and
# because the first version of this arm spelled it and then reported its own
# prose as drift, which is a fair demonstration that it looks at what it says.
MARKER_FAMILY="${MARKER%-*}"
MARKER_SITE_FLOOR=20
MARKER_TOKENS="$(grep -rnoE "${MARKER_FAMILY}[A-Z0-9_-]*" crates libs tests)"
N_MARKER=0
[ -n "$MARKER_TOKENS" ] && N_MARKER="$(printf '%s\n' "$MARKER_TOKENS" | wc -l | tr -d ' ')"

# MEASURED 2026-08-20: 27 tokens. Floor kept at the value this check already
# used before the arm contract existed - it is well under 27, and the comment
# above already explains why the count can only shrink one edit at a time.
if ! gate_arm marker_identical "$N_MARKER" "$MARKER_SITE_FLOOR"; then
  echo "GATE CANNOT ANSWER: only $N_MARKER marker token(s) found, below the floor"
  echo "  of $MARKER_SITE_FLOOR. 27 were measured 2026-08-20. The search stopped"
  echo "  matching, so 'no drift' would mean nothing."
  exit 1
fi

DRIFTED="$(printf '%s\n' "$MARKER_TOKENS" | grep -v ":${MARKER}\$" || true)"
if [ -z "$DRIFTED" ]; then
  echo "  ok   all $N_MARKER copies of $MARKER are byte-identical"
else
  echo "  FAIL these tokens are in the marker's family but are not the marker:"
  printf '%s\n' "$DRIFTED" | sed 's/^/       /'
  echo "       tests/lib/skip_census.sh enumerates a run log by the exact string."
  echo "       A skip announced through a drifted copy leaves the census without"
  echo "       a number moving, which is the failure this whole gate is about."
  STATUS=1
fi

RAW="$(skip_shaped_lines crates libs)"

# `printf | wc -l` and not `grep -c`: an empty string still has one line, and
# `grep -c` exits 1 on zero matches, which an `&&` chain would swallow into a
# missing legitimate zero.
n_lines() { [ -n "$1" ] && printf '%s\n' "$1" | wc -l | tr -d ' ' || echo 0; }

N_RAW="$(n_lines "$RAW")"

# THE CANNOT-ANSWER BRANCH. A pattern that stopped matching and a tree with no
# skip-shaped prints in it print the same verdict, and this gate's whole subject
# is the difference between those two. 8 rows on 2026-08-20; the floor is 1
# because ANY row proves the detector ran, and because the honest end state of
# this gate is a small number, not a large one.
if ! gate_arm raw_detection "$N_RAW" 1; then
  echo "GATE CANNOT ANSWER: the skip-shaped-print pattern matched nothing in"
  echo "  crates/ or libs/. Every skip announcement in this workspace would have"
  echo "  to have been deleted for that to be real; the likelier reading is that"
  echo "  the pattern or the paths stopped matching. Run --self-test."
  exit 1
fi

# Direction 1: a row nobody has ruled on.
OFFENDERS=""
N_EXCUSED=0
while IFS= read -r row; do
  [ -n "$row" ] || continue
  excused=0
  for entry in "${ALLOW[@]}"; do
    printf '%s\n' "$row" | grep -qE "${entry%%|*}" && { excused=1; break; }
  done
  if [ "$excused" -eq 1 ]; then
    N_EXCUSED=$((N_EXCUSED + 1))
  else
    OFFENDERS="${OFFENDERS}${row}
"
  fi
done < <(printf '%s\n' "$RAW")

COUNT="$(n_lines "${OFFENDERS%$'\n'}")"

# Direction 2, the one this gate did not have. `tests/tests_do_not_create_
# databases_gate.sh` is the model: an exemption that no longer matches anything
# must FAIL, or the ALLOW list silently becomes the whole world. It doubles as
# this gate's positive control: an entry that stops matching is either code
# that went away or a detector that broke, and both have to be said out loud.
#
# ONE ENTRY IS LEFT AND IT IS CURRENTLY FAILING HERE. The
# zeroship-platform-migrate entry went with the binary it excused on
# 2026-08-28. The surviving WPT entry still names `crates/runtime/tests/`; the
# crate is `zeroship-runtime`, so the pattern matches nothing and this loop
# says so. That break PREDATES the removal - measured 2026-08-28 with the
# removed entry restored, this arm reads examined=0 either way, so the removal
# took one dead entry out of two rather than causing the failure.
N_STILL_MATCHING=0
for entry in "${ALLOW[@]}"; do
  pattern="${entry%%|*}"
  reason="${entry#*|}"
  if printf '%s\n' "$RAW" | grep -qE "$pattern"; then
    N_STILL_MATCHING=$((N_STILL_MATCHING + 1))
    printf '  exempt %s\n         %s\n' \
      "$(printf '%s\n' "$RAW" | grep -cE "$pattern") row(s) matching ${pattern%%:*}" "$reason"
  else
    echo "  FAIL ALLOW excuses a shape nothing matches: $pattern"
    echo "       Either the code it excused is gone -- remove the entry -- or the"
    echo "       detector stopped matching, in which case the verdict below is"
    echo "       meaningless. A stale exemption reads as a decision somebody is"
    echo "       still making."
    STATUS=1
  fi
done

# THE ARM THIS GATE ACTUALLY NEEDS, and the reason it exists at all: the
# FORWARD count (COUNT below, "unruled") was 8 raw hits, 8 excused, 0 unruled
# on 2026-08-20, because every offender then was legitimately excused.
# Declaring an arm on THAT number would be exactly the defect this file was
# rewritten to stop being: a floor >=1 on a count that is honestly 0 would fail
# every clean run, and a floor of 0 is refused by the library on purpose. So
# this arm counts the REVERSE direction instead - how many ALLOW entries still
# match something in $RAW, which is what direction 2 above rules on line by
# line. Floor 1: one entry is left, so that entry has to keep matching.
#
# MEASURED 2026-08-28: 7 raw, 0 excused, 7 unruled, and this arm at 0 - the
# exact collapse it was built to announce, the same shape arm 1 of
# ws_subscription_stub_gate.sh had. The pattern stopped matching because the
# crate moved to `zeroship-runtime`, NOT because the report headers went away,
# so the forward count's 7 unruled rows are not a finding about the tree
# either. Fix the pattern; do not lower the floor.
if ! gate_arm allow_entries_live "$N_STILL_MATCHING" 1; then
  echo "GATE CANNOT ANSWER: no ALLOW entry in this file still matches a raw"
  echo "  skip-shaped print. Either both were legitimately retired (the forward"
  echo "  count above should then be nonzero for the same edit) or the allowlist"
  echo "  itself stopped matching, in which case direction 2's clean report means"
  echo "  nothing."
  STATUS=1
fi

echo "  ${N_RAW} skip-shaped print(s) found, ${N_EXCUSED} excused, ${COUNT} unruled"

# Folded into STATUS, not exit()'d directly, because the COUNT==0 branch right
# below is this gate's normal PASS path and still has to carry an arm refusal
# out through the same variable everything else here uses.
gate_arms_finish || STATUS=1

if [ "$COUNT" -eq 0 ]; then
  [ "$STATUS" -eq 0 ] && echo "  ok   no test announces a skip the census cannot see"
  exit "$STATUS"
fi

echo "  FAIL $COUNT skip-shaped line(s) do not carry $MARKER:"
printf '%s' "$OFFENDERS" | sed 's/^/       /'
cat <<EOF

Each of these announces that a test did nothing, in a string
tests/lib/skip_census.sh cannot find. The suite reports it as PASSED and no
number anybody watches moves.

Three fixes, in order of preference:

  1. Make it FAIL. Postgres and Redis are not optional for this workspace --
     see the policy in crates/zeroship-test-support/src/lib.rs. If the test cannot run
     without a backend, panic with the address it dialled and
     tests/provision_test_backends.sh, do not skip. This is what most of the
     original sixteen became.

  2. If the absence really is tolerable (no docker for a MinIO container, a
     pgvector extension that is not installed), announce it through
     zeroship_test_support::skip(reason), which writes the marker straight to
     the stderr HANDLE. A println!/eprintln! will not do: the harness buffers
     those and replays them only for a FAILING test, so the announcement is
     invisible on a pass, which is the run where it matters.

  3. If it is not a test skip at all, add it to ALLOW at the top of this file
     WITH A REASON, and make the pattern as narrow as the line it excuses --
     a bare path prefix excuses every future line in that file too.
EOF
exit 1
