use super::{
    store::{Row, Transaction, WorkflowStore},
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
use std::sync::Arc;
use zeroship_core::{app_id::AppId, typed_id};

#[derive(Clone)]
pub struct WorkflowService {
    pub(crate) store: Arc<dyn WorkflowStore>,
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
    pub async fn open(store: Arc<dyn WorkflowStore>) -> Result<Self, WorkflowServiceError> {
        store.verify().await?;
        Ok(Self {
            store,
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

    /// Install host policy. This is a trusted composition operation, never an app API.
    pub async fn register_app(
        &self,
        app: &AppId,
        policy: &AppPolicy,
    ) -> Result<(), WorkflowServiceError> {
        policy.validate()?;
        let mut tx = self.store.begin().await?;
        if tx.platform_policy.is_some() {
            return Err(WorkflowServiceError::InvalidRequest(
                "platform workflow policy is owned by Control".into(),
            ));
        }
        let table = tx.table("app_state");
        tx.execute(&format!("INSERT INTO {table} (app_id,revision,policy,signal_epoch) VALUES ($1,0,$2,0) \
            ON CONFLICT (app_id) DO UPDATE SET revision = {table}.revision + 1, policy = excluded.policy"),
            &[app.as_str().into(), encode(policy)?.into()]).await?;
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
        let mut tx = self.service.store.begin().await?;
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
        let deploy = active_deploy(&mut tx, &self.app, &policy).await?;
        if !deploy.workflows.contains(name) {
            return Err(not_found("workflow"));
        }
        let runs = tx.table("runs");
        let mut joined = None;
        if let Some(key) = &options.key {
            let rows = tx.query(&format!("SELECT id,state FROM {runs} WHERE app_id=$1 AND workflow_name=$2 AND key=$3"),
                &[self.app.as_str().into(),name.into(),key.clone().into()]).await?;
            if let Some(existing) = rows.first() {
                let state = parse_state(&existing.text("state")?)?;
                match options.on_conflict {
                    ConflictPolicy::Join => {
                        joined = Some(StartedRun {
                            id: existing.text("id")?,
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
                            &[self.app.as_str().into(),existing.text("id")?.into(),now.into()]).await?;
                    }
                }
            }
        }
        let result = if let Some(joined) = joined {
            joined
        } else {
            let live = tx.query(&format!("SELECT COUNT(*) AS total FROM {runs} WHERE app_id=$1 AND state NOT IN ('completed','failed','cancelled')"), &[self.app.as_str().into()]).await?;
            if live[0].integer("total")? >= policy.max_live_runs {
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
        let mut tx = self.service.store.begin().await?;
        let runs = tx.table("runs");
        let generations = tx.table("generations");
        let rows=tx.query(&format!("SELECT r.state,g.output,g.output_ref,g.error FROM {runs} r JOIN {generations} g ON g.app_id=r.app_id AND g.run_id=r.id AND g.generation=r.generation WHERE r.app_id=$1 AND r.id=$2"), &[self.app.as_str().into(),run_id.into()]).await?;
        let row = rows.first().ok_or_else(|| not_found("workflow run"))?;
        let status = RunStatus {
            state: parse_state(&row.text("state")?)?,
            output: if let Some(reference) = row.optional_text("output_ref")? {
                let reference: crate::engine::WorkflowOutputRef = decode(&reference)?;
                Some(
                    json!({"kind":"ref","ref":format!("wfblob:sha256:{}",reference.hash),"hash":reference.hash,"size":reference.size,"contentType":reference.content_type}),
                )
            } else {
                row.optional_text("output")?
                    .map(|value| decode(&value))
                    .transpose()?
            },
            error: row
                .optional_text("error")?
                .map(|value| decode(&value))
                .transpose()?,
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
        let mut tx = self.service.store.begin().await?;
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

pub(crate) async fn lock_app(
    tx: &mut Transaction,
    app: &AppId,
) -> Result<AppPolicy, WorkflowServiceError> {
    let platform = if let Some(source) = tx.platform_policy.clone() {
        let policy = source.lock(tx, app).await?;
        tx.execute(
            &format!("INSERT INTO {} (app_id,revision,signal_epoch) VALUES ($1,0,0) ON CONFLICT (app_id) DO NOTHING", tx.table("app_state")),
            &[app.as_str().into()],
        ).await?;
        Some(policy)
    } else {
        None
    };
    let sql = format!(
        "SELECT policy FROM {} WHERE app_id=$1{}",
        tx.table("app_state"),
        tx.lock_clause()
    );
    let rows = tx.query(&sql, &[app.as_str().into()]).await?;
    let row = rows.first().ok_or_else(|| not_found("workflow app"))?;
    match platform {
        Some(policy) => Ok(policy),
        None => decode(&row.text("policy")?),
    }
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
    policy: &AppPolicy,
) -> Result<DeployRegistration, WorkflowServiceError> {
    super::deploys::reconcile_platform(tx, app, policy).await?;
    let table = tx.table("deploys");
    let rows = tx
        .query(
            &format!(
                "SELECT manifest FROM {table} WHERE app_id=$1 AND active=1 AND state='available'"
            ),
            &[app.as_str().into()],
        )
        .await?;
    if rows.len() != 1 {
        return Err(WorkflowServiceError::Unavailable(
            "workflow app has no active executable deployment".into(),
        ));
    }
    decode(&rows[0].text("manifest")?)
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
    let runs = tx.table("runs");
    let generations = tx.table("generations");
    tx.execute(&format!("INSERT INTO {runs} (app_id,id,workflow_name,deploy_id,generation,state,control,due_at,lease_epoch,key,cascade,depth,created_at,signal_epoch) \
        VALUES ($1,$2,$3,$4,0,'queued','none',$5,0,$6,0,0,$5,0)"),
        &[app.as_str().into(),id.into(),name.into(),deploy.into(),now.into(),options.key.clone().into()]).await?;
    tx.execute(&format!("INSERT INTO {generations} (app_id,run_id,generation,deploy_id,input,state,started_at) VALUES ($1,$2,0,$3,$4,'queued',$5)"),
        &[app.as_str().into(),id.into(),deploy.into(),encode(&options.input)?.into(),now.into()]).await?;
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
    let table = tx.table("requests");
    tx.execute(
        &format!("DELETE FROM {table} WHERE app_id=$1 AND id=$2 AND expires_at <= $3"),
        &[app.as_str().into(), id.as_str().into(), now.into()],
    )
    .await?;
    let rows = tx
        .query(
            &format!("SELECT operation,digest,result FROM {table} WHERE app_id=$1 AND id=$2"),
            &[app.as_str().into(), id.as_str().into()],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    if row.text("operation")? != operation || row.text("digest")? != digest {
        return Err(WorkflowServiceError::Conflict(
            "workflow request identity was reused with another operation or body".into(),
        ));
    }
    Ok(Some(decode(&row.text("result")?)?))
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
    let table = tx.table("requests");
    tx.execute(&format!("INSERT INTO {table} (app_id,id,operation,digest,result,expires_at) VALUES ($1,$2,$3,$4,$5,$6)"),
        &[app.as_str().into(),id.as_str().into(),operation.into(),digest.into(),encode(result)?.into(),expires_at.into()]).await?;
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
