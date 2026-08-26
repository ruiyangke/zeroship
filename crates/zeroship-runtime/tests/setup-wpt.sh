#!/usr/bin/env bash
# Fetch the WPT working tree to crates/runtime/tests/wpt/.
#
# WPT is NOT managed as a git submodule — the submodule pack was ~140 MB
# even at depth=1, dwarfing the rest of .git. Instead we shallow-fetch
# a pinned commit on demand and let .gitignore keep it out of the index.
#
# Pin a specific commit via WPT_COMMIT env var (or edit the default
# below). Bump intentionally when a runner needs newer test vectors.
#
# Usage:
#   ./crates/runtime/tests/setup-wpt.sh                   # pinned commit
#   WPT_COMMIT=abc123 ./crates/runtime/tests/setup-wpt.sh # specific commit
set -euo pipefail

# Last-known-good WPT pin. Bump after the wpt_*.rs runners are
# verified green against a newer commit.
WPT_COMMIT="${WPT_COMMIT:-e053afbbd005bed4b6100f98f0de744da8d1d09d}"
WPT_REPO="${WPT_REPO:-https://github.com/web-platform-tests/wpt.git}"
TARGET="$(cd "$(dirname "$0")" && pwd)/wpt"

if [ -d "$TARGET/.git" ]; then
    echo "WPT clone exists at $TARGET — fetching pin ${WPT_COMMIT:0:12}..."
    cd "$TARGET"
    git fetch --depth=1 origin "$WPT_COMMIT"
    git reset --hard "$WPT_COMMIT"
else
    echo "Initialising WPT clone at $TARGET (pin ${WPT_COMMIT:0:12})..."
    mkdir -p "$TARGET"
    cd "$TARGET"
    # Init + fetch a single commit avoids cloning master HEAD as well —
    # keeps the pack at ~119 MB (just the pinned commit's blobs) instead
    # of ~280 MB (master + pin).
    git init -q
    git remote add origin "$WPT_REPO"
    git fetch --depth=1 origin "$WPT_COMMIT"
    git reset --hard FETCH_HEAD
fi

echo "WPT ready at $TARGET (commit ${WPT_COMMIT:0:12}). Working tree:"
du -sh .
