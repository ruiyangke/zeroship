#!/bin/sh
# Build the zeroship/sandbox-base image used by zeroship-sandbox to
# spawn editor sessions.
#
# Usage:
#   ./build.sh                  → builds zeroship/sandbox-base:latest
#   ./build.sh v0.1.0           → tags both v0.1.0 and latest
set -eu

cd "$(dirname "$0")"

TAG="${1:-latest}"
IMAGE="zeroship/sandbox-base"

echo "[build] building ${IMAGE}:${TAG}"
docker build -t "${IMAGE}:${TAG}" -t "${IMAGE}:latest" .

echo "[build] done — image: ${IMAGE}:${TAG}"
docker images --format "{{.Repository}}:{{.Tag}}\t{{.Size}}" | grep "^${IMAGE}:" | head -5
