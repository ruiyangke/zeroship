use super::{count, Coordinator, Error};
use crate::{
    management as commands,
    models::{management, management_scopes, queue_scopes, Scope},
    retention,
};
use zeroship_core::{
    app_id::AppId,
    service_assertion::ServiceIssuer,
    service_peers::{service_issuer, CONTROL_SERVICE_NAME},
    typed_id,
    workflow_coordination::{
        ManageRun, ManagementOperation, ManagementReceipt, RequestId, RestartDeploy,
    },
    workflow_jobs::{DeploymentId, JobId, JobOperation, JobSpec, ManagementCommand},
};
use zeroship_data_orm::{orm::Database, value};

enum Acceptance {
    Ready(ManagementReceipt),
    Retain(DeploymentId),
}

impl Coordinator {
    /// Accept a Control command and its ordered delivery atomically. Exact raw
    /// retries use the retained job.
    ///
    /// # What this validates about the deployment, and what it does not
    ///
    /// A latest restart names its deployment on the wire, and this call does
    /// NOT decide which deployment is current. It cannot: the endpoint this
    /// serves is authorized to Control alone, and Control is the authority for
    /// `zeroship.apps.deploy_hash` and `zeroship.app_deploys`. Asking the
    /// catalog again would re-derive an answer its only caller already holds,
    /// and would answer it a moment later than the caller decided.
    ///
    /// What the manager still enforces about the named deployment:
    ///
    /// - The queue must hold it. An unheld deployment leaves acceptance for
    ///   `ensure_deployment_for`, whose acquisition runs inside Control's row
    ///   lock on `zeroship.app_deploys`: a deployment belonging to another app
    ///   finds no row and is denied, and one whose retention state has left
    ///   `available` -- reclaiming or deleted -- is a conflict.
    /// - The hash must match the hold. `require_held` returns the hash Control
    ///   minted when it granted the hold, and a request naming a different one
    ///   is a conflict rather than a restart onto code the caller did not name.
    ///
    /// What it has stopped enforcing: that the named deployment is the app's
    /// CURRENT one. A deployment that really was current, is still available
    /// and still held, but has since been superseded, is accepted here. Nothing
    /// in the manager distinguishes that from a fresh command; `request_id`
    /// bounds the damage to one accepted command per request, and Control being
    /// the sole caller is what stands in the place a staleness check would.
    ///
    /// # Errors
    /// Rejects unauthorized issuers, changed requests, unavailable deployments,
    /// exhausted capacity and malformed or unavailable storage.
    pub async fn manage(
        &self,
        actor: &ServiceIssuer,
        request: &ManageRun,
    ) -> Result<ManagementReceipt, Error> {
        let control = service_issuer(CONTROL_SERVICE_NAME).map_err(|_| Error::Storage)?;
        if actor.principal() != control.principal() {
            return Err(Error::Denied);
        }
        commands::validate_request(request)?;
        self.queue.encode(&(actor.as_str(), request))?;
        let budget = self.budget();
        loop {
            let result = self
                .queue
                .transact_for(budget.clone(), |tx| async move {
                    self.accept(&tx, actor, request).await
                })
                .await?;
            match result {
                Acceptance::Ready(receipt) => return Ok(receipt),
                Acceptance::Retain(deployment) => {
                    self.queue
                        .ensure_deployment_for(&request.app_id, &deployment, budget.clone())
                        .await?;
                }
            }
        }
    }

    async fn accept(
        &self,
        tx: &Database,
        actor: &ServiceIssuer,
        request: &ManageRun,
    ) -> Result<Acceptance, Error> {
        self.scope(tx, &request.app_id, true).await?;
        if let Some(row) = commands::record(tx, &request.app_id, &request.request_id).await? {
            commands::linked(tx, &row).await?;
            if row.request_digest != commands::request_digest(actor.as_str(), request)? {
                return Err(Error::Conflict);
            }
            return Ok(Acceptance::Ready(commands::receipt(&row)?));
        }
        commands::validate_pending(tx, &request.app_id).await?;
        let pending = count::<management::Entity>(
            tx,
            management::app_id
                .eq(request.app_id.as_str())?
                .and(management::outcome.is_null()),
        )
        .await?;
        if pending
            >= i64::try_from(self.options.max_pending_management).map_err(|_| Error::Invalid)?
        {
            return Err(Error::Capacity);
        }
        let command = match &request.command {
            ManagementOperation::Transition { operation } => ManagementCommand::Transition {
                operation: *operation,
            },
            ManagementOperation::Restart {
                options,
                deployment,
            } => match options.effective_deploy().map_err(|_| Error::Invalid)? {
                RestartDeploy::Started => ManagementCommand::RestartStarted {
                    from: options.from.clone(),
                },
                RestartDeploy::Latest => {
                    let target = deployment.as_ref().ok_or(Error::Invalid)?;
                    if !retention::prepared(tx, &request.app_id, &target.deployment_id).await? {
                        return Ok(Acceptance::Retain(target.deployment_id.clone()));
                    }
                    let held =
                        retention::require_held(tx, &request.app_id, &target.deployment_id).await?;
                    if held.deploy_hash != target.deploy_hash {
                        return Err(Error::Conflict);
                    }
                    ManagementCommand::RestartLatest {
                        deployment_id: target.deployment_id.clone(),
                    }
                }
            },
        };
        self.insert_command(tx, actor, request, command)
            .await
            .map(Acceptance::Ready)
    }

    async fn insert_command(
        &self,
        tx: &Database,
        actor: &ServiceIssuer,
        request: &ManageRun,
        command: ManagementCommand,
    ) -> Result<ManagementReceipt, Error> {
        let order = commands::scope(tx, &request.app_id, &request.run_id).await?;
        let (scope_id, revision) = if let Some(order) = order {
            (
                order.id,
                order
                    .accepted_revision
                    .checked_add(1)
                    .ok_or(Error::Capacity)?,
            )
        } else {
            let id = typed_id::generate("wmo");
            tx.collection("management_scopes")?.insert(value!({"id":id,"app_id":request.app_id.as_str(),"run_id":request.run_id.as_str(),"accepted_revision":0,"settled_revision":0})).await?;
            (id, 1)
        };
        let now = self.queue.clock.now().await?;
        let blocks_execution = commands::blocks_execution(&command);
        let job = JobSpec {
            id: JobId::mint(),
            app_id: request.app_id.clone(),
            available_at: now.try_into().map_err(|_| Error::Storage)?,
            operation: JobOperation::Management {
                request_id: request.request_id.clone(),
                run_id: request.run_id.clone(),
                revision: revision.try_into().map_err(|_| Error::Capacity)?,
                command,
            },
        };
        self.queue.insert(tx, &job, now).await?;
        tx.collection("management")?.insert(value!({
            "id":job.id.as_str(),"app_id":request.app_id.as_str(),"request_id":request.request_id.as_str(),"run_id":request.run_id.as_str(),
            "revision":revision,"actor":actor.as_str(),"request":serde_json::to_string(request).map_err(|_| Error::Invalid)?,
            "request_digest":commands::request_digest(actor.as_str(), request)?,"blocks_execution":blocks_execution,"created_at":now
        })).await?;
        let _: crate::models::ManagementScope = tx
            .entity::<management_scopes::Entity>()?
            .update(
                management_scopes::id.eq(scope_id.as_str())?,
                management_scopes::accepted_revision.set(revision)?,
            )
            .await?
            .ok_or(Error::Storage)?;
        Ok(ManagementReceipt {
            app_id: request.app_id.clone(),
            request_id: request.request_id.clone(),
            outcome: None,
        })
    }

    /// # Errors
    /// Rejects malformed command/job linkage or unavailable receipt metadata.
    pub async fn management_receipt(
        &self,
        app: &AppId,
        request: &RequestId,
    ) -> Result<Option<ManagementReceipt>, Error> {
        self.queue
            .transact(|tx| async move {
                if tx
                    .entity::<queue_scopes::Entity>()?
                    .query()
                    .filter(queue_scopes::id.eq(app.as_str())?)
                    .first::<Scope>()
                    .await?
                    .is_none()
                {
                    return Ok(None);
                }
                self.scope(&tx, app, false).await?;
                let Some(row) = commands::record(&tx, app, request).await? else {
                    return Ok(None);
                };
                commands::linked(&tx, &row).await?;
                commands::receipt(&row).map(Some)
            })
            .await
    }
}
