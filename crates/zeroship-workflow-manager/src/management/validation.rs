use super::{validate_link, validate_scope};
use crate::{
    models::{jobs, management, management_scopes, Job, Management, ManagementScope},
    Error,
};
use std::collections::{BTreeMap, BTreeSet};
use zeroship_core::{app_id::AppId, workflow_coordination::RunId};
use zeroship_data_orm::orm::Database;

const PAGE_SIZE: i64 = 256;
const MAX_VALIDATION_ROWS: usize = super::MAX_PENDING_COMMANDS * 4;

#[derive(Default)]
struct Pending {
    seen: BTreeSet<String>,
    runs: BTreeMap<String, (i64, i64, BTreeSet<i64>)>,
    scanned: usize,
}
impl Pending {
    fn scanned(&mut self, rows: usize) -> Result<(), Error> {
        self.scanned = self.scanned.checked_add(rows).ok_or(Error::Capacity)?;
        if self.scanned > MAX_VALIDATION_ROWS {
            return Err(Error::Capacity);
        }
        Ok(())
    }
    fn insert(
        &mut self,
        command: Management,
        job: &Job,
        scope: &ManagementScope,
    ) -> Result<(), Error> {
        if !self.seen.insert(command.id.clone()) {
            return Ok(());
        }
        validate_link(&command, job, scope)?;
        if command.outcome.is_some() || job.state == "settled" {
            return Err(Error::Storage);
        }
        let run = self.runs.entry(command.run_id).or_insert_with(|| {
            (
                scope.accepted_revision,
                scope.settled_revision,
                BTreeSet::new(),
            )
        });
        if run.0 != scope.accepted_revision
            || run.1 != scope.settled_revision
            || !run.2.insert(command.revision)
        {
            return Err(Error::Storage);
        }
        Ok(())
    }
}

/// Join each pending record to its independent job and order in bounded pages.
/// Independent command and job predicates expose a damaged outcome or projection;
/// open order ranges expose missing records without scanning completed history.
pub async fn validate_pending(tx: &Database, app: &AppId) -> Result<(), Error> {
    let mut pending = Pending::default();
    command_pages(tx, app, &mut pending).await?;
    job_pages(tx, app, &mut pending).await?;
    scope_pages(tx, app, &mut pending).await?;
    if !pending.runs.is_empty() {
        return Err(Error::Storage);
    }
    Ok(())
}

async fn command_pages(tx: &Database, app: &AppId, pending: &mut Pending) -> Result<(), Error> {
    let command = tx
        .entity::<management::Entity>()?
        .alias("pending_command")?;
    let job = tx.entity::<jobs::Entity>()?.alias("command_job")?;
    let scope = tx
        .entity::<management_scopes::Entity>()?
        .alias("command_order")?;
    let mut after: Option<String> = None;
    loop {
        let mut filter = command
            .column(management::app_id)
            .eq(app.as_str())?
            .and(command.column(management::outcome).is_null());
        if let Some(after) = &after {
            filter = filter.and(command.column(management::id).gt(after.as_str())?);
        }
        let rows = tx
            .from(&command)
            .left_join(
                &job,
                command
                    .column(management::app_id)
                    .eq(job.column(jobs::app_id))?
                    .and(command.column(management::id).eq(job.column(jobs::id))?),
            )?
            .left_join(
                &scope,
                command
                    .column(management::app_id)
                    .eq(scope.column(management_scopes::app_id))?
                    .and(
                        command
                            .column(management::run_id)
                            .eq(scope.column(management_scopes::run_id))?,
                    ),
            )?
            .filter(filter)
            .order_by(command.column(management::id).asc())
            .select((
                command.row::<Management>(),
                job.optional_row::<Job>(),
                scope.optional_row::<ManagementScope>(),
            ))?
            .limit(PAGE_SIZE)?
            .all()
            .await?;
        if rows.is_empty() {
            return Ok(());
        }
        pending.scanned(rows.len())?;
        for (command, job, scope) in rows {
            after = Some(command.id.clone());
            pending.insert(
                command,
                job.as_ref().ok_or(Error::Storage)?,
                scope.as_ref().ok_or(Error::Storage)?,
            )?;
        }
    }
}

async fn job_pages(tx: &Database, app: &AppId, pending: &mut Pending) -> Result<(), Error> {
    let job = tx.entity::<jobs::Entity>()?.alias("pending_job")?;
    let command = tx.entity::<management::Entity>()?.alias("job_command")?;
    let scope = tx
        .entity::<management_scopes::Entity>()?
        .alias("job_order")?;
    let mut after: Option<String> = None;
    loop {
        let mut filter = job
            .column(jobs::app_id)
            .eq(app.as_str())?
            .and(job.column(jobs::operation_kind).eq("management")?)
            .and(job.column(jobs::state).eq("settled")?.negate());
        if let Some(after) = &after {
            filter = filter.and(job.column(jobs::id).gt(after.as_str())?);
        }
        let rows = tx
            .from(&job)
            .left_join(
                &command,
                job.column(jobs::app_id)
                    .eq(command.column(management::app_id))?
                    .and(job.column(jobs::id).eq(command.column(management::id))?),
            )?
            .left_join(
                &scope,
                command
                    .column(management::app_id)
                    .eq(scope.column(management_scopes::app_id))?
                    .and(
                        command
                            .column(management::run_id)
                            .eq(scope.column(management_scopes::run_id))?,
                    ),
            )?
            .filter(filter)
            .order_by(job.column(jobs::id).asc())
            .select((
                job.row::<Job>(),
                command.optional_row::<Management>(),
                scope.optional_row::<ManagementScope>(),
            ))?
            .limit(PAGE_SIZE)?
            .all()
            .await?;
        if rows.is_empty() {
            return Ok(());
        }
        pending.scanned(rows.len())?;
        for (job, command, scope) in rows {
            after = Some(job.id.clone());
            pending.insert(
                command.ok_or(Error::Storage)?,
                &job,
                scope.as_ref().ok_or(Error::Storage)?,
            )?;
        }
    }
}

async fn scope_pages(tx: &Database, app: &AppId, pending: &mut Pending) -> Result<(), Error> {
    let scope = tx
        .entity::<management_scopes::Entity>()?
        .alias("pending_order")?;
    let mut after: Option<String> = None;
    loop {
        let mut filter = scope
            .column(management_scopes::app_id)
            .eq(app.as_str())?
            .and(
                scope
                    .column(management_scopes::accepted_revision)
                    .gt(scope.column(management_scopes::settled_revision))?
                    .or(scope.column(management_scopes::accepted_revision).lt(0)?)
                    .or(scope.column(management_scopes::settled_revision).lt(0)?)
                    .or(scope
                        .column(management_scopes::settled_revision)
                        .gt(scope.column(management_scopes::accepted_revision))?),
            );
        if let Some(after) = &after {
            filter = filter.and(scope.column(management_scopes::id).gt(after.as_str())?);
        }
        let rows = tx
            .from(&scope)
            .filter(filter)
            .order_by(scope.column(management_scopes::id).asc())
            .select(scope.row::<ManagementScope>())?
            .limit(PAGE_SIZE)?
            .all()
            .await?;
        if rows.is_empty() {
            return Ok(());
        }
        pending.scanned(rows.len())?;
        for scope in rows {
            after = Some(scope.id.clone());
            let run = RunId::parse(&scope.run_id).map_err(|_| Error::Storage)?;
            validate_scope(&scope, app, &run)?;
            let (accepted, settled, revisions) =
                pending.runs.remove(&scope.run_id).ok_or(Error::Storage)?;
            if accepted != scope.accepted_revision
                || settled != scope.settled_revision
                || i64::try_from(revisions.len()).map_err(|_| Error::Storage)? != accepted - settled
            {
                return Err(Error::Storage);
            }
        }
    }
}
