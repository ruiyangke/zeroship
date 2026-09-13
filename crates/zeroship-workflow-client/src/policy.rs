use super::{Error, WorkerCoordinator};
use std::time::{Duration, Instant};
use zeroship_core::{
    app_id::AppId,
    service_identity::endpoints,
    workflow_coordination::{AssignedScope, Revision, WorkerId},
    workflow_policy::{AppPolicy, PolicyLease},
};

/// Validated policy for an exact worker key and app assignment.
/// Cloning retains the original deadline; this handle cannot refresh a retired
/// host binding or replace its refresh ticket.
#[derive(Clone, Debug)]
pub struct LeasedPolicy {
    lease: PolicyLease,
    expires_at: Instant,
}

impl LeasedPolicy {
    #[must_use]
    pub const fn app_id(&self) -> &AppId {
        &self.lease.app_id
    }

    #[must_use]
    pub const fn worker_id(&self) -> &WorkerId {
        &self.lease.worker_id
    }

    #[must_use]
    pub fn signing_key_id(&self) -> &str {
        &self.lease.signing_key_id
    }

    #[must_use]
    pub const fn assignment_revision(&self) -> Revision {
        self.lease.assignment_revision
    }

    /// Immutable raw values identified by the source revision.
    #[must_use]
    pub const fn policy(&self) -> &AppPolicy {
        &self.lease.policy
    }

    #[must_use]
    pub const fn revision(&self) -> Revision {
        self.lease.policy_revision
    }

    /// Install this original deadline; sampling remaining time and adding it
    /// to a later instant would extend authority.
    #[must_use]
    pub const fn expires_at(&self) -> Instant {
        self.expires_at
    }

    /// # Errors
    /// Returns `Timeout` after the original monotonic grant expires.
    pub fn remaining(&self) -> Result<Duration, Error> {
        self.expires_at
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(Error::Timeout)
    }

    fn received(lease: PolicyLease, started: Instant) -> Result<Self, Error> {
        lease
            .policy
            .validate()
            .map_err(|_| Error::InvalidResponse)?;
        let remaining_ms =
            i64::try_from(lease.remaining_ms.get()).map_err(|_| Error::InvalidResponse)?;
        if remaining_ms > lease.policy.lease_ms {
            return Err(Error::InvalidResponse);
        }
        let expires_at = started
            .checked_add(Duration::from_millis(lease.remaining_ms.get()))
            .ok_or(Error::InvalidResponse)?;
        let received = Self { lease, expires_at };
        received.remaining()?;
        Ok(received)
    }
}

impl WorkerCoordinator {
    /// Obtain policy under this client's enrolled key and current app assignment.
    /// The host must install it through the refresh ticket reserved before I/O.
    ///
    /// # Errors
    /// Refuses substituted identities, invalid or incomplete policy, failed
    /// exchanges and authority exhausted by transport or outside clock bounds.
    pub async fn policy_lease(&self, scope: &AssignedScope) -> Result<LeasedPolicy, Error> {
        let started = Instant::now();
        let lease: PolicyLease = self
            .transport
            .post(endpoints::WORKFLOW_POLICY_LEASE, scope)
            .await?;
        if lease.app_id != scope.app_id
            || lease.worker_id != self.worker_id
            || lease.signing_key_id != self.signing_key_id
            || lease.assignment_revision != scope.assignment_revision
        {
            return Err(Error::InvalidResponse);
        }
        LeasedPolicy::received(lease, started)
    }
}
