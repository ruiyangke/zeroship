#!/usr/bin/env bash
# Compile shipped library and binary targets with ordinary feature resolution.
# Audit Cargo artifacts against metadata and reject test-only feature declarations.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init shipped_config

# ---------------------------------------------------------------------------
# Floors guard target enumeration and workspace feature declarations.
MIN_TARGETS=30
MIN_BUILT=30
MIN_FEATURE_MEMBERS=10

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

# Feature posture rules on every member, including crates without features.
jq -r '.packages[].manifest_path' "$TMP/meta.json" | LC_ALL=C sort -u > "$TMP/feature_members.txt"

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
# Feature-only fixture APIs and opt-in live tests must not return.
n_feature_ruled="$(wc -l < "$TMP/feature_members.txt")"
forbidden="$(jq -r '.packages[] | select(.features | has("test-helpers") or has("live-db-tests")) | .manifest_path' "$TMP/meta.json")"
if [ -n "$forbidden" ]; then
  echo "  x workspace members declare forbidden test features:" >&2
  echo "$forbidden" >&2
  fail=1
fi
gate_arm test_features_absent "$n_feature_ruled" "$MIN_FEATURE_MEMBERS" || fail=1

gate_arms_finish || fail=1

if [ "$fail" -ne 0 ]; then
  echo "" >&2
  echo "SHIPPED CONFIG GATE: FAILED" >&2
  exit 1
fi
echo "SHIPPED CONFIG GATE: ok ($n_built shipped target(s) compiled with default features)"
