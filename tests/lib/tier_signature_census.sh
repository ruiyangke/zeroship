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
# USAGE
#   tests/lib/tier_signature_census.sh              # violations only
#   tests/lib/tier_signature_census.sh --all        # every marker use, incl. ok
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SRC="$ROOT/crates/zeroship-plugin-db/src"
SHOW_ALL=0
[ "${1:-}" = "--all" ] && SHOW_ALL=1

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

printf '%-10s %-36s %-17s %5s   %s\n' TIER FILE MARKER SIGS VERDICT
echo "----------------------------------------------------------------------------------------"
viol=0; rows=0
while read -r f; do
  t=$(tier "$f")
  # Production region only: everything before the first #[cfg(test)].
  cut=$(awk '/#\[cfg\(test\)\]/{print NR; exit}' "$f"); [ -z "$cut" ] && cut=$(wc -l < "$f")
  for m in v8 zeroship_runtime compio_postgres rusqlite; do
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
echo "signature-position violations: $viol"
echo
echo "Baseline measured 2026-08-31 at e39699df8: 49 violations across 8 files."
echo "A LOWER number is not automatically progress - check that tier() still"
echo "matches the proposal before reading any movement as a fix."
