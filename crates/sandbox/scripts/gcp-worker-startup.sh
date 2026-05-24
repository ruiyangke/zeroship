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
#        - vmlinuz, rootfs-slim.img.virtio-blk-v5 → rootfs-slim.img
#          (per the virtio-blk pivot — bug #20 cluster smoke 2026-05-23
#          showed v15 failures because the worker startup still pulled
#          the pre-pivot rootfs-slim.img.fp32, whose in-VM /sbin/init
#          mounts virtio-fs shares the wrapper no longer provides.
#          Bumped v3 → v4 for bug #22 fix: the new agent baked into v4
#          adds POST /_clock_resync so the controller can repair the
#          guest's frozen-at-snapshot CLOCK_REALTIME post-CH-restore.)
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
#   - server-ips:          newline-separated server private IPs.
#                          Delivered via --metadata-from-file (NOT
#                          --metadata=key=value); the dict form rejects
#                          comma-bearing values with "Bad syntax for
#                          dict arg" once SERVER_COUNT>1. (Bug #23 / B23.)
#   - datacenter:          Nomad datacenter (default zsbx-prod)
#   - pg-host:             private IP of the postgres server (server-1)
#   - pg-password:         postgres superuser password
#   - sandbox-token:       bearer for /sandboxes/*  (≥32 bytes)
#   - sandbox-admin-token: bearer for /admin/sandboxes/*
#   - artifact-bucket:     GCS bucket name (no `gs://`)
#   - controller-object:   GCS object name for the controller binary
#                          (e.g. zeroship-sandbox.snapshot-v30)
#   - vm-index-ceil:       int, default 12; number of taps to create
#   - snapshot-bucket:     GCS bucket for L2 snapshot storage
#                          (default = artifact-bucket; can be same/separate)
#   - install-ch-plugin-driver:
#                          "1" to install the Go-based nomad-driver-ch
#                          plugin alongside the bash wrapper and switch
#                          zsbx-ctl to `SANDBOX_TASK_DRIVER=ch_plugin`
#                          (T-8 cutover gate). Default unset → no-op,
#                          preserving the raw_exec wrapper path. Will be
#                          flipped on in the next worker provision once
#                          T-8b smoke validation confirms the cutover.
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
# T-8 cutover gate. Default "" → no-op; "1" installs the Go plugin
# driver and flips zsbx-ctl into ch_plugin jobspec mode.
INSTALL_CH_PLUGIN_DRIVER=$(md install-ch-plugin-driver); INSTALL_CH_PLUGIN_DRIVER=${INSTALL_CH_PLUGIN_DRIVER:-0}

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
gs_pull rootfs-slim.img.virtio-blk-v5 "$ART/rootfs-slim.img"          0644
gs_pull nomad-vm-wrapper.sh        "$ART/nomad-vm-wrapper.sh"      0755
gs_pull "$CONTROLLER_OBJECT"       /usr/local/bin/zeroship-sandbox 0755

# T-8 cutover gate: pull the Go-based ch_plugin driver alongside the
# bash wrapper. Both coexist until T-8b smoke confirms parity; until
# the install flag flips on, this block is a no-op so existing worker
# nodes (raw_exec wrapper path) are unaffected.
if [ "$INSTALL_CH_PLUGIN_DRIVER" = "1" ]; then
  echo "[startup] INSTALL_CH_PLUGIN_DRIVER=1 — installing nomad-driver-ch"
  mkdir -p /etc/zeroship/nomad-plugins
  gs_pull nomad-driver-ch.v11 /etc/zeroship/nomad-plugins/nomad-driver-ch 0755
  chown root:root /etc/zeroship/nomad-plugins/nomad-driver-ch
  # Surface the embedded gitSHA so we can confirm which build landed.
  /etc/zeroship/nomad-plugins/nomad-driver-ch --version || true

  # Tell Nomad where to find plugins. The HCL fragment is loaded
  # alongside /etc/nomad.d/nomad.hcl (Nomad concatenates everything
  # in /etc/nomad.d/*.hcl), so writing it BEFORE `systemctl enable
  # --now nomad` below means we don't need a restart afterwards.
  #
  # The explicit `plugin "nomad-driver-ch" { config {} }` stanza is
  # REQUIRED on Nomad 2.0.2 — `plugin_dir` alone makes the loader emit
  #   [WARN] agent.plugin_loader: plugin not referenced in the agent
  #                                configuration file, loading skipped
  # and skip the driver entirely. The empty `config {}` block is
  # mandatory; Nomad refuses to load plugins it doesn't see configured,
  # even with empty config. Confirmed via T-8b-smoke FAIL r1 — see
  # docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-smoke-r1.md.
  cat > /etc/nomad.d/plugin-dir.hcl <<EOF
plugin_dir = "/etc/zeroship/nomad-plugins"

plugin "nomad-driver-ch" {
  config {}
}
EOF
fi

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

# Ensure the rootfs image is the virtio-blk variant the wrapper
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

# server-ips arrives newline-separated via --metadata-from-file (B23
# fix). `tr ',\r' '\n\n'` also accepts the legacy comma form / strips
# CR so older provisioners and copy-pasted values still work.
RETRY_JOIN=$(echo "$SERVER_IPS" | tr ',\r' '\n\n' | awk 'NF{printf "\"%s\",", $0}' | sed 's/,$//')

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

# ───── 4b. ch driver-health gate (T-8b-prereqs-config) ──────────
# When INSTALL_CH_PLUGIN_DRIVER=1 we MUST confirm the ch driver is
# both detected and healthy before letting startup proceed. Without
# this gate `zsbx-worker-ready` was emitted even when the plugin
# loader silently skipped the driver (Nomad 2.0.2 WARN: plugin not
# referenced in agent config), so the failure only surfaced on the
# first sandbox-create — many minutes later, far from the actual
# cause. Surface plugin-load failures at provision time instead.
#
# Probe: `nomad node status -self -verbose` lists drivers as
#   <name>  <detected>  <healthy>  <message>  <time>
# We match `^ch\s+true\s+true` (whitespace-tolerant), with 10x 3s
# retries (30s total) before failing the worker startup. raw_exec
# path (INSTALL_CH_PLUGIN_DRIVER unset/0) is unchanged.
if [ "$INSTALL_CH_PLUGIN_DRIVER" = "1" ]; then
  echo "[startup] probing ch driver health (Detected=true, Healthy=true) ..."
  CH_OK=0
  for attempt in 1 2 3 4 5 6 7 8 9 10; do
    if nomad node status -self -verbose 2>/dev/null \
         | grep -E '^ch[[:space:]]+true[[:space:]]+true' >/dev/null; then
      CH_OK=1
      echo "[startup] ch driver healthy (attempt $attempt)"
      break
    fi
    echo "[startup] ch driver not yet healthy (attempt $attempt/10); retry in 3s"
    sleep 3
  done
  if [ "$CH_OK" -ne 1 ]; then
    echo "[startup] FATAL: ch driver did not reach Detected=true,Healthy=true within 30s" >&2
    echo "[startup] last node status:" >&2
    nomad node status -self -verbose 2>&1 | tail -40 >&2 || true
    echo "[startup] nomad agent logs (tail):" >&2
    journalctl -u nomad --no-pager -n 80 2>&1 | tail -80 >&2 || true
    exit 1
  fi
fi

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

# Persistence AEAD key (B21 fix / R5-S1 piggyback). `Persistence::from_env`
# requires `SANDBOX_PERSIST_AUTH=1` AND a file-mounted 32-byte AEAD key at
# `SANDBOX_AEAD_KEY_PATH` (round-6 H8: env-var sourcing forbidden because
# `/proc/<pid>/environ` leaks). Without the AEAD key, `from_env()` returns
# `Ok(None)`, `state.persist` is `None`, and `do_restore_inner`'s post-livez
# `register_restored` call falls into the warn-skip branch — every wake
# returns 200 but the state map gets no entry; downstream exec/stop/delete
# return "sandbox not found" and the vm_index allocator slot leaks. After
# B21 fix lands (with the corresponding `AppState::from_config` assertion
# from R5-S1), the controller will refuse to boot in that misconfigured
# shape, so this provisioning step is mandatory once `SNAPSHOT_ENABLED=true`.
#
# AEAD key file MUST be exactly 32 bytes with mode 0o400 (the controller
# `AeadKey::from_path` refuses anything else). Generated once at startup;
# idempotent so reboots reuse the same key (and any sealed records written
# under it stay readable). DO NOT log or echo the contents.
AEAD_KEY_PATH="$ART/sandbox-aead-key"
if [ ! -s "$AEAD_KEY_PATH" ]; then
  ( umask 077; head -c 32 /dev/urandom > "$AEAD_KEY_PATH" )
  chmod 0400 "$AEAD_KEY_PATH"
  echo "[startup] generated AEAD key at $AEAD_KEY_PATH (32 bytes, 0400)"
else
  # Re-tighten mode in case a previous run left it wider.
  chmod 0400 "$AEAD_KEY_PATH"
  echo "[startup] reusing AEAD key at $AEAD_KEY_PATH"
fi

# Snapshot AEAD root KEK (arch-r9 fail-CLOSED, R10-A1). Controller
# refuses to boot when `SNAPSHOT_ENABLED=true` + `SNAPSHOT_USE_GCS=true`
# but `SANDBOX_SNAPSHOT_ROOT_KEK_PATH` is unset — without it, guest RAM
# pages land in GCS in clear while the pg audit row stamps
# `snapshot_aead_dek_id="v1"`, an audit-trail-vs-reality gap. The KEK
# must be exactly 32 raw bytes, mode 0o400, owned by uid 0. Generated
# once at startup and reused idempotently across reboots so any DEKs
# previously wrapped under it stay decryptable. DO NOT log or echo
# the contents.
ROOT_KEK_PATH="$ART/snapshot-root-kek"
if [ ! -s "$ROOT_KEK_PATH" ]; then
  ( umask 077; head -c 32 /dev/urandom > "$ROOT_KEK_PATH" )
  chmod 0400 "$ROOT_KEK_PATH"
  echo "[startup] generated snapshot root KEK at $ROOT_KEK_PATH (32 bytes, 0400)"
else
  chmod 0400 "$ROOT_KEK_PATH"
  echo "[startup] reusing snapshot root KEK at $ROOT_KEK_PATH"
fi

mkdir -p /var/lib/zeroship/sandbox/sealed-records
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
# C-8 fix (T-8b-smoke-r9 cluster review): the Rust default fence is
# 120 s — over-conservative for the cluster-smoke workload, where the
# observed source-teardown completes well under 30 s. Smoke-r9
# confirmed C-7's 48 s wake retry budget but exposed that a 150 s
# source-teardown wall-time (host_fence 120 s + Nomad purge 30 s)
# exceeds it, surfacing as a clean 503 vm_index_unavailable. Capping
# the fence at 30 s here reduces source teardown to ~60 s total,
# fitting inside the 48 s retry budget with ~12 s residual headroom.
# Production deployments needing the conservative 120 s default can
# override via metadata; this is the cluster-smoke baseline.
Environment=SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30

# C-7-LT (T-8b-smoke-r12, controller v27): activate async wake response
# mode. The synchronous-response contract was empirically shown across
# r4-r11 to be under-budgetable inside the 60 s ntex client deadline
# (smoke-r11 measured a 60.166 s teardown vs a hard-capped 50 s wake
# budget — C-8c). Async mode returns 202 + {wake_id, poll_url} from
# POST /wake immediately; the state machine runs server-side without
# the client deadline binding it. Clients GET /wake/{wake_id} every
# 500ms-1s until terminal. Legacy sync remains available via ?sync=1
# (used only by older clients during cutover; new smoke flow polls).
Environment=SANDBOX_WAKE_RESPONSE_MODE=async

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

# Sealed-record persistence (B21 fix / R5-S1 piggyback). Required when
# SNAPSHOT_ENABLED=true — without it, the post-wake register_restored
# call falls into the warn-skip branch (state.persist=None), leaving
# the restored VM out of the backend's in-memory state map. The boot
# assertion in AppState::from_config refuses to start the controller
# if SNAPSHOT_ENABLED && persist.is_none(), so this triplet is now
# mandatory for production worker hosts.
Environment=SANDBOX_PERSIST_AUTH=1
Environment=SANDBOX_AEAD_KEY_PATH=$AEAD_KEY_PATH
Environment=SANDBOX_PERSIST_DIR=/var/lib/zeroship/sandbox
# Snapshot root KEK (32 bytes, mode 0o400). Required by the fail-CLOSED
# boot assertion when SNAPSHOT_ENABLED + SNAPSHOT_USE_GCS are both true.
Environment=SANDBOX_SNAPSHOT_ROOT_KEK_PATH=$ROOT_KEK_PATH

# Admin token file
Environment=SANDBOX_ADMIN_TOKEN_PATH=$ART/sandbox-admin-token

# pg migrations: run only on worker-1.
$( [ "$IS_MIGRATOR" = "1" ] && echo "Environment=SANDBOX_PG_RUN_MIGRATIONS=1" )

# T-8 cutover gate. With INSTALL_CH_PLUGIN_DRIVER=1 the controller
# routes through the Go nomad-driver-ch plugin (driver="ch", typed
# task_config); without it, the bash wrapper raw_exec path stays in
# effect. nomad_ch::build_nomad_job_json branches on this env.
$( [ "$INSTALL_CH_PLUGIN_DRIVER" = "1" ] && echo "Environment=SANDBOX_TASK_DRIVER=ch_plugin" )

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
