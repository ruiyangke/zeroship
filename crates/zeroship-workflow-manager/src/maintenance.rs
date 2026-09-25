//! The authority a process asserts to sweep its own queue's maintenance rows.
#![expect(
    clippy::future_not_send,
    reason = "maintenance authority shares the queue's owning compio runtime"
)]

use crate::{models::Claimant, queue::DeliveryGrant, Error, Queue};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{Assignment, Revision, WorkerId},
    workflow_jobs::{Settlement, SettlementReceipt},
};

/// The revision every asserted authority carries.
///
/// Nothing orders these against each other: an asserted authority is not a
/// placement, so there is no newer one to supersede it. It is constant so that
/// a claim and the settlement that follows present the same identity to
/// `Queue::settle`, which compares the two.
const ASSERTED: i64 = 1;

/// One process's claim on the maintenance rows of one app's queue.
///
/// Hold one per app and reuse it: the delivery it claims and the settlement
/// that discharges the delivery are compared on the identity this carries.
#[derive(Clone, Debug)]
pub struct MaintenanceAuthority {
    app: AppId,
    identity: WorkerId,
}

impl MaintenanceAuthority {
    /// `identity` is the process's own, and appears on every row it leases.
    #[must_use]
    pub const fn new(app: AppId, identity: WorkerId) -> Self {
        Self { app, identity }
    }

    #[must_use]
    pub const fn app(&self) -> &AppId {
        &self.app
    }

    #[must_use]
    pub const fn identity(&self) -> &WorkerId {
        &self.identity
    }

    /// Take the next maintenance row of this app's queue, if it has one.
    ///
    /// # Errors
    /// Refuses an unreadable or invalid ceiling, an unreachable database clock,
    /// exhausted attempt numbering and failed queue transactions.
    pub async fn claim(
        &self,
        queue: &Queue,
        max_delivery_attempts: Result<i64, Error>,
    ) -> Result<Option<DeliveryGrant>, Error> {
        let authority = self.asserted(queue).await?;
        queue
            .claim_authorized(
                &(&authority).into(),
                Claimant::Maintenance,
                max_delivery_attempts,
                |_| std::future::ready(Ok(authority.clone())),
            )
            .await
    }

    /// Discharge a delivery this authority claimed.
    ///
    /// # Errors
    /// Refuses a delivery leased by another identity, a lapsed lease, conflicting
    /// successors and failed queue transactions.
    pub async fn settle(
        &self,
        queue: &Queue,
        settlement: &Settlement,
    ) -> Result<SettlementReceipt, Error> {
        queue.settle(&self.asserted(queue).await?, settlement).await
    }

    /// State the authority this process claims with. THE DECISION POINT: the
    /// authority is asserted by the process that owns the queue, not read from
    /// a placement, and this is the only place that says so.
    ///
    /// A placement authorizes a REMOTE identity to execute creator code for one
    /// app, and it is read at ingress for a caller whose own assertion cannot be
    /// taken. A maintenance lane is neither remote nor an executor of creator
    /// code, and the process it runs in already operates this queue without a
    /// placement at a wider scope than a claim: the recovery lane inserts jobs,
    /// `Queue::submit` takes no authorizer, and the placement lane writes
    /// `assignments` itself. None of those take an instance lease or hold an
    /// election; they are deconflicted by the per-app lock and by each
    /// operation's own compare-and-swap, and a lane that leases a maintenance
    /// row is the same kind of thing.
    ///
    /// THE ALTERNATIVE was to give the lane a recorded placement identity, so
    /// that `assignments` answered which process is touching an app. It is not
    /// taken because `assignments` does not answer that and is not meant to -
    /// the recovery, retention and closing lanes all act on an app without ever
    /// appearing in it. It answers which worker may execute creator code there,
    /// and admission reads a zone match, a live instance, a ready registration
    /// and spare capacity, none of which a non-worker claimant has.
    ///
    /// Reversing the decision replaces this function's body with a placement
    /// read and leaves its callers alone.
    async fn asserted(&self, queue: &Queue) -> Result<Assignment, Error> {
        // The queue's own clock, because the expiry is compared against it, and
        // the queue's configured lease, so an asserted authority grants the same
        // window a placed one does rather than a number of its own.
        let now = queue.now().await?;
        let lease = i64::try_from(queue.lease().as_millis()).map_err(|_| Error::Invalid)?;
        Ok(Assignment {
            app_id: self.app.clone(),
            worker_id: self.identity.clone(),
            revision: Revision::try_from(ASSERTED).map_err(|_| Error::Invalid)?,
            expires_at: now
                .checked_add(lease)
                .ok_or(Error::Capacity)?
                .try_into()
                .map_err(|_| Error::Storage)?,
        })
    }
}
