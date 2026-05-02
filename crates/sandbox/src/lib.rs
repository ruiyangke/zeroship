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
pub mod preview;
pub mod preview_share;
pub mod preview_ws;
pub mod registry;
pub mod restore;

use std::sync::Arc;
use std::time::Duration;

use crate::backend::Backend;
use crate::config::SandboxConfig;
use crate::persist::Persistence;
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
        // Build the shared persistence handle FIRST so the same `Arc`
        // can be cloned into both the backend (for seal-on-create /
        // delete-on-stop) and the boot-restore loop below (for the
        // initial directory walk). `from_env` returns `None` when
        // `SANDBOX_PERSIST_AUTH` is not `1` — the disabled shape both
        // for the backend and the restore call. Failures here are
        // fail-fast: an operator who set the flag with a missing /
        // wrong-mode key file wants to see that at boot, not silently
        // run with persistence off.
        let persist: Option<Arc<Persistence>> =
            Persistence::from_env()?.map(Arc::new);
        let backend = Backend::from_config_with_persist(&config, persist.clone())?;
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
        // Reuses the shared `persist` handle built above so we don't
        // re-open the AEAD key file or re-read the env. `None` is the
        // disabled/no-op shape — feature-flagged behind
        // `SANDBOX_PERSIST_AUTH=1`; default OFF.
        if let Some(p) = &persist {
            let dir = p.persist_dir();
            eprintln!(
                "[sandbox] persist: SANDBOX_PERSIST_AUTH=1; \
                 restoring sealed records from {dir:?}"
            );
            match restore::restore_at_startup(
                &dir,
                p.aead_key(),
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
