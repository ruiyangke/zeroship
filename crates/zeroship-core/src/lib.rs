//! zeroship-common — shared types and abstractions for the zeroship platform.

extern crate self as zeroship_core;

/// Re-exports consumed by generated code without forcing service manifests to
/// depend directly on proc-macro implementation details.
#[doc(hidden)]
pub mod __private {
    pub use clap;
    pub use linkme;
    pub use toml;
}

pub mod app_derivation;
pub mod auth;
pub mod auth_provider;
pub mod client_ip;
pub mod config;
pub mod crypto;
pub mod database_role;
pub mod db_url;
pub mod device_grant;
pub mod dispatch_frame;
pub mod logout_token;
pub mod net_policy;
pub mod observability;
pub mod oidc_verify;
pub mod pkce;
pub mod preview_ports;
pub mod project_data_key;
pub mod readiness;
pub mod replication_names;
pub mod schema_bundle;
pub mod schema_name;
pub mod service_assertion;
pub mod service_identity;
pub mod service_peers;
pub mod superjson;
pub mod types;
pub mod usage_event;
pub mod user_envelope;
pub mod worker_join;
pub mod worker_ring;
pub mod workflow_coordination;
pub mod workflow_deployments;
pub mod workflow_jobs;
pub mod workflow_policy;
pub mod workflow_schedules;
pub mod workflow_signal_token;

// The entity-id vocabulary lives in `zeroship-id`, a leaf that carries only
// `uuid` and `serde`. It is re-exported at the paths it has always occupied
// because this crate owns the wire types that NAME these ids, so a caller
// reaching a wire type has the id vocabulary in scope by construction.
//
// The leaf exists because `zeroship-bundle` and `zeroship-cdc-wire` also name an
// app id and sit BELOW this crate: nothing that carries an HTTP client and AEAD
// keys can be depended on by an artifact format. One `AppId` for the whole tree
// is the property being bought.
pub use zeroship_id::{
    app_id, deploy_command, entity_id, invite_id, organization_id, project_id, typed_id, user_id,
    AppId, DeployCommandId, InviteId, OrganizationId, ProjectId, UserId,
};

pub use superjson::Envelope;
pub use types::*;
