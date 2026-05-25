//! Pluggable sandbox backend.
//!
//! The service abstracts "what runs the user's code" behind three
//! implementations:
//!
//!   - **`docker`** — local Docker daemon. The controller `docker
//!     run`s a container, bind-mounts a host directory as
//!     `/workspace`, and uses `docker exec` for shell commands.
//!     File operations use the host bind-mount path directly. Best
//!     for single-host dev / on-prem.
//!
//!   - **`k8s`** — Kubernetes Pod with `runtimeClassName:
//!     kvm-sandbox`, running the in-VM `zeroship-sandbox-agent` as
//!     PID 1. The controller mints a fresh Ed25519 keypair per
//!     session, ships the public half via a `ConfigMap` (read-only
//!     mount, no Secret), and drives the Pod over HTTP using
//!     Ed25519-signed requests. The signing key never leaves this
//!     process. Best for fleet deployments where each sandbox needs
//!     its own kernel.
//!
//!   - **`nomad-ch`** — Nomad job per sandbox using the `ch` Go
//!     plugin driver that launches a `cloud-hypervisor` microVM with
//!     three virtio-blk disks (rootfs + per-sandbox workspace.img +
//!     per-user home.img) and the controller's signing pubkey hex on
//!     the kernel cmdline. The same in-VM `zeroship-sandbox-agent`
//!     runs as PID 1 (signed-request contract identical to `k8s`).
//!     No Kubernetes — no kubelet, no CNI, no CSI — just `nomad agent`
//!     + the Go ch driver. Best for single-node / small-cluster
//!     operators who already run Nomad.
//!
//! ## Why an enum, not a `dyn Trait`
//!
//! Three backends, all static-known at compile time, all used from
//! handler hot paths. Enum dispatch is cheaper, simpler, and lets
//! us avoid `async-trait`'s heap-boxed futures. Adding a fourth
//! backend (e.g. raw Firecracker, gVisor on a serverless host) is
//! a couple of variants and match arms.
//!
//! ## What the trait surface promises
//!
//! - Backend `create` is responsible for both spawning the runtime
//!   and recording per-session state internally. The handler hands
//!   it a freshly-minted `session_id` and a `project_id`; what
//!   comes back is `SandboxInfo` for the registry.
//! - Every other op is keyed by `session_id`. The backend looks up
//!   its own internal state. Looking-up-by-id is the only contract;
//!   how the backend stores it is private.
//! - Every method is async; long-running CLI shell-outs run on
//!   `compio::runtime::spawn_blocking` so the ntex worker stays
//!   responsive.

use std::sync::Arc;

use ed25519_dalek::SigningKey;
use serde::Serialize;
use uuid::Uuid;

use crate::config::SandboxConfig;

pub mod docker;
pub mod k8s;
pub mod nomad_ch;

/// Authentication material the controller uses to drive the in-VM
/// agent. Lifted out of each backend's per-sandbox record so handlers
/// (preview proxy, future signed-RPC dispatch) can call into the
/// agent without knowing which backend hosts the sandbox.
///
/// **What lives here, and why:**
/// - `signing_key`: the controller-side per-sandbox Ed25519 SK. The
///   agent inside the VM holds only the matching verifying key, mounted
///   read-only at `/run/keys/controller-pubkey`. The SK never leaves
///   this process.
/// - `agent_url`: where to reach the agent. NomadCh derives it from
///   `vm_index` (`http://10.99.<100+idx>.2:7777`); K8s uses Pod-IP or
///   the port-forward loopback; Docker uses the container's bridge IP.
/// - `pubkey_fp`: stable short fingerprint of the verifying key
///   (`sig::pubkey_fingerprint(...)` — first 16 bytes of SHA-256 over
///   the 32-byte pubkey, hex-encoded → 32 ASCII chars). Width is
///   load-bearing: `sandbox.sandboxes.key_fp` has a `CHECK (key_fp ~
///   '^[0-9a-f]{32}$')` constraint. Used by the controller's
///   restart-time `/version` rebind probe (§ II.5 of
///   docs/proposals/sandbox-preview-urls.md) to confirm the agent at
///   `agent_url` is the same agent the controller minted keys for.
///
/// **Debug discipline.** The signing key is private; this struct's
/// hand-rolled `Debug` impl elides it. Do NOT `#[derive(Debug)]`.
#[derive(Clone)]
pub struct SandboxAuth {
    pub signing_key: Arc<SigningKey>,
    pub agent_url: String,
    pub pubkey_fp: String,
}

impl std::fmt::Debug for SandboxAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // signing_key intentionally omitted — see struct doc-comment.
        f.debug_struct("SandboxAuth")
            .field("agent_url", &self.agent_url)
            .field("pubkey_fp", &self.pubkey_fp)
            .finish_non_exhaustive()
    }
}

/// Unified exec result — same shape regardless of backend so handlers
/// don't branch.
#[derive(Debug, Serialize)]
pub struct ExecOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
    /// Whether the wall-clock timeout fired before the process exited.
    /// Docker backend reports best-effort (uses `timeout` inside the
    /// container); k8s backend reads it from the agent's `/exec`.
    #[serde(default)]
    pub timed_out: bool,
}

/// File-tree entry. Path is relative to the workspace root, forward
/// slashes, no leading slash.
#[derive(Debug, Serialize)]
pub struct TreeEntry {
    pub path: String,
    pub kind: &'static str, // "file" | "dir"
    pub size: u64,
}

/// Public-facing sandbox record. Backend-specific bookkeeping
/// (container-name, pod-name, signing keys, etc.) lives inside each
/// backend's private state and never leaks here.
///
/// **Why "sandbox" not "session":** the id is keyed on
/// `(user_id, project_id)`, not on a connection or a time window.
/// Browser refresh, multiple tabs, intermittent reconnects — all
/// hit the same `sandbox_id`. If we ever introduce a separate
/// session-level concept (presence, audit context, pre-warmed
/// pool), it'll layer on top of `sandbox_id`.
#[derive(Debug, Clone, Serialize)]
pub struct SandboxInfo {
    pub sandbox_id: String,
    /// Identifies which **creator** owns this sandbox. Drives
    /// per-user PVC mounting in the K8s backend (a freshly-mounted
    /// PVC at `/home/u` carries this user's package caches across
    /// every sandbox they open) and "one active sandbox per user"
    /// scheduling. Constrained to `[a-z0-9-_]{1,64}`.
    pub user_id: String,
    pub project_id: String,
    /// `"docker"`, `"k8s"`, or `"nomad-ch"` — for debug / list
    /// output, NOT for dispatch (handlers always call through the
    /// Backend enum).
    pub backend: String,
    /// Human-readable hint identifying the backing runtime. Format
    /// depends on the backend (`container=zsbx-...` for Docker,
    /// `pod=agent-...` for K8s, `job=zsbx-... vm_index=N key_fp=...`
    /// for Nomad-CH). Opaque to clients.
    pub backend_hint: String,
    pub created_at_secs: u64,
    pub last_used_at_secs: u64,
}

/// The Backend enum. Static dispatch at the call site.
///
/// **B19 fix (2026-05-23)**: the `NomadCh` variant wraps its inner
/// backend in `Arc` so the snapshot/wake-path's `RealRestoreBackend`
/// can hold a shared handle and call `register_restored` after
/// `wait_for_livez` succeeds. Without that shared handle, the
/// restored VM never landed in `NomadCHBackend::state`, and every
/// post-wake `exec`/`stop`/`delete` hit the "sandbox not found"
/// branch + leaked the vm_index slot. Wrapping is asymmetric
/// (Docker/K8s stay by-value) because only nomad-ch hits the
/// restore path in v1.
#[derive(Debug)]
pub enum Backend {
    Docker(docker::DockerBackend),
    K8s(k8s::K8sBackend),
    NomadCh(std::sync::Arc<nomad_ch::NomadCHBackend>),
}

/// Builder for [`Backend`]. Returned by [`Backend::builder`].
///
/// Setters take `T` (not `Option<T>`); callers only invoke
/// `.with_persist(p)` / `.with_local_nomad_node_id(id)` when they have
/// a value. Missing optional fields fall through as `None` to
/// [`BackendBuilder::build`].
///
/// `local_nomad_node_id` is meaningful only for the `nomad-ch`
/// backend (installed on the inner `NomadCHBackend` for r3-A node-pin
/// constraints); other backends ignore it.
///
/// **Why a builder, not a flat struct of `Option` fields**: tests +
/// lifecycle examples that don't exercise persistence / node-pin
/// stay one-liners (`Backend::builder(&cfg).build()?`); orthogonal
/// extension fields (r27-A1 staging-locality, future VFIO-handoff or
/// tap-leak edges) absorb as new `.with_*()` setters without
/// reshaping any existing call site. Replaces R26-I2 / R27-I1's
/// telescoping `from_config*` cascade.
#[must_use]
#[allow(missing_debug_implementations)] // Persistence has no Debug; see sweep.rs convention
pub struct BackendBuilder<'a> {
    cfg: &'a SandboxConfig,
    persist: Option<std::sync::Arc<crate::persist::Persistence>>,
    local_nomad_node_id: Option<String>,
}

impl<'a> BackendBuilder<'a> {
    /// Attach a shared sealed-record persistence handle. The same
    /// handle is cloned (`Arc::clone`) into all three backend variants
    /// so the file I/O state (sealed-records dir + AEAD key) lives in
    /// one place. Omit to disable seal-on-create + delete-on-stop
    /// entirely (Phase 0 default off behaviour).
    pub fn with_persist(
        mut self,
        persist: std::sync::Arc<crate::persist::Persistence>,
    ) -> Self {
        self.persist = Some(persist);
        self
    }

    /// r3-A (T-8b-stress-r3 fix): pin sandbox creates to this Nomad
    /// node_id. Installed on the inner `NomadCHBackend` when the
    /// backend variant is `nomad-ch`. Other backends (`docker`, `k8s`)
    /// silently ignore the value — the constraint is meaningful only
    /// for the Nomad-driven path.
    pub fn with_local_nomad_node_id(mut self, node_id: String) -> Self {
        self.local_nomad_node_id = Some(node_id);
        self
    }

    /// Materialize the [`Backend`] enum variant selected by
    /// `cfg.backend`. Fails fast on an unknown backend string.
    pub fn build(self) -> Result<Backend, String> {
        let Self {
            cfg,
            persist,
            local_nomad_node_id,
        } = self;
        match cfg.backend.as_str() {
            "docker" => Ok(Backend::Docker(docker::DockerBackend::new(
                cfg.clone(),
                persist,
            ))),
            "k8s" => Ok(Backend::K8s(k8s::K8sBackend::new(
                cfg.clone(),
                persist,
            )?)),
            "nomad-ch" => Ok(Backend::NomadCh(std::sync::Arc::new(
                nomad_ch::NomadCHBackend::new(cfg.clone(), persist)?
                    .with_local_nomad_node_id(local_nomad_node_id),
            ))),
            other => Err(format!(
                "unknown SANDBOX_BACKEND={other:?}; expected \"docker\", \"k8s\", or \"nomad-ch\""
            )),
        }
    }
}

impl Backend {
    /// Entry point to construct a [`Backend`]. Optional fields default
    /// to `None`; opt in by chaining `.with_*()` setters before
    /// `.build()`. See [`BackendBuilder`] for the available setters.
    ///
    /// Replaces the prior 3-level telescoping constructor cascade
    /// (R27-I1) — see git history (commit landing R27-I1) for the
    /// pre-builder shape.
    ///
    /// Typical call sites:
    /// ```ignore
    /// // tests / lifecycle examples (no persistence, no node-pin)
    /// let backend = Backend::builder(&cfg).build()?;
    ///
    /// // boot path (`crate::AppState::from_config`)
    /// let backend = Backend::builder(&cfg)
    ///     .with_persist(persist)
    ///     .with_local_nomad_node_id(node_id)
    ///     .build()?;
    /// ```
    pub fn builder(cfg: &SandboxConfig) -> BackendBuilder<'_> {
        BackendBuilder {
            cfg,
            persist: None,
            local_nomad_node_id: None,
        }
    }

    /// Backend label, for debug / list output.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Docker(_) => "docker",
            Self::K8s(_) => "k8s",
            Self::NomadCh(_) => "nomad-ch",
        }
    }

    /// One-shot health probe at startup. Fails fast if the backend
    /// is misconfigured (Docker daemon down, kubectl missing, etc).
    pub async fn probe(&self) -> Result<(), String> {
        match self {
            Self::Docker(b) => b.probe().await,
            Self::K8s(b) => b.probe().await,
            Self::NomadCh(b) => b.probe().await,
        }
    }

    /// Whether the backend is currently healthy. Updated by
    /// [`probe`] (synchronous one-shot) and by an optional
    /// background loop. Read by `/readyz` and any handler that
    /// wants to fast-fail rather than queue against a dead backend.
    pub fn is_healthy(&self) -> bool {
        match self {
            // Docker has no separate health flag; assume healthy
            // once the initial probe succeeded (no background
            // monitor today).
            Self::Docker(_) => true,
            Self::K8s(b) => b.is_healthy(),
            Self::NomadCh(b) => b.is_healthy(),
        }
    }

    /// Best-effort cleanup of runtime objects (Pods, Nomad jobs)
    /// that the controller no longer holds in-memory state for.
    /// Docker containers stop when the daemon restarts; the K8s
    /// and Nomad-CH backends optionally prune by label / job
    /// prefix at startup. Called once from [`crate::AppState::from_config`].
    pub async fn cleanup_orphans_at_startup(&self) -> Result<usize, String> {
        match self {
            Self::Docker(_) => Ok(0),
            Self::K8s(b) => b.cleanup_orphans_at_startup().await,
            Self::NomadCh(b) => b.cleanup_orphans_at_startup().await,
        }
    }

    pub async fn create(
        &self,
        sandbox_id: Uuid,
        user_id: &str,
        project_id: &str,
    ) -> Result<SandboxInfo, String> {
        match self {
            Self::Docker(b) => b.create(sandbox_id, user_id, project_id).await,
            Self::K8s(b) => b.create(sandbox_id, user_id, project_id).await,
            Self::NomadCh(b) => b.create(sandbox_id, user_id, project_id).await,
        }
    }

    pub async fn stop(&self, session_id: Uuid) -> Result<(), String> {
        match self {
            Self::Docker(b) => b.stop(session_id).await,
            Self::K8s(b) => b.stop(session_id).await,
            Self::NomadCh(b) => b.stop(session_id).await,
        }
    }

    pub async fn exec(
        &self,
        sandbox_id: Uuid,
        cmd: &str,
        cwd: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> Result<ExecOutput, String> {
        match self {
            Self::Docker(b) => b.exec(sandbox_id, cmd, cwd, timeout_ms).await,
            Self::K8s(b) => b.exec(sandbox_id, cmd, cwd, timeout_ms).await,
            Self::NomadCh(b) => b.exec(sandbox_id, cmd, cwd, timeout_ms).await,
        }
    }

    pub async fn read_file(&self, sandbox_id: Uuid, path: &str) -> Result<Vec<u8>, String> {
        match self {
            Self::Docker(b) => b.read_file(sandbox_id, path).await,
            Self::K8s(b) => b.read_file(sandbox_id, path).await,
            Self::NomadCh(b) => b.read_file(sandbox_id, path).await,
        }
    }

    pub async fn write_file(
        &self,
        sandbox_id: Uuid,
        path: &str,
        body: &[u8],
    ) -> Result<(), String> {
        match self {
            Self::Docker(b) => b.write_file(sandbox_id, path, body).await,
            Self::K8s(b) => b.write_file(sandbox_id, path, body).await,
            Self::NomadCh(b) => b.write_file(sandbox_id, path, body).await,
        }
    }

    pub async fn delete_file(&self, sandbox_id: Uuid, path: &str) -> Result<bool, String> {
        match self {
            Self::Docker(b) => b.delete_file(sandbox_id, path).await,
            Self::K8s(b) => b.delete_file(sandbox_id, path).await,
            Self::NomadCh(b) => b.delete_file(sandbox_id, path).await,
        }
    }

    pub async fn file_tree(&self, session_id: Uuid) -> Result<Vec<TreeEntry>, String> {
        match self {
            Self::Docker(b) => b.file_tree(session_id).await,
            Self::K8s(b) => b.file_tree(session_id).await,
            Self::NomadCh(b) => b.file_tree(session_id).await,
        }
    }

    /// Lift the per-sandbox authentication material out of the backend
    /// so it can be used uniformly by the preview proxy and the
    /// sealed-record persistence layer.
    ///
    /// Returns `Err` for sandboxes the backend doesn't know about
    /// (e.g. a stale sandbox-id from a controller-restart race).
    /// Returns `Err` from the Docker backend if its agent-launch
    /// path was disabled (ed25519 keys are not minted).
    ///
    /// **Cheap to call.** Each backend stores `signing_key` as
    /// `Arc<SigningKey>` and clones the Arc, not the secret bytes.
    pub async fn session_auth(&self, sandbox_id: Uuid) -> Result<SandboxAuth, String> {
        match self {
            Self::Docker(b) => b.session_auth(sandbox_id).await,
            Self::K8s(b) => b.session_auth(sandbox_id).await,
            Self::NomadCh(b) => b.session_auth(sandbox_id).await,
        }
    }

    /// Re-install per-sandbox state from a sealed record. Called
    /// from the controller's restart-restore path
    /// (`crate::AppState::from_config`) after the boot loop has
    /// signed-`/version` probed the agent and confirmed the
    /// fingerprint.
    ///
    /// Phase-0 status: only nomad-ch implements full backend-state
    /// rehydration (the structural model + the integration-test
    /// target per the design's Phase-0 plan). Docker and K8s return
    /// `Err` until their re-derive paths land — `agent_url` is not
    /// deterministic for those backends (Docker: bridge IP requires
    /// `docker inspect`; K8s: requires `kubectl get pod -o jsonpath`)
    /// and Phase 0 doesn't ship that re-derive code yet. Tracked as
    /// a Phase-1 follow-up.
    pub async fn restore_from_sealed(
        &self,
        sandbox_id: Uuid,
        sealed: &crate::persist::SealedAuth,
    ) -> Result<SandboxAuth, String> {
        match self {
            Self::NomadCh(b) => b.restore_from_sealed(sandbox_id, sealed).await,
            Self::Docker(_) | Self::K8s(_) => Err(format!(
                "restore_from_sealed: backend {:?} doesn't yet support \
                 restart-restore (Phase-0 nomad-ch-only; tracked as a \
                 Phase-1 follow-up)",
                self.name()
            )),
        }
    }

    /// Phase B snapshot wiring: resolve `(api_socket, vm_index, alloc_dir)`
    /// for a running sandbox so the snapshot handler can `ch-remote
    /// pause` + `ch-remote snapshot`.
    ///
    /// Only `nomad-ch` supports this in v1 — k8s/docker would each
    /// need a different "where is the VM running?" lookup (kubelet
    /// alloc lookup; container PID lookup) which Phase B does not
    /// implement. The other variants return `Err` so admin handlers
    /// can map to a 501 / 503 envelope.
    pub async fn lookup_source_vm_ops(
        &self,
        sandbox_id: Uuid,
    ) -> Result<nomad_ch::SourceVmOpsHandle, String> {
        match self {
            Self::NomadCh(b) => b.lookup_source_vm_ops(sandbox_id).await,
            Self::Docker(_) | Self::K8s(_) => Err(format!(
                "lookup_source_vm_ops: backend {:?} doesn't support \
                 snapshot/restore (Phase B nomad-ch-only)",
                self.name()
            )),
        }
    }

    /// Phase B snapshot wiring: tear down the source VM after a
    /// snapshot lands in pg. Mirrors [`Self::stop`] but the in-memory
    /// state has already been removed by `lookup_source_vm_ops`'s
    /// caller path. Idempotent + best-effort: any error is logged but
    /// not propagated (the snapshot artifact is already authoritative).
    ///
    /// Calls [`nomad_ch::NomadCHBackend::stop_preserving_state`] —
    /// runs the Nomad-job purge + host-fence + vm_index release +
    /// in-memory map removal, but **DOES NOT remove the per-sandbox
    /// `host_dir`**. That dir holds `workspace.img`, which the next
    /// wake re-attaches as a virtio-blk disk via the ch driver's
    /// `TaskConfig.Disks` field; deleting it here causes the next
    /// wake's `TaskConfig.Disks` path to be missing and CH to
    /// refuse to start (bug #15 — see
    /// `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-23-r1.md`).
    /// The host_dir is finally reaped by the next real [`Self::stop`]
    /// call (operator delete, or terminal-not-restorable transition).
    /// Symmetric with how `home.img` is intentionally preserved
    /// across snapshot lifetimes.
    pub async fn teardown_source_for_snapshot(
        &self,
        sandbox_id: Uuid,
    ) -> Result<(), String> {
        match self {
            Self::NomadCh(b) => b.stop_preserving_state(sandbox_id).await,
            Self::Docker(_) | Self::K8s(_) => Err(format!(
                "teardown_source_for_snapshot: backend {:?} doesn't support \
                 snapshot/restore (Phase B nomad-ch-only)",
                self.name()
            )),
        }
    }

    /// Shared handle to the per-worker `vm_index` allocator, when the
    /// backend has one. **B18 fix wiring**: `crate::restore_handler::
    /// RealRestoreBackend::with_shared_allocator` consumes the result
    /// so create-side `alloc()` and restore-side `reserve()` share
    /// state — otherwise a restored VM holding slot N (private map)
    /// stays invisible to the create-side allocator, which hands the
    /// same tap/IP to a fresh sandbox → stale-pubkey 401 on /version.
    /// Returns `None` for backends that don't have a slot pool
    /// (Docker, K8s — they map sandboxes to ephemeral container/Pod
    /// IPs assigned by the host runtime).
    ///
    /// `pub(crate)` because the returned `Arc<Mutex<>>` is a footgun on
    /// the public surface — downstream code could `.lock()` it and
    /// deadlock create/restore. The only legitimate caller is
    /// `crate::restore_handler::RealRestoreBackend::with_shared_allocator`
    /// inside `crate::AppState::from_config`. Sandbox v1 has no
    /// out-of-crate consumer.
    pub(crate) fn vm_index_allocator(
        &self,
    ) -> Option<std::sync::Arc<std::sync::Mutex<nomad_ch::VmIndexAllocator>>> {
        match self {
            Self::NomadCh(b) => Some(b.vm_index_allocator()),
            Self::Docker(_) | Self::K8s(_) => None,
        }
    }

    /// Shared `Arc<NomadCHBackend>` handle when the backend is
    /// nomad-ch. **B19 fix wiring**: `crate::restore_handler::
    /// RealRestoreBackend::with_nomad_handle` consumes the result so
    /// the wake path can call `register_restored(...)` after
    /// `wait_for_livez` Ok — without that, the restored VM never
    /// landed in `NomadCHBackend::state` and post-wake exec/stop/
    /// delete all returned "sandbox not found" + leaked the slot.
    /// Returns `None` for Docker/K8s (those backends don't expose
    /// a restore registry surface today).
    ///
    /// `pub(crate)` because handing out a concrete
    /// `Arc<NomadCHBackend>` bypasses the "enum dispatch is the only
    /// contract" promise — out-of-crate callers could reach past the
    /// `Backend` enum and call backend-specific methods directly,
    /// stranding the trait surface. The only legitimate caller is
    /// `crate::restore_handler::RealRestoreBackend::with_nomad_handle`
    /// inside `crate::AppState::from_config`.
    pub(crate) fn nomad_ch_handle(
        &self,
    ) -> Option<std::sync::Arc<nomad_ch::NomadCHBackend>> {
        match self {
            Self::NomadCh(b) => Some(std::sync::Arc::clone(b)),
            Self::Docker(_) | Self::K8s(_) => None,
        }
    }

    /// B19 fix: install a restored sandbox into the backend's in-memory
    /// state map so post-wake `exec`/`stop`/`delete` find it. Called by
    /// `restore_handler::do_restore_inner` after `wait_for_livez` Ok.
    ///
    /// Only `nomad-ch` implements it (the restore path is nomad-ch-only
    /// in v1; Docker/K8s lack a deterministic agent_url + a slot pool,
    /// so they return Err). Inputs are everything `NomadCHBackend::
    /// create`'s state-map insert needs: the sandbox id, the source
    /// vm_index, the per-sandbox signing key (already unsealed), the
    /// agent URL (already derived), and the user id (from the snapshot
    /// row).
    ///
    /// `pub(crate)` because the signature takes raw `[u8; 32]` SK bytes
    /// — a key-material footgun that does not belong on the public
    /// surface. The only legitimate caller is
    /// `crate::restore_handler::RealRestoreBackend::register_restored`
    /// (via the trait), which itself routes through the
    /// `nomad_ch_handle()`-borrowed `Arc<NomadCHBackend>`. Out-of-crate
    /// code must drive restore through `crate::AppState`, not by
    /// hand-rolling state-map inserts.
    ///
    /// `#[allow(dead_code)]`: the trait-dispatch path in
    /// `restore_handler::RealRestoreBackend::register_restored` calls
    /// `NomadCHBackend::register_restored` directly off the
    /// `nomad_ch_handle()`-borrowed `Arc`, bypassing this enum-level
    /// delegator. Kept for symmetry with the other `Backend::*` variants
    /// and for the eventual Docker/K8s restore impls — when those land,
    /// restore_handler can switch back to the enum surface and drop this
    /// allow.
    #[allow(dead_code)]
    pub(crate) fn register_restored(
        &self,
        sandbox_id: Uuid,
        vm_index: u16,
        signing_key_bytes: [u8; 32],
        agent_url: String,
        user_id: String,
    ) -> Result<(), String> {
        match self {
            Self::NomadCh(b) => {
                b.register_restored(sandbox_id, vm_index, signing_key_bytes, agent_url, user_id)
            }
            Self::Docker(_) | Self::K8s(_) => Err(format!(
                "register_restored: backend {:?} doesn't support \
                 snapshot/restore (Phase B nomad-ch-only)",
                self.name()
            )),
        }
    }

    /// Round-8 Phase-1 restore. Pg row is canonical for non-secret
    /// fields; sealed record is canonical for the signing key. The
    /// boot loop has already probed the agent before this is called.
    pub async fn restore_from_pg_and_sealed(
        &self,
        sandbox_id: Uuid,
        row: &crate::db::SandboxRow,
        sealed: &crate::persist::SealedAuth,
        agent_url: String,
    ) -> Result<SandboxAuth, String> {
        match self {
            Self::NomadCh(b) => {
                b.restore_from_pg_and_sealed(sandbox_id, row, sealed, agent_url).await
            }
            Self::Docker(_) | Self::K8s(_) => Err(format!(
                "restore_from_pg_and_sealed: backend {:?} doesn't yet support \
                 restart-restore (Phase-1 nomad-ch-only; Docker/K8s pending \
                 deterministic agent_url re-derivation)",
                self.name()
            )),
        }
    }

    /// Seal an updated `SealedAuth` for `sandbox_id` that carries the
    /// caller-provided preview-share state (`preview_secrets`,
    /// `preview_audit`). Used by the share-token mint / rotate
    /// handlers to persist newly-minted secrets + audit rows so a
    /// controller crash mid-flight doesn't lose them.
    ///
    /// `info` is the registry's [`SandboxInfo`] for the sandbox —
    /// supplies `user_id` / `project_id` / `created_at_secs` since
    /// not every backend stores those internally (Docker doesn't).
    ///
    /// Returns:
    /// - `Ok(true)` — record sealed.
    /// - `Ok(false)` — persistence disabled (`SANDBOX_PERSIST_AUTH != 1`)
    ///   OR sandbox unknown to the backend; nothing written. Caller
    ///   treats both as best-effort no-ops.
    /// - `Err(e)` — backend was supposed to seal but I/O / encryption
    ///   failed. Caller logs at WARN and proceeds (Phase-3 mint MUST
    ///   NOT fail an API call on seal failure — the record's
    ///   in-memory state is still authoritative for live traffic).
    ///
    /// **Concurrency.** Each backend's session map is read under its
    /// own lock; the sealed record is rewritten in full (no partial
    /// updates). Two concurrent share-mints on the same sandbox each
    /// re-seal the full state; last-writer-wins on disk and matches
    /// the in-memory ring's last-writer-wins.
    pub async fn seal_with_preview_state(
        &self,
        sandbox_id: Uuid,
        info: &SandboxInfo,
        secrets: Option<crate::persist::SealedPreviewSecrets>,
        audit: Vec<crate::persist::SealedAuditEntry>,
    ) -> Result<bool, String> {
        match self {
            Self::Docker(b) => {
                b.seal_with_preview_state(sandbox_id, info, secrets, audit).await
            }
            Self::K8s(b) => {
                b.seal_with_preview_state(sandbox_id, info, secrets, audit).await
            }
            Self::NomadCh(b) => {
                b.seal_with_preview_state(sandbox_id, info, secrets, audit).await
            }
        }
    }
}
