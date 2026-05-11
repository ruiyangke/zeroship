#!/usr/bin/env bash
#
# Tear down the zsbx GCP validation cluster.
#
# Deletes instances + reserved internal addresses. Leaves network /
# subnet / firewall rules in place (cheap, safe to keep — re-running
# provision picks them up).
#
# Hard requirement from the task brief: this MUST be runnable even
# on partial failure of the validation run. The script is exit-0 on
# "nothing to delete" and continues past per-resource failures so
# one bad delete doesn't strand the rest.
#
# Env (same defaults as provision-gcp-cluster.sh):
#   PROJECT, ZONE, REGION, PREFIX, SERVER_COUNT, WORKER_COUNT

set -uo pipefail

PROJECT=${PROJECT:-$(gcloud config get-value project 2>/dev/null || true)}
: "${PROJECT:?gcloud project not set}"

ZONE=${ZONE:-asia-northeast3-a}
REGION=${REGION:-asia-northeast3}
PREFIX=${PREFIX:-zsbx-prod}
SERVER_COUNT=${SERVER_COUNT:-3}
WORKER_COUNT=${WORKER_COUNT:-5}

echo "[teardown] project=$PROJECT zone=$ZONE prefix=$PREFIX"

# Delete all instances matching the prefix (catches both server-* and
# worker-* in one shot).
INSTANCES=$(gcloud compute instances list \
  --project "$PROJECT" \
  --filter="name~^${PREFIX}-" \
  --format="value(name)" 2>/dev/null | tr '\n' ' ')

if [ -n "$INSTANCES" ]; then
  echo "[teardown] deleting instances: $INSTANCES"
  # shellcheck disable=SC2086
  gcloud compute instances delete $INSTANCES \
    --project "$PROJECT" --zone "$ZONE" --quiet || true
else
  echo "[teardown] no instances to delete"
fi

# Release internal addresses (these are tied to the deleted instances
# but linger until explicitly freed).
ADDRS=$(gcloud compute addresses list \
  --project "$PROJECT" --regions "$REGION" \
  --filter="name~^${PREFIX}-server-" \
  --format="value(name)" 2>/dev/null | tr '\n' ' ')
if [ -n "$ADDRS" ]; then
  echo "[teardown] releasing internal addresses: $ADDRS"
  # shellcheck disable=SC2086
  gcloud compute addresses delete $ADDRS \
    --project "$PROJECT" --region "$REGION" --quiet || true
fi

# Verify zero instances remain matching the prefix.
REMAINING=$(gcloud compute instances list \
  --project "$PROJECT" \
  --filter="name~^${PREFIX}-" \
  --format="value(name)" 2>/dev/null | wc -l | tr -d ' ')
echo "[teardown] remaining instances matching ^${PREFIX}-: $REMAINING"

if [ "$REMAINING" -eq 0 ]; then
  echo "[teardown] OK: cluster fully torn down"
  exit 0
else
  echo "[teardown] WARN: $REMAINING instance(s) still present — investigate" >&2
  gcloud compute instances list \
    --project "$PROJECT" --filter="name~^${PREFIX}-" 2>&1 | head -20 >&2 || true
  exit 1
fi
