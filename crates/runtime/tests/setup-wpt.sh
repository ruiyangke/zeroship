#!/usr/bin/env bash
# Initialize sparse-checkout for the crates/runtime/tests/wpt submodule.
#
# Run once after a fresh clone:
#   git clone --recurse-submodules --shallow-submodules <repo>
#   ./crates/runtime/tests/setup-wpt.sh
#
# Or, if you cloned without --recurse-submodules:
#   git submodule update --init --depth=1
#   ./crates/runtime/tests/setup-wpt.sh
#
# This trims the working tree to ~9MB (vs the full WPT ~2GB) by checking
# out only the suites our runtime exercises: fetch, streams, encoding
# (excluding the legacy multi-byte fixtures we don't need), compression,
# dom/{abort,events} (DOM EventTarget + AbortSignal), plus the testharness
# in resources/ and common/.
set -euo pipefail

cd "$(dirname "$0")/wpt"

git sparse-checkout init --no-cone
git sparse-checkout set \
    "/fetch/" \
    "/streams/" \
    "/encoding/*.js" \
    "/encoding/*.html" \
    "/encoding/resources/" \
    "/encoding/streams/" \
    "/compression/" \
    "/dom/abort/" \
    "/dom/events/" \
    "/xhr/formdata/" \
    "/FileAPI/" \
    "/url/" \
    "/WebCryptoAPI/" \
    "/resources/" \
    "/common/"

echo "crates/runtime/tests/wpt sparse-checkout configured. Working tree:"
du -sh .
