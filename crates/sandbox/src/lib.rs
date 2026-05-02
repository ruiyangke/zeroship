//! `zeroship-sandbox` — pluggable sandbox-session controller.
//!
//! The crate is split between a thin binary (`src/main.rs`) and this
//! library, so integration tests and the lifecycle e2e example can
//! reuse `AppState`, the [`backend::Backend`] enum, and the session
//! registry without re-launching the HTTP server in-process.
//!
//! See [`backend`] for the backend contract and the docker / k8s
//! implementations.

pub mod auth;
pub mod backend;
pub mod config;
pub mod files;
pub mod handlers;
pub mod persist;
pub mod registry;
pub mod restore;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::backend::Backend;
use crate::config::SandboxConfig;
use crate::persist::AeadKey;
use crate::registry::SandboxRegistry;

/// Shared application state passed to every handler.
#[allow(missing_debug_implementations)]
pub struct AppState {
    pub config: SandboxConfig,
    pub sandboxes: SandboxRegistry,
    pub backend: Backend,
}

impl AppState {
    /// Build a fresh state from config. Probes the chosen backend at
    /// construction time so callers fail fast on misconfiguration,
    /// then cleans up any in-cluster runtime objects orphaned by a
    /// previous controller process (the per-sandbox signing keys
    /// only live in process memory; orphan Pods would 401 every
    /// signed request from the new controller forever).
    pub async fn from_config(config: SandboxConfig) -> Result<Arc<Self>, String> {
        let backend = Backend::from_config(&config)?;
        backend.probe().await?;
        // Clean up orphan Pods + ConfigMaps from a previous run.
        // Errors here are non-fatal — operators may want to keep
        // orphans around for forensics; we log and continue.
        match backend.cleanup_orphans_at_startup().await {
            Ok(0) => {}
            Ok(n) => eprintln!("[sandbox] startup cleanup: removed {n} orphan(s)"),
            Err(e) => eprintln!("[sandbox] startup cleanup failed (non-fatal): {e}"),
        }
        let registry = SandboxRegistry::new();

        // Sealed-record restore (preview-URL § II.0 §4 + § II.5).
        // Feature-flagged behind `SANDBOX_PERSIST_AUTH=1`; default
        // OFF so existing operators see no behaviour change. When
        // enabled, requires `SANDBOX_AEAD_KEY_PATH` (round-6 H8:
        // file-mount only, never an env var). A boot that succeeds
        // without the key file fails-fast — operators MUST opt in
        // intentionally.
        if persist_auth_enabled() {
            match load_aead_key() {
                Ok(key) => {
                    let persist_dir = persist_dir();
                    eprintln!(
                        "[sandbox] persist: SANDBOX_PERSIST_AUTH=1; \
                         restoring sealed records from {persist_dir:?}"
                    );
                    match restore::restore_at_startup(
                        &persist_dir,
                        &key,
                        &backend,
                        &registry,
                        restore::DEFAULT_PROBE_TIMEOUT,
                    )
                    .await
                    {
                        Ok(s) => eprintln!(
                            "[sandbox] persist: restore done seen={} \
                             restored={} mismatched={} unreachable={} \
                             corrupt={} unsupported={}",
                            s.records_seen,
                            s.restored,
                            s.mismatched,
                            s.unreachable,
                            s.corrupt,
                            s.unsupported,
                        ),
                        Err(e) => eprintln!(
                            "[sandbox] persist: restore_at_startup IO failure \
                             (non-fatal; sealed records left in place): {e}"
                        ),
                    }
                }
                Err(e) => {
                    return Err(format!(
                        "SANDBOX_PERSIST_AUTH=1 but AEAD key load failed: {e}. \
                         Set SANDBOX_AEAD_KEY_PATH to a 0400-mode 32-byte file \
                         (round-6 H8: env-var sourcing not supported)."
                    ));
                }
            }
        }
        let state = Arc::new(Self {
            config,
            sandboxes: registry,
            backend,
        });
        // Background re-probe so /readyz reflects current backend
        // state. Without this, the `is_healthy()` flag is set once
        // at boot and stays true even if kubectl auth expires or
        // the cluster goes unreachable.
        start_health_loop(state.clone());
        Ok(state)
    }
}

/// Feature-flag gate for the sealed-record restore path. Defaults
/// off so existing deployments see no change.
fn persist_auth_enabled() -> bool {
    matches!(std::env::var("SANDBOX_PERSIST_AUTH").as_deref(), Ok("1"))
}

/// Load the controller-wide AEAD key from `$SANDBOX_AEAD_KEY_PATH`.
/// Round-6 H8: file-mount only; env-var sourcing is intentionally
/// not supported (procfs leaks).
fn load_aead_key() -> Result<AeadKey, String> {
    let path = std::env::var("SANDBOX_AEAD_KEY_PATH").map_err(|_| {
        "SANDBOX_AEAD_KEY_PATH not set (file-mount only — see preview-URL § IX.a)"
            .to_string()
    })?;
    AeadKey::from_path(path)
}

/// Per-controller persist root. `$SANDBOX_PERSIST_DIR` or default
/// `/var/lib/zeroship/sandbox`. The sealed-records subdir is
/// computed inside `restore`.
fn persist_dir() -> PathBuf {
    std::env::var("SANDBOX_PERSIST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/var/lib/zeroship/sandbox"))
}

/// Periodic backend probe. `probe()` updates the `healthy` flag
/// that `/readyz` exposes; without this loop the flag is set once
/// at boot and stays stale forever (e.g. true after kubectl auth
/// has expired). Re-probes every 30s on a detached compio task.
///
/// We don't wrap async calls in `catch_unwind` — `probe` is
/// designed to return `Result`, not panic. If it does panic the
/// task dies and re-probes stop; that's a real bug worth crashing
/// loudly rather than papering over.
fn start_health_loop(state: Arc<AppState>) {
    compio::runtime::spawn(async move {
        loop {
            compio::time::sleep(Duration::from_secs(30)).await;
            if let Err(e) = state.backend.probe().await {
                eprintln!("[sandbox] health re-probe failed: {e}");
            }
        }
    })
    .detach();
}
