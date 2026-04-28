//! Sandbox process configuration. Read once at startup from env / argv;
//! the resulting struct is cloned into the shared `AppState`.

use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct SandboxConfig {
    /// Port the HTTP API listens on. `SANDBOX_PORT` (default 9091).
    pub port: u16,

    /// Bearer token for the HTTP API. `SANDBOX_TOKEN`. Empty disables auth
    /// (dev only — main.rs prints a warning).
    pub token: String,

    /// Docker image tag spawned for new sessions. `SANDBOX_IMAGE`
    /// (default `zeroship/sandbox-base:latest`).
    pub image: String,

    /// Host directory where per-project workspaces live. Each session's
    /// `/workspace` is bind-mounted from `{workspace_root}/{project_id}/`.
    /// `SANDBOX_WORKSPACE_ROOT` (default `/var/zeroship/projects`).
    pub workspace_root: PathBuf,

    /// Docker network the sandbox containers join. `SANDBOX_NETWORK`
    /// (default `zeroship-sandbox-net`). Must be created out-of-band
    /// (`docker network create zeroship-sandbox-net`).
    pub network: String,

    /// Per-container memory limit in MiB. `SANDBOX_MEMORY_MB` (default
    /// 1024). Passed to `docker run --memory={N}m`.
    pub memory_mb: u32,

    /// Per-container CPU quota. `SANDBOX_CPUS` (default 2.0). Passed
    /// to `docker run --cpus={N}`.
    pub cpus: f32,

    /// Idle session GC threshold. `SANDBOX_IDLE_TIMEOUT_SECS`
    /// (default 1800 — 30 min).
    pub idle_timeout_secs: u64,

    /// Hard ceiling regardless of activity. `SANDBOX_MAX_LIFETIME_SECS`
    /// (default 28800 — 8h).
    pub max_lifetime_secs: u64,

    /// Pull the image at startup if missing. `SANDBOX_AUTO_PULL`
    /// (default false — admins should pre-pull for predictable boot).
    pub auto_pull: bool,
}

impl SandboxConfig {
    pub fn from_env() -> Result<Self, String> {
        let port = parse_env("SANDBOX_PORT", 9091u16)?;
        let token = std::env::var("SANDBOX_TOKEN").unwrap_or_default();
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

        Ok(Self {
            port, token, image, workspace_root, network,
            memory_mb, cpus, idle_timeout_secs, max_lifetime_secs, auto_pull,
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
