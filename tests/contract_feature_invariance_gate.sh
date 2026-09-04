#!/usr/bin/env bash
# ============================================================================
# A CAPABILITY CONTRACT MUST NOT CHANGE SHAPE WITH A FEATURE.
#
# `zeroship-data-core` declares the traits every storage backend implements.
# None of them, and none of their members, may carry `#[cfg(feature = ...)]`.
# `test-helpers` gates HELPERS AND FIXTURES - `DbBinding::cold_start`, the
# broker's per-thread test isolation, `schema_cache::reset_for_tests` - and
# never the shape of a capability.
#
# ---------------------------------------------------------------------------
# WHY A GATE AND NOT A CONVENTION
# ---------------------------------------------------------------------------
#
# This rule has been broken three times, and every time the whole verification
# suite was green while it was:
#
#   * `DialectBuilder::sql_dialect` - a REQUIRED member with no default. Turn
#     data-core's feature on without a vendor's and the vendor stops compiling:
#     `error[E0046]: not all trait items implemented, missing: sql_dialect`,
#     twice in zeroship-data-postgres and twice in zeroship-data-sqlite.
#   * `SqlExecutor::pool_exec_ddl` - a DEFAULTED member whose PG override
#     carried the same gate. `error[E0407]` in one direction; in the other it
#     COMPILES and the default body silently sends multi-statement DDL down
#     the extended protocol PostgreSQL rejects.
#   * `SchemaIntrospect` - a whole trait. On 2026-09-04 a new ungated
#     production caller reached it and zeroship-worker, the zeroship CLI and 5
#     other shipped targets stopped building, for a day.
#
# ONE MECHANISM BEHIND ALL THREE. `test-helpers` is a DEV-dependency feature of
# every crate above data-core, so `--all-targets`, `--all-features`, clippy and
# every `cargo test` unify it ON. Each of those reports a trait surface no
# shipped binary has, and each reported zero errors throughout.
#
# `tests/shipped_config_gate.sh` closes the release-build half of that. This
# gate closes the other half, which that one cannot see: A CONFIGURATION THE
# WORKSPACE DOES NOT CONTAIN. Nothing here enables `zeroship-data-core`'s
# feature without a vendor's, so no invocation over this workspace resolves
# features that way - and a hazard that only a configuration nobody builds can
# trip is not retired by deleting that configuration. Arm 3 builds it on
# purpose.
#
# ---------------------------------------------------------------------------
# THE THREE ARMS, and what each alone would miss
# ---------------------------------------------------------------------------
#
# 1. `contract_items` - the STATIC rule, over every `pub trait` data-core
#    declares and every member inside it. This is the only arm that catches a
#    re-gated `Backup`: gating a whole trait whose impls are gated in lockstep
#    breaks no build, so arm 3 stays green while the contract's shape once
#    again depends on a feature.
#
# 2. `signature_types` - the vocabulary. A member's signature naming a type
#    that is itself `cfg`-gated has exactly the same defect one level down:
#    `SnapshotOpts`, `SnapshotHandle` and `PitrTarget` were gated for as long
#    as `Backup` was, and a build that cannot name them cannot state the
#    contract.
#
# 3. `split_feature_builds` - the EMPIRICAL check, and the regression test for
#    the E0046 above. For every member forwarding `zeroship-data-core/
#    test-helpers`, build it with core's feature ON and its own OFF. Arms 1
#    and 2 read source; only this one asks rustc, and only this one would
#    catch a mismatch introduced on the IMPL side in a vendor crate.
#
# ---------------------------------------------------------------------------
# MUTATION PROOF, measured 2026-09-04, and the reason arms 1 and 3 are both here
# ---------------------------------------------------------------------------
#
# Restoring the gate on `DialectBuilder::sql_dialect` ALONE:
#
#   arm 1  RED  "1 CONTRACT ITEM(S) CARRY A cfg ATTRIBUTE: storage.rs
#                member sql_dialect of trait DialectBuilder"
#   arm 3  green - all four builds clean, because the vendor impls that
#                would have gone missing are ungated now.
#
# Restoring it on the two `impl DialectBuilder` blocks in
# zeroship-data-postgres as well, which is the shape the tree had at 26e996ef5:
#
#   arm 3  RED  E0046 in zeroship-data-{postgres,engine} and zeroship-plugin-db
#               (data-sqlite clean - its own impls were left ungated in this
#                mutation, which is itself the point: the arm reports per
#                dependent rather than as one verdict)
#
# Neither arm subsumes the other. Arm 1 sees a gate no build can trip; arm 3
# sees a mismatch introduced in a vendor crate that arm 1 does not read. The
# gate exits 1 in both mutations and 0 with them reverted.
#
# ---------------------------------------------------------------------------
# SECOND MUTATION PROOF, 2026-09-04, for the normaliser
# ---------------------------------------------------------------------------
#
# Arms 1 and 2 were rewritten that day and are NOT bound by the two mutations
# above, which both use the multi-line attribute spelling that already worked.
# The binding evidence is an 11-file corpus, each file checked with
# `rustc --edition 2021 --crate-type lib --emit=metadata` so that "valid Rust"
# is measured rather than assumed, run through the extractor lifted verbatim
# out of this file. Six shapes went from 0 violations to 1:
#
#   attribute + member on one line, attribute + trait on one line, a wrapped
#   attribute, `pub unsafe trait`, `unsafe fn`, and a one-line trait body.
#
# Two files are the controls. A cleanly ungated trait stays at 0 violations
# before and after; a trait declared inside a `/* */` block comment stopped
# being counted as an item (3 -> 2), which is the false POSITIVE the old
# scanner carried in the other direction.
#
# On the real tree the two predicates agree exactly - 9 traits, 37 items, 6
# signature types before and after - which is the point of diffing counts
# against the same input rather than trusting that a rewrite preserved
# behaviour.
#
# NOT MEASURED HERE: this gate was not driven red by editing
# crates/zeroship-data-core, which another agent owned at the time. The
# predicate under test is byte-identical to the shipped one; the wiring from
# it to the exit code is unchanged and was not re-proved.
#
# ---------------------------------------------------------------------------
# WHAT THIS DOES NOT RULE ON
# ---------------------------------------------------------------------------
#
# Arms 1 and 2 read a NORMALISED record stream, not raw lines - see
# `normalize_rs` below for what that buys and what it cost to learn. It handles
# `//` and `/* */` (this paragraph said block comments were NOT handled until
# 2026-09-04, and a trait smuggled inside one was measured missed), string
# literals, wrapped attributes and one-line items. It does NOT handle raw
# strings or character literals, neither of which data-core contains; an
# unterminated block comment, attribute or string is reported as a violation
# rather than silently truncating the record set.
#
# Both arms still key on a DECLARATION's spelling: `pub trait`, `fn`, `type`,
# `pub struct`. A trait produced by a macro is invisible to them, and there is
# no structural alternative short of parsing Rust. Arm 3 is what rules on the
# macro case, by asking rustc.
#
# Visibility: the trait test accepts `pub`, `pub(...)` and `unsafe`, but not a
# private `trait`. A private trait is not a contract, and admitting one would
# make every `#[cfg(feature = ...)]` helper trait inside a `#[cfg(test)] mod
# tests` a violation.
#
# `cfg` attributes OUTSIDE a trait block are none of this gate's business -
# `lock_policy.rs`'s `#[cfg(test)] mod tests` is correct and must stay.
#
# Other crates: by scope. The rule is about the crate that DEFINES the
# contracts. A vendor's own extension traits (`PgSqlExecutor`) are gated on
# purpose, and shipped_config_gate.sh is what refuses a production caller of
# one.
#
# Run:  tests/contract_feature_invariance_gate.sh
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init contract_feature_invariance

# ---------------------------------------------------------------------------
# THE FLOORS. Measured 2026-09-04 by running this gate, whose arms print the
# sets they derive:
#
#   contract_items       37   (9 pub traits in crates/zeroship-data-core/src -
#                              8 in storage.rs plus lock_policy.rs's
#                              BoundedLockAcquire - and their 28 members: 26
#                              across storage.rs's eight, 2 on
#                              BoundedLockAcquire. 9 + 28 = 37, and the
#                              arithmetic is here because a bare 37 cannot be
#                              checked against the file by eye.)
#   signature_types       6   (DbBinding, DbError, LockScope, PitrTarget,
#                              SnapshotHandle, SnapshotOpts)
#   split_feature_builds  4   (zeroship-data-{postgres,sqlite,engine},
#                              zeroship-plugin-db)
#
# Set well under today's numbers: far enough that ordinary editing does not
# reach them, close enough that a collapse does.
# ---------------------------------------------------------------------------
MIN_ITEMS=20
MIN_TYPES=3
MIN_BUILDS=3

CORE_SRC="$ROOT/crates/zeroship-data-core/src"

fail=0
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

command -v cargo >/dev/null 2>&1 || {
  echo "  x REFUSED: cargo is not on PATH. Arm 3 derives its set from cargo's" >&2
  echo "             own metadata and asks rustc for the verdict; without it" >&2
  echo "             this gate would rule on two thirds of its question and" >&2
  echo "             print the same thing a clean tree prints." >&2
  exit 1
}
command -v jq >/dev/null 2>&1 || {
  echo "  x REFUSED: jq is not on PATH; arm 3 cannot read cargo's metadata." >&2
  exit 1
}
[ -d "$CORE_SRC" ] || {
  echo "  x REFUSED: $CORE_SRC does not exist. The contract tier moved; point" >&2
  echo "             this gate at its new home rather than letting it scan an" >&2
  echo "             empty directory and pass." >&2
  exit 1
}

# ---------------------------------------------------------------------------
# THE NORMALISER, and why arms 1 and 2 no longer read raw lines.
#
# Both static arms are line-oriented, and until 2026-09-04 a LINE was the unit
# they matched on. That is a prose key wearing a syntax costume, and six shapes
# of genuinely cfg-gated contract item walked past it. Each was checked with
# `rustc --edition 2021 --crate-type lib --emit=metadata` first, so each is real
# Rust and not a strawman:
#
#   #[cfg(feature = "x")] fn sql_dialect(&self);   attribute and item on ONE
#       line. The old `line ~ /^#\[/` arm consumed the line and `next`ed, so
#       the member was never even counted, let alone flagged. THE CHEAPEST.
#   #[cfg(feature = "x")] pub trait SchemaIntrospect {   the same, on a trait.
#       Measured: 0 items, 0 violations - the whole trait vanished, which is
#       exactly the outage shape in the header above.
#   #[cfg(                    an attribute wrapped over three lines. The
#       feature = "x"         intervening lines reset `pending_cfg`, so the
#   )]                        member was counted and reported CLEAN.
#   pub unsafe trait ...      the trait regex demanded `pub` then `trait`.
#   unsafe fn ...             the member regex allowed only `pub`/`async`.
#   pub trait T { #[cfg..] fn f(); }   a one-line trait: `in_trait` was set
#       after the member test and cleared by the same line's closing brace.
#
# So the fix is to stop matching lines and start matching ITEMS. `normalize_rs`
# turns a Rust file into one record per logical unit, `<origline><TAB><text>`:
#
#   * block comments are consumed (nested `/*` included) - the old scanner did
#     not know them at all, and the header used to admit that a trait smuggled
#     inside one would be missed. `//` is handled BEFORE `/*` in the same
#     left-to-right pass, which matters: error.rs line 23 is a `//!` doc line
#     containing `auth/*`, and a scanner that looked for `/*` first would treat
#     the rest of that file as a comment.
#   * `//` line comments are dropped, including trailing ones, which the
#     old whole-line test could not do.
#   * string literals are respected, so a `//` or a `#[` inside one is text.
#   * every `#[...]` / `#![...]` becomes its OWN record, however it was spelled:
#     joined across lines when wrapped, split off from whatever followed it.
#   * `{`, `}` and `;` end a record, so a one-line trait becomes the same
#     record sequence a multi-line one does. Braces are preserved in the text,
#     so the depth counting downstream is unchanged.
#
# It is a scanner, not a Rust parser. It does not know raw strings (`r#"..."#`)
# or character literals, neither of which appears in data-core today; the END
# rule refuses on an unterminated block comment or attribute rather than
# emitting a truncated record set.
# ---------------------------------------------------------------------------
normalize_rs() {
  awk '
    function flush(   t) {
      t = obuf
      gsub(/^[ \t]+|[ \t]+$/, "", t)
      if (t != "") print oline "\t" t
      obuf = ""; oline = 0
    }
    function put(c) { if (obuf == "") oline = NR; obuf = obuf c }
    BEGIN { bc = 0; instr = 0; inattr = 0; adepth = 0; obuf = ""; oline = 0 }
    {
      line = $0
      sub(/\r$/, "", line)
      n = length(line)
      for (i = 1; i <= n; i++) {
        c = substr(line, i, 1)
        d = substr(line, i + 1, 1)
        if (bc > 0) {
          if (c == "*" && d == "/") { bc--; i++ }
          else if (c == "/" && d == "*") { bc++; i++ }
          continue
        }
        if (instr) {
          if (inattr) abuf = abuf c; else put(c)
          if (c == "\\") { i++; if (inattr) abuf = abuf substr(line, i, 1); else put(substr(line, i, 1)); continue }
          if (c == "\"") instr = 0
          continue
        }
        if (inattr) {
          abuf = abuf c
          if (c == "\"") { instr = 1; continue }
          if (c == "[") adepth++
          else if (c == "]") {
            adepth--
            if (adepth == 0) { print aline "\t" abuf; inattr = 0; abuf = "" }
          }
          continue
        }
        if (c == "/" && d == "/") break
        if (c == "/" && d == "*") { bc++; i++; continue }
        if (c == "\"") { instr = 1; put(c); continue }
        if (c == "#" && (d == "[" || (d == "!" && substr(line, i + 2, 1) == "["))) {
          flush()
          inattr = 1; aline = NR; abuf = c; adepth = 0
          continue
        }
        put(c)
        if (c == "{" || c == "}" || c == ";") flush()
      }
      if (inattr) abuf = abuf " "
      else if (!instr) flush()
    }
    END {
      flush()
      if (bc > 0)   print "0\tZSNORM-UNTERMINATED-BLOCK-COMMENT"
      if (inattr)   print "0\tZSNORM-UNTERMINATED-ATTRIBUTE"
      if (instr)    print "0\tZSNORM-UNTERMINATED-STRING"
    }
  ' "$1"
}

# ---------------------------------------------------------------------------
# ARMS 1 + 2 - the static rule.
#
# One awk pass per normalised file. It tracks whether it is inside a trait
# block by brace depth and reports:
#
#   ITEM   <file>:<line>  <trait|member> <name>     (something ruled on)
#   VIOL   <file>:<line>  <what>                    (a cfg attribute on one)
#   NAME   <ident>                                  (an identifier in a
#                                                    signature, for arm 2)
# ---------------------------------------------------------------------------
: > "$TMP/items.txt"
: > "$TMP/viol.txt"
: > "$TMP/names.txt"

while IFS= read -r f; do
  normalize_rs "$f" | awk -v file="${f#"$ROOT"/}" '
    # Records arrive as <origline><TAB><text>, comment-free, one logical unit
    # each. VIS is the visibility prefix an item may carry; it is written once
    # here so the trait and member tests cannot drift apart.
    BEGIN {
      VIS  = "(pub([[:space:]]*\\([^)]*\\))?[[:space:]]+)?"
      TRT  = "^" VIS "(unsafe[[:space:]]+)?trait[[:space:]]"
      MEM  = "^" VIS "(default[[:space:]]+)?(const[[:space:]]+)?(async[[:space:]]+)?(unsafe[[:space:]]+)?(extern[[:space:]]+\"[^\"]*\"[[:space:]]+)?fn[[:space:]]"
      TYP  = "^" VIS "type[[:space:]]"
    }
    {
      p = index($0, "\t")
      ln = substr($0, 1, p - 1)
      raw = substr($0, p + 1)
      line = raw
    }
    # The normaliser refuses rather than truncating; carry that through as a
    # violation so it cannot be mistaken for a clean file.
    line ~ /^ZSNORM-/ {
      printf "VIOL\t%s\t%s (the scanner could not read this file)\n", file, line
      next
    }
    # An attribute is now always its own record, wherever it was written.
    line ~ /^#!?\[/ {
      if (line ~ /cfg[[:space:]]*\(/) pending_cfg = pending_cfg "\n" ln "\t" line
      next
    }
    {
      if (in_trait) {
        # A member: `fn` in any of its prefixed spellings, or an associated
        # `type`.
        if (line ~ MEM || line ~ TYP) {
          name = line
          sub(VIS "(default[[:space:]]+)?(const[[:space:]]+)?(async[[:space:]]+)?(unsafe[[:space:]]+)?(extern[[:space:]]+\"[^\"]*\"[[:space:]]+)?", "", name)
          sub(/^(fn|type)[[:space:]]+/, "", name)
          sub(/[^A-Za-z0-9_].*$/, "", name)
          printf "ITEM\t%s:%d\tmember\t%s\n", file, ln, name
          if (pending_cfg != "")
            printf "VIOL\t%s:%d\tmember %s of trait %s\n", file, ln, name, trait_name
        }
        # Every capitalised identifier inside the block is a candidate type
        # for arm 2. Over-collecting is safe: arm 2 intersects with the set
        # data-core actually declares.
        s = raw
        while (match(s, /[A-Z][A-Za-z0-9_]*/)) {
          printf "NAME\t%s\n", substr(s, RSTART, RLENGTH)
          s = substr(s, RSTART + RLENGTH)
        }
      }
      # A trait opens here.
      if (line ~ TRT) {
        trait_name = line
        sub(TRT, "", trait_name)
        sub(/^[[:space:]]+/, "", trait_name)
        sub(/[^A-Za-z0-9_].*$/, "", trait_name)
        printf "ITEM\t%s:%d\ttrait\t%s\n", file, ln, trait_name
        if (pending_cfg != "")
          printf "VIOL\t%s:%d\ttrait %s\n", file, ln, trait_name
        in_trait = 1
        depth = 0
        # A trait header records ends at its `{`, so this record opens the
        # block; count it below and let the closing `}` record shut it.
      }
      if (in_trait) {
        n_open = gsub(/{/, "{", raw)
        n_close = gsub(/}/, "}", raw)
        depth += n_open - n_close
        if (depth <= 0 && (n_open + n_close) > 0) { in_trait = 0; trait_name = "" }
      }
      pending_cfg = ""
    }
  '
done < <(LC_ALL=C find "$CORE_SRC" -name '*.rs' -type f | LC_ALL=C sort) \
  > "$TMP/awk.txt"

grep -E '^ITEM' "$TMP/awk.txt" > "$TMP/items.txt" || true
grep -E '^VIOL' "$TMP/awk.txt" > "$TMP/viol.txt" || true
grep -E '^NAME' "$TMP/awk.txt" | cut -f2 | LC_ALL=C sort -u > "$TMP/names.txt" || true

n_items="$(grep -c . "$TMP/items.txt" || true)"
n_traits="$(grep -cP '\ttrait\t' "$TMP/items.txt" 2>/dev/null || grep -c '	trait	' "$TMP/items.txt" || true)"
echo "  - contract traits in ${CORE_SRC#"$ROOT"/}: ${n_traits:-0}; trait items: ${n_items:-0}"
gate_arm contract_items "${n_items:-0}" "$MIN_ITEMS" || fail=1

n_viol="$(grep -c . "$TMP/viol.txt" || true)"
if [ "${n_viol:-0}" -gt 0 ]; then
  echo "" >&2
  echo "  x ${n_viol} CONTRACT ITEM(S) CARRY A cfg ATTRIBUTE:" >&2
  sed 's/^VIOL\t/      /' "$TMP/viol.txt" >&2
  echo "    A trait or trait member whose presence depends on a feature is" >&2
  echo "    not a contract. Move the gate to the HELPER that needs it, or" >&2
  echo "    delete the item; do not gate the shape." >&2
  fail=1
fi

# ---------------------------------------------------------------------------
# ARM 2 - the vocabulary those signatures name.
# ---------------------------------------------------------------------------
: > "$TMP/types.txt"
: > "$TMP/type_viol.txt"
while IFS= read -r f; do
  normalize_rs "$f" | awk -v file="${f#"$ROOT"/}" '
    # Same normalised records as arm 1, and the same reason: `#[cfg(feature =
    # "x")] pub struct SnapshotOpts;` on one line used to be consumed as an
    # attribute and never reach the declaration test.
    BEGIN { VIS = "(pub([[:space:]]*\\([^)]*\\))?[[:space:]]+)?" }
    {
      p = index($0, "\t")
      ln = substr($0, 1, p - 1)
      line = substr($0, p + 1)
    }
    line ~ /^ZSNORM-/ { next }
    line ~ /^#!?\[/ {
      if (line ~ /cfg[[:space:]]*\([[:space:]]*feature/) pending_cfg = 1
      next
    }
    line ~ "^" VIS "(struct|enum|union)[[:space:]]" {
      name = line
      sub(VIS "(struct|enum|union)[[:space:]]+", "", name)
      sub(/[^A-Za-z0-9_].*$/, "", name)
      printf "%s\t%s:%d\t%d\n", name, file, ln, pending_cfg
    }
    { pending_cfg = 0 }
  '
done < <(LC_ALL=C find "$CORE_SRC" -name '*.rs' -type f | LC_ALL=C sort) \
  > "$TMP/decls.tsv"

n_types=0
while IFS=$'\t' read -r name loc gated; do
  [ -n "$name" ] || continue
  LC_ALL=C grep -qxF "$name" "$TMP/names.txt" || continue
  n_types=$((n_types + 1))
  if [ "$gated" = "1" ]; then
    echo "$name  $loc" >> "$TMP/type_viol.txt"
  fi
done < "$TMP/decls.tsv"

echo "  - signature types data-core declares and its traits name: $n_types"
gate_arm signature_types "$n_types" "$MIN_TYPES" || fail=1

n_type_viol="$(grep -c . "$TMP/type_viol.txt" 2>/dev/null || true)"
if [ "${n_type_viol:-0}" -gt 0 ]; then
  echo "" >&2
  echo "  x ${n_type_viol} TYPE(S) NAMED IN A CONTRACT SIGNATURE ARE FEATURE-GATED:" >&2
  sed 's/^/      /' "$TMP/type_viol.txt" >&2
  echo "    A build that cannot name the type cannot state the contract." >&2
  fail=1
fi

# ---------------------------------------------------------------------------
# ARM 3 - the configuration this workspace does not contain.
#
# For each member forwarding `zeroship-data-core/test-helpers`, turn CORE's
# feature on and leave the member's own off. That is exactly the resolution a
# single dependent's manifest produces, and it is the one that reported E0046
# against both vendors before 2026-09-04.
# ---------------------------------------------------------------------------
if ! (cd "$ROOT" && cargo metadata --format-version 1 --no-deps) \
    > "$TMP/meta.json" 2> "$TMP/meta.err"; then
  echo "  x REFUSED: cargo metadata failed; arm 3 has no set to derive." >&2
  cat "$TMP/meta.err" >&2
  exit 1
fi

jq -r '
  .packages[]
  | select((.features["test-helpers"] // [])
           | any(. == "zeroship-data-core/test-helpers"))
  | .name
' "$TMP/meta.json" | LC_ALL=C sort -u > "$TMP/forwarders.txt"

n_forwarders="$(grep -c . "$TMP/forwarders.txt" || true)"
if [ "${n_forwarders:-0}" -lt 1 ]; then
  echo "  x REFUSED: no workspace member forwards zeroship-data-core/test-helpers." >&2
  echo "             The derivation matched nothing, so it proves nothing." >&2
  exit 1
fi

n_builds=0
while IFS= read -r pkg; do
  [ -n "$pkg" ] || continue
  n_builds=$((n_builds + 1))
  if ! (cd "$ROOT" && cargo check -p "$pkg" \
          --features zeroship-data-core/test-helpers \
          --message-format=json) > "$TMP/$pkg.json" 2> "$TMP/$pkg.err"; then
    :
  fi
  jq -r 'select(.reason == "compiler-message")
         | select(.message.level == "error")
         | .message.rendered // empty' "$TMP/$pkg.json" > "$TMP/$pkg.errors" 2>/dev/null
  n_err="$(grep -c '^error' "$TMP/$pkg.errors" || true)"
  if [ "${n_err:-0}" -gt 0 ]; then
    echo "" >&2
    echo "  x $pkg DOES NOT COMPILE with zeroship-data-core/test-helpers ON" >&2
    echo "    and its own test-helpers OFF - ${n_err} error(s):" >&2
    cat "$TMP/$pkg.errors" >&2
    fail=1
  else
    echo "  - ok  $pkg (core feature on, own feature off)"
  fi
done < "$TMP/forwarders.txt"

gate_arm split_feature_builds "$n_builds" "$MIN_BUILDS" || fail=1

gate_arms_finish || fail=1

if [ "$fail" -ne 0 ]; then
  echo "" >&2
  echo "CONTRACT FEATURE INVARIANCE GATE: FAILED" >&2
  exit 1
fi
echo "CONTRACT FEATURE INVARIANCE GATE: ok (${n_items:-0} contract item(s), $n_types signature type(s), $n_builds split-feature build(s))"
