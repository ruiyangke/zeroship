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
    /// Stripe webhook signing secret — set via --stripe-webhook-secret
    /// or the STRIPE_WEBHOOK_SECRET env var. Empty disables signature
    /// checks (dev only).
    pub stripe_webhook_secret: String,
}
