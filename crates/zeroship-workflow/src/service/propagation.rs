//! Creator-owned bounded propagation of cancellation and terminal results
//! across child dependencies.
//!
//! A settling or terminal generation records one obligation and its first page
//! intent. Delivered pages apply bounded effects with their cursor, receipt and
//! successor publication. An unfinished cascade obligation also fences its
//! source generation's cascading children until every page has reached them.

#![expect(
    clippy::future_not_send,
    reason = "propagation owns a compio-local creator transaction"
)]

use super::{
    app::{decode, encode, lock_app_state, parse_state},
    continuations,
    delivery::{self, CapturedLease, JobReceipt},
    models, publication,
    store::{Row, Transaction},
    AppWorkflows, ControlIntent,
};
use crate::WorkflowServiceError;
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::Revision,
    workflow_jobs::{JobLease, JobOperation, JobOutcome, JobSpec, PropagationId},
};
use zeroship_data_orm::{orm::FindOptions, sql::MAX_ROW_LIMIT};

mod application;
mod history;
mod records;
use records::{Kind, Obligation, Page, PageResult, Pending};

/// Upper bound on the children or parent waits one delivered page selects.
#[derive(Debug, Clone, Copy)]
pub struct PropagationOptions {
    pub page_size: u32,
}
impl Default for PropagationOptions {
    fn default() -> Self {
        Self { page_size: 128 }
    }
}
impl PropagationOptions {
    pub(super) fn validate(self) -> Result<(), WorkflowServiceError> {
        if self.page_size == 0 || i64::from(self.page_size) > MAX_ROW_LIMIT {
            return Err(WorkflowServiceError::InvalidRequest(
                "invalid workflow propagation page bound".into(),
            ));
        }
        Ok(())
    }
}

impl AppWorkflows {
    /// Apply one delivered propagation page. Committed pages replay their exact
    /// receipt without fresh authority; a page never waits for another job.
    ///
    /// # Errors
    /// Refuses changed jobs, invalid journal linkage and exhausted original policy
    /// or delivery authority. Failed transactions preserve the same retryable page.
    pub async fn propagation_job(
        &self,
        grant: &impl JobLease,
        options: PropagationOptions,
    ) -> Result<JobReceipt, WorkflowServiceError> {
        options.validate()?;
        let job = &grant.delivery().job;
        delivery::check_scope(self.app_id(), job)?;
        if !matches!(job.operation, JobOperation::Propagate { .. }) {
            return Err(WorkflowServiceError::InvalidRequest(
                "expected workflow propagation job".into(),
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
                let mut tx = if captured.is_ok() {
                    scope.service.begin().await?
                } else {
                    scope.service.begin_history().await?
                };
                lock_app_state(&mut tx, self.app_id()).await?;
                if let Some(receipt) = history::receipt(&tx, job).await? {
                    tx.commit().await?;
                    return Ok(receipt);
                }
                let (scope, authority) = captured?;
                authority.check(&scope)?;
                tx.capture_mutation(self.app_id())?;
                let receipt = Box::pin(application::apply(&mut tx, job, options)).await?;
                authority.check(&scope)?;
                tx.commit().await?;
                authority.check(&scope)?;
                Ok(receipt)
            }),
        );
        match policy {
            Some(policy) => policy.run(operation).await,
            None => operation.await,
        }
    }
}

pub(super) async fn receipt(
    tx: &Transaction,
    job: &JobSpec,
) -> Result<Option<JobReceipt>, WorkflowServiceError> {
    history::receipt(tx, job).await
}

/// Record the cascade obligation of a settling generation and its first page.
/// The probe is one parent-linkage index lookup; repeated settlement reuses
/// the obligation.
pub(super) async fn cascade(
    tx: &Transaction,
    app: &AppId,
    run: &str,
    generation: i64,
    now: i64,
) -> Result<(), WorkflowServiceError> {
    let owns_child = tx
        .database()
        .entity::<models::runs::Entity>()?
        .exists(
            models::runs::app_id
                .eq(app.as_str())?
                .and(models::runs::parent_id.eq(Some(run))?)
                .and(models::runs::parent_generation.eq(Some(generation))?)
                .and(models::runs::cascade.eq(1_i64)?),
        )
        .await?;
    if !owns_child {
        return Ok(());
    }
    Box::pin(record(tx, app, Kind::Cascade, run, generation, now)).await
}

/// Record the notify obligation of a terminal head generation and its first
/// page. The probe stops at the first current parent wait on the head.
pub(super) async fn notify(
    tx: &Transaction,
    app: &AppId,
    member: &continuations::Member,
    now: i64,
) -> Result<(), WorkflowServiceError> {
    if !member.is_current {
        return Err(invalid());
    }
    if !continuations::has_waiting(tx, app, member).await? {
        return Ok(());
    }
    Box::pin(record(
        tx,
        app,
        Kind::Notify,
        &member.run_id,
        member.generation,
        now,
    ))
    .await
}

async fn record(
    tx: &Transaction,
    app: &AppId,
    kind: Kind,
    run: &str,
    generation: i64,
    now: i64,
) -> Result<(), WorkflowServiceError> {
    if let Some(existing) = source_obligation(tx, app, kind, run, generation).await? {
        existing.validate(app)?;
        return Ok(());
    }
    let id = PropagationId::mint();
    let saved = tx
        .database()
        .entity::<models::propagations::Entity>()?
        .insert::<_, Obligation>(Obligation {
            id: id.as_str().to_owned(),
            app_id: app.as_str().to_owned(),
            run_id: run.to_owned(),
            generation,
            kind: kind.as_str().to_owned(),
            cursor: None,
            revision: 1,
            finished: 0,
            created_at: now,
        })
        .await?;
    if saved.validate(app)? != kind || saved.id != id.as_str() {
        return Err(invalid());
    }
    publication::propagate(tx, app, &id, revision(1)?, now).await?;
    Ok(())
}

/// Recorded lifecycle intent, strengthened to cancellation while the run's
/// parent generation still propagates its cascade.
pub(super) async fn effective_control(
    tx: &Transaction,
    app: &AppId,
    run: &Row,
) -> Result<ControlIntent, WorkflowServiceError> {
    let recorded = ControlIntent::parse(&run.text("control")?)?;
    if recorded == ControlIntent::Cancel || !fenced(tx, app, run).await? {
        return Ok(recorded);
    }
    Ok(ControlIntent::Cancel)
}

/// Whether an unfinished cascade obligation of the run's parent generation
/// applies to this cascading child.
pub(super) async fn fenced(
    tx: &Transaction,
    app: &AppId,
    run: &Row,
) -> Result<bool, WorkflowServiceError> {
    if run.integer("cascade")? != 1 {
        return Ok(false);
    }
    let (Some(parent), Some(generation)) = (
        run.optional_text("parent_id")?,
        run.optional_integer("parent_generation")?,
    ) else {
        return Ok(false);
    };
    Ok(tx
        .database()
        .entity::<models::propagations::Entity>()?
        .exists(
            models::propagations::app_id
                .eq(app.as_str())?
                .and(models::propagations::run_id.eq(parent.as_str())?)
                .and(models::propagations::generation.eq(generation)?)
                .and(models::propagations::kind.eq(Kind::Cascade.as_str())?)
                .and(models::propagations::finished.eq(0_i64)?),
        )
        .await?)
}

async fn source_obligation(
    tx: &Transaction,
    app: &AppId,
    kind: Kind,
    run: &str,
    generation: i64,
) -> Result<Option<Obligation>, WorkflowServiceError> {
    Ok(tx
        .database()
        .entity::<models::propagations::Entity>()?
        .find::<Obligation>(
            models::propagations::app_id
                .eq(app.as_str())?
                .and(models::propagations::run_id.eq(run)?)
                .and(models::propagations::generation.eq(generation)?)
                .and(models::propagations::kind.eq(kind.as_str())?),
            one(),
        )
        .await?
        .into_iter()
        .next())
}

async fn obligation(
    tx: &Transaction,
    app: &AppId,
    id: &PropagationId,
) -> Result<(Obligation, Kind), WorkflowServiceError> {
    let row = tx
        .database()
        .entity::<models::propagations::Entity>()?
        .find::<Obligation>(
            models::propagations::app_id
                .eq(app.as_str())?
                .and(models::propagations::id.eq(id.as_str())?),
            one(),
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(invalid)?;
    let kind = row.validate(app)?;
    if row.id != id.as_str() {
        return Err(invalid());
    }
    Ok((row, kind))
}

fn one() -> FindOptions {
    FindOptions {
        limit: Some(1),
        ..Default::default()
    }
}
fn revision(value: i64) -> Result<Revision, WorkflowServiceError> {
    Revision::try_from(value).map_err(|_| invalid())
}
fn increment(value: i64) -> Result<i64, WorkflowServiceError> {
    if value <= 0 {
        return Err(invalid());
    }
    value.checked_add(1).ok_or_else(|| {
        WorkflowServiceError::ResourceExhausted("workflow propagation revision exhausted".into())
    })
}
fn changed(count: i64) -> Result<(), WorkflowServiceError> {
    if count == 1 {
        Ok(())
    } else {
        Err(invalid())
    }
}
fn invalid() -> WorkflowServiceError {
    WorkflowServiceError::Internal("invalid workflow dependency propagation journal".into())
}
