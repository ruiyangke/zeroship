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
#     3  ENGINE     crud, transaction, exec
#     2  PG SQLITE CDC   drivers and the relay - peers, mutually forbidden
#     1  ENCRYPT
#     0  CORE       DbError, descriptor, binding, budgets, broker, read_set
#
#   `broker` sat at rank 3 until 2026-09-03 and that was the error the CDC cut
#   kept tripping over: the ENGINE publishes into it on local mutation and CDC
#   publishes into it from the WAL, so it is named by two tiers and belongs
#   below both. It is rank 0 now, with `read_set` (which it evaluates) beside it.
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
#   5. TEST-ONLY MODULES WERE SCANNED AS PRODUCTION. Documented in full beside
#      `declared_test_only()` below, because the fix IS that function.
#
#   6. A SELF-REFERENCE SKIP KEYED ON THE FIRST PATH SEGMENT, so every file
#      under backend/ computed selfmod=backend and discarded every
#      `crate::backend...` target - the exact region three open design tasks
#      are about. Defect 2 had already re-keyed tier_of_file to the FULL path
#      so backend/postgres.rs and backend/sqlite/* would land in different
#      tiers; this line kept the old key, so the files were judged as three
#      tiers while all their mutual references were thrown away as "self".
#      It was also redundant: an intra-tier reference is already recorded as
#      no edge and marked `ok`, which the default mode filters.
#      FIX: the skip is deleted, and a CONTESTED target is now COUNTED and
#      listable (`--dropped`) instead of silently discarded.
#      MEASURED with a four-tree control, one variable apart. Removing the skip
#      ALONE changed the violation count not at all (21 in all four trees) -
#      the two mechanisms mask each other, and either fix alone reads as a
#      no-op. Together they took the dropped count from 60 to 66; the six are
#      backend/{postgres.rs,sqlite/{cdc,dialect,mod,spatial,vector}.rs}
#      -> crate::backend.
#      Cost of the miss: NOT a wrong verdict, but a blind one. The header's
#      own peer rule - backend/sqlite/ must not name the Postgres decoder -
#      was advertised and unenforced. It is clean today by luck.
#      WHAT THE FIX REVEALS is larger than the defect: 66 references from
#      tiered files reached targets with no tier.
#
#   7. THE FIRST READING OF THAT 66 WAS WRONG, WITHIN THE HOUR, AND IT IS THE
#      MORE USEFUL DEFECT. It was reported here as "seven untiered modules, led
#      by `crate::query` at 22 - the module decision 4 routes ALL SQL through,
#      never placed in the lattice". `crate::query` IS NOT A MODULE OF THIS
#      CRATE. There is no query.rs and no query/ directory; lib.rs:94 says
#      `pub use zeroship_schema::query;`, and lib.rs:150 does the same for
#      `diff`. Both resolve to a DEPENDENCY crate, which is below every tier
#      here by construction - cargo already forbids that cycle.
#      The error was reading "the census has no arm for it" as "nobody has
#      placed it", when the true cause was "it is not ours to place". A grep
#      for the module NAME cannot tell those apart; only resolving the name
#      can, which is why the fix RESOLVES rather than lists.
#      FIX: `tier_of_target`'s default arm now checks lib.rs for a re-export
#      and answers EXTERNAL, counted in its own bucket.
#      MEASURED: 66 splits exactly into 42 unplaced + 24 external, and the
#      violation count is 21 before and after - this changes what the dropped
#      set MEANS, not what the census judges. The 42 are context (19),
#      backend (13), metrics (5), cdc_lifecycle (4), init_pool_async (1).
#      Consequence for the split: the query builder needs no new home. It has
#      one, and decision 4's "everything goes to the query builder" already
#      names a separate crate rather than proposing one.
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
#   tests/lib/tier_direction_census.sh --dropped    # refs INTO what is not judged
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# ---------------------------------------------------------------------------
# THE TIERS SPAN TWO CRATES SINCE 2026-09-03, AND SO DOES THIS SCAN.
# ---------------------------------------------------------------------------
# `SRC` was a single path, `crates/zeroship-plugin-db/src`, and the ENGINE tier
# left that tree for `crates/zeroship-data-engine/src` the day this changed.
# Left alone, the census would have scanned the ~15k lines that stayed and
# printed exactly what a clean tree prints about the ~22k that went - the defect
# class this file's own header is a catalogue of.
#
# SCANNED AS A UNION, NOT SUMMED, and the reason is that a tier is not a crate:
# what this census judges is whether a file in tier N references a module in
# tier M, and that question is identical whether the two ended up in one cargo
# package or two. Summing two independent runs would ALSO lose every edge whose
# source and target now sit on opposite sides of the cut - which is precisely
# the set the split is about. One `tier_of_file` map, one `tier_of_target` map,
# one table, one set of counts: the arms rule on the same files they ruled on
# before, plus the ones that moved.
#
# `crate::` still means "this tier's vocabulary" on both sides, because
# `zeroship-data-engine/src/lib.rs` re-exports the same neutral modules
# `zeroship-plugin-db/src/lib.rs` does (`query`, `diff`, `broker`, `read_set`,
# `encryption`, `budgets`, `lock_policy`), and the adapter re-exports the engine's
# back. A path spelled `crate::crud::…` resolves to the same item from either
# crate, which is exactly what makes the union scan sound.
SRC_ROOTS=(
  "$ROOT/crates/zeroship-plugin-db/src"
  "$ROOT/crates/zeroship-data-engine/src"
)
for _root in "${SRC_ROOTS[@]}"; do
  [ -d "$_root" ] || { echo "no such tree: $_root" >&2; exit 1; }
done
MODE="${1:-violations}"

# Does a path exist under ANY source root?
#
# The file-existence resolutions in `tier_of_target` below used a bare `[ -f x ]`
# against one cwd. With two roots that answers "is it in the crate I happen to
# be scanning", which is not the question - the question is whether the module is
# still OURS to place at all. This asks it across the whole scanned region.
src_path_exists() {
  local p
  for p in "${SRC_ROOTS[@]}"; do
    [ -e "$p/$1" ] && return 0
  done
  return 1
}

# Destination crate per FILE, keyed on the full relative path. This is a copy of
# tier_signature_census.sh's tier(), deliberately - the two censuses judging the
# same file differently is defect 1 above.
tier_of_file() {
  case "$1" in
    ./v8_classes/*|./v8_bridge.rs|./lib.rs|./tx_scope.rs)  echo ADAPTER ;;
    ./crud/*|./transaction/*|./exec.rs|./backend_selection.rs|./tx_route.rs|./drop_namespace.rs) echo ENGINE ;;
    ./auth/bootstrap.rs)                                 echo ENGINE ;;
    # NO ARMS for ./backend/pg_*.rs, ./backend/postgres.rs, ./backend/sqlite/*,
    # ./encryption/*, ./lock_policy.rs, ./broker.rs or ./read_set.rs. Every one
    # of those was extracted into a dependency crate, and an arm for a file that
    # does not exist is not inert - see the tier_of_target note below for what it
    # cost. Measured 2026-09-03: ./backend/ holds only cancel.rs and mod.rs, and
    # ./encryption/ is gone. `broker.rs` and `read_set.rs` left the same day, for
    # `zeroship-data-core`: the broker is published into by the ENGINE
    # (`exec::emit_local`) AND by CDC (`wal_consumer`), so it had to sit below
    # both, and `read_set` went first because the broker names `ReadSetEntry`.
    # NO ARMS for ./error.rs, ./binding.rs or ./budgets.rs either. Same reason,
    # earlier moves: all three are `zeroship-data-core`'s now.
    ./wal_consumer.rs|./replication.rs|./slot_reaper.rs) echo CDC ;;
    # Settled by docs/proposals/2026-09-02-thread-context-ownership.md, whose
    # ownership table places `lanes` and `mask_policies` in data-engine,
    # `schemas` in data-core, and the pool/backend slots in the adapter -
    # making context.rs "the composition root: an adapter concern by
    # definition". Until 2026-09-02 these files had NO arm and so were skipped
    # entirely, which is how 42 references INTO them were dropped unjudged,
    # including every engine-to-adapter call the split has left to remove.
    ./context.rs|./service.rs|./op_error.rs)              echo ADAPTER ;;
    # BackendHandle and TxCanceller name BOTH vendors, so only a tier above both
    # may hold them; the crate-shape proposal puts them in data-engine.
    ./tx_lanes.rs|./backend_handle.rs|./backend/cancel.rs|./system_shape_charter.rs|./metrics.rs) echo ENGINE ;;
    ./cdc_lifecycle.rs|./change_stream_pg.rs)            echo CDC ;;
    # `descriptor.rs` was CORE here and data-engine in the crate-shape proposal,
    # and the two disagreed for two days. SETTLED as ENGINE on 2026-09-03 by the
    # cut itself: the file left with the engine tier, and the argument that put
    # it at rank 0 - "named by two tiers, so below both" - does not survive
    # measurement. It is 130 lines wrapping `zeroship_data_core::schema_cache`,
    # 25 of its call sites are in `crud/`, and three are in the adapter, which is
    # ADAPTER -> ENGINE and legal. The rank-0 primitive it wraps is already in
    # data-core; this is the engine's accessor for it.
    ./descriptor.rs)                                     echo ENGINE ;;
    # `backend/mod.rs` was CONTESTED for one stated reason: it "also names
    # replication, wal_consumer and change_stream_pg, which are CDC". That was
    # true of ONE `#[cfg(test)]` assertion, which moved to `change_stream_pg.rs`
    # on 2026-09-03 - the fact it pins is about `PgChangeStream`, so it belongs
    # beside it. What is left is a prelude of re-exports from data-core, both
    # vendor crates and zeroship-schema, plus this tier's own `BackendHandle`
    # and a test-only conformance marker: all at or below ENGINE. Issue #170
    # closes here.
    ./backend/mod.rs)                                    echo ENGINE ;;
    # THREE FILES ARE DELIBERATELY LEFT CONTESTED, and each has a reason that is
    # an open QUESTION rather than an omission:
    #   auth/mod.rs, auth/util.rs - #156 asks whether auth/ is deleted outright
    #     (zero production callers, a live twin in migrate-server). Tiering code
    #     that may not exist would assert a placement for it. They travelled to
    #     `zeroship-data-engine` with `auth/bootstrap.rs`, which needs the
    #     `APP_ROLE_TEMPLATE` anchor `mod.rs` holds; that is a consequence of
    #     the move, not an answer to #156.
    #   test_support/mod.rs - test-only; it ships in no build.
    # Anything else landing here IS an omission. Add an arm above.
    *)                                                   echo CONTESTED ;;
  esac
}

# Destination crate per TARGET PATH, keyed on the 1-or-2 segments captured after
# `crate::`. Distinct from tier_of_file because a target is a module path, not a
# file path, and may be a crate-root item.
tier_of_target() {
  case "$1" in
    # RESOLVE these, do not assert them. Each of these names was a module of THIS
    # crate when the arm was written and is now a re-export of a dependency crate
    # (`backend/mod.rs`: `pub use zeroship_data_sqlite as sqlite`, `pub use
    # zeroship_data_postgres::{PostgresBackend, pg_error, pg_row_json, postgres}`,
    # `pub use zeroship_data_postgres::{pg_autocommit, pg_session_sql}`).
    #
    # THIS IS DEFECT 7 IN MIRROR IMAGE. Defect 7 was reading "the census has no arm
    # for it" as "nobody has placed it". This was an arm existing for something no
    # longer OURS to place, so a resolved dependency edge was reported as an
    # unresolved placement question. The EXTERNAL fallback below cannot save it:
    # a hardcoded arm wins first, and that fallback greps only lib.rs while these
    # re-exports live in backend/mod.rs.
    #
    # MEASURED 2026-09-03 with a one-variable control (two copies of this script,
    # ROOT pinned, differing only in these arms): 7 violations before, 4 after. The
    # three that vanished were `change_stream_pg.rs -> backend::postgres`,
    # `replication.rs -> backend::pg_error` and `slot_reaper.rs -> backend::pg_error`
    # - CDC files naming a dependency crate, which cargo already governs.
    #
    # Resolving by file existence is self-maintaining: the day a module leaves the
    # crate, the census follows it instead of silently mis-tiering the edge.
    backend::sqlite)
      if src_path_exists backend/sqlite; then echo SQLITE; else echo EXTERNAL; fi ;;
    backend::postgres|backend::pg_row_json|backend::pg_session_sql|backend::pg_autocommit|backend::pg_error|backend::pg_introspect)
      if src_path_exists "backend/${1#backend::}.rs"; then echo PG; else echo EXTERNAL; fi ;;
    v8_classes*|v8_bridge*|tx_scope*)                    echo ADAPTER ;;
    crud*|transaction*|exec*|backend_selection*|tx_route*|drop_namespace*) echo ENGINE ;;
    auth::bootstrap)                                     echo ENGINE ;;
    # Same file-existence resolution as `backend::*` and `encryption*` above,
    # and for the same reason: both left for `zeroship-data-core` on 2026-09-03
    # and lib.rs now re-exports them (`pub use zeroship_data_core::broker;`,
    # `pub use zeroship_data_core::read_set;`). Asserting ENGINE here would keep
    # reporting `wal_consumer.rs -> crate::broker` as a CDC-to-ENGINE up-edge
    # after cargo had already made it a plain dependency edge - which is the
    # whole point of the move.
    broker*)
      if src_path_exists broker.rs; then echo ENGINE; else echo EXTERNAL; fi ;;
    read_set*)
      if src_path_exists read_set.rs; then echo ENGINE; else echo EXTERNAL; fi ;;
    # Same treatment: `src/encryption/` no longer exists; lib.rs re-exports
    # `zeroship_data_core::encryption`. The ENCRYPT tier has zero files in this
    # crate - it is already extracted.
    encryption*)
      if src_path_exists encryption; then echo ENCRYPT; else echo EXTERNAL; fi ;;
    wal_consumer*|replication*|slot_reaper*)             echo CDC ;;
    context*|service*|op_error*)                          echo ADAPTER ;;
    tx_lanes*|backend_handle*|system_shape_charter*|metrics*) echo ENGINE ;;
    # Same file-existence resolution, and it earned it the same way: `cancel.rs`
    # left with the ENGINE tier for `zeroship-data-engine`, and an arm asserting
    # a tier for a file this region no longer holds is the rot documented at the
    # top of this function.
    backend::cancel)
      if src_path_exists backend/cancel.rs; then echo ENGINE; else echo EXTERNAL; fi ;;
    cdc_lifecycle*|change_stream_pg*)                    echo CDC ;;
    # DEFECT 7 IN MIRROR IMAGE, ONE ARM SHORT, FOUND 2026-09-02 AND FIXED HERE.
    # This read `error*|descriptor*|binding*|budgets*) echo CORE`, and THREE of
    # those four files did not exist. As a `tier_of_file` key that is inert -
    # `find` never yields them - but as a TARGET key it is not: `crate::error`
    # and `crate::budgets` resolve through `zeroship-data-core` re-exports and
    # are EXTERNAL, so the arm asserted CORE for a resolved dependency edge.
    # It changed no verdict (CORE and EXTERNAL both rank below every ENGINE
    # source), which is exactly why it survived - a wrong arm that agrees with
    # the right answer on today's inputs is invisible until the inputs move.
    #
    # `descriptor` is the one that still exists, and it is ENGINE now: see the
    # `./descriptor.rs` note in `tier_of_file`.
    descriptor*)
      if src_path_exists descriptor.rs; then echo ENGINE; else echo EXTERNAL; fi ;;
    error*|binding*|budgets*)
      if src_path_exists "${1%%::*}.rs"; then echo CORE; else echo EXTERNAL; fi ;;
    [A-Z]*)
      # A crate-root item. Resolve it rather than assume: lib.rs is where
      # crate-root items live TODAY, and the script must say so out loud if that
      # ever stops being true.
      if grep -qE "(enum|struct|trait|type|fn|const)[[:space:]]+${1%%::*}\b|\b${1%%::*},$" lib.rs 2>/dev/null; then
        echo ADAPTER
      else
        echo UNRESOLVED
      fi ;;
    *)
      # Not a known module. Before calling it unplaced, ask whether it is a
      # RE-EXPORT of a dependency crate: `pub use zeroship_schema::query;` makes
      # `crate::query::quote_ident` resolve OUTSIDE this crate entirely, so it
      # is a dependency edge, not an intra-crate placement question at all.
      # Missing this is how 24 of 66 dropped references read as "modules nobody
      # has tiered" on 2026-09-01, `crate::query` at 22 among them - the query
      # builder, reported as unplaced when it is already its own crate.
      if grep -qE "^pub use [a-z_][a-z_0-9]*::${1%%::*};" lib.rs 2>/dev/null; then
        echo EXTERNAL
      else
        echo CONTESTED
      fi ;;
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

# Is this file's module declared ONLY behind a test gate in its parent?
#
# DEFECT 5, found 2026-09-01 by a reviewer re-deriving a count the brief handed
# out as settled. `prod()` was taught (defect 4) to see an item-level `#[cfg]`
# INSIDE a file. It still could not see a gate one directory up, on the parent's
# `mod` line - and a per-file scan structurally cannot. So `crud/mask_drift.rs`
# contributed SIX edge rows, including one of the eight `ENGINE -> SQLITE`
# down-edges, while its sole declaration was
#
#     crud/mod.rs  #[cfg(any(test, feature = "test-helpers"))]
#     crud/mod.rs  pub mod mask_drift;
#
# i.e. it was in no production build at all. The true production count for that
# cycle was 7 down, not 8.
#
# That file was DELETED on 2026-09-03, so it is history rather than a live
# example; `transaction/probe.rs` and `auth/util.rs` are the modules this rule
# still excludes. The rule is unchanged - the deletion removes a case, not the
# need for the check.
#
# THIS MATTERS BEYOND ONE FILE: the header promises the count is a FLOOR. For
# UP-edges it is. For DOWN-edges it was not - the census could OVER-count, and a
# floor that can over-count is not a floor. Anyone doing arithmetic on down-edge
# tallies (progress on the ADAPTER <-> ENGINE cycle runs through this same
# scanner) was measuring test-only code.
#
# The rule mirrors `prod()`'s: a module declared TWICE is the two-arm
# production/test-helpers pair and is production. Declared ONCE and gated, it is
# test-only. Declared once ungated, production.
declared_test_only() {
  local rel="$1" modname parent decls gated
  modname="$(basename "$rel" .rs)"
  case "$rel" in
    */*) parent="$(dirname "$rel")/mod.rs"
         # A `foo/mod.rs` is declared by its GRANDparent as `mod foo;`.
         if [ "$modname" = mod ]; then
           modname="$(basename "$(dirname "$rel")")"
           parent="$(dirname "$(dirname "$rel")")/mod.rs"
           [ "$(dirname "$(dirname "$rel")")" = "." ] && parent="./lib.rs"
         fi ;;
    *)   parent="./lib.rs" ;;
  esac
  [ -f "$parent" ] || return 1
  decls=$(grep -cE "^[[:space:]]*(pub(\([^)]*\))?[[:space:]]+)?mod[[:space:]]+${modname}[[:space:]]*;" "$parent")
  [ "$decls" -eq 1 ] || return 1
  # Exactly one declaration: is the line above it a cfg gate?
  gated=$(grep -B1 -E "^[[:space:]]*(pub(\([^)]*\))?[[:space:]]+)?mod[[:space:]]+${modname}[[:space:]]*;" "$parent" \
            | grep -cE '^[[:space:]]*#\[cfg\((any\()?test|^[[:space:]]*#\[cfg\(feature[[:space:]]*=[[:space:]]*"test-helpers"')
  [ "$gated" -ge 1 ]
}

EDGES="$(mktemp)"
DROPPED="$(mktemp)"
EXTERNALS="$(mktemp)"
trap 'rm -f "$EDGES" "$DROPPED" "$EXTERNALS"' EXIT

# Every `.rs` under every source root, as `<root>\t<./-relative path>`.
#
# The `./`-relative half is what `tier_of_file` and `declared_test_only` are
# keyed on and must stay exactly that; the root half is what the loop `cd`s into
# so `prod`, `declared_test_only` and `tier_of_target`'s lib.rs lookups resolve
# against the crate the file actually lives in.
all_sources() {
  local p
  for p in "${SRC_ROOTS[@]}"; do
    ( cd "$p" && find . -name '*.rs' | LC_ALL=C sort | sed "s|^|$p\t|" )
  done
}

# `plugin-db/crud/mod.rs` rather than `./crud/mod.rs`: with two crates scanned as
# one region, a bare relative path no longer says which tree a row came from.
#
# NEWLINE-TERMINATED, and it must be: `n_contested` pipes this into
# `sort -u | grep -c .`, and a `printf` without the `\n` ran all three contested
# files together on one line and reported the blind spot as 1 file instead of 3.
# The LIST printed correctly the whole time, because `echo "$(label ...)"` adds
# its own - so the count and the listing disagreed while both looked right.
label() { printf '%s/%s\n' "$(basename "$(dirname "$1")" | sed 's/^zeroship-//')" "$2"; }

n_test_only=0
printf '%-9s %-46s %-10s %-26s %s\n' FROM FILE TO TARGET VERDICT
echo "--------------------------------------------------------------------------------------------------------"
while IFS=$'\t' read -r croot f; do
  cd "$croot" || continue
  rel="$(label "$croot" "${f#./}")"
  st=$(tier_of_file "$f")
  [ "$st" = CONTESTED ] && continue
  # Defect 5: a module gated one-arm in its PARENT is in no production build.
  declared_test_only "${f#./}" && { n_test_only=$((n_test_only + 1)); continue; }
  sr=$(rank "$st")

  # DEFECT 6, 2026-09-01. There used to be a self-reference skip here:
  #
  #     case "$rel" in */*) selfmod="${rel%%/*}" ;; *) selfmod="${rel%.rs}" ;; esac
  #     [ "${tpath%%::*}" = "$selfmod" ] && continue
  #
  # It keyed on the FIRST PATH SEGMENT, so every file under `backend/` yielded
  # `selfmod=backend` and silently discarded every `crate::backend...` target -
  # in BOTH directions. Defect 2 re-keyed `tier_of_file` to the full path
  # precisely so `backend/postgres.rs` and `backend/sqlite/*` would land in
  # DIFFERENT tiers; this line was left on the old key, so the fix was half
  # applied and the files were judged as three tiers while all their mutual
  # references were thrown away as "self".
  #
  # IT WAS ALSO REDUNDANT. Its stated job - not reporting intra-module
  # references - is already done below: `:301` records an edge only when
  # `$st != $tt`, and the verdict marks same-tier as `ok`, which the default
  # mode filters. Removing it changes nothing for a file referencing its own
  # tier, and restores every cross-tier edge inside `backend/`.
  #
  # THAT IS WHAT MAKES THE HEADER'S OWN RULE ENFORCEABLE. This script claims to
  # stop `backend/sqlite/` naming the Postgres WAL decoder; with the skip in
  # place a `crate::backend::sqlite::...` reference from `backend/postgres.rs`
  # was dropped, so the rule was advertised and unguarded. It is clean today by
  # luck, not by measurement.
  prod "$f" | grep -oP 'crate::\K[A-Za-z_0-9]+(::[a-z_0-9]+)?' | sort -u | while read -r tpath; do
    tt=$(tier_of_target "$tpath")
    # A CONTESTED target is dropped, and until 2026-09-01 that was silent too.
    # Count it: an unplaced target is a HOLE IN THE MEASUREMENT, and the reader
    # needs its size to know what a clean run is worth. `crate::backend::X` with
    # an uppercase item lands here, because the extractor's second segment is
    # lowercase-only and the capture degrades to bare `backend`.
    # A re-export resolves to a DEPENDENCY crate, which is strictly below every
    # tier here by construction: cargo already forbids the cycle. It is not an
    # unplaced module and must not be counted as one.
    if [ "$tt" = EXTERNAL ]; then
      printf '%s -> crate::%s\n' "$rel" "$tpath" >> "$EXTERNALS"
      continue
    fi
    if [ "$tt" = CONTESTED ]; then
      printf '%s -> crate::%s\n' "$rel" "$tpath" >> "$DROPPED"
      continue
    fi
    if [ "$tt" = UNRESOLVED ]; then
      printf '%-9s %-46s %-10s %-26s %s\n' "$st" "$rel" "UNRESOLVED" "crate::$tpath" "** UNRESOLVED - tier it **"
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
    printf '%-9s %-46s %-10s %-26s %s\n' "$st" "$rel" "$tt" "crate::$tpath" "$v"
  done
done < <(all_sources)

echo "--------------------------------------------------------------------------------------------------------"

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

# A CONTESTED file is skipped at `:233`, and until 2026-09-01 the default run
# said nothing about that - the list was behind an opt-in `--contested` flag you
# had to know to ask for. So a NEW file was invisible by default, and on
# 2026-09-01 one was: `backend/pg_autocommit.rs` landed in f77ead1d8 carrying the
# edge that commit exists to redirect, and every edge into and out of it was
# dropped silently. The cycle verdict happened to survive - the upward half was
# genuinely gone - but the instrument had been blinded by the same commit it was
# being used to judge. An unjudged file is a HOLE IN THE MEASUREMENT and the
# default output must say how big it is.
n_contested=$(
  all_sources | while IFS=$'\t' read -r croot f; do
    [ "$(tier_of_file "$f")" = CONTESTED ] && label "$croot" "${f#./}"
  done | sort -u | grep -c . || true
)
echo
if [ "$MODE" = "--dropped" ]; then
  echo "Tiered-source references to an untiered target ($(sort -u "$DROPPED" | wc -l)):"
  sort -u "$DROPPED" | sed 's/^/  /'
elif [ "$MODE" = "--contested" ]; then
  echo "Files with no settled tier ($n_contested; neither judged nor trusted):"
  all_sources | while IFS=$'\t' read -r croot f; do
    [ "$(tier_of_file "$f")" = CONTESTED ] && echo "  $(label "$croot" "${f#./}")"
  done | sort -u
else
  echo "TEST-ONLY: $n_test_only file(s) whose module is gated one-arm in its parent"
  echo "  were excluded. Their edges are not in any production build; counting them"
  echo "  is how the down-edge tallies over-reported (defect 5 in this header)."
  echo
  echo "UNJUDGED: $n_contested file(s) have no settled tier and were SKIPPED."
  echo "  Every edge into or out of them is absent from the table above. Run"
  echo "  '$0 --contested' to list them. A file added without a tier_of_file arm"
  echo "  lands here, and a skipped file prints exactly what a clean file prints."
  echo
  n_dropped=$(sort -u "$DROPPED" 2>/dev/null | wc -l)
  n_ext=$(sort -u "$EXTERNALS" 2>/dev/null | wc -l)
  echo "DROPPED: $n_dropped reference(s) from a TIERED file to an UNTIERED module."
  echo "  These are the edges the table cannot judge from the OTHER side: the"
  echo "  source has a tier, the target does not, so the row is discarded. Until"
  echo "  2026-09-01 that discard was silent, which is defect 6 - the census could"
  echo "  report zero violations while dropping every edge in the region under"
  echo "  active design. Run '$0 --dropped' to list them."
  echo
  echo "EXTERNAL: $n_ext reference(s) resolve through a lib.rs re-export to a"
  echo "  DEPENDENCY crate. Not a placement question - cargo already forbids the"
  echo "  cycle. Counted apart from DROPPED because lumping the two is how the"
  echo "  query builder was reported as an untiered module on 2026-09-01."
fi
echo
echo "Every verdict is relative to tier_of_file/tier_of_target above, which are a"
echo "copy of the proposal's assignment table. Re-draw a boundary there and re-draw"
echo "it here in the same commit, or this reports on a shape nobody proposed."
echo "UP-edge counts are a FLOOR - the DEFECTS block in this header lists what this"
echo "script has already been blind to. DOWN-edge counts were NOT a floor until"
echo "2026-09-01: they could OVER-report, because test-only modules were scanned."
echo "See defect 5. Re-derive any tally before doing arithmetic on it."
