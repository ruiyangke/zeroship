#!/usr/bin/env bash
# ============================================================================
# tests/e2e_browser/scripts/down.sh — tear down the stack brought up by up.sh.
#
# Reads tests/e2e_browser/.stack.json for the pidfile / PG container / work
# dir, kills the binary PIDs, docker rm -f the container, and removes the work
# dir + descriptor. Idempotent: a missing descriptor is not an error.
# ============================================================================
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
BROWSER_DIR="$(cd "$HERE/.." && pwd)"
DESCRIPTOR="$BROWSER_DIR/.stack.json"

if [ ! -f "$DESCRIPTOR" ]; then
  echo "no descriptor at $DESCRIPTOR — nothing to tear down"
  exit 0
fi

PIDFILE="$(node -e 'const d=require(process.argv[1]);process.stdout.write(d.pidfile||"")' "$DESCRIPTOR")"
PG_CONTAINER="$(node -e 'const d=require(process.argv[1]);process.stdout.write(d.pgContainer||"")' "$DESCRIPTOR")"
WORK="$(node -e 'const d=require(process.argv[1]);process.stdout.write(d.work||"")' "$DESCRIPTOR")"

echo "=== browser-E2E stack teardown ==="
if [ -n "$PIDFILE" ] && [ -f "$PIDFILE" ]; then
  while read -r pid; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done < "$PIDFILE"
  echo "  killed binary PIDs"
fi
if [ -n "$PG_CONTAINER" ]; then
  docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
  echo "  removed PG container $PG_CONTAINER"
fi
if [ -n "$WORK" ] && [ -d "$WORK" ]; then
  rm -rf "$WORK"
  echo "  removed work dir"
fi
rm -f "$DESCRIPTOR"
echo "  stack down."
exit 0
