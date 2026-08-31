//! Shared-store mutation throttling for the edge-facing migration service.

use std::net::IpAddr;
use std::sync::Arc;

use async_trait::async_trait;
use compio_postgres::Client;
use zeroship_authn::rate_limit::{self, Quota, RateLimitDecision};

const UNRESOLVED_CLIENT_IDENTITY: &str = "unresolved";

/// One mutation-rate-limit decision source.
///
/// Production uses [`PostgresMutationRateLimiter`]. The trait keeps liveness
/// and readiness tests independent of a database-backed request path they do
/// not invoke.
#[async_trait(?Send)]
pub trait MutationRateLimiter: Send + Sync {
    /// Consume one mutation token for the resolved source identity.
    async fn consume(&self, source_ip: Option<IpAddr>) -> Result<RateLimitDecision, String>;
}

/// The cross-process implementation backed by `zeroship.rate_limits`.
#[derive(Debug)]
pub struct PostgresMutationRateLimiter {
    control_pg: Arc<Client>,
    quota: Quota,
}

impl PostgresMutationRateLimiter {
    #[must_use]
    pub fn new(control_pg: Arc<Client>, quota: Quota) -> Self {
        Self { control_pg, quota }
    }
}

#[async_trait(?Send)]
impl MutationRateLimiter for PostgresMutationRateLimiter {
    async fn consume(&self, source_ip: Option<IpAddr>) -> Result<RateLimitDecision, String> {
        let identity = source_ip.map_or_else(
            || UNRESOLVED_CLIENT_IDENTITY.to_string(),
            |ip| ip.to_string(),
        );
        let key = format!("migrate:mutation:ip:{identity}");
        rate_limit::consume(&self.control_pg, &key, self.quota)
            .await
            .map_err(|error| format!("shared mutation bucket {key}: {error}"))
    }
}
