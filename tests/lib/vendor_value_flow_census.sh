#!/usr/bin/env bash
# Which modules HOLD a vendor value without ever NAMING the vendor?
#
# WHY THIS EXISTS, AND WHY THE OTHER TWO CENSUSES CANNOT ANSWER IT
#   The proposal's vendor-neutrality mechanism is the manifest fence: a crate
#   that does not declare `compio-postgres` cannot write `compio_postgres::Error`,
#   because that is E0433 at compile time. On 2026-09-01 a reviewer showed the
#   fence stops the SPELLING, not the VALUE:
#
#       zeroship-schema/src/error.rs:31   pub source: compio_postgres::Error
#       plugin-db/src/error.rs:944        coded_sql(&format!(...), e.source)
#
#   The second line hands a live driver error to a Postgres classifier and the
#   token `compio_postgres` appears nowhere on it. The type arrives by field
#   access; it could equally arrive by inference from a call:
#
#       crud/unmask.rs:526  let rows = crate::exec::query_postgres_pool_with_
#                                        autocommit_role(...).await?;
#
#   `crud/unmask.rs` names `compio_postgres` ZERO times and holds
#   `Vec<compio_postgres::Row>` across three production functions.
#
#   - tier_signature_census.sh looks at SIGNATURES. Neither site is a signature.
#   - tier_direction_census.sh looks at `crate::` module direction. `crate::exec`
#     is the same tier as its engine callers today, so it reports nothing.
#
#   This census asks the third question: for every function whose RETURN TYPE
#   names a vendor, which tiers CALL it? Every non-vendor caller is a module
#   that holds a driver value the manifest fence would not stop.
#
# WHAT A HIT MEANS
#   Not automatically a defect. An engine calling DOWN into a vendor is legal by
#   the lattice. The point is that the coupling is INVISIBLE to the fence the
#   design relies on, so each hit is a placement decision that must be made
#   deliberately rather than discovered after the split does not compile.
#
# DEFECTS FOUND IN THIS SCRIPT WITHIN AN HOUR OF IT LANDING, BOTH FIXED.
# It reported 5. The true count is 4. Read every number here as a FLOOR.
#
#   1. THE CALLER SCAN DID NOT STRIP COMMENTS (found by a reviewer).
#      The source collector skipped comment lines; the caller matcher did not.
#      It reported
#          ADAPTER   lib.rs   ensure_pool
#      where lib.rs's four `ensure_pool` mentions are ALL comments, and :276
#      reads "The synchronous `ensure_pool(scope)` helper that used to live here
#      HAS BEEN REMOVED". The only real caller is replication_ops.rs:23, which is
#      correctly skipped as CONTESTED. The false row sat in the ADAPTER line, the
#      one a reader is least likely to question. Both halves now run through
#      prod(), which strips comments AND cfg-gated items.
#
#   2. `grep -q` UNDER `set -o pipefail` SILENTLY DROPPED THE LARGEST FILES
#      (found by me, by disbelieving an implausible count).
#      The test was `prod "$f" | grep -qE ...`. `grep -q` exits on the FIRST
#      match, which SIGPIPEs the upstream awk; with `pipefail` the pipeline
#      status becomes 141, so the `if` read FALSE on a file that had matched.
#      It only bites when grep matches before awk finishes - i.e. on big files -
#      so it kept transaction/cancel.rs (156 lines) and silently discarded
#      exec.rs (1769) and crud/unmask.rs (2260). The count read 1 instead of 4,
#      and the drop was invisible: no error, no warning, just absent rows.
#      A size-dependent silent filter is the worst shape a census can have.
#      FIX: `grep -c`, which reads to EOF and cannot SIGPIPE.
#      After the fix the count is 4, matching an independent hand-derivation.
#
# KNOWN LIMITS, stated so the count is read as a FLOOR
#   - Multi-line signatures ARE handled (the collector joins to the `{` or `;`),
#     which a one-line `fn .* -> .*vendor` grep is not: it misses
#     exec.rs:288-293 outright, whose `->` is five lines below its `fn`.
#   - A vendor value reached through a public FIELD (the SchemaError case that
#     prompted this) is NOT detected here - that needs the dependency's public
#     API, and every instrument in this tree stops at the crate boundary.
#   - Re-exported aliases (`use compio_postgres::Row as R; fn f() -> R`) are
#     missed; the bare-name resolution the signature census grew is not
#     replicated here.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# TWO SOURCE ROOTS SINCE 2026-09-03. The ENGINE tier - which is where nearly
# every vendor-value holder this census reports lives - left
# `zeroship-data-v8/src` for `zeroship-data-orm/src`. Pinned to the first
# root the census would have scanned the adapter and CDC files that stayed and
# printed a small clean number about the ~22k lines that went.
#
# Scanned as one region under one `tier_of_file` map, for the reason at the head
# of tier_direction_census.sh: what this asks - does a NON-VENDOR tier hold a
# vendor value - is a question about tiers, not about cargo packages.
SRC_ROOTS=(
  "$ROOT/crates/zeroship-data-v8/src"
  "$ROOT/crates/zeroship-data-orm/src"
)
for _root in "${SRC_ROOTS[@]}"; do
  [ -d "$_root" ] || { echo "no such tree: $_root" >&2; exit 1; }
done

# Every `.rs` under every root, as `<root>\t<./-relative path>`. The relative
# half keys `tier_of_file` and the gating set; the root half makes the path
# absolute for the readers.
all_sources() {
  local p
  for p in "${SRC_ROOTS[@]}"; do
    ( cd "$p" && find . -name '*.rs' | LC_ALL=C sort | sed "s|^|$p\t|" )
  done
}

# The `mod.rs` / `lib.rs` files that carry the cfg-gated `mod` declarations.
all_mod_decls() {
  local p
  for p in "${SRC_ROOTS[@]}"; do
    ( cd "$p" && find . \( -name 'mod.rs' -o -name 'lib.rs' \) | LC_ALL=C sort | sed "s|^|$p\t|" )
  done
}

label() { printf '%s/%s\n' "$(basename "$(dirname "$1")" | sed 's/^zeroship-//')" "$2"; }

# Production region of one file: no comments, no cfg-gated items. Same rule as
# tier_direction_census.sh, and it must be applied to the CALLER scan as well as
# the source collection.
#
# DEFECT, found 2026-09-01 by a reviewer within an hour of this file landing: the
# collector stripped comments but the caller matcher did not, so a mention inside
# a `//` comment counted as a call. It reported
#     ADAPTER   lib.rs   ensure_pool
# where lib.rs's four mentions are ALL comments and :276 reads "The synchronous
# `ensure_pool(scope)` helper that used to live here HAS BEEN REMOVED". The only
# real caller is replication_ops.rs:23, correctly skipped as CONTESTED.
# The reported count was 5; the true count is 4, and the false row sat in the
# ADAPTER line a reader is least likely to question. Exactly the "a grep counts
# text" failure this file's own header was written to escape.
prod() {
  awk '
    /^[[:space:]]*(\/\/\/|\/\/!|\/\/)/ { next }
    /^#\[cfg\(not\(/ { pend=0; print; next }
    !intest && /^#\[cfg\(/ && /(^|[^A-Za-z_])test([^A-Za-z_]|$)/ { pend=1; next }
    pend && /^#\[/ { next }
    pend && /^[[:space:]]*$/ { next }
    pend && /^[a-zA-Z]/ { pend=0; seek=1 }
    seek {
      if ($0 ~ /;[[:space:]]*$/) { seek=0; next }
      if ($0 ~ /\{/ && $0 ~ /\}/) { seek=0; next }
      if ($0 ~ /\{[[:space:]]*$/) { seek=0; intest=1; next }
      next
    }
    { pend=0 }
    intest { if ($0 == "}") intest=0; next }
    { print }' "$1"
}

tier_of_file() {
  case "$1" in
    ./v8_classes/*|./v8_bridge.rs|./lib.rs|./tx_scope.rs)  echo ADAPTER ;;
    ./crud/*|./transaction/*|./exec.rs|./backend_selection.rs|./tx_route.rs|./drop_namespace.rs) echo ENGINE ;;
    ./auth/bootstrap.rs)                                 echo ENGINE ;;
    # NO ARMS for ./broker.rs, ./read_set.rs, ./backend/postgres.rs,
    # ./backend/pg_*.rs, ./backend/sqlite/*, ./encryption/*, ./error.rs or
    # ./binding.rs. Every one was extracted into a dependency crate, and an arm
    # for a file this region does not hold is a pattern matching nothing - kept
    # in step with tier_direction_census.sh, where the two censuses judging one
    # file differently is defect 1.
    ./wal_consumer.rs|./replication.rs|./slot_reaper.rs) echo CDC ;;
    # `descriptor.rs` and `backend/mod.rs`: ENGINE, settled by the 2026-09-03
    # cut. See the same two arms in tier_direction_census.sh.
    ./descriptor.rs|./backend/mod.rs)                    echo ENGINE ;;
    ./tx_lanes.rs|./backend_handle.rs|./backend/cancel.rs|./system_shape_charter.rs|./metrics.rs) echo ENGINE ;;
    ./context.rs|./service.rs|./op_error.rs)             echo ADAPTER ;;
    ./cdc_lifecycle.rs|./change_stream_pg.rs)            echo CDC ;;
    *)                                                   echo CONTESTED ;;
  esac
}

SRCS="$(mktemp)"; HITS="$(mktemp)"; GATED="$(mktemp)"
trap 'rm -f "$SRCS" "$HITS" "$GATED"' EXIT

# CROSS-FILE GATING. A file is test-only when the `mod` DECLARATION that pulls it
# in is cfg-gated - and that declaration lives in a DIFFERENT file. Walking files
# independently misses this entirely: transaction/probe.rs looks like ordinary
# engine code, and `#[cfg(any(test, feature = "test-helpers"))] pub mod probe;`
# in transaction/mod.rs is what makes it test-only.
# Without this, the census reports it as a production vendor holder.
# The rule was written for crud/mask_drift.rs, which was deleted on 2026-09-03,
# and auth/util.rs followed on 2026-09-04; probe.rs is the live case it still
# catches.
# Same failure family as defect 4 of tier_direction_census.sh, one level up.
# A module declared under BOTH arms of the two-arm pattern
#   #[cfg(not(feature = "test-helpers"))] pub(crate) mod m;
#   #[cfg(feature     = "test-helpers")]  pub mod m;
# IS production - the second arm only widens visibility. Only a module that
# appears under a test cfg and NEVER under a production declaration is test-only.
# Collect both sets and subtract; marking on the test arm alone wrongly gated
# crud/unmask.rs and three others, taking the count from 7 to 1.
TESTMODS="$(mktemp)"; PRODMODS="$(mktemp)"
trap 'rm -f "$SRCS" "$HITS" "$GATED" "$TESTMODS" "$PRODMODS"' EXIT

while IFS=$'\t' read -r croot m; do
  # Keyed by ROOT and directory, not by directory alone: two crates each hold a
  # `./crud/mod.rs`-shaped tree, and a module gated in one must not gate a
  # same-named file in the other.
  dir="$croot/$(dirname "${m#./}")"
  dir="${dir%/.}"
  awk -v D="$dir" '
    function emit(tag,   name) {
      name=$0; sub(/.*mod /, "", name); sub(/;.*/, "", name)
      print tag "\t" D "/" name
    }
    /^#\[cfg\(not\(/ { pend=2; next }                                   # production arm
    /^#\[cfg\(/ && /(^|[^A-Za-z_])test([^A-Za-z_]|$)/ { pend=1; next }  # test arm
    /^#\[/ { next }
    /^[[:space:]]*$/ { next }
    /^(pub |pub\([a-z()]*\) )?mod [a-z_0-9]+;/ {
      if (pend == 1) emit("TEST"); else emit("PROD")
      pend=0; next
    }
    { pend=0 }
  ' "$croot/${m#./}"
done < <(all_mod_decls) > "$GATED".raw

grep -P '^TEST\t' "$GATED".raw | cut -f2 | sort -u > "$TESTMODS"
grep -P '^PROD\t' "$GATED".raw | cut -f2 | sort -u > "$PRODMODS"
# test-only = declared under a test cfg and never under a production one
comm -23 "$TESTMODS" "$PRODMODS" | while read -r base; do
  echo "${base}.rs"; echo "${base}/"
done > "$GATED"
rm -f "$GATED".raw

# `$1` is an ABSOLUTE path, and so is every entry in $GATED - the two roots make
# a bare relative key ambiguous.
is_gated_file() {
  local abs="$1"
  while read -r g; do
    [ -z "$g" ] && continue
    case "$g" in
      */) case "$abs" in "$g"*) return 0 ;; esac ;;
      *)  [ "$abs" = "$g" ] && return 0 ;;
    esac
  done < "$GATED"
  return 1
}

# Collect every fn whose RETURN TYPE names a vendor. Joins the signature across
# lines up to the opening `{` or the `;` of a trait method.
while IFS=$'\t' read -r croot rel; do
  f="$croot/${rel#./}"
  awk -v F="$f" '
    /^[[:space:]]*(\/\/\/|\/\/!|\/\/)/ { next }
    /(^|[^A-Za-z_])fn [a-z_]+/ && !collecting { collecting=1; sig=""; name=$0
      sub(/.*[^A-Za-z_]fn /, "", name); sub(/[^A-Za-z_0-9].*/, "", name) }
    collecting { sig = sig " " $0 }
    collecting && /(\{|;)[[:space:]]*$/ {
      if (sig ~ /->/ && sig ~ /(compio_postgres|rusqlite)/) {
        arrow = sig; sub(/.*->/, "", arrow)
        if (arrow ~ /(compio_postgres|rusqlite)/) print name
      }
      collecting=0
    }
  ' "$f"
done < <(all_sources) | sort -u > "$SRCS"

echo "Vendor-returning functions (the sources a value can flow FROM): $(wc -l < "$SRCS")"
sed 's/^/  /' "$SRCS"
echo
printf '%-9s %-46s %s\n' TIER FILE 'HOLDS A VENDOR VALUE FROM'
echo "--------------------------------------------------------------------------------------------"

while read -r fn; do
  [ -z "$fn" ] && continue
  while IFS=$'\t' read -r croot relpath; do
    f="$croot/${relpath#./}"
    t=$(tier_of_file "$relpath")
    # A vendor tier holding its own vendor's values is the point of that tier.
    case "$t" in PG|SQLITE|CONTESTED) continue ;; esac
    # A file whose `mod` declaration is gated is not production, however
    # ordinary its own contents look.
    is_gated_file "$f" && continue
    rel="$(label "$croot" "${relpath#./}")"
    # Skip the file that DEFINES it - a definition is a signature, which the
    # signature census already rules on.
    grep -qE "(^|[^A-Za-z_])fn ${fn}\b" "$f" && continue
    # Scan the PRODUCTION region, not the raw file: a mention in a comment or a
    # cfg-gated item is not a call. See the prod() header.
    #
    # `grep -c`, NOT `grep -q`. Under `set -o pipefail` (line 1), `grep -q`
    # exits on the first match, SIGPIPEs the upstream awk, and the pipeline's
    # status becomes 141 - so the `if` reads FALSE on a file that DID match.
    # It bites only where grep matches before awk finishes, i.e. large files, so
    # it silently dropped exec.rs (1769 lines) and crud/unmask.rs (2260) while
    # keeping transaction/cancel.rs (156). `grep -c` reads to EOF and cannot
    # SIGPIPE. See defect 2 in the header.
    n=$(prod "$f" | grep -cE "(^|[^A-Za-z_])${fn}\(")
    if [ "${n:-0}" -gt 0 ]; then
      printf '%-9s %-46s %s\n' "$t" "$rel" "$fn"
      echo "$t" >> "$HITS"
    fi
  done < <(all_sources)
done < "$SRCS"

echo "--------------------------------------------------------------------------------------------"
echo "Non-vendor modules holding vendor values: $(wc -l < "$HITS")"
echo
echo "Read as a FLOOR - see KNOWN LIMITS in this header. A hit is not a defect by"
echo "itself; it is coupling the manifest fence CANNOT catch, so each one has to be"
echo "placed deliberately before the split rather than discovered by a build break."
