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
#                 crates/zeroship-migrate-server/src/policy.rs       (deployed ceiling)
#                 crates/zeroship-migrate-server/tests/smoke_apply_pg.rs
#                 crates/zeroship-migrate-server/tests/author_and_apply_pg.rs
#                 crates/zeroship-migrate/tests/column_shapes/injected_column_collation.rs
#                 crates/zeroship-plugin-db/tests/distributed_live.rs
#   TypeScript  policies/codegen.mjs emits TWO views, because the two consumers
#               ask different questions of the same bytes:
#                 sdks/vite-plugin/src/gen-types/confined-system-shape.generated.ts
#                     the fragment verbatim as a const, which the emit ceiling
#                     and the dev-apply charter concatenate grants onto and load
#                     as a policy document.
#                 sdks/db/src/generated/confined-system-shape.generated.ts
#                     the rule PROJECTED into data - column name, nullability,
#                     assign - because @zeroship/db asks "does the platform
#                     compute this field?" per insert, and shipping it the TOML
#                     would mean shipping a TOML parser into the data plane.
#               Both are COMMITTED, not built, so tsc/tsx/the editor keep working
#               with no build-order knowledge - and so they are the two things
#               here that can still go stale. Arm 3 regenerates and diffs both.
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
#     measured 2026-08-10 on a deployed app schema). Closing THE TYPES needs a
#     check of a different kind - comparing rendered DDL, not text. See the
#     fragment's header. Both producers now pin the same bytewise comparison
#     intent, but this gate still cannot prove that rendered-DDL agreement.
#
#     THE NAMES, HOWEVER, ARE TEXT, and arm 5 now compares them. That paragraph
#     read as though the whole seventh producer were out of reach, and the part
#     that was in reach went unguarded for as long as it did partly because this
#     header said it could not be done. What the Rust producer publishes as
#     `SYSTEM_FIELD_NAMES` is an ordered list of the same seven names, and
#     nothing bound it to the fragment.
#
#     Why that matters more than a tidiness argument: `SYSTEM_FIELD_NAMES` is
#     not only DDL input. `implicit_read_projection_parts`
#     (crates/zeroship-schema/src/query.rs) walks it to build the SELECT column
#     list, reached from four production builders in that file, and
#     `read_surface_columns` derives from it.
#
#     THE FIRST DRAFT OF THIS ARM GOT THE CONSEQUENCE WRONG, and the wrong
#     version is the intuitive one, so it is written out here rather than
#     silently replaced. It said an eighth column added to the fragment alone
#     would never be projected. It would be. `implicit_read_projection_parts`
#     runs TWO loops: the const first, then every readable descriptor key not
#     already covered, and real descriptors carry the injected columns as
#     ordinary readable fields (examples/db-todos/generated/zeroship/
#     schema.runtime.json), generated from this same fragment.
#
#     The real asymmetry is sharper, and it runs both ways.
#
#     Const-only (a name here that the fragment does not inject): the first loop
#     projects it UNCONDITIONALLY, so every SELECT and every RETURNING names a
#     column no migration created. That is not a degraded read, it is every read
#     and every write of every collection failing on `column does not exist`.
#
#     Fragment-only (an eighth injected column not added here): it is created,
#     assigned, and projected - but only via the SECOND loop, which honours
#     `readable`. The seven in the const are projected without consulting it.
#     So the const is what makes the platform's own columns unhideable by a
#     hand-edited descriptor, and an eighth column would not inherit that: the
#     creator could drop the key or set `readable: false` and stop projecting
#     the value the platform still writes to their row.
#
#     Either way the two lists have to move together, which is what this arm
#     enforces. Only the failure mode differs.
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
    "crates/zeroship-migrate-core/src/model/table_shape.rs|unit-test policy fixtures compiled only under cfg(test)"
    "crates/zeroship-migrate-core/src/render/fold.rs|unit-test policy fixtures compiled only under cfg(test)"
    "crates/zeroship-migrate-core/src/render/lower.rs|unit-test policy fixtures compiled only under cfg(test)"
    "crates/zeroship-migrate-core/src/schema/query.rs|unit-test policy fixtures compiled only under cfg(test)"
    "crates/zeroship-migrate-core/src/test_fixtures.rs|shared test-only policy fixtures"
    "crates/zeroship-migrate-node/__test__/gen_artifacts.mjs|Node binding test fixture"
    "crates/zeroship-migrate-node/src/test_fixtures.rs|Node binding test-only policy fixture"
    "crates/zeroship-migrate-node/tests/support/mod.rs|integration-test policy fixture"
    "crates/zeroship-migrate-policy/tests/compose_oracle.rs|policy composer integration-test input"
    "crates/zeroship-migrate-policy/tests/loader.rs|policy loader integration-test input"
    "crates/zeroship-migrate-postgres/tests/namespace_authority.rs|PostgreSQL adapter integration-test input"
    "crates/zeroship-migrate-postgres/tests/support/mod.rs|PostgreSQL adapter test support input"
    "crates/zeroship-migrate/tests/column_shapes/injected_column_collation.rs|cross-dialect integration-test input"
    "crates/zeroship-migrate/tests/dialect_matrix/dialectal_ops.rs|dialect matrix integration-test input"
    "crates/zeroship-migrate/tests/policy_charter/layered_policy.rs|policy layering integration-test input"
    "crates/zeroship-migrate/tests/rename/rename_column_fk_definition_sqlite.rs|SQLite rename integration-test input"
    "crates/zeroship-migrate/tests/rename/rename_column_indexed_sqlite.rs|SQLite rename integration-test input"
    "crates/zeroship-migrate/tests/support/mod.rs|integration-test support policy fixture"
)

# Discovery finds this file's own declarations otherwise.
SELF="tests/inject_policy_mirror_gate.sh"

# MEASURED 2026-09-01: one authored fragment, two generated views, 19 inert
# fixtures, and eight consumers. Every number is ASSERTED, not merely reported: a gate
# that adapts to whatever it finds cannot tell "nothing was added" from
# "something was added and I adjusted".
#
# The six Rust consumers are the deployed ceiling, two adapter PG tests, the
# production-charter collation integration test, plugin-db's distributed live
# test, and plugin-db's own compiled-in charter. Three of those are tests, but
# consuming the shared fragment is the point: none is an inert copy of the
# platform shape.
#
# THIS SAID FIVE UNTIL 2026-09-01, and had been wrong since 27f4d5f45 ("feat(db):
# compile the operator charter into the worker") added
# crates/zeroship-plugin-db/src/system_shape_charter.rs as the sixth. That is the
# failure mode the asserted counts exist to produce - a new consumer is supposed
# to fail this gate until someone states it - so the red was the gate working.
# What it also shows is that the count and the prose above it rot together: the
# sentence naming the five was not re-read when the sixth landed.
EXPECTED_INERT=19
EXPECTED_RUST_CONSUMERS=6
EXPECTED_TS_CONSUMERS=2

# ---------------------------------------------------------------------------
# Arm 1 (discovery): what does the TREE hold?
#
# The predicate is an actual TOML assignment (`author_primary_key =` at the
# start of a line), not the bare word. Prose that merely names the key -
# crates/zeroship-migrate-server/src/apply.rs's comments, the migrate guide's markdown table -
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

# TWO generated targets are ruled on, and the floor is 2 because a run that
# byte-compared only one of them would print exactly what a clean tree prints.
# The two are different VIEWS of the same fragment, not copies of each other:
#
#   sdks/vite-plugin/.../confined-system-shape.generated.ts   the TOML verbatim,
#       because its consumer concatenates grants onto it and loads a policy
#       document.
#   sdks/db/src/generated/confined-system-shape.generated.ts  the rule PROJECTED
#       into data (column name, nullability, assign), because its consumer asks
#       "does the platform compute this field?" per insert and must not carry a
#       TOML parser into the data plane.
#
# Neither can be derived from the other by this gate, so both are regenerated in
# memory and diffed. The arm's other anti-vacuity guard is that the codegen
# itself refuses (exit 2) when the fragment carries no rule, no columns, or no
# column bearing an `assign`.
gate_arm codegen 2 2 || true

if [ "$codegen_rc" -ne 0 ]; then
    echo "  FAIL: a generated TypeScript view drifted from the fragment."
    sed 's/^/        /' "$codegen_log"
    echo "        Run \`node $CODEGEN\` and commit the result. Do NOT hand-edit"
    echo "        the file the line above marks DRIFT - both views are overwritten."
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

# ---------------------------------------------------------------------------
# Arm 5 (the seventh producer's NAME SET): the fragment assigns seven columns;
# `zeroship-schema` publishes those same seven as an ordered Rust const. Prove
# the two lists still agree, in content AND in order.
#
# ORDER, not just membership. `build_system_field_columns` renders its DDL by
# walking the const, and the const's own rustdoc says "Order MUST match
# SYSTEM_FIELD_NAMES" of the column emitter. A set comparison would pass on a
# reordering that changes the emitted column order of every creator table.
#
# WHAT THIS ARM DOES NOT CLOSE. Types, nullability, defaults and collation are
# still uncompared - see the header. This arm rules on names and their order,
# which is exactly the part that is text on both sides.
# ---------------------------------------------------------------------------
#
# A NOTE ON THE SINGLE PATH BELOW. An earlier draft of this arm special-cased a
# missing Rust file with its own `gate_arm name_agreement 0 4` call before the
# real one. tests/gate_arm_census.sh refused it: an arm id declared twice means
# one declaration can vouch for the other's count. It was right, and the fix is
# better than the code it rejected - a missing file makes the extractor yield
# nothing, which is already the floor's job to catch, so there is one path and
# one declaration. Do not reintroduce an early gate_arm call here.
RUST_NAMES_FILE="crates/zeroship-schema/src/query.rs"

# The fragment's side: every injected column carrying an `assign =` binding, in
# declaration order. `assign` is the discriminator on purpose - it selects the
# columns the PLATFORM computes, which is precisely the set the runtime write
# pass and the read projection both have to know about. An injected column
# without one would be creator-writable and does not belong in this comparison.
fragment_names=$(
    sed -n '/^\[\[inject\]\]/,/^\[\[/p' "$ROOT/$FRAGMENT" \
        | grep -E '^\s*\{ *name *=.*assign *=' \
        | sed -E 's/^\s*\{ *name *= *"([^"]+)".*/\1/' \
        || true
)

# The Rust side: the const's string literals, in declaration order.
rust_names=$(
    awk '/^pub const SYSTEM_FIELD_NAMES/{f=1;next} f&&/^\];/{exit} f' \
        "$ROOT/$RUST_NAMES_FILE" \
        | sed -nE 's/^\s*"([^"]+)",?\s*$/\1/p' \
        || true
)

fragment_n=$(printf '%s\n' "$fragment_names" | grep -c . || true)
rust_n=$(printf '%s\n' "$rust_names" | grep -c . || true)

# Examined = names this arm actually ruled on. Deliberately the SMALLER of the
# two: if one extraction collapses to nothing, the arm ruled on nothing, and
# taking the larger would let a broken extractor borrow the other side's count
# and report a healthy number while comparing against an empty list.
name_agreement_n=$fragment_n
[ "$rust_n" -lt "$name_agreement_n" ] && name_agreement_n=$rust_n

# Floor 4 against today's 7: low enough that deliberately retiring a system
# column does not turn this red, high enough that either sed/awk silently
# ceasing to match cannot pass. Both extractors are anchored on syntax that a
# reformat could move, which is the failure this floor is really watching for.
if ! gate_arm name_agreement "$name_agreement_n" 4; then
    echo "  FAIL: extracted $fragment_n assigned column(s) from $FRAGMENT and"
    echo "        $rust_n name(s) from SYSTEM_FIELD_NAMES."
    echo "        One of the two extractors stopped matching. In order of how"
    echo "        often each has actually happened: the file moved (is"
    echo "        $RUST_NAMES_FILE still there?), the const was renamed, or the"
    echo "        [[inject]] block was reformatted. Re-point this arm; do not"
    echo "        lower the floor."
    gate_arms_finish || true
    exit 2
fi

if [ "$fragment_names" != "$rust_names" ]; then
    echo "  FAIL: the platform column set has drifted between its two producers."
    echo
    echo "        $FRAGMENT (assigned columns, in order):"
    printf '%s\n' "$fragment_names" | sed 's/^/          /'
    echo "        $RUST_NAMES_FILE SYSTEM_FIELD_NAMES (in order):"
    printf '%s\n' "$rust_names" | sed 's/^/          /'
    echo
    echo "        These are not interchangeable copies. The fragment drives the"
    echo "        migration that CREATES the columns and the runtime pass that"
    echo "        ASSIGNS them; the const drives the SELECT and RETURNING lists"
    echo "        (implicit_read_projection_parts), which project it whether or"
    echo "        not the descriptor says the field is readable."
    echo
    echo "        In the const and not the fragment: every read and every write"
    echo "        of every collection fails on a column no migration created."
    echo "        In the fragment and not the const: the column works, but its"
    echo "        projection now depends on the creator-authored descriptor, so"
    echo "        the creator can hide a value the platform still writes."
    echo "        Change both in the same commit."
    gate_arms_finish || true
    exit 2
fi

echo "  ok: one [[inject]] rule ($rule_lines semantic lines), $consumers_n consumers," \
     "generated view fresh, $name_agreement_n platform column names agree with" \
     "SYSTEM_FIELD_NAMES"
gate_arms_finish || exit 1
exit 0
