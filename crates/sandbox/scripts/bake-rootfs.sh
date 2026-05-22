#!/usr/bin/env bash
#
# Bake a freshly-built sandbox-agent into a debian-trixie rootfs image.
#
# Why this script exists:
#   Nix-host builds (the dev workflow most contributors use) link the
#   agent against `/nix/store/...` glibc + a Nix-store ELF interpreter.
#   When the binary is dropped into a debian-trixie rootfs and the VM
#   boots, the kernel comes up, init starts agent — but agent's PT_INTERP
#   points at a Nix path that doesn't exist inside debian, so execve()
#   silently fails. No agent log is produced; the failure surfaces only
#   as "/livez never returned 200" several layers up. End-to-end testing
#   surfaced this and worked around it with manual `patchelf`. This
#   script bakes that fix in.
#
#   We rewrite PT_INTERP to debian's standard `/lib64/ld-linux-x86-64.so.2`
#   and strip the Nix-store RPATH so the binary uses the debian rootfs's
#   `ld.so` cache. The agent's only dynamic deps (glibc) are present on
#   debian-trixie at the canonical paths.
#
# We also install the in-repo `init.sh` (the PID-1 script that parses
# `zsbx_pubkey=` from /proc/cmdline and mounts /dev/vdb,/vdc) to
# `/sbin/init` inside the rootfs, so the wire contract between the
# wrapper (cmdline-injected pubkey + two extra virtio-blk disks) and
# the guest's boot path stays in source. Prior to the virtio-blk
# pivot init.sh mounted three virtio-fs shares; bumping the rootfs
# image without rebaking init.sh would silently break booting.
#
# Usage:
#   bake-rootfs.sh <source-binary> <rootfs-img> [--mount-dir <dir>] [--build]
#
#   --build       cargo build --release -p zeroship-sandbox-agent first.
#                 <source-binary> is then required to point at the produced
#                 target/release/sandbox-agent (or wherever the operator
#                 keeps it).
#   --mount-dir   reuse an existing mount point instead of mktemp (useful
#                 if the rootfs is already mounted somewhere).
#
# Requires: sudo (for mount/umount), patchelf, strings.

set -euo pipefail

SCRIPT_NAME=$(basename "$0")

usage() {
    cat <<EOF
Usage: ${SCRIPT_NAME} <source-binary> <rootfs-img> [--mount-dir <dir>] [--build]

Patches a Nix-built sandbox-agent for debian-trixie rootfs portability and
copies it into the rootfs at /usr/local/bin/sandbox-agent (mode 0755).

Arguments:
  <source-binary>   Path to the freshly-built agent binary on the host.
                    With --build this path is the build output to bake.
  <rootfs-img>      Path to the raw ext4 rootfs image (loop-mountable).

Options:
  --mount-dir DIR   Mount the image at DIR instead of a fresh mktemp.
  --build           Run 'cargo build --release -p zeroship-sandbox-agent'
                    from the worktree root before patching.
  -h, --help        Show this help.
EOF
}

# ---- arg parsing ----

SRC_BIN=""
ROOTFS_IMG=""
MOUNT_DIR=""
DO_BUILD=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h|--help)
            usage
            exit 0
            ;;
        --mount-dir)
            [[ $# -ge 2 ]] || { echo "FATAL: --mount-dir requires a value" >&2; exit 2; }
            MOUNT_DIR="$2"
            shift 2
            ;;
        --build)
            DO_BUILD=1
            shift
            ;;
        --)
            shift
            break
            ;;
        -*)
            echo "FATAL: unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
        *)
            if [[ -z "${SRC_BIN}" ]]; then
                SRC_BIN="$1"
            elif [[ -z "${ROOTFS_IMG}" ]]; then
                ROOTFS_IMG="$1"
            else
                echo "FATAL: unexpected positional argument: $1" >&2
                usage >&2
                exit 2
            fi
            shift
            ;;
    esac
done

if [[ -z "${SRC_BIN}" || -z "${ROOTFS_IMG}" ]]; then
    echo "FATAL: <source-binary> and <rootfs-img> are required" >&2
    usage >&2
    exit 2
fi

# ---- locate worktree root (script lives at crates/sandbox/scripts/) ----

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKTREE_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"

# ---- prerequisite checks ----

for tool in patchelf strings sudo mount umount; do
    if ! command -v "${tool}" >/dev/null 2>&1; then
        echo "FATAL: required tool not found on PATH: ${tool}" >&2
        exit 1
    fi
done

# ---- optional build step ----

if [[ "${DO_BUILD}" -eq 1 ]]; then
    echo "[bake] cargo build --release -p zeroship-sandbox-agent (in ${WORKTREE_ROOT})"
    ( cd "${WORKTREE_ROOT}" && cargo build --release -p zeroship-sandbox-agent )
fi

if [[ ! -f "${SRC_BIN}" ]]; then
    echo "FATAL: source binary does not exist: ${SRC_BIN}" >&2
    exit 1
fi
if [[ ! -f "${ROOTFS_IMG}" ]]; then
    echo "FATAL: rootfs image does not exist: ${ROOTFS_IMG}" >&2
    exit 1
fi

# ---- working copy + cleanup trap ----

WORK_DIR="$(mktemp -d -t bake-rootfs.XXXXXX)"
PATCHED_BIN="${WORK_DIR}/sandbox-agent"
OWN_MOUNT=0

cleanup() {
    local rc=$?
    set +e
    if [[ -n "${MOUNT_DIR:-}" && "${OWN_MOUNT}" -eq 1 ]]; then
        # Best-effort unmount; ignore errors during cleanup.
        if mountpoint -q "${MOUNT_DIR}" 2>/dev/null; then
            sudo umount "${MOUNT_DIR}" || true
        fi
        rmdir "${MOUNT_DIR}" 2>/dev/null || true
    fi
    rm -rf "${WORK_DIR}" 2>/dev/null || true
    exit "${rc}"
}
trap cleanup EXIT INT TERM

# ---- patch the binary in a working copy ----

cp -f "${SRC_BIN}" "${PATCHED_BIN}"
chmod 0755 "${PATCHED_BIN}"

echo "[bake] patchelf: set interpreter to /lib64/ld-linux-x86-64.so.2"
patchelf --set-interpreter /lib64/ld-linux-x86-64.so.2 "${PATCHED_BIN}"

echo "[bake] patchelf: remove RPATH"
patchelf --remove-rpath "${PATCHED_BIN}"

# ---- capability-token verification (fail-loud) ----

echo "[bake] verifying capability tokens are present in the patched binary"
# NOTE: extract strings to a temp file rather than `strings | grep -q`, because
# `set -o pipefail` + `grep -q` race: grep closes its stdin on first match,
# strings gets SIGPIPE, the pipeline status is non-zero, and the `if !` arm
# fires a false-negative FATAL. Materializing the strings output sidesteps it.
STRINGS_OUT="${WORK_DIR}/strings.txt"
strings "${PATCHED_BIN}" > "${STRINGS_OUT}"
if ! grep -qE 'proxy\.http-v1' "${STRINGS_OUT}"; then
    echo "FATAL: 'proxy.http-v1' capability token not found in ${SRC_BIN}" >&2
    echo "       The binary you are baking is older than the preview-URLs feature." >&2
    echo "       Rebuild from this worktree (or pass --build) and retry." >&2
    exit 1
fi
if ! grep -qE 'auth\.ed25519-v1\.1' "${STRINGS_OUT}"; then
    echo "FATAL: 'auth.ed25519-v1.1' capability token not found in ${SRC_BIN}" >&2
    echo "       The binary you are baking is older than the preview-URLs feature." >&2
    echo "       Rebuild from this worktree (or pass --build) and retry." >&2
    exit 1
fi
rm -f "${STRINGS_OUT}"

# ---- mount the rootfs ----

if [[ -z "${MOUNT_DIR}" ]]; then
    MOUNT_DIR="$(mktemp -d -t bake-rootfs-mnt.XXXXXX)"
    OWN_MOUNT=1
else
    if [[ ! -d "${MOUNT_DIR}" ]]; then
        echo "FATAL: --mount-dir does not exist: ${MOUNT_DIR}" >&2
        exit 1
    fi
fi

echo "[bake] mounting ${ROOTFS_IMG} at ${MOUNT_DIR} (loop, rw)"
sudo mount -o loop,rw "${ROOTFS_IMG}" "${MOUNT_DIR}"

# ---- install the patched binary ----

DEST_DIR="${MOUNT_DIR}/usr/local/bin"
DEST_BIN="${DEST_DIR}/sandbox-agent"

echo "[bake] installing patched agent at ${DEST_BIN} (mode 0755)"
sudo install -d -m 0755 "${DEST_DIR}"
sudo install -m 0755 "${PATCHED_BIN}" "${DEST_BIN}"

# Install the in-repo PID-1 init script at /sbin/init. This is the
# script the wrapper's `init=/sbin/init` cmdline points at; it parses
# zsbx_pubkey= from /proc/cmdline, formats + mounts /dev/vdb,/vdc,
# then execs sandbox-agent. See ./init.sh for the contract.
INIT_SRC="${SCRIPT_DIR}/init.sh"
if [[ ! -f "${INIT_SRC}" ]]; then
    echo "FATAL: init.sh missing next to bake-rootfs.sh: ${INIT_SRC}" >&2
    exit 1
fi
echo "[bake] installing /sbin/init from ${INIT_SRC} (mode 0755)"
sudo install -m 0755 "${INIT_SRC}" "${MOUNT_DIR}/sbin/init"
sudo sync

# ---- explicit unmount (cleanup trap is best-effort fallback) ----

echo "[bake] unmounting ${MOUNT_DIR}"
sudo umount "${MOUNT_DIR}"
if [[ "${OWN_MOUNT}" -eq 1 ]]; then
    rmdir "${MOUNT_DIR}" 2>/dev/null || true
    MOUNT_DIR=""
    OWN_MOUNT=0
fi

echo "[bake] OK — ${ROOTFS_IMG} now contains a debian-portable sandbox-agent"
