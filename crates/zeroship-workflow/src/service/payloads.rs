use super::{
    app::{deadline, emit, lock_app, lock_run, not_found, validate_run},
    store::{Row, Transaction},
    tasks::authorized_task,
    AppWorkflows, RequestId, TaskToken, WorkerIdentity, WorkflowService,
};
use crate::{
    engine::{StepCheckpoint, WorkflowOutputRef},
    validation, WorkflowServiceError,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{cell::Cell, rc::Rc, time::Duration};
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_storage::{
    backend::{BoxByteStream, BoxChunkSource, ChunkResult, ChunkSource, OnceChunk},
    Namespace, Storage, StorageError, StorageStore,
};

pub(crate) const MAX_COLLECTION_BATCH: usize = 1024;

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
    pub async fn stage_payload(
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
        let table = tx.table("payloads");
        let existing = tx
            .query(
                &format!("SELECT * FROM {table} WHERE app_id=$1 AND task_id=$2 AND request_id=$3"),
                &[
                    claim.app.as_str().into(),
                    task_id.into(),
                    request.as_str().into(),
                ],
            )
            .await?;
        let id = if let Some(row) = existing.first() {
            if reference_from(row)? != reference {
                return Err(WorkflowServiceError::Conflict(
                    "payload request was used for another object".into(),
                ));
            }
            let id = row.text("id")?;
            if matches!(row.text("state")?.as_str(), "staged" | "referenced") {
                tx.commit().await?;
                return Ok(StagedPayload { id, reference });
            }
            claim.validate_live()?;
            if row.text("state")? != "uploading" || row.integer("expires_at")? <= claim.now {
                return Err(WorkflowServiceError::Conflict(
                    "payload upload has expired".into(),
                ));
            }
            id
        } else {
            claim.validate_live()?;
            claim.policy.admit()?;
            if reference.size > claim.policy.max_payload_bytes {
                return Err(WorkflowServiceError::PayloadTooLarge);
            }
            let total = tx.query(&format!("SELECT CAST(COALESCE(SUM(size),0) AS BIGINT) AS total,COUNT(*) AS objects FROM {table} WHERE app_id=$1 AND state <> 'deleted'"), &[claim.app.as_str().into()]).await?;
            if total[0].integer("objects")? >= claim.policy.max_payload_objects
                || total[0]
                    .integer("total")?
                    .checked_add(reference.size)
                    .is_none_or(|total| total > claim.policy.max_payload_storage_bytes)
            {
                return Err(WorkflowServiceError::ResourceExhausted(
                    "workflow payload storage limit reached".into(),
                ));
            }
            let id = typed_id::generate(typed_id::WORKFLOW_PAYLOAD_PREFIX);
            tx.execute(&format!("INSERT INTO {table} (app_id,run_id,generation,id,task_id,request_id,hash,size,content_type,state,created_at,expires_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,'uploading',$10,$11)"),
                &[claim.app.as_str().into(),claim.run.text("id")?.into(),claim.run.integer("generation")?.into(),id.clone().into(),task_id.into(),request.as_str().into(),reference.hash.clone().into(),reference.size.into(),reference.content_type.clone().into(),claim.now.into(),deadline(claim.now,claim.policy.payload_staging_retention_ms)?.into()]).await?;
            id
        };
        tx.commit().await?;

        // Lock in the same order as completion and GC. A bounded upload holds
        // this lock until the store finishes, so GC cannot race a live writer.
        let mut tx = self.begin().await?;
        let claim = authorized_task(&mut tx, worker, task_id, token).await?;
        claim.validate_live()?;
        claim.policy.admit()?;
        let row = payload(&mut tx, &claim.app, &id).await?;
        match row.text("state")?.as_str() {
            "staged" | "referenced" => {
                tx.commit().await?;
                return Ok(StagedPayload { id, reference });
            }
            "uploading" if row.integer("expires_at")? > claim.now => {}
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
        let remaining = claim
            .task
            .integer("deadline")?
            .min(row.integer("expires_at")?)
            - claim.now;
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
        tx.execute(
            &format!("UPDATE {table} SET state='staged' WHERE app_id=$1 AND id=$2"),
            &[claim.app.as_str().into(), id.clone().into()],
        )
        .await?;
        tx.commit().await?;
        Ok(StagedPayload { id, reference })
    }

    /// Task reads follow committed replay edges or that task's own staged objects.
    pub async fn read_task_payload(
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
        let table = tx.table("payloads");
        let now = tx.now().await?;
        let (scope, app_ids) = tx.host_app_scope()?;
        let candidates = tx.query(&format!("SELECT app_id,id FROM {table} WHERE app_id IN ({scope}) AND state IN ('uploading','staged','deleting','deleted') AND expires_at <= $2 ORDER BY expires_at,app_id,id LIMIT $3"), &[app_ids,now.into(),(limit as i64).into()]).await?;
        tx.commit().await?;
        let mut collected = 0;
        for candidate in candidates {
            let app = AppId::parse(&candidate.text("app_id")?).map_err(|_| {
                WorkflowServiceError::Internal("invalid payload app identity".into())
            })?;
            let id = candidate.text("id")?;
            let mut tx = self.begin().await?;
            lock_app(&mut tx, &app).await?;
            let now = tx.now().await?;
            let row = payload(&mut tx, &app, &id).await?;
            if !matches!(
                row.text("state")?.as_str(),
                "uploading" | "staged" | "deleting" | "deleted"
            ) || row.integer("expires_at")? > now
            {
                continue;
            }
            let refs = tx.table("payload_refs");
            if !tx
                .query(
                    &format!(
                        "SELECT payload_id FROM {refs} WHERE app_id=$1 AND payload_id=$2 LIMIT 1"
                    ),
                    &[app.as_str().into(), id.clone().into()],
                )
                .await?
                .is_empty()
            {
                return Err(WorkflowServiceError::Internal(
                    "referenced payload was scheduled for collection".into(),
                ));
            }
            tx.execute(
                &format!("UPDATE {table} SET state='deleting' WHERE app_id=$1 AND id=$2"),
                &[app.as_str().into(), id.clone().into()],
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
            tx.execute(&format!("UPDATE {table} SET state='deleted',expires_at=$3 WHERE app_id=$1 AND id=$2 AND state='deleting'"), &[app.as_str().into(),id.into(),deadline(now,policy.payload_staging_retention_ms)?.into()]).await?;
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
        validate_run(run_id)?;
        validation::step_name(name)?;
        let occurrence = i32::try_from(occurrence).map_err(|_| {
            WorkflowServiceError::InvalidRequest("invalid step name occurrence".into())
        })?;
        let mut tx = self.service.begin().await?;
        lock_app(&mut tx, &self.app).await?;
        let run = lock_run(&mut tx, &self.app, run_id).await?;
        let generation = run.integer("generation")?;
        let rows = tx.query(
            &format!("SELECT record FROM {} WHERE app_id=$1 AND run_id=$2 AND generation=$3 AND name=$4 AND occurrence=$5", tx.table("steps")),
            &[self.app.as_str().into(), run_id.into(), generation.into(), name.into(), i64::from(occurrence).into()],
        ).await?;
        let row = rows
            .first()
            .ok_or_else(|| not_found("workflow step output"))?;
        let step: StepCheckpoint = super::app::decode(&row.text("record")?)?;
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
            if reference_from(&row)? != reference {
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

    pub async fn read_payload(
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
    row: &Row,
    now: i64,
) -> Result<(), WorkflowServiceError> {
    let (kind, ordinal) = slot.coordinates()?;
    let table = tx.table("payloads");
    let refs = tx.table("payload_refs");
    let id = row.text("id")?;
    tx.execute(&format!("INSERT INTO {refs} (app_id,run_id,generation,slot,ordinal,payload_id) VALUES ($1,$2,$3,$4,$5,$6)"), &[app.as_str().into(),run_id.into(),generation.into(),kind.into(),ordinal.into(),id.clone().into()]).await?;
    if row.text("state")? == "staged" {
        tx.execute(
            &format!("UPDATE {table} SET state='referenced' WHERE app_id=$1 AND id=$2"),
            &[app.as_str().into(), id.clone().into()],
        )
        .await?;
        emit(
            tx,
            app,
            &format!("{id}:retained"),
            "workflow.payload.retained",
            serde_json::json!({"payloadId":id,"bytes":row.integer("size")?}),
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
    reference_from(&row)
}

async fn owned_reference(
    tx: &mut Transaction,
    app: &AppId,
    run: &Row,
    reference: &WorkflowOutputRef,
    now: i64,
) -> Result<Row, WorkflowServiceError> {
    let table = tx.table("payloads");
    let refs = tx.table("payload_refs");
    let rows = tx.query(&format!("SELECT p.* FROM {table} p WHERE p.app_id=$1 AND p.hash=$2 AND p.size=$3 AND (p.content_type=$4 OR (p.content_type IS NULL AND $4 IS NULL)) AND ((p.state='staged' AND p.run_id=$5 AND p.generation=$6 AND p.task_id=$7 AND p.expires_at>$8) OR (p.state='referenced' AND EXISTS (SELECT 1 FROM {refs} r WHERE r.app_id=p.app_id AND r.payload_id=p.id AND r.run_id=$5 AND r.generation=$6))) ORDER BY p.id LIMIT 1"),
        &[app.as_str().into(),reference.hash.clone().into(),reference.size.into(),reference.content_type.clone().into(),run.text("id")?.into(),run.integer("generation")?.into(),run.optional_text("task_id")?.into(),now.into()]).await?;
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
) -> Result<Row, WorkflowServiceError> {
    let (kind, ordinal) = slot.coordinates()?;
    let table = tx.table("payloads");
    let refs = tx.table("payload_refs");
    tx.query(&format!("SELECT p.* FROM {refs} r JOIN {table} p ON p.app_id=r.app_id AND p.id=r.payload_id WHERE r.app_id=$1 AND r.run_id=$2 AND r.generation=$3 AND r.slot=$4 AND r.ordinal=$5 AND p.state='referenced'"), &[app.as_str().into(),run_id.into(),generation.into(),kind.into(),ordinal.into()]).await?.into_iter().next().ok_or_else(|| not_found("workflow payload"))
}

async fn payload(tx: &mut Transaction, app: &AppId, id: &str) -> Result<Row, WorkflowServiceError> {
    let table = tx.table("payloads");
    tx.query(
        &format!("SELECT * FROM {table} WHERE app_id=$1 AND id=$2"),
        &[app.as_str().into(), id.into()],
    )
    .await?
    .into_iter()
    .next()
    .ok_or_else(|| not_found("workflow payload"))
}
fn reference_from(row: &Row) -> Result<WorkflowOutputRef, WorkflowServiceError> {
    Ok(WorkflowOutputRef {
        hash: row.text("hash")?,
        size: row.integer("size")?,
        content_type: row.optional_text("content_type")?,
    })
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
    row: &Row,
) -> Result<PayloadRead, WorkflowServiceError> {
    let reference = reference_from(row)?;
    let (meta, body) = storage
        .get_stream(app.as_str(), &row.text("id")?)
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
