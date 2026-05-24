#!/usr/bin/env bash
#
# Provision a GCP cluster for zsbx snapshot-restore validation.
#
# Layout (defaults are smoke-test sized — override via env vars):
#   - $SERVER_COUNT × n2-standard-4 Nomad servers (server-1 also runs postgres)
#   - $WORKER_COUNT × n2-standard-32 Nomad workers (CH host, /dev/kvm needed)
#   - One VPC subnet shared by both pools
#   - Firewall rule: open 4646/4647/4648 (Nomad), 5432 (postgres),
#                    9091 (controller), 22 (SSH) within the cluster network
#   - Each instance gets the right startup-script as metadata; the
#     script self-bootstraps then emits a sentinel on the serial console.
#
# Idempotent: re-running with the same names skips already-existing
# resources. Use `gcloud compute instances delete ...` (or the
# companion `teardown-gcp-cluster.sh`) to clean up.
#
# Hard cost ceiling: $30 — see AGENTS / task brief. Caller is expected
# to budget runtime accordingly.
#
# Required env (or fall back to defaults shown):
#   PROJECT          (gcloud project)               default: $(gcloud config get-value project)
#   REGION                                          default: asia-northeast3
#   ZONE                                            default: asia-northeast3-a
#   PREFIX           (instance-name prefix)         default: zsbx-prod
#   SERVER_COUNT                                    default: 3
#   WORKER_COUNT                                    default: 5
#   SERVER_MACHINE                                  default: n2-standard-4
#   WORKER_MACHINE                                  default: n2-standard-32
#   ARTIFACT_BUCKET  (no gs:// prefix)              default: suger-dev-zsbx-artifacts
#   CONTROLLER_OBJECT                               default: zeroship-sandbox.snapshot-v36
#   SNAPSHOT_BUCKET  (L2 store)                     default: $ARTIFACT_BUCKET
#   VM_INDEX_CEIL    (taps per worker)              default: 12
#   PG_PASSWORD                                     default: auto-generated, written to /tmp/.zsbx-pg.pw
#   SANDBOX_TOKEN                                   default: auto-generated 48-byte base64
#   SANDBOX_ADMIN_TOKEN                             default: auto-generated 48-byte base64
#
# Network image: Debian 12 (`debian-12-bookworm-v...`) — debian-cloud project.
# Nested virt: enabled via `--enable-nested-virtualization` on workers
# (required for /dev/kvm inside the n2 VM).

set -Eeuo pipefail

PROJECT=${PROJECT:-$(gcloud config get-value project 2>/dev/null || true)}
: "${PROJECT:?gcloud project not set}"

REGION=${REGION:-asia-northeast3}
ZONE=${ZONE:-asia-northeast3-a}
PREFIX=${PREFIX:-zsbx-prod}
SERVER_COUNT=${SERVER_COUNT:-3}
WORKER_COUNT=${WORKER_COUNT:-5}
SERVER_MACHINE=${SERVER_MACHINE:-n2-standard-4}
WORKER_MACHINE=${WORKER_MACHINE:-n2-standard-32}
ARTIFACT_BUCKET=${ARTIFACT_BUCKET:-suger-dev-zsbx-artifacts}
CONTROLLER_OBJECT=${CONTROLLER_OBJECT:-zeroship-sandbox.snapshot-v36}
SNAPSHOT_BUCKET=${SNAPSHOT_BUCKET:-$ARTIFACT_BUCKET}
VM_INDEX_CEIL=${VM_INDEX_CEIL:-12}
DATACENTER=${DATACENTER:-$PREFIX}
# Optional extra worker metadata, comma-separated key=value pairs.
# Appended to the worker --metadata line as-is. Empty by default.
# Example: EXTRA_WORKER_METADATA="install-ch-plugin-driver=1"
EXTRA_WORKER_METADATA=${EXTRA_WORKER_METADATA:-}

NETWORK=${NETWORK:-${PREFIX}-net}
SUBNET=${SUBNET:-${PREFIX}-subnet}
FIREWALL_INTERNAL=${FIREWALL_INTERNAL:-${PREFIX}-fw-internal}
FIREWALL_SSH=${FIREWALL_SSH:-${PREFIX}-fw-ssh}

HERE=$(cd "$(dirname "$0")" && pwd)
SERVER_STARTUP="$HERE/gcp-server-startup.sh"
WORKER_STARTUP="$HERE/gcp-worker-startup.sh"

[ -r "$SERVER_STARTUP" ] || { echo "FATAL: $SERVER_STARTUP not found" >&2; exit 1; }
[ -r "$WORKER_STARTUP" ] || { echo "FATAL: $WORKER_STARTUP not found" >&2; exit 1; }

# Token / password generation. Tokens persist across re-runs of the
# script so a redeploy uses the same credentials (consistent with
# postgres + already-running controllers).
gen_token() { head -c 48 /dev/urandom | base64 | tr -d '\n=' | head -c 48; }

PG_PASSWORD_FILE=/tmp/.zsbx-pg.pw
SANDBOX_TOKEN_FILE=/tmp/.zsbx-sandbox.tok
SANDBOX_ADMIN_TOKEN_FILE=/tmp/.zsbx-admin.tok

if [ -z "${PG_PASSWORD:-}" ]; then
  if [ -f "$PG_PASSWORD_FILE" ]; then
    PG_PASSWORD=$(cat "$PG_PASSWORD_FILE")
  else
    PG_PASSWORD=$(gen_token)
    umask 077; printf '%s' "$PG_PASSWORD" > "$PG_PASSWORD_FILE"; umask 022
  fi
fi
if [ -z "${SANDBOX_TOKEN:-}" ]; then
  if [ -f "$SANDBOX_TOKEN_FILE" ]; then
    SANDBOX_TOKEN=$(cat "$SANDBOX_TOKEN_FILE")
  else
    SANDBOX_TOKEN=$(gen_token)
    umask 077; printf '%s' "$SANDBOX_TOKEN" > "$SANDBOX_TOKEN_FILE"; umask 022
  fi
fi
if [ -z "${SANDBOX_ADMIN_TOKEN:-}" ]; then
  if [ -f "$SANDBOX_ADMIN_TOKEN_FILE" ]; then
    SANDBOX_ADMIN_TOKEN=$(cat "$SANDBOX_ADMIN_TOKEN_FILE")
  else
    SANDBOX_ADMIN_TOKEN=$(gen_token)
    umask 077; printf '%s' "$SANDBOX_ADMIN_TOKEN" > "$SANDBOX_ADMIN_TOKEN_FILE"; umask 022
  fi
fi

echo "[provision] project=$PROJECT region=$REGION zone=$ZONE prefix=$PREFIX"
echo "[provision] servers=$SERVER_COUNT × $SERVER_MACHINE | workers=$WORKER_COUNT × $WORKER_MACHINE"
echo "[provision] artifact-bucket=$ARTIFACT_BUCKET controller=$CONTROLLER_OBJECT"
echo "[provision] secrets cached in /tmp/.zsbx-*.pw|.tok"

# ────────── Network + subnet + firewall (idempotent) ──────────
ensure_network() {
  if ! gcloud compute networks describe "$NETWORK" --project "$PROJECT" --quiet >/dev/null 2>&1; then
    echo "[provision] creating network $NETWORK"
    gcloud compute networks create "$NETWORK" \
      --project "$PROJECT" \
      --subnet-mode=custom \
      --bgp-routing-mode=regional \
      --mtu=1460 \
      --quiet
  fi
  if ! gcloud compute networks subnets describe "$SUBNET" \
       --project "$PROJECT" --region "$REGION" --quiet >/dev/null 2>&1; then
    echo "[provision] creating subnet $SUBNET (10.178.0.0/20)"
    gcloud compute networks subnets create "$SUBNET" \
      --project "$PROJECT" \
      --network "$NETWORK" \
      --region "$REGION" \
      --range 10.178.0.0/20 \
      --quiet
  fi
}

ensure_firewall() {
  if ! gcloud compute firewall-rules describe "$FIREWALL_INTERNAL" \
       --project "$PROJECT" --quiet >/dev/null 2>&1; then
    echo "[provision] creating firewall $FIREWALL_INTERNAL (intra-cluster)"
    gcloud compute firewall-rules create "$FIREWALL_INTERNAL" \
      --project "$PROJECT" \
      --network "$NETWORK" \
      --direction=INGRESS \
      --action=ALLOW \
      --source-ranges=10.178.0.0/20 \
      --rules=tcp:22,tcp:4646,tcp:4647,tcp:4648,udp:4648,tcp:5432,tcp:9091,tcp:7777 \
      --quiet
  fi
  if ! gcloud compute firewall-rules describe "$FIREWALL_SSH" \
       --project "$PROJECT" --quiet >/dev/null 2>&1; then
    echo "[provision] creating firewall $FIREWALL_SSH (SSH from IAP)"
    # IAP TCP-forwarding range 35.235.240.0/20 covers `gcloud ssh`.
    gcloud compute firewall-rules create "$FIREWALL_SSH" \
      --project "$PROJECT" \
      --network "$NETWORK" \
      --direction=INGRESS \
      --action=ALLOW \
      --source-ranges=35.235.240.0/20 \
      --rules=tcp:22 \
      --quiet
  fi
}

ensure_network
ensure_firewall

# ────────── Compute server private-IP plan ──────────
# We reserve a deterministic /20 range for the servers (10.178.0.10+i)
# so worker startup metadata can reference the IPs before the workers
# boot — we create servers first, read their actual IPs, then create
# workers with metadata pointing at those IPs.
SERVER_NAMES=()
for i in $(seq 1 "$SERVER_COUNT"); do
  SERVER_NAMES+=("${PREFIX}-server-${i}")
done
WORKER_NAMES=()
for i in $(seq 1 "$WORKER_COUNT"); do
  WORKER_NAMES+=("${PREFIX}-worker-${i}")
done

# ────────── Server creation ──────────
DEBIAN_IMAGE_FAMILY=debian-12
DEBIAN_IMAGE_PROJECT=debian-cloud

# Pre-stage: we need the server IPs to template into each server's
# `server-ips` metadata (servers retry-join each other). We create
# servers one-by-one with placeholder metadata, then re-read assigned
# IPs and re-apply final metadata so retry-join converges.
#
# Simpler alternative: reserve static internal addresses for each
# server first. That's what we do — gcloud supports
# `compute addresses create --subnet=$SUBNET --addresses=10.178.0.10`.
SERVER_IPS=()
for i in $(seq 1 "$SERVER_COUNT"); do
  addr_name="${PREFIX}-server-${i}-ip"
  addr_value="10.178.0.$((9 + i))"   # 10.178.0.10, .11, .12 ...
  if ! gcloud compute addresses describe "$addr_name" \
       --project "$PROJECT" --region "$REGION" --quiet >/dev/null 2>&1; then
    echo "[provision] reserving internal IP $addr_value for ${SERVER_NAMES[i-1]}"
    gcloud compute addresses create "$addr_name" \
      --project "$PROJECT" \
      --region "$REGION" \
      --subnet "$SUBNET" \
      --addresses "$addr_value" \
      --purpose=GCE_ENDPOINT \
      --quiet
  fi
  SERVER_IPS+=("$addr_value")
done
# Server IPs are delivered via --metadata-from-file (newline-separated)
# instead of --metadata (comma-separated). gcloud parses --metadata as
# a key=value,key=value,... dict — any value containing commas (e.g.,
# the multi-IP list when SERVER_COUNT>1) is mis-parsed and the next
# token is rejected with `Bad syntax for dict arg: [<ip>]`. The
# file-backed form has no delimiter constraint. Both startup scripts
# consume the value through `tr ',' '\n'` already, so they accept
# newline-separated input unchanged. (Bug #23, B23 fix.)
SERVER_IPS_FILE=/tmp/zsbx-nomad-server-ips.txt
printf '%s\n' "${SERVER_IPS[@]}" > "$SERVER_IPS_FILE"
SERVER_IPS_CSV=$(IFS=, ; echo "${SERVER_IPS[*]}")   # log-only
echo "[provision] server IPs: $SERVER_IPS_CSV (file: $SERVER_IPS_FILE)"
PG_HOST_IP="${SERVER_IPS[0]}"

create_server() {
  local name=$1
  local idx=$2
  local addr_name="${PREFIX}-server-${idx}-ip"
  local pg_flag=""
  if [ "$idx" = "1" ]; then pg_flag="1"; fi

  if gcloud compute instances describe "$name" \
       --project "$PROJECT" --zone "$ZONE" --quiet >/dev/null 2>&1; then
    echo "[provision] $name already exists; skipping"
    return 0
  fi
  echo "[provision] creating $name ($SERVER_MACHINE)"
  gcloud compute instances create "$name" \
    --project "$PROJECT" \
    --zone "$ZONE" \
    --machine-type "$SERVER_MACHINE" \
    --image-family "$DEBIAN_IMAGE_FAMILY" \
    --image-project "$DEBIAN_IMAGE_PROJECT" \
    --boot-disk-size 30GB \
    --boot-disk-type pd-balanced \
    --network "$NETWORK" \
    --subnet "$SUBNET" \
    --private-network-ip "$addr_name" \
    --scopes=storage-ro,logging-write,monitoring-write \
    --metadata-from-file "startup-script=$SERVER_STARTUP,server-ips=$SERVER_IPS_FILE" \
    --metadata \
      "role=server,server-count=$SERVER_COUNT,datacenter=$DATACENTER,pg-host=$pg_flag,pg-password=$PG_PASSWORD,sandbox-token=$SANDBOX_TOKEN,sandbox-admin-token=$SANDBOX_ADMIN_TOKEN,artifact-bucket=$ARTIFACT_BUCKET,controller-object=$CONTROLLER_OBJECT" \
    --quiet >/dev/null
}

for i in $(seq 1 "$SERVER_COUNT"); do
  create_server "${SERVER_NAMES[i-1]}" "$i"
done

# ────────── Worker creation ──────────
create_worker() {
  local name=$1
  if gcloud compute instances describe "$name" \
       --project "$PROJECT" --zone "$ZONE" --quiet >/dev/null 2>&1; then
    echo "[provision] $name already exists; skipping"
    return 0
  fi
  echo "[provision] creating $name ($WORKER_MACHINE, nested-virt enabled)"
  local meta="role=worker,datacenter=$DATACENTER,pg-host=$PG_HOST_IP,pg-password=$PG_PASSWORD,sandbox-token=$SANDBOX_TOKEN,sandbox-admin-token=$SANDBOX_ADMIN_TOKEN,artifact-bucket=$ARTIFACT_BUCKET,controller-object=$CONTROLLER_OBJECT,vm-index-ceil=$VM_INDEX_CEIL,snapshot-bucket=$SNAPSHOT_BUCKET"
  if [ -n "$EXTRA_WORKER_METADATA" ]; then
    meta="${meta},${EXTRA_WORKER_METADATA}"
    echo "[provision] extra worker metadata: $EXTRA_WORKER_METADATA"
  fi
  gcloud compute instances create "$name" \
    --project "$PROJECT" \
    --zone "$ZONE" \
    --machine-type "$WORKER_MACHINE" \
    --image-family "$DEBIAN_IMAGE_FAMILY" \
    --image-project "$DEBIAN_IMAGE_PROJECT" \
    --boot-disk-size 80GB \
    --boot-disk-type pd-balanced \
    --network "$NETWORK" \
    --subnet "$SUBNET" \
    --enable-nested-virtualization \
    --scopes=storage-rw,logging-write,monitoring-write \
    --metadata-from-file "startup-script=$WORKER_STARTUP,server-ips=$SERVER_IPS_FILE" \
    --metadata "$meta" \
    --quiet >/dev/null
}

for n in "${WORKER_NAMES[@]}"; do
  create_worker "$n"
done

# ────────── Wait for sentinels ──────────
wait_sentinel() {
  local name=$1
  local sentinel=$2
  local budget_secs=${3:-900}   # 15 min default per host
  local elapsed=0
  echo "[provision] waiting for sentinel '$sentinel' on $name (budget ${budget_secs}s)"
  while [ "$elapsed" -lt "$budget_secs" ]; do
    if gcloud compute instances get-serial-port-output "$name" \
         --project "$PROJECT" --zone "$ZONE" --port=1 --quiet 2>/dev/null \
         | grep -q "$sentinel"; then
      echo "[provision] sentinel hit on $name (${elapsed}s)"
      return 0
    fi
    sleep 15
    elapsed=$((elapsed + 15))
  done
  echo "[provision] TIMEOUT: sentinel not seen on $name after ${budget_secs}s" >&2
  # Tail last 200 lines of the serial log to help debug.
  gcloud compute instances get-serial-port-output "$name" \
    --project "$PROJECT" --zone "$ZONE" --port=1 --quiet 2>/dev/null | tail -200 \
    >&2 || true
  return 1
}

echo "[provision] waiting for server sentinels"
for n in "${SERVER_NAMES[@]}"; do
  wait_sentinel "$n" "zsbx-server-ready" 900
done

echo "[provision] waiting for worker sentinels"
for n in "${WORKER_NAMES[@]}"; do
  wait_sentinel "$n" "zsbx-worker-ready" 1200   # workers do more
done

# ────────── Summary ──────────
echo "[provision] cluster up. Summary:"
gcloud compute instances list \
  --project "$PROJECT" \
  --filter="name~^${PREFIX}-" \
  --format="table(name,zone.basename(),machineType.basename(),networkInterfaces[0].networkIP:label=PRIVATE_IP,status)"

cat <<EOF

[provision] credentials:
  pg password         /tmp/.zsbx-pg.pw
  sandbox token       /tmp/.zsbx-sandbox.tok
  sandbox admin token /tmp/.zsbx-admin.tok

[provision] SSH:
  gcloud compute ssh ${PREFIX}-worker-1 --zone=$ZONE --tunnel-through-iap

[provision] teardown:
  $HERE/teardown-gcp-cluster.sh
EOF
