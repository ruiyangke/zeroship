use super::*;
use crate::{
    backend::WorkflowBackend,
    engine::WorkflowOutputRef,
    service::{app::lock_app, publication::JobPublisher, PayloadRead, PayloadSlot},
};
use futures::{
    future::{select, Either},
    FutureExt,
};
use std::{
    cell::Cell,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};
use zeroship_core::{
    workflow_coordination::WorkerId,
    workflow_jobs::{Delivery, JobLease, JobSpec},
};
use zeroship_storage::{
    backend::{
        BoxByteStream, BoxChunkSource, ChunkResult, ChunkSource, ListPage, ListRequest, ObjectMeta,
        OnceChunk,
    },
    Backend, LocalFs, StorageError, StorageStore,
};

#[compio::test]
async fn sqlite_backend_queue_keeps_original_deadline_after_refresh() {
    let directory = tempfile::tempdir().unwrap();
    queued_backend(Rc::new(
        sqlite_store(&directory.path().join("app.sqlite")).await,
    ))
    .await;
}

#[compio::test]
async fn postgres_backend_queue_keeps_original_deadline_after_refresh() {
    let fixture = PostgresFixture::start().await;
    queued_backend(Rc::new(fixture.store.clone())).await;
}

#[expect(
    clippy::future_not_send,
    reason = "the fixture drives a native app journal"
)]
async fn queued_backend(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let binding = service.policies.current_binding(&app).unwrap();
    let backend = service
        .bind_app(&binding)
        .unwrap()
        .into_backend(1024)
        .unwrap();
    let before = ingress_state(&service, &app).await;
    let mut blocker = service.begin().await.unwrap();
    lock_app(&mut blocker, &app).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    binding
        .begin_refresh()
        .unwrap()
        .install(
            PolicySnapshot::lease(2.try_into().unwrap(), AppPolicy::default(), deadline)
                .unwrap()
                .with_ingress_epoch(Some(open_epoch())),
        )
        .unwrap();
    let mut pending = backend
        .start("Example".into(), StartOptions::default())
        .boxed_local();
    // Polling enqueues without yielding to the local dispatch task. Refresh then
    // precedes dequeue, while the app lock keeps the eventual mutation blocked.
    assert!(futures::poll!(pending.as_mut()).is_pending());
    binding
        .begin_refresh()
        .unwrap()
        .install(
            PolicySnapshot::lease(
                2.try_into().unwrap(),
                AppPolicy::default(),
                deadline + Duration::from_secs(30),
            )
            .unwrap()
            .with_ingress_epoch(Some(open_epoch())),
        )
        .unwrap();
    assert!(matches!(
        compio::time::timeout(Duration::from_secs(3), pending)
            .await
            .unwrap(),
        Err(WorkflowServiceError::Unavailable(_))
    ));
    binding.authority().unwrap().check().unwrap();
    blocker.commit().await.unwrap();
    assert_eq!(ingress_state(&service, &app).await, before);
    backend
        .start("Example".into(), StartOptions::default())
        .await
        .unwrap();
}

struct Publisher {
    app: AppId,
    calls: Cell<usize>,
}

struct Lease {
    delivery: Delivery,
    expires: Instant,
}
impl JobLease for Lease {
    fn delivery(&self) -> &Delivery {
        &self.delivery
    }
    fn remaining(&self) -> Option<Duration> {
        self.expires.checked_duration_since(Instant::now())
    }
}
impl JobPublisher for Publisher {
    fn app_id(&self) -> &AppId {
        &self.app
    }
    fn submit(
        &self,
        job: &JobSpec,
    ) -> impl std::future::Future<Output = Result<JobSpec, WorkflowServiceError>> {
        self.calls.set(self.calls.get() + 1);
        std::future::ready(Ok(job.clone()))
    }
}

#[compio::test]
async fn sqlite_retired_publication_handle_only_replays_committed_receipt() {
    let directory = tempfile::tempdir().unwrap();
    Box::pin(retired_publication(Rc::new(
        sqlite_store(&directory.path().join("app.sqlite")).await,
    )))
    .await;
}

#[compio::test]
async fn postgres_retired_publication_handle_only_replays_committed_receipt() {
    let fixture = PostgresFixture::start().await;
    Box::pin(retired_publication(Rc::new(fixture.store.clone()))).await;
}

#[expect(
    clippy::future_not_send,
    reason = "publication fixtures use the native app journal"
)]
async fn retired_publication(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let old = service.fixture_app(app.clone());
    let publisher = Publisher {
        app: app.clone(),
        calls: Cell::new(0),
    };
    old.start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let confirmed = old.pending_jobs(None, 1).await.unwrap().remove(0);
    assert_eq!(
        old.publish_job(&confirmed.id, &publisher).await.unwrap(),
        confirmed
    );
    old.start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let pending = old.pending_jobs(None, 1).await.unwrap().remove(0);
    let replacement = service.policies.bind(app.clone()).unwrap();
    replacement
        .begin_refresh()
        .unwrap()
        .install(leased_policy(1, AppPolicy::default()))
        .unwrap();
    let before = ingress_state(&service, &app).await;
    let calls = publisher.calls.get();
    assert!(matches!(
        old.pending_jobs(None, 1).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert!(matches!(
        old.publish_job(&pending.id, &publisher).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_eq!(
        old.publish_job(&confirmed.id, &publisher).await.unwrap(),
        confirmed
    );
    assert_eq!(publisher.calls.get(), calls);
    assert_eq!(ingress_state(&service, &app).await, before);
    let current = service.bind_app(&replacement).unwrap();
    assert_eq!(
        current.pending_jobs(None, 1).await.unwrap().as_slice(),
        std::slice::from_ref(&pending)
    );
    assert_eq!(
        current.publish_job(&pending.id, &publisher).await.unwrap(),
        pending
    );
    assert_eq!(publisher.calls.get(), calls + 1);

    let captured_scope = current
        .with_authority(replacement.authority().unwrap())
        .unwrap();
    let lease = Lease {
        delivery: Delivery {
            job: pending.clone(),
            worker_id: WorkerId::mint(),
            assignment_revision: 1.try_into().unwrap(),
            attempt: 1.try_into().unwrap(),
            deadline: 1.try_into().unwrap(),
        },
        expires: Instant::now() + Duration::from_secs(30),
    };
    let crate::service::delivery::JobAcceptance::Execute(task) =
        captured_scope.accept_job(&lease).await.unwrap()
    else {
        panic!("published run must be executable")
    };
    let receipt = captured_scope
        .complete_job(&task, &lease, execution(json!([{"kind":"RunCompleted"}])))
        .await
        .unwrap();
    replacement.revoke().unwrap();
    assert_eq!(
        captured_scope.job_receipt(&pending).await.unwrap(),
        Some(receipt)
    );
    assert_eq!(
        captured_scope
            .status(&task.assignment().invocation.run_id)
            .await
            .unwrap()
            .state,
        RunState::Completed
    );
    assert!(matches!(
        captured_scope.pending_jobs(None, 1).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
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

#[expect(
    clippy::future_not_send,
    reason = "payload fixture I/O stays on its compio thread"
)]
async fn payload_bodies(store: Rc<OrmStore>) {
    let directory = tempfile::tempdir().unwrap();
    let storage = Arc::new(HeldReads {
        inner: LocalFs::new(directory.path()),
        gate: Mutex::new(None),
    });
    let (service, app, _, _deployments) = registered_service(store).await;
    let service = service
        .with_payload_storage(StorageStore::from_backend(storage.clone()))
        .unwrap();
    let scope = service.fixture_app(app.clone());
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let worker = WorkerIdentity::new("payload-policy".into()).unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    let bytes = br#"{"value":"private"}"#;
    let reference = WorkflowOutputRef {
        hash: crate::service::types::hash(bytes),
        size: i64::try_from(bytes.len()).unwrap(),
        content_type: Some("application/json".into()),
    };
    service
        .stage_payload(
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
    check_returned_bodies(&service, &app, &run.id, storage.as_ref(), bytes).await;
}

#[expect(
    clippy::future_not_send,
    reason = "payload fixture I/O stays on its compio thread"
)]
async fn check_returned_bodies(
    service: &WorkflowService,
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
            let binding = service.policies.bind(app.clone()).unwrap();
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
            let mut read = read_body(&scoped, run, named).await;
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
                    .policies
                    .current_binding(app)
                    .unwrap_or_else(|_| service.policies.bind(app.clone()).unwrap());
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
                    read_body(&renewed, run, named)
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
    service: &WorkflowService,
    app: &AppId,
    binding: &crate::service::PolicyBinding,
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
                .policies
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

#[expect(
    clippy::future_not_send,
    reason = "payload fixture I/O stays on its compio thread"
)]
async fn read_body(scope: &crate::service::AppWorkflows, run: &str, named: bool) -> PayloadRead {
    if named {
        scope.read_step_output(run, "value", 0).await.unwrap()
    } else {
        scope
            .read_payload(run, 0, PayloadSlot::Output)
            .await
            .unwrap()
    }
}
