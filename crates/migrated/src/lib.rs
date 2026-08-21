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
// level, not the ~130 reported. `crates/plugin-db/src/lib.rs` and
// `crates/gateway/src/main.rs` already carry this for the same reason. It is a
// compiler resource limit, not a correctness guard.
#![recursion_limit = "256"]

pub mod api;
pub mod apply;
pub mod auth;
pub mod config;
pub mod migration_store;
pub mod policy;
pub mod policy_store;
pub mod provisioning;
pub mod publication;

use std::path::PathBuf;
use std::sync::Arc;

use auth::Authenticator;
use migration_store::MigrationStore;
use policy::ManagedPolicyConfig;
use policy_store::AppPolicyStore;
use zeroship_core::readiness::ReadinessGate;

#[allow(missing_debug_implementations)]
pub struct MigrationServiceState {
    pub provision_dsn: String,
    pub tmp_dir: PathBuf,
    pub authenticator: Arc<dyn Authenticator>,
    pub policy_config: ManagedPolicyConfig,
    pub policy_store: AppPolicyStore,
    pub migration_store: MigrationStore,
    /// Bounds `/readyz`. The probe opens a connection (this service has no
    /// shared client to reuse - see `AppPolicyStore::connect`), so the gate's
    /// TTL is what keeps an unauthenticated probe flood from becoming a
    /// connection flood.
    pub readiness: ReadinessGate,
}

impl MigrationServiceState {
    #[must_use]
    pub fn new(
        provision_dsn: String,
        policy_store_dsn: String,
        tmp_dir: PathBuf,
        authenticator: Arc<dyn Authenticator>,
        policy_config: ManagedPolicyConfig,
    ) -> Self {
        let policy_store = AppPolicyStore::new(policy_store_dsn.clone());
        let migration_store = MigrationStore::new(policy_store_dsn);
        Self {
            provision_dsn,
            tmp_dir,
            authenticator,
            policy_config,
            policy_store,
            migration_store,
            readiness: ReadinessGate::with_defaults(),
        }
    }
}

pub fn configure(cfg: &mut ntex::web::ServiceConfig) {
    api::configure(cfg);
}
