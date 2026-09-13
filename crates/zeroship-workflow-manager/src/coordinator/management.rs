use super::{count, one, rows, update, Coordinator, Error};
use crate::models::{management, Management};
use zeroship_core::{
    app_id::AppId,
    service_assertion::ServiceIssuer,
    service_peers::{service_issuer, CONTROL_SERVICE_NAME},
    typed_id,
    workflow_coordination::{
        AcknowledgeManagement, AssignedScope, ManageRun, ManagementOperation, ManagementOutcome,
        ManagementReceipt, RequestId, RestartDeploy, RestartOptions, RestartTarget, RunId,
        RunOperation, WorkerId,
    },
};
use zeroship_data_orm::{
    orm::{Database, Entity},
    value,
};

impl Coordinator {
    /// Persist lifecycle metadata selected by authenticated Control.
    ///
    /// # Errors
    /// Rejects unauthorized issuers, invalid commands, changed receipts and capacity exhaustion.
    pub async fn manage(
        &self,
        actor: &ServiceIssuer,
        request: &ManageRun,
    ) -> Result<ManagementReceipt, Error> {
        let control = service_issuer(CONTROL_SERVICE_NAME).map_err(|_| Error::Storage)?;
        if actor.principal() != control.principal() {
            return Err(Error::Denied);
        }
        let fields = CommandFields::encode(&request.command)?;
        self.queue.transact(|tx| async move {
            self.scope(&tx, &request.app_id, true).await?;
            if let Some(row) = record(&tx, &request.app_id, &request.request_id).await? {
                if row.run_id != request.run_id.as_str() || row.actor != actor.as_str() || CommandFields::from_record(&row) != fields { return Err(Error::Conflict); }
                return receipt(&row);
            }
            let pending = count::<management::Entity>(&tx,
                management::app_id.eq(request.app_id.as_str())?.and(management::outcome.is_null()),
            ).await?;
            if pending >= i64::try_from(self.options.max_pending_management).map_err(|_| Error::Invalid)? { return Err(Error::Capacity); }
            let now = self.queue.clock.now().await?;
            tx.collection(management::Entity::COLLECTION)?.insert(value!({
                "id":typed_id::generate("wcm"),"app_id":request.app_id.as_str(),"request_id":request.request_id.as_str(),
                "run_id":request.run_id.as_str(),"actor":actor.as_str(),"operation":fields.operation,
                "restart_name":fields.name,"restart_occurrence":fields.occurrence,"restart_deploy":fields.deploy,"created_at":now
            })).await?;
            Ok(ManagementReceipt { app_id:request.app_id.clone(),request_id:request.request_id.clone(),outcome:None })
        }).await
    }

    /// # Errors
    /// Rejects stale assignments and unavailable or malformed command records.
    pub async fn pending_management(
        &self,
        worker: &WorkerId,
        request: &AssignedScope,
    ) -> Result<Vec<ManageRun>, Error> {
        let budget = self.budget();
        self.queue
            .transact_for(budget.clone(), |tx| async move {
                self.scope(&tx, &request.app_id, false).await?;
                let (_, sample, expires) = self
                    .bound(&tx, worker, &request.app_id, request.assignment_revision)
                    .await?;
                budget.cap(sample, expires)?;
                let pending = rows::<management::Entity, Management>(
                    &tx,
                    management::app_id
                        .eq(request.app_id.as_str())?
                        .and(management::outcome.is_null()),
                    [management::created_at.asc(), management::request_id.asc()],
                    self.options.batch_limit,
                )
                .await?;
                let commands = pending
                    .into_iter()
                    .map(|row| {
                        Ok(ManageRun {
                            request_id: RequestId::parse(&row.request_id)
                                .map_err(|_| Error::Storage)?,
                            app_id: request.app_id.clone(),
                            run_id: RunId::parse(&row.run_id).map_err(|_| Error::Storage)?,
                            command: CommandFields::from_record(&row).decode()?,
                        })
                    })
                    .collect();
                budget.cap(self.queue.clock.sample().await?, expires)?;
                commands
            })
            .await
    }

    /// # Errors
    /// Rejects missing commands, changed outcomes, stale assignments and storage failures.
    pub async fn acknowledge_management(
        &self,
        worker: &WorkerId,
        request: &AcknowledgeManagement,
    ) -> Result<ManagementReceipt, Error> {
        let budget = self.budget();
        self.queue.transact_for(budget.clone(), |tx| async move {
            self.scope(&tx, &request.app_id, false).await?;
            let (_, sample, expires) = self.bound(&tx, worker, &request.app_id, request.assignment_revision).await?;
            budget.cap(sample, expires)?;
            let row = record(&tx, &request.app_id, &request.request_id).await?.ok_or(Error::Denied)?;
            if let Some(existing) = outcome(&row)? {
                if existing != request.outcome { return Err(Error::Conflict); }
                budget.cap(self.queue.clock.sample().await?, expires)?;
                return receipt(&row);
            }
            let (outcome, state) = match request.outcome {
                ManagementOutcome::Applied { state } => ("applied",Some(state.as_str())),
                ManagementOutcome::NotFound {} => ("not_found",None),
                ManagementOutcome::Conflict {} => ("conflict",None),
                ManagementOutcome::Denied {} => ("denied",None),
            };
            update::<management::Entity>(&tx, value!({"app_id":request.app_id.as_str(),"request_id":request.request_id.as_str(),"outcome":null}),
                value!({"outcome":outcome,"run_state":state,"ack_worker_id":worker.as_str(),"ack_revision":request.assignment_revision.get()})).await?;
            budget.cap(self.queue.clock.sample().await?, expires)?;
            Ok(ManagementReceipt { app_id:request.app_id.clone(),request_id:request.request_id.clone(),outcome:Some(request.outcome) })
        }).await
    }

    /// # Errors
    /// Rejects unavailable or malformed receipt metadata.
    pub async fn management_receipt(
        &self,
        app: &AppId,
        request: &RequestId,
    ) -> Result<Option<ManagementReceipt>, Error> {
        self.queue
            .transact(|tx| async move {
                record(&tx, app, request)
                    .await?
                    .as_ref()
                    .map(receipt)
                    .transpose()
            })
            .await
    }
}

#[derive(PartialEq, Eq)]
struct CommandFields {
    operation: String,
    name: Option<String>,
    occurrence: Option<i64>,
    deploy: Option<String>,
}

impl CommandFields {
    fn encode(command: &ManagementOperation) -> Result<Self, Error> {
        match command {
            ManagementOperation::Transition { operation } => Ok(Self {
                operation: operation.as_str().into(),
                name: None,
                occurrence: None,
                deploy: None,
            }),
            ManagementOperation::Restart { options } => {
                if options.from.as_ref().is_some_and(|target| {
                    target.name.is_empty()
                        || target.name.len() > 256
                        || target.occurrence.is_some_and(|n| n > i32::MAX as u32)
                }) || (options.from.is_some() && options.deploy == Some(RestartDeploy::Latest))
                {
                    return Err(Error::Invalid);
                }
                Ok(Self {
                    operation: "restart".into(),
                    name: options.from.as_ref().map(|target| target.name.clone()),
                    occurrence: options
                        .from
                        .as_ref()
                        .and_then(|target| target.occurrence)
                        .map(i64::from),
                    deploy: options.deploy.map(|deploy| {
                        match deploy {
                            RestartDeploy::Started => "started",
                            RestartDeploy::Latest => "latest",
                        }
                        .into()
                    }),
                })
            }
        }
    }

    fn from_record(row: &Management) -> Self {
        Self {
            operation: row.operation.clone(),
            name: row.restart_name.clone(),
            occurrence: row.restart_occurrence,
            deploy: row.restart_deploy.clone(),
        }
    }

    fn decode(&self) -> Result<ManagementOperation, Error> {
        let command = match self.operation.as_str() {
            "pause" => ManagementOperation::Transition {
                operation: RunOperation::Pause,
            },
            "resume" => ManagementOperation::Transition {
                operation: RunOperation::Resume,
            },
            "cancel" => ManagementOperation::Transition {
                operation: RunOperation::Cancel,
            },
            "restart" => ManagementOperation::Restart {
                options: RestartOptions {
                    from: self
                        .name
                        .as_ref()
                        .map(|name| {
                            Ok::<_, Error>(RestartTarget {
                                name: name.clone(),
                                occurrence: self
                                    .occurrence
                                    .map(u32::try_from)
                                    .transpose()
                                    .map_err(|_| Error::Storage)?,
                            })
                        })
                        .transpose()?,
                    deploy: self
                        .deploy
                        .as_deref()
                        .map(|deploy| match deploy {
                            "started" => Ok(RestartDeploy::Started),
                            "latest" => Ok(RestartDeploy::Latest),
                            _ => Err(Error::Storage),
                        })
                        .transpose()?,
                },
            },
            _ => return Err(Error::Storage),
        };
        if Self::encode(&command).map_err(|_| Error::Storage)? != *self {
            return Err(Error::Storage);
        }
        Ok(command)
    }
}

async fn record(
    tx: &Database,
    app: &AppId,
    request: &RequestId,
) -> Result<Option<Management>, Error> {
    one::<management::Entity, Management>(
        tx,
        management::app_id
            .eq(app.as_str())?
            .and(management::request_id.eq(request.as_str())?),
    )
    .await
}

fn outcome(row: &Management) -> Result<Option<ManagementOutcome>, Error> {
    Ok(match (row.outcome.as_deref(), row.run_state.as_deref()) {
        (None, None) => None,
        (Some("applied"), Some(state)) => Some(ManagementOutcome::Applied {
            state: state.parse().map_err(|_| Error::Storage)?,
        }),
        (Some("not_found"), None) => Some(ManagementOutcome::NotFound {}),
        (Some("conflict"), None) => Some(ManagementOutcome::Conflict {}),
        (Some("denied"), None) => Some(ManagementOutcome::Denied {}),
        _ => return Err(Error::Storage),
    })
}

fn receipt(row: &Management) -> Result<ManagementReceipt, Error> {
    Ok(ManagementReceipt {
        app_id: AppId::parse(&row.app_id).map_err(|_| Error::Storage)?,
        request_id: RequestId::parse(&row.request_id).map_err(|_| Error::Storage)?,
        outcome: outcome(row)?,
    })
}
