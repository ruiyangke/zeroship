//! Standalone creator migration service.
//!
//! Phase 1 exposes the frozen `.ir.json` apply surface. Phase 2 routes creator
//! applies through a server-managed policy ceiling, engine composition, and the
//! sealed shared-infra apply path.

// `target_session_attrs` grew from three variants to six and each connect path
// now carries a recovery probe, which widened the async chain reaching this
// crate past rustc's default layout-query depth of 128. Structural fixes were
// tried first and do not help: the depth is cumulative across the whole
// pool -> connect -> handshake chain, so boxing any single future removes one
// level, not the ~130 reported. `crates/zeroship-data-v8/src/lib.rs` and
// `crates/zeroship-gateway/src/main.rs` already carry this for the same reason. It is a
// compiler resource limit, not a correctness guard.
#![recursion_limit = "256"]

pub mod api;
pub mod apply;
pub mod auth;
pub mod config;
pub mod policy;
pub mod provisioning;
pub mod publication;
pub mod rate_limit;
pub mod schema_apply_store;
pub mod session;

#[cfg(test)]
pub(crate) mod test_database;

use std::path::PathBuf;
use std::sync::Arc;

use auth::Authenticator;
use policy::ManagedPolicyConfig;
use rate_limit::MutationRateLimiter;
use schema_apply_store::SchemaApplyStore;
use zeroship_core::readiness::ReadinessGate;

#[allow(missing_debug_implementations)]
pub struct MigrationServiceState {
    pub provision_dsn: String,
    pub tmp_dir: PathBuf,
    pub authenticator: Arc<dyn Authenticator>,
    pub mutation_rate_limiter: Arc<dyn MutationRateLimiter>,
    pub trust_proxy: bool,
    pub policy_config: ManagedPolicyConfig,
    pub schema_apply_store: SchemaApplyStore,
    /// Bounds `/readyz`. The probe opens a connection (this service has no
    /// shared client to reuse - see `SchemaApplyStore::connect`), so the gate's
    /// TTL is what keeps an unauthenticated probe flood from becoming a
    /// connection flood.
    pub readiness: ReadinessGate,
}

impl MigrationServiceState {
    #[must_use]
    pub fn new(
        provision_dsn: String,
        control_dsn: String,
        tmp_dir: PathBuf,
        authenticator: Arc<dyn Authenticator>,
        mutation_rate_limiter: Arc<dyn MutationRateLimiter>,
        trust_proxy: bool,
        policy_config: ManagedPolicyConfig,
    ) -> Self {
        Self {
            provision_dsn,
            tmp_dir,
            authenticator,
            mutation_rate_limiter,
            trust_proxy,
            policy_config,
            schema_apply_store: SchemaApplyStore::new(control_dsn),
            readiness: ReadinessGate::with_defaults(),
        }
    }
}

pub fn configure(cfg: &mut ntex::web::ServiceConfig) {
    api::configure(cfg);
}
