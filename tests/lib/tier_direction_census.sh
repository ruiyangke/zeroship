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
# USAGE
#   tests/lib/tier_direction_census.sh              # violations
#   tests/lib/tier_direction_census.sh --all        # every cross-tier edge
#   tests/lib/tier_direction_census.sh --contested  # what is not being judged
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SRC="$ROOT/crates/zeroship-plugin-db/src"
MODE="${1:-violations}"
cd "$SRC" || { echo "no such tree: $SRC" >&2; exit 1; }

tier() {
  case "$1" in
    v8_classes|v8_bridge|lib|tx_scope)                    echo ADAPTER ;;
    crud|transaction|exec|broker|read_set|tx_route|drop_namespace|cross_app_fk) echo ENGINE ;;
    postgres)                                             echo PG ;;
    sqlite)                                               echo SQLITE ;;
    encryption)                                           echo ENCRYPT ;;
    wal_consumer|replication|slot_reaper)                 echo CDC ;;
    error|descriptor|binding)                             echo CORE ;;
    *)                                                    echo CONTESTED ;;
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
    !intest && /^#\[cfg\(.*(^|[^A-Za-z_])test([^A-Za-z_]|$).*\)\]/ { pend=1; next }
    pend && /^(pub )?mod [A-Za-z_]+ \{/ { pend=0; intest=1; next }
    pend && /^[[:space:]]*$/ { next }
    { pend=0 }
    intest { if ($0 == "}") intest=0; next }
    { print }' "$1"
}

printf '%-9s %-34s %-9s %-22s %s\n' FROM FILE TO TARGET VERDICT
echo "-------------------------------------------------------------------------------------"
viol=0; rows=0; skipped=0
while read -r f; do
  # Source module name: first path segment under src/, or the file stem.
  rel="${f#./}"
  case "$rel" in */*) smod="${rel%%/*}" ;; *) smod="${rel%.rs}" ;; esac
  st=$(tier "$smod")
  if [ "$st" = CONTESTED ]; then skipped=$((skipped+1)); continue; fi
  sr=$(rank "$st")

  prod "$f" | grep -oP 'crate::\K[a-z_0-9]+' | sort -u | while read -r tmod; do
    [ "$tmod" = "$smod" ] && continue
    tt=$(tier "$tmod")
    [ "$tt" = CONTESTED ] && continue
    tr=$(rank "$tt")
    if [ "$tr" -lt "$sr" ]; then v=ok
    elif [ "$tr" -eq "$sr" ] && [ "$st" = "$tt" ]; then v=ok
    else v="** VIOLATION **"
    fi
    [ "$v" = ok ] && [ "$MODE" != "--all" ] && continue
    printf '%-9s %-34s %-9s %-22s %s\n' "$st" "$rel" "$tt" "crate::$tmod" "$v"
  done
done < <(find . -name '*.rs' | LC_ALL=C sort)

echo "-------------------------------------------------------------------------------------"
if [ "$MODE" = "--contested" ]; then
  echo "Modules with no settled tier (neither judged nor trusted):"
  find . -name '*.rs' | LC_ALL=C sort | while read -r f; do
    rel="${f#./}"; case "$rel" in */*) m="${rel%%/*}" ;; *) m="${rel%.rs}" ;; esac
    [ "$(tier "$m")" = CONTESTED ] && echo "  $rel"
  done | sort -u
fi
echo
echo "Every verdict is relative to tier() above, which is a copy of the proposal's"
echo "assignment table. Re-draw a boundary there and re-draw it here in the same"
echo "commit, or this reports on a shape nobody proposed."
