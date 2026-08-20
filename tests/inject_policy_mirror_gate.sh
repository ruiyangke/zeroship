#!/usr/bin/env bash
# ============================================================================
# inject_policy_mirror_gate.sh — the [[inject]] rule is written twice; prove
# the two copies still agree.
#
#   crates/migrated/policies/confined.policy.toml      the server ceiling,
#                                                      include_str!'d into the
#                                                      zeroship-migrated binary
#   sdks/vite-plugin/src/gen-types/confined-ceiling.ts the build-time ceiling,
#                                                      a TOML string the plugin
#                                                      emits
#
# They declare the same thing: the seven platform system columns injected into
# every creator table, the pinned ["id"] primary key, and the three system
# indexes. Nothing enforces that they match. The TS copy says it "Mirrors the
# [[inject]] rule of crates/migrated/policies/confined.policy.toml", and that
# comment was the entire mechanism.
#
# Why this exists: the columns carried no `default`, so created_at/updated_at/
# version were emitted NOT NULL with nothing to populate them, and the first
# insert into any migration-created table failed. Fixing it meant editing both
# files. If only one had been edited, the build-time ceiling and the server
# ceiling would disagree about the shape of every creator table, and no test
# would have said so.
#
# What this does NOT check: that either copy is CORRECT. It compares them to
# each other. A wrong rule copied faithfully into both passes here — the
# end-to-end proof that the shape actually works is
# tests/e2e_db_app_end_to_end.sh stage 4.
#
# Usage:  ./tests/inject_policy_mirror_gate.sh
# Exit:   0 identical, 1 drifted, 2 could not read a file / found no rule
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init inject_policy_mirror

TOML="$ROOT/crates/migrated/policies/confined.policy.toml"
TS="$ROOT/sdks/vite-plugin/src/gen-types/confined-ceiling.ts"

for f in "$TOML" "$TS"; do
    [ -f "$f" ] || { echo "  ✗ missing $f"; exit 2; }
done

# Extract the semantic lines of the [[inject]] rule: the scalar settings plus
# every `{ name = ... }` entry (columns AND indexes both use that shape).
# Comments and blank lines are dropped, leading indentation is stripped and
# internal runs of spaces are collapsed, so the two copies may be formatted
# independently without tripping the gate.
extract_rule() {
    sed -n '/^\[\[inject\]\]/,/^\[\[/p' "$1" \
        | grep -E '^\s*(scope|mandatory|primary_key|author_primary_key)\s*=|^\s*\{ *name *=' \
        | sed -e 's/#.*$//' -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' -e 's/[[:space:]]\{1,\}/ /g'
}

A="$(extract_rule "$TOML")"
B="$(extract_rule "$TS")"

# A rule that extracts to nothing would make any two files "agree". Refuse.
A_LINES=$(printf '%s\n' "$A" | grep -c . || true)
B_LINES=$(printf '%s\n' "$B" | grep -c . || true)
# MEASURED 2026-08-20: 14 semantic lines extracted from each ceiling. Floor 5
# is reused unchanged from the pre-library hand-rolled check this replaces -
# below it there is too little left to distinguish "the rule shrank" from
# "the sed stopped matching the [[inject]] block".
toml_ok=1
ts_ok=1
gate_arm toml_lines "$A_LINES" 5 || toml_ok=0
gate_arm ts_lines "$B_LINES" 5 || ts_ok=0
if [ "$toml_ok" -ne 1 ] || [ "$ts_ok" -ne 1 ]; then
    echo "  ✗ extracted too little to compare (toml=$A_LINES ts=$B_LINES lines)."
    echo "    The [[inject]] rule moved or changed shape; fix this gate before trusting it."
    gate_arms_finish || true
    exit 2
fi

if [ "$A" = "$B" ]; then
    echo "  ✓ [[inject]] rule identical in both ceilings ($A_LINES lines compared)"
    gate_arms_finish || exit 1
    exit 0
fi

echo "  ✗ [[inject]] rule DRIFTED between the two ceilings"
echo "    server ceiling: $TOML"
echo "    build ceiling:  $TS"
echo "    < server-only, > build-only:"
diff <(printf '%s\n' "$A") <(printf '%s\n' "$B") | sed 's/^/      /'
gate_arms_finish || true
exit 1
