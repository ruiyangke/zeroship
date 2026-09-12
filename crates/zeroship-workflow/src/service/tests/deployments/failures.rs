use super::*;
use crate::service::{runner::TaskTransport, WorkerIdentity};
use futures::channel::oneshot;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};
use std::time::Duration;
use zeroship_bundle::{BlobError, PutOutcome};

#[derive(Debug)]
struct Gate {
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}
impl Gate {
    async fn wait(self) {
        self.entered.send(()).unwrap();
        self.release.await.unwrap();
    }
}
fn gate(slot: &Mutex<Option<Gate>>) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (entered, observe) = oneshot::channel();
    let (release, wait) = oneshot::channel();
    *slot.lock().unwrap() = Some(Gate {
        entered,
        release: wait,
    });
    (observe, release)
}

#[derive(Debug)]
struct Faults {
    local: LocalDiskBlobStore,
    fail_read: AtomicBool,
    missing_read: Mutex<Option<Gate>>,
    slow_read: Mutex<Option<Gate>>,
}
#[async_trait::async_trait(?Send)]
impl BlobStore for Faults {
    async fn get_blob(&self, hash: &str) -> Result<bytes::Bytes, BlobError> {
        self.local.get_blob(hash).await
    }
    fn local_path(&self, hash: &str) -> Option<std::path::PathBuf> {
        self.local.local_path(hash)
    }
    async fn put_blob_stream(
        &self,
        hash: &str,
        expected_size: u64,
        reader: &mut dyn std::io::Read,
    ) -> Result<PutOutcome, BlobError> {
        self.local
            .put_blob_stream(hash, expected_size, reader)
            .await
    }
    async fn has_blob(&self, hash: &str) -> Result<bool, BlobError> {
        self.local.has_blob(hash).await
    }
    async fn probe(&self) -> Result<(), BlobError> {
        self.local.probe().await
    }
    async fn get_blob_to_file(
        &self,
        hash: &str,
        out: &compio::fs::File,
        expected_size: Option<u64>,
        max_bytes: u64,
    ) -> Result<u64, BlobError> {
        self.local
            .get_blob_to_file(hash, out, expected_size, max_bytes)
            .await
    }
    async fn put_manifest(
        &self,
        app: &uuid::Uuid,
        hash: &str,
        bytes: &[u8],
    ) -> Result<(), BlobError> {
        self.local.put_manifest(app, hash, bytes).await
    }
    async fn get_manifest(&self, app: &uuid::Uuid, hash: &str) -> Result<bytes::Bytes, BlobError> {
        if self.fail_read.swap(false, Ordering::SeqCst) {
            return Err(BlobError::Backend("injected read failure".into()));
        }
        let missing = self.missing_read.lock().unwrap().take();
        if let Some(gate) = missing {
            gate.wait().await;
            return Err(BlobError::NotFound(hash.into()));
        }
        let object = self.local.get_manifest(app, hash).await?;
        let slow = self.slow_read.lock().unwrap().take();
        if let Some(gate) = slow {
            gate.wait().await;
        }
        Ok(object)
    }
    async fn delete_manifest(&self, app: &uuid::Uuid, hash: &str) -> Result<bool, BlobError> {
        self.local.delete_manifest(app, hash).await
    }
    async fn delete_app_manifests(&self, app: &uuid::Uuid) -> Result<(), BlobError> {
        self.local.delete_app_manifests(app).await
    }
}

#[compio::test]
async fn sqlite_artifact_io_does_not_block_work_or_revoke_a_repair() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    Box::pin(fault_contract(Rc::new(sqlite_store(&path).await))).await;
}

#[compio::test]
async fn postgres_artifact_io_does_not_block_work_or_revoke_a_repair() {
    let fixture = PostgresFixture::start().await;
    Box::pin(fault_contract(Rc::new(fixture.store.clone()))).await;
}

#[expect(
    clippy::too_many_lines,
    reason = "exercise failures and competing operations over the same live claims"
)]
#[expect(
    clippy::future_not_send,
    reason = "the database and storage contract runs on its compio thread"
)]
async fn fault_contract(store: Rc<OrmStore>) {
    let dir = Arc::new(tempfile::tempdir().unwrap());
    let faults = Arc::new(Faults {
        local: LocalDiskBlobStore::new(dir.path().join("artifacts")).unwrap(),
        fail_read: false.into(),
        missing_read: Mutex::new(None),
        slow_read: Mutex::new(None),
    });
    let deployments = Deployments::with_source(dir, faults.clone()).await;
    let (service, app, _, deployments) = registered_with_deployments(store, deployments).await;
    let scope = service.for_app(app.clone());
    let tasks = service.tasks(WorkerIdentity::new("artifact-worker".into()).unwrap());
    let old = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = tasks.poll().await.unwrap().unwrap();
    let next_image = image("next");
    let next = deployments
        .publish(&app, &deployment('b'), &next_image)
        .await
        .unwrap();
    faults.fail_read.store(true, Ordering::SeqCst);
    assert!(matches!(
        service.activate_deploy(&app, &next).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    let retained = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let retained_task = tasks.poll().await.unwrap().unwrap();
    assert_eq!(retained_task.invocation.run_id, retained.id);
    assert_eq!(
        retained_task.invocation.deploy_id,
        task.invocation.deploy_id
    );
    Sources::default().assert_loaded(&tasks.executable(&retained_task).await.unwrap());

    let (entered, release) = gate(&faults.slow_read);
    let (activated, concurrent) = futures::join!(service.activate_deploy(&app, &next), async {
        entered.await.unwrap();
        let result = compio::time::timeout(
            Duration::from_secs(5),
            tasks.heartbeat(&task.id, &task.token),
        )
        .await;
        release.send(()).unwrap();
        result
    });
    concurrent
        .expect("artifact read held the customer app lock")
        .unwrap();
    activated.unwrap();
    let new = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let new_task = tasks.poll().await.unwrap().unwrap();
    assert_eq!(new_task.invocation.run_id, new.id);
    assert_eq!(new_task.invocation.deploy_id, next.id);

    faults.fail_read.store(true, Ordering::SeqCst);
    assert!(matches!(
        tasks.executable(&new_task).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    next_image.assert_loaded(&tasks.executable(&new_task).await.unwrap());

    let (entered, release) = gate(&faults.missing_read);
    let (stale, repaired) = futures::join!(tasks.executable(&new_task), async {
        entered.await.unwrap();
        let result = compio::time::timeout(
            Duration::from_secs(5),
            Box::pin(service.retain_deploy(&app, &next)),
        )
        .await;
        release.send(()).unwrap();
        result
    });
    repaired
        .expect("artifact read held the customer app lock")
        .unwrap();
    assert!(matches!(stale, Err(WorkflowServiceError::Unavailable(_))));
    next_image.assert_loaded(&tasks.executable(&new_task).await.unwrap());

    // A successful read cannot outlive the captured task claim.
    let (entered, release) = gate(&faults.slow_read);
    let (stale, released) = futures::join!(tasks.executable(&new_task), async {
        entered.await.unwrap();
        let result = compio::time::timeout(
            Duration::from_secs(5),
            tasks.release(&new_task.id, &new_task.token),
        )
        .await;
        release.send(()).unwrap();
        result
    });
    released
        .expect("artifact read blocked task release")
        .unwrap();
    assert!(matches!(stale, Err(WorkflowServiceError::Conflict(_))));
    assert!(tasks.executable(&new_task).await.is_err());
    Sources::default().assert_loaded(&tasks.executable(&task).await.unwrap());
    assert_eq!(task.invocation.run_id, old.id);

    // Preparation cannot reopen admission after its hold has been replaced.
    let unused = deployments.deploy(&app).await;
    let client = deployments.client(&app);
    let (entered, release) = gate(&faults.slow_read);
    let (stale, reacquired) = futures::join!(service.retain_deploy(&app, &unused), async {
        entered.await.unwrap();
        let result = compio::time::timeout(Duration::from_secs(5), async {
            let released = service
                .release_deployment_hold(&app, &unused.id, &client)
                .await?;
            let held = service
                .acquire_deployment_hold(&app, &unused.id, &unused.hash, &client)
                .await?;
            assert!(held.generation.get() > released.generation.get());
            Ok::<_, WorkflowServiceError>(())
        })
        .await;
        release.send(()).unwrap();
        result
    });
    reacquired
        .expect("artifact read blocked hold replacement")
        .unwrap();
    assert!(matches!(stale, Err(WorkflowServiceError::Conflict(_))));
    let tx = service.begin().await.unwrap();
    assert!(super::super::super::deploys::read(&tx, &app, &unused.id)
        .await
        .unwrap()
        .is_none());
    tx.commit().await.unwrap();
    service.activate_deploy(&app, &unused).await.unwrap();
}
