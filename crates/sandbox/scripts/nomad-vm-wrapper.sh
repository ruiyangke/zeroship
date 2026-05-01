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
#   ZSBX_VM_INDEX        small int in [1,155] → tap zsbx-nm-$IDX, IP 10.99.$((100+IDX)).2
#                        (controller-side ceiling enforced at config load —
#                        100+IDX must fit a u8 octet, hence ≤ 255 → IDX ≤ 155.)
#   ZSBX_HERE            artifact dir; must contain vmlinuz + rootfs-slim.img
#   ZSBX_RUNTIME         per-allocation working dir (Nomad sets NOMAD_TASK_DIR)
#   ZSBX_KEYS_DIR        host dir holding controller-pubkey  (virtiofs tag=keys)
#   ZSBX_WORKSPACE_DIR   host dir for the project workspace  (virtiofs tag=workspace)
#   ZSBX_USER_HOME_DIR   host dir for the per-user $HOME      (virtiofs tag=userhome)
#   ZSBX_VM_MEMORY_MB    integer MiB → CH `--memory size=${N}M,shared=on`
#   ZSBX_VM_CPUS_BOOT    integer vCPU count → CH `--cpus boot=${N}`
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

set -Eeuo pipefail

# `set -E` makes the ERR trap inherit through functions and
# subshells; combined with the trap below it gives us a single
# "where did we die" log line, which is much easier to grep out
# of the Nomad task log than a `set -e` exit with no context.
err_trap() {
  local rc=$?
  local line=$1
  echo "[wrapper] FATAL: command failed at line $line (exit=$rc)" >&2
}
trap 'err_trap $LINENO' ERR

: "${ZSBX_VM_INDEX:?missing ZSBX_VM_INDEX}"
: "${ZSBX_HERE:?missing ZSBX_HERE}"
: "${ZSBX_RUNTIME:?missing ZSBX_RUNTIME}"
: "${ZSBX_KEYS_DIR:?missing ZSBX_KEYS_DIR}"
: "${ZSBX_WORKSPACE_DIR:?missing ZSBX_WORKSPACE_DIR}"
: "${ZSBX_USER_HOME_DIR:?missing ZSBX_USER_HOME_DIR}"
: "${ZSBX_VM_MEMORY_MB:?missing ZSBX_VM_MEMORY_MB}"
: "${ZSBX_VM_CPUS_BOOT:?missing ZSBX_VM_CPUS_BOOT}"

# Defensive bounds check on the VM index. The controller enforces
# 1..=155 at config load (third octet of 10.99.{100+idx}.x must fit
# a u8), but a hand-edited Nomad job spec or a misconfigured
# operator override could slip a bad value through. A bad index here
# would either collide with a sentinel subnet (idx=0 → 10.99.100.x,
# the .100 reservation) or overflow the third octet (idx>155 →
# `printf '%02x'` truncates, MAC duplication across VMs). Cheap to
# check; surfaces in the Nomad task log immediately.
case "$ZSBX_VM_INDEX" in
  ''|*[!0-9]*)
    echo "[wrapper] FATAL: ZSBX_VM_INDEX=$ZSBX_VM_INDEX is not a positive integer" >&2
    exit 1
    ;;
esac
if [ "$ZSBX_VM_INDEX" -lt 1 ] || [ "$ZSBX_VM_INDEX" -gt 155 ]; then
  echo "[wrapper] FATAL: ZSBX_VM_INDEX=$ZSBX_VM_INDEX out of range [1,155]" >&2
  exit 1
fi

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
#
# Doing this BEFORE virtiofsd spawn is intentional: a cp failure
# (missing source, no disk space, FS read-only) used to manifest
# downstream as "agent never returned 200 on /livez" — misleading,
# because the agent never even got a chance to start. With cp first
# AND the explicit error message below, the Nomad task log carries
# "rootfs copy failed" within ~250 ms; the controller's
# `wait_for_alloc_running` surfaces it immediately.
if [ ! -f "$DISK" ]; then
  if ! cp --reflink=auto "$ZSBX_HERE/rootfs-slim.img" "$DISK"; then
    echo "[wrapper] FATAL: rootfs copy failed: $ZSBX_HERE/rootfs-slim.img → $DISK" >&2
    exit 1
  fi
fi

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
#
# If a socket never shows up the corresponding virtiofsd died at
# startup (bad config, missing share dir, permission error). Failing
# loud HERE means Nomad reports the alloc as failed; the controller's
# `wait_for_alloc_running` surfaces it within ~250 ms. Without this
# guard the script proceeded into `cloud-hypervisor`, which would
# fail to register the missing fs device and the controller would
# only notice via the 30 s `/livez` timeout — much worse signal.
for sock in "$VFS_KEYS_SOCK" "$VFS_WS_SOCK" "$VFS_HOME_SOCK"; do
  for _ in $(seq 1 50); do [ -S "$sock" ] && break; sleep 0.02; done
  if [ ! -S "$sock" ]; then
    echo "[wrapper] virtiofsd socket $sock did not appear within ~1s; aborting" >&2
    # Try to surface what virtiofsd logged before we exit. The trap
    # will then tear down whatever did manage to start.
    case "$sock" in
      "$VFS_KEYS_SOCK") tail -n 20 "$ZSBX_RUNTIME/vfs-keys.log" 2>/dev/null || true ;;
      "$VFS_WS_SOCK")   tail -n 20 "$ZSBX_RUNTIME/vfs-ws.log"   2>/dev/null || true ;;
      "$VFS_HOME_SOCK") tail -n 20 "$ZSBX_RUNTIME/vfs-home.log" 2>/dev/null || true ;;
    esac
    exit 1
  fi
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
  --memory    size=${ZSBX_VM_MEMORY_MB}M,shared=on \
  --cpus      boot=${ZSBX_VM_CPUS_BOOT} \
  --console   off \
  --serial    file="$ZSBX_RUNTIME/serial.log" \
  > "$ZSBX_RUNTIME/ch.log" 2>&1 &
CH_PID=$!

echo "[wrapper] cloud-hypervisor pid=$CH_PID, vfs keys=$VFS_KEYS_PID ws=$VFS_WS_PID home=$VFS_HOME_PID"
# Port 7777 mirrors `zeroship_sandbox_agent::AGENT_PORT` — keep them
# in sync if either side ever needs a different port.
echo "[wrapper] agent reachable at http://${VM_IP}:7777/"

# Block on CH; if it exits the trap fires and tears down virtiofsd.
# Capture the exit code so we can propagate it (the EXIT trap will
# still fire afterwards for virtiofsd cleanup).
wait "$CH_PID"
ch_rc=$?
# Clear CH_PID so the cleanup trap doesn't `kill -TERM` a *reused*
# PID. Linux can recycle PIDs aggressively under load; without this,
# the trap could TERM/KILL an unrelated process that happened to
# inherit CH's PID between `wait` returning and `cleanup` running.
CH_PID=""
exit "$ch_rc"
