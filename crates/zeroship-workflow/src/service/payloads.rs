use super::{
    app::{deadline, emit, lock_app, lock_run, not_found, validate_run},
    models,
    policy::PolicyAuthority,
    store::{Row, Transaction},
    tasks::authorized_task,
    AppWorkflows, RequestId, TaskToken, WorkerIdentity, WorkflowService,
};
use crate::service::policy::admit;
use crate::{engine::WorkflowOutputRef, validation, WorkflowServiceError};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_data_orm::{
    orm::{Entity, FindOptions, FromRow, Operation, Output},
    value, Value,
};

mod collection;

pub(crate) const MAX_COLLECTION_BATCH: usize = 1024;

#[derive(FromRow)]
#[orm(entity = models::payloads)]
struct ExpiredPayload {
    app_id: String,
    id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum PayloadSlot {
    Input,
    Output,
    Step { ordinal: i32 },
}
impl PayloadSlot {
    fn coordinates(self) -> Result<(&'static str, i64), WorkflowServiceError> {
        match self {
            Self::Input => Ok(("input", 0)),
            Self::Output => Ok(("output", 0)),
            Self::Step { ordinal } if ordinal >= 0 => Ok(("step", i64::from(ordinal))),
            Self::Step { .. } => Err(WorkflowServiceError::InvalidRequest(
                "invalid workflow payload operation".into(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StagedPayload {
    pub id: String,
    pub reference: WorkflowOutputRef,
}

/// One object an execution-side effect acts on, named by the record admission
/// authorized. The descriptor is the contract the bytes must satisfy.
#[derive(Debug, Clone, Copy)]
pub struct PayloadTarget<'a> {
    pub app: &'a AppId,
    pub id: &'a str,
    pub reference: &'a WorkflowOutputRef,
    /// The policy generation this operation is bound to. A body handed back to
    /// the caller outlives the transaction, so the opener guards it with this.
    pub authority: Option<&'a Arc<PolicyAuthority>>,
}

/// Stores one staged object. Admission holds the upload claim and the payload
/// record across this call and admits nothing the writer did not confirm.
#[async_trait::async_trait(?Send)]
pub trait PayloadWriter {
    /// Write the target and verify it against its descriptor. `budget` is what
    /// remains of the shorter of the task lease and the staging window.
    ///
    /// # Errors
    /// Reports a refused, interrupted, oversized or unverifiable write.
    async fn write(
        self,
        target: PayloadTarget<'_>,
        budget: Duration,
    ) -> Result<(), WorkflowServiceError>;
}

/// Opens one object whose ownership admission has just proven.
#[async_trait::async_trait(?Send)]
pub trait PayloadOpener {
    /// The execution-side read handle this opener produces.
    type Read;

    /// # Errors
    /// Reports a missing, unavailable or changed object.
    async fn open(self, target: PayloadTarget<'_>) -> Result<Self::Read, WorkflowServiceError>;
}

/// Deletes objects during collection, once per payload admission fenced.
#[async_trait::async_trait(?Send)]
pub trait PayloadDeleter {
    /// # Errors
    /// Reports a refused or unavailable deletion; collection stays retryable.
    async fn delete(&self, app: &AppId, id: &str) -> Result<(), WorkflowServiceError>;
}

/// A completed step's recorded output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutput<R> {
    /// The journal holds the value itself; no object was ever written.
    Inline(serde_json::Value),
    /// The journal holds a descriptor, and `R` is what the opener made of it.
    Object(R),
}

impl WorkflowService {
    /// Upload against current task ownership. The upload identity is durable
    /// before object I/O, so an interrupted writer leaves a collectible record.
    ///
    /// # Errors
    /// Rejects stale task or policy authority, invalid content, and a write the
    /// caller could not complete.
    pub async fn stage_payload<W: PayloadWriter>(
        &self,
        worker: &WorkerIdentity,
        task_id: &str,
        token: &TaskToken,
        request: &RequestId,
        reference: WorkflowOutputRef,
        writer: W,
    ) -> Result<StagedPayload, WorkflowServiceError> {
        let authority = payload_authority(self)?;
        let service = match authority.as_deref() {
            Some(authority) => self.with_authority(authority.clone())?,
            None => self.clone(),
        };
        guarded_payload(
            authority.as_deref(),
            Box::pin(service.stage_payload_inner(
                worker, task_id, token, request, reference, writer,
            )),
        )
        .await
    }

    async fn stage_payload_inner<W: PayloadWriter>(
        &self,
        worker: &WorkerIdentity,
        task_id: &str,
        token: &TaskToken,
        request: &RequestId,
        reference: WorkflowOutputRef,
        writer: W,
    ) -> Result<StagedPayload, WorkflowServiceError> {
        validate_reference(&reference)?;
        let mut tx = self.begin().await?;
        let claim = authorized_task(&mut tx, worker, task_id, token).await?;
        let existing = tx
            .database()
            .entity::<models::payloads::Entity>()?
            .find::<models::PayloadRecord>(
                models::payloads::app_id
                    .eq(claim.app.as_str())?
                    .and(models::payloads::task_id.eq(task_id)?)
                    .and(models::payloads::request_id.eq(request.as_str())?),
                FindOptions {
                    limit: Some(1),
                    ..Default::default()
                },
            )
            .await?;
        let id = if let Some(row) = existing.first() {
            if reference_from(row) != reference {
                return Err(WorkflowServiceError::Conflict(
                    "payload request was used for another object".into(),
                ));
            }
            let id = row.id.clone();
            if matches!(row.state.as_str(), "staged" | "referenced") {
                tx.commit().await?;
                return Ok(StagedPayload { id, reference });
            }
            claim.validate_live()?;
            if row.state != "uploading" || row.expires_at <= claim.now {
                return Err(WorkflowServiceError::Conflict(
                    "payload upload has expired".into(),
                ));
            }
            id
        } else {
            claim.validate_live()?;
            admit(&claim.policy)?;
            if reference.size > claim.policy.max_payload_bytes {
                return Err(WorkflowServiceError::PayloadTooLarge);
            }
            let (total, objects) = payload_usage(&tx, &claim.app).await?;
            if objects >= claim.policy.max_payload_objects
                || total
                    .checked_add(reference.size)
                    .is_none_or(|total| total > claim.policy.max_payload_storage_bytes)
            {
                return Err(WorkflowServiceError::ResourceExhausted(
                    "workflow payload storage limit reached".into(),
                ));
            }
            let id = typed_id::generate(typed_id::WORKFLOW_PAYLOAD_PREFIX);
            tx.database().collection(models::payloads::Entity::COLLECTION)?.insert(value!({
                "app_id":claim.app.as_str(), "run_id":claim.run.text("id")?, "generation":claim.run.integer("generation")?,
                "id":id.clone(), "task_id":task_id, "request_id":request.as_str(), "hash":reference.hash.clone(),
                "size":reference.size, "content_type":reference.content_type.clone(), "state":"uploading",
                "created_at":claim.now, "expires_at":deadline(claim.now,claim.policy.payload_staging_retention_ms)?,
            })).await?;
            id
        };
        claim.validate_at(tx.now().await?)?;
        tx.commit().await?;

        // Lock in the same order as completion and GC. A bounded upload holds
        // this lock until the store finishes, so GC cannot race a live writer.
        let mut tx = self.begin().await?;
        let claim = authorized_task(&mut tx, worker, task_id, token).await?;
        claim.validate_live()?;
        admit(&claim.policy)?;
        let row = payload(&tx, &claim.app, &id).await?;
        match row.state.as_str() {
            "staged" | "referenced" => {
                tx.commit().await?;
                return Ok(StagedPayload { id, reference });
            }
            "uploading" if row.expires_at > claim.now => {}
            _ => {
                return Err(WorkflowServiceError::Conflict(
                    "payload upload has expired".into(),
                ));
            }
        }
        let remaining = claim.task.deadline.min(row.expires_at) - claim.now;
        writer
            .write(
                PayloadTarget {
                    app: &claim.app,
                    id: &id,
                    reference: &reference,
                    authority: None,
                },
                Duration::from_millis(remaining as u64),
            )
            .await?;
        claim.validate_at(tx.now().await?)?;
        tx.database()
            .collection(models::payloads::Entity::COLLECTION)?
            .update(
                value!({"app_id":claim.app.as_str(), "id":id.clone()}),
                value!({"state":"staged"}),
            )
            .await?;
        claim.validate_at(tx.now().await?)?;
        tx.commit().await?;
        Ok(StagedPayload { id, reference })
    }

    /// Task reads follow committed replay edges or that task's own staged objects.
    ///
    /// # Errors
    /// Rejects stale authority, unrelated payloads, and an object the caller
    /// could not open.
    pub async fn read_task_payload<O: PayloadOpener>(
        &self,
        worker: &WorkerIdentity,
        task_id: &str,
        token: &TaskToken,
        reference: &WorkflowOutputRef,
        opener: O,
    ) -> Result<O::Read, WorkflowServiceError> {
        let authority = payload_authority(self)?;
        let service = match authority.as_deref() {
            Some(authority) => self.with_authority(authority.clone())?,
            None => self.clone(),
        };
        guarded_payload(
            authority.as_deref(),
            Box::pin(service.read_task_payload_inner(
                worker,
                task_id,
                token,
                reference,
                opener,
                authority.as_ref(),
            )),
        )
        .await
    }

    async fn read_task_payload_inner<O: PayloadOpener>(
        &self,
        worker: &WorkerIdentity,
        task_id: &str,
        token: &TaskToken,
        reference: &WorkflowOutputRef,
        opener: O,
        authority: Option<&Arc<PolicyAuthority>>,
    ) -> Result<O::Read, WorkflowServiceError> {
        validate_reference(reference)?;
        let mut tx = self.begin().await?;
        let claim = authorized_task(&mut tx, worker, task_id, token).await?;
        claim.validate_live()?;
        let row = owned_reference(&mut tx, &claim.app, &claim.run, reference, claim.now).await?;
        let read = open_payload(opener, &claim.app, &row, authority).await?;
        claim.validate_at(tx.now().await?)?;
        tx.commit().await?;
        Ok(read)
    }

    /// Collect only unreferenced expired uploads. Failed deletion remains
    /// retryable; a transaction failure never authorizes a reference promotion.
    ///
    /// # Errors
    /// Rejects an invalid batch size and reports journal failures.
    pub async fn collect_payloads<D: PayloadDeleter>(
        &self,
        limit: usize,
        deleter: &D,
    ) -> Result<usize, WorkflowServiceError> {
        if limit == 0 || limit > MAX_COLLECTION_BATCH {
            return Err(WorkflowServiceError::InvalidRequest(
                "invalid payload collection batch".into(),
            ));
        }
        let mut tx = self.begin().await?;
        let now = tx.now().await?;
        let db = tx.database();
        let object = db.entity::<models::payloads::Entity>()?.alias("p")?;
        let candidates = db
            .from(&object)
            .filter(
                object
                    .column(models::payloads::app_id)
                    .in_values(
                        tx.host_app_ids()?
                            .into_iter()
                            .map(|app| app.as_str().to_owned()),
                    )?
                    .and(object.column(models::payloads::state).in_values([
                        "uploading",
                        "staged",
                        "deleting",
                        "deleted",
                    ])?)
                    .and(object.column(models::payloads::expires_at).lte(now)?),
            )
            .order_by(object.column(models::payloads::expires_at).asc())
            .order_by(object.column(models::payloads::app_id).asc())
            .order_by(object.column(models::payloads::id).asc())
            .select(object.row::<ExpiredPayload>())?
            .limit(limit as i64)?
            .all()
            .await?;
        tx.commit().await?;
        let mut collected = 0;
        for candidate in candidates {
            let app = AppId::parse(&candidate.app_id).map_err(|_| {
                WorkflowServiceError::Internal("invalid payload app identity".into())
            })?;
            if Box::pin(self.collect_payload_checked(
                &app,
                &candidate.id,
                now,
                &|| Ok(()),
                deleter,
            ))
            .await?
            {
                collected += 1;
            }
        }
        Ok(collected)
    }
}

impl AppWorkflows {
    /// Read a completed step from the run's current generation. Generation
    /// selection and reference resolution share the run lock with restart.
    ///
    /// # Errors
    /// Rejects invalid names, unavailable steps and object failures.
    pub async fn read_step_output<O: PayloadOpener>(
        &self,
        run_id: &str,
        name: &str,
        occurrence: u32,
        opener: O,
    ) -> Result<StepOutput<O::Read>, WorkflowServiceError> {
        let authority = Arc::new(self.capture_policy().authority()?.clone());
        let scope = self.clone().with_authority(authority.as_ref().clone())?;
        authority
            .run(Box::pin(scope.read_step_output_inner(
                run_id,
                name,
                occurrence,
                opener,
                &authority,
            )))
            .await
    }

    async fn read_step_output_inner<O: PayloadOpener>(
        &self,
        run_id: &str,
        name: &str,
        occurrence: u32,
        opener: O,
        authority: &Arc<PolicyAuthority>,
    ) -> Result<StepOutput<O::Read>, WorkflowServiceError> {
        validate_run(run_id)?;
        validation::step_name(name)?;
        let occurrence = i32::try_from(occurrence).map_err(|_| {
            WorkflowServiceError::InvalidRequest("invalid step name occurrence".into())
        })?;
        let mut tx = self.service.begin().await?;
        lock_app(&mut tx, &self.app).await?;
        let run = lock_run(&mut tx, &self.app, run_id).await?;
        let generation = run.integer("generation")?;
        let rows = tx
            .database()
            .entity::<models::steps::Entity>()?
            .find::<models::StoredStep>(
                models::steps::app_id
                    .eq(self.app.as_str())?
                    .and(models::steps::run_id.eq(run_id)?)
                    .and(models::steps::generation.eq(generation)?)
                    .and(models::steps::name.eq(name)?)
                    .and(models::steps::occurrence.eq(i64::from(occurrence))?),
                FindOptions {
                    limit: Some(1),
                    ..Default::default()
                },
            )
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| not_found("workflow step output"))?;
        let step = super::journal::read_checkpoint(&tx, &self.app, row).await?;
        if step.state != "completed" {
            return Err(not_found("workflow step output"));
        }
        let read = if let Some(reference) = step.output_ref {
            let row = reference_at(
                &mut tx,
                &self.app,
                run_id,
                generation,
                PayloadSlot::Step {
                    ordinal: step.ordinal,
                },
            )
            .await?;
            if reference_from(&row) != reference {
                return Err(WorkflowServiceError::Unavailable(
                    "workflow step payload reference changed".into(),
                ));
            }
            StepOutput::Object(open_payload(opener, &self.app, &row, Some(authority)).await?)
        } else {
            StepOutput::Inline(step.output.unwrap_or(serde_json::Value::Null))
        };
        tx.commit().await?;
        Ok(read)
    }

    /// Read a retained payload through this app's original policy authority.
    ///
    /// # Errors
    /// Rejects missing history, unavailable policy and failed object reads.
    pub async fn read_payload<O: PayloadOpener>(
        &self,
        run_id: &str,
        generation: i64,
        slot: PayloadSlot,
        opener: O,
    ) -> Result<O::Read, WorkflowServiceError> {
        let authority = Arc::new(self.capture_policy().authority()?.clone());
        let scope = self.clone().with_authority(authority.as_ref().clone())?;
        authority
            .run(Box::pin(scope.read_payload_inner(
                run_id,
                generation,
                slot,
                opener,
                &authority,
            )))
            .await
    }

    async fn read_payload_inner<O: PayloadOpener>(
        &self,
        run_id: &str,
        generation: i64,
        slot: PayloadSlot,
        opener: O,
        authority: &Arc<PolicyAuthority>,
    ) -> Result<O::Read, WorkflowServiceError> {
        validate_run(run_id)?;
        let mut tx = self.service.begin().await?;
        lock_app(&mut tx, &self.app).await?;
        lock_run(&mut tx, &self.app, run_id).await?;
        let row = reference_at(&mut tx, &self.app, run_id, generation, slot).await?;
        let read = open_payload(opener, &self.app, &row, Some(authority)).await?;
        tx.commit().await?;
        Ok(read)
    }
}

fn payload_authority(
    service: &WorkflowService,
) -> Result<Option<Arc<PolicyAuthority>>, WorkflowServiceError> {
    service
        .capture_policy()
        .map(|captured| captured.authority().cloned().map(Arc::new))
        .transpose()
}

fn guarded_payload<'a, T: 'a>(
    authority: Option<&'a PolicyAuthority>,
    operation: futures::future::LocalBoxFuture<'a, Result<T, WorkflowServiceError>>,
) -> futures::future::LocalBoxFuture<'a, Result<T, WorkflowServiceError>> {
    match authority {
        Some(authority) => authority.run(operation),
        None => operation,
    }
}

pub(crate) struct RunGeneration<'a> {
    pub id: &'a str,
    pub generation: i64,
}

pub(crate) async fn promote(
    tx: &mut Transaction,
    app: &AppId,
    source: &Row,
    target: RunGeneration<'_>,
    slot: PayloadSlot,
    reference: &WorkflowOutputRef,
    now: i64,
) -> Result<(), WorkflowServiceError> {
    validate_reference(reference)?;
    let row = owned_reference(tx, app, source, reference, now).await?;
    attach(tx, app, target.id, target.generation, slot, &row, now).await
}

async fn attach(
    tx: &mut Transaction,
    app: &AppId,
    run_id: &str,
    generation: i64,
    slot: PayloadSlot,
    row: &models::PayloadRecord,
    now: i64,
) -> Result<(), WorkflowServiceError> {
    let (kind, ordinal) = slot.coordinates()?;
    let id = &row.id;
    tx.database()
        .collection(models::payload_refs::Entity::COLLECTION)?
        .insert(value!({
            "id":super::types::storage_id(), "app_id":app.as_str(), "run_id":run_id, "generation":generation, "slot":kind,
            "ordinal":ordinal, "payload_id":id.as_str(),
        }))
        .await?;
    if row.state == "staged" {
        tx.database()
            .collection(models::payloads::Entity::COLLECTION)?
            .update(
                value!({"app_id":app.as_str(), "id":id.as_str()}),
                value!({"state":"referenced"}),
            )
            .await?;
        emit(
            tx,
            app,
            &format!("{id}:retained"),
            "workflow.payload.retained",
            serde_json::json!({"payloadId":id,"bytes":row.size}),
            now,
        )
        .await?;
    }
    Ok(())
}

pub(crate) async fn inherit_child_output(
    tx: &mut Transaction,
    app: &AppId,
    child: RunGeneration<'_>,
    parent: RunGeneration<'_>,
    ordinal: i32,
    now: i64,
) -> Result<WorkflowOutputRef, WorkflowServiceError> {
    let row = reference_at(tx, app, child.id, child.generation, PayloadSlot::Output).await?;
    attach(
        tx,
        app,
        parent.id,
        parent.generation,
        PayloadSlot::Step { ordinal },
        &row,
        now,
    )
    .await?;
    Ok(reference_from(&row))
}

async fn owned_reference(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
    reference: &WorkflowOutputRef,
    now: i64,
) -> Result<models::PayloadRecord, WorkflowServiceError> {
    let db = tx.database();
    let object = db.entity::<models::payloads::Entity>()?.alias("p")?;
    let edge = db.entity::<models::payload_refs::Entity>()?.alias("r")?;
    let id = run.text("id")?;
    let generation = run.integer("generation")?;
    let mut ownership = object
        .column(models::payloads::state)
        .eq("referenced")?
        .and(edge.column(models::payload_refs::run_id).eq(id.as_str())?);
    if let Some(task) = run.optional_text("task_id")? {
        ownership = ownership.or(object
            .column(models::payloads::state)
            .eq("staged")?
            .and(object.column(models::payloads::run_id).eq(id.as_str())?)
            .and(object.column(models::payloads::generation).eq(generation)?)
            .and(object.column(models::payloads::task_id).eq(task.as_str())?)
            .and(object.column(models::payloads::expires_at).gt(now)?));
    }
    let rows = db
        .from(&object)
        .left_join(
            &edge,
            object
                .column(models::payloads::app_id)
                .eq(edge.column(models::payload_refs::app_id))?
                .and(
                    object
                        .column(models::payloads::id)
                        .eq(edge.column(models::payload_refs::payload_id))?,
                )
                .and(edge.column(models::payload_refs::run_id).eq(id.as_str())?)
                .and(
                    edge.column(models::payload_refs::generation)
                        .eq(generation)?,
                ),
        )?
        .filter(
            object
                .column(models::payloads::app_id)
                .eq(app.as_str())?
                .and(
                    object
                        .column(models::payloads::hash)
                        .eq(reference.hash.as_str())?,
                )
                .and(object.column(models::payloads::size).eq(reference.size)?)
                .and(
                    object
                        .column(models::payloads::content_type)
                        .eq(reference.content_type.as_deref())?,
                )
                .and(ownership),
        )
        .order_by(object.column(models::payloads::id).asc())
        .select(object.row::<models::PayloadRecord>())?
        .limit(1)?
        .all()
        .await?;
    rows.into_iter()
        .next()
        .ok_or_else(|| not_found("workflow payload"))
}

async fn reference_at(
    tx: &mut Transaction,
    app: &AppId,
    run_id: &str,
    generation: i64,
    slot: PayloadSlot,
) -> Result<models::PayloadRecord, WorkflowServiceError> {
    let (kind, ordinal) = slot.coordinates()?;
    let db = tx.database();
    let object = db.entity::<models::payloads::Entity>()?.alias("p")?;
    let edge = db.entity::<models::payload_refs::Entity>()?.alias("r")?;
    db.from(&edge)
        .inner_join(
            &object,
            edge.column(models::payload_refs::app_id)
                .eq(object.column(models::payloads::app_id))?
                .and(
                    edge.column(models::payload_refs::payload_id)
                        .eq(object.column(models::payloads::id))?,
                ),
        )?
        .filter(
            edge.column(models::payload_refs::app_id)
                .eq(app.as_str())?
                .and(edge.column(models::payload_refs::run_id).eq(run_id)?)
                .and(
                    edge.column(models::payload_refs::generation)
                        .eq(generation)?,
                )
                .and(edge.column(models::payload_refs::slot).eq(kind)?)
                .and(edge.column(models::payload_refs::ordinal).eq(ordinal)?)
                .and(object.column(models::payloads::state).eq("referenced")?),
        )
        .select(object.row::<models::PayloadRecord>())?
        .limit(1)?
        .all()
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| not_found("workflow payload"))
}

async fn payload(
    tx: &Transaction,
    app: &AppId,
    id: &str,
) -> Result<models::PayloadRecord, WorkflowServiceError> {
    tx.database()
        .entity::<models::payloads::Entity>()?
        .find::<models::PayloadRecord>(
            models::payloads::app_id
                .eq(app.as_str())?
                .and(models::payloads::id.eq(id)?),
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| not_found("workflow payload"))
}
fn reference_from(row: &models::PayloadRecord) -> WorkflowOutputRef {
    WorkflowOutputRef {
        hash: row.hash.clone(),
        size: row.size,
        content_type: row.content_type.clone(),
    }
}

async fn payload_usage(tx: &Transaction, app: &AppId) -> Result<(i64, i64), WorkflowServiceError> {
    let Output::Rows { rows, .. } = tx
        .database()
        .collection(models::payloads::Entity::COLLECTION)?
        .execute(Operation::Aggregate {
            pipeline: value!([
                {"$match":{"app_id":app.as_str(), "state":{"$nin":["deleted","purged"]}}},
                {"$group":{"total":{"$sum":"size"}, "objects":{"$count":true}}},
            ]),
            options: value!({}),
        })
        .await?
    else {
        return Err(WorkflowServiceError::Internal(
            "workflow payload aggregate returned a count".into(),
        ));
    };
    let [row] = rows.as_slice() else {
        return Err(WorkflowServiceError::Internal(
            "workflow payload aggregate returned invalid rows".into(),
        ));
    };
    let integer = |name: &str| {
        // Aggregates may widen an integer input to the ORM's exact decimal value.
        match &row[name] {
            Value::Decimal(value) => value.parse::<i64>().ok(),
            value => value.as_i64(),
        }
        .filter(|value| *value >= 0)
        .ok_or_else(|| WorkflowServiceError::Internal("invalid workflow payload aggregate".into()))
    };
    let objects = integer("objects")?;
    let total = if objects == 0 && row["total"].is_null() {
        0
    } else {
        integer("total")?
    };
    Ok((total, objects))
}
/// Check a payload descriptor before it reaches the journal or an object store.
///
/// # Errors
/// Rejects a malformed hash, a negative size and an invalid content type.
pub fn validate_reference(
    reference: &WorkflowOutputRef,
) -> Result<(), WorkflowServiceError> {
    if reference.hash.len() != 64
        || !reference
            .hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || reference.size < 0
        || reference.content_type.as_ref().is_some_and(|value| {
            value.is_empty()
                || value.len() > 256
                || !value.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
        })
    {
        return Err(WorkflowServiceError::InvalidRequest(
            "invalid workflow payload descriptor".into(),
        ));
    }
    Ok(())
}
async fn open_payload<O: PayloadOpener>(
    opener: O,
    app: &AppId,
    row: &models::PayloadRecord,
    authority: Option<&Arc<PolicyAuthority>>,
) -> Result<O::Read, WorkflowServiceError> {
    let reference = reference_from(row);
    opener
        .open(PayloadTarget {
            app,
            id: &row.id,
            reference: &reference,
            authority,
        })
        .await
}
