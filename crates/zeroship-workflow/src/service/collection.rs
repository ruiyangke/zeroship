//! Bounded collection of abandoned payload preparations in the assigned app.

#![expect(
    clippy::future_not_send,
    reason = "collection owns compio-local journal and object operations"
)]

use super::{
    AppWorkflows,
    app::{decode, encode, lock_app_state},
    delivery::{self, CapturedLease, JobReceipt},
    models::{collection_pages, collection_scans, job_receipts},
    payloads,
    store::Transaction,
};
use crate::WorkflowServiceError;
use std::time::{Duration, Instant};
use zeroship_core::workflow_jobs::{JobLease, JobOperation, JobOutcome, JobSpec};
use zeroship_data_orm::orm::{FindOptions, Insertable};

mod scan;
use scan::{Page, Plan};

/// Bounds a page and each reserved item; original policy and delivery bound the attempt.
#[derive(Debug, Clone, Copy)]
pub struct CollectionOptions {
    pub page_size: u32,
    pub item_timeout: Duration,
}
impl Default for CollectionOptions {
    fn default() -> Self {
        Self {
            page_size: 64,
            item_timeout: Duration::from_millis(500),
        }
    }
}
impl CollectionOptions {
    pub(super) fn validate(self) -> Result<(), WorkflowServiceError> {
        if self.page_size == 0
            || self.page_size as usize > payloads::MAX_COLLECTION_BATCH
            || i64::from(self.page_size) > zeroship_data_orm::sql::MAX_ROW_LIMIT
            || self.item_timeout.is_zero()
            || Instant::now().checked_add(self.item_timeout).is_none()
        {
            return Err(WorkflowServiceError::InvalidRequest(
                "invalid workflow collection bounds".into(),
            ));
        }
        Ok(())
    }
}

enum Stored {
    Missing,
    Pending { plan: Plan, next: usize },
    Settled(JobReceipt),
}
enum Admission {
    Page(Plan),
    Settled(JobReceipt),
}
enum Progress {
    Item(String),
    Settled(JobReceipt),
}

#[derive(Insertable)]
#[orm(entity = job_receipts)]
struct PendingReceipt {
    id: String,
    app_id: String,
    specification: String,
    created_at: i64,
}

impl AppWorkflows {
    /// Visit a durable page of abandoned payload preparations and deletion tombstones.
    /// A completed sweep does not imply that failed items or the app have drained.
    ///
    /// # Errors
    /// Refuses foreign or changed jobs, damaged page metadata, unavailable storage and
    /// exhausted authority. Committed receipts replay without live authority or storage.
    pub async fn collect_job(
        &self,
        grant: &impl JobLease,
        options: CollectionOptions,
    ) -> Result<JobReceipt, WorkflowServiceError> {
        options.validate()?;
        let job = &grant.delivery().job;
        delivery::check_scope(self.app_id(), job)?;
        if !matches!(job.operation, JobOperation::Collect {}) {
            return Err(WorkflowServiceError::InvalidRequest(
                "expected workflow collection job".into(),
            ));
        }
        let captured = CapturedLease::capture(self, grant)
            .and_then(|authority| Ok((authority.bind(self)?, authority)));
        let policy = captured
            .as_ref()
            .ok()
            .map(|(scope, _)| scope.capture_policy());
        let budget =
            delivery::attempt_budget(captured.as_ref().ok().map(|(_, authority)| authority), None);
        let operation = delivery::run_attempt(
            None,
            budget,
            Box::pin(async {
                let scope = captured.as_ref().map_or(self, |(scope, _)| scope);
                let plan = match Box::pin(scope.prepare_collection(
                    job,
                    captured.as_ref().map(|(_, authority)| authority),
                    options,
                ))
                .await?
                {
                    Admission::Settled(receipt) => return Ok(receipt),
                    Admission::Page(plan) => plan,
                };
                let (scope, authority) = captured?;
                loop {
                    let id =
                        match Box::pin(scope.next_collection_item(job, &plan, &authority)).await? {
                            Progress::Settled(receipt) => return Ok(receipt),
                            Progress::Item(id) => id,
                        };
                    let check = || authority.check(&scope);
                    let operation = scope.service.collect_payload_checked(
                        scope.app_id(),
                        &id,
                        plan.observed_at,
                        &check,
                    );
                    let timeout = options.item_timeout.min(delivery::remaining(&authority)?);
                    match compio::time::timeout(timeout, Box::pin(operation)).await {
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => tracing::warn!(
                            code = error.code(),
                            "workflow payload collection remains pending"
                        ),
                        Err(_) => tracing::warn!("workflow payload collection item timed out"),
                    }
                }
            }),
        );
        match policy {
            Some(policy) => policy.run(operation).await,
            None => operation.await,
        }
    }

    async fn prepare_collection(
        &self,
        job: &JobSpec,
        authority: Result<&CapturedLease, &WorkflowServiceError>,
        options: CollectionOptions,
    ) -> Result<Admission, WorkflowServiceError> {
        let mut tx = if authority.is_ok() {
            self.service.begin().await?
        } else {
            self.service.begin_history().await?
        };
        lock_app_state(&mut tx, self.app_id()).await?;
        let stored = inspect(&tx, job).await?;
        if let Stored::Settled(receipt) = stored {
            tx.commit().await?;
            return Ok(Admission::Settled(receipt));
        }
        let authority = authority.map_err(Clone::clone)?;
        authority.check(self)?;
        payloads::storage(&self.service)?;
        tx.capture_mutation(self.app_id())?;
        let plan = match stored {
            Stored::Pending { plan, .. } => plan,
            Stored::Missing => {
                let now = tx.now().await?;
                let scan = scan::initialize(&tx, self.app_id(), now).await?;
                let plan = scan::plan(&tx, self.app_id(), &scan, options).await?;
                let pending = tx
                    .database()
                    .entity::<job_receipts::Entity>()?
                    .insert::<_, delivery::Record>(PendingReceipt {
                        id: job.id.as_str().to_owned(),
                        app_id: self.app_id().as_str().to_owned(),
                        specification: encode(job)?,
                        created_at: now,
                    })
                    .await?;
                if pending.receipt(job)?.is_some() {
                    return Err(invalid());
                }
                let page = tx
                    .database()
                    .entity::<collection_pages::Entity>()?
                    .insert::<_, Page>(Page {
                        id: job.id.as_str().to_owned(),
                        app_id: self.app_id().as_str().to_owned(),
                        plan: encode(&plan)?,
                        next_index: 0,
                    })
                    .await?;
                if page.id != job.id.as_str()
                    || page.app_id != self.app_id().as_str()
                    || decode::<Plan>(&page.plan)? != plan
                    || page.next_index != 0
                {
                    return Err(invalid());
                }
                plan
            }
            Stored::Settled(_) => return Err(invalid()),
        };
        authority.check(self)?;
        tx.commit().await?;
        authority.check(self)?;
        Ok(Admission::Page(plan))
    }

    async fn next_collection_item(
        &self,
        job: &JobSpec,
        expected: &Plan,
        authority: &CapturedLease,
    ) -> Result<Progress, WorkflowServiceError> {
        let mut tx = self.service.begin().await?;
        lock_app_state(&mut tx, self.app_id()).await?;
        let stored = inspect(&tx, job).await?;
        if let Stored::Settled(receipt) = stored {
            tx.commit().await?;
            return Ok(Progress::Settled(receipt));
        }
        authority.check(self)?;
        tx.capture_mutation(self.app_id())?;
        let Stored::Pending { plan, next } = stored else {
            return Err(invalid());
        };
        if plan != *expected {
            return Err(invalid());
        }
        let progress = if let Some(id) = plan.ids.get(next) {
            changed_once(
                tx.database()
                    .entity::<collection_pages::Entity>()?
                    .update_many(
                        collection_pages::id
                            .eq(job.id.as_str())?
                            .and(collection_pages::app_id.eq(self.app_id().as_str())?)
                            .and(
                                collection_pages::next_index
                                    .eq(i64::try_from(next).map_err(|_| invalid())?)?,
                            ),
                        collection_pages::next_index
                            .set(i64::try_from(next + 1).map_err(|_| invalid())?)?,
                    )
                    .await?,
            )?;
            Progress::Item(id.clone())
        } else {
            Box::pin(advance(&tx, job, &plan)).await?;
            let now = tx.now().await?;
            Progress::Settled(delivery::finish(&tx, job, outcome(&plan), now).await?)
        };
        authority.check(self)?;
        tx.commit().await?;
        if matches!(progress, Progress::Item(_)) {
            authority.check(self)?;
        }
        Ok(progress)
    }
}

pub(super) async fn receipt(
    tx: &Transaction,
    job: &JobSpec,
) -> Result<Option<JobReceipt>, WorkflowServiceError> {
    match inspect(tx, job).await? {
        Stored::Settled(receipt) => Ok(Some(receipt)),
        _ => Ok(None),
    }
}

async fn inspect(tx: &Transaction, job: &JobSpec) -> Result<Stored, WorkflowServiceError> {
    if !matches!(job.operation, JobOperation::Collect {}) {
        return Err(invalid());
    }
    let record = delivery::read(tx, job).await?;
    let page = tx
        .database()
        .entity::<collection_pages::Entity>()?
        .find::<Page>(
            collection_pages::id
                .eq(job.id.as_str())?
                .and(collection_pages::app_id.eq(job.app_id.as_str())?),
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next();
    let (record, page) = match (record, page) {
        (None, None) => return Ok(Stored::Missing),
        (Some(record), Some(page)) => (record, page),
        _ => return Err(invalid()),
    };
    if page.id != job.id.as_str() || page.app_id != job.app_id.as_str() {
        return Err(invalid());
    }
    let plan: Plan = decode(&page.plan)?;
    plan.validate()?;
    let next = usize::try_from(page.next_index).map_err(|_| invalid())?;
    if next > plan.ids.len() {
        return Err(invalid());
    }
    let current = scan::read(tx, &job.app_id).await?.ok_or_else(invalid)?;
    plan.check_scan(&current)?;
    if let Some(receipt) = record.receipt(job)? {
        if next != plan.ids.len()
            || receipt.outcome != outcome(&plan)
            || current.revision <= plan.revision
        {
            return Err(invalid());
        }
        Ok(Stored::Settled(receipt))
    } else {
        Ok(Stored::Pending { plan, next })
    }
}

async fn advance(tx: &Transaction, job: &JobSpec, plan: &Plan) -> Result<(), WorkflowServiceError> {
    let current = scan::read(tx, &job.app_id).await?.ok_or_else(invalid)?;
    plan.check_scan(&current)?;
    if current.revision == plan.revision {
        let revision = current.revision.checked_add(1).ok_or_else(invalid)?;
        let after = if plan.more {
            plan.ids.last().map(String::as_str)
        } else {
            None
        };
        let upper = if plan.more {
            plan.upper.as_deref()
        } else {
            None
        };
        let observed_at = plan.more.then_some(plan.observed_at);
        changed_once(
            tx.database()
                .entity::<collection_scans::Entity>()?
                .update_many(
                    collection_scans::id
                        .eq(job.app_id.as_str())?
                        .and(collection_scans::revision.eq(plan.revision)?),
                    collection_scans::revision
                        .set(revision)?
                        .and(collection_scans::after_id.set(after)?)?
                        .and(collection_scans::upper_id.set(upper)?)?
                        .and(collection_scans::observed_at.set(observed_at)?)?,
                )
                .await?,
        )?;
    }
    Ok(())
}

const fn outcome(plan: &Plan) -> JobOutcome {
    if plan.more {
        JobOutcome::Waiting {}
    } else {
        JobOutcome::Completed {}
    }
}
fn changed_once(changed: i64) -> Result<(), WorkflowServiceError> {
    if changed == 1 { Ok(()) } else { Err(invalid()) }
}
fn invalid() -> WorkflowServiceError {
    WorkflowServiceError::Internal("invalid workflow collection journal".into())
}
