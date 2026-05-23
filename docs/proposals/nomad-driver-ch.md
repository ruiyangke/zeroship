# Nomad task driver plugin for Cloud Hypervisor

**Date:** 2026-05-22
**Status:** Draft v0
**Audience:** sandbox/controller, platform-ops, security
**Depends on:**
- `crates/sandbox/scripts/nomad-vm-wrapper.sh` (666 LOC bash) — replaced wholesale by this driver (§ 1, § 5)
- `crates/sandbox/src/backend/nomad_ch.rs` — keeps the HTTP REST control-plane integration; `submit_nomad_job` gains a driver-name switch (§ 5)
- `crates/sandbox/src/restore_handler.rs::do_restore_inner` — restore orchestrator that today writes wrapper env vars; gains a `restore_from` task-config field path (§ 4)
- `crates/sandbox/src/persist.rs`, `registry.rs` — unchanged; the driver is invisible to the lifecycle state machine
- `docs/proposals/sandbox-snapshot-restore.md` — sibling proposal; this one is the *transport replacement*, that one is the *new lifecycle*. They land independently and can ship in either order.

---

## 1. Goal & non-goals

**Goal:** retire `crates/sandbox/scripts/nomad-vm-wrapper.sh` (666 LOC of bash currently in production) and replace it with a Rust-native Nomad task driver plugin that speaks the HashiCorp go-plugin gRPC wire protocol directly to the Nomad client, giving us:

1. **A native `RecoverTask` path** — the wrapper has no orphan-CH recovery story. After a Nomad client restart, in-flight CH processes become unparented and the controller can only kill+recreate. A driver gets `RecoverTask` for free.
2. **First-class `TaskStats`** — we have no per-VM memory/CPU telemetry today. The driver exposes a `Stats` stream backed by `ch-remote info`, and Nomad surfaces it in `nomad alloc status`, the UI, and Prometheus.
3. **Zero bash in the hot path.** Every cluster-affecting bug we've shipped in the last quarter (B12 missing `xxd`, B13 `printf %b`, B17 vCPU resume, B19 state registration, B22 clock resync, R8-DEPLOY1 env injection, W1 `sed` code-exec sink, R6-C1 subshell reap) has lived at the wrapper layer. Removing the bash removes the entire class.
4. **Nomad-native lifecycle semantics.** `StartTask`/`StopTask(timeout)`/`WaitTask`/`DestroyTask` map cleanly to CH operations. The wrapper's ad-hoc `trap`/`wait`/`kill -9` ladder becomes the driver's `StopTask` graceful-then-forceful path.

**Non-goals (this iteration):**
- Replacing Nomad. Still Nomad.
- Replacing Cloud Hypervisor. Still CH (≥ v51.1).
- Replacing the controller. `crates/sandbox/src/backend/nomad_ch.rs` continues to drive Nomad via HTTP REST; the controller doesn't know or care which driver Nomad uses.
- Replacing the in-VM agent. The signed-RPC channel between controller and sandbox-agent is untouched.
- Per-task network policy beyond what we do today (tap + virtio-net). CNI integration is a follow-up.

## 2. Background — the wrapper at end of life

The bash wrapper does five things, in order:

1. **vm_index → identity expansion.** Reads `ZSBX_VM_INDEX`, derives tap name, IP, MAC, virtio-blk disk path, virtiofsd socket paths.
2. **virtiofsd × 3 spawn.** Forks three virtiofsd daemons (`keys`, `userhome`, `assets`), each on a known UDS path.
3. **CH spawn.** Either `cloud-hypervisor --kernel ... --cmdline ...` (cold boot) or `cloud-hypervisor --restore ... && ch-remote resume` (restore).
4. **Process supervision.** `wait`s on the CH PID; on signal, `ch-remote shutdown` graceful → `kill -9` after timeout.
5. **Cleanup trap.** On exit: tear down tap, kill virtiofsd children, free vm_index lease.

Every Bxx/W*/R* bug in the project changelog has been caused by one of these five steps failing in a way that bash couldn't express or detect. Examples:

| Bug | Step | Bash failure mode |
|---|---|---|
| B12 | (1) | `xxd` not installed on worker AMI; hex MAC generation produced empty string; CH refused to start. |
| B13 | (1) | `printf '%b'` on busybox doesn't honour `\x` escapes; MAC contained literal `\xAB`. |
| B17 | (3) | `ch-remote resume` raced CH's vCPU thread spin-up; resume returned 200 while vCPUs were still parked. |
| B19 | (3→4) | Wrapper printed `RUNNING` to stdout before `ch-remote info` confirmed liveness; controller registered a dead VM. |
| B22 | (3) | After restore, in-VM clock was 4h stale; the wrapper has no place to inject a clock-resync hook. |
| R8-DEPLOY1 | (1) | Env vars from Nomad's `template` block weren't visible to subshells; `ZSBX_RESTORE_FROM` arrived as empty. |
| W1 | (1) | `sed -e "s/.../.../g" <<< "$ZSBX_TASK_ID"` was a code-exec sink if `$ZSBX_TASK_ID` contained `/`. |
| R6-C1 | (4) | `trap` handlers ran in a subshell, so the parent shell's `wait $CH_PID` reaped a different PID and reported the wrong exit code. |

Bash is the wrong tool for any of this. A driver written in Rust gives us typed args, structured errors, real process supervision via `compio`, and gRPC-level back-pressure with Nomad.

## 3. The go-plugin / Nomad-driver wire surface (research summary)

A Nomad task driver is, mechanically, a HashiCorp go-plugin gRPC server. The contract has three layers:

### 3.1 The go-plugin handshake (`hashicorp/go-plugin`)

The Nomad client `exec()`s the plugin binary and reads **one line from stdout**. The line format ([source](https://github.com/hashicorp/go-plugin/blob/main/server.go) line 413):

```
CORE-PROTO-VERSION|APP-PROTO-VERSION|NETWORK|ADDR|PROTOCOL|SERVER-CERT
```

For Nomad drivers, concretely:

- `CORE-PROTO-VERSION` = `1` (fixed, hard requirement).
- `APP-PROTO-VERSION` = `2` (Nomad gRPC drivers; `1` is the deprecated netRPC path we will *not* implement).
- `NETWORK` = `unix`.
- `ADDR` = a path to a Unix socket the plugin has already bound.
- `PROTOCOL` = `grpc`.
- `SERVER-CERT` = base64(DER) of a one-shot self-signed cert if AutoMTLS is enabled by the host; absent otherwise.

Before printing the line, the plugin must:

1. Verify `NOMAD_PLUGIN_MAGIC_COOKIE` env var equals `e4327c2e01eabfd75a8a67adb114fb34a757d57eee7728d857a8cec6e91a7255` (the literal value baked into [`plugins/base/plugin.go`](https://github.com/hashicorp/nomad/blob/main/plugins/base/plugin.go) — Nomad asserts this on the client side and silently refuses to load the plugin if it's wrong).
2. Bind a Unix listener at a path of the plugin's choosing (we'll use `${NOMAD_PLUGIN_SOCKET_DIR:-/tmp}/zsbx-ch-XXXXXX.sock`).
3. If AutoMTLS is requested (Nomad sets `PLUGIN_CLIENT_CERT` env), accept the client cert and present our own.
4. Print the line, **then `os.Stdout.Sync()`**, then keep stdout/stderr available for logs (Nomad streams them).

Process lifetime: the plugin runs as long as Nomad's plugin client connection is alive. Nomad supervises restart; on SIGTERM the plugin is expected to flush state and exit ≤ 30s.

### 3.2 The Nomad base-plugin RPCs

Every plugin (driver, device, autoscaler) must implement `BasePlugin` ([base.proto](https://github.com/hashicorp/nomad/blob/main/plugins/base/proto/base.proto)):

- `PluginInfo() → (type=DRIVER, plugin_api_versions=["0.1.0"], plugin_version, name)`
- `ConfigSchema() → hclspec.Spec` (driver-level config)
- `SetConfig(MsgpackConfig, NomadConfig)` (called once at load)

### 3.3 The driver RPCs

From [driver.proto](https://github.com/hashicorp/nomad/blob/main/plugins/drivers/proto/driver.proto). Required, in order of when Nomad calls them:

| RPC | Streaming? | Maps to current wrapper step | Maps to controller flow |
|---|---|---|---|
| `TaskConfigSchema` | unary | (HCL → struct) | new — declares `vm_index`, `kernel`, `cmdline`, `disks[]`, `fs[]`, `net[]`, `restore_from?` |
| `Capabilities` | unary | n/a | `SendSignals=true`, `Exec=false`, `FSIsolation=IMAGE`, `NetIsolation=NONE` |
| `Fingerprint` | server-stream | `init.sh` health checks | reports CH version, kvm presence, virtiofsd presence, vsock support |
| `RecoverTask(task_id, TaskHandle)` | unary | **does not exist today** | reattach to CH via existing `ch-remote --api-socket` |
| `StartTask(TaskConfig) → (Result, TaskHandle, DriverNetwork)` | unary | wrapper steps 1-3 cold-boot branch | unchanged controller-side |
| `WaitTask(task_id) → ExitResult` | unary (returns channel) | wrapper step 4 (`wait $CH_PID`) | controller polls task status today; gets a notify-channel instead |
| `StopTask(task_id, timeout, signal)` | unary | wrapper step 4 (graceful shutdown) | unchanged |
| `DestroyTask(task_id, force)` | unary | wrapper step 5 (cleanup trap) | unchanged |
| `InspectTask(task_id) → TaskStatus` | unary | wrapper logs (ad-hoc) | new — driver returns CH state + last-known live time |
| `TaskStats(task_id, interval) → stream<TaskStats>` | server-stream | **does not exist today** | new — driver polls `ch-remote info` and forwards |
| `TaskEvents()` | server-stream | n/a | new — emits structured events (e.g. `vm.restored`, `ch.oom`) |
| `SignalTask(task_id, signal)` | unary | n/a | rare; dev-only |
| `ExecTask` / `ExecTaskStreaming` | n/a | **not implemented** | user code runs in-VM, not in-task; capability says `Exec=false` |

`TaskHandle` is opaque-to-Nomad bytes the driver authors. Convention: serialize a small Rust struct (CH PID, API socket path, vm_index, started_at, restore_marker) with `serde_json` or `prost`. Nomad persists this to disk between client restarts; on restart it calls `RecoverTask(handle_bytes)` and we use it to reattach.

### 3.4 Capability claims and how they affect Nomad scheduling

Returning `FSIsolation=IMAGE` lets us run jobs that specify `image=` without a chroot. `NetIsolation=NONE` keeps Nomad out of our network setup (we own the tap). `SendSignals=true` lets `nomad alloc signal` work for dev. Capabilities are stable across the driver's lifetime; Nomad caches the response.

### 3.5 Prior art

- **`hashicorp/nomad-driver-virt`** — libvirt-based; Go; the closest official prior art for a hypervisor driver.
- **`cneira/firecracker-task-driver`** — Go; ~5k LOC; uses CNI for networking; doesn't implement `TaskStats`. Last touched 2024-pinned to Firecracker 0.25.2.
- **`volantvm/nomad-driver-ch`** — Go; the *only* Cloud-Hypervisor-specific driver in the wild. Implements virtiofs, tap, cloud-init. **No snapshot/restore.** MPL-2.0. ~3-4k Go LOC by structure of the repo (`chnet/`, `cloudhypervisor/`, `cloudinit/`, `virt/`).
- **`hashicorp/nomad-skeleton-driver-plugin`** — the official Go skeleton; gives us the canonical shape of `main.go` (calls `plugins.Serve(factory)`) and the `PluginInfoResponse` literal.
- **No Rust Nomad drivers exist in the wild as of search.** The `grr-plugin` Rust crate ([Medium write-up](https://medium.com/@archisgore/write-go-plugins-in-rust-5e7afcabde6d)) implements the go-plugin handshake in Rust generically but is unmaintained and predates the AutoMTLS protocol. We will not depend on it.

This means we are writing the *first* production Rust Nomad driver. Cost: more upfront work on the handshake. Benefit: we keep the entire stack in our existing language and toolchain.

## 4. Restore semantics: what the driver needs to know

The snapshot/restore work described in `docs/proposals/sandbox-snapshot-restore.md` lives in `crates/sandbox/src/restore_handler.rs::do_restore_inner`. The wrapper today receives `ZSBX_RESTORE_FROM` as an env var; if set, it switches from cold-boot to `cloud-hypervisor --restore ... && ch-remote resume`.

In the driver world this becomes a typed `TaskConfig` field:

```rust
#[derive(serde::Deserialize)]
struct TaskConfig {
    vm_index: u16,
    kernel: PathBuf,
    cmdline: String,
    cpus: u8,
    memory_mb: u32,
    disks: Vec<DiskSpec>,
    fs: Vec<VirtioFsSpec>,
    net: Vec<NetSpec>,
    /// When Some, CH is spawned with --restore from this path and
    /// resumed post-spawn. When None, cold-boot.
    restore_from: Option<PathBuf>,
    /// True iff this task is a snapshot-restored sandbox.
    /// Drives the post-Running clock-resync handshake (R7-S1).
    is_restore: bool,
}
```

The driver's responsibility ends at "CH is running and `ch-remote info` returns 200". The controller-side orchestration — `register_restored` (B19), `clock_resync` (R7-S1), the sealed-record unwrap (`persist.rs`) — happens *after* the driver reports the task as `Running`, exactly as today. The driver doesn't know about snapshots; it knows about CH start modes.

This is a deliberate design choice. **The driver is a transport for CH lifecycle; it is not aware of sandbox lifecycle.** The state machine in `registry.rs` is owned by the controller. Any "is this a snapshot restore?" branching the controller wants stays controller-side. The driver only needs `restore_from: Option<PathBuf>` to know whether to pass `--restore` and follow with `ch-remote resume`.

## 5. Architecture

### 5.1 New crate

```
crates/nomad-driver-ch/
├── Cargo.toml
├── README.md
├── build.rs                   # invokes prost-build on vendored .proto
├── proto/
│   ├── base.proto             # vendored from hashicorp/nomad@<pinned>
│   ├── driver.proto           # vendored from hashicorp/nomad@<pinned>
│   └── hclspec.proto          # vendored (TaskConfigSchema returns this)
├── src/
│   ├── main.rs                # handshake + tonic Server::bind, ≤ 100 LOC
│   ├── handshake.rs           # cookie check, listener bind, stdout line
│   ├── plugin/
│   │   ├── mod.rs
│   │   ├── base.rs            # impl BasePlugin (PluginInfo/ConfigSchema/SetConfig)
│   │   └── driver.rs          # impl Driver (all RPCs from § 3.3)
│   ├── ch/
│   │   ├── mod.rs
│   │   ├── spawn.rs           # cold-boot + restore spawn paths
│   │   ├── api.rs             # ch-remote / API-socket client (compio)
│   │   ├── stats.rs           # TaskStats poller
│   │   └── shutdown.rs        # graceful → forceful
│   ├── net/
│   │   ├── tap.rs             # ip tuntap add/del, ip link set up
│   │   └── mac.rs             # deterministic MAC from vm_index (replaces xxd/printf)
│   ├── fs/
│   │   └── virtiofsd.rs       # spawn × 3, wait-for-socket, kill
│   ├── handle.rs              # TaskHandle (serde): pid, sock_path, vm_index, mode
│   └── recover.rs             # RecoverTask: reattach via API socket + handle bytes
└── tests/
    ├── handshake_smoke.rs     # spawn binary, parse handshake line
    ├── driver_grpc.rs         # invoke Start/Wait/Stop via tonic client
    └── ch_lifecycle.rs        # full cold-boot + stop, requires kvm
```

### 5.2 How it links to `zeroship-sandbox`

We deliberately do **not** make `nomad-driver-ch` depend on the `zeroship-sandbox` crate. Reasoning:

- Driver runs as a separate process supervised by Nomad. It has no access to the controller's pg pool, redis, GCS clients, or sealed-record codec. Pulling in `sandbox` would force linking all of those into the driver binary.
- The driver is a transport-layer concern; the sandbox crate is a control-plane concern. Mixing them couples wire format (which Nomad pins) to internal types (which we rev freely).

Instead, we extract a small **shared types crate**:

```
crates/sandbox-ch-types/        # new
├── Cargo.toml                  # no deps beyond serde, prost
└── src/lib.rs                  # MacAddr, VmIndex, DiskSpec, VirtioFsSpec, NetSpec
```

Both `crates/sandbox` (the controller's `nomad_ch.rs::build_jobspec`) and `crates/nomad-driver-ch` (the driver's `TaskConfig` deserializer) depend on `sandbox-ch-types`. This guarantees that when the controller writes a jobspec, the driver can deserialize it byte-for-byte.

### 5.3 Wire-format coupling

The contract surface is:

1. **Controller → Nomad job's `Config{}` block:** typed via `sandbox-ch-types::TaskConfig`, serialized to HCL.
2. **Nomad client → driver `StartTask`:** typed via `sandbox-ch-types::TaskConfig`, deserialized from msgpack.
3. **Driver → controller:** indirect, via Nomad alloc status (which the controller polls today). The driver does not directly contact the controller.

This is the same shape we have today; the difference is `TaskConfig` is now a Rust struct instead of `env -i ZSBX_FOO=$bar ZSBX_BAR=$baz ./nomad-vm-wrapper.sh`.

## 6. Wire protocol decision: B1 vs B2 vs B3

We considered three implementation paths.

### B1 — Pure-Rust driver speaking go-plugin gRPC

**Approach:** vendor `base.proto`, `driver.proto`, `hclspec.proto` from `hashicorp/nomad@<pinned>`. Use `tonic` + `prost` for codegen. Implement the handshake by hand (~50 LOC: env-var check, UDS bind, stdout one-liner). Serve with `tonic::transport::Server::serve_with_incoming` over a `UnixListenerStream`.

**Risks identified:**
- AutoMTLS handshake details: tonic supports rustls, and the Nomad host sends its cert via `PLUGIN_CLIENT_CERT`. Need to confirm the cert wire format (PEM in env var vs DER). Worst case we ship without AutoMTLS in v1 and add it in v2 — Nomad supports both modes.
- The `Fingerprint` and `TaskStats` streams are server-streaming. tonic handles this natively; no concern.
- proto pin drift: Nomad bumps proto periodically. We pin to a specific commit and document the upgrade procedure in § 8.

**Verification:** `tonic` can serve gRPC over UDS ([tonic UDS example](https://github.com/hyperium/tonic/tree/master/examples/src/uds) — explicit pattern: `UnixListener::bind(path)?` → `UnixListenerStream::new(listener)` → `Server::builder().add_service(...).serve_with_incoming(stream)`). go-plugin's wire is plain gRPC over HTTP/2 over UDS; there is nothing Go-specific about it. Confirmed by the existence of `grr-plugin` (Rust) and `@lukekaalim/hashicorp-go-plugin` (TS) — both speak the same wire to live Go hosts.

**Cost estimate:** ~2.5k Rust LOC for the driver itself, ~500 LOC of generated prost code, ~100 LOC of handshake.

### B2 — Thin Go shim delegating to a Rust subprocess

**Approach:** a 150-200 LOC Go program that calls `plugins.Serve(factory)` against a `nomad/plugins/drivers` shim. The shim forwards each RPC over its own stdio pipe (or a child UDS) to a Rust subprocess that does the real work.

**Pros:** zero risk of handshake/protocol incompatibility — Go owns the handshake.
**Cons:** two binaries to ship and version-pin; two languages in the deploy artifact; extra serialization hop per RPC (msgpack → Go struct → bincode → Rust struct); doesn't really eliminate Go from the deployment surface, only from the implementation surface.

### B3 — Cgo: link Rust as a static library into a Go binary

**Approach:** compile a Rust `staticlib` (`#[no_mangle] extern "C"` exports for each driver method); link into Go via `cgo`.

**Pros:** single binary; Go owns handshake; Rust owns logic.
**Cons:** cgo is famously painful (cross-compilation, build-time C toolchain on every developer machine, runtime cost of every FFI hop, harder debugging across the language boundary, no async story). The error surface of cgo + tokio + goroutines is a nightmare; we'd rather not.

### Recommendation: **B1**

B1 is the right call because:

1. `tonic` over UDS is a solved problem; the go-plugin handshake is 50 LOC of stdout printing.
2. Two prior art projects (`grr-plugin`, `@lukekaalim/hashicorp-go-plugin`) demonstrate that non-Go go-plugin servers work in production against live Go hosts.
3. We avoid maintaining two build pipelines and a cross-language ABI.
4. The only B1-specific risk is AutoMTLS, and we can ship v1 without it (Nomad supports plain UDS plugins) while we work out the TLS plumbing for v2.
5. Time cost is not the binding constraint per the user; the cleaner architecture wins.

**Fallback plan:** if AutoMTLS proves unworkable in tonic *and* the host config can't disable it (e.g. some compliance regime forces it on), we drop to B2. We do not consider B3.

## 7. Lifecycle hooks: detailed mapping

### `Fingerprint` (server-streaming)

Emits one `FingerprintResponse` immediately and then one every 30s (Nomad's default cadence). Fields we report:

```rust
HealthState::Healthy
attributes = {
    "driver.ch.version":   "51.1",
    "driver.ch.kvm":       "true",
    "driver.ch.virtiofs":  "true",
    "driver.ch.vsock":     "true",
    "driver.ch.snapshot":  "true",   // gates the snapshot-restore proposal
    "driver.ch.max_vms":   "32",     // matches our vm_index pool size
}
```

Implementation: probe `cloud-hypervisor --version`, `/dev/kvm` open-for-rw, `virtiofsd --version` once at SetConfig; cache. The 30s ticker just re-emits the cached struct unless we detect drift (e.g., CH binary upgraded under us — we detect via mtime).

### `StartTask`

Inputs: `TaskConfig` from § 4.

Flow:

1. Acquire vm_index lease (today: filesystem lock at `/var/lib/zsbx/vm-index/<n>.lock`; same scheme).
2. Set up tap (`ip tuntap add zsbx-nm-<n> mode tap` → `ip link set ... up`). Uses `rtnetlink` crate or shells out to `ip(8)` via `compio::process` — either works; `rtnetlink` is cleaner.
3. Compute deterministic MAC: `12:34:56:78:9b:<vm_index>`. **In Rust this is `format!("12:34:56:78:9b:{:02x}", vm_index)` — three lines, no `xxd`, no `printf %b`, no shell escapes.** This single change retires B12, B13, and W1.
4. Spawn virtiofsd × 3 (`keys`, `userhome`, `assets`) with known socket paths. Wait for each socket to be `accept()`-able before continuing.
5. Either:
   - **Cold boot:** spawn `cloud-hypervisor --kernel ... --cmdline ... --disk ... --fs ... --net ...` with API socket at `/var/lib/zsbx/run/ch-<vm_index>.sock`.
   - **Restore:** spawn `cloud-hypervisor --restore source_url=file://${restore_from} --api-socket /var/lib/zsbx/run/ch-<vm_index>.sock`, then `ch-remote --api-socket ... resume`.
6. Poll `ch-remote info` until it returns 200 (`vm_state: Running` or `Resumed`). **Wait for vCPUs to be actually parked-out of init — addresses B17 by checking `cpu_state` field explicitly, not just HTTP 200.**
7. Build `TaskHandle` (serde): `{ pid, api_socket, vm_index, mode: ColdBoot|Restored, started_at }`.
8. Return `StartTaskResponse { result: SUCCESS, handle, driver_network }`.

Each step's failure mode is a typed Rust error with a structured cause. On any failure: roll back in reverse order (kill CH, kill virtiofsd, tear tap, release lease) and return `FATAL` or `RETRY`. `RETRY` is for transient errors (e.g., tap name collision); `FATAL` is for non-retryable (e.g., CH binary missing).

### `WaitTask`

Returns a future that resolves with `ExitResult { exit_code, signal, oom_killed, exited_at }`. Implementation: `compio` adopts the CH child PID; the wait future resolves on reap. We additionally set `oom_killed=true` if `ch-remote info` reported any memory pressure events in the last 5s before exit, or if `/sys/fs/cgroup/.../memory.events` shows `oom_kill > 0`.

### `StopTask(timeout, signal)`

If signal is `SIGTERM` (default): `ch-remote shutdown` → wait `timeout` → if still alive, `kill -9 $pid`. If signal is anything else: forward via `kill -<signal>` (used for dev-only). Does **not** clean up: `DestroyTask` does that.

This is exactly what the wrapper does in step 4, expressed in 30 LOC of Rust instead of 90 LOC of bash with a `trap` and a `wait` and an `if [[ -d /proc/$pid ]]`.

### `DestroyTask(force)`

1. Kill CH if still alive (force ⇒ `SIGKILL`, else `SIGTERM` with 5s grace).
2. Kill virtiofsd × 3.
3. `ip tuntap del zsbx-nm-<n>`.
4. Release vm_index lease.
5. Remove the API socket file and any leftover ch-remote state.

Idempotent. Safe to call after a `RecoverTask` that found the task already dead.

### `RecoverTask(task_id, handle_bytes)`

**This is the headline feature.** Today, a Nomad client restart with running CH processes leaves them as orphans: PPID becomes 1, the controller's Nomad-task-id pointer is stale, and we have no clean way to reattach. Fixed by:

1. Deserialize `handle_bytes` into our `TaskHandle` struct.
2. Check `/proc/<pid>/comm` — is it still `cloud-hypervisor`?
3. If yes: open the API socket at `handle.api_socket`. Issue `ch-remote info`. If it returns 200, the CH is still alive and we re-adopt it: spawn a watcher future on the PID, register internal handle in our `HashMap<task_id, TaskHandle>`, return success.
4. If the process is gone or the socket is unresponsive: return error; Nomad marks the task as lost and the controller's reconciler will recreate.

We deliberately do not try to recover virtiofsd children — they're tied to the CH process and will exit when CH exits. If somehow CH is alive but virtiofsd children are gone (shouldn't happen), CH will I/O-error on its next mount-target operation and we'll catch it via the WaitTask path.

### `InspectTask`

Returns `TaskStatus { id, name, state: TaskState::Running|Exited, started_at, completed_at, exit_result, driver_status: {ch_pid, api_socket, vm_state} }`. Cheap: reads from our internal `HashMap`, no syscalls.

### `TaskStats` (server-streaming)

Every `interval` (Nomad default 5s):

1. `ch-remote --api-socket ... info` returns JSON with `memory.actual_size`, `cpu.utilization`, `vm_state`.
2. Map into `TaskResourceUsage { memory: {rss, cache, swap, usage}, cpu: {user, system, percent} }`.
3. Stream one `TaskStatsResponse` per tick.

If `ch-remote info` fails 3 times in a row, we kill the stream and the task transitions to lost. This is the first time we'll have real per-VM telemetry; expect to wire it into Prometheus in a follow-up.

### `SignalTask`

Forward `signal` to the CH PID via `kill(2)`. Used by `nomad alloc signal` for dev. Rare.

### `ExecTask` / `ExecTaskStreaming`

**Not implemented.** Capabilities returns `Exec: false`. User code runs inside the VM (over our existing sandbox-agent RPC channel), not in the Nomad task. If an operator needs a shell in the VM, they use the existing `zsbx debug shell` path.

## 8. Migration strategy

We ship behind a controller-side env var, identical in shape to the `SANDBOX_BACKEND` switch we already use for `docker` vs `nomad_ch`:

```
SANDBOX_TASK_DRIVER=raw_exec     # default; bash wrapper path
SANDBOX_TASK_DRIVER=ch_plugin    # new driver
```

`crates/sandbox/src/backend/nomad_ch.rs::submit_nomad_job` switches on this:

```rust
let driver_name = match config.task_driver {
    TaskDriver::RawExec  => "raw_exec",
    TaskDriver::ChPlugin => "ch_plugin",
};
let driver_config = match config.task_driver {
    TaskDriver::RawExec  => raw_exec_args_with_wrapper(&task_cfg),
    TaskDriver::ChPlugin => msgpack_encode(&task_cfg),
};
```

Both code paths stay alive during the transition. The bash wrapper does not move; we add the driver alongside it.

### Cutover sequence (per cluster)

1. **Build + publish driver binary.** CI emits a `nomad-driver-ch-${git-sha}` linux-amd64 static binary; publish to `gs://zsbx-prod-artifacts/drivers/`.
2. **Worker AMI bump.** Bake the driver binary into `/opt/nomad/plugins/nomad-driver-ch` on the worker image. Alternatively (faster iteration): `gcp-worker-startup.sh` `gsutil cp` the binary on first boot, same path we already use for `cloud-hypervisor` and `virtiofsd`.
3. **Nomad client config update.**

   ```hcl
   plugin "ch_plugin" {
     config {
       cloud_hypervisor_bin = "/usr/local/bin/cloud-hypervisor"
       virtiofsd_bin        = "/usr/local/bin/virtiofsd"
       vm_index_lockdir     = "/var/lib/zsbx/vm-index"
       run_dir              = "/var/lib/zsbx/run"
     }
   }
   ```

   plus `plugin_dir = "/opt/nomad/plugins"` in the client block. Then `systemctl restart nomad`.

4. **Soak on 1 worker.** Set `SANDBOX_TASK_DRIVER=ch_plugin` on a single controller node only (controller env, not jobspec). Run cluster smoke: `tests/smoke_cluster.sh 1+1`, `c=4`, `c=20`. Bash wrapper still serves every other request.
5. **Full cluster rollout.** Flip the controller env globally. Watch error rate, p99, OOM-killer counts for 1 week.
6. **Remove `raw_exec` path.** Delete `crates/sandbox/scripts/nomad-vm-wrapper.sh`, the `RawExec` arm of `TaskDriver`, the `gcp-worker-startup.sh` copy of the wrapper. PR title: `sandbox: retire bash wrapper`.

Rollback at any step is a single env-var flip back to `raw_exec`. The wrapper remains executable on workers until step 6.

## 9. Build + deploy

### 9.1 Build

`cargo build -p nomad-driver-ch --release --target x86_64-unknown-linux-musl` produces a static binary (musl-targeted to dodge glibc version skew between build env and worker AMIs — same approach as our other worker binaries). The binary is ~12 MB stripped (estimate from comparable tonic-based servers; will measure once we have it).

`build.rs` runs `prost-build` against vendored proto. Proto pin: a sha-locked git submodule or a vendored copy under `crates/nomad-driver-ch/proto/`; pin to the same Nomad version we deploy with (today: 1.7.x).

### 9.2 Deploy

`gsutil cp gs://zsbx-prod-artifacts/drivers/nomad-driver-ch-${sha} /opt/nomad/plugins/nomad-driver-ch && chmod +x` from `gcp-worker-startup.sh`. Same pattern as today's `cloud-hypervisor` and `virtiofsd` provisioning, so ops mental model is unchanged.

### 9.3 Versioning

`PluginInfoResponse.plugin_version` is set from `env!("CARGO_PKG_VERSION")` at build time, prefixed with the git SHA. Nomad logs this on plugin load; we can correlate any driver-level incident with an exact build. Bumping is a `cargo set-version` + tag.

Nomad's plugin API version (`PluginApiVersions: ["0.1.0"]`) is pinned to the Nomad we run. If we upgrade Nomad and it bumps to `0.2.0`, we re-vendor the protos and emit both versions during the transition.

## 10. Testing

### 10.1 Unit tests (in-crate)

- `handle.rs` round-trip serde (we serialize TaskHandle into Nomad's persistent store; back-compat matters).
- `mac.rs` deterministic MAC from vm_index, byte-exact against fixtures.
- `ch/stats.rs` parses `ch-remote info` JSON fixtures from CH v51.1 (and v52 when it lands).

### 10.2 Driver gRPC contract tests (in-crate, no Nomad)

`tests/driver_grpc.rs` boots the driver binary as a subprocess, parses the handshake line, connects with a tonic client, exercises every RPC. This catches handshake/protocol regressions without needing a real Nomad client.

### 10.3 In-Nomad integration tests

Nomad's [`nomad/plugins/drivers/testutils`](https://github.com/hashicorp/nomad/tree/main/plugins/drivers/testutils) package is Go-only and assumes a Go driver. We can't use it directly. Alternative: spin up a single-node Nomad client in CI, register our plugin, submit a no-op job (CH with a `/bin/sleep 5` kernel cmdline equivalent), assert it reaches Running and exits with 0.

### 10.4 Cluster smoke parity test

`tests/smoke_parity.sh`: run the same set of sandbox lifecycle operations (create, idle, snapshot, restore, stop) through `SANDBOX_TASK_DRIVER=raw_exec` and then `SANDBOX_TASK_DRIVER=ch_plugin`. Diff the resulting controller logs (modulo timestamps and PIDs). Any structural difference is a bug. Run at every cutover step.

### 10.5 What we can't easily test

- `RecoverTask` after a real Nomad client crash. We can simulate by sending SIGKILL to the Nomad client, but Nomad's recovery flow involves disk-state replay we can't easily black-box test. We rely on cluster-soak observation.
- AutoMTLS handshake interop. Until we have it working in B1 we can't unit test it. Mitigation: ship v1 without AutoMTLS, then enable it in v2 with a dedicated soak.

## 11. Risks + mitigations

### R1 — go-plugin handshake details we missed

The handshake spec is reasonably well-documented but the AutoMTLS path is sparsely specified outside the Go code. A subtle mismatch (e.g., we present a cert when host didn't ask, or wrong base64 variant, or wrong cert SAN) will cause the host to drop the connection with a generic message.

**Mitigation:** prototype the handshake first, against a real Nomad client, before writing the driver methods. If it works for `PluginInfo()`, the rest follows. Worst case we drop AutoMTLS for v1 (Nomad supports unencrypted UDS plugins; the socket lives in a 0700 dir).

### R2 — tonic / async runtime mismatch with Nomad's reconcile loop

Nomad's plugin client expects request-scoped lifetimes. Our `compio` runtime is fundamentally different from Go's goroutines. If we deadlock on a streaming RPC (Fingerprint, TaskStats, TaskEvents) — e.g., never close the stream — Nomad will hang the corresponding goroutine and eventually time out the plugin.

**Mitigation:** every streaming RPC has an explicit close path on driver shutdown (we listen for SIGTERM and drop our `Server` future, which closes all server streams). Test by sending SIGTERM mid-stream and asserting Nomad's client logs `plugin exited` within 5s.

### R3 — Operational debug surface

When the wrapper misbehaves today, ops can `journalctl -u nomad` and see the bash stderr inline. With a separate driver binary, errors land in Nomad's plugin-stderr stream (which Nomad does forward to journald, but with extra wrapping) and in our own driver logs (which we'll write to journald via `tracing` + `tracing-journald`).

**Mitigation:**
- `tracing_subscriber` with JSON output to stderr + journald.
- Structured fields: every log line includes `task_id`, `vm_index`, `ch_pid` when available.
- A `zsbx ops driver-tail <task_id>` command that scrapes the driver logs for a given task.

### R4 — Proto version drift on Nomad upgrade

When Nomad ships a proto change (it has happened: e.g., adding `DriverNetwork.HostsConfig` in 1.5), our pinned copy becomes stale. If the change is additive (new field with default-zero behaviour), it's a no-op for us. If it's a new RPC, Nomad's client may call it and get `Unimplemented`.

**Mitigation:** subscribe Nomad changelog; re-vendor protos and rerun `cargo build` whenever we bump the deployed Nomad version. The driver's `PluginApiVersions` field signals which version we speak; Nomad refuses to load if it doesn't match.

### R5 — Performance regression vs the wrapper

The wrapper's hot path (cold boot) is ~4.2s end-to-end (cf. § 2 of sibling proposal). Most of that is CH, not bash. The driver replaces ~50ms of bash startup with ~5ms of "Nomad calls StartTask on an already-running driver". So we expect a small *win*, not a regression. But until we measure on a real worker, we don't know.

**Mitigation:** smoke-parity test (§ 10.4) measures end-to-end create wall on both paths; we hold the cutover until ch_plugin is within ±50ms of raw_exec.

## 12. Out of scope

- Replacing Nomad.
- Replacing Cloud Hypervisor.
- Replacing the controller.
- The snapshot/restore lifecycle itself (see sibling proposal).
- CNI integration. Today we own tap directly; this stays the case.
- Pre-empting Nomad's own scheduling decisions (CPU, memory). The driver reports usage; Nomad's allocator does what it does.

## 13. Effort estimate

Time cost is explicitly not a constraint per the user; this section is for shape and risk weighting, not gating.

A reasonable breakdown of effort, expressed in focused work (not calendar weeks):

| Block | Description | Estimate |
|---|---|---|
| Handshake prototype | env-var check, UDS bind, stdout line, hit Nomad client with `PluginInfo` and see it loaded | small |
| Proto codegen | vendor + prost-build + manual sanity-check on generated types | small |
| `BasePlugin` impl | 3 unary RPCs | small |
| `Driver` impl: lifecycle | Start/Stop/Wait/Destroy/Recover, no streaming | medium |
| `Driver` impl: streaming | Fingerprint, TaskStats, TaskEvents | medium |
| CH integration layer | spawn, ch-remote client, virtiofsd, tap, vm_index lease, deterministic MAC | medium (most code lifted in spirit from the bash) |
| Tests | unit + gRPC contract + Nomad smoke | medium |
| Cluster validation | parity test on 1 worker, then full cluster, 1 week soak | dominant by wall-clock but not by effort |
| Wrapper retirement | delete bash, prune startup scripts | small |

**Total effort:** moderate. The handshake is the main unknown; once that's working, the rest is mechanical translation of the wrapper into Rust with proper types.

**Calendar:** if the handshake prototype works on day 1 (no AutoMTLS surprises), the soak window dominates and 2-3 weeks of calendar time is reasonable. If the handshake reveals an issue requiring B2 fallback, add another week. Either way the dominant *calendar* cost is the soak, which is the same for both paths.

## 14. Open questions for the user

These should be resolved before we start coding:

1. **AutoMTLS for v1?** I lean toward shipping plain UDS (0700 socket dir) for v1 and adding AutoMTLS in v2. Confirms we ship faster; the attack surface is "an attacker with shell on the worker as root", which we already concede. Acceptable?
2. **Proto pin policy.** Pin to the Nomad version we deploy (1.7.x at time of writing) and re-vendor on Nomad upgrade. Or pin to a known-stable proto and stay there until forced. Preference?
3. **Shared types crate name.** `sandbox-ch-types` is what I have above. Alternative: hoist these types into `crates/core/` since `core/` already holds inter-service wire types. The latter is cleaner if you consider Nomad jobspecs an inter-service wire format, which they are.
4. **Driver binary distribution channel.** Bake into worker AMI (slower iteration, atomic with worker version) vs. `gsutil cp` on boot (faster iteration, version drift possible). Today we do the latter for `cloud-hypervisor`; consistency suggests same for the driver. Confirm?
5. **CI for the driver test that needs `/dev/kvm`.** GHA runners don't have kvm. Options: (a) self-hosted runner on a GCP `n2-standard-2` (kvm yes), (b) cloud-build with nested-virt VM, (c) skip in CI and rely on cluster soak. (a) is cleanest.
6. **Naming.** `nomad-driver-ch` matches the existing volantvm repo, which could be confusing if anyone googles. Alternative: `zsbx-nomad-driver`. Preference?
7. **Concurrency model inside the driver.** We have `compio` everywhere else but `tonic` is tokio-only. We'll need a tokio runtime *in this binary specifically* — it's the only zeroship binary that doesn't run pure compio. Acceptable carve-out, or do we want to find a compio-tonic bridge first? (My recommendation: accept the tokio carve-out for this one binary; it doesn't touch the rest of the stack and dropping tonic in favor of a hand-rolled HTTP/2 server is a rabbit hole.)

---

## Appendix A: handshake reference values

For implementers, the literal values from upstream:

```
NOMAD_PLUGIN_MAGIC_COOKIE = "e4327c2e01eabfd75a8a67adb114fb34a757d57eee7728d857a8cec6e91a7255"
Nomad base plugin ProtocolVersion = 2
Core protocol version = 1
Network = "unix"
Protocol = "grpc"
PluginApiVersions = ["0.1.0"]   // current Nomad driver API
Plugin type = "driver"          // base.PluginTypeDriver in Go
```

Stdout handshake line our binary must emit, post-bind:

```
1|2|unix|/tmp/zsbx-ch-AbCdEf.sock|grpc
```

(or with an optional 6th field for AutoMTLS server cert if/when we enable it).

## Appendix B: sources

- [HashiCorp go-plugin internals](https://github.com/hashicorp/go-plugin/blob/main/docs/internals.md)
- [go-plugin Serve() handshake line emit](https://github.com/hashicorp/go-plugin/blob/main/server.go) (lines 413-433)
- [Nomad plugins/base/plugin.go — Handshake constants](https://github.com/hashicorp/nomad/blob/main/plugins/base/plugin.go)
- [Nomad plugins/drivers/proto/driver.proto](https://github.com/hashicorp/nomad/blob/main/plugins/drivers/proto/driver.proto)
- [Nomad plugins/base/proto/base.proto](https://github.com/hashicorp/nomad/blob/main/plugins/base/proto/base.proto)
- [Nomad task-driver authoring guide](https://developer.hashicorp.com/nomad/plugins/author/task-driver)
- [Nomad agent plugin block reference](https://developer.hashicorp.com/nomad/docs/configuration/plugin)
- [hashicorp/nomad-skeleton-driver-plugin](https://github.com/hashicorp/nomad-skeleton-driver-plugin) (reference shape)
- [hashicorp/nomad-driver-virt](https://github.com/hashicorp/nomad-driver-virt) (closest hypervisor prior art, Go)
- [cneira/firecracker-task-driver](https://github.com/cneira/firecracker-task-driver) (firecracker prior art, Go)
- [volantvm/nomad-driver-ch](https://github.com/volantvm/nomad-driver-ch) (CH-specific prior art, Go, no snapshot/restore)
- [tonic UDS example](https://github.com/hyperium/tonic/tree/master/examples/src/uds)
- [grr-plugin: Rust go-plugin shim (unmaintained, reference only)](https://medium.com/@archisgore/write-go-plugins-in-rust-5e7afcabde6d)
