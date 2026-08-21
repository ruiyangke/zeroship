#!/usr/bin/env bash
# ============================================================================
# inject_policy_mirror_gate.sh - the confined [[inject]] rule is written ONCE.
# Prove it is still once, that everything which needs it still takes it from
# there, and that the one generated view of it has not gone stale.
#
# THE RULE. Seven platform system columns (id / created_at / updated_at /
# created_by / updated_by / version / deleted_at), a pinned ["id"] primary key,
# author_primary_key = "forbid", and three system indexes, injected into every
# table a creator migration creates. It is the shape of every creator table on
# the platform, so a copy that drifts is a creator app whose table does not
# match what the rest of the toolchain believes it built.
#
# THIS FILE USED TO COMPARE SIX HAND-MAINTAINED COPIES of that rule. It no
# longer does, because the copies are gone. The rule lives in
# policies/confined-system-shape.inject.toml and every consumer concatenates it:
#
#   Rust        concat!(include_str!("<its grants>"), include_str!("<fragment>"))
#               rustc folds both at compile time, so the fragment's bytes are
#               literally in the binary. Five consumers:
#                 crates/migrated/src/policy.rs                (deployed ceiling)
#                 crates/plugin-db/src/register_model/sqlite_engine.rs (dev)
#                 crates/plugin-db/tests/parity/mod.rs         (PG/SQLite matrix)
#                 crates/zeroship-migrate-adapter/tests/smoke_apply_pg.rs
#                 crates/zeroship-migrate-adapter/tests/author_and_apply_pg.rs
#   TypeScript  policies/codegen.mjs emits the fragment as a const into
#               sdks/vite-plugin/src/gen-types/confined-system-shape.generated.ts,
#               which the emit ceiling and the dev-apply charter import. That
#               file is COMMITTED, not built, so tsc/tsx/the editor keep working
#               with no build-order knowledge - and so it is the one thing here
#               that can still go stale. Arm 3 regenerates and diffs it.
#
# WHAT THAT CHANGES ABOUT THIS GATE. Comparing copies is no longer a question
# anyone can answer wrongly; the compiler answers it. What is left is the part a
# comparison never covered:
#
#   ONE SOURCE OF TRUTH IS NOT SELF-ENFORCING. Re-inlining the block into a
#   consumer is a two-line edit and compiles clean. So arm 1 still scans the tree
#   for inject rules it was not told about, and still refuses on a count it does
#   not recognise. That arm predates the de-duplication and outlives it.
#
#   A CONSUMER CAN STOP CONSUMING. Deleting the include_str! from a ceiling is
#   also a two-line edit; the charter then injects nothing and the app's tables
#   lose their system columns. No text comparison of the remaining copies would
#   have seen that either. Arm 2 counts the consumers.
#
#   Arm 2 is not hypothetical. crates/plugin-db/tests/parity/mod.rs was NEVER one
#   of the six copies, correctly - it include_str!s plugin-db's ceiling instead of
#   restating the rule, which is why the old gate had nothing to compare and said
#   nothing about it. Moving the rule out of that ceiling would have silently left
#   the PG/SQLite parity matrix comparing two dialects that both inject nothing.
#   It was found by reading, not by the gate, and arm 2 exists so the next one is
#   not.
#
# WHAT THIS GATE CANNOT SEE, stated so nobody reads a green run as more than it
# is:
#   - It does NOT check that the shape is CORRECT. A wrong rule, now shared by
#     everything, passes here. The end-to-end proof that the shape works is
#     tests/e2e_db_app_end_to_end.sh stage 4 and tests/golden_path.sh.
#   - A rule ASSEMBLED at runtime (string concatenation, a builder, a serde
#     struct rendered to TOML) carries no `author_primary_key =` line and is
#     invisible to arm 1. `zeroship-schema`'s `build_system_field_columns`
#     (crates/zeroship-schema/src/query.rs) is a live example: it emits the same
#     seven columns from Rust, is a genuine SEVENTH producer of this shape, and
#     it cannot be made to consume the fragment: it renders DDL directly, with
#     no policy document in the path, and the two producers disagree on the
#     id/created_by/updated_by TYPE (varchar(255) via the engine vs TEXT here,
#     measured 2026-08-10 on a deployed app schema). Closing that needs a check
#     of a different kind - comparing rendered DDL, not text. See the fragment's
#     header, which until 2026-08-20 wrongly said zeroship-schema had already
#     pinned COLLATE "C"; neither producer has, and #255 is open on both.
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
# Exit:   0 one fragment, every consumer takes it, the generated view is fresh
#         1 the generated TypeScript view drifted from the fragment
#         2 the tree holds an inject rule this gate was not told about, a
#           declared file is missing, a consumer stopped consuming, or the
#           fragment no longer carries a rule
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init inject_policy_mirror

# ---------------------------------------------------------------------------
# The one authored copy.
# ---------------------------------------------------------------------------
FRAGMENT="policies/confined-system-shape.inject.toml"

# The one GENERATED copy. Committed on purpose (see the header); regenerated and
# byte-compared by arm 3.
GENERATED="sdks/vite-plugin/src/gen-types/confined-system-shape.generated.ts"
CODEGEN="policies/codegen.mjs"

# Files that carry an inject rule and are deliberately neither the fragment nor
# generated from it. Each needs a reason, and the reason has to be that the copy
# is INERT - not that it is inconvenient. A live copy consumes the fragment.
NOT_MIRRORED=(
    "docs/proposals/2026-08-09-dev-sqlite-migration-apply-ahead-of-runtime.md|a design snapshot, superseded by the shipped dev-apply.ts (whose header records the supersession)"
)

# Discovery finds this file's own declarations otherwise.
SELF="tests/inject_policy_mirror_gate.sh"

# MEASURED 2026-08-20: one authored fragment, one generated view, one inert
# snapshot, six consumers. Every number is ASSERTED, not merely reported: a gate
# that adapts to whatever it finds cannot tell "nothing was added" from
# "something was added and I adjusted".
EXPECTED_INERT=1
EXPECTED_RUST_CONSUMERS=5
EXPECTED_TS_CONSUMERS=2

# ---------------------------------------------------------------------------
# Arm 1 (discovery): what does the TREE hold?
#
# The predicate is an actual TOML assignment (`author_primary_key =` at the
# start of a line), not the bare word. Prose that merely names the key -
# crates/migrated/src/apply.rs's comments, the migrate guide's markdown table -
# is correctly not a copy and is not counted.
#
# `git ls-files` prints paths relative to the repo root, so the grep has to run
# THERE - not in whatever directory the gate was invoked from. Running it in the
# caller's cwd made every path fail to open, which the anti-vacuity arm below
# caught as a zero-file scan rather than reporting a clean tree.
# ---------------------------------------------------------------------------
found_list="$(
    cd "$ROOT" && git ls-files -z \
        | xargs -0 -r grep -lE '^[[:space:]]*author_primary_key[[:space:]]*=' -- 2>/dev/null \
        | grep -v -x -F "$SELF" \
        | LC_ALL=C sort
)"
found_n=$(printf '%s\n' "$found_list" | grep -c . || true)

# `git ls-files` returning nothing (not a git checkout, git absent) would make
# every later count zero and every set comparison vacuously fine. Refuse first.
gate_arm discovery "$found_n" 2 || {
    echo "  FAIL: the tree scan found $found_n file(s) carrying an [[inject]] rule."
    echo "        The fragment and its generated view are two of them, so a count"
    echo "        below that means the scan broke (no git checkout? predicate no"
    echo "        longer matches?) or the rule was spelled some other way."
    gate_arms_finish || true
    exit 2
}

declared_list="$(
    {
        printf '%s\n' "$FRAGMENT"
        printf '%s\n' "$GENERATED"
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
    echo "        There is supposed to be ONE copy of the platform table shape,"
    echo "        in $FRAGMENT, plus the generated"
    echo "        TypeScript view of it. Delete this one and consume the fragment:"
    echo "          Rust  concat!(include_str!(\"<grants>\"), include_str!(\"<fragment>\"))"
    echo "          TS    import from the generated module"
    echo "        If it genuinely must exist and is INERT, add it to NOT_MIRRORED"
    echo "        with a reason and raise EXPECTED_INERT to match."
    gate_arms_finish || true
    exit 2
fi
if [ -n "$vanished" ]; then
    echo "  FAIL: a declared file is no longer found by the tree scan:"
    printf '%s\n' "$vanished" | sed 's/^/        /'
    echo "        The fragment or its generated view has moved, been deleted, or"
    echo "        no longer carries the rule. Nothing below is worth trusting"
    echo "        until the scan finds them again."
    gate_arms_finish || true
    exit 2
fi
if [ "${#NOT_MIRRORED[@]}" -ne "$EXPECTED_INERT" ]; then
    echo "  FAIL: declared ${#NOT_MIRRORED[@]} inert copies, expected $EXPECTED_INERT."
    echo "        The list and the expected count are edited together on purpose:"
    echo "        the count is the part that rots silently."
    gate_arms_finish || true
    exit 2
fi

echo "  discovery: $found_n inject rule(s) in tracked files; 1 fragment," \
     "1 generated view, $EXPECTED_INERT inert, all accounted for"

# ---------------------------------------------------------------------------
# Arm 2 (consumers): does everything that needs the rule still TAKE it?
#
# Losing an include is as silent as adding a copy and arm 1 cannot see it: the
# consumer's charter simply stops injecting, and every creator table it governs
# comes out without its system columns.
# ---------------------------------------------------------------------------
rust_consumers="$(
    cd "$ROOT" && git ls-files -z '*.rs' \
        | xargs -0 -r grep -lF "include_str!(\"" -- 2>/dev/null \
        | xargs -r grep -lF "policies/confined-system-shape.inject.toml" -- 2>/dev/null \
        | LC_ALL=C sort
)"
rust_n=$(printf '%s\n' "$rust_consumers" | grep -c . || true)

# The generated module DEFINES the const; it does not consume it. Excluding it
# is what makes this a count of consumers rather than of mentions.
ts_consumers="$(
    cd "$ROOT" && git ls-files -z '*.ts' \
        | xargs -0 -r grep -lF "CONFINED_SYSTEM_SHAPE_INJECT_TOML" -- 2>/dev/null \
        | grep -v -x -F "$GENERATED" \
        | LC_ALL=C sort
)"
ts_n=$(printf '%s\n' "$ts_consumers" | grep -c . || true)

consumers_n=$((rust_n + ts_n))
consumers_ok=1
gate_arm consumers "$consumers_n" 4 || consumers_ok=0
if [ "$consumers_ok" -ne 1 ] \
   || [ "$rust_n" -ne "$EXPECTED_RUST_CONSUMERS" ] \
   || [ "$ts_n" -ne "$EXPECTED_TS_CONSUMERS" ]; then
    echo "  FAIL: found $rust_n Rust + $ts_n TypeScript consumers of the fragment," \
         "expected $EXPECTED_RUST_CONSUMERS + $EXPECTED_TS_CONSUMERS."
    echo "        Rust:"
    printf '%s\n' "$rust_consumers" | sed 's/^/          /'
    echo "        TypeScript:"
    printf '%s\n' "$ts_consumers" | sed 's/^/          /'
    echo "        A consumer that stopped taking the fragment injects NOTHING, and"
    echo "        every table it governs loses its system columns. If a consumer"
    echo "        was deliberately deleted, lower the expected count here in the"
    echo "        same commit."
    gate_arms_finish || true
    exit 2
fi

echo "  consumers: $rust_n Rust include_str! + $ts_n TypeScript imports"

# ---------------------------------------------------------------------------
# Arm 3 (codegen freshness): the generated TypeScript view is committed, so it
# is the one copy that can still drift. Regenerate in memory and byte-compare.
#
# This is STRICTLY STRONGER than the six-way text comparison it replaces, which
# folded whitespace and dropped comments and so could not tell a stale comment
# from a current one. Here a single changed byte anywhere in the fragment fails.
# ---------------------------------------------------------------------------
if [ ! -f "$ROOT/$CODEGEN" ]; then
    echo "  FAIL: $CODEGEN is missing; the generated view cannot be checked."
    gate_arms_finish || true
    exit 2
fi

codegen_log="$(mktemp)"
trap 'rm -f "$codegen_log"' EXIT
node "$ROOT/$CODEGEN" --check >"$codegen_log" 2>&1
codegen_rc=$?

# One generated target is ruled on. The floor is 1 because there is exactly one,
# and the arm's real anti-vacuity guard is that the codegen itself refuses (exit
# 2) when the fragment carries no rule to generate from.
gate_arm codegen 1 1 || true

if [ "$codegen_rc" -ne 0 ]; then
    echo "  FAIL: the generated TypeScript view drifted from the fragment."
    sed 's/^/        /' "$codegen_log"
    echo "        Run \`node $CODEGEN\` and commit the result. Do NOT hand-edit"
    echo "        $GENERATED - it is overwritten."
    gate_arms_finish || true
    # exit 2 when the codegen refused outright (no rule to generate from, an
    # unembeddable character); exit 1 when it simply found drift.
    [ "$codegen_rc" -ge 2 ] && exit 2
    exit 1
fi
sed 's/^/  codegen:/' "$codegen_log"

# ---------------------------------------------------------------------------
# Arm 4 (the rule is still a rule): extract the semantic lines of the fragment -
# the scalar settings plus every `{ name = ... }` entry (columns AND indexes both
# use that shape). A fragment that extracts to nothing would sail through every
# arm above: the codegen would faithfully generate an empty ceiling and every
# consumer would faithfully inject nothing.
# ---------------------------------------------------------------------------
rule_lines=$(
    sed -n '/^\[\[inject\]\]/,/^\[\[/p' "$ROOT/$FRAGMENT" \
        | grep -cE '^\s*(scope|mandatory|primary_key|author_primary_key)\s*=|^\s*\{ *name *=' \
        || true
)

# Floor 5 is carried over unchanged from the per-copy floor of the gate this
# replaces: below it there is too little left to distinguish "the rule shrank"
# from "the sed stopped matching the [[inject]] block". Today it yields 14.
if ! gate_arm rule_lines "$rule_lines" 5; then
    echo "  FAIL: $FRAGMENT extracted to $rule_lines semantic line(s)."
    echo "        The [[inject]] rule moved or changed shape; fix this gate before"
    echo "        trusting it."
    gate_arms_finish || true
    exit 2
fi

echo "  ok: one [[inject]] rule ($rule_lines semantic lines), $consumers_n consumers," \
     "generated view fresh"
gate_arms_finish || exit 1
exit 0
