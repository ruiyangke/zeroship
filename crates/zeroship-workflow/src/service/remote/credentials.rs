use super::{CapabilityToken, WorkflowEndpoint, WorkflowServiceError, CLIENT};
use crate::service::capability::{IssuedAppCapability, APP_CAPABILITY_MAX_LIFETIME_SECONDS};
use futures::StreamExt;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use zeroship_core::{
    app_id::AppId,
    service_identity::endpoints,
    service_peers::{service_issuer, ServiceAuth, CONTROL_SERVICE_NAME, WORKER_SERVICE_NAME},
    typed_id,
};

const REFRESH_MARGIN_SECONDS: i64 = APP_CAPABILITY_MAX_LIFETIME_SECONDS / 10;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub(super) enum AppCredentials {
    Fixed(CapabilityToken),
    Worker(Arc<WorkerCredentials>),
}

#[derive(Debug)]
pub(super) struct WorkerCredentials {
    control: WorkflowEndpoint,
    identity: Arc<ServiceAuth>,
    cached: Mutex<Option<CachedCapability>>,
}

#[derive(Debug)]
struct CachedCapability {
    issued: IssuedAppCapability,
    refresh_at: Instant,
}
impl CachedCapability {
    fn new(issued: IssuedAppCapability, now: i64) -> Result<Self, WorkflowServiceError> {
        let remaining = issued.expires_at.checked_sub(now).filter(|remaining| {
            *remaining > 0 && *remaining <= APP_CAPABILITY_MAX_LIFETIME_SECONDS
        });
        let Some(remaining) = remaining else {
            return Err(WorkflowServiceError::Unavailable(
                "invalid workflow capability expiry".into(),
            ));
        };
        let refresh_after = Duration::from_secs(
            remaining
                .saturating_sub(REFRESH_MARGIN_SECONDS)
                .max(0)
                .unsigned_abs(),
        );
        Ok(Self {
            issued,
            refresh_at: Instant::now() + refresh_after,
        })
    }

    fn reusable(&self, now: i64) -> bool {
        // A wall-clock rollback must not extend the cache's original lifetime.
        Instant::now() < self.refresh_at
            && now
                .checked_add(REFRESH_MARGIN_SECONDS)
                .is_some_and(|refresh| refresh < self.issued.expires_at)
    }
}

impl AppCredentials {
    pub(super) fn worker(
        control: &str,
        identity: Arc<ServiceAuth>,
        timeout: Duration,
    ) -> Result<Self, WorkflowServiceError> {
        let worker = service_issuer(WORKER_SERVICE_NAME)
            .map_err(|_| WorkflowServiceError::Unauthenticated)?;
        let (issuer, _) = identity
            .signing_identity()
            .ok_or(WorkflowServiceError::Unauthenticated)?;
        if issuer.principal() != worker.principal() {
            return Err(WorkflowServiceError::Unauthenticated);
        }
        let instance = issuer
            .instance()
            .ok_or(WorkflowServiceError::Unauthenticated)?;
        typed_id::parse_with_prefix(instance, typed_id::WORKER_INSTANCE_PREFIX)
            .map_err(|_| WorkflowServiceError::Unauthenticated)?;
        Ok(Self::Worker(Arc::new(WorkerCredentials {
            control: WorkflowEndpoint::new(control)?.with_limits(timeout, MAX_RESPONSE_BYTES)?,
            identity,
            cached: Mutex::new(None),
        })))
    }

    pub(super) async fn token(&self, app: &AppId) -> Result<CapabilityToken, WorkflowServiceError> {
        match self {
            Self::Fixed(token) => Ok(token.clone()),
            Self::Worker(worker) => worker.token(app).await,
        }
    }

    pub(super) fn reject(&self, rejected: &CapabilityToken) -> Result<bool, WorkflowServiceError> {
        let Self::Worker(worker) = self else {
            return Ok(false);
        };
        let mut cached = worker.cached.lock().map_err(|_| cache_unavailable())?;
        // A slower request may fail after another clone has already refreshed.
        if cached
            .as_ref()
            .is_some_and(|cached| cached.issued.token.as_str() == rejected.as_str())
        {
            *cached = None;
        }
        drop(cached);
        Ok(true)
    }
}

impl WorkerCredentials {
    async fn token(&self, app: &AppId) -> Result<CapabilityToken, WorkflowServiceError> {
        {
            let cached = self.cached.lock().map_err(|_| cache_unavailable())?;
            if let Some(cached) = cached.as_ref().filter(|cached| cached.reusable(now())) {
                return Ok(cached.issued.token.clone());
            }
        }
        // Never hold the cache lock while connecting to Control. Independent
        // runtimes may share a binding without blocking each other's executor.
        let issued = self.fetch(app).await?;
        let fresh = CachedCapability::new(issued, now())?;
        let token = fresh.issued.token.clone();
        let mut cached = self.cached.lock().map_err(|_| cache_unavailable())?;
        if cached
            .as_ref()
            .is_none_or(|cached| cached.issued.expires_at <= fresh.issued.expires_at)
        {
            *cached = Some(fresh);
        }
        drop(cached);
        Ok(token)
    }

    async fn fetch(&self, app: &AppId) -> Result<IssuedAppCapability, WorkflowServiceError> {
        let control = service_issuer(CONTROL_SERVICE_NAME)
            .map_err(|_| WorkflowServiceError::Unauthenticated)?;
        let authorization = self
            .identity
            .authorization_for(&control)
            .ok_or(WorkflowServiceError::Unauthenticated)?;
        let path = endpoints::CONTROL_WORKFLOW_CAPABILITY
            .path_template()
            .replace("{app_id}", app.as_str());
        let builder = CLIENT
            .with(Clone::clone)
            .post(format!("{}{path}", self.control.base))
            .map_err(|_| WorkflowServiceError::InvalidRequest("invalid Control URL".into()))?
            .header("authorization", authorization)
            .map_err(|_| WorkflowServiceError::Unauthenticated)?;
        compio::time::timeout(self.control.timeout, async {
            let mut response = builder.send().await.map_err(|_| {
                WorkflowServiceError::Unavailable("workflow capability issuer unreachable".into())
            })?;
            match response.status().as_u16() {
                200 => {}
                401 => return Err(WorkflowServiceError::Unauthenticated),
                403 => return Err(WorkflowServiceError::PermissionDenied),
                404 => {
                    return Err(WorkflowServiceError::NotFound(
                        "workflow app not found".into(),
                    ))
                }
                _ => {
                    return Err(WorkflowServiceError::Unavailable(
                        "workflow capability issuer unavailable".into(),
                    ))
                }
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.next().await {
                let chunk = chunk.map_err(|_| {
                    WorkflowServiceError::Unavailable(
                        "workflow capability response interrupted".into(),
                    )
                })?;
                if bytes
                    .len()
                    .checked_add(chunk.len())
                    .is_none_or(|size| size > MAX_RESPONSE_BYTES)
                {
                    return Err(WorkflowServiceError::PayloadTooLarge);
                }
                bytes.extend_from_slice(&chunk);
            }
            serde_json::from_slice(&bytes).map_err(|_| {
                WorkflowServiceError::Unavailable("invalid workflow capability response".into())
            })
        })
        .await
        .map_err(|_| WorkflowServiceError::Timeout)?
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn cache_unavailable() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable("workflow credential cache unavailable".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issued(expires_at: i64) -> IssuedAppCapability {
        IssuedAppCapability {
            token: serde_json::from_value(serde_json::json!("redacted-test-token")).unwrap(),
            expires_at,
        }
    }

    #[test]
    fn cache_expiry_is_bounded_and_refreshes_before_expiration() {
        let now = 1000;
        for expiry in [
            now,
            now - 1,
            i64::MAX,
            now + APP_CAPABILITY_MAX_LIFETIME_SECONDS + 1,
        ] {
            assert!(CachedCapability::new(issued(expiry), now).is_err());
        }
        let mut cached = CachedCapability::new(issued(now + 120), now).unwrap();
        assert!(cached.reusable(now));
        assert!(!cached.reusable(now + 120 - REFRESH_MARGIN_SECONDS));
        assert!(!cached.reusable(now + 120));
        cached.refresh_at = Instant::now();
        assert!(!cached.reusable(now - 60));
    }
}
