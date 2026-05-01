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
pub mod session;

use std::sync::Arc;

use crate::backend::Backend;
use crate::config::SandboxConfig;
use crate::session::SessionRegistry;

/// Shared application state passed to every handler.
#[allow(missing_debug_implementations)]
pub struct AppState {
    pub config: SandboxConfig,
    pub sessions: SessionRegistry,
    pub backend: Backend,
}

impl AppState {
    /// Build a fresh state from config. Probes the chosen backend at
    /// construction time so callers fail fast on misconfiguration.
    pub async fn from_config(config: SandboxConfig) -> Result<Arc<Self>, String> {
        let backend = Backend::from_config(&config)?;
        backend.probe().await?;
        Ok(Arc::new(Self {
            config,
            sessions: SessionRegistry::new(),
            backend,
        }))
    }
}
