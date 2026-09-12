use super::*;
use crate::{
    engine::WorkflowOutputRef,
    operations::RunState,
    service::{PayloadRead, PayloadSlot, WorkerIdentity},
};
use crate::service::runner::TaskPayloadReader;
use std::rc::Rc;
use bytes::Bytes;
use zeroship_storage::{
    backend::{BoxChunkSource, OnceChunk},
    LocalFs, StorageStore,
};

#[path = "../../../../../tests/fixtures/s3.rs"]
mod s3_fixture;

fn body(value: &'static [u8]) -> BoxChunkSource {
    Box::new(OnceChunk::new(Bytes::from_static(value)))
}
fn reference(value: &[u8]) -> WorkflowOutputRef {
    WorkflowOutputRef {
        hash: crate::service::types::hash(value),
        size: value.len() as i64,
        content_type: Some("application/json".into()),
    }
}
async fn drain(mut value: PayloadRead) -> Vec<u8> {
    let mut bytes = Vec::new();
    while let Some(chunk) = value.body.next_chunk().await {
        bytes.extend(chunk.unwrap());
    }
    bytes
}
fn local(dir: &Path) -> StorageStore {
    StorageStore::from_backend(Arc::new(LocalFs::new(dir)))
}

#[compio::test]
async fn sqlite_payload_ownership_and_retention() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    payload_contract(
        Arc::new(SqliteStore::new(path)),
        local(&dir.path().join("payloads")),
    )
    .await;
}

#[compio::test]
async fn postgres_payload_ownership_and_retention() {
    let fixture = PostgresFixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    payload_contract(Arc::new(fixture.store.clone()), local(dir.path())).await;
}

#[compio::test]
async fn s3_payload_ownership_and_retention() {
    let fixture = s3_fixture::Minio::start();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    let storage = StorageStore::from_backend(Arc::new(zeroship_storage::S3::new(
        fixture.config("workflows"),
        fixture.credentials(),
    )));
    payload_contract(Arc::new(SqliteStore::new(path)), storage).await;
}

async fn payload_contract(store: Arc<dyn WorkflowStore>, storage: StorageStore) {
    let (service, a, b) = registered_service(store.clone()).await;
    let service = service.with_payload_storage(storage.clone()).unwrap();
    let scope = service.for_app(a.clone());
    let foreign = service.for_app(b);
    let worker = WorkerIdentity::new("payload-worker".into()).unwrap();
    let stranger = WorkerIdentity::new("stranger".into()).unwrap();
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    let data = br#"{"value":"retained"}"#;
    let output = reference(data);
    let request = RequestId::mint();
    assert!(matches!(
        service
            .stage_payload(
                &stranger,
                &task.id,
                &task.token,
                &request,
                output.clone(),
                body(data)
            )
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    let staged = service
        .stage_payload(
            &worker,
            &task.id,
            &task.token,
            &request,
            output.clone(),
            body(data),
        )
        .await
        .unwrap();
    assert_eq!(
        staged,
        service
            .stage_payload(
                &worker,
                &task.id,
                &task.token,
                &request,
                output.clone(),
                body(b"ignored retry body")
            )
            .await
            .unwrap()
    );
    assert!(matches!(
        service
            .stage_payload(
                &worker,
                &task.id,
                &task.token,
                &request,
                reference(b"another"),
                body(b"another")
            )
            .await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    assert_eq!(
        drain(
            service
                .read_task_payload(&worker, &task.id, &task.token, &output)
                .await
                .unwrap()
        )
        .await,
        data
    );
    assert!(matches!(
        scope.read_payload(&run.id, 0, PayloadSlot::Output).await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert!(matches!(
        foreign.read_payload(&run.id, 0, PayloadSlot::Output).await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    for other in [&foreign, &scope] {
        other
            .start(&RequestId::mint(), "Child", StartOptions::default())
            .await
            .unwrap();
        let other_task = service.poll(&worker).await.unwrap().unwrap();
        assert_ne!(other_task.id, task.id);
        assert!(matches!(
            service
                .read_task_payload(&worker, &other_task.id, &other_task.token, &output)
                .await,
            Err(WorkflowServiceError::NotFound(_))
        ));
        assert!(matches!(
            service
                .complete(
                    &worker,
                    &other_task.id,
                    &other_task.token,
                    execution(json!([{"kind":"RunCompleted","outputRef":output}]))
                )
                .await,
            Err(WorkflowServiceError::NotFound(_))
        ));
        service
            .complete(
                &worker,
                &other_task.id,
                &other_task.token,
                execution(json!([{"kind":"RunCompleted"}])),
            )
            .await
            .unwrap();
    }
    // A mismatched body never becomes an admissible descriptor.
    let wrong = reference(b"x");
    let interrupted = RequestId::mint();
    assert!(matches!(
        service
            .stage_payload(
                &worker,
                &task.id,
                &task.token,
                &interrupted,
                wrong.clone(),
                body(b"y")
            )
            .await,
        Err(WorkflowServiceError::InvalidRequest(_))
    ));
    assert!(matches!(
        service
            .read_task_payload(&worker, &task.id, &task.token, &wrong)
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    service
        .stage_payload(
            &worker,
            &task.id,
            &task.token,
            &interrupted,
            wrong.clone(),
            body(b"x"),
        )
        .await
        .unwrap();
    let uncommitted = service
        .stage_payload(
            &worker,
            &task.id,
            &task.token,
            &RequestId::mint(),
            reference(b"uncommitted"),
            body(b"uncommitted"),
        )
        .await
        .unwrap();
    // A late invalid frontier rolls back the earlier payload promotion.
    assert!(service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([
                {"kind":"StepCompleted","ordinal":0,"name":"result","outputRef":output},
                {"kind":"StepCompleted","ordinal":9,"name":"invalid"}
            ]))
        )
        .await
        .is_err());
    let mut tx = store.begin().await.unwrap();
    let payloads = tx.table("payloads");
    let state = tx
        .query(
            &format!("SELECT state FROM {payloads} WHERE app_id=$1 AND id=$2"),
            &[a.as_str().into(), staged.id.clone().into()],
        )
        .await
        .unwrap();
    assert_eq!(state[0].text("state").unwrap(), "staged");
    tx.commit().await.unwrap();
    let completion = execution(json!([
        {"kind":"StepCompleted","ordinal":0,"name":"result","outputRef":output},
        {"kind":"Wait","ordinal":1,"name":"approval","signalType":"approved"}
    ]));
    service
        .complete(&worker, &task.id, &task.token, completion.clone())
        .await
        .unwrap();
    service
        .complete(&worker, &task.id, &task.token, completion)
        .await
        .unwrap();
    expire_uploads(&store).await;
    assert!(service.collect_payloads(64).await.unwrap() > 0);
    assert_eq!(service.collect_payloads(64).await.unwrap(), 0);
    let objects = storage.namespace(zeroship_storage::Namespace::platform("workflow").unwrap());
    assert_eq!(
        scope
            .read_step_output(&run.id, "result", 0)
            .await
            .unwrap()
            .into_bytes(data.len())
            .await
            .unwrap(),
        data
    );
    assert!(matches!(
        foreign.read_step_output(&run.id, "result", 0).await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert!(objects
        .get(a.as_str(), &uncommitted.id)
        .await
        .unwrap()
        .is_none());
    // Simulate an already-sent remote upload arriving after its writer died.
    objects
        .put(
            a.as_str(),
            &uncommitted.id,
            b"uncommitted",
            Some("application/json"),
        )
        .await
        .unwrap();
    expire_uploads(&store).await;
    service.collect_payloads(64).await.unwrap();
    assert!(objects
        .get(a.as_str(), &uncommitted.id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        drain(
            scope
                .read_payload(&run.id, 0, PayloadSlot::Step { ordinal: 0 })
                .await
                .unwrap()
        )
        .await,
        data
    );
    scope
        .restart(
            &RequestId::mint(),
            &run.id,
            crate::operations::RestartOptions {
                from: Some(crate::operations::RestartTarget {
                    name: "approval".into(),
                    occurrence: None,
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let recovered = WorkflowService::open(store.clone(), service.policies.clone())
        .await
        .unwrap()
        .with_payload_storage(storage.clone())
        .unwrap();
    let next = recovered.poll(&worker).await.unwrap().unwrap();
    assert_eq!(next.generation, 1);
    let transport = Rc::new(recovered.tasks(worker.clone()));
    let reader = TaskPayloadReader::new(transport.clone(), &next, data.len()).unwrap();
    assert_eq!(reader.app_id(), &a);
    assert_eq!(reader.run_id(), run.id);
    assert_eq!(reader.read_step_output("result", 0).await.unwrap(), data);
    assert!(matches!(reader.read_step_output("missing", 0).await, Err(WorkflowServiceError::NotFound(_))));
    assert!(matches!(reader.read_step_output("result", 1).await, Err(WorkflowServiceError::NotFound(_))));
    let limited = TaskPayloadReader::new(transport, &next, 1).unwrap();
    assert!(matches!(limited.read_step_output("result", 0).await, Err(WorkflowServiceError::PayloadTooLarge)));
    assert_eq!(
        scope
            .read_step_output(&run.id, "result", 0)
            .await
            .unwrap()
            .into_bytes(data.len())
            .await
            .unwrap(),
        data
    );
    assert_eq!(
        next.invocation.journal[0].output_ref.as_ref(),
        Some(&output)
    );
    assert_eq!(
        drain(
            recovered
                .read_task_payload(&worker, &next.id, &next.token, &output)
                .await
                .unwrap()
        )
        .await,
        data
    );
    assert!(recovered
        .read_task_payload(&worker, &task.id, &task.token, &output)
        .await
        .is_err());
    assert!(recovered
        .read_task_payload(&worker, &next.id, &next.token, &uncommitted.reference)
        .await
        .is_err());
    recovered
        .complete(
            &worker,
            &next.id,
            &next.token,
            execution(json!([{"kind":"RunCompleted","outputRef":output}])),
        )
        .await
        .unwrap();
    assert_eq!(
        scope.status(&run.id).await.unwrap().output.unwrap()["hash"],
        output.hash
    );
    assert!(reader.read_step_output("result", 0).await.is_err());
    assert_eq!(
        drain(
            scope
                .read_payload(&run.id, 1, PayloadSlot::Output)
                .await
                .unwrap()
        )
        .await,
        data
    );

    continuation_and_child(&recovered, &a, &worker).await;
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let quota_task = recovered.poll(&worker).await.unwrap().unwrap();
    let policy = AppPolicy {
        max_payload_bytes: 1,
        max_payload_objects: 1,
        ..Default::default()
    };
    recovered
        .register_app(&a, super::configured_policy(2, policy))
        .await
        .unwrap();
    assert!(matches!(
        recovered
            .stage_payload(
                &worker,
                &quota_task.id,
                &quota_task.token,
                &RequestId::mint(),
                output.clone(),
                body(data)
            )
            .await,
        Err(WorkflowServiceError::PayloadTooLarge)
    ));
    assert!(matches!(
        recovered
            .stage_payload(
                &worker,
                &quota_task.id,
                &quota_task.token,
                &RequestId::mint(),
                reference(b""),
                body(b"")
            )
            .await,
        Err(WorkflowServiceError::ResourceExhausted(_))
    ));
    recovered
        .register_app(&a, super::configured_policy(3, AppPolicy::default()))
        .await
        .unwrap();
    recovered
        .complete(
            &worker,
            &quota_task.id,
            &quota_task.token,
            execution(json!([{ "kind":"RunCompleted" }])),
        )
        .await
        .unwrap();
    expire_uploads(&store).await;
    recovered.collect_payloads(64).await.unwrap();
    assert_eq!(
        drain(
            scope
                .read_payload(&run.id, 0, PayloadSlot::Step { ordinal: 0 })
                .await
                .unwrap()
        )
        .await,
        data
    );
}

async fn expire_uploads(store: &Arc<dyn WorkflowStore>) {
    let mut tx = store.begin().await.unwrap();
    let table = tx.table("payloads");
    tx.execute(&format!("UPDATE {table} SET expires_at=0"), &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

async fn continuation_and_child(service: &WorkflowService, app: &AppId, worker: &WorkerIdentity) {
    let scope = service.for_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = service.poll(worker).await.unwrap().unwrap();
    service.complete(worker, &task.id, &task.token, execution(json!([{"kind":"Child","ordinal":0,"name":"child","childWorkflowName":"Child","input":{},"options":{}}]))).await.unwrap();
    let child = service.poll(worker).await.unwrap().unwrap();
    let data = br#"{"continued":true}"#;
    let output = reference(data);
    service
        .stage_payload(
            worker,
            &child.id,
            &child.token,
            &RequestId::mint(),
            output.clone(),
            body(data),
        )
        .await
        .unwrap();
    service
        .complete(
            worker,
            &child.id,
            &child.token,
            execution(json!([{"kind":"ContinueAsNew","inputRef":output}])),
        )
        .await
        .unwrap();
    let successor = service.poll(worker).await.unwrap().unwrap();
    assert_eq!(successor.invocation.trigger.input_ref, Some(output.clone()));
    let reader = TaskPayloadReader::new(Rc::new(service.tasks(worker.clone())), &successor, data.len()).unwrap();
    assert_eq!(reader.input().await.unwrap(), Some(json!({"continued":true})));
    assert_eq!(
        drain(
            service
                .read_task_payload(worker, &successor.id, &successor.token, &output)
                .await
                .unwrap()
        )
        .await,
        data
    );
    service
        .complete(
            worker,
            &successor.id,
            &successor.token,
            execution(json!([{"kind":"RunCompleted","outputRef":output}])),
        )
        .await
        .unwrap();
    let parent = service.poll(worker).await.unwrap().unwrap();
    assert_eq!(parent.invocation.run_id, task.invocation.run_id);
    assert_eq!(
        parent.invocation.journal[0].output_ref,
        Some(output.clone())
    );
    assert_eq!(
        drain(
            service
                .read_task_payload(worker, &parent.id, &parent.token, &output)
                .await
                .unwrap()
        )
        .await,
        data
    );
    let done = service
        .complete(
            worker,
            &parent.id,
            &parent.token,
            execution(json!([{"kind":"RunCompleted","outputRef":output}])),
        )
        .await
        .unwrap();
    assert_eq!(done.state, RunState::Completed);
}

#[derive(Debug)]
struct FailingDelete {
    inner: LocalFs,
    fail: std::sync::atomic::AtomicBool,
}
#[async_trait::async_trait(?Send)]
impl zeroship_storage::Backend for FailingDelete {
    async fn put_stream(
        &self,
        app: &str,
        bucket: &str,
        key: &str,
        body: BoxChunkSource,
        content_type: Option<&str>,
    ) -> Result<u64, zeroship_storage::StorageError> {
        self.inner
            .put_stream(app, bucket, key, body, content_type)
            .await
    }
    async fn get_stream(
        &self,
        app: &str,
        bucket: &str,
        key: &str,
    ) -> Result<
        Option<(
            zeroship_storage::backend::ObjectMeta,
            zeroship_storage::backend::BoxByteStream,
        )>,
        zeroship_storage::StorageError,
    > {
        self.inner.get_stream(app, bucket, key).await
    }
    async fn list(
        &self,
        app: &str,
        bucket: &str,
        req: zeroship_storage::backend::ListRequest<'_>,
    ) -> Result<zeroship_storage::backend::ListPage, zeroship_storage::StorageError> {
        self.inner.list(app, bucket, req).await
    }
    async fn delete(
        &self,
        app: &str,
        bucket: &str,
        key: &str,
    ) -> Result<bool, zeroship_storage::StorageError> {
        if self.fail.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err(zeroship_storage::StorageError::InvalidArgument(
                "injected delete failure".into(),
            ));
        }
        self.inner.delete(app, bucket, key).await
    }
}

#[compio::test]
async fn deletion_failure_recovers_without_reopening_payload_authority() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    let store: Arc<dyn WorkflowStore> = Arc::new(SqliteStore::new(path));
    let (service, app, _) = registered_service(store.clone()).await;
    let backend = Arc::new(FailingDelete {
        inner: LocalFs::new(dir.path().join("objects")),
        fail: true.into(),
    });
    let storage = StorageStore::from_backend(backend);
    let service = service.with_payload_storage(storage.clone()).unwrap();
    let scope = service.for_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let worker = WorkerIdentity::new("worker".into()).unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    let request = RequestId::mint();
    let output = reference(b"abandoned");
    let staged = service
        .stage_payload(
            &worker,
            &task.id,
            &task.token,
            &request,
            output.clone(),
            body(b"abandoned"),
        )
        .await
        .unwrap();
    expire_uploads(&store).await;
    assert!(service.collect_payloads(64).await.is_err());
    assert!(service
        .stage_payload(
            &worker,
            &task.id,
            &task.token,
            &request,
            output.clone(),
            body(b"abandoned")
        )
        .await
        .is_err());
    assert!(matches!(
        service
            .complete(
                &worker,
                &task.id,
                &task.token,
                execution(json!([{"kind":"RunCompleted","outputRef":output}]))
            )
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    let recovered = WorkflowService::open(store, service.policies.clone())
        .await
        .unwrap()
        .with_payload_storage(storage.clone())
        .unwrap();
    assert_eq!(recovered.collect_payloads(64).await.unwrap(), 1);
    assert!(storage
        .namespace(zeroship_storage::Namespace::platform("workflow").unwrap())
        .get(app.as_str(), &staged.id)
        .await
        .unwrap()
        .is_none());
    recovered
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
}

#[compio::test]
async fn postgres_collection_rechecks_references_after_waiting_for_completion() {
    let fixture = PostgresFixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn WorkflowStore> = Arc::new(fixture.store.clone());
    let (service, app, _) = registered_service(store.clone()).await;
    let service = service.with_payload_storage(local(dir.path())).unwrap();
    let scope = service.for_app(app.clone());
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let worker = WorkerIdentity::new("worker".into()).unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    let output = reference(b"survives");
    service
        .stage_payload(
            &worker,
            &task.id,
            &task.token,
            &RequestId::mint(),
            output.clone(),
            body(b"survives"),
        )
        .await
        .unwrap();
    let admin = connect(&fixture.admin_url).await;
    admin.batch_execute("CREATE FUNCTION customer.delay_payload_promotion() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.state='referenced' THEN PERFORM pg_sleep(1); END IF; RETURN NEW; END $$; CREATE TRIGGER delay_payload_promotion BEFORE UPDATE ON customer.__zeroship_workflow_payloads FOR EACH ROW EXECUTE FUNCTION customer.delay_payload_promotion(); UPDATE customer.__zeroship_workflow_payloads SET expires_at = (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::bigint + 500;").await.unwrap();
    let completing_service = service.clone();
    let completing = compio::runtime::spawn(async move {
        completing_service
            .complete(
                &worker,
                &task.id,
                &task.token,
                execution(json!([{"kind":"RunCompleted","outputRef":output}])),
            )
            .await
    });
    compio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let row = admin.query_one("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE usename='customer_worker' AND wait_event='PgSleep')", &[]).await.unwrap();
            if row.get::<_, bool>(0) { break; }
            compio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("completion reached the delayed promotion");
    compio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert_eq!(service.collect_payloads(64).await.unwrap(), 0);
    completing.await.unwrap().unwrap();
    assert_eq!(
        drain(
            scope
                .read_payload(&run.id, 0, PayloadSlot::Output)
                .await
                .unwrap()
        )
        .await,
        b"survives"
    );
}
