use super::{Error, WorkerCoordinator};
use std::time::{Duration, Instant};
use zeroship_core::{
    app_id::AppId,
    service_identity::endpoints,
    workflow_coordination::{Revision, WorkerId},
    workflow_policy::{AppPolicy, PolicyLease, PolicyLeaseRequest},
};

/// Validated policy for an exact worker key and app assignment.
/// Cloning retains the original deadline; this handle cannot refresh a retired
/// host binding or replace its refresh ticket.
#[derive(Clone, Debug)]
pub struct LeasedPolicy {
    lease: PolicyLease,
    expires_at: Instant,
    anchor_slack: Duration,
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

    /// The manager's open or closing ingress epoch, absent once responsibility
    /// retired. Creator acceptance captures it beside the policy.
    #[must_use]
    pub const fn ingress_epoch(&self) -> Option<Revision> {
        self.lease.ingress_epoch
    }

    /// Install this original deadline; sampling remaining time and adding it
    /// to a later instant would extend authority.
    #[must_use]
    /// How far this deadline may under-state the granted expiry, measured as
    /// the round trip that produced it. A later lease whose deadline is earlier
    /// by no more than this is the SAME window re-anchored, not a shortening.
    #[must_use]
    pub const fn anchor_slack(&self) -> Duration {
        self.anchor_slack
    }

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
        // ANCHORED CONSERVATIVELY, AND THE ERROR IS RECORDED.
        //
        // The manager measured `remaining_ms` at some instant between this
        // request leaving and its reply arriving. Anchoring at `started` - the
        // earlier end - therefore UNDER-states the true expiry, which is the
        // safe direction: the host never believes it holds authority longer
        // than it was granted.
        //
        // The cost is that the same unchanged lease yields a slightly EARLIER
        // absolute deadline on every renewal, because the anchor moves forward
        // by one round trip each time. A reader comparing two such deadlines
        // sees a shortening that never happened. `anchor_slack` is the measured
        // width of that error, so the reader can tell an artefact of this
        // anchoring from a real reduction in authority.
        let anchor_slack = Instant::now().saturating_duration_since(started);
        let expires_at = started
            .checked_add(Duration::from_millis(lease.remaining_ms.get()))
            .ok_or(Error::InvalidResponse)?;
        let received = Self {
            lease,
            expires_at,
            anchor_slack,
        };
        received.remaining()?;
        Ok(received)
    }
}

impl WorkerCoordinator {
    /// Obtain policy under this client's enrolled key and current app assignment.
    /// The host must install it through the refresh ticket reserved before I/O.
    /// An establishment request is honored only by an epoch above the named one,
    /// or by any epoch when it names none.
    ///
    /// # Errors
    /// Refuses substituted identities, invalid or incomplete policy, an
    /// establishment reply without a newer epoch, failed exchanges and authority
    /// exhausted by transport or outside clock bounds.
    pub async fn policy_lease(&self, request: &PolicyLeaseRequest) -> Result<LeasedPolicy, Error> {
        let started = Instant::now();
        let lease: PolicyLease = self
            .transport
            .post(endpoints::WORKFLOW_POLICY_LEASE, request)
            .await?;
        if lease.app_id != request.scope.app_id
            || lease.worker_id != self.worker_id
            || lease.signing_key_id != self.signing_key_id
            || lease.assignment_revision != request.scope.assignment_revision
            || request.establish.is_some_and(|establish| {
                lease
                    .ingress_epoch
                    .is_none_or(|epoch| establish.after.is_some_and(|after| epoch <= after))
            })
        {
            return Err(Error::InvalidResponse);
        }
        LeasedPolicy::received(lease, started)
    }
}
