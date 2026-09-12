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
pub mod worker_ring;
pub mod workflow_coordination;
pub mod app_id;
pub mod auth;
pub mod auth_provider;
pub mod client_ip;
pub mod config;
pub mod crypto;
pub mod database_role;
pub mod db_url;
pub mod device_grant;
pub mod dispatch_frame;
pub(crate) mod entity_id;
pub mod invite_id;
pub mod logout_token;
pub mod net_policy;
pub mod observability;
pub mod oidc_verify;
pub mod organization_id;
pub mod pkce;
pub mod preview_ports;
pub mod project_id;
pub mod project_data_key;
pub mod readiness;
pub mod replication_names;
pub mod service_assertion;
pub mod service_identity;
pub mod service_peers;
pub mod superjson;
pub mod typed_id;
pub mod types;
pub mod usage_event;
pub mod user_envelope;

pub use superjson::Envelope;
pub use types::*;
