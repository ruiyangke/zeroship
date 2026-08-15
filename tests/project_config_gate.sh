#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# `zeroship.jsonc` has TWO readers and ONE scope invariant. This gate is what
# makes both survivable.
#
#   Two readers: `crates/cli/src/project_config/` (Rust) and
#   `sdks/vite-plugin/src/project-config/` (TypeScript). Both are generated
#   from `schema/project-v1.json`, which holds every default. The failure mode
#   that survives generation is DIVERGENT DEFAULTS -- and one layer up, that is
#   the exact bug `zeroship.jsonc` was built to remove: four independent
#   derivations of `migrations.out`.
#
#   One scope invariant: the file is read by the CLI and the
#   build, NEVER by the runtime, and is NEVER packed into a `.zship`. It holds
#   today BY CONSTRUCTION -- the packer walks only `distDir` and the file lives
#   one level above it -- which is exactly why it will erode quietly. A `dist/`
#   that is the project root in some static configuration, a `copyPublicDir`
#   step, or a well-meaning "the gateway could read the resource tree directly"
#   all break it without touching a line anyone would think to review.
#
# SIX CHECKS. 1-3 are the two-parser half, 4-6 the invariant half.
#
#   1. codegen drift        both generated files match schema/project-v1.json
#   2. no Rust defaults     every schema `default` is in the TS reader and in
#                           NO Rust reader; the CLI-read intersection is empty
#   3. round trip           both readers dump the same fixture byte-for-byte,
#                           at the root AND under --env=staging; plus a
#                           MUTATION showing the asymmetry is real
#   4. not packed           a sentinel in a root zeroship.jsonc reaches no byte
#                           of the archive -- WITH the one-variable control
#                           that plants it in dist/ and requires a FIND
#   5. no runtime parser    no runtime-side crate names the file or the module
#   6. parser confined      the JSONC reader exists in crates/cli and nowhere
#                           else, and no other crate declares a JSONC dep
#
# CHECK 4 IS THE ONE THAT MATTERS. 5 and 6 are cheap defence in depth against
# defeating 4 by indirection.
#
# WHAT THIS GATE DOES NOT CATCH, stated so its greenness is not overread:
#
#   - It compares the two readers on ONE fixture. A construct the fixture does
#     not contain (a deeply nested environment override, a numeric value where
#     a string is expected) is uncompared. The fixture is written to be
#     awkward on purpose; it is not exhaustive.
#   - Check 4 packs a synthetic app through `emitZship`. It does not run a real
#     `vite build`, so a future leak introduced by a Vite hook that copies
#     files into `dist/` before the packer runs would be FOUND by this check
#     only if that hook also ran here -- and it would not.
#   - Checks 5 and 6 are source greps. They cannot see a filename assembled at
#     runtime (`format!("zeroship.{ext}")`), and this repo has shipped that
#     exact blind spot before.
#   - Nothing here proves the runtime BEHAVES correctly without the file. It
#     proves the file is not present and not read.
#
# Runs without Postgres and without a six-binary build: it needs a built
# `zeroship` binary and node.
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1

BIN="${ZEROSHIP_BIN:-$ROOT/target/release/zeroship}"
[ -x "$BIN" ] || BIN="$ROOT/target/debug/zeroship"
FIXTURE="$ROOT/tests/fixtures/project-config/zeroship.jsonc"
SCHEMA="$ROOT/schema/project-v1.json"
TS_DUMP="$ROOT/sdks/vite-plugin/scripts/project-config-dump.ts"
PROBE="$ROOT/tests/lib/project_config_pack_probe.mjs"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }

for f in "$SCHEMA" "$FIXTURE" "$TS_DUMP" "$PROBE"; do
  [ -f "$f" ] || { echo "FAIL: missing $f"; exit 1; }
done
if [ ! -x "$BIN" ]; then
  echo "FAIL: no zeroship binary at $BIN (cargo build -p zeroship, or set ZEROSHIP_BIN)"
  exit 1
fi
NODE_RUN=(node --import tsx)

echo "== 1. codegen drift =="
if node "$ROOT/schema/codegen.mjs" --check >"$WORK/codegen.log" 2>&1; then
  pass "both generated readers match $(basename "$SCHEMA")"
else
  fail "generated readers drifted from the schema"
  sed 's/^/       /' "$WORK/codegen.log"
fi

echo
echo "== 2. no Rust defaults for cross-tool facts =="
# The schema's `default` set, read from the schema rather than from a list
# typed here -- a gate whose expectations are hand-maintained is the thing it
# is meant to prevent.
DEFAULT_PATHS="$(node -e '
const s = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
const out = [];
const deref = (n) => n && n.$ref ? { ...s.$defs[n.$ref.split("/").pop()], ...n } : n;
const walk = (node, path) => {
  for (const [k, raw] of Object.entries(node.properties ?? {})) {
    const c = deref(raw);
    const p = path ? path + "." + k : k;
    if (c.default !== undefined) out.push(p + "\t" + JSON.stringify(c.default));
    if (c.type === "object") walk(c, p);
  }
};
walk(s, "");
process.stdout.write(out.join("\n") + "\n");
' "$SCHEMA")"
N_DEFAULTS=$(printf '%s\n' "$DEFAULT_PATHS" | grep -c . || true)
if [ "$N_DEFAULTS" -lt 3 ]; then
  fail "extracted $N_DEFAULTS defaults from the schema - the extractor is broken, not the code"
else
  ts_missing=""; rs_missing=""
  while IFS=$'\t' read -r path lit; do
    [ -n "$path" ] || continue
    grep -qF -- "\"$path\": $lit" sdks/vite-plugin/src/project-config/generated.ts \
      || ts_missing="$ts_missing $path"
    # The Rust reader must NAME every defaulted path and BIND none of them.
    # Naming is what makes the absence a decision rather than an omission; the
    # binding is what check 3 tests behaviourally, because a grep for a literal
    # here cannot tell a default value from an enum member (`"full"`) or a key
    # name (`"dist"`), and an earlier draft of this check failed on exactly
    # that confusion.
    grep -qF -- "\"$path\"" crates/cli/src/project_config/generated.rs \
      || rs_missing="$rs_missing $path"
  done <<< "$DEFAULT_PATHS"

  [ -z "$ts_missing" ] \
    && pass "all $N_DEFAULTS schema defaults are in the TypeScript reader with their literal" \
    || fail "TypeScript reader is missing defaults:$ts_missing"
  [ -z "$rs_missing" ] \
    && pass "the Rust reader NAMES all $N_DEFAULTS defaulted paths (SCHEMA_DEFAULTED_FIELDS)" \
    || fail "the Rust reader does not name defaulted paths:$rs_missing"
fi

echo
echo "== 3. round trip: the two readers agree byte for byte =="
for ENVSEL in "" "staging"; do
  label="root"; envflag=()
  if [ -n "$ENVSEL" ]; then label="--env=$ENVSEL"; envflag=("--env=$ENVSEL"); fi
  ( cd "$ROOT/sdks/vite-plugin" && "${NODE_RUN[@]}" "$TS_DUMP" "$FIXTURE" "${envflag[@]+"${envflag[@]}"}" ) \
    >"$WORK/ts.json" 2>"$WORK/ts.err"
  ts_rc=$?
  ( cd "$(dirname "$FIXTURE")" && "$BIN" config show "${envflag[@]+"${envflag[@]}"}" ) \
    >"$WORK/rs.json" 2>"$WORK/rs.err"
  rs_rc=$?
  if [ "$ts_rc" != 0 ] || [ "$rs_rc" != 0 ]; then
    fail "a reader REFUSED the fixture at $label (ts=$ts_rc rs=$rs_rc) - that is not a disagreement, it is a broken reader"
    sed 's/^/       ts: /' "$WORK/ts.err"; sed 's/^/       rs: /' "$WORK/rs.err"
  elif [ ! -s "$WORK/ts.json" ] || [ ! -s "$WORK/rs.json" ]; then
    fail "a reader produced an EMPTY dump at $label - two empty files compare equal, which would be a fake pass"
  elif diff -q "$WORK/ts.json" "$WORK/rs.json" >/dev/null; then
    pass "TypeScript and Rust resolve the fixture identically at $label ($(wc -c <"$WORK/ts.json") bytes)"
  else
    fail "the two readers DISAGREE at $label"
    diff "$WORK/rs.json" "$WORK/ts.json" | sed 's/^/       /' | head -20
  fi
done

# MUTATION A. Remove a cross-tool key from a file that exists. NEITHER reader
# may quietly supply a value: the schema requires it, so both must refuse and
# both must name it. A run where one of them answered with a path would be the
# original four-derivations bug, moved up one layer.
MUT="$WORK/mutant"; mkdir -p "$MUT"
sed 's|"out": "generated/zeroship",||' "$FIXTURE" >"$MUT/zeroship.jsonc"
if grep -q '"out": "generated/zeroship"' "$MUT/zeroship.jsonc"; then
  fail "mutation A did not apply - a setup failure and a proved hypothesis print the same green"
else
  ( cd "$ROOT/sdks/vite-plugin" && "${NODE_RUN[@]}" "$TS_DUMP" "$MUT/zeroship.jsonc" ) \
    >"$WORK/mut-ts.json" 2>"$WORK/mut-ts.err"
  mut_ts_rc=$?
  ( cd "$MUT" && "$BIN" config show ) >"$WORK/mut-rs.json" 2>"$WORK/mut-rs.err"
  mut_rs_rc=$?
  if [ "$mut_ts_rc" != 0 ] && grep -q "migrations.out" "$WORK/mut-ts.err"; then
    pass "MUTATION A: a file that omits migrations.out is refused by the TypeScript reader, naming the key"
  else
    fail "the TypeScript reader ACCEPTED a file with no migrations.out (rc=$mut_ts_rc) - it guessed"
  fi
  if [ "$mut_rs_rc" != 0 ] && grep -q "migrations.out" "$WORK/mut-rs.err"; then
    pass "MUTATION A: the same file is refused by the Rust reader, naming the key"
  else
    fail "the Rust reader ACCEPTED a file with no migrations.out (rc=$mut_rs_rc) - a fallback has come back"
  fi
fi

# MUTATION B. THE ASYMMETRY, and the one-variable pair for the no-default rule:
# an EMPTY directory. Same command, same key, and the only thing that changes
# is whether a config file exists.
#
#   TypeScript -> the schema defaults, because the plugin must work with
#                 `zeroship()` and no file at all. That is the scaffold's
#                 `vite.config.ts` today.
#   Rust       -> refuses, because a CLI that guessed a control URL would
#                 deploy somewhere the creator never named.
#
# Both answering the same way is the failure, in either direction.
EMPTY="$WORK/empty"; mkdir -p "$EMPTY"
( cd "$ROOT/sdks/vite-plugin" && "${NODE_RUN[@]}" "$TS_DUMP" "--root=$EMPTY" ) \
  >"$WORK/empty-ts.json" 2>"$WORK/empty-ts.err"
empty_ts_rc=$?
( cd "$EMPTY" && "$BIN" config show ) >"$WORK/empty-rs.json" 2>"$WORK/empty-rs.err"
empty_rs_rc=$?
if [ "$empty_ts_rc" = 0 ] && grep -q '"out":"generated/zeroship"' "$WORK/empty-ts.json"; then
  pass "MUTATION B: with NO file, the TypeScript reader supplies the schema defaults"
else
  fail "with no file the TypeScript reader did not default (rc=$empty_ts_rc): $(head -c 200 "$WORK/empty-ts.err")"
fi
if [ "$empty_rs_rc" != 0 ] && grep -qF "$(basename "$FIXTURE")" "$WORK/empty-rs.err"; then
  pass "MUTATION B: with NO file, the Rust reader refuses and names the file it wanted"
else
  fail "with no file the Rust reader produced a config (rc=$empty_rs_rc) - it has defaults"
fi

echo
echo "== 4. the scope invariant: zeroship.jsonc is never packed =="
SENTINEL="zsprojectcfg$(date +%s)$$"
probe() {
  ( cd "$ROOT/sdks/vite-plugin" && "${NODE_RUN[@]}" "$PROBE" "--sentinel=$SENTINEL" "--plant=$1" ) \
    2>"$WORK/probe-$1.err"
}

P_NONE="$(probe none)"; P_ROOT="$(probe root)"; P_DIST="$(probe dist)"
P_UNSAFE="$(
  cd "$ROOT/sdks/vite-plugin" &&
    "${NODE_RUN[@]}" "$PROBE" "--sentinel=$SENTINEL" --plant=root --dist=.
)" 2>"$WORK/probe-unsafe.err"
ran=1
for arm in none root dist; do
  case "$arm" in none) v="$P_NONE" ;; root) v="$P_ROOT" ;; dist) v="$P_DIST" ;; esac
  case "$v" in
    FOUND|ABSENT) ;;
    *) ran=0
       fail "probe --plant=$arm did not answer (got '$v') - the instrument did not run, which is not a result"
       sed 's/^/       /' "$WORK/probe-$arm.err" | head -8 ;;
  esac
done

case "$P_UNSAFE" in
  REJECTED) ;;
  *) ran=0
     fail "probe --dist=. did not report a safe rejection (got '$P_UNSAFE')"
     sed 's/^/       /' "$WORK/probe-unsafe.err" | head -8 ;;
esac

if [ "$ran" = "1" ]; then
  # The CONTROL first: a sentinel that IS inside dist/ must be found. A search
  # that cannot find a present sentinel proves nothing about an absent one, and
  # that is the whole reason this arm exists.
  [ "$P_DIST" = "FOUND" ] \
    && pass "CONTROL: a sentinel planted inside dist/ IS found in the archive bytes" \
    || fail "CONTROL FAILED: a sentinel inside dist/ was NOT found - the search is broken, so ABSENT below means nothing"

  [ "$P_NONE" = "ABSENT" ] \
    && pass "with no sentinel anywhere, the search reports ABSENT (it is not stuck on FOUND)" \
    || fail "the search reported FOUND with no sentinel planted - it is not discriminating"

  [ "$P_UNSAFE" = "REJECTED" ] \
    && pass "build.dist=. is rejected before an archive can contain zeroship.jsonc" \
    || fail "build.dist=. was packed instead of rejected"

  if [ "$P_DIST" = "FOUND" ] && [ "$P_NONE" = "ABSENT" ]; then
    [ "$P_ROOT" = "ABSENT" ] \
      && pass "a sentinel in <root>/zeroship.jsonc reaches NO byte of the .zship (one variable from the control: the directory)" \
      || fail "zeroship.jsonc CONTENT reached the .zship - the scope invariant is broken"
  else
    fail "not evaluating the root arm: the control did not discriminate, so ABSENT would be unearned"
  fi
fi

echo
echo "== 5. no runtime-side parser =="
RUNTIME_DIRS=(crates/runtime crates/worker crates/gateway)
for d in crates/plugin-*; do RUNTIME_DIRS+=("$d"); done
hits=""
for d in "${RUNTIME_DIRS[@]}"; do
  [ -d "$d" ] || continue
  # `zeroship.jsonc` (the filename) or `project_config` (the module).
  h="$(grep -rnE 'zeroship\.jsonc|project_config' "$d" --include='*.rs' --include='*.toml' 2>/dev/null || true)"
  [ -n "$h" ] && hits="$hits$h"$'\n'
done
if [ -z "$hits" ]; then
  pass "no runtime-side crate (${#RUNTIME_DIRS[@]} scanned) names the config file or its module"
else
  fail "a runtime-side crate references the creator project config:"
  printf '%s' "$hits" | sed 's/^/       /' | head -10
fi

echo
echo "== 6. the JSONC parser is confined to crates/cli =="
PARSERS="$(grep -rl 'fn strip' crates --include='jsonc.rs' 2>/dev/null || true)"
N_PARSERS=$(printf '%s\n' "$PARSERS" | grep -c . || true)
if [ "$N_PARSERS" != "1" ]; then
  fail "expected exactly one JSONC parser under crates/, found $N_PARSERS - the extractor or the confinement is wrong"
  printf '%s\n' "$PARSERS" | sed 's/^/       /'
elif [ "$PARSERS" = "crates/cli/src/project_config/jsonc.rs" ]; then
  pass "the only JSONC parser under crates/ is $PARSERS"
else
  fail "the JSONC parser moved to $PARSERS"
fi

# And no crate has taken a JSONC dependency, which is how (5) would be defeated
# by indirection. There is no such dependency ANYWHERE today: the parser is a
# module, which is a stronger form of "confined" than one Cargo.toml.
DEPS="$(grep -rn 'jsonc\|json5\|json_spanned' --include='Cargo.toml' crates libs 2>/dev/null || true)"
if [ -z "$DEPS" ]; then
  pass "no crate declares a JSONC parsing dependency"
else
  fail "a crate declares a JSONC parsing dependency:"
  printf '%s\n' "$DEPS" | sed 's/^/       /'
fi

echo
echo "----------------------------------------------------------------------"
echo "  passed: $PASS   failed: $FAIL"
[ "$FAIL" -eq 0 ] || exit 1
