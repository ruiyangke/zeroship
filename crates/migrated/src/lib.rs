//! Standalone creator migration service.
//!
//! Phase 1 exposes the frozen `.ir.json` apply surface. Phase 2 routes creator
//! applies through a server-managed policy ceiling, engine composition, and the
//! sealed shared-infra apply path.

pub mod api;
pub mod apply;
pub mod auth;
pub mod migration_store;
pub mod policy;
pub mod policy_store;

use std::path::PathBuf;
use std::sync::Arc;

use auth::Authenticator;
use migration_store::MigrationStore;
use policy::ManagedPolicyConfig;
use policy_store::AppPolicyStore;

#[allow(missing_debug_implementations)]
pub struct MigrationServiceState {
    pub provision_dsn: String,
    pub tmp_dir: PathBuf,
    pub authenticator: Arc<dyn Authenticator>,
    pub policy_config: ManagedPolicyConfig,
    pub policy_store: AppPolicyStore,
    pub migration_store: MigrationStore,
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
        }
    }
}

pub fn configure(cfg: &mut ntex::web::ServiceConfig) {
    api::configure(cfg);
}
