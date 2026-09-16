//! Durable publication and hold intents delivered by the manager for recovery.

#![expect(
    clippy::future_not_send,
    reason = "reconciliation owns compio-local ORM operations"
)]

use super::{
    app::{decode, encode, lock_app_state},
    delivery::{self, CapturedLease, JobReceipt},
    models::{app_state, job_receipts, reconciliation_pages},
    publication::JobPublisher,
    store::Transaction,
    AppWorkflows,
};
use crate::WorkflowServiceError;
use std::time::{Duration, Instant};
use zeroship_core::workflow_jobs::{JobId, JobLease, JobOperation, JobOutcome, JobSpec};
use zeroship_data_orm::{
    orm::{Entity, Operation, Output},
    value,
};

/// Bounds a delivered page; the original manager and policy leases bound its total work.
#[derive(Debug, Clone, Copy)]
pub struct ReconciliationOptions {
    pub page_size: u32,
    pub item_timeout: Duration,
}
impl Default for ReconciliationOptions {
    fn default() -> Self {
        Self {
            page_size: 64,
            item_timeout: Duration::from_millis(500),
        }
    }
}
impl ReconciliationOptions {
    pub(super) fn validate(self) -> Result<(), WorkflowServiceError> {
        if self.page_size == 0
            || i64::from(self.page_size) > zeroship_data_orm::sql::MAX_ROW_LIMIT
            || self.item_timeout.is_zero()
            || Instant::now().checked_add(self.item_timeout).is_none()
        {
            return Err(WorkflowServiceError::InvalidRequest(
                "invalid workflow reconciliation bounds".into(),
            ));
        }
        Ok(())
    }
}

mod scan;
use scan::{pending_ids, scan, Page, Phase, Plan};

enum Admission {
    Page(Plan),
    Settled(JobReceipt),
}
enum Progress {
    Item(String),
    Settled(JobReceipt),
}

impl AppWorkflows {
    /// Visit a persisted page of this app's publication or hold intents.
    ///
    /// Failed items remain pending for a later scan. This operation runs no app code and does
    /// not interpret a completed scan as proof that the app has drained.
    ///
    /// # Errors
    /// Refuses foreign scope, unsupported jobs, exhausted authority and storage
    /// failures. A lost response is retried with the same job identity and plan.
    pub async fn reconcile_job(
        &self,
        grant: &impl JobLease,
        publisher: &impl JobPublisher,
        options: ReconciliationOptions,
    ) -> Result<JobReceipt, WorkflowServiceError> {
        options.validate()?;
        let job = &grant.delivery().job;
        delivery::check_scope(self.app_id(), job)?;
        if publisher.app_id() != self.app_id() {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        if !matches!(job.operation, JobOperation::Reconcile {}) {
            return Err(WorkflowServiceError::InvalidRequest(
                "expected workflow reconciliation job".into(),
            ));
        }
        let authority = CapturedLease::capture(self, grant);
        let budget = delivery::attempt_budget(authority.as_ref().ok(), None);
        delivery::run_attempt(
            authority.as_ref().ok().map(CapturedLease::cancelled),
            budget,
            Box::pin(async {
                let plan = match self
                    .prepare_reconciliation(job, authority.as_ref(), options)
                    .await?
                {
                    Admission::Settled(receipt) => return Ok(receipt),
                    Admission::Page(plan) => plan,
                };
                let authority = authority?;
                loop {
                    let id = match self
                        .next_reconciliation_item(job, &plan, &authority)
                        .await?
                    {
                        Progress::Settled(receipt) => return Ok(receipt),
                        Progress::Item(id) => id,
                    };
                    let result = async {
                        self.reconcile_item(plan.phase, &id, publisher, &authority)
                            .await
                    };
                    let timeout = options.item_timeout.min(delivery::remaining(&authority)?);
                    match compio::time::timeout(timeout, Box::pin(result)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => tracing::warn!(
                            code = error.code(),
                            "workflow reconciliation intent remains pending"
                        ),
                        Err(_) => tracing::warn!("workflow reconciliation intent timed out"),
                    }
                }
            }),
        )
        .await
    }

    async fn reconcile_item(
        &self,
        phase: Phase,
        id: &str,
        publisher: &impl JobPublisher,
        authority: &CapturedLease,
    ) -> Result<(), WorkflowServiceError> {
        authority.check(self)?;
        match phase {
            Phase::Publications => {
                let id = JobId::parse(id).map_err(|_| invalid())?;
                self.publish_job_authorized(&id, publisher, Some(authority))
                    .await?;
            }
            Phase::DeploymentHolds => {
                let source = self
                    .service
                    .deployments
                    .as_ref()
                    .ok_or_else(super::deployments::unavailable)?;
                let client = source.client(self.app_id())?;
                // The page selects existing intents. This operation revalidates
                // their current generation and only confirms the matching RPC;
                // it never decides to acquire or release a deployment.
                self.service
                    .reconcile_deployment_hold_checked(
                        self.app_id(),
                        id,
                        None,
                        None,
                        client.as_ref(),
                        &|| authority.check(self),
                    )
                    .await?;
            }
        }
        Ok(())
    }

    async fn prepare_reconciliation(
        &self,
        job: &JobSpec,
        authority: Result<&CapturedLease, &WorkflowServiceError>,
        options: ReconciliationOptions,
    ) -> Result<Admission, WorkflowServiceError> {
        let mut tx = self.service.begin().await?;
        lock_app_state(&mut tx, self.app_id()).await?;
        let stored = read_page(&tx, job).await?;
        if let Some((record, _)) = &stored {
            if let Some(receipt) = record.receipt(job)? {
                tx.commit().await?;
                return Ok(Admission::Settled(receipt));
            }
        }
        let authority = authority.map_err(Clone::clone)?;
        authority.check(self)?;
        let plan = if let Some((_, page)) = stored {
            let plan: Plan = decode(&page.plan)?;
            plan.validate()?;
            plan
        } else {
            let scan = scan(&tx, self.app_id().as_str()).await?;
            if scan.reconciliation_revision <= 0
                || scan.reconciliation_after_id.as_ref().is_some_and(|after| {
                    scan.reconciliation_upper_id
                        .as_ref()
                        .is_none_or(|upper| after >= upper)
                })
            {
                return Err(invalid());
            }
            let phase = Phase::parse(&scan.reconciliation_phase)?;
            let previous_upper = scan.reconciliation_upper_id.clone();
            let upper = match scan.reconciliation_upper_id {
                Some(upper) => Some(upper),
                None => pending_ids(&tx, self.app_id().as_str(), phase, None, None, 1, true)
                    .await?
                    .into_iter()
                    .next(),
            };
            let ids = if let Some(upper) = &upper {
                pending_ids(
                    &tx,
                    self.app_id().as_str(),
                    phase,
                    scan.reconciliation_after_id.as_deref(),
                    Some(upper),
                    options.page_size,
                    false,
                )
                .await?
            } else {
                Vec::new()
            };
            let more = ids.len() == options.page_size as usize
                && ids
                    .last()
                    .is_some_and(|last| upper.as_ref().is_some_and(|upper| last < upper));
            let plan = Plan {
                phase,
                revision: scan.reconciliation_revision,
                after: scan.reconciliation_after_id,
                previous_upper,
                upper,
                ids,
                more,
            };
            plan.validate()?;
            let now = tx.now().await?;
            tx.database().collection(job_receipts::Entity::COLLECTION)?.insert(value!({
                "id":job.id.as_str(), "app_id":self.app_id().as_str(), "specification":encode(job)?, "created_at":now,
            })).await?;
            tx.database().collection(reconciliation_pages::Entity::COLLECTION)?.insert(value!({
                "id":job.id.as_str(), "app_id":self.app_id().as_str(), "plan":encode(&plan)?, "next_index":0,
            })).await?;
            plan
        };
        authority.check(self)?;
        tx.commit().await?;
        authority.check(self)?;
        Ok(Admission::Page(plan))
    }

    async fn next_reconciliation_item(
        &self,
        job: &JobSpec,
        plan: &Plan,
        authority: &CapturedLease,
    ) -> Result<Progress, WorkflowServiceError> {
        let mut tx = self.service.begin().await?;
        lock_app_state(&mut tx, self.app_id()).await?;
        let (record, page) = read_page(&tx, job).await?.ok_or_else(invalid)?;
        if let Some(receipt) = record.receipt(job)? {
            tx.commit().await?;
            return Ok(Progress::Settled(receipt));
        }
        authority.check(self)?;
        if decode::<Plan>(&page.plan)? != *plan {
            return Err(invalid());
        }
        let index = usize::try_from(page.next_index).map_err(|_| invalid())?;
        if index > plan.ids.len() {
            return Err(invalid());
        }
        let current = scan(&tx, self.app_id().as_str()).await?;
        plan.check_scan(&current)?;
        let progress = if let Some(id) = plan.ids.get(index) {
            // Reserve before I/O. Cancellation may skip this attempt, but the
            // intent stays pending for the next sweep; redelivery reaches its suffix.
            delivery::reserve::<reconciliation_pages::Entity>(&tx, job, index, invalid).await?;
            Progress::Item(id.clone())
        } else {
            if current.reconciliation_revision == plan.revision {
                let revision = current
                    .reconciliation_revision
                    .checked_add(1)
                    .ok_or_else(invalid)?;
                let after = if plan.more {
                    plan.ids.last().cloned()
                } else {
                    None
                };
                let upper = if plan.more { plan.upper.clone() } else { None };
                let phase = if plan.more {
                    plan.phase
                } else {
                    plan.phase.next()
                };
                // Scoping by app and matching the revision this page read is what
                // makes a lost update impossible: a concurrent advance moved the
                // revision, so it matches no row and the zero-row result fails
                // the attempt.
                let changed = tx
                    .database()
                    .collection(app_state::Entity::COLLECTION)?
                    .execute(Operation::Update {
                        filter: value!({"app_id":self.app_id().as_str(), "reconciliation_revision":plan.revision}),
                        patch: value!({"reconciliation_revision":revision, "reconciliation_phase":phase.as_str(), "reconciliation_after_id":after, "reconciliation_upper_id":upper}),
                        many: true,
                    })
                    .await?;
                changed_once(&changed)?;
            }
            let now = tx.now().await?;
            Progress::Settled(
                delivery::finish(
                    &tx,
                    job,
                    if plan.more || plan.phase == Phase::Publications {
                        JobOutcome::Waiting {}
                    } else {
                        JobOutcome::Completed {}
                    },
                    now,
                )
                .await?,
            )
        };
        authority.check(self)?;
        tx.commit().await?;
        if matches!(progress, Progress::Item(_)) {
            authority.check(self)?;
        }
        Ok(progress)
    }
}

/// Replay a delivered page's committed outcome. The page is read with the
/// receipt, so a settled reconciliation that has lost its page reports a
/// damaged journal rather than an outcome.
pub(super) async fn receipt(
    tx: &Transaction,
    job: &JobSpec,
) -> Result<Option<JobReceipt>, WorkflowServiceError> {
    match read_page(tx, job).await? {
        Some((record, _)) => record.receipt(job),
        None => Ok(None),
    }
}

/// Read the receipt together with the page it is settling. The page carries the
/// job's own identity, scoped by the app, so a row naming anything else is a
/// damaged journal rather than another app's page.
async fn read_page(
    tx: &Transaction,
    job: &JobSpec,
) -> Result<Option<(delivery::Record, Page)>, WorkflowServiceError> {
    let stored =
        delivery::read_extended::<reconciliation_pages::Entity, Page>(tx, job, invalid).await?;
    if let Some((_, page)) = &stored {
        if page.id != job.id.as_str() || page.app_id != job.app_id.as_str() {
            return Err(invalid());
        }
    }
    Ok(stored)
}

fn changed_once(output: &Output) -> Result<(), WorkflowServiceError> {
    if matches!(output, Output::Count(1)) {
        Ok(())
    } else {
        Err(invalid())
    }
}
fn invalid() -> WorkflowServiceError {
    WorkflowServiceError::Internal("invalid workflow reconciliation journal".into())
}
