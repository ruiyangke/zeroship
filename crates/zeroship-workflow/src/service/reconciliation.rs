//! Creator outbox pages delivered by the manager, without local scheduling.

#![expect(
    clippy::future_not_send,
    reason = "reconciliation owns compio-local ORM operations"
)]

use super::{
    app::{decode, encode, lock_app_state},
    delivery::{self, CapturedLease, JobReceipt},
    models::{job_publications, job_receipts, publication_scans},
    publication::JobPublisher,
    store::Transaction,
    AppWorkflows,
};
use crate::WorkflowServiceError;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use zeroship_core::workflow_jobs::{JobId, JobLease, JobOperation, JobOutcome, JobSpec};
use zeroship_data_orm::{
    orm::{Entity, FindOptions, FromRow, Operation, Output},
    sql::Predicate,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    revision: i64,
    after: Option<String>,
    upper: Option<String>,
    ids: Vec<String>,
    more: bool,
}
impl Plan {
    fn validate(&self) -> Result<(), WorkflowServiceError> {
        if self.revision <= 0
            || i64::try_from(self.ids.len()).map_err(|_| invalid())?
                > zeroship_data_orm::sql::MAX_ROW_LIMIT
            || self
                .after
                .as_ref()
                .is_some_and(|after| self.upper.as_ref().is_none_or(|upper| after >= upper))
        {
            return Err(invalid());
        }
        let mut previous = self.after.as_deref();
        for id in &self.ids {
            if previous.is_some_and(|after| after >= id.as_str())
                || self.upper.as_ref().is_none_or(|upper| id > upper)
            {
                return Err(invalid());
            }
            previous = Some(id);
        }
        if self.more
            && self
                .ids
                .last()
                .is_none_or(|last| self.upper.as_ref().is_none_or(|upper| last >= upper))
        {
            return Err(invalid());
        }
        Ok(())
    }
}

#[derive(FromRow)]
#[orm(entity = publication_scans)]
struct Scan {
    revision: i64,
    after_job: Option<String>,
    upper_job: Option<String>,
}

#[derive(FromRow)]
#[orm(entity = job_publications)]
struct PublicationId {
    id: String,
}

enum Admission {
    Page(Plan),
    Settled(JobReceipt),
}
enum Progress {
    Item(String),
    Settled(JobReceipt),
}

impl AppWorkflows {
    /// Visit a persisted page of this app's publication intents. Failed items
    /// remain pending for a later scan. This operation runs no app code and does
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
                        let id = JobId::parse(&id).map_err(|_| invalid())?;
                        self.publish_job_authorized(&id, publisher, Some(&authority))
                            .await
                    };
                    let timeout = options.item_timeout.min(delivery::remaining(&authority)?);
                    match compio::time::timeout(timeout, Box::pin(result)).await {
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => tracing::warn!(
                            code = error.code(),
                            "workflow reconciliation publication remains pending"
                        ),
                        Err(_) => tracing::warn!("workflow reconciliation publication timed out"),
                    }
                }
            }),
        )
        .await
    }

    async fn prepare_reconciliation(
        &self,
        job: &JobSpec,
        authority: Result<&CapturedLease, &WorkflowServiceError>,
        options: ReconciliationOptions,
    ) -> Result<Admission, WorkflowServiceError> {
        let mut tx = self.service.begin().await?;
        lock_app_state(&mut tx, self.app_id()).await?;
        let record = delivery::read(&tx, job).await?;
        if let Some(record) = &record {
            if let Some(receipt) = record.receipt(job)? {
                tx.commit().await?;
                return Ok(Admission::Settled(receipt));
            }
        }
        let authority = authority.map_err(Clone::clone)?;
        authority.check(self)?;
        let plan = if let Some(record) = record {
            let plan: Plan = decode(record.reconciliation.as_deref().ok_or_else(invalid)?)?;
            plan.validate()?;
            plan
        } else {
            let scan = if let Some(scan) = scan(&tx, self.app_id().as_str()).await? {
                scan
            } else {
                tx.database()
                    .collection(publication_scans::Entity::COLLECTION)?
                    .insert(value!({"id":self.app_id().as_str(), "revision":1}))
                    .await?;
                Scan {
                    revision: 1,
                    after_job: None,
                    upper_job: None,
                }
            };
            if scan.revision <= 0
                || scan
                    .after_job
                    .as_ref()
                    .is_some_and(|after| scan.upper_job.as_ref().is_none_or(|upper| after >= upper))
            {
                return Err(invalid());
            }
            let upper = match scan.upper_job {
                Some(upper) => Some(upper),
                None => pending_ids(&tx, self.app_id().as_str(), None, None, 1, true)
                    .await?
                    .into_iter()
                    .next(),
            };
            let ids = if let Some(upper) = &upper {
                pending_ids(
                    &tx,
                    self.app_id().as_str(),
                    scan.after_job.as_deref(),
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
                revision: scan.revision,
                after: scan.after_job,
                upper,
                ids,
                more,
            };
            plan.validate()?;
            let now = tx.now().await?;
            tx.database().collection(job_receipts::Entity::COLLECTION)?.insert(value!({
                "id":job.id.as_str(), "app_id":self.app_id().as_str(), "specification":encode(job)?, "reconciliation":encode(&plan)?, "reconciliation_next":0, "created_at":now,
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
        let record = delivery::read(&tx, job).await?.ok_or_else(invalid)?;
        if let Some(receipt) = record.receipt(job)? {
            tx.commit().await?;
            return Ok(Progress::Settled(receipt));
        }
        authority.check(self)?;
        if decode::<Plan>(record.reconciliation.as_deref().ok_or_else(invalid)?)? != *plan {
            return Err(invalid());
        }
        let index = usize::try_from(record.reconciliation_next.ok_or_else(invalid)?)
            .map_err(|_| invalid())?;
        if index > plan.ids.len() {
            return Err(invalid());
        }
        let progress = if let Some(id) = plan.ids.get(index) {
            // Reserve before I/O. Cancellation may skip this attempt, but the
            // intent stays pending for the next sweep; redelivery reaches its suffix.
            let next = i64::try_from(index)
                .map_err(|_| invalid())?
                .checked_add(1)
                .ok_or_else(invalid)?;
            let changed = tx.database().collection(job_receipts::Entity::COLLECTION)?.execute(Operation::Update {
                filter:value!({"id":job.id.as_str(), "app_id":self.app_id().as_str(), "reconciliation_next":record.reconciliation_next}),
                patch:value!({"reconciliation_next":next}), many:true,
            }).await?;
            changed_once(&changed)?;
            Progress::Item(id.clone())
        } else {
            let current = scan(&tx, self.app_id().as_str())
                .await?
                .ok_or_else(invalid)?;
            if current.revision < plan.revision {
                return Err(invalid());
            }
            if current.revision == plan.revision {
                let revision = current.revision.checked_add(1).ok_or_else(invalid)?;
                let after = if plan.more {
                    plan.ids.last().cloned()
                } else {
                    None
                };
                let upper = if plan.more { plan.upper.clone() } else { None };
                let changed = tx
                    .database()
                    .collection(publication_scans::Entity::COLLECTION)?
                    .execute(Operation::Update {
                        filter: value!({"id":self.app_id().as_str(), "revision":plan.revision}),
                        patch: value!({"revision":revision, "after_job":after, "upper_job":upper}),
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
                    if plan.more {
                        JobOutcome::Waiting
                    } else {
                        JobOutcome::Completed
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

async fn scan(tx: &Transaction, app: &str) -> Result<Option<Scan>, WorkflowServiceError> {
    Ok(tx
        .database()
        .entity::<publication_scans::Entity>()?
        .find::<Scan>(
            publication_scans::id.eq(app)?,
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next())
}

async fn pending_ids(
    tx: &Transaction,
    app: &str,
    after: Option<&str>,
    upper: Option<&str>,
    limit: u32,
    descending: bool,
) -> Result<Vec<String>, WorkflowServiceError> {
    let source = tx
        .database()
        .entity::<job_publications::Entity>()?
        .alias("p")?;
    let mut predicates = vec![
        source.column(job_publications::app_id).eq(app)?,
        source
            .column(job_publications::confirmed_at)
            .eq(None::<i64>)?,
    ];
    if let Some(after) = after {
        predicates.push(source.column(job_publications::id).gt(after)?);
    }
    if let Some(upper) = upper {
        predicates.push(source.column(job_publications::id).lte(upper)?);
    }
    let order = if descending {
        source.column(job_publications::id).desc()
    } else {
        source.column(job_publications::id).asc()
    };
    Ok(tx
        .database()
        .from(&source)
        .filter(Predicate::And(predicates))
        .order_by(order)
        .select(source.row::<PublicationId>())?
        .limit(i64::from(limit))?
        .all()
        .await?
        .into_iter()
        .map(|row| row.id)
        .collect())
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
