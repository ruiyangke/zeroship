#!/usr/bin/env bash
# Self-test for tests/clippy_gate.sh.
#
# The gate exists because a run that STOPPED EARLY and a run that FOUND NOTHING
# print the same thing. Every case below is one of the ways those two can be
# confused, and the middle group is the whole point:
#
#   a deny-level lint MUST be red             - else the gate is decoration
#   a crate that was never reached MUST be
#     red, and MUST NOT read as clean         - this is the failure that hid a
#                                               red main for days
#   a crate that could not COMPILE must say
#     so, distinctly from a crate that
#     failed a lint                           - one means "fix the code", the
#                                               other means "no verdict here is
#                                               valid"
#   a broken instrument MUST NOT read as a
#     clean tree                              - an empty stream and a spotless
#                                               workspace are the same bytes
#   a feature the run did not enable MUST
#     be red and NAMED                        - the same failure one level out:
#                                               a target the run cannot build is
#                                               not "unlinted", it is filtered
#                                               out of the expectation, so the
#                                               coverage numbers balance on a
#                                               workspace the run made smaller
#
# Every case runs against fixtures. None of it builds anything, so this is a
# second-scale check that can run on every commit, unlike the gate itself.
#
# Run directly: tests/clippy_gate_selftest.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GATE="$ROOT/tests/clippy_gate.sh"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail=0
pass=0

# ---------------------------------------------------------------- fixtures
#
# Three workspace packages. `pkg-mike` is deliberately in the MIDDLE
# alphabetically: a gate that only ever reached the first package would still
# look green on a fault planted in the first one, so every planted fault below
# goes into `mike` or `zulu`.
#
# `pkg-zulu` uses the package_id form whose fragment omits the name - the form
# real cargo emits for libs/compio-postgres - so a gate that split the name out
# of the id by hand would mis-key it here.
ID_A='path+file:///w/crates/alpha#pkg-alpha@0.1.0'
ID_M='path+file:///w/crates/mike#pkg-mike@0.1.0'
ID_Z='path+file:///w/libs/pkg-zulu#0.1.0'
ID_EXT='registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0'

cat > "$TMP/metadata.json" <<EOF
{
  "workspace_members": ["$ID_A", "$ID_M", "$ID_Z"],
  "packages": [
    {"id": "$ID_A", "name": "pkg-alpha", "targets": [
      {"name": "pkg-alpha", "kind": ["lib"]},
      {"name": "alpha_it", "kind": ["test"]}
    ]},
    {"id": "$ID_M", "name": "pkg-mike", "features": {"default": [], "live-db-tests": []}, "targets": [
      {"name": "pkg-mike", "kind": ["lib"]},
      {"name": "mike_it", "kind": ["test"]},
      {"name": "mike_live", "kind": ["test"], "required-features": ["live-db-tests"]}
    ]},
    {"id": "$ID_Z", "name": "pkg-zulu", "features": {"cfg-only": []}, "targets": [
      {"name": "pkg-zulu", "kind": ["lib"]}
    ]},
    {"id": "$ID_EXT", "name": "serde", "targets": [
      {"name": "serde", "kind": ["lib"]}
    ]}
  ],
  "resolve": {"nodes": [
    {"id": "$ID_A", "features": []},
    {"id": "$ID_M", "features": ["live-db-tests"]},
    {"id": "$ID_Z", "features": ["cfg-only"]},
    {"id": "$ID_EXT", "features": []}
  ]}
}
EOF

# artifact <pkg_id> <target> <kind>
artifact() {
  printf '{"reason":"compiler-artifact","package_id":"%s","target":{"name":"%s","kind":["%s"]},"fresh":false}\n' "$1" "$2" "$3"
}
# diag <pkg_id> <level> <code|-> <text> <target> <kind>
#
# The target is not decoration. A target that errors emits no artifact, so the
# gate has to know WHICH target an error belongs to before it can tell a failing
# target apart from one that was never scheduled.
diag() {
  local code
  if [ "$3" = "-" ]; then code='null'; else code="{\"code\":\"$3\"}"; fi
  printf '{"reason":"compiler-message","package_id":"%s","target":{"name":"%s","kind":["%s"]},"message":{"level":"%s","code":%s,"message":"%s","rendered":"%s\\n"}}\n' \
    "$1" "$5" "$6" "$2" "$code" "$4" "$4"
}

# The complete, everything-linted stream.
full_stream() {
  artifact "$ID_A" pkg-alpha lib
  artifact "$ID_A" alpha_it test
  artifact "$ID_M" pkg-mike lib
  artifact "$ID_M" mike_it test
  artifact "$ID_M" mike_live test
  artifact "$ID_Z" pkg-zulu lib
  artifact "$ID_EXT" serde lib
}

# run <json-file> [metadata-file] -> sets RC and OUT
#
# THE FIXTURE DECLARES ITS OWN BOUNDS. Until 2026-08-20 the gate's arms carried
# constants sized to THIS fixture - 3 members, 4 expected targets, 1 include_str!
# literal - because every case here drives them through the same code path, and
# a floor near the real workspace's 30/146/239 would have failed the self-test
# instead of a broken tree. That made the self-test set the bound the real tree
# was held to, and the real tree passed while ruling on a tenth of itself.
#
# So the corpus and its bounds travel together, on argv, declared here per call.
# `--min-targets 5` is the number case 6 deliberately resolves down to; if a
# target is later deleted from the fixture this goes red, which is correct - the
# fixture changed and its declaration must change with it. `--min-features 2` is
# the two non-`default` features the fixture declares: `pkg-mike/live-db-tests`
# (which gates a target) and `pkg-zulu/cfg-only` (which gates none, the shape
# plugin-db's `test-helpers` has - 290 cfg sites inside an already-linted lib).
run() {
  OUT="$("$GATE" --audit-only "$1" \
           --metadata "${2:-$TMP/metadata.json}" \
           --min-members 3 --min-targets 5 --min-features 2 2>&1)"
  RC=$?
}

# preflight <src-root> -> sets RC and OUT. Two literals is what fixture_src
# holds; the empty root is the same declaration over a corpus that offers none.
preflight() {
  OUT="$("$GATE" --preflight-only --src-roots "$1" --min-literals 2 2>&1)"
  RC=$?
}

check() {
  local name="$1" want_rc="$2"; shift 2
  if [ "$RC" != "$want_rc" ]; then
    echo "FAIL: $name - exit $RC, wanted $want_rc"
    printf '%s\n' "$OUT" | sed 's/^/       /'
    fail=$((fail + 1))
    return
  fi
  local pat
  for pat in "$@"; do
    if ! printf '%s\n' "$OUT" | grep -qF -- "$pat"; then
      echo "FAIL: $name - output does not mention: $pat"
      printf '%s\n' "$OUT" | sed 's/^/       /'
      fail=$((fail + 1))
      return
    fi
  done
  pass=$((pass + 1))
}

# ------------------------------------------------------------------- case 1
# A clean workspace passes, and says how much of it it looked at. The count is
# load-bearing: it is what a reader compares against on the day one of the
# other cases fires for real.
full_stream > "$TMP/clean.json"
run "$TMP/clean.json"
check "clean workspace passes" 0 "linted:   6 targets in 3 packages (expected 6 in 3)"

# ------------------------------------------------------------------- case 2
# A deny-level clippy lint in the MIDDLE package is red, and is reported as a
# lint and NOTHING ELSE.
#
# The failing target emits no artifact - that is what failing means - so a naive
# coverage arm reports it a second time as "unlinted". Two headings for one
# fault, and the second is supposed to mean "we could not see this crate". A
# heading that fires for two different things stops being read as either, which
# is precisely the confusion this gate exists to remove.
{
  full_stream | grep -v 'mike_it'
  diag "$ID_M" error "clippy::disallowed_methods" "use of a disallowed method" mike_it test
} > "$TMP/lint.json"
run "$TMP/lint.json"
check "deny-level lint in a middle package is red" 1 \
  "deny-level clippy lint" "pkg-mike" "clippy::disallowed_methods"
for wrong in "NOT REACHED" "went unlinted"; do
  if printf '%s\n' "$OUT" | grep -q "$wrong"; then
    echo "FAIL: a failing target must not ALSO be reported as '$wrong'"
    printf '%s\n' "$OUT" | sed 's/^/       /'
    fail=$((fail + 1)); pass=$((pass - 1))
  fi
done

# ------------------------------------------------------------------- case 3
# THE FAILURE THIS GATE EXISTS FOR. `pkg-alpha` fails, cargo aborts, and the two
# packages downstream of it are never scheduled - so the stream contains their
# name nowhere at all. A gate that only looked for errors would find exactly one
# and call the other two clean.
{
  artifact "$ID_A" pkg-alpha lib
  diag "$ID_A" error "clippy::needless_borrow" "this expression borrows a value" alpha_it test
} > "$TMP/aborted.json"
run "$TMP/aborted.json"
check "packages never scheduled are named, not counted clean" 1 \
  "NOT REACHED" "pkg-mike" "pkg-zulu" "linted:   1 targets in 1 packages (expected 6 in 3)"

# ------------------------------------------------------------------- case 4
# A rustc error is not a lint. `pkg-zulu` could not be compiled at all, so no
# verdict about it - clean or otherwise - means anything. It must be reported
# under its own heading, because the fix is a different fix.
{
  full_stream
  diag "$ID_Z" error "-" "cannot find type \`Foo\` in this scope" pkg-zulu lib
} > "$TMP/hard.json"
run "$TMP/hard.json"
check "a compile error is reported as could-not-lint, not as a lint" 1 \
  "could not be LINTED AT ALL" "pkg-zulu"
if printf '%s\n' "$OUT" | grep -q "deny-level clippy lint"; then
  echo "FAIL: a rustc error must not be filed as a clippy lint"
  fail=$((fail + 1)); pass=$((pass - 1))
fi

# ------------------------------------------------------------------- case 5
# ARM 2 CANNOT SEE THIS ONE. `pkg-mike` keeps its lib linted, so it is present
# in the artifact stream and every package is accounted for - but one of its
# test targets was never built. Package-level existence passes; only the
# per-target comparison catches it.
full_stream | grep -v 'mike_it' > "$TMP/lost_target.json"
run "$TMP/lost_target.json"
check "a single lost target inside a present package is red" 1 \
  "went unlinted" "mike_it" "stops being linted, silently"
if printf '%s\n' "$OUT" | grep -q "NOT REACHED"; then
  echo "FAIL: pkg-mike still built targets; it is not an unreached package"
  fail=$((fail + 1)); pass=$((pass - 1))
fi

# ------------------------------------------------------------------- case 6
# THE DEFECT ARM 4 EXISTS FOR, and case 1 is its one-variable control: the same
# artifact stream, the same fixture, differing only in whether the resolve node
# enables `live-db-tests`.
#
# With the feature off, `mike_live` leaves the EXPECTED set and the OBSERVED set
# together. Arm 3's comparison stays balanced - correctly, since a target cargo
# cannot build is not a target that went unlinted - and the count silently drops
# from 6 to 5. Until 2026-08-20 that was the whole result: exit 0, "expected 5",
# green. That is how zeroship-migrate-adapter's `platform_migrate` sat outside
# the gate with eleven deny-level errors in it.
#
# So the shrink must be RED, named by feature and by the target it took with it,
# and it must NOT be double-reported by arm 3 under a heading that means
# something else.
sed 's/"features": \["live-db-tests"\]/"features": []/' "$TMP/metadata.json" > "$TMP/metadata_nofeat.json"
run "$TMP/clean.json" "$TMP/metadata_nofeat.json"
check "a feature the run did not enable is named, not silently dropped" 1 \
  "were NOT enabled by this run" "pkg-mike/live-db-tests" "gates target mike_live (test)" \
  "linted:   6 targets in 3 packages (expected 5 in 3)" \
  "features: 1 of 2 declared non-default workspace features enabled"
if printf '%s\n' "$OUT" | grep -q "went unlinted"; then
  echo "FAIL: a target cargo was never asked to build is not an unlinted target"
  printf '%s\n' "$OUT" | sed 's/^/       /'
  fail=$((fail + 1)); pass=$((pass - 1))
fi

# ------------------------------------------------------------------ case 6b
# A feature that gates NO target at all. This is the case no target-level
# accounting can ever reach: `pkg-zulu/cfg-only` gates only `#[cfg(feature)]`
# code inside a lib that is linted either way, so every artifact still appears
# and every count still balances. It is the shape of plugin-db's `test-helpers`
# (~290 cfg sites, and four test targets besides). Arm 4 must name it with no
# "gates target" line to hang it on.
sed 's/"features": \["cfg-only"\]/"features": []/' "$TMP/metadata.json" > "$TMP/metadata_nocfg.json"
run "$TMP/clean.json" "$TMP/metadata_nocfg.json"
check "a feature that gates only cfg-code, not targets, is still named" 1 \
  "were NOT enabled by this run" "pkg-zulu/cfg-only" \
  "linted:   6 targets in 3 packages (expected 6 in 3)"

# ------------------------------------------------------------------ case 6c
# THE INSTRUMENT ARM FOR ARM 4. Metadata whose packages declare no features at
# all makes "every declared feature was enabled" vacuously true, and a vacuous
# arm 4 prints exactly what full feature coverage prints. Refuse it.
jq '.packages = [.packages[] | del(.features)]' "$TMP/metadata.json" > "$TMP/metadata_nofeatures.json"
run "$TMP/clean.json" "$TMP/metadata_nofeatures.json"
check "metadata declaring no features at all is refused, not read as full coverage" 2 \
  "too few declared features"

# ------------------------------------------------------------------- case 7
# THE INSTRUMENT ARM. An empty stream and a spotless workspace are the same
# bytes to anything that only counts errors. Exit 2, not 0 and not 1: nothing
# was measured, which is neither a pass nor a lint failure.
: > "$TMP/empty.json"
run "$TMP/empty.json"
check "an empty stream is refused, not passed" 2 "nothing was measured"

# ------------------------------------------------------------------- case 8
# Diagnostics from crates.io dependencies are not this workspace's problem and
# must not turn a clean tree red. (`--cap-lints` normally makes this moot; the
# filter is here so that a day it does not, the gate still points at us.)
{ full_stream; diag "$ID_EXT" error "clippy::needless_borrow" "in a vendored crate" serde lib; } > "$TMP/external.json"
run "$TMP/external.json"
check "an error attributed to a non-workspace crate does not fail us" 0 \
  "linted:   6 targets in 3 packages (expected 6 in 3)"

# ------------------------------------------------------------------- case 9
# The preflight. A gitignored build input that no longer exists must stop the
# run with exit 2 and name the command that produces it, rather than letting
# cargo report ~200 "couldn't read" errors that look like lint findings.
mkdir -p "$TMP/fixture_src"
cat > "$TMP/fixture_src/gen.rs" <<'EOF'
const A: &str = include_str!("generated/present.js");
const B: &str = include_str!(
    "generated/absent.js"
);
EOF
mkdir -p "$TMP/fixture_src/generated"
: > "$TMP/fixture_src/generated/present.js"
preflight "$TMP/fixture_src"
check "a missing include_str! input refuses before linting" 2 \
  "do not exist" "generated/absent.js"

# The MULTI-LINE form is what the first draft of that scan missed, and it missed
# it on a real file (crates/zeroship-runtime/src/core/init.rs:391). Case 9's `absent.js`
# is written in exactly that form, so a single-line-only scanner passes case 9
# by finding nothing - which is why case 10 asserts the count too.
: > "$TMP/fixture_src/generated/absent.js"
preflight "$TMP/fixture_src"
check "the preflight scan sees both the inline and the multi-line form" 0 \
  "2 include_str! literals"

# ------------------------------------------------------------------ case 11
# A scan that reads NOTHING and a tree with nothing to read produce the same
# empty missing-list. Only the count tells them apart, so an empty scan is
# refused rather than waved through.
mkdir -p "$TMP/fixture_empty"
preflight "$TMP/fixture_empty"
check "an empty preflight scan is refused, not passed" 2 "found no literals at all"

# ------------------------------------------------------------------ case 12
# THE ARM FLOORS ARE FUNCTIONS OF THE CORPUS, and arm 2's is derived from it.
# Assert the emitted census line: the fixture declares three workspace members
# and the arm must rule on all three, so a constant would show here as floor=3
# only by coincidence and as floor=20 (this workspace's default) if the caller's
# declaration were ignored. Case 13 is the half that makes this discriminating.
run "$TMP/clean.json"
if printf '%s\n' "$OUT" | grep -qx 'zsgate-arm gate=clippy arm=workspace_members examined=3 floor=3'; then
  pass=$((pass + 1))
else
  echo "FAIL: arm 2 did not gate on the corpus it was handed"
  printf '%s\n' "$OUT" | grep '^zsgate-arm' | sed 's/^/       /'
  fail=$((fail + 1))
fi

# ------------------------------------------------------------------ case 13
# A metadata blob whose member list collapsed must not satisfy completeness with
# nothing in it - examined == offered == 0 is the same defect one level out. The
# declared minimum is the bound on the denominator.
jq '.workspace_members = []' "$TMP/metadata.json" > "$TMP/metadata_nomembers.json"
run "$TMP/clean.json" "$TMP/metadata_nomembers.json"
check "a collapsed member list is refused, not completed vacuously" 2 \
  "below the declared minimum"

# ------------------------------------------------------------------ case 14
# ONE-VARIABLE CONTROL for the two above: a corpus supplied without its bounds
# is a refusal. This is the pairing the whole change rests on - if the flags
# could be given apart, a fixture would again be gated against the real tree's
# numbers or vice versa.
OUT="$("$GATE" --audit-only "$TMP/clean.json" --metadata "$TMP/metadata.json" 2>&1)"; RC=$?
check "redirecting the metadata without redeclaring its bounds is refused" 2 \
  "must be given together"

OUT="$("$GATE" --preflight-only --src-roots "$TMP/fixture_src" 2>&1)"; RC=$?
check "redirecting the source roots without redeclaring their bound is refused" 2 \
  "must be given together"

# --------------------------------------------------------------------------
echo "clippy gate self-test: $pass passed, $fail failed"
[ "$fail" -eq 0 ] || exit 1
