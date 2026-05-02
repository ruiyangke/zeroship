# sandbox-nomad-ch operator runbook

Audience: SRE/operators running zeroship's `nomad-ch` sandbox backend on bare metal — `nomad agent` (raw_exec) + Cloud Hypervisor + virtiofsd, no Kubernetes.

## What this is

A controller-side `Backend::NomadCh` variant that submits one Nomad `raw_exec` job per sandbox. Each job invokes [`crates/sandbox/scripts/nomad-vm-wrapper.sh`](../../crates/sandbox/scripts/nomad-vm-wrapper.sh), which spawns:

- 3 × `virtiofsd` (one per share — `keys`, `workspace`, `userhome`)
- 1 × `cloud-hypervisor` foreground process

The CH VM boots a tiny Linux kernel + raw ext4 rootfs whose `/sbin/init` mounts the three virtio-fs shares and execs `zeroship-sandbox-agent` (the same binary the K8s backend uses). Wire-protocol-v1 auth (Ed25519 signed requests) is identical to the K8s backend; only the runtime plumbing differs.

Crate: [`crates/sandbox/src/backend/nomad_ch.rs`](../../crates/sandbox/src/backend/nomad_ch.rs).

## When to use this vs. k8s

| Question | nomad-ch | k8s |
| --- | --- | --- |
| Operator already runs Nomad? | yes — minimal additional surface | k8s anyway |
| Cluster networking abstraction? | tap+/30 (operator-managed) | CNI |
| Per-user persistent storage? | host bind-mount (single-node) | PVC + StorageClass |
| HA controller? | not designed for it (one controller per host) | yes |
| Upgrade story for the runtime? | re-bake rootfs + bump artifact dir | rolling RuntimeClass |

For single-node / small-fleet operators with libkrun-equivalent isolation needs, `nomad-ch` is the bare minimum. For multi-tenant fleet operators with a real cluster, use `k8s`.

## Host prerequisites

1. **Nomad agent** with `raw_exec` enabled. The controller talks JSON to the Nomad HTTP API (default `http://127.0.0.1:4646`) — not the `nomad` CLI.
2. **Cloud Hypervisor** (`cloud-hypervisor`) and **virtiofsd** on `PATH`.
3. **A vmlinuz built with `CONFIG_IP_PNP=y`** at `${ZSBX_ARTIFACT_DIR}/vmlinuz`. The wrapper passes `ip=…::…:255.255.255.252::eth0:none` on the kernel command line; without `IP_PNP` the agent boots without a network address.
4. **A raw ext4 rootfs** at `${ZSBX_ARTIFACT_DIR}/rootfs-slim.img`. `/sbin/init` must:
   - mount `keys`, `workspace`, `userhome` virtio-fs tags
   - exec `/usr/local/bin/zeroship-sandbox-agent`
5. **Pre-provisioned tap devices** named `zsbx-nm-${IDX}` for every IDX in `[SANDBOX_NOMAD_CH_VM_INDEX_FLOOR, SANDBOX_NOMAD_CH_VM_INDEX_CEIL]`. Each tap lives in its own `/30`:
   - host  IP: `10.${SUBNET_BASE}.$((100+IDX)).1`
   - VM    IP: `10.${SUBNET_BASE}.$((100+IDX)).2`
   - mask:    `255.255.255.252`
   - `${SUBNET_BASE}` is the `subnet_second_octet` config (default 99).

   The wrapper does NOT manage these. They survive across sandbox lifetimes; the controller's `vm_index_allocator` hands them out from a free list.
6. **Wrapper installed at the controller-configured path** (`SANDBOX_NOMAD_CH_WRAPPER_PATH`, default `/etc/zeroship/nomad-vm-wrapper.sh`). On nodes where the controller and Nomad client share a filesystem the controller validates `wrapper_path.exists() && executable` at startup; on split deployments this check is best-effort.

## Configuration

All env-var defaults match what the controller validates at boot. Failing-fast on misconfig is the design.

| Env var | Default | Purpose |
| --- | --- | --- |
| `SANDBOX_NOMAD_ADDR` | `http://127.0.0.1:4646` | Nomad HTTP API. Must start with `http://` or `https://`. Trailing slash is auto-trimmed. |
| `SANDBOX_NOMAD_DATACENTER` | `dc1` | Datacenter advertised in the job spec. |
| `SANDBOX_NOMAD_CH_WRAPPER_PATH` | `/etc/zeroship/nomad-vm-wrapper.sh` | Path the `raw_exec` task invokes. |
| `SANDBOX_NOMAD_CH_RUNTIME_DIR` | `/var/lib/zeroship/ch` | Artifact dir holding `vmlinuz` + `rootfs-slim.img`. Becomes `ZSBX_ARTIFACT_DIR` inside the wrapper. |
| `SANDBOX_NOMAD_CH_HOST_STATE_DIR` | `/var/zeroship/ch` | Per-sandbox host state. Each sandbox gets `<dir>/<sandbox-id>/{keys,workspace}`. |
| `SANDBOX_NOMAD_CH_USER_HOME_ROOT` | `/var/zeroship/ch/users` | Per-user persistent home. Each user gets `<root>/<user_id>/home`. **Must be either `<host_state_dir>/users` or fully disjoint** — descendant overlap is rejected at config load. |
| `SANDBOX_NOMAD_CH_VM_INDEX_FLOOR` | `1` | Lower bound of the index pool. Must be ≥ 1 (idx=0 reserves the `.100` subnet for what's effectively a sentinel). |
| `SANDBOX_NOMAD_CH_VM_INDEX_CEIL` | `155` | Upper bound. `100 + ceil ≤ 255` so the third octet fits a u8 — config load rejects ≥ 156. |
| `SANDBOX_NOMAD_CH_SUBNET_BASE_OCTET` | `99` | Second octet of the per-VM /30. Shift this if the host has a corp `10.99/16` collision. Loopback (127), link-local (169), multicast (224-255) are rejected. |
| `SANDBOX_NOMAD_CH_ALLOC_RUNNING_TIMEOUT_SECS` | `60` | Bound on Nomad scheduling latency to "alloc running". |
| `SANDBOX_NOMAD_CH_AGENT_LIVEZ_TIMEOUT_SECS` | `30` | Bound on CH boot + kernel + init.sh + agent up. Separate budget from alloc-running so operators can tell apart "Nomad slow" from "in-VM slow". |
| `SANDBOX_NOMAD_CH_STARTUP_ORPHAN_CLEANUP` | `false` | On startup, list every Nomad job with prefix `zsbx-` and stop+purge. **Defaults OFF**; opt-in for single-replica deployments. **Dangerous in HA**: the first replica nukes every other replica's active sandboxes on rolling restart. |

## Health and the circuit-breaker

The backend exposes `is_healthy()` (read by `/readyz` and consulted by `create()`). Probe loop hits `/v1/status/leader` on a 5-second budget; status==200 flips healthy true. Anything else captures `last_probe_err` and sets healthy false.

`create()` checks `is_healthy()` before submitting any RPC. When false, the call returns a *terminal* error:

```
nomad-ch backend unhealthy; refusing new sandboxes
(most recent probe error: <captured>). This is a config or infra
problem; the probe loop will flip the bit back when Nomad recovers.
```

This is the M7 circuit-breaker — without it, 50 concurrent stalled Nomad RPCs would saturate the `compio::runtime::spawn_blocking` pool and queue every other backend op controller-wide.

## Cleanup contract (CreateGuard)

- `create()` is wrapped in a `CreateGuard` whose Drop spawns a detached compio task.
- The task purges the Nomad job (best-effort, 10s timeout), then on **confirmed purge** releases the `vm_index` to the pool and `rm -rf`s the host_dir.
- On purge failure (5xx, transport error, timeout) the index is **leaked**. `cleanup_orphans_at_startup` (or the next-boot orphan prune) reclaims it indirectly by deleting the surviving job. This avoids the failure mode where a retry-`create()` for the same user grabs the same index and races a still-alive prior wrapper for `tap=zsbx-nm-<idx>`.
- On controller crash or runtime-shutdown the cleanup task may not run. **Turn `SANDBOX_NOMAD_CH_STARTUP_ORPHAN_CLEANUP=true` on single-replica hosts so the next-boot prune mops up.**

The `stop()` path uses the same vm_index policy: release on confirmed purge, leak otherwise.

## Triage

### "alloc never reached running ... last status=&lt;no allocs&gt;"

Two distinct causes used to collapse into this single message; the round-3 fix surfaces them separately.

- If the timeout error appends `; last HTTP error (Nomad reachability): <e>`: **Nomad is unreachable**. Check the `nomad agent` process, ACL token, network path from controller → Nomad addr. The controller will mark the backend unhealthy and refuse new sandboxes via the circuit-breaker described above.
- If only `; last parse error` is appended: Nomad returned 200s but with garbage (HTML proxy interstitial?). Check for an HTTP proxy in front of Nomad.
- If neither is appended: the alloc genuinely never reached `running`. Check `nomad alloc status <alloc-id>`. Common causes: no nodes match constraints, raw_exec disabled, missing `cloud-hypervisor` / `virtiofsd` on `PATH`, kernel/rootfs missing at the artifact path.

### "agent at http://10.X.Y.2:7777 never returned 200 on /livez"

Allocation reached `running` (the wrapper started) but the in-VM agent didn't come up.

- Check `${NOMAD_TASK_DIR}/serial.log` for kernel output. Common culprits: missing `CONFIG_IP_PNP=y`, init.sh failing to mount one of the virtio-fs tags, agent binary missing from rootfs.
- Check `${NOMAD_TASK_DIR}/vfs-{keys,ws,home}.log` — virtiofsd often dies silently if the host share dir is missing or inaccessible.
- Check the controller log for `[sandbox=<id> vm_index=<idx> job=<job>] agent /livez: ...` (round 3 M1 added the prefix) to correlate.

### "vm-index allocator exhausted"

The pool is full. Either:

- Genuine fleet saturation — bump `SANDBOX_NOMAD_CH_VM_INDEX_CEIL` (up to 155).
- Leaked indices from CreateGuard purge failures or stop() Nomad-down events. Run `nomad job list -prefix=zsbx-` to count surviving jobs; turning on `SANDBOX_NOMAD_CH_STARTUP_ORPHAN_CLEANUP=true` and bouncing the controller will reclaim them.

### Wrapper "FATAL: virtiofsd socket ... did not appear within ~1s"

The wrapper waited up to 1s for each virtiofsd UDS, then bailed. Look at `${NOMAD_TASK_DIR}/vfs-{keys,ws,home}.log`. Most common cause: the host share dir doesn't exist or isn't readable by the user the Nomad agent runs as.

## Rebaking the rootfs after agent changes

Whenever the sandbox-agent source changes you need to bake a fresh agent
binary into `${ZSBX_ARTIFACT_DIR}/rootfs-slim.img`. There is one subtlety:
agent binaries built on a Nix host link against `/nix/store/...` glibc and
a Nix-store ELF interpreter. Dropped into a debian-trixie rootfs as-is the
binary boots with the kernel, then init's exec of the agent silently
fails (interpreter doesn't exist), and the failure surfaces several layers
up as "/livez never returned 200" — no agent log, no obvious cause.

The `bake-rootfs.sh` helper rewrites PT_INTERP to debian's canonical
`/lib64/ld-linux-x86-64.so.2`, strips the Nix RPATH, verifies the new
preview-feature capability tokens are present in the patched binary, and
copies it into the mounted rootfs at `/usr/local/bin/sandbox-agent`:

```sh
./crates/sandbox/scripts/bake-rootfs.sh \
    --build \
    target/release/sandbox-agent \
    /var/lib/zeroship/ch/rootfs-slim.img
```

Drop `--build` if you've already built the binary. The script needs
`sudo` (for the loop mount), `patchelf`, and `strings`. If the
capability-token check fails the binary is older than the preview-URL
feature; rebuild from this worktree and retry.

## Running long-lived processes inside the sandbox

`/exec` runs commands in a process group and kills the whole group with
SIGKILL when the request returns. This is intentional anti-DoS — the
shell can otherwise `cmd &`-fork a daemonized grandchild that lives
forever. Long-lived processes (Vite, Node, Python dev servers) must
escape the group with `setsid` AND have stdio detached:

    setsid sh -c 'node server.js >/tmp/server.log 2>&1 < /dev/null &'

A future "service API" (sandbox-controller side, not /exec) will
replace this pattern; until then, every long-lived launcher needs the
setsid wrapper.

## What's deliberately not in scope

- **Per-user `/home/u` persistence across hosts.** The current single-node design uses a host bind-mount. The next milestone replaces this with Ceph-RBD-backed volumes via a CSI plugin; until then, sandboxes scheduled on a different host won't see the user's package caches.
- **Multi-tenant cluster networking.** No CNI, no NetworkPolicy. The `/30` design intentionally has no gateway — sandboxes can only talk to the controller. If you need east-west traffic between sandboxes you want the k8s backend.
- **HA controller.** Two `nomad-ch` controllers on the same Nomad cluster will fight over `zsbx-` jobs at startup if both have orphan-cleanup on.
