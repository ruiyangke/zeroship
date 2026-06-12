#!/usr/bin/env bash
# ============================================================================
# tests/e2e_browser/scripts/up.sh — bring up the full zeroship stack for the
# browser-level Playwright E2E and deploy the three render examples.
#
# Sources the shared bring-up library (tests/lib/e2e_stack.sh), then:
#   • stack_up        — ephemeral PG + Liquibase + control/worker/gateway
#   • mint_admin_pat  — offline platform-admin PAT
#   • deploy_zship    — csr-todo / ssr-blog / ssg-docs (slugs ...-bx, disjoint
#                       from the curl harness's ...-e2e apps)
#
# Writes a descriptor JSON to tests/e2e_browser/.stack.json that the Playwright
# specs read for the dynamic gateway port + per-app slugs. LEAVES THE STACK
# RUNNING and exits 0 on success (the Playwright globalSetup spawns this; the
# globalTeardown later runs down.sh against the same descriptor).
#
# Ports live in a private band offset from BOTH e2e_app_primitives.sh and the
# render harness so this can coexist on the same host.
# ============================================================================
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
BROWSER_DIR="$(cd "$HERE/.." && pwd)"
ROOT="$(cd "$BROWSER_DIR/../.." && pwd)"
DESCRIPTOR="$BROWSER_DIR/.stack.json"

# --- ports / container name (disjoint from the other two harnesses) ---------
export CONTROL_PORT=9140
export WORKER_PORT=8108
export GATE_PORT=8022
export PG_PORT=5464
export PG_CONTAINER="zs-e2e-browser-pg"

# shellcheck source=../../lib/e2e_stack.sh
source "$ROOT/tests/lib/e2e_stack.sh"

echo "============================================"
echo "  zeroship browser-E2E stack bring-up"
echo "============================================"

stack_up   || { echo "FATAL: stack_up failed"; exit 1; }
mint_admin_pat || { echo "FATAL: mint_admin_pat failed"; exit 1; }

# --- deploy the three render examples (skip-with-note if a dist is missing) --
CSR_ZSHIP="$ROOT/examples/csr-todo/dist/app.zship"
SSR_ZSHIP="$ROOT/examples/ssr-blog/dist/app.zship"
SSG_ZSHIP="$ROOT/examples/ssg-docs/dist/app.zship"

CSR_SLUG="csr-todo-bx"
SSR_SLUG="ssr-blog-bx"
SSG_SLUG="ssg-docs-bx"

deploy_one() {
  local slug="$1" zship="$2" label="$3"
  if [ ! -f "$zship" ]; then
    echo "  ⚠ SKIP $label — missing $zship (build: pnpm --filter $label build)"
    echo ""   # empty id ⇒ spec skips
    return 0
  fi
  local id
  if id="$(deploy_zship "$slug" "$zship")"; then
    echo "  ✓ deployed $label as $slug ($id)"
    echo "$id"
    return 0
  fi
  echo "  ✗ deploy $label failed"
  echo ""
  return 1
}

echo ""
echo "=== Deploy render examples ==="
# Capture id on last line, log on earlier lines.
CSR_OUT="$(deploy_one "$CSR_SLUG" "$CSR_ZSHIP" csr-todo)"; echo "$CSR_OUT" | sed '$d'
CSR_ID="$(echo "$CSR_OUT" | tail -1)"
SSR_OUT="$(deploy_one "$SSR_SLUG" "$SSR_ZSHIP" ssr-blog)"; echo "$SSR_OUT" | sed '$d'
SSR_ID="$(echo "$SSR_OUT" | tail -1)"
SSG_OUT="$(deploy_one "$SSG_SLUG" "$SSG_ZSHIP" ssg-docs)"; echo "$SSG_OUT" | sed '$d'
SSG_ID="$(echo "$SSG_OUT" | tail -1)"

# Give the worker/gateway a moment to pull the freshly-deployed manifests.
sleep 5

# --- write the descriptor the specs read ------------------------------------
node -e '
const fs = require("fs");
const d = {
  gatePort: Number(process.env.GATE_PORT),
  controlPort: Number(process.env.CONTROL_PORT),
  workerPort: Number(process.env.WORKER_PORT),
  work: process.env.WORK,
  pidfile: process.env.PIDFILE,
  pgContainer: process.env.PG_CONTAINER,
  apps: {
    csr: process.argv[1] ? "'"$CSR_SLUG"'" : null,
    ssr: process.argv[2] ? "'"$SSR_SLUG"'" : null,
    ssg: process.argv[3] ? "'"$SSG_SLUG"'" : null,
  },
  appIds: {
    csr: process.argv[1] || null,
    ssr: process.argv[2] || null,
    ssg: process.argv[3] || null,
  },
};
fs.writeFileSync("'"$DESCRIPTOR"'", JSON.stringify(d, null, 2));
console.log("wrote descriptor:", "'"$DESCRIPTOR"'");
console.log(JSON.stringify(d.apps));
' "$CSR_ID" "$SSR_ID" "$SSG_ID"

echo ""
echo "  stack is UP — gateway on :$GATE_PORT (descriptor: $DESCRIPTOR)"
exit 0
