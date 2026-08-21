#!/usr/bin/env bash
# ============================================================================
# inject_policy_mirror_gate.sh - the confined [[inject]] rule is written SIX
# times; prove every copy still agrees, and prove there are still six.
#
# THE RULE. Seven platform system columns (id / created_at / updated_at /
# created_by / updated_by / version / deleted_at), a pinned ["id"] primary key,
# author_primary_key = "forbid", and three system indexes, injected into every
# table a creator migration creates. It is the shape of every creator table on
# the platform, so a copy that drifts is a creator app whose table does not
# match what the rest of the toolchain believes it built.
#
# THE SIX COPIES, and why each exists (a copy nobody can justify should be
# deleted, not gated - see "the case against this gate" at the bottom):
#
#   crates/migrated/policies/confined.policy.toml
#       the DEPLOYED server ceiling. include_str!'d into zeroship-migrated;
#       this is the shape a creator's tables get in production.
#   crates/plugin-db/policies/confined.policy.toml
#       the DEV SQLite registerModel ceiling. include_str!'d into
#       plugin-db's sqlite_engine.rs; this is the shape `zeroship serve`
#       gives the same tables locally.
#   sdks/vite-plugin/src/gen-types/confined-ceiling.ts
#       the BUILD-TIME emit ceiling. A TOML string threaded into genArtifacts;
#       it decides what env.db.ts and schema.runtime.json say the table is.
#   sdks/vite-plugin/src/gen-types/dev-apply.ts
#       the DEV APPLY charter. What `pnpm dev` actually applies to
#       .zeroship/zs-<app>.sqlite before the runtime boots.
#   crates/zeroship-migrate-adapter/tests/smoke_apply_pg.rs
#   crates/zeroship-migrate-adapter/tests/author_and_apply_pg.rs
#       two test ceilings, each documented in its own file as "the confined
#       table-shape ceiling". They are Postgres-gated and skip without a DSN,
#       so their drift is invisible at runtime - which is exactly how they
#       drifted (see below).
#
# WHY ALL SIX AND NOT THE TWO THIS GATE USED TO COMPARE. Until 2026-08-20 this
# file compared crates/migrated + confined-ceiling.ts and nothing else. Those
# are the two copies FURTHEST from a creator's machine. The four it skipped
# include both copies a creator's own `pnpm dev` runs under.
#
# It was not a hypothetical gap. `ddf636140` ("fix(migrated): give the injected
# system columns their defaults") is the very bug this gate's original header
# cites as its reason for existing: created_at/updated_at/version were emitted
# NOT NULL with no default, so the first insert into any migration-created
# table failed. Its own commit body says it fixed "both confined-ceiling
# copies". There were four at the time. The two adapter test ceilings were
# never touched and still carried the pre-fix shape when this gate was widened
# - green the whole way, because they were not being looked at.
#
# THE COUNT IS THE THING THAT ROTS. A gate that compares "whatever copies it
# happens to know about" degrades silently every time someone adds a copy,
# which is the same defect one level up from the one it is guarding. So this
# gate ALSO scans the tree for inject rules it was not told about and refuses
# on a count it does not recognise. Adding a seventh copy makes this gate red
# until the copy is either declared here or (better) not added.
#
# WHAT THIS GATE CANNOT SEE, stated so nobody reads a green run as more than
# it is:
#   - It compares copies to each other; it does NOT check that the shape is
#     CORRECT. A wrong rule copied faithfully into all six passes here. The
#     end-to-end proof that the shape works is tests/e2e_db_app_end_to_end.sh
#     stage 4 and tests/golden_path.sh.
#   - It reads TOML text. A rule ASSEMBLED at runtime (string concatenation,
#     a builder, a serde struct rendered to TOML) carries no
#     `author_primary_key =` line and is invisible here. `zeroship-schema`'s
#     `build_system_field_columns` (crates/zeroship-schema/src/query.rs) is a
#     live example: it emits the same seven columns from Rust, is a genuine
#     seventh producer of this shape, and this gate has no way to compare it.
#     The migrated ceiling's own header records where the two already differ
#     (varchar(255) vs text, and the COLLATE "C" pin the engine cannot
#     express).
#   - It reads TRACKED files only, via `git ls-files`. Build output
#     (sdks/vite-plugin/dist/), node_modules, target/, and sibling worktrees
#     under .worktrees/ are all ignored and therefore unscanned. That is
#     deliberate - dist/ holds compiled copies of the same constants and would
#     double every count - but it means a copy that exists only as build output
#     is not ruled on.
#   - It does NOT descend into the third_party/zero-migrate submodule. The
#     vendored engine carries its own `#[cfg(test)]` ceiling fixtures
#     (ZEROSHIP_CONFINED_CEILING_TOML and friends); they belong to that repo's
#     history and move only on a deliberate submodule bump.
#   - It skips ITSELF. This file necessarily contains the strings it searches
#     for, and a gate that counts its own detection string as a finding is a
#     known way to be wrong. The cost is that a copy pasted into this file
#     would not be discovered.
#   - PROSE is not compared. docs/reference/zeroship-migrate-guide.md describes
#     the same shape in a sentence and a table; it can go stale without
#     tripping anything here.
#
# Usage:  ./tests/inject_policy_mirror_gate.sh
# Exit:   0 all copies identical
#         1 two or more copies drifted
#         2 a declared copy is missing, a copy extracts to nothing, or the tree
#           holds an inject rule this gate was not told about
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init inject_policy_mirror

# ---------------------------------------------------------------------------
# The declared mirror set. Every entry is `<id>|<path>`; the FIRST is the
# reference every other copy is diffed against.
#
# The reference is crates/migrated because it is the deployed one: it is the
# shape live creator tables actually have, so a disagreement is by definition
# the other file being wrong about production.
# ---------------------------------------------------------------------------
COPIES=(
    "server_ceiling|crates/migrated/policies/confined.policy.toml"
    "sqlite_ceiling|crates/plugin-db/policies/confined.policy.toml"
    "emit_ceiling|sdks/vite-plugin/src/gen-types/confined-ceiling.ts"
    "dev_charter|sdks/vite-plugin/src/gen-types/dev-apply.ts"
    "adapter_smoke|crates/zeroship-migrate-adapter/tests/smoke_apply_pg.rs"
    "adapter_author|crates/zeroship-migrate-adapter/tests/author_and_apply_pg.rs"
)

# Files that carry an inject rule and are deliberately NOT mirrored. Each needs
# a reason, and the reason has to be that the copy is INERT - not that it is
# inconvenient. A live copy belongs in COPIES above.
NOT_MIRRORED=(
    "docs/proposals/2026-08-09-dev-sqlite-migration-apply-ahead-of-runtime.md|a design snapshot, superseded by the shipped dev-apply.ts (whose header records the supersession)"
)

# Discovery finds this file's own COPIES/NOT_MIRRORED lines otherwise.
SELF="tests/inject_policy_mirror_gate.sh"

# MEASURED 2026-08-20: six live copies, one inert. Both numbers are asserted,
# not merely reported: a gate that adapts to whatever it finds cannot tell
# "nothing was added" from "something was added and I adjusted".
EXPECTED_COPIES=6
EXPECTED_INERT=1

# ---------------------------------------------------------------------------
# Extract the semantic lines of the [[inject]] rule: the scalar settings plus
# every `{ name = ... }` entry (columns AND indexes both use that shape).
# Comments and blank lines are dropped, leading indentation is stripped and
# internal runs of spaces are collapsed, so the copies may be formatted
# independently without tripping the gate.
#
# The same extractor works on .toml, .ts and .rs because in all three the rule
# is written at column 0 - inside a template literal or an r#""# raw string.
# ---------------------------------------------------------------------------
extract_rule() {
    sed -n '/^\[\[inject\]\]/,/^\[\[/p' "$1" \
        | grep -E '^\s*(scope|mandatory|primary_key|author_primary_key)\s*=|^\s*\{ *name *=' \
        | sed -e 's/#.*$//' -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' -e 's/[[:space:]]\{1,\}/ /g'
}

# ---------------------------------------------------------------------------
# Arm 1 (discovery): what does the TREE hold?
#
# The predicate is an actual TOML assignment (`author_primary_key =` at the
# start of a line), not the bare word. Prose that merely names the key -
# crates/migrated/src/apply.rs's comments, the migrate guide's markdown table -
# is correctly not a copy and is not counted.
# ---------------------------------------------------------------------------
#
# `git ls-files` prints paths relative to the repo root, so the grep has to run
# THERE - not in whatever directory the gate was invoked from. Running it in the
# caller's cwd made every path fail to open, which the anti-vacuity arm below
# caught as a zero-file scan rather than reporting a clean tree.
found_list="$(
    cd "$ROOT" && git ls-files -z \
        | xargs -0 -r grep -lE '^[[:space:]]*author_primary_key[[:space:]]*=' -- 2>/dev/null \
        | grep -v -x -F "$SELF" \
        | LC_ALL=C sort
)"
found_n=$(printf '%s\n' "$found_list" | grep -c . || true)

# `git ls-files` returning nothing (not a git checkout, git absent) would make
# every later count zero and every set comparison vacuously fine. Refuse first.
gate_arm discovery "$found_n" 5 || {
    echo "  FAIL: the tree scan found $found_n file(s) carrying an [[inject]] rule."
    echo "        Either the scan broke (no git checkout? predicate no longer"
    echo "        matches?) or the rule was spelled some other way. Fix the scan."
    gate_arms_finish || true
    exit 2
}

declared_list="$(
    {
        for entry in "${COPIES[@]}"; do printf '%s\n' "${entry#*|}"; done
        for entry in "${NOT_MIRRORED[@]}"; do printf '%s\n' "${entry%%|*}"; done
    } | LC_ALL=C sort
)"

# Both lists are sorted LC_ALL=C, so `comm` has to compare the same way or it
# reports spurious set differences on any path a locale collates differently.
undeclared="$(LC_ALL=C comm -23 <(printf '%s\n' "$found_list") <(printf '%s\n' "$declared_list"))"
vanished="$(LC_ALL=C comm -13 <(printf '%s\n' "$found_list") <(printf '%s\n' "$declared_list"))"

if [ -n "$undeclared" ]; then
    echo "  FAIL: the tree holds an [[inject]] rule this gate was not told about:"
    printf '%s\n' "$undeclared" | sed 's/^/        /'
    echo "        A SEVENTH copy of the platform table shape is a worse problem"
    echo "        than a stale gate. Delete it and share an existing one if you"
    echo "        can. If it genuinely must exist, add it to COPIES (to be kept"
    echo "        in lockstep) or, if it is inert, to NOT_MIRRORED with a reason,"
    echo "        and raise EXPECTED_COPIES / EXPECTED_INERT to match."
    gate_arms_finish || true
    exit 2
fi
if [ -n "$vanished" ]; then
    echo "  FAIL: a declared copy is no longer found by the tree scan:"
    printf '%s\n' "$vanished" | sed 's/^/        /'
    echo "        If it was deleted (good - one fewer copy), remove it from the"
    echo "        list above and lower EXPECTED_COPIES. If it merely moved or was"
    echo "        reworded, the gate has stopped watching a live copy."
    gate_arms_finish || true
    exit 2
fi
if [ "${#COPIES[@]}" -ne "$EXPECTED_COPIES" ] \
   || [ "${#NOT_MIRRORED[@]}" -ne "$EXPECTED_INERT" ]; then
    echo "  FAIL: declared ${#COPIES[@]} mirrored + ${#NOT_MIRRORED[@]} inert copies," \
         "expected $EXPECTED_COPIES + $EXPECTED_INERT."
    echo "        The lists and the expected counts are edited together on"
    echo "        purpose: the count is the part that rots silently."
    gate_arms_finish || true
    exit 2
fi

echo "  discovery: $found_n inject rule(s) in tracked files;" \
     "$EXPECTED_COPIES mirrored, $EXPECTED_INERT inert, all accounted for"

# ---------------------------------------------------------------------------
# Arms 2 and 3: extract every declared copy, then compare.
# ---------------------------------------------------------------------------
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

ids=()
extracted=0
total_lines=0
short=""
for entry in "${COPIES[@]}"; do
    id="${entry%%|*}"
    rel="${entry#*|}"
    abs="$ROOT/$rel"
    if [ ! -f "$abs" ]; then
        echo "  FAIL: missing $rel"
        gate_arms_finish || true
        exit 2
    fi
    extract_rule "$abs" > "$tmp/$id"
    n=$(grep -c . "$tmp/$id" || true)
    # A rule that extracts to nothing would make any two files "agree", so a
    # per-copy floor is checked as well as the total. Floor 5 is carried over
    # unchanged from the pre-library hand-rolled check this replaces: below it
    # there is too little left to distinguish "the rule shrank" from "the sed
    # stopped matching the [[inject]] block". Today each copy yields 14.
    [ "$n" -ge 5 ] || short="$short  $rel ($n lines)"$'\n'
    ids+=("$id")
    extracted=$((extracted + 1))
    total_lines=$((total_lines + n))
done

copies_ok=1
lines_ok=1
gate_arm copies "$extracted" 4 || copies_ok=0
gate_arm rule_lines "$total_lines" 30 || lines_ok=0
if [ "$copies_ok" -ne 1 ] || [ "$lines_ok" -ne 1 ] || [ -n "$short" ]; then
    echo "  FAIL: extracted too little to compare" \
         "(copies=$extracted lines=$total_lines)."
    [ -n "$short" ] && { echo "    copies under the per-copy floor of 5:";
                         printf '%s' "$short" | sed 's/^/    /'; }
    echo "    The [[inject]] rule moved or changed shape; fix this gate before"
    echo "    trusting it."
    gate_arms_finish || true
    exit 2
fi

ref_id="${ids[0]}"
ref_rel="${COPIES[0]#*|}"
drift=0
for entry in "${COPIES[@]:1}"; do
    id="${entry%%|*}"
    rel="${entry#*|}"
    if ! cmp -s "$tmp/$ref_id" "$tmp/$id"; then
        drift=$((drift + 1))
        echo "  FAIL: [[inject]] rule DRIFTED between $ref_id and $id"
        echo "        $ref_id: $ref_rel"
        echo "        $id: $rel"
        echo "        < $ref_id only, > $id only:"
        diff "$tmp/$ref_id" "$tmp/$id" | sed 's/^/          /'
    fi
done

if [ "$drift" -ne 0 ]; then
    echo "  $drift of $((extracted - 1)) copies disagree with $ref_id."
    gate_arms_finish || true
    exit 1
fi

echo "  ok: [[inject]] rule identical across all $extracted copies" \
     "($((total_lines / extracted)) semantic lines each)"
gate_arms_finish || exit 1
exit 0

# ---------------------------------------------------------------------------
# THE CASE AGAINST THIS GATE, kept here because whoever next edits it should
# read it before adding a seventh entry.
#
# Six hand-maintained copies of one security-relevant table shape is the actual
# defect. This gate makes the drift DETECTABLE; it does not make it impossible,
# and it quietly legitimises the duplication by making it survivable.
#
# The copies CAN be one. The invariant half is only the [[inject]] block - the
# grants around it legitimately differ per consumer (the emit ceiling needs
# none, the adapter fixtures pin cross_schema). So a single fragment holding
# just that block would serve every consumer:
#
#   Rust      concat!(include_str!("<grants>"), include_str!("<fragment>"))
#             - MEASURED 2026-08-20: concat! accepts include_str! and produces
#               the concatenation at compile time, so both .toml files become
#               grants-only and the shape has exactly one home.
#   TypeScript a prebuild codegen step emitting the fragment as a `const`,
#             alongside the scripts/build-bootstrap.ts step that already runs
#             in sdks/vite-plugin's build.
#
# What would still be needed afterwards is the DISCOVERY arm above: one source
# of truth is a property nothing enforces, and re-inlining a copy is a two-line
# edit. The comparison arms would go away; the count arm is the part that has
# to stay either way.
# ---------------------------------------------------------------------------
