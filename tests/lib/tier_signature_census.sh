#!/usr/bin/env bash
# Tier-signature census for the zeroship-plugin-db crate split.
#
# WHAT IT ANSWERS
#   For each module, does any function SIGNATURE name a crate that the module's
#   proposed destination crate is forbidden to depend on?
#
# WHY IT EXISTS
#   docs/proposals/2026-08-31-data-crate-shape.md assigns all 57,427 lines of
#   zeroship-plugin-db to six crates. That assignment was produced by walking
#   MODULES and then by walking public TYPES. Neither instrument can see a
#   foreign-tier type sitting in a function signature, and on 2026-08-31 that
#   blind spot was measured at 49 violations - including 39 v8 dispatch
#   functions inside a crate whose entire premise is that it links no V8, and
#   six compio_postgres signatures in error.rs, which is assigned to the
#   vendor-NEUTRAL contract crate that data-sqlite would depend on.
#
#   Four review rounds read that assignment without finding either.
#
# THIS IS A CENSUS, NOT A GATE. It reports; it does not rule. It is deliberately
# NOT named *_gate.sh and NOT at tests/ depth 1, so tests/gate_arm_census.sh
# does not adopt it (that census globs `find tests -maxdepth 1 -name '*_gate.sh'`).
# It becomes a gate - with arms and floors per tests/lib/gate_arms.sh - once the
# first crate boundary actually exists and TIER below stops being a proposal.
#
# EVERY VERDICT IS RELATIVE TO THE TIER MAP. Re-drawing a boundary in the
# proposal invalidates this file's output completely; update `tier()` and
# `allowed()` in the same change, or the census reports on a shape nobody
# proposed.
#
# TWO REGIONS, AND THE SECOND ONE IS NOT OPTIONAL READING.
#   default   scans the PRODUCTION region (everything before the first
#             #[cfg(test)]) for foreign crates in function SIGNATURES.
#   --tests   scans the TEST region for foreign crates ANYWHERE, because a test
#             body is not a signature and the question there is different: does
#             this module's test build link something its crate may not?
#
# The test region matters for the same reason the proposal's Phase 0.2 gives for
# moving auth/util.rs - "test-tier today, but test builds must compile". A module
# whose tests use v8 makes `cargo test -p <its crate>` link V8 even when the
# shipped lib does not. Measured 2026-08-31: 39 such refs, the sharpest being
# error.rs, whose 16 zeroship_runtime references are all OpErrorKind matches
# beside 17 to_op_error mentions - i.e. they ARE the tests for the one method
# Phase 0.1 relocates. Move the method without them and data-core keeps a
# dev-dependency on the V8 runtime.
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

# Destination crate per module, from the proposal's assignment table.
tier() {
  case "$1" in
    ./v8_classes/*|./v8_bridge.rs|./lib.rs)              echo "ADAPTER" ;;
    ./crud/*|./transaction/*|./exec.rs|./broker.rs|./read_set.rs|./tx_route.rs|./tx_scope.rs|./drop_namespace.rs|./cross_app_fk.rs) echo "ENGINE" ;;
    ./backend/postgres.rs)                               echo "PG" ;;
    ./backend/sqlite/*)                                  echo "SQLITE" ;;
    ./encryption/*)                                      echo "ENCRYPT" ;;
    ./wal_consumer.rs|./replication.rs|./slot_reaper.rs) echo "CDC" ;;
    ./error.rs|./descriptor.rs)                          echo "CORE" ;;
    *)                                                   echo "CONTESTED" ;;
  esac
}

# May tier $1 name crate $2? ADAPTER sits above everything; CONTESTED modules
# have no destination yet, so they cannot be in violation of one.
allowed() {
  case "$1:$2" in
    ADAPTER:*|CONTESTED:*) return 0 ;;
    PG:compio_postgres)    return 0 ;;
    SQLITE:rusqlite)       return 0 ;;
    CDC:compio_postgres)   return 0 ;;   # the relay reads WAL over the pg protocol
    *)                     return 1 ;;
  esac
}

COL=$([ "$TEST_REGION" -eq 1 ] && echo TESTREFS || echo SIGS)
printf '%-10s %-36s %-17s %5s   %s\n' TIER FILE MARKER "$COL" VERDICT
echo "----------------------------------------------------------------------------------------"
viol=0; rows=0
while read -r f; do
  t=$(tier "$f")
  cut=$(awk '/#\[cfg\(test\)\]/{print NR; exit}' "$f")
  if [ "$TEST_REGION" -eq 1 ]; then
    # No test region means nothing to say about this file in this mode.
    [ -z "$cut" ] && continue
  else
    # Production region: everything before the first #[cfg(test)].
    [ -z "$cut" ] && cut=$(wc -l < "$f")
  fi
  for m in v8 zeroship_runtime compio_postgres rusqlite; do
    if [ "$TEST_REGION" -eq 1 ]; then
      # Occurrences, not signatures: a test body has no signature to inspect,
      # and the question is only whether the marker is linked at all.
      n=$(tail -n +"$cut" "$f" | grep -vP '^\s*(///|//!|//)' | grep -cP "(^|[^a-zA-Z_])${m}::")
      [ "$n" -eq 0 ] && continue
      if allowed "$t" "$m"; then
        [ "$SHOW_ALL" -eq 1 ] || continue
        v="ok"
      else
        v="** VIOLATION **"; viol=$((viol+n))
      fi
      rows=$((rows+1))
      printf '%-10s %-36s %-17s %5s   %s\n' "$t" "$f" "$m" "$n" "$v"
      continue
    fi
    # Signatures span lines, so collect from `fn` until the opening brace.
    # Whole-line comments are stripped FIRST: without that, a doc comment
    # mentioning the marker is absorbed into the collected signature and
    # reported as a real one. That defect was live in the first run of this
    # census and produced its single most alarming row - backend/sqlite/mod.rs
    # apparently importing the Postgres driver, which was three doc comments.
    n=$(head -n "$cut" "$f" | grep -vP '^\s*(///|//!|//)' | awk -v M="$m" '
      /^[[:space:]]*(pub(\([a-z]+\))?[[:space:]]+)?(async[[:space:]]+)?fn[[:space:]]/ {
        s=NR; sig=$0; c=1; next
      }
      c { sig = sig " " $0
          if (NR-s > 8 || /\{[[:space:]]*$/) { if (sig ~ M"::") k++; c=0 } }
      END { print k+0 }')
    [ "$n" -eq 0 ] && continue
    if allowed "$t" "$m"; then
      v="ok"; [ "$SHOW_ALL" -eq 1 ] || continue
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
  echo "Baseline measured 2026-08-31 at 5f6e32f6d: 39 refs across 8 files."
  echo "These are dev-dependency crossings, not shipped ones - and they still bind,"
  echo "because a test build must compile. Anything moved by Phase 0 has a test tail"
  echo "that must move with it or the vacated crate keeps the dependency."
else
  echo "signature-position violations: $viol"
  echo
  echo "Baseline measured 2026-08-31 at e39699df8: 49 violations across 8 files."
fi
echo "A LOWER number is not automatically progress - check that tier() still"
echo "matches the proposal before reading any movement as a fix."
