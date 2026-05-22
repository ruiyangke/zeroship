#!/usr/bin/env bash
#
# Nomad raw_exec wrapper — launches one Cloud-Hypervisor microVM and
# ties its lifetime to this script. When Nomad sends SIGTERM (on `nomad
# job stop` or alloc reschedule), the trap below tears everything
# down cleanly: just cloud-hypervisor.
#
# **virtio-blk pivot (closes bug #11).** Prior to this revision the
# wrapper spawned virtiofsd × 3 (keys / workspace / userhome) and
# wired them into CH as `--fs`. virtio-fs requires the host-side
# virtiofsd to hold vhost-user negotiated state; on `--restore` CH
# resumes from the snapshot's recorded vring state but a fresh
# virtiofsd doesn't know the prior handshake → `Connection reset by
# peer` on `SetVringEnable`. The pivot replaces those three shares
# with:
#   - `/workspace` → virtio-blk disk backed by `$ZSBX_WORKSPACE_IMG`
#     (raw ext4 image, per-sandbox, lives in host_dir)
#   - `/userhome`  → virtio-blk disk backed by `$ZSBX_USER_HOME_IMG`
#     (raw ext4 image, per-user, reused across sandboxes)
#   - `/keys/controller-pubkey` → written by the guest's /sbin/init
#     from the kernel command line `zsbx_pubkey=<hex>` we inject
#     here. No host-side daemon; nothing to survive snapshot.
#
# This is the controller-shipped wrapper; the controller writes a
# Nomad job spec whose `Config.command` points at this path. The
# script is committed alongside the Rust crate so the wire contract
# (env vars + disk image paths + IP plumbing + pubkey hex) is
# visible in source.
#
# Inputs (all required, set by zeroship-sandbox in the Nomad job env):
#   ZSBX_VM_INDEX        small int in [1,155] → tap zsbx-nm-$IDX,
#                        IP 10.${ZSBX_SUBNET_BASE_OCTET}.$((100+IDX)).2
#                        (controller-side ceiling enforced at config load —
#                        100+IDX must fit a u8 octet, hence ≤ 255 → IDX ≤ 155.)
#   ZSBX_ARTIFACT_DIR    artifact dir; must contain vmlinuz + rootfs-slim.img.
#                        (Renamed from ZSBX_HERE in round 3 — the new name
#                        matches the Rust-side struct field `runtime_dir`'s
#                        intent.)
#   ZSBX_RUNTIME         per-allocation working dir (Nomad sets NOMAD_TASK_DIR)
#   ZSBX_WORKSPACE_IMG   absolute path to the workspace ext4 image
#                        (raw, attached as virtio-blk → guest /dev/vdb → /workspace).
#                        Created + formatted by the controller on cold-boot;
#                        idempotent on re-create.
#   ZSBX_USER_HOME_IMG   absolute path to the per-user $HOME ext4 image
#                        (raw, attached as virtio-blk → guest /dev/vdc → /userhome).
#                        Per-user, reused across that user's sandboxes.
#   ZSBX_PUBKEY_HEX      hex-encoded controller signing pubkey bytes (lowercase,
#                        no `0x` prefix). Injected into the guest via
#                        `zsbx_pubkey=$ZSBX_PUBKEY_HEX` on the kernel cmdline;
#                        the guest's /sbin/init decodes + writes it to
#                        /keys/controller-pubkey for sandbox-agent to read.
#   ZSBX_VM_MEMORY_MB    integer MiB → CH `--memory size=${N}M,shared=on`
#   ZSBX_VM_CPUS_BOOT    integer vCPU count → CH `--cpus boot=${N}`
#   ZSBX_SUBNET_BASE_OCTET   second octet of the per-VM /30 subnet (default 99
#                        on the controller side); host = 10.${BASE}.${IDX+100}.1
#                        VM   = 10.${BASE}.${IDX+100}.2.  Configurable so the
#                        operator can shift off the default 10.99/16 if the
#                        host has a corp collision.  See M6 in the round-3
#                        review notes.
#
# Optional inputs (snapshot/restore — § 11 of the snapshot proposal):
#   ZSBX_RESTORE_FROM    when set, switches the wrapper from cold-boot to
#                        the restore path. Value is an absolute directory
#                        containing a CH snapshot dir (config.json,
#                        memory-ranges, state.json) prepared by the
#                        controller's `RestoreHandler`. The controller is
#                        responsible for staging the alloc dir to match
#                        the rewritten `config.json`'s disk / serial-log
#                        paths; the wrapper then execs `cloud-hypervisor
#                        --restore source_url=file://$ZSBX_RESTORE_FROM`.
#                        Snapshots taken under the virtio-fs era will fail
#                        to restore (their config.json carries `fs[]`
#                        entries CH can't reconstruct); this is acceptable
#                        because the pivot lands pre-launch.
#
# The host operator is responsible for pre-provisioning:
#   - tap device `zsbx-nm-$IDX` in the /30 subnet
#     10.${ZSBX_SUBNET_BASE_OCTET}.$((100+IDX)).0/30
#     (host=.1, VM=.2, no gateway, broadcast at .3)
#   - cloud-hypervisor installed and on PATH
#   - vmlinuz built with CONFIG_IP_PNP=y at "$ZSBX_ARTIFACT_DIR/vmlinuz"
#   - rootfs-slim.img at "$ZSBX_ARTIFACT_DIR/rootfs-slim.img" with an
#     /sbin/init that: parses zsbx_pubkey= from /proc/cmdline → writes
#     /keys/controller-pubkey; mounts /dev/vdb → /workspace and
#     /dev/vdc → /userhome (mkfs.ext4 on first boot if unformatted);
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
: "${ZSBX_ARTIFACT_DIR:?missing ZSBX_ARTIFACT_DIR}"
: "${ZSBX_RUNTIME:?missing ZSBX_RUNTIME}"
: "${ZSBX_VM_MEMORY_MB:?missing ZSBX_VM_MEMORY_MB}"
: "${ZSBX_VM_CPUS_BOOT:?missing ZSBX_VM_CPUS_BOOT}"
# WORKSPACE_IMG / USER_HOME_IMG / PUBKEY_HEX: required on cold-boot
# (the wrapper passes them to CH as `--disk` + cmdline arg). On
# restore the snapshot's recorded config.json already carries the
# disk paths and the cmdline (CH `--restore` ignores `--cmdline`),
# so they're not strictly needed — but we still validate them when
# present so a hand-edited restore jobspec with a typo'd path
# surfaces in the Nomad task log instead of as a 401-loop in the
# agent. The controller passes the same three values on both
# branches (cheap; derived from the sandbox row); the cold-boot
# branch uses them, the restore branch ignores them.
if [ -z "${ZSBX_RESTORE_FROM:-}" ]; then
  : "${ZSBX_WORKSPACE_IMG:?missing ZSBX_WORKSPACE_IMG}"
  : "${ZSBX_USER_HOME_IMG:?missing ZSBX_USER_HOME_IMG}"
  : "${ZSBX_PUBKEY_HEX:?missing ZSBX_PUBKEY_HEX}"
fi
# M6: subnet base octet is configurable on the controller side; default
# 99 keeps the historical 10.99/16 layout. The wrapper validates it's a
# u8 to catch typos that would otherwise silently produce a different
# IP than the controller computed.
: "${ZSBX_SUBNET_BASE_OCTET:=99}"
case "$ZSBX_SUBNET_BASE_OCTET" in
  ''|*[!0-9]*)
    echo "[wrapper] FATAL: ZSBX_SUBNET_BASE_OCTET=$ZSBX_SUBNET_BASE_OCTET is not a non-negative integer" >&2
    exit 1
    ;;
esac
if [ "$ZSBX_SUBNET_BASE_OCTET" -lt 0 ] || [ "$ZSBX_SUBNET_BASE_OCTET" -gt 255 ]; then
  echo "[wrapper] FATAL: ZSBX_SUBNET_BASE_OCTET=$ZSBX_SUBNET_BASE_OCTET out of u8 range" >&2
  exit 1
fi

# Defensive bounds check on the VM index. The controller enforces
# 1..=155 at config load (third octet of
# 10.${ZSBX_SUBNET_BASE_OCTET}.{100+idx}.x must fit a u8), but a
# hand-edited Nomad job spec or a misconfigured operator override
# could slip a bad value through. A bad index here would either
# collide with a sentinel subnet (idx=0 → ...100.x, the .100
# reservation) or overflow the third octet (idx>155 →
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

# Defensive: the pubkey hex must be even-length and hex-only. The
# guest's init.sh decodes it with `xxd -r -p`; a malformed value
# would silently produce garbage bytes and the agent would reject
# every signed request from the controller. Surface it here, in the
# Nomad task log, rather than chasing a "401 invalid signature" loop.
# Only validated on cold-boot — on restore the cmdline (with its
# embedded pubkey) is preserved from the snapshot, so this env var
# is unused.
if [ -z "${ZSBX_RESTORE_FROM:-}" ]; then
  case "$ZSBX_PUBKEY_HEX" in
    *[!0-9a-fA-F]*)
      echo "[wrapper] FATAL: ZSBX_PUBKEY_HEX contains non-hex characters" >&2
      exit 1
      ;;
  esac
  if [ $(( ${#ZSBX_PUBKEY_HEX} % 2 )) -ne 0 ]; then
    echo "[wrapper] FATAL: ZSBX_PUBKEY_HEX has odd length ${#ZSBX_PUBKEY_HEX}" >&2
    exit 1
  fi
fi

cd "$ZSBX_ARTIFACT_DIR"

TAP=zsbx-nm-${ZSBX_VM_INDEX}
SUBNET_IDX=$((100 + ZSBX_VM_INDEX))   # 10.${BASE}.101.2 for index=1, etc.
VM_IP=10.${ZSBX_SUBNET_BASE_OCTET}.${SUBNET_IDX}.2
HOST_IP=10.${ZSBX_SUBNET_BASE_OCTET}.${SUBNET_IDX}.1
MAC=$(printf '12:34:56:78:9b:%02x' "$ZSBX_VM_INDEX")

API_SOCK="$ZSBX_RUNTIME/ch.sock"
DISK="$ZSBX_RUNTIME/rootfs.img"

# Tap re-up. CH brings the tap UP when it attaches and leaves it DOWN
# when it exits (KVM_TUN device close path on Linux 6.x). Between the
# source VM's exit and a restore alloc's spawn, the tap is DOWN; CH
# `--restore` then fails to attach and the agent is unreachable
# (`No route to host`). `ip link set up` is idempotent and cheap;
# keep it here (not the host startup script) so cold-boot, restart,
# and restore all go through the same setup path. Requires
# CAP_NET_ADMIN — Nomad raw_exec on the worker runs as root, which
# the host operator has already accepted as the trust boundary.
if [ -e "/sys/class/net/$TAP" ]; then
  ip link set "$TAP" up || {
    echo "[wrapper] FATAL: failed to bring $TAP up (need root + CAP_NET_ADMIN)" >&2
    exit 1
  }
else
  echo "[wrapper] FATAL: tap $TAP missing — host setup script did not pre-create it" >&2
  exit 1
fi

# The workspace + user-home disk images are owned by the controller
# (see `create_sandbox` in `nomad_ch.rs`): they're sparse ext4 images
# created via `truncate -s … + mkfs.ext4` on first sandbox/user.
# Defensive existence check here: a missing image would manifest
# downstream as CH "Error opening block device file" — surface it now
# in the Nomad task log instead.
#
# Restore branch uses paths embedded in the snapshot's config.json
# (not these env vars), but we still check the env-supplied paths
# exist so a hand-edited restore jobspec with a typo'd path doesn't
# get past this gate.
if [ -n "${ZSBX_WORKSPACE_IMG:-}" ] && [ ! -f "$ZSBX_WORKSPACE_IMG" ]; then
  echo "[wrapper] FATAL: workspace image missing: $ZSBX_WORKSPACE_IMG" >&2
  exit 1
fi
if [ -n "${ZSBX_USER_HOME_IMG:-}" ] && [ ! -f "$ZSBX_USER_HOME_IMG" ]; then
  echo "[wrapper] FATAL: user-home image missing: $ZSBX_USER_HOME_IMG" >&2
  exit 1
fi

# Per-allocation copy of the rootfs (so concurrent jobs don't share
# state — each VM writes its own rootfs in /, which is r/w).
# `--reflink=auto` is fast on btrfs/xfs; falls back to a normal copy
# elsewhere.
#
# Doing this BEFORE CH spawn is intentional: a cp failure (missing
# source, no disk space, FS read-only) used to manifest downstream
# as "agent never returned 200 on /livez" — misleading, because the
# agent never even got a chance to start. With cp first AND the
# explicit error message below, the Nomad task log carries
# "rootfs copy failed" within ~250 ms; the controller's
# `wait_for_alloc_running` surfaces it immediately.
#
# Restore path note: when `ZSBX_RESTORE_FROM` is set, the snapshot's
# memory image carries the entire VM state (including the rootfs's
# in-RAM page cache view), so we still need a writable rootfs file
# at the same path the restored config.json expects. The controller
# stages `$ZSBX_RESTORE_FROM` such that this path is consistent with
# the snapshot's `disks[]` entry (today: shared read-only template
# from $ZSBX_ARTIFACT_DIR — see § 5.2). The cp below remains a
# defensive belt-and-braces (idempotent due to the [ ! -f ] guard).
if [ ! -f "$DISK" ]; then
  if ! cp --reflink=auto "$ZSBX_ARTIFACT_DIR/rootfs-slim.img" "$DISK"; then
    echo "[wrapper] FATAL: rootfs copy failed: $ZSBX_ARTIFACT_DIR/rootfs-slim.img → $DISK" >&2
    exit 1
  fi
fi

# Clean any stale CH API socket from a crashed prior run (Nomad gives
# us a fresh NOMAD_TASK_DIR per alloc, so this should already be
# empty, but defensive). Post-pivot there are no virtiofsd sockets to
# clean.
rm -f "$API_SOCK"

echo "[wrapper] index=$ZSBX_VM_INDEX  tap=$TAP  vm_ip=$VM_IP  host_ip=$HOST_IP"
echo "[wrapper] workspace_img=$ZSBX_WORKSPACE_IMG  userhome_img=$ZSBX_USER_HOME_IMG"

# Cleanup trap. Any signal (or the script exits naturally on CH
# termination) → kill CH. SIGTERM first with a brief grace, then
# SIGKILL.
#
# Post-pivot the trap is much simpler: virtiofsd is gone, so the
# only child we own is cloud-hypervisor. We still `wait` after kill
# so the wrapper bash doesn't exit ahead of CH being fully reaped
# (Nomad's `raw_exec` ClientStatus flips to "complete" only when the
# wrapper bash itself exits; pre-reap exit would leave a brief
# zombie window the host_fence has to ride out).
cleanup() {
  echo "[wrapper] cleaning up (ch ${CH_PID:-already-exited})"
  [ -n "${CH_PID-}" ] && kill -TERM "$CH_PID" 2>/dev/null || true
  sleep 0.2
  [ -n "${CH_PID-}" ] && kill -KILL "$CH_PID" 2>/dev/null || true
  # Reap so the wrapper bash doesn't exit ahead of CH.
  # `wait` on a PID we don't own (because some other ancestor
  # collected it) returns immediately with status 127 — harmless,
  # we discard via `|| true`. The point is: when this function
  # returns, CH has been collected.
  [ -n "${CH_PID-}" ] && wait "$CH_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# Spawn cloud-hypervisor in the background so we can capture its PID
# for the cleanup trap.
#
# Important CH quirk: `--disk` takes ALL disk arguments as ONE space-
# separated token, NOT one per flag. Listing them as separate `--disk`
# args makes CH only register the last one. Same applies to `--fs`
# (which we no longer use).
#
# Branch: restore vs. cold boot. When ZSBX_RESTORE_FROM is set, the
# memory/state/config triple at that path carries the entire VM
# config (cmdline, disks, net, memory, cpus, serial), so CH
# `--restore source_url=file://$DIR` is invoked WITHOUT the
# kernel / cmdline / disk / net / memory / cpus / serial flags —
# those would conflict with the snapshot's embedded config.
# `--api-socket` is fresh (unrelated to the snapshot's recorded
# api-socket path; CH treats it as a new control channel).
if [ -n "${ZSBX_RESTORE_FROM:-}" ]; then
  # Defensive: confirm the staged dir exists + is non-empty. The
  # controller stages prior to job submission so the typical failure
  # mode is "controller rolled back mid-stage" — surface it loudly.
  if [ ! -d "$ZSBX_RESTORE_FROM" ] \
     || [ ! -f "$ZSBX_RESTORE_FROM/memory-ranges" ] \
     || [ ! -f "$ZSBX_RESTORE_FROM/config.json" ] \
     || [ ! -f "$ZSBX_RESTORE_FROM/state.json" ]; then
    echo "[wrapper] FATAL: ZSBX_RESTORE_FROM=$ZSBX_RESTORE_FROM missing one of {memory-ranges,config.json,state.json}" >&2
    exit 1
  fi

  # Rewrite path-bearing fields in config.json to point at THIS
  # alloc's NOMAD_TASK_DIR. The controller can't do this at job-
  # submit time because Nomad assigns the alloc UUID only after the
  # job is submitted, so the controller's restore_handler leaves
  # disks[].path / serial.file pointing at the source alloc's path.
  # Without this rewrite CH --restore opens
  # `<source_alloc>/serial.log` → ENOENT → "Error creating console
  # device" → CH exits at t=3ms before reaching net/disk setup.
  # Diagnostic capture 2026-05-22; see commit message of bug-#8 fix.
  #
  # The pattern matches /opt/nomad/data/alloc/<alloc-id>/<task>/local
  # (Nomad's per-task local-dir layout). The substitution is
  # idempotent across re-wakes — a prior wake's task-dir also matches
  # the same prefix pattern and gets replaced with the current one.
  #
  # Post virtio-blk pivot: snapshots no longer carry `fs[].socket`
  # entries, so the pattern only matches `disks[].path` and
  # `serial.file`. Older (pre-pivot) snapshots that DO still have
  # `fs[].socket` would also get rewritten by this sed and then
  # fail to restore at CH level (no virtiofsd backing the socket) —
  # acceptable, the pivot lands pre-launch and we don't carry
  # legacy snapshots.
  if ! sed -i -E "s#/opt/nomad/data/alloc/[^/]+/[^/]+/local#${NOMAD_TASK_DIR}#g" \
       "$ZSBX_RESTORE_FROM/config.json"; then
    echo "[wrapper] FATAL: config.json path rewrite failed" >&2
    exit 1
  fi

  echo "[wrapper] restore path: source=$ZSBX_RESTORE_FROM, NOMAD_TASK_DIR=$NOMAD_TASK_DIR"
  cloud-hypervisor \
    --api-socket "$API_SOCK" \
    --restore    "source_url=file://$ZSBX_RESTORE_FROM" \
    > "$ZSBX_RUNTIME/ch.log" 2>&1 &
  CH_PID=$!
else
  # Cold boot. The cmdline carries the controller pubkey as hex
  # (`zsbx_pubkey=<hex>`); /sbin/init in the guest decodes it and
  # writes /keys/controller-pubkey before exec'ing sandbox-agent.
  # The two extra `--disk` entries (workspace + userhome) appear in
  # the guest as /dev/vdb + /dev/vdc respectively (PCI device order
  # matches CH argument order); init.sh mounts them at /workspace
  # and /userhome, formatting on first boot if unformatted.
  cloud-hypervisor \
    --api-socket "$API_SOCK" \
    --kernel    vmlinuz \
    --cmdline   "console=ttyS0 root=/dev/vda rw init=/sbin/init reboot=t panic=1 ip=${VM_IP}::${HOST_IP}:255.255.255.252::eth0:none zsbx_pubkey=${ZSBX_PUBKEY_HEX}" \
    --disk      path="$DISK",readonly=off,direct=off,image_type=raw path="$ZSBX_WORKSPACE_IMG",readonly=off,direct=off,image_type=raw path="$ZSBX_USER_HOME_IMG",readonly=off,direct=off,image_type=raw \
    --net       tap="$TAP",mac="$MAC" \
    --memory    size=${ZSBX_VM_MEMORY_MB}M,shared=on \
    --cpus      boot=${ZSBX_VM_CPUS_BOOT} \
    --console   off \
    --serial    file="$ZSBX_RUNTIME/serial.log" \
    > "$ZSBX_RUNTIME/ch.log" 2>&1 &
  CH_PID=$!
fi

echo "[wrapper] cloud-hypervisor pid=$CH_PID"
# Port 7777 mirrors `zeroship_sandbox_agent::AGENT_PORT` — keep them
# in sync if either side ever needs a different port.
echo "[wrapper] agent reachable at http://${VM_IP}:7777/"

# Block on CH; if it exits the trap fires and tears down anything
# left over. Capture the exit code so we can propagate it.
wait "$CH_PID"
ch_rc=$?
# Clear CH_PID so the cleanup trap doesn't `kill -TERM` a *reused*
# PID. Linux can recycle PIDs aggressively under load; without this,
# the trap could TERM/KILL an unrelated process that happened to
# inherit CH's PID between `wait` returning and `cleanup` running.
CH_PID=""
exit "$ch_rc"
