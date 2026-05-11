#!/usr/bin/env bash
#
# GCP startup script for zsbx Nomad WORKER nodes (Cloud-Hypervisor host).
#
# Runs as root on first boot. Idempotent.
#
# Responsibilities (in order):
#   1. apt deps: nomad, kvm, dnsmasq, virtiofsd build deps, python3
#   2. pull binaries from GCS:
#        - cloud-hypervisor (v51.1), ch-remote, virtiofsd
#        - vmlinuz, rootfs-slim.img.fp32 → rootfs-slim.img
#        - zeroship-sandbox (controller, from metadata `controller-object`)
#        - nomad-vm-wrapper.sh
#        - stress harness (stress_one.py, snapshot_stress.py, typed_id.py)
#   3. set up 12 taps on 10.99.10X.1/30  (X = vm index, see wrapper)
#   4. write Nomad client config pointing at the server fleet
#   5. write /etc/zeroship/{sandbox-token,sandbox-admin-token,sandbox-admin-token.env}
#   6. write the zsbx-ctl systemd unit with DB URL = postgres://postgres:<pw>@<pg-host>:5432/zeroship
#   7. start nomad.service, then zsbx-ctl.service
#   8. emit sentinel `[startup] zsbx-worker-ready`
#
# Instance metadata read (set by provision-gcp-cluster.sh):
#   - role:                must be "worker"
#   - server-ips:          comma-separated server private IPs
#   - datacenter:          Nomad datacenter (default zsbx-prod)
#   - pg-host:             private IP of the postgres server (server-1)
#   - pg-password:         postgres superuser password
#   - sandbox-token:       bearer for /sandboxes/*  (≥32 bytes)
#   - sandbox-admin-token: bearer for /admin/sandboxes/*
#   - artifact-bucket:     GCS bucket name (no `gs://`)
#   - controller-object:   GCS object name for the controller binary
#                          (e.g. zeroship-sandbox.snapshot-v6)
#   - vm-index-ceil:       int, default 12; number of taps to create
#   - snapshot-bucket:     GCS bucket for L2 snapshot storage
#                          (default = artifact-bucket; can be same/separate)
#
# Sentinel: the FINAL echo line:  `[startup] zsbx-worker-ready`

set -Eeuo pipefail

LOG=/var/log/zsbx-startup.log
# stdout/stderr is mirrored to the serial console by
# gce_metadata_script_runner (see serial port 1), so we just need to
# also persist to a file. Do NOT tee to /dev/kmsg — the metadata
# script runner's context has it as a non-printk FIFO that fails
# `tee` with EINVAL, prematurely SIGPIPE-killing the script.
exec > >(tee -a "$LOG") 2>&1
echo "[startup] $(date -u +%FT%TZ) zsbx worker startup begin"

md() {
  curl -sS --max-time 5 -H "Metadata-Flavor: Google" \
    "http://metadata.google.internal/computeMetadata/v1/instance/attributes/$1" \
    2>/dev/null || true
}

ROLE=$(md role)
if [ "$ROLE" != "worker" ]; then
  echo "[startup] FATAL: role=$ROLE (expected 'worker')" >&2
  exit 1
fi

SERVER_IPS=$(md server-ips)
DATACENTER=$(md datacenter); DATACENTER=${DATACENTER:-zsbx-prod}
PG_HOST=$(md pg-host)
PG_PASSWORD=$(md pg-password)
SANDBOX_TOKEN=$(md sandbox-token)
SANDBOX_ADMIN_TOKEN=$(md sandbox-admin-token)
ARTIFACT_BUCKET=$(md artifact-bucket)
CONTROLLER_OBJECT=$(md controller-object)
VM_INDEX_CEIL=$(md vm-index-ceil); VM_INDEX_CEIL=${VM_INDEX_CEIL:-12}
SNAPSHOT_BUCKET=$(md snapshot-bucket); SNAPSHOT_BUCKET=${SNAPSHOT_BUCKET:-$ARTIFACT_BUCKET}

: "${SERVER_IPS:?missing server-ips}"
: "${PG_HOST:?missing pg-host}"
: "${PG_PASSWORD:?missing pg-password}"
: "${SANDBOX_TOKEN:?missing sandbox-token}"
: "${SANDBOX_ADMIN_TOKEN:?missing sandbox-admin-token}"
: "${ARTIFACT_BUCKET:?missing artifact-bucket}"
: "${CONTROLLER_OBJECT:?missing controller-object}"

PRIVATE_IP=$(hostname -I | awk '{print $1}')
HOSTNAME=$(hostname)
echo "[startup] role=worker hostname=$HOSTNAME private_ip=$PRIVATE_IP pg=$PG_HOST ceil=$VM_INDEX_CEIL"

# Idempotency: if zsbx-ctl is active, treat as re-run.
if systemctl is-active --quiet zsbx-ctl 2>/dev/null \
   && systemctl is-active --quiet nomad 2>/dev/null; then
  echo "[startup] already provisioned; emitting sentinel"
  echo "[startup] zsbx-worker-ready"
  exit 0
fi

# ───── 1. apt deps ───────────────────────────────────────────────
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq \
  curl gnupg lsb-release ca-certificates unzip wget jq \
  qemu-kvm libvirt-clients bridge-utils iproute2 \
  python3 python3-pip apparmor-utils \
  >/dev/null

# HashiCorp Nomad
install -m 0755 -d /etc/apt/keyrings
if [ ! -f /etc/apt/keyrings/hashicorp.gpg ]; then
  curl -fsSL https://apt.releases.hashicorp.com/gpg | gpg --dearmor -o /etc/apt/keyrings/hashicorp.gpg
fi
echo "deb [signed-by=/etc/apt/keyrings/hashicorp.gpg] https://apt.releases.hashicorp.com $(lsb_release -cs) main" \
  > /etc/apt/sources.list.d/hashicorp.list
apt-get update -qq
apt-get install -y -qq nomad >/dev/null

# Confirm /dev/kvm
if [ ! -e /dev/kvm ]; then
  echo "[startup] FATAL: /dev/kvm missing — instance must be nested-virt-enabled" >&2
  exit 1
fi
chmod 0666 /dev/kvm

# ───── 2. pull binaries from GCS ────────────────────────────────
ART=/etc/zeroship
mkdir -p "$ART" /var/lib/zeroship/ch /var/zeroship/ch /var/zeroship/ch/users \
         /var/zeroship/ch/snapshots /opt/stress /var/log

gs_pull() {
  local src=$1 dst=$2 mode=${3:-0755}
  echo "[startup] pulling gs://$ARTIFACT_BUCKET/$src → $dst"
  for attempt in 1 2 3 4 5; do
    if gsutil -q cp "gs://$ARTIFACT_BUCKET/$src" "$dst"; then
      chmod "$mode" "$dst"
      return 0
    fi
    echo "[startup] gsutil cp failed (attempt $attempt); retry in 4s"
    sleep 4
  done
  echo "[startup] FATAL: gs://$ARTIFACT_BUCKET/$src after 5 attempts" >&2
  exit 1
}

gs_pull cloud-hypervisor.v51.1     /usr/local/bin/cloud-hypervisor 0755
gs_pull ch-remote.v51.1            /usr/local/bin/ch-remote        0755
gs_pull virtiofsd                  /usr/local/bin/virtiofsd        0755
gs_pull vmlinuz                    "$ART/vmlinuz"                  0644
gs_pull rootfs-slim.img.fp32       "$ART/rootfs-slim.img"          0644
gs_pull nomad-vm-wrapper.sh        "$ART/nomad-vm-wrapper.sh"      0755
gs_pull "$CONTROLLER_OBJECT"       /usr/local/bin/zeroship-sandbox 0755

# Stress harness (best-effort: a missing file is non-fatal for cluster
# bringup; the provisioner uses `gsutil cp` directly to push these too).
for f in stress_one.py snapshot_stress.py typed_id.py; do
  if gsutil -q stat "gs://$ARTIFACT_BUCKET/stress/$f" 2>/dev/null; then
    gsutil -q cp "gs://$ARTIFACT_BUCKET/stress/$f" "/opt/stress/$f"
    chmod 0755 "/opt/stress/$f"
  else
    echo "[startup] WARN: /opt/stress/$f not on GCS — skipping"
  fi
done

# Ensure the rootfs image is the slim/fp32 variant the wrapper
# expects (`$ZSBX_ARTIFACT_DIR/rootfs-slim.img`). gs_pull dropped it
# at $ART/rootfs-slim.img directly; nothing else to do here.

# Sanity check binaries are runnable.
/usr/local/bin/cloud-hypervisor --version | head -1 || {
  echo "[startup] FATAL: cloud-hypervisor refuses to run" >&2; exit 1
}
/usr/local/bin/zeroship-sandbox --help 2>&1 | head -1 || true

# ───── 3. taps (10.99.{100+idx}.0/30, idx in [1, CEIL]) ─────────
modprobe tun || true
# Enable forwarding so the VM can reach the GCP default GW. Even
# though sandboxes don't NEED inbound NAT for this test, the controller
# polls the agent over the per-VM /30 — that's host-to-VM, not
# internet-to-VM, so no SNAT is required.
sysctl -w net.ipv4.ip_forward=1 >/dev/null
sysctl -w net.ipv6.conf.all.forwarding=1 >/dev/null
cat > /etc/sysctl.d/99-zsbx.conf <<EOF
net.ipv4.ip_forward=1
net.ipv6.conf.all.forwarding=1
EOF

for idx in $(seq 1 "$VM_INDEX_CEIL"); do
  tap="zsbx-nm-$idx"
  host_ip="10.99.$((100 + idx)).1"
  if ! ip link show "$tap" >/dev/null 2>&1; then
    ip tuntap add "$tap" mode tap
    ip addr add "$host_ip/30" dev "$tap"
    ip link set "$tap" up
  fi
done
echo "[startup] $VM_INDEX_CEIL taps up"

# Persist tap creation across reboots — startup-script only runs once
# but if the VM reboots we don't want to lose the taps. Drop a oneshot.
cat > /usr/local/sbin/zsbx-taps-up.sh <<EOF
#!/usr/bin/env bash
set -e
sysctl -w net.ipv4.ip_forward=1 >/dev/null
for idx in \$(seq 1 $VM_INDEX_CEIL); do
  tap="zsbx-nm-\$idx"
  host_ip="10.99.\$((100 + idx)).1"
  if ! ip link show "\$tap" >/dev/null 2>&1; then
    ip tuntap add "\$tap" mode tap
    ip addr add "\$host_ip/30" dev "\$tap"
    ip link set "\$tap" up
  fi
done
EOF
chmod 0755 /usr/local/sbin/zsbx-taps-up.sh

cat > /etc/systemd/system/zsbx-taps.service <<EOF
[Unit]
Description=zsbx tap devices (idempotent)
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/zsbx-taps-up.sh
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable --now zsbx-taps.service >/dev/null

# ───── 4. nomad client ─────────────────────────────────────────
# NOTE: data_dir MUST be /opt/nomad/data — the sandbox controller
# hardcodes `NOMAD_ALLOC_ROOT = "/opt/nomad/data/alloc"` for its
# snapshot-handler `lookup_source_vm_ops`. Overriding to /var/lib/nomad
# silently breaks snapshot/wake (`api_socket not accessible`). See
# crates/sandbox/src/backend/nomad_ch.rs:120.
mkdir -p /etc/nomad.d /opt/nomad/data /var/log/nomad
chown -R nomad:nomad /opt/nomad/data /var/log/nomad

RETRY_JOIN=$(echo "$SERVER_IPS" | tr ',' '\n' | awk 'NF{printf "\"%s\",", $0}' | sed 's/,$//')

cat > /etc/nomad.d/nomad.hcl <<EOF
datacenter = "$DATACENTER"
data_dir   = "/opt/nomad/data"
log_level  = "INFO"
bind_addr  = "0.0.0.0"

advertise {
  http = "$PRIVATE_IP"
  rpc  = "$PRIVATE_IP"
  serf = "$PRIVATE_IP"
}

# This node is a client only.
server {
  enabled = false
}

client {
  enabled = true
  server_join {
    retry_join = [ $RETRY_JOIN ]
    retry_interval = "5s"
    retry_max      = 0
  }
  options = {
    "driver.raw_exec.enable" = "1"
    "user.blacklist"         = ""
  }
}
EOF

systemctl enable --now nomad
echo "[startup] nomad client started"

# Wait until this client registers with a leader.
for _ in $(seq 1 60); do
  if curl -sS --max-time 2 http://127.0.0.1:4646/v1/agent/self >/dev/null 2>&1; then
    break
  fi
  sleep 1
done

# ───── 5. token files (mode 0o400, owned by root) ───────────────
umask 077
printf '%s' "$SANDBOX_TOKEN"        > "$ART/sandbox-token"
printf '%s' "$SANDBOX_ADMIN_TOKEN"  > "$ART/sandbox-admin-token"
chmod 0400 "$ART/sandbox-token" "$ART/sandbox-admin-token"

# EnvironmentFile for systemd (sandbox token is read here; admin token
# is read from disk via SANDBOX_ADMIN_TOKEN_PATH).
cat > "$ART/sandbox-token.env" <<EOF
SANDBOX_TOKEN=$SANDBOX_TOKEN
EOF
chmod 0400 "$ART/sandbox-token.env"

# DB URL env file (per resolved-blocker pattern #8: metadata → env-file).
cat > "$ART/sandbox-db.env" <<EOF
SANDBOX_DATABASE_URL=postgres://postgres:$PG_PASSWORD@$PG_HOST:5432/zeroship
EOF
chmod 0400 "$ART/sandbox-db.env"
umask 022

# ───── 6. zsbx-ctl systemd unit ────────────────────────────────
# Mirrors stress/zsbx-ctl.service.template with these additions:
#   - SANDBOX_DATABASE_URL via EnvironmentFile (templated above)
#   - SANDBOX_PG_RUN_MIGRATIONS=1 on worker-1 only (the migrator)
#   - SANDBOX_SNAPSHOT_ENABLED=true + GCS L2 wiring (snapshot-v6 path)
#   - SANDBOX_ADMIN_TOKEN_PATH points at /etc/zeroship/sandbox-admin-token
WORKER_INDEX=$(echo "$HOSTNAME" | grep -oE '[0-9]+$' || echo 1)
IS_MIGRATOR=0
if [ "$WORKER_INDEX" = "1" ]; then
  IS_MIGRATOR=1
fi

cat > /etc/systemd/system/zsbx-ctl.service <<EOF
[Unit]
Description=zeroship-sandbox controller
After=nomad.service zsbx-taps.service network-online.target
Wants=nomad.service zsbx-taps.service network-online.target

[Service]
Type=simple
EnvironmentFile=$ART/sandbox-token.env
EnvironmentFile=$ART/sandbox-db.env

# Backend
Environment=SANDBOX_BACKEND=nomad-ch
Environment=SANDBOX_NOMAD_ADDR=http://127.0.0.1:4646
Environment=SANDBOX_NOMAD_DATACENTER=$DATACENTER
Environment=SANDBOX_NOMAD_CH_WRAPPER_PATH=$ART/nomad-vm-wrapper.sh
Environment=SANDBOX_NOMAD_CH_RUNTIME_DIR=/var/lib/zeroship/ch
Environment=SANDBOX_NOMAD_CH_HOST_STATE_DIR=/var/zeroship/ch
Environment=SANDBOX_NOMAD_CH_USER_HOME_ROOT=/var/zeroship/ch/users
Environment=SANDBOX_NOMAD_CH_VM_INDEX_FLOOR=1
Environment=SANDBOX_NOMAD_CH_VM_INDEX_CEIL=$VM_INDEX_CEIL
Environment=SANDBOX_NOMAD_CH_SUBNET_BASE_OCTET=99

# Wrapper inputs: point at the artifact dir holding vmlinuz + rootfs-slim.img.
# The wrapper reads ZSBX_ARTIFACT_DIR from its Nomad task env; the
# controller propagates SANDBOX_NOMAD_CH_RUNTIME_DIR there.
# (We additionally pre-seed the runtime dir with the artifacts so the
# wrapper's \`cd "\$ZSBX_ARTIFACT_DIR"\` finds vmlinuz + rootfs-slim.img.)

# Snapshot / restore (Phase B feature flag + L2 GCS)
Environment=SANDBOX_SNAPSHOT_ENABLED=true
Environment=SANDBOX_SNAPSHOT_L1_ROOT=/var/zeroship/ch/snapshots
Environment=SANDBOX_SNAPSHOT_USE_GCS=true
Environment=SANDBOX_SNAPSHOT_GCS_BUCKET=$SNAPSHOT_BUCKET

# Admin token file
Environment=SANDBOX_ADMIN_TOKEN_PATH=$ART/sandbox-admin-token

# pg migrations: run only on worker-1.
$( [ "$IS_MIGRATOR" = "1" ] && echo "Environment=SANDBOX_PG_RUN_MIGRATIONS=1" )

# API
Environment=SANDBOX_PORT=9091
Environment=RUST_LOG=info,zeroship_sandbox=info

ExecStart=/usr/local/bin/zeroship-sandbox
StandardOutput=append:/var/log/zeroship-sandbox.log
StandardError=append:/var/log/zeroship-sandbox.log
Restart=no

[Install]
WantedBy=multi-user.target
EOF

# Stage the artifact dir the wrapper will `cd` into.
ln -sf "$ART/vmlinuz"         /var/lib/zeroship/ch/vmlinuz
ln -sf "$ART/rootfs-slim.img" /var/lib/zeroship/ch/rootfs-slim.img

systemctl daemon-reload
systemctl enable --now zsbx-ctl.service

# ───── 7. wait for /livez=200 ───────────────────────────────────
echo "[startup] waiting for controller /livez=200 ..."
LIVEZ=0
for _ in $(seq 1 60); do
  if curl -sS --max-time 2 http://127.0.0.1:9091/livez | grep -q '"status":"ok"'; then
    LIVEZ=1; break
  fi
  sleep 1
done
if [ "$LIVEZ" -eq 0 ]; then
  echo "[startup] WARN: /livez did not 200 within 60s; controller may still come up"
  systemctl status zsbx-ctl --no-pager | tail -40 || true
  tail -40 /var/log/zeroship-sandbox.log 2>/dev/null || true
fi

# ───── 8. SENTINEL ──────────────────────────────────────────────
echo "[startup] zsbx-worker-ready"
echo "[startup] $(date -u +%FT%TZ) zsbx worker startup complete"
