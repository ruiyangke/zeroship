//! zeroship-control — library crate.
//!
//! This lib exists so integration tests under `tests/` can reach the
//! registry + env store + handler types. The `zeroship-control` binary
//! (`src/main.rs`) is a thin wrapper around these modules.

pub mod api;
pub mod audit;
pub mod env_handlers;
pub mod env_store;
pub mod internal;
pub mod metering;
pub mod registry;
pub mod stripe_handlers;
pub mod stripe_store;

use std::sync::Arc;

use zeroize::Zeroizing;
use zeroship_core::vfs::BundleStore;

pub use env_store::EnvStore;
pub use registry::Registry;
pub use stripe_store::StripeStore;

/// String that zeroizes its heap buffer on drop. Use for any secret
/// that lives in `AppState` or other long-lived structs — keeps
/// post-mortem `/proc/<pid>/mem` reads from recovering the value.
pub type SecretString = Zeroizing<String>;

/// Shared application state injected into every handler.
///
/// Sensitive secrets are wrapped in `SecretString` (Zeroizing<String>)
/// so they don't linger in heap for /proc/<pid>/mem or core-dump reads
/// after the process exits or panics.
#[allow(missing_debug_implementations)]
pub struct AppState {
    pub registry: Registry,
    pub env_store: EnvStore,
    pub stripe_store: StripeStore,
    pub vfs: Arc<dyn BundleStore + Send + Sync>,
    pub control_key: SecretString,
    pub master_key: SecretString,
    /// Stripe webhook signing secret. Required in prod; empty +
    /// `insecure_dev=true` skips verification.
    pub stripe_webhook_secret: SecretString,
    /// Set to `true` by the `--dev-insecure` CLI flag (or
    /// `ZEROSHIP_DEV_INSECURE=1` env var). ONLY permits empty admin /
    /// control / webhook secrets when explicitly opted in. Production
    /// must leave this false; the startup guard in `main.rs` refuses
    /// to boot with missing secrets otherwise.
    pub insecure_dev: bool,
}
