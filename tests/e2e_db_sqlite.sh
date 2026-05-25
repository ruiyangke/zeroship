#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

pnpm install --frozen-lockfile
pnpm build
cargo build -p zeroship
pnpm --dir examples/db-e2e typecheck
pnpm --dir examples/db-e2e build
pnpm --dir examples/db-e2e e2e
