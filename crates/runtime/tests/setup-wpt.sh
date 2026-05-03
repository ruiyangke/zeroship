#!/usr/bin/env bash
# Initialize the WPT submodule for runtime tests.
#
# Run once after a fresh clone:
#   git clone --recurse-submodules --shallow-submodules <repo>
#
# Or, if you cloned without --recurse-submodules:
#   git submodule update --init --depth=1
#
# Depth=1 alone (no sparse-checkout) — pack stays at ~119 MB compressed
# regardless of which paths the working tree shows, so sparse-checkout
# only complicates debugging without saving disk space. Working tree at
# full depth=1 is ~930 MB.
set -euo pipefail

cd "$(dirname "$0")/wpt"

# If a previous setup ran sparse-checkout, disable it so the full
# working tree is materialised. Idempotent on fresh checkouts.
if git sparse-checkout list >/dev/null 2>&1 && \
   [ -n "$(git sparse-checkout list 2>/dev/null)" ]; then
    git sparse-checkout disable
fi

echo "crates/runtime/tests/wpt at depth=1. Working tree:"
du -sh .
