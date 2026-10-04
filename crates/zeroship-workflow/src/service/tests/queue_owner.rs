//! One enrolled worker acting on one app through the manager queue.
//!
//! The manager's worker claim is `Coordinator::claim_in_zone`, which pages a
//! zone. These contracts drive one app's delivery against the journal, so they
//! call the queue's single-app authorized operations directly, always as the
//! worker the owner names.

use std::future::ready;
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::WorkerId,
    workflow_jobs::{Delivery, JournalSettlement, SettlementReceipt},
    workflow_policy::AppPolicy,
};
use zeroship_workflow_manager::{Claimant, DeliveryGrant, Error, Queue};

/// The worker a case claims, renews and settles as.
#[derive(Clone, Debug)]
pub(in crate::service) struct Owner {
    pub app_id: AppId,
    pub worker_id: WorkerId,
}

impl Owner {
    /// A freshly minted worker acting on `app`.
    pub(in crate::service) fn new(app: &AppId) -> Self {
        Self {
            app_id: app.clone(),
            worker_id: WorkerId::mint(),
        }
    }
}

/// Queue calls a case makes as one owner.
pub(in crate::service) trait QueueCalls {
    /// Claim the owner's app's next ready job.
    async fn claim(&self, owner: &Owner) -> Result<Option<DeliveryGrant>, Error>;

    /// Renew a delivery the owner holds.
    async fn heartbeat(&self, owner: &Owner, delivery: &Delivery) -> Result<DeliveryGrant, Error>;

    /// Settle a delivery the owner holds with the journal's settlement.
    async fn settle(
        &self,
        owner: &Owner,
        settlement: &JournalSettlement,
    ) -> Result<SettlementReceipt, Error>;
}

impl QueueCalls for Queue {
    async fn claim(&self, owner: &Owner) -> Result<Option<DeliveryGrant>, Error> {
        let worker = owner.worker_id.clone();
        self.claim_authorized(
            &owner.app_id,
            &owner.worker_id,
            Claimant::Worker,
            Ok(AppPolicy::default().max_delivery_attempts),
            |_| ready(Ok(worker.clone())),
        )
        .await
    }

    async fn heartbeat(&self, owner: &Owner, delivery: &Delivery) -> Result<DeliveryGrant, Error> {
        let worker = owner.worker_id.clone();
        self.heartbeat_authorized(&owner.worker_id, delivery, |_| ready(Ok(worker.clone())))
            .await
    }

    async fn settle(
        &self,
        owner: &Owner,
        settlement: &JournalSettlement,
    ) -> Result<SettlementReceipt, Error> {
        let worker = owner.worker_id.clone();
        self.settle_authorized(
            &owner.worker_id,
            settlement,
            |_| ready(Ok(worker.clone())),
            |_| ready(Ok(worker.clone())),
        )
        .await
    }
}
