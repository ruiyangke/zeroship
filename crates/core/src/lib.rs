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

pub mod types;
pub mod auth;
pub mod auth_provider;
pub mod db_url;
pub mod device_grant;
pub mod typed_id;
pub mod usage_event;
pub mod net_policy;
pub mod crypto;
pub mod config;
pub mod dispatch_frame;
pub mod observability;
pub mod logout_token;
pub mod oidc_verify;
pub mod pkce;
pub mod preview_ports;
pub mod readiness;
pub mod replication_names;
pub mod service_assertion;
pub mod service_identity;
pub mod superjson;

pub use superjson::Envelope;
pub use types::*;
