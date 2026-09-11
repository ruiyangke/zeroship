#!/usr/bin/env bash
# ============================================================================
# THE .node MUST RESOLVE THE NODE ABI FROM ITS HOST, AND ONLY THE TEST BINARIES
# MAY RESOLVE IT THROUGH libloading.
#
# One manifest knob decides both, and getting it wrong is silent in one
# direction and loud-but-misattributed in the other.
#
# ---------------------------------------------------------------------------
# THE DEFECT THIS EXISTS FOR
# ---------------------------------------------------------------------------
#
# Measured at 1144d787c, before the fix this gate guards:
#
#     $ cargo test -p zeroship-migrate-node --no-run
#     exit 101, 1719 "undefined reference" lines, the first:
#       crates/zeroship-migrate-node/src/bridge.rs:1222: undefined reference
#       to `napi_create_function'   (in `_napi_rs_internal_register_status')
#
# `napi_*` symbols are exported by the Node host process and resolve only when
# a `.node` is dlopen'd. A cdylib may leave them undefined; a test EXECUTABLE
# may not. Since `napi` is a default feature, a bare `cargo test` - and a bare
# workspace `cargo test`, since this package is a full default member - selected
# a package that could not link. `crates/zeroship-migrate-node/Cargo.toml` now
# carries `napi` in `[dev-dependencies]` with `dyn-symbols`, which swaps
# napi-sys's extern block for a libloading-populated pointer table. Under
# resolver v3 a dev-dependency's features unify into the normal edge ONLY when a
# test target is in the unit graph, so `cargo test` gets it and `--lib`/`--bins`
# does not.
#
# ---------------------------------------------------------------------------
# WHY A GATE, AND NOT JUST "cargo test passes"
# ---------------------------------------------------------------------------
#
# Because the two ways of getting this wrong fail in opposite directions:
#
#   THE WRONG REPAIR IS SILENT. Moving `dyn-symbols` onto the `[dependencies]`
#   napi entry, or routing it through this crate's own `[features]`, compiles
#   perfectly and makes `cargo test` pass. It also puts libloading into the
#   SHIPPED `.node`, which would then resolve N-API through a dlopen of its own
#   host instead of against the symbols Node already exported. Nothing in the
#   Rust build says a word about it. Arms 1 and 2 are what refuse it.
#
#   THE MISSING DEV-DEPENDENCY IS LOUD BUT MISATTRIBUTED. Delete it and you get
#   the 1719-line linker wall above, which names a symbol and a source line and
#   says nothing about the manifest. Arm 3 goes red naming the cause.
#
# ---------------------------------------------------------------------------
# THE THREE ARMS, and what each one alone would miss
# ---------------------------------------------------------------------------
#
# 1. `shipped_symbol_shape` - THE ARTIFACT. Builds the shipped `--lib`
#    configuration and reads the emitted cdylib's dynamic symbol table. Two
#    halves, and both are needed:
#      (a) `napi_create_function` must be UNDEFINED (` U `). If it is absent,
#          dyn-symbols reached the shipped object and the `.node` would carry
#          its own loader.
#      (b) `napi_register_module_v1` must be DEFINED (` T `). If it is absent
#          the object cannot self-register and `require()` fails with a message
#          blaming npm optional dependencies - a diagnostic that sends the
#          reader to their lockfile rather than to this manifest.
#    Arm 2 cannot see (b) at all: an object can resolve features correctly and
#    still fail to export its registrar.
#
# 2. `shipped_features_no_dyn` - THE MECHANISM, shipped side. Asserts no
#    napi-family unit in the default `--lib` build resolved `dyn-symbols`. Arm 1
#    reads a consequence; this reads the cause, and names it. Same shape and
#    same purpose as tests/shipped_config_gate.sh arm 3.
#
# 3. `test_features_have_dyn` - THE MECHANISM, test side. Asserts every
#    napi-family unit in the `cargo test --no-run` graph DOES resolve it. This
#    is the arm that goes red the day somebody "tidies away" the dev-dependency,
#    and it is this change's regression test.
#
# ---------------------------------------------------------------------------
# WHAT THIS DOES NOT RULE ON - read this before trusting arm 1 too far
# ---------------------------------------------------------------------------
#
# A TRANSIENTLY WRONG .so ON DISK. `cargo build -p zeroship-migrate-node --lib
# --all-targets` unifies the dev-dependency feature into the cdylib and leaves
# an object with ZERO undefined `napi_*` symbols in target/. THIS GATE DOES NOT
# DETECT THAT STATE, and it would be dishonest to imply otherwise: arm 1 runs
# its own `--lib` build first, whose feature resolution differs, so cargo
# REBUILDS the object correctly and the arm then reads a good one. The arm
# corrects the leak rather than catching it.
#
# That is the right trade and not a gap being excused. A gate that skipped the
# build to inspect whatever object happened to be lying there would be ruling on
# the last command somebody typed, which is not reproducible from its own
# invocation. What arm 1 does rule on is the DURABLE version of the same defect
# - a manifest that puts dyn-symbols in the shipped configuration - and that is
# the one that survives a commit. Do not add `--all-targets` to any build of
# this crate; nothing here will tell you that you did.
#
# THE .node ITSELF. `napi build` is not run here. This rules on the cdylib cargo
# emits, which is the object `napi build` wraps, not the wrapper.
#
# WHETHER N-API ACTUALLY WORKS. No Rust test may CALL an N-API function: with no
# host loaded the napi-sys stub prints `Node-API symbol ... has not been loaded`
# and returns a value that can read as success. That boundary is `npm test`'s
# question, through a real `.node`.
#
# NON-ELF PLATFORMS. `nm -D` reads ELF. On macOS/Windows arm 1 refuses rather
# than guesses; arms 2 and 3 are platform-neutral.
#
# Run:  tests/napi_symbol_shape_gate.sh
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init napi_symbol_shape

PKG=zeroship-migrate-node

# ---------------------------------------------------------------------------
# THE FLOORS. Measured 2026-09-04 by running this gate:
#
#   shipped_symbol_shape   52  (undefined `napi_*` dynamic symbols in the
#                               shipped cdylib)
#   shipped_features_no_dyn 3  (napi build-script, napi lib, napi-sys lib)
#   test_features_have_dyn  3  (the same three, in the test graph)
#
# Arm 1's floor is set well under 52: the count moves whenever bridge.rs uses a
# new N-API call, and a floor near today's number would go red on ordinary
# editing. Reaching single digits means the extern block was replaced, which is
# the defect. Arms 2 and 3 take floor 1 - their job is to refuse a stream that
# contained no napi unit at all, which would otherwise pass vacuously.
# ---------------------------------------------------------------------------
MIN_UNDEF_SYMBOLS=30
MIN_SHIPPED_UNITS=1
MIN_TEST_UNITS=1

fail=0
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

command -v cargo >/dev/null 2>&1 || {
  echo "  x REFUSED: cargo is not on PATH. Every set here comes from cargo's" >&2
  echo "             own resolution; without it this would inspect nothing" >&2
  echo "             and exit 0." >&2
  exit 1
}
command -v jq >/dev/null 2>&1 || {
  echo "  x REFUSED: jq is not on PATH; this gate cannot read cargo's json." >&2
  exit 1
}
command -v nm >/dev/null 2>&1 || {
  echo "  x REFUSED: nm is not on PATH (binutils). Arm 1 rules on the emitted" >&2
  echo "             cdylib's dynamic symbol table and cannot be skipped -" >&2
  echo "             skipping it is indistinguishable from passing it." >&2
  exit 1
}

# ---------------------------------------------------------------------------
# The napi-family unit filter, shared by arms 2 and 3 so the two sides cannot
# drift apart. The package NAME is extracted from the package_id rather than
# substring-matched: `napi-build`, `napi-derive` and `napi-derive-backend` are
# different packages that declare no `dyn-symbols` at all, and counting them
# would inflate both arms' `examined` with units that can never fail.
# ---------------------------------------------------------------------------
NAPI_UNITS='
  select(.reason == "compiler-artifact")
  | (.package_id | sub("^.*#"; "") | sub("@.*$"; "")) as $name
  | select($name == "napi" or $name == "napi-sys")
  | [$name, .target.name, ((.features // []) | join(","))]
  | @tsv
'

# ---------------------------------------------------------------------------
# THE SHIPPED BUILD. `--lib` and nothing else. Adding any test target here would
# put the dev-dependency back in the unit graph and make arms 1 and 2 rule on
# the configuration they exist to distinguish this one FROM.
# ---------------------------------------------------------------------------
echo "  - building: cargo build -p $PKG --lib (default features)"
(cd "$ROOT" && cargo build -p "$PKG" --lib --message-format=json) \
  > "$TMP/lib.json" 2> "$TMP/lib.err"
lib_status=$?
if [ "$lib_status" -ne 0 ]; then
  echo "  x REFUSED: the shipped --lib build failed (exit $lib_status). Nothing" >&2
  echo "             below can rule on an artifact that was never emitted." >&2
  tail -30 "$TMP/lib.err" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# ARM 1 - the artifact.
# ---------------------------------------------------------------------------
# The path comes from THIS build's artifact record, never a hardcoded
# target/debug/. CARGO_TARGET_DIR, a workspace target dir and the platform
# suffix all move it, and a hardcoded path that stopped existing would send this
# arm down the "missing prerequisite" branch while the real object sat elsewhere.
CDYLIB="$(jq -r '
  select(.reason == "compiler-artifact")
  | select(.target.kind | any(. == "cdylib"))
  | .filenames[]
  | select(endswith(".so"))
' "$TMP/lib.json" 2>/dev/null | head -1)"

if [ -z "$CDYLIB" ]; then
  echo "  x REFUSED: cargo emitted no .so cdylib artifact for $PKG." >&2
  echo "             Either the crate stopped declaring crate-type cdylib, or" >&2
  echo "             this is not an ELF platform. Arm 1 rules on a dynamic" >&2
  echo "             symbol table and has nothing to read." >&2
  exit 1
fi
if [ ! -f "$CDYLIB" ]; then
  echo "  x REFUSED: cargo named $CDYLIB but it is not on disk." >&2
  exit 1
fi
echo "  - cdylib: ${CDYLIB#"$ROOT"/}"

nm -D "$CDYLIB" > "$TMP/syms.txt" 2> "$TMP/nm.err"
if [ ! -s "$TMP/syms.txt" ]; then
  echo "  x REFUSED: nm -D produced no output for $CDYLIB." >&2
  tail -5 "$TMP/nm.err" >&2
  exit 1
fi

n_undef="$(grep -c ' U napi_' "$TMP/syms.txt")" || n_undef=0

# (a) the host-resolved half.
if ! grep -q ' U napi_create_function$' "$TMP/syms.txt"; then
  echo "" >&2
  echo "  x THE SHIPPED CDYLIB DOES NOT LEAVE napi_create_function UNDEFINED." >&2
  echo "    Undefined napi_* symbols found: $n_undef" >&2
  echo "    The Node ABI is supposed to be resolved BY THE HOST at .node dlopen" >&2
  echo "    time. If that symbol is not undefined, napi/dyn-symbols leaked into" >&2
  echo "    the shipped build and the addon now carries a libloading table of" >&2
  echo "    its own. The cause is almost always dyn-symbols moved onto the" >&2
  echo "    [dependencies] napi entry, or routed through this crate's" >&2
  echo "    [features]. It belongs on the [dev-dependencies] entry and nowhere" >&2
  echo "    else. See arm 2, which names the unit." >&2
  fail=1
fi

# (b) the self-registration half.
if ! grep -qE ' T napi_register_module_v1$' "$TMP/syms.txt"; then
  echo "" >&2
  echo "  x THE SHIPPED CDYLIB DOES NOT DEFINE napi_register_module_v1." >&2
  echo "    Node calls that symbol to register the addon. Without it," >&2
  echo "    require() of the built .node fails with a message about missing" >&2
  echo "    npm optional dependencies, which sends the reader to their" >&2
  echo "    lockfile instead of to this crate." >&2
  fail=1
fi

gate_arm shipped_symbol_shape "$n_undef" "$MIN_UNDEF_SYMBOLS" || fail=1

# ---------------------------------------------------------------------------
# ARM 2 - the mechanism, shipped side.
# ---------------------------------------------------------------------------
jq -r "$NAPI_UNITS" "$TMP/lib.json" 2>/dev/null \
  | LC_ALL=C sort -u > "$TMP/shipped_units.tsv"

n_shipped_units="$(grep -c . "$TMP/shipped_units.tsv")" || n_shipped_units=0
while IFS=$'\t' read -r pkg target feats; do
  [ -n "$pkg" ] || continue
  case ",$feats," in
    *,dyn-symbols,*)
      echo "" >&2
      echo "  x $pkg ($target) SHIPS WITH dyn-symbols ON." >&2
      echo "    features resolved: [$feats]" >&2
      echo "    That replaces the Node ABI extern block with a libloading" >&2
      echo "    pointer table IN THE SHIPPED .node. It is for test binaries" >&2
      echo "    only. Declare it on the [dev-dependencies] napi entry; do NOT" >&2
      echo "    add it to [dependencies] and do NOT route it through this" >&2
      echo "    crate's [features] (xtask/tests/repository_architecture.rs)." >&2
      fail=1
      ;;
  esac
done < "$TMP/shipped_units.tsv"
gate_arm shipped_features_no_dyn "$n_shipped_units" "$MIN_SHIPPED_UNITS" || fail=1

# ---------------------------------------------------------------------------
# ARM 3 - the mechanism, test side.
# ---------------------------------------------------------------------------
echo "  - building: cargo test -p $PKG --no-run"
(cd "$ROOT" && cargo test -p "$PKG" --no-run --message-format=json) \
  > "$TMP/test.json" 2> "$TMP/test.err"
test_status=$?
if [ "$test_status" -ne 0 ]; then
  echo "" >&2
  echo "  x cargo test -p $PKG --no-run FAILED (exit $test_status)." >&2
  echo "    If this is a wall of 'undefined reference to napi_*', the" >&2
  echo "    [dev-dependencies] napi entry carrying \"dyn-symbols\" is gone from" >&2
  echo "    crates/$PKG/Cargo.toml. Node ABI symbols resolve only at .node" >&2
  echo "    dlopen time; that entry is what lets a test binary link at all." >&2
  tail -15 "$TMP/test.err" >&2
  fail=1
fi

jq -r "$NAPI_UNITS" "$TMP/test.json" 2>/dev/null \
  | LC_ALL=C sort -u > "$TMP/test_units.tsv"

n_test_units="$(grep -c . "$TMP/test_units.tsv")" || n_test_units=0
while IFS=$'\t' read -r pkg target feats; do
  [ -n "$pkg" ] || continue
  case ",$feats," in
    *,dyn-symbols,*) ;;
    *)
      echo "" >&2
      echo "  x $pkg ($target) BUILT FOR TESTS WITHOUT dyn-symbols." >&2
      echo "    features resolved: [$feats]" >&2
      echo "    Without it the test binaries link the Node ABI statically and" >&2
      echo "    fail with 'undefined reference to napi_create_function'." >&2
      echo "    Restore the napi entry in [dev-dependencies] of" >&2
      echo "    crates/$PKG/Cargo.toml with \"dyn-symbols\" in its features." >&2
      fail=1
      ;;
  esac
done < "$TMP/test_units.tsv"
gate_arm test_features_have_dyn "$n_test_units" "$MIN_TEST_UNITS" || fail=1

gate_arms_finish || fail=1

if [ "$fail" -ne 0 ]; then
  echo "" >&2
  echo "NAPI SYMBOL SHAPE GATE: FAILED" >&2
  exit 1
fi
echo "NAPI SYMBOL SHAPE GATE: ok ($n_undef undefined napi_* symbol(s) in the" \
     "shipped cdylib; $n_shipped_units unit(s) without dyn-symbols," \
     "$n_test_units with it)"
