#!/usr/bin/env bash
# ORM driver and capability contracts must retain their shape across features.
# Static checks cover trait members and their vocabulary. Consumers build
# through their ordinary library targets.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init contract_feature_invariance

# Floors guard the source scans and the derived consumer build set.
MIN_ITEMS=20
MIN_TYPES=3
MIN_BUILDS=1

CORE_SRC="$ROOT/crates/zeroship-data-orm/src"
contract_sources() {
  printf '%s\n' "$CORE_SRC/storage.rs" "$CORE_SRC/driver.rs" "$CORE_SRC/lock_policy.rs"
}
vocabulary_sources() {
  printf '%s\n' "$CORE_SRC/capability.rs" "$CORE_SRC/driver.rs" "$CORE_SRC/error.rs" "$CORE_SRC/binding.rs" "$CORE_SRC/encryption/keys.rs"
}

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
done < <(contract_sources) \
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
done < <(vocabulary_sources) \
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

echo "  - signature types the ORM declares and its traits name: $n_types"
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
# Production consumers must compile without a fixture feature.
n_builds=0
for pkg in zeroship-data-orm zeroship-data-v8; do
  n_builds=$((n_builds + 1))
  if ! (cd "$ROOT" && cargo check -p "$pkg" --lib) > "$TMP/$pkg.log" 2>&1; then
    cat "$TMP/$pkg.log" >&2
    fail=1
  fi
done
gate_arm ordinary_consumer_builds "$n_builds" "$MIN_BUILDS" || fail=1

gate_arms_finish || fail=1

if [ "$fail" -ne 0 ]; then
  echo "" >&2
  echo "CONTRACT FEATURE INVARIANCE GATE: FAILED" >&2
  exit 1
fi
echo "CONTRACT FEATURE INVARIANCE GATE: ok (${n_items:-0} contract item(s), $n_types signature type(s), $n_builds split-feature build(s))"
