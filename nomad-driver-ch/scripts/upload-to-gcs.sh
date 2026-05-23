#!/usr/bin/env bash
#
# Dry-run printer for uploading `dist/nomad-driver-ch` to the zsbx artifact
# bucket. This script DOES NOT actually call `gcloud storage cp` — it prints
# the exact commands the human operator should run. We do this on purpose
# for cost-control: GCS egress + the chance of force-overwriting a prod
# artifact warrant a manual confirmation step until we have a release
# pipeline with proper guard-rails.
#
# Usage:
#   ./scripts/upload-to-gcs.sh <version-tag>           # dry-run print
#   ./scripts/upload-to-gcs.sh <version-tag> --force   # allow overwrite
#
# Example:
#   ./scripts/upload-to-gcs.sh v1
#     → prints `gcloud storage cp dist/nomad-driver-ch
#       gs://suger-dev-zsbx-artifacts/nomad-driver-ch.v1 ...`
#
# Pre-flight checks (THESE run; the upload itself does not):
#   - dist/nomad-driver-ch must exist (run scripts/build-binary.sh first)
#   - the target object must not already exist (unless --force is passed)
#   - sha256 of the local binary is logged so the operator can spot-check
#     after the manual upload completes

set -Eeuo pipefail

BUCKET="suger-dev-zsbx-artifacts"
OBJECT_PREFIX="nomad-driver-ch"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BINARY="$REPO_ROOT/dist/nomad-driver-ch"

usage() {
  cat >&2 <<EOF
usage: $0 <version-tag> [--force]

  <version-tag>   e.g. v1, v2 — appended to the object name as
                  nomad-driver-ch.<version-tag>.
  --force         Allow overwriting an existing object. Default is refuse.
EOF
  exit 2
}

if [ $# -lt 1 ] || [ $# -gt 2 ]; then
  usage
fi

VERSION="$1"
FORCE=0
if [ $# -eq 2 ]; then
  if [ "$2" = "--force" ]; then
    FORCE=1
  else
    usage
  fi
fi

OBJECT="${OBJECT_PREFIX}.${VERSION}"
GCS_URI="gs://${BUCKET}/${OBJECT}"

if [ ! -f "$BINARY" ]; then
  echo "[upload-to-gcs] FATAL: $BINARY does not exist. Run scripts/build-binary.sh first." >&2
  exit 1
fi

BINARY_SHA256="$(sha256sum "$BINARY" | awk '{print $1}')"
BINARY_SIZE="$(stat -c %s "$BINARY")"

# Existence probe — needs gcloud locally but it's a STAT, not a write, so
# the cost is zero. We still print rather than block if gcloud is missing.
EXISTS=unknown
if command -v gcloud >/dev/null 2>&1; then
  if gcloud storage objects describe "$GCS_URI" >/dev/null 2>&1; then
    EXISTS=yes
  else
    EXISTS=no
  fi
fi

if [ "$EXISTS" = "yes" ] && [ "$FORCE" -ne 1 ]; then
  echo "[upload-to-gcs] FATAL: $GCS_URI already exists. Re-run with --force to overwrite." >&2
  exit 1
fi

cat <<EOF
[upload-to-gcs] DRY-RUN — printing commands; not executing.
[upload-to-gcs] local:    $BINARY
[upload-to-gcs] size:     $BINARY_SIZE bytes
[upload-to-gcs] sha256:   $BINARY_SHA256
[upload-to-gcs] target:   $GCS_URI
[upload-to-gcs] existing: $EXISTS  force=$FORCE

# Run these manually:

gcloud storage cp \\
  --content-type=application/octet-stream \\
  "$BINARY" \\
  "$GCS_URI"

# Verify the upload landed with the expected digest:
gcloud storage objects describe "$GCS_URI" --format='value(md5Hash,size)'
sha256sum "$BINARY"   # local hash for comparison: $BINARY_SHA256
EOF
