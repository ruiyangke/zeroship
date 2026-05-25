#!/usr/bin/env bash
#
# Reproducible release build for nomad-driver-ch.
#
# Produces `dist/nomad-driver-ch` — a stripped Go binary with the current
# repo's short git SHA embedded as `main.gitSHA`. Re-running this script on
# the same commit yields a byte-identical binary (verified via sha256).
#
# Flags used:
#   -trimpath          Strip absolute paths from the binary so /nix/store/...
#                      build paths don't bleed into a controller-portable
#                      artifact (T-7 / B16 lesson — paths shipped in the
#                      bash-wrapper binary leaked the build host's nix
#                      profile).
#   -ldflags="-s -w"   Drop the symbol table + DWARF debug info. ~30%
#                      smaller binary, no impact on runtime behaviour.
#   -ldflags="-X main.gitSHA=<sha>"
#                      Embed the short git SHA so `nomad-driver-ch --version`
#                      surfaces what's on disk. The default is "dev" so a
#                      forgotten -X always looks wrong.
#
# CGO is disabled by the Makefile and inherited here; this is a pure-Go
# binary (no libvirt link). The result is statically linkable as far as Go
# cares — `file dist/nomad-driver-ch` should report "statically linked".
#
# Usage:
#   ./scripts/build-binary.sh                # build dist/nomad-driver-ch
#   ./scripts/build-binary.sh --verify       # build twice + diff sha256

set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST="$REPO_ROOT/dist"
OUT="$DIST/nomad-driver-ch"

cd "$REPO_ROOT"

if ! command -v git >/dev/null 2>&1; then
  echo "[build-binary] FATAL: git not in PATH" >&2
  exit 1
fi

GIT_SHA="$(git rev-parse --short HEAD)"
mkdir -p "$DIST"

build_once() {
  local out=$1
  nix develop -c env CGO_ENABLED=0 go build \
    -trimpath \
    -ldflags="-s -w -X main.gitSHA=${GIT_SHA}" \
    -o "$out" \
    ./cmd/nomad-driver-ch
}

echo "[build-binary] commit=${GIT_SHA} → ${OUT}"
build_once "$OUT"

# Surface the size and ELF facts so the operator can sanity-check at a glance.
ls -la "$OUT"
file "$OUT" || true
sha256sum "$OUT"

if [ "${1:-}" = "--verify" ]; then
  echo "[build-binary] --verify: building a second copy for reproducibility diff"
  TMP="$(mktemp -d)"
  trap 'rm -rf "$TMP"' EXIT
  build_once "$TMP/nomad-driver-ch"
  SHA_A="$(sha256sum "$OUT"             | awk '{print $1}')"
  SHA_B="$(sha256sum "$TMP/nomad-driver-ch" | awk '{print $1}')"
  if [ "$SHA_A" = "$SHA_B" ]; then
    echo "[build-binary] reproducible: sha256=$SHA_A"
  else
    echo "[build-binary] NON-reproducible: ${SHA_A} != ${SHA_B}" >&2
    exit 2
  fi
fi
