//! Ordered command records share the queue's app lock and commit boundary.
#![expect(
    clippy::future_not_send,
    reason = "native manager reads remain compio-local"
)]

pub const MAX_PENDING_COMMANDS: usize = 4096;

mod validation;
pub use validation::validate_pending;

use crate::{
    models::{jobs, management, management_scopes, Job, Management, ManagementScope},
    queue, Error,
};
use zeroship_core::{
    app_id::AppId,
    service_assertion::ServiceIssuer,
    service_peers::{service_issuer, CONTROL_SERVICE_NAME},
    typed_id,
    workflow_coordination::{
        ManageRun, ManagementOperation, ManagementOutcome, ManagementReceipt, RequestId,
        RestartDeploy, RunId, RunOperation,
    },
    workflow_jobs::{JobOperation, JobOutcome, JobSpec, ManagementCommand},
};
use zeroship_data_orm::orm::Database;

/// A restart names its deployment exactly when its effective deploy policy is
/// `Latest`. Both mismatches are refused: a latest restart without one leaves
/// the manager nothing to replay against, and a started restart carrying one
/// asserts a deployment no arm of acceptance will ever read, which would let a
/// caller believe it had pinned a restart it had not.
pub fn validate_request(request: &ManageRun) -> Result<(), Error> {
    if let ManagementOperation::Restart {
        options,
        deployment,
    } = &request.command
    {
        let deploy = options.effective_deploy().map_err(|_| Error::Invalid)?;
        if (deploy == RestartDeploy::Latest) != deployment.is_some() {
            return Err(Error::Invalid);
        }
        if deployment
            .as_ref()
            .is_some_and(|target| !zeroship_bundle::validate_hash_format(&target.deploy_hash))
        {
            return Err(Error::Invalid);
        }
        if options
            .from
            .as_ref()
            .is_some_and(|target| target.name.len() > 256)
        {
            return Err(Error::Invalid);
        }
    }
    Ok(())
}

pub fn request_digest(actor: &str, request: &ManageRun) -> Result<String, Error> {
    Ok(queue::digest(
        &serde_json::to_vec(&(actor, request)).map_err(|_| Error::Storage)?,
    ))
}

pub const fn blocks_execution(command: &ManagementCommand) -> bool {
    !matches!(
        command,
        ManagementCommand::Transition {
            operation: RunOperation::Resume
        }
    )
}

pub async fn record(
    tx: &Database,
    app: &AppId,
    request: &RequestId,
) -> Result<Option<Management>, Error> {
    let row = tx
        .entity::<management::Entity>()?
        .query()
        .filter(
            management::app_id
                .eq(app.as_str())?
                .and(management::request_id.eq(request.as_str())?),
        )
        .first::<Management>()
        .await?;
    let job = tx
        .entity::<jobs::Entity>()?
        .query()
        .filter(
            jobs::app_id
                .eq(app.as_str())?
                .and(jobs::management_request_id.eq(Some(request.as_str()))?),
        )
        .first::<Job>()
        .await?;
    match (row, job) {
        (None, None) => Ok(None),
        (Some(row), Some(job)) if row.id == job.id => {
            job.spec()?;
            Ok(Some(row))
        }
        _ => Err(Error::Storage),
    }
}

pub async fn scope(
    tx: &Database,
    app: &AppId,
    run: &RunId,
) -> Result<Option<ManagementScope>, Error> {
    let row = tx
        .entity::<management_scopes::Entity>()?
        .query()
        .filter(
            management_scopes::app_id
                .eq(app.as_str())?
                .and(management_scopes::run_id.eq(run.as_str())?),
        )
        .first::<ManagementScope>()
        .await?;
    if let Some(row) = &row {
        validate_scope(row, app, run)?;
    }
    Ok(row)
}

fn validate_scope(row: &ManagementScope, app: &AppId, run: &RunId) -> Result<(), Error> {
    typed_id::parse_with_prefix(&row.id, "wmo").map_err(|_| Error::Storage)?;
    if row.app_id != app.as_str()
        || row.run_id != run.as_str()
        || row.accepted_revision < 0
        || row.settled_revision < 0
        || row.settled_revision > row.accepted_revision
    {
        return Err(Error::Storage);
    }
    Ok(())
}

fn outcome(row: &Management) -> Result<Option<ManagementOutcome>, Error> {
    row.outcome
        .as_deref()
        .map(|value| serde_json::from_str(value).map_err(|_| Error::Storage))
        .transpose()
}

pub fn receipt(row: &Management) -> Result<ManagementReceipt, Error> {
    Ok(ManagementReceipt {
        app_id: AppId::parse(&row.app_id).map_err(|_| Error::Storage)?,
        request_id: RequestId::parse(&row.request_id).map_err(|_| Error::Storage)?,
        outcome: outcome(row)?,
    })
}

fn raw_request(row: &Management) -> Result<ManageRun, Error> {
    let request: ManageRun = serde_json::from_str(&row.request).map_err(|_| Error::Storage)?;
    validate_request(&request).map_err(|_| Error::Storage)?;
    let actor = ServiceIssuer::parse(&row.actor).map_err(|_| Error::Storage)?;
    let control = service_issuer(CONTROL_SERVICE_NAME).map_err(|_| Error::Storage)?;
    if actor.principal() != control.principal()
        || row.app_id != request.app_id.as_str()
        || row.run_id != request.run_id.as_str()
        || row.request_id != request.request_id.as_str()
        || row.request_digest != request_digest(&row.actor, &request)?
    {
        return Err(Error::Storage);
    }
    Ok(request)
}

fn resolved_matches(
    raw: &ManagementOperation,
    resolved: &ManagementCommand,
) -> Result<bool, Error> {
    Ok(match (raw, resolved) {
        (
            ManagementOperation::Transition { operation: left },
            ManagementCommand::Transition { operation: right },
        ) => left == right,
        (
            ManagementOperation::Restart { options, .. },
            ManagementCommand::RestartStarted { from },
        ) => {
            options.effective_deploy().map_err(|_| Error::Storage)? == RestartDeploy::Started
                && &options.from == from
        }
        (
            ManagementOperation::Restart {
                options,
                deployment,
            },
            ManagementCommand::RestartLatest { deployment_id },
        ) => {
            options.effective_deploy().map_err(|_| Error::Storage)? == RestartDeploy::Latest
                && deployment
                    .as_ref()
                    .is_some_and(|target| &target.deployment_id == deployment_id)
        }
        _ => false,
    })
}

pub async fn linked(tx: &Database, row: &Management) -> Result<(Job, ManagementScope), Error> {
    let raw = raw_request(row)?;
    let job = queue::load(tx, &raw.app_id, &row.id)
        .await?
        .ok_or(Error::Storage)?;
    let scope = scope(tx, &raw.app_id, &raw.run_id)
        .await?
        .ok_or(Error::Storage)?;
    validate_link(row, &job, &scope)?;
    Ok((job, scope))
}

fn validate_link(row: &Management, job: &Job, scope: &ManagementScope) -> Result<(), Error> {
    let raw = raw_request(row)?;
    validate_scope(scope, &raw.app_id, &raw.run_id)?;
    let spec = job.spec()?;
    let JobOperation::Management {
        request_id,
        run_id,
        revision,
        command,
    } = &spec.operation
    else {
        return Err(Error::Storage);
    };
    if spec.id.as_str() != row.id
        || spec.app_id != raw.app_id
        || request_id != &raw.request_id
        || run_id != &raw.run_id
        || row.revision != revision.get()
        || row.created_at != spec.available_at.get()
        || row.blocks_execution != blocks_execution(command)
        || !resolved_matches(&raw.command, command)?
    {
        return Err(Error::Storage);
    }
    if scope.accepted_revision < row.revision {
        return Err(Error::Storage);
    }
    match (job.state.as_str(), outcome(row)?) {
        ("ready" | "leased", None) if row.revision > scope.settled_revision => {
            if job.outcome.is_some() || job.settlement_digest.is_some() {
                return Err(Error::Storage);
            }
        }
        ("settled", Some(outcome)) if row.revision <= scope.settled_revision => {
            let stored: JobOutcome =
                serde_json::from_str(job.outcome.as_deref().ok_or(Error::Storage)?)
                    .map_err(|_| Error::Storage)?;
            if stored != (JobOutcome::Management { outcome }) || job.settlement_digest.is_none() {
                return Err(Error::Storage);
            }
        }
        _ => return Err(Error::Storage),
    }
    Ok(())
}

pub async fn validate_job(tx: &Database, spec: &JobSpec, next: bool) -> Result<(), Error> {
    let JobOperation::Management { request_id, .. } = &spec.operation else {
        return Ok(());
    };
    let row = record(tx, &spec.app_id, request_id)
        .await?
        .ok_or(Error::Storage)?;
    let (job, scope) = linked(tx, &row).await?;
    if job.spec()? != *spec
        || (next
            && row.revision
                != scope
                    .settled_revision
                    .checked_add(1)
                    .ok_or(Error::Storage)?)
    {
        return Err(Error::Storage);
    }
    Ok(())
}

pub fn settle<'a>(
    tx: &'a Database,
    spec: &'a JobSpec,
    result: &'a JobOutcome,
    replay: bool,
) -> impl std::future::Future<Output = Result<(), Error>> + 'a {
    Box::pin(async move {
        let JobOperation::Management { request_id, .. } = &spec.operation else {
            return Ok(());
        };
        let JobOutcome::Management { outcome: result } = result else {
            return Err(Error::Invalid);
        };
        let row = record(tx, &spec.app_id, request_id)
            .await?
            .ok_or(Error::Storage)?;
        let (job, scope) = linked(tx, &row).await?;
        if job.spec()? != *spec {
            return Err(Error::Storage);
        }
        if replay {
            return if outcome(&row)?.as_ref() == Some(result) && job.state == "settled" {
                Ok(())
            } else {
                Err(Error::Storage)
            };
        }
        if job.state != "leased"
            || row.revision
                != scope
                    .settled_revision
                    .checked_add(1)
                    .ok_or(Error::Storage)?
        {
            return Err(Error::Storage);
        }
        let encoded = serde_json::to_string(result).map_err(|_| Error::Storage)?;
        let _: Management = tx
            .entity::<management::Entity>()?
            .update(
                management::id.eq(row.id.as_str())?,
                management::outcome.set(Some(encoded))?,
            )
            .await?
            .ok_or(Error::Storage)?;
        let _: ManagementScope = tx
            .entity::<management_scopes::Entity>()?
            .update(
                management_scopes::id.eq(scope.id.as_str())?,
                management_scopes::settled_revision.set(row.revision)?,
            )
            .await?
            .ok_or(Error::Storage)?;
        Ok(())
    })
}
