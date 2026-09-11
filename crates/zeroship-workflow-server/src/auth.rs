use async_trait::async_trait;
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use zeroship_core::{
    app_id::AppId,
    service_assertion::{
        presented_issuer, thumbprint_key_id, ReplayStore, ServiceAssertionVerifier,
        ServiceTrustBundle,
    },
    service_identity::{verify_service_call, AuthError, IdentityVerifier, ServiceEndpoint},
    service_peers::{service_issuer, WORKER_SERVICE_NAME},
    typed_id,
};
use zeroship_workflow::{
    service::{
        capability::{verify_app_capability, AppOperation, WORKFLOW_AUDIENCE},
        WorkerIdentity,
    },
    WorkflowServiceError,
};

/// The host resolves enrolled worker keys from trusted registry state.
#[async_trait(?Send)]
pub trait WorkerRegistry: Send + Sync + std::fmt::Debug {
    async fn active_key(&self, instance: &str) -> Result<Option<[u8; 32]>, WorkflowServiceError>;
    async fn ready(&self) -> Result<(), WorkflowServiceError>;
}

#[derive(Debug)]
pub struct PostgresWorkerRegistry {
    client: Arc<compio_postgres::Client>,
}
impl PostgresWorkerRegistry {
    #[must_use]
    pub fn new(client: Arc<compio_postgres::Client>) -> Self {
        Self { client }
    }
}
#[async_trait(?Send)]
impl WorkerRegistry for PostgresWorkerRegistry {
    async fn ready(&self) -> Result<(), WorkflowServiceError> {
        self.client
            .query(
                "SELECT id,status,public_key FROM zeroship.worker_instances LIMIT 0",
                &[],
            )
            .await
            .map(|_| ())
            .map_err(|_| WorkflowServiceError::Unavailable("worker registry unavailable".into()))
    }
    async fn active_key(&self, instance: &str) -> Result<Option<[u8; 32]>, WorkflowServiceError> {
        let rows = self
            .client
            .query(
                "SELECT public_key FROM zeroship.worker_instances WHERE id=$1 AND status='active'",
                &[&instance],
            )
            .await
            .map_err(|_| WorkflowServiceError::Unavailable("worker registry unavailable".into()))?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let key: &[u8] = row
            .try_get(0)
            .map_err(|_| WorkflowServiceError::Unavailable("invalid worker registry key".into()))?;
        let key = key
            .try_into()
            .map_err(|_| WorkflowServiceError::Unavailable("invalid worker registry key".into()))?;
        Ok(Some(key))
    }
}

pub struct WorkflowAuth {
    app_keys: ServiceTrustBundle,
    peers: Arc<dyn IdentityVerifier + Send + Sync>,
    workers: Arc<dyn WorkerRegistry>,
    replay: Arc<dyn ReplayStore + Send + Sync>,
}
impl std::fmt::Debug for WorkflowAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowAuth").finish_non_exhaustive()
    }
}
impl WorkflowAuth {
    pub async fn ready(&self) -> Result<(), WorkflowServiceError> {
        self.workers.ready().await
    }
    #[must_use]
    pub fn new(
        app_keys: ServiceTrustBundle,
        peers: Arc<dyn IdentityVerifier + Send + Sync>,
        workers: Arc<dyn WorkerRegistry>,
        replay: Arc<dyn ReplayStore + Send + Sync>,
    ) -> Self {
        Self {
            app_keys,
            peers,
            workers,
            replay,
        }
    }
    pub fn app(
        &self,
        header: Option<&str>,
        app: &AppId,
        operation: AppOperation,
    ) -> Result<(), WorkflowServiceError> {
        let token = header
            .and_then(zeroship_core::auth::extract_bearer)
            .ok_or(WorkflowServiceError::Unauthenticated)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| WorkflowServiceError::Unavailable("workflow clock unavailable".into()))?
            .as_secs();
        let now = i64::try_from(now)
            .map_err(|_| WorkflowServiceError::Unavailable("workflow clock unavailable".into()))?;
        verify_app_capability(token, &self.app_keys, now)?.authorize(app, operation)
    }
    pub async fn peer(
        &self,
        header: Option<&str>,
        endpoint: ServiceEndpoint,
    ) -> Result<(), WorkflowServiceError> {
        verify_service_call(self.peers.as_ref(), header, WORKFLOW_AUDIENCE, endpoint)
            .await
            .map(|_| ())
            .map_err(auth_error)
    }
    pub async fn worker(
        &self,
        header: Option<&str>,
        endpoint: ServiceEndpoint,
    ) -> Result<WorkerIdentity, WorkflowServiceError> {
        let issuer = presented_issuer(header).ok_or(WorkflowServiceError::Unauthenticated)?;
        let role = service_issuer(WORKER_SERVICE_NAME)
            .map_err(|_| WorkflowServiceError::Internal("invalid worker service role".into()))?;
        if issuer.principal() != role.principal() {
            return Err(WorkflowServiceError::Unauthenticated);
        }
        let instance = issuer
            .instance()
            .ok_or(WorkflowServiceError::Unauthenticated)?;
        typed_id::parse_with_prefix(instance, typed_id::WORKER_INSTANCE_PREFIX)
            .map_err(|_| WorkflowServiceError::Unauthenticated)?;
        let public = self
            .workers
            .active_key(instance)
            .await?
            .ok_or(WorkflowServiceError::Unauthenticated)?;
        let mut keys = ServiceTrustBundle::new();
        keys.trust(&issuer, thumbprint_key_id(&public), public)
            .map_err(|_| WorkflowServiceError::Unauthenticated)?;
        let verifier = ServiceAssertionVerifier::new(keys, self.replay.clone());
        // The selector is untrusted until verification succeeds against the
        // registry key bound to that exact issuer. Role keys are never a fallback.
        verify_service_call(&verifier, header, WORKFLOW_AUDIENCE, endpoint)
            .await
            .map_err(auth_error)?;
        WorkerIdentity::new(issuer.as_str().into())
    }
}
fn auth_error(error: AuthError) -> WorkflowServiceError {
    match error {
        AuthError::StoreUnavailable => {
            WorkflowServiceError::Unavailable("workflow authentication store unavailable".into())
        }
        _ => WorkflowServiceError::Unauthenticated,
    }
}
