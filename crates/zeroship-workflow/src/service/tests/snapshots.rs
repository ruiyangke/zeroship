use super::*;
use crate::operations::{RunOperation, RunState};
use crate::service::{ExecutableSnapshot, SnapshotStore};
use zeroship_storage::{
    backend::{BoxByteStream, BoxChunkSource, ListPage, ListRequest, ObjectMeta},
    Backend, LocalFs, StorageError, StorageStore,
};

pub(super) fn test_snapshot() -> ExecutableSnapshot {
    ExecutableSnapshot::new(
        "index.js".into(),
        [("index.js".into(), "export default {};".into())].into(),
        None,
    )
    .unwrap()
}

pub(super) fn fixture_snapshot_store() -> SnapshotStore {
    let directory = tempfile::tempdir().unwrap();
    let local = LocalFs::new(directory.path());
    SnapshotStore::new(
        &StorageStore::from_backend(Arc::new(OwnedFiles {
            local,
            _directory: directory,
        })),
        1024 * 1024,
    )
    .unwrap()
}

#[derive(Debug)]
struct OwnedFiles {
    local: LocalFs,
    _directory: tempfile::TempDir,
}
#[async_trait::async_trait(?Send)]
impl Backend for OwnedFiles {
    async fn put_stream(
        &self,
        app: &str,
        bucket: &str,
        key: &str,
        body: BoxChunkSource,
        content_type: Option<&str>,
    ) -> Result<u64, StorageError> {
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
        self.local.get_stream(app, bucket, key).await
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

mod failures;

fn image(value: &str) -> ExecutableSnapshot {
    ExecutableSnapshot::new(
        "entry.js".into(),
        [
            (
                "entry.js".into(),
                "import value from './dependency.js'; export default value;".into(),
            ),
            (
                "dependency.js".into(),
                format!("export default {};", serde_json::to_string(value).unwrap()),
            ),
        ]
        .into(),
        Some(json!({"collections": {}})),
    )
    .unwrap()
}
fn deployment(hash: char) -> DeployRegistration {
    DeployRegistration {
        id: typed_id::generate("dep"),
        hash: hash.to_string().repeat(64),
        workflows: ["Example".into()].into(),
        schedules: vec![],
    }
}

#[compio::test]
async fn sqlite_snapshots_survive_redeploy_restart_corruption_and_repair() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    snapshot_contract(
        Arc::new(SqliteStore::new(path)),
        StorageStore::from_backend(Arc::new(LocalFs::new(dir.path().join("objects")))),
    )
    .await;
}

#[compio::test]
async fn postgres_snapshots_survive_redeploy_restart_corruption_and_repair() {
    let fixture = PostgresFixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    snapshot_contract(
        Arc::new(fixture.store.clone()),
        StorageStore::from_backend(Arc::new(LocalFs::new(dir.path()))),
    )
    .await;
}

#[compio::test]
async fn customer_snapshots_use_s3_without_platform_bundle_access() {
    let fixture = s3_fixture::Minio::start();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    snapshot_contract(
        Arc::new(SqliteStore::new(path)),
        StorageStore::from_backend(Arc::new(zeroship_storage::S3::new(
            fixture.config("snapshots"),
            fixture.credentials(),
        ))),
    )
    .await;
}

#[expect(
    clippy::too_many_lines,
    reason = "exercise the ordered retention, corruption and recovery contract"
)]
#[expect(
    clippy::future_not_send,
    reason = "the database and storage contract runs on its compio thread"
)]
async fn snapshot_contract(store: Arc<dyn WorkflowStore>, storage: StorageStore) {
    use crate::service::{runner::TaskTransport, WorkerIdentity};
    let snapshots = SnapshotStore::new(&storage, 1024 * 1024).unwrap();
    let (service, app, other) = registered_with_snapshots(store.clone(), snapshots.clone()).await;
    let first = deployment('b');
    let second = deployment('c');
    let original = image("original");
    let replacement = image("replacement");
    service
        .activate_deploy(&app, &first, &original)
        .await
        .unwrap();
    let scope = service.for_app(app.clone());
    let old_run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    service
        .activate_deploy(&app, &second, &replacement)
        .await
        .unwrap();
    let new_run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    // Even an identical deployment identity selects objects in its own app.
    service
        .activate_deploy(&other, &first, &replacement)
        .await
        .unwrap();
    let other_run = service
        .for_app(other.clone())
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let reopened = WorkflowService::open(store.clone(), service.policies.clone())
        .await
        .unwrap()
        .with_snapshots(snapshots);
    let tasks = reopened.tasks(WorkerIdentity::new("customer".into()).unwrap());
    let foreign = reopened.tasks(WorkerIdentity::new("foreign".into()).unwrap());
    let mut active = Vec::new();
    while let Some(task) = tasks.poll().await.unwrap() {
        active.push(task);
    }
    assert_eq!(active.len(), 3);
    for task in &active {
        let expected = if task.invocation.run_id == old_run.id {
            &original
        } else {
            &replacement
        };
        assert_eq!(&tasks.snapshot(task).await.unwrap(), expected);
        assert!(foreign.snapshot(task).await.is_err());
        assert!(
            matches!(task.invocation.run_id.as_str(), id if id == old_run.id || id == new_run.id || id == other_run.id)
        );
    }
    assert!(matches!(
        service.activate_deploy(&app, &first, &replacement).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    let old_task = active
        .iter()
        .find(|task| task.invocation.run_id == old_run.id)
        .unwrap();
    assert_eq!(tasks.snapshot(old_task).await.unwrap(), original);
    tasks
        .complete(
            &old_task.id,
            &old_task.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    assert!(tasks.snapshot(old_task).await.is_err());
    let new_task = active
        .iter()
        .find(|task| task.invocation.run_id == new_run.id)
        .unwrap();
    let objects =
        storage.namespace(zeroship_storage::Namespace::platform("workflow-snapshots").unwrap());
    let (bytes, _) = objects
        .get(app.as_str(), &second.id)
        .await
        .unwrap()
        .unwrap();
    let mut corrupt = bytes.clone();
    let last = corrupt.last_mut().unwrap();
    *last ^= 1;
    objects
        .put(app.as_str(), &second.id, &corrupt, None)
        .await
        .unwrap();
    assert!(matches!(
        tasks.snapshot(new_task).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    tasks.release(&new_task.id, &new_task.token).await.unwrap();
    assert!(
        tasks.poll().await.unwrap().is_none(),
        "corrupt deployment was dispatched again"
    );
    assert!(scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .is_err());
    service
        .activate_deploy(&app, &first, &original)
        .await
        .unwrap();
    let unaffected = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = tasks.poll().await.unwrap().unwrap();
    assert_eq!(
        task.invocation.run_id, unaffected.id,
        "parked work starved another deployment"
    );
    tasks
        .complete(
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    service
        .retain_deploy(&app, &second, &replacement)
        .await
        .unwrap();
    let recovered = tasks.poll().await.unwrap().unwrap();
    assert_eq!(recovered.invocation.run_id, new_run.id);
    assert_eq!(tasks.snapshot(&recovered).await.unwrap(), replacement);
    let post_repair = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let post_repair_task = tasks.poll().await.unwrap().unwrap();
    assert_eq!(post_repair_task.invocation.run_id, post_repair.id);
    assert_eq!(
        post_repair_task.invocation.deploy_id, first.id,
        "repair changed the active deployment"
    );
    tasks
        .complete(
            &post_repair_task.id,
            &post_repair_task.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    objects.delete(app.as_str(), &second.id).await.unwrap();
    assert!(matches!(
        tasks.snapshot(&recovered).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    let mut tx = reopened.begin().await.unwrap();
    let expired = tx.now().await.unwrap() - 1;
    tx.execute(
        &format!("UPDATE {} SET deadline=$2 WHERE id=$1", tx.table("tasks")),
        &[recovered.id.clone().into(), expired.into()],
    )
    .await
    .unwrap();
    tx.execute(
        &format!(
            "UPDATE {} SET due_at=$3 WHERE app_id=$1 AND id=$2",
            tx.table("runs")
        ),
        &[
            app.as_str().into(),
            new_run.id.clone().into(),
            expired.into(),
        ],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(tasks.poll().await.unwrap().is_none());
    let mut tx = reopened.begin().await.unwrap();
    let rows = tx
        .query(
            &format!("SELECT state FROM {} WHERE id=$1", tx.table("tasks")),
            &[recovered.id.clone().into()],
        )
        .await
        .unwrap();
    assert_eq!(rows[0].text("state").unwrap(), "expired");
    tx.commit().await.unwrap();
    // Corruption or deletion in A's scope must leave B's bytes intact.
    let other_task = active
        .iter()
        .find(|task| task.invocation.run_id == other_run.id)
        .unwrap();
    assert_eq!(tasks.snapshot(other_task).await.unwrap(), replacement);
    scope
        .transition(&RequestId::mint(), &new_run.id, RunOperation::Cancel)
        .await
        .unwrap();
    assert!(tasks.poll().await.unwrap().is_none());
    assert_eq!(
        scope.status(&new_run.id).await.unwrap().state,
        RunState::Cancelled
    );
}

#[test]
fn snapshot_images_reject_live_urls_missing_entries_and_oversized_content() {
    for entry in [
        "",
        "missing.js",
        "../entry.js",
        "/entry.js",
        "http://localhost/entry.js",
    ] {
        let modules = if entry == "missing.js" {
            [("index.js".into(), String::new())].into()
        } else {
            [(entry.into(), String::new())].into()
        };
        assert!(ExecutableSnapshot::new(entry.into(), modules, None).is_err());
    }
    let store = SnapshotStore::new(
        &StorageStore::from_backend(Arc::new(LocalFs::new("unused"))),
        16,
    )
    .unwrap();
    assert!(matches!(
        store.encode(&test_snapshot()),
        Err(WorkflowServiceError::PayloadTooLarge)
    ));
}
