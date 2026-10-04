//! Trusted platform policy observations, separate from creator execution storage.

use crate::Error;
use std::{fmt::Debug, future::Future, pin::Pin, sync::Arc, time::Instant};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::Revision,
    workflow_policy::AppPolicy,
    zone_id::ZoneId,
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
    /// The app's execution zone, frozen when Control created it. Retained from
    /// the same source read that produced the policy; it does not move a
    /// revision because it cannot change.
    execution_zone_id: ZoneId,
    /// `deleted_at` is set. Retained from the same source read; a deletion
    /// follows an archive, which already advanced the revision, so it moves no
    /// revision either.
    deleted: bool,
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
        execution_zone_id: ZoneId,
        deleted: bool,
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
            execution_zone_id,
            deleted,
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

    /// The app's frozen execution zone.
    #[must_use]
    pub const fn execution_zone_id(&self) -> &ZoneId {
        &self.execution_zone_id
    }

    /// Terminal deletion, read beside the policy it does not change.
    #[must_use]
    pub const fn deleted(&self) -> bool {
        self.deleted
    }

    /// Whether the run and claim paths may serve `zone` this app.
    ///
    /// A deleted app is refused to every zone: deletion is terminal and has no
    /// successor to hand the call to. A live app is served only by its own
    /// frozen execution zone. The caller passes a zone it has already taken
    /// from a verified credential; this type does not enforce that and cannot,
    /// so both call sites name `VerifiedWorker::zone`.
    ///
    /// # Errors
    /// `Denied` for a deleted app or a foreign zone.
    pub fn admits_zone(&self, zone: &ZoneId) -> Result<(), Error> {
        if self.deleted || &self.execution_zone_id != zone {
            return Err(Error::Denied);
        }
        Ok(())
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
}
