#!/usr/bin/env bash
# Link spec imports to the exact @playwright/test copy used by the Nix runner.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$HERE/.." && pwd)"

PW_BIN="$(command -v playwright || true)"
if [ -z "$PW_BIN" ]; then
  echo "FATAL: no playwright on PATH; run inside nix develop." >&2
  exit 1
fi

REAL="$(readlink -f "$PW_BIN")"
CAND="$(dirname "$(dirname "$REAL")")/lib/node_modules/@playwright/test"

if [ ! -e "$CAND/package.json" ]; then
  PW_LIB_DIR="$(dirname "$REAL")"
  CAND="$(node -e '
    try {
      const { createRequire } = require("module");
      const req = createRequire(process.argv[1] + "/");
      const pkg = req.resolve("@playwright/test/package.json");
      process.stdout.write(require("path").dirname(pkg));
    } catch (_) {
      process.stdout.write("");
    }
  ' "$PW_LIB_DIR" 2>/dev/null)"
fi

if [ -z "$CAND" ] || [ ! -e "$CAND/package.json" ]; then
  echo "FATAL: could not locate the runner's @playwright/test next to $PW_BIN" >&2
  exit 1
fi

VERSION="$(node -p 'require(process.argv[1]).version' "$CAND/package.json")"
if [ "$VERSION" != "1.58.2" ]; then
  echo "FATAL: expected @playwright/test 1.58.2, found $VERSION at $CAND" >&2
  exit 1
fi

mkdir -p "$PROJECT_DIR/node_modules/@playwright"
ln -sfn "$CAND" "$PROJECT_DIR/node_modules/@playwright/test"
echo "linked @playwright/test -> $CAND"
