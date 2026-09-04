#!/usr/bin/env bash
# ============================================================================
# REFUSE A #[cfg(feature = "F")] / #[cfg(not(feature = "F"))] PAIR WHOSE TWO
# ARMS DECLARE THE IDENTICAL ITEM WITH THE IDENTICAL VISIBILITY.
#
# THE DEFECT. `crates/zeroship-data-engine/src/crud/mod.rs` declared six
# modules twice, gated in both directions on `test-helpers`:
#
#     #[cfg(not(feature = "test-helpers"))]
#     pub mod encryption_pass;
#     #[cfg(feature = "test-helpers")]
#     pub mod encryption_pass;
#
# and the comment above it read "crate-private in release builds; `pub` under
# `test-helpers`". BOTH ARMS SAID `pub mod`. The pair was a no-op: the module
# was `pub` in every feature configuration the crate can be built with, and
# the comment described a protection that did not exist. Measured 2026-09-04:
# the same shape, one level down, at THREE FUNCTIONS inside
# `crud/system_fields_pass.rs` - `apply_system_fields_on_insert`,
# `_insert_many` and `_on_update` were each declared twice, both arms
# `pub fn` with a byte-identical one-line body. Nine no-op pairs total, not
# six - the module-level defect this gate was written for, and a second copy
# of it that a file-scoped search would have missed.
#
# WHAT THIS GATE DOES NOT DO. It does not decide whether an item SHOULD be
# narrower under `test-helpers` - that is a per-item measurement (does
# anything outside the defining crate name it in the DEFAULT feature
# configuration?), done by hand for each of the nine above and recorded in the
# commit that fixed them. This gate only makes the two arms of a cfg pair
# incapable of drifting back into agreement without someone noticing: if a
# future edit makes both arms read the same again, this goes red.
#
# THE SHAPE, exactly. Two adjacent attribute+item blocks:
#
#     #[cfg(not(feature = "F"))]        #[cfg(feature = "F"))]
#     <item A>                    ...    <item B>
#
# in either order, same F, with no other declaration between the first item
# and the second attribute. An ITEM is either one line ending in `;` with no
# `{` (a `mod`/`use`/`type`/`const`/`static` declaration - the six original
# instances), or a brace-delimited item (`fn`/`struct`/`enum`/`impl`/`trait`/
# `mod { }`) closed by counting braces forward from its first line, skipping
# string-literal contents - the three function instances. The pair is a NO-OP
# when the two items' text is identical after collapsing internal whitespace.
#
# WHAT THIS DOES NOT CATCH, stated so nobody reads it as complete: two items
# separated by a blank-line-only gap other than immediately after the
# attribute/before the next attribute (a real intervening declaration defeats
# the "immediately following" scan by design - that is not this shape); a pair
# whose bodies differ only in comments (comments are stripped by `norm`
# only inside whitespace runs, not removed, so a body differing purely in a
# `//` comment is correctly treated as a real difference, since the comment
# IS part of what ships in the source and might be the only intended change -
# a conservative choice, not an oversight); and doc comments/attributes
# BETWEEN the cfg line and the item (the scan takes the very next non-blank
# line as the item and would misread `#[allow(..)]\npub fn f() {}` as the
# item being `#[allow(..)]` - none of the nine measured instances have this
# shape, and `--self-test` below does not claim otherwise).
# ============================================================================
set -uo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=tests/lib/gate_arms.sh
. "$(dirname "$0")/lib/gate_arms.sh"
gate_arms_init noop_cfg_pair

PASS=0
FAIL=0
RAN=0
pass() { PASS=$((PASS + 1)); RAN=$((RAN + 1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL + 1)); RAN=$((RAN + 1)); echo "  FAIL $1"; }

AWKPROG="$(mktemp)"
trap 'rm -f "$AWKPROG"' EXIT

# The scan is one program shared by the real run and --self-test, so the
# mechanism under test in --self-test IS the mechanism the real arm below
# runs - not a paraphrase of it.
cat > "$AWKPROG" <<'AWK'
function trim(s) { gsub(/^[ \t]+|[ \t]+$/, "", s); return s }
function is_not(line) { return line ~ /^[ \t]*#\[cfg\(not\(feature[ \t]*=[ \t]*"[^"]*"\)\)\][ \t]*$/ }
function is_pos(line) { return line ~ /^[ \t]*#\[cfg\(feature[ \t]*=[ \t]*"[^"]*"\)\][ \t]*$/ }
function quoted(line) {
  if (match(line, /"[^"]*"/)) return substr(line, RSTART + 1, RLENGTH - 2)
  return ""
}

# 1-based end line of the item starting at line s in lines[]. A one-line item
# (ends ';' with no '{' on that line) ends at s. Otherwise scan forward
# counting braces - skipping string-literal contents so a brace inside a
# string cannot desync the count - until they balance.
function item_end(s,   t, depth, i, j, n, c, in_str, opened) {
  t = trim(lines[s])
  if (t ~ /;[ \t]*$/ && t !~ /\{/) return s
  depth = 0; opened = 0
  for (i = s; i <= maxline; i++) {
    n = length(lines[i]); in_str = 0
    for (j = 1; j <= n; j++) {
      c = substr(lines[i], j, 1)
      if (in_str) {
        if (c == "\\") { j++; continue }
        if (c == "\"") in_str = 0
        continue
      }
      if (c == "\"") { in_str = 1; continue }
      if (c == "{") { depth++; opened = 1 }
      else if (c == "}") depth--
    }
    if (opened && depth <= 0) return i
  }
  return 0
}

function norm(s) { gsub(/^[ \t]+|[ \t]+$/, "", s); gsub(/[ \t]+/, " ", s); return s }

function assemble(s, e,   out, i) {
  out = ""
  for (i = s; i <= e; i++) out = out norm(lines[i]) "\n"
  return out
}

# Emits one "NOOP\t<file>\t<line>\t<feature>" row per no-op pair found in the
# just-finished file, plus running TOTAL_PAIRS / NOOP_PAIRS counters.
function process(fname,   i, j, k, l, pol1, pol2, feat1, feat2, e1, e2, decl1, decl2) {
  for (i = 1; i <= maxline; i++) {
    pol1 = ""
    if (is_not(lines[i])) { pol1 = "not"; feat1 = quoted(lines[i]) }
    else if (is_pos(lines[i])) { pol1 = "pos"; feat1 = quoted(lines[i]) }
    if (pol1 == "") continue

    j = i + 1
    while (j <= maxline && trim(lines[j]) == "") j++
    if (j > maxline) continue
    e1 = item_end(j)
    if (e1 == 0) continue
    decl1 = assemble(j, e1)

    k = e1 + 1
    while (k <= maxline && trim(lines[k]) == "") k++
    if (k > maxline) continue
    pol2 = ""
    if (is_not(lines[k])) { pol2 = "not"; feat2 = quoted(lines[k]) }
    else if (is_pos(lines[k])) { pol2 = "pos"; feat2 = quoted(lines[k]) }
    if (pol2 == "" || pol2 == pol1 || feat2 != feat1) continue

    l = k + 1
    while (l <= maxline && trim(lines[l]) == "") l++
    if (l > maxline) continue
    e2 = item_end(l)
    if (e2 == 0) continue
    decl2 = assemble(l, e2)

    total++
    if (decl1 == decl2) {
      printf "NOOP\t%s\t%d\t%s\n", fname, i, feat1
      noop++
    }
  }
}

FNR == 1 {
  if (NR > 1) process(prevfile)
  delete lines
  maxline = 0
  prevfile = FILENAME
}
{ lines[FNR] = $0; maxline = FNR }
END {
  if (maxline > 0) process(prevfile)
  printf "TOTAL_PAIRS\t%d\n", total + 0
  printf "NOOP_PAIRS\t%d\n", noop + 0
}
AWK

# scan_dir <dir> - runs the shared program over every .rs file under <dir>,
# emitting the same NOOP / TOTAL_PAIRS / NOOP_PAIRS lines the program prints.
# NO `2>/dev/null` anywhere: a `find` that cannot read the tree and a clean
# tree must not print the same thing.
scan_dir() {
  find "$1" -name '*.rs' -type f -print0 | xargs -0 awk -f "$AWKPROG"
}

self_test() {
  echo "noop cfg pair gate self-test"
  local tmp status=0 out
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  # POSITIVE 1: the exact module-level shape from crud/mod.rs before the fix -
  # a one-line `pub mod x;` item, gated both ways, both arms identical.
  cat > "$tmp/a.rs" <<'RS'
#[cfg(not(feature = "test-helpers"))]
pub mod encryption_pass;
#[cfg(feature = "test-helpers")]
pub mod encryption_pass;
RS
  out="$(find "$tmp" -name 'a.rs' -print0 | xargs -0 awk -f "$AWKPROG")"
  if printf '%s\n' "$out" | grep -q '^NOOP	'"$tmp"'/a.rs	1	test-helpers$'; then
    echo "  ok   a one-line pub-mod pair, identical both arms, is caught"
  else
    echo "  FAIL the module-level positive fixture was not flagged: $out"
    status=1
  fi
  rm -f "$tmp/a.rs"

  # NEGATIVE CONTROL, differing in ONE variable: the same shape, same place,
  # but the non-test-helpers arm is narrowed to pub(crate). Without this the
  # positive above proves only that the scan found A pair, not that it
  # DISCRIMINATES on content - a detector that flagged every pair regardless
  # of text would pass the positive too.
  cat > "$tmp/b.rs" <<'RS'
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod encryption_pass;
#[cfg(feature = "test-helpers")]
pub mod encryption_pass;
RS
  out="$(find "$tmp" -name 'b.rs' -print0 | xargs -0 awk -f "$AWKPROG")"
  if printf '%s\n' "$out" | grep -q '^NOOP	'; then
    echo "  FAIL a genuinely narrowed pair (pub(crate) vs pub) was flagged as a no-op"
    status=1
  else
    echo "  ok   a genuinely narrowed pair (pub(crate) vs pub) is left alone"
  fi
  rm -f "$tmp/b.rs"

  # POSITIVE 2: the function-level shape from system_fields_pass.rs - a
  # brace-delimited, multi-line item, both arms byte-identical once
  # whitespace is collapsed. Exercises item_end's brace counter, not just the
  # one-line ';' path POSITIVE 1 exercises.
  cat > "$tmp/c.rs" <<'RS'
#[cfg(not(feature = "test-helpers"))]
pub fn apply_system_fields_on_insert(
    doc: &mut Value,
) -> Result<(), DbError> {
    apply_system_fields_on_insert_impl(doc)
}

#[cfg(feature = "test-helpers")]
pub fn apply_system_fields_on_insert(
    doc: &mut Value,
) -> Result<(), DbError> {
    apply_system_fields_on_insert_impl(doc)
}
RS
  out="$(find "$tmp" -name 'c.rs' -print0 | xargs -0 awk -f "$AWKPROG")"
  if printf '%s\n' "$out" | grep -q '^NOOP	'"$tmp"'/c.rs	1	test-helpers$'; then
    echo "  ok   a multi-line fn pair, identical both arms, is caught"
  else
    echo "  FAIL the function-level positive fixture was not flagged: $out"
    status=1
  fi
  rm -f "$tmp/c.rs"

  return "$status"
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi

echo "noop cfg pair gate"

# --- Arm 1: the pair-detection scan ran over a real body of candidates -----
#
# Floor well under the observed count (26, measured 2026-09-04 on the fixed
# tree; 32 on the tree as found, before the nine were fixed): ordinary
# editing moves this by ones on either side of a cfg-gated re-export or
# module split, while the failure this arm exists to catch - the regex stops
# matching a cfg attribute, or `crates/` stops resolving - collapses it to
# zero, not to fifteen.
PAIR_FLOOR=15

RESULT="$(scan_dir crates)"
TOTAL_PAIRS="$(printf '%s\n' "$RESULT" | awk -F'\t' '$1 == "TOTAL_PAIRS" {print $2}')"
NOOP_PAIRS="$(printf '%s\n' "$RESULT" | awk -F'\t' '$1 == "NOOP_PAIRS" {print $2}')"
NOOP_ROWS="$(printf '%s\n' "$RESULT" | awk -F'\t' '$1 == "NOOP" {printf "%s:%s (feature=%s)\n", $2, $3, $4}')"

if ! gate_arm pairs_examined "${TOTAL_PAIRS:-}" "$PAIR_FLOOR"; then
  fail "the scan over crates/ ruled on ${TOTAL_PAIRS:-0} cfg-pair candidate(s),
       under its floor of $PAIR_FLOOR. Whatever it reports about no-op pairs
       is meaningless: fix the scan, do not lower the floor."
elif [ "${NOOP_PAIRS:-0}" -eq 0 ]; then
  pass "all $TOTAL_PAIRS cfg-pair candidate(s) under crates/ have two arms that
       differ - no no-op pair found"
else
  fail "these #[cfg(not(feature = \"F\"))] / #[cfg(feature = \"F\")] pairs
       declare the IDENTICAL item with the IDENTICAL visibility in both arms -
       the cfg makes no difference to what ships:
$NOOP_ROWS
       Either narrow the non-test-helpers arm to the visibility its comment
       claims (pub(crate), or crate-private for a bare item), or delete the
       pair and keep one unconditional declaration whose comment says what is
       actually true. See crud/mod.rs and crud/system_fields_pass.rs for both
       repairs, made 2026-09-04."
fi

# --- Arm 2: the mechanism itself discriminates -----------------------------
#
# Runs the exact three fixtures --self-test drives (two positive, one
# negative control) inline, so a normal invocation of this gate - the one CI
# runs - is what proves the detector still tells a no-op pair from a real
# one, not a flag a human has to remember to pass.
SELF_TEST_OUT="$(self_test 2>&1)"
SELF_TEST_STATUS=$?
N_SELF_TEST_OK="$(printf '%s\n' "$SELF_TEST_OUT" | grep -c '^  ok   ')"
gate_arm self_test_scenarios "$N_SELF_TEST_OK" 2 || true
if [ "$SELF_TEST_STATUS" -eq 0 ]; then
  pass "the detector's self-test passed ($N_SELF_TEST_OK scenario(s): one-line
       no-op, brace-delimited no-op, and the pub(crate)-vs-pub negative
       control all resolved correctly)"
else
  fail "the detector's self-test failed - see below:
$SELF_TEST_OUT"
fi

# --- anti-hollow guard ------------------------------------------------------
if [ "$RAN" -lt 2 ]; then
  echo "GATE DID NOT RUN: expected 2 arms, ran $RAN"
  exit 1
fi

gate_arms_finish || FAIL=$((FAIL + 1))

echo "  noop cfg pair gate: $PASS passed, $FAIL failed ($RAN arms ran,"
echo "  $TOTAL_PAIRS cfg-pair candidate(s) examined under crates/, $NOOP_PAIRS no-op)"
[ "$FAIL" -eq 0 ] || exit 1
