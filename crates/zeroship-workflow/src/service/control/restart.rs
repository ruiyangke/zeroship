use super::super::{
    app::{active_deploy, emit, live_runs},
    deployment_retention::admission_generation,
    deployments::unavailable,
    deploys, journal,
    store::Row,
    DeployRegistration,
};
use super::{
    lock_run, models, parse_state, replay, value, AppId, AppPolicy, Entity, Preparation, Rejection,
    RestartOptions, RestartedRun, RunState, Transaction, WorkflowServiceError,
};
use crate::service::policy::admit;
use crate::{
    deployment_holds::HoldScope, engine::StepCheckpoint, lifecycle::RestartSafety,
    operations::RestartDeploy,
};
use serde_json::json;
use std::collections::BTreeSet;
use zeroship_data_orm::{
    orm::{FindOptions, FromRow, Operation, Output},
    sql::RowLimit,
};

const MAX_DESCENDANT_INSPECTIONS: usize = 16_384;

#[derive(FromRow)]
#[orm(entity = models::generations)]
struct SourceGeneration {
    deploy_id: String,
    input: String,
    input_ref: Option<String>,
}

struct RestartContext<'tx> {
    tx: &'tx mut Transaction,
    app: AppId,
    run_id: String,
    now: i64,
}

/// Lifecycle preparation remains inside the caller's app-locked transaction.
pub(in crate::service) struct RestartDraft<'tx> {
    context: RestartContext<'tx>,
    run: Row,
    current: i64,
    steps: Vec<StepCheckpoint>,
    from: Option<i32>,
    previous: SourceGeneration,
    workflow: String,
    deploy_policy: RestartDeploy,
}

pub(in crate::service) struct RestartPlan<'tx> {
    context: RestartContext<'tx>,
    current: i64,
    generation: i64,
    signal_epoch: i64,
    steps: Vec<StepCheckpoint>,
    from: Option<i32>,
    deploy: String,
    previous: SourceGeneration,
}

pub(in crate::service) async fn prepare<'tx>(
    tx: &'tx mut Transaction,
    app: &AppId,
    run_id: &str,
    options: &RestartOptions,
    policy: &AppPolicy,
    now: i64,
) -> Result<Preparation<RestartPlan<'tx>>, WorkflowServiceError> {
    match prepare_draft(tx, app, run_id, options, policy, now).await? {
        Preparation::Ready(draft) => draft.bind_configured().await,
        Preparation::Rejected(reason) => Ok(Preparation::Rejected(reason)),
    }
}

/// The caller holds the app lock and keeps its original authority through commit.
pub(in crate::service) async fn prepare_draft<'tx>(
    tx: &'tx mut Transaction,
    app: &AppId,
    run_id: &str,
    options: &RestartOptions,
    policy: &AppPolicy,
    now: i64,
) -> Result<Preparation<RestartDraft<'tx>>, WorkflowServiceError> {
    tx.check_app(app)?;
    if admit(policy).is_err() {
        return Ok(Preparation::Rejected(Rejection::Denied));
    }
    let deploy_policy = match options
        .effective_deploy()
        .map_err(WorkflowServiceError::from)
    {
        Ok(policy) => policy,
        Err(WorkflowServiceError::InvalidRequest(message)) => {
            return Ok(Preparation::Rejected(Rejection::Invalid(message)))
        }
        Err(WorkflowServiceError::Conflict(message)) => {
            return Ok(Preparation::Rejected(Rejection::Conflict(message)))
        }
        Err(error) => return Err(error),
    };
    let run = match lock_run(tx, app, run_id).await {
        Ok(run) => run,
        Err(WorkflowServiceError::NotFound(_)) => {
            return Ok(Preparation::Rejected(Rejection::NotFound))
        }
        Err(error) => return Err(error),
    };
    let current = run.integer("generation")?;
    if current < 0 {
        return Err(unavailable());
    }
    let steps = journal::load(tx, app, run_id, current).await?;
    let from = if let Some(target) = &options.from {
        let matches: Vec<_> = steps
            .iter()
            .filter(|step| {
                step.name == target.name
                    && target.occurrence.is_none_or(|occurrence| {
                        i64::from(occurrence) == i64::from(step.name_occurrence)
                    })
            })
            .collect();
        if matches.len() != 1 {
            return Ok(Preparation::Rejected(Rejection::Invalid(
                "restart target is missing or ambiguous".into(),
            )));
        }
        Some(matches[0].ordinal)
    } else {
        None
    };
    let prefix = from.unwrap_or(0);
    let retained: Vec<_> = steps.iter().filter(|step| step.ordinal < prefix).collect();
    let tasks = tx
        .database()
        .collection(models::tasks::Entity::COLLECTION)?;
    if parse_state(&run.text("state")?)?.is_terminal()
        && live_runs(tx, app).await? >= policy.max_live_runs
    {
        return Err(WorkflowServiceError::ResourceExhausted(
            "workflow live-run limit reached".into(),
        ));
    }
    let Output::Count(live) = tasks
        .count(
            value!({"app_id":app.as_str(), "run_id":run_id, "generation":current,
                "state":"leased", "deadline":{"$gt":now}}),
            value!({}),
        )
        .await?
    else {
        return Err(WorkflowServiceError::Internal(
            "workflow count returned rows".into(),
        ));
    };
    let active_descendants = has_active_descendants(tx, app, run_id).await?;
    let waiting_children = has_active_children(tx, app, run_id, current).await?;
    let safety = RestartSafety {
        live_lease: live != 0,
        active_descendants: active_descendants || waiting_children,
        active_compensation: run.optional_text("compensation_target")?.is_some()
            && steps.iter().any(|step| {
                step.compensation_state
                    .as_deref()
                    .is_some_and(|state| matches!(state, "pending" | "running"))
            }),
        compensated_prefix: retained.iter().any(|step| {
            step.compensation_state
                .as_deref()
                .is_some_and(|state| state != "pending")
        }),
    }
    .check();
    if let Err(WorkflowServiceError::Conflict(message)) = safety {
        return Ok(Preparation::Rejected(Rejection::Conflict(message)));
    }
    safety?;
    // A pending cascade page would cancel the restarted generation. The
    // restart becomes admissible once the parent's obligation finishes.
    if super::super::propagation::fenced(tx, app, &run).await? {
        return Ok(Preparation::Rejected(Rejection::Conflict(
            "workflow cancellation is still propagating to this run".into(),
        )));
    }
    if retained.iter().any(|step| step.state == "running") {
        return Ok(Preparation::Rejected(Rejection::Conflict(
            "restart prefix contains unresolved operations".into(),
        )));
    }
    let previous = tx
        .database()
        .entity::<models::generations::Entity>()?
        .find::<SourceGeneration>(
            models::generations::app_id
                .eq(app.as_str())?
                .and(models::generations::run_id.eq(run_id)?)
                .and(models::generations::generation.eq(current)?),
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(unavailable)?;
    let source_deployment = run.text("deploy_id")?;
    if previous.deploy_id != source_deployment {
        return Err(unavailable());
    }
    let workflow = run.text("workflow_name")?;
    Ok(Preparation::Ready(RestartDraft {
        context: RestartContext {
            tx,
            app: app.clone(),
            run_id: run_id.to_owned(),
            now,
        },
        run,
        current,
        steps,
        from,
        previous,
        workflow,
        deploy_policy,
    }))
}

impl<'tx> RestartDraft<'tx> {
    async fn bind_configured(self) -> Result<Preparation<RestartPlan<'tx>>, WorkflowServiceError> {
        if self.deploy_policy == RestartDeploy::Latest {
            let deploy = active_deploy(self.context.tx, &self.context.app).await?;
            // Preserve ordinary lifecycle refusal before additional integrity checks.
            if !deploy.workflows.contains(&self.workflow) {
                return Ok(Preparation::Rejected(Rejection::Conflict(
                    "workflow is absent from the active deployment".into(),
                )));
            }
            if !self
                .context
                .tx
                .database()
                .entity::<models::deploys::Entity>()?
                .exists(
                    models::deploys::app_id
                        .eq(self.context.app.as_str())?
                        .and(models::deploys::id.eq(deploy.id.as_str())?)
                        .and(models::deploys::active.eq(1_i64)?)
                        .and(models::deploys::state.eq("available")?),
                )
                .await?
            {
                return Err(unavailable());
            }
            self.bind_exact(&deploy).await
        } else {
            retained_source(
                self.context.tx,
                &self.context.app,
                &self.previous.deploy_id,
                &self.workflow,
            )
            .await?;
            let deploy = self.previous.deploy_id.clone();
            self.finish(deploy).map(Preparation::Ready)
        }
    }

    /// Bind verified local metadata without selecting the current deployment.
    /// The app lock protects source, availability and retention until application.
    pub(in crate::service) async fn bind_exact(
        self,
        expected: &DeployRegistration,
    ) -> Result<Preparation<RestartPlan<'tx>>, WorkflowServiceError> {
        if self.deploy_policy != RestartDeploy::Latest {
            return Ok(Preparation::Rejected(Rejection::Conflict(
                "an explicit deployment requires a latest restart".into(),
            )));
        }
        exact_target(self.context.tx, &self.context.app, expected).await?;
        if !expected.workflows.contains(&self.workflow) {
            return Ok(Preparation::Rejected(Rejection::Conflict(
                "workflow is absent from the target deployment".into(),
            )));
        }
        self.finish(expected.id.clone()).map(Preparation::Ready)
    }

    fn finish(self, deploy: String) -> Result<RestartPlan<'tx>, WorkflowServiceError> {
        let generation = self.current.checked_add(1).ok_or_else(|| {
            WorkflowServiceError::ResourceExhausted("workflow generation exhausted".into())
        })?;
        let signal_epoch = self
            .run
            .integer("signal_epoch")?
            .checked_add(1)
            .ok_or_else(|| {
                WorkflowServiceError::ResourceExhausted("workflow signal epoch exhausted".into())
            })?;
        Ok(RestartPlan {
            context: self.context,
            current: self.current,
            generation,
            signal_epoch,
            steps: self.steps,
            from: self.from,
            deploy,
            previous: self.previous,
        })
    }
}

async fn exact_target(
    tx: &Transaction,
    app: &AppId,
    expected: &DeployRegistration,
) -> Result<(), WorkflowServiceError> {
    let record = deploys::read(tx, app, &expected.id)
        .await?
        .ok_or_else(unavailable)?;
    record.available()?;
    let registration = record.registration()?;
    if registration != *expected
        || record.hash != expected.hash
        || zeroship_core::typed_id::parse_with_prefix(&expected.id, "dep").is_err()
        || !zeroship_bundle::validate_hash_format(&record.hash)
        || record.availability_epoch < 0
        || registration
            .workflows
            .iter()
            .any(|name| crate::validation::workflow_name(name).is_err())
    {
        return Err(unavailable());
    }
    require_journal_hold(tx, app, &expected.id, &record.hash).await
}

/// The app lock protects this source through the new generation's commit.
/// Started restart needs its existing journal hold, without loading an artifact
/// or consulting the active deployment or a platform service.
async fn retained_source(
    tx: &Transaction,
    app: &AppId,
    deployment: &str,
    workflow: &str,
) -> Result<(), WorkflowServiceError> {
    let record = deploys::read(tx, app, deployment)
        .await?
        .ok_or_else(unavailable)?;
    record.available()?;
    let registration = record.registration()?;
    if registration.id != deployment
        || registration.hash != record.hash
        || !zeroship_bundle::validate_hash_format(&record.hash)
        || record.availability_epoch < 0
        || !registration.workflows.contains(workflow)
        || registration
            .workflows
            .iter()
            .any(|name| crate::validation::workflow_name(name).is_err())
    {
        return Err(unavailable());
    }
    require_journal_hold(tx, app, deployment, &record.hash).await
}

async fn require_journal_hold(
    tx: &Transaction,
    app: &AppId,
    deployment: &str,
    hash: &str,
) -> Result<(), WorkflowServiceError> {
    admission_generation(tx, app, deployment, hash, &HoldScope::for_app(app.clone()))
        .await
        .map_err(|error| match error {
            WorkflowServiceError::Conflict(_) => unavailable(),
            other => other,
        })?;
    Ok(())
}

impl RestartPlan<'_> {
    pub(in crate::service) async fn apply(self) -> Result<RestartedRun, WorkflowServiceError> {
        let Self {
            context,
            current,
            generation,
            signal_epoch,
            steps,
            from,
            deploy,
            previous,
        } = self;
        let tx = context.tx;
        let app = &context.app;
        let run_id = context.run_id.as_str();
        let now = context.now;
        let source = super::super::continuations::member(tx, app, run_id, current).await?;
        let prefix = from.unwrap_or(0);
        let restarted_from_ordinal = from
            .map(|ordinal| {
                u32::try_from(ordinal).map_err(|_| {
                    WorkflowServiceError::Internal("invalid workflow restart ordinal".into())
                })
            })
            .transpose()?;
        let tasks = tx
            .database()
            .collection(models::tasks::Entity::COLLECTION)?;
        tasks.execute(Operation::Update {
            filter:value!({"app_id":app.as_str(), "run_id":run_id, "generation":current, "state":"leased"}),
            patch:value!({"state":"expired", "finished_at":now}), many:true,
        }).await?;
        for table in [
            models::waits::Entity::COLLECTION,
            models::subscriptions::Entity::COLLECTION,
        ] {
            tx.database()
                .collection(table)?
                .execute(Operation::Purge {
                    filter: value!({"app_id":app.as_str(), "run_id":run_id, "generation":current}),
                    many: true,
                })
                .await?;
        }
        let signals = tx
            .database()
            .collection(models::signals::Entity::COLLECTION)?;
        for step in steps.iter().filter(|step| step.ordinal >= prefix) {
            if let Some(signal) = &step.consumed_signal_id {
                signals.execute(Operation::Purge {
                    filter:value!({"app_id":app.as_str(), "run_id":run_id, "id":signal.as_str(), "delivery":"topic"}), many:false,
                }).await?;
                signals.update(
                    value!({"app_id":app.as_str(), "run_id":run_id, "id":signal.as_str(), "delivery":"direct"}),
                    value!({"consumed_generation":null, "consumed_ordinal":null}),
                ).await?;
            }
        }
        signals
            .execute(Operation::Purge {
                filter: value!({"app_id":app.as_str(), "run_id":run_id, "delivery":"topic",
                "target_generation":current, "target_ordinal":{"$gte":i64::from(prefix)}}),
                many: true,
            })
            .await?;
        tx.database()
            .collection(models::generations::Entity::COLLECTION)?
            .insert(value!({
                "id":super::super::types::storage_id(), "app_id":app.as_str(), "run_id":run_id, "generation":generation,
                "deploy_id":deploy.clone(), "input":previous.input, "input_ref":previous.input_ref,
                "state":"queued", "started_at":now,
            }))
            .await?;
        super::super::continuations::restart(tx, app, &source, run_id, generation).await?;
        tx.database().collection(models::generations::Entity::COLLECTION)?.update(
            value!({"app_id":app.as_str(), "run_id":run_id, "generation":current, "terminal_at":null}),
            value!({"state":"restarted", "terminal_at":now}),
        ).await?;
        replay::copy_prefix(tx, app, run_id, current, generation, prefix).await?;
        tx.database().collection(models::runs::Entity::COLLECTION)?.update(
            value!({"app_id":app.as_str(), "id":run_id}),
            value!({"generation":generation, "deploy_id":deploy.clone(), "state":"queued", "control":"none",
                "due_at":now, "task_id":null, "terminal_at":null, "compensation_target":null, "signal_epoch":signal_epoch,
                "frontier_revision":1}),
        ).await?;
        super::super::continuations::member(tx, app, run_id, generation).await?;
        super::super::publication::record(tx, app, run_id, now).await?;
        let result = RestartedRun {
            run_id: run_id.into(),
            state: RunState::Queued,
            restarted_from_ordinal,
            pinned_to: deploy,
        };
        emit(
            tx,
            app,
            &format!("{run_id}:{generation}:restart"),
            "workflow.restart",
            json!({"runId":run_id,"generation":generation}),
            now,
        )
        .await?;

        Ok(result)
    }
}

/// Restart holds the app lock while inspecting descendants and changing the
/// generation. Exhaustion or corrupted ancestry cannot authorize a restart.
async fn has_active_descendants(
    tx: &Transaction,
    app: &AppId,
    root: &str,
) -> Result<bool, WorkflowServiceError> {
    let db = tx.database();
    let run = db.entity::<models::runs::Entity>()?.alias("r")?;
    let page_limit = RowLimit::default().get();
    let page_size = usize::try_from(page_limit).map_err(|_| {
        WorkflowServiceError::Internal("invalid workflow descendant page size".into())
    })?;
    let mut pending = vec![root.to_owned()];
    let mut inspected = BTreeSet::from([root.to_owned()]);
    while let Some(parent) = pending.pop() {
        let mut after: Option<String> = None;
        loop {
            let mut filter = run.column(models::runs::app_id).eq(app.as_str())?.and(
                run.column(models::runs::parent_id)
                    .eq(Some(parent.as_str()))?,
            );
            if let Some(after) = &after {
                filter = filter.and(run.column(models::runs::id).gt(after.as_str())?);
            }
            let page = db
                .from(&run)
                .filter(filter)
                .order_by(run.column(models::runs::id).asc())
                .select(run.row::<models::KeyedRun>())?
                .limit(page_limit)?
                .all()
                .await?;
            let count = page.len();
            for descendant in page {
                if !inspected.insert(descendant.id.clone()) {
                    return Err(WorkflowServiceError::Internal(
                        "workflow descendants contain a cycle".into(),
                    ));
                }
                if inspected.len() > MAX_DESCENDANT_INSPECTIONS {
                    return Err(WorkflowServiceError::ResourceExhausted(
                        "workflow descendant inspection limit reached".into(),
                    ));
                }
                if !parse_state(&descendant.state)?.is_terminal() {
                    return Ok(true);
                }
                after = Some(descendant.id.clone());
                pending.push(descendant.id);
            }
            if count < page_size {
                break;
            }
        }
    }
    Ok(false)
}

#[derive(FromRow)]
#[orm(entity = models::waits)]
struct ChildWait {
    id: String,
    ordinal: i64,
}

async fn has_active_children(
    tx: &Transaction,
    app: &AppId,
    run: &str,
    generation: i64,
) -> Result<bool, WorkflowServiceError> {
    let db = tx.database();
    let source = db.entity::<models::waits::Entity>()?.alias("w")?;
    let page_limit = RowLimit::default().get();
    let mut after: Option<String> = None;
    let mut inspected = 0;
    loop {
        let mut filter = source
            .column(models::waits::app_id)
            .eq(app.as_str())?
            .and(source.column(models::waits::run_id).eq(run)?)
            .and(source.column(models::waits::generation).eq(generation)?)
            .and(source.column(models::waits::kind).eq("child")?);
        if let Some(after) = &after {
            filter = filter.and(source.column(models::waits::id).gt(after.as_str())?);
        }
        let waits = db
            .from(&source)
            .filter(filter)
            .order_by(source.column(models::waits::id).asc())
            .select(source.row::<ChildWait>())?
            .limit(page_limit)?
            .all()
            .await?;
        let count = waits.len();
        for wait in waits {
            inspected += 1;
            if inspected > MAX_DESCENDANT_INSPECTIONS {
                return Err(WorkflowServiceError::ResourceExhausted(
                    "workflow child inspection limit reached".into(),
                ));
            }
            let child =
                super::super::continuations::resolve(tx, app, run, generation, wait.ordinal)
                    .await?;
            if !child.state.is_terminal() {
                return Ok(true);
            }
            after = Some(wait.id);
        }
        if count < page_limit as usize {
            return Ok(false);
        }
    }
}
