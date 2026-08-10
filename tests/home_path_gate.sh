#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# A tracked source or config file must not hardcode a home directory.
#
# WHY, from three instances in one day (2026-08-10):
#
#   1. sdks/vite-plugin/package.json depended on
#      `file:/home/ruiyang/Projects/zero-migrate/sdks/migrate`. Published, it
#      became a dependency on a path that exists on one machine (task #265).
#   2. examples/db-todos/playwright.config.ts set
#      `ZEROSHIP_BIN: "/home/ruiyang/Projects/appbase/target/release/zeroship"`.
#      dev-server.ts uses ZEROSHIP_BIN directly with no existence check, so on
#      any other checkout that suite spawned a binary that does not exist.
#   3. crates/runtime/tests/codec.rs cited a doc by absolute home path.
#
# All three read correctly to their author and are broken for everyone else.
# None was caught by a test, because each is correct on the machine that runs
# the tests.
#
# SCOPE IS BY EXTENSION, NOT BY FILENAME. In prose a path is a CITATION - a
# record of a command as it was actually run - and rewriting it to be portable
# makes it a worse record. In code or config it is a DEPENDENCY that resolves
# or does not. So this scans source and config and leaves docs alone, and no
# file is exempt by name. If a .md ever becomes load-bearing for a path, that
# is a decision to write down, not a filename to add here.
# (The extension-vs-allowlist framing is zero-migrate's, from
#  ZERO-MIGRATE-2026-08-10-186; their gate is a36485d5 in that repo.)
#
# URLS ARE NOT PATHS, and this repo has already been bitten by conflating
# them: task #58 was "the doc-citation CI check matches paths inside URLs and
# is green by coincidence". Measured here before writing this gate: the naive
# pattern reports 42 hits in
# examples/apple-website-study/src/assets/apple/sources.json, every one of
# them inside `https://www.apple.com/v/iphone/home/cj/...`, where `home` is an
# apple.com path segment. So URLs are stripped from each line BEFORE matching.
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# third_party/ is a vendored submodule owned by another project; its contents
# are not ours to gate.
EXTS=('*.rs' '*.ts' '*.tsx' '*.mjs' '*.cjs' '*.js' '*.toml' '*.yml' '*.yaml' '*.nix' '*.json')

# `/home/<user>/` and `/Users/<user>/`. The trailing slash matters: it is what
# separates a real home directory from a test fixture like "/home/leak" or
# "/home/should-not-leak", which are deliberately fake values asserting that a
# leak does NOT happen and must not trip this gate.
PAT='/(home|Users)/[A-Za-z0-9._-]+/'

enumerated=0
hits=""
while IFS= read -r f; do
  case "$f" in third_party/*) continue ;; esac
  enumerated=$((enumerated + 1))
  # Strip URLs, then match. Order matters: matching first and filtering after
  # would drop a line that contains BOTH a URL and a real home path.
  line=$(sed 's#https\?://[^"'"'"' )]*##g' "$f" | grep -nE "$PAT" || true)
  [ -n "$line" ] && hits="${hits}${f}: ${line}"$'\n'
done < <(git ls-files -- "${EXTS[@]}" 2>/dev/null)

# THE CANNOT-ANSWER BRANCH. A scan that enumerated nothing and a scan that
# enumerated everything and found nothing print the same thing: silence. The
# floor is the only thing separating "clean" from "did not look". Measured by
# running this gate, 2026-08-10: 2018 files match these extensions outside
# third_party/, so a floor of 400 tolerates large deletions while still
# catching a glob that stopped matching. (Idea from zero-migrate's gate, which
# hit exactly this.)
# The floor is a variable so the failure message names the threshold that was
# actually applied. It said "below the 400 floor" while comparing against 9000
# during its own mutation test - a message describing a different check than
# the one that ran, which is the exact defect class this repo keeps finding.
ENUM_FLOOR="${ENUM_FLOOR:-400}"
if [ "$enumerated" -lt "$ENUM_FLOOR" ]; then
  echo "FAIL: only $enumerated files enumerated, below the $ENUM_FLOOR floor." >&2
  echo "      The listing is truncated or the globs stopped matching, so a" >&2
  echo "      clean result would mean nothing. This is not a pass." >&2
  exit 2
fi

if [ -n "$hits" ]; then
  echo "FAIL: tracked source/config hardcodes a home directory, which resolves" >&2
  echo "      on exactly one machine:" >&2
  printf '%s' "$hits" | sed 's/^/  /' >&2
  echo "      Resolve it relative to the file, or read it from the environment." >&2
  exit 1
fi

echo "home-path gate: $enumerated source/config files, 0 hardcoded home paths"
