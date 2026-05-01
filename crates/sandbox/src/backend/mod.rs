//! Pluggable sandbox backend.
//!
//! The service abstracts "what runs the user's code" behind two
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
//! ## Why an enum, not a `dyn Trait`
//!
//! Two backends, both static-known at compile time, both used from
//! handler hot paths. Enum dispatch is cheaper, simpler, and lets
//! us avoid `async-trait`'s heap-boxed futures. Adding a third
//! backend (e.g. Firecracker, gVisor) is a couple of variants and
//! match arms.
//!
//! ## What the trait surface promises
//!
//! - Backend `create` is responsible for both spawning the runtime
//!   and recording per-session state internally. The handler hands
//!   it a freshly-minted `session_id` and a `project_id`; what
//!   comes back is `SessionInfo` for the registry.
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

/// Public-facing session record. Backend-specific bookkeeping
/// (container-name, pod-name, signing keys, etc.) lives inside each
/// backend's private state and never leaks here.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub project_id: String,
    /// `"docker"` or `"k8s"` — for debug / list output, NOT for
    /// dispatch (handlers always call through the Backend enum).
    pub backend: String,
    /// Human-readable hint identifying the backing runtime. Format
    /// depends on the backend (`container=zsbx-...` for Docker,
    /// `pod=agent-...` for K8s). Opaque to clients.
    pub backend_hint: String,
    pub created_at_secs: u64,
    pub last_used_at_secs: u64,
}

/// The Backend enum. Static dispatch at the call site.
#[derive(Debug)]
pub enum Backend {
    Docker(docker::DockerBackend),
    K8s(k8s::K8sBackend),
}

impl Backend {
    pub fn from_config(cfg: &SandboxConfig) -> Result<Self, String> {
        match cfg.backend.as_str() {
            "docker" => Ok(Self::Docker(docker::DockerBackend::new(cfg.clone()))),
            "k8s" => Ok(Self::K8s(k8s::K8sBackend::new(cfg.clone())?)),
            other => Err(format!(
                "unknown SANDBOX_BACKEND={other:?}; expected \"docker\" or \"k8s\""
            )),
        }
    }

    /// Backend label, for debug / list output.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Docker(_) => "docker",
            Self::K8s(_) => "k8s",
        }
    }

    /// One-shot health probe at startup. Fails fast if the backend
    /// is misconfigured (Docker daemon down, kubectl missing, etc).
    pub async fn probe(&self) -> Result<(), String> {
        match self {
            Self::Docker(b) => b.probe().await,
            Self::K8s(b) => b.probe().await,
        }
    }

    pub async fn create(
        &self,
        session_id: Uuid,
        project_id: &str,
    ) -> Result<SessionInfo, String> {
        match self {
            Self::Docker(b) => b.create(session_id, project_id).await,
            Self::K8s(b) => b.create(session_id, project_id).await,
        }
    }

    pub async fn stop(&self, session_id: Uuid) -> Result<(), String> {
        match self {
            Self::Docker(b) => b.stop(session_id).await,
            Self::K8s(b) => b.stop(session_id).await,
        }
    }

    pub async fn exec(
        &self,
        session_id: Uuid,
        cmd: &str,
        cwd: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> Result<ExecOutput, String> {
        match self {
            Self::Docker(b) => b.exec(session_id, cmd, cwd, timeout_ms).await,
            Self::K8s(b) => b.exec(session_id, cmd, cwd, timeout_ms).await,
        }
    }

    pub async fn read_file(&self, session_id: Uuid, path: &str) -> Result<Vec<u8>, String> {
        match self {
            Self::Docker(b) => b.read_file(session_id, path).await,
            Self::K8s(b) => b.read_file(session_id, path).await,
        }
    }

    pub async fn write_file(
        &self,
        session_id: Uuid,
        path: &str,
        body: &[u8],
    ) -> Result<(), String> {
        match self {
            Self::Docker(b) => b.write_file(session_id, path, body).await,
            Self::K8s(b) => b.write_file(session_id, path, body).await,
        }
    }

    pub async fn delete_file(&self, session_id: Uuid, path: &str) -> Result<bool, String> {
        match self {
            Self::Docker(b) => b.delete_file(session_id, path).await,
            Self::K8s(b) => b.delete_file(session_id, path).await,
        }
    }

    pub async fn file_tree(&self, session_id: Uuid) -> Result<Vec<TreeEntry>, String> {
        match self {
            Self::Docker(b) => b.file_tree(session_id).await,
            Self::K8s(b) => b.file_tree(session_id).await,
        }
    }
}
