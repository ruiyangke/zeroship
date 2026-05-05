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
pub mod db;
pub mod files;
pub mod handlers;
pub mod persist;
pub mod preview;
pub mod preview_share;
pub mod preview_share_handlers;
pub mod preview_ws;
pub mod registry;
pub mod restore;

use std::sync::Arc;
use std::time::Duration;

use crate::backend::Backend;
use crate::config::SandboxConfig;
use crate::db::{Database, LATEST_MIGRATION_VERSION};
use crate::persist::Persistence;
use crate::preview_share_handlers::MintRateLimiter;
use crate::registry::SandboxRegistry;

/// Shared application state passed to every handler.
#[allow(missing_debug_implementations)]
pub struct AppState {
    pub config: SandboxConfig,
    pub sandboxes: SandboxRegistry,
    pub backend: Backend,
    /// Phase-3 mint-side rate limiter. `Some` in production; `None`
    /// for tests that build `AppState` directly without
    /// `from_config`. Handlers that consume it `expect()` on `Some`.
    pub mint_rate_limiter: Option<MintRateLimiter>,
    /// Phase-0 pg-backed non-secret state handle (sandbox-pg-state
    /// design § 8.2). `None` is the disabled-by-absence shape:
    /// `SANDBOX_DATABASE_URL` is unset, so pg integration is off.
    /// In Phase 0, the handle exists but no live call sites consume
    /// it — Phase 1 wires `insert_sandbox` / `record_event` into
    /// the backends. The schema is brought to
    /// [`LATEST_MIGRATION_VERSION`] before this state ships.
    pub database: Option<Arc<Database>>,
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

        // Phase-0 pg-backed state: build BEFORE the backend probe so
        // the schema reaches the right version before any backend op
        // could try to write. Pg-required features stay dormant in
        // Phase 0 — the handle is plumbed-but-unused; Phase 1 wires
        // call sites. (`docs/proposals/sandbox-pg-state.md` § 15
        // Phase 0.)
        let database: Option<Arc<Database>> = match Database::from_env().await {
            Ok(opt) => opt.map(Arc::new),
            Err(e) => {
                // Dev escape hatch: SANDBOX_PG_OPTIONAL=1 (D-11)
                // logs and continues with pg disabled. Production
                // refuses to boot; the operator must fix the
                // config.
                if matches!(std::env::var("SANDBOX_PG_OPTIONAL").as_deref(), Ok("1")) {
                    tracing::warn!(
                        error = %e,
                        "SANDBOX_PG_OPTIONAL=1: Database::from_env failed; pg integration disabled"
                    );
                    None
                } else {
                    return Err(format!("Database::from_env: {e}"));
                }
            }
        };

        if let Some(db) = &database {
            // Block boot until the schema is at the version this
            // binary was built against. A designated migrator
            // applies pending migrations; everyone else polls.
            // Failure aborts startup.
            if let Err(e) = db
                .ensure_schema_at_version(LATEST_MIGRATION_VERSION)
                .await
            {
                if matches!(std::env::var("SANDBOX_PG_OPTIONAL").as_deref(), Ok("1")) {
                    tracing::warn!(
                        error = %e,
                        target_version = LATEST_MIGRATION_VERSION,
                        "SANDBOX_PG_OPTIONAL=1: ensure_schema_at_version failed; \
                         pg integration disabled"
                    );
                } else {
                    return Err(format!(
                        "ensure_schema_at_version({LATEST_MIGRATION_VERSION}): {e}"
                    ));
                }
            } else {
                tracing::info!(
                    target_version = LATEST_MIGRATION_VERSION,
                    host_id = %db.host_id(),
                    "sandbox pg: schema ready"
                );
            }
        }

        let backend = Backend::from_config_with_persist(&config, persist.clone())?;
        backend.probe().await?;
        // Clean up orphan Pods + ConfigMaps from a previous run.
        // Errors here are non-fatal — operators may want to keep
        // orphans around for forensics; we log and continue.
        match backend.cleanup_orphans_at_startup().await {
            Ok(0) => {}
            Ok(n) => tracing::info!(orphans = n, "sandbox startup cleanup: removed orphans"),
            Err(e) => tracing::warn!(error = %e, "sandbox startup cleanup failed (non-fatal)"),
        }
        let registry = SandboxRegistry::new();

        // Sealed-record restore (preview-URL § II.0 §4 + § II.5).
        // Reuses the shared `persist` handle built above so we don't
        // re-open the AEAD key file or re-read the env. `None` is the
        // disabled/no-op shape — feature-flagged behind
        // `SANDBOX_PERSIST_AUTH=1`; default OFF.
        if let Some(p) = &persist {
            let dir = p.persist_dir();
            tracing::info!(
                persist_dir = ?dir,
                "sandbox persist: SANDBOX_PERSIST_AUTH=1; restoring sealed records"
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
                Ok(s) => tracing::info!(
                    seen = s.records_seen,
                    restored = s.restored,
                    mismatched = s.mismatched,
                    unreachable = s.unreachable,
                    corrupt = s.corrupt,
                    unsupported = s.unsupported,
                    "sandbox persist: restore done"
                ),
                Err(e) => tracing::warn!(
                    error = %e,
                    "sandbox persist: restore_at_startup IO failure (non-fatal; sealed records left in place)"
                ),
            }
        }
        let state = Arc::new(Self {
            config,
            sandboxes: registry,
            backend,
            mint_rate_limiter: Some(MintRateLimiter::new()),
            database,
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
                tracing::warn!(error = %e, "sandbox health re-probe failed");
            }
        }
    })
    .detach();
}
