#!/usr/bin/env bash
# Control the census's prod() filter BOTH ways: a gated item must vanish, an
# ungated one must survive. A filter that drops everything passes the first
# check alone - which is how the previous version's blindness went unnoticed,
# and how my first fix silently ate a live impl.
#
# Sources the LIVE function out of the census so this cannot drift from the
# code it claims to test. Resolve the census RELATIVE to this file - an absolute
# path would pin the control to one machine's worktree.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CENSUS="$HERE/tier_direction_census.sh"
[ -r "$CENSUS" ] || { echo "no census beside this control: $CENSUS" >&2; exit 2; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

sed -n '/^prod() {/,/^}$/p' "$CENSUS" > "$TMP/prod_live.sh"
# A silently empty extraction would make every check "pass" by printing nothing.
grep -q 'awk' "$TMP/prod_live.sh" || { echo "prod() not extracted from $CENSUS" >&2; exit 2; }
. "$TMP/prod_live.sh"

F="$TMP/cfgprobe.rs"
cat > "$F" <<'EOF'
#[cfg(any(test, feature = "test-helpers"))]
impl Gated {
    fn leaks() { crate::gated_impl_marker }
}
#[cfg(test)]
pub mod gated_mod {
    fn x() { crate::gated_mod_marker }
}
#[cfg(any(test, feature = "test-helpers"))]
pub mod gated_decl;
#[cfg(test)]
pub fn gated_multiline(
    a: A,
) -> B { crate::gated_multiline_marker }
#[cfg(test)]
const GATED: X = crate::gated_const_marker;
#[cfg(not(feature = "test-helpers"))]
pub(crate) fn production_arm() { crate::prod_arm_marker() }
#[cfg(not(test))]
pub(crate) fn production_arm2() {
    crate::prod_arm2_marker()
}
impl Live {
    fn real() { crate::broker::publish() }
}
pub fn also_live() { crate::descriptor::x() }
pub fn third_live(
    a: A,
) -> B {
    crate::exec::run()
}
EOF

echo "=== surviving crate:: targets ==="
prod "$F" | grep -oP 'crate::\K[A-Za-z_0-9]+' | sort -u | sed 's/^/  /'
echo
fail=0
echo "=== MUST be absent (gated) ==="
for m in gated_impl_marker gated_mod_marker gated_multiline_marker gated_const_marker; do
  if prod "$F" | grep -q "$m"; then echo "  LEAKED: $m"; fail=1; else echo "  ok gated: $m"; fi
done
echo
echo "=== MUST survive (live) - proves the filter is not just eating everything ==="
for m in broker descriptor exec prod_arm_marker prod_arm2_marker; do
  if prod "$F" | grep -q "crate::$m"; then echo "  ok kept: $m"; else echo "  WRONGLY DROPPED: $m"; fail=1; fi
done
echo
[ "$fail" = 0 ] && echo "CONTROL PASSED both directions" || echo "CONTROL FAILED"
exit "$fail"
