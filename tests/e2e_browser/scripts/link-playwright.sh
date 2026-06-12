#!/usr/bin/env bash
# ============================================================================
# tests/e2e_browser/scripts/link-playwright.sh — make `@playwright/test`
# resolvable from the spec files, pointing at the SAME copy the on-PATH
# `playwright` runner uses.
#
# Why this exists: tests/e2e_browser is not a pnpm workspace package, so
# `pnpm install` does not link node_modules here. And the runner is the nix
# `playwright` binary — spec `import { test } from "@playwright/test"` MUST
# resolve to the runner's own copy, or Playwright errors with "did not expect
# test.describe() to be called here" (two module instances → two registries).
#
# We derive the runner's `@playwright/test` location from the `playwright`
# binary on PATH (nix store path is machine-specific, so we never hardcode it)
# and symlink `node_modules/@playwright/test` to it. node_modules/ here is
# gitignored — this runs at global-setup time.
# ============================================================================
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
BROWSER_DIR="$(cd "$HERE/.." && pwd)"

PW_BIN="$(command -v playwright || true)"
if [ -z "$PW_BIN" ]; then
  echo "FATAL: no \`playwright\` on PATH — run inside the nix env (PLAYWRIGHT_BROWSERS_PATH must be set too)." >&2
  exit 1
fi

REAL="$(readlink -f "$PW_BIN")"

# Candidate 1: <store>/bin/playwright → <store>/lib/node_modules/@playwright/test
CAND="$(dirname "$(dirname "$REAL")")/lib/node_modules/@playwright/test"

# Candidate 2 (fallback): ask node to resolve it from the playwright CLI's dir.
if [ ! -e "$CAND/package.json" ]; then
  PW_LIB_DIR="$(dirname "$REAL")"
  CAND="$(node -e '
    try {
      const { createRequire } = require("module");
      const req = createRequire(process.argv[1] + "/");
      const pkg = req.resolve("@playwright/test/package.json");
      process.stdout.write(require("path").dirname(pkg));
    } catch (e) { process.stdout.write(""); }
  ' "$PW_LIB_DIR" 2>/dev/null)"
fi

if [ -z "$CAND" ] || [ ! -e "$CAND/package.json" ]; then
  echo "FATAL: could not locate the runner's @playwright/test next to $PW_BIN" >&2
  exit 1
fi

mkdir -p "$BROWSER_DIR/node_modules/@playwright"
ln -sfn "$CAND" "$BROWSER_DIR/node_modules/@playwright/test"
echo "linked @playwright/test -> $CAND"
exit 0
