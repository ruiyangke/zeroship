//! Trusted platform policy observations, separate from creator execution storage.

use crate::Error;
use std::{fmt::Debug, future::Future, num::NonZeroU64, pin::Pin, sync::Arc, time::Instant};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{AssignedScope, Revision, WorkerId},
    workflow_policy::{AppPolicy, PolicyLease},
};

pub mod control;

/// A provider's immutable, finite observation of complete authoritative policy.
/// Cloning retains source validity; observing cached data must not renew it.
#[derive(Clone, Debug)]
pub struct PolicyObservation {
    identity: Arc<()>,
    app_id: AppId,
    revision: Revision,
    policy: AppPolicy,
    expires_at: Instant,
}

impl PolicyObservation {
    /// Construct only from a consistent, revisioned authoritative source.
    ///
    /// # Errors
    /// Invalid policy or exhausted source validity is an infrastructure failure,
    /// never a durable refusal attributed to creator input.
    pub fn new(
        app_id: AppId,
        revision: Revision,
        policy: AppPolicy,
        expires_at: Instant,
    ) -> Result<Self, Error> {
        policy.validate().map_err(|_| Error::Unavailable)?;
        if expires_at <= Instant::now() {
            return Err(Error::Unavailable);
        }
        Ok(Self {
            identity: Arc::new(()),
            app_id,
            revision,
            policy,
            expires_at,
        })
    }

    #[must_use]
    pub const fn app_id(&self) -> &AppId {
        &self.app_id
    }

    #[must_use]
    pub const fn revision(&self) -> Revision {
        self.revision
    }

    #[must_use]
    pub const fn policy(&self) -> &AppPolicy {
        &self.policy
    }

    #[must_use]
    pub const fn expires_at(&self) -> Instant {
        self.expires_at
    }

    /// Compare the exact source observation, independently of equal values or
    /// deadlines. Clones retain identity; a new authoritative observation does
    /// not revive an invalidated predecessor with the same content.
    #[must_use]
    pub fn same_observation(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.identity, &other.identity)
    }
}

/// Platform-owned policy authority; implementations never read creator storage.
///
/// The source revision covers every contributing value and its original validity
/// comes from an authoritative observation. Caches preserve that deadline.
/// Missing freshness, inconsistent revisions, revocation and source failures
/// return `Unavailable`. Disabled policy remains a successful observation.
pub trait PolicySource: Debug {
    /// Fetch outside manager app and worker locks. Repeated observations of
    /// cached values clone the retained observation and preserve its deadline.
    fn observe<'a>(
        &'a self,
        app: &'a AppId,
    ) -> Pin<Box<dyn Future<Output = Result<PolicyObservation, Error>> + 'a>>;

    /// Recheck the exact observation without blocking or performing source I/O.
    ///
    /// # Errors
    /// Reject any invalidated observation, including after a later restoration.
    /// A shortening can reduce validity; it must not later revive that retained
    /// observation. A newer valid observation requires a new issuing attempt.
    fn revalidate(&self, observation: &PolicyObservation) -> Result<Instant, Error>;
}

/// An issued lease retains source authority through response construction.
/// There is no public constructor and no serde implementation for this handle.
/// The ingress epoch was committed before the grant was built.
#[derive(Clone, Debug)]
pub struct PolicyGrant<'a> {
    observation: PolicyObservation,
    source: &'a dyn PolicySource,
    worker_id: WorkerId,
    signing_key_id: String,
    assignment_revision: Revision,
    ingress_epoch: Option<Revision>,
    expires_at: Instant,
}

impl<'a> PolicyGrant<'a> {
    pub(crate) fn new(
        observation: PolicyObservation,
        source: &'a dyn PolicySource,
        worker_id: WorkerId,
        signing_key_id: String,
        scope: &AssignedScope,
        ingress_epoch: Option<Revision>,
        expires_at: Instant,
    ) -> Self {
        Self {
            observation,
            source,
            worker_id,
            signing_key_id,
            assignment_revision: scope.assignment_revision,
            ingress_epoch,
            expires_at,
        }
    }

    /// The open or closing ingress epoch this grant carries, if any.
    #[must_use]
    pub const fn ingress_epoch(&self) -> Option<Revision> {
        self.ingress_epoch
    }

    /// Convert after transaction settlement, charging all intervening waits.
    ///
    /// # Errors
    /// Refuses unavailable source authority or exhausted remaining validity.
    pub fn lease(&self) -> Result<PolicyLease, Error> {
        let expires_at = self
            .expires_at
            .min(source_deadline(self.source, &self.observation)?);
        let remaining_ms = u64::try_from(
            expires_at
                .saturating_duration_since(Instant::now())
                .as_millis(),
        )
        .ok()
        .and_then(NonZeroU64::new)
        .ok_or(Error::Timeout)?;
        Ok(PolicyLease {
            app_id: self.observation.app_id.clone(),
            worker_id: self.worker_id.clone(),
            signing_key_id: self.signing_key_id.clone(),
            assignment_revision: self.assignment_revision,
            policy_revision: self.observation.revision,
            policy: self.observation.policy.clone(),
            ingress_epoch: self.ingress_epoch,
            remaining_ms,
        })
    }
}

pub(crate) fn source_deadline(
    source: &dyn PolicySource,
    observation: &PolicyObservation,
) -> Result<Instant, Error> {
    let expires_at = source
        .revalidate(observation)
        .map_err(|_| Error::Unavailable)?
        .min(observation.expires_at);
    if expires_at <= Instant::now() {
        return Err(Error::Unavailable);
    }
    Ok(expires_at)
}
