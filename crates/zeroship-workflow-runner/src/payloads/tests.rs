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
    ObjectStepOutputs, PayloadObjects, PayloadRead, RunPayloads, TaskPayloadReader, TaskPayloads,
    WorkerBinding, WorkerPayloads, WorkerTasks,
};
use futures::{
    future::{select, Either},
    FutureExt,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
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
    service::{
        AppPolicy, AppWorkflows, PayloadSlot, PolicySnapshot, RequestId, TaskToken,
        WorkerIdentity,
    },
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
    let server = crate::s3_fixture::S3Server::start();
    let directory = tempfile::tempdir().unwrap();
    let objects = PayloadObjects::open(StorageStore::from_backend(Arc::new(
        zeroship_storage::S3::new(server.config("payloads"), server.credentials()),
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

/// A transport that answers a payload read with a different object of the same
/// run than the one the reader named.
///
/// Every answer is a real read through the real transport, so the body still
/// verifies against the descriptor that comes back and the substitute is an
/// object this app owns. The one thing that changes is which descriptor that
/// is -- the shape a payload read takes once it crosses a network boundary and
/// the reader can no longer be the thing that selected the row.
struct SubstitutedDescriptor {
    inner: WorkerTasks,
    answer: Option<WorkflowOutputRef>,
    asked: RefCell<Vec<WorkflowOutputRef>>,
}
impl SubstitutedDescriptor {
    fn passthrough(inner: &WorkerTasks) -> Rc<Self> {
        Rc::new(Self {
            inner: inner.clone(),
            answer: None,
            asked: RefCell::default(),
        })
    }
    fn answering(inner: &WorkerTasks, answer: &WorkflowOutputRef) -> Rc<Self> {
        Rc::new(Self {
            inner: inner.clone(),
            answer: Some(answer.clone()),
            asked: RefCell::default(),
        })
    }
}
#[async_trait::async_trait(?Send)]
impl TaskPayloads for SubstitutedDescriptor {
    async fn executable(
        &self,
        task: &str,
        token: &TaskToken,
    ) -> Result<zeroship_bundle::LoadedWorker, WorkflowServiceError> {
        TaskPayloads::executable(&self.inner, task, token).await
    }
    async fn stage(
        &self,
        task: &str,
        token: &TaskToken,
        request: &RequestId,
        reference: WorkflowOutputRef,
        body: BoxChunkSource,
    ) -> Result<crate::payloads::UploadReceipt, WorkflowServiceError> {
        self.inner
            .stage(task, token, request, reference, body)
            .await
    }
    async fn read(
        &self,
        task: &str,
        token: &TaskToken,
        reference: &WorkflowOutputRef,
    ) -> Result<PayloadRead, WorkflowServiceError> {
        self.asked.borrow_mut().push(reference.clone());
        self.inner
            .read(task, token, self.answer.as_ref().unwrap_or(reference))
            .await
    }
}

/// A replay read whose descriptor is not the one the journal named is refused,
/// whether the hash moved or the size did.
///
/// Three real objects belong to this run. `swapped` is byte-for-byte the same
/// length as the one the journal names, so substituting it moves the hash and
/// nothing else; `longer` moves the size too, which is what a stale descriptor
/// looks like. Both are read back through a pass-through transport first, so a
/// refusal below cannot be an object the run could not reach.
///
/// The last arm is the other refusal `read_verified` can produce. It differs
/// from the control in the host's read budget alone and answers
/// `PayloadTooLarge`, which is how this test tells the descriptor comparison
/// apart from the pre-open size check standing in front of it.
///
/// The store is not a variable here: the comparison is in the runner, over
/// whatever a transport answered, so one backend exercises it.
#[compio::test]
async fn replay_refuses_a_payload_read_that_answers_with_another_descriptor() {
    let directory = tempfile::tempdir().unwrap();
    let store = Rc::new(sqlite_store(&directory.path().join("app.sqlite")).await);
    let objects = PayloadObjects::open(StorageStore::from_backend(Arc::new(LocalFs::new(
        directory.path(),
    ))))
    .unwrap();
    let (service, app, _, _deployments) = Box::pin(registered_service(store)).await;
    let scope = service.fixture_app(app);
    let worker = WorkerIdentity::new("descriptor-guard".into()).unwrap();
    let tasks = service.tasks(worker.clone(), objects);

    let named = br#"{"value":"original"}"#.to_vec();
    let swapped = br#"{"value":"replaced"}"#.to_vec();
    let longer = br#"{"value":"original and then some"}"#.to_vec();
    assert_eq!(
        named.len(),
        swapped.len(),
        "the hash arm must move the hash and nothing else"
    );
    assert_ne!(named.len(), longer.len(), "the size arm must move the size");
    assert_ne!(reference(&named).hash, reference(&swapped).hash);

    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    for bytes in [&named, &swapped, &longer] {
        tasks
            .stage(
                &task.id,
                &task.token,
                &RequestId::mint(),
                reference(bytes),
                Box::new(OnceChunk::new(bytes.clone().into())),
            )
            .await
            .unwrap();
    }
    service
        .complete(&worker, 
            &task.id,
            &task.token,
            execution(json!([
                {"kind":"StepCompleted","ordinal":0,"name":"named","outputRef":reference(&named)},
                {"kind":"StepCompleted","ordinal":1,"name":"swapped","outputRef":reference(&swapped)},
                {"kind":"StepCompleted","ordinal":2,"name":"longer","outputRef":reference(&longer)},
            ])),
        )
        .await
        .unwrap();
    let replay = service.poll(&worker).await.unwrap().unwrap();

    let budget = 256;
    assert!(
        longer.len() < budget,
        "no arm here may be the read budget refusing"
    );

    // The control: the same fake, answering with the descriptor it was handed.
    let control = SubstitutedDescriptor::passthrough(&tasks);
    let reader = TaskPayloadReader::new(control.clone(), &replay, budget).unwrap();
    assert_eq!(reader.read_step_output("named", 0).await.unwrap(), named);
    assert_eq!(*control.asked.borrow(), vec![reference(&named)]);

    // Both substitutes are objects this replay can read on their own name.
    for (step, bytes) in [("swapped", &swapped), ("longer", &longer)] {
        let reader =
            TaskPayloadReader::new(SubstitutedDescriptor::passthrough(&tasks), &replay, budget)
                .unwrap();
        assert_eq!(&reader.read_step_output(step, 0).await.unwrap(), bytes);
    }

    // The arms. One variable differs from the control: which descriptor comes
    // back from a read the reader still issued for the journal's own. Both run
    // before either is judged, so one verdict covers the hash and the size.
    let mut outcomes = Vec::new();
    for substitute in [&swapped, &longer] {
        let fake = SubstitutedDescriptor::answering(&tasks, &reference(substitute));
        let reader = TaskPayloadReader::new(fake.clone(), &replay, budget).unwrap();
        let outcome = reader.read_step_output("named", 0).await;
        assert_eq!(*fake.asked.borrow(), vec![reference(&named)]);
        // A reader that lost a replay dependency stays lost, so the host
        // abandons this execution instead of journaling an app-visible error.
        assert_eq!(reader.check().err(), outcome.as_ref().err().cloned());
        outcomes.push(outcome);
    }
    let refused =
        WorkflowServiceError::Unavailable("workflow replay payload descriptor changed".into());
    assert_eq!(outcomes, vec![Err(refused.clone()), Err(refused)]);

    // The pre-open budget check, which the arms above must not have taken.
    let tight = TaskPayloadReader::new(
        SubstitutedDescriptor::passthrough(&tasks),
        &replay,
        named.len() - 1,
    )
    .unwrap();
    assert_eq!(
        tight.read_step_output("named", 0).await.unwrap_err(),
        WorkflowServiceError::PayloadTooLarge
    );
}

/// A transport holding a fixed set of objects, answering a read with the one
/// whose descriptor it names and recording every read it is asked for.
#[derive(Default)]
struct HeldObjects {
    objects: Vec<Vec<u8>>,
    asked: RefCell<Vec<WorkflowOutputRef>>,
}
#[async_trait::async_trait(?Send)]
impl TaskPayloads for HeldObjects {
    async fn executable(
        &self,
        _task: &str,
        _token: &TaskToken,
    ) -> Result<zeroship_bundle::LoadedWorker, WorkflowServiceError> {
        Err(WorkflowServiceError::Internal(
            "replay reads no executable".into(),
        ))
    }
    async fn stage(
        &self,
        _task: &str,
        _token: &TaskToken,
        _request: &RequestId,
        _reference: WorkflowOutputRef,
        _body: BoxChunkSource,
    ) -> Result<crate::payloads::UploadReceipt, WorkflowServiceError> {
        Err(WorkflowServiceError::Internal(
            "replay stages nothing".into(),
        ))
    }
    async fn read(
        &self,
        _task: &str,
        _token: &TaskToken,
        named: &WorkflowOutputRef,
    ) -> Result<PayloadRead, WorkflowServiceError> {
        self.asked.borrow_mut().push(named.clone());
        let bytes = self
            .objects
            .iter()
            .find(|bytes| reference(bytes) == *named)
            .ok_or_else(|| WorkflowServiceError::NotFound("no such fixture object".into()))?;
        PayloadRead::checked(
            named.clone(),
            Box::new(OnceChunk::new(bytes.clone().into())),
        )
    }
}

/// A delivered assignment replaying `journal`, as the service hands one out.
fn replaying(journal: &serde_json::Value) -> zeroship_workflow::service::TaskAssignment {
    serde_json::from_value(json!({
        "id": "task_replay", "token": "0".repeat(64), "generation": 1, "epoch": 1,
        "deadline": 0, "leaseMs": 60_000,
        "invocation": {
            "appId": AppId::mint().as_str(), "deployId": "dep_replay", "deployHash": "a".repeat(64),
            "runId": "run_replay", "generation": 1, "workflowName": "Example", "phase": "running",
            "trigger": {"input": null, "startedAt": "2026-09-30T00:00:00Z",
                "runId": "run_replay", "workflowName": "Example"},
            "journal": journal,
        },
    }))
    .unwrap()
}

fn completed(ordinal: i32, name: &str, kind: &str, bytes: &[u8]) -> serde_json::Value {
    json!({"ordinal":ordinal,"name":name,"kind":kind,"state":"completed",
        "output":null,"outputRef":reference(bytes),"error":null})
}

const CHILD: &[u8] = br#"{"doubled":42}"#;
const STEP: &[u8] = br#"{"stored":true}"#;
/// The replay budget a configured host runs with, so only the cases about the
/// budget are decided by it.
const BUDGET: usize = zeroship_core::workflow_policy::MAX_CHILD_OUTPUT_BYTES_CEILING;

/// A completed child's bytes take the place of its descriptor. A `run` step's
/// descriptor stays, because its body asked for a reference, and a child that
/// returned nothing stays JSON null. Only the child's object is read.
#[compio::test]
async fn a_replay_journal_carries_child_bytes_and_keeps_run_references() {
    let transport = Rc::new(HeldObjects {
        objects: vec![CHILD.to_vec(), STEP.to_vec()],
        ..HeldObjects::default()
    });
    let assignment = replaying(&json!([
        completed(0, "Child", "child", CHILD),
        completed(1, "stored", "run", STEP),
        {"ordinal":2,"name":"Quiet","kind":"child","state":"completed","output":null,"error":null},
    ]));
    let reader = TaskPayloadReader::new(transport.clone(), &assignment, 1024).unwrap();
    let journal = reader
        .replay_journal(BUDGET)
        .await
        .unwrap()
        .expect("a journal naming a child's output is hydrated");
    let child = serde_json::to_value(&journal[0]).unwrap();
    assert_eq!(child["output"], json!({"doubled":42}), "{child}");
    assert_eq!(child["outputRef"], serde_json::Value::Null, "{child}");
    // The `run` step and the child that returned nothing are left exactly as
    // assigned.
    for index in [1, 2] {
        assert_eq!(
            serde_json::to_value(&journal[index]).unwrap(),
            serde_json::to_value(&assignment.invocation.journal[index]).unwrap()
        );
    }
    assert_eq!(journal[1].output_ref, Some(reference(STEP)));
    assert_eq!(*transport.asked.borrow(), vec![reference(CHILD)]);
}

/// The envelope carries a stored child's bytes exactly as the object holds
/// them: spliced, not parsed and encoded again, so the envelope grows by the
/// size the descriptor names and no more.
#[compio::test]
async fn the_replay_envelope_carries_stored_child_bytes_verbatim() {
    // Spacing and a trailing zero a parse and re-encode would both drop.
    let stored = br#"{"b": 1,  "a": 2.50}"#;
    let reencoded =
        serde_json::to_vec(&serde_json::from_slice::<serde_json::Value>(stored).unwrap()).unwrap();
    assert_ne!(
        reencoded, stored,
        "the bytes must tell a splice from a re-encode"
    );
    let transport = Rc::new(HeldObjects {
        objects: vec![stored.to_vec()],
        ..HeldObjects::default()
    });
    let assignment = replaying(&json!([completed(0, "Child", "child", stored)]));
    let reader = TaskPayloadReader::new(transport, &assignment, 1024).unwrap();
    let envelope = reader.replay_envelope(BUDGET).await.unwrap();
    let spliced = format!("\"output\":{}", std::str::from_utf8(stored).unwrap());
    assert!(envelope.contains(&spliced), "{envelope}");
    assert!(!envelope.contains("outputRef\":{"), "{envelope}");
    // The whole envelope is the assigned invocation with the child's output in
    // place of its descriptor and nothing else moved, so the borrowed identity
    // fields cannot drop out of the serialized form.
    let mut expected = serde_json::to_value(&assignment.invocation).unwrap();
    expected["journal"][0]["output"] = json!({"b": 1, "a": 2.5});
    expected["journal"][0]["outputRef"] = serde_json::Value::Null;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&envelope).unwrap(),
        expected
    );
}

/// A journal naming no child's output is already what the isolate replays: it
/// comes back as nothing to replace, and nothing is read.
#[compio::test]
async fn a_journal_naming_no_child_output_is_replayed_as_assigned() {
    let transport = Rc::new(HeldObjects {
        objects: vec![STEP.to_vec()],
        ..HeldObjects::default()
    });
    let assignment = replaying(&json!([completed(0, "stored", "run", STEP)]));
    let reader = TaskPayloadReader::new(transport.clone(), &assignment, 1024).unwrap();
    assert!(reader.replay_journal(BUDGET).await.unwrap().is_none());
    assert!(transport.asked.borrow().is_empty());
}

/// A child object that is not JSON is refused as unavailable.
#[compio::test]
async fn a_child_output_that_is_not_json_is_unavailable() {
    let garbage = b"not json".to_vec();
    let transport = Rc::new(HeldObjects {
        objects: vec![garbage.clone()],
        ..HeldObjects::default()
    });
    let assignment = replaying(&json!([completed(0, "Child", "child", &garbage)]));
    let reader = TaskPayloadReader::new(transport, &assignment, 1024).unwrap();
    assert_eq!(
        reader.replay_journal(BUDGET).await.unwrap_err(),
        WorkflowServiceError::Unavailable("workflow child output payload is not valid JSON".into())
    );
}

/// A child object over the read budget is refused before it is opened, and the
/// reader stays failed, so the host abandons the execution.
#[compio::test]
async fn a_child_output_over_the_read_budget_fails_the_reader() {
    let transport = Rc::new(HeldObjects {
        objects: vec![CHILD.to_vec()],
        ..HeldObjects::default()
    });
    let assignment = replaying(&json!([completed(0, "Child", "child", CHILD)]));
    let reader = TaskPayloadReader::new(transport.clone(), &assignment, CHILD.len() - 1).unwrap();
    assert_eq!(
        reader.replay_journal(BUDGET).await.unwrap_err(),
        WorkflowServiceError::PayloadTooLarge
    );
    assert_eq!(reader.check(), Err(WorkflowServiceError::PayloadTooLarge));
    assert!(transport.asked.borrow().is_empty());
}

/// The replay budget bounds the stored bytes a replay splices in, summed over
/// its children: at their total they are read, one byte under it nothing is
/// read and the replay is refused as the internal fault it is, since the
/// journal admits no more than a configured budget's floor.
#[compio::test]
async fn the_replay_budget_bounds_the_stored_bytes_of_every_child() {
    let assignment = replaying(&json!([
        completed(0, "First", "child", CHILD),
        completed(1, "Second", "child", STEP),
    ]));
    let stored = CHILD.len() + STEP.len();
    let transport = Rc::new(HeldObjects {
        objects: vec![CHILD.to_vec(), STEP.to_vec()],
        ..HeldObjects::default()
    });
    let reader = TaskPayloadReader::new(transport.clone(), &assignment, 1024).unwrap();
    assert!(reader.replay_journal(stored).await.unwrap().is_some());
    assert_eq!(transport.asked.borrow().len(), 2);

    let transport = Rc::new(HeldObjects {
        objects: vec![CHILD.to_vec(), STEP.to_vec()],
        ..HeldObjects::default()
    });
    let reader = TaskPayloadReader::new(transport.clone(), &assignment, 1024).unwrap();
    assert!(matches!(
        reader.replay_journal(stored - 1).await.unwrap_err(),
        WorkflowServiceError::Internal(_)
    ));
    assert!(transport.asked.borrow().is_empty());
}

/// A child-output total the host cannot represent is refused as unrepresentable,
/// naming the overflow rather than a budget comparison it never made.
#[compio::test]
async fn a_child_output_total_the_host_cannot_represent_is_unrepresentable() {
    let mut journal = json!([
        completed(0, "First", "child", CHILD),
        completed(1, "Second", "child", STEP),
        completed(2, "Third", "child", CHILD),
    ]);
    // Each size fits the signed field a descriptor carries; their sum overflows
    // the host's `usize`, so no budget can bound it.
    for index in 0..3 {
        journal[index]["outputRef"]["size"] = json!(i64::MAX);
    }
    let transport = Rc::new(HeldObjects::default());
    let reader = TaskPayloadReader::new(transport.clone(), &replaying(&journal), BUDGET).unwrap();
    assert_eq!(
        reader.replay_journal(BUDGET).await.unwrap_err(),
        WorkflowServiceError::Internal("workflow child output total is unrepresentable".into())
    );
    assert!(transport.asked.borrow().is_empty());
}
