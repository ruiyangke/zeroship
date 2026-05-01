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
//!   - **`nomad-ch`** — Nomad `raw_exec` job per sandbox; the job
//!     invokes a wrapper script that spawns 3 × `virtiofsd` plus a
//!     `cloud-hypervisor` microVM. The same in-VM
//!     `zeroship-sandbox-agent` runs as PID 1 (signed-request
//!     contract identical to `k8s`). No Kubernetes — no kubelet,
//!     no CNI, no CSI — just `nomad agent` + a shell wrapper.
//!     Best for single-node / small-cluster operators who already
//!     run Nomad and want libkrun-equivalent isolation without the
//!     k8s control-plane overhead.
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

use serde::Serialize;
use uuid::Uuid;

use crate::config::SandboxConfig;

pub mod docker;
pub mod k8s;
pub mod nomad_ch;

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
#[derive(Debug)]
pub enum Backend {
    Docker(docker::DockerBackend),
    K8s(k8s::K8sBackend),
    NomadCh(nomad_ch::NomadCHBackend),
}

impl Backend {
    pub fn from_config(cfg: &SandboxConfig) -> Result<Self, String> {
        match cfg.backend.as_str() {
            "docker" => Ok(Self::Docker(docker::DockerBackend::new(cfg.clone()))),
            "k8s" => Ok(Self::K8s(k8s::K8sBackend::new(cfg.clone())?)),
            "nomad-ch" => Ok(Self::NomadCh(nomad_ch::NomadCHBackend::new(cfg.clone())?)),
            other => Err(format!(
                "unknown SANDBOX_BACKEND={other:?}; expected \"docker\", \"k8s\", or \"nomad-ch\""
            )),
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
}
