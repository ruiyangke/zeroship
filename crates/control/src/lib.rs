//! zeroship-control — library crate.
//!
//! This lib exists so integration tests under `tests/` can reach the
//! registry + env store + handler types. The `zeroship-control` binary
//! (`src/main.rs`) is a thin wrapper around these modules.

pub mod api;
pub mod env_handlers;
pub mod env_store;
pub mod internal;
pub mod metering;
pub mod registry;
pub mod stripe_handlers;
pub mod stripe_store;

use std::sync::Arc;

use zeroship_core::vfs::BundleStore;

pub use env_store::EnvStore;
pub use registry::Registry;
pub use stripe_store::StripeStore;

/// Shared application state injected into every handler.
#[allow(missing_debug_implementations)]
pub struct AppState {
    pub registry: Registry,
    pub env_store: EnvStore,
    pub stripe_store: StripeStore,
    pub vfs: Arc<dyn BundleStore + Send + Sync>,
    pub control_key: String,
    pub master_key: String,
    /// Stripe webhook signing secret. Required in prod; `"dev-insecure"`
    /// sentinel with `insecure_dev=true` skips verification.
    pub stripe_webhook_secret: String,
    /// Set to `true` by the `--dev-insecure` CLI flag (or
    /// `ZEROSHIP_DEV_INSECURE=1` env var). ONLY permits empty admin /
    /// control / webhook secrets when explicitly opted in. Production
    /// must leave this false; the startup guard in `main.rs` refuses
    /// to boot with missing secrets otherwise.
    pub insecure_dev: bool,
}
