//! Nomad + Cloud Hypervisor backend.
//!
//! Submits a Nomad job (using the `ch` Go plugin driver) per sandbox.
//! The jobspec carries a typed `TaskConfig` block the driver decodes
//! directly; the driver spawns:
//!
//!   - 1 × `cloud-hypervisor` foreground process
//!
//! CH attaches three virtio-blk disks (rootfs + per-sandbox workspace
//! image + per-user home image) and injects the controller's signing
//! pubkey via the kernel cmdline (`zsbx_pubkey=<hex>`). The previous
//! revision used virtio-fs with three host-side `virtiofsd` daemons;
//! the pivot to virtio-blk eliminated the only userspace process
//! whose vhost-user state had to survive snapshot/restore (closes
//! bug #11). See `docs/proposals/sandbox-snapshot-restore.md` §
//! virtio-blk pivot.
//!
//! The CH VM boots a tiny Linux kernel (CONFIG_IP_PNP=y) + raw ext4
//! rootfs containing `/sbin/init` (a shell script that parses the
//! `zsbx_pubkey=` cmdline arg to /run/keys/controller-pubkey, formats
//! + mounts /dev/vdb at /workspace and /dev/vdc at /home/u, then
//! execs `zeroship-sandbox-agent`). The agent's wire-protocol-v1
//! auth (Ed25519 signed requests, 5 s skew + 30 s nonce LRU) is
//! **identical** to the K8s backend; only the runtime plumbing
//! differs.
//!
//! ## Why this exists
//!
//! Kubernetes is heavyweight to operate on bare metal �� kubelet,
//! coredns, CNI, RuntimeClass + crun + libkrun, PVC + StorageClass.
//! For single-node / small-cluster operators who already run Nomad,
//! the `ch` Go plugin driver is the production transport: no cluster
//! networking abstraction, no CSI, just the driver that ties
//! one VM's lifetime to one Nomad alloc.
//!
//! ## Per-sandbox host layout
//!
//! ```text
//! /var/zeroship/ch/                                 # host_state_dir
//!   <sandbox-id>/
//!     workspace.img                                 # virtio-blk → /dev/vdb → /workspace
//!                                                   # (per-sandbox, raw ext4, sparse)
//!   users/<user_id>/
//!     home.img                                      # virtio-blk → /dev/vdc → /home/u
//!                                                   # (per-user, raw ext4, persists
//!                                                   #  across this user's sandboxes)
//! ```
//!
//! The controller's signing pubkey is no longer a file on the host —
//! it's hex-encoded into `ZSBX_PUBKEY_HEX` and the wrapper injects
//! it into the guest's kernel cmdline as `zsbx_pubkey=<hex>`. The
//! guest's /sbin/init decodes it back into 32 raw bytes at
//! `/run/keys/controller-pubkey`, which is the path the agent's
//! `auth.rs::DEFAULT_PUBKEY_PATH` already points at.
//!
//! ## Network
//!
//! Each sandbox gets a `vm_index` from a free-list allocator
//! (`VmIndexAllocator`). Index → `/30` subnet:
//!
//! ```text
//!   tap device  : zsbx-nm-<idx>     (host operator pre-creates)
//!   host IP     : 10.99.<100+idx>.1
//!   VM IP       : 10.99.<100+idx>.2
//!   MAC         : 12:34:56:78:9b:<idx hex>
//! ```
//!
//! The controller computes only the **VM IP** (to reach the agent at
//! `http://10.99.<100+idx>.2:7777`); the wrapper script computes
//! everything else from `ZSBX_VM_INDEX`.
//!
//! ## Agent reachability
//!
//! The controller (running on the same host as the Nomad agent in
//! the demo, eventually on its own node with routing into the tap
//! subnets) talks **directly** to `10.99.<100+idx>.2:7777`. Unlike
//! the K8s backend's `kubectl port-forward` dev mode, we do not
//! tunnel — the assumption is the controller has L3 connectivity to
//! the tap subnets. A future single-binary `zeroship-sandbox-router`
//! sidecar would proxy this when the controller is off-host.
//!
//! ## Cleanup contract
//!
//! **T-8b-stress-r2 controller v34: host_dir cleanup is sweeper-owned,
//! not per-alloc.** Stress-r2 (`docs/reviews/sandbox-snapshot-restore-
//! cluster-2026-05-25-T8b-stress-r2.md`) showed that the per-alloc
//! `rm -rf host_dir` in CreateGuard::drop and stop_inner's step 5 was
//! racing with concurrent retry-`create` for the same sandbox_id: the
//! failing alloc's DestroyTask removed `workspace.img` while the retry's
//! StartTask was running, causing 48/60 CREATEs to fail with
//! "workspace.img does not exist (controller must stage before spawn)".
//! v34 moves host_dir GC out of the per-alloc hot path entirely.
//!
//! Lifecycle:
//!
//! - `create` is wrapped in a `CreateGuard` whose Drop spawns a
//!   detached compio task that tears down partial state. The task:
//!   (a) purges the Nomad job, (b) on confirmed-purge releases the
//!   vm_index back to the pool, **(c) intentionally LEAKS the host_dir
//!   for the sweeper to reap.** The vm_index is intentionally NOT
//!   released until the Nomad purge confirms — releasing it earlier
//!   risks a retry-`create` for the same user grabbing the same index
//!   and racing the still-alive prior wrapper for `tap=zsbx-nm-<idx>`.
//!   "release on confirmed purge; leak otherwise; orphan-prune mops
//!   up later."
//! - On controller crash or runtime-shutdown the cleanup task may
//!   not run; any leaked Nomad jobs persist until
//!   [`NomadCHBackend::cleanup_orphans_at_startup`] reclaims them at
//!   next boot. **That cleanup defaults OFF and is opt-in via
//!   `SANDBOX_NOMAD_CH_STARTUP_ORPHAN_CLEANUP=true`** — single-
//!   replica operators should turn it on. host_dir cleanup is sweeper-
//!   owned regardless of opt-in.
//! - `stop` is idempotent (returns Ok if the sandbox isn't in the
//!   in-memory map). The Nomad job is purged, vm_index returned to
//!   the pool **only on confirmed purge** (else leaked), and the
//!   sealed-auth record is `persist.delete`d. **The per-sandbox
//!   host_dir is LEAKED for sweeper cleanup** — see the v34 note
//!   above. The per-user home dir is **never** deleted by `stop` —
//!   it's user-scoped state owned by the user lifecycle, not the
//!   sandbox lifecycle.
//! - **host_dir GC (sweeper-owned, v34)**:
//!   [`crate::sweep::spawn_host_dir_gc`] runs every 5 minutes,
//!   enumerates `<host_state_dir>/<uuid>/` entries, looks each up in
//!   the `sandboxes` table, and `rm -rf`s the dir when the sandbox is
//!   in a terminal state (`stopped`/`lost`/`orphan`), no pending
//!   `wake_jobs` row exists, and the directory mtime is older than
//!   `GRACE_SECS` (default 1 hour). Operators retain on-disk artefacts
//!   during the grace window for inspection.
//!
//! ## Note on rootfs init.sh
//!
//! Post virtio-blk pivot, the rootfs ships an `init.sh` (committed
//! at `crates/sandbox/scripts/init.sh`) that:
//!
//!   1. Parses `zsbx_pubkey=<hex>` from /proc/cmdline, decodes hex,
//!      writes `/run/keys/controller-pubkey` (tmpfs-backed /run).
//!   2. Formats `/dev/vdb` + `/dev/vdc` if `blkid` reports no FS,
//!      then mounts them at `/workspace` and `/home/u` respectively.
//!   3. Execs `/usr/local/bin/sandbox-agent`.
//!
//! The rootfs must be re-baked (`bake-rootfs.sh`) before any cluster
//! can run this code — the in-tree init.sh ships only when the
//! operator rebakes + reuploads the rootfs image.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Default Nomad alloc data root. Phase B's snapshot wiring derives
/// `<NOMAD_ALLOC_ROOT>/<alloc-id>/ch/local/ch.sock` to reach
/// cloud-hypervisor's API socket on the same worker. This matches
/// Nomad's default `data_dir = /opt/nomad/data`; the path is wired
/// here as a const because (a) it's already implicit in the wrapper's
/// `ZSBX_RUNTIME=${NOMAD_TASK_DIR}` expansion that the controller
/// reads back, and (b) the wider Nomad agent config isn't surfaced
/// to the controller crate.
const NOMAD_ALLOC_ROOT: &str = "/opt/nomad/data/alloc";

use ed25519_dalek::SigningKey;
use uuid::Uuid;
use zeroship_sandbox_agent::sig;
use zeroship_sandbox_agent::AGENT_PORT;

use super::{ExecOutput, SandboxInfo, TreeEntry};
use crate::config::SandboxConfig;

#[derive(Debug)]
pub struct NomadCHBackend {
    cfg: SandboxConfig,
    state: Arc<RwLock<HashMap<Uuid, NomadChSandbox>>>,
    /// Per-VM-index pool, free-list backed. Renamed from
    /// `vm_indices` in round 3 (m1) — the new name reads as a
    /// component (an allocator) rather than a plural noun
    /// (a collection of indices).
    vm_index_allocator: Arc<Mutex<VmIndexAllocator>>,
    /// Per-user serialization gate (one in-flight `create` per user).
    creating_users: Arc<Mutex<HashSet<String>>>,
    healthy: Arc<AtomicBool>,
    /// M7 circuit-breaker: most recent probe error, captured by
    /// [`probe`]. Read by [`create`] to assemble the operator-
    /// readable "backend unhealthy" message. Mutex (not RwLock)
    /// because the only writer is the periodic probe and reads are
    /// rare; Mutex is simpler and the contention is irrelevant.
    last_probe_err: Arc<Mutex<Option<String>>>,
    /// Sealed-record persistence (preview-URL § II.0 §4). See
    /// [`crate::persist::Persistence`]. `None` when
    /// `SANDBOX_PERSIST_AUTH` is unset.
    persist: Option<Arc<crate::persist::Persistence>>,
    /// r3-A (T-8b-stress-r3 fix): cached local Nomad node ID for
    /// the `Constraints` block emitted on every cold-boot jobspec.
    /// `Some` when [`fetch_local_nomad_node_id`] returned Ok at
    /// boot; `None` (fallback to random cross-node placement) when
    /// the boot-time lookup failed. Installed via
    /// [`Self::with_local_nomad_node_id`] from
    /// `crate::AppState::from_config`; tests construct `None` by
    /// default and exercise the `Some` shape via the builder.
    local_nomad_node_id: Option<String>,
    /// r30-A1: global cap on in-flight `stop_inner` calls, shared with
    /// `AppState::nomad_stop_permits`. Set via
    /// [`Self::install_nomad_stop_permits`] from `AppState::from_config`
    /// after the semaphore is sized from `SANDBOX_NOMAD_STOP_CONCURRENCY`.
    /// `OnceLock` (not `Option`) so the install is observable from
    /// `&self` paths (`stop_inner`) without taking a write-lock on the
    /// whole backend; uninstalled → `stop_inner` runs without the cap
    /// (the unit-test default + the legacy single-tenant binary path).
    /// In production the cap is always installed because
    /// `AppState::from_config` is the only construction path that reaches
    /// the HTTP server.
    nomad_stop_permits: std::sync::OnceLock<Arc<NomadStopPermits>>,
}

/// Phase B / snapshot wiring: resolved source-VM identity for a
/// running sandbox. Returned by [`NomadCHBackend::lookup_source_vm_ops`]
/// and consumed by the snapshot admin handler before it issues
/// `ch-remote pause` + `ch-remote snapshot`.
///
/// All three fields are derived from the live Nomad alloc:
///   - `api_socket` — path to cloud-hypervisor's HTTP API UDS,
///     `<alloc_dir>/ch/local/ch.sock` (the wrapper's `${ZSBX_RUNTIME}/ch.sock`).
///   - `vm_index` — pulled from the in-memory backend record (NOT from
///     Nomad meta) so it stays consistent with the registry's view of
///     the IP/MAC/tap derivation.
///   - `alloc_dir` — Nomad's per-alloc data dir; passed through to the
///     restore handler's `ZSBX_RESTORE_FROM` env if the operator wakes
///     a same-worker snapshot. v1 doesn't restore from this dir
///     directly (the snapshot store re-stages into a fresh `restore/`
///     subdir), but capturing it now keeps the audit-log + future
///     in-place restore optimisation cheap.
#[derive(Debug, Clone)]
pub struct SourceVmOpsHandle {
    pub api_socket: PathBuf,
    pub vm_index: u16,
    pub alloc_dir: PathBuf,
}

/// Per-sandbox bookkeeping. Lives only in process memory; on
/// controller restart the in-memory map is rebuilt from scratch and
/// any orphan jobs are (optionally) cleaned up at startup — see
/// [`NomadCHBackend::cleanup_orphans_at_startup`].
struct NomadChSandbox {
    user_id: String,
    /// Nomad job ID — `zsbx-<sandbox-id-simple>`.
    job_id: String,
    /// Index allocated from `VmIndexAllocator`. Released on `stop`.
    vm_index: u16,
    /// Root host directory for this sandbox's virtio-fs shares.
    /// `<host_state_dir>/<sandbox-id>/` — `keys/` and `workspace/`
    /// subdirs.
    host_dir: PathBuf,
    /// Base URL the controller uses to reach the agent. Derived from
    /// `vm_index`: `http://10.99.<100+idx>.2:7777`.
    agent_url: String,
    /// Per-sandbox signing key. Generated at create-time, lives only
    /// in this process. The corresponding **public** key is the only
    /// thing that ships into the VM (mounted via virtio-fs `keys`
    /// share at `/run/keys/controller-pubkey`).
    ///
    /// **Wrapped in Arc** so signed-RPC dispatch can clone a refcount
    /// (cheap) instead of the 32-byte secret bytes (which would mean
    /// two heap copies of the secret coexisting during every signed
    /// request, since `ed25519_dalek::SigningKey` doesn't zeroize on
    /// drop).
    signing_key: Arc<SigningKey>,
}

// SECRET-HYGIENE: this Debug impl is **load-bearing**. The
// `signing_key: Arc<SigningKey>` field MUST NEVER appear in any
// debug output, even via the transitive chain
//   Backend::Debug → state.read() → HashMap → NomadChSandbox::fmt
// Adding `#[derive(Debug)]` to NomadChSandbox would dump the secret
// into any panic backtrace / error log / `dbg!()` call. The
// finish_non_exhaustive() below is what closes that hole — keep
// that final clause and do NOT switch to a derive even when adding
// a new field. ed25519-dalek::SigningKey doesn't have a redacting
// Debug impl of its own.
impl std::fmt::Debug for NomadChSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NomadChSandbox")
            .field("user_id", &self.user_id)
            .field("job_id", &self.job_id)
            .field("vm_index", &self.vm_index)
            .field("host_dir", &self.host_dir)
            .field("agent_url", &self.agent_url)
            // signing_key intentionally omitted — see above.
            .finish_non_exhaustive()
    }
}

// ─── VmIndexAllocator ────────────────────────────────────────────

/// Free-list-backed allocator for the per-sandbox VM index. Hands
/// out the smallest free index ≥ `floor`; reclaims released indices
/// so we don't run off the end of the (host-pre-provisioned) tap
/// pool. Bounded above by `ceil` (inclusive). Behaviour and rationale
/// mirror `PortAllocator` in `k8s.rs`.
///
/// **Visibility note (B18 fix)**: this type is `pub` (not `pub(crate)`)
/// because `RealRestoreBackend` in `crate::restore_handler` needs to
/// hold an `Arc<Mutex<VmIndexAllocator>>` shared with `NomadCHBackend`,
/// so create-side `alloc()` and restore-side `reserve()` go through the
/// same state. Without the share, the restore path's `reserve(i)`
/// silently passes against a private allocator while the create-side
/// allocator still sees `i` as free → the next CREATE hands the same
/// IP/tap to a fresh sandbox that collides with the live restored VM
/// (cluster smoke 2026-05-24 r4; 11/16 c=4 cycles hit a stale-pubkey
/// 401 once slots 1-6 had been used once).
#[derive(Debug)]
pub struct VmIndexAllocator {
    floor: u16,
    ceil: u16,
    /// Highest index we've ever handed out (well, `next` is "the
    /// next index to try if `freed` is empty"). New allocs prefer
    /// `freed`, fall back to `next`, fail when `next > ceil`.
    next: u16,
    /// Returned indices, sorted ascending — smallest is reused first
    /// so we keep allocation density high near `floor`.
    freed: BTreeSet<u16>,
}

impl VmIndexAllocator {
    pub fn new(floor: u16, ceil: u16) -> Self {
        Self {
            floor,
            ceil,
            next: floor,
            freed: BTreeSet::new(),
        }
    }

    pub fn alloc(&mut self) -> Result<u16, String> {
        if let Some(&i) = self.freed.iter().next() {
            self.freed.remove(&i);
            return Ok(i);
        }
        if self.next > self.ceil {
            return Err(format!(
                "vm-index allocator exhausted (floor={}, ceil={})",
                self.floor, self.ceil
            ));
        }
        let i = self.next;
        self.next = self.next.saturating_add(1);
        Ok(i)
    }

    pub fn release(&mut self, i: u16) {
        if i >= self.floor && i <= self.ceil {
            self.freed.insert(i);
        }
    }

    /// T-8b-stress-r8 r24-A2-S3 / r29-A2: inline-await variant of
    /// the delayed release. Sleeps `delay`, then releases slot `i`
    /// back into the allocator + emits the operator-facing log
    /// line. This is the safe default everywhere — the caller's
    /// runtime stays alive for the full sleep because the future is
    /// driven by `.await`, not detached.
    ///
    /// Use this from:
    ///
    /// - `stop_inner` (long-lived ntex-worker runtime — could also
    ///   use the detached variant, but inline keeps the call sites
    ///   uniform with the short-lived runtime paths)
    /// - `CreateGuard::drop`'s detached cleanup future (runs under
    ///   `detach_isolated`'s SHORT-LIVED private compio runtime —
    ///   detaching here would lose the timer, see r29-A2 below)
    /// - any future caller running under `detach_isolated` that
    ///   needs to release a slot after a delay
    ///
    /// r29-A2 history: this replaces the pre-r29 `spawn_delayed_release`
    /// fire-and-forget helper. That helper used
    /// `compio::runtime::spawn(...).detach()` against the CURRENT
    /// runtime — fine on the long-lived worker, but planted a
    /// timer task that got discarded by `Scheduler::clear()` when
    /// any short-lived runtime (`detach_isolated`'s private mint)
    /// dropped. R28-C1 found one such leak (CreateGuard::drop);
    /// R29-C1 found the second (`snap-teardown-<tail>` →
    /// `stop_preserving_state` → `stop_inner` → spawn_delayed_release).
    /// Both close by routing through this inline-await helper so
    /// the timer is bound to the caller's task, not detached onto a
    /// runtime that may not outlive the delay.
    ///
    /// If a caller genuinely cannot await (returning a `Task`
    /// JoinHandle is acceptable but `.detach()` is not), use
    /// [`Self::spawn_delayed_release_in_worker`] — which makes the
    /// runtime-lifetime decision explicit via the return type.
    ///
    /// Behaviour at `delay == 0`: skips the sleep and releases
    /// synchronously inside the calling task.
    pub async fn release_vm_index_after(
        allocator: Arc<Mutex<Self>>,
        i: u16,
        delay: Duration,
        reason: &'static str,
        sandbox_id: Uuid,
    ) {
        if !delay.is_zero() {
            compio::time::sleep(delay).await;
        }
        allocator
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .release(i);
        tracing::info!(
            vm_index = i,
            reason = %reason,
            sandbox_id = %sandbox_id,
            delay_ms = delay.as_millis() as u64,
            "sandbox/nomad-ch vm_index released (r24-A2-S3 delayed)"
        );
    }

    /// r29-A2: spawn the delayed release on the CURRENT compio
    /// runtime and return a `Task<()>` JoinHandle. The caller MUST
    /// either hold the task to completion OR call `.detach()` on it
    /// — but `.detach()` is only sound when the current runtime is
    /// guaranteed to outlive the delay. The pre-r29
    /// `spawn_delayed_release` helper hid this decision; returning
    /// `Task<()>` here forces the caller to confront it.
    ///
    /// Long-lived ntex / snap-idle-gc / sweep loop callers can
    /// safely call `.detach()` on the returned task. Short-lived
    /// runtimes (`detach_isolated`'s private mint) MUST use
    /// [`Self::release_vm_index_after`] instead — `.await`-ing it
    /// inline keeps the timer alive across the sleep.
    ///
    /// No current call site in this crate uses this helper; it
    /// exists as the type-safe escape hatch for any future
    /// background-task path that needs fire-and-forget delayed
    /// release on a long-lived runtime without blocking the caller.
    #[allow(dead_code)] // typed escape hatch — see rustdoc
    pub fn spawn_delayed_release_in_worker(
        allocator: Arc<Mutex<Self>>,
        i: u16,
        delay: Duration,
        reason: &'static str,
        sandbox_id: Uuid,
    ) -> compio::runtime::Task<
        Result<(), Box<dyn std::any::Any + Send>>,
    > {
        // `compio::runtime::spawn` wraps the spawned future's output
        // in `Result<T, Box<dyn Any + Send>>` (panic-catch). We
        // surface that wrapper in the return type rather than
        // hide it — if a caller `.await`s the Task and the release
        // panicked (it doesn't today; `release` is a BTreeSet
        // insert that can't panic), the Err is reachable.
        compio::runtime::spawn(Self::release_vm_index_after(
            allocator, i, delay, reason, sandbox_id,
        ))
    }

    /// Non-destructive read of the `freed` set so tests can pin
    /// "was slot N released?" without consuming the slot via
    /// `alloc()`. The r24-A2-S3 release happens asynchronously
    /// (detached compio task); a destructive `alloc()` poll could
    /// race against `next` and return a different slot, masking
    /// the release.
    ///
    /// Test-only because the freed set is a private implementation
    /// detail — production code should only call `alloc` /
    /// `release` / `reserve`.
    #[cfg(test)]
    pub fn freed_for_test(&self) -> &BTreeSet<u16> {
        &self.freed
    }

    /// Mark `i` as in-use without taking it from the free list. Used
    /// by the controller's restart-restore path (preview-URL § II.0):
    /// a sealed record's `vm_index` must be claimed in the allocator
    /// before normal `alloc()` traffic resumes — otherwise a fresh
    /// `create()` could hand the same index to a new sandbox while
    /// the original VM is still alive.
    ///
    /// Returns `Err` if `i` is out of `[floor, ceil]` OR if `i` is
    /// already in-use (a hard-error reserve — the caller cannot
    /// silently overwrite a live slot). B18 fix: the prior
    /// `restore_handler::VmIndexReservations` was a "set if not
    /// present" map with `Err("already reserved")` on collision; the
    /// same contract is preserved here so the restore handler's
    /// collision error message format ("vm_index N already
    /// reserved") stays intact.
    pub fn reserve(&mut self, i: u16) -> Result<(), String> {
        if i < self.floor || i > self.ceil {
            return Err(format!(
                "vm-index {i} out of range [{}, {}]",
                self.floor, self.ceil
            ));
        }
        // If the index is currently in flight from `alloc()` (i.e.,
        // already handed out and not in `freed`), reserving it would
        // hand the same slot to two callers — the exact B18 race.
        // Detect: `i < self.next` AND `i ∉ self.freed` ⇒ in flight.
        if i < self.next && !self.freed.contains(&i) {
            return Err(format!("vm_index {i} already reserved"));
        }
        // Bump `next` past `i` so future first-time allocs don't
        // hand it out, and remove `i` from the freed set if the
        // pre-restart sandbox happened to land on a previously-
        // released index.
        if i >= self.next {
            self.next = i.saturating_add(1);
        }
        self.freed.remove(&i);
        Ok(())
    }
}

// ─── NomadStopPermits ────────────────────────────────────────────
//
// r30-A1 (concurrency-r30 CRITICAL #A1): global semaphore gating
// concurrent `stop_inner` calls — and thereby every in-flight Nomad
// `/shutdown` ladder. Shared across all 7 production teardown call
// sites (`AppStateGcStopper`, snap-idle-evict, snap-idle-gc, admin
// snapshot teardown, transient-state takeover, registry GC, restore-
// failure rollback) so per-loop caps don't compound on the same
// downstream (Nomad RPC queue + host CH process budget).
//
// **Why a flume bounded channel and not `compio::sync::Semaphore`**:
// `compio` 0.18 (this workspace's pin) does not ship a `sync::Semaphore`.
// `tokio::sync::Semaphore` is off-limits — this workspace is zero-tokio
// (see AGENTS.md "Key invariants"). flume is already in the dep graph
// (`crates/sandbox/src/db.rs` Phase-1 worker queue), its `async`
// feature is runtime-agnostic by design, and a bounded channel of N
// `()` tokens IS the canonical async-semaphore pattern in
// zero-tokio Rust: `acquire = recv_async()`, `release =
// guard-Drop-try_send(())`. Capacity-bounded by construction, never
// negative, no busy spin.
//
// **Lifecycle**:
//
// 1. `AppState::from_config` reads `SANDBOX_NOMAD_STOP_CONCURRENCY`
//    (default 16), constructs `Arc<NomadStopPermits>` with that
//    capacity, calls `crate::metrics::set_nomad_stop_permits_total`,
//    holds it on `AppState.nomad_stop_permits`, and calls
//    `nomad_ch_backend.install_nomad_stop_permits(perms.clone())`.
//
// 2. Every `stop_inner` invocation calls `permits.acquire().await`
//    BEFORE the `/shutdown` ladder; the returned `NomadStopPermitGuard`
//    holds the permit for the full `/shutdown` → Nomad-purge →
//    host-fence → vm_index-release tail. The guard's Drop releases
//    the permit + decrements the gauge.
//
// 3. Tests assert N permits is enforced by spawning N+1 concurrent
//    `acquire()` calls and verifying exactly N complete before any
//    guard drops.
//
// Per-loop caps (`GC_STOP_CONCURRENCY=8` in registry.rs, the snap-
// idle-evict default-4 in sweep.rs) stay in place as defense-in-depth
// soft caps — they bound runaway fan-out BEFORE it ever reaches the
// semaphore. The load-bearing global cap is here.
#[derive(Debug)]
pub struct NomadStopPermits {
    /// The token pool. Each `recv_async()` ≡ acquire one permit;
    /// each `try_send(())` on the matching Sender ≡ release.
    tokens: flume::Receiver<()>,
    /// Refill channel — the guard's Drop calls `try_send(())` to put
    /// the permit back. `Sender` is Clone (cheap; refcounts the inner
    /// flume state), so the guard owns a clone and the semaphore can
    /// outlive any individual guard.
    refill: flume::Sender<()>,
    /// Configured capacity. Pinned at construction; `permits_available()`
    /// + `in_use()` derive from this and the live channel state.
    capacity: usize,
}

impl NomadStopPermits {
    /// Construct a semaphore pre-loaded with `capacity` permits. Panics
    /// on `capacity == 0` (a zero-permit semaphore would deadlock every
    /// caller); production config validation rejects 0 at boot —
    /// `crate::config::NomadCHConfig::validate` — so this panic is the
    /// belt-and-suspenders backstop for an in-process bug, not a user-
    /// facing failure mode.
    pub fn new(capacity: usize) -> Arc<Self> {
        assert!(
            capacity > 0,
            "NomadStopPermits capacity must be ≥ 1 (got 0); a zero-permit \
             semaphore deadlocks every teardown path. Boot-time config \
             validation rejects 0 at the env-parse layer — reaching this \
             panic means an in-process caller built a Self with capacity=0.",
        );
        let (tx, rx) = flume::bounded::<()>(capacity);
        for _ in 0..capacity {
            // bounded channel cap == capacity ⇒ first `capacity` sends
            // always succeed. `try_send` is the right primitive because
            // we never want this to block (we're in `new`, not on a
            // hot path); the `expect` is the assertion this invariant
            // holds for the lifetime of the cargo build.
            tx.try_send(())
                .expect("flume::bounded(N) accepts first N try_sends");
        }
        Arc::new(Self {
            tokens: rx,
            refill: tx,
            capacity,
        })
    }

    /// Acquire one permit. Awaits if the pool is exhausted; resolves
    /// (and bumps `sandbox_nomad_stop_permits_in_use`) once a permit is
    /// available. The returned guard releases the permit on Drop —
    /// callers should hold it for exactly the lifetime of the
    /// downstream operation (the full `stop_inner` `/shutdown` ladder).
    pub async fn acquire(&self) -> NomadStopPermitGuard {
        // `recv_async()` resolves to `Err` only if the channel is
        // disconnected (every Sender dropped). The semaphore holds its
        // own `Sender` clone (`refill`), so disconnection is impossible
        // for the lifetime of `self` — `expect` documents that invariant
        // explicitly rather than silently swallowing the result.
        self.tokens
            .recv_async()
            .await
            .expect(
                "NomadStopPermits tokens channel is never disconnected — \
                 the semaphore holds the matching Sender for its lifetime",
            );
        crate::metrics::inc_nomad_stop_permits_in_use();
        NomadStopPermitGuard {
            refill: self.refill.clone(),
        }
    }

    /// Number of permits currently available (not in flight). Cheap —
    /// `flume::Receiver::len()` reads the channel's pending-item count.
    /// Used by tests + `metrics_export` (the live in-use gauge is
    /// derived as `capacity - permits_available`).
    pub fn permits_available(&self) -> usize {
        self.tokens.len()
    }

    /// Boot-resolved capacity (does not change at runtime). Pair with
    /// `permits_available()` to compute in-use: `capacity - available`.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// RAII guard returned by [`NomadStopPermits::acquire`]. Drop releases
/// the permit back to the pool AND decrements
/// `sandbox_nomad_stop_permits_in_use`. The guard holds a clone of the
/// refill `Sender` (not a borrow) so the caller can stash it on the
/// stack across awaits without lifetime gymnastics.
///
/// `must_use`: dropping a permit guard without using it (`let _ =
/// permits.acquire().await`) IS the release-it-immediately shape and
/// is legal — `must_use` would flag legitimate test patterns. The
/// guard does its work in `Drop`, not via a method call.
#[derive(Debug)]
pub struct NomadStopPermitGuard {
    refill: flume::Sender<()>,
}

impl Drop for NomadStopPermitGuard {
    fn drop(&mut self) {
        // `try_send(())` on a bounded channel of capacity N MUST succeed
        // for the first N sends — and we never send more than `capacity`
        // tokens (every send is paired 1:1 with an acquire-recv). The
        // only failure mode would be a programming error (someone
        // smuggled an extra `Sender` outside this module and over-sent);
        // even then we'd rather lose a permit than panic in a Drop on
        // a teardown path.
        let _ = self.refill.try_send(());
        crate::metrics::dec_nomad_stop_permits_in_use();
    }
}

impl NomadCHBackend {
    pub fn new(
        cfg: SandboxConfig,
        persist: Option<Arc<crate::persist::Persistence>>,
    ) -> Result<Self, String> {
        let alloc = VmIndexAllocator::new(
            cfg.nomad_ch.vm_index_floor,
            cfg.nomad_ch.vm_index_ceil,
        );
        Ok(Self {
            cfg,
            state: Arc::new(RwLock::new(HashMap::new())),
            vm_index_allocator: Arc::new(Mutex::new(alloc)),
            creating_users: Arc::new(Mutex::new(HashSet::new())),
            healthy: Arc::new(AtomicBool::new(false)),
            last_probe_err: Arc::new(Mutex::new(None)),
            persist,
            // r3-A default: None. Production wiring sets this via
            // `with_local_nomad_node_id` from
            // `AppState::from_config` after the boot-time
            // `/v1/agent/self` lookup succeeds.
            local_nomad_node_id: None,
            // r30-A1: empty by default; production installs the shared
            // semaphore via `install_nomad_stop_permits` from
            // `AppState::from_config`. Unit tests that don't exercise
            // the cap leave it empty (stop_inner skips the acquire).
            nomad_stop_permits: std::sync::OnceLock::new(),
        })
    }

    /// r30-A1: install the shared `Arc<NomadStopPermits>` semaphore.
    /// Called exactly once from `AppState::from_config`, after the
    /// boot-time parse of `SANDBOX_NOMAD_STOP_CONCURRENCY` sizes the
    /// pool. Idempotent — a second install attempt is a silent no-op
    /// (the `OnceLock::set` Err arm), because the shared semaphore is
    /// process-global and there is exactly one `AppState` per process.
    ///
    /// Why `&self` (not `&mut self`) + `OnceLock`: the backend is held
    /// behind `Arc<NomadCHBackend>` inside the `Backend::NomadCh(arc)`
    /// enum variant, so the post-construction install can't take
    /// `&mut self`. `OnceLock` gives a publish-once, read-many shape
    /// that `stop_inner` consults from `&self` without contending the
    /// rest of the backend's locks.
    pub fn install_nomad_stop_permits(&self, permits: Arc<NomadStopPermits>) {
        // Silent on duplicate-install: the only legitimate caller is
        // `AppState::from_config`, and that path runs once. A test
        // re-install would be a footgun (the second semaphore is
        // dropped, leaving the first wired) — but explicit panic /
        // error here would break the `from_config` reentrancy the
        // sandbox_pg_e2e fixtures lean on.
        let _ = self.nomad_stop_permits.set(permits);
    }

    /// r30-A1: read-side accessor for the installed semaphore. Returns
    /// `None` when the cap is uninstalled (unit-test / single-tenant
    /// binary path). Used by `stop_inner` to acquire a permit before
    /// the `/shutdown` ladder, and by tests to inspect the live cap.
    pub fn nomad_stop_permits(&self) -> Option<&Arc<NomadStopPermits>> {
        self.nomad_stop_permits.get()
    }

    /// r3-A (T-8b-stress-r3 fix): install the cached local Nomad
    /// node_id. When set, [`build_nomad_job_json_with`] emits a
    /// `Constraints` block pinning every cold-boot alloc to THIS
    /// worker — closing the cross-node placement race where
    /// `workspace.img` is staged on local fs but Nomad schedules
    /// the alloc on a different worker (the driver's
    /// `assert_disk_image_present` ENOENTs there).
    ///
    /// Mirrors the [`Self::vm_index_allocator`] / [`RealRestoreBackend::
    /// with_nomad_handle`] builder shape: replaces any prior value;
    /// `None` (the constructor default) disables the constraint
    /// emission entirely. The production wiring (`AppState::from_config`)
    /// passes either `Some(id)` (lookup succeeded) or never calls
    /// the builder (lookup failed → metric bumped at boot).
    pub fn with_local_nomad_node_id(mut self, node_id: Option<String>) -> Self {
        self.local_nomad_node_id = node_id;
        self
    }

    /// r3-A: read accessor for the cached local Nomad node_id. Mainly
    /// here so the AppState wiring (which constructs the backend via
    /// `Backend::builder(&cfg).with_persist(...).with_local_nomad_node_id(...).build()`)
    /// can assert post-install state.
    pub fn local_nomad_node_id(&self) -> Option<&str> {
        self.local_nomad_node_id.as_deref()
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// Shared handle to the per-worker `vm_index` allocator. Used by
    /// `crate::restore_handler::RealRestoreBackend` so the wake path's
    /// `reserve(source_slot)` and the create path's `alloc()` go
    /// through the same allocator state. Without this share, the
    /// restore path reserved into a private map while the create
    /// allocator still saw the slot as free — a subsequent CREATE on
    /// the same worker handed the same tap/IP to a fresh sandbox that
    /// collided with the live restored VM, surfacing as a stale-pubkey
    /// 401 on `/version` (cluster smoke 2026-05-24 r4 bug B18; 11/16
    /// c=4 cycles failed once slots 1-6 had been used once).
    pub fn vm_index_allocator(&self) -> Arc<Mutex<VmIndexAllocator>> {
        Arc::clone(&self.vm_index_allocator)
    }

    pub async fn probe(&self) -> Result<(), String> {
        // `/v1/status/leader` is the cheapest reachability probe —
        // returns the leader's `host:port` as a JSON-quoted string,
        // or 500 if no quorum. We don't parse the body; status==200
        // is enough to flip the `healthy` bit.
        let url = format!("{}/v1/status/leader", self.cfg.nomad_ch.nomad_addr);
        let resp = http_get_unsigned(&url, Duration::from_secs(5)).await;
        match resp {
            Ok(r) if r.status == 200 => {
                self.healthy.store(true, Ordering::Relaxed);
                *self.last_probe_err.lock().unwrap_or_else(|p| p.into_inner()) = None;
                Ok(())
            }
            Ok(r) => {
                self.healthy.store(false, Ordering::Relaxed);
                let msg = format!(
                    "nomad /v1/status/leader → status {}: {}",
                    r.status,
                    r.body.trim()
                );
                *self.last_probe_err.lock().unwrap_or_else(|p| p.into_inner()) =
                    Some(msg.clone());
                Err(msg)
            }
            Err(e) => {
                self.healthy.store(false, Ordering::Relaxed);
                let msg = format!("nomad probe failed: {e}");
                *self.last_probe_err.lock().unwrap_or_else(|p| p.into_inner()) =
                    Some(msg.clone());
                Err(msg)
            }
        }
    }

    /// Best-effort cleanup of leftover `zsbx-` jobs from a previous
    /// controller process. Same gating + HA caveat as the K8s
    /// equivalent: defaults off; opt-in via
    /// `SANDBOX_NOMAD_CH_STARTUP_ORPHAN_CLEANUP=true` for single-
    /// replica operators.
    pub async fn cleanup_orphans_at_startup(&self) -> Result<usize, String> {
        if !self.cfg.nomad_ch.startup_orphan_cleanup {
            return Ok(0);
        }
        let base = self.cfg.nomad_ch.nomad_addr.clone();
        let url = format!("{base}/v1/jobs?prefix=zsbx-");
        let resp = http_get_unsigned(&url, Duration::from_secs(10)).await?;
        if resp.status != 200 {
            return Err(format!(
                "nomad list jobs prefix=zsbx- → status {}: {}",
                resp.status,
                resp.body.trim()
            ));
        }
        let jobs: serde_json::Value = serde_json::from_str(&resp.body)
            .map_err(|e| format!("nomad list jobs not JSON: {e}"))?;
        let mut deleted = 0usize;
        for j in jobs.as_array().into_iter().flatten() {
            let id = match j["ID"].as_str() {
                Some(s) if s.starts_with("zsbx-") => s.to_string(),
                _ => continue,
            };
            let stop_url = format!("{base}/v1/job/{id}?purge=true");
            if let Err(e) = http_delete_unsigned(&stop_url, Duration::from_secs(10)).await {
                tracing::warn!(job_id = %id, error = %e, "sandbox/nomad-ch orphan-cleanup: purge failed");
                continue;
            }
            deleted += 1;
        }
        if deleted > 0 {
            tracing::info!(deleted, "sandbox/nomad-ch orphan-cleanup: purged orphan jobs");
        }
        Ok(deleted)
    }

    pub async fn create(
        &self,
        sandbox_id: Uuid,
        user_id: &str,
        project_id: &str,
    ) -> Result<SandboxInfo, String> {
        validate_typed_id(user_id, "usr", "user_id")?;
        validate_typed_id(project_id, "prj", "project_id")?;

        // M7 circuit-breaker. Pool exhaustion under partial-failure
        // storm: 50 concurrent stalled Nomad RPCs would saturate
        // the spawn_blocking pool, queueing every other backend op
        // controller-wide. The probe loop sets `healthy=false` on
        // any 5xx / transport error from Nomad; bail BEFORE we
        // submit the next RPC into a backend known to be down. A
        // *terminal* error — by contract this is configuration /
        // infra, not transient (the probe is what flips healthy
        // back to true), so callers should not retry.
        if !self.is_healthy() {
            let last = self
                .last_probe_err
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
                .unwrap_or_else(|| "<no probe error captured>".to_string());
            return Err(format!(
                "nomad-ch backend unhealthy; refusing new sandboxes \
                 (most recent probe error: {last}). This is a config \
                 or infra problem; the probe loop will flip the bit \
                 back when Nomad recovers."
            ));
        }

        // Per-user serialization gate. Two concurrent creates for
        // the same user racing the "one active sandbox per user"
        // check would each see "no existing", both would submit
        // jobs, and both would race the per-user home dir. Fail
        // fast on collision so the caller can retry; mirrors
        // K8sBackend.
        {
            let mut creating = self
                .creating_users
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if !creating.insert(user_id.to_string()) {
                return Err(format!(
                    "concurrent sandbox create in progress for user {user_id:?}; retry"
                ));
            }
        }
        let release_creating =
            ReleaseCreating::new(self.creating_users.clone(), user_id.to_string());

        // One active sandbox per user — stop any existing sandbox
        // for this user before creating the new one. (Today the
        // per-user home dir is on a shared host fs; concurrent
        // mounts would be safe but the next milestone moves it to
        // RWO Ceph RBD — same-user double-attach would error there.
        // Keeping the same one-per-user invariant now means no
        // contract change later.)
        // HashMap value is plain owned data; poison can't break
        // invariants — recover.
        //
        // MUST collect into Vec; do not iterate while holding the read
        // lock — stop() takes write, and a future refactor that drops
        // the .collect() and iterates lazily would deadlock the
        // first time a user has > 0 active sandboxes.
        let existing: Vec<Uuid> = self
            .state
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(_, s)| s.user_id == user_id)
            .map(|(id, _)| *id)
            .collect();
        for old_id in existing {
            tracing::info!(
                user_id = %user_id,
                old_sandbox_id = %old_id,
                "sandbox/nomad-ch create: user already has sandbox; stopping first"
            );
            if let Err(e) = self.stop(old_id).await {
                tracing::warn!(sandbox_id = %old_id, error = %e, "sandbox/nomad-ch create: stop failed");
            }
        }

        let job_id = format!("zsbx-{}", sandbox_id.simple());
        let host_dir = self
            .cfg
            .nomad_ch
            .host_state_dir
            .join(sandbox_id.simple().to_string());
        // virtio-blk pivot (bug #11): the per-user home is now a single
        // raw ext4 image file rather than a directory. The file lives
        // at `<user_home_dir_root>/<user_id>/home.img` and is reused
        // across all of that user's sandboxes (package caches, dotfiles
        // persist). The wrapper attaches it as the guest's /dev/vdc.
        let user_home_img = user_home_image_path(
            &self.cfg.nomad_ch.user_home_dir_root,
            user_id,
        );

        let mut guard = CreateGuard::new(
            self.vm_index_allocator.clone(),
            self.cfg.nomad_ch.nomad_addr.clone(),
            job_id.clone(),
            host_dir.clone(),
            sandbox_id,
            Duration::from_secs(
                self.cfg.nomad_ch.vm_index_release_delay_secs,
            ),
        );

        let result = self
            .try_create(
                sandbox_id,
                user_id,
                project_id,
                &job_id,
                &host_dir,
                &user_home_img,
                &mut guard,
            )
            .await;

        match result {
            Ok(info) => {
                guard.disarm();
                drop(release_creating);
                Ok(info)
            }
            Err(e) => {
                drop(release_creating);
                Err(e)
            }
        }
    }

    /// Inner body of [`Self::create`] — split out so the
    /// `CreateGuard` Drop runs on every error path without the
    /// caller having to remember `?` discipline. Not part of the
    /// public API; called only from `create()`.
    #[allow(clippy::too_many_arguments)]
    async fn try_create(
        &self,
        sandbox_id: Uuid,
        user_id: &str,
        project_id: &str,
        job_id: &str,
        host_dir: &Path,
        user_home_img: &Path,
        guard: &mut CreateGuard,
    ) -> Result<SandboxInfo, String> {
        let create_started = Instant::now();
        // 1. Mint Ed25519 keypair. Public half is the only thing
        //    that leaves this process; the private half stays in
        //    `signing_key` for the lifetime of the sandbox.
        //
        //    Wrap in Arc immediately so step 7's livez+fingerprint
        //    probe can sign /version without taking ownership; we
        //    take a fresh `Arc::clone` (cheap refcount bump) at the
        //    commit step so the in-state-map sandbox owns its own
        //    handle.
        let sk_bytes = random_key32()?;
        let signing_key = Arc::new(SigningKey::from_bytes(&sk_bytes));
        let pubkey = signing_key.verifying_key();
        // virtio-blk pivot (bug #11): the pubkey no longer travels
        // through a virtiofs-mounted file; we hex-encode it and the
        // wrapper injects it into the guest's kernel cmdline as
        // `zsbx_pubkey=<hex>`. The guest's /sbin/init decodes the
        // hex back to 32 raw bytes at /run/keys/controller-pubkey,
        // which is the path the agent's auth loader reads. base64
        // is no longer emitted here (the agent accepts either raw
        // 32 bytes or base64, and the cmdline path produces raw).
        let pubkey_hex = hex::encode(pubkey.as_bytes());
        let key_fp = sig::pubkey_fingerprint(&pubkey);
        tracing::info!(
            sandbox_id = %sandbox_id,
            user_id = %user_id,
            project_id = %project_id,
            key_fp = %key_fp,
            "sandbox/nomad-ch create"
        );

        // 2. Allocate VM index from the pool. Track in the guard so
        //    cleanup-on-failure releases it.
        let vm_index = self
            .vm_index_allocator
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .alloc()
            .map_err(|e| {
                tracing::warn!(
                    sandbox_id = %sandbox_id,
                    step = "vm_index_alloc",
                    error = %e,
                    "sandbox/nomad-ch create error"
                );
                e
            })?;
        guard.vm_index = Some(vm_index);
        tracing::info!(
            vm_index,
            sandbox_id = %sandbox_id,
            "sandbox/nomad-ch vm_index allocated"
        );

        // 3. Materialize host disk images for the two virtio-blk
        //    devices the wrapper attaches:
        //      - `<host_dir>/workspace.img` — per-sandbox; freshly
        //        created (sparse `truncate -s … + mkfs.ext4`).
        //      - `<user_home_dir_root>/<user>/home.img` — per-user;
        //        created once on the user's first sandbox, reused
        //        across subsequent sandboxes (package caches +
        //        dotfiles persist). The directory tree is mkdir'd
        //        for the image's parent.
        //
        //    Idempotent: if the file already exists at the path we
        //    skip both `truncate` and `mkfs.ext4`. This matters for
        //    `home.img` (per-user reuse) and is harmless belt-and-
        //    braces for `workspace.img` (per-sandbox dir is unique
        //    by UUID).
        //
        //    Disk-image creation costs ~1–3s on first invocation
        //    (mostly the mkfs.ext4 metadata write); per-user reuse
        //    means the cost amortizes to ~0 after the user's first
        //    sandbox.
        //
        //    R26-I2 / R25-I1 / R23-A2: the bundle of sync-IO ops below
        //    (`create_dir_all` ×2, `mkfs.ext4` subprocess ×2 via
        //    `create_ext4_image_if_missing`, plus the inner `fsync_dir`
        //    on the image parents) is moved off the ntex worker via
        //    `compio::runtime::spawn_blocking`. Without this, cold-boot
        //    first-sandbox-per-user pegged a ntex worker for ~3-5s on
        //    the mkfs metadata writes; at c=20 stress (post r3-A node-
        //    pin where all CREATEs land on one controller's pool of
        //    4-8 ntex workers) sibling requests added ~9-15s to their
        //    p99. The wrap unblocks the worker; per-CREATE wall time
        //    is unchanged.
        //
        //    CreateGuard handling: `host_dir_created` is set on the
        //    calling thread AFTER spawn_blocking returns Ok, mirroring
        //    the pre-wrap behaviour. The guard reference stays on the
        //    ntex worker; only owned clones of `host_dir` and
        //    `user_home_img` cross the spawn_blocking boundary. This
        //    keeps guard ownership trivial — no Send/Sync threading
        //    through the closure required.
        // Option C Phase 2 (2026-05-25 staging-locality ADR): if the
        // operator has flipped `driver_stages_disk_images=true`, we
        // BYPASS the spawn_blocking truncate+mkfs.ext4 block below
        // and let the driver materialize the images on the worker
        // that runs the alloc. The jobspec carries a typed meta
        // field (`zsbx_stage_disks`) plus the typed driver Config
        // field (`stage_disk_images`) the Go driver decodes from
        // its TaskConfig HCL schema.
        //
        // The workspace_img path itself is STILL derived
        // declaratively here so the existing `build_nomad_job_json`
        // signature is unchanged — the path is what the driver
        // creates an image at, regardless of which side does the
        // mkfs.ext4. `guard.host_dir_created` stays FALSE in this
        // branch (the driver owns the dirent's lifecycle now;
        // CreateGuard's host_dir rollback is a no-op under
        // driver-side staging, per the ADR Phase 2 plan).
        //
        // When the flag is false (Phase 2 default), the legacy
        // spawn_blocking block runs verbatim — controller stages,
        // driver consumes pre-staged paths via preflightDiskPaths.
        // Phase 4 cluster validation flips the default; Phase 3
        // deletes the spawn_blocking branch entirely.
        let workspace_img: PathBuf = if self.cfg.driver_stages_disk_images {
            workspace_image_path(host_dir)
        } else {
            let host_dir_owned = host_dir.to_path_buf();
            let user_home_img_owned = user_home_img.to_path_buf();
            let workspace_img_size_gb = self.cfg.workspace_image_size_gb;
            let staged = compio::runtime::spawn_blocking(move || -> Result<PathBuf, String> {
                std::fs::create_dir_all(&host_dir_owned)
                    .map_err(|e| format!("mkdir {}: {}", host_dir_owned.display(), e))?;
                if let Some(parent) = user_home_img_owned.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("mkdir {}: {}", parent.display(), e))?;
                }
                let workspace_img = workspace_image_path(&host_dir_owned);
                create_ext4_image_if_missing(&workspace_img, workspace_img_size_gb)
                    .map_err(|e| format!("workspace.img: {e}"))?;
                create_ext4_image_if_missing(&user_home_img_owned, workspace_img_size_gb)
                    .map_err(|e| format!("home.img: {e}"))?;
                Ok(workspace_img)
            })
            .await
            .unwrap_or_else(|p| Err(format!("spawn_blocking panic: {p:?}")))?;
            guard.host_dir_created = true;
            staged
        };

        // 4. (was: write pubkey file — now baked into the cmdline by
        //    the wrapper, see step 5's ZSBX_PUBKEY_HEX env var.)

        // 5. Build + submit the Nomad job spec.
        //
        // B24 / R8-DEPLOY1: the sandbox_id flows into
        // `ZSBX_SANDBOX_ID`, which the wrapper embeds VERBATIM in
        // the guest's kernel cmdline. The wrapper's validator
        // (nomad-vm-wrapper.sh:222) rejects any character outside
        // `[0-9a-zA-Z_]` — that's hyphens too. Uuid's hyphenated
        // form (`to_string()`) would fail it; `.simple()` (32-hex,
        // no hyphens) passes and matches the format the rest of
        // this file already uses for `job_id` and `host_dir`.
        let job_json = build_nomad_job_json(
            job_id,
            &self.cfg,
            vm_index,
            &workspace_img,
            user_home_img,
            &pubkey_hex,
            user_id,
            project_id,
            &sandbox_id.simple().to_string(),
            self.local_nomad_node_id.as_deref(),
        );
        submit_nomad_job(&self.cfg.nomad_ch.nomad_addr, &job_json)
            .await
            .map_err(|e| {
                tracing::warn!(
                    sandbox_id = %sandbox_id,
                    step = "submit_nomad_job",
                    job = %job_id,
                    error = %e,
                    "sandbox/nomad-ch create error"
                );
                e
            })?;
        guard.job_submitted = true;

        // 6. Poll until at least one alloc reaches running. Bounded
        //    by the Nomad-scheduling budget (alloc_running_timeout_secs);
        //    "running" here means the wrapper script started, NOT that
        //    the VM is up — the agent /livez wait below covers the
        //    in-VM boot path.
        wait_for_alloc_running(
            &self.cfg.nomad_ch.nomad_addr,
            job_id,
            Duration::from_secs(self.cfg.nomad_ch.alloc_running_timeout_secs),
        )
        .await
        .map_err(|e| {
            tracing::warn!(
                sandbox_id = %sandbox_id,
                step = "wait_for_alloc_running",
                error = %e,
                "sandbox/nomad-ch create error"
            );
            e
        })?;
        tracing::info!(
            sandbox_id = %sandbox_id,
            vm_index,
            job = %job_id,
            elapsed_ms = %create_started.elapsed().as_millis(),
            "sandbox/nomad-ch create alloc running"
        );

        // 7. Wait for the in-VM agent to come up. The wrapper boots
        //    CH; CH boots Linux; init.sh execs sandbox-agent. Bound
        //    this with its own budget (agent_livez_timeout_secs) so
        //    operators can tell apart "Nomad slow to schedule" from
        //    "VM/kernel/agent slow to boot".
        // M6: second octet is configurable so an operator with a
        // corp 10.99/16 collision can shift to a different private
        // /16. Both the controller and the wrapper read the same
        // value (controller from `cfg.nomad_ch.subnet_second_octet`,
        // wrapper from `ZSBX_SUBNET_BASE_OCTET` env var passed by
        // build_nomad_job_json).
        //
        // FM-A: also pass `key_fp` + signing_key so wait_for_agent_livez
        // verifies the agent answering /livez is OUR agent (verifies
        // our pubkey on /version), not a stale tenant whose CH is
        // still alive after Nomad already reported the prior alloc
        // terminal. Without this, a fresh create() racing the prior
        // wrapper's process tree returns 201 in 0.25 s pointing at
        // an agent that dies seconds later → "No route to host" on
        // every subsequent /exec.
        let agent_url = format!(
            "http://10.{}.{}.2:{AGENT_PORT}",
            self.cfg.nomad_ch.subnet_second_octet,
            100u16 + vm_index
        );
        let livez_started = Instant::now();
        wait_for_agent_livez(
            &agent_url,
            &key_fp,
            &signing_key,
            Duration::from_secs(self.cfg.nomad_ch.agent_livez_timeout_secs),
        )
        .await
        .map_err(|e| {
            tracing::warn!(
                sandbox_id = %sandbox_id,
                step = "wait_for_agent_livez",
                agent_url = %agent_url,
                error = %e,
                "sandbox/nomad-ch create error"
            );
            e
        })?;
        tracing::info!(
            sandbox_id = %sandbox_id,
            vm_index,
            key_fp = %key_fp,
            elapsed_ms = %livez_started.elapsed().as_millis(),
            "sandbox/nomad-ch create agent_ready"
        );

        // 8. Commit state. Refuse to overwrite an existing entry —
        //    a duplicate sandbox_id is a controller-bug or caller-bug,
        //    and silently overwriting would leak the prior entry's
        //    vm_index, host_dir, and Nomad job (the values still
        //    referenced by the old NomadChSandbox would never run
        //    through stop()). Bail with an error and let
        //    CreateGuard's Drop tear down the partial state we
        //    just built. DO NOT disarm the guard on this branch.
        {
            use std::collections::hash_map::Entry;
            let mut guard_state = self
                .state
                .write()
                .unwrap_or_else(|p| p.into_inner());
            match guard_state.entry(sandbox_id) {
                Entry::Vacant(slot) => {
                    slot.insert(NomadChSandbox {
                        user_id: user_id.to_string(),
                        job_id: job_id.to_string(),
                        vm_index,
                        host_dir: host_dir.to_path_buf(),
                        agent_url: agent_url.clone(),
                        // signing_key is already Arc<SigningKey> at
                        // step 1 (so wait_for_agent_livez can borrow
                        // it for its /version probe); move into the
                        // state map verbatim.
                        signing_key,
                    });
                }
                Entry::Occupied(_) => {
                    return Err(format!(
                        "[sandbox/nomad-ch] commit: sandbox_id {sandbox_id} \
                         already present in state map; refusing to overwrite \
                         (CreateGuard will tear down the just-built partial state)"
                    ));
                }
            }
        }

        let now = unix_now();

        // Seal the per-sandbox auth to disk (preview-URL § II.0 §4).
        // BEST-EFFORT: a seal failure does NOT fail create() — the
        // sandbox is live and usable; persistence is for restart
        // resilience only. Log loudly so operators see when the
        // restart-restore guarantee is degraded for this sandbox.
        // nomad-ch records intentionally seal `agent_url = None`:
        // it's deterministically derived from `vm_index` at restore
        // time, which shrinks the AEAD plaintext + removes a
        // migration hazard if the agent listen address ever changes.
        if let Some(persist) = &self.persist {
            // v3 (round-8): sealed record carries secrets only —
            // user_id, project_id, backend, vm_index, agent_url,
            // pubkey_fp, created_at_secs all live in pg now.
            let record = crate::persist::SealedAuth {
                version: crate::persist::SEAL_VERSION,
                sandbox_id: sandbox_id.to_string(),
                signing_key_bytes: sk_bytes,
                preview_secrets: None,
                boot_id: None,
            };
            if let Err(e) = persist.seal(sandbox_id, &record).await {
                tracing::warn!(
                    sandbox_id = %sandbox_id,
                    vm_index,
                    error = %e,
                    "sandbox/nomad-ch persist.seal failed (non-fatal; sandbox live, restart-restore unavailable for this record)"
                );
            }
        }

        Ok(SandboxInfo {
            sandbox_id: sandbox_id.to_string(),
            user_id: user_id.to_string(),
            project_id: project_id.to_string(),
            backend: "nomad-ch".to_string(),
            backend_hint: format!("job={job_id} vm_index={vm_index} key_fp={key_fp}"),
            created_at_secs: now,
            last_used_at_secs: now,
        })
    }

    /// Stop the sandbox.
    ///
    /// **Concurrent-stop semantics:** when two callers race `stop` on
    /// the same sandbox_id, the first one removes the entry from the
    /// in-memory state map and proceeds with Nomad-job-purge +
    /// vm_index release; the second one finds nothing in the map and
    /// returns `Ok(())` immediately, even though the underlying Nomad
    /// job teardown is still in flight from the first caller. This is
    /// intentional — `stop` is a "best-effort, idempotent" contract.
    /// Callers that need strict "fully gone" semantics (e.g. wait
    /// until the tap device is freed) should poll [`list`] or
    /// equivalent until the sandbox no longer appears.
    ///
    /// **vm_index leak on Nomad failure:** if `wait_for_job_gone`
    /// errors (Nomad API down, timeout, etc.) we do NOT release the
    /// vm_index back to the pool — a follow-up `create` for the same
    /// user could otherwise grab the same index and bind a tap device
    /// that the still-alive previous job is using. Better a slowly-
    /// shrinking pool than a tap collision; orphan-prune at next
    /// controller boot reclaims indices indirectly (by deleting the
    /// jobs that were holding them). The host_dir is left in place
    /// for the same reason — virtiofsd may still hold its socket open.
    pub async fn stop(&self, sandbox_id: Uuid) -> Result<(), String> {
        // The for-real stop: tear everything down INCLUDING the
        // per-sandbox host_dir (which owns workspace.img). Callers
        // who need to keep `workspace.img` alive across a snapshot →
        // wake gap MUST use [`Self::stop_preserving_state`] instead.
        self.stop_inner(sandbox_id, true).await
    }

    /// Snapshot-aware variant of [`Self::stop`] that runs steps 1-4
    /// of the standard teardown (Nomad job purge + host-fence +
    /// vm_index release + in-memory map removal) **but skips both
    /// step 5's `remove_dir_all(host_dir)` AND the trailing
    /// `persist.delete(sandbox_id)`**. Symmetric with how `home.img`
    /// is intentionally preserved across snapshot lifetimes: the
    /// per-sandbox `workspace.img` (created under `host_dir`) holds
    /// durable user state that the restored VM re-mounts on wake,
    /// and the sealed record carries the signing key the next wake
    /// needs to talk to the restored agent. Wiping either would
    /// silently break wake.
    ///
    /// Used by the snapshot path's post-success teardown
    /// ([`super::Backend::teardown_source_for_snapshot`]). The
    /// host_dir AND sealed record are finally reaped by the next
    /// [`Self::stop`] call (operator delete, or terminal-not-
    /// restorable transition).
    ///
    /// Bug #15 fix (`docs/reviews/sandbox-snapshot-restore-cluster-
    /// 2026-05-23-r1.md`): the prior code called `stop` directly,
    /// which deleted `host_dir/workspace.img`, and the next wake's
    /// wrapper `[ ! -f $ZSBX_WORKSPACE_IMG ]` gate then tripped. C2
    /// (deferred 2026-05-24): the B15 fix only gated the host_dir
    /// rm; `persist.delete` still fired unconditionally. Now both
    /// share the gate.
    pub async fn stop_preserving_state(
        &self,
        sandbox_id: Uuid,
    ) -> Result<(), String> {
        self.stop_inner(sandbox_id, false).await
    }

    /// Shared implementation of [`Self::stop`] /
    /// [`Self::stop_preserving_state`]. The `remove_host_dir` bool
    /// gates **all on-host durable-state cleanup**:
    ///
    /// - step 5 (`rm -rf host_dir` — owns `workspace.img`), and
    /// - the trailing `persist.delete(sandbox_id)` call (the sealed
    ///   record carrying the signing key for restart-restore).
    ///
    /// When `true` (the regular stop path) both fire: host_dir is
    /// `rm -rf`'d after the Nomad job is confirmed gone + the
    /// host_fence has cleared, and the sealed record is removed.
    /// When `false` (the snapshot-aware teardown via
    /// [`Self::stop_preserving_state`]) both are skipped: the
    /// per-sandbox `workspace.img` AND its sealed record survive
    /// across the snapshot → wake gap. Wiping either would silently
    /// break wake — `workspace.img` because the wrapper's
    /// `[ ! -f $ZSBX_WORKSPACE_IMG ]` gate trips (bug #15), the
    /// sealed record because the moment wake plumbs sealed-record-
    /// based key recovery the agent becomes unreachable (deferred
    /// item C2).
    async fn stop_inner(
        &self,
        sandbox_id: Uuid,
        remove_host_dir: bool,
    ) -> Result<(), String> {
        let sandbox = match self
            .state
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&sandbox_id)
        {
            Some(s) => s,
            None => return Ok(()), // idempotent
        };
        // r30-A1 (concurrency-r30 CRITICAL #A1): acquire one permit
        // from the global `NomadStopPermits` semaphore BEFORE the
        // `/shutdown` ladder fires. All 7 production teardown call sites
        // funnel here; without the global cap the per-loop caps (snap-
        // idle-gc=8, snap-idle-evict default 4, three serial loops, two
        // unbounded admin paths) don't compose against the single
        // downstream (Nomad /shutdown RPC queue + host CH process
        // budget). Acquired inside stop_inner (not at the call sites)
        // so the cap is structurally impossible to bypass — even a
        // future teardown caller that forgets the convention still
        // contends for the global pool.
        //
        // The acquire happens AFTER the in-memory state remove so an
        // idempotent re-stop (Ok branch above) doesn't burn a permit.
        // The guard binds to `_permit` so its Drop runs at the END of
        // this function (post host_dir leak log + persist.delete tail),
        // covering every observable downstream interaction.
        //
        // `nomad_stop_permits().is_none()` is the unit-test +
        // single-tenant binary path; in those builds the field is
        // uninstalled, and the cap is a no-op (matches the legacy
        // behaviour those paths already tolerate).
        let _permit: Option<NomadStopPermitGuard> = match self.nomad_stop_permits() {
            Some(p) => Some(p.acquire().await),
            None => None,
        };
        let stop_started = Instant::now();
        tracing::info!(
            sandbox_id = %sandbox_id,
            vm_index = sandbox.vm_index,
            job = %sandbox.job_id,
            "sandbox/nomad-ch stop: started"
        );
        // Channel split: `errs` accumulates per-step failures the
        // caller needs to see (joined into the returned Result) so
        // the API surface lines up with the K8s backend; tracing
        // logs are reserved for operator-only diagnostics that don't
        // belong in the API response (vm_index leak warnings,
        // host_dir-skipped notices). Keeping these distinct means a
        // 200 stop() with operator log noise is observable, and a
        // 5xx stop() carries the actionable error text.
        let mut errs: Vec<String> = Vec::new();

        // 1. Drain the agent. Best-effort — if /shutdown 5xx-s the
        //    Nomad purge in step 2 still tears the VM down. We feed
        //    the error into `errs` (rather than via tracing) so
        //    the caller sees it; symmetric with steps 2/3/5 below.
        //    The aggregate error is non-fatal — we keep going through
        //    the cleanup tail regardless.
        if let Err(e) = http_signed_async(
            &sandbox.signing_key,
            "POST",
            &format!("{}/shutdown", sandbox.agent_url),
            &[],
        )
        .await
        {
            errs.push(format!(
                "/shutdown to {}: {e} (continuing with Nomad purge)",
                sandbox.job_id
            ));
        }

        // 2. Stop + purge the Nomad job.
        if let Err(e) =
            stop_nomad_job(&self.cfg.nomad_ch.nomad_addr, &sandbox.job_id, true).await
        {
            errs.push(format!("stop_nomad_job({}): {e}", sandbox.job_id));
        }

        // 3. Wait for the job to actually be gone before we hand the
        //    vm_index back to the pool. Otherwise a follow-up
        //    `create` for the same user races a still-running
        //    wrapper script binding the same tap device + IP.
        let job_gone = wait_for_job_gone(
            &self.cfg.nomad_ch.nomad_addr,
            &sandbox.job_id,
            Duration::from_secs(30),
        )
        .await;
        let job_confirmed_gone = match job_gone {
            Ok(()) => true,
            Err(e) => {
                errs.push(format!("wait_for_job_gone({}): {e}", sandbox.job_id));
                false
            }
        };

        // 4. Release the VM index ONLY if the job is confirmed gone
        //    AND the host-side `/livez` fence (FM-F) confirms the
        //    previous tenant's agent has stopped answering.
        //
        //    FM-F: Nomad's "alloc terminal" lags the host-process
        //    tree by 0.5–60 s under N=8 stress; releasing the
        //    vm_index inside that window hands a still-live IP to a
        //    new tenant whose subsequent /livez probe succeeds
        //    against the previous tenant's agent (the FM-A race the
        //    fingerprint check defends against). The fence here is
        //    the *primary* fix; the fingerprint check becomes pure
        //    defense-in-depth.
        //
        //    Fence policy: poll /livez for up to
        //    `host_fence_timeout_secs` (default 120s). Two
        //    consecutive failures (connect-refused, timeout, or 5xx)
        //    → "no agent listening" → release. If the fence times
        //    out, **leak** the vm_index — handing out a live IP is
        //    strictly worse than shrinking the pool. Orphan-prune at
        //    next controller boot reclaims it.
        //
        //    `host_fence_timeout_secs == 0` disables the fence
        //    entirely (legacy behaviour pre-FM-F; NOT recommended).
        let mut fence_passed = false;
        let mut fence_err: Option<String> = None;
        if job_confirmed_gone {
            let fence_secs = self.cfg.nomad_ch.host_fence_timeout_secs;
            if fence_secs == 0 {
                // Operator opted out. Preserve the legacy 500 ms
                // grace-sleep so the cleanup tail (virtiofsd
                // unmounts, network namespace teardown) gets at
                // least *some* breathing room before reuse.
                compio::time::sleep(Duration::from_millis(500)).await;
                fence_passed = true;
            } else {
                let fence_started = Instant::now();
                match wait_for_agent_silent(
                    &sandbox.agent_url,
                    Duration::from_secs(fence_secs),
                )
                .await
                {
                    Ok(()) => {
                        fence_passed = true;
                        tracing::info!(
                            sandbox_id = %sandbox_id,
                            agent_url = %sandbox.agent_url,
                            elapsed_ms = %fence_started.elapsed().as_millis(),
                            "sandbox/nomad-ch host_fence: cleared"
                        );
                    }
                    Err(e) => {
                        // Fence timed out — agent still answering.
                        // Leak the index loudly. The errs accumulator
                        // surfaces this to the API caller too — a
                        // failed fence on stop() means the platform
                        // is mid-pathology and the operator wants to
                        // know.
                        tracing::error!(
                            sandbox_id = %sandbox_id,
                            agent_url = %sandbox.agent_url,
                            elapsed_ms = %fence_started.elapsed().as_millis(),
                            error = %e,
                            "sandbox/nomad-ch host_fence: timeout"
                        );
                        errs.push(format!("host_fence({}): {e}", sandbox.agent_url));
                        fence_err = Some(e);
                    }
                }
            }
        }

        if fence_passed {
            // T-8b-stress-r8 r24-A2-S3: delay the release so the
            // host kernel has time to evict the tap netdev / drain
            // fcntl locks from this tenant's CH process before a
            // fresh CREATE picks up the same vm_index. The driver's
            // r24-A2-S2 closes the worker-side tuntap-add window;
            // this controller-side delay adds defense-in-depth.
            // Production default 5 s; 0 in tests via the
            // vm_index_release_delay_secs config knob.
            //
            // r29-A2 (R29-C1 class-fix): inline-await the release
            // instead of detaching the timer task. `stop_inner` is
            // reached from BOTH long-lived (ntex-worker, snap-idle-
            // gc, sweeper) and short-lived (`detach_isolated`'s
            // private compio runtime, used by `snap-teardown-<tail>`)
            // call paths. The detach-onto-current-runtime shape that
            // pre-r29 `spawn_delayed_release` used silently dropped
            // the timer on the short-lived path → vm_index leaked on
            // every successful admin-snapshot teardown (R29-C1).
            // Awaiting inline adds the delay to stop_inner's wall
            // (5 s in prod), which is negligible compared to the
            // host_fence + Nomad-purge waits already on this path.
            VmIndexAllocator::release_vm_index_after(
                Arc::clone(&self.vm_index_allocator),
                sandbox.vm_index,
                Duration::from_secs(
                    self.cfg.nomad_ch.vm_index_release_delay_secs,
                ),
                "stop-fence-passed",
                sandbox_id,
            )
            .await;
        } else if !job_confirmed_gone {
            // C-7-LT-2-PR2 defense-in-depth: bump the per-reason leak
            // counter so operators can `rate(sandbox_vm_index_leaks_total{
            // reason="wait_failed"})` and alert on a slot-leak storm
            // independently of the host_fence_timeout bucket.
            crate::metrics::inc_vm_index_leak("wait_failed");
            tracing::warn!(
                target: "sandbox::teardown::leak",
                vm_index = sandbox.vm_index,
                reason = "wait_failed",
                sandbox_id = %sandbox_id,
                job = %sandbox.job_id,
                "sandbox/nomad-ch vm_index leak"
            );
            tracing::warn!(
                target: "sandbox::teardown::leak",
                job = %sandbox.job_id,
                vm_index = sandbox.vm_index,
                "sandbox/nomad-ch stop: wait_for_job_gone failed; leaking vm_index to avoid tap collision (orphan-prune will reclaim on next boot)"
            );
        } else {
            // job_confirmed_gone but fence_err is Some.
            // C-7-LT-2-PR2 defense-in-depth: bump the per-reason leak
            // counter so smoke-r14 has a quantitative signal even when
            // logs are sampled. A healthy cluster's rate(…{
            // reason="host_fence_timeout"}) is near zero post-PR1.
            crate::metrics::inc_vm_index_leak("host_fence_timeout");
            tracing::warn!(
                target: "sandbox::teardown::leak",
                vm_index = sandbox.vm_index,
                reason = "host_fence_timeout",
                sandbox_id = %sandbox_id,
                job = %sandbox.job_id,
                "sandbox/nomad-ch vm_index leak"
            );
            tracing::warn!(
                target: "sandbox::teardown::leak",
                job = %sandbox.job_id,
                vm_index = sandbox.vm_index,
                error = %fence_err.as_deref().unwrap_or("<unknown>"),
                "sandbox/nomad-ch stop: host_fence timeout; leaking vm_index to avoid handing out a live IP (orphan-prune will reclaim on next boot)"
            );
        }

        // 5. host_dir — INTENTIONALLY NOT REMOVED.
        //
        //    T-8b-stress-r2 controller v34: per-alloc host_dir cleanup
        //    is the load-bearing race behind Bug 1 (`workspace.img does
        //    not exist`). See the doc-comment block at the top of this
        //    file ("Cleanup contract") and the matching block in
        //    CreateGuard::drop's step-3 comment for the full diagnosis.
        //
        //    New invariant: host_dir is reaped EXCLUSIVELY by the
        //    sweeper task (`crate::sweep::spawn_host_dir_gc`). Per-alloc
        //    paths — CreateGuard rollback, stop_inner, the
        //    restore-failure tail — leak the host_dir deliberately so
        //    a concurrent retry's StartTask never observes a missing
        //    workspace.img. The sweeper's 1-hour grace + terminal-state
        //    + no-pending-wake-jobs gate ensures we don't reap data
        //    out from under an in-flight retry or a wake.
        //
        //    `remove_host_dir == false` (snapshot-aware teardown, bug
        //    #15) was the original gate; under v34 both branches behave
        //    the same way w.r.t. host_dir — the variable now only gates
        //    `persist.delete` below. Logging the difference so an
        //    operator can still distinguish the two stop_inner shapes
        //    in journalctl.
        if !remove_host_dir {
            tracing::info!(
                sandbox_id = %sandbox_id,
                job = %sandbox.job_id,
                host_dir = %sandbox.host_dir.display(),
                "sandbox/nomad-ch stop_preserving_state: leaking host_dir (sweeper-owned, snapshot-aware teardown)"
            );
        } else {
            tracing::info!(
                sandbox_id = %sandbox_id,
                job = %sandbox.job_id,
                host_dir = %sandbox.host_dir.display(),
                job_confirmed_gone,
                fence_passed,
                "sandbox/nomad-ch stop: leaking host_dir (sweeper-owned; v34 invariant)"
            );
        }

        // Delete the sealed record (preview-URL § II.0 §4). BEST-EFFORT:
        // a delete failure is logged but does NOT fail stop(). The next
        // boot's restore loop probes the sandbox's `/version`, finds it
        // unreachable (the VM is gone), and leaves the file in place
        // for periodic prune (Phase 5) to mop up.
        //
        // Gated by `remove_host_dir` for symmetry with step 5 above:
        // when the caller is the snapshot-aware teardown
        // (`stop_preserving_state` → `remove_host_dir == false`) the
        // sandbox is being put to sleep, not killed — `workspace.img`
        // survives on disk and so MUST the sealed record carrying the
        // signing key the next wake needs to talk to the restored
        // agent. Wiping it here is the C2 latent bug
        // (`docs/reviews/sandbox-snapshot-restore-deferred.md`): it
        // bites the moment wake plumbs sealed-record-based key
        // recovery. Today's wake path doesn't (yet) read the sealed
        // record, but the contract is "preserve everything across the
        // snapshot → wake gap" — host_dir and sealed record alike.
        if remove_host_dir {
            if let Some(persist) = &self.persist {
                if let Err(e) = persist.delete(sandbox_id).await {
                    tracing::warn!(
                        sandbox_id = %sandbox_id,
                        error = %e,
                        "sandbox/nomad-ch persist.delete failed (non-fatal; sealed record will be cleaned by next-boot unreachable-probe + Phase-5 prune)"
                    );
                }
            }
        } else {
            tracing::info!(
                sandbox_id = %sandbox_id,
                job = %sandbox.job_id,
                "sandbox/nomad-ch stop_preserving_state: skipping persist.delete (snapshot-aware teardown; sealed record must survive to wake)"
            );
        }

        tracing::info!(
            sandbox_id = %sandbox_id,
            vm_index = sandbox.vm_index,
            job = %sandbox.job_id,
            errs = errs.len(),
            job_confirmed_gone,
            fence_passed,
            elapsed_ms = %stop_started.elapsed().as_millis(),
            "sandbox/nomad-ch stop: complete"
        );
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs.join("; "))
        }
    }

    pub async fn exec(
        &self,
        sandbox_id: Uuid,
        cmd: &str,
        cwd: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> Result<ExecOutput, String> {
        let (sk, url) = self.sandbox_keys(sandbox_id)?;
        let ctx = self.sandbox_log_ctx(sandbox_id);
        let body = serde_json::json!({
            "cmd": cmd,
            "cwd": cwd,
            "timeout_ms": timeout_ms,
        })
        .to_string();
        let resp = http_signed_async(&sk, "POST", &format!("{url}/exec"), body.as_bytes())
            .await
            .map_err(|e| format!("{ctx} agent /exec: {e}"))?;
        if resp.status != 200 {
            log_agent_error(sandbox_id, "exec", resp.status, &resp.body);
            return Err(format!(
                "{ctx} agent /exec status {}: {}",
                resp.status, resp.body
            ));
        }
        let v: serde_json::Value = serde_json::from_str(&resp.body)
            .map_err(|e| format!("{ctx} agent /exec response not JSON: {e}"))?;
        Ok(ExecOutput {
            // try_into instead of `as i32` — a status outside i32
            // range is almost certainly garbage from a buggy agent;
            // falling back to -1 is no worse than the previous
            // wrap-on-cast and avoids signed-overflow surprises.
            status: v["status"]
                .as_i64()
                .unwrap_or(-1)
                .try_into()
                .unwrap_or(-1),
            stdout: v["stdout"].as_str().unwrap_or("").to_string(),
            stderr: v["stderr"].as_str().unwrap_or("").to_string(),
            timed_out: v["timed_out"].as_bool().unwrap_or(false),
        })
    }

    pub async fn read_file(&self, sandbox_id: Uuid, path: &str) -> Result<Vec<u8>, String> {
        let (sk, url) = self.sandbox_keys(sandbox_id)?;
        let ctx = self.sandbox_log_ctx(sandbox_id);
        let p = sanitize_path(path)?;
        let resp = http_signed_async(&sk, "GET", &format!("{url}/files/{p}"), &[])
            .await
            .map_err(|e| format!("{ctx} agent /files GET: {e}"))?;
        if resp.status == 404 {
            return Err(format!("{ctx} file not found: {p}"));
        }
        if resp.status != 200 {
            log_agent_error(sandbox_id, "files.get", resp.status, &resp.body);
            return Err(format!(
                "{ctx} agent /files GET status {}: {}",
                resp.status, resp.body
            ));
        }
        Ok(resp.bytes)
    }

    pub async fn write_file(
        &self,
        sandbox_id: Uuid,
        path: &str,
        body: &[u8],
    ) -> Result<(), String> {
        let (sk, url) = self.sandbox_keys(sandbox_id)?;
        let ctx = self.sandbox_log_ctx(sandbox_id);
        let p = sanitize_path(path)?;
        let resp = http_signed_async(&sk, "PUT", &format!("{url}/files/{p}"), body)
            .await
            .map_err(|e| format!("{ctx} agent /files PUT: {e}"))?;
        if resp.status != 200 {
            log_agent_error(sandbox_id, "files.put", resp.status, &resp.body);
            return Err(format!(
                "{ctx} agent /files PUT status {}: {}",
                resp.status, resp.body
            ));
        }
        Ok(())
    }

    pub async fn delete_file(&self, sandbox_id: Uuid, path: &str) -> Result<bool, String> {
        let (sk, url) = self.sandbox_keys(sandbox_id)?;
        let ctx = self.sandbox_log_ctx(sandbox_id);
        let p = sanitize_path(path)?;
        let resp = http_signed_async(&sk, "DELETE", &format!("{url}/files/{p}"), &[])
            .await
            .map_err(|e| format!("{ctx} agent /files DELETE: {e}"))?;
        match resp.status {
            200 => Ok(true),
            404 => Ok(false),
            s => {
                log_agent_error(sandbox_id, "files.delete", s, &resp.body);
                Err(format!(
                    "{ctx} agent /files DELETE status {s}: {}",
                    resp.body
                ))
            }
        }
    }

    pub async fn file_tree(&self, sandbox_id: Uuid) -> Result<Vec<TreeEntry>, String> {
        let (sk, url) = self.sandbox_keys(sandbox_id)?;
        let ctx = self.sandbox_log_ctx(sandbox_id);
        let resp = http_signed_async(&sk, "GET", &format!("{url}/tree"), &[])
            .await
            .map_err(|e| format!("{ctx} agent /tree: {e}"))?;
        if resp.status != 200 {
            log_agent_error(sandbox_id, "tree", resp.status, &resp.body);
            return Err(format!(
                "{ctx} agent /tree status {}: {}",
                resp.status, resp.body
            ));
        }
        let v: serde_json::Value = serde_json::from_str(&resp.body)
            .map_err(|e| format!("{ctx} agent /tree response not JSON: {e}"))?;
        let entries = v["entries"]
            .as_array()
            .ok_or_else(|| format!("{ctx} agent /tree: missing 'entries' array"))?;
        Ok(entries
            .iter()
            .filter_map(|e| {
                let path = e["path"].as_str()?.to_string();
                let kind = if e["is_dir"].as_bool().unwrap_or(false) {
                    "dir"
                } else {
                    "file"
                };
                let size = e["size"].as_u64().unwrap_or(0);
                Some(TreeEntry { path, kind, size })
            })
            .collect())
    }

    fn sandbox_keys(&self, id: Uuid) -> Result<(Arc<SigningKey>, String), String> {
        let guard = self.state.read().unwrap_or_else(|p| p.into_inner());
        let s = guard
            .get(&id)
            .ok_or_else(|| "sandbox not found in nomad-ch backend".to_string())?;
        // Arc clone is a refcount bump — cheap. Cloning the SigningKey
        // by value would heap-copy the 32-byte secret on every signed
        // RPC, doubling the in-memory key count for the duration of
        // the request (ed25519-dalek::SigningKey doesn't zeroize on
        // drop).
        Ok((s.signing_key.clone(), s.agent_url.clone()))
    }

    /// Lift the per-sandbox auth material into a backend-agnostic
    /// envelope. See `super::SandboxAuth` for the contract.
    pub async fn session_auth(
        &self,
        sandbox_id: Uuid,
    ) -> Result<super::SandboxAuth, String> {
        let guard = self.state.read().unwrap_or_else(|p| p.into_inner());
        let s = guard
            .get(&sandbox_id)
            .ok_or_else(|| "sandbox not found in nomad-ch backend".to_string())?;
        let pubkey_fp = sig::pubkey_fingerprint(&s.signing_key.verifying_key());
        Ok(super::SandboxAuth {
            signing_key: s.signing_key.clone(),
            agent_url: s.agent_url.clone(),
            pubkey_fp,
        })
    }

    /// Persist-on-mint helper: rebuild the on-disk `SealedAuth` for
    /// `sandbox_id` carrying the caller-provided preview-share state
    /// and seal it. See [`super::Backend::seal_with_preview_state`]
    /// for the contract; this implementation reads the per-sandbox
    /// `signing_key` + `vm_index` from the backend's own session map
    /// and pulls `user_id` / `project_id` / `created_at_secs` from
    /// the supplied [`super::SandboxInfo`] (the registry-side view).
    ///
    /// Nomad-CH records seal `agent_url = None` (it's deterministic
    /// from `vm_index` at restore time; round-6 I3) — same as
    /// [`Self::create`] writes at sandbox-create time.
    pub async fn seal_with_preview_state(
        &self,
        sandbox_id: Uuid,
        info: &super::SandboxInfo,
        secrets: Option<crate::persist::SealedPreviewSecrets>,
        audit: Vec<crate::persist::SealedAuditEntry>,
    ) -> Result<bool, String> {
        let Some(persist) = self.persist.clone() else {
            return Ok(false);
        };
        let _ = (info, audit); // round-8: legacy fields no longer sealed
        let sk_bytes = {
            let guard = self.state.read().unwrap_or_else(|p| p.into_inner());
            let Some(s) = guard.get(&sandbox_id) else {
                return Ok(false);
            };
            s.signing_key.to_bytes()
        };
        // v3: secret material only. Pg holds info.user_id /
        // project_id / vm_index / created_at_secs; the share-token
        // audit is a sandbox.shares row.
        let record = crate::persist::SealedAuth {
            version: crate::persist::SEAL_VERSION,
            sandbox_id: sandbox_id.to_string(),
            signing_key_bytes: sk_bytes,
            preview_secrets: secrets,
            boot_id: None,
        };
        persist
            .seal(sandbox_id, &record)
            .await
            .map(|()| true)
            .map_err(|e| format!("seal failed: {e}"))
    }

    /// **Test-only.** Inject a synthetic sandbox record with a
    /// caller-supplied `agent_url`. Bypasses the full Nomad/CH
    /// create flow + the `derive_agent_url` rule (which targets
    /// `10.99.<100+idx>.2`). Used by the preview-proxy e2e tests
    /// to point the controller at a fixture HTTP listener on
    /// `127.0.0.1:<ephemeral>`.
    ///
    /// Gated under `#[cfg(any(test, feature = "test-support"))]`
    /// so the symbol is stripped from production binaries (R27-API2
    /// close-out, mirrors the `freed_for_test` precedent at :399).
    /// Integration tests in `tests/` link as external crates and
    /// pick the function up via the `test-support` feature, which
    /// the in-crate self dev-dep enables automatically (see
    /// `Cargo.toml [dev-dependencies] zeroship-sandbox`).
    #[cfg(any(test, feature = "test-support"))]
    pub fn _test_inject_sandbox(
        &self,
        sandbox_id: Uuid,
        user_id: &str,
        signing_key: SigningKey,
        agent_url: String,
        vm_index: u16,
    ) {
        let job_id = Self::derive_job_id(sandbox_id);
        let host_dir = self.derive_host_dir(sandbox_id);
        let mut g = self.state.write().unwrap_or_else(|p| p.into_inner());
        g.insert(
            sandbox_id,
            NomadChSandbox {
                user_id: user_id.to_string(),
                job_id,
                vm_index,
                host_dir,
                agent_url,
                signing_key: Arc::new(signing_key),
            },
        );
    }

    /// Re-derive the deterministic `agent_url` for a given vm_index.
    /// `http://10.<subnet_second_octet>.<100+idx>.2:7777`. Public so
    /// the controller's restart-restore path can recompute the URL
    /// from a sealed record's `vm_index` without re-running create().
    pub fn derive_agent_url(&self, vm_index: u16) -> String {
        format!(
            "http://10.{}.{}.2:{AGENT_PORT}",
            self.cfg.nomad_ch.subnet_second_octet,
            100u16 + vm_index
        )
    }

    /// Re-derive the per-sandbox host directory: same layout the
    /// `create()` path writes (`<host_state_dir>/<sandbox-id>/`).
    fn derive_host_dir(&self, sandbox_id: Uuid) -> PathBuf {
        self.cfg.nomad_ch.host_state_dir.join(sandbox_id.to_string())
    }

    /// Re-derive the deterministic Nomad job-id format used by the
    /// create path: `zsbx-<sandbox-id-simple>`. Kept private (the
    /// boot-restore code below is the sole caller); callers outside
    /// the backend always look the job up by its sandbox-id key.
    fn derive_job_id(sandbox_id: Uuid) -> String {
        format!("zsbx-{}", sandbox_id.simple())
    }

    /// Restart-restore: re-install in-memory state for a sandbox the
    /// controller minted before its previous lifetime ended. Caller
    /// (the boot-path in `lib.rs`) is expected to have already
    /// (a) read the sealed record from disk, (b) signed-`/version`
    /// probed the agent, and (c) confirmed the agent's reported
    /// `pubkey_fingerprint` byte-matches the sealed `pubkey_fp`.
    ///
    /// On success the backend's per-sandbox HashMap holds the same
    /// shape as a fresh `create()` would have produced; `exec` /
    /// `read_file` / etc. all dispatch normally. The vm_index is
    /// reserved in the allocator so a concurrent fresh `create()`
    /// can't hand the same tap subnet to a different tenant.
    ///
    /// Returns `Err` if the sealed record lacks `vm_index` (a
    /// schema violation for a `backend = "nomad-ch"` record), if
    /// the index is outside the configured pool, or if the
    /// in-memory map already has an entry for `sandbox_id`.
    /// Round-8 Phase-1 restore. Pg row is canonical for `user_id`,
    /// `backend`, `vm_index`, `agent_url`, `key_fp`; sealed record is
    /// canonical for `signing_key_bytes`. Boot loop has already
    /// signed-`/version` probed the agent before calling this.
    pub async fn restore_from_pg_and_sealed(
        &self,
        sandbox_id: Uuid,
        row: &crate::db::SandboxRow,
        sealed: &crate::persist::SealedAuth,
        agent_url: String,
    ) -> Result<super::SandboxAuth, String> {
        if row.backend != "nomad-ch" {
            return Err(format!(
                "restore: backend mismatch (pg row says {:?}, this backend is nomad-ch)",
                row.backend
            ));
        }
        let vm_index_i32 = row.vm_index.ok_or_else(|| {
            "restore: pg row for nomad-ch backend has no vm_index".to_string()
        })?;
        let vm_index = u16::try_from(vm_index_i32)
            .map_err(|e| format!("restore: vm_index out of u16 range: {e}"))?;
        // Reserve the index BEFORE inserting state.
        self.vm_index_allocator
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .reserve(vm_index)
            .map_err(|e| format!("restore: vm_index reserve: {e}"))?;

        let signing_key = Arc::new(SigningKey::from_bytes(&sealed.signing_key_bytes));
        let pubkey_fp = sig::pubkey_fingerprint(&signing_key.verifying_key());
        if pubkey_fp != row.key_fp {
            return Err(format!(
                "restore: derived pubkey_fp ({pubkey_fp}) != pg key_fp ({})",
                row.key_fp
            ));
        }
        let host_dir = self.derive_host_dir(sandbox_id);
        let job_id = Self::derive_job_id(sandbox_id);

        {
            let mut g = self.state.write().unwrap_or_else(|p| p.into_inner());
            if g.contains_key(&sandbox_id) {
                return Err(format!(
                    "restore: sandbox {sandbox_id} already present in nomad-ch state"
                ));
            }
            g.insert(
                sandbox_id,
                NomadChSandbox {
                    user_id: row.user_id.clone(),
                    job_id,
                    vm_index,
                    host_dir,
                    agent_url: agent_url.clone(),
                    signing_key: signing_key.clone(),
                },
            );
        }

        Ok(super::SandboxAuth {
            signing_key,
            agent_url,
            pubkey_fp,
        })
    }

    /// B19 fix (cluster smoke 2026-05-23 r4): install a restored
    /// sandbox into the in-memory state map so `exec`/`stop`/
    /// `delete`/`sandbox_keys` look-ups succeed after a wake. Mirrors
    /// the state-map insert at the tail of [`Self::create`] +
    /// [`Self::restore_from_pg_and_sealed`], but does NOT touch the
    /// vm_index allocator (the restore handler already reserved the
    /// slot before this is called) and does NOT issue any I/O — the
    /// caller (`restore_handler::do_restore_inner` after
    /// `wait_for_livez` Ok) has already brought the VM live and
    /// unsealed the signing key.
    ///
    /// Returns `Err` if `sandbox_id` is already present in the state
    /// map: the contract is "freshly restored entry", not "overwrite a
    /// live one". The caller maps the error to a 500.
    ///
    /// **What this fixes**: pre-B19, `do_restore_inner` returned Ok
    /// after `wait_for_livez` but never inserted the per-sandbox
    /// record. Subsequent `exec` returned 500 "sandbox not found in
    /// nomad-ch backend", `stop`/`delete` returned 404, and the
    /// vm_index slot leaked across controller uptime (the
    /// stop_inner's idempotent-Ok branch fired without releasing the
    /// allocator). After ~10 successful wakes the allocator
    /// exhausted (floor=1, ceil=12) and blocked new creates.
    pub(crate) fn register_restored(
        &self,
        sandbox_id: Uuid,
        vm_index: u16,
        signing_key_bytes: [u8; 32],
        agent_url: String,
        user_id: String,
    ) -> Result<(), String> {
        let signing_key = Arc::new(SigningKey::from_bytes(&signing_key_bytes));
        let host_dir = self.derive_host_dir(sandbox_id);
        let job_id = Self::derive_job_id(sandbox_id);

        let mut g = self.state.write().unwrap_or_else(|p| p.into_inner());
        use std::collections::hash_map::Entry;
        match g.entry(sandbox_id) {
            Entry::Vacant(slot) => {
                slot.insert(NomadChSandbox {
                    user_id,
                    job_id,
                    vm_index,
                    host_dir,
                    agent_url,
                    signing_key,
                });
                Ok(())
            }
            Entry::Occupied(_) => Err(format!(
                "register_restored: sandbox {sandbox_id} already present in \
                 nomad-ch state map (would clobber live record); refusing"
            )),
        }
    }

    /// **R10-C1 fix (concurrency-r10 2026-05-25)**: the symmetric
    /// inverse of [`Self::register_restored`]. Removes the state-map
    /// entry the restore-success branch inserted. Called from
    /// `RealRestoreBackend::teardown_restore` on the rollback path so
    /// that, after a late failure (e.g. `update_sandbox_status(Running)`
    /// returning CasLost), the vm_index release is matched by a
    /// state-map remove — preventing a ghost entry at the released slot
    /// that the next `create` would land on top of.
    ///
    /// Mirrors [`Self::stop_inner`]'s `state.write().remove(&sandbox_id)`
    /// pattern (the idempotent-on-missing case). Returns `true` if an
    /// entry was actually removed, `false` if there was nothing to
    /// remove (early-rollback before `register_restored` ever ran — the
    /// no-op branch matches stop_inner's tolerance).
    pub(crate) fn unregister_restored(&self, sandbox_id: Uuid) -> bool {
        self.state
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&sandbox_id)
            .is_some()
    }

    /// Test-only helper: returns `true` iff the state map holds an
    /// entry for `sandbox_id`. Used by the cross-module R10-C1
    /// integration test in `restore_handler.rs` that needs to peek at
    /// the state map after a `teardown_restore`. Kept `pub(crate)` so
    /// it can't leak to out-of-crate callers.
    #[cfg(test)]
    pub(crate) fn contains_for_test(&self, sandbox_id: Uuid) -> bool {
        self.state
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(&sandbox_id)
    }

    /// Legacy v2-shape restore. Round-8 keeps this so existing tests
    /// in `tests/sandbox_persist_e2e.rs` still compile; new code goes
    /// through [`Self::restore_from_pg_and_sealed`].
    #[allow(dead_code)]
    pub async fn restore_from_sealed(
        &self,
        _sandbox_id: Uuid,
        _sealed: &crate::persist::SealedAuth,
    ) -> Result<super::SandboxAuth, String> {
        Err("restore_from_sealed: round-8 deprecated — use restore_from_pg_and_sealed".into())
    }

    /// Phase B: resolve the source VM's `(api_socket, vm_index, alloc_dir)`
    /// for a sandbox the snapshot handler is about to pause+snapshot.
    ///
    /// Returns `Err` when:
    ///   - The sandbox is unknown to this controller (lease-takeover
    ///     orphan, or peer-owned).
    ///   - Nomad is unreachable / returns no running alloc (the VM has
    ///     already terminated).
    ///   - The derived `ch.sock` path is not a Unix socket on local fs
    ///     (the alloc is on a different worker — single-controller-per-
    ///     -worker model means we cannot reach a remote socket).
    ///
    /// Caller (admin_handlers::snapshot_sandbox) maps the Err to a 503
    /// CAS-rollback so the row stays `running` and operators can retry.
    pub async fn lookup_source_vm_ops(
        &self,
        sandbox_id: Uuid,
    ) -> Result<SourceVmOpsHandle, String> {
        // 1. Look up the in-memory sandbox record. We need the `job_id`
        //    (to query Nomad for allocs) and the `vm_index` (which is
        //    the same field the snapshot handler stamps onto the row).
        let (job_id, vm_index) = {
            let guard = self.state.read().unwrap_or_else(|p| p.into_inner());
            let s = guard.get(&sandbox_id).ok_or_else(|| {
                format!("lookup_source_vm_ops: sandbox {sandbox_id} not in nomad-ch state map")
            })?;
            (s.job_id.clone(), s.vm_index)
        };

        // 2. Query Nomad for the running alloc on this job. We mirror
        //    the shape of `wait_for_alloc_running` but only need a
        //    single-shot read — the snapshot handler is invoked while
        //    the row is `running`, so by definition there's at least
        //    one alloc and it's already past the `running` ClientStatus.
        let url = format!(
            "{}/v1/job/{}/allocations",
            self.cfg.nomad_ch.nomad_addr, job_id
        );
        let resp = http_get_unsigned(&url, Duration::from_secs(5))
            .await
            .map_err(|e| {
                format!("lookup_source_vm_ops: nomad GET {url}: {e}")
            })?;
        if resp.status != 200 {
            return Err(format!(
                "lookup_source_vm_ops: nomad GET {url} → status {}: {}",
                resp.status,
                resp.body.trim()
            ));
        }
        let allocs: serde_json::Value = serde_json::from_str(&resp.body)
            .map_err(|e| {
                format!("lookup_source_vm_ops: parse alloc list: {e}")
            })?;
        let mut alloc_id: Option<String> = None;
        for a in allocs.as_array().into_iter().flatten() {
            let cs = a["ClientStatus"].as_str().unwrap_or("");
            if cs == "running" {
                if let Some(id) = a["ID"].as_str() {
                    alloc_id = Some(id.to_string());
                    break;
                }
            }
        }
        let alloc_id = alloc_id.ok_or_else(|| {
            format!(
                "lookup_source_vm_ops: no running alloc found for job {job_id} \
                 (VM may have already terminated)"
            )
        })?;

        // 3. Derive alloc_dir + api_socket. Same convention used by
        //    Phase 3's stop path and the wrapper's `ZSBX_RUNTIME =
        //    ${NOMAD_TASK_DIR}` expansion: the task is named "ch", so
        //    the per-task dir is `<alloc_dir>/ch/local/`, and the
        //    wrapper writes its API socket as `${ZSBX_RUNTIME}/ch.sock`.
        let alloc_dir = PathBuf::from(NOMAD_ALLOC_ROOT).join(&alloc_id);
        let api_socket = alloc_dir.join("ch").join("local").join("ch.sock");

        // 4. Verify the socket exists locally. The single-controller-
        //    per-worker model puts the alloc on the same host as us;
        //    a missing socket means either (a) the alloc is on a
        //    different worker (peer-owned via lease-takeover), or
        //    (b) the wrapper has already torn down. Either case is
        //    a snapshot-impossible signal.
        match std::fs::metadata(&api_socket) {
            Ok(md) => {
                if !md.file_type().is_socket() {
                    return Err(format!(
                        "lookup_source_vm_ops: {} exists but is not a unix socket",
                        api_socket.display()
                    ));
                }
            }
            Err(e) => {
                return Err(format!(
                    "lookup_source_vm_ops: api_socket {} not accessible: {e}",
                    api_socket.display()
                ));
            }
        }

        Ok(SourceVmOpsHandle {
            api_socket,
            vm_index,
            alloc_dir,
        })
    }

    /// Build a short prefix for agent-error log lines so a fleet-
    /// wide log search can pivot on sandbox / vm_index / job (M1).
    /// Format: `[sandbox=<id> vm_index=<idx> job=<job>]`. Returns
    /// just `[sandbox=<id>]` when the entry is missing — the
    /// callers already handle "sandbox not found" via
    /// `sandbox_keys`, so this only fires on the wide-window after
    /// a stop() racing with an in-flight RPC.
    fn sandbox_log_ctx(&self, id: Uuid) -> String {
        let guard = self.state.read().unwrap_or_else(|p| p.into_inner());
        match guard.get(&id) {
            Some(s) => format!(
                "[sandbox={} vm_index={} job={}]",
                id, s.vm_index, s.job_id
            ),
            None => format!("[sandbox={id}]"),
        }
    }
}

// ─── create-time bookkeeping ────────────────────────────────────

/// RAII for the `creating_users` set. Same poison-recovery pattern
/// as the K8s backend: a panic that poisons the mutex would otherwise
/// lock the user out forever.
struct ReleaseCreating {
    set: Arc<Mutex<HashSet<String>>>,
    user_id: String,
}

impl ReleaseCreating {
    fn new(set: Arc<Mutex<HashSet<String>>>, user_id: String) -> Self {
        Self { set, user_id }
    }
}

impl Drop for ReleaseCreating {
    fn drop(&mut self) {
        let mut g = self.set.lock().unwrap_or_else(|p| p.into_inner());
        g.remove(&self.user_id);
    }
}

/// Tracks partial state during `create` so the failure tail can
/// undo whatever the success tail had already done. Drop runs when
/// `armed == true`; `disarm()` flips it on full success. The
/// per-step booleans (`host_dir_created`, `job_submitted`) gate
/// their respective cleanup branches so we don't, e.g., DELETE a
/// job that was never submitted.
///
/// **Drop is sync, but Drop must NOT block the compio worker.** When
/// a panic during `wait_for_alloc_running` (mid-`.await`) triggers
/// stack unwind, this `drop` runs *on the compio worker thread* —
/// blocking it on a 10-second `ureq::delete().call()` is the exact
/// stall the codebase is allergic to. Instead we hand the cleanup
/// I/O off to a detached compio task: it owns its own data, runs
/// best-effort, and the worker thread is freed immediately.
///
/// **vm_index ordering (C1).** The vm_index is released ONLY after
/// the Nomad purge HTTP call confirms (status 200/404). Mirrors the
/// `stop` path's policy: a follow-up `create` for the same user
/// could otherwise reuse the index and race the still-alive prior
/// `raw_exec` wrapper for `tap=zsbx-nm-<idx>`. Up to ~10s elapse
/// between "Drop fires" and "purge confirms"; releasing the index
/// inline (as a previous revision did) reopened the same window the
/// `stop`-path I1 fix was guarding against. On purge failure (5xx,
/// timeout) the index is leaked; `cleanup_orphans_at_startup` (or
/// the next-boot orphan prune) reclaims it indirectly by deleting
/// the surviving job.
///
/// **Isolation (R17-A5, C-6 family).** Dispatch goes through
/// [`crate::detach::detach_isolated`] — a dedicated OS thread with a
/// private compio runtime — so the cleanup tail (Nomad purge, host_dir
/// rm -rf, vm_index release) never lands on the shared ntex-worker
/// compio runtime where subsequent HTTP requests / wake-machine ticks
/// run. Without this isolation, a stuck 10s `http_delete_unsigned`
/// against a half-dead Nomad would block sibling tasks on the worker
/// runtime; the symptom would be identical to the C-3/C-6 fingerprint
/// the production sites already migrated. Risk here is lower because
/// the create failed → no live agent racing concurrent work — but the
/// detach pattern is uniform across the crate now.
///
/// `detach_isolated` mints its own runtime; the only failure mode is
/// OS-thread spawn failure (ENOMEM/EAGAIN), which the helper logs at
/// `tracing::error!`. There is no "runtime-down" branch to fall back
/// to — the next-boot orphan prune is the recovery path, same as it
/// was for the prior `compio::runtime::spawn` panic fallback.
struct CreateGuard {
    vm_index_allocator: Arc<Mutex<VmIndexAllocator>>,
    nomad_addr: String,
    job_id: String,
    host_dir: PathBuf,
    /// FM-B': sandbox_id is captured at guard construction so the
    /// detached cleanup task can tag its log line with it. Without
    /// this, an operator scanning logs for `sandbox=<uuid>` saw the
    /// `create: error step=…` line but no matching vm_index release,
    /// making a failed-create cleanup look like a leak.
    sandbox_id: Uuid,
    pub vm_index: Option<u16>,
    pub host_dir_created: bool,
    pub job_submitted: bool,
    /// T-8b-stress-r8 r24-A2-S3: the configured delay before
    /// releasing `vm_index` back to the allocator. Captured at
    /// guard construction so the detached drop task doesn't need
    /// to re-read the cfg; production default 5 s, 0 in tests.
    release_delay: Duration,
    armed: bool,
}

impl CreateGuard {
    fn new(
        vm_index_allocator: Arc<Mutex<VmIndexAllocator>>,
        nomad_addr: String,
        job_id: String,
        host_dir: PathBuf,
        sandbox_id: Uuid,
        release_delay: Duration,
    ) -> Self {
        Self {
            vm_index_allocator,
            nomad_addr,
            job_id,
            host_dir,
            sandbox_id,
            vm_index: None,
            host_dir_created: false,
            job_submitted: false,
            release_delay,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CreateGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Move ALL cleanup state into the detached task. The
        // vm_index release is intentionally part of the detached
        // task too — see the C1 note on the type. Releasing the
        // index inline (before the Nomad purge confirms) is a race
        // window: a retry-`create` for the same user could grab the
        // same index and bind a tap device the still-alive prior
        // wrapper is using.
        let job_submitted = self.job_submitted;
        let nomad_addr = std::mem::take(&mut self.nomad_addr);
        let job_id = std::mem::take(&mut self.job_id);
        let host_dir_created = self.host_dir_created;
        let host_dir = std::mem::take(&mut self.host_dir);
        let vm_index_allocator = self.vm_index_allocator.clone();
        let vm_index_opt = self.vm_index.take();
        let sandbox_id = self.sandbox_id;
        let release_delay = self.release_delay;

        // R17-A5: dispatch through `detach_isolated` so the cleanup
        // tail runs on a dedicated OS thread with its own private
        // compio runtime — never on the shared ntex-worker runtime
        // where wake/heartbeat work also runs. Thread name
        // `create-rollbk` (13 bytes) fits under Linux's 15-byte
        // `pr_set_name` limit (TASK_COMM_LEN-1). `detach_isolated`
        // already logs OS-thread spawn failures internally; no
        // caller-side fallback path is needed (the next-boot orphan
        // prune is the recovery, same as it was for the prior
        // `compio::runtime::spawn` panic-catch fallback).
        crate::detach::detach_isolated("create-rollbk", move || async move {
            // Local equivalent of the runtime crate's
            // `panic_util::guard` (which is pub(crate) to
            // `zeroship-runtime` and not reachable from here
            // without taking a heavy crate-graph edge). Wraps
            // the body in `catch_unwind` so a panic inside the
            // detached task doesn't get swallowed silently.
            guard_detached("nomad_ch_create_guard_cleanup", async move {
                // 1. Nomad purge — gates the vm_index release.
                let purge_ok = if job_submitted {
                    let url = format!("{nomad_addr}/v1/job/{job_id}?purge=true");
                    match http_delete_unsigned(&url, Duration::from_secs(10)).await {
                        Ok(r) if r.status == 200 || r.status == 404 => true,
                        Ok(r) => {
                            tracing::warn!(
                                job = %job_id,
                                status = r.status,
                                body = %r.body.trim(),
                                "sandbox/nomad-ch guard cleanup: purge non-2xx (best-effort; vm_index will be leaked)"
                            );
                            false
                        }
                        Err(e) => {
                            tracing::warn!(
                                job = %job_id,
                                error = %e,
                                "sandbox/nomad-ch guard cleanup: purge failed (best-effort; vm_index will be leaked)"
                            );
                            false
                        }
                    }
                } else {
                    // Job was never submitted, so there's nothing on
                    // the Nomad side to race; the vm_index is safe
                    // to release immediately.
                    true
                };

                // 2. vm_index release — only on confirmed purge.
                //    Same policy as the `stop` path (lines 644-649
                //    of the file's stable doc-comment).
                //
                //    FM-B': mirror the `stop()` path's
                //    `vm_index: release=<n>` log line so an operator
                //    scanning for "where did the index go?" finds
                //    the cleanup-tail event. Tagged
                //    `reason=create-failure-cleanup` so it's
                //    distinguishable from the normal stop() path;
                //    previously the detached task did the release
                //    silently and a failed create looked like a
                //    leak.
                if purge_ok {
                    if let Some(i) = vm_index_opt {
                        // T-8b-stress-r8 r24-A2-S3: delay release
                        // same as the stop path so a retry-CREATE
                        // doesn't pick up an index whose kernel
                        // state is still being torn down. Tagged
                        // `reason=create-failure-cleanup` to keep
                        // FM-B's failed-create attribution.
                        //
                        // r29-A2 (R28-C1 + R29-C1 class-fix): use
                        // the inline-await helper so the timer is
                        // bound to THIS task, not detached onto the
                        // short-lived private compio runtime minted
                        // by `detach_isolated("create-rollbk", …)`.
                        // Detaching here would land the timer task
                        // on the same runtime the cleanup future is
                        // on — when `block_on` returns Ready and the
                        // private runtime drops, `Scheduler::clear`
                        // discards the pending timer, leaking the
                        // vm_index until the next controller-boot
                        // orphan prune. See `release_vm_index_after`
                        // rustdoc for the full r29-A2 history.
                        VmIndexAllocator::release_vm_index_after(
                            Arc::clone(&vm_index_allocator),
                            i,
                            release_delay,
                            "create-failure-cleanup",
                            sandbox_id,
                        )
                        .await;
                    }
                } else if let Some(i) = vm_index_opt {
                    tracing::warn!(
                        vm_index = i,
                        reason = "create-failure-cleanup-purge-failed",
                        sandbox_id = %sandbox_id,
                        job = %job_id,
                        "sandbox/nomad-ch vm_index leak (orphan-prune will reclaim on next boot)"
                    );
                }

                // 3. host_dir — INTENTIONALLY NOT REMOVED.
                //
                //    T-8b-stress-r2 controller v34: the per-alloc
                //    host_dir cleanup path is THE load-bearing race
                //    that caused Bug 1 (48/60 CREATEs failing with
                //    `workspace.img does not exist` — the failing
                //    alloc's DestroyTask `rm -rf`'d the dir while a
                //    concurrent retry's StartTask was still running).
                //    See the stress-r2 review file and the doc-comment
                //    block at the top of this file ("Cleanup contract")
                //    for the full diagnosis.
                //
                //    New invariant: host_dir is created on-demand by
                //    `create_ext4_image_if_missing` and reaped EXCLUSIVELY
                //    by the sweeper task (`crate::sweep::spawn_host_dir_gc`).
                //    Per-alloc paths — CreateGuard rollback, stop_inner,
                //    the restore-failure tail — all LEAK the host_dir
                //    deliberately. Retries that re-use the same
                //    sandbox_id see workspace.img still on disk (the
                //    `[ ! -f $WORKSPACE_IMG ]` cold-boot gate flips
                //    to "exists, skip mkfs"); retries with a fresh
                //    sandbox_id get a fresh host_dir mkdir'd by step
                //    3 of try_create. Either way, no race.
                //
                //    Sweeper grace: 1 hour after the sandbox transitions
                //    to a terminal state with no pending wake_jobs (cf.
                //    `crate::sweep::spawn_host_dir_gc`'s `GRACE_SECS`).
                //    Operators retain on-disk artefacts during the grace
                //    window for inspection.
                //
                //    Note: `host_dir_created` is still tracked above for
                //    diagnostic logging — a guard that never mkdir'd
                //    nothing is a different failure shape from one that
                //    successfully mkdir'd but failed on a later step.
                if host_dir_created {
                    tracing::info!(
                        host_dir = %host_dir.display(),
                        purge_ok,
                        sandbox_id = %sandbox_id,
                        "sandbox/nomad-ch guard cleanup: leaking host_dir (sweeper will GC after 1h grace + terminal state)"
                    );
                }
            })
            .await;
        });
    }
}

/// Local equivalent of `zeroship-runtime`'s `panic_util::guard` —
/// runs `fut` under `catch_unwind` so a panic inside a `.detach()`-ed
/// compio task gets a stderr log line instead of being silently
/// swallowed. The runtime crate's helper is `pub(crate)` to that
/// crate; rather than expose it cross-crate (which would force
/// `zeroship-sandbox` to take an edge on `zeroship-runtime` for one
/// helper) we keep a tiny local copy here.
async fn guard_detached<F, T>(site: &'static str, fut: F) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    use futures::FutureExt as _;
    use std::panic::AssertUnwindSafe;
    match AssertUnwindSafe(fut).catch_unwind().await {
        Ok(v) => Some(v),
        Err(p) => {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "<non-string panic>".to_string());
            tracing::error!(
                site,
                panic = %msg,
                "sandbox/nomad-ch panic in detached task"
            );
            None
        }
    }
}

/// Map fractional `SandboxConfig.cpus` (e.g. 2.0, 1.5) to the
/// integer `boot=N` count Cloud Hypervisor needs at command-line.
/// Round up — cfg.cpus is the *target* allocation; never starve the
/// VM by rounding down a 1.5 to 1. Floor at 1 so a misconfigured
/// cpus=0 (or NaN) still produces a bootable VM (the validate at
/// config load rejects cpus≤0, but defense-in-depth).
fn cpus_boot(cpus: f32) -> u32 {
    if !cpus.is_finite() {
        return 1;
    }
    let n = cpus.ceil() as i64;
    if n < 1 { 1 } else { n as u32 }
}

/// Nomad `Resources.CPU` advisory (MHz). Constant 500 MHz —
/// neither raw_exec nor Cloud Hypervisor enforce CPU quota, so this
/// number only feeds Nomad's bin-packing arithmetic. Scaling it with
/// `cfg.cpus` artificially capped placement at 20 VMs/worker on
/// n2-standard-32 (80,000 advertised MHz / 4,000) when the real
/// binding constraints are tap count (12/worker today) and memory.
/// Pinning at the floor lets bin-packing match physical limits.
/// 500 MHz is the smallest plausible non-zero value Nomad accepts.
pub(crate) const NOMAD_CPU_MHZ_ADVISORY: u32 = 500;

// ─── Nomad job spec construction ────────────────────────────────

/// Build the JSON body for `POST /v1/jobs`. Returns the `{"Job": ...}`
/// envelope ready to ship.
///
/// The shape is the absolute minimum that Nomad accepts for a
/// service-type job: TaskGroup count=1, RestartPolicy with 0 attempts
/// (the ch driver exits = the alloc dies; we don't want Nomad
/// to retry, the controller is the orchestrator), one Task using
/// `Driver: "ch"` with a typed `Config` block matching
/// `nomad-driver-ch/ch/task_config.go::TaskConfig`.
///
/// The Env block is populated alongside the typed Config for
/// debugging + the B24 / R8-DEPLOY1 regression pin on
/// `ZSBX_SANDBOX_ID`. The Go driver ignores Env.
///
/// `restore_from`, when `Some`, switches the per-task surface to the
/// restore branch: it sets the typed `RestoreFrom` Config field
/// (`task_config.go:73`). When `None`, cold-boot (the original
/// behaviour and the only path used by `Self::create` callers).
///
/// KillTimeout=10s is the same window the cleanup trap uses.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_nomad_job_json(
    job_id: &str,
    cfg: &SandboxConfig,
    vm_index: u16,
    workspace_img: &Path,
    user_home_img: &Path,
    pubkey_hex: &str,
    user_id: &str,
    project_id: &str,
    sandbox_id: &str,
    local_nomad_node_id: Option<&str>,
) -> serde_json::Value {
    build_nomad_job_json_with(
        job_id,
        cfg,
        vm_index,
        workspace_img,
        user_home_img,
        pubkey_hex,
        user_id,
        project_id,
        sandbox_id,
        None,
        local_nomad_node_id,
    )
}

/// Lower-level builder used by [`build_nomad_job_json`] and by tests
/// that want to pin a restore path without reaching for
/// `std::env::set_var`. Production code paths go through
/// `build_nomad_job_json`; this helper is `pub(crate)` to keep the
/// test surface clean.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_nomad_job_json_with(
    job_id: &str,
    cfg: &SandboxConfig,
    vm_index: u16,
    workspace_img: &Path,
    user_home_img: &Path,
    pubkey_hex: &str,
    user_id: &str,
    project_id: &str,
    sandbox_id: &str,
    restore_from: Option<&Path>,
    local_nomad_node_id: Option<&str>,
) -> serde_json::Value {
    // Per-VM env block. Largely redundant with the typed Config block
    // below, but kept for debugging + the B24 / R8-DEPLOY1 regression
    // pin on `ZSBX_SANDBOX_ID`. The Go driver ignores Env.
    //
    // virtio-blk pivot (bug #11): the three virtio-fs share dirs
    // are gone — we pass the two image paths + the controller pubkey
    // as hex. The driver attaches the images as /dev/vdb,/vdc and
    // injects the pubkey into the kernel cmdline.
    let mut env = serde_json::json!({
        "ZSBX_VM_INDEX": vm_index.to_string(),
        // Artifact directory holding kernel + rootfs. Renamed from
        // ZSBX_HERE in round 3 (M4) — the new name matches the Rust
        // struct field `runtime_dir`'s intent.
        "ZSBX_ARTIFACT_DIR": cfg.nomad_ch.runtime_dir.display().to_string(),
        // ZSBX_RUNTIME is the per-allocation working dir Nomad
        // provisions per task; the literal `${NOMAD_TASK_DIR}` here
        // is a Nomad template variable the agent expands at launch.
        "ZSBX_RUNTIME": "${NOMAD_TASK_DIR}",
        "ZSBX_WORKSPACE_IMG": workspace_img.display().to_string(),
        "ZSBX_USER_HOME_IMG": user_home_img.display().to_string(),
        "ZSBX_PUBKEY_HEX": pubkey_hex,
        // Memory / CPU — debugging parity with the typed Config block.
        "ZSBX_VM_MEMORY_MB": cfg.memory_mb.to_string(),
        "ZSBX_VM_CPUS_BOOT": cpus_boot(cfg.cpus).to_string(),
        // M6: pair the second octet with the controller-side
        // computation of `agent_url`. Both sides MUST read the
        // same value so the tap/IP the driver provisions matches
        // the IP the controller dials.
        "ZSBX_SUBNET_BASE_OCTET":
            cfg.nomad_ch.subnet_second_octet.to_string(),
        // B24 / R8-DEPLOY1 regression pin: ZSBX_SANDBOX_ID must be
        // present. The value is passed in Uuid::simple() form
        // (32-hex, no hyphens) — matching the format used by job_id
        // and host_dir derivation elsewhere in this file.
        "ZSBX_SANDBOX_ID": sandbox_id,
    });
    // Restore-path env entry. Cold-boot leaves it unset.
    if let Some(p) = restore_from {
        env["ZSBX_RESTORE_FROM"] = serde_json::Value::String(p.display().to_string());
    }

    let resources = serde_json::json!({
        // CPU MHz is advisory — see `NOMAD_CPU_MHZ_ADVISORY`. Memory
        // is the real bin-packing input.
        //
        // MemoryMaxMB = 2 × MemoryMB (bug-#9 fix, 2026-05-22 cluster
        // validation). CH v51.1 mmap-faults the full guest RAM
        // during snapshot/restore which gets accounted to the task's
        // memcg; without slack the cgroup OOM-killer fires when CH
        // approaches the hard limit. MemoryMB stays as the
        // bin-packing input; MemoryMaxMB is the oversubscription
        // ceiling Nomad enforces via memory.high.
        "CPU": NOMAD_CPU_MHZ_ADVISORY,
        "MemoryMB": cfg.memory_mb as u32,
        "MemoryMaxMB": (cfg.memory_mb * 2) as u32,
    });

    // Typed Config block — field names + types mirror
    // `nomad-driver-ch/ch/task_config.go::TaskConfig`:
    //
    //   vm_index          uint16 (1..ceil)
    //   kernel            string  (host path to vmlinux)
    //   cpus              uint8
    //   memory_mb         uint32
    //   restore_from      string  (empty for cold-boot)
    //   sandbox_id        string  (32-hex, no hyphens)
    //   user_id           string  (typed_id `usr_...`, C-7-LT-7)
    //   workspace_img     string  (host path)
    //   user_home_img     string  (host path)
    //   pubkey_hex        string  (64-hex, no `0x`)
    //   subnet_base_octet uint16  (0..255, default 99)
    //   disks/fs/net      block-lists (empty → driver auto-
    //                     synthesises from the above)
    //
    // `kernel` path: the driver's StartTask appends `/vmlinuz` to the
    // runtime_dir. We forward the full path derived here for
    // explicitness; if the driver evolves its schema the controller
    // side picks up the change in one place.
    let kernel_path = cfg.nomad_ch.runtime_dir.join("vmlinuz");
    let restore_str = restore_from
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    // Option C Phase 2 (2026-05-25 staging-locality ADR):
    // emit `stage_disk_images: true` ONLY when the operator
    // has flipped the controller-side flag AND we're on the
    // cold-boot branch (the restore branch stages rootfs
    // via its own RootfsSource hardlink/copy and does not
    // re-stage workspace.img / home.img). The Go driver's
    // TaskConfig decodes this from the HCL schema; when
    // false (Phase 2 default), the driver's StartTask
    // skips its stageDiskImages op and the controller-side
    // spawn_blocking block above retains responsibility.
    let stage_disk_images = cfg.driver_stages_disk_images
        && restore_from.is_none();
    let config = serde_json::json!({
        "vm_index": vm_index,
        "kernel": kernel_path.display().to_string(),
        "cpus": cpus_boot(cfg.cpus),
        "memory_mb": cfg.memory_mb as u32,
        "restore_from": restore_str,
        "sandbox_id": sandbox_id,
        // C-7-LT-7: user_id feeds the driver's per-user-home
        // path allow-list. Without this, the restore-branch
        // rewriter rejects /var/zeroship/ch/users/<usr>/home.img
        // (smoke-r17 verbatim failure mode). Cross-tenant
        // isolation is preserved on the driver side via
        // strict user_id equality in the prefix check.
        "user_id": user_id,
        "workspace_img": workspace_img.display().to_string(),
        "user_home_img": user_home_img.display().to_string(),
        "pubkey_hex": pubkey_hex,
        "subnet_base_octet": cfg.nomad_ch.subnet_second_octet,
        "stage_disk_images": stage_disk_images,
        // Block-lists — empty triggers driver-side
        // auto-synthesis from the typed fields above
        // (matches T-3 default behaviour).
        "disks": [],
        "fs": [],
        "net": [],
    });

    // Option C Phase 2: also propagate the staging-locality flag
    // via job-level Meta. The driver reads `stage_disk_images` from
    // its typed TaskConfig (above), so this Meta entry is observer-
    // facing only (Nomad UI / `nomad job inspect` / log aggregators);
    // it gives operators a one-glance signal that the alloc was
    // submitted under driver-side staging. Cold-boot only (the
    // restore branch sets stage_disk_images=false above).
    let stage_disk_images_meta = cfg.driver_stages_disk_images
        && restore_from.is_none();
    let mut job = serde_json::json!({
        "ID": job_id,
        "Name": job_id,
        "Type": "service",
        "Datacenters": [cfg.nomad_ch.datacenter],
        "Meta": {
            "zeroship.user": user_id,
            "zeroship.project": project_id,
            "zeroship.sandbox": sandbox_id,
            "zeroship.vm_index": vm_index.to_string(),
            "zsbx_stage_disks": stage_disk_images_meta.to_string(),
        },
        "TaskGroups": [{
            "Name": "vm",
            "Count": 1,
            "RestartPolicy": {
                "Attempts": 0,
                "Mode": "fail",
                "Interval": 30_000_000_000u64,    // 30s, ns
                "Delay":     5_000_000_000u64,    //  5s, ns
            },
            "ReschedulePolicy": {
                "Attempts": 0,
                "Unlimited": false,
            },
            "Tasks": [{
                "Name": "ch",
                "Driver": "ch",
                "Config": config,
                "Env": env,
                "Resources": resources,
                "KillTimeout": 10_000_000_000u64,  // 10s, ns
            }],
        }],
    });
    // r3-A (T-8b-stress-r3 fix): pin alloc placement to THIS worker
    // when the controller cached its local Nomad node_id at boot.
    // The constraint targets `${node.unique.id}` (Nomad's per-client
    // unique-ID interpolation, equal to `stats.client.node_id`) with
    // a strict equality operand — Nomad rejects the alloc as
    // unschedulable if no client matches, surfacing the
    // misconfiguration loudly rather than silently scheduling
    // elsewhere. See `crate::backend::nomad_ch::fetch_local_nomad_node_id`
    // for the boot-time lookup; `None` (lookup failed / dev tests)
    // omits the Constraints block entirely so the pre-r3-A
    // random-placement shape is preserved as the fallback.
    if let Some(node_id) = local_nomad_node_id {
        job["Constraints"] = serde_json::json!([
            {
                "LTarget": "${node.unique.id}",
                "Operand": "=",
                "RTarget": node_id,
            }
        ]);
    }
    serde_json::json!({ "Job": job })
}

// ─── Nomad HTTP helpers ─────────────────────────────────────────

#[derive(Debug)]
struct AgentResponse {
    status: u16,
    body: String,
    bytes: Vec<u8>,
}

/// Submit a job spec to Nomad. The body is the JSON returned by
/// [`build_nomad_job_json`].
async fn submit_nomad_job(
    nomad_addr: &str,
    job_json: &serde_json::Value,
) -> Result<(), String> {
    let url = format!("{nomad_addr}/v1/jobs");
    let body = serde_json::to_vec(job_json)
        .map_err(|e| format!("serialize Nomad job JSON: {e}"))?;
    let resp = http_post_json_unsigned(&url, &body, Duration::from_secs(15)).await?;
    if resp.status != 200 {
        return Err(format!(
            "POST {url} → status {}: {}",
            resp.status,
            resp.body.trim()
        ));
    }
    // Body is a JobRegisterResponse; we don't need to parse it for
    // success — Nomad returns 200 only after enqueue.
    Ok(())
}

async fn stop_nomad_job(
    nomad_addr: &str,
    job_id: &str,
    purge: bool,
) -> Result<(), String> {
    let url = format!(
        "{nomad_addr}/v1/job/{job_id}?purge={}",
        if purge { "true" } else { "false" }
    );
    let resp = http_delete_unsigned(&url, Duration::from_secs(15)).await?;
    if resp.status != 200 && resp.status != 404 {
        return Err(format!(
            "DELETE {url} → status {}: {}",
            resp.status,
            resp.body.trim()
        ));
    }
    Ok(())
}

/// Poll the job's allocations until at least one has
/// `ClientStatus == "running"`, or the deadline expires.
///
/// JSON parse errors **and HTTP transport errors** are tracked +
/// log-rate-limited (~once per 5s) and surfaced in the timeout
/// message. Without the HTTP-error track, a Nomad-unreachable
/// outage and an alloc-never-scheduled outage produce the same
/// "alloc never reached running ... last status=<no allocs>"
/// message — completely different triage paths collapsed into one
/// (C3 fix).
async fn wait_for_alloc_running(
    nomad_addr: &str,
    job_id: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let url = format!("{nomad_addr}/v1/job/{job_id}/allocations");
    let mut last_status: Option<String> = None;
    let mut last_parse_err: Option<String> = None;
    let mut last_parse_log_at: Option<Instant> = None;
    let mut last_http_err: Option<String> = None;
    let mut last_http_log_at: Option<Instant> = None;
    while Instant::now() < deadline {
        let resp = http_get_unsigned(&url, Duration::from_secs(5)).await;
        match resp {
            Ok(r) if r.status == 200 => {
                let allocs = match serde_json::from_str::<serde_json::Value>(&r.body) {
                    Ok(v) => v,
                    Err(e) => {
                        let msg = format!("{e}");
                        // Rate-limit the log so a sustained
                        // garbage stream doesn't flood.
                        let now = Instant::now();
                        let stale = last_parse_log_at
                            .map(|t| now.duration_since(t) > Duration::from_secs(5))
                            .unwrap_or(true);
                        if stale {
                            tracing::warn!(
                                error = %msg,
                                "sandbox/nomad-ch alloc poll: JSON parse error (will retry)"
                            );
                            last_parse_log_at = Some(now);
                        }
                        last_parse_err = Some(msg);
                        compio::time::sleep(Duration::from_millis(250)).await;
                        continue;
                    }
                };
                let mut latest: Option<String> = None;
                for a in allocs.as_array().into_iter().flatten() {
                    let cs = a["ClientStatus"].as_str().unwrap_or("").to_string();
                    if cs == "running" {
                        return Ok(());
                    }
                    // Surface terminal failures fast — no point
                    // sitting through the timeout if the alloc
                    // already died.
                    if cs == "failed" || cs == "lost" {
                        let desc = a["ClientDescription"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        // T-8b-stress-r2 controller v34: also harvest
                        // the per-task TaskEvent DisplayMessage. Nomad's
                        // alloc-level `ClientDescription` is a generic
                        // rollup ("Failed tasks") that loses the
                        // actionable driver-side message — operators
                        // SSH'ing the worker to read `nomad alloc
                        // status` is the friction this surface
                        // removes. The driver-side msg lives at
                        // `TaskStates[<task>].Events[].DisplayMessage`;
                        // we collate the messages from any task with
                        // `Failed: true` so the controller's wire
                        // envelope carries the full chain
                        // (`backend_create_failed: <generic>: <driver
                        // verbatim>`).
                        let driver_msgs = extract_failed_task_event_msgs(a);
                        let composed = if driver_msgs.is_empty() {
                            format!("nomad alloc terminal status={cs}: {desc}")
                        } else {
                            format!(
                                "nomad alloc terminal status={cs}: {desc}: {}",
                                driver_msgs.join(" | ")
                            )
                        };
                        return Err(composed);
                    }
                    latest = Some(cs);
                }
                last_status = latest.or(last_status);
            }
            Ok(r) => {
                // 200 is the only happy status; 4xx/5xx surface as
                // an HTTP error track too — operators need to see
                // 401/403 (auth misconfig) and 5xx separately from
                // "no allocs yet".
                let msg = format!(
                    "status {} body={}",
                    r.status,
                    r.body.trim()
                );
                let now = Instant::now();
                let stale = last_http_log_at
                    .map(|t| now.duration_since(t) > Duration::from_secs(5))
                    .unwrap_or(true);
                if stale {
                    tracing::warn!(
                        error = %msg,
                        "sandbox/nomad-ch alloc poll: HTTP non-200 (will retry)"
                    );
                    last_http_log_at = Some(now);
                }
                last_http_err = Some(msg);
            }
            Err(e) => {
                // Transport-level failure (connection refused,
                // DNS, TLS, timeout). Track this distinctly so
                // the timeout message can say "Nomad unreachable"
                // rather than the misleading "alloc never reached
                // running, last status=<no allocs>".
                let now = Instant::now();
                let stale = last_http_log_at
                    .map(|t| now.duration_since(t) > Duration::from_secs(5))
                    .unwrap_or(true);
                if stale {
                    tracing::warn!(
                        error = %e,
                        "sandbox/nomad-ch alloc poll: HTTP transport error (will retry)"
                    );
                    last_http_log_at = Some(now);
                }
                last_http_err = Some(e);
            }
        }
        compio::time::sleep(Duration::from_millis(250)).await;
    }
    let mut msg = format!(
        "nomad alloc never reached running for job {job_id} (last status={:?})",
        last_status.unwrap_or_else(|| "<no allocs>".to_string())
    );
    if let Some(e) = last_http_err {
        msg.push_str(&format!(
            "; last HTTP error (Nomad reachability): {e}"
        ));
    }
    if let Some(e) = last_parse_err {
        msg.push_str(&format!("; last parse error: {e}"));
    }
    Err(msg)
}

/// T-8b-stress-r2 controller v34: collate the per-task DisplayMessage
/// strings from any TaskState marked `Failed: true` so the controller's
/// terminal-error envelope carries the driver-side verbatim message
/// instead of just Nomad's generic "Failed tasks" rollup.
///
/// Shape of the Nomad alloc JSON the function walks:
///
/// ```json
/// {
///   "ClientStatus": "failed",
///   "TaskStates": {
///     "ch": {
///       "State": "dead",
///       "Failed": true,
///       "Events": [
///         {"Type": "Driver Failure", "DisplayMessage": "...verbatim...", "Time": ...},
///         {"Type": "Alloc Unhealthy", "DisplayMessage": "Unhealthy because of failed task"},
///         ...
///       ]
///     }
///   }
/// }
/// ```
///
/// Returns a deduplicated, ordered list of `"<task>=<msg>"` strings
/// from one diagnostic event per failed task. Multiple failed tasks
/// (rare — a Nomad alloc typically has one) are joined by
/// `wait_for_alloc_running` with ` | `.
///
/// Empty list if the alloc has no `TaskStates` or no failed tasks.
/// Pure function, no I/O — pinned by unit tests below.
///
/// `pub(crate)` so the restore-path sibling (`restore_handler.rs::
/// wait_for_alloc_running_blocking`) reuses the same extraction. Keeping
/// the implementations in lockstep is the whole point of the v34
/// verbatim-msg propagation — divergence would silently re-introduce
/// the observability gap on the wake path.
///
/// ### T-8b-stress-r3 fix: event-type preference
///
/// Pre-r3 logic walked `Events[]` in REVERSE and took the first non-
/// empty DisplayMessage. That selects the LAST event, which on a
/// failed alloc is almost always Nomad's `Alloc Unhealthy` event with
/// the useless generic message `"Unhealthy because of failed task"`.
/// The actionable driver-emitted message (`Driver Failure` with the
/// `StartTask: disk[1] workspace.img does not exist ...` text) appears
/// EARLIER in the array and was silently masked.
///
/// Stress-r3 verbatim: 47/47 CREATE failures surfaced as `... Failed
/// tasks: ch: Unhealthy because of failed task` — confirming the
/// regression. The fix walks the Events array and prefers any event
/// whose `Type` is in [`DIAGNOSTIC_EVENT_TYPES`] (Driver Failure, Task
/// Setup Failure, etc.) over the generic Nomad-emitted `Alloc
/// Unhealthy` / `Restart Signaled` etc. If no diagnostic event is
/// present, falls back to the last non-empty DisplayMessage (the
/// pre-r3 behaviour) so we never lose information.
pub(crate) fn extract_failed_task_event_msgs(alloc: &serde_json::Value) -> Vec<String> {
    let task_states = match alloc["TaskStates"].as_object() {
        Some(o) => o,
        None => return Vec::new(),
    };
    let mut out: Vec<String> = Vec::with_capacity(task_states.len());
    for (task_name, ts) in task_states {
        let failed = ts["Failed"].as_bool().unwrap_or(false);
        if !failed {
            continue;
        }
        let events = match ts["Events"].as_array() {
            Some(e) => e,
            None => continue,
        };
        // Two-pass selection:
        //   pass 1: prefer an event whose Type is in the diagnostic
        //           allow-list (Driver Failure et al.). Walk forward so
        //           the FIRST diagnostic event wins (drivers typically
        //           emit only one Driver Failure event per alloc; if
        //           multiple appear, the first one is the root cause
        //           and subsequent ones are restart-retry side effects).
        //   pass 2: fall back to the LAST non-empty DisplayMessage —
        //           previous behaviour, retained so we never lose
        //           information when the driver omits a typed event.
        let mut picked: Option<&str> = None;
        for ev in events.iter() {
            let ty = ev["Type"].as_str().unwrap_or("");
            if !is_diagnostic_event_type(ty) {
                continue;
            }
            if let Some(msg) = ev["DisplayMessage"].as_str() {
                let trimmed = msg.trim();
                if !trimmed.is_empty() {
                    picked = Some(trimmed);
                    break;
                }
            }
        }
        if picked.is_none() {
            for ev in events.iter().rev() {
                if let Some(msg) = ev["DisplayMessage"].as_str() {
                    let trimmed = msg.trim();
                    if !trimmed.is_empty() {
                        picked = Some(trimmed);
                        break;
                    }
                }
            }
        }
        if let Some(trimmed) = picked {
            // Cap each task's message at 2 KiB so a pathological
            // driver that emits a multi-MB error doesn't bloat
            // the wire envelope. Truncation is rare but bounded.
            //
            // r27-M2: byte-indexing `&trimmed[..PER_TASK_CAP]` would
            // panic if the boundary lands mid-UTF-8 codepoint (e.g. a
            // driver-emitted error containing a multi-byte char that
            // straddles byte 2048). Mirror the canonical char-boundary
            // decrement pattern from `wake_machine::sanitize_error_message`
            // (the `is_char_boundary` loop at `wake_machine.rs:811`)
            // so any input — including ones engineered to land a
            // multi-byte char on the cap boundary — produces a valid
            // `&str` slice. Cost: a handful of byte comparisons per
            // truncation event (truncations are rare; the
            // `extract_failed_task_event_msgs_caps_oversized_message`
            // test is the only fixture that hits this path today).
            const PER_TASK_CAP: usize = 2048;
            let bounded: String = if trimmed.len() > PER_TASK_CAP {
                let mut end = PER_TASK_CAP;
                while end > 0 && !trimmed.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}…(truncated)", &trimmed[..end])
            } else {
                trimmed.to_string()
            };
            out.push(format!("{task_name}: {bounded}"));
        }
    }
    out
}

/// Nomad TaskEvent `Type` strings that carry the load-bearing driver-
/// side or scheduler-side failure cause. Source-of-truth is Nomad's
/// `nomad/structs/structs.go::TaskEvent*` constants; this list pins
/// the subset where DisplayMessage is the actionable error rather
/// than a generic rollup. Match is case-insensitive on full string
/// equality to avoid substring false-positives ("Driver" alone is a
/// healthy event type for happy-path "downloading artifacts" lines).
///
/// `Alloc Unhealthy`, `Restart Signaled`, `Terminated`, `Killing`,
/// `Killed` are deliberately EXCLUDED: they're emitted by Nomad's
/// alloc/task state machine AFTER the underlying failure and their
/// DisplayMessage is the generic envelope ("Unhealthy because of
/// failed task" etc.). The diagnostic-source event always precedes
/// these in the Events array.
fn is_diagnostic_event_type(ty: &str) -> bool {
    // Lower-case once; full-string equality on each candidate.
    let t = ty.trim().to_ascii_lowercase();
    matches!(
        t.as_str(),
        "driver failure"
            | "task setup failure"
            | "setup failure"
            | "failed validating task"
            | "failed artifact download"
            | "exec plugin"
    )
}

/// True when every alloc in the array has a terminal client status.
/// An empty / missing array is treated as terminal (no allocs to
/// wait on). Pulled out as a free helper for unit testing —
/// [`wait_for_job_gone`] is HTTP-bound and not unit-testable end to
/// end without a fake.
fn allocs_all_terminal(allocs: Option<&Vec<serde_json::Value>>) -> bool {
    let arr = match allocs {
        Some(a) => a,
        None => return true,
    };
    if arr.is_empty() {
        return true;
    }
    arr.iter().all(|a| {
        matches!(
            a["ClientStatus"].as_str().unwrap_or(""),
            "complete" | "failed" | "lost",
        )
    })
}

/// Block until the job's allocations are all in a terminal client
/// state (the wrapper script has exited → tap device + IP + virtiofsd
/// sockets released). Returns Ok on:
///
///   - `GET /v1/job/<id>` → 404 (Nomad GC removed the record), OR
///   - `GET /v1/job/<id>/allocations` → every alloc has
///     `ClientStatus ∈ {complete, failed, lost}` (or the array is
///     empty / 404).
///
/// We previously short-circuited on `Status == "dead" && Stop == true`
/// at the *job* level, but that races the wrapper-script teardown:
/// the job record is dead while individual alloc tasks (the
/// `raw_exec` wrapper, virtiofsd children) are still reaping. A
/// follow-up `create` reusing the released vm_index can collide on
/// the still-bound tap device. Polling the allocation client-status
/// instead catches the actual underlying-process termination.
///
/// Surfaces the Nomad status + any sustained JSON parse errors on
/// timeout so operators can investigate.
async fn wait_for_job_gone(
    nomad_addr: &str,
    job_id: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let job_url = format!("{nomad_addr}/v1/job/{job_id}");
    let allocs_url = format!("{nomad_addr}/v1/job/{job_id}/allocations");
    let mut last_parse_err: Option<String> = None;
    let mut last_parse_log_at: Option<Instant> = None;
    let mut last_status: Option<String> = None;
    // C3: track HTTP transport / non-2xx errors distinctly so a
    // Nomad-unreachable outage doesn't masquerade as "alloc still
    // reaping" in the timeout message.
    let mut last_http_err: Option<String> = None;
    let mut last_http_log_at: Option<Instant> = None;

    fn note_parse_err(
        last_parse_err: &mut Option<String>,
        last_parse_log_at: &mut Option<Instant>,
        scope: &str,
        e: serde_json::Error,
    ) {
        let msg = format!("{e}");
        let now = Instant::now();
        let stale = last_parse_log_at
            .map(|t| now.duration_since(t) > Duration::from_secs(5))
            .unwrap_or(true);
        if stale {
            tracing::warn!(
                scope,
                error = %msg,
                "sandbox/nomad-ch job-gone poll: JSON parse error (will retry)"
            );
            *last_parse_log_at = Some(now);
        }
        *last_parse_err = Some(msg);
    }

    fn note_http_err(
        last_http_err: &mut Option<String>,
        last_http_log_at: &mut Option<Instant>,
        scope: &str,
        msg: String,
    ) {
        let now = Instant::now();
        let stale = last_http_log_at
            .map(|t| now.duration_since(t) > Duration::from_secs(5))
            .unwrap_or(true);
        if stale {
            tracing::warn!(
                scope,
                error = %msg,
                "sandbox/nomad-ch job-gone poll: HTTP error (will retry)"
            );
            *last_http_log_at = Some(now);
        }
        *last_http_err = Some(msg);
    }

    while Instant::now() < deadline {
        // First check if the job record is gone entirely.
        let job_resp = http_get_unsigned(&job_url, Duration::from_secs(5)).await;
        match &job_resp {
            Ok(r) if r.status == 404 => return Ok(()),
            Ok(r) if r.status == 200 => {
                // Job still present; fall through to alloc poll.
            }
            Ok(r) => note_http_err(
                &mut last_http_err,
                &mut last_http_log_at,
                "job",
                format!("status {} body={}", r.status, r.body.trim()),
            ),
            Err(e) => note_http_err(
                &mut last_http_err,
                &mut last_http_log_at,
                "job",
                e.clone(),
            ),
        }

        // Otherwise look at the allocations: a job in Status=dead
        // can still have allocations whose underlying processes are
        // mid-reap. We require every alloc be in a terminal client
        // state before declaring the job "gone".
        let allocs_resp =
            http_get_unsigned(&allocs_url, Duration::from_secs(5)).await;
        match allocs_resp {
            Ok(r) if r.status == 404 => return Ok(()),
            Ok(r) if r.status == 200 => {
                match serde_json::from_str::<serde_json::Value>(&r.body) {
                    Ok(v) => {
                        let arr = v.as_array();
                        if allocs_all_terminal(arr) {
                            return Ok(());
                        }
                        // Surface the latest non-terminal status for
                        // the timeout error message.
                        if let Some(a) = arr.and_then(|a| a.last()) {
                            last_status = a["ClientStatus"]
                                .as_str()
                                .map(str::to_string);
                        }
                    }
                    Err(e) => {
                        note_parse_err(
                            &mut last_parse_err,
                            &mut last_parse_log_at,
                            "allocations",
                            e,
                        );
                    }
                }
            }
            Ok(r) => note_http_err(
                &mut last_http_err,
                &mut last_http_log_at,
                "allocations",
                format!("status {} body={}", r.status, r.body.trim()),
            ),
            Err(e) => note_http_err(
                &mut last_http_err,
                &mut last_http_log_at,
                "allocations",
                e,
            ),
        }
        compio::time::sleep(Duration::from_millis(250)).await;
    }
    let mut msg = format!(
        "job {job_id} allocs did not reach terminal state within timeout \
         (last alloc client_status={:?})",
        last_status.unwrap_or_else(|| "<unknown>".to_string())
    );
    if let Some(e) = last_http_err {
        msg.push_str(&format!(
            "; last HTTP error (Nomad reachability): {e}"
        ));
    }
    if let Some(e) = last_parse_err {
        msg.push_str(&format!("; last parse error: {e}"));
    }
    Err(msg)
}

// ─── Nomad agent self-identification ─────────────────────────────

/// r3-A (T-8b-stress-r3 fix). Fetch the Nomad agent's local node ID by
/// querying `GET /v1/agent/self` against `nomad_addr`. Returns the node
/// ID string (`Stats.client.node_id`) when the local Nomad agent is
/// reachable and running in client mode.
///
/// **Why this matters**: the controller stages `workspace.img` on its
/// LOCAL filesystem before submitting the Nomad job. Without a placement
/// constraint pinning the alloc to THIS node, Nomad's scheduler can pick
/// any client in the cluster — when it picks a different worker, the
/// driver's `assert_disk_image_present` stats the path on that node's
/// local fs and ENOENTs. T-8b-stress-r3 surfaced 78% cross-node-placement
/// failure at WORKER_COUNT=3 from exactly this gap.
///
/// **Failure shape**: returns `Err(_)` when Nomad is unreachable, the
/// response is non-200, the JSON is unparseable, or `Stats.client.node_id`
/// is absent (single-server mode, or an unexpected agent shape). Callers
/// at boot are expected to demote the failure to a WARN + bump the
/// `inc_nomad_node_id_lookup_failure` counter, then continue with the
/// `Option<String>` set to `None`. The fallback shape (no Constraints
/// block) restores the pre-r3-A behaviour — random cross-node placement
/// — so a controller restart against a transiently-unavailable Nomad
/// agent doesn't fail the boot path.
pub(crate) async fn fetch_local_nomad_node_id(
    nomad_addr: &str,
) -> Result<String, String> {
    let url = format!("{nomad_addr}/v1/agent/self");
    let resp = http_get_unsigned(&url, Duration::from_secs(5)).await?;
    if resp.status != 200 {
        return Err(format!(
            "GET {url} → status {}: {}",
            resp.status,
            resp.body.trim()
        ));
    }
    parse_nomad_agent_self_node_id(&resp.body)
}

/// Pure parser for the `/v1/agent/self` body. Extracted from
/// [`fetch_local_nomad_node_id`] so unit tests can exercise the
/// response-shape handling (Nomad-version drift, missing fields,
/// empty strings) without launching an HTTP fixture.
///
/// **Field path**: `stats.client.node_id`. Live Nomad 1.x agents emit
/// lower-case Go-json-tag keys (`stats`/`client`/`node_id`); some
/// older API references show PascalCase. Accept both for defensiveness
/// — a server-mode-only agent has no `stats.client` block, which is
/// the legitimate "no client node_id" shape and surfaces here as
/// `Err("missing stats.client.node_id…")`.
pub(crate) fn parse_nomad_agent_self_node_id(body: &str) -> Result<String, String> {
    let body: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| format!("parse /v1/agent/self body: {e}"))?;
    // Nomad's /v1/agent/self body shape (verified against Nomad 1.x):
    //   { "config": {...}, "stats": {...}, "member": {...} }
    // The client's node ID lives under `stats.client.node_id`. Note the
    // lowercase `stats` / `client` — older docs sometimes show `Stats`,
    // but the live API response is lower-case (Go json tags). Accept
    // both for defensiveness against agent-version drift.
    let node_id = body
        .get("stats")
        .or_else(|| body.get("Stats"))
        .and_then(|stats| stats.get("client").or_else(|| stats.get("Client")))
        .and_then(|client| {
            client.get("node_id").or_else(|| client.get("NodeID"))
        })
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            "missing stats.client.node_id in /v1/agent/self response \
             (single-server-only agent? unexpected response shape?)"
                .to_string()
        })?;
    if node_id.is_empty() {
        return Err(
            "stats.client.node_id present but empty in /v1/agent/self response"
                .to_string(),
        );
    }
    Ok(node_id.to_string())
}

// ─── Generic HTTP (unsigned — Nomad API) ─────────────────────────

async fn http_get_unsigned(url: &str, timeout: Duration) -> Result<AgentResponse, String> {
    let url = url.to_string();
    compio::runtime::spawn_blocking(move || {
        let req = ureq::get(&url).timeout(timeout);
        send_ureq(req, &[])
    })
    .await
    .map_err(|e| format!("blocking task panic: {e:?}"))?
}

async fn http_delete_unsigned(
    url: &str,
    timeout: Duration,
) -> Result<AgentResponse, String> {
    let url = url.to_string();
    compio::runtime::spawn_blocking(move || {
        let req = ureq::delete(&url).timeout(timeout);
        send_ureq(req, &[])
    })
    .await
    .map_err(|e| format!("blocking task panic: {e:?}"))?
}

async fn http_post_json_unsigned(
    url: &str,
    body: &[u8],
    timeout: Duration,
) -> Result<AgentResponse, String> {
    let url = url.to_string();
    let body = body.to_vec();
    compio::runtime::spawn_blocking(move || {
        let req = ureq::post(&url)
            .timeout(timeout)
            .set("content-type", "application/json");
        send_ureq(req, &body)
    })
    .await
    .map_err(|e| format!("blocking task panic: {e:?}"))?
}

fn send_ureq(req: ureq::Request, body: &[u8]) -> Result<AgentResponse, String> {
    let send = if body.is_empty() {
        req.call()
    } else {
        req.send_bytes(body)
    };
    match send {
        Ok(resp) => {
            let status = resp.status();
            let mut bytes = Vec::new();
            let _ = resp
                .into_reader()
                .take(64 * 1024 * 1024)
                .read_to_end(&mut bytes);
            let body = String::from_utf8_lossy(&bytes).into_owned();
            Ok(AgentResponse {
                status,
                body,
                bytes,
            })
        }
        Err(ureq::Error::Status(code, resp)) => {
            // 8 KiB cap on error bodies — matches k8s.rs. Nomad +
            // agent error responses are tiny JSON; anything larger
            // is almost certainly an HTML interstitial we don't want
            // copied verbatim into our log lines.
            let mut bytes = Vec::new();
            let _ = resp.into_reader().take(8 * 1024).read_to_end(&mut bytes);
            let body = String::from_utf8_lossy(&bytes).into_owned();
            Ok(AgentResponse {
                status: code,
                body,
                bytes,
            })
        }
        Err(e) => Err(format!("{e}")),
    }
}

// ─── Signed agent HTTP (mirror of K8s `http_signed_async`) ───────

/// Async wrapper that signs + sends to the in-VM agent without
/// blocking the ntex worker. **Signing happens inside the closure**
/// (after the spawn_blocking queue drains) so the agent's 5-second
/// skew window doesn't fire on a queued request. See
/// `crates/sandbox/src/backend/k8s.rs::http_signed_async` for the
/// full rationale; this is a verbatim copy with the same semantics.
async fn http_signed_async(
    signing_key: &Arc<SigningKey>,
    method: &str,
    url: &str,
    body: &[u8],
) -> Result<AgentResponse, String> {
    let path = url
        .splitn(4, '/')
        .nth(3)
        .map(|p| format!("/{p}"))
        .unwrap_or_else(|| "/".to_string());
    let path = path.split('?').next().unwrap_or("/").to_string();

    // Arc clone — refcount bump, NOT a 32-byte secret copy.
    let signing_key = Arc::clone(signing_key);
    let method = method.to_string();
    let url = url.to_string();
    let body = body.to_vec();
    compio::runtime::spawn_blocking(move || {
        let ts = unix_now();
        let nonce = random_nonce()?;
        let signature = sig::sign(&signing_key, &method, &path, &body, ts, &nonce);
        signed_blocking_call(&method, &url, &body, ts, &nonce, &signature)
    })
    .await
    .map_err(|e| format!("blocking task panic: {e:?}"))?
}

fn signed_blocking_call(
    method: &str,
    url: &str,
    body: &[u8],
    ts: u64,
    nonce: &str,
    signature: &str,
) -> Result<AgentResponse, String> {
    let mut req = match method {
        "GET" => ureq::get(url),
        "POST" => ureq::post(url),
        "PUT" => ureq::put(url),
        "DELETE" => ureq::delete(url),
        m => return Err(format!("unsupported method {m}")),
    };
    req = req
        .timeout(Duration::from_secs(60))
        .set("x-sbx-timestamp", &ts.to_string())
        .set("x-sbx-nonce", nonce)
        .set("x-sbx-signature", signature);
    // `send_ureq`'s error already carries the URL via the underlying
    // `ureq::Error: Display` impl; prefixing the method+url again
    // here just produced doubled-up
    // "POST http://...: connection refused: POST http://...:" log
    // lines. Propagate `send_ureq` directly.
    send_ureq(req, body)
}

/// FM-B: structured operator-visible log line for non-2xx agent
/// responses. The error already propagates to the HTTP response,
/// but during the N=8 stress run the controller logged ZERO error
/// lines despite 14 5xx-to-client across the c2 cycle — every
/// failure was buried inside the returned `Result`. Logging at the
/// agent boundary closes that observability gap without growing
/// the public error surface.
///
/// Body excerpt is capped to 256 bytes; trailing newline / bulk
/// HTML interstitials would otherwise wreck the per-line grep
/// shape that operators rely on.
fn log_agent_error(sandbox_id: Uuid, op: &str, status: u16, body: &str) {
    let excerpt: String = body.chars().take(256).collect();
    tracing::warn!(
        sandbox_id = %sandbox_id,
        op,
        status,
        body = ?excerpt,
        "sandbox/nomad-ch agent error"
    );
}

/// Poll `/livez` until 200 AND the agent's `/version` reports the
/// **expected pubkey fingerprint**, or the deadline expires.
///
/// **Why both checks?** During N=8 rapid-recycle stress testing the
/// host-side process tree (cloud-hypervisor + 3× virtiofsd + the
/// bash wrapper + the tap binding) was observed to lag Nomad's view
/// of alloc-terminal by 0.5–2 s. A fresh `create()` for the same VM
/// index could land while a *previous* tenant's agent was still
/// answering `/livez=200` on the same IP — the controller would
/// return 201 in 0.25 s (vs the ~6 s healthy baseline), then every
/// subsequent `/exec` would 502 with "No route to host" once the old
/// VM finally died.
///
/// The fingerprint is the kernel of "is this OUR agent?" — the
/// controller mints a fresh Ed25519 keypair per sandbox; the agent
/// publishes the pubkey-SHA256[..16] (32 hex chars) under
/// `pubkey_fingerprint` on `/version`. A stale-tenant agent has a *different* fingerprint
/// (different keypair → different pubkey → different hash), so we
/// keep polling until either:
///   1. `/version.pubkey_fingerprint` matches `expected_fp` → ready, OR
///   2. Deadline expires → "stale agent at <ip>: expected <fp>, got <fp>"
///      — operator gets actionable text instead of a silent racy 201.
///
/// The `/version` endpoint is **auth-gated**, so we sign the probe
/// with the controller-side `signing_key` we just minted. That
/// reinforces the same-tenant guarantee: an agent that doesn't have
/// our pubkey at `/run/keys/controller-pubkey` 401s the probe; we
/// retry until either the right agent comes up or we time out.
///
/// **Backward compatibility:** older agents (pre-`pubkey_fingerprint`
/// in `/version`) will return JSON without the field. We treat a
/// missing/empty fingerprint as "this agent is too old to attest";
/// emit a one-shot warning and fall back to /livez-only behaviour.
/// Removing this fallback once the agent fleet is fully upgraded is
/// a one-line change.
///
/// ## R19-I1: two-phase probe (compio-native connect gate, then ureq)
///
/// Previous implementation: a single `compio::runtime::spawn_blocking(||
/// ureq::get(livez).timeout(500ms).call())` per loop iteration. That
/// carried the *same* wedge shape C-7-LT-2-PR1 just fixed on the
/// teardown side: ureq's `.timeout()` is a request-deadline timeout,
/// not a connect timeout. A half-collapsed TAP route at create time
/// (stale-tenant CH still alive, new TAP not fully wired) hangs SYN
/// for the kernel's retransmit ceiling (~30 s on Linux defaults). Each
/// stuck probe burns the entire intended cadence; `agent_livez_timeout`
/// then collapses to "one probe" instead of the designed poll loop.
///
/// Today: a per-iteration `probe_agent_reachable_tcp(addr, 150ms)`
/// gate (Phase 1) decides whether to issue the ureq /livez call
/// (Phase 2). Phase 1's outer `compio::time::timeout` caps a stuck
/// SYN at 150 ms exactly — independent of kernel SYN-retransmit. Only
/// once the TCP layer ACKs do we spend a spawn_blocking + ureq on the
/// /livez HTTP check; a stuck ureq there now means the agent's TCP
/// listener is up but its HTTP server is wedged, a much rarer failure
/// shape and one that's still bounded by `agent_livez_timeout_secs` +
/// `CreateGuard::drop` tear-down via the fixed teardown probe.
///
/// Closes the second of two ureq-probe wedge sites flagged by r19-A5
/// / R19-I1 (`docs/reviews/sandbox-snapshot-restore-concurrency-2026-05-25-r19.md`).
async fn wait_for_agent_livez(
    base_url: &str,
    expected_fp: &str,
    signing_key: &Arc<SigningKey>,
    timeout: Duration,
) -> Result<(), String> {
    const CONNECT_TIMEOUT: Duration = Duration::from_millis(150);
    let deadline = Instant::now() + timeout;
    let livez_url = format!("{base_url}/livez");
    // Parse host:port from the base_url ONCE — every Phase 1 probe in
    // the loop reuses the SocketAddr. If we can't parse it the gate
    // can't proceed; surface that immediately rather than burning
    // budget on doomed probes (mirrors `wait_for_agent_silent`).
    let probe_addr = match parse_agent_probe_addr(base_url) {
        Ok(a) => a,
        Err(e) => {
            return Err(format!(
                "agent at {base_url} unparseable; refusing to probe \
                 (parse error: {e}; expected fp={expected_fp})"
            ));
        }
    };
    let mut last_fp: Option<String> = None;
    let mut last_version_status: Option<u16> = None;
    while Instant::now() < deadline {
        // Phase 1 (R19-I1): compio-native TCP-connect gate. Caps a
        // stuck SYN at CONNECT_TIMEOUT (no kernel SYN-retransmit
        // wedge). An agent that's not yet listening turns into a
        // fast miss; we sleep the cadence and retry.
        if !probe_agent_reachable_tcp(probe_addr, CONNECT_TIMEOUT).await {
            // 150 ms livez poll cadence — matches the post-gate path.
            compio::time::sleep(Duration::from_millis(150)).await;
            continue;
        }
        // Phase 2: cheap unsigned /livez HTTP probe — gates the more
        //    expensive signed /version call. TCP layer just verified
        //    above, so a stuck ureq here is "HTTP server wedged after
        //    socket up" — rarer than the SYN-retransmit wedge, and
        //    still bounded by the outer deadline.
        let probe_url = livez_url.clone();
        let livez_status = compio::runtime::spawn_blocking(move || {
            ureq::get(&probe_url)
                .timeout(Duration::from_millis(500))
                .call()
                .map(|r| r.status())
                .ok()
        })
        .await
        .ok()
        .flatten();
        if livez_status == Some(200) {
            // 2. Agent is answering. Now confirm it's OUR agent by
            //    asking /version for its pubkey fingerprint.
            let version_url = format!("{base_url}/version");
            match http_signed_async(signing_key, "GET", &version_url, &[]).await {
                Ok(resp) => {
                    last_version_status = Some(resp.status);
                    if resp.status == 200 {
                        let fp_opt = serde_json::from_str::<serde_json::Value>(&resp.body)
                            .ok()
                            .and_then(|v| {
                                v.get("pubkey_fingerprint")
                                    .and_then(|s| s.as_str())
                                    .map(|s| s.to_string())
                            });
                        match fp_opt {
                            Some(fp) if fp == expected_fp => {
                                return Ok(());
                            }
                            Some(fp) => {
                                last_fp = Some(fp);
                                // Stale tenant. Keep polling — either
                                // it dies and our agent comes up, or
                                // the deadline expires and we surface
                                // the mismatch.
                            }
                            None => {
                                // Backward-compat: legacy agent without
                                // pubkey_fingerprint in /version. The
                                // /version call already passed our
                                // signed-auth check, so the agent IS
                                // verifying with our pubkey → it's
                                // ours. Warn and accept.
                                tracing::warn!(
                                    base_url = %base_url,
                                    "sandbox/nomad-ch wait_for_agent: legacy agent returned no pubkey_fingerprint on /version; falling back to signed-auth-only attestation (upgrade the agent to close the stale-tenant race on /livez=200 before /version is signed-auth gated)"
                                );
                                return Ok(());
                            }
                        }
                    }
                    // 401 means a stale-tenant agent that's verifying
                    // a *different* pubkey. Keep polling; same
                    // rationale as the fp-mismatch branch.
                }
                Err(_e) => {
                    // Transport error on /version — agent just came
                    // up answering /livez but isn't fully ready, or
                    // the connection raced a tear-down. Retry.
                }
            }
        }
        // 150 ms livez poll cadence — matches k8s.rs.
        compio::time::sleep(Duration::from_millis(150)).await;
    }
    // Timeout. Distinguish:
    //   - never saw /livez=200 → "agent at <url> never returned 200"
    //   - saw /livez=200 but fingerprint mismatched → stale-tenant
    //   - saw /livez=200 but /version 401'd → wrong-pubkey agent
    if let Some(actual_fp) = last_fp {
        Err(format!(
            "stale agent at {base_url}: expected pubkey_fingerprint={expected_fp}, \
             got {actual_fp}; previous tenant's wrapper still owns the IP"
        ))
    } else if last_version_status == Some(401) {
        Err(format!(
            "stale agent at {base_url}: /version returned 401 (agent is verifying with a \
             different controller pubkey); expected fp={expected_fp}"
        ))
    } else {
        Err(format!("agent at {base_url} never returned 200 on /livez (expected fp={expected_fp})"))
    }
}

/// FM-F: host-side fence — verify the agent on `base_url/livez` has
/// **stopped answering** before releasing the vm_index.
///
/// `wait_for_job_gone` returns Ok when Nomad reports the alloc
/// terminal + job purged, but that lags the host process tree
/// (cloud-hypervisor + 3× virtiofsd + the bash wrapper) by 0.5–60 s
/// under N=8 concurrent stop+create cycles. Releasing the vm_index
/// inside that window hands the same `10.99.<100+idx>.2:7777` IP to
/// a fresh tenant whose `/livez=200` probe succeeds — but against
/// the *previous* tenant's still-alive agent. This is the FM-F race
/// that produced 6/8 30 s timeouts in cycle 2 of the stress test.
///
/// The fence requires **two consecutive failures** within the
/// timeout window. A single failure is too easy: a transient TCP
/// reset during graceful shutdown can flap. Two-in-a-row inside the
/// 100 ms cadence is overwhelmingly indicative of "no listener" —
/// the agent process is gone.
///
/// A "miss" is a TCP-connect failure (refused / unreachable / our
/// 150 ms connect-timeout exceeded). A successful connect counts as
/// "agent still answering" and resets the counter — the socket is
/// alive regardless of what HTTP status it would have returned.
///
/// ## C-7-LT-2-PR1: compio-native TCP-connect probe (NOT ureq HTTP)
///
/// Previous implementation: `compio::runtime::spawn_blocking(|| ureq::
/// get(&probe_url).timeout(Duration::from_millis(500)).call()).await`.
/// That looked correct but had a fatal pathology on the TAP-collapsing
/// teardown path: ureq's `.timeout()` is a *request-deadline* timeout,
/// not a *connect* timeout. On a half-collapsed TAP route, TCP-connect
/// hangs waiting for the OS to surface ECONNREFUSED/ETIMEDOUT (Linux
/// SYN-retransmit ceiling ~30 s), so the single `ureq.call()` consumed
/// the entire fence budget. Smoke-r13 (`docs/reviews/…T8b-smoke-r13.md`)
/// observed `probes=1, consecutive_misses=1, elapsed_ms=30129` —
/// exactly the "one probe in 30 s" wedge.
///
/// Today: a `compio::net::TcpStream::connect(addr)` wrapped in
/// `compio::time::timeout(150ms, …)`. compio's outer timeout is a
/// hard cancellation on a stuck connect future (io_uring CANCEL),
/// independent of any kernel-level SYN-retransmit behaviour. So a
/// black-hole TAP returns "miss" at 150 ms exactly, the 100 ms cadence
/// runs as designed, and the 2-in-a-row contract clears the fence.
///
/// Connect-only: we don't need an HTTP request to know the agent is
/// up — the socket either accepts SYN+ACK or it doesn't. The 150 ms
/// connect-timeout is slightly larger than the 100 ms probe cadence
/// so a normally-responsive agent (loopback ms latency) ACKs well
/// inside the window.
///
/// Returns:
///   - Ok(()) — fence passed (two consecutive misses); safe to
///     release vm_index.
///   - Err(timeout text) — agent still answering at deadline; the
///     caller MUST leak the vm_index.
async fn wait_for_agent_silent(
    base_url: &str,
    timeout: Duration,
) -> Result<(), String> {
    // R16-A2 / R16-I2 phase instrumentation. Log target lets
    // smoke-r12 (and future cluster runs) `RUST_LOG` just this
    // module: `sandbox::teardown::fence=debug`. Threshold (2) is
    // the structural invariant the smoke-r10 LEAK case violates;
    // it's logged on entry so a future tuning is immediately
    // visible in the trace.
    const MISS_THRESHOLD: u32 = 2;
    const PROBE_CADENCE: Duration = Duration::from_millis(100);
    const CONNECT_TIMEOUT: Duration = Duration::from_millis(150);
    let fn_started = Instant::now();
    let deadline = fn_started + timeout;
    // Parse host:port from the base_url ONCE — every probe in the
    // loop reuses the SocketAddr. If we can't parse it the fence
    // can't proceed; surface that immediately rather than burning
    // the budget on doomed probes.
    let probe_addr = match parse_agent_probe_addr(base_url) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(
                target: "sandbox::teardown::fence",
                base_url = %base_url,
                error = %e,
                "host_fence: refusing to probe — base_url unparseable"
            );
            return Err(format!(
                "agent at {base_url} unparseable; leaking vm_index to avoid \
                 handing out a live IP (parse error: {e})"
            ));
        }
    };
    let mut consecutive_misses = 0u32;
    // Keep enough state to produce a useful timeout error. With the
    // PR1 connect-only probe we no longer have a per-probe HTTP
    // status; `last_status` stays `None` and is preserved in the
    // error format only for log-pattern stability with prior
    // smoke logs (operators grep for `last_http_status=`).
    let last_status: Option<u16> = None;
    let mut probe_count: u32 = 0;
    tracing::debug!(
        target: "sandbox::teardown::fence",
        base_url = %base_url,
        probe_addr = %probe_addr,
        timeout_ms = %timeout.as_millis(),
        miss_threshold = MISS_THRESHOLD,
        connect_timeout_ms = %CONNECT_TIMEOUT.as_millis(),
        cadence_ms = %PROBE_CADENCE.as_millis(),
        "host_fence: entered"
    );
    loop {
        if Instant::now() >= deadline {
            break;
        }
        let probe_start = Instant::now();
        tracing::debug!(
            target: "sandbox::teardown::fence",
            base_url = %base_url,
            probe = probe_count + 1,
            elapsed_ms = %fn_started.elapsed().as_millis(),
            "host_fence: poll start"
        );
        let reachable =
            probe_agent_reachable_tcp(probe_addr, CONNECT_TIMEOUT).await;
        probe_count += 1;
        // C-7-LT-2-PR1: classify is now connect-only. Reachable =>
        // socket alive (the agent OR the wrapper's tap is still
        // ACKing SYN); not reachable => miss (connect-refused,
        // connect-unreachable, or our 150 ms connect-timeout
        // exceeded — all three classify identically).
        let is_miss = !reachable;
        if is_miss {
            consecutive_misses += 1;
            tracing::debug!(
                target: "sandbox::teardown::fence",
                base_url = %base_url,
                probe = probe_count,
                consecutive_misses,
                miss_threshold = MISS_THRESHOLD,
                probe_duration_ms = %probe_start.elapsed().as_millis(),
                "host_fence: miss"
            );
            if consecutive_misses >= MISS_THRESHOLD {
                tracing::info!(
                    target: "sandbox::teardown::fence",
                    base_url = %base_url,
                    probes = probe_count,
                    consecutive_misses,
                    elapsed_ms = %fn_started.elapsed().as_millis(),
                    "host_fence: threshold reached — agent silent fence cleared"
                );
                return Ok(());
            }
        } else {
            // R16-I2 diagnostic surface: a non-zero -> 0 transition
            // here is the alternating-answer LEAK pathology.
            // Logged at info so smoke-r12 catches it without a
            // verbose subscriber.
            if consecutive_misses > 0 {
                tracing::info!(
                    target: "sandbox::teardown::fence",
                    base_url = %base_url,
                    probe = probe_count,
                    prev_consecutive_misses = consecutive_misses,
                    probe_duration_ms = %probe_start.elapsed().as_millis(),
                    "host_fence: agent reachable mid-fence — consecutive_misses counter reset (R16-I2 LEAK signal)"
                );
            } else {
                tracing::debug!(
                    target: "sandbox::teardown::fence",
                    base_url = %base_url,
                    probe = probe_count,
                    probe_duration_ms = %probe_start.elapsed().as_millis(),
                    "host_fence: agent reachable"
                );
            }
            consecutive_misses = 0;
        }
        // 100 ms cadence — tight enough that a 0.5 s tail is caught
        // in ~5 polls; loose enough that a 30 s budget on a stuck
        // agent doesn't burn 300+ tasks. PR1 changes: subtract the
        // probe's own elapsed from the cadence so a probe that
        // takes 0 ms (loopback ACK) and a probe that takes 150 ms
        // (connect-timeout) BOTH produce the same ~100 ms inter-
        // probe interval — the loop's wall-time pacing is now
        // decoupled from probe latency, which is the structural
        // invariant smoke-r13's "probes=1" pathology violated.
        let probe_elapsed = probe_start.elapsed();
        if probe_elapsed < PROBE_CADENCE {
            compio::time::sleep(PROBE_CADENCE - probe_elapsed).await;
        }
    }
    tracing::warn!(
        target: "sandbox::teardown::fence",
        base_url = %base_url,
        probes = probe_count,
        consecutive_misses,
        last_status = ?last_status,
        elapsed_ms = %fn_started.elapsed().as_millis(),
        fence_passed = false,
        "host_fence: deadline reached — agent still answering (R16-I2 final consecutive_misses)"
    );
    Err(format!(
        "agent at {base_url} still answering at fence deadline \
         (probes={probe_count}, last_http_status={:?}, consecutive_misses={consecutive_misses}); \
         leaking vm_index to avoid handing out a live IP",
        last_status,
    ))
}

/// C-7-LT-2-PR1: parse the `host:port` `SocketAddr` from a probe
/// `base_url` like `http://10.99.101.2:7777`. Connect-only — no path
/// segment, no scheme validation beyond the `://` separator the
/// `wait_for_agent_silent` caller's url-shape requires.
///
/// The agent URL the controller passes in is built locally from
/// `vm_index` (`http://10.99.<100+idx>.2:7777`); it's never
/// user-input. We still parse defensively because a future caller
/// (e.g. a wake-side smoke probe) might hand in an ipv6-literal or
/// a slightly different shape, and the fence MUST surface a parse
/// error rather than silently probe a wrong address.
fn parse_agent_probe_addr(base_url: &str) -> Result<std::net::SocketAddr, String> {
    // Strip the scheme if present.
    let after_scheme = match base_url.find("://") {
        Some(i) => &base_url[i + 3..],
        None => base_url,
    };
    // Strip any path segment (e.g. `/livez`).
    let host_port = match after_scheme.find('/') {
        Some(i) => &after_scheme[..i],
        None => after_scheme,
    };
    if host_port.is_empty() {
        return Err("empty host:port".into());
    }
    // `SocketAddr::from_str` handles both ipv4 (`a.b.c.d:port`) and
    // ipv6-literal-with-brackets (`[::1]:port`). For non-literal
    // hostnames we fall through to `ToSocketAddrs` (DNS).
    use std::str::FromStr;
    if let Ok(addr) = std::net::SocketAddr::from_str(host_port) {
        return Ok(addr);
    }
    // Hostname path. The fence's caller only ever passes the
    // controller-derived IP literal, so this branch is "be liberal in
    // what you accept" — picks the first resolved addr.
    use std::net::ToSocketAddrs;
    match host_port.to_socket_addrs() {
        Ok(mut iter) => iter
            .next()
            .ok_or_else(|| format!("no addresses resolved for {host_port}")),
        Err(e) => Err(format!("resolve {host_port}: {e}")),
    }
}

/// C-7-LT-2-PR1: compio-native TCP-connect probe with a hard outer
/// timeout. Returns `true` iff the kernel ACKs the SYN within
/// `connect_timeout`; `false` for refused / unreachable / our own
/// timeout (all three are equivalent "miss" outcomes for the fence).
///
/// Why `compio::time::timeout` over the kernel's TCP retry behaviour:
/// the kernel's SYN-retransmit ceiling on Linux is typically 30-90 s
/// before ECONNREFUSED/ETIMEDOUT surfaces, which is what produced
/// the smoke-r13 "1 probe in 30 s" wedge with ureq. compio's outer
/// timeout cancels the connect future at exactly the budget; the
/// underlying TCP socket is dropped via io_uring CANCEL on the next
/// runtime tick.
async fn probe_agent_reachable_tcp(
    addr: std::net::SocketAddr,
    connect_timeout: Duration,
) -> bool {
    let connect_fut = compio::net::TcpStream::connect(addr);
    match compio::time::timeout(connect_timeout, connect_fut).await {
        Ok(Ok(_stream)) => true, // SYN ACKed → socket alive
        Ok(Err(_)) => false,     // ECONNREFUSED / EHOSTUNREACH / etc.
        Err(_) => false,         // connect_timeout exceeded
    }
}

// ─── small utilities (mirrored from k8s.rs) ─────────────────────

/// Derive the per-user home-image path from the configured root.
/// Layout: `<user_home_dir_root>/<user_id>/home.img`. The path is
/// reused across every sandbox the user creates so package caches
/// and dotfiles persist (see `create_sandbox` step 3).
pub(crate) fn user_home_image_path(
    user_home_dir_root: &Path,
    user_id: &str,
) -> PathBuf {
    user_home_dir_root.join(user_id).join("home.img")
}

/// Derive the per-sandbox workspace-image path inside a sandbox's
/// host_dir. Per-sandbox, freshly created on cold-boot.
pub(crate) fn workspace_image_path(host_dir: &Path) -> PathBuf {
    host_dir.join("workspace.img")
}

/// Create a raw ext4 image at `path` of `size_gb` gigabytes if and
/// only if the file does not already exist. Uses `truncate -s` to
/// produce a sparse image (no zero-write up front, just metadata)
/// and `mkfs.ext4 -q -F` to format. Idempotent: a second invocation
/// against the same path is a no-op.
///
/// The `-F` flag on mkfs.ext4 is required to format a regular file
/// that isn't a block device; without it mkfs prompts and aborts.
///
/// **Post-condition (T-8b-stress Bug 1 fix):** after either branch
/// (skip-because-exists OR truncate+mkfs), the function asserts that
/// `path` exists, is a file, and has non-zero size via [`assert_disk_image_present`]
/// before returning Ok. This catches a silent-staging-failure mode
/// (e.g., a future refactor that no-op's the truncate step would
/// otherwise return Ok and let the driver's preflight surface the
/// confusing "disk[N] /…/<img> does not exist" error far from the
/// real fault). The parent directory is fsync'd so the dirent is
/// visible to a peer process (the Nomad client) that may stat the
/// path on a different mount-namespace or before the kernel's lazy
/// dirent commit.
///
/// Returns `Err` with a String describing which subprocess failed.
/// The caller maps that into a controller-side error log; the
/// `CreateGuard` Drop on the calling path tears down the partial
/// host_dir state.
pub(crate) fn create_ext4_image_if_missing(
    path: &Path,
    size_gb: u32,
) -> Result<(), String> {
    if path.exists() {
        // Idempotent skip path. Still re-assert the post-condition so
        // a stale dirent or zero-byte sentinel surfaces here at the
        // controller, not later at the driver's preflight stat.
        return assert_disk_image_present(path);
    }
    // truncate(1) is universally present on debian + bash; using it
    // (rather than `std::fs::File::set_len`) keeps the path-and-size
    // contract identical to the CLI an operator would type, which
    // makes the failure mode easier to reproduce by hand.
    let size = format!("{size_gb}G");
    let truncate_status = std::process::Command::new("truncate")
        .args(["-s", &size])
        .arg(path)
        .status()
        .map_err(|e| format!("spawn truncate: {e}"))?;
    if !truncate_status.success() {
        return Err(format!(
            "truncate -s {size} {} exited {}",
            path.display(),
            truncate_status,
        ));
    }
    let mkfs_status = std::process::Command::new("mkfs.ext4")
        .args(["-q", "-F"])
        .arg(path)
        .status()
        .map_err(|e| format!("spawn mkfs.ext4: {e}"))?;
    if !mkfs_status.success() {
        // Clean up the half-created image so a retry doesn't see
        // an unformatted file at the same path and skip the mkfs.
        let _ = std::fs::remove_file(path);
        return Err(format!(
            "mkfs.ext4 -q -F {} exited {}",
            path.display(),
            mkfs_status,
        ));
    }
    // Fsync the parent directory so the new dirent is durable AND
    // visible to a peer process statting the path before the kernel
    // would otherwise commit. Best-effort: a failure here is logged
    // by the caller via the returned Err but doesn't unwind the
    // mkfs; the file is still on disk (just not necessarily
    // crash-safe). See T-8b-stress Bug 1: 49/60 CREATEs failed with
    // the driver reporting "workspace.img does not exist" despite
    // the controller having just staged it — the staging-and-submit
    // window is tight enough that a missing fsync is plausible.
    if let Some(parent) = path.parent() {
        if let Err(e) = fsync_dir(parent) {
            return Err(format!(
                "fsync parent of {}: {e}",
                path.display()
            ));
        }
    }
    assert_disk_image_present(path)
}

/// Post-condition assertion for [`create_ext4_image_if_missing`] and
/// any sibling staging path that needs to guarantee a disk image is
/// on disk before the controller hands the path off to Nomad.
///
/// Mirrors the driver-side `preflightDiskPaths` check
/// (`nomad-driver-ch/ch/start_task.go::preflightDiskPaths`) so a
/// missing-or-empty image is surfaced at the CONTROLLER's submit
/// site, not buried in an alloc-failure rollup. Same three checks:
///
///   1. `path` exists (otherwise: dirent never landed).
///   2. `path` is a regular file (otherwise: a directory at the same
///      name, structural surprise).
///   3. `path`'s size > 0 (otherwise: `truncate` produced a sparse
///      file but `mkfs.ext4` was skipped, or a half-written image
///      lingered after a prior failure).
///
/// T-8b-stress Bug 1 trace: the driver's preflight reported "disk[1]
/// <path> does not exist" in 49/60 cold-boot creates. This helper
/// makes that observation symmetrical on the controller side so a
/// repeat of the bug surfaces at the staging step (where the
/// CreateGuard's host_dir teardown can fire cleanly) instead of mid-
/// alloc (where the failure is generic "Failed tasks").
pub(crate) fn assert_disk_image_present(path: &Path) -> Result<(), String> {
    let md = std::fs::metadata(path).map_err(|e| {
        format!(
            "disk image post-stage stat failed: {} ({e}); \
             controller-side parity check for driver preflight",
            path.display()
        )
    })?;
    if md.is_dir() {
        return Err(format!(
            "disk image post-stage check: {} is a directory \
             (must be a file)",
            path.display()
        ));
    }
    if md.len() == 0 {
        return Err(format!(
            "disk image post-stage check: {} is empty (size 0); \
             truncate likely succeeded but mkfs.ext4 was skipped \
             or the image was overwritten by an empty file",
            path.display()
        ));
    }
    Ok(())
}

/// Fsync a directory inode so dirent changes (file creates, renames,
/// deletes) are durable AND visible to peer processes that stat the
/// directory's contents before the kernel's lazy dirent commit. Used
/// after [`create_ext4_image_if_missing`] stages a new disk image so
/// the Nomad client's driver-side stat sees the inode immediately
/// rather than a brief NotFound window.
///
/// Implemented via [`std::fs::File::sync_all`] on a `File::open`-ed
/// directory handle. On Linux this issues `fsync(dirfd)` (the kernel
/// accepts fsync on directory fds since forever — it's the
/// canonical way to flush dirent changes on ext4 / xfs / btrfs).
///
/// Best-effort error semantics: the caller treats failure as a
/// staging error (returns Err), since a non-durable dirent is the
/// exact failure mode T-8b-stress Bug 1 produced.
fn fsync_dir(dir: &Path) -> Result<(), String> {
    let f = std::fs::File::open(dir)
        .map_err(|e| format!("open {}: {e}", dir.display()))?;
    f.sync_all()
        .map_err(|e| format!("fsync {}: {e}", dir.display()))?;
    Ok(())
}

fn random_key32() -> Result<[u8; 32], String> {
    let mut buf = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .map_err(|e| format!("open /dev/urandom: {e}"))?
        .read_exact(&mut buf)
        .map_err(|e| format!("read /dev/urandom: {e}"))?;
    Ok(buf)
}

fn random_nonce() -> Result<String, String> {
    let bytes = random_key32()?;
    Ok(bytes.iter().take(16).map(|b| format!("{b:02x}")).collect())
}

/// Wall-clock seconds since UNIX_EPOCH.
///
/// Crash-loud on clock-before-epoch instead of silently falling back
/// to 0 — a `ts=0` would put every signed RPC's timestamp 56 years
/// in the past, the agent's 5-second skew window would 401
/// permanently, the idle reaper would believe every sandbox was
/// born at the dawn of UNIX and cull them all, and operators would
/// be staring at a UI that says "your sandbox was last used 56
/// years ago." The right behaviour for a clock that's gone backwards
/// to before 1970 is to panic and let the orchestrator surface the
/// problem; the silent-zero fallback hides catastrophic state.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX_EPOCH")
        .as_secs()
}

/// Pre-validate request paths before forwarding to the agent. The
/// agent has its own (stricter) checks via openat2; this gives a
/// cleaner 4xx without consuming a nonce on a doomed request.
/// Identical to the K8s helper — the contract is per-agent, not
/// per-backend, so the rules must match.
fn sanitize_path(p: &str) -> Result<String, String> {
    if p.is_empty() {
        return Err("path is empty".into());
    }
    if p.starts_with('/') {
        return Err("absolute paths not allowed".into());
    }
    for seg in p.split('/') {
        if seg == ".." {
            return Err("'..' segments not allowed".into());
        }
    }
    Ok(p.to_string())
}

/// Validate a typed-id (`<prefix>_<base62-uuidv7>`) at the backend
/// boundary. **Mirror of `k8s.rs::validate_typed_id` — keep them in
/// sync.** Both repeat the HTTP-handler rule as defense-in-depth;
/// cross-module sharing is intentionally avoided in this PR (the
/// deduplication belongs in a follow-up that consolidates the validate
/// helpers once we have ≥ 3 backends needing them).
///
/// Phase-1+2 wire migration: previously this enforced the legacy
/// DNS-1123 charset `[a-z0-9-]{1,50}`, which rejected typed-ids
/// (they contain `_`) and 500'd every real HTTP create.
fn validate_typed_id(
    id: &str,
    expected_prefix: &str,
    what: &'static str,
) -> Result<(), String> {
    zeroship_core::typed_id::parse_with_prefix(id, expected_prefix)
        .map(|_uuid| ())
        .map_err(|e| format!("{what}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── VmIndexAllocator ────────────────────────────────────

    #[test]
    fn vm_alloc_starts_at_floor() {
        let mut a = VmIndexAllocator::new(1, 5);
        assert_eq!(a.alloc().unwrap(), 1);
        assert_eq!(a.alloc().unwrap(), 2);
        assert_eq!(a.alloc().unwrap(), 3);
    }

    #[test]
    fn vm_alloc_reuses_freed_smallest_first() {
        let mut a = VmIndexAllocator::new(10, 20);
        let i1 = a.alloc().unwrap();
        let i2 = a.alloc().unwrap();
        let i3 = a.alloc().unwrap();
        assert_eq!((i1, i2, i3), (10, 11, 12));

        // Free out of order — smallest reused first.
        a.release(i2);
        a.release(i1);
        assert_eq!(a.alloc().unwrap(), 10);
        assert_eq!(a.alloc().unwrap(), 11);
        // After exhausting freed, fall back to next monotonic.
        assert_eq!(a.alloc().unwrap(), 13);
    }

    #[test]
    fn vm_alloc_exhaustion() {
        let mut a = VmIndexAllocator::new(1, 3);
        assert_eq!(a.alloc().unwrap(), 1);
        assert_eq!(a.alloc().unwrap(), 2);
        assert_eq!(a.alloc().unwrap(), 3);
        let err = a.alloc().expect_err("should be exhausted");
        assert!(err.contains("exhausted"), "{err}");
    }

    #[test]
    fn vm_alloc_release_outside_range_is_noop() {
        let mut a = VmIndexAllocator::new(10, 20);
        // Below floor: ignored.
        a.release(5);
        // Above ceil: ignored.
        a.release(25);
        // Allocator is still in its initial state.
        assert_eq!(a.alloc().unwrap(), 10);
    }

    #[test]
    fn vm_alloc_single_element_range() {
        let mut a = VmIndexAllocator::new(7, 7);
        assert_eq!(a.alloc().unwrap(), 7);
        assert!(a.alloc().is_err());
        a.release(7);
        assert_eq!(a.alloc().unwrap(), 7);
    }

    #[test]
    fn vm_alloc_single_element_at_boundaries() {
        // M6: pool of size 1 at the floor (1,1) and ceil (155,155)
        // boundaries — the same edge case at both ends of the
        // controller-validated index range.
        for boundary in [1u16, 155u16] {
            let mut a = VmIndexAllocator::new(boundary, boundary);
            assert_eq!(
                a.alloc().unwrap(),
                boundary,
                "boundary={boundary} first alloc"
            );
            assert!(
                a.alloc().is_err(),
                "boundary={boundary} second alloc must fail"
            );
            a.release(boundary);
            assert_eq!(
                a.alloc().unwrap(),
                boundary,
                "boundary={boundary} reuse after release"
            );
        }
    }

    // ─── CreateGuard cleanup ordering (C1) ──────────────────
    //
    // The fix for C1 moves vm_index release into the detached
    // cleanup task and gates it on Nomad-purge confirmation. We
    // can exercise the no-Nomad-call branch (job_submitted=false ⇒
    // purge_ok=true ⇒ release fires) end-to-end inside a compio
    // runtime — which proves the index makes it back to the pool
    // after Drop runs the detached task.
    #[compio::test]
    async fn create_guard_releases_vm_index_when_no_job_submitted() {
        // Pool of a single index — easiest way to detect leak
        // (next alloc would fail) vs. correct release (next alloc
        // succeeds). Pre-allocate so the pool is empty at the
        // start of the test, then assert the detached cleanup
        // refills it.
        let pool = Arc::new(Mutex::new(VmIndexAllocator::new(7, 7)));
        let allocated =
            pool.lock().unwrap().alloc().expect("first alloc");
        assert_eq!(allocated, 7);
        // Pool is now empty — alloc would fail.
        assert!(pool.lock().unwrap().alloc().is_err());

        {
            let mut g = CreateGuard::new(
                pool.clone(),
                "http://127.0.0.1:1".to_string(), // unreachable
                "zsbx-test-no-purge".to_string(),
                PathBuf::from("/tmp/zsbx-c1-test"),
                Uuid::nil(),
                Duration::ZERO, // r24-A2-S3: no delay in this test
            );
            g.vm_index = Some(allocated);
            g.job_submitted = false; // skip http_delete entirely
            g.host_dir_created = false; // skip rm -rf entirely
            // Drop here triggers the detached cleanup task.
        }
        // The detached task may not have run yet — yield until
        // the index reappears in the pool. Bound by a generous
        // timeout so a regression of "release happens, but on the
        // wrong path" still fails the test rather than hanging.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut released = false;
        while Instant::now() < deadline {
            // Try to alloc; if we get the index back, release was
            // performed by the detached cleanup.
            if let Ok(i) = pool.lock().unwrap().alloc() {
                assert_eq!(i, 7);
                released = true;
                break;
            }
            compio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            released,
            "CreateGuard did not release vm_index back to the pool — \
             C1 regression: a retry-create() for the same user would \
             see a phantom-exhausted pool until orphan-prune"
        );
    }

    #[compio::test]
    async fn create_guard_leaks_vm_index_on_purge_failure() {
        // Pool of a single index. Job submitted but Nomad addr is
        // a guaranteed-unroutable port — the http_delete will fail
        // within the per-call 10s timeout. We assert the index is
        // NOT released within the first ~250ms (the detached task
        // is still mid-http_delete). C1 policy: leak on purge
        // failure rather than risk a tap collision on the retry
        // path. Orphan-prune at next boot reclaims it indirectly.
        //
        // We don't wait for the full failure path — the
        // observable distinction from the no-purge branch is
        // exactly that the index does NOT come back immediately.
        let pool = Arc::new(Mutex::new(VmIndexAllocator::new(13, 13)));
        let allocated =
            pool.lock().unwrap().alloc().expect("first alloc");
        assert_eq!(allocated, 13);
        assert!(pool.lock().unwrap().alloc().is_err());

        {
            let mut g = CreateGuard::new(
                pool.clone(),
                // Non-routable: port 1, no listener.
                "http://127.0.0.1:1".to_string(),
                "zsbx-test-leak".to_string(),
                PathBuf::from("/tmp/zsbx-c1-leak"),
                Uuid::nil(),
                Duration::ZERO, // r24-A2-S3: no delay in this test
            );
            g.vm_index = Some(allocated);
            g.job_submitted = true;
            g.host_dir_created = false;
            // Drop fires; the detached cleanup will spend up to
            // 10s on the http_delete before deciding to leak.
        }
        // Yield once so the detached task actually starts running.
        compio::time::sleep(Duration::from_millis(50)).await;
        // The cleanup task is mid-http_delete with a 10s timeout
        // → the index is still allocated (pool empty). That's the
        // C1 invariant: don't release until the Nomad side
        // confirms the job is gone.
        let immediate = pool.lock().unwrap().alloc();
        assert!(
            immediate.is_err(),
            "CreateGuard released vm_index BEFORE the Nomad purge \
             completed — that's the exact race C1 is guarding against"
        );
    }

    /// R17-A5: post-migration to `detach_isolated`, `CreateGuard::drop`
    /// dispatches the cleanup tail onto a dedicated OS thread with its
    /// own private compio runtime — so Drop is callable without any
    /// ambient compio runtime. Pre-migration, this exact construction
    /// would have hit the `compio::runtime::spawn` panic-catch fallback
    /// (no current runtime → panic → `catch_unwind` → sync vm_index
    /// reclaim). Post-migration, the same cleanup tail runs to
    /// completion on its private runtime and the vm_index reappears
    /// in the pool.
    ///
    /// This is a `#[test]` (NOT `#[compio::test]`) on purpose — the
    /// whole point is that no compio runtime is running on the
    /// thread where Drop fires.
    #[test]
    fn create_guard_drop_runs_without_ambient_compio_runtime() {
        let pool = Arc::new(Mutex::new(VmIndexAllocator::new(42, 42)));
        let allocated = pool.lock().unwrap().alloc().expect("first alloc");
        assert_eq!(allocated, 42);
        assert!(pool.lock().unwrap().alloc().is_err());

        {
            let mut g = CreateGuard::new(
                pool.clone(),
                "http://127.0.0.1:1".to_string(),
                "zsbx-test-no-runtime".to_string(),
                PathBuf::from("/tmp/zsbx-r17a5-test"),
                Uuid::nil(),
                Duration::ZERO, // r24-A2-S3: no delay in this test
            );
            g.vm_index = Some(allocated);
            g.job_submitted = false; // skip http_delete; purge_ok = true
            g.host_dir_created = false; // skip rm -rf
            // Drop fires HERE on a plain std::thread (no compio
            // runtime). `detach_isolated` mints its own runtime on a
            // dedicated OS thread.
        }

        // Poll for the index to reappear (the detached cleanup must
        // make it back to the pool). Bounded so a regression of
        // "Drop did nothing" fails the test rather than hanging.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut released = false;
        while Instant::now() < deadline {
            if let Ok(i) = pool.lock().unwrap().alloc() {
                assert_eq!(i, 42);
                released = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            released,
            "CreateGuard::drop did not release vm_index when called \
             outside an ambient compio runtime — R17-A5 regression: \
             `detach_isolated` should mint its own runtime"
        );
    }

    /// R28-C1: regression test for the vm_index leak that fires
    /// when `CreateGuard::drop` runs under `detach_isolated`'s
    /// short-lived runtime AND `release_delay > 0`.
    ///
    /// Pre-fix behaviour: `spawn_delayed_release` planted a
    /// `compio::runtime::spawn(sleep(delay).then(release)).detach()`
    /// task on the SAME short-lived runtime the cleanup future was
    /// running on. The cleanup future returns Ready before the
    /// delay elapses → `block_on` returns → `Runtime::Drop` runs
    /// `Scheduler::clear()` (compio 0.11) → the pending timer task
    /// is dropped → release never fires → vm_index leaks until the
    /// next controller-boot orphan prune. The
    /// `create_guard_releases_vm_index_when_no_job_submitted` and
    /// `create_guard_drop_runs_without_ambient_compio_runtime`
    /// tests above do NOT catch this because they use
    /// `Duration::ZERO`, which collapses the spawn-and-sleep to a
    /// near-synchronous shape that fits inside the post-Ready
    /// `self.run()` cycle.
    ///
    /// Post-fix behaviour: the delay + release is inlined into the
    /// cleanup future itself, so `block_on` cannot return until the
    /// release has happened.
    ///
    /// This is a `#[test]` (NOT `#[compio::test]`) so the ambient
    /// runtime is plain `std::thread`, exactly mirroring the
    /// `detach_isolated` dispatch shape that triggers the bug.
    #[test]
    fn create_guard_drop_releases_vm_index_under_isolated_runtime() {
        // Single-element pool so a leak is observable as a failed
        // alloc and a correct release as a successful one.
        let pool = Arc::new(Mutex::new(VmIndexAllocator::new(99, 99)));
        let allocated = pool.lock().unwrap().alloc().expect("first alloc");
        assert_eq!(allocated, 99);
        assert!(pool.lock().unwrap().alloc().is_err());

        {
            let mut g = CreateGuard::new(
                pool.clone(),
                "http://127.0.0.1:1".to_string(), // unreachable
                "zsbx-test-r28-c1".to_string(),
                PathBuf::from("/tmp/zsbx-r28-c1-test"),
                Uuid::nil(),
                // Non-zero delay is the load-bearing detail: the
                // pre-fix bug was specifically that a delay > 0
                // gave the short-lived runtime time to drop before
                // the timer fired. 100 ms is long enough to outlive
                // the post-Ready `self.run()` cycle (well under 1
                // ms) yet short enough that the test wall-time
                // stays cheap.
                Duration::from_millis(100),
            );
            g.vm_index = Some(allocated);
            g.job_submitted = false; // skip http_delete; purge_ok = true
            g.host_dir_created = false; // skip rm -rf
            // Drop fires HERE on a plain std::thread. Inside Drop,
            // `detach_isolated("create-rollbk", …)` spawns a fresh
            // OS thread + private compio runtime that block_on's
            // the cleanup future. With the fix in place the
            // inlined `sleep(100ms).await; release` runs to
            // completion before `block_on` returns; without the
            // fix the detached timer is discarded when the
            // private runtime drops.
        }

        // Poll up to 5 s for the index to reappear. The fix means
        // it should show up ~100 ms after Drop (delay + OS thread
        // spawn + runtime mint). Without the fix it never does.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut released = false;
        while Instant::now() < deadline {
            if let Ok(i) = pool.lock().unwrap().alloc() {
                assert_eq!(i, 99);
                released = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            released,
            "CreateGuard::drop did not release vm_index when \
             release_delay > 0 and Drop runs under a short-lived \
             detach_isolated runtime — R28-C1 regression: the \
             delayed-release task is being planted on a runtime \
             that drops before the timer fires"
        );
    }

    #[test]
    fn vm_alloc_release_is_idempotent() {
        // M8: releasing an index that's already in `freed` should
        // not corrupt state — the same index must NOT be handed out
        // twice on the next two allocs.
        let mut a = VmIndexAllocator::new(1, 5);
        let i = a.alloc().unwrap();
        a.release(i);
        a.release(i); // double-release: BTreeSet dedups, no panic
        a.release(i); // triple, for good measure
        let j1 = a.alloc().unwrap();
        let j2 = a.alloc().unwrap();
        assert_eq!(j1, i, "first realloc reuses the freed index");
        assert_ne!(
            j2, j1,
            "second alloc must NOT hand back the same index — \
             double-release must not double-insert"
        );
    }

    // ─── T-8b-stress-r8 r24-A2-S3 / r29-A2 delayed release ────

    /// Zero-delay path: `release_vm_index_after` collapses to a
    /// synchronous release inside the calling task — the slot is in
    /// freed by the time the future returns Ready.
    #[compio::test]
    async fn release_vm_index_after_with_zero_delay_releases_immediately() {
        let pool = Arc::new(Mutex::new(VmIndexAllocator::new(11, 11)));
        let i = pool.lock().unwrap().alloc().expect("alloc");
        assert_eq!(i, 11);

        VmIndexAllocator::release_vm_index_after(
            Arc::clone(&pool),
            i,
            Duration::ZERO,
            "test-zero-delay",
            Uuid::nil(),
        )
        .await;

        assert!(
            pool.lock().unwrap().freed_for_test().contains(&11),
            "release_vm_index_after(ZERO) did not release inline"
        );
    }

    /// Non-zero delay path: the release waits the configured delay
    /// before releasing. We pin (a) the slot is NOT in freed before
    /// the delay elapses, and (b) IS in freed after. The inline-
    /// await shape means we drive the future via a sibling spawn so
    /// we can observe the "halfway" state from the main task.
    #[compio::test]
    async fn release_vm_index_after_honors_configured_delay() {
        let pool = Arc::new(Mutex::new(VmIndexAllocator::new(22, 22)));
        let i = pool.lock().unwrap().alloc().expect("alloc");
        assert_eq!(i, 22);

        let delay = Duration::from_millis(200);
        // Spawn the release on a sibling task (joinable) so the main
        // task can poll the allocator while the delay elapses. This
        // mirrors how `spawn_delayed_release_in_worker` would land
        // in a long-lived caller — the Task handle gives us the
        // joinable shape needed for the halfway-check.
        let task = VmIndexAllocator::spawn_delayed_release_in_worker(
            Arc::clone(&pool),
            i,
            delay,
            "test-honor-delay",
            Uuid::nil(),
        );

        // Before the delay elapses, slot 22 MUST still be allocated.
        compio::time::sleep(delay / 2).await;
        let halfway_present = pool.lock().unwrap().freed_for_test().contains(&22);
        assert!(
            !halfway_present,
            "release_vm_index_after fired before the configured delay \
             ({delay:?} elapsed 50%) — r24-A2-S3 release MUST be deferred"
        );

        // Await the task: by the time it returns, the release has
        // happened. The Result wrapper is the compio spawn panic-
        // catch wrapper; `release_vm_index_after` cannot panic so
        // Ok is the only observed shape today.
        task.await.expect("release_vm_index_after must not panic");
        assert!(
            pool.lock().unwrap().freed_for_test().contains(&22),
            "release_vm_index_after did not fire after delay ({delay:?}) elapsed"
        );
    }

    /// r29-A2: `spawn_delayed_release_in_worker` returns a typed
    /// `compio::runtime::Task<()>` JoinHandle. The contract is "if
    /// you don't await or detach, the timer is bound to the Task
    /// you hold." This test pins that the return type IS joinable
    /// and that awaiting it observes the release deterministically
    /// — the property the deleted `spawn_delayed_release`
    /// fire-and-forget shape did NOT give callers (R28-C1 / R29-C1
    /// both surfaced because callers couldn't observe the timer's
    /// fate, only hope the current runtime outlived it).
    #[compio::test]
    async fn spawn_delayed_release_in_worker_returns_joinable_task() {
        let pool = Arc::new(Mutex::new(VmIndexAllocator::new(33, 33)));
        let i = pool.lock().unwrap().alloc().expect("alloc");
        assert_eq!(i, 33);

        let task = VmIndexAllocator::spawn_delayed_release_in_worker(
            Arc::clone(&pool),
            i,
            Duration::from_millis(50),
            "test-joinable",
            Uuid::nil(),
        );
        // Type-level assertion via shadowing: the returned value
        // MUST be `compio::runtime::Task<Result<(), Box<dyn Any+Send>>>`.
        // The `Result` wrapper is compio's panic-catch shape on
        // `spawn`. If a future refactor changes this contract the
        // compiler catches it here before the rustdoc drifts.
        let task: compio::runtime::Task<
            Result<(), Box<dyn std::any::Any + Send>>,
        > = task;

        // Joining the task waits for the release to complete; the
        // slot is then guaranteed to be in `freed` without a poll.
        task.await.expect("release_vm_index_after must not panic");
        assert!(
            pool.lock().unwrap().freed_for_test().contains(&33),
            "spawn_delayed_release_in_worker: awaiting the Task did \
             not observe a completed release"
        );
    }

    /// R29-C1 regression: a release dispatched from inside a
    /// `detach_isolated` body (short-lived private compio runtime)
    /// MUST complete before `block_on` returns. The pre-r29
    /// `spawn_delayed_release` planted a detached timer on the same
    /// short-lived runtime; `Scheduler::clear` discarded the timer
    /// at runtime drop, leaking the slot.
    ///
    /// This mirrors the R28-C1 `CreateGuard::drop` regression test
    /// but exercises the helper that's reachable from
    /// `admin_handlers.rs::snap-teardown-<tail>` →
    /// `teardown_source_for_snapshot` → `stop_preserving_state` →
    /// `stop_inner` (which now awaits `release_vm_index_after`
    /// inline). We don't need the full stack; what we're pinning is
    /// the helper's behaviour: when invoked inside a detached
    /// future on a short-lived runtime with `delay > 0`, the
    /// release MUST still observe before the runtime drops.
    ///
    /// `#[test]` (not `#[compio::test]`) so the outer thread is
    /// plain `std::thread`, exactly the shape `detach_isolated`
    /// uses (a fresh OS thread + private runtime per dispatch).
    #[test]
    fn release_vm_index_after_survives_short_lived_runtime() {
        let pool = Arc::new(Mutex::new(VmIndexAllocator::new(77, 77)));
        let allocated = pool.lock().unwrap().alloc().expect("alloc");
        assert_eq!(allocated, 77);
        assert!(pool.lock().unwrap().alloc().is_err());

        // Non-zero delay is the load-bearing detail: the pre-fix
        // R29-C1 bug fired specifically when `delay > 0` gave the
        // short-lived runtime time to drop before a detached timer
        // task could run. 100 ms is enough to outlive the post-Ready
        // `block_on` cycle yet short enough for cheap test wall.
        let pool_for_fut = Arc::clone(&pool);
        crate::detach::detach_isolated(
            "test-r29-c1",
            move || async move {
                VmIndexAllocator::release_vm_index_after(
                    pool_for_fut,
                    allocated,
                    Duration::from_millis(100),
                    "test-r29-c1",
                    Uuid::nil(),
                )
                .await;
            },
        );

        // Poll up to 5 s for the index to reappear. With the
        // inline-await fix it shows up ~100 ms after dispatch (delay
        // + OS-thread spawn + runtime mint). Without the fix (the
        // pre-r29 `.detach()`-onto-current-runtime shape) it never
        // does — the timer task is discarded when the private
        // runtime drops.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut released = false;
        while Instant::now() < deadline {
            if let Ok(i) = pool.lock().unwrap().alloc() {
                assert_eq!(i, 77);
                released = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            released,
            "R29-C1 regression: release dispatched from inside a \
             `detach_isolated` body with delay > 0 did not observe \
             before the short-lived runtime dropped — `Scheduler::clear` \
             discarded the timer task"
        );
    }

    // ─── wait_for_job_gone alloc-terminal predicate (I2) ─────

    fn alloc(status: &str) -> serde_json::Value {
        serde_json::json!({"ClientStatus": status})
    }

    #[test]
    fn allocs_terminal_empty_or_none_is_ok() {
        assert!(allocs_all_terminal(None));
        let empty: Vec<serde_json::Value> = Vec::new();
        assert!(allocs_all_terminal(Some(&empty)));
    }

    #[test]
    fn allocs_terminal_all_terminal_states_pass() {
        let all = vec![alloc("complete"), alloc("failed"), alloc("lost")];
        assert!(allocs_all_terminal(Some(&all)));
    }

    #[test]
    fn allocs_terminal_running_blocks() {
        let mixed = vec![alloc("complete"), alloc("running")];
        assert!(!allocs_all_terminal(Some(&mixed)));
    }

    #[test]
    fn allocs_terminal_pending_blocks() {
        let v = vec![alloc("pending")];
        assert!(!allocs_all_terminal(Some(&v)));
    }

    #[test]
    fn allocs_terminal_unknown_status_blocks() {
        // Defensive: an unrecognised string is not treated as
        // terminal (avoids races on future Nomad alloc-status
        // additions).
        let v = vec![alloc("future-status-we-dont-know")];
        assert!(!allocs_all_terminal(Some(&v)));
    }

    // ─── T-8b-stress-r2 v34: verbatim driver-msg propagation ────
    //
    // `extract_failed_task_event_msgs` walks the alloc JSON's
    // `TaskStates[<task>].Events[]` chain to harvest the per-task
    // DisplayMessage from any task marked `Failed: true`. Pre-v34 the
    // controller surfaced only Nomad's generic `ClientDescription`
    // ("Failed tasks") and operators had to SSH the worker to read
    // the actionable driver-side error. v34 threads the verbatim msg
    // into the wire envelope.

    #[test]
    fn extract_failed_task_event_msgs_collates_driver_failure_text() {
        // Shape mirrors the verbatim T-8b-stress-r2 review excerpt:
        // a single failed `ch` task with a Driver Failure event
        // carrying the `disk[1] /var/zeroship/ch/<uuid>/workspace.img
        // does not exist (controller must stage before spawn)` text.
        let alloc = serde_json::json!({
            "ClientStatus": "failed",
            "ClientDescription": "Failed tasks",
            "TaskStates": {
                "ch": {
                    "State": "dead",
                    "Failed": true,
                    "Events": [
                        {
                            "Type": "Driver",
                            "DisplayMessage": "Downloading artifacts"
                        },
                        {
                            "Type": "Driver Failure",
                            "DisplayMessage": "rpc error: code = Unknown desc = ch: StartTask: disk[1] /var/zeroship/ch/019e5a6bf7e67280953fc425c2fc3487/workspace.img does not exist (controller must stage before spawn)"
                        }
                    ]
                }
            }
        });
        let msgs = extract_failed_task_event_msgs(&alloc);
        assert_eq!(msgs.len(), 1, "single failed task → one entry; got {msgs:?}");
        let m = &msgs[0];
        assert!(m.starts_with("ch: "), "task name prefix missing: {m:?}");
        assert!(
            m.contains("workspace.img does not exist"),
            "verbatim driver msg lost: {m:?}"
        );
        assert!(
            m.contains("controller must stage before spawn"),
            "verbatim driver msg lost: {m:?}"
        );
    }

    #[test]
    fn extract_failed_task_event_msgs_returns_empty_on_no_failed_tasks() {
        // Healthy alloc: every TaskState has Failed=false. The helper
        // returns an empty vec so the caller falls back to the
        // ClientDescription path.
        let alloc = serde_json::json!({
            "ClientStatus": "running",
            "TaskStates": {
                "ch": {
                    "State": "running",
                    "Failed": false,
                    "Events": [
                        {"Type": "Started", "DisplayMessage": "Task started by user"}
                    ]
                }
            }
        });
        let msgs = extract_failed_task_event_msgs(&alloc);
        assert!(msgs.is_empty(), "no failed task → empty list; got {msgs:?}");
    }

    #[test]
    fn extract_failed_task_event_msgs_returns_empty_when_taskstates_missing() {
        // Defensive: malformed / partial alloc JSON. The helper must
        // not panic — caller's fallback path is the ClientDescription.
        let alloc = serde_json::json!({
            "ClientStatus": "failed",
            "ClientDescription": "Failed tasks"
            // no TaskStates at all
        });
        let msgs = extract_failed_task_event_msgs(&alloc);
        assert!(msgs.is_empty(), "missing TaskStates → empty; got {msgs:?}");
    }

    #[test]
    fn extract_failed_task_event_msgs_skips_failed_task_with_empty_display() {
        // A failed task with no DisplayMessage on any event yields no
        // entry (caller's fallback handles the empty-list case). This
        // is defensive — a buggy driver could emit a Failed=true state
        // with empty events; we don't want to surface a misleading
        // "ch: " (just the task name with no message) in that case.
        let alloc = serde_json::json!({
            "ClientStatus": "failed",
            "TaskStates": {
                "ch": {
                    "Failed": true,
                    "Events": [
                        {"Type": "Driver Failure", "DisplayMessage": ""}
                    ]
                }
            }
        });
        let msgs = extract_failed_task_event_msgs(&alloc);
        assert!(msgs.is_empty(), "empty DisplayMessage → no entry; got {msgs:?}");
    }

    #[test]
    fn extract_failed_task_event_msgs_caps_oversized_message() {
        // A pathological driver emitting a multi-MB error must not
        // bloat the wire envelope. The 2 KiB per-task cap kicks in
        // with a "…(truncated)" sentinel.
        let big = "X".repeat(10_000);
        let alloc = serde_json::json!({
            "ClientStatus": "failed",
            "TaskStates": {
                "ch": {
                    "Failed": true,
                    "Events": [
                        {"Type": "Driver Failure", "DisplayMessage": big}
                    ]
                }
            }
        });
        let msgs = extract_failed_task_event_msgs(&alloc);
        assert_eq!(msgs.len(), 1);
        let m = &msgs[0];
        assert!(m.contains("…(truncated)"), "truncation sentinel missing: starts={:?}", &m[..50.min(m.len())]);
        // Bounded: task-name prefix (~4 chars) + 2 KiB body + sentinel
        // (~14 chars). Tolerate a small slack for the prefix.
        assert!(
            m.len() < 2200,
            "truncation cap not enforced; got len={}",
            m.len()
        );
    }

    /// r27-M2: a driver-emitted error containing a multi-byte UTF-8
    /// character that straddles the PER_TASK_CAP byte boundary must
    /// NOT panic. Pre-fix, `&trimmed[..PER_TASK_CAP]` byte-indexed
    /// without a char-boundary check; if the boundary landed
    /// mid-codepoint the slice constructor would panic. The fix
    /// mirrors `sanitize_error_message`'s `is_char_boundary`
    /// decrement loop: walk down byte-by-byte until the boundary
    /// lands on a valid char.
    ///
    /// Construction: build a body whose total byte length crosses
    /// 2048 AND whose 2048th byte is the middle of a 3-byte UTF-8
    /// codepoint (`€` = `\xE2\x82\xAC`). Specifically: 2047 ASCII
    /// `X`s + `€` + filler. Byte 2048 is the SECOND byte of `€`'s
    /// 3-byte encoding, which is NOT a char boundary. The cap must
    /// decrement to 2047 (the start of `€`) and slice there.
    #[test]
    fn extract_failed_task_event_msgs_truncate_handles_multibyte_at_boundary() {
        // 2047 ASCII filler so the next codepoint starts at byte 2047.
        // Then `€` (3 bytes: E2 82 AC) occupies bytes 2047-2049, so
        // byte 2048 lands MID-codepoint. Append enough trailing
        // bytes to push total length well past PER_TASK_CAP so the
        // truncation branch fires.
        let mut body = "X".repeat(2047);
        body.push('€'); // bytes 2047-2049
        body.push_str(&"Y".repeat(1000)); // pad past the cap
        // Sanity check the fixture: the boundary IS mid-codepoint.
        assert!(
            !body.is_char_boundary(2048),
            "fixture must put a non-boundary at byte 2048; \
             otherwise the test doesn't exercise the fix",
        );
        assert!(
            body.len() > 2048,
            "fixture must exceed PER_TASK_CAP so truncation fires",
        );

        let alloc = serde_json::json!({
            "ClientStatus": "failed",
            "TaskStates": {
                "ch": {
                    "Failed": true,
                    "Events": [
                        {"Type": "Driver Failure", "DisplayMessage": body}
                    ]
                }
            }
        });
        // The whole call would panic pre-fix at the `&trimmed[..2048]`
        // line because byte 2048 is not a char boundary. Post-fix it
        // succeeds and the slice ends at byte 2047 (the start of `€`).
        let msgs = extract_failed_task_event_msgs(&alloc);
        assert_eq!(msgs.len(), 1);
        let m = &msgs[0];
        // The output must contain the truncation sentinel.
        assert!(
            m.contains("…(truncated)"),
            "expected truncation sentinel; got starts={:?}",
            &m[..50.min(m.len())],
        );
        // The truncated body ends just before `€` (byte 2047), so the
        // last non-sentinel character of the body is the ASCII `X`,
        // never a partial codepoint. Confirm `€` is NOT in the
        // pre-sentinel slice — if char-boundary decrement worked,
        // the body cut at 2047 (well below 2049 where `€` ends).
        let sentinel_at = m
            .rfind("…(truncated)")
            .expect("sentinel must be present");
        let body_slice = &m[..sentinel_at];
        // Body slice = "ch: " + 2047 X's. No `€` in there.
        assert!(
            !body_slice.contains('€'),
            "decrement must have cut at 2047 (before €); got body containing €",
        );
    }
    //
    // The r3 regression: 47/47 CREATE failures surfaced as the generic
    // "Unhealthy because of failed task" envelope despite the v34 commit
    // having added `extract_failed_task_event_msgs`. The root cause: the
    // extractor walked Events[] in REVERSE and took the first non-empty
    // DisplayMessage — which is the LAST event in the array. Nomad emits
    // `Alloc Unhealthy` AFTER `Driver Failure` in the temporal ordering;
    // reverse-walk picked the useless Nomad-side `Alloc Unhealthy` event
    // and masked the load-bearing driver-emitted `StartTask: workspace.img
    // does not exist ...` text.
    //
    // The fix prefers events whose Type is in the diagnostic allow-list
    // (Driver Failure, Task Setup Failure, ...) and falls back to the
    // reverse-walk only if none are present.

    #[test]
    fn extract_failed_task_event_msgs_prefers_driver_failure_over_alloc_unhealthy() {
        // VERBATIM CLUSTER SHAPE from T-8b-stress-r3 (review: docs/reviews/
        // sandbox-snapshot-restore-cluster-2026-05-25-T8b-stress-r3.md).
        // Nomad emits Events[] in temporal order: Driver Failure first,
        // Alloc Unhealthy last. Pre-r3 reverse-walk picked Alloc Unhealthy
        // and the cluster envelope was useless.
        let alloc = serde_json::json!({
            "ClientStatus": "failed",
            "ClientDescription": "Failed tasks",
            "TaskStates": {
                "ch": {
                    "State": "dead",
                    "Failed": true,
                    "Events": [
                        {"Type": "Received", "DisplayMessage": "Task received by client"},
                        {"Type": "Task Setup", "DisplayMessage": "Building Task Directory"},
                        {
                            "Type": "Driver Failure",
                            "DisplayMessage": "rpc error: code = Unknown desc = ch: StartTask: disk[1] /var/zeroship/ch/019e5a6bf7e67280953fc425c2fc3487/workspace.img does not exist (controller must stage before spawn)"
                        },
                        {"Type": "Restart Signaled", "DisplayMessage": "Policy allows no restarts"},
                        {"Type": "Alloc Unhealthy", "DisplayMessage": "Unhealthy because of failed task"}
                    ]
                }
            }
        });
        let msgs = extract_failed_task_event_msgs(&alloc);
        assert_eq!(msgs.len(), 1, "single failed task → one entry; got {msgs:?}");
        let m = &msgs[0];
        assert!(
            m.contains("workspace.img does not exist"),
            "verbatim driver msg lost — reverse-walk picked Alloc Unhealthy instead of Driver Failure: {m:?}"
        );
        assert!(
            m.contains("controller must stage before spawn"),
            "verbatim driver msg lost — reverse-walk picked Alloc Unhealthy instead of Driver Failure: {m:?}"
        );
        // Negative assertion: the generic Nomad envelope MUST NOT be
        // the chosen message; if it is, we've regressed back to the
        // r2/r3 silent no-op.
        assert!(
            !m.contains("Unhealthy because of failed task"),
            "the diagnostic preference is silently no-op — picked Alloc Unhealthy instead of Driver Failure: {m:?}"
        );
    }

    #[test]
    fn extract_failed_task_event_msgs_falls_back_to_last_event_when_no_diagnostic_type() {
        // Defensive: if Nomad's event chain has no Driver Failure /
        // Task Setup Failure event (e.g., the driver crashed without
        // emitting a typed event), the pre-r3 behaviour of taking the
        // last non-empty DisplayMessage is the correct fallback. We
        // must not return empty.
        let alloc = serde_json::json!({
            "ClientStatus": "failed",
            "TaskStates": {
                "ch": {
                    "Failed": true,
                    "Events": [
                        {"Type": "Received", "DisplayMessage": "Task received by client"},
                        {"Type": "Restart Signaled", "DisplayMessage": "Policy allows no restarts"},
                        {"Type": "Alloc Unhealthy", "DisplayMessage": "Unhealthy because of failed task"}
                    ]
                }
            }
        });
        let msgs = extract_failed_task_event_msgs(&alloc);
        assert_eq!(msgs.len(), 1, "fallback path lost the entry; got {msgs:?}");
        assert!(
            msgs[0].contains("Unhealthy because of failed task"),
            "fallback path didn't pick the last non-empty event; got {msgs:?}"
        );
    }

    #[test]
    fn extract_failed_task_event_msgs_picks_first_driver_failure_when_multiple() {
        // Edge case: if the driver emits MULTIPLE Driver Failure events
        // (e.g., retry budget > 1; not currently configured for the ch
        // driver but defensive), the FIRST one is the root cause and
        // subsequent ones are retry-cascade side effects. Pin this.
        let alloc = serde_json::json!({
            "ClientStatus": "failed",
            "TaskStates": {
                "ch": {
                    "Failed": true,
                    "Events": [
                        {"Type": "Driver Failure", "DisplayMessage": "root cause: tap collision EBUSY"},
                        {"Type": "Restart Signaled", "DisplayMessage": "Restarting"},
                        {"Type": "Driver Failure", "DisplayMessage": "cascade: tap still busy"},
                        {"Type": "Alloc Unhealthy", "DisplayMessage": "Unhealthy because of failed task"}
                    ]
                }
            }
        });
        let msgs = extract_failed_task_event_msgs(&alloc);
        assert_eq!(msgs.len(), 1);
        assert!(
            msgs[0].contains("root cause"),
            "must pick FIRST Driver Failure (root cause), got: {msgs:?}"
        );
        assert!(
            !msgs[0].contains("cascade"),
            "must NOT pick the cascade Driver Failure, got: {msgs:?}"
        );
    }

    #[test]
    fn is_diagnostic_event_type_matches_known_types() {
        // Positive cases — these must surface the driver-side error.
        assert!(is_diagnostic_event_type("Driver Failure"));
        assert!(is_diagnostic_event_type("Task Setup Failure"));
        assert!(is_diagnostic_event_type("driver failure")); // case-insensitive
        assert!(is_diagnostic_event_type("  Driver Failure  ")); // trim
        // Negative cases — these are envelope/orchestration events.
        assert!(!is_diagnostic_event_type("Alloc Unhealthy"));
        assert!(!is_diagnostic_event_type("Restart Signaled"));
        assert!(!is_diagnostic_event_type("Terminated"));
        assert!(!is_diagnostic_event_type("Killing"));
        assert!(!is_diagnostic_event_type("Killed"));
        assert!(!is_diagnostic_event_type("Started"));
        assert!(!is_diagnostic_event_type("Task Setup")); // happy-path setup, NOT failure
        assert!(!is_diagnostic_event_type("Received"));
        assert!(!is_diagnostic_event_type(""));
    }

    // ─── C3: HTTP error tracked in poll-loop timeout messages ──
    //
    // Without these the timeout message lies: a Nomad-unreachable
    // outage produces the same "alloc never reached running …
    // last status=<no allocs>" message as a real scheduling
    // problem, collapsing two completely different triage paths
    // into one. The fix tracks `last_http_err` distinctly and
    // appends it to the timeout text.

    #[compio::test]
    async fn wait_for_alloc_running_surfaces_unreachability() {
        // 127.0.0.1:1 is reserved/unbound on standard hosts → ureq
        // returns a transport error within the per-call 5s budget.
        // Use a tiny outer timeout so the test finishes fast.
        let err = wait_for_alloc_running(
            "http://127.0.0.1:1",
            "zsbx-c3-test",
            Duration::from_millis(400),
        )
        .await
        .expect_err("must time out");
        // The error MUST mention reachability so an operator
        // doesn't go hunting for an alloc-scheduling bug when the
        // real problem is that Nomad is down.
        assert!(
            err.contains("reachability") || err.contains("HTTP error"),
            "C3 regression: timeout error did not surface HTTP \
             reachability hint; got {err:?}"
        );
    }

    #[compio::test]
    async fn wait_for_job_gone_surfaces_unreachability() {
        let err = wait_for_job_gone(
            "http://127.0.0.1:1",
            "zsbx-c3-jg-test",
            Duration::from_millis(400),
        )
        .await
        .expect_err("must time out");
        assert!(
            err.contains("reachability") || err.contains("HTTP error"),
            "C3 regression: wait_for_job_gone timeout error did not \
             surface HTTP reachability hint; got {err:?}"
        );
    }

    // ─── State-map collision (C1) ────────────────────────────
    //
    // The fix in `try_create` step 8 uses `HashMap::entry` to refuse
    // overwriting an existing sandbox_id. We can't drive the full
    // `try_create` path from a unit test (no Nomad), but we can
    // verify the entry-API contract directly: a duplicate insert
    // must NOT overwrite the prior value.
    #[test]
    fn state_map_entry_api_refuses_overwrite() {
        use std::collections::hash_map::Entry;
        let mut m: HashMap<Uuid, &'static str> = HashMap::new();
        let id = Uuid::nil();
        // First insert: vacant → take.
        match m.entry(id) {
            Entry::Vacant(slot) => {
                slot.insert("first");
            }
            Entry::Occupied(_) => panic!("first insert should be vacant"),
        }
        // Second insert with same id: occupied → must NOT overwrite.
        let mut would_overwrite = false;
        match m.entry(id) {
            Entry::Vacant(_) => {
                would_overwrite = true;
            }
            Entry::Occupied(o) => {
                // Existing entry is preserved unchanged.
                assert_eq!(*o.get(), "first");
            }
        }
        assert!(
            !would_overwrite,
            "entry-API must report Occupied on duplicate id"
        );
        // Final state is the original — proves no leak of prior
        // bookkeeping (vm_index/host_dir/job_id in the real type).
        assert_eq!(m.get(&id), Some(&"first"));
    }

    // ─── Nomad job spec ──────────────────────────────────────

    fn make_cfg() -> SandboxConfig {
        let cfg = SandboxConfig {
            port: 9091,
            token: crate::config::ApiToken::new("x"),
            backend: "nomad-ch".into(),
            image: "img".into(),
            workspace_root: PathBuf::from("/var/zeroship/projects"),
            network: "n".into(),
            memory_mb: 1024,
            cpus: 2.0,
            idle_timeout_secs: 1800,
            max_lifetime_secs: 28800,
            auto_pull: false,
            k8s: crate::config::K8sConfig {
                namespace: "default".into(),
                image: "i".into(),
                runtime_class: "kvm-sandbox".into(),
                ready_timeout_secs: 120,
                use_port_forward: false,
                port_forward_start: 18000,
                user_home_size: "5Gi".into(),
                user_home_storage_class: None,
                startup_orphan_cleanup: false,
            },
            nomad_ch: crate::config::NomadCHConfig {
                nomad_addr: "http://127.0.0.1:4646".into(),
                datacenter: "dc1".into(),
                runtime_dir: PathBuf::from("/var/lib/zeroship/ch"),
                host_state_dir: PathBuf::from("/var/zeroship/ch"),
                user_home_dir_root: PathBuf::from("/var/zeroship/ch/users"),
                vm_index_floor: 1,
                vm_index_ceil: 155,
                alloc_running_timeout_secs: 120,
                agent_livez_timeout_secs: 30,
                host_fence_timeout_secs: 30,
                startup_orphan_cleanup: false,
                subnet_second_octet: 99,
                vm_index_release_delay_secs: 0, // r24-A2-S3: test default 0
                nomad_stop_concurrency: 16,     // r30-A1: prod default
            },
            create_retry_max: 2,
            create_retry_total_timeout_secs: 90,
            snapshot_enabled: false,
            snapshot_l1_root: std::path::PathBuf::from("/var/zeroship/ch/snapshots"),
            snapshot_use_gcs: false,
            snapshot_gcs_bucket: None,
            snapshot_root_kek_path: None,
            workspace_image_size_gb: 20,
            driver_stages_disk_images: false,
        };
        cfg
    }

    #[test]
    fn cpus_boot_rounds_up_and_floors_at_1() {
        assert_eq!(cpus_boot(2.0), 2);
        assert_eq!(cpus_boot(1.5), 2);
        assert_eq!(cpus_boot(0.1), 1);
        assert_eq!(cpus_boot(0.0), 1);
        assert_eq!(cpus_boot(-1.0), 1);
        assert_eq!(cpus_boot(8.0), 8);
    }

    #[test]
    fn cpus_boot_handles_nan_and_inf() {
        // M7: NaN / Inf must not produce 0 or saturating-cast garbage.
        assert_eq!(cpus_boot(f32::NAN), 1);
        assert_eq!(cpus_boot(f32::INFINITY), 1);
        assert_eq!(cpus_boot(f32::NEG_INFINITY), 1);
    }

    #[test]
    fn nomad_cpu_advisory_is_500_mhz() {
        // Bin-packing-only advisory; constant by design. See the doc
        // on `NOMAD_CPU_MHZ_ADVISORY` for why scaling with cfg.cpus
        // was removed (artificial 20-VM/worker placement cap).
        assert_eq!(NOMAD_CPU_MHZ_ADVISORY, 500);
    }

    #[test]
    fn nomad_job_json_basic_shape() {
        let cfg = make_cfg();
        // virtio-blk pivot (bug #11): build_nomad_job_json takes
        // workspace.img + home.img paths + pubkey hex. The driver
        // attaches these as virtio-blk and injects the pubkey on
        // the cmdline. The fixture pubkey is 64 hex chars (32 bytes).
        let v = build_nomad_job_json_with(
            "zsbx-abc",
            &cfg,
            7,
            Path::new("/var/zeroship/ch/abc/workspace.img"),
            Path::new("/var/zeroship/ch/users/alice/home.img"),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "alice",
            "proj1",
            "abc",
            None,
            None, // r3-A: no node pin in tests
        );
        let job = &v["Job"];
        assert_eq!(job["ID"], "zsbx-abc");
        assert_eq!(job["Name"], "zsbx-abc");
        assert_eq!(job["Type"], "service");
        assert_eq!(job["Datacenters"][0], "dc1");
        assert_eq!(job["Meta"]["zeroship.user"], "alice");
        assert_eq!(job["Meta"]["zeroship.vm_index"], "7");

        let group = &job["TaskGroups"][0];
        assert_eq!(group["Count"], 1);
        assert_eq!(group["RestartPolicy"]["Attempts"], 0);
        assert_eq!(group["RestartPolicy"]["Mode"], "fail");
        assert_eq!(group["ReschedulePolicy"]["Attempts"], 0);

        let task = &group["Tasks"][0];
        // T-7 + T-8 cutover: ch driver is the unconditional default.
        assert_eq!(task["Driver"], "ch");
        // Config carries typed fields; raw_exec `command` MUST NOT appear.
        assert!(
            task["Config"]["command"].is_null(),
            "ch driver Config must NOT carry raw_exec `command` field"
        );
        assert_eq!(task["Env"]["ZSBX_VM_INDEX"], "7");
        // M4: ZSBX_HERE renamed to ZSBX_ARTIFACT_DIR; verify the
        // new name is in the env block AND the legacy name is NOT
        // (so a future ad-hoc deploy that references the old name
        // fails fast instead of silently picking up nothing).
        assert_eq!(
            task["Env"]["ZSBX_ARTIFACT_DIR"],
            "/var/lib/zeroship/ch"
        );
        assert!(
            task["Env"]["ZSBX_HERE"].is_null(),
            "ZSBX_HERE should be gone (renamed to ZSBX_ARTIFACT_DIR)"
        );
        assert_eq!(task["Env"]["ZSBX_RUNTIME"], "${NOMAD_TASK_DIR}");
        // virtio-blk pivot (bug #11): the three virtio-fs share-dir
        // env vars are gone; we now emit two image paths and the
        // pubkey hex. Assert both the new names appear AND the old
        // names do NOT — so a future ad-hoc deploy that references
        // ZSBX_KEYS_DIR / ZSBX_WORKSPACE_DIR / ZSBX_USER_HOME_DIR
        // fails fast instead of silently picking up nothing.
        assert_eq!(
            task["Env"]["ZSBX_WORKSPACE_IMG"],
            "/var/zeroship/ch/abc/workspace.img"
        );
        assert_eq!(
            task["Env"]["ZSBX_USER_HOME_IMG"],
            "/var/zeroship/ch/users/alice/home.img"
        );
        assert_eq!(
            task["Env"]["ZSBX_PUBKEY_HEX"],
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
        for legacy in ["ZSBX_KEYS_DIR", "ZSBX_WORKSPACE_DIR", "ZSBX_USER_HOME_DIR"] {
            assert!(
                task["Env"][legacy].is_null(),
                "{legacy} should be gone (virtio-blk pivot)",
            );
        }
        assert_eq!(task["Env"]["ZSBX_VM_MEMORY_MB"], "1024");
        assert_eq!(task["Env"]["ZSBX_VM_CPUS_BOOT"], "2");
        // M6: subnet base octet is paired between Rust and driver.
        // Default 99 keeps the historical 10.99/16 layout.
        assert_eq!(task["Env"]["ZSBX_SUBNET_BASE_OCTET"], "99");
        // Constant 500 MHz advisory; see NOMAD_CPU_MHZ_ADVISORY.
        assert_eq!(task["Resources"]["CPU"], 500);
        assert_eq!(task["Resources"]["MemoryMB"], 1024);
        // KillTimeout is 10 seconds in nanoseconds.
        assert_eq!(task["KillTimeout"], 10_000_000_000u64);
    }

    #[test]
    fn nomad_job_json_uses_configured_subnet_base_octet() {
        // M6: changing the config's subnet_second_octet must flow
        // into the env var. The unit test for the controller-side IP
        // computation lives in create_guard_uses_subnet_octet (further
        // down) — these two together pin the pairing.
        let mut cfg = make_cfg();
        cfg.nomad_ch.subnet_second_octet = 50;
        let v = build_nomad_job_json_with(
            "zsbx-y", &cfg, 1,
            Path::new("/w.img"), Path::new("/u.img"), "ab",
            "u", "p", "s",
            None,
            None, // r3-A: no node pin
        );
        assert_eq!(
            v["Job"]["TaskGroups"][0]["Tasks"][0]["Env"]["ZSBX_SUBNET_BASE_OCTET"],
            "50"
        );
    }

    /// B24 / R8-DEPLOY1 regression pin: `ZSBX_SANDBOX_ID` must be
    /// present in the Env block. This test fails if the env entry is
    /// ever removed, AND asserts the value is in Uuid::simple() form
    /// (32-hex, no hyphens) — the value is embedded verbatim in the
    /// guest's kernel cmdline as `SANDBOX_AGENT_SANDBOX_ID=<value>`.
    #[test]
    fn nomad_job_spec_includes_sandbox_id_env() {
        let cfg = make_cfg();
        let sandbox_id = Uuid::now_v7();
        let sandbox_id_simple = sandbox_id.simple().to_string();
        let v = build_nomad_job_json_with(
            "zsbx-r8",
            &cfg,
            3,
            Path::new("/var/zeroship/ch/r8/workspace.img"),
            Path::new("/var/zeroship/ch/users/alice/home.img"),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "alice",
            "proj1",
            &sandbox_id_simple,
            None,
            None, // r3-A: no node pin
        );
        let env = &v["Job"]["TaskGroups"][0]["Tasks"][0]["Env"];
        // 1. The entry exists and equals the passed value.
        assert_eq!(
            env["ZSBX_SANDBOX_ID"],
            sandbox_id_simple,
            "ZSBX_SANDBOX_ID must be wired through verbatim (B24 / R8-DEPLOY1)"
        );
        // 2. The value is alphanumeric-only (no hyphens) — must pass
        //    the kernel cmdline embedding without corruption.
        let val = env["ZSBX_SANDBOX_ID"].as_str().expect("string env value");
        assert!(
            val.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "ZSBX_SANDBOX_ID='{val}' contains chars outside [0-9a-zA-Z_]"
        );
        // 3. Defense-in-depth: Uuid::simple() is exactly 32 hex chars.
        assert_eq!(
            val.len(),
            32,
            "ZSBX_SANDBOX_ID should be Uuid::simple() form (32 hex chars), \
             got {} chars: '{val}'",
            val.len(),
        );
        assert!(
            !val.contains('-'),
            "ZSBX_SANDBOX_ID must not contain hyphens"
        );
    }

    #[test]
    fn nomad_job_json_serializes_to_valid_json() {
        let cfg = make_cfg();
        let v = build_nomad_job_json_with(
            "zsbx-x",
            &cfg,
            1,
            Path::new("/w.img"),
            Path::new("/u.img"),
            "ab",
            "u",
            "p",
            "s",
            None,
            None, // r3-A: no node pin
        );
        let s = serde_json::to_string(&v).expect("serialize");
        // Round-trip — Nomad parses as JSON, so we should too.
        let _: serde_json::Value =
            serde_json::from_str(&s).expect("round-trip parse");
    }

    /// The ch driver is the unconditional default (T-7 + T-8 cutover).
    /// Driver MUST equal "ch" — matches nomad-driver-ch::ch::PluginName.
    #[test]
    fn nomad_job_spec_always_uses_ch_driver() {
        let cfg = make_cfg();
        let v = build_nomad_job_json(
            "zsbx-ch", &cfg, 4,
            Path::new("/w.img"), Path::new("/u.img"),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "alice", "proj1", "abcdef0123456789abcdef0123456789",
            None, // r3-A: no node pin
        );
        let task = &v["Job"]["TaskGroups"][0]["Tasks"][0];
        assert_eq!(
            task["Driver"], "ch",
            "ch driver is unconditional — T-7 + T-8 cutover complete"
        );
        assert!(
            task["Config"]["command"].is_null(),
            "raw_exec `command` field must NOT appear in ch driver Config"
        );
    }

    /// Every field in the Go driver's `TaskConfig` struct
    /// (`nomad-driver-ch/ch/task_config.go`) must be present in
    /// the Nomad Config block, with
    /// the correct JSON type. This is the controller↔driver
    /// wire-format pin — a rename / drop on either side breaks
    /// jobspec decode.
    #[test]
    fn ch_plugin_jobspec_includes_all_task_config_fields() {
        let cfg = make_cfg();
        let v = build_nomad_job_json_with(
            "zsbx-typed",
            &cfg,
            7,
            Path::new("/var/zeroship/ch/abc/workspace.img"),
            Path::new("/var/zeroship/ch/users/alice/home.img"),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "alice",
            "proj1",
            "abcdef0123456789abcdef0123456789",
            None,
            None, // r3-A: no node pin
        );
        let config = &v["Job"]["TaskGroups"][0]["Tasks"][0]["Config"];

        // Scalars — types matter, the Go driver decodes via msgpack
        // codec tags so JSON-number-vs-string mismatches drop fields
        // silently.
        assert_eq!(config["vm_index"].as_u64(), Some(7));
        assert_eq!(
            config["kernel"].as_str(),
            Some("/var/lib/zeroship/ch/vmlinuz"),
            "kernel is derived from cfg.nomad_ch.runtime_dir + /vmlinuz",
        );
        assert_eq!(config["cpus"].as_u64(), Some(2));
        assert_eq!(config["memory_mb"].as_u64(), Some(1024));
        assert_eq!(
            config["sandbox_id"].as_str(),
            Some("abcdef0123456789abcdef0123456789"),
        );
        // C-7-LT-7: user_id feeds the driver's per-user-home path
        // allow-list. Without this, the restore-branch rewriter
        // rejects /var/zeroship/ch/users/<usr>/home.img.
        assert_eq!(
            config["user_id"].as_str(),
            Some("alice"),
        );
        assert_eq!(
            config["workspace_img"].as_str(),
            Some("/var/zeroship/ch/abc/workspace.img"),
        );
        assert_eq!(
            config["user_home_img"].as_str(),
            Some("/var/zeroship/ch/users/alice/home.img"),
        );
        assert_eq!(
            config["pubkey_hex"].as_str(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
        );
        assert_eq!(config["subnet_base_octet"].as_u64(), Some(99));

        // Block-lists must be present as empty arrays — an absent
        // field decodes to nil in the Go driver, which is also "auto-
        // synthesise", but emitting `[]` pins the contract.
        assert!(config["disks"].is_array());
        assert_eq!(config["disks"].as_array().unwrap().len(), 0);
        assert!(config["fs"].is_array());
        assert_eq!(config["fs"].as_array().unwrap().len(), 0);
        assert!(config["net"].is_array());
        assert_eq!(config["net"].as_array().unwrap().len(), 0);

        // restore_from is present as an empty string on cold-boot —
        // separate test below pins the non-empty case.
        assert_eq!(config["restore_from"].as_str(), Some(""));
    }

    /// Restore path: when `restore_from` is `Some`, the typed Config
    /// MUST carry the non-empty path under ChPlugin mode. The Go
    /// driver's `StartTask` branches on
    /// `TaskConfig.RestoreFrom != ""` to choose `cloud-hypervisor
    /// --restore source_url=file://…` vs. cold-boot.
    #[test]
    fn ch_plugin_restore_jobspec_includes_restore_from() {
        let cfg = make_cfg();
        let restore_dir = Path::new("/var/zeroship/ch/snapshots/snap-abc");
        let v = build_nomad_job_json_with(
            "zsbx-restore",
            &cfg,
            5,
            Path::new("/w.img"),
            Path::new("/u.img"),
            "ab",
            "alice",
            "proj1",
            "abcdef0123456789abcdef0123456789",
            Some(restore_dir),
            None, // r3-A: no node pin
        );
        let config = &v["Job"]["TaskGroups"][0]["Tasks"][0]["Config"];
        assert_eq!(
            config["restore_from"].as_str(),
            Some("/var/zeroship/ch/snapshots/snap-abc"),
            "ChPlugin restore path MUST set Config.restore_from — \
             the Go driver dispatches on the empty/non-empty test",
        );
    }

    /// Cold-boot path: `restore_from = None` → typed Config has
    /// `restore_from = ""`. The driver treats empty as cold-boot.
    #[test]
    fn ch_plugin_cold_boot_jobspec_has_empty_restore_from() {
        let cfg = make_cfg();
        let v = build_nomad_job_json_with(
            "zsbx-cold",
            &cfg,
            2,
            Path::new("/w.img"),
            Path::new("/u.img"),
            "ab",
            "alice",
            "proj1",
            "abcdef0123456789abcdef0123456789",
            None,
            None, // r3-A: no node pin
        );
        let config = &v["Job"]["TaskGroups"][0]["Tasks"][0]["Config"];
        // Cold-boot: the field must be PRESENT (so the Go driver's
        // codec decode never sees a nil/missing) and empty. A
        // non-empty value would mis-route cold-boot through the
        // restore branch and crash CH on a phantom snapshot path.
        assert_eq!(
            config["restore_from"].as_str(),
            Some(""),
            "cold-boot ChPlugin Config.restore_from must be empty",
        );
    }

    /// The `command` field (raw_exec-only) MUST NOT appear in the
    /// ch driver Config block. The Go driver's TaskConfig has no
    /// such field, and forwarding it would be either silently
    /// ignored or (under stricter HCL decode) fail jobspec
    /// validation at submit time.
    #[test]
    fn ch_plugin_jobspec_does_not_include_command_field() {
        let cfg = make_cfg();
        let v = build_nomad_job_json_with(
            "zsbx-no-cmd",
            &cfg,
            1,
            Path::new("/w.img"),
            Path::new("/u.img"),
            "ab",
            "alice",
            "proj1",
            "abcdef0123456789abcdef0123456789",
            None,
            None, // r3-A: no node pin
        );
        let config = &v["Job"]["TaskGroups"][0]["Tasks"][0]["Config"];
        assert!(
            config["command"].is_null(),
            "ch driver Config must NOT carry `command` \
             field — Go driver TaskConfig has no such tag, got: {config:?}",
        );
    }

    // ─── virtio-blk disk-image helpers (bug #11 pivot) ───────

    /// `workspace_image_path` derivation: per-sandbox image lives
    /// inside the sandbox's host_dir, always named `workspace.img`.
    /// The wrapper attaches this as /dev/vdb.
    #[test]
    fn workspace_image_path_is_host_dir_join_workspace_img() {
        let host_dir = Path::new("/var/zeroship/ch/abc123");
        assert_eq!(
            workspace_image_path(host_dir),
            PathBuf::from("/var/zeroship/ch/abc123/workspace.img"),
        );
    }

    /// `user_home_image_path` derivation: per-user image lives
    /// under <user_home_dir_root>/<user_id>/home.img. The wrapper
    /// attaches this as /dev/vdc. The path is reused across the
    /// user's sandboxes; the controller idempotently mkfs's it
    /// on first use only.
    #[test]
    fn user_home_image_path_is_root_user_home_img() {
        let root = Path::new("/var/zeroship/ch/users");
        assert_eq!(
            user_home_image_path(root, "alice"),
            PathBuf::from("/var/zeroship/ch/users/alice/home.img"),
        );
        // typed-id-shaped user ids round-trip identically.
        assert_eq!(
            user_home_image_path(root, "usr_01h5x2"),
            PathBuf::from("/var/zeroship/ch/users/usr_01h5x2/home.img"),
        );
    }

    /// Idempotent ext4 image creation: a second invocation against
    /// the same path is a no-op (file already exists, helper
    /// returns Ok without re-running truncate or mkfs). This is the
    /// invariant per-user home.img depends on (every sandbox after
    /// the user's first must NOT clobber their package caches).
    ///
    /// We can't exercise the success path of mkfs.ext4 in a unit
    /// test (it needs root, requires e2fsprogs, and writes ~MiBs of
    /// metadata). We can exercise the idempotency path: pre-create
    /// the file, then call the helper and assert the file content
    /// is unchanged.
    #[test]
    fn create_ext4_image_if_missing_skips_when_file_exists() {
        let dir = std::env::temp_dir().join(format!(
            "zsbx-img-test-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("home.img");

        // Pre-stamp a file with known contents that NEITHER truncate
        // NOR mkfs.ext4 would leave intact. If the helper short-
        // circuits on "exists", these bytes survive.
        let sentinel = b"i-am-a-pre-existing-image-do-not-touch";
        std::fs::write(&img, sentinel).unwrap();
        let stat_before = std::fs::metadata(&img).unwrap();
        let len_before = stat_before.len();

        create_ext4_image_if_missing(&img, 20)
            .expect("idempotent path must succeed");

        let stat_after = std::fs::metadata(&img).unwrap();
        assert_eq!(stat_after.len(), len_before, "len must not change");
        assert_eq!(
            std::fs::read(&img).unwrap(),
            sentinel,
            "contents must survive the idempotent call",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T-8b-stress Bug 1 / 3-strike cross-emitter parity test #1:
    /// `create_ext4_image_if_missing`'s idempotent skip-because-exists
    /// branch MUST re-assert the post-condition (file present, > 0
    /// bytes). Without this, a zero-byte sentinel left at the path
    /// (e.g., a `truncate` that partially succeeded then was
    /// interrupted) would slip through as Ok, and the driver's
    /// preflight stat would catch it far later with a generic
    /// "Failed tasks" rollup. This test pins the "skip path must
    /// still validate" invariant.
    #[test]
    fn create_ext4_image_if_missing_skip_path_rejects_zero_byte_file() {
        let dir = std::env::temp_dir().join(format!(
            "zsbx-img-test-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("workspace.img");

        // Touch a zero-byte sentinel. `path.exists()` returns true →
        // skip-because-exists branch fires → post-condition assertion
        // catches the size==0 case.
        std::fs::write(&img, b"").unwrap();
        assert_eq!(std::fs::metadata(&img).unwrap().len(), 0);

        let err = create_ext4_image_if_missing(&img, 20)
            .expect_err("zero-byte file must be rejected by the post-condition");
        assert!(
            err.contains("empty (size 0)"),
            "err must mention empty/size; got: {err}"
        );
        assert!(
            err.contains("workspace.img"),
            "err must name the path; got: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T-8b-stress Bug 1 / 3-strike cross-emitter parity test #2:
    /// `assert_disk_image_present` returns Ok for a normal file with
    /// > 0 bytes. Direct test of the post-condition helper so the
    /// CI signal is precise.
    #[test]
    fn assert_disk_image_present_accepts_normal_nonempty_file() {
        let dir = std::env::temp_dir().join(format!(
            "zsbx-assert-test-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("workspace.img");
        std::fs::write(&img, b"non-empty").unwrap();

        assert_disk_image_present(&img).expect("normal non-empty file must pass");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T-8b-stress Bug 1 / 3-strike cross-emitter parity test #3:
    /// `assert_disk_image_present` rejects a missing path with an
    /// error message that names the path (so an operator can grep
    /// for it in the controller log). Pins the "controller catches
    /// missing image at submit time, not after the alloc burns its
    /// $0.20" contract.
    #[test]
    fn assert_disk_image_present_rejects_missing_path() {
        let dir = std::env::temp_dir().join(format!(
            "zsbx-assert-test-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("nope.img"); // do not create

        let err = assert_disk_image_present(&img)
            .expect_err("missing path must be rejected");
        assert!(
            err.contains("nope.img"),
            "err must name the path; got: {err}"
        );
        assert!(
            err.contains("post-stage stat failed"),
            "err must say what step failed; got: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T-8b-stress Bug 1 / 3-strike cross-emitter parity test #4:
    /// `assert_disk_image_present` rejects a directory at the same
    /// name (the structural-surprise case). Catches a future bug
    /// where some staging path mkdir's where it should be touching a
    /// file.
    #[test]
    fn assert_disk_image_present_rejects_directory() {
        let dir = std::env::temp_dir().join(format!(
            "zsbx-assert-test-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let img_as_dir = dir.join("workspace.img");
        std::fs::create_dir(&img_as_dir).unwrap();

        let err = assert_disk_image_present(&img_as_dir)
            .expect_err("directory at image path must be rejected");
        assert!(
            err.contains("is a directory"),
            "err must say 'directory'; got: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── path / id sanitizers (mirror k8s.rs unit tests) ────

    #[test]
    fn sanitize_rejects_parent() {
        assert!(sanitize_path("../etc/passwd").is_err());
        assert!(sanitize_path("foo/../bar").is_err());
    }

    #[test]
    fn sanitize_rejects_absolute() {
        assert!(sanitize_path("/etc/passwd").is_err());
    }

    #[test]
    fn sanitize_rejects_empty() {
        assert!(sanitize_path("").is_err());
    }

    #[test]
    fn sanitize_accepts_relative() {
        assert_eq!(sanitize_path("src/main.rs").unwrap(), "src/main.rs");
    }

    #[test]
    fn validate_typed_id_accepts_typed_form() {
        // Phase-1+2 wire shape: handlers and backends both speak
        // `usr_<22-base62>` end-to-end. The typed-id check is the
        // single source of truth.
        let usr = zeroship_core::typed_id::generate("usr");
        assert!(validate_typed_id(&usr, "usr", "user_id").is_ok());
        let prj = zeroship_core::typed_id::generate("prj");
        assert!(validate_typed_id(&prj, "prj", "project_id").is_ok());
    }

    #[test]
    fn validate_typed_id_rejects_legacy_and_garbage() {
        // Legacy DNS-1123 charset (no prefix) — wire migration is
        // complete; refuse the old shape.
        assert!(validate_typed_id("alice", "usr", "user_id").is_err());
        assert!(validate_typed_id("alice-1", "usr", "user_id").is_err());
        assert!(validate_typed_id("Alice", "usr", "user_id").is_err());
        // Wrong prefix.
        let usr = zeroship_core::typed_id::generate("usr");
        assert!(validate_typed_id(&usr, "prj", "project_id").is_err());
        // Garbage / empty.
        assert!(validate_typed_id("", "usr", "user_id").is_err());
        assert!(validate_typed_id("usr_", "usr", "user_id").is_err());
        assert!(validate_typed_id("usr_xx", "usr", "user_id").is_err());
    }

    // ─── FM-A: stale-tenant fingerprint check in wait_for_agent_livez ────
    //
    // Stress finding (N=8 rapid recycle): a fresh `create()` for the
    // same vm_index / VM IP returned 201 in 0.25 s while pointing at
    // the *previous* tenant's still-alive agent — Nomad's view said
    // alloc=terminal, but the host-side cloud-hypervisor process tree
    // hadn't reaped yet. /livez=200 was the wrong attestation; the
    // controller needs to verify the agent is signing with OUR
    // pubkey before declaring the sandbox ready.
    //
    // The mock here is a stdlib TcpListener that speaks just enough
    // HTTP to mimic the agent's /livez and /version handlers. It
    // deliberately doesn't verify the request signature (we're not
    // testing the agent; we're testing the controller's behaviour
    // when the agent reports a particular fingerprint).

    /// Spin up a tiny `std::net::TcpListener`-backed mock that
    /// answers /livez=200 and /version=`version_body` (raw bytes,
    /// caller controls the JSON).
    ///
    /// Returns the bound port + a shutdown flag. The thread exits
    /// when the flag is flipped (caller does this in Drop, or the
    /// test ends and we leak the thread — fine for unit tests).
    fn spawn_mock_agent(
        version_body: String,
        version_status: u16,
    ) -> (u16, std::sync::Arc<std::sync::atomic::AtomicBool>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let port = listener.local_addr().expect("addr").port();
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_millis(200)))
                            .ok();
                        stream
                            .set_write_timeout(Some(Duration::from_millis(200)))
                            .ok();
                        // Read the request line + headers (don't bother
                        // with the body; we only switch on path).
                        let mut buf = [0u8; 1024];
                        let n = stream.read(&mut buf).unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..n]);
                        let path = req
                            .lines()
                            .next()
                            .and_then(|l| l.split_whitespace().nth(1))
                            .unwrap_or("");
                        let resp = if path == "/livez" {
                            "HTTP/1.1 200 OK\r\nContent-Length: 16\r\n\r\n{\"status\":\"ok\"}"
                                .to_string()
                        } else if path == "/version" {
                            let status_text = match version_status {
                                200 => "200 OK",
                                401 => "401 Unauthorized",
                                _ => "500 Internal Server Error",
                            };
                            format!(
                                "HTTP/1.1 {}\r\nContent-Type: application/json\r\n\
                                 Content-Length: {}\r\n\r\n{}",
                                status_text,
                                version_body.len(),
                                version_body,
                            )
                        } else {
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string()
                        };
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        (port, stop)
    }

    fn make_sk() -> Arc<SigningKey> {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = i as u8;
        }
        Arc::new(SigningKey::from_bytes(&bytes))
    }

    #[compio::test]
    async fn wait_for_agent_livez_returns_ok_when_fp_matches() {
        let sk = make_sk();
        let our_fp = sig::pubkey_fingerprint(&sk.verifying_key());
        let body = format!(
            r#"{{"agent_version":"x","pubkey_fingerprint":"{our_fp}"}}"#
        );
        let (port, stop) = spawn_mock_agent(body, 200);
        let url = format!("http://127.0.0.1:{port}");
        let res = wait_for_agent_livez(
            &url,
            &our_fp,
            &sk,
            Duration::from_secs(2),
        )
        .await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(res.is_ok(), "expected Ok on fp match, got {res:?}");
    }

    #[compio::test]
    async fn wait_for_agent_livez_times_out_on_fp_mismatch() {
        let sk = make_sk();
        let our_fp = sig::pubkey_fingerprint(&sk.verifying_key());
        // Mock returns a DIFFERENT fingerprint — simulates stale
        // tenant whose CH is still alive on the same IP.
        let stale_fp = "deadbeef00112233";
        assert_ne!(our_fp, stale_fp);
        let body = format!(
            r#"{{"agent_version":"x","pubkey_fingerprint":"{stale_fp}"}}"#
        );
        let (port, stop) = spawn_mock_agent(body, 200);
        let url = format!("http://127.0.0.1:{port}");
        // Short timeout — we want to verify the function gives up
        // and surfaces the "stale agent" error, not just hangs.
        let res = wait_for_agent_livez(
            &url,
            &our_fp,
            &sk,
            Duration::from_millis(800),
        )
        .await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let err = res.expect_err("must surface stale-agent error");
        assert!(
            err.contains("stale agent"),
            "FM-A regression: error did not surface stale-agent text; got {err:?}"
        );
        assert!(
            err.contains(stale_fp) && err.contains(&our_fp),
            "error must include both expected and actual fp for triage; got {err:?}"
        );
    }

    #[compio::test]
    async fn wait_for_agent_livez_legacy_agent_no_fp_field_accepted() {
        // Backward-compat: a /version response without the
        // pubkey_fingerprint field falls back to "signed-auth-only"
        // attestation. The /version request was signed with the
        // controller's key; if the agent answered 200 it's verifying
        // with our pubkey (i.e. it IS our agent). This branch keeps
        // a gradual rollout path open — old agents still work.
        let sk = make_sk();
        let our_fp = sig::pubkey_fingerprint(&sk.verifying_key());
        let body = r#"{"agent_version":"legacy"}"#.to_string();
        let (port, stop) = spawn_mock_agent(body, 200);
        let url = format!("http://127.0.0.1:{port}");
        let res = wait_for_agent_livez(
            &url,
            &our_fp,
            &sk,
            Duration::from_secs(2),
        )
        .await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(
            res.is_ok(),
            "legacy /version (no fp field) must fall back to ok; got {res:?}"
        );
    }

    #[compio::test]
    async fn wait_for_agent_livez_times_out_when_unreachable() {
        // No mock — point at an unbound port. /livez never returns
        // 200 → timeout path with "never returned 200" message.
        let sk = make_sk();
        let our_fp = sig::pubkey_fingerprint(&sk.verifying_key());
        let res = wait_for_agent_livez(
            "http://127.0.0.1:1",
            &our_fp,
            &sk,
            Duration::from_millis(400),
        )
        .await;
        let err = res.expect_err("must time out");
        assert!(
            err.contains("never returned 200"),
            "expected /livez-unreachable timeout text; got {err:?}"
        );
        assert!(
            err.contains(&our_fp),
            "timeout error must include expected fp for triage; got {err:?}"
        );
    }

    // ─── R19-I1: two-phase probe (TCP-connect gate, then ureq /livez) ─
    //
    // wait_for_agent_livez's R19-I1 fix mirrors the C-7-LT-2-PR1
    // pattern: a compio-native TCP-connect probe gates the ureq HTTP
    // call so a half-collapsed TAP route doesn't burn the entire
    // agent_livez_timeout budget on one stuck SYN. These tests pin:
    //   1. happy path: socket up + /livez=200 + /version=200 with
    //      matching fp → Ok (Phase 1 + Phase 2 + signed /version
    //      compose correctly).
    //   2. unroutable address (TEST-NET-1): Phase 1 times out
    //      cleanly at the connect-timeout ceiling, never spending a
    //      spawn_blocking+ureq on a doomed HTTP call; total wall-time
    //      stays well under the kernel's SYN-retransmit ceiling.
    //   3. socket accepts late: listener binds mid-deadline; first
    //      few Phase 1 probes miss, subsequent ones succeed → Ok
    //      within budget.
    //   4. HTTP-layer wedge: socket accepts but /livez returns 500 →
    //      Phase 2 rejects the probe; loop polls until deadline with
    //      "never returned 200" error.

    /// Variant of `spawn_mock_agent` that lets the caller control the
    /// /livez response status (the original always answers 200). Used
    /// by the R19-I1 "HTTP-layer wedge" test where the TCP layer is
    /// up but the HTTP /livez handler refuses.
    fn spawn_mock_agent_with_livez_status(
        livez_status: u16,
        version_body: String,
        version_status: u16,
    ) -> (u16, std::sync::Arc<std::sync::atomic::AtomicBool>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let port = listener.local_addr().expect("addr").port();
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_millis(200)))
                            .ok();
                        stream
                            .set_write_timeout(Some(Duration::from_millis(200)))
                            .ok();
                        let mut buf = [0u8; 1024];
                        let n = stream.read(&mut buf).unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..n]);
                        let path = req
                            .lines()
                            .next()
                            .and_then(|l| l.split_whitespace().nth(1))
                            .unwrap_or("");
                        let resp = if path == "/livez" {
                            let status_text = match livez_status {
                                200 => "200 OK",
                                500 => "500 Internal Server Error",
                                503 => "503 Service Unavailable",
                                _ => "500 Internal Server Error",
                            };
                            format!(
                                "HTTP/1.1 {}\r\nContent-Length: 16\r\n\r\n{}",
                                status_text,
                                "{\"status\":\"x\"}"
                            )
                        } else if path == "/version" {
                            let status_text = match version_status {
                                200 => "200 OK",
                                401 => "401 Unauthorized",
                                _ => "500 Internal Server Error",
                            };
                            format!(
                                "HTTP/1.1 {}\r\nContent-Type: application/json\r\n\
                                 Content-Length: {}\r\n\r\n{}",
                                status_text,
                                version_body.len(),
                                version_body,
                            )
                        } else {
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string()
                        };
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        (port, stop)
    }

    #[compio::test]
    async fn wait_for_agent_livez_happy_socket_then_livez_ok() {
        // R19-I1 happy path under the two-phase probe: TCP listener
        // accepts (Phase 1 reachable), /livez returns 200 (Phase 2
        // gate passes), /version returns 200 with matching fp →
        // function returns Ok well inside the budget.
        let sk = make_sk();
        let our_fp = sig::pubkey_fingerprint(&sk.verifying_key());
        let body = format!(
            r#"{{"agent_version":"x","pubkey_fingerprint":"{our_fp}"}}"#
        );
        let (port, stop) = spawn_mock_agent(body, 200);
        let url = format!("http://127.0.0.1:{port}");
        let started = Instant::now();
        let res = wait_for_agent_livez(
            &url,
            &our_fp,
            &sk,
            Duration::from_secs(2),
        )
        .await;
        let elapsed = started.elapsed();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(res.is_ok(), "expected Ok on happy path, got {res:?}");
        // Loopback connect + ureq /livez + signed /version on a
        // healthy mock should clear in well under 500 ms. If this
        // ever takes seconds, Phase 1 is wedging — exactly what
        // R19-I1 closes.
        assert!(
            elapsed < Duration::from_millis(500),
            "R19-I1 regression: happy path took {elapsed:?}; \
             two-phase probe should resolve in tens of ms on loopback"
        );
    }

    #[compio::test]
    async fn wait_for_agent_livez_socket_never_accepts_returns_timeout_clean() {
        // R19-I1 wedge-fix invariant: an unroutable address (RFC 5737
        // TEST-NET-1, no host) MUST surface a clean timeout bounded
        // by our compio-side 150 ms connect-timeout * (poll cadence),
        // NOT by the kernel's 30-90 s SYN-retransmit ceiling.
        //
        // The pre-R19-I1 code spent ureq.timeout(500ms) on a request-
        // deadline, not connect-deadline, and so could burn 30 s on a
        // single probe. With Phase 1 in place, every iteration costs
        // at most CONNECT_TIMEOUT + cadence; the 700 ms budget below
        // exits cleanly with the "never returned 200" message.
        let sk = make_sk();
        let our_fp = sig::pubkey_fingerprint(&sk.verifying_key());
        let started = Instant::now();
        let res = wait_for_agent_livez(
            "http://192.0.2.1:7777",
            &our_fp,
            &sk,
            Duration::from_millis(700),
        )
        .await;
        let elapsed = started.elapsed();
        let err = res.expect_err("must time out cleanly");
        // The error path for "/livez never 200" surfaces this string.
        assert!(
            err.contains("never returned 200"),
            "expected /livez-unreachable timeout text; got {err:?}"
        );
        // The load-bearing R19-I1 assertion: the deadline holds. A
        // regression that drops Phase 1 and reverts to ureq.timeout()
        // would burn 30+ s here. 3 s is a generous CI cap above the
        // 700 ms budget (allowing one ~150 ms in-flight probe at
        // deadline + spawn_blocking scheduling jitter).
        assert!(
            elapsed < Duration::from_secs(3),
            "R19-I1 regression: unroutable connect ran {elapsed:?} on \
             a 700 ms budget — Phase 1 connect-gate is not capping the \
             stuck SYN. Did the ureq probe come back without the TCP \
             pre-gate?"
        );
    }

    #[compio::test]
    async fn wait_for_agent_livez_socket_accepts_late_succeeds_within_budget() {
        // R19-I1: agent comes up partway through the budget. The
        // helper thread holds the port closed for ~250 ms, then
        // binds + serves /livez+/version. Initial Phase 1 probes
        // miss (kernel returns ECONNREFUSED on an unbound port,
        // fast); once the listener is up, Phase 1 returns reachable,
        // Phase 2 hits /livez=200, signed /version returns matching
        // fp → Ok before the deadline. Exercises the poll-cadence
        // loop end-to-end across an empty→up transition.
        //
        // Port-reservation dance: bind a listener to grab a free
        // port, drop it, then re-bind in the helper thread after a
        // delay. The drop-then-rebind window relies on SO_REUSEADDR
        // semantics; if the kernel hands out the same port to a
        // different process during the gap, the helper's bind retry
        // loop falls through to its panic — which fails the test
        // with a clear message rather than silently mis-asserting.
        let sk = make_sk();
        let our_fp = sig::pubkey_fingerprint(&sk.verifying_key());
        let body = format!(
            r#"{{"agent_version":"x","pubkey_fingerprint":"{our_fp}"}}"#
        );
        let probe_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let probe_port = probe_listener.local_addr().unwrap().port();
        drop(probe_listener);
        let body_for_thread = body.clone();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_clone = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            use std::io::{Read, Write};
            use std::net::TcpListener;
            use std::sync::atomic::Ordering;
            // Retry-bind: tolerate a brief TIME_WAIT race on the
            // dropped listener. Up to 20×25ms = 500ms — well inside
            // the test's 2s outer budget.
            let listener = {
                let mut attempts = 0;
                loop {
                    match TcpListener::bind(format!("127.0.0.1:{probe_port}")) {
                        Ok(l) => break l,
                        Err(_) if attempts < 20 => {
                            attempts += 1;
                            std::thread::sleep(Duration::from_millis(25));
                        }
                        Err(e) => panic!("R19-I1 late-bind fixture failed: {e}"),
                    }
                }
            };
            listener.set_nonblocking(true).ok();
            while !stop_clone.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_millis(200)))
                            .ok();
                        stream
                            .set_write_timeout(Some(Duration::from_millis(200)))
                            .ok();
                        let mut buf = [0u8; 1024];
                        let n = stream.read(&mut buf).unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..n]);
                        let path = req
                            .lines()
                            .next()
                            .and_then(|l| l.split_whitespace().nth(1))
                            .unwrap_or("");
                        let resp = if path == "/livez" {
                            "HTTP/1.1 200 OK\r\nContent-Length: 16\r\n\r\n{\"status\":\"ok\"}"
                                .to_string()
                        } else if path == "/version" {
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                                 Content-Length: {}\r\n\r\n{}",
                                body_for_thread.len(),
                                body_for_thread,
                            )
                        } else {
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string()
                        };
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        let url = format!("http://127.0.0.1:{probe_port}");
        let res = wait_for_agent_livez(
            &url,
            &our_fp,
            &sk,
            Duration::from_millis(2000),
        )
        .await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(
            res.is_ok(),
            "R19-I1: late-bind agent must clear inside 2000 ms \
             budget once the socket comes up; got {res:?}"
        );
    }

    #[compio::test]
    async fn wait_for_agent_livez_socket_accepts_but_livez_500() {
        // R19-I1 Phase 2 contract: TCP-layer up but /livez returns
        // 500 → loop polls until deadline because Phase 2 never
        // observes status==200. Surfaces "never returned 200" error.
        // This is the "HTTP server wedged after socket up" failure
        // shape; it's the rarer post-fix wedge surface noted in the
        // R19-I1 doc.
        let sk = make_sk();
        let our_fp = sig::pubkey_fingerprint(&sk.verifying_key());
        let body = format!(
            r#"{{"agent_version":"x","pubkey_fingerprint":"{our_fp}"}}"#
        );
        let (port, stop) =
            spawn_mock_agent_with_livez_status(500, body, 200);
        let url = format!("http://127.0.0.1:{port}");
        let res = wait_for_agent_livez(
            &url,
            &our_fp,
            &sk,
            Duration::from_millis(600),
        )
        .await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let err = res.expect_err("must time out with /livez 500");
        assert!(
            err.contains("never returned 200"),
            "expected /livez-not-200 timeout text; got {err:?}"
        );
    }

    // ─── FM-F: host-side fence in stop() before vm_index release ──
    //
    // The fence verifies the previous tenant's agent has stopped
    // answering /livez before we hand the IP back to the pool.
    // Two consecutive misses (connect-refused / timeout / 5xx) →
    // OK; deadline before two-in-a-row → Err (caller must leak).

    #[compio::test]
    async fn host_fence_returns_ok_when_no_agent_listens() {
        // No mock — a port that refuses connections turns into two
        // consecutive misses well inside the budget.
        let res =
            wait_for_agent_silent("http://127.0.0.1:1", Duration::from_secs(2))
                .await;
        assert!(
            res.is_ok(),
            "FM-F regression: fence must clear when /livez never answers; \
             got {res:?}"
        );
    }

    #[compio::test]
    async fn host_fence_times_out_when_agent_keeps_answering() {
        // Spin up the same /livez=200 mock the FM-A tests use. The
        // fence must NEVER pass while the socket is alive — it has
        // to time out and surface "still answering at fence deadline"
        // so the caller leaks the index instead of handing out a
        // live IP.
        let body = r#"{"agent_version":"x","pubkey_fingerprint":"deadbeef00112233"}"#
            .to_string();
        let (port, stop) = spawn_mock_agent(body, 200);
        let url = format!("http://127.0.0.1:{port}");
        let res = wait_for_agent_silent(&url, Duration::from_millis(700)).await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let err = res.expect_err("must time out while agent answers");
        assert!(
            err.contains("still answering"),
            "FM-F regression: fence-timeout error did not surface \
             'still answering' text; got {err:?}"
        );
        assert!(
            err.contains("leaking vm_index"),
            "FM-F regression: fence-timeout error did not signal \
             leak intent to operator; got {err:?}"
        );
    }

    #[compio::test]
    async fn host_fence_clears_quickly_after_two_consecutive_misses() {
        // Cadence sanity: at 100 ms intervals two consecutive misses
        // ≈ ~200 ms of wall time. Confirm the fence doesn't burn the
        // full budget when the agent is already gone — we want fast
        // index turnaround on the happy path.
        let started = Instant::now();
        let res = wait_for_agent_silent(
            "http://127.0.0.1:1",
            Duration::from_secs(5),
        )
        .await;
        assert!(res.is_ok());
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(800),
            "FM-F: fence took {elapsed:?} for two-in-a-row misses on a \
             refused port; expected well under 800ms — cadence regression?"
        );
    }

    /// C-7-LT-2-PR1 regression pin: the R16-I2 "alternating-answer
    /// LEAK" pathology is structurally impossible under the new
    /// connect-only probe. Previously a mock that accepted-then-dropped
    /// the socket (counter % 2 == 1) returned an HTTP transport error
    /// (= miss) via ureq; the cycle 0→1→0→1 produced a permanent
    /// `consecutive_misses=1` LEAK signal. With the PR1 compio-native
    /// TCP-connect probe, `accept()` succeeded means SYN was ACKed
    /// means socket is alive — there's no HTTP-layer disambiguation
    /// of "accepted but dropped" anymore. The same mock under PR1
    /// now produces `consecutive_misses=0` (persistent reachability)
    /// at deadline. This is THE fix: the LEAK shape cannot manifest.
    ///
    /// Test invariant: alternating accept-vs-drop must time out with
    /// `consecutive_misses=0`, NOT `=1`. If a future regression
    /// re-introduces HTTP-layer classification, this test fires.
    #[compio::test]
    async fn host_fence_pr1_alternating_accept_drop_times_out_with_misses_zero()
    {
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let counter = std::sync::Arc::new(AtomicU32::new(0));
        std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut s, _)) => {
                        use std::io::{Read, Write};
                        let mut buf = [0u8; 1024];
                        let _ = s.set_read_timeout(Some(
                            Duration::from_millis(200),
                        ));
                        let _ = s.read(&mut buf);
                        let n = counter.fetch_add(1, Ordering::Relaxed);
                        if n % 2 == 0 {
                            let _ = s.write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
                            );
                        } else {
                            // Old probe: ureq saw transport error → miss.
                            // New probe: connect already succeeded → hit.
                            drop(s);
                        }
                    }
                    Err(ref e)
                        if e.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        let url = format!("http://127.0.0.1:{port}");
        let res =
            wait_for_agent_silent(&url, Duration::from_millis(800)).await;
        stop.store(true, Ordering::Relaxed);
        let err = res.expect_err("alternating-accept must still time out");
        // PR1 inversion of the prior R16-I2 invariant: persistent
        // accept → counter stays at 0, regardless of post-accept
        // drop semantics.
        assert!(
            err.contains("consecutive_misses=0"),
            "PR1 regression: connect-only probe must NOT classify \
             post-accept drop as a miss; expected final \
             consecutive_misses=0; got {err:?}"
        );
        assert!(
            err.contains("still answering"),
            "fence-timeout text invariant; got {err:?}"
        );
    }

    /// R16-A2 / R16-I2 TIMEOUT-case pin. Persistent 200 means
    /// `consecutive_misses` never increments past 0 — that's the
    /// observable difference from the LEAK case above. The final
    /// counter value MUST land in the error string so the
    /// `host_fence: deadline reached` log + the propagated `errs`
    /// vector both carry the disambiguator.
    #[compio::test]
    async fn host_fence_timeout_case_persistent_answer_reports_misses_zero() {
        let body = r#"{"agent_version":"x","pubkey_fingerprint":"deadbeef00112233"}"#
            .to_string();
        let (port, stop) = spawn_mock_agent(body, 200);
        let url = format!("http://127.0.0.1:{port}");
        let res =
            wait_for_agent_silent(&url, Duration::from_millis(500)).await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let err = res.expect_err("persistent-200 must time out");
        // Persistent answer → counter resets every probe → final 0.
        // This is the discriminator from the LEAK case (=1).
        assert!(
            err.contains("consecutive_misses=0"),
            "TIMEOUT case must report consecutive_misses=0 in the \
             error string (vs LEAK's =1); got {err:?}"
        );
    }

    /// R16-A2 cadence + 2-in-a-row contract pin. Locks the two
    /// load-bearing magic numbers in `wait_for_agent_silent` so a
    /// future "tune the cadence" PR can't silently break the
    /// smoke-r12 phase-log latency assumptions:
    ///   • 100 ms sleep between polls (lower bound: 2 misses → ≥ 100 ms)
    ///   • 2-consecutive-misses threshold (upper bound: ≤ 800 ms for
    ///     a refused port).
    /// If either constant changes, this test fires.
    #[compio::test]
    async fn host_fence_cadence_and_threshold_pin() {
        let started = Instant::now();
        let res = wait_for_agent_silent(
            "http://127.0.0.1:1",
            Duration::from_secs(5),
        )
        .await;
        assert!(res.is_ok());
        let elapsed = started.elapsed();
        // Lower bound: 2 misses at 100 ms cadence → first miss is
        // immediate, second is after >= 100 ms of sleep. If someone
        // shrinks the cadence below 50 ms the OK case becomes too
        // tight for the smoke phase-log heuristics; this fires.
        assert!(
            elapsed >= Duration::from_millis(50),
            "R16-A2 cadence floor: 2-miss path returned in {elapsed:?}; \
             expected >= 50ms (100ms cadence between miss #1 and miss #2). \
             Did the inter-probe sleep change?"
        );
        // Upper bound: the existing 800ms cap, kept for redundancy
        // with `host_fence_clears_quickly_after_two_consecutive_misses`
        // so a regression flips both tests.
        assert!(
            elapsed < Duration::from_millis(800),
            "R16-A2 threshold pin: 2-in-a-row took {elapsed:?}; if the \
             threshold moved from 2 the smoke-r12 phase-log latency \
             window needs to follow."
        );
    }

    // ─── C-7-LT-2-PR1: compio-native TCP-connect probe behavior ──
    //
    // The smoke-r13 wedge ("probes=1 in 30 s") came from ureq's
    // request-deadline timeout NOT being a connect-timeout on a
    // half-collapsed TAP. PR1 replaces that with an outer
    // `compio::time::timeout` over `TcpStream::connect`. These
    // tests pin the contract:
    //   1. reachable port → returns true fast (~ms)
    //   2. refused port → returns false fast (~ms; ECONNREFUSED)
    //   3. unroutable / black-hole address → returns false bounded by
    //      our connect_timeout (NOT by the kernel's SYN-retransmit).

    #[compio::test]
    async fn pr1_probe_reachable_port_returns_true() {
        // Bind + accept on loopback. The probe must see SYN-ACK and
        // return true well inside the 150 ms connect window.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((_s, _)) => {} // accept-and-drop; we only care about SYN
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        let addr: std::net::SocketAddr =
            format!("127.0.0.1:{port}").parse().unwrap();
        let started = Instant::now();
        let r =
            probe_agent_reachable_tcp(addr, Duration::from_millis(150)).await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(r, "loopback listener must read as reachable");
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "reachable connect should complete well under timeout; \
             got {:?}",
            started.elapsed()
        );
    }

    #[compio::test]
    async fn pr1_probe_refused_port_returns_false_fast() {
        // 127.0.0.1:1 is reserved + bound to nothing on every CI host
        // we run; kernel returns ECONNREFUSED ~immediately (no SYN
        // retransmit on the loopback). Must be < 10 ms.
        let addr: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
        let started = Instant::now();
        let r =
            probe_agent_reachable_tcp(addr, Duration::from_millis(150)).await;
        let elapsed = started.elapsed();
        assert!(!r, "refused port must read as miss");
        // 50 ms is conservative for CI; loopback refused is typically
        // sub-millisecond. This is the "kernel says no fast" lane —
        // confirms we're not stuck in the timeout path.
        assert!(
            elapsed < Duration::from_millis(50),
            "refused connect on loopback should be fast (<50ms); \
             got {elapsed:?} — is the kernel SYN-retransmitting?"
        );
    }

    /// The wedge fix invariant: a connect target that the kernel
    /// can't reach (no route OR firewall DROP) MUST surface "miss"
    /// within our compio-side `connect_timeout`, NOT after the
    /// kernel's SYN-retransmit ceiling (typically 30-90 s on Linux
    /// defaults). This is the difference between smoke-r13's
    /// "1 probe in 30 s" pathology and a healthy fence.
    ///
    /// TEST-NET-1 (`192.0.2.0/24`, RFC 5737) is documentation-only —
    /// no host should answer. On most Linux configs the kernel
    /// either:
    ///   (a) returns EHOSTUNREACH/ENETUNREACH at connect() — fast miss; OR
    ///   (b) sends SYNs that get no reply — slow miss, capped by our
    ///       outer compio timeout at 150 ms (this is the wedge case).
    /// Either way the probe MUST return false in well under 1 second
    /// — if a future regression drops the outer timeout, the kernel's
    /// 30-90 s SYN-retransmit ceiling will fire this test.
    #[compio::test]
    async fn pr1_probe_unroutable_address_returns_false_within_timeout() {
        // 192.0.2.1 is RFC 5737 TEST-NET-1 — should never route.
        let addr: std::net::SocketAddr = "192.0.2.1:7777".parse().unwrap();
        let started = Instant::now();
        let r =
            probe_agent_reachable_tcp(addr, Duration::from_millis(150)).await;
        let elapsed = started.elapsed();
        assert!(!r, "unroutable address must read as miss");
        // The wedge fix's load-bearing assertion: the outer compio
        // timeout caps the connect; the kernel's SYN-retransmit
        // ceiling MUST NOT govern. 750 ms is a generous CI cap;
        // healthy machines see this in <200 ms (timeout case) or
        // ~10 ms (EHOSTUNREACH-at-connect case).
        assert!(
            elapsed < Duration::from_millis(750),
            "C-7-LT-2-PR1 regression: unroutable connect took \
             {elapsed:?} — the outer compio::time::timeout is NOT \
             capping a stuck SYN. Did the timeout get removed or did \
             a future regression bring back ureq's request-deadline?"
        );
    }

    /// Counter-reset contract. With the new connect-only probe:
    /// 1 miss (e.g. refused) followed by a reachable port → counter
    /// returns to 0. This invariant is what prevents the
    /// alternating-shape failures from compounding.
    ///
    /// Verified indirectly via the PR1-alternating test above (which
    /// pins `consecutive_misses=0` at deadline → counter MUST be
    /// resetting each reachable probe). This unit-level test fixes
    /// the property at the function boundary: 1 refused + N reachable
    /// in a row → fence does NOT clear at "2 in a row" because the
    /// first miss is followed by a hit (counter resets), and a single
    /// reachable forever after pins to 0.
    #[compio::test]
    async fn pr1_one_miss_then_reachable_resets_counter() {
        // Listener that goes ACTIVE after a brief delay: first probe
        // refused → second+ probe reachable. Implementation: bind
        // 50 ms after the fence starts. While bound and accepting,
        // the connect-probe returns reachable; the fence then
        // persistently reads "agent answering" and must time out
        // with consecutive_misses=0 (NOT cleared at threshold=2).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        std::thread::spawn(move || {
            while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((_s, _)) => {} // accept-and-drop
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        // Drive the fence at this listener for a short budget. The
        // socket is up immediately, so every probe sees reachable
        // and the fence times out cleanly with consecutive_misses=0.
        let url = format!("http://127.0.0.1:{port}");
        let res =
            wait_for_agent_silent(&url, Duration::from_millis(500)).await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let err = res.expect_err("reachable listener must time out");
        assert!(
            err.contains("consecutive_misses=0"),
            "PR1 counter-reset: every reachable probe must reset the \
             miss counter; expected final consecutive_misses=0; got \
             {err:?}"
        );
    }

    /// Host-port parser surface check. The fence's caller hands in
    /// `http://<ipv4>:<port>` strings built from `vm_index`; PR1's
    /// new `parse_agent_probe_addr` must handle that shape AND a few
    /// reasonable variants without burning fence budget on a parse
    /// failure mid-loop. Also pins the empty-input failure path
    /// because a malformed `agent_url` should surface as a leak with
    /// a clear error (not a panic, not a silent OK).
    #[test]
    fn pr1_parse_agent_probe_addr_accepts_expected_shapes() {
        // The canonical shape the controller builds.
        let a = parse_agent_probe_addr("http://10.99.101.2:7777")
            .expect("canonical http://ipv4:port must parse");
        assert_eq!(a.port(), 7777);
        assert_eq!(a.ip(), "10.99.101.2".parse::<std::net::IpAddr>().unwrap());
        // With a trailing path (in case a future caller hands in
        // `…/livez`).
        let a = parse_agent_probe_addr("http://127.0.0.1:8080/livez")
            .expect("path suffix must be stripped");
        assert_eq!(a.port(), 8080);
        // Bare host:port (no scheme) — defensive accept.
        let a = parse_agent_probe_addr("127.0.0.1:9999")
            .expect("scheme-less host:port must parse");
        assert_eq!(a.port(), 9999);
        // Empty → Err.
        assert!(
            parse_agent_probe_addr("").is_err(),
            "empty input must Err"
        );
        // Scheme-only → Err (no host:port after `://`).
        assert!(
            parse_agent_probe_addr("http://").is_err(),
            "scheme-without-host must Err"
        );
    }

    #[compio::test]
    async fn wait_for_agent_livez_times_out_on_persistent_401() {
        // /livez=200 but /version=401 — agent is verifying with a
        // *different* pubkey (stale tenant whose key file at
        // /run/keys/controller-pubkey predates our create()).
        // Polling never resolves; deadline expires.
        let sk = make_sk();
        let our_fp = sig::pubkey_fingerprint(&sk.verifying_key());
        let (port, stop) = spawn_mock_agent("{\"error\":\"unauthorized\"}".to_string(), 401);
        let url = format!("http://127.0.0.1:{port}");
        let res = wait_for_agent_livez(
            &url,
            &our_fp,
            &sk,
            Duration::from_millis(600),
        )
        .await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let err = res.expect_err("must time out");
        assert!(
            err.contains("401") || err.contains("different controller pubkey"),
            "FM-A regression: persistent-401 timeout did not surface \
             'verifying with a different controller pubkey'; got {err:?}"
        );
    }

    /// Phase-0 surface check (preview-URL design § II.0): the
    /// nomad-ch backend's `session_auth` lifts `signing_key`,
    /// `agent_url`, and a derived `pubkey_fp` into the
    /// backend-agnostic `SandboxAuth` envelope. Missing-id surfaces
    /// as `Err`. This is what the preview proxy + sealed-record
    /// persistence layer call.
    #[compio::test]
    async fn session_auth_returns_lifted_record() {
        let backend = NomadCHBackend::new(make_cfg(), None).expect("new");

        // Hand-insert a sandbox record with a known signing key to
        // bypass the full Nomad-create path (this is the standard
        // unit-test trick used elsewhere in this module).
        let sk = make_sk();
        let expected_fp = sig::pubkey_fingerprint(&sk.verifying_key());
        let id = Uuid::now_v7();
        let agent_url = "http://10.99.107.2:7777".to_string();
        backend.state.write().unwrap().insert(
            id,
            NomadChSandbox {
                user_id: "alice".into(),
                job_id: "zsbx-test".into(),
                vm_index: 7,
                host_dir: PathBuf::from("/tmp/zsbx-test"),
                agent_url: agent_url.clone(),
                signing_key: sk.clone(),
            },
        );

        let auth = backend.session_auth(id).await.expect("session_auth");
        assert_eq!(auth.agent_url, agent_url);
        assert_eq!(auth.pubkey_fp, expected_fp);
        // Pointer-equal Arc clone: no secret-bytes copy.
        assert!(Arc::ptr_eq(&auth.signing_key, &sk));

        // Unknown id surfaces a clear Err.
        let other = Uuid::now_v7();
        let err = backend.session_auth(other).await.expect_err("missing");
        assert!(
            err.contains("not found"),
            "Err must mention not-found; got {err:?}"
        );
    }

    // ─── Bug #15: stop_preserving_state preserves host_dir ──────
    //
    // The snapshot teardown path (`teardown_source_for_snapshot` →
    // `stop_preserving_state` → `stop_inner(.., false)`) MUST NOT
    // remove the per-sandbox `host_dir`, because that directory owns
    // `workspace.img` — the durable per-sandbox storage that the
    // next wake's wrapper re-mounts. The wrapper's gate
    // `[ ! -f $ZSBX_WORKSPACE_IMG ] && exit 1` (see
    // `crates/sandbox/scripts/nomad-vm-wrapper.sh`) is what blew up
    // empirically in the 2026-05-23 cluster smoke.
    //
    // This test exercises the full `stop_inner` path against a tiny
    // TcpListener-backed Nomad mock that 404s every request — which
    // both `stop_nomad_job` (DELETE accepts 200|404) and
    // `wait_for_job_gone` (GET 404 → Ok) consume as "job is gone".
    // `host_fence_timeout_secs = 0` bypasses the fence loop (the
    // 500ms grace sleep is acceptable inside a #[compio::test]).
    // We then assert: (1) the in-memory record is removed (steps
    // 1-4 ran), and (2) the host_dir + its sentinel file survive
    // (step 5 was skipped).

    /// Spin up a TcpListener that responds 404 to every request
    /// (with Content-Length: 0). Returns (port, stop_flag). The
    /// thread exits when the flag flips. Reusing the existing
    /// `spawn_mock_agent` style.
    fn spawn_404_mock() -> (u16, std::sync::Arc<std::sync::atomic::AtomicBool>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let port = listener.local_addr().expect("addr").port();
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_millis(200)))
                            .ok();
                        stream
                            .set_write_timeout(Some(Duration::from_millis(200)))
                            .ok();
                        let mut buf = [0u8; 1024];
                        let _ = stream.read(&mut buf).unwrap_or(0);
                        let resp =
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        (port, stop)
    }

    /// Process-unique tempdir (mirrors the
    /// `crates/sandbox/src/snapshot_store.rs::fresh_root` pattern —
    /// avoids pulling in the `tempfile` crate for one test).
    fn fresh_host_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "zsbx-b15-{}-{}-{}",
            tag,
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[compio::test]
    async fn stop_preserving_state_does_not_remove_host_dir() {
        // 1. Spin up the 404-mock for Nomad. Every request → 404.
        let (port, stop_flag) = spawn_404_mock();
        let nomad_addr = format!("http://127.0.0.1:{port}");

        // 2. Build a cfg pointed at the mock + fence disabled.
        let mut cfg = make_cfg();
        cfg.nomad_ch.nomad_addr = nomad_addr;
        cfg.nomad_ch.host_fence_timeout_secs = 0; // skip /livez fence

        let backend = NomadCHBackend::new(cfg, None).expect("new");

        // 3. Hand-insert a sandbox record with a real on-disk host_dir
        //    + sentinel file (stand-in for workspace.img).
        let id = Uuid::now_v7();
        let host_dir = fresh_host_dir("preserve");
        let sentinel = host_dir.join("workspace.img");
        std::fs::write(&sentinel, b"PRESERVE-ME").expect("write sentinel");
        backend.state.write().unwrap().insert(
            id,
            NomadChSandbox {
                user_id: "usr_b15_preserve".into(),
                job_id: "zsbx-b15-preserve".into(),
                vm_index: 42,
                host_dir: host_dir.clone(),
                // agent_url 127.0.0.1:1 is unreachable; /shutdown
                // errors but it's best-effort and ignored.
                agent_url: "http://127.0.0.1:1".into(),
                signing_key: make_sk(),
            },
        );

        // 4. Snapshot-teardown variant: MUST preserve host_dir.
        let res = backend.stop_preserving_state(id).await;
        // The /shutdown call to 127.0.0.1:1 errors → res is Err
        // with a /shutdown blurb, but the post-conditions we care
        // about are observed regardless.
        let _ = res;

        // 5a. In-memory state was reaped (step 4 ran).
        assert!(
            backend.state.read().unwrap().get(&id).is_none(),
            "stop_preserving_state must remove the in-memory record \
             (step 4 of the teardown ran)"
        );

        // 5b. host_dir + sentinel file SURVIVE (step 5 was skipped —
        //     this is the bug #15 invariant).
        assert!(
            host_dir.exists(),
            "B15 regression: stop_preserving_state removed host_dir; \
             the next wake's wrapper [ ! -f $ZSBX_WORKSPACE_IMG ] gate \
             will exit 1"
        );
        assert!(
            sentinel.exists(),
            "B15 regression: stop_preserving_state removed \
             host_dir/workspace.img; durable per-sandbox storage gone"
        );
        let contents = std::fs::read(&sentinel).expect("read sentinel");
        assert_eq!(
            contents,
            b"PRESERVE-ME",
            "B15 regression: workspace.img sentinel was modified"
        );

        // Cleanup.
        stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(&host_dir);
    }

    #[compio::test]
    async fn stop_for_real_leaks_host_dir_for_sweeper() {
        // T-8b-stress-r2 controller v34: stop() now LEAKS the
        // host_dir on purpose so a concurrent retry-CREATE for the
        // same sandbox_id never observes workspace.img missing mid-
        // alloc. Sweeper-owned GC (`sweep::spawn_host_dir_gc`) is the
        // catchall — see `crates/sandbox/src/sweep.rs::run_host_dir_gc_once`
        // for the eligibility gates (terminal state, no pending
        // wake_jobs row, mtime > grace).
        //
        // This test is the mirror of B15's
        // `stop_preserving_state_does_not_remove_host_dir`: both stop
        // paths now share the leak-and-defer-to-sweeper contract.
        // Pre-v34 this test asserted `!host_dir.exists()`; flipped
        // here to assert the NEW contract.
        let (port, stop_flag) = spawn_404_mock();
        let nomad_addr = format!("http://127.0.0.1:{port}");

        let mut cfg = make_cfg();
        cfg.nomad_ch.nomad_addr = nomad_addr;
        cfg.nomad_ch.host_fence_timeout_secs = 0;

        let backend = NomadCHBackend::new(cfg, None).expect("new");
        let id = Uuid::now_v7();
        let host_dir = fresh_host_dir("for-real");
        let sentinel = host_dir.join("workspace.img");
        std::fs::write(&sentinel, b"WIPE-ME").expect("write sentinel");
        backend.state.write().unwrap().insert(
            id,
            NomadChSandbox {
                user_id: "usr_b15_forreal".into(),
                job_id: "zsbx-b15-forreal".into(),
                vm_index: 43,
                host_dir: host_dir.clone(),
                agent_url: "http://127.0.0.1:1".into(),
                signing_key: make_sk(),
            },
        );

        let _ = backend.stop(id).await;

        assert!(
            backend.state.read().unwrap().get(&id).is_none(),
            "stop must remove the in-memory record"
        );
        // v34 invariant: host_dir survives stop() under favourable
        // conditions (404-mock → job_confirmed_gone=true,
        // fence_secs=0 → fence_passed=true). Pre-v34 this assertion
        // was inverted; the v34 fix moves host_dir GC to the
        // sweeper.
        assert!(
            host_dir.exists(),
            "v34 regression: stop() removed host_dir; the per-alloc \
             host_dir cleanup MUST leak so a concurrent retry-CREATE \
             for the same sandbox_id never observes workspace.img \
             missing mid-alloc. Sweeper-owned GC reaps it later."
        );
        assert!(
            sentinel.exists(),
            "v34 regression: stop() removed workspace.img"
        );
        let contents = std::fs::read(&sentinel).expect("read sentinel");
        assert_eq!(
            contents,
            b"WIPE-ME",
            "v34 regression: workspace.img sentinel was modified by stop()"
        );

        stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        // Cleanup our own test fixture (the sweeper isn't running
        // here, so we manually rm the leaked dir).
        let _ = std::fs::remove_dir_all(&host_dir);
    }

    // ─── C2 regression: persist.delete must be gated on
    //     remove_host_dir, mirroring step 5's host_dir rm. The B15
    //     fix gated host_dir cleanup but left persist.delete firing
    //     unconditionally — latent today (wake doesn't read the
    //     sealed record yet) but bites the moment sealed-record-
    //     based key recovery lands. Source:
    //     `docs/reviews/sandbox-snapshot-restore-deferred.md` C2.

    #[compio::test]
    async fn stop_preserving_state_does_not_delete_sealed_record() {
        // 1. Spin up the 404-mock for Nomad (same fixture as the
        //    B15 host_dir test). Every request → 404.
        let (port, stop_flag) = spawn_404_mock();
        let nomad_addr = format!("http://127.0.0.1:{port}");

        // 2. cfg pointed at the mock + fence disabled (so the
        //    teardown reaches the persist.delete tail).
        let mut cfg = make_cfg();
        cfg.nomad_ch.nomad_addr = nomad_addr;
        cfg.nomad_ch.host_fence_timeout_secs = 0;

        // 3. Build a real Persistence rooted at a fresh tempdir,
        //    wire it into the backend, and seal a record for our
        //    synthetic sandbox so the C2 invariant has something to
        //    observe.
        let persist_root = fresh_host_dir("c2-persist");
        let key = crate::persist::AeadKey::from_bytes([0x5c; 32]);
        let persist = Arc::new(crate::persist::Persistence::new(persist_root.clone(), key));
        let backend = NomadCHBackend::new(cfg, Some(persist.clone())).expect("new");

        let id = Uuid::now_v7();
        let record = crate::persist::SealedAuth {
            version: crate::persist::SEAL_VERSION,
            sandbox_id: id.to_string(),
            signing_key_bytes: [0xab; 32],
            preview_secrets: None,
            boot_id: Some(1),
        };
        persist.seal(id, &record).await.expect("seal sealed record");
        let sealed_path = persist
            .sealed_records_dir()
            .join(crate::persist::seal_filename_for(id));
        assert!(
            sealed_path.exists(),
            "test setup: persist.seal must place the sealed file on disk"
        );

        // 4. Hand-insert the in-memory sandbox record. The host_dir
        //    is irrelevant to the C2 contract but stop_inner expects
        //    a real path it can stat; reuse the B15 fixture style.
        let host_dir = fresh_host_dir("c2-host");
        let sentinel = host_dir.join("workspace.img");
        std::fs::write(&sentinel, b"PRESERVE-ME").expect("write sentinel");
        backend.state.write().unwrap().insert(
            id,
            NomadChSandbox {
                user_id: "usr_c2_preserve".into(),
                job_id: "zsbx-c2-preserve".into(),
                vm_index: 51,
                host_dir: host_dir.clone(),
                agent_url: "http://127.0.0.1:1".into(),
                signing_key: make_sk(),
            },
        );

        // 5. Snapshot-teardown variant: MUST preserve the sealed
        //    record on disk. /shutdown to 127.0.0.1:1 errors → Err
        //    return is expected, but the post-conditions are what
        //    we're pinning.
        let _ = backend.stop_preserving_state(id).await;

        // 5a. In-memory state was reaped (the rest of the teardown
        //     ran — proves we hit the persist.delete branch, not an
        //     early-return).
        assert!(
            backend.state.read().unwrap().get(&id).is_none(),
            "stop_preserving_state must still remove the in-memory \
             record (steps 1-4 ran)"
        );

        // 5b. host_dir + sentinel survive (B15 invariant — sanity
        //     check we didn't accidentally regress while wiring C2).
        assert!(
            host_dir.exists(),
            "regression check: stop_preserving_state removed host_dir"
        );
        assert!(
            sentinel.exists(),
            "regression check: stop_preserving_state removed workspace.img"
        );

        // 5c. THE C2 INVARIANT: the sealed record survives.
        assert!(
            sealed_path.exists(),
            "C2 regression: stop_preserving_state deleted the sealed \
             record at {}; the next wake's sealed-record-based key \
             recovery will fail",
            sealed_path.display()
        );

        // 6. Now the for-real stop must finally reap the sealed
        //    record (symmetric with how it reaps host_dir). Re-
        //    insert the in-memory record since step 5 removed it.
        backend.state.write().unwrap().insert(
            id,
            NomadChSandbox {
                user_id: "usr_c2_preserve".into(),
                job_id: "zsbx-c2-preserve".into(),
                vm_index: 51,
                host_dir: host_dir.clone(),
                agent_url: "http://127.0.0.1:1".into(),
                signing_key: make_sk(),
            },
        );
        let _ = backend.stop(id).await;

        assert!(
            !sealed_path.exists(),
            "stop (the for-real variant) MUST delete the sealed \
             record at {}; got file still present",
            sealed_path.display()
        );

        // Cleanup.
        stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(&host_dir);
        let _ = std::fs::remove_dir_all(&persist_root);
    }

    // ─── B19 regression: register_restored must install the
    //     post-wake VM into the backend's state map so subsequent
    //     exec/stop/delete find it (closing the cluster-smoke
    //     2026-05-23 r4 "sandbox not found" + vm_index leak).
    //     Source: docs/reviews/sandbox-snapshot-restore-deferred.md
    //     B19.

    #[compio::test]
    async fn register_restored_inserts_into_state_map() {
        // Construct a NomadCHBackend with no Persistence (this method
        // never touches persist; only state.write().insert(...)).
        // Call register_restored with synthetic values and assert the
        // state map carries the matching NomadChSandbox.
        let cfg = make_cfg();
        let backend = NomadCHBackend::new(cfg, None).expect("new");
        let id = Uuid::now_v7();
        let user_id = "usr_b19_register".to_string();
        let vm_index: u16 = 5;
        let signing_seed = [0x9eu8; 32];
        let expected_pubkey_fp = sig::pubkey_fingerprint(
            &SigningKey::from_bytes(&signing_seed).verifying_key(),
        );
        let agent_url = backend.derive_agent_url(vm_index);

        backend
            .register_restored(
                id,
                vm_index,
                signing_seed,
                agent_url.clone(),
                user_id.clone(),
            )
            .expect("first register must succeed");

        let guard = backend.state.read().unwrap();
        let entry = guard
            .get(&id)
            .expect("B19 regression: register_restored did not insert into state map");
        assert_eq!(entry.user_id, user_id);
        assert_eq!(entry.vm_index, vm_index);
        assert_eq!(entry.agent_url, agent_url);
        // Job id matches the create-side derivation (zsbx-<simple>).
        assert_eq!(entry.job_id, format!("zsbx-{}", id.simple()));
        // signing_key is the same 32-byte seed we passed in.
        let got_fp = sig::pubkey_fingerprint(&entry.signing_key.verifying_key());
        assert_eq!(
            got_fp, expected_pubkey_fp,
            "B19 regression: register_restored stored a signing key \
             whose pubkey fingerprint doesn't match the seed we handed in"
        );
        drop(guard);

        // Second register against the same sandbox_id must Err: a
        // live state-map entry MUST NOT be clobbered.
        let err = backend
            .register_restored(
                id,
                vm_index,
                signing_seed,
                agent_url,
                user_id,
            )
            .expect_err("second register must reject (would clobber live record)");
        assert!(
            err.contains("already present"),
            "B19 regression: clobber-refusal error did not surface 'already present'; got {err:?}"
        );
    }

    #[compio::test]
    async fn restored_sandbox_is_stoppable_and_releases_vm_index() {
        // Closes the second half of B19: the slot leak. Before the
        // fix, stop_inner's idempotent-Ok branch fired without
        // releasing the allocator because the post-wake sandbox was
        // never in the state map. Register a restored sandbox via
        // the new method, then stop it under favourable conditions,
        // assert (a) the state-map entry is gone and (b) the
        // vm_index is back in the allocator's free list (the
        // create-side `alloc()` would hand it out again next).
        let (port, stop_flag) = spawn_404_mock();
        let nomad_addr = format!("http://127.0.0.1:{port}");

        // Use a parent dir under temp so the backend derives a host_dir
        // we can pre-materialise (stop_inner step 5 will rm it).
        let host_state_parent = fresh_host_dir("b19-stop");

        let mut cfg = make_cfg();
        cfg.nomad_ch.nomad_addr = nomad_addr;
        cfg.nomad_ch.host_fence_timeout_secs = 0;
        cfg.nomad_ch.host_state_dir = host_state_parent.clone();
        // Tight pool so we can assert the slot is reclaimed by index
        // (after register/stop it should be the smallest free index).
        cfg.nomad_ch.vm_index_floor = 4;
        cfg.nomad_ch.vm_index_ceil = 6;

        let backend = NomadCHBackend::new(cfg, None).expect("new");
        // Pre-reserve slot 4 so the next alloc() would hand out 5 if
        // the free-list was empty. We restore into slot 4 and prove
        // it lands back in the free-list after stop.
        let allocator = backend.vm_index_allocator();
        allocator
            .lock()
            .unwrap()
            .reserve(4)
            .expect("pre-reserve slot 4");

        let id = Uuid::now_v7();
        // Materialise the per-sandbox host_dir the backend derives
        // (`<host_state_dir>/<sandbox-id>/`) so step 5's rm has
        // something to wipe.
        let host_dir = host_state_parent.join(id.to_string());
        std::fs::create_dir_all(&host_dir).expect("mkdir host_dir");
        std::fs::write(host_dir.join("workspace.img"), b"WIPE-ME")
            .expect("write sentinel");

        backend
            .register_restored(
                id,
                4u16,
                [0xb1; 32],
                "http://127.0.0.1:1".into(),
                "usr_b19_stop".into(),
            )
            .expect("register restored");

        // Sanity: state map carries the entry, and slot 4 is held by
        // the allocator (we pre-reserved it; register_restored does
        // not touch the allocator).
        assert!(
            backend.state.read().unwrap().get(&id).is_some(),
            "test setup: register_restored must place an entry"
        );

        // Now stop. Under the favourable 404-mock + fence_secs=0
        // setup, the stop path reaches step 6 (vm_index release).
        let _ = backend.stop(id).await;

        assert!(
            backend.state.read().unwrap().get(&id).is_none(),
            "B19 regression: stop on a registered-restored sandbox \
             must remove the state-map entry"
        );
        // Slot 4 must be back in the allocator's free list. Round-
        // trip via alloc(): it should hand out 4 first (freed slots
        // win over `next`).
        //
        // T-8b-stress-r8 r24-A2-S3: release is now spawned as a
        // detached compio task so the controller can delay release
        // in production without blocking the stop ACK. The test
        // config sets `vm_index_release_delay_secs=0` so the task
        // still fires fast — poll for slot 4 to appear in the
        // freed set before consuming via alloc(). A direct alloc()
        // race against the release task could return 5 (next slot)
        // while the freed set is still empty.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut freed_observed = false;
        while Instant::now() < deadline {
            let has_4 = {
                let a = allocator.lock().unwrap();
                a.freed_for_test().contains(&4)
            };
            if has_4 {
                freed_observed = true;
                break;
            }
            compio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            freed_observed,
            "B19 regression: slot 4 not present in freed set after \
             stop within 2 s budget — r24-A2-S3 delayed-release task \
             never fired"
        );
        let reclaimed = allocator.lock().unwrap().alloc().expect("alloc");
        assert_eq!(
            reclaimed, 4,
            "B19 regression: vm_index slot 4 not returned to allocator \
             after stop; got {reclaimed} (pool=[4,6], pre-reserved slot 4)"
        );

        stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(&host_state_parent);
    }

    // ─── R10-C1 regression (concurrency-r10 2026-05-25):
    //     `unregister_restored` is the symmetric inverse of
    //     `register_restored` used by the rollback path. Without it,
    //     a CasLost on the final `update_sandbox_status(Running)` left
    //     the state-map entry alive while vm_index went back into the
    //     allocator pool — a ghost sandbox at a slot the next create
    //     would land on top of. Source:
    //     docs/reviews/sandbox-snapshot-restore-concurrency-2026-05-25-r10.md
    //     R10-C1.

    #[compio::test]
    async fn unregister_restored_removes_state_map_entry() {
        let cfg = make_cfg();
        let backend = NomadCHBackend::new(cfg, None).expect("new");
        let id = Uuid::now_v7();
        let vm_index: u16 = 7;
        let agent_url = backend.derive_agent_url(vm_index);

        backend
            .register_restored(
                id,
                vm_index,
                [0xa1; 32],
                agent_url,
                "usr_r10c1_unreg".into(),
            )
            .expect("register must succeed");

        // Pre-condition: state map carries the entry.
        assert!(
            backend.state.read().unwrap().get(&id).is_some(),
            "test setup: register_restored must place an entry"
        );

        let removed = backend.unregister_restored(id);
        assert!(removed, "R10-C1: unregister_restored must report removal");
        assert!(
            backend.state.read().unwrap().get(&id).is_none(),
            "R10-C1 regression: unregister_restored did not remove the \
             state-map entry"
        );

        // Idempotency: a second call is a no-op and returns false.
        let removed_again = backend.unregister_restored(id);
        assert!(
            !removed_again,
            "R10-C1: second unregister must be a no-op (no entry to remove)"
        );
    }

    /// R10-C1 follow-on invariant: after `register_restored` +
    /// `unregister_restored`, a subsequent `register_restored` at the
    /// SAME `sandbox_id` must succeed (the Vacant slot is the inverse
    /// of the rollback symmetry).
    #[compio::test]
    async fn unregister_restored_re_register_succeeds() {
        let cfg = make_cfg();
        let backend = NomadCHBackend::new(cfg, None).expect("new");
        let id = Uuid::now_v7();
        let vm_index: u16 = 9;
        let agent_url = backend.derive_agent_url(vm_index);

        backend
            .register_restored(
                id,
                vm_index,
                [0x33; 32],
                agent_url.clone(),
                "usr_r10c1_rereg".into(),
            )
            .expect("first register");
        assert!(backend.unregister_restored(id), "unregister returns true");

        backend
            .register_restored(
                id,
                vm_index,
                [0x33; 32],
                agent_url,
                "usr_r10c1_rereg".into(),
            )
            .expect(
                "R10-C1: post-unregister, register_restored at the same \
                 sandbox_id must succeed (Vacant slot)",
            );
    }

    // ─── r3-A — local Nomad node_id discovery (parser) ───────
    //
    // The HTTP transport is exercised end-to-end at cluster boot;
    // these tests pin the JSON-decoder invariants so a Nomad-version
    // shape drift surfaces with a unit failure instead of a silent
    // `None` fallback (which would degrade to pre-r3-A random
    // placement under WORKER_COUNT>1).

    /// Happy path: lowercase keys (Nomad 1.x live shape) yield the
    /// node_id verbatim.
    #[test]
    fn parse_nomad_agent_self_node_id_lowercase_keys() {
        let body = r#"{
            "config": {},
            "stats": {
                "client": {
                    "node_id": "0123456789abcdef-1111-2222-3333-444455556666"
                }
            },
            "member": {}
        }"#;
        let id = parse_nomad_agent_self_node_id(body)
            .expect("lowercase shape must parse");
        assert_eq!(id, "0123456789abcdef-1111-2222-3333-444455556666");
    }

    /// Defensive fallback: PascalCase keys (older docs / agent-version
    /// drift) also yield the node_id verbatim.
    #[test]
    fn parse_nomad_agent_self_node_id_pascal_case_keys() {
        let body = r#"{
            "Stats": {
                "Client": {
                    "NodeID": "PASCAL-CASE-NODE-ID"
                }
            }
        }"#;
        let id = parse_nomad_agent_self_node_id(body)
            .expect("PascalCase shape must parse");
        assert_eq!(id, "PASCAL-CASE-NODE-ID");
    }

    /// Server-only agent: no `stats.client` block. The function
    /// returns Err so the boot path can demote to WARN + leave the
    /// AppState field as None (random-placement fallback).
    #[test]
    fn parse_nomad_agent_self_node_id_missing_client_block_errs() {
        let body = r#"{
            "config": {},
            "stats": {
                "runtime": { "version": "1.7.0" }
            }
        }"#;
        let err = parse_nomad_agent_self_node_id(body)
            .expect_err("server-only agent must surface as Err");
        assert!(
            err.contains("missing stats.client.node_id"),
            "error message must point operator at the missing field, got: {err}"
        );
    }

    /// Empty node_id is treated as a hard error — an agent that
    /// reports the field but with empty content is malformed in the
    /// same way a missing field is.
    #[test]
    fn parse_nomad_agent_self_node_id_empty_string_errs() {
        let body = r#"{
            "stats": { "client": { "node_id": "" } }
        }"#;
        let err = parse_nomad_agent_self_node_id(body)
            .expect_err("empty node_id must surface as Err");
        assert!(
            err.contains("present but empty"),
            "error message must distinguish empty-vs-missing, got: {err}"
        );
    }

    /// Non-JSON body (e.g., HTML interstitial from a misrouted proxy)
    /// surfaces as a parse error.
    #[test]
    fn parse_nomad_agent_self_node_id_malformed_body_errs() {
        let body = "<html>nope</html>";
        let err = parse_nomad_agent_self_node_id(body)
            .expect_err("non-JSON body must surface as Err");
        assert!(
            err.contains("parse /v1/agent/self body"),
            "error message must name the source, got: {err}"
        );
    }

    // ─── Option C Phase 2 — staging-locality flag emission ────
    //
    // Mirrors R22-T1 / r3-A node-affinity parity test shape: pin
    // the cross-emitter behaviour of the new `stage_disk_images`
    // ChPlugin Config field + `zsbx_stage_disks` job-level Meta.
    // When the controller-side flag is true, every cold-boot
    // jobspec MUST advertise driver-side staging via BOTH wire
    // surfaces (typed Config for the driver, Meta for observability);
    // when false, both surfaces fall back to the legacy false value.
    // Restore-path emitter is unaffected by the flag (its
    // `stage_disk_images` is hard-coded false per ADR Phase 2).

    /// Cold-boot under `driver_stages_disk_images=true`: the ch driver
    /// Config carries `stage_disk_images=true` (Go driver decodes it
    /// from the HCL schema and runs `stageDiskImages` before CH
    /// spawn) AND the job-level Meta carries `zsbx_stage_disks=true`
    /// (operator-facing signal via `nomad job inspect`).
    #[test]
    fn cold_boot_jobspec_includes_stage_disks_meta_when_flag_set() {
        let mut cfg = make_cfg();
        cfg.driver_stages_disk_images = true;
        let v = build_nomad_job_json_with(
            "zsbx-stage-on",
            &cfg,
            7,
            Path::new("/var/zeroship/ch/abc/workspace.img"),
            Path::new("/var/zeroship/ch/users/alice/home.img"),
            "deadbeef",
            "usr_alice",
            "proj1",
            "abcdef0123456789abcdef0123456789",
            None, // cold-boot: no restore_from
            None,
        );
        let task = &v["Job"]["TaskGroups"][0]["Tasks"][0];
        assert_eq!(
            task["Config"]["stage_disk_images"], true,
            "ChPlugin Config must carry stage_disk_images=true when \
             SandboxConfig.driver_stages_disk_images=true on cold-boot \
             (Option C Phase 2: driver materializes images in StartTask)",
        );
        assert_eq!(
            v["Job"]["Meta"]["zsbx_stage_disks"], "true",
            "Job-level Meta must advertise zsbx_stage_disks=true so \
             operators see staging-locality at-a-glance via \
             `nomad job inspect`",
        );
    }

    /// Cold-boot under `driver_stages_disk_images=false` (Phase 2
    /// default): both wire surfaces emit `false` so the driver's
    /// StartTask retains its existing back-compat behaviour
    /// (consumes pre-staged paths via preflightDiskPaths).
    #[test]
    fn cold_boot_jobspec_omits_stage_disks_meta_when_flag_unset() {
        let cfg = make_cfg(); // flag defaults to false in fixture
        assert!(
            !cfg.driver_stages_disk_images,
            "fixture sanity: flag MUST default to false (Phase 2 \
             back-compat default; Phase 4 flips after stress validation)",
        );
        let v = build_nomad_job_json_with(
            "zsbx-stage-off",
            &cfg,
            7,
            Path::new("/var/zeroship/ch/abc/workspace.img"),
            Path::new("/var/zeroship/ch/users/alice/home.img"),
            "deadbeef",
            "usr_alice",
            "proj1",
            "abcdef0123456789abcdef0123456789",
            None,
            None,
        );
        let task = &v["Job"]["TaskGroups"][0]["Tasks"][0];
        assert_eq!(
            task["Config"]["stage_disk_images"], false,
            "ChPlugin Config must carry stage_disk_images=false when \
             SandboxConfig.driver_stages_disk_images=false (Phase 2 default)",
        );
        assert_eq!(
            v["Job"]["Meta"]["zsbx_stage_disks"], "false",
            "Job-level Meta must advertise zsbx_stage_disks=false so \
             operators can distinguish a missing-flag jobspec from one \
             that opted out explicitly",
        );
    }

    /// Even with the controller-side flag flipped to true, the
    /// COLD-BOOT emitter MUST still fall back to false when the
    /// `restore_from` arg is Some — the restore branch stages
    /// rootfs via its own RootfsSource hardlink/copy and never
    /// re-mkfs's workspace.img / home.img (ADR Phase 2).
    #[test]
    fn cold_boot_jobspec_with_restore_from_overrides_stage_flag_to_false() {
        let mut cfg = make_cfg();
        cfg.driver_stages_disk_images = true;
        let restore_dir = Path::new("/var/zeroship/ch/snap-deadbeef/restore");
        let v = build_nomad_job_json_with(
            "zsbx-stage-restore-collision",
            &cfg,
            7,
            Path::new("/var/zeroship/ch/abc/workspace.img"),
            Path::new("/var/zeroship/ch/users/alice/home.img"),
            "deadbeef",
            "usr_alice",
            "proj1",
            "abcdef0123456789abcdef0123456789",
            Some(restore_dir),
            None,
        );
        let task = &v["Job"]["TaskGroups"][0]["Tasks"][0];
        assert_eq!(
            task["Config"]["stage_disk_images"], false,
            "ChPlugin Config stage_disk_images MUST be false under \
             restore-with-flag-set (the restore branch stages rootfs \
             via its own RootfsSource path and does not re-mkfs the \
             ext4 images — ADR Phase 2 cold-boot-only contract)",
        );
        assert_eq!(
            v["Job"]["Meta"]["zsbx_stage_disks"], "false",
            "Job-level Meta MUST mirror the Config field (false on \
             restore-with-flag-set)",
        );
    }

    // ─── r30-A1: global Nomad /shutdown semaphore ──────────────────
    //
    // Three load-bearing tests for the `NomadStopPermits` semaphore
    // (the global cap shared across all 7 production teardown call
    // paths). The architecture review (concurrency r30 CRITICAL #A1)
    // flagged that per-loop caps don't compose against the single
    // downstream (Nomad /shutdown RPC queue + host CH process budget);
    // these tests pin the new global cap's invariants.
    //
    // The tests exercise the semaphore type directly rather than
    // through `stop_inner` to keep the assertions independent of the
    // /shutdown ladder's many other concerns (Nomad mock setup, fence
    // timeouts, host_dir bookkeeping). The integration shape — that
    // stop_inner DOES acquire a permit when the field is installed —
    // is covered by the call-site test `stop_inner_acquires_permit_
    // when_installed` below.

    /// r30-A1: a single acquire claims a permit (in-use bumps,
    /// available drops by one); the guard's Drop releases it (both
    /// counters return to baseline). The fundamental contract of any
    /// semaphore — broken silently if the gauge stops tracking the
    /// underlying flume channel.
    #[compio::test]
    async fn nomad_stop_permits_acquire_release_balance() {
        // Capacity 4 — small enough to assert exact deltas, large
        // enough that one acquire doesn't exhaust the pool (catches a
        // regression where exhaust + recover collapse).
        let permits = NomadStopPermits::new(4);
        assert_eq!(permits.capacity(), 4);
        assert_eq!(permits.permits_available(), 4);
        // Capture pre-test in-use baseline. Process-global gauge: other
        // tests may have left it at any value; we assert deltas, not
        // absolutes.
        let pre_in_use = crate::metrics::nomad_stop_permits_in_use_value();

        // Scope the guard so Drop fires at the closing brace.
        {
            let _g = permits.acquire().await;
            assert_eq!(
                permits.permits_available(),
                3,
                "after one acquire, exactly one permit must be in flight"
            );
            assert_eq!(
                crate::metrics::nomad_stop_permits_in_use_value(),
                pre_in_use + 1,
                "in-use gauge MUST bump by 1 on acquire"
            );
        }
        // Drop ran.
        assert_eq!(
            permits.permits_available(),
            4,
            "after guard drop, all permits must be back in the pool"
        );
        assert_eq!(
            crate::metrics::nomad_stop_permits_in_use_value(),
            pre_in_use,
            "in-use gauge MUST decrement by 1 on guard drop"
        );
    }

    /// r30-A1 / **load-bearing**: with capacity N, at most N concurrent
    /// `acquire().await` calls resolve; the N+1th MUST block until a
    /// permit is released. This is the entire reason the global cap
    /// exists — if this assertion regresses, the per-loop caps can
    /// re-compound on the downstream and the architecture review's
    /// CRITICAL #A1 reopens.
    ///
    /// Test shape: spawn N+1 acquire futures; poll them with a tight
    /// `compio::time::sleep` ceiling; assert exactly N have resolved
    /// before any guard drops. Then drop one guard and assert the
    /// (N+1)th unblocks within the same ceiling.
    #[compio::test]
    async fn nomad_stop_permits_cap_enforced_n_plus_one_blocks() {
        const N: usize = 3;
        let permits = NomadStopPermits::new(N);

        // Acquire N permits inline — must all complete immediately
        // (no pending await).
        let g0 = permits.acquire().await;
        let g1 = permits.acquire().await;
        let g2 = permits.acquire().await;
        assert_eq!(
            permits.permits_available(),
            0,
            "after N acquires, pool MUST be empty"
        );

        // The (N+1)th acquire MUST block. Race it against a sleep
        // ceiling; if the acquire wins, the cap leaked.
        //
        // Use `futures::pin_mut!` + `select!` so we don't allocate a
        // task (the workspace ban on tokio means we'd otherwise need
        // `compio::runtime::spawn` and join). `futures` is already in
        // the workspace dep graph.
        use futures::future::FutureExt;
        let acquire_fut = permits.acquire().fuse();
        let timeout_fut =
            compio::time::sleep(std::time::Duration::from_millis(100)).fuse();
        futures::pin_mut!(acquire_fut, timeout_fut);
        let blocked = futures::select! {
            _ = acquire_fut => false,  // resolved before timeout = cap leaked
            _ = timeout_fut => true,    // timeout fired first = correctly blocking
        };
        assert!(
            blocked,
            "r30-A1 CRITICAL: the (N+1)th acquire on a capacity-{N} semaphore \
             MUST block; resolving immediately means the cap is not enforced \
             and concurrent teardowns can overload Nomad /shutdown."
        );

        // Drop one guard — pool now has 1 permit. The previously-
        // blocked acquire should resolve within the same ceiling.
        drop(g1);
        let acquire_after = permits.acquire();
        let timeout2 =
            compio::time::sleep(std::time::Duration::from_millis(500)).fuse();
        futures::pin_mut!(acquire_after, timeout2);
        let unblocked = futures::select! {
            _g3 = acquire_after.fuse() => true,
            _ = timeout2 => false,
        };
        assert!(
            unblocked,
            "after one guard drop, a fresh acquire MUST resolve within the \
             ceiling (the released permit feeds the next waiter)"
        );
        drop(g0);
        drop(g2);
    }

    /// r30-A1: the `sandbox_nomad_stop_permits_in_use` gauge tracks
    /// guard lifetime 1:1 even under multiple concurrent acquires.
    /// Without this, the metric would diverge from the underlying
    /// semaphore state and the operator-facing saturation view
    /// (`in_use / total`) would silently lie. Pair with the metrics
    /// crate's saturating-underflow test.
    #[compio::test]
    async fn nomad_stop_permits_in_use_gauge_decrements_on_release() {
        let permits = NomadStopPermits::new(2);
        let pre = crate::metrics::nomad_stop_permits_in_use_value();

        let g_a = permits.acquire().await;
        let g_b = permits.acquire().await;
        assert_eq!(
            crate::metrics::nomad_stop_permits_in_use_value(),
            pre + 2,
            "two concurrent acquires MUST raise the in-use gauge by 2"
        );

        drop(g_a);
        assert_eq!(
            crate::metrics::nomad_stop_permits_in_use_value(),
            pre + 1,
            "dropping one guard MUST decrement the gauge by exactly 1 \
             (Drop on NomadStopPermitGuard calls dec_nomad_stop_permits_in_use)"
        );

        drop(g_b);
        assert_eq!(
            crate::metrics::nomad_stop_permits_in_use_value(),
            pre,
            "dropping the second guard MUST return the gauge to its baseline"
        );
    }

    /// r30-A1 integration: when the semaphore is installed on a
    /// `NomadCHBackend`, `stop_inner` actually acquires it. Asserted
    /// indirectly: capacity=1; install on the backend; race two
    /// `stop` calls; verify the in-use gauge held a non-zero value
    /// during the race (the second `stop` waits behind the first).
    ///
    /// We use the same 404-mock Nomad agent the b15 tests use so the
    /// `/shutdown` + Nomad-purge + wait-for-job-gone chain completes
    /// fast (404 = "job is gone" for purge, and the agent_url stop
    /// is best-effort). With `host_fence_timeout_secs=0` the fence
    /// is bypassed too, so stop_inner runs in single-digit ms.
    #[compio::test]
    async fn stop_inner_acquires_permit_when_installed() {
        let (port, stop_flag) = spawn_404_mock();
        let nomad_addr = format!("http://127.0.0.1:{port}");

        let mut cfg = make_cfg();
        cfg.nomad_ch.nomad_addr = nomad_addr;
        cfg.nomad_ch.host_fence_timeout_secs = 0; // skip fence loop
        cfg.nomad_ch.vm_index_release_delay_secs = 0; // skip post-fence delay

        let backend = NomadCHBackend::new(cfg, None).expect("new");
        let permits = NomadStopPermits::new(1);
        backend.install_nomad_stop_permits(permits.clone());
        assert!(
            backend.nomad_stop_permits().is_some(),
            "install_nomad_stop_permits MUST be observable via the \
             accessor (the OnceLock::set must have succeeded)"
        );

        // Hand-insert two sandbox records so we have two stop targets.
        let id_a = Uuid::now_v7();
        let id_b = Uuid::now_v7();
        let host_dir_a = fresh_host_dir("permit-a");
        let host_dir_b = fresh_host_dir("permit-b");
        for (id, host_dir) in [(id_a, host_dir_a.clone()), (id_b, host_dir_b.clone())]
        {
            backend.state.write().unwrap().insert(
                id,
                NomadChSandbox {
                    user_id: "usr_r30_a1".into(),
                    job_id: format!("zsbx-r30-a1-{}", id.simple()),
                    vm_index: 42,
                    host_dir,
                    // Unreachable; /shutdown errors are best-effort.
                    agent_url: "http://127.0.0.1:1".into(),
                    signing_key: make_sk(),
                },
            );
        }

        // Pre-acquire the only permit on a separate task to model
        // "another teardown is mid-flight". The first stop will then
        // have to wait for our held permit to drop.
        let held_guard = permits.acquire().await;
        assert_eq!(permits.permits_available(), 0);

        // Race a single stop against a sleep ceiling — must block on
        // the permit (we hold it).
        use futures::future::FutureExt;
        let stop_fut = backend.stop(id_a).fuse();
        let timeout = compio::time::sleep(std::time::Duration::from_millis(100)).fuse();
        futures::pin_mut!(stop_fut, timeout);
        let stop_blocked = futures::select! {
            _ = stop_fut => false,
            _ = timeout => true,
        };
        assert!(
            stop_blocked,
            "r30-A1: backend.stop() MUST block when the global semaphore \
             is exhausted — the permit is held by another teardown caller. \
             Resolving immediately means stop_inner skipped the acquire \
             and the global cap is not load-bearing."
        );

        // Drop the held permit; the in-flight stop should now make
        // progress. Don't assert success (the unreachable agent_url
        // makes /shutdown Err) — assert the function RETURNS within
        // a generous budget, which means the acquire unblocked.
        drop(held_guard);
        let final_timeout =
            compio::time::sleep(std::time::Duration::from_secs(5)).fuse();
        futures::pin_mut!(final_timeout);
        let stop_completed = futures::select! {
            _ = stop_fut => true,
            _ = final_timeout => false,
        };
        assert!(
            stop_completed,
            "after releasing the held permit, the pending stop MUST \
             complete (the released permit feeds the waiter)"
        );

        // Clean up the second sandbox record so the mock + state are
        // tidy; also bumps coverage that stop runs with a free pool.
        let _ = backend.stop(id_b).await;

        // Tidy up the mock thread + tempdirs.
        stop_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = std::fs::remove_dir_all(&host_dir_a);
        let _ = std::fs::remove_dir_all(&host_dir_b);
    }
}
