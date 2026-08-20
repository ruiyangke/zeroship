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
    {"id": "$ID_M", "name": "pkg-mike", "targets": [
      {"name": "pkg-mike", "kind": ["lib"]},
      {"name": "mike_it", "kind": ["test"]},
      {"name": "mike_live", "kind": ["test"], "required-features": ["live-db-tests"]}
    ]},
    {"id": "$ID_Z", "name": "pkg-zulu", "targets": [
      {"name": "pkg-zulu", "kind": ["lib"]}
    ]},
    {"id": "$ID_EXT", "name": "serde", "targets": [
      {"name": "serde", "kind": ["lib"]}
    ]}
  ],
  "resolve": {"nodes": [
    {"id": "$ID_A", "features": []},
    {"id": "$ID_M", "features": ["live-db-tests"]},
    {"id": "$ID_Z", "features": []},
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

# run <json-file> -> sets RC and OUT
run() {
  OUT="$(ZS_CLIPPY_METADATA="$TMP/metadata.json" "$GATE" --audit-only "$1" 2>&1)"
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
# A target whose required-features the run did NOT enable is out of scope and
# must not be demanded. Same stream as case 1, but resolved with the feature
# off: `mike_live` disappears from BOTH sides and the run is still green.
sed 's/"features": \["live-db-tests"\]/"features": []/' "$TMP/metadata.json" > "$TMP/metadata_nofeat.json"
OUT="$(ZS_CLIPPY_METADATA="$TMP/metadata_nofeat.json" "$GATE" --audit-only "$TMP/clean.json" 2>&1)"; RC=$?
check "a target gated off by required-features is not demanded" 0 \
  "linted:   6 targets in 3 packages (expected 5 in 3)"

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
OUT="$(ZS_CLIPPY_SRC_ROOTS="$TMP/fixture_src" "$GATE" --preflight-only 2>&1)"; RC=$?
check "a missing include_str! input refuses before linting" 2 \
  "do not exist" "generated/absent.js"

# The MULTI-LINE form is what the first draft of that scan missed, and it missed
# it on a real file (crates/runtime/src/core/init.rs:391). Case 9's `absent.js`
# is written in exactly that form, so a single-line-only scanner passes case 9
# by finding nothing - which is why case 10 asserts the count too.
: > "$TMP/fixture_src/generated/absent.js"
OUT="$(ZS_CLIPPY_SRC_ROOTS="$TMP/fixture_src" "$GATE" --preflight-only 2>&1)"; RC=$?
check "the preflight scan sees both the inline and the multi-line form" 0 \
  "2 include_str! literals"

# ------------------------------------------------------------------ case 11
# A scan that reads NOTHING and a tree with nothing to read produce the same
# empty missing-list. Only the count tells them apart, so an empty scan is
# refused rather than waved through.
mkdir -p "$TMP/fixture_empty"
OUT="$(ZS_CLIPPY_SRC_ROOTS="$TMP/fixture_empty" "$GATE" --preflight-only 2>&1)"; RC=$?
check "an empty preflight scan is refused, not passed" 2 "found no literals at all"

# --------------------------------------------------------------------------
echo "clippy gate self-test: $pass passed, $fail failed"
[ "$fail" -eq 0 ] || exit 1
