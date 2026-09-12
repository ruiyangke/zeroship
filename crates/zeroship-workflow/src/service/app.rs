use super::{
    models,
    store::{OrmStore, Row, Transaction},
    types::{digest, AppPolicy, DeployRegistration, RequestId},
};
use crate::{
    operations::{
        ConflictPolicy, DeliveredSignal, RunState, RunStatus, SignalOptions, StartOptions,
        StartedRun,
    },
    validation, WorkflowServiceError,
};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::json;
use std::rc::Rc;
use std::sync::Arc;
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_data_orm::{
    orm::{Entity, FindOptions, Operation, Output},
    sql::Predicate,
    value,
};

#[derive(Clone)]
pub struct WorkflowService {
    pub(crate) store: Rc<OrmStore>,
    pub(crate) policies: Arc<super::HostPolicies>,
    pub(crate) deployments: Option<super::AppDeployments>,
    pub(crate) signal_authority: Option<Arc<super::SignalAuthority>>,
    pub(crate) payload_storage: Option<zeroship_storage::Storage>,
}
impl std::fmt::Debug for WorkflowService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowService").finish_non_exhaustive()
    }
}
#[derive(Debug, Clone)]
pub struct AppWorkflows {
    pub(crate) service: WorkflowService,
    pub(crate) app: AppId,
}

impl WorkflowService {
    pub async fn verify(&self) -> Result<(), WorkflowServiceError> {
        self.store.verify().await
    }
    /// Open an already provisioned customer journal with trusted host policy.
    ///
    /// # Errors
    /// Refuses unavailable or incompatible journal storage.
    pub async fn open(
        store: Rc<OrmStore>,
        policies: Arc<super::HostPolicies>,
    ) -> Result<Self, WorkflowServiceError> {
        store.verify().await?;
        Ok(Self {
            store,
            policies,
            deployments: None,
            signal_authority: None,
            payload_storage: None,
        })
    }
    #[must_use]
    pub fn with_signal_authority(mut self, authority: Arc<super::SignalAuthority>) -> Self {
        self.signal_authority = Some(authority);
        self
    }
    /// Bind an app whose identity the host has already authorized.
    #[must_use]
    pub fn for_app(&self, app: AppId) -> AppWorkflows {
        AppWorkflows {
            service: self.clone(),
            app,
        }
    }

    #[expect(
        clippy::future_not_send,
        reason = "Compio drives the journal on its owning runtime thread"
    )]
    pub(crate) async fn begin(&self) -> Result<Transaction, WorkflowServiceError> {
        let mut tx = self.store.begin().await?;
        tx.policies = Some(self.policies.clone());
        Ok(tx)
    }

    /// Register an app using policy already authorized by the trusted host.
    ///
    /// Policy installation takes effect before the journal transaction, including
    /// when that transaction fails. Storage failure cannot roll back revocation.
    ///
    /// # Errors
    /// Rejects stale or conflicting policy and reports journal storage failures.
    pub async fn register_app(
        &self,
        app: &AppId,
        policy: super::PolicySnapshot,
    ) -> Result<(), WorkflowServiceError> {
        self.policies.install(app, policy)?;
        let mut tx = self.begin().await?;
        let table = tx.table("app_state");
        tx.execute(&format!("INSERT INTO {table} (app_id,signal_epoch) VALUES ($1,0) ON CONFLICT (app_id) DO NOTHING"),
            &[app.as_str().into()]).await?;
        tx.commit().await
    }
}

impl AppWorkflows {
    #[must_use]
    pub fn app_id(&self) -> &AppId {
        &self.app
    }

    pub async fn start(
        &self,
        request_id: &RequestId,
        name: &str,
        options: StartOptions,
    ) -> Result<StartedRun, WorkflowServiceError> {
        validation::workflow_name(name)?;
        validation::start(&options)?;
        let digest = digest(&(name, &options))?;
        let mut tx = self.service.begin().await?;
        let policy = lock_app(&mut tx, &self.app).await?;
        let now = tx.now().await?;
        if let Some(receipt) =
            request_result(&mut tx, &self.app, request_id, "start", &digest, now).await?
        {
            return Ok(receipt);
        }
        policy.admit()?;
        if encode(&options.input)?.len() > policy.max_input_bytes {
            return Err(WorkflowServiceError::PayloadTooLarge);
        }
        let deploy = active_deploy(&mut tx, &self.app).await?;
        if !deploy.workflows.contains(name) {
            return Err(not_found("workflow"));
        }
        let runs = tx.table("runs");
        let mut joined = None;
        if let Some(key) = &options.key {
            if let Some(existing) = keyed_run(&tx, &self.app, name, key).await? {
                let state = parse_state(&existing.state)?;
                match options.on_conflict {
                    ConflictPolicy::Join => {
                        joined = Some(StartedRun {
                            id: existing.id,
                            state,
                        });
                    }
                    ConflictPolicy::Reject => {
                        return Err(WorkflowServiceError::Conflict(
                            "workflow run already exists for key".into(),
                        ))
                    }
                    ConflictPolicy::Replace => {
                        // Release the business key while cancellation is durable. The
                        // incumbent's accepted frontier still belongs to its task.
                        tx.execute(&format!("UPDATE {runs} SET key=NULL,control='cancel',due_at=CASE WHEN task_id IS NULL THEN $3 ELSE due_at END WHERE app_id=$1 AND id=$2"),
                            &[self.app.as_str().into(),existing.id.into(),now.into()]).await?;
                    }
                }
            }
        }
        let result = if let Some(joined) = joined {
            joined
        } else {
            if live_runs(&tx, &self.app).await? >= policy.max_live_runs {
                return Err(WorkflowServiceError::ResourceExhausted(
                    "workflow live-run limit reached".into(),
                ));
            }
            let id = typed_id::new_workflow_run_id();
            insert_root_run(&mut tx, &self.app, &id, name, &deploy.id, &options, now).await?;
            StartedRun {
                id,
                state: RunState::Queued,
            }
        };
        store_request(
            &mut tx,
            &self.app,
            request_id,
            "start",
            &digest,
            &result,
            deadline(now, policy.request_retention_ms)?,
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn status(&self, run_id: &str) -> Result<RunStatus, WorkflowServiceError> {
        validate_run(run_id)?;
        let tx = self.service.begin().await?;
        let (run, outcome) = current_run(&tx, &self.app, run_id)
            .await?
            .ok_or_else(|| not_found("workflow run"))?;
        let status = RunStatus {
            state: parse_state(&run.state)?,
            output: if let Some(reference) = outcome.output_ref {
                let reference: crate::engine::WorkflowOutputRef = decode(&reference)?;
                Some(
                    json!({"kind":"ref","ref":format!("wfblob:sha256:{}",reference.hash),"hash":reference.hash,"size":reference.size,"contentType":reference.content_type}),
                )
            } else {
                outcome.output.map(|value| decode(&value)).transpose()?
            },
            error: outcome.error.map(|value| decode(&value)).transpose()?,
        };
        tx.commit().await?;
        Ok(status)
    }

    pub async fn signal(
        &self,
        request_id: &RequestId,
        run_id: &str,
        options: SignalOptions,
    ) -> Result<DeliveredSignal, WorkflowServiceError> {
        validate_run(run_id)?;
        validation::signal_type(&options.signal_type)?;
        let digest = digest(&(run_id, &options))?;
        let mut tx = self.service.begin().await?;
        let policy = lock_app(&mut tx, &self.app).await?;
        let now = tx.now().await?;
        if let Some(receipt) =
            request_result(&mut tx, &self.app, request_id, "signal", &digest, now).await?
        {
            return Ok(receipt);
        }
        if encode(&options.payload)?.len() > policy.max_input_bytes {
            return Err(WorkflowServiceError::PayloadTooLarge);
        }
        let result =
            super::signals::deliver(&mut tx, &self.app, run_id, &options, "app", now).await?;
        store_request(
            &mut tx,
            &self.app,
            request_id,
            "signal",
            &digest,
            &result,
            deadline(now, policy.request_retention_ms)?,
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }
}

pub(crate) async fn keyed_run(
    tx: &Transaction,
    app: &AppId,
    workflow: &str,
    key: &str,
) -> Result<Option<models::KeyedRun>, WorkflowServiceError> {
    Ok(tx
        .database()
        .entity::<models::runs::Entity>()?
        .find::<models::KeyedRun>(
            models::runs::app_id
                .eq(app.as_str())?
                .and(models::runs::workflow_name.eq(workflow)?)
                .and(models::runs::key.eq(Some(key))?),
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next())
}

pub(crate) async fn live_runs(tx: &Transaction, app: &AppId) -> Result<i64, WorkflowServiceError> {
    let output = tx
        .database()
        .collection(models::runs::Entity::COLLECTION)?
        .count(
            value!({"app_id":app.as_str(), "state":{"$nin":["completed","failed","cancelled"]}}),
            value!({}),
        )
        .await?;
    match output {
        Output::Count(count) => Ok(count),
        _ => Err(WorkflowServiceError::Internal(
            "workflow count returned rows".into(),
        )),
    }
}

/// Read the run and its current generation in one database snapshot.
pub(crate) async fn current_run(
    tx: &Transaction,
    app: &AppId,
    id: &str,
) -> Result<Option<(models::RunHead, models::GenerationOutcome)>, WorkflowServiceError> {
    let db = tx.database();
    let run = db.entity::<models::runs::Entity>()?.alias("r")?;
    let generation = db.entity::<models::generations::Entity>()?.alias("g")?;
    Ok(db
        .from(&run)
        .inner_join(
            &generation,
            Predicate::And(vec![
                run.column(models::runs::app_id)
                    .eq_column(generation.column(models::generations::app_id))?,
                run.column(models::runs::id)
                    .eq_column(generation.column(models::generations::run_id))?,
                run.column(models::runs::generation)
                    .eq_column(generation.column(models::generations::generation))?,
            ]),
        )?
        .filter(Predicate::And(vec![
            run.column(models::runs::app_id).eq(app.as_str())?,
            run.column(models::runs::id).eq(id)?,
        ]))
        .select((
            run.row::<models::RunHead>(),
            generation.row::<models::GenerationOutcome>(),
        ))?
        .limit(1)?
        .all()
        .await?
        .into_iter()
        .next())
}

pub(crate) async fn lock_app(
    tx: &mut Transaction,
    app: &AppId,
) -> Result<AppPolicy, WorkflowServiceError> {
    let sql = format!(
        "SELECT app_id FROM {} WHERE app_id=$1{}",
        tx.table("app_state"),
        tx.lock_clause()
    );
    let rows = tx.query(&sql, &[app.as_str().into()]).await?;
    if rows.is_empty() {
        return Err(not_found("workflow app"));
    }
    tx.policies
        .as_ref()
        .ok_or_else(|| WorkflowServiceError::Unavailable("workflow host policy not bound".into()))?
        .resolve(app)
}

pub(crate) async fn lock_run(
    tx: &mut Transaction,
    app: &AppId,
    run_id: &str,
) -> Result<Row, WorkflowServiceError> {
    let sql = format!(
        "SELECT * FROM {} WHERE app_id=$1 AND id=$2{}",
        tx.table("runs"),
        tx.lock_clause()
    );
    tx.query(&sql, &[app.as_str().into(), run_id.into()])
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| not_found("workflow run"))
}
pub(crate) async fn active_deploy(
    tx: &mut Transaction,
    app: &AppId,
) -> Result<DeployRegistration, WorkflowServiceError> {
    let rows = tx
        .database()
        .entity::<models::deploys::Entity>()?
        .find::<models::DeploymentManifest>(
            models::deploys::app_id
                .eq(app.as_str())?
                .and(models::deploys::active.eq(1_i64)?)
                .and(models::deploys::state.eq("available")?),
            FindOptions {
                limit: Some(2),
                ..Default::default()
            },
        )
        .await?;
    if rows.len() != 1 {
        return Err(WorkflowServiceError::Unavailable(
            "workflow app has no active executable deployment".into(),
        ));
    }
    decode(&rows[0].manifest)
}
pub(crate) async fn insert_root_run(
    tx: &mut Transaction,
    app: &AppId,
    id: &str,
    name: &str,
    deploy: &str,
    options: &StartOptions,
    now: i64,
) -> Result<(), WorkflowServiceError> {
    tx.database()
        .collection(models::runs::Entity::COLLECTION)?
        .insert(value!({
            "app_id":app.as_str(), "id":id, "workflow_name":name, "deploy_id":deploy,
            "generation":0, "state":"queued", "control":"none", "due_at":now,
            "lease_epoch":0, "key":options.key.clone(), "cascade":0, "depth":0,
            "created_at":now, "signal_epoch":0,
        }))
        .await?;
    tx.database()
        .collection(models::generations::Entity::COLLECTION)?
        .insert(value!({
            "app_id":app.as_str(), "run_id":id, "generation":0, "deploy_id":deploy,
            "input":encode(&options.input)?, "state":"queued", "started_at":now,
        }))
        .await?;
    emit(
        tx,
        app,
        &format!("{id}:start"),
        "workflow.start",
        json!({"runId":id}),
        now,
    )
    .await
}
pub(crate) async fn request_result<T: DeserializeOwned>(
    tx: &mut Transaction,
    app: &AppId,
    id: &RequestId,
    operation: &str,
    digest: &str,
    now: i64,
) -> Result<Option<T>, WorkflowServiceError> {
    tx.database()
        .collection(models::requests::Entity::COLLECTION)?
        .execute(Operation::Purge {
            filter: value!({"app_id":app.as_str(), "id":id.as_str(), "expires_at":{"$lte":now}}),
            many: false,
        })
        .await?;
    let rows = tx
        .database()
        .entity::<models::requests::Entity>()?
        .find::<models::RequestResult>(
            models::requests::app_id
                .eq(app.as_str())?
                .and(models::requests::id.eq(id.as_str())?),
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    if row.operation != operation || row.digest != digest {
        return Err(WorkflowServiceError::Conflict(
            "workflow request identity was reused with another operation or body".into(),
        ));
    }
    Ok(Some(decode(&row.result)?))
}
pub(crate) async fn store_request<T: Serialize>(
    tx: &mut Transaction,
    app: &AppId,
    id: &RequestId,
    operation: &str,
    digest: &str,
    result: &T,
    expires_at: i64,
) -> Result<(), WorkflowServiceError> {
    tx.database()
        .collection(models::requests::Entity::COLLECTION)?
        .insert(value!({
            "app_id":app.as_str(), "id":id.as_str(), "operation":operation, "digest":digest,
            "result":encode(result)?, "expires_at":expires_at,
        }))
        .await?;
    Ok(())
}
pub(crate) async fn emit(
    tx: &mut Transaction,
    app: &AppId,
    id: &str,
    kind: &str,
    payload: serde_json::Value,
    now: i64,
) -> Result<(), WorkflowServiceError> {
    let table = tx.table("outbox");
    tx.execute(&format!("INSERT INTO {table} (app_id,id,kind,payload,created_at) VALUES ($1,$2,$3,$4,$5) ON CONFLICT (app_id,id) DO NOTHING"),
        &[app.as_str().into(),id.into(),kind.into(),encode(&payload)?.into(),now.into()]).await?;
    Ok(())
}
pub(crate) fn validate_run(id: &str) -> Result<(), WorkflowServiceError> {
    typed_id::parse_with_prefix(id, typed_id::WORKFLOW_RUN_PREFIX)
        .map_err(|_| not_found("workflow run"))?;
    Ok(())
}
pub(crate) fn not_found(name: &str) -> WorkflowServiceError {
    WorkflowServiceError::NotFound(format!("{name} not found"))
}
pub(crate) fn parse_state(value: &str) -> Result<RunState, WorkflowServiceError> {
    value.parse().map_err(WorkflowServiceError::Internal)
}
pub(crate) fn encode<T: Serialize>(value: &T) -> Result<String, WorkflowServiceError> {
    serde_json::to_string(value)
        .map_err(|_| WorkflowServiceError::Internal("encode workflow record".into()))
}
pub(crate) fn decode<T: DeserializeOwned>(value: &str) -> Result<T, WorkflowServiceError> {
    serde_json::from_str(value)
        .map_err(|_| WorkflowServiceError::Internal("invalid persisted workflow record".into()))
}
pub(crate) fn deadline(now: i64, duration: i64) -> Result<i64, WorkflowServiceError> {
    now.checked_add(duration).ok_or_else(|| {
        WorkflowServiceError::InvalidRequest("workflow deadline is out of range".into())
    })
}
