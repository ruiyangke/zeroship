#!/usr/bin/env bash
# ============================================================================
# THE CONFIGURATION THAT SHIPS MUST COMPILE.
#
# Lib and bin targets, DEFAULT features, no dev-dependencies in the unit graph.
# That is what a release worker, gateway, control plane and CLI are built from,
# and until 2026-09-04 nothing in this repo ever built it.
#
# ---------------------------------------------------------------------------
# WHAT WENT WRONG, and why every existing check was green while it did
# ---------------------------------------------------------------------------
#
# d6418b39b added `BackendHandle::introspect_schema` to
# crates/zeroship-data-orm/src/backend_handle.rs as UNGATED production code,
# called from the production write path by
# `zeroship_data_orm::crud::protection_floor`. The trait it calls,
# `SchemaIntrospect`, and both vendor impls were `#[cfg(feature =
# "test-helpers")]`. So:
#
#     cargo check -p zeroship-data-orm --lib   ->  3 errors
#     cargo check -p zeroship-worker     --bins   ->  1 error
#
#     error[E0432]: unresolved import `crate::backend::SchemaIntrospect`
#     error[E0599]: no method named `introspect_schema` for `&Rc<PostgresBackend>`
#     error[E0599]: no method named `introspect_schema` for `&Rc<SqliteBackend>`
#
# THE SHIPPED BINARIES DID NOT BUILD, on main, for a day. Everything anybody
# ran said otherwise:
#
#     cargo check --workspace --all-targets     0 errors
#     cargo check --workspace --all-features    0 errors
#     tests/clippy_gate.sh                      0 errors
#     cargo test -p <anything>                  green
#
# ONE THING WOULD HAVE CAUGHT IT, and saying so is the point rather than a
# caveat: CI's own `cargo check --workspace` (.github/workflows/ci.yml, the
# `rust` job). Measured on the broken tree, 2026-09-04: exit 101, the same three
# errors. Its BARE target selection is load-bearing and nothing said so - cargo
# defaults to lib and bins, and appending `--all-targets`, which checks strictly
# more and is the obvious improvement, would have made it green on a workspace
# that does not build. That line is one edit away from silence, it reads cargo's
# exit code rather than auditing what cargo actually attempted, and it has no
# opinion at all about feature resolution. This gate is the version of it that
# states its own contract and can be run locally in 20 seconds.
#
# A SECOND ONE WENT RED AND NAMED THE WRONG DEFECT, which is the more useful
# half of the lesson. `tests/run_doc_gate.sh` also builds default features, so
# its default arm failed too - measured on the broken tree as:
#
#     --- default features : exit=101 documented=19 unresolved=3 (max 0) ---
#
# `cargo doc` aborted 19 crates into 44, and the gate reported "3 unresolved doc
# links". Nothing in that line says the workspace does not compile, and the
# number it prints is not a reading of the tree at all - it is whatever 19 of 44
# crates happened to contain before the abort. A gate that goes red for the
# right reason and says the wrong thing sends the next person to fix doc links.
# That is why arm 2 here audits the target set and prints rustc's own errors.
#
# THE MECHANISM, and it is not a quirk of one crate. `test-helpers` is declared
# in zeroship-data-orm's `[dev-dependencies]` and NOT in `[dependencies]`
# (the same shape zeroship-plugin-db uses). Under resolver v3 a dev-dependency's
# features are unified into the normal dependency edge WHENEVER A TEST TARGET IS
# IN THE UNIT GRAPH. `--all-targets` puts one there. `--all-features` turns the
# feature on outright. `cargo test` builds test targets by definition. Every one
# of those invocations therefore compiles the lib WITH `test-helpers`, and a
# `#[cfg(feature = "test-helpers")]` item that production code calls is present
# in all of them. The only invocation that resolves features the way a release
# build does is a lib/bins-only build with default features - which nothing ran.
#
# Measured, on this same tree, from the two `compiler-artifact` feature arrays:
#
#     cargo check --workspace --lib --bins    zeroship-data-orm features: []
#     cargo check --workspace --all-targets   zeroship-data-orm features:
#                                                 ["test-helpers"]
#
# That contrast is what arm 3 rules on directly, and it is why this gate is not
# just "run cargo check again with different flags": the flags are the finding.
#
# ---------------------------------------------------------------------------
# THE THREE ARMS, and what each one alone would miss
# ---------------------------------------------------------------------------
#
# 1. `shipped_targets` - the EXPECTATION. Every workspace-member target that a
#    default-feature `--lib --bins` build selects, derived from `cargo metadata`
#    rather than listed here. Targets whose `required-features` the default
#    resolution does not satisfy are filtered OUT, because cargo silently skips
#    them and counting them as unbuilt would make the gate permanently red.
#    (Today that is exactly two: zeroship-runtime's `echo-server` and
#    `zeroship-bench-server`, both behind `bench-bins`.) This arm exists because
#    arm 2 is a comparison, and a comparison against an empty expectation
#    passes.
#
# 2. `built_targets` - the VERDICT. Audits cargo's own `--message-format=json`
#    artifact stream against that expectation and names any expected target
#    cargo never reported. It is written as an audit and not as `exit 0` from
#    cargo for the reason tests/clippy_gate.sh records: A DEPENDENCY FAILURE
#    ABORTS SCHEDULING, so the crates downstream of it are never attempted and
#    absence proves nothing. Naming them is the difference between "the tree is
#    clean" and "cargo stopped".
#
# 3. `test_helpers_off` - the MECHANISM. For every member declaring a
#    `test-helpers` feature, assert the shipped build really resolved it OFF.
#    Arms 1 and 2 cannot see this: they would both stay green if somebody
#    "fixed" a future instance of this defect by adding `test-helpers` to
#    `[dependencies]` or to `default`, which compiles fine and puts test
#    scaffolding - `DbBinding::cold_start`, the `PgSqlExecutor` raw-pool escape
#    hatch, `reset_*_for_tests` - into the release binary. That is the wrong
#    repair for this defect and this arm is what refuses it.
#
# ---------------------------------------------------------------------------
# WHAT THIS DOES NOT RULE ON
# ---------------------------------------------------------------------------
#
# Warnings: it counts errors only. Lints are tests/clippy_gate.sh's question,
# and that gate deliberately builds a DIFFERENT configuration (`--all-features`,
# `--all-targets`) for a different reason - the two are complements, not
# duplicates, and neither substitutes for the other.
#
# Test targets: by construction. A `cargo test` failure is loud and everything
# in tests/ already builds them.
#
# The release PROFILE: this runs `cargo check` under `dev`. A `--release`-only
# breakage (an `#[cfg(debug_assertions)]` item a release path calls) would slip
# past. The feature resolution is the axis this gate exists for; the profile
# axis has never bitten here and a full release check costs minutes, not
# seconds.
#
# Run:  tests/shipped_config_gate.sh
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init shipped_config

# ---------------------------------------------------------------------------
# Floors guard target enumeration and both crates retaining test helpers.
MIN_TARGETS=30
MIN_BUILT=30
MIN_FEATURE_MEMBERS=2

fail=0
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

command -v cargo >/dev/null 2>&1 || {
  echo "  x REFUSED: cargo is not on PATH. This gate derives every set from" >&2
  echo "             cargo's own resolution and would otherwise inspect" >&2
  echo "             nothing and exit 0." >&2
  exit 1
}
command -v jq >/dev/null 2>&1 || {
  echo "  x REFUSED: jq is not on PATH; this gate cannot read cargo's json." >&2
  exit 1
}

# ---------------------------------------------------------------------------
# PREFLIGHT: the generated inputs the LIBRARIES `include_str!`.
#
# NOT an arm. An arm rules on the tree; this rules on whether this machine can
# compile it at all. crates/zeroship-runtime's lib embeds three files that
# `pnpm build` emits and git does not track, and without them the workspace
# fails to build for a reason that has nothing to do with the shipped
# configuration. Refusing here, naming the command, is the difference between a
# usable red and a confusing one.
#
# The list is DERIVED (every `include_str!` literal in a crate's `src/` that
# resolves under `sdks/`), not written out, so a fourth one is covered the day
# it lands. A derived list that matched nothing would refuse silently, so the
# empty case is itself a refusal.
# ---------------------------------------------------------------------------
grep -rhoE '"\.\.[^"]*/sdks/[^"]+"' "$ROOT"/crates/*/src "$ROOT"/libs/*/src \
  --include='*.rs' 2>/dev/null \
  | tr -d '"' | sed 's#^.*/sdks/#sdks/#' | LC_ALL=C sort -u > "$TMP/generated.txt"

n_generated="$(grep -c . "$TMP/generated.txt" || true)"
if [ "${n_generated:-0}" -lt 1 ]; then
  echo "  x REFUSED: found no include_str! literal resolving under sdks/." >&2
  echo "             The scan matched nothing, so it proves nothing. Either" >&2
  echo "             the embeds moved or this grep stopped matching them." >&2
  exit 1
fi

missing_generated=0
while IFS= read -r rel; do
  [ -n "$rel" ] || continue
  if [ ! -f "$ROOT/$rel" ]; then
    echo "  x REFUSED: $rel is missing." >&2
    missing_generated=$((missing_generated + 1))
  fi
done < "$TMP/generated.txt"
if [ "$missing_generated" -ne 0 ]; then
  echo "             The runtime crate include_str!s these; they are emitted" >&2
  echo "             by \`pnpm build\` and are not tracked in git. Run it from" >&2
  echo "             the repo root, then re-run this gate. Building a smaller" >&2
  echo "             workspace and calling it green is what this refuses." >&2
  exit 1
fi
echo "  - preflight: $n_generated generated include_str! input(s) present"

# ---------------------------------------------------------------------------
# ARM 1 - the expectation.
# ---------------------------------------------------------------------------
if ! (cd "$ROOT" && cargo metadata --format-version 1 --no-deps) \
    > "$TMP/meta.json" 2> "$TMP/meta.err"; then
  echo "  x REFUSED: cargo metadata failed; no expectation can be derived." >&2
  cat "$TMP/meta.err" >&2
  exit 1
fi

# The default-feature set per package, so a `required-features` target can be
# ruled in or out the way cargo rules it. `default` is expanded one level, which
# is all any `required-features` in this workspace needs; a nested default that
# this misses would make the arm expect a target cargo skips, which shows up as
# a NAMED shortfall in arm 2 rather than as a silent pass.
jq -r '
  .packages[]
  | . as $p
  | (($p.features.default // []) + ["default"]) as $defaults
  | $p.targets[]
  | select(.kind | any(. == "lib" or . == "rlib" or . == "proc-macro" or . == "bin"))
  | select(((.["required-features"] // []) - $defaults) | length == 0)
  | [$p.manifest_path, .name, (.kind | join(","))]
  | @tsv
' "$TMP/meta.json" | LC_ALL=C sort -u > "$TMP/expected.tsv"

n_expected="$(grep -c . "$TMP/expected.tsv" || true)"
gate_arm shipped_targets "${n_expected:-0}" "$MIN_TARGETS" || fail=1

# The members that declare a `test-helpers` feature at all - arm 3's
# denominator, derived from the same metadata.
jq -r '.packages[] | select(.features | has("test-helpers")) | .manifest_path' \
  "$TMP/meta.json" | LC_ALL=C sort -u > "$TMP/feature_members.txt"

# ---------------------------------------------------------------------------
# THE BUILD. Default features. `--lib --bins` and nothing else: adding any test,
# bench or example target here would put dev-dependencies back in the unit graph
# and reproduce exactly the blind spot this gate exists to close.
# ---------------------------------------------------------------------------
echo "  - building: cargo check --workspace --lib --bins (default features)"
(cd "$ROOT" && cargo check --workspace --lib --bins --message-format=json) \
  > "$TMP/build.json" 2> "$TMP/build.err"
build_status=$?

# Print the errors themselves. The target audit below says WHICH targets never
# built; only rustc says why, and a gate that withholds that makes the operator
# re-run the command by hand to find out.
jq -r 'select(.reason == "compiler-message")
       | select(.message.level == "error")
       | .message.rendered // empty' "$TMP/build.json" > "$TMP/errors.txt" 2>/dev/null
n_errors="$(grep -c '^error' "$TMP/errors.txt" || true)"
if [ "${n_errors:-0}" -gt 0 ]; then
  echo "" >&2
  echo "  x THE SHIPPED CONFIGURATION DOES NOT COMPILE - ${n_errors} error(s):" >&2
  cat "$TMP/errors.txt" >&2
  fail=1
fi

# ---------------------------------------------------------------------------
# ARM 2 - the verdict, as an audit of cargo's own stream.
# ---------------------------------------------------------------------------
jq -r 'select(.reason == "compiler-artifact")
       | [.manifest_path, .target.name, (.target.kind | join(","))]
       | @tsv' "$TMP/build.json" 2>/dev/null \
  | LC_ALL=C sort -u > "$TMP/observed.tsv"

LC_ALL=C comm -12 "$TMP/expected.tsv" "$TMP/observed.tsv" > "$TMP/built.tsv"
LC_ALL=C comm -23 "$TMP/expected.tsv" "$TMP/observed.tsv" > "$TMP/unbuilt.tsv"

n_built="$(grep -c . "$TMP/built.tsv" || true)"
n_unbuilt="$(grep -c . "$TMP/unbuilt.tsv" || true)"
gate_arm built_targets "${n_built:-0}" "$MIN_BUILT" || fail=1

if [ "${n_unbuilt:-0}" -gt 0 ]; then
  echo "" >&2
  echo "  x ${n_unbuilt} SHIPPED TARGET(S) NEVER COMPILED:" >&2
  while IFS=$'\t' read -r manifest target kind; do
    [ -n "$manifest" ] || continue
    echo "      ${manifest#"$ROOT"/}  $target ($kind)" >&2
  done < "$TMP/unbuilt.tsv"
  echo "    Cargo emits a compiler-artifact for every unit it finishes," >&2
  echo "    including cached ones, so a target absent from that stream was" >&2
  echo "    never attempted. A failure upstream aborts scheduling, which is" >&2
  echo "    why this is an audit and not a reading of cargo's exit code." >&2
  fail=1
fi

if [ "$build_status" -ne 0 ] && [ "${n_errors:-0}" -eq 0 ] \
   && [ "${n_unbuilt:-0}" -eq 0 ]; then
  echo "  x cargo exited $build_status with no error message and no unbuilt" >&2
  echo "    target. Something failed outside the compile itself:" >&2
  tail -20 "$TMP/build.err" >&2
  fail=1
fi

# ---------------------------------------------------------------------------
# ARM 3 - the mechanism: `test-helpers` resolved OFF in the shipped build.
# ---------------------------------------------------------------------------
jq -r 'select(.reason == "compiler-artifact")
       | select(.target.kind | any(. == "lib" or . == "rlib" or . == "proc-macro"))
       | [.manifest_path, ((.features // []) | join(","))]
       | @tsv' "$TMP/build.json" 2>/dev/null \
  | LC_ALL=C sort -u > "$TMP/lib_features.tsv"

n_feature_ruled=0
while IFS= read -r manifest; do
  [ -n "$manifest" ] || continue
  # A member whose lib never built is arm 2's finding, not this arm's; it is
  # not counted here, so this arm never vouches for a unit it did not see.
  line="$(LC_ALL=C grep -F "$manifest	" "$TMP/lib_features.tsv" | head -1)"
  [ -n "$line" ] || continue
  n_feature_ruled=$((n_feature_ruled + 1))
  feats="${line#*	}"
  case ",$feats," in
    *,test-helpers,*)
      echo "" >&2
      echo "  x ${manifest#"$ROOT"/} SHIPS WITH test-helpers ON." >&2
      echo "    features resolved: [$feats]" >&2
      echo "    That feature gates test scaffolding - DbBinding::cold_start," >&2
      echo "    the PgSqlExecutor raw-pool escape hatch, the reset_*_for_tests" >&2
      echo "    helpers - and a release binary must not contain it. If a" >&2
      echo "    production path needs something behind that gate, UNGATE THAT" >&2
      echo "    ITEM; do not turn the feature on for the shipped build." >&2
      fail=1
      ;;
  esac
done < "$TMP/feature_members.txt"
gate_arm test_helpers_off "$n_feature_ruled" "$MIN_FEATURE_MEMBERS" || fail=1

gate_arms_finish || fail=1

if [ "$fail" -ne 0 ]; then
  echo "" >&2
  echo "SHIPPED CONFIG GATE: FAILED" >&2
  exit 1
fi
echo "SHIPPED CONFIG GATE: ok ($n_built shipped target(s) compiled with default features)"
