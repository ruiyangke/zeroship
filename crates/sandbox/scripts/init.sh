#!/bin/sh
# Tiny PID-1 wrapper for the sandbox microVM. Brings up minimal
# namespaces, parses the controller's signing pubkey out of the
# kernel command line, formats + mounts the virtio-blk data disks,
# then execs the agent.
#
# **virtio-blk pivot (closes bug #11).** The previous revision of
# this script mounted three virtio-fs shares (`keys`, `workspace`,
# `userhome`). virtio-fs requires a host-side virtiofsd whose
# vhost-user state must survive snapshot/restore — which it can't,
# because the controller can't recreate the negotiated state on
# fresh process spawn. The pivot replaces those shares with:
#   - `/run/keys/controller-pubkey` — written here from the kernel
#     cmdline arg `zsbx_pubkey=<hex>` (tmpfs-backed `/run`)
#   - `/workspace` — virtio-blk /dev/vdb (ext4, formatted on first
#     boot if blkid reports no FS)
#   - `/home/u`    — virtio-blk /dev/vdc (ext4, formatted on first
#     boot if blkid reports no FS). The agent's dropuser module
#     hard-codes `/home/u` as `USER_HOME`; do not rename.
#
# This script is shipped as part of the rootfs image; bake-rootfs.sh
# installs it (committed in the same series as this file lands).
# It is committed to the repo so the wire contract (cmdline arg
# name, mount points, mkfs policy) is visible in source.

set -e

mount -t proc       proc      /proc
mount -t sysfs      sysfs     /sys
mount -t devtmpfs   devtmpfs  /dev
mount -t tmpfs      tmpfs     /run
mount -t tmpfs      tmpfs     /tmp

# `/run/keys` is the directory the agent's auth loader reads
# `controller-pubkey` from (see crates/sandbox-agent/src/auth.rs
# `DEFAULT_PUBKEY_PATH`). Tmpfs-backed because the pubkey is non-
# secret but ephemeral per boot — the controller may rotate it
# between sandboxes, and we don't want a stale file from a prior
# image bake to win over a fresh cmdline value.
mkdir -p /run/keys /workspace /home/u

# ---- pubkey from kernel cmdline ----
#
# CH boots the guest with `zsbx_pubkey=<hex>` on the command line
# (the wrapper builds this from $ZSBX_PUBKEY_HEX). Parse it out,
# decode the hex back to bytes, and drop it at the path the agent
# expects. `xxd -r -p` is the busybox-friendly hex→bin decoder.
#
# Failure mode design: if the arg is missing or malformed, we
# refuse to proceed. A boot that silently lacks the pubkey would
# turn every controller request into a 401 with no diagnostic; far
# better to die at PID 1 with a serial-console message.
PUBKEY_HEX=""
for arg in $(cat /proc/cmdline); do
    case "$arg" in
        zsbx_pubkey=*)
            PUBKEY_HEX=${arg#zsbx_pubkey=}
            ;;
    esac
done

if [ -z "$PUBKEY_HEX" ]; then
    echo "[init] FATAL: zsbx_pubkey= missing from /proc/cmdline" >&2
    exit 1
fi

# Even length, hex-only — defensive check; the wrapper already
# validates this host-side but the cmdline could be malformed by
# some other path (e.g. a hand-edited snapshot config.json).
case "$PUBKEY_HEX" in
    *[!0-9a-fA-F]*)
        echo "[init] FATAL: zsbx_pubkey contains non-hex characters" >&2
        exit 1
        ;;
esac
PUBKEY_HEX_LEN=$(printf %s "$PUBKEY_HEX" | wc -c)
if [ $(( PUBKEY_HEX_LEN % 2 )) -ne 0 ]; then
    echo "[init] FATAL: zsbx_pubkey has odd hex length ($PUBKEY_HEX_LEN)" >&2
    exit 1
fi

# Hex → binary via portable sed + printf '%b'. Earlier revision used
# `xxd -r -p` but debian-trixie-slim ships without `vim-common`, so
# xxd is absent — boot panics on /sbin/init exit (bug #12, 2026-05-22
# cluster smoke). `sed 's/\(..\)/\\x\1/g'` transforms `ab12cd` into
# `\xab\x12\xcd`; `printf '%b'` interprets the backslash escapes and
# emits raw bytes. Both `sed` and `printf` are POSIX-mandated and
# always present in any /sbin/init runtime.
printf '%b' "$(printf '%s' "$PUBKEY_HEX" | sed 's/\(..\)/\\x\1/g')" \
    > /run/keys/controller-pubkey
chmod 0444 /run/keys/controller-pubkey

# ---- data disks (workspace + userhome) ----
#
# CH attaches three disks in cmdline order: vda (rootfs), vdb
# (workspace.img), vdc (user_home.img). On first boot of a fresh
# image the filesystem is absent; we mkfs.ext4 in place. Subsequent
# boots find a formatted FS and skip straight to mount. `blkid`
# returning a non-empty TYPE= line is our "already-formatted"
# signal — works for any FS, not just ext4, in case future bakes
# pre-populate.
format_if_needed() {
    dev=$1
    if [ ! -b "$dev" ]; then
        echo "[init] FATAL: block device $dev missing — check CH --disk ordering" >&2
        exit 1
    fi
    if ! blkid "$dev" >/dev/null 2>&1; then
        echo "[init] formatting $dev (no existing FS detected)"
        mkfs.ext4 -q "$dev"
    fi
}

format_if_needed /dev/vdb
format_if_needed /dev/vdc

mount -t ext4 /dev/vdb /workspace
mount -t ext4 /dev/vdc /home/u

# Network is brought up by the kernel via the `ip=...` cmdline arg
# (CONFIG_IP_PNP). No iproute2 binary needed in the rootfs.

# Workspace + home have to be writable by the dropped /exec
# children; dropuser later chowns /home/u to the drop user but the
# directory mode must permit traversal.
chmod 0755 /home/u
chmod 0755 /workspace

echo "[init] handing off to sandbox-agent ($(uname -r))"
echo "[init] /run/keys contents:"
ls -la /run/keys
echo "[init] mounts:"
mount | grep -E '/(workspace|home/u|run/keys)'

# The agent reads /run/keys/controller-pubkey at startup.
export SANDBOX_AGENT_LOG=info
exec /usr/local/bin/sandbox-agent
