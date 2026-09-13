use super::{
    models,
    policy::CapturedPolicy,
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
    pub(crate) bound_policy: Option<super::PolicyBinding>,
    operation_policy: Option<CapturedPolicy>,
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
    pub(crate) binding: super::PolicyBinding,
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
            bound_policy: None,
            operation_policy: None,
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
    /// Retain the host's exact app authority, including across asynchronous calls.
    ///
    /// # Errors
    /// Refuses a binding from another registry or retargeting an app-bound service.
    pub fn bind_app(
        &self,
        binding: &super::PolicyBinding,
    ) -> Result<AppWorkflows, WorkflowServiceError> {
        if !binding.belongs_to(&self.policies) || self.bound_policy.is_some() {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        let mut service = self.clone();
        service.bound_policy = Some(binding.clone());
        Ok(AppWorkflows {
            service,
            app: binding.app_id().clone(),
            binding: binding.clone(),
        })
    }

    #[expect(
        clippy::future_not_send,
        reason = "Compio drives the journal on its owning runtime thread"
    )]
    pub(crate) async fn begin(&self) -> Result<Transaction, WorkflowServiceError> {
        let captured = self.capture_policy();
        let mut tx = match &captured {
            Some(captured) => captured.run(self.store.begin()).await?,
            None => self.store.begin().await?,
        };
        tx.observed_policy = captured;
        tx.policies = Some(self.policies.clone());
        tx.policy_binding.clone_from(&self.bound_policy);
        Ok(tx)
    }

    /// Open immutable history without using a live mutation capability.
    #[expect(
        clippy::future_not_send,
        reason = "history uses its owning compio thread"
    )]
    pub(super) async fn begin_history(&self) -> Result<Transaction, WorkflowServiceError> {
        let mut tx = self.store.begin().await?;
        tx.policies = Some(self.policies.clone());
        tx.policy_binding.clone_from(&self.bound_policy);
        Ok(tx)
    }

    pub(crate) fn with_authority(
        &self,
        authority: super::policy::PolicyAuthority,
    ) -> Result<Self, WorkflowServiceError> {
        if self
            .bound_policy
            .as_ref()
            .is_none_or(|binding| !authority.belongs_to(binding))
        {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        authority.check()?;
        let mut service = self.clone();
        service.operation_policy = Some(CapturedPolicy::retained(authority));
        Ok(service)
    }

    pub(super) fn capture_policy(&self) -> Option<CapturedPolicy> {
        self.operation_policy
            .clone()
            .or_else(|| self.bound_policy.as_ref().map(CapturedPolicy::capture))
    }

    pub(super) fn run_bound<'a, T: 'a>(
        &self,
        operation: impl FnOnce(Self) -> futures::future::LocalBoxFuture<'a, Result<T, WorkflowServiceError>>
            + 'a,
    ) -> futures::future::LocalBoxFuture<'a, Result<T, WorkflowServiceError>> {
        let captured = self.capture_policy();
        let mut service = self.clone();
        service.operation_policy.clone_from(&captured);
        Box::pin(async move {
            let operation = operation(service);
            match captured {
                Some(captured) => captured.run(operation).await,
                None => operation.await,
            }
        })
    }

    pub(crate) fn policy_for(&self, app: &AppId) -> Result<AppPolicy, WorkflowServiceError> {
        if let Some(binding) = &self.bound_policy {
            if binding.app_id() != app {
                return Err(WorkflowServiceError::PermissionDenied);
            }
            return binding.resolve();
        }
        self.policies.resolve(app)
    }

    /// Register the app selected by an already configured host binding.
    /// Policy installation and revocation remain independent of creator storage.
    ///
    /// # Errors
    /// Rejects foreign or unavailable authority and reports journal storage failures.
    pub async fn register_app(
        &self,
        binding: &super::PolicyBinding,
    ) -> Result<AppWorkflows, WorkflowServiceError> {
        let scope = self.bind_app(binding)?;
        let captured = CapturedPolicy::capture(binding);
        captured
            .run(async {
                captured.check()?;
                let tx = scope.service.begin().await?;
                let app = scope.app_id();
                tx.database()
                    .collection(models::app_state::Entity::COLLECTION)?
                    .execute(Operation::Upsert {
                        document: value!({"id":app.as_str(), "app_id":app.as_str()}),
                        conflict_fields: value!(["app_id"]),
                    })
                    .await?;
                captured.check()?;
                tx.commit().await
            })
            .await?;
        Ok(scope)
    }
}

impl AppWorkflows {
    pub(super) fn capture_policy(&self) -> CapturedPolicy {
        self.service
            .capture_policy()
            .expect("app workflow policy binding")
    }

    pub(crate) fn with_authority(
        mut self,
        authority: super::policy::PolicyAuthority,
    ) -> Result<Self, WorkflowServiceError> {
        if !authority.belongs_to(&self.binding) {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        authority.check()?;
        self.service = self.service.with_authority(authority)?;
        Ok(self)
    }

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
        let captured = self.capture_policy();
        captured
            .run(self.start_captured(request_id, name, options, &captured))
            .await
    }

    async fn start_captured(
        &self,
        request_id: &RequestId,
        name: &str,
        options: StartOptions,
        captured: &CapturedPolicy,
    ) -> Result<StartedRun, WorkflowServiceError> {
        validation::workflow_name(name)?;
        validation::start(&options)?;
        let digest = digest(&(name, &options))?;
        let mut tx = self.service.begin().await?;
        lock_app_state(&mut tx, &self.app).await?;
        let now = tx.now().await?;
        if let Some(receipt) = request_result(&tx, &self.app, request_id, "start", &digest).await? {
            return Ok(receipt);
        }
        captured.recheck()?;
        let policy = &captured.authority()?.policy;
        policy.admit()?;
        captured.check()?;
        if encode(&options.input)?.len() > policy.max_input_bytes {
            return Err(WorkflowServiceError::PayloadTooLarge);
        }
        let deploy = active_deploy(&mut tx, &self.app).await?;
        if !deploy.workflows.contains(name) {
            return Err(not_found("workflow"));
        }
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
                        let runs = tx.database().collection(models::runs::Entity::COLLECTION)?;
                        runs.update(
                            value!({"app_id":self.app.as_str(), "id":existing.id.clone()}),
                            value!({"$set":{"key":null, "control":"cancel"}}),
                        )
                        .await?;
                        let woke = runs.execute(Operation::Update {
                            filter: value!({"app_id":self.app.as_str(), "id":existing.id.clone(), "task_id":null}),
                            patch: value!({"$set":{"due_at":now}}), many: true,
                        }).await?;
                        if matches!(woke, Output::Count(1)) {
                            super::publication::advance(&tx, &self.app, &existing.id, now).await?;
                        }
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
            &mut tx, &self.app, request_id, "start", &digest, &result, now,
        )
        .await?;
        captured.check()?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn status(&self, run_id: &str) -> Result<RunStatus, WorkflowServiceError> {
        validate_run(run_id)?;
        let tx = self.service.begin_history().await?;
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
        let captured = self.capture_policy();
        captured
            .run(async {
                validate_run(run_id)?;
                validation::signal_type(&options.signal_type)?;
                let digest = digest(&(run_id, &options))?;
                let mut tx = self.service.begin().await?;
                lock_app_state(&mut tx, &self.app).await?;
                let now = tx.now().await?;
                if let Some(receipt) =
                    request_result(&tx, &self.app, request_id, "signal", &digest).await?
                {
                    return Ok(receipt);
                }
                captured.check()?;
                let policy = &captured.authority()?.policy;
                if encode(&options.payload)?.len() > policy.max_input_bytes {
                    return Err(WorkflowServiceError::PayloadTooLarge);
                }
                let result =
                    super::signals::deliver(&mut tx, &self.app, run_id, &options, "app", now)
                        .await?;
                store_request(
                    &mut tx, &self.app, request_id, "signal", &digest, &result, now,
                )
                .await?;
                captured.check()?;
                tx.commit().await?;
                Ok(result)
            })
            .await
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
    tx.capture_mutation(app)?;
    lock_app_state(tx, app).await?;
    tx.policy(app)
}

/// Serialize journal state without granting permission for a fresh mutation.
/// Receipt replay uses this path before consulting live policy authority.
#[expect(
    clippy::future_not_send,
    reason = "app locks use the creator transaction thread"
)]
#[expect(
    clippy::needless_pass_by_ref_mut,
    reason = "app lock acquisition is an exclusive transaction operation"
)]
pub(super) async fn lock_app_state(
    tx: &mut Transaction,
    app: &AppId,
) -> Result<(), WorkflowServiceError> {
    tx.check_app(app)?;
    let Output::Count(locked) = tx
        .database()
        .collection(models::app_state::Entity::COLLECTION)?
        .execute(Operation::Update {
            filter: value!({"app_id":app.as_str()}),
            patch: value!({"$inc":{"signal_epoch":0}}),
            many: true,
        })
        .await?
    else {
        return Err(WorkflowServiceError::Internal(
            "workflow lock returned rows".into(),
        ));
    };
    if locked != 1 {
        return Err(not_found("workflow app"));
    }
    Ok(())
}

pub(crate) async fn lock_run(
    tx: &mut Transaction,
    app: &AppId,
    run_id: &str,
) -> Result<Row, WorkflowServiceError> {
    let runs = tx.database().collection(models::runs::Entity::COLLECTION)?;
    // All journal mutations first lock the app. Retain a row lock as well so
    // maintenance and task fencing share the same serialization point.
    let Output::Count(locked) = runs
        .execute(Operation::Update {
            filter: value!({"app_id":app.as_str(), "id":run_id}),
            patch: value!({"$inc":{"lease_epoch":0}}),
            many: true,
        })
        .await?
    else {
        return Err(WorkflowServiceError::Internal(
            "workflow lock returned rows".into(),
        ));
    };
    if locked != 1 {
        return Err(not_found("workflow run"));
    }
    let Output::Rows { rows, .. } = runs
        .find(
            value!({"app_id":app.as_str(), "id":run_id}),
            value!({"limit":1}),
        )
        .await?
    else {
        return Err(WorkflowServiceError::Internal(
            "workflow read returned count".into(),
        ));
    };
    rows.into_iter()
        .next()
        .map(Row)
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
            "id":super::types::storage_id(), "app_id":app.as_str(), "run_id":id, "generation":0, "deploy_id":deploy,
            "input":encode(&options.input)?, "state":"queued", "started_at":now,
        }))
        .await?;
    super::publication::record(tx, app, id, now).await?;
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
    tx: &Transaction,
    app: &AppId,
    id: &RequestId,
    operation: &str,
    digest: &str,
) -> Result<Option<T>, WorkflowServiceError> {
    let rows = tx
        .database()
        .entity::<models::requests::Entity>()?
        .find::<models::RequestResult>(
            models::requests::app_id
                .eq(app.as_str())?
                .and(models::requests::request_id.eq(id.as_str())?),
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
    created_at: i64,
) -> Result<(), WorkflowServiceError> {
    tx.database()
        .collection(models::requests::Entity::COLLECTION)?
        .insert(value!({
            "id":super::types::storage_id(), "app_id":app.as_str(), "request_id":id.as_str(), "operation":operation, "digest":digest,
            "result":encode(result)?, "created_at":created_at,
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
    let outbox = tx
        .database()
        .collection(models::outbox::Entity::COLLECTION)?;
    let Output::Count(count) = outbox
        .count(value!({"app_id":app.as_str(), "id":id}), value!({}))
        .await?
    else {
        return Err(WorkflowServiceError::Internal(
            "workflow count returned rows".into(),
        ));
    };
    if count == 0 {
        outbox.insert(value!({"app_id":app.as_str(), "id":id, "kind":kind, "payload":encode(&payload)?, "created_at":now})).await?;
    }
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
