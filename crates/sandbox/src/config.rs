//! Sandbox process configuration. Read once at startup from env / argv;
//! the resulting struct is cloned into the shared `AppState`.

use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct SandboxConfig {
    /// Port the HTTP API listens on. `SANDBOX_PORT` (default 9091).
    pub port: u16,

    /// Bearer token for the HTTP API. `SANDBOX_TOKEN`. Empty value
    /// disables auth and is **rejected at startup** unless the
    /// operator also set `SANDBOX_ALLOW_NO_AUTH=1` (dev opt-in).
    /// Production deployments without a token simply refuse to
    /// boot, so an unconfigured pod can't accidentally become a
    /// public RCE.
    pub token: String,

    /// Backend selector. `SANDBOX_BACKEND=docker|k8s` (default docker).
    pub backend: String,

    /// Docker image tag spawned for new sessions. `SANDBOX_IMAGE`
    /// (default `zeroship/sandbox-base:latest`).
    /// Used by the **docker** backend.
    pub image: String,

    /// Host directory where per-project workspaces live. Each session's
    /// `/workspace` is bind-mounted from `{workspace_root}/{project_id}/`.
    /// `SANDBOX_WORKSPACE_ROOT` (default `/var/zeroship/projects`).
    /// Used by the **docker** backend only — k8s sessions store their
    /// workspace inside the Pod via emptyDir.
    pub workspace_root: PathBuf,

    /// Docker network the sandbox containers join. `SANDBOX_NETWORK`
    /// (default `zeroship-sandbox-net`). Must be created out-of-band
    /// (`docker network create zeroship-sandbox-net`).
    /// Used by the **docker** backend only.
    pub network: String,

    /// Per-container memory limit in MiB. `SANDBOX_MEMORY_MB` (default
    /// 1024).
    pub memory_mb: u32,

    /// Per-container CPU quota. `SANDBOX_CPUS` (default 2.0).
    pub cpus: f32,

    /// Idle session GC threshold. `SANDBOX_IDLE_TIMEOUT_SECS`
    /// (default 1800 — 30 min).
    pub idle_timeout_secs: u64,

    /// Hard ceiling regardless of activity. `SANDBOX_MAX_LIFETIME_SECS`
    /// (default 28800 — 8h).
    pub max_lifetime_secs: u64,

    /// Pull the image at startup if missing. `SANDBOX_AUTO_PULL`
    /// (default false — admins should pre-pull for predictable boot).
    /// Docker backend only.
    pub auto_pull: bool,

    /// K8s-backend settings. Read from env even when `backend=docker`
    /// (cheap; lets you switch backends without restart-time config
    /// gymnastics).
    pub k8s: K8sConfig,
}

#[derive(Clone, Debug)]
pub struct K8sConfig {
    /// Namespace where Pods are created. `SANDBOX_K8S_NAMESPACE`
    /// (default `default`).
    pub namespace: String,

    /// Agent OCI image, including tag (or pinned digest in prod).
    /// `SANDBOX_K8S_IMAGE` (default `docker.io/zeroship/sandbox-agent:dev`).
    pub image: String,

    /// `runtimeClassName` to apply to the Pod. Must point at the
    /// crun+libkrun handler. `SANDBOX_K8S_RUNTIME_CLASS` (default
    /// `kvm-sandbox`).
    pub runtime_class: String,

    /// `kubectl wait --for=condition=Ready` timeout in seconds.
    /// `SANDBOX_K8S_READY_TIMEOUT_SECS` (default 120).
    pub ready_timeout_secs: u64,

    /// Use `kubectl port-forward` per session instead of dialing the
    /// Pod IP directly. Required when the controller runs outside
    /// the cluster (typical for local dev). In-cluster controllers
    /// should set this to false. `SANDBOX_K8S_USE_PORT_FORWARD`
    /// (default true — safe for local dev; switch off in cluster).
    pub use_port_forward: bool,

    /// Loopback port allocator base when `use_port_forward=true`.
    /// `SANDBOX_K8S_PORT_FORWARD_START` (default 18000). Allocator
    /// is monotonic per process; never reuses ports.
    pub port_forward_start: u16,

    /// PVC size for the per-user `/home/u` mount. Holds package
    /// caches (pnpm, npm, pip, cargo), dotfiles, ssh config —
    /// non-secret, deduplicated state that survives across every
    /// sandbox the user opens. `SANDBOX_K8S_USER_HOME_SIZE`
    /// (default `5Gi`). k8s parses this as a Quantity.
    pub user_home_size: String,

    /// StorageClass used for per-user PVCs. Empty / unset = use
    /// the cluster default (`storageclass.kubernetes.io/is-default-class: "true"`).
    /// In production pick one with snapshot+clone support
    /// (Longhorn, Ceph RBD, EBS gp3, GCE PD) so the per-project
    /// snapshot / fork features in the storage design are
    /// buildable later. `SANDBOX_K8S_USER_HOME_STORAGE_CLASS`
    /// (default empty).
    pub user_home_storage_class: Option<String>,
}

impl SandboxConfig {
    pub fn from_env() -> Result<Self, String> {
        let port = parse_env("SANDBOX_PORT", 9091u16)?;
        let token = std::env::var("SANDBOX_TOKEN").unwrap_or_default();
        // Fail-closed: an unset/empty token disables auth. We refuse
        // to start in that state unless the operator opts in via
        // SANDBOX_ALLOW_NO_AUTH=1. Tighten further: when a token is
        // set, require ≥32 bytes — anything shorter is brute-forceable.
        let allow_no_auth = parse_env("SANDBOX_ALLOW_NO_AUTH", false)?;
        if token.is_empty() && !allow_no_auth {
            return Err(
                "SANDBOX_TOKEN is empty; refusing to start. \
                 Set SANDBOX_TOKEN to a strong (≥32 byte) random value, \
                 or set SANDBOX_ALLOW_NO_AUTH=1 for explicit dev mode."
                    .to_string(),
            );
        }
        if !token.is_empty() && token.len() < 32 {
            return Err(format!(
                "SANDBOX_TOKEN is too short ({} bytes; need ≥ 32). \
                 Generate with: head -c 32 /dev/urandom | base64",
                token.len()
            ));
        }
        let backend = std::env::var("SANDBOX_BACKEND").unwrap_or_else(|_| "docker".to_string());
        if !matches!(backend.as_str(), "docker" | "k8s") {
            return Err(format!(
                "SANDBOX_BACKEND={backend:?}; expected \"docker\" or \"k8s\""
            ));
        }
        let image = std::env::var("SANDBOX_IMAGE")
            .unwrap_or_else(|_| "zeroship/sandbox-base:latest".to_string());
        let workspace_root = PathBuf::from(
            std::env::var("SANDBOX_WORKSPACE_ROOT")
                .unwrap_or_else(|_| "/var/zeroship/projects".to_string()),
        );
        let network = std::env::var("SANDBOX_NETWORK")
            .unwrap_or_else(|_| "zeroship-sandbox-net".to_string());
        let memory_mb = parse_env("SANDBOX_MEMORY_MB", 1024u32)?;
        let cpus = parse_env("SANDBOX_CPUS", 2.0f32)?;
        let idle_timeout_secs = parse_env("SANDBOX_IDLE_TIMEOUT_SECS", 1800u64)?;
        let max_lifetime_secs = parse_env("SANDBOX_MAX_LIFETIME_SECS", 28800u64)?;
        let auto_pull = parse_env("SANDBOX_AUTO_PULL", false)?;

        if cpus <= 0.0 || cpus > 64.0 {
            return Err(format!("SANDBOX_CPUS out of range: {cpus}"));
        }
        if memory_mb < 64 {
            return Err(format!("SANDBOX_MEMORY_MB too small: {memory_mb}"));
        }

        let user_home_storage_class = std::env::var("SANDBOX_K8S_USER_HOME_STORAGE_CLASS")
            .ok()
            .filter(|s| !s.is_empty());
        let k8s = K8sConfig {
            namespace: std::env::var("SANDBOX_K8S_NAMESPACE")
                .unwrap_or_else(|_| "default".to_string()),
            image: std::env::var("SANDBOX_K8S_IMAGE")
                .unwrap_or_else(|_| "docker.io/zeroship/sandbox-agent:dev".to_string()),
            runtime_class: std::env::var("SANDBOX_K8S_RUNTIME_CLASS")
                .unwrap_or_else(|_| "kvm-sandbox".to_string()),
            ready_timeout_secs: parse_env("SANDBOX_K8S_READY_TIMEOUT_SECS", 120u64)?,
            use_port_forward: parse_env("SANDBOX_K8S_USE_PORT_FORWARD", true)?,
            port_forward_start: parse_env("SANDBOX_K8S_PORT_FORWARD_START", 18000u16)?,
            user_home_size: std::env::var("SANDBOX_K8S_USER_HOME_SIZE")
                .unwrap_or_else(|_| "5Gi".to_string()),
            user_home_storage_class,
        };

        Ok(Self {
            port, token, backend, image, workspace_root, network,
            memory_mb, cpus, idle_timeout_secs, max_lifetime_secs, auto_pull,
            k8s,
        })
    }
}

fn parse_env<T: std::str::FromStr>(key: &str, default: T) -> Result<T, String>
where
    T::Err: std::fmt::Display,
{
    match std::env::var(key) {
        Ok(v) => v.parse().map_err(|e| format!("{key}: {e}")),
        Err(_) => Ok(default),
    }
}
