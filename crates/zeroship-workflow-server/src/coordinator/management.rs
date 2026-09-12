use super::{get, now, placement::bound, scope, Coordinator, Error};
use compio_postgres::{Row, Transaction};
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

impl Coordinator {
    /// The authenticated Control issuer supplies actor provenance. Commands
    /// contain typed lifecycle metadata and are applied in the customer worker.
    ///
    /// # Errors
    /// Rejects unauthorized issuers, invalid commands, changed receipts, queue
    /// exhaustion and database failures.
    pub async fn manage(
        &self,
        actor: &ServiceIssuer,
        request: &ManageRun,
    ) -> Result<ManagementReceipt, Error> {
        let control = service_issuer(CONTROL_SERVICE_NAME).map_err(|_| Error::Unavailable)?;
        if actor.principal() != control.principal() {
            return Err(Error::Denied);
        }
        let fields = CommandFields::encode(&request.command)?;
        self.transact(async |tx| {
            scope(tx,&request.app_id,true).await?;
            if let Some(row) = management(tx,&request.app_id,&request.request_id).await? {
                if get::<&str>(&row,"run_id")? != request.run_id.as_str()
                    || get::<&str>(&row,"actor")? != actor.as_str()
                    || CommandFields::from_row(&row)? != fields
                { return Err(Error::Conflict); }
                return receipt_from_row(&row);
            }
            let row = tx.query_one(
                "SELECT count(*) AS pending FROM workflow_coordination.management WHERE app_id=$1 AND outcome IS NULL",
                &[&request.app_id.as_str()],
            ).await?;
            let maximum = i64::try_from(self.options.max_pending_management).map_err(|_| Error::Invalid)?;
            if get::<i64>(&row,"pending")? >= maximum { return Err(Error::Capacity); }
            let now = now(tx).await?;
            tx.execute(
                "INSERT INTO workflow_coordination.management(app_id,request_id,run_id,actor,operation,restart_name,restart_occurrence,restart_deploy,created_at,id)
                 VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
                &[&request.app_id.as_str(), &request.request_id.as_str(), &request.run_id.as_str(), &actor.as_str(),
                  &fields.operation, &fields.name, &fields.occurrence, &fields.deploy, &now, &typed_id::generate("wcm")],
            ).await?;
            Ok(ManagementReceipt { app_id: request.app_id.clone(), request_id: request.request_id.clone(), outcome: None })
        }).await
    }

    /// Delivery may repeat or reach another assigned worker after failure.
    /// The customer engine deduplicates the command by its stable request ID.
    ///
    /// # Errors
    /// Rejects stale/foreign assignments and unavailable or invalid command records.
    pub async fn pending_management(
        &self,
        worker: &WorkerId,
        scope_request: &AssignedScope,
    ) -> Result<Vec<ManageRun>, Error> {
        self.transact(async |tx| {
            scope(tx,&scope_request.app_id,false).await?;
            bound(tx,worker,&scope_request.app_id,scope_request.assignment_revision).await?;
            let limit = i64::try_from(self.options.batch_limit).map_err(|_| Error::Invalid)?;
            tx.query(
                "SELECT app_id,request_id,run_id,operation,restart_name,restart_occurrence,restart_deploy
                 FROM workflow_coordination.management WHERE app_id=$1 AND outcome IS NULL
                 ORDER BY created_at,request_id LIMIT $2", &[&scope_request.app_id.as_str(), &limit],
            ).await?.iter().map(|row| Ok(ManageRun {
                request_id: RequestId::parse(get(row,"request_id")?).map_err(|_| Error::Unavailable)?,
                app_id: scope_request.app_id.clone(),
                run_id: RunId::parse(get(row,"run_id")?).map_err(|_| Error::Unavailable)?,
                command: CommandFields::from_row(row)?.decode()?,
            })).collect()
        }).await
    }

    /// # Errors
    /// Rejects stale/foreign assignments, missing commands, changed outcomes and database failures.
    pub async fn acknowledge_management(
        &self,
        worker: &WorkerId,
        request: &AcknowledgeManagement,
    ) -> Result<ManagementReceipt, Error> {
        self.transact(async |tx| {
            scope(tx,&request.app_id,false).await?;
            bound(tx,worker,&request.app_id,request.assignment_revision).await?;
            let row = management(tx,&request.app_id,&request.request_id).await?.ok_or(Error::Denied)?;
            if let Some(outcome) = outcome_from_row(&row)? {
                if outcome != request.outcome { return Err(Error::Conflict); }
                return receipt_from_row(&row);
            }
            let (outcome,state) = match request.outcome {
                ManagementOutcome::Applied { state } => ("applied",Some(state.as_str())),
                ManagementOutcome::NotFound {} => ("not_found",None),
                ManagementOutcome::Conflict {} => ("conflict",None),
                ManagementOutcome::Denied {} => ("denied",None),
            };
            tx.execute(
                "UPDATE workflow_coordination.management SET outcome=$3,run_state=$4,ack_worker_id=$5,ack_revision=$6
                 WHERE app_id=$1 AND request_id=$2",
                &[&request.app_id.as_str(), &request.request_id.as_str(), &outcome, &state, &worker.as_str(), &request.assignment_revision.get()],
            ).await?;
            Ok(ManagementReceipt { app_id: request.app_id.clone(), request_id: request.request_id.clone(), outcome: Some(request.outcome) })
        }).await
    }

    /// High-level acknowledgement metadata; detailed run reads go to the worker.
    ///
    /// # Errors
    /// Returns `Unavailable` when receipts cannot be read or decoded.
    pub async fn management_receipt(
        &self,
        app: &AppId,
        request: &RequestId,
    ) -> Result<Option<ManagementReceipt>, Error> {
        self.transact(async |tx| {
            management(tx, app, request)
                .await?
                .as_ref()
                .map(receipt_from_row)
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
    fn from_row(row: &Row) -> Result<Self, Error> {
        Ok(Self {
            operation: get(row, "operation")?,
            name: get(row, "restart_name")?,
            occurrence: get(row, "restart_occurrence")?,
            deploy: get(row, "restart_deploy")?,
        })
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
                                    .map_err(|_| Error::Unavailable)?,
                            })
                        })
                        .transpose()?,
                    deploy: self
                        .deploy
                        .as_deref()
                        .map(|deploy| match deploy {
                            "started" => Ok(RestartDeploy::Started),
                            "latest" => Ok(RestartDeploy::Latest),
                            _ => Err(Error::Unavailable),
                        })
                        .transpose()?,
                },
            },
            _ => return Err(Error::Unavailable),
        };
        if Self::encode(&command).map_err(|_| Error::Unavailable)? != *self {
            return Err(Error::Unavailable);
        }
        Ok(command)
    }
}
async fn management(
    tx: &Transaction<'_>,
    app: &AppId,
    request: &RequestId,
) -> Result<Option<Row>, Error> {
    Ok(tx.query_opt(
        "SELECT app_id,request_id,run_id,actor,operation,restart_name,restart_occurrence,restart_deploy,outcome,run_state
         FROM workflow_coordination.management WHERE app_id=$1 AND request_id=$2", &[&app.as_str(), &request.as_str()],
    ).await?)
}
fn outcome_from_row(row: &Row) -> Result<Option<ManagementOutcome>, Error> {
    let state: Option<&str> = get(row, "run_state")?;
    Ok(match (get::<Option<&str>>(row, "outcome")?, state) {
        (None, None) => None,
        (Some("applied"), Some(state)) => Some(ManagementOutcome::Applied {
            state: state.parse().map_err(|_| Error::Unavailable)?,
        }),
        (Some("not_found"), None) => Some(ManagementOutcome::NotFound {}),
        (Some("conflict"), None) => Some(ManagementOutcome::Conflict {}),
        (Some("denied"), None) => Some(ManagementOutcome::Denied {}),
        _ => return Err(Error::Unavailable),
    })
}
fn receipt_from_row(row: &Row) -> Result<ManagementReceipt, Error> {
    Ok(ManagementReceipt {
        app_id: AppId::parse(get(row, "app_id")?).map_err(|_| Error::Unavailable)?,
        request_id: RequestId::parse(get(row, "request_id")?).map_err(|_| Error::Unavailable)?,
        outcome: outcome_from_row(row)?,
    })
}
