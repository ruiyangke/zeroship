#!/usr/bin/env bash
# Tier-signature census for the zeroship-plugin-db crate split.
#
# WHAT IT ANSWERS
#   For each module, does any function SIGNATURE name a crate that the module's
#   proposed destination crate is forbidden to depend on - and does any module
#   reach UPWARD into a tier above it?
#
# WHY IT EXISTS
#   docs/proposals/2026-08-31-data-crate-shape.md assigns all 57,427 lines of
#   zeroship-plugin-db to six crates. That assignment was produced by walking
#   MODULES and then by walking public TYPES. Neither instrument can see a
#   foreign-tier type sitting in a function signature.
#
# THIS IS A CENSUS, NOT A GATE. It reports; it does not rule. Deliberately NOT
# named *_gate.sh and NOT at tests/ depth 1, so tests/gate_arm_census.sh does not
# adopt it (that census globs `find tests -maxdepth 1 -name '*_gate.sh'`). It
# becomes a gate - with arms and floors per tests/lib/gate_arms.sh - once the
# first crate boundary exists and TIER below stops being a proposal.
#
# EVERY VERDICT IS RELATIVE TO THE TIER MAP. Re-drawing a boundary in the
# proposal invalidates this file's output completely; update `tier()` and
# `allowed()` in the same change, or the census reports on a shape nobody
# proposed. THAT HAPPENED ONCE ALREADY: the proposal moved tx_scope.rs to the
# adapter in e39699df8 and this file kept routing it to ENGINE until an
# adversarial reviewer diffed the two.
#
# FOUR DEFECTS FOUND BY REVIEW ON 2026-08-31, ALL FIXED HERE. Recorded because
# each one printed a plausible number:
#
#   1. TEST BOUNDARY. The production region was "everything before the first
#      #[cfg(test)] anywhere in the file". That attribute is usually a one-line
#      test hook INSIDE a production function, not the terminal test module.
#      exec.rs:370 is `#[cfg(test)] tests::record_sqlite_shared_route();` inside a
#      live `if` block; its real `mod tests` is at :671. The census was reading
#      370 of exec.rs's 1,769 lines and calling the rest test code. Crate-wide it
#      discarded ~2,995 production lines across 8 files, and hid
#      crud/mask_policy.rs:431 - an 18th V8-signature dispatch function.
#      It also caused a FALSE CORRECTION to be published against the proposal's
#      dependency-cycle paragraph, since retracted.
#      Now: the boundary is the first `#[cfg(test)]` that IMMEDIATELY PRECEDES a
#      `mod` declaration. No such pair means the whole file is production.
#
#   2. SINGLE-LINE SIGNATURES WERE NEVER CHECKED. The `fn` rule ended in `next`,
#      so a signature opening and closing on its own line was never tested at its
#      own terminator. tx_scope.rs:63 `scope_symbol` was missed this way.
#
#   3. THE LAST FUNCTION IN A FILE WAS NEVER CHECKED. Collection stayed open at
#      EOF and END printed the tally without flushing. tx_scope.rs:138 `leave`.
#      (2 and 3 together made tx_scope.rs report 4 where the answer is 6 - a
#      number this file printed while the proposal's prose correctly said 6.)
#
#   4. THE RUNTIME MARKER WAS BLIND TO ORDINARY RUST. It matched only the
#      qualified spelling `zeroship_runtime::`, but every runtime type here is
#      imported unqualified (`use zeroship_runtime::state::OpError;` then bare
#      `OpError`). It therefore missed `pub fn to_op_error(self) -> OpError` in
#      error.rs - the single edge that started this whole investigation.
#      Now: bare OpError/OpResult/ResolveValue/SharedState count too.
#
# AND ONE POSITION IT STILL CANNOT SEE, stated rather than silently missing:
# `impl` headers. error.rs:797 `impl From<compio_postgres::Error> for DbError` is
# a real vendor edge in a CORE-tier module and no `fn` line starts it. Adding
# impl-header parsing needs a different collector; until then, that edge is known
# and uncounted.
#
# TWO REGIONS
#   default   PRODUCTION region, foreign crates in function SIGNATURES.
#   --tests   TEST region, foreign crates ANYWHERE. A test body is not a
#             signature; the question there is only whether the module's test
#             build links something its crate may not. It matters for the same
#             reason the proposal's Phase 0.2 gives for moving auth/util.rs -
#             "test-tier today, but test builds must compile".
#
# USAGE
#   tests/lib/tier_signature_census.sh              # production signatures, violations only
#   tests/lib/tier_signature_census.sh --all        # production signatures, incl. ok rows
#   tests/lib/tier_signature_census.sh --tests      # test-region marker use, violations only
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SRC="$ROOT/crates/zeroship-plugin-db/src"
SHOW_ALL=0
TEST_REGION=0
case "${1:-}" in
  --all)   SHOW_ALL=1 ;;
  --tests) TEST_REGION=1 ;;
  "")      ;;
  *)       echo "tier_signature_census: unknown option '$1'" >&2; exit 2 ;;
esac

[ -d "$SRC" ] || { echo "tier_signature_census: no such tree: $SRC" >&2; exit 1; }
cd "$SRC" || exit 1

# First line of the terminal test module, or "" if the file has none.
# Defect 1: this is NOT "the first #[cfg(test)]".
test_mod_line() {
  awk '
    /^[[:space:]]*#\[cfg\(test\)\]/ { pend = NR; next }
    pend && /^[[:space:]]*(pub[[:space:]]+)?mod[[:space:]]/ { print pend; exit }
    /^[[:space:]]*$/ { next }        # blank lines may sit between the two
    { pend = 0 }
  ' "$1"
}

# Destination crate per module, from the proposal's assignment table.
tier() {
  case "$1" in
    # tx_scope.rs is ADAPTER: all six of its production functions are V8
    # context-map manipulation, so the proposal moves the file whole.
    ./v8_classes/*|./v8_bridge.rs|./lib.rs|./tx_scope.rs)  echo "ADAPTER" ;;
    ./crud/*|./transaction/*|./exec.rs|./broker.rs|./read_set.rs|./tx_route.rs|./drop_namespace.rs|./cross_app_fk.rs) echo "ENGINE" ;;
    ./auth/bootstrap.rs)                                 echo "ENGINE" ;;
    ./backend/postgres.rs)                               echo "PG" ;;
    ./backend/sqlite/*)                                  echo "SQLITE" ;;
    ./encryption/*)                                      echo "ENCRYPT" ;;
    ./wal_consumer.rs|./replication.rs|./slot_reaper.rs) echo "CDC" ;;
    ./error.rs|./descriptor.rs|./binding.rs)             echo "CORE" ;;
    *)                                                   echo "CONTESTED" ;;
  esac
}

# May tier $1 name marker $2? ADAPTER sits above everything; CONTESTED modules
# have no destination yet, so they cannot be in violation of one.
#
# NOTE the deliberate exception: ADAPTER is NOT blanket-allowed for
# `upward` - nothing is above the adapter, so that combination is impossible
# rather than permitted - and it is NOT allowed to name both vendors, because
# v8_bridge.rs doing so is the proposal's own row-to-JSON finding. An earlier
# `ADAPTER:*` blanket meant this census could never report that.
allowed() {
  case "$1:$2" in
    ADAPTER:upward)        return 0 ;;
    ADAPTER:compio_postgres|ADAPTER:rusqlite) return 1 ;;   # row-to-JSON finding
    ADAPTER:*)             return 0 ;;
    CONTESTED:*)           return 0 ;;
    PG:compio_postgres)    return 0 ;;
    SQLITE:rusqlite)       return 0 ;;
    CDC:compio_postgres)   return 0 ;;   # the relay reads WAL over the pg protocol
    *)                     return 1 ;;
  esac
}

# Regex per marker. `upward` is a module reference, not a crate: an engine module
# naming crate::v8_bridge or crate::v8_classes points INTO the adapter, which is
# the direction a layered split exists to forbid. No crate-name marker can
# express it.
marker_re() {
  case "$1" in
    v8)               echo '(^|[^A-Za-z0-9_])v8::' ;;
    zeroship_runtime) echo '(^|[^A-Za-z0-9_])(zeroship_runtime::|OpError|OpResult|ResolveValue|SharedState)' ;;
    compio_postgres)  echo '(^|[^A-Za-z0-9_])compio_postgres::' ;;
    rusqlite)         echo '(^|[^A-Za-z0-9_])rusqlite::' ;;
    upward)           echo 'crate::(v8_bridge|v8_classes)' ;;
  esac
}

COL=$([ "$TEST_REGION" -eq 1 ] && echo TESTREFS || echo SIGS)
printf '%-10s %-36s %-17s %5s   %s\n' TIER FILE MARKER "$COL" VERDICT
echo "----------------------------------------------------------------------------------------"
viol=0; rows=0
while read -r f; do
  t=$(tier "$f")
  tm=$(test_mod_line "$f")
  if [ "$TEST_REGION" -eq 1 ]; then
    [ -z "$tm" ] && continue          # no test module: nothing to say in this mode
    start=$tm
  else
    cut=${tm:-$(wc -l < "$f")}
  fi
  for m in v8 zeroship_runtime compio_postgres rusqlite upward; do
    re=$(marker_re "$m")
    if [ "$TEST_REGION" -eq 1 ]; then
      # Occurrences, not signatures: a test body has no signature to inspect.
      n=$(tail -n +"$start" "$f" | grep -vP '^\s*(///|//!|//)' | grep -cP "$re")
    elif [ "$m" = upward ]; then
      # `upward` is ALWAYS an occurrence scan, never a signature scan. A module
      # reaches up through `use crate::v8_bridge::...` and through expressions
      # like `transform: crate::v8_classes::masked_value::rehydrate_masked_values`
      # - neither is a function signature, so scanning signatures for it finds
      # nothing and reports a clean tier that is not clean. Scanning signatures
      # for this marker returned 0 across the whole crate while 12 real upward
      # references existed.
      n=$(head -n "$cut" "$f" | grep -vP '^\s*(///|//!|//)' | grep -cP "$re")
    else
      n=$(head -n "$cut" "$f" | grep -vP '^\s*(///|//!|//)' | awk -v RE="$re" '
        function flush(  ) { if (c && sig ~ RE) k++; c = 0 }
        # Defect 2: test the fn line itself before moving on - a one-line
        # signature terminates immediately and used to be skipped by `next`.
        /^[[:space:]]*(pub([[:space:]]*\([^)]*\))?[[:space:]]+)?(async[[:space:]]+)?fn[[:space:]]/ {
          flush(); s = NR; sig = $0; c = 1
          if ($0 ~ /\{[[:space:]]*$/ || $0 ~ /;[[:space:]]*$/) flush()
          next
        }
        c { sig = sig " " $0
            if (NR - s > 8 || /\{[[:space:]]*$/ || /;[[:space:]]*$/) flush() }
        # Defect 3: flush the final function, which used to die unchecked at EOF.
        END { flush(); print k+0 }')
    fi
    [ "$n" -eq 0 ] && continue
    if allowed "$t" "$m"; then
      [ "$SHOW_ALL" -eq 1 ] || continue
      v="ok"
    else
      v="** VIOLATION **"; viol=$((viol+n))
    fi
    rows=$((rows+1))
    printf '%-10s %-36s %-17s %5s   %s\n' "$t" "$f" "$m" "$n" "$v"
  done
done < <(find . -name '*.rs' | LC_ALL=C sort)
echo "----------------------------------------------------------------------------------------"
echo "rows reported: $rows"
if [ "$TEST_REGION" -eq 1 ]; then
  echo "test-region marker refs whose tier forbids them: $viol"
  echo
  echo "These are dev-dependency crossings, not shipped ones - and they still bind,"
  echo "because a test build must compile. Anything moved by Phase 0 has a test tail"
  echo "that must move with it or the vacated crate keeps the dependency."
else
  echo "signature-position violations: $viol"
fi
echo
echo "A LOWER number is not automatically progress - check that tier() still"
echo "matches the proposal before reading any movement as a fix. The 2026-08-31"
echo "headline of 49 was right by CANCELLATION: an under-reading instrument and a"
echo "stale tier map erred in opposite directions by the same amount."
