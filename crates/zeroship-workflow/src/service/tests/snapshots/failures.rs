use super::*;
use crate::service::{runner::TaskTransport, WorkerIdentity};
use futures::channel::oneshot;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};
use std::time::Duration;

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
    local: LocalFs,
    fail_write: AtomicBool,
    fail_read: AtomicBool,
    slow_write: Mutex<Option<Gate>>,
    missing_read: Mutex<Option<Gate>>,
    slow_read: Mutex<Option<Gate>>,
}
#[async_trait::async_trait(?Send)]
impl Backend for Faults {
    async fn put_stream(
        &self,
        app: &str,
        bucket: &str,
        key: &str,
        body: BoxChunkSource,
        content_type: Option<&str>,
    ) -> Result<u64, StorageError> {
        if self.fail_write.swap(false, Ordering::SeqCst) {
            return Err(StorageError::InvalidArgument(
                "injected write failure".into(),
            ));
        }
        let gate = self.slow_write.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.wait().await;
        }
        self.local
            .put_stream(app, bucket, key, body, content_type)
            .await
    }
    async fn get_stream(
        &self,
        app: &str,
        bucket: &str,
        key: &str,
    ) -> Result<Option<(ObjectMeta, BoxByteStream)>, StorageError> {
        if self.fail_read.swap(false, Ordering::SeqCst) {
            return Err(StorageError::InvalidArgument(
                "injected read failure".into(),
            ));
        }
        let gate = self.missing_read.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.wait().await;
            return Ok(None);
        }
        let object = self.local.get_stream(app, bucket, key).await?;
        let gate = self.slow_read.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.wait().await;
        }
        Ok(object)
    }
    async fn delete(&self, app: &str, bucket: &str, key: &str) -> Result<bool, StorageError> {
        self.local.delete(app, bucket, key).await
    }
    async fn list(
        &self,
        app: &str,
        bucket: &str,
        req: ListRequest<'_>,
    ) -> Result<ListPage, StorageError> {
        self.local.list(app, bucket, req).await
    }
}

#[compio::test]
async fn sqlite_snapshot_io_does_not_block_work_or_revoke_a_repair() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    fault_contract(Arc::new(SqliteStore::new(path))).await;
}

#[compio::test]
async fn postgres_snapshot_io_does_not_block_work_or_revoke_a_repair() {
    let fixture = PostgresFixture::start().await;
    fault_contract(Arc::new(fixture.store.clone())).await;
}

#[expect(
    clippy::too_many_lines,
    reason = "exercise failures and competing operations over the same live claims"
)]
#[expect(
    clippy::future_not_send,
    reason = "the database and storage contract runs on its compio thread"
)]
async fn fault_contract(store: Arc<dyn WorkflowStore>) {
    let dir = tempfile::tempdir().unwrap();
    let faults = Arc::new(Faults {
        local: LocalFs::new(dir.path()),
        fail_write: false.into(),
        fail_read: false.into(),
        slow_write: Mutex::new(None),
        missing_read: Mutex::new(None),
        slow_read: Mutex::new(None),
    });
    let snapshots =
        SnapshotStore::new(&StorageStore::from_backend(faults.clone()), 1024 * 1024).unwrap();
    let (service, app, _) = registered_with_snapshots(store, snapshots).await;
    let scope = service.for_app(app.clone());
    let tasks = service.tasks(WorkerIdentity::new("snapshot-worker".into()).unwrap());
    let old = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = tasks.poll().await.unwrap().unwrap();
    let next = deployment('b');
    let next_image = image("next");
    faults.fail_write.store(true, Ordering::SeqCst);
    assert!(matches!(
        service.activate_deploy(&app, &next, &next_image).await,
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
    assert_eq!(
        tasks.snapshot(&retained_task).await.unwrap(),
        test_snapshot()
    );

    let (entered, release) = gate(&faults.slow_write);
    let (activated, concurrent) =
        futures::join!(service.activate_deploy(&app, &next, &next_image), async {
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
        .expect("snapshot upload held the customer app lock")
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
        tasks.snapshot(&new_task).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_eq!(
        tasks.snapshot(&new_task).await.unwrap(),
        next_image,
        "transient storage outage parked intact code"
    );

    let (entered, release) = gate(&faults.missing_read);
    let (stale, repaired) = futures::join!(tasks.snapshot(&new_task), async {
        entered.await.unwrap();
        let result = compio::time::timeout(
            Duration::from_secs(5),
            service.retain_deploy(&app, &next, &next_image),
        )
        .await;
        release.send(()).unwrap();
        result
    });
    repaired
        .expect("snapshot read held the customer app lock")
        .unwrap();
    assert!(matches!(stale, Err(WorkflowServiceError::Unavailable(_))));
    assert_eq!(
        tasks.snapshot(&new_task).await.unwrap(),
        next_image,
        "stale missing-object observation revoked the repair"
    );

    // A successful read cannot outlive the captured task claim.
    let (entered, release) = gate(&faults.slow_read);
    let (stale, released) = futures::join!(tasks.snapshot(&new_task), async {
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
        .expect("snapshot read blocked task release")
        .unwrap();
    assert!(matches!(stale, Err(WorkflowServiceError::Conflict(_))));
    assert!(tasks.snapshot(&new_task).await.is_err());
    assert_eq!(tasks.snapshot(&task).await.unwrap(), test_snapshot());
    assert_eq!(task.invocation.run_id, old.id);
}
