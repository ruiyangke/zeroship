//! Prompt publication of committed creator intents by the workflow host.
//!
//! A request-path mutation or a settled delivery can leave publication intents
//! in an app's journal. Marking the app wakes the host thread, which submits
//! them under the app's current assignment. Marks from any number of threads
//! coalesce per app, so a burst of hints costs one pass. A failed or missed
//! pass leaves the intents pending for the manager's reconciliation job, which
//! remains the authority for eventual publication.

#![expect(
    clippy::future_not_send,
    reason = "publication runs on the host's compio thread"
)]

use super::delivery::{bounded, JobTransport};
use crate::{
    service::{publication::JobPublisher, AppWorkflows, CommitHint},
    WorkflowServiceError,
};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::AssignedScope,
    workflow_jobs::{JobSpec, Settlement, SettlementReceipt},
};
use zeroship_workflow_client::{LeasedJob, WorkerCoordinator};

const PAGE: u32 = 64;

/// Marks apps whose committed intents the host should publish. `Send`, so
/// request threads can hold it inside their backend's commit hint.
#[derive(Clone)]
pub(crate) struct PublicationWake {
    pending: Arc<Mutex<BTreeSet<AppId>>>,
    signal: flume::Sender<()>,
}

/// The host thread's end of [`PublicationWake`].
pub(crate) struct PublicationWait {
    pending: Arc<Mutex<BTreeSet<AppId>>>,
    signal: flume::Receiver<()>,
}

pub(crate) fn channel() -> (PublicationWake, PublicationWait) {
    let pending = Arc::new(Mutex::new(BTreeSet::new()));
    // One buffered signal is enough: the set carries every marked app.
    let (signal, receiver) = flume::bounded(1);
    (
        PublicationWake {
            pending: pending.clone(),
            signal,
        },
        PublicationWait {
            pending,
            signal: receiver,
        },
    )
}

impl PublicationWake {
    pub(crate) fn mark(&self, app: &AppId) {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(app.clone());
        // A full channel already holds a wake that will observe this mark.
        let _ = self.signal.try_send(());
    }

    /// A hint for `app`'s backend. It carries no customer data and grants
    /// nothing: the host publishes only intents already in the journal.
    pub(crate) fn hint(&self, app: AppId) -> CommitHint {
        let wake = self.clone();
        Arc::new(move || wake.mark(&app))
    }
}

impl PublicationWait {
    /// Wait until at least one app is marked, then take every marked app.
    pub(crate) async fn next(&self) -> BTreeSet<AppId> {
        loop {
            let marked = std::mem::take(
                &mut *self.pending.lock().unwrap_or_else(PoisonError::into_inner),
            );
            if !marked.is_empty() {
                return marked;
            }
            if self.signal.recv_async().await.is_err() {
                // Every sender is gone, so nothing can be marked again.
                std::future::pending::<()>().await;
            }
        }
    }
}

/// Submit an app's pending intents under its current assignment, each
/// exchange bounded by `timeout`. Stops at the first failure; the remainder
/// stays pending for the next pass or the manager's reconciliation.
pub(crate) async fn publish_pending(
    api: &AppWorkflows,
    publisher: &impl JobPublisher,
    timeout: Duration,
) -> Result<(), WorkflowServiceError> {
    let mut after = None;
    loop {
        let page = bounded(timeout, api.pending_jobs(after.as_ref(), PAGE)).await?;
        for job in &page {
            bounded(timeout, api.publish_job(&job.id, publisher)).await?;
        }
        if page.len() < PAGE as usize {
            return Ok(());
        }
        after = page.last().map(|job| job.id.clone());
    }
}

/// The authenticated manager transport of a host's consumer. A settled
/// delivery may have committed successor intents, so settlement marks its app.
#[derive(Clone)]
pub(crate) struct HostTransport {
    pub(crate) client: WorkerCoordinator,
    pub(crate) settled: PublicationWake,
}

impl JobTransport for HostTransport {
    type Lease = LeasedJob;

    async fn claim(
        &self,
        scope: &AssignedScope,
    ) -> Result<Option<LeasedJob>, WorkflowServiceError> {
        JobTransport::claim(&self.client, scope).await
    }

    async fn submit(
        &self,
        scope: &AssignedScope,
        job: &JobSpec,
    ) -> Result<JobSpec, WorkflowServiceError> {
        JobTransport::submit(&self.client, scope, job).await
    }

    async fn heartbeat(&self, lease: &LeasedJob) -> Result<LeasedJob, WorkflowServiceError> {
        JobTransport::heartbeat(&self.client, lease).await
    }

    async fn settle(
        &self,
        settlement: &Settlement,
    ) -> Result<SettlementReceipt, WorkflowServiceError> {
        let receipt = JobTransport::settle(&self.client, settlement).await?;
        self.settled.mark(&settlement.delivery.job.app_id);
        Ok(receipt)
    }
}
