//! The payload body contract: integrity verification, and the policy authority
//! a returned body carries past the transaction that authorized it.

#![expect(
    clippy::future_not_send,
    reason = "payload fixture I/O stays on its compio thread"
)]

use crate::{
    journal_fixture::{
        execution, leased_policy, registered_service, sqlite_store, PostgresFixture,
    },
    service_binding::ServiceFixture,
    ObjectStepOutputs, PayloadObjects, PayloadRead, RunPayloads, WorkerPayloads,
};
use futures::{
    future::{select, Either},
    FutureExt,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use zeroship_core::app_id::AppId;
use zeroship_storage::{
    backend::{
        BoxByteStream, BoxChunkSource, ChunkResult, ChunkSource, ListPage, ListRequest, ObjectMeta,
        OnceChunk,
    },
    Backend, LocalFs, StorageError, StorageStore,
};
use zeroship_workflow::{
    engine::WorkflowOutputRef,
    operations::StartOptions,
    service::{AppPolicy, AppWorkflows, PayloadSlot, PolicySnapshot, RequestId, WorkerIdentity},
    StepOutputReader, WorkflowServiceError,
};

fn reference(bytes: &[u8]) -> WorkflowOutputRef {
    WorkflowOutputRef {
        hash: format!("{:x}", Sha256::digest(bytes)),
        size: i64::try_from(bytes.len()).unwrap(),
        content_type: Some("application/json".into()),
    }
}

/// A buffered read reports corruption at end of stream, where the digest is
/// settled, rather than handing a caller content that never matched.
#[compio::test]
async fn buffered_payload_reads_consume_integrity_verification_at_eof() {
    let bytes = b"valid";
    for actual in [b"wrong".as_slice(), b"truncated", b""] {
        let read = PayloadRead::checked(
            WorkflowOutputRef {
                hash: format!("{:x}", Sha256::digest(bytes)),
                size: i64::try_from(bytes.len()).unwrap(),
                content_type: None,
            },
            Box::new(OnceChunk::new(actual.to_vec().into())),
        )
        .unwrap();
        assert!(matches!(
            read.into_bytes(1024).await,
            Err(WorkflowServiceError::Unavailable(_))
        ));
    }
}

#[compio::test]
async fn sqlite_journal_step_outputs_read_as_verified_json_bodies() {
    let directory = tempfile::tempdir().unwrap();
    Box::pin(inline_step_bodies(Rc::new(
        sqlite_store(&directory.path().join("app.sqlite")).await,
    )))
    .await;
}

#[compio::test]
async fn postgres_journal_step_outputs_read_as_verified_json_bodies() {
    let fixture = PostgresFixture::start().await;
    Box::pin(inline_step_bodies(Rc::new(fixture.store.clone()))).await;
}

/// A step small enough to stay in the journal still reaches execution as a
/// payload body: the runner gives it a descriptor and the read budget applies.
async fn inline_step_bodies(store: Rc<zeroship_workflow::service::store::OrmStore>) {
    let directory = tempfile::tempdir().unwrap();
    let objects =
        PayloadObjects::open(StorageStore::from_backend(Arc::new(LocalFs::new(
            directory.path(),
        ))))
        .unwrap();
    assert!(matches!(
        ObjectStepOutputs::new(objects.clone(), 0),
        Err(WorkflowServiceError::InvalidRequest(_))
    ));
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app);
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let worker = WorkerIdentity::new("inline-step-reader".into()).unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([
                {"kind":"StepCompleted", "ordinal":0, "name":"value", "output":{"value":1}},
                {"kind":"RunCompleted"}
            ])),
        )
        .await
        .unwrap();
    let body = json!({"value":1});
    let expected = serde_json::to_vec(&body).unwrap();
    let read = scope
        .payloads(&objects)
        .read_step_output(&run.id, "value", 0)
        .await
        .unwrap();
    assert_eq!(
        read.reference.content_type.as_deref(),
        Some("application/json")
    );
    assert_eq!(read.reference, reference(&expected));
    assert_eq!(read.into_bytes(1024).await.unwrap(), expected);
    let read = scope
        .payloads(&objects)
        .read_step_output(&run.id, "value", 0)
        .await
        .unwrap();
    assert!(matches!(
        read.into_bytes(1).await,
        Err(WorkflowServiceError::PayloadTooLarge)
    ));
}

#[compio::test]
async fn sqlite_run_outputs_read_as_bodies_inside_the_host_read_budget() {
    let directory = tempfile::tempdir().unwrap();
    Box::pin(run_output_bodies(Rc::new(
        sqlite_store(&directory.path().join("app.sqlite")).await,
    )))
    .await;
}

#[compio::test]
async fn postgres_run_outputs_read_as_bodies_inside_the_host_read_budget() {
    let fixture = PostgresFixture::start().await;
    Box::pin(run_output_bodies(Rc::new(fixture.store.clone()))).await;
}

/// A run's final output reaches a caller holding only the run id: under this
/// app's ownership, under the host's read budget, and only when an object
/// holds it.
async fn run_output_bodies(store: Rc<zeroship_workflow::service::store::OrmStore>) {
    let directory = tempfile::tempdir().unwrap();
    let objects =
        PayloadObjects::open(StorageStore::from_backend(Arc::new(LocalFs::new(
            directory.path(),
        ))))
        .unwrap();
    let (service, app, other, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app);
    let stranger = service.fixture_app(other);
    let worker = WorkerIdentity::new("run-output-reader".into()).unwrap();
    let blob = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    let bytes = br#"{"value":"final"}"#;
    let output = reference(bytes);
    service
        .payloads(&objects)
        .stage(
            &worker,
            &task.id,
            &task.token,
            &RequestId::mint(),
            output.clone(),
            Box::new(OnceChunk::new(bytes.to_vec().into())),
        )
        .await
        .unwrap();
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunCompleted", "outputRef":output}])),
        )
        .await
        .unwrap();
    let reader = ObjectStepOutputs::new(objects.clone(), 1024).unwrap();
    assert_eq!(reader.read_output(&scope, &blob.id).await.unwrap(), bytes);
    // One variable differs from the read above: the budget the host granted.
    let budgeted = ObjectStepOutputs::new(objects.clone(), bytes.len() - 1).unwrap();
    assert!(matches!(
        budgeted.read_output(&scope, &blob.id).await,
        Err(WorkflowServiceError::PayloadTooLarge)
    ));
    // One variable differs from the first read: who is asking.
    assert!(matches!(
        reader.read_output(&stranger, &blob.id).await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    // One variable differs from the first read: whether the run produced a
    // result at all. A run that returned nothing stages no object, so a caller
    // holding its id has nothing to open.
    let empty = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    assert_eq!(
        scope.status(&empty.id).await.unwrap().output,
        None,
        "the control run must report no output for this to be a control"
    );
    assert!(matches!(
        reader.read_output(&scope, &empty.id).await,
        Err(WorkflowServiceError::NotFound(_))
    ));
}

/// The staged-upload, verified-read and collection path over an S3-compatible
/// store, whose transfer, digest and deletion semantics differ from a local
/// filesystem's.
#[compio::test]
async fn s3_staged_payloads_round_trip_and_collect() {
    let minio = crate::s3_fixture::Minio::start();
    let directory = tempfile::tempdir().unwrap();
    let objects = PayloadObjects::open(StorageStore::from_backend(Arc::new(
        zeroship_storage::S3::new(minio.config("payloads"), minio.credentials()),
    )))
    .unwrap();
    let store = Rc::new(sqlite_store(&directory.path().join("app.sqlite")).await);
    let (service, app, _, _deployments) = registered_service(store.clone()).await;
    let scope = service.fixture_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let worker = WorkerIdentity::new("s3-payload-worker".into()).unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    let bytes = br#"{"value":"stored"}"#;

    // A body that does not match its descriptor never becomes a staged record.
    assert!(service
        .payloads(&objects)
        .stage(
            &worker,
            &task.id,
            &task.token,
            &RequestId::mint(),
            reference(bytes),
            Box::new(OnceChunk::new(b"other".to_vec().into())),
        )
        .await
        .is_err());

    let staged = service
        .payloads(&objects)
        .stage(
            &worker,
            &task.id,
            &task.token,
            &RequestId::mint(),
            reference(bytes),
            Box::new(OnceChunk::new(bytes.to_vec().into())),
        )
        .await
        .unwrap();
    assert_eq!(
        service
            .payloads(&objects)
            .read_task(&worker, &task.id, &task.token, &reference(bytes))
            .await
            .unwrap()
            .into_bytes(1024)
            .await
            .unwrap(),
        bytes
    );

    expire(&service, &app, &staged.id).await;
    while service.payloads(&objects).collect(64).await.unwrap() > 0 {
        expire(&service, &app, &staged.id).await;
    }
    assert!(service
        .payloads(&objects)
        .read_task(&worker, &task.id, &task.token, &reference(bytes))
        .await
        .is_err());
}

/// Bring a staged payload's retention deadline forward so collection selects it.
async fn expire(
    service: &zeroship_workflow::service::WorkflowService,
    app: &AppId,
    id: &str,
) {
    use zeroship_data_orm::value;
    let tx = service.begin().await.unwrap();
    tx.database()
        .collection("__zeroship_workflow_payloads")
        .unwrap()
        .update(
            value!({"app_id":app.as_str(), "id":id}),
            value!({"expires_at":0}),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

#[derive(Debug)]
struct ReadGate {
    entered: flume::Sender<()>,
    release: flume::Receiver<()>,
    dropped: Arc<AtomicBool>,
}

struct BlockedBody {
    inner: BoxByteStream,
    gate: Option<ReadGate>,
    dropped: Arc<AtomicBool>,
}
impl Drop for BlockedBody {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}
#[async_trait::async_trait(?Send)]
impl ChunkSource for BlockedBody {
    async fn next_chunk(&mut self) -> Option<ChunkResult> {
        if let Some(gate) = self.gate.take() {
            gate.entered.send(()).unwrap();
            gate.release.recv_async().await.unwrap();
        }
        self.inner.next_chunk().await
    }
}

#[derive(Debug)]
struct HeldReads {
    inner: LocalFs,
    gate: Mutex<Option<ReadGate>>,
}
impl HeldReads {
    fn arm(&self) -> (flume::Receiver<()>, flume::Sender<()>, Arc<AtomicBool>) {
        let (entered, observed) = flume::bounded(1);
        let (release, blocked) = flume::bounded(1);
        let dropped = Arc::new(AtomicBool::new(false));
        assert!(self
            .gate
            .lock()
            .unwrap()
            .replace(ReadGate {
                entered,
                release: blocked,
                dropped: dropped.clone()
            })
            .is_none());
        (observed, release, dropped)
    }
}

#[async_trait::async_trait(?Send)]
impl Backend for HeldReads {
    async fn put_stream(
        &self,
        app: &str,
        bucket: &str,
        key: &str,
        body: BoxChunkSource,
        content_type: Option<&str>,
    ) -> Result<u64, StorageError> {
        self.inner
            .put_stream(app, bucket, key, body, content_type)
            .await
    }
    async fn get_stream(
        &self,
        app: &str,
        bucket: &str,
        key: &str,
    ) -> Result<Option<(ObjectMeta, BoxByteStream)>, StorageError> {
        let Some((meta, body)) = self.inner.get_stream(app, bucket, key).await? else {
            return Ok(None);
        };
        let gate = self.gate.lock().unwrap().take();
        let body: BoxByteStream = if let Some(gate) = gate {
            let dropped = gate.dropped.clone();
            Box::new(BlockedBody {
                inner: body,
                gate: Some(gate),
                dropped,
            })
        } else {
            body
        };
        Ok(Some((meta, body)))
    }
    async fn list(
        &self,
        app: &str,
        bucket: &str,
        request: ListRequest<'_>,
    ) -> Result<ListPage, StorageError> {
        self.inner.list(app, bucket, request).await
    }
    async fn delete(&self, app: &str, bucket: &str, key: &str) -> Result<bool, StorageError> {
        self.inner.delete(app, bucket, key).await
    }
}

#[derive(Clone, Copy, Debug)]
enum BodyChange {
    Extend,
    Revoke,
    Replace,
    Shorten,
}

#[compio::test]
async fn sqlite_returned_payload_body_retains_original_policy_authority() {
    let directory = tempfile::tempdir().unwrap();
    Box::pin(payload_bodies(Rc::new(
        sqlite_store(&directory.path().join("app.sqlite")).await,
    )))
    .await;
}

#[compio::test]
async fn postgres_returned_payload_body_retains_original_policy_authority() {
    let fixture = PostgresFixture::start().await;
    Box::pin(payload_bodies(Rc::new(fixture.store.clone()))).await;
}

async fn payload_bodies(store: Rc<zeroship_workflow::service::store::OrmStore>) {
    let directory = tempfile::tempdir().unwrap();
    let backend = Arc::new(HeldReads {
        inner: LocalFs::new(directory.path()),
        gate: Mutex::new(None),
    });
    let objects = PayloadObjects::open(StorageStore::from_backend(backend.clone())).unwrap();
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let worker = WorkerIdentity::new("payload-policy".into()).unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    let bytes = br#"{"value":"private"}"#;
    let reference = reference(bytes);
    service
        .payloads(&objects)
        .stage(
            &worker,
            &task.id,
            &task.token,
            &RequestId::mint(),
            reference.clone(),
            Box::new(OnceChunk::new(bytes.to_vec().into())),
        )
        .await
        .unwrap();
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([
                {"kind":"StepCompleted", "ordinal":0, "name":"value", "outputRef":reference},
                {"kind":"RunCompleted", "outputRef":reference}
            ])),
        )
        .await
        .unwrap();
    check_returned_bodies(&service, &objects, &app, &run.id, backend.as_ref(), bytes).await;
}

async fn check_returned_bodies(
    service: &zeroship_workflow::service::WorkflowService,
    objects: &PayloadObjects,
    app: &AppId,
    run: &str,
    storage: &HeldReads,
    bytes: &[u8],
) {
    for named in [false, true] {
        for change in [
            BodyChange::Extend,
            BodyChange::Revoke,
            BodyChange::Replace,
            BodyChange::Shorten,
        ] {
            let binding = service.policies().bind(app.clone()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(60);
            binding
                .begin_refresh()
                .unwrap()
                .install(
                    PolicySnapshot::lease(2.try_into().unwrap(), AppPolicy::default(), deadline)
                        .unwrap(),
                )
                .unwrap();
            let scoped = service.bind_app(&binding).unwrap();
            let (entered, release, dropped) = storage.arm();
            let mut read = read_body(&scoped, objects, run, named).await;
            let pending = read.body.next_chunk().boxed_local();
            let pending = match select(entered.recv_async().boxed_local(), pending).await {
                Either::Left((observed, pending)) => {
                    observed.unwrap();
                    pending
                }
                Either::Right((chunk, _)) => panic!("body bypassed its storage barrier: {chunk:?}"),
            };
            change_body_policy(service, app, &binding, deadline, change);
            if matches!(change, BodyChange::Extend) {
                assert!(!dropped.load(Ordering::SeqCst));
                release.send(()).unwrap();
                assert_eq!(pending.await.unwrap().unwrap().as_ref(), bytes);
                assert!(read.body.next_chunk().await.is_none());
            } else {
                assert!(
                    matches!(
                        compio::time::timeout(Duration::from_secs(3), pending)
                            .await
                            .unwrap(),
                        Some(Err(StorageError::Stream(_)))
                    ),
                    "{change:?}"
                );
                assert!(dropped.load(Ordering::SeqCst), "{change:?}");
                assert!(
                    release.send(()).is_err(),
                    "invalidated read released its storage source"
                );
                let current = service
                    .policies()
                    .current_binding(app)
                    .unwrap_or_else(|_| service.policies().bind(app.clone()).unwrap());
                current
                    .begin_refresh()
                    .unwrap()
                    .install(leased_policy(2, AppPolicy::default()))
                    .unwrap();
                assert!(
                    read.body.next_chunk().await.is_none(),
                    "refresh resurrected an invalidated stream"
                );
                let renewed = service.bind_app(&current).unwrap();
                assert_eq!(
                    read_body(&renewed, objects, run, named)
                        .await
                        .into_bytes(1024)
                        .await
                        .unwrap(),
                    bytes
                );
            }
            assert!(dropped.load(Ordering::SeqCst));
        }
    }
}

fn change_body_policy(
    service: &zeroship_workflow::service::WorkflowService,
    app: &AppId,
    binding: &zeroship_workflow::service::PolicyBinding,
    deadline: Instant,
    change: BodyChange,
) {
    match change {
        BodyChange::Extend => binding
            .begin_refresh()
            .unwrap()
            .install(
                PolicySnapshot::lease(
                    2.try_into().unwrap(),
                    AppPolicy::default(),
                    deadline + Duration::from_secs(30),
                )
                .unwrap(),
            )
            .unwrap(),
        BodyChange::Revoke => binding.revoke().unwrap(),
        BodyChange::Replace => {
            service
                .policies()
                .bind(app.clone())
                .unwrap()
                .begin_refresh()
                .unwrap()
                .install(leased_policy(2, AppPolicy::default()))
                .unwrap();
        }
        BodyChange::Shorten => binding
            .begin_refresh()
            .unwrap()
            .install(
                PolicySnapshot::lease(
                    2.try_into().unwrap(),
                    AppPolicy::default(),
                    Instant::now() + Duration::from_secs(30),
                )
                .unwrap(),
            )
            .unwrap(),
    }
}

async fn read_body(
    scope: &AppWorkflows,
    objects: &PayloadObjects,
    run: &str,
    named: bool,
) -> PayloadRead {
    if named {
        scope
            .payloads(objects)
            .read_step_output(run, "value", 0)
            .await
            .unwrap()
    } else {
        scope
            .payloads(objects)
            .read(run, 0, PayloadSlot::Output)
            .await
            .unwrap()
    }
}
