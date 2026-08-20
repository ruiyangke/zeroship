#!/usr/bin/env bash
# Self-test for tests/test_target_census_gate.sh.
#
# The gate replaced a floor on test-BINARY count that had become unsatisfiable,
# because two consolidations removed 183 binaries without removing one test. The
# replacement is only worth having if it is right in three directions, and the
# middle one is the whole reason the first was thrown away:
#
#   a genuine collapse MUST be red    - else the gate is decoration
#   a consolidation MUST be green     - else the gate fires on the change we
#                                       want, and the repair everyone reaches
#                                       for is lowering the number, which is
#                                       how the last one became a rubber stamp
#   a broken instrument MUST be red   - a gate that reads nothing and a tree
#     and must NOT read as             that has nothing produce the same
#     lost coverage                    output; only saying so tells them apart
#
# Every case runs against fixtures, so none of it needs a cargo build. The stub
# executables answer `--list` the way libtest does, which is the only thing the
# gate asks of them.
#
# Run directly: tests/test_target_census_selftest.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GATE="$ROOT/tests/test_target_census_gate.sh"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail=0
pass=0

# ---------------------------------------------------------------- fixtures
# Two workspace packages. `beta` deliberately uses the package_id form whose
# fragment omits the name (the form real cargo emits for libs/compio-postgres),
# so a gate that split the name out of the id by hand would mis-key it here.
cat > "$TMP/metadata.json" <<'EOF'
{"packages":[
  {"id":"path+file:///w/crates/alpha#pkg-alpha@0.1.0","name":"pkg-alpha"},
  {"id":"path+file:///w/libs/pkg-beta#0.1.0","name":"pkg-beta"}
]}
EOF
ID_ALPHA='path+file:///w/crates/alpha#pkg-alpha@0.1.0'
ID_BETA='path+file:///w/libs/pkg-beta#0.1.0'

# mk_exe <path> <n_tests>   a stub that lists n test names, as libtest does
mk_exe() {
  local path="$1" n="$2"
  cat > "$path" <<EOF
#!/usr/bin/env bash
if [ "\${1:-}" = "--list" ]; then
  for i in \$(seq 1 $n); do echo "mod::t\$i: test"; done
  echo "mod::b1: benchmark"
  echo
  echo "$n tests, 1 benchmark"
  exit 0
fi
exit 0
EOF
  chmod +x "$path"
}

# mk_broken_exe <path>      a stub that cannot list (the instrument failure)
mk_broken_exe() {
  printf '#!/usr/bin/env bash\nexit 3\n' > "$1"
  chmod +x "$1"
}

# artifact <pkg_id> <target> <exe> -> one cargo compiler-artifact json line
artifact() {
  printf '{"reason":"compiler-artifact","package_id":"%s","target":{"name":"%s","kind":["test"]},"profile":{"test":true},"executable":"%s"}\n' "$1" "$2" "$3"
}

# a non-test [[bin]] artifact: non-null executable, profile.test false. The
# retired count included these; this gate must not, because arm 2 RUNS what it
# enumerates and a bin is a server, not a harness.
bin_artifact() {
  printf '{"reason":"compiler-artifact","package_id":"%s","target":{"name":"%s","kind":["bin"]},"profile":{"test":false},"executable":"%s"}\n' "$1" "$2" "$3"
}

# run_gate <json> <census> <floor> -> sets OUT, RC
run_gate() {
  OUT="$(ZS_CENSUS_METADATA="$TMP/metadata.json" \
         ZS_CENSUS_FILE="$2" \
         ZS_CENSUS_NAME_FLOOR="$3" \
         bash "$GATE" "$1" 2>&1)"
  RC=$?
}

# check <expected_rc> <label> [required substring]
check() {
  local want="$1" label="$2" needle="${3:-}"
  local ok=1
  [ "$RC" = "$want" ] || ok=0
  if [ -n "$needle" ] && ! printf '%s' "$OUT" | grep -qF "$needle"; then ok=0; fi
  if [ "$ok" = 1 ]; then
    pass=$((pass + 1))
    echo "ok   - $label (rc=$RC)"
  else
    fail=$((fail + 1))
    echo "FAIL - $label: wanted rc=$want${needle:+ containing '$needle'}, got rc=$RC" >&2
    printf '%s\n' "$OUT" | sed 's/^/       | /' >&2
  fi
}

printf 'pkg-alpha\npkg-beta\n' > "$TMP/census.txt"

# ============================================================== BASELINE
# pkg-alpha: 3 binaries x 10 tests. pkg-beta: 1 binary x 20. Total 50 names.
mkdir -p "$TMP/before"
for i in 1 2 3; do mk_exe "$TMP/before/a$i" 10; done
mk_exe "$TMP/before/b1" 20
mk_exe "$TMP/before/server" 0   # stands in for a [[bin]]; must never be listed
{
  artifact "$ID_ALPHA" "a1" "$TMP/before/a1"
  artifact "$ID_ALPHA" "a2" "$TMP/before/a2"
  artifact "$ID_ALPHA" "a3" "$TMP/before/a3"
  artifact "$ID_BETA"  "b1" "$TMP/before/b1"
  bin_artifact "$ID_ALPHA" "server" "$TMP/before/server"
} > "$TMP/before.json"

run_gate "$TMP/before.json" "$TMP/census.txt" 50
check 0 "ARM 1: a healthy tree passes at the measured floor" "test names:     50"

run_gate "$TMP/before.json" "$TMP/census.txt" 50
check 0 "the [[bin]] artifact is excluded from the test-binary count" "test binaries:  4 across 2 packages"

# ============================================== ARM 3: LEGITIMATE CONSOLIDATION
# The exact shape of 3f7d74ee9 and 2ee8bba08: pkg-alpha's three binaries become
# one binary carrying all 30 of the same test names. Binaries 4 -> 2, names 50.
mkdir -p "$TMP/consolidated"
mk_exe "$TMP/consolidated/main" 30
mk_exe "$TMP/consolidated/b1" 20
{
  artifact "$ID_ALPHA" "main" "$TMP/consolidated/main"
  artifact "$ID_BETA"  "b1"   "$TMP/consolidated/b1"
} > "$TMP/consolidated.json"

run_gate "$TMP/consolidated.json" "$TMP/census.txt" 50
check 0 "ARM 3: consolidating 3 binaries into 1 stays green" "test names:     50"

run_gate "$TMP/consolidated.json" "$TMP/census.txt" 50
check 0 "ARM 3: and the binary count really did halve, so the old floor would have fired" "test binaries:  2 across 2 packages"

# ARM 3 control, differing in ONE variable: same collapse to a single binary,
# but the binary carries 29 names instead of 30. Consolidation is green; losing
# one test through the same restructuring is red. Without this the case above
# only proves the gate is insensitive, not that it discriminates.
mkdir -p "$TMP/lossy"
mk_exe "$TMP/lossy/main" 29
mk_exe "$TMP/lossy/b1" 20
{
  artifact "$ID_ALPHA" "main" "$TMP/lossy/main"
  artifact "$ID_BETA"  "b1"   "$TMP/lossy/b1"
} > "$TMP/lossy.json"

run_gate "$TMP/lossy.json" "$TMP/census.txt" 50
check 1 "ARM 2: the same collapse that drops ONE test is red" "fell below the floor"

# ==================================================== ARM 2: GENUINE COLLAPSE
# pkg-alpha keeps a binary - so arm 1 is satisfied - and loses 29 of its 30
# tests. This is the failure a per-package existence check cannot see, and the
# reason the name floor exists alongside it.
mkdir -p "$TMP/gutted"
mk_exe "$TMP/gutted/main" 1
mk_exe "$TMP/gutted/b1" 20
{
  artifact "$ID_ALPHA" "main" "$TMP/gutted/main"
  artifact "$ID_BETA"  "b1"   "$TMP/gutted/b1"
} > "$TMP/gutted.json"

run_gate "$TMP/gutted.json" "$TMP/census.txt" 50
check 1 "ARM 2: a package that keeps a target and loses its tests is red" "test names:     21"

# ============================================ ARM 2: A PACKAGE VANISHES ENTIRELY
# pkg-beta stops building any test binary. Its 20 names go with it, so the floor
# would catch this too - but only while the floor is tight. Arm 1 names the
# package, which is the part that survives a loose floor.
{
  artifact "$ID_ALPHA" "a1" "$TMP/before/a1"
  artifact "$ID_ALPHA" "a2" "$TMP/before/a2"
  artifact "$ID_ALPHA" "a3" "$TMP/before/a3"
} > "$TMP/dropped.json"

run_gate "$TMP/dropped.json" "$TMP/census.txt" 30
check 1 "ARM 1: a package that builds no test binary is named" "pkg-beta"

run_gate "$TMP/dropped.json" "$TMP/census.txt" 30
check 1 "ARM 1: and it is red even when the name floor alone would pass" "coverage disappeared"

# ================================================== ARM 1: AN UNLISTED PACKAGE
# The mirror image: a package builds tests but nothing expects it to, so its
# disappearance tomorrow would be silent.
printf 'pkg-alpha\n' > "$TMP/census_short.txt"
run_gate "$TMP/before.json" "$TMP/census_short.txt" 50
check 1 "ARM 1: a package building tests but absent from the census is red" "pkg-beta"

# ============================================= INSTRUMENT: BROKEN ENUMERATOR
# The mutation the acceptance criteria demand: break enumeration itself. An
# empty artifact stream is what a partial build, a wrong path, or a jq filter
# that stopped matching all produce.
: > "$TMP/empty.json"
run_gate "$TMP/empty.json" "$TMP/census.txt" 50
check 1 "INSTRUMENT: an empty artifact stream is red, not a green zero" "no test executables at all"

# A stream that has artifacts but none of them test binaries - the shape a
# `profile.test` filter typo would produce.
bin_artifact "$ID_ALPHA" "server" "$TMP/before/server" > "$TMP/bins_only.json"
run_gate "$TMP/bins_only.json" "$TMP/census.txt" 50
check 1 "INSTRUMENT: a stream of non-test artifacts is red" "no test executables at all"

# A binary that cannot be asked for its names. Counting it as zero would present
# an instrument fault as lost coverage, and would pass outright under a loose
# floor. It must say which it is.
mkdir -p "$TMP/broken"
mk_broken_exe "$TMP/broken/main"
mk_exe "$TMP/broken/b1" 20
{
  artifact "$ID_ALPHA" "main" "$TMP/broken/main"
  artifact "$ID_BETA"  "b1"   "$TMP/broken/b1"
} > "$TMP/broken.json"

run_gate "$TMP/broken.json" "$TMP/census.txt" 20
check 1 "INSTRUMENT: a binary that cannot list is reported as an instrument fault" "not a measurement"

# The package_id form with no name in the fragment must map. If it did not, the
# baseline case above would have reported an UNMAPPED package rather than
# pkg-beta, so this asserts the id parsing directly rather than by implication.
run_gate "$TMP/before.json" "$TMP/census.txt" 50
check 0 "pkg-beta's name-less package_id maps to its real name" "test binaries:  4 across 2 packages"

# A census file that exists but lists nothing must not read as "everything is
# expected to be absent, therefore nothing is missing, therefore green".
: > "$TMP/census_empty.txt"
run_gate "$TMP/before.json" "$TMP/census_empty.txt" 50
check 2 "an empty census file is a configuration fault, not a pass" "lists no packages"

# A missing json argument must not be a pass either.
OUT="$(bash "$GATE" 2>&1)"; RC=$?
check 2 "no argument is a usage error" "usage:"

echo
echo "passed $pass, failed $fail"
[ "$fail" = 0 ]
