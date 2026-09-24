//! Verification for Control assertions and enrolled worker identities.
#![allow(
    clippy::future_not_send,
    reason = "registry queries run on the owning compio runtime"
)]

use crate::coordinator::Error;
use async_trait::async_trait;
use std::sync::Arc;
use zeroship_core::{
    service_assertion::{
        presented_issuer, thumbprint_key_id, ReplayStore, ServiceAssertionVerifier,
        ServiceTrustBundle,
    },
    service_identity::{verify_service_call, AuthError, IdentityVerifier, ServiceEndpoint},
    service_peers::{service_issuer, WORKER_SERVICE_NAME},
    workflow_coordination::{WorkerId, AUDIENCE},
};

/// The host resolves enrolled worker keys from trusted registry state.
#[async_trait(?Send)]
pub trait WorkerRegistry: Send + Sync + std::fmt::Debug {
    /// # Errors
    /// Returns `Unavailable` when the trusted registry cannot be read.
    async fn active_key(&self, instance: &str) -> Result<Option<[u8; 32]>, Error>;
    /// # Errors
    /// Returns `Unavailable` when registry access is not ready.
    async fn ready(&self) -> Result<(), Error>;
}

#[derive(Debug)]
pub struct PostgresWorkerRegistry {
    client: Arc<compio_postgres::Client>,
}
impl PostgresWorkerRegistry {
    #[must_use]
    pub const fn new(client: Arc<compio_postgres::Client>) -> Self {
        Self { client }
    }
}
#[async_trait(?Send)]
impl WorkerRegistry for PostgresWorkerRegistry {
    /// Prove the registry is reachable and readable for EXACTLY the columns
    /// [`Self::active_key`] projects and filters on. A probe narrower than the
    /// authentication query reports ready while every worker call fails.
    async fn ready(&self) -> Result<(), Error> {
        self.client
            .query(
                "SELECT id,status,public_key,expires_at FROM zeroship.worker_instances LIMIT 0",
                &[],
            )
            .await
            .map(|_| ())
            .map_err(|_| Error::Unavailable)
    }
    /// The key a LIVE instance's assertions verify under, or nothing.
    ///
    /// TWO FILTERS, AND EACH IS A DIFFERENT WAY A CREDENTIAL STOPS WORKING.
    /// `status` is retirement and purge. `expires_at` is the LEASE Control
    /// renews, and it is what makes an instance nobody retired - crashed,
    /// killed or forgotten - stop authenticating with nobody acting; nothing
    /// reaps the row, so without this comparison that key never dies. The
    /// comparison is against the DATABASE's clock, so replicas whose clocks
    /// differ answer the same. This is the predicate
    /// `zeroship_control::worker_join::active_instance_public_key` resolves
    /// the same table with: one identity is live for both hosts or neither.
    async fn active_key(&self, instance: &str) -> Result<Option<[u8; 32]>, Error> {
        let rows = self
            .client
            .query(
                "SELECT public_key FROM zeroship.worker_instances \
                   WHERE id=$1 AND status='active' AND expires_at > now()",
                &[&instance],
            )
            .await
            .map_err(|_| Error::Unavailable)?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let key: &[u8] = row.try_get(0).map_err(|_| Error::Unavailable)?;
        let key = key.try_into().map_err(|_| Error::Unavailable)?;
        Ok(Some(key))
    }
}

pub struct WorkflowAuth {
    peers: Arc<dyn IdentityVerifier + Send + Sync>,
    workers: Arc<dyn WorkerRegistry>,
    replay: Arc<dyn ReplayStore + Send + Sync>,
}

/// An enrolled worker and the exact public key that verified this request.
#[derive(Debug)]
pub struct VerifiedWorker {
    id: WorkerId,
    public_key: [u8; 32],
}

impl VerifiedWorker {
    #[must_use]
    pub const fn id(&self) -> &WorkerId {
        &self.id
    }

    #[must_use]
    pub fn signing_key_id(&self) -> String {
        thumbprint_key_id(&self.public_key)
    }
}
impl std::fmt::Debug for WorkflowAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowAuth").finish_non_exhaustive()
    }
}
impl WorkflowAuth {
    /// Recheck enrollment without reusing or verifying the request assertion again.
    ///
    /// # Errors
    /// Refuses revocation, lapse, key replacement and unavailable registry storage.
    pub async fn revalidate_worker(&self, worker: &VerifiedWorker) -> Result<WorkerId, Error> {
        match self.workers.active_key(worker.id.as_str()).await? {
            Some(public_key) if public_key == worker.public_key => Ok(worker.id.clone()),
            _ => Err(Error::Denied),
        }
    }

    /// Control may verify placement only for an instance still live: enrolled
    /// as active and inside its lease.
    ///
    /// # Errors
    /// Refuses revoked, lapsed or missing workers and unavailable registry storage.
    pub async fn active_worker(&self, worker: &WorkerId) -> Result<(), Error> {
        self.workers
            .active_key(worker.as_str())
            .await?
            .map(|_| ())
            .ok_or(Error::Denied)
    }

    /// # Errors
    /// Returns `Unavailable` when the worker registry cannot be queried.
    pub async fn ready(&self) -> Result<(), Error> {
        self.workers.ready().await
    }
    #[must_use]
    pub fn new(
        peers: Arc<dyn IdentityVerifier + Send + Sync>,
        workers: Arc<dyn WorkerRegistry>,
        replay: Arc<dyn ReplayStore + Send + Sync>,
    ) -> Self {
        Self {
            peers,
            workers,
            replay,
        }
    }
    /// # Errors
    /// Rejects unauthenticated or unauthorized assertions and unavailable replay storage.
    pub async fn peer(
        &self,
        header: Option<&str>,
        endpoint: ServiceEndpoint,
    ) -> Result<zeroship_core::service_assertion::ServiceIssuer, Error> {
        let issuer = presented_issuer(header).ok_or(Error::Unauthenticated)?;
        verify_service_call(self.peers.as_ref(), header, AUDIENCE, endpoint)
            .await
            .map(|_| issuer)
            .map_err(|error| auth_error(&error))
    }
    /// Verify a caller of an endpoint TWO kinds of identity may reach.
    ///
    /// The journal-ensure endpoint is granted to Control, which signs under a
    /// role key published in the peer bundle, and to a worker, which signs under
    /// an enrolled INSTANCE key held in the registry. The presented issuer
    /// selects the verifier, so this is a dispatch rather than a fallback: a
    /// caller claiming `svc/worker` is verified against the registry and nothing
    /// else, and one claiming any other principal never reaches it.
    ///
    /// # Errors
    /// Rejects unauthenticated or unauthorized assertions and unavailable stores.
    pub async fn journal_caller(
        &self,
        header: Option<&str>,
        endpoint: ServiceEndpoint,
    ) -> Result<String, Error> {
        let issuer = presented_issuer(header).ok_or(Error::Unauthenticated)?;
        let worker_role = service_issuer(WORKER_SERVICE_NAME).map_err(|_| Error::Unavailable)?;
        if issuer.principal() == worker_role.principal() {
            return self
                .worker(header, endpoint)
                .await
                .map(|worker| worker.id().as_str().to_owned());
        }
        self.peer(header, endpoint)
            .await
            .map(|issuer| issuer.as_str().to_owned())
    }

    /// # Errors
    /// Rejects inactive or mismatched identities, invalid assertions and unavailable stores.
    pub async fn worker(
        &self,
        header: Option<&str>,
        endpoint: ServiceEndpoint,
    ) -> Result<VerifiedWorker, Error> {
        let issuer = presented_issuer(header).ok_or(Error::Unauthenticated)?;
        let role = service_issuer(WORKER_SERVICE_NAME).map_err(|_| Error::Unavailable)?;
        if issuer.principal() != role.principal() {
            return Err(Error::Unauthenticated);
        }
        let instance = issuer.instance().ok_or(Error::Unauthenticated)?;
        let worker = WorkerId::parse(instance).map_err(|_| Error::Unauthenticated)?;
        let public = self
            .workers
            .active_key(instance)
            .await?
            .ok_or(Error::Unauthenticated)?;
        let mut keys = ServiceTrustBundle::new();
        keys.trust(&issuer, thumbprint_key_id(&public), public)
            .map_err(|_| Error::Unauthenticated)?;
        let verifier = ServiceAssertionVerifier::new(keys, self.replay.clone());
        // The selector is untrusted until verification succeeds against the
        // registry key bound to that exact issuer. Role keys are never a fallback.
        verify_service_call(&verifier, header, AUDIENCE, endpoint)
            .await
            .map_err(|error| auth_error(&error))?;
        Ok(VerifiedWorker {
            id: worker,
            public_key: public,
        })
    }
}
const fn auth_error(error: &AuthError) -> Error {
    match error {
        AuthError::StoreUnavailable => Error::Unavailable,
        _ => Error::Unauthenticated,
    }
}
