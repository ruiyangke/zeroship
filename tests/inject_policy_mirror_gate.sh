#!/usr/bin/env bash
# The confined injection policy is shared by migration hosts. Runtime assignments
# and projections come from generated collection descriptors, without a policy copy.
# Check policy discovery, consumers, generated build-time TOML and rule presence.
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
GENERATED="packages/vite-plugin/src/gen-types/confined-system-shape.generated.ts"
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

EXPECTED_INERT=19
EXPECTED_RUST_CONSUMERS=5
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
# ONE REGEX BINDING THE MACRO TO THE PATH, not two file-level greps ANDed.
#
# This was `grep -lF 'include_str!("'` piped into `grep -lF '<the path>'`, which
# asks whether a file contains BOTH somewhere - a relation neither grep can see.
# Measured 2026-09-04: that returned 6 files, the bound regex returns 5, and the
# extra was `crates/zeroship-data-v8/tests/distributed_live.rs`, whose two
# qualifying lines are a DOC COMMENT naming the policy (`:66`) and an
# `include_str!` of the db SDK's generated bundle (`:488`). Nothing in that file
# takes the fragment.
#
# It was wrong in both directions, and the silent one is the reason this arm
# exists: deleting that doc comment - a pure prose edit - would have taken the
# count to 5 and failed the gate with "a consumer stopped taking the fragment";
# and a real consumer dropping its include while any `.rs` file gained a prose
# mention of the path would have held the count at 6 and said nothing.
rust_consumers="$(
    cd "$ROOT" && git ls-files -z '*.rs' \
        | xargs -0 -r grep -lE \
            'include_str!\([[:space:]]*"[^"]*policies/confined-system-shape\.inject\.toml"' \
            -- 2>/dev/null \
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

# The runtime SDK consumes assignments from runtime.json; only the build-time
# policy module is generated here.
gate_arm codegen 1 1 || true

if [ "$codegen_rc" -ne 0 ]; then
    echo "  FAIL: a generated TypeScript view drifted from the fragment."
    sed 's/^/        /' "$codegen_log"
    echo "        Run \`node $CODEGEN\` and commit the result. Do NOT hand-edit"
    echo "        the file the line above marks DRIFT - the generated module is overwritten."
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

# Require meaningful rule content so an empty extraction cannot pass.
if ! gate_arm rule_lines "$rule_lines" 5; then
    echo "  FAIL: $FRAGMENT extracted to $rule_lines semantic line(s)."
    echo "        The [[inject]] rule moved or changed shape; fix this gate before"
    echo "        trusting it."
    gate_arms_finish || true
    exit 2
fi

echo "  ok: policy consumers and generated build-time view agree"
gate_arms_finish || exit 1
exit 0
