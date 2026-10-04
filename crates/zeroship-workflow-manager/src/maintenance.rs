//! The authority a process asserts to sweep its own queue's maintenance rows.
#![expect(
    clippy::future_not_send,
    reason = "maintenance authority shares the queue's owning compio runtime"
)]

use crate::{models::Claimant, queue::DeliveryGrant, Error, Queue};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::WorkerId,
    workflow_jobs::{JournalSettlement, SettlementReceipt},
};

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
        queue
            .claim_authorized(
                &self.app,
                &self.identity,
                Claimant::Maintenance,
                max_delivery_attempts,
                |_| std::future::ready(Ok(self.identity.clone())),
            )
            .await
    }

    /// Discharge a delivery this authority claimed.
    ///
    /// # Errors
    /// Refuses a delivery leased by another identity, a lapsed lease, conflicting
    /// settlements and failed queue transactions.
    pub async fn settle(
        &self,
        queue: &Queue,
        settlement: &JournalSettlement,
    ) -> Result<SettlementReceipt, Error> {
        queue
            .settle_authorized(
                &self.identity,
                settlement,
                |_| std::future::ready(Ok(self.identity.clone())),
                |_| std::future::ready(Ok(self.identity.clone())),
            )
            .await
    }
}
