#!/usr/bin/env bash
# Census of the test binaries `cargo test --workspace` builds, and of the tests
# inside them.
#
# WHAT WENT WRONG WITH THE THING THIS REPLACES
#
# ci.yml counted built test EXECUTABLES and failed if the number fell below a
# floor. The intent was right - "a suite that quietly stops being built produces
# a smaller run, and a smaller run is indistinguishable from a smaller project" -
# but the measure was a proxy, and the proxy broke:
#
#   3f7d74ee9 (2026-08-19) collapsed crates/auth from 53 test binaries to 1
#   2ee8bba08 (2026-08-20) collapsed crates/runtime from 140 to 9
#
# Neither lost a single test. The auth change was proved by a byte-identical
# 415-name `--list` across the old 48 binaries and the new target. Both moved
# the guarded number by -183 against 15 points of slack.
#
# So the guard fired on exactly the change everyone wanted, and the obvious
# repair - lower the floor until it passes - turns a guard into a rubber stamp,
# because nothing then distinguishes "we consolidated" from "a suite vanished".
# A count of CONTAINERS cannot tell those apart. A count of TESTS can.
#
# WHAT THIS MEASURES INSTEAD - two arms, and they cover different failures
#
#   1. PER-PACKAGE EXISTENCE. Every workspace package listed in the census file
#      must still build at least one test binary. Existence, not a number: a
#      package may consolidate 53 targets into 1 (still >= 1, still green) and
#      may add ten more (still green, no edit). Only going to ZERO is red.
#
#      This is the same conclusion the doc-citation gate in ci.yml reached for
#      itself - "the per-root assertion has to be EXISTENCE and not 'yielded at
#      least one'" - and for the same reason: a workspace-wide count cannot see
#      one whole crate stop being covered, because the slack that lets ordinary
#      churn through is wider than the crate.
#
#      It is checked in BOTH directions. A package that builds test binaries and
#      is NOT in the census is also red: an unlisted package is an unguarded
#      one, which is the disappearance failure wearing a new crate's name.
#
#   2. TEST-NAME FLOOR. Every test binary is asked for its test names with
#      `--list`, and the total must not fall below a floor. Consolidation cannot
#      move this - merging binaries preserves names - so arm 2 stays green
#      through exactly the change that broke the old floor, while catching what
#      arm 1 cannot: a package that keeps one target and loses forty tests.
#
# The old executable count is still PRINTED, because it is the number the ci.yml
# comments have been quoting since 2026-08-07 and the next reader will want it.
# It is not enforced. Printing a number nobody gates on is how the next tightening
# gets made against evidence instead of arithmetic.
#
# Usage:
#   tests/test_target_census_gate.sh <cargo-json>     check (what CI runs)
#   tests/test_target_census_gate.sh <cargo-json> --print-census
#                                                     emit the observed package
#                                                     list for a human to paste
#                                                     into the census file after
#                                                     deciding the change is
#                                                     wanted. Deliberately NOT
#                                                     something CI runs: a gate
#                                                     that regenerates its own
#                                                     expectation from the thing
#                                                     it guards agrees with
#                                                     itself by construction.
#
# <cargo-json> is the output of
#   cargo test --workspace --no-run --message-format=json
#
# Env overrides exist for the selftest only:
#   ZS_CENSUS_FILE          path to the expected-package list
#   ZS_CENSUS_METADATA      pre-captured `cargo metadata` json (skips cargo)
#   ZS_CENSUS_NAME_FLOOR    override the test-name floor
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# MEASURED 2026-08-20 on 4abf0f74a, by running the job's own
# `cargo test --workspace --no-run --message-format=json` to completion:
#
#   5444 test names across 122 test binaries in 28 packages
#   (131 executables by the retired count's reckoning, against its floor of 250)
#
# The floor is 5444 minus 31, and the 31 is a measured component rather than
# slack: it is the whole of crates/runtime's `wpt` target. That target
# `include_str!`s crates/runtime/tests/wpt/, which is gitignored, has never been
# tracked in any ref, and which no step of this job fetches - so on a runner it
# does not compile. Today that makes the BUILD fail before this gate is reached;
# if it is ever excluded with required-features instead, its 31 names go with it
# and the floor already expects that. No other allowance is made.
#
# What makes this go stale: someone adds tests (floor gets loose - harmless, and
# the step prints the real number every run so the next reader can tighten it),
# or the runner resolves features differently from this machine, which has never
# been observed because no CI run of this job has ever reported its own number.
NAME_FLOOR_DEFAULT=5413

CENSUS_FILE="${ZS_CENSUS_FILE:-$ROOT/tests/test_target_packages.txt}"
NAME_FLOOR="${ZS_CENSUS_NAME_FLOOR:-$NAME_FLOOR_DEFAULT}"

JSON="${1:-}"
MODE="${2:-check}"

if [ -z "$JSON" ] || [ ! -f "$JSON" ]; then
  echo "usage: $0 <cargo-test-no-run-message-format-json> [--print-census]" >&2
  echo "error: no readable cargo json at '${JSON:-<none>}'" >&2
  exit 2
fi

# jq is not optional and its absence must not read as a clean tree. Without it
# every extraction below yields nothing, which presents as "no package built any
# test target" - a confusing red - or, if anyone ever relaxes an arm, as a quiet
# green. Say which it is.
if ! command -v jq >/dev/null 2>&1; then
  echo "error: jq is required by $0 and is not on PATH" >&2
  exit 2
fi

# --- package id -> package name -------------------------------------------
#
# Parsing the name out of `package_id` by hand is a trap, and it is a trap that
# only bites SOME packages. Cargo 1.94 emits a PURL whose fragment carries the
# name only when it differs from the last path component:
#
#   path+file:///.../crates/auth#zeroship-auth@0.1.0    <- name present
#   path+file:///.../libs/compio-postgres#0.1.0         <- name absent
#
# A splitter written against the first form returns "0.1.0" for the second and
# the census then guards a package called "0.1.0" while compio-postgres goes
# unwatched. `cargo metadata` states the mapping instead of inferring it.
META="${ZS_CENSUS_METADATA:-}"
if [ -z "$META" ]; then
  META="$(mktemp)"
  trap 'rm -f "$META"' EXIT
  if ! (cd "$ROOT" && cargo metadata --format-version 1 --no-deps) > "$META" 2>/dev/null; then
    echo "error: cargo metadata failed; cannot map package ids to names" >&2
    exit 2
  fi
fi

ID_TO_NAME="$(jq -r '.packages[] | [.id, .name] | @tsv' "$META")"
if [ -z "$ID_TO_NAME" ]; then
  echo "error: cargo metadata yielded no workspace packages" >&2
  exit 2
fi

# --- the built test binaries ----------------------------------------------
#
# `profile.test == true` is the discriminator, NOT "executable is non-null".
# `cargo test` also builds every [[bin]] in the workspace, and those artifacts
# carry a non-null executable too. The old count included them; that was
# harmless while nothing ran them, but arm 2 EXECUTES what it enumerates, and
# `zeroship-gate --list` is a web server being asked to start, not a test
# harness being asked for its names.
ARTIFACTS="$(jq -r '
  select(.reason == "compiler-artifact"
         and .executable != null
         and .profile.test == true)
  | [.package_id, .target.name, .executable] | @tsv' "$JSON" | sort -u)"

if [ -z "$ARTIFACTS" ]; then
  echo "error: the cargo json contains no test executables at all" >&2
  echo "       (a partial or failed build, or the wrong file)" >&2
  exit 1
fi

# Continuity number only: what the retired floor used to count.
ALL_EXES="$(jq -r 'select(.reason == "compiler-artifact" and .executable != null)
                   | .executable' "$JSON" | sort -u | wc -l)"

# Join artifacts to package names.
OBSERVED="$(
  awk -F'\t' '
    NR == FNR { name[$1] = $2; next }
    { print (($1 in name) ? name[$1] : "UNMAPPED:" $1) "\t" $2 "\t" $3 }
  ' <(printf '%s\n' "$ID_TO_NAME") <(printf '%s\n' "$ARTIFACTS")
)"

UNMAPPED="$(printf '%s\n' "$OBSERVED" | grep -c '^UNMAPPED:' || true)"
if [ "$UNMAPPED" -gt 0 ]; then
  echo "error: $UNMAPPED test binaries have a package_id absent from cargo metadata" >&2
  printf '%s\n' "$OBSERVED" | grep '^UNMAPPED:' | head -5 >&2
  exit 2
fi

OBSERVED_PKGS="$(printf '%s\n' "$OBSERVED" | cut -f1 | sort -u)"
TARGET_COUNT="$(printf '%s\n' "$OBSERVED" | grep -c . || true)"
PKG_COUNT="$(printf '%s\n' "$OBSERVED_PKGS" | grep -c . || true)"

if [ "$MODE" = "--print-census" ]; then
  printf '%s\n' "$OBSERVED_PKGS"
  exit 0
fi

if [ ! -f "$CENSUS_FILE" ]; then
  echo "error: census file not found: $CENSUS_FILE" >&2
  exit 2
fi

EXPECTED_PKGS="$(grep -v '^[[:space:]]*#' "$CENSUS_FILE" | grep -v '^[[:space:]]*$' | sort -u)"
if [ -z "$EXPECTED_PKGS" ]; then
  echo "error: census file $CENSUS_FILE lists no packages" >&2
  exit 2
fi

rc=0

# --- arm 1: per-package existence, both directions ------------------------
MISSING="$(comm -23 <(printf '%s\n' "$EXPECTED_PKGS") <(printf '%s\n' "$OBSERVED_PKGS"))"
EXTRA="$(comm -13 <(printf '%s\n' "$EXPECTED_PKGS") <(printf '%s\n' "$OBSERVED_PKGS"))"

if [ -n "$MISSING" ]; then
  rc=1
  echo "::error::these packages built NO test binary; their coverage disappeared"
  printf '  %s\n' $MISSING
  echo "  If a package's tests moved behind required-features on purpose, say so"
  echo "  in $CENSUS_FILE and remove the row - naming where they run now."
fi

if [ -n "$EXTRA" ]; then
  rc=1
  echo "::error::these packages build test binaries but are not in the census, so nothing would notice if they stopped"
  printf '  %s\n' $EXTRA
  echo "  Add them to $CENSUS_FILE."
fi

# --- arm 2: test-name floor ------------------------------------------------
#
# A binary that fails to list is an INSTRUMENT failure and is reported as one.
# Counting it as zero would let a broken enumerator present as lost coverage,
# and - once anyone widened the slack - as a pass.
names_total=0
list_failures=0
while IFS=$'\t' read -r pkg target exe; do
  [ -n "${exe:-}" ] || continue
  if ! out="$("$exe" --list 2>/dev/null)"; then
    list_failures=$((list_failures + 1))
    echo "  could not list tests in $pkg/$target ($exe)" >&2
    continue
  fi
  n="$(printf '%s\n' "$out" | grep -c ': test$' || true)"
  names_total=$((names_total + n))
done <<< "$OBSERVED"

if [ "$list_failures" -gt 0 ]; then
  rc=1
  echo "::error::$list_failures test binaries could not be asked for their test names; the count below is not a measurement"
fi

echo "test binaries:  $TARGET_COUNT across $PKG_COUNT packages"
echo "test names:     $names_total (floor $NAME_FLOOR)"
echo "all executables: $ALL_EXES (not gated; the number the retired target floor counted)"

if [ "$names_total" -lt "$NAME_FLOOR" ]; then
  rc=1
  echo "::error::test names ($names_total) fell below the floor ($NAME_FLOOR); tests stopped being built or run"
  echo "  Consolidating binaries does NOT move this number. A drop here is lost tests."
fi

exit $rc
