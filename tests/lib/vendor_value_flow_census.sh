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
SRC="$ROOT/crates/zeroship-plugin-db/src"
cd "$SRC" || { echo "no such tree: $SRC" >&2; exit 1; }

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
    ./crud/*|./transaction/*|./exec.rs|./broker.rs|./read_set.rs|./tx_route.rs|./drop_namespace.rs|./cross_app_fk.rs) echo ENGINE ;;
    ./auth/bootstrap.rs)                                 echo ENGINE ;;
    ./backend/postgres.rs|./backend/pg_row_json.rs|./backend/pg_error.rs|./backend/pg_introspect.rs) echo PG ;;
    ./backend/sqlite/*)                                  echo SQLITE ;;
    ./encryption/*)                                      echo ENCRYPT ;;
    ./wal_consumer.rs|./replication.rs|./slot_reaper.rs) echo CDC ;;
    ./error.rs|./descriptor.rs|./binding.rs)             echo CORE ;;
    *)                                                   echo CONTESTED ;;
  esac
}

SRCS="$(mktemp)"; HITS="$(mktemp)"; GATED="$(mktemp)"
trap 'rm -f "$SRCS" "$HITS" "$GATED"' EXIT

# CROSS-FILE GATING. A file is test-only when the `mod` DECLARATION that pulls it
# in is cfg-gated - and that declaration lives in a DIFFERENT file. Walking files
# independently misses this entirely: crud/mask_drift.rs looks like ordinary
# engine code, and `#[cfg(any(test, feature = "test-helpers"))] pub mod
# mask_drift;` at crud/mod.rs:90-91 is what makes it test-only.
# Without this, the census reports mask_drift as a production vendor holder.
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

while read -r m; do
  dir="$(dirname "$m")"
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
  ' "$m"
done < <(find . -name 'mod.rs' -o -name 'lib.rs') > "$GATED".raw

grep -P '^TEST\t' "$GATED".raw | cut -f2 | sort -u > "$TESTMODS"
grep -P '^PROD\t' "$GATED".raw | cut -f2 | sort -u > "$PRODMODS"
# test-only = declared under a test cfg and never under a production one
comm -23 "$TESTMODS" "$PRODMODS" | sed 's|^\./||' | while read -r base; do
  echo "${base}.rs"; echo "${base}/"
done > "$GATED"
rm -f "$GATED".raw

is_gated_file() {
  local rel="${1#./}"
  while read -r g; do
    [ -z "$g" ] && continue
    case "$g" in
      */) case "$rel" in "$g"*) return 0 ;; esac ;;
      *)  [ "$rel" = "$g" ] && return 0 ;;
    esac
  done < "$GATED"
  return 1
}

# Collect every fn whose RETURN TYPE names a vendor. Joins the signature across
# lines up to the opening `{` or the `;` of a trait method.
while read -r f; do
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
done < <(find . -name '*.rs') | sort -u > "$SRCS"

echo "Vendor-returning functions (the sources a value can flow FROM): $(wc -l < "$SRCS")"
sed 's/^/  /' "$SRCS"
echo
printf '%-9s %-34s %s\n' TIER FILE 'HOLDS A VENDOR VALUE FROM'
echo "--------------------------------------------------------------------------------"

while read -r fn; do
  [ -z "$fn" ] && continue
  while read -r f; do
    t=$(tier_of_file "$f")
    # A vendor tier holding its own vendor's values is the point of that tier.
    case "$t" in PG|SQLITE|CONTESTED) continue ;; esac
    # A file whose `mod` declaration is gated is not production, however
    # ordinary its own contents look.
    is_gated_file "$f" && continue
    rel="${f#./}"
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
      printf '%-9s %-34s %s\n' "$t" "$rel" "$fn"
      echo "$t" >> "$HITS"
    fi
  done < <(find . -name '*.rs')
done < "$SRCS"

echo "--------------------------------------------------------------------------------"
echo "Non-vendor modules holding vendor values: $(wc -l < "$HITS")"
echo
echo "Read as a FLOOR - see KNOWN LIMITS in this header. A hit is not a defect by"
echo "itself; it is coupling the manifest fence CANNOT catch, so each one has to be"
echo "placed deliberately before the split rather than discovered by a build break."
