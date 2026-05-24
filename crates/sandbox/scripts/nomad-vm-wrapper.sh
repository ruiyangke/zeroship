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
#   ZSBX_SANDBOX_ID      typed-id (`sbx_…`) of this sandbox row. R8-DEPLOY1:
#                        injected into the guest on the kernel cmdline as
#                        `SANDBOX_AGENT_SANDBOX_ID=<id>`. The Linux boot
#                        protocol passes every `KEY=VALUE` cmdline token the
#                        kernel doesn't recognise straight through to
#                        /sbin/init's *environment*; sandbox-agent's
#                        `init_sandbox_id_from_env`
#                        (crates/sandbox-agent/src/handlers.rs:95) reads
#                        `SANDBOX_AGENT_SANDBOX_ID` via `std::env::var`
#                        (preferred) with `/run/keys/sandbox-id` as a
#                        fallback. R7-S1 hard-errors the agent boot if
#                        neither source provides an id
#                        (crates/sandbox-agent/src/main.rs:97-102), so
#                        every cluster cold boot wedged until this env
#                        was wired. Cold-boot only — the restore branch
#                        is a no-op because the agent's `SANDBOX_ID`
#                        OnceLock is preserved in the snapshot's memory
#                        image; see restore-branch comments below.
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
#   - python3 on PATH (used here to safely rewrite the snapshot's
#     config.json on restore; see W1 below). The
#     `gcp-worker-startup.sh` apt set already includes `python3`, so
#     this is satisfied in the shipped fleet.
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
# WORKSPACE_IMG / USER_HOME_IMG / PUBKEY_HEX / SANDBOX_ID: required on
# cold-boot (the wrapper passes them to CH as `--disk` + cmdline arg).
# On restore the snapshot's recorded config.json already carries the
# disk paths and the cmdline (CH `--restore` ignores `--cmdline`),
# so they're not strictly needed — but we still validate them when
# present so a hand-edited restore jobspec with a typo'd path
# surfaces in the Nomad task log instead of as a 401-loop in the
# agent. The controller passes the same values on both branches
# (cheap; derived from the sandbox row); the cold-boot branch uses
# them, the restore branch ignores them (with one informational log
# line — see the restore branch).
if [ -z "${ZSBX_RESTORE_FROM:-}" ]; then
  : "${ZSBX_WORKSPACE_IMG:?missing ZSBX_WORKSPACE_IMG}"
  : "${ZSBX_USER_HOME_IMG:?missing ZSBX_USER_HOME_IMG}"
  : "${ZSBX_PUBKEY_HEX:?missing ZSBX_PUBKEY_HEX}"
  # R8-DEPLOY1: sandbox_id is required on cold boot — R7-S1 hard-errors
  # the agent if neither SANDBOX_AGENT_SANDBOX_ID env nor
  # /run/keys/sandbox-id mount provides it
  # (crates/sandbox-agent/src/main.rs:97-102). We inject via the
  # kernel cmdline below (Linux passes unrecognised `KEY=VALUE`
  # cmdline tokens straight through to init's environment). Missing
  # → fail loudly here, not as a 1-shot 401 loop in the agent.
  : "${ZSBX_SANDBOX_ID:?missing ZSBX_SANDBOX_ID (R8-DEPLOY1)}"
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
  # R8-DEPLOY1: ZSBX_SANDBOX_ID is embedded VERBATIM in the kernel
  # cmdline as `SANDBOX_AGENT_SANDBOX_ID=<value>` a few lines below.
  # The kernel splits the cmdline on whitespace; a value with embedded
  # space / quote / `=` would silently chop the agent's seen value, or
  # in the worst case a ` foo=bar` token would inject a *second* env
  # binding into init's environment. typed_id values are
  # `[a-z]+_[0-9a-zA-Z]+` by construction (crates/core/src/typed_id.rs),
  # so the only legal chars are [0-9a-zA-Z_]. Defence in depth: even
  # if the controller is later changed, a malformed value surfaces
  # here in the Nomad task log instead of as an unbound OnceLock at
  # boot.
  case "$ZSBX_SANDBOX_ID" in
    *[!0-9a-zA-Z_]*|'')
      echo "[wrapper] FATAL: ZSBX_SANDBOX_ID='$ZSBX_SANDBOX_ID' contains characters outside [0-9a-zA-Z_] (would corrupt kernel cmdline)" >&2
      exit 1
      ;;
  esac
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
  # R6-C1: reap the restore-resume background subshell (spawned at the
  # `( ... ) &` block below). Usually it has already exited (the poll
  # completes in <10s), in which case kill is a no-op and wait returns
  # immediately; under Nomad SIGTERM mid-poll it would otherwise orphan.
  if [ -n "${RESUME_PID:-}" ]; then
    kill -TERM "$RESUME_PID" 2>/dev/null || true
    wait "$RESUME_PID" 2>/dev/null || true
  fi
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
  # Bug-#14 diagnostic: surface the actual on-disk state of the
  # staging dir at the moment the wrapper inspects it. Prior cluster
  # smokes (2026-05-22) reported the dir was empty at wake time
  # despite the controller's store.get having returned Ok. Logging
  # the dir contents + sizes + tap state to stderr (Nomad's task
  # log) gives us the evidence the next cluster cycle needs.
  echo "[wrapper] restore: ZSBX_RESTORE_FROM=$ZSBX_RESTORE_FROM" >&2
  echo "[wrapper] restore: ls -la \$ZSBX_RESTORE_FROM:" >&2
  ls -la "$ZSBX_RESTORE_FROM" 2>&1 | sed 's/^/[wrapper] restore:   /' >&2 || true
  echo "[wrapper] restore: tap $TAP pre-CH-spawn:" >&2
  ip -br link show "$TAP" 2>&1 | sed 's/^/[wrapper] restore:   /' >&2 || true

  # R8-DEPLOY1 (restore branch): no host-side sandbox_id injection
  # is performed or possible. CH `--restore source_url=...` ignores
  # `--cmdline`; the snapshot's recorded kernel cmdline is replayed
  # verbatim. The agent's `SANDBOX_ID` OnceLock was set by the
  # ORIGINATING cold-boot's `init_sandbox_id_from_env` call
  # (crates/sandbox-agent/src/handlers.rs:95) BEFORE the snapshot
  # was taken, and OnceLock's state lives in the agent's heap which
  # is part of the CH memory image preserved across
  # pause/snapshot/restore. So the restored agent already has its
  # sandbox_id bound; no host-side injection is required (and the
  # mechanism doesn't exist).
  #
  # Caveat: a snapshot taken on a pre-R7-S1 agent (no OnceLock
  # bound before the snapshot) cannot be retro-fitted by the
  # wrapper — the rootfs the agent runs from is committed to the VM
  # image at snapshot time. The controller's R7-S1 land predates any
  # shipped restore artifacts, so this is empty-set today.
  if [ -n "${ZSBX_SANDBOX_ID:-}" ]; then
    echo "[wrapper] restore: ZSBX_SANDBOX_ID=$ZSBX_SANDBOX_ID (informational; agent OnceLock preserved in snapshot memory image)" >&2
  fi

  # Defensive: confirm the staged dir exists + is non-empty. The
  # controller stages prior to job submission so the typical failure
  # mode is "controller rolled back mid-stage" — surface it loudly.
  if [ ! -d "$ZSBX_RESTORE_FROM" ] \
     || [ ! -f "$ZSBX_RESTORE_FROM/memory-ranges" ] \
     || [ ! -f "$ZSBX_RESTORE_FROM/config.json" ] \
     || [ ! -f "$ZSBX_RESTORE_FROM/state.json" ]; then
    echo "[wrapper] FATAL: ZSBX_RESTORE_FROM=$ZSBX_RESTORE_FROM missing one of {memory-ranges,config.json,state.json}" >&2
    echo "[wrapper] FATAL: stat of each expected file:" >&2
    for f in memory-ranges config.json state.json; do
      stat -c '  %n: size=%s mtime=%y' "$ZSBX_RESTORE_FROM/$f" >&2 2>&1 || \
        echo "  $ZSBX_RESTORE_FROM/$f: STAT FAILED (likely missing)" >&2
    done
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
  # The match anchor is /opt/nomad/data/alloc/<alloc-id>/<task>/local
  # (Nomad's per-task local-dir layout) with a trailing `/` or end-
  # of-string boundary. The substitution is idempotent across re-
  # wakes — a prior wake's task-dir also matches the same prefix
  # pattern and gets replaced with the current one.
  #
  # Post virtio-blk pivot: snapshots no longer carry `fs[].socket`
  # entries, so the rewrite touches `disks[].path` and `serial.file`.
  # Older (pre-pivot) snapshots that DO still have `fs[].socket`
  # would also get rewritten and then fail to restore at CH level
  # (no virtiofsd backing the socket) — acceptable, the pivot lands
  # pre-launch and we don't carry legacy snapshots.
  #
  # W1 (security-r8): the previous implementation used
  #   sed -i -E "s#/opt/.../local#${NOMAD_TASK_DIR}#g" config.json
  # which is an UNANCHORED textual substitution. The replacement
  # side is a sed replacement string, so any `&`, `\`, `\1`-`\9`,
  # or the chosen delimiter `#` appearing in NOMAD_TASK_DIR would
  # be interpreted by sed — not as literal characters but as sed
  # metacharacters. A future codepath that lets any control-plane
  # field influence NOMAD_TASK_DIR (or a Nomad alloc UUID that
  # contained `#`) could rewrite config.json in unexpected ways.
  # The match side `[^/]+` was also not anchored to a known-safe
  # path boundary, so `/opt/nomad/data/alloc/x/y/localfoo` was a
  # candidate too.
  #
  # The replacement here uses a JSON-aware rewriter (stdlib python
  # only — python3 ships in the gcp-worker-startup.sh apt set so no
  # extra deps): walk the parsed config, find every JSON *string*
  # whose value starts with the anchored prefix `^/opt/nomad/data/
  # alloc/<uuid-ish>/<task>/local(/|$)`, and replace that prefix
  # slice with NOMAD_TASK_DIR. Non-string values at those keys are
  # treated as untouched (cannot be mis-substituted). The new
  # NOMAD_TASK_DIR is passed via env (NOT argv-interpolated into a
  # shell-built python source string) so no shell-quoting boundary
  # can leak. The write is atomic (tmp file + fsync + rename) to
  # avoid leaving a half-written config.json that CH would then
  # refuse to parse.
  #
  # R15-S2 (security-r15): a snapshot's `disks[].path` whose value
  # does NOT match the alloc-prefix used to pass through verbatim.
  # With AEAD authentication on the snapshot artifact (post-A1-
  # FOLLOWUP) a forged config.json requires KEK compromise — but
  # defence-in-depth says belt-and-braces. After rewriting, every
  # `disks[].path` / `serial.file` / `console.file` MUST resolve
  # under `NOMAD_TASK_DIR`. Any path that doesn't (e.g. a malicious
  # `/etc/shadow`, or a relative path that `..`'s out of the alloc
  # dir) is rejected with a clear error pointing at both the
  # offending value and the expected prefix.
  if ! NOMAD_TASK_DIR="$NOMAD_TASK_DIR" \
       CONFIG_JSON="$ZSBX_RESTORE_FROM/config.json" \
       /usr/bin/python3 - <<'PY'
import json, os, re, sys, tempfile

config_path = os.environ["CONFIG_JSON"]
task_dir = os.environ["NOMAD_TASK_DIR"]

# Anchored prefix match. The Nomad alloc layout is
# /opt/nomad/data/alloc/<36-char-uuid-with-dashes>/<task-name>/local
# but we accept any non-`/` chars in the uuid and task slots so this
# survives a future Nomad rename. The trailing group enforces that
# the match ends at a path separator (or end-of-string), preventing
# accidental rewrites of e.g. `/opt/nomad/data/alloc/x/y/localfoo`.
ALLOC_PREFIX = re.compile(
    r"^/opt/nomad/data/alloc/[^/]+/[^/]+/local(/|$)"
)


def rewrite(value):
    """Return rewritten string if value is a str matching the anchored
    prefix, else return the value unchanged. Non-string inputs are
    never substituted — defends against a hand-edited config.json
    that put e.g. a number where CH expects a path."""
    if not isinstance(value, str):
        return value
    m = ALLOC_PREFIX.match(value)
    if not m:
        return value
    # group(1) is `/` when the match ended at a path separator, or
    # `""` when it ended at end-of-string. value[m.end():] is the
    # remainder AFTER that boundary character (always `""` in the
    # EOS case since the regex consumed through end-of-string).
    # Reassemble task_dir + (separator if present) + remainder so
    # `…/local` rewrites to `<task_dir>` (no trailing /) and
    # `…/local/x` rewrites to `<task_dir>/x`.
    return task_dir + m.group(1) + value[m.end():]


# R15-S2: allow-list / prefix-guard. After rewriting, every path
# fed to CH MUST live under the alloc's task_dir. A malicious
# snapshot whose `disks[].path` is something like `/etc/shadow`
# would otherwise reach CH verbatim (since it doesn't match the
# `ALLOC_PREFIX` regex above) and CH would open it as a backing
# block device — disastrous if the snapshot AEAD KEK were ever
# compromised or the AEAD path bypassed.
#
# The guard:
#   * The path must be a non-empty string.
#   * It must be absolute (start with `/`). Relative paths are not
#     a thing CH accepts; reject them rather than letting CH's CWD
#     (the alloc dir per `cd "$ZSBX_ARTIFACT_DIR"` above) silently
#     re-anchor them.
#   * No path component may equal `..` (path-traversal defence in
#     depth — even if realpath resolution below would catch the
#     escape, an explicit reject lets the operator see the intent).
#   * `os.path.realpath(value)` (which resolves `..` and symlinks
#     against the live filesystem) must equal `task_dir` itself or
#     start with `task_dir + os.sep`. Comparing realpaths defends
#     against symlinked components inside the alloc dir that point
#     outside it.
TASK_DIR_REAL = os.path.realpath(task_dir)


def assert_under_task_dir(field_name, value):
    """Reject any path that doesn't live under task_dir. Exits 1
    with a clear operator-readable error on the first violation."""
    if not isinstance(value, str) or not value:
        print(
            f"[wrapper] FATAL: R15-S2 reject: config.json {field_name} "
            f"is empty/non-string (got type={type(value).__name__}); "
            f"expected absolute path under {task_dir}",
            file=sys.stderr,
        )
        sys.exit(1)
    if not value.startswith("/"):
        print(
            f"[wrapper] FATAL: R15-S2 reject: config.json {field_name} "
            f"= {value!r} is not absolute; "
            f"expected a path under {task_dir}",
            file=sys.stderr,
        )
        sys.exit(1)
    # Path-component traversal defence. `..` as ANY component is a
    # red flag in a snapshot config.json — the controller's
    # rewriter never emits one, and CH itself doesn't need them.
    parts = value.split("/")
    if any(part == ".." for part in parts):
        print(
            f"[wrapper] FATAL: R15-S2 reject: config.json {field_name} "
            f"= {value!r} contains a `..` component (path-traversal "
            f"defence); expected a path under {task_dir}",
            file=sys.stderr,
        )
        sys.exit(1)
    # Realpath check — resolves symlinks and remaining `.` segments
    # (`..` already rejected). `realpath` on a non-existent path
    # resolves the components that DO exist and leaves the tail
    # literal, which is fine for our prefix check.
    real = os.path.realpath(value)
    if real != TASK_DIR_REAL and not real.startswith(
        TASK_DIR_REAL + os.sep
    ):
        print(
            f"[wrapper] FATAL: R15-S2 reject: config.json {field_name} "
            f"= {value!r} resolves to {real!r}, which is NOT under "
            f"expected prefix {TASK_DIR_REAL!r} (task_dir). "
            f"Possible malicious snapshot or misrouted restore.",
            file=sys.stderr,
        )
        sys.exit(1)


with open(config_path, "r", encoding="utf-8") as f:
    config = json.load(f)

# disks[].path — list of dicts, each with a `path` string.
disks = config.get("disks") or []
if not isinstance(disks, list):
    print(
        f"[wrapper] FATAL: config.json 'disks' is not a list "
        f"(type={type(disks).__name__})",
        file=sys.stderr,
    )
    sys.exit(1)
for i, disk in enumerate(disks):
    if isinstance(disk, dict) and "path" in disk:
        disk["path"] = rewrite(disk["path"])
        assert_under_task_dir(f"disks[{i}].path", disk["path"])

# serial.file — single dict, optional `file` string.
serial = config.get("serial")
if isinstance(serial, dict) and "file" in serial:
    serial["file"] = rewrite(serial["file"])
    assert_under_task_dir("serial.file", serial["file"])

# console.file — same shape as serial, may also carry a path post-
# pivot. Touch it for symmetry; no-op when not present.
console = config.get("console")
if isinstance(console, dict) and "file" in console:
    console["file"] = rewrite(console["file"])
    assert_under_task_dir("console.file", console["file"])

# fs[].socket — legacy virtio-fs sockets. Rewritten for diagnostic
# clarity (the restore will still fail at CH level — see comment
# block above), but the rewrite itself is safe.
fs_entries = config.get("fs") or []
if isinstance(fs_entries, list):
    for i, entry in enumerate(fs_entries):
        if isinstance(entry, dict) and "socket" in entry:
            entry["socket"] = rewrite(entry["socket"])
            assert_under_task_dir(f"fs[{i}].socket", entry["socket"])

# Atomic write: write to a temp file in the same dir, fsync, rename.
# Same-dir rename is atomic on ext4/xfs, so a crash mid-write can't
# leave a half-written config.json that CH would then fail to parse.
config_dir = os.path.dirname(config_path) or "."
fd, tmp_path = tempfile.mkstemp(prefix=".config.json.", dir=config_dir)
try:
    with os.fdopen(fd, "w", encoding="utf-8") as out:
        json.dump(config, out)
        out.flush()
        os.fsync(out.fileno())
    os.replace(tmp_path, config_path)
except Exception:
    try:
        os.unlink(tmp_path)
    except OSError:
        pass
    raise
PY
  then
    echo "[wrapper] FATAL: config.json path rewrite failed" >&2
    exit 1
  fi

  # R15-S2 test sketch — no automated harness exists for this heredoc
  # today (adding one would mean a new Rust integration test under
  # `crates/sandbox/tests/`, outside this fixer's scope; tracked
  # implicitly in the deferred backlog under "wrapper test harness").
  # Manual repro recipe for the new allow-list / prefix-guard:
  #
  #   tmp=$(mktemp -d) && mkdir -p "$tmp/alloc"
  #
  #   # Case 1 — happy path: alloc-prefix path rewrites + passes.
  #   printf '%s\n' '{"disks": [{"path":
  #     "/opt/nomad/data/alloc/aaaa/t/local/rootfs.img"}]}' \
  #     > "$tmp/config.json"
  #   NOMAD_TASK_DIR="$tmp/alloc" CONFIG_JSON="$tmp/config.json" \
  #       python3 <(awk '/^import json, os, re, sys, tempfile/,/^PY$/' \
  #                   crates/sandbox/scripts/nomad-vm-wrapper.sh \
  #                   | sed '$d')
  #   # → exit 0, config.json now has "$tmp/alloc/rootfs.img"
  #
  #   # Case 2 — attack: `/etc/shadow` rejected (R15-S2 reject + rc!=0).
  #   #   {"disks": [{"path": "/etc/shadow"}]}
  #
  #   # Case 3 — `..` traversal: rejected by the explicit component
  #   # check before realpath.
  #   #   {"disks": [{"path":
  #   #     "/opt/nomad/data/alloc/aaaa/t/local/../../escape"}]}

  echo "[wrapper] restore path: source=$ZSBX_RESTORE_FROM, NOMAD_TASK_DIR=$NOMAD_TASK_DIR"
  cloud-hypervisor \
    --api-socket "$API_SOCK" \
    --restore    "source_url=file://$ZSBX_RESTORE_FROM" \
    > "$ZSBX_RUNTIME/ch.log" 2>&1 &
  CH_PID=$!

  # Bug #17 fix (B17, 2026-05-24): CH `--restore` brings the VM back
  # in a **paused** state — vCPUs are not running until something
  # explicitly resumes them. Without `ch-remote resume` after restore
  # the guest's eth0 never replies to ARP and the controller's
  # /livez probe gets EHOSTUNREACH ("No route to host"). CH's
  # documented snapshot/restore protocol since v23: caller must
  # `ch-remote resume` after `--restore`. See CH docs
  # `docs/snapshot-restore.md` § "Restore from a VM snapshot".
  #
  # The cold-boot path doesn't need this: a normal `cloud-hypervisor
  # --kernel ...` starts the VM running from boot.
  #
  # We poll the API socket first (CH may take up to ~1s to bind it
  # after mmap'ing the snapshot memory), then issue the resume. Both
  # steps are bounded; total budget ~10s. If resume fails the
  # subsequent /livez probe in the controller will surface it.
  (
    # Wait for the CH HTTP API socket to appear + accept connections.
    # `ch-remote ping` is the lightweight liveness probe.
    for attempt in $(seq 1 50); do
      if [ -S "$API_SOCK" ] && \
         ch-remote --api-socket "$API_SOCK" ping >/dev/null 2>&1; then
        echo "[wrapper] restore: ch-remote api ready (attempt=$attempt)" >&2
        break
      fi
      sleep 0.2
    done
    # Issue resume. CH returns success even if VM is already running,
    # so this is idempotent on re-wakes.
    if ch-remote --api-socket "$API_SOCK" resume 2>&1 | \
         sed 's/^/[wrapper] restore: ch-remote resume: /' >&2; then
      echo "[wrapper] restore: VM resumed" >&2
    else
      echo "[wrapper] restore: WARN ch-remote resume failed; /livez probe will surface it" >&2
    fi

    # Bug-#14b speculative fix kept as defensive belt-and-braces:
    # re-up the tap after CH spawn. CH's `--restore` re-attaches to
    # the tap by name; depending on driver behaviour the tap can end
    # up admin-DOWN even though we set it UP pre-spawn. Log state at
    # 0/1/3s post-spawn so the next cluster cycle has empirical
    # evidence.
    for delay in 0.3 1 3; do
      sleep "$delay"
      ip -br link show "$TAP" 2>&1 | sed "s/^/[wrapper] restore: tap@+${delay}s   /" >&2 || true
      ip link set "$TAP" up 2>&1 | sed "s/^/[wrapper] restore: tap-up-retry@+${delay}s: /" >&2 || true
    done
  ) &
  RESUME_PID=$!   # R6-C1: capture for cleanup trap reap
else
  # Cold boot. The cmdline carries the controller pubkey as hex
  # (`zsbx_pubkey=<hex>`); /sbin/init in the guest decodes it and
  # writes /keys/controller-pubkey before exec'ing sandbox-agent.
  # The two extra `--disk` entries (workspace + userhome) appear in
  # the guest as /dev/vdb + /dev/vdc respectively (PCI device order
  # matches CH argument order); init.sh mounts them at /workspace
  # and /userhome, formatting on first boot if unformatted.
  #
  # R8-DEPLOY1: append `SANDBOX_AGENT_SANDBOX_ID=$ZSBX_SANDBOX_ID`
  # to the cmdline. The Linux boot protocol passes any `KEY=VALUE`
  # cmdline token the kernel doesn't recognise straight through to
  # /sbin/init's *environment*; sandbox-agent's
  # `init_sandbox_id_from_env`
  # (crates/sandbox-agent/src/handlers.rs:95) reads it via
  # `std::env::var("SANDBOX_AGENT_SANDBOX_ID")`. This satisfies
  # R7-S1's fail-closed boot assertion without a rootfs/init.sh
  # change. (The `zsbx_pubkey=…` arg above uses the same kernel
  # pass-through; init.sh chooses to re-read /proc/cmdline for it
  # only because it needs to hex-decode + write a file before
  # exec.) Validated above (alnum + `_` only) so the kernel's
  # whitespace-split tokeniser sees exactly one token.
  cloud-hypervisor \
    --api-socket "$API_SOCK" \
    --kernel    vmlinuz \
    --cmdline   "console=ttyS0 root=/dev/vda rw init=/sbin/init reboot=t panic=1 ip=${VM_IP}::${HOST_IP}:255.255.255.252::eth0:none zsbx_pubkey=${ZSBX_PUBKEY_HEX} SANDBOX_AGENT_SANDBOX_ID=${ZSBX_SANDBOX_ID}" \
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
