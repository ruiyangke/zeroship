#!/usr/bin/env bash
#
# GCP startup script for zsbx Nomad SERVER nodes.
#
# Runs as root on first boot (Compute Engine metadata `startup-script`).
# Idempotent: a re-run on an already-bootstrapped instance is a no-op.
#
# Responsibilities (in order):
#   1. apt-update + install deps (Nomad, postgresql-15 on `pg-host` only)
#   2. write /etc/nomad.d/nomad.hcl in `server` mode (bootstrap_expect=N)
#   3. start nomad.service
#   4. (server-1 only, the "pg-host"): bootstrap postgresql, create the
#      `zeroship` DB + role, apply the sandbox schema by running the
#      controller binary once with SANDBOX_PG_RUN_MIGRATIONS=1.
#   5. emit the sentinel `[startup] zsbx-server-ready` to the serial
#      console so the provisioner can detect completion via
#      `gcloud compute instances get-serial-port-output`.
#
# Instance metadata read (set by provision-gcp-cluster.sh):
#   - role:               must be "server"
#   - server-count:       integer, used for bootstrap_expect
#   - server-ips:         newline-separated server private IPs for
#                          retry_join. Delivered via --metadata-from-file
#                          (NOT --metadata=key=value); the dict form
#                          rejects comma-bearing values with "Bad syntax
#                          for dict arg" once SERVER_COUNT>1. (Bug #23 /
#                          B23.)
#   - datacenter:         Nomad datacenter name (default: "zsbx-prod")
#   - pg-host:            "1" on the host that runs postgres (server-1); ""/absent otherwise
#   - pg-password:        password for the `postgres` superuser AND the app role
#   - sandbox-token:      bearer token for /sandboxes/* (≥32 bytes)
#   - sandbox-admin-token:bearer token for /admin/sandboxes/*
#   - artifact-bucket:    gs://... bucket name (no `gs://` prefix)
#   - controller-object:  object name in artifact-bucket (e.g. zeroship-sandbox.snapshot-v29)
#
# Sentinel for the provisioner: the FINAL echo at the end of the
# script. Anything earlier means startup is still running.
#
# Cost: ~$0.038/hr for n2-standard-4 on-demand in asia-northeast3.

set -Eeuo pipefail

LOG=/var/log/zsbx-startup.log
# stdout/stderr is mirrored to the serial console by
# gce_metadata_script_runner; we just persist to a file too.
exec > >(tee -a "$LOG") 2>&1
echo "[startup] $(date -u +%FT%TZ) zsbx server startup begin"

# ───── metadata helpers ──────────────────────────────────────────
md() {
  # Read an instance-attribute metadata key. Returns empty string if
  # the key is absent (no `-f` so we don't fail on optional keys).
  curl -sS --max-time 5 -H "Metadata-Flavor: Google" \
    "http://metadata.google.internal/computeMetadata/v1/instance/attributes/$1" \
    2>/dev/null || true
}

ROLE=$(md role)
if [ "$ROLE" != "server" ]; then
  echo "[startup] FATAL: role=$ROLE (expected 'server'); aborting" >&2
  exit 1
fi

SERVER_COUNT=$(md server-count)
SERVER_IPS=$(md server-ips)
DATACENTER=$(md datacenter)
DATACENTER=${DATACENTER:-zsbx-prod}
PG_HOST_FLAG=$(md pg-host)
PG_PASSWORD=$(md pg-password)
SANDBOX_TOKEN=$(md sandbox-token)
SANDBOX_ADMIN_TOKEN=$(md sandbox-admin-token)
ARTIFACT_BUCKET=$(md artifact-bucket)
CONTROLLER_OBJECT=$(md controller-object)

: "${SERVER_COUNT:?missing server-count metadata}"
: "${SERVER_IPS:?missing server-ips metadata}"
: "${SANDBOX_TOKEN:?missing sandbox-token metadata}"
: "${SANDBOX_ADMIN_TOKEN:?missing sandbox-admin-token metadata}"
: "${ARTIFACT_BUCKET:?missing artifact-bucket metadata}"
: "${CONTROLLER_OBJECT:?missing controller-object metadata}"

PRIVATE_IP=$(hostname -I | awk '{print $1}')
HOSTNAME=$(hostname)
echo "[startup] role=server count=$SERVER_COUNT private_ip=$PRIVATE_IP hostname=$HOSTNAME pg=$PG_HOST_FLAG"

# Idempotency guard: if nomad.service is already enabled+active AND
# (on pg host) postgres is up, treat this as a re-run and skip.
if systemctl is-active --quiet nomad 2>/dev/null \
   && { [ "$PG_HOST_FLAG" != "1" ] || systemctl is-active --quiet postgresql 2>/dev/null; }; then
  echo "[startup] already provisioned; emitting sentinel"
  echo "[startup] zsbx-server-ready"
  exit 0
fi

# ───── 1. apt deps ───────────────────────────────────────────────
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq curl gnupg lsb-release ca-certificates unzip wget jq >/dev/null

# HashiCorp apt repo for Nomad.
install -m 0755 -d /etc/apt/keyrings
if [ ! -f /etc/apt/keyrings/hashicorp.gpg ]; then
  curl -fsSL https://apt.releases.hashicorp.com/gpg | gpg --dearmor -o /etc/apt/keyrings/hashicorp.gpg
fi
echo "deb [signed-by=/etc/apt/keyrings/hashicorp.gpg] https://apt.releases.hashicorp.com $(lsb_release -cs) main" \
  > /etc/apt/sources.list.d/hashicorp.list
apt-get update -qq
apt-get install -y -qq nomad >/dev/null
echo "[startup] nomad installed: $(nomad --version | head -1)"

# postgresql-15 on pg-host only (server-1).
if [ "$PG_HOST_FLAG" = "1" ]; then
  apt-get install -y -qq postgresql-15 >/dev/null
  echo "[startup] postgresql-15 installed"
fi

# ───── 2. nomad.hcl ──────────────────────────────────────────────
# Servers don't run client tasks so data_dir choice is less load-bearing,
# but we keep it consistent with workers (/opt/nomad/data) so operator
# muscle-memory matches and Nomad's default path is used everywhere.
mkdir -p /etc/nomad.d /opt/nomad/data /var/log/nomad
chown -R nomad:nomad /opt/nomad/data /var/log/nomad

# Build retry_join list (one quoted IP per element). server-ips arrives
# newline-separated via --metadata-from-file (B23 fix); `tr ',\r' '\n\n'`
# also accepts the legacy comma form / strips CR for resilience.
RETRY_JOIN=$(echo "$SERVER_IPS" | tr ',\r' '\n\n' | awk 'NF{printf "\"%s\",", $0}' | sed 's/,$//')

cat > /etc/nomad.d/nomad.hcl <<EOF
# Nomad server config (rendered by gcp-server-startup.sh)
datacenter = "$DATACENTER"
data_dir   = "/opt/nomad/data"
log_level  = "INFO"
bind_addr  = "0.0.0.0"

advertise {
  http = "$PRIVATE_IP"
  rpc  = "$PRIVATE_IP"
  serf = "$PRIVATE_IP"
}

server {
  enabled          = true
  bootstrap_expect = $SERVER_COUNT
  server_join {
    retry_join = [ $RETRY_JOIN ]
    retry_interval = "5s"
    retry_max      = 0
  }

  # Bug-#10 fix (2026-05-22). Nomad ignores task-level MemoryMaxMB
  # unless memory_oversubscription_enabled = true at the scheduler
  # level. Without this, the controller's 2 × MemoryMB ceiling
  # (bug-#9 fix) is silently dropped and the task's cgroup
  # memory.max stays at MemoryMB (1024). CH v51.1 mmap-faults the
  # full guest RAM during snapshot/restore and gets oom-killed at
  # ~1 GB shmem-rss before /livez is reachable. Enabling this lets
  # the controller's MemoryMaxMB ceiling flow through to memory.high.
  default_scheduler_config {
    memory_oversubscription_enabled = true
  }
}

# Servers do NOT run client tasks. Workloads land on the worker nodes.
client {
  enabled = false
}

# No ACLs / TLS for this validation cluster — the GCP firewall fences
# 4646/4647/4648 to the cluster network only.
EOF

systemctl enable --now nomad
echo "[startup] nomad.service started"

# ───── 3. wait for nomad HTTP API ────────────────────────────────
for _ in $(seq 1 60); do
  if curl -sS --max-time 2 http://127.0.0.1:4646/v1/status/leader >/dev/null 2>&1; then
    break
  fi
  sleep 1
done

# ───── 3.5. enable memory oversubscription via API ───────────────
# Belt-and-suspenders for the HCL default_scheduler_config above:
# different Nomad versions parse the block at different layers and
# some need an explicit operator API call to flip the flag. Idempotent.
# Only the leader can write scheduler config; non-leaders 4xx, which we
# swallow because the leader handles it.
for _ in $(seq 1 30); do
  LEADER=$(curl -sS --max-time 2 http://127.0.0.1:4646/v1/status/leader 2>/dev/null | tr -d '"')
  if [ -n "$LEADER" ]; then
    curl -sS -X POST -H 'Content-Type: application/json' \
      -d '{"MemoryOversubscriptionEnabled": true, "SchedulerAlgorithm": "binpack"}' \
      "http://127.0.0.1:4646/v1/operator/scheduler/configuration" \
      >/dev/null 2>&1 || true
    break
  fi
  sleep 1
done
echo "[startup] scheduler config: memory_oversubscription_enabled requested"

# ───── 4. postgres bootstrap (pg-host only) ──────────────────────
if [ "$PG_HOST_FLAG" = "1" ]; then
  echo "[startup] bootstrapping postgres on $HOSTNAME"
  : "${PG_PASSWORD:?missing pg-password metadata on pg-host}"

  # Listen on all interfaces so worker VMs can reach it.
  PG_CONF=/etc/postgresql/15/main/postgresql.conf
  PG_HBA=/etc/postgresql/15/main/pg_hba.conf
  sed -i "s/^#\?listen_addresses.*/listen_addresses = '*'/" "$PG_CONF"
  # Allow auth from the cluster /20 (default GCP subnet 10.178.0.0/20 in apnortheast3).
  # We add a broad 10.0.0.0/8 rule and rely on the GCP firewall to gate 5432.
  if ! grep -q "host all all 10.0.0.0/8 md5" "$PG_HBA"; then
    echo "host all all 10.0.0.0/8 md5" >> "$PG_HBA"
  fi
  systemctl restart postgresql

  # Set postgres superuser password + create the zeroship DB.
  sudo -u postgres psql -v ON_ERROR_STOP=1 <<SQL
ALTER USER postgres WITH PASSWORD '$PG_PASSWORD';
SELECT 'CREATE DATABASE zeroship'
WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = 'zeroship')\gexec
SQL

  # Migrations: we let the controller apply its own schema with
  # SANDBOX_PG_RUN_MIGRATIONS=1 on first run from a worker.
  # Nothing to do here — the migration is idempotent on the worker side.
  echo "[startup] postgres ready: $(sudo -u postgres psql -tA -c 'SELECT version();' | head -1)"
fi

# ───── 5. SENTINEL ───────────────────────────────────────────────
echo "[startup] zsbx-server-ready"
echo "[startup] $(date -u +%FT%TZ) zsbx server startup complete"
