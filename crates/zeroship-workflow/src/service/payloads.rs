use super::{
    app::{deadline, emit, lock_app, lock_run, not_found, validate_run},
    models,
    policy::PolicyAuthority,
    store::{Row, Transaction},
    tasks::authorized_task,
    AppWorkflows, RequestId, TaskToken, WorkerIdentity, WorkflowService,
};
use crate::service::policy::admit;
use crate::{
    engine::{StepCheckpoint, WorkflowOutputRef},
    validation, WorkflowServiceError,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{cell::Cell, rc::Rc, sync::Arc, time::Duration};
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_data_orm::{
    orm::{Entity, FindOptions, FromRow, Operation, Output},
    sql::{CompareOp, Literal, Operand, Predicate},
    value, Value,
};
use zeroship_storage::{
    backend::{BoxByteStream, BoxChunkSource, ChunkResult, ChunkSource, OnceChunk},
    Namespace, Storage, StorageError, StorageStore,
};

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

pub struct PayloadRead {
    pub reference: WorkflowOutputRef,
    pub body: BoxByteStream,
}
impl PayloadRead {
    fn guarded(mut self, authority: Option<Arc<PolicyAuthority>>) -> Self {
        if let Some(authority) = authority {
            self.body = Box::new(AuthorizedSource {
                inner: Some(self.body),
                authority,
            });
        }
        self
    }

    /// Collect a verified payload within the host's memory budget.
    ///
    /// # Errors
    /// Rejects oversized descriptors, interrupted bodies and corrupt content.
    pub async fn into_bytes(mut self, limit: usize) -> Result<Vec<u8>, WorkflowServiceError> {
        let size = usize::try_from(self.reference.size)
            .ok()
            .filter(|size| *size <= limit)
            .ok_or(WorkflowServiceError::PayloadTooLarge)?;
        let mut bytes = Vec::new();
        while let Some(chunk) = self.body.next_chunk().await {
            let chunk = chunk.map_err(|_| {
                WorkflowServiceError::Unavailable(
                    "workflow payload read failed integrity verification".into(),
                )
            })?;
            if bytes
                .len()
                .checked_add(chunk.len())
                .is_none_or(|size| size > limit)
            {
                return Err(WorkflowServiceError::PayloadTooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.len() != size {
            return Err(WorkflowServiceError::Unavailable(
                "workflow payload size changed".into(),
            ));
        }
        Ok(bytes)
    }

    pub(crate) fn checked(
        reference: WorkflowOutputRef,
        body: BoxByteStream,
    ) -> Result<Self, WorkflowServiceError> {
        validate_reference(&reference)?;
        Ok(Self {
            reference: reference.clone(),
            body: Box::new(VerifiedSource {
                inner: body,
                expected: reference,
                bytes: 0,
                hash: Sha256::new(),
                verified: Rc::new(Cell::new(false)),
                finished: false,
            }),
        })
    }
}
impl std::fmt::Debug for PayloadRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PayloadRead")
            .field("reference", &self.reference)
            .finish_non_exhaustive()
    }
}

impl WorkflowService {
    /// Use a store whose credentials are private to the workflow host.
    pub fn with_payload_storage(
        mut self,
        store: StorageStore,
    ) -> Result<Self, WorkflowServiceError> {
        self.payload_storage =
            Some(store.namespace(Namespace::platform("workflow").map_err(storage_error)?));
        Ok(self)
    }

    /// Upload against current task ownership. The upload identity is durable
    /// before object I/O, so an interrupted writer leaves a collectible record.
    ///
    /// # Errors
    /// Rejects stale task or policy authority, invalid content and storage failures.
    pub async fn stage_payload(
        &self,
        worker: &WorkerIdentity,
        task_id: &str,
        token: &TaskToken,
        request: &RequestId,
        reference: WorkflowOutputRef,
        body: BoxChunkSource,
    ) -> Result<StagedPayload, WorkflowServiceError> {
        let authority = payload_authority(self)?;
        let service = match authority.as_deref() {
            Some(authority) => self.with_authority(authority.clone())?,
            None => self.clone(),
        };
        guarded_payload(
            authority.as_deref(),
            Box::pin(service.stage_payload_inner(worker, task_id, token, request, reference, body)),
        )
        .await
    }

    async fn stage_payload_inner(
        &self,
        worker: &WorkerIdentity,
        task_id: &str,
        token: &TaskToken,
        request: &RequestId,
        reference: WorkflowOutputRef,
        body: BoxChunkSource,
    ) -> Result<StagedPayload, WorkflowServiceError> {
        validate_reference(&reference)?;
        let storage = storage(self)?;
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
                ))
            }
        }
        let verified = Rc::new(Cell::new(false));
        let source = VerifiedSource {
            inner: body,
            expected: reference.clone(),
            bytes: 0,
            hash: Sha256::new(),
            verified: verified.clone(),
            finished: false,
        };
        let remaining = claim.task.deadline.min(row.expires_at) - claim.now;
        let written = compio::time::timeout(
            Duration::from_millis(remaining as u64),
            storage.put_stream(
                claim.app.as_str(),
                &id,
                Box::new(source),
                reference.content_type.as_deref(),
            ),
        )
        .await
        .map_err(|_| WorkflowServiceError::Timeout)?
        .map_err(storage_error)?;
        if !verified.get() || written != reference.size as u64 {
            return Err(WorkflowServiceError::Unavailable(
                "workflow payload store did not verify the upload".into(),
            ));
        }
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
    /// Rejects stale authority, unrelated payloads and unavailable or corrupt storage.
    pub async fn read_task_payload(
        &self,
        worker: &WorkerIdentity,
        task_id: &str,
        token: &TaskToken,
        reference: &WorkflowOutputRef,
    ) -> Result<PayloadRead, WorkflowServiceError> {
        let authority = payload_authority(self)?;
        let service = match authority.as_deref() {
            Some(authority) => self.with_authority(authority.clone())?,
            None => self.clone(),
        };
        let read = guarded_payload(
            authority.as_deref(),
            Box::pin(service.read_task_payload_inner(worker, task_id, token, reference)),
        )
        .await?;
        Ok(read.guarded(authority))
    }

    async fn read_task_payload_inner(
        &self,
        worker: &WorkerIdentity,
        task_id: &str,
        token: &TaskToken,
        reference: &WorkflowOutputRef,
    ) -> Result<PayloadRead, WorkflowServiceError> {
        validate_reference(reference)?;
        let storage = storage(self)?;
        let mut tx = self.begin().await?;
        let claim = authorized_task(&mut tx, worker, task_id, token).await?;
        claim.validate_live()?;
        let row = owned_reference(&mut tx, &claim.app, &claim.run, reference, claim.now).await?;
        let read = open_payload(&storage, &claim.app, &row).await?;
        claim.validate_at(tx.now().await?)?;
        tx.commit().await?;
        Ok(read)
    }

    /// Collect only unreferenced expired uploads. Failed deletion remains
    /// retryable; a transaction failure never authorizes a reference promotion.
    pub async fn collect_payloads(&self, limit: usize) -> Result<usize, WorkflowServiceError> {
        if limit == 0 || limit > MAX_COLLECTION_BATCH {
            return Err(WorkflowServiceError::InvalidRequest(
                "invalid payload collection batch".into(),
            ));
        }
        let storage = storage(self)?;
        let mut tx = self.begin().await?;
        let now = tx.now().await?;
        let db = tx.database();
        let object = db.entity::<models::payloads::Entity>()?.alias("p")?;
        let candidates = db
            .from(&object)
            .filter(Predicate::And(vec![
                Predicate::Or(
                    tx.host_app_ids()?
                        .into_iter()
                        .map(|app| object.column(models::payloads::app_id).eq(app.as_str()))
                        .collect::<Result<Vec<_>, _>>()?,
                ),
                Predicate::Or(
                    ["uploading", "staged", "deleting", "deleted"]
                        .into_iter()
                        .map(|state| object.column(models::payloads::state).eq(state))
                        .collect::<Result<Vec<_>, _>>()?,
                ),
                Predicate::compare(
                    Operand::Path(object.column(models::payloads::expires_at).asc().path),
                    CompareOp::Lte,
                    Operand::Lit(Literal::Int(now)),
                ),
            ]))
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
            let id = candidate.id;
            let mut tx = self.begin().await?;
            lock_app(&mut tx, &app).await?;
            let now = tx.now().await?;
            let row = payload(&tx, &app, &id).await?;
            if !matches!(
                row.state.as_str(),
                "uploading" | "staged" | "deleting" | "deleted"
            ) || row.expires_at > now
            {
                continue;
            }
            let Output::Rows {
                rows: references, ..
            } = tx
                .database()
                .collection(models::payload_refs::Entity::COLLECTION)?
                .find(
                    value!({"app_id":app.as_str(), "payload_id":id.clone()}),
                    value!({"select":["payload_id"], "limit":1}),
                )
                .await?
            else {
                return Err(WorkflowServiceError::Internal(
                    "workflow payload reference lookup returned a count".into(),
                ));
            };
            if !references.is_empty() {
                return Err(WorkflowServiceError::Internal(
                    "referenced payload was scheduled for collection".into(),
                ));
            }
            tx.database()
                .collection(models::payloads::Entity::COLLECTION)?
                .update(
                    value!({"app_id":app.as_str(), "id":id.clone()}),
                    value!({"state":"deleting"}),
                )
                .await?;
            tx.commit().await?;
            // The committed deleting state fences all upload and promotion paths.
            storage
                .delete(app.as_str(), &id)
                .await
                .map_err(storage_error)?;
            let mut tx = self.begin().await?;
            let policy = lock_app(&mut tx, &app).await?;
            let now = tx.now().await?;
            // Keep a tombstone and sweep it again. A remote store may finish an
            // already-sent upload after the writer process dies. That object
            // must remain inadmissible and must be collected on a later sweep.
            tx.database().collection(models::payloads::Entity::COLLECTION)?.execute(Operation::Update {
                filter:value!({"app_id":app.as_str(), "id":id, "state":"deleting"}),
                patch:value!({"state":"deleted", "expires_at":deadline(now,policy.payload_staging_retention_ms)?}),
                many:true,
            }).await?;
            tx.commit().await?;
            collected += 1;
        }
        Ok(collected)
    }
}

impl AppWorkflows {
    /// Read a completed step from the run's current generation. Generation
    /// selection and reference resolution share the run lock with restart.
    ///
    /// # Errors
    /// Rejects invalid names, unavailable steps and storage failures.
    pub async fn read_step_output(
        &self,
        run_id: &str,
        name: &str,
        occurrence: u32,
    ) -> Result<PayloadRead, WorkflowServiceError> {
        let authority = Arc::new(self.capture_policy().authority()?.clone());
        let scope = self.clone().with_authority(authority.as_ref().clone())?;
        let read = authority
            .run(Box::pin(
                scope.read_step_output_inner(run_id, name, occurrence),
            ))
            .await?;
        Ok(read.guarded(Some(authority)))
    }

    async fn read_step_output_inner(
        &self,
        run_id: &str,
        name: &str,
        occurrence: u32,
    ) -> Result<PayloadRead, WorkflowServiceError> {
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
        let step: StepCheckpoint = super::app::decode(&row.record)?;
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
            open_payload(&storage(&self.service)?, &self.app, &row).await?
        } else {
            let bytes = serde_json::to_vec(&step.output)
                .map_err(|_| WorkflowServiceError::Internal("invalid step output".into()))?;
            let reference = WorkflowOutputRef {
                hash: super::types::hash(&bytes),
                size: i64::try_from(bytes.len())
                    .map_err(|_| WorkflowServiceError::PayloadTooLarge)?,
                content_type: Some("application/json".into()),
            };
            PayloadRead::checked(reference, Box::new(OnceChunk::new(bytes.into())))?
        };
        tx.commit().await?;
        Ok(read)
    }

    /// Read a retained payload through this app's original policy authority.
    ///
    /// # Errors
    /// Rejects missing history, unavailable policy and failed storage reads.
    pub async fn read_payload(
        &self,
        run_id: &str,
        generation: i64,
        slot: PayloadSlot,
    ) -> Result<PayloadRead, WorkflowServiceError> {
        let authority = Arc::new(self.capture_policy().authority()?.clone());
        let scope = self.clone().with_authority(authority.as_ref().clone())?;
        let read = authority
            .run(Box::pin(scope.read_payload_inner(run_id, generation, slot)))
            .await?;
        Ok(read.guarded(Some(authority)))
    }

    async fn read_payload_inner(
        &self,
        run_id: &str,
        generation: i64,
        slot: PayloadSlot,
    ) -> Result<PayloadRead, WorkflowServiceError> {
        validate_run(run_id)?;
        let storage = storage(&self.service)?;
        let mut tx = self.service.begin().await?;
        lock_app(&mut tx, &self.app).await?;
        lock_run(&mut tx, &self.app, run_id).await?;
        let row = reference_at(&mut tx, &self.app, run_id, generation, slot).await?;
        let read = open_payload(&storage, &self.app, &row).await?;
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

struct AuthorizedSource {
    inner: Option<BoxByteStream>,
    authority: Arc<PolicyAuthority>,
}

#[async_trait::async_trait(?Send)]
impl ChunkSource for AuthorizedSource {
    async fn next_chunk(&mut self) -> Option<ChunkResult> {
        let inner = self.inner.as_mut()?;
        let read = self
            .authority
            .run(async { Ok(inner.next_chunk().await) })
            .await;
        if let Ok(chunk) = read {
            if !matches!(chunk, Some(Ok(_))) {
                self.inner = None;
            }
            chunk
        } else {
            self.inner = None;
            Some(Err(StorageError::Stream(
                "workflow payload authority is unavailable".into(),
            )))
        }
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
    let mut ownership = vec![Predicate::And(vec![
        object.column(models::payloads::state).eq("referenced")?,
        edge.column(models::payload_refs::run_id).eq(id.as_str())?,
    ])];
    if let Some(task) = run.optional_text("task_id")? {
        ownership.push(Predicate::And(vec![
            object.column(models::payloads::state).eq("staged")?,
            object.column(models::payloads::run_id).eq(id.as_str())?,
            object.column(models::payloads::generation).eq(generation)?,
            object.column(models::payloads::task_id).eq(task.as_str())?,
            Predicate::compare(
                Operand::Path(object.column(models::payloads::expires_at).asc().path),
                CompareOp::Gt,
                Operand::Lit(Literal::Int(now)),
            ),
        ]));
    }
    let rows = db
        .from(&object)
        .left_join(
            &edge,
            Predicate::And(vec![
                object
                    .column(models::payloads::app_id)
                    .eq_column(edge.column(models::payload_refs::app_id))?,
                object
                    .column(models::payloads::id)
                    .eq_column(edge.column(models::payload_refs::payload_id))?,
                edge.column(models::payload_refs::run_id).eq(id.as_str())?,
                edge.column(models::payload_refs::generation)
                    .eq(generation)?,
            ]),
        )?
        .filter(Predicate::And(vec![
            object.column(models::payloads::app_id).eq(app.as_str())?,
            object
                .column(models::payloads::hash)
                .eq(reference.hash.as_str())?,
            object.column(models::payloads::size).eq(reference.size)?,
            object
                .column(models::payloads::content_type)
                .eq(reference.content_type.as_deref())?,
            Predicate::Or(ownership),
        ]))
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
            Predicate::And(vec![
                edge.column(models::payload_refs::app_id)
                    .eq_column(object.column(models::payloads::app_id))?,
                edge.column(models::payload_refs::payload_id)
                    .eq_column(object.column(models::payloads::id))?,
            ]),
        )?
        .filter(Predicate::And(vec![
            edge.column(models::payload_refs::app_id).eq(app.as_str())?,
            edge.column(models::payload_refs::run_id).eq(run_id)?,
            edge.column(models::payload_refs::generation)
                .eq(generation)?,
            edge.column(models::payload_refs::slot).eq(kind)?,
            edge.column(models::payload_refs::ordinal).eq(ordinal)?,
            object.column(models::payloads::state).eq("referenced")?,
        ]))
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
                {"$match":{"app_id":app.as_str(), "state":{"$ne":"deleted"}}},
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
pub(crate) fn validate_reference(
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
fn storage(service: &WorkflowService) -> Result<Storage, WorkflowServiceError> {
    service.payload_storage.clone().ok_or_else(|| {
        WorkflowServiceError::Unavailable("workflow payload storage is not configured".into())
    })
}
fn storage_error(error: StorageError) -> WorkflowServiceError {
    match error {
        StorageError::InvalidArgument(_) => WorkflowServiceError::InvalidRequest(
            "workflow payload did not match its descriptor".into(),
        ),
        StorageError::LimitExceeded(_) => WorkflowServiceError::PayloadTooLarge,
        _ => WorkflowServiceError::Unavailable("workflow payload storage failed".into()),
    }
}
async fn open_payload(
    storage: &Storage,
    app: &AppId,
    row: &models::PayloadRecord,
) -> Result<PayloadRead, WorkflowServiceError> {
    let reference = reference_from(row);
    let (meta, body) = storage
        .get_stream(app.as_str(), &row.id)
        .await
        .map_err(storage_error)?
        .ok_or_else(|| {
            WorkflowServiceError::Unavailable("committed workflow payload is missing".into())
        })?;
    if meta.size != reference.size as u64 {
        return Err(WorkflowServiceError::Unavailable(
            "committed workflow payload size changed".into(),
        ));
    }
    PayloadRead::checked(reference, body)
}

struct VerifiedSource {
    inner: BoxChunkSource,
    expected: WorkflowOutputRef,
    bytes: u64,
    hash: Sha256,
    verified: Rc<Cell<bool>>,
    finished: bool,
}
#[async_trait::async_trait(?Send)]
impl ChunkSource for VerifiedSource {
    async fn next_chunk(&mut self) -> Option<ChunkResult> {
        if self.finished {
            return None;
        }
        match self.inner.next_chunk().await {
            Some(Ok(chunk)) => {
                if self
                    .bytes
                    .checked_add(chunk.len() as u64)
                    .is_none_or(|size| size > self.expected.size as u64)
                {
                    self.finished = true;
                    return Some(Err(StorageError::InvalidArgument(
                        "workflow payload size mismatch".into(),
                    )));
                }
                self.bytes += chunk.len() as u64;
                self.hash.update(&chunk);
                Some(Ok(chunk))
            }
            Some(Err(error)) => {
                self.finished = true;
                Some(Err(error))
            }
            None => {
                self.finished = true;
                let hash = format!("{:x}", self.hash.clone().finalize());
                if self.bytes != self.expected.size as u64 || hash != self.expected.hash {
                    Some(Err(StorageError::InvalidArgument(
                        "workflow payload digest mismatch".into(),
                    )))
                } else {
                    self.verified.set(true);
                    None
                }
            }
        }
    }
}
