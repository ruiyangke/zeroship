# Spike: volantvm/nomad-driver-ch — adopt/reference/reject

**Date:** 2026-05-22
**Author:** sandbox-snapshot-restore worktree
**Feeds:** `docs/proposals/nomad-driver-ch.md`
**Verdict (TL;DR):** **REFERENCE.** Worth reading; not worth forking.

---

## 0. Provenance — what we actually looked at

The user asked us to investigate `volantvm/nomad-driver-ch`. The `volantvm` GitHub org has been renamed to the personal handle `0xchasercat` (formerly `ccheshirecat`); all `volantvm/*` URLs now silently 301 to `0xchasercat/*`. Confirmed by:

- `gh api orgs/volantvm` → `404 Not Found`.
- `curl -L https://github.com/volantvm/volant` → 200, but final URL = `https://github.com/0xchasercat/volant`.
- The Hacker News launch thread for the driver ([HN #45480523](https://news.ycombinator.com/item?id=45480523)) is titled "Show HN: Nomad task driver for Cloud Hypervisor" and links to what is now `0xchasercat/nomad-driver-ch`.

The driver is the work of a single developer (`0xchasercat` / "mx" / chaser.sh, who also maintains the sibling `volant` microVM orchestrator under HYPR PTE. LTD. — a one-person company per GitHub bio). The driver is licensed **MPL-2.0** with `Copyright (c) 2024 HashiCorp, Inc.` headers throughout — it is a hard fork of `hashicorp/nomad-driver-virt` (the libvirt-backed driver), with the libvirt layer ripped out and replaced by direct CH spawning + REST API control. The fork relationship is visible in the `CHANGELOG.md`, which still references `hashicorp/nomad-driver-virt` PRs.

**Repo stats** ([repo](https://github.com/0xchasercat/nomad-driver-ch)):

| | |
|---|---|
| Stars | 19 |
| Forks | 4 |
| Open issues | 0 |
| Total commits | **2** (`5f7f628` initial + `683a1e0` README update) |
| Created | 2025-10-02 |
| Last push | 2025-10-05 (then quiet for ~7 months) |
| Language | Go (HashiCorp go-plugin v2) |
| License | MPL-2.0 |
| Tests | Yes — `*_test.go` for `driver`, `config`, `handle`, `state`, `vfio_manager`, `cloudinit`, `net/config`, `chnet`. No integration / Nomad-cluster smoke. |
| LOC | ~5,100 Go (excl. tests) — see file breakdown below. |

Files and sizes (bytes):

```
virt/driver.go                  27,404   the gRPC driver plugin (Start/Stop/Recover/Wait)
virt/config.go                  12,339   HCL spec + TaskConfig decoder
virt/handle.go                   3,693   per-task handle, state monitor
virt/state.go                      773   tiny state-machine wrapper
virt/net/{config,net}.go         7,079   HCL net spec + types
cloudhypervisor/driver.go       30,738   CH backend (Virtualizer impl)
cloudhypervisor/vm_operations.go 17,952   CH spawn / REST PUT vm.create + vm.boot
cloudhypervisor/vfio_manager.go 13,263   PCI passthrough plumbing
cloudinit/{cloudinit,iso9660}.go 6,590   ISO9660 NoCloud datasource builder
chnet/controller.go             20,718   bridge + iptables NAT port-forward controller
chnet/controller_default.go      1,545   non-linux stub
internal/shared/domain.go        7,150   shared types (Config, CloudHypervisor, Network, VFIO)
main.go                            413   plugins.Serve(factory)
```

There are **zero open issues** because there are **zero users** filing any. The repo is best understood as a working proof-of-concept that one person pushed to GitHub, did a Show-HN on, and then walked away from.

---

## 1. What the driver actually does

This is `nomad-driver-virt` (libvirt) reskinned to talk to CH directly. The flow per `cloudhypervisor/driver.go::CreateDomain`:

1. **Allocate IP** from a configurable pool inside a single shared subnet (`network.subnet_cidr` / `ip_pool_start` / `ip_pool_end`). Single global gateway. No `/30` concept.
2. **Generate a deterministic MAC** from a 31-multiplier hash of the VM name. (Same trick we use today, but ours is HMAC of a worker-secret.)
3. **Mint a TAP name** as `tap_prefix + sha256(name||unix_nano)[:8]` to dodge IFNAMSIZ.
4. **Build a NoCloud cloud-init ISO** (`cloudinit/`) — meta-data, user-data, vendor-data, network-config rendered to iso9660 written to a per-VM workdir.
5. **Create + up the TAP**, attach it to the configured bridge (`exec.Command("ip", "tuntap", "add" / "link set up" / "link set master <bridge>")`).
6. **Start virtiofsd × N** — one per virtio-fs `Mount`, with the tag-named socket in the workdir.
7. **`exec.Command(cloud-hypervisor, "--api-socket", path, "--log-file", path, "--seccomp", true)`**. That's the entire CH argv — no `--kernel`, `--cmdline`, `--memory`, `--cpus`, `--disk`, `--net`, `--serial`. CH starts with no config; the plugin then …
8. **`PUT /api/v1/vm.create`** over the unix socket with a JSON `VMConfig` body, then `PUT /api/v1/vm.boot`. That JSON includes `payload.kernel`, `payload.initramfs`, `payload.cmdline`, `disks`, `net`, `fs`, `console`, `serial`, `platform.iommu*`, `devices` (VFIO).
9. **`waitForVMState("running")`** by polling `GET /api/v1/vm.info` once per second up to 60s.

Stop path:

- `StopDomain` → `PUT /api/v1/vm.shutdown` → on failure `os.Process.Kill()` (SIGKILL). No `ch-remote shutdown`, no SIGTERM step, no configurable timeout. (Driver has a hard-coded `defaultShutdownTimeout = 30s` used only as the deadline `waitForVMState(shutoff)` waits before the loop in `monitor` notices and the *controller* may then call DestroyTask.)
- `DestroyDomain` → `shutdownVM` (best effort) → `cleanupProcess` (SIGKILL CH + SIGKILL virtiofsd + `ip link delete tap…` + `os.RemoveAll(workDir)`).

`RecoverTask` (`virt/driver.go:RecoverTask`) decodes the persisted handle, then calls `taskGetter.GetDomain(name)` — which **looks up `Driver.processes[name]`**, an in-memory `map[string]*VMProcess` that **was not persisted to disk**. After a Nomad-client (and therefore plugin-process) restart, the map is empty. `GetDomain` returns `nil`, `RecoverTask` returns `drivers.ErrTaskNotFound`, and the alloc is treated as gone. **The orphan-CH recovery story is broken.** This is the one capability we most wanted from a real driver.

---

## 2. Requirement-by-requirement gap analysis

| # | Capability we need | What volantvm does | Gap |
|---|---|---|---|
| 1 | **Cold-boot CH spawn with our cmdline shape** (`--kernel`, `--cmdline 'console=hvc0 reboot=k panic=-1 ip=<IP>::<GW>:255.255.255.252::eth0:off zsbx_pubkey=<HEX> SANDBOX_AGENT_SANDBOX_ID=<UUID>'`, `--memory size=<MB>`, `--cpus boot=2`, three `--disk` entries, `--net tap=…`, `--api-socket`, `--serial file=`, `--console null`) | Spawns CH with **only `--api-socket --log-file --seccomp`**, then `PUT vm.create` with a JSON body. The JSON does carry `payload.kernel/cmdline/initramfs`, one base-image disk, and a cloud-init ISO disk. Cmdline is auto-decorated with a Linux-style `ip=A::G:M:H:eth0:none` token if a static IP exists (`vm_operations.go:313-329`). | **Large.** Our cmdline is opaque to the driver (carries `zsbx_pubkey`, `SANDBOX_AGENT_SANDBOX_ID`); we'd need to either pass it verbatim through `task.config.cmdline` (the driver does honour that field) and disable the auto `ip=` injection, or rewrite the JSON builder. Doable but invasive. Multi-disk (item 5 below) is the bigger blocker. **~3-5 days** to retool. |
| 2 | **`--restore <snapshot-dir>` + post-spawn `ch-remote resume`** (B17 fix) | **Not implemented.** Driver has no `vm.snapshot` or `vm.restore` REST calls, no `--restore` argv path, no resume step. Snapshot/restore is entirely absent from the codebase (verified by `grep restore`, `grep snapshot` across the tree — zero hits in production code paths). | **Critical missing feature.** The driver would need a new task-config field `restore_from`, a switch on `startCHProcess` to use `--restore` instead of `--api-socket`-then-`vm.create`, and the explicit resume RPC. **~2-3 days** to add, plus the redesign of `createAndBootVM` to be skipped on restore. |
| 3 | **Per-VM /30 tap** (10.99.X.1/30 host, 10.99.X.2/30 guest, host-as-gateway, **no shared bridge**) | Pure shared-bridge model. Creates `br0`, attaches all taps to it, allocates IPs from one shared pool, configures iptables NAT in chains `NOMAD_CH_PRT` / `NOMAD_CH_FW` for port-forward. The `--net` JSON entry has `tap`, `mac`, `ip`, `mask` — but no support for "host-as-gateway, point-to-point". | **Architecturally incompatible.** Driver assumes one bridge per host with N VMs sharing it; we explicitly want one /30 per VM (no inter-VM L2). Would need to delete `chnet/controller.go`, replace `setupNetworking`, replace `ensureBridgeConfigured`, and remove the IP-pool allocator. **~4-6 days** + tests. |
| 4 | **Multi-disk virtio-blk** (rootfs.img RO + workspace.img RW per-sandbox + userhome.img RW per-user) | HCL spec `disks` block is *parsed* (`virt/config.go::DiskConfig`) but **never propagated to `domain.Config` or the CH JSON.** `buildVMConfig` builds `vmConfig.Disks` from exactly two sources: `config.BaseImage` (single image) + the cloud-init ISO. The user's `disks` list is silently dropped. (Real bug — the HCL block is dead code.) | **Medium.** Need to (a) extend `domain.Config` with a `Disks []DiskSpec`, (b) wire it through `virt/driver.go::StartTask`, (c) make `buildVMConfig` emit them. We also need to drop cloud-init entirely (we don't use it; we inject identity via cmdline). **~2 days** if we don't fight the cloud-init coupling, **~5 days** if we tear out cloud-init too. |
| 5 | **`RecoverTask` re-attaches to existing CH** by reconstructing the `VMProcess` map from on-disk state (workdir + api.sock + pid file) | `RecoverTask` calls `GetDomain` which only reads an **in-memory** `processes` map. The map is not persisted. After plugin restart, recovery always returns `ErrTaskNotFound`. The wrapper has a `WorkDir` and an `api.sock` per VM on disk — the building blocks are there, but the glue isn't. | **Medium.** Need to persist process metadata (pid, api.sock, tap, ip, mac) per task, ideally via Nomad's `TaskHandle.DriverState` which already round-trips through `MsgPackEncode`. The driver does set `TaskState{TaskConfig, StartedAt, NetTeardown}`, but **not `Pid` / `APISocket`**. Adding them + a recovery path that pings `/api/v1/vm.info` over the socket is **~2-3 days**. |
| 6 | **Graceful stop chain**: `ch-remote shutdown` → wait → SIGTERM CH → wait → SIGKILL | Driver does `PUT /api/v1/vm.shutdown` then on failure `os.Process.Kill()` (SIGKILL only, no SIGTERM). Hard-coded 30s timeout; the Nomad-provided `StopTask(taskID, timeout, signal)` `timeout` argument is ignored. | **Small.** Implement the ladder in `StopDomain`. **~0.5 days.** |
| 7 | **TaskStats**: report memory / cpu / paused | Driver exposes `TaskStats` (channel-based), but `GetStats()` returns `info.Memory` (from `vm.info`'s `memory.actual_size`) and `CPUTime: 0` (literal hard-coded zero — `cloudhypervisor/driver.go:917`). | **Small.** CH does expose CPU time via `vm.info`'s per-vCPU stats and `vm.counters`; need to plumb them. **~1 day.** |
| 8 | **Clock-resync hook** between agent `/livez` and `register_restored` | Driver-agnostic — our controller-side concern; the driver only needs to *not* mark the task running until the agent is reachable, or to expose an extension point. The current `waitForVMState("running")` doesn't probe the guest at all. | **Zero on the driver side** — the controller does the resync. But we lose nothing here. |
| 9 | **CH v51.1 compatibility** | Driver targets CH **v48+**. The CLI flags it uses (`--api-socket`, `--log-file`, `--seccomp`) and JSON shapes (`vm.create`/`vm.boot`/`vm.info`/`vm.shutdown`) are stable across v48-v51 per the CH changelog, so this is fine. `--restore` is also stable, but again — not used. | **None.** |

### Decision matrix (summary)

| Capability | Required | volantvm has it | Gap effort |
|---|---|---|---|
| Cold-boot CH spawn (our cmdline) | yes | partial (different shape) | 3-5 days |
| `--restore` + `ch-remote resume` | yes | **no** | 2-3 days |
| Per-VM tap /30 | yes | **no** (shared bridge) | 4-6 days |
| Multi-disk virtio-blk | yes | **no** (HCL parsed, dropped) | 2-5 days |
| RecoverTask | yes | **broken** | 2-3 days |
| Graceful stop chain | yes | partial (no SIGTERM) | 0.5 days |
| TaskStats | nice-to-have | partial (no CPU) | 1 day |
| Clock-resync hook | yes | n/a (driver-agnostic) | 0 |
| **Sum** | | | **~15-23 days** to retool volantvm to fit |

For comparison, our proposal (`docs/proposals/nomad-driver-ch.md`) is a **Rust** driver written from scratch against the go-plugin gRPC wire protocol, targeting the **compio/io_uring + zero-tokio** house style. It would not use any volantvm code; it would build directly on `compio-net`'s unix-socket support, `prost` for the gRPC types we vendor from Nomad protobufs, and our existing `compio-postgres` patterns.

---

## 3. Why REFERENCE (not ADOPT, not REJECT)

### Against ADOPT (fork-and-modify)

1. **Wrong language.** Driver is Go. Our entire stack — including the controller that drives Nomad — is Rust on `compio`. A Go driver introduces a Go toolchain dependency, a Go runtime + GC in our hot path, and a foreign concurrency model (goroutines, `time.Ticker`) right where our zero-tokio invariant matters most.

2. **Wrong network model.** Shared bridge with central IP pool collides head-on with our per-VM /30 + host-as-gateway requirement. We would delete the entire `chnet/` package (~570 LOC) and the `setupNetworking` half of `cloudhypervisor/vm_operations.go`.

3. **Wrong control path.** We spawn CH via argv (`--kernel`, `--cmdline`, `--memory`, `--cpus`, three `--disk`s, `--net tap=…`, `--api-socket`, `--serial`, `--console null`). The driver bare-spawns CH and *then* calls `PUT vm.create`. Either we (a) rewrite `startCHProcess` to use our argv shape (re-derives our wrapper's flag-building logic in Go, in a separate codebase, in MPL-2.0), or (b) move our cmdline into the JSON body (kills our existing snapshot-restore plan in `docs/proposals/sandbox-snapshot-restore.md` which uses `--restore <dir>` for the cold path).

4. **No snapshot/restore.** The single most-valuable feature we need from a real driver — `--restore` + `ch-remote resume` — is missing. This is the entire reason this worktree exists.

5. **`RecoverTask` is broken.** The second-most-valuable feature we wanted (orphan-CH safety after Nomad-client restart) does not actually work. We'd ship our own persistence layer.

6. **Single-person, abandoned, 2 commits, MPL-2.0.** MPL-2.0 is fork-friendly (file-level copyleft only — we can statically link from proprietary Rust without infecting it), but the upstream is a dead branch we would carry forever with no contributions inbound. The CHANGELOG still has unreleased entries from `hashicorp/nomad-driver-virt`.

7. **Cloud-init coupling.** Every code path assumes a NoCloud ISO is mounted at `<workDir>/<name>.iso`. We don't use cloud-init at all — our agent is wired in via cmdline (`zsbx_pubkey=`, `SANDBOX_AGENT_SANDBOX_ID=`) and the agent ramdisk. Removing cloud-init is invasive (it's also how the driver passes the env file to the guest via `/etc/profile.d/virt.sh`, which we'd replace with cmdline-injected env).

8. **MPL-2.0 mechanics: an exercise in lawyering.** MPL-2.0 is file-level copyleft. If we copy a file and modify it, *that file* must remain MPL-2.0 and source-available — but our combined Rust binary is fine. This is workable, but it means every Go file we touch is a permanent licensing burden, and we'd still be writing Rust around it (via cgo or out-of-process), which is a worse architecture than just writing the whole thing in Rust.

### Against REJECT (ignore entirely)

The driver is the closest published reference for **how to talk to CH's REST API for non-trivial lifecycle management from a Nomad-driver context**. Specifically valuable as a reference:

- **`vm_operations.go::buildVMConfig`** is a clean worked example of the v48+ `vm.create` JSON shape: `cpus.boot_vcpus`, `memory.size` (in **bytes**, not MB), `payload.{kernel,initramfs,cmdline}`, `disks[].{path,readonly,serial}`, `net[].{tap,mac,ip,mask}`, `rng.src`, `fs[].{tag,socket,num_queues,queue_size}`, `platform.{num_pci_segments,iommu_segments,iommu_address_width}`, `devices[].{path,id,iommu,pci_segment}`, `console.mode`, `serial.{mode,file}`. We can crib the field names directly. (We'd verify against CH v51.1's `vmm/src/api/openapi/cloud-hypervisor.yaml` rather than trust the Go.)

- **`vm_operations.go::httpRequest`** shows the right unix-socket HTTP transport idiom (`http.Transport.DialContext` returning `net.Dial("unix", socketPath)`, dummy hostname). We'll do the same with `compio-net`'s `UnixStream` + a minimal HTTP/1.1 codec, since we don't want a tokio-based HTTP client in this code path.

- **`virt/driver.go::Capabilities`** documents the right set of capabilities flags for a VM driver: `SendSignals: false`, `Exec: false`, `DisableLogCollection: true`, `FSIsolation: Image`, `NetIsolationModes: [Host, Group]`, `MustInitiateNetwork: false`, `MountConfigs: None`. We can copy this verbatim.

- **`virt/driver.go::RecoverTask` skeleton** is correct in *shape* (decode `TaskState`, look up by domain name, set `procState`); just the lookup target is wrong. We'd use the same shape and persist to disk properly.

- **`virt/handle.go::monitor`** is a correct example of a per-task ticker-based state poller that exits the WaitTask channel exactly once. Our compio equivalent will mirror this structure (replacing `time.NewTicker` + select with `compio::time::interval`).

- **`virt/config.go::configSpec`** is a complete, vetted HCL schema for the *driver-config* surface (`cloud_hypervisor.bin`, `network.bridge`, `image_paths`, …) and the *task-config* surface (`image`, `kernel`, `initramfs`, `cmdline`, `disks`, `fs_mounts`, `vsock`, `rng`, `devices`, `platform`, `vfio_devices`). Even though we'll use a different shape (per-VM /30, no cloud-init), the HCL DSL conventions and the field names (`use_thin_copy`, `default_user_authorized_ssh_key`) are useful to read.

- **The go-plugin handshake serialization** (in `main.go` + `plugins.Serve`) — our proposal already documents this from `hashicorp/go-plugin`'s source, but having a working example to diff against during implementation is useful.

We will **NOT** crib:

- `chnet/controller.go` (wrong network model);
- `cloudinit/` (we don't use cloud-init);
- `vfio_manager*` (we don't pass through devices in sandbox);
- the `Driver.processes` in-memory map approach (broken for recovery);
- the deterministic MAC hash function (we use HMAC-of-worker-secret);
- the IP-pool allocator (we deterministically derive /30s from `vm_index`).

---

## 4. Specific code-reading itinerary for the implementing PR

When the proposal author starts the implementation, the *highest-yield* files to read first are, in order:

1. `cloudhypervisor/vm_operations.go` lines **275-398** — `buildVMConfig`. The exact JSON shape we'll post to `vm.create`.
2. `cloudhypervisor/vm_operations.go` lines **400-464** — `startCHProcess` + `waitForAPISocket`. The right way to fork CH and wait for its API socket to come up.
3. `cloudhypervisor/vm_operations.go` lines **582-612** — `httpRequest`. The unix-socket HTTP transport idiom.
4. `virt/driver.go` lines **460-530** — `StartTask`. The right argument-marshaling pattern for `TaskConfig → domain.Config` (we'll write `TaskConfig → SandboxBootSpec` instead).
5. `virt/driver.go` lines **560-600** — `RecoverTask`. The shape is right; we replace the in-memory lookup with on-disk reconstruction.
6. `virt/handle.go` — the whole file (~150 lines). The monitor-ticker pattern for `WaitTask` + `TaskStats`.
7. `virt/config.go` lines **15-140** — the HCL `configSpec` / `taskConfigSpec` DSL. We're not using this DSL (we'll define `TaskConfig` as a Rust struct + serde), but the field set is informative.

Everything else can be skipped. The whole spike took ~3 hours of reading and the implementation will not benefit from re-reading the cloud-init builder or the VFIO manager.

---

## 5. Verdict

**REFERENCE.** Read it, crib the CH-REST JSON shape and the go-plugin capability flags, then write our own driver in Rust per `docs/proposals/nomad-driver-ch.md`. Forking saves **0 days net** (the gap work to retool volantvm to our requirements is roughly equal to writing from scratch in Rust on top of compio, and leaves us with a Go binary in our build pipeline forever).

**Days saved by referencing (vs greenfield reading the Nomad source ourselves):** ~2 days of reverse-engineering CH's `vm.create` JSON contract and the go-plugin handshake. That's the entire value extracted.

**Days saved by adopting:** approximately zero, possibly negative. ~15-23 days of gap-closing work to retrofit volantvm to our network model + multi-disk + restore path + persisted recovery, in a language we don't ship anywhere else, against a MPL-2.0 upstream with one author and no commits in seven months.

---

## Sources

- [0xchasercat/nomad-driver-ch](https://github.com/0xchasercat/nomad-driver-ch) — the actual repo (was `volantvm/nomad-driver-ch`).
- [README.md](https://github.com/0xchasercat/nomad-driver-ch/blob/main/README.md) — feature list, install instructions, CH v48+ pin.
- [CHANGELOG.md](https://github.com/0xchasercat/nomad-driver-ch/blob/main/CHANGELOG.md) — references the `hashicorp/nomad-driver-virt` fork lineage.
- [virt/driver.go](https://github.com/0xchasercat/nomad-driver-ch/blob/main/virt/driver.go) — driver plugin, `StartTask`/`RecoverTask`/`StopTask`/`DestroyTask`.
- [cloudhypervisor/vm_operations.go](https://github.com/0xchasercat/nomad-driver-ch/blob/main/cloudhypervisor/vm_operations.go) — CH spawn, `vm.create` JSON build, REST loop.
- [cloudhypervisor/driver.go](https://github.com/0xchasercat/nomad-driver-ch/blob/main/cloudhypervisor/driver.go) — `CreateDomain`, MAC/TAP/IP allocators, IP pool.
- [chnet/controller.go](https://github.com/0xchasercat/nomad-driver-ch/blob/main/chnet/controller.go) — shared-bridge + iptables NAT controller (we are not using this).
- [LICENSE](https://github.com/0xchasercat/nomad-driver-ch/blob/main/LICENSE) — MPL-2.0, HashiCorp copyright.
- [HN #45480523](https://news.ycombinator.com/item?id=45480523) — Show HN launch thread (Oct 2025).
- [0xchasercat/volant](https://github.com/0xchasercat/volant) — sibling microVM orchestrator from same author, custom-licensed (HYPR PTE. LTD.). Snapshot/restore is roadmap'd for Q4 2025, not shipped.
- [hashicorp/nomad-driver-virt](https://github.com/hashicorp/nomad-driver-virt) — the libvirt-based driver this is forked from.
- [Cloud Hypervisor OpenAPI](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/vmm/src/api/openapi/cloud-hypervisor.yaml) — authoritative source for the REST shape we'll match against (verify against v51.1 tag).
