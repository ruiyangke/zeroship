#!/usr/bin/env bash
# Cross-tier MODULE reference census: does every dependency point inward?
#
# WHAT IT ANSWERS, AND WHY IT IS SEPARATE FROM tier_signature_census.sh
#   The signature census asks "does this module NAME a foreign crate it may not
#   link?". This asks the other half of the same rule: "does this module REACH
#   INTO a tier above or beside it?".
#
#   Both are the clean-architecture dependency rule. Neither implies the other,
#   and the signature census demonstrably cannot substitute: on 2026-08-31 the
#   contract tier (backend/mod.rs) called the CDC relay through
#   `crate::wal_consumer::suppress_app`, a textbook inward-rule violation, and
#   the signature census reported nothing - because its `upward` marker is
#   hardcoded to `crate::v8_bridge|v8_classes` and knows only about the adapter.
#   A reviewer found that cycle; this file is so the next one does not have to.
#
# THE LATTICE. A module may reference a STRICTLY LOWER rank. Same-rank
# references are allowed only within the same tier - the vendor backends and the
# relay are peers and must not reach each other, which is what stops
# backend/sqlite/ naming the Postgres WAL decoder.
#
#     4  ADAPTER    plugin-db: the Rust <-> V8 seam
#     3  ENGINE     crud, transaction, exec, broker
#     2  PG SQLITE CDC   drivers and the relay - peers, mutually forbidden
#     1  ENCRYPT
#     0  CORE       DbError, descriptor, binding
#
#   CONTESTED modules have no settled destination, so they are neither judged
#   nor trusted: they are skipped as a SOURCE and ignored as a TARGET. That is
#   an under-report, and it is deliberate - the alternative is inventing a
#   verdict for a placement nobody has decided. `--contested` lists them so the
#   size of the blind spot is visible rather than silent.
#
# DEFECTS FOUND IN THIS SCRIPT, AND FIXED. Read the total as a FLOOR.
#
#   1. FIRST-PATH-SEGMENT TIERING (found 2026-09-01, by review).
#      `tier()` was keyed on `${rel%%/*}`, so `backend/postgres.rs` resolved to
#      module `backend`, hit the default arm, and became CONTESTED - skipped as
#      a source and ignored as a target. The `postgres)` and `sqlite)` arms were
#      therefore DEAD FOR EXACTLY THE FILES THEY EXIST TO JUDGE.
#      Measured blast radius before the fix: 18,803 of 57,391 lines, 32.8% of
#      the crate, unjudged as a source - all of backend/ (13,829), auth/ (1,459),
#      context.rs (1,688), service.rs (606), cdc_lifecycle.rs (523),
#      test_support/ (336), change_stream_pg.rs (315), replication_ops.rs (47).
#      This was NOT the documented CONTESTED policy: backend/postgres.rs IS
#      settled, and tier_signature_census.sh has always tiered it PG by matching
#      the full path. The two instruments silently disagreed about 13,829 lines
#      while both headers claimed to copy the same proposal table.
#      FIX: tier on the full relative path, mirroring the signature census.
#      It hid a real two-way cycle: backend/sqlite/cdc.rs calls up into
#      crate::broker at :543/:547/:646 while crud/ calls down into sqlite
#      encoders - neither arm was ever reported.
#      AFTER the fix the blind spot is 6,377 lines / 11.1%, across 10 files that
#      are genuinely unplaced (backend/mod.rs the contract tier, context.rs and
#      service.rs which the proposal itself files as undecided). That is the
#      honest residual, and it is the CONTESTED policy working as documented
#      rather than a path bug wearing its name.
#      Two violations became visible that no reviewer and neither census had
#      ever reported, both production:
#        backend/postgres.rs:469,565 -> crate::exec::query_postgres_pool_with_
#          autocommit_role   (PG -> ENGINE, a vendor calling UP into the engine)
#        backend/sqlite/session_minter.rs:116,139,154 -> crate::PluginDbConsumer
#          (SQLITE -> ADAPTER, rank 2 -> 4)
#
#   2. THE TARGET REGEX COULD NOT MATCH A CAPITAL (same review).
#      It was `crate::\K[a-z_0-9]+`, so a crate-root item spelled with an
#      uppercase initial matched NOTHING. Control:
#        printf 'use crate::PluginDbConsumer;\nuse crate::broker::X;\n' \
#          | grep -oP 'crate::\K[a-z_0-9]+'
#      prints `broker` and nothing else. That hid ENGINE -> ADAPTER
#      (crud/mod.rs `crate::BackendUrl`) and ENCRYPT -> ADAPTER
#      (encryption/keys.rs `crate::PluginDbConsumer`, a three-rank jump).
#
#   3. THE TARGET REGEX TRUNCATED TO ONE SEGMENT (same review).
#      `crate::backend::sqlite::foo` and `crate::backend::postgres::foo` both
#      collapsed to `backend`, so the two vendors were indistinguishable as
#      targets even once they were tierable as sources.
#
#   An uppercase crate-root target is resolved by LOOKING IT UP in lib.rs
#   rather than assuming it lives there. If it is not found, the row prints
#   UNRESOLVED and is counted - a future crate-root item must not be silently
#   mis-tiered the way defect 2 mis-tiered these two.
#
#   4. prod() WAS BLIND TO AN ITEM-LEVEL cfg (found 2026-09-01, within an hour
#      of the defect-1 fix, by a reviewer refuting a finding THIS SCRIPT had
#      just produced and I had reported as verified).
#      It excised `#[cfg(test)] mod X { .. }` REGIONS by the column-0 rule, and
#      nothing else. A gated `impl` / `fn` / `const` / `mod x;` passed straight
#      through as production. Proof, against the old filter:
#          #[cfg(any(test, feature = "test-helpers"))]
#          impl Foo { fn leaks() { crate::PluginDbConsumer } }
#      printed in full. That is how backend/sqlite/session_minter.rs:116/:139/
#      :154 were reported as production `SQLITE -> ADAPTER` violations when the
#      whole impl is gated at :100 - the file's own comment at :107-109 says so,
#      and the declared_env! calls even pass class `test`.
#      It is the SAME instrument error the signature census header names as its
#      artefact class, and the same one behind the exec.rs:370 retraction.
#      FIX: the column-0 rule now covers ANY gated item - block, one-line
#      declaration, or multi-line signature - and `#[cfg(not(...))]` is
#      explicitly NOT a gate, since it is the PRODUCTION arm of the two-arm
#      pattern and its `test-helpers` text would otherwise match.
#      Controlled BOTH ways, because a filter that drops everything passes a
#      one-directional check: four gated forms must vanish AND five live forms
#      (including both cfg(not(..)) arms) must survive. An intermediate version
#      passed the first half while silently eating a live impl whose gated
#      neighbour closed its body inline.
#      Cost of the miss: two FALSE cycles and one false task.
#
# TIER CYCLES - the question a crate split actually asks.
#   The per-edge verdict judges ONE edge at a time, so it structurally cannot
#   see a cycle: ENGINE -> SQLITE is rank 3 -> 2 and therefore "ok", while
#   SQLITE -> ENGINE is a violation, and a MUTUAL dependency prints as one
#   stray violation instead of as the hard blocker it is. Two crates that
#   reference each other cannot be separated at all - cargo has no way to
#   express it. Every run now ends with the cycle list.
#   Measured 2026-09-01, after defect 4 below was fixed: THREE cycles, where the
#   per-edge table had read as a scatter of 24 unrelated violations.
#     ADAPTER <-> ENGINE   (17 up / 7 down)  the dispatch surface
#     ENGINE  <-> SQLITE   (4 up / 8 down)   the broker publish path
#     ENGINE  <-> PG       (1/1)             backend/postgres.rs -> crate::exec
#   The FIRST run reported FIVE, adding ADAPTER <-> ENCRYPT and
#   ADAPTER <-> SQLITE. Both were FALSE, manufactured by defect 4: every edge
#   involved sits inside a cfg-gated item. Do not cite the five-cycle figure.
#
# USAGE
#   tests/lib/tier_direction_census.sh              # violations + cycles
#   tests/lib/tier_direction_census.sh --all        # every cross-tier edge
#   tests/lib/tier_direction_census.sh --contested  # what is not being judged
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SRC="$ROOT/crates/zeroship-plugin-db/src"
MODE="${1:-violations}"
cd "$SRC" || { echo "no such tree: $SRC" >&2; exit 1; }

# Destination crate per FILE, keyed on the full relative path. This is a copy of
# tier_signature_census.sh's tier(), deliberately - the two censuses judging the
# same file differently is defect 1 above.
tier_of_file() {
  case "$1" in
    ./v8_classes/*|./v8_bridge.rs|./lib.rs|./tx_scope.rs)  echo ADAPTER ;;
    ./crud/*|./transaction/*|./exec.rs|./broker.rs|./read_set.rs|./tx_route.rs|./drop_namespace.rs|./cross_app_fk.rs) echo ENGINE ;;
    ./auth/bootstrap.rs)                                 echo ENGINE ;;
    ./backend/postgres.rs|./backend/pg_row_json.rs)      echo PG ;;
    ./backend/sqlite/*)                                  echo SQLITE ;;
    ./encryption/*)                                      echo ENCRYPT ;;
    ./wal_consumer.rs|./replication.rs|./slot_reaper.rs) echo CDC ;;
    ./error.rs|./descriptor.rs|./binding.rs)             echo CORE ;;
    *)                                                   echo CONTESTED ;;
  esac
}

# Destination crate per TARGET PATH, keyed on the 1-or-2 segments captured after
# `crate::`. Distinct from tier_of_file because a target is a module path, not a
# file path, and may be a crate-root item.
tier_of_target() {
  case "$1" in
    backend::sqlite)                                     echo SQLITE ;;
    backend::postgres|backend::pg_row_json)              echo PG ;;
    v8_classes*|v8_bridge*|tx_scope*)                    echo ADAPTER ;;
    crud*|transaction*|exec*|broker*|read_set*|tx_route*|drop_namespace*|cross_app_fk*) echo ENGINE ;;
    auth::bootstrap)                                     echo ENGINE ;;
    encryption*)                                         echo ENCRYPT ;;
    wal_consumer*|replication*|slot_reaper*)             echo CDC ;;
    error*|descriptor*|binding*)                         echo CORE ;;
    [A-Z]*)
      # A crate-root item. Resolve it rather than assume: lib.rs is where
      # crate-root items live TODAY, and the script must say so out loud if that
      # ever stops being true.
      if grep -qE "(enum|struct|trait|type|fn|const)[[:space:]]+${1%%::*}\b|\b${1%%::*},$" lib.rs 2>/dev/null; then
        echo ADAPTER
      else
        echo UNRESOLVED
      fi ;;
    *)                                                   echo CONTESTED ;;
  esac
}

rank() {
  case "$1" in
    ADAPTER) echo 4 ;; ENGINE) echo 3 ;;
    PG|SQLITE|CDC) echo 2 ;; ENCRYPT) echo 1 ;; CORE) echo 0 ;;
    *) echo -1 ;;
  esac
}

# Production region: strip comments, excise cfg(test)-gated modules by the
# column-0 rule. Same approach as the signature census, and for the same reason
# - brace counting loses to string literals, and #[cfg(all(test, ...))] is not
# #[cfg(test)].
prod() {
  awk '
    /^[[:space:]]*(\/\/\/|\/\/!|\/\/)/ { next }
    # A column-0 #[cfg(...test...)] gates WHATEVER ITEM FOLLOWS - not only a
    # `mod`. See defect 4 in the header: restricting this to `mod` let every
    # gated `impl` / `fn` / `const` through as production.
    # `#[cfg(not(...))]` is the PRODUCTION arm of a two-arm pattern, and the
    # word `test` inside `not(feature = "test-helpers")` must NOT gate it.
    # Matching it would drop the production half of all 24 two-arm sites.
    !intest && /^#\[cfg\(not\(/ { pend=0; print; next }
    !intest && /^#\[cfg\(/ && /(^|[^A-Za-z_])test([^A-Za-z_]|$)/ { pend=1; next }
    # Further attributes and blank lines sit between the cfg and its item.
    pend && /^#\[/ { next }
    pend && /^[[:space:]]*$/ { next }
    pend && /^[a-zA-Z]/ { pend=0; seek=1 }
    # `seek` spans the gated item HEADER, which may run over several lines for a
    # multi-line signature. Resolve it three ways, and only the middle one opens
    # a skipped block - an earlier version entered the block unconditionally and
    # ate the next LIVE item when the gated one closed its body inline.
    seek {
      # `pub mod x;` / `pub use y;` / `const Z: T = v;` - one line, done.
      if ($0 ~ /;[[:space:]]*$/) { seek=0; next }
      # Body opens and closes on this line - done, no block to skip.
      if ($0 ~ /\{/ && $0 ~ /\}/) { seek=0; next }
      # Body opens here - skip to the column-0 close.
      if ($0 ~ /\{[[:space:]]*$/) { seek=0; intest=1; next }
      # Still inside a multi-line signature.
      next
    }
    { pend=0 }
    intest { if ($0 == "}") intest=0; next }
    { print }' "$1"
}

EDGES="$(mktemp)"
trap 'rm -f "$EDGES"' EXIT

printf '%-9s %-34s %-10s %-26s %s\n' FROM FILE TO TARGET VERDICT
echo "----------------------------------------------------------------------------------------------"
while read -r f; do
  rel="${f#./}"
  st=$(tier_of_file "$f")
  [ "$st" = CONTESTED ] && continue
  sr=$(rank "$st")
  # Own module name, for skipping self-references.
  case "$rel" in */*) selfmod="${rel%%/*}" ;; *) selfmod="${rel%.rs}" ;; esac

  prod "$f" | grep -oP 'crate::\K[A-Za-z_0-9]+(::[a-z_0-9]+)?' | sort -u | while read -r tpath; do
    [ "${tpath%%::*}" = "$selfmod" ] && continue
    tt=$(tier_of_target "$tpath")
    [ "$tt" = CONTESTED ] && continue
    if [ "$tt" = UNRESOLVED ]; then
      printf '%-9s %-34s %-10s %-26s %s\n' "$st" "$rel" "UNRESOLVED" "crate::$tpath" "** UNRESOLVED - tier it **"
      continue
    fi
    tr=$(rank "$tt")
    # Record EVERY cross-tier edge, ok or not. The verdict below judges one edge
    # at a time and so cannot see a cycle: ENGINE -> SQLITE is rank 3 -> 2 and
    # therefore "ok", while SQLITE -> ENGINE is a violation, so a mutual
    # dependency prints as one stray violation rather than as the hard blocker
    # it is. Two crates that reference each other cannot be split at all.
    [ "$st" != "$tt" ] && echo "$st $tt" >> "$EDGES"
    if [ "$tr" -lt "$sr" ]; then v=ok
    elif [ "$tr" -eq "$sr" ] && [ "$st" = "$tt" ]; then v=ok
    else v="** VIOLATION **"
    fi
    [ "$v" = ok ] && [ "$MODE" != "--all" ] && continue
    printf '%-9s %-34s %-10s %-26s %s\n' "$st" "$rel" "$tt" "crate::$tpath" "$v"
  done
done < <(find . -name '*.rs' | LC_ALL=C sort)

echo "----------------------------------------------------------------------------------------------"

# TIER CYCLES. A pair of tiers with edges in BOTH directions cannot become two
# crates - cargo has no way to express it. This is strictly stronger than the
# per-edge verdict above and is the question a crate split actually asks.
if [ -s "$EDGES" ]; then
  echo
  echo "TIER CYCLES (both directions present - these pairs CANNOT be separate crates):"
  sort -u "$EDGES" | while read -r a b; do
    # Print each unordered pair once.
    [ "$a" \> "$b" ] && continue
    if grep -qx "$b $a" "$EDGES"; then
      fwd=$(grep -cx "$a $b" "$EDGES")
      rev=$(grep -cx "$b $a" "$EDGES")
      printf '  %-9s <-> %-9s   %s edges one way, %s the other\n' "$a" "$b" "$fwd" "$rev"
    fi
  done
  if [ -z "$(sort -u "$EDGES" | while read -r a b; do [ "$a" \> "$b" ] && continue; grep -qx "$b $a" "$EDGES" && echo x; done)" ]; then
    echo "  none - every tier pair references in one direction only"
  fi
  echo
  echo "  A cycle is a HARD blocker; a lone upward edge is a design smell. The"
  echo "  per-edge table above reports only the upward half of each cycle, which"
  echo "  is why they were read as scattered violations rather than as pairs."
fi

if [ "$MODE" = "--contested" ]; then
  echo "Files with no settled tier (neither judged nor trusted):"
  find . -name '*.rs' | LC_ALL=C sort | while read -r f; do
    [ "$(tier_of_file "$f")" = CONTESTED ] && echo "  ${f#./}"
  done | sort -u
fi
echo
echo "Every verdict is relative to tier_of_file/tier_of_target above, which are a"
echo "copy of the proposal's assignment table. Re-draw a boundary there and re-draw"
echo "it here in the same commit, or this reports on a shape nobody proposed."
echo "Read the count as a FLOOR: see the DEFECTS block in this header for what it"
echo "has already been blind to, twice."
