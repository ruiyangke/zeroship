#!/usr/bin/env bash
#
# Nomad raw_exec wrapper — launches one Cloud-Hypervisor microVM and
# ties its lifetime to this script. When Nomad sends SIGTERM (on `nomad
# job stop` or alloc reschedule), the trap below tears everything
# down cleanly: virtiofsd × 3 + cloud-hypervisor.
#
# This is the controller-shipped wrapper; the controller writes a
# Nomad job spec whose `Config.command` points at this path. The
# script is committed alongside the Rust crate so the wire contract
# (env vars + virtiofs tags + IP plumbing) is visible in source.
#
# Inputs (all required, set by zeroship-sandbox in the Nomad job env):
#   ZSBX_VM_INDEX        small int → tap zsbx-nm-$IDX, IP 10.99.$((100+IDX)).2
#   ZSBX_HERE            artifact dir; must contain vmlinuz + rootfs-slim.img
#   ZSBX_RUNTIME         per-allocation working dir (Nomad sets NOMAD_TASK_DIR)
#   ZSBX_KEYS_DIR        host dir holding controller-pubkey  (virtiofs tag=keys)
#   ZSBX_WORKSPACE_DIR   host dir for the project workspace  (virtiofs tag=workspace)
#   ZSBX_USER_HOME_DIR   host dir for the per-user $HOME      (virtiofs tag=userhome)
#
# The host operator is responsible for pre-provisioning:
#   - tap device `zsbx-nm-$IDX` in the /30 subnet 10.99.$((100+IDX)).0/30
#     (host=.1, VM=.2, no gateway, broadcast at .3)
#   - cloud-hypervisor + virtiofsd installed and on PATH
#   - vmlinuz built with CONFIG_IP_PNP=y at "$ZSBX_HERE/vmlinuz"
#   - rootfs-slim.img at "$ZSBX_HERE/rootfs-slim.img" with an /sbin/init
#     that mounts the three virtiofs tags (keys, workspace, userhome) and
#     execs /usr/local/bin/sandbox-agent
#
# This wrapper does NOT provision tap devices or kernels — that's
# fleet-level setup, not per-sandbox.

set -euo pipefail

: "${ZSBX_VM_INDEX:?missing ZSBX_VM_INDEX}"
: "${ZSBX_HERE:?missing ZSBX_HERE}"
: "${ZSBX_RUNTIME:?missing ZSBX_RUNTIME}"
: "${ZSBX_KEYS_DIR:?missing ZSBX_KEYS_DIR}"
: "${ZSBX_WORKSPACE_DIR:?missing ZSBX_WORKSPACE_DIR}"
: "${ZSBX_USER_HOME_DIR:?missing ZSBX_USER_HOME_DIR}"

cd "$ZSBX_HERE"

TAP=zsbx-nm-${ZSBX_VM_INDEX}
SUBNET_IDX=$((100 + ZSBX_VM_INDEX))   # 10.99.101.2 for index=1, etc.
VM_IP=10.99.${SUBNET_IDX}.2
HOST_IP=10.99.${SUBNET_IDX}.1
MAC=$(printf '12:34:56:78:9b:%02x' "$ZSBX_VM_INDEX")

API_SOCK="$ZSBX_RUNTIME/ch.sock"
VFS_KEYS_SOCK="$ZSBX_RUNTIME/vfs-keys.sock"
VFS_WS_SOCK="$ZSBX_RUNTIME/vfs-ws.sock"
VFS_HOME_SOCK="$ZSBX_RUNTIME/vfs-home.sock"
DISK="$ZSBX_RUNTIME/rootfs.img"

# The workspace + user-home dirs are owned by the controller; the
# wrapper only ensures they exist as a defensive measure (the
# controller mkdir -p's them before submitting the job, so this is a
# belt-and-braces no-op in the happy path).
mkdir -p "$ZSBX_KEYS_DIR" "$ZSBX_WORKSPACE_DIR" "$ZSBX_USER_HOME_DIR"

# Per-allocation copy of the rootfs (so concurrent jobs don't share
# state — each VM writes its own rootfs in /, which is r/w).
# `--reflink=auto` is fast on btrfs/xfs; falls back to a normal copy
# elsewhere.
[ -f "$DISK" ] || cp --reflink=auto "$ZSBX_HERE/rootfs-slim.img" "$DISK"

# Clean any stale sockets from a crashed prior run (Nomad gives us a
# fresh NOMAD_TASK_DIR per alloc, so this should already be empty,
# but defensive).
rm -f "$API_SOCK" "$VFS_KEYS_SOCK" "$VFS_WS_SOCK" "$VFS_HOME_SOCK"

echo "[wrapper] index=$ZSBX_VM_INDEX  tap=$TAP  vm_ip=$VM_IP  host_ip=$HOST_IP"
echo "[wrapper] keys=$ZSBX_KEYS_DIR  workspace=$ZSBX_WORKSPACE_DIR  userhome=$ZSBX_USER_HOME_DIR"

# Spawn virtiofsd × 3, one per share. `cache=auto` — virtiofsd will
# use writeback caching if the kernel supports it. We background each
# and capture its PID for the cleanup trap.
virtiofsd --socket-path="$VFS_KEYS_SOCK" --shared-dir="$ZSBX_KEYS_DIR"      --cache=auto \
    > "$ZSBX_RUNTIME/vfs-keys.log" 2>&1 &
VFS_KEYS_PID=$!

virtiofsd --socket-path="$VFS_WS_SOCK"   --shared-dir="$ZSBX_WORKSPACE_DIR" --cache=auto \
    > "$ZSBX_RUNTIME/vfs-ws.log" 2>&1 &
VFS_WS_PID=$!

virtiofsd --socket-path="$VFS_HOME_SOCK" --shared-dir="$ZSBX_USER_HOME_DIR" --cache=auto \
    > "$ZSBX_RUNTIME/vfs-home.log" 2>&1 &
VFS_HOME_PID=$!

# Cleanup trap. Any signal (or the script exits naturally on CH
# termination) → kill all three virtiofsd processes plus CH itself.
# SIGTERM first with a brief grace, then SIGKILL.
cleanup() {
  echo "[wrapper] cleaning up (vfs $VFS_KEYS_PID/$VFS_WS_PID/$VFS_HOME_PID, ch ${CH_PID-?})"
  [ -n "${CH_PID-}" ] && kill -TERM "$CH_PID" 2>/dev/null || true
  kill -TERM "$VFS_KEYS_PID" "$VFS_WS_PID" "$VFS_HOME_PID" 2>/dev/null || true
  sleep 0.5
  [ -n "${CH_PID-}" ] && kill -KILL "$CH_PID" 2>/dev/null || true
  kill -KILL "$VFS_KEYS_PID" "$VFS_WS_PID" "$VFS_HOME_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# Wait for each virtiofsd UDS to appear before launching CH.
# A few hundred ms is typical; bound to ~1 s.
for sock in "$VFS_KEYS_SOCK" "$VFS_WS_SOCK" "$VFS_HOME_SOCK"; do
  for _ in $(seq 1 50); do [ -S "$sock" ] && break; sleep 0.02; done
done

# Spawn cloud-hypervisor in the background so we can capture its PID
# for the cleanup trap.
#
# Important CH quirk: `--fs` takes ALL fs arguments as ONE space-
# separated token, NOT one per flag. Listing them as separate `--fs`
# args makes CH only register the last one. Same for `--disk` etc.
cloud-hypervisor \
  --api-socket "$API_SOCK" \
  --kernel    vmlinuz \
  --cmdline   "console=ttyS0 root=/dev/vda rw init=/sbin/init reboot=t panic=1 ip=${VM_IP}::${HOST_IP}:255.255.255.252::eth0:none" \
  --disk      path="$DISK",readonly=off,direct=off,image_type=raw \
  --net       tap="$TAP",mac="$MAC" \
  --fs        tag=keys,socket="$VFS_KEYS_SOCK" tag=workspace,socket="$VFS_WS_SOCK" tag=userhome,socket="$VFS_HOME_SOCK" \
  --memory    size=1024M,shared=on \
  --cpus      boot=2 \
  --console   off \
  --serial    file="$ZSBX_RUNTIME/serial.log" \
  > "$ZSBX_RUNTIME/ch.log" 2>&1 &
CH_PID=$!

echo "[wrapper] cloud-hypervisor pid=$CH_PID, vfs keys=$VFS_KEYS_PID ws=$VFS_WS_PID home=$VFS_HOME_PID"
echo "[wrapper] agent reachable at http://${VM_IP}:7777/"

# Block on CH; if it exits the trap fires and tears down virtiofsd.
wait "$CH_PID"
