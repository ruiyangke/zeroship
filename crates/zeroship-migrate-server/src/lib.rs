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
pub mod bindings;
pub mod bundle;
pub mod config;
pub mod datastore;
pub mod policy;
pub mod provisioning;
pub mod publication;
pub mod rate_limit;
pub mod rotation;
pub mod session;

#[cfg(test)]
pub(crate) mod test_database;

use std::path::PathBuf;
use std::sync::Arc;

use auth::Authenticator;
use bindings::BindingStore;
use policy::ManagedPolicyConfig;
use rate_limit::MutationRateLimiter;
use zeroship_core::readiness::ReadinessGate;

#[allow(missing_debug_implementations)]
pub struct MigrationServiceState {
    pub provision_dsn: String,
    /// The control plane's DSN, for the one write an apply makes there: the
    /// projection of a rotated schema epoch onto `zeroship.databases`.
    pub control_dsn: String,
    pub tmp_dir: PathBuf,
    pub authenticator: Arc<dyn Authenticator>,
    /// Verifies inbound PLATFORM-service assertions.
    ///
    /// A separate identity from [`MigrationServiceState::authenticator`], which
    /// resolves a creator PRINCIPAL through an OAuth bearer. A platform caller
    /// has no creator principal and no app to be authorized against: it presents
    /// a service assertion, and the endpoint allowlist decides what that identity
    /// may reach. `None` when no peer bundle is configured.
    pub peers: Option<Arc<dyn zeroship_core::service_identity::IdentityVerifier + Send + Sync>>,
    pub mutation_rate_limiter: Arc<dyn MutationRateLimiter>,
    pub trust_proxy: bool,
    pub policy_config: ManagedPolicyConfig,
    /// Whether the app a request authorizes as still reaches the database it
    /// named. Opened per request; this service holds no shared client.
    pub bindings: BindingStore,
    /// Bounds `/readyz`. The probe opens a connection (this service has no
    /// shared client to reuse - see `BindingStore::probe`), so the gate's
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
            peers: None,
            mutation_rate_limiter,
            trust_proxy,
            policy_config,
            bindings: BindingStore::new(control_dsn.clone()),
            control_dsn,
            readiness: ReadinessGate::with_defaults(),
        }
    }

    /// Accept platform-service assertions verified against `peers`.
    #[must_use]
    pub fn verifying_peers(
        mut self,
        peers: Arc<dyn zeroship_core::service_identity::IdentityVerifier + Send + Sync>,
    ) -> Self {
        self.peers = Some(peers);
        self
    }
}

pub fn configure(cfg: &mut ntex::web::ServiceConfig) {
    api::configure(cfg);
}
