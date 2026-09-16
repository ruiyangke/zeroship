use super::*;
use std::sync::Mutex;
use zeroship_data_orm::orm::Insertable;
use zeroship_storage::backend::{BoxByteStream, ListPage, ListRequest, ObjectMeta};
use zeroship_storage::{Backend, StorageError};

#[derive(Clone)]
pub(in crate::service::tests) struct Grant {
    pub delivery: Delivery,
    pub expires: Instant,
}
impl Grant {
    pub fn new(app: &AppId) -> Self {
        Self {
            delivery: Delivery {
                job: JobSpec {
                    id: JobId::mint(),
                    app_id: app.clone(),
                    operation: JobOperation::Collect {},
                    available_at: 0.try_into().unwrap(),
                },
                worker_id: WorkerId::mint(),
                assignment_revision: 1.try_into().unwrap(),
                attempt: 1.try_into().unwrap(),
                deadline: 0.try_into().unwrap(),
            },
            expires: Instant::now() + Duration::from_secs(30),
        }
    }
    pub fn retry(&self) -> Self {
        let mut retry = self.clone();
        retry.delivery.attempt = (retry.delivery.attempt.get() + 1).try_into().unwrap();
        retry.expires = Instant::now() + Duration::from_secs(30);
        retry
    }
}
impl JobLease for Grant {
    fn delivery(&self) -> &Delivery {
        &self.delivery
    }
    fn remaining(&self) -> Option<Duration> {
        self.expires
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
    }
}

pub(in crate::service::tests) const fn options(page_size: u32) -> CollectionOptions {
    CollectionOptions {
        page_size,
        item_timeout: Duration::from_secs(2),
    }
}

pub(super) struct Fixture {
    pub service: WorkflowService,
    pub scope: AppWorkflows,
    pub other: AppId,
    pub store: Rc<OrmStore>,
    pub backend: Arc<Objects>,
    pub storage: StorageStore,
    pub task: TaskAssignment,
    pub worker: WorkerIdentity,
    _deployments: Deployments,
    _directory: tempfile::TempDir,
}
impl Fixture {
    pub async fn new(store: Rc<OrmStore>) -> Self {
        let (service, app, other, deployments) = registered_service(store.clone()).await;
        let directory = tempfile::tempdir().unwrap();
        let backend = Arc::new(Objects {
            inner: LocalFs::new(directory.path()),
            calls: Mutex::new(Vec::new()),
            faults: Mutex::new(Vec::new()),
        });
        let storage = StorageStore::from_backend(backend.clone());
        let service = service.with_payload_storage(storage.clone()).unwrap();
        let scope = service.fixture_app(app);
        scope
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await
            .unwrap();
        let worker = WorkerIdentity::new("collection-worker".into()).unwrap();
        let task = service.poll(&worker).await.unwrap().unwrap();
        Self {
            service,
            scope,
            other,
            store,
            backend,
            storage,
            task,
            worker,
            _deployments: deployments,
            _directory: directory,
        }
    }
    pub async fn stage(&self) -> String {
        self.service
            .stage_payload(
                &self.worker,
                &self.task.id,
                &self.task.token,
                &RequestId::mint(),
                reference(b"collect-me"),
                body(b"collect-me"),
            )
            .await
            .unwrap()
            .id
    }
    pub async fn expire(&self, id: &str) {
        self.set_expiry(id, 0).await;
    }
    pub async fn damage_identity(&self, id: &str) {
        use crate::service::models::payloads;
        let tx = self.store.begin().await.unwrap();
        let mut records = tx
            .database()
            .entity::<payloads::Entity>()
            .unwrap()
            .find::<PayloadRow>(payloads::id.eq(id).unwrap(), FindOptions::default())
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        let mut record = records.remove(0);
        assert_eq!(
            tx.database()
                .entity::<payloads::Entity>()
                .unwrap()
                .delete_many(payloads::id.eq(id).unwrap())
                .await
                .unwrap(),
            1
        );
        record.id.clear();
        let inserted = tx
            .database()
            .entity::<payloads::Entity>()
            .unwrap()
            .insert::<_, PayloadRow>(record)
            .await
            .unwrap();
        assert!(inserted.id.is_empty());
        tx.commit().await.unwrap();
    }
    pub async fn set_expiry(&self, id: &str, expiry: i64) {
        use crate::service::models::payloads;
        let tx = self.store.begin().await.unwrap();
        assert_eq!(
            tx.database()
                .entity::<payloads::Entity>()
                .unwrap()
                .update_many(
                    payloads::id.eq(id).unwrap(),
                    payloads::expires_at.set(expiry).unwrap()
                )
                .await
                .unwrap(),
            1
        );
        tx.commit().await.unwrap();
    }
    pub async fn payload(&self, id: &str) -> Payload {
        use crate::service::models::payloads;
        let tx = self.store.begin().await.unwrap();
        let mut rows = tx
            .database()
            .entity::<payloads::Entity>()
            .unwrap()
            .find::<Payload>(payloads::id.eq(id).unwrap(), FindOptions::default())
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(rows.len(), 1);
        rows.remove(0)
    }
    pub async fn exists(&self, app: &AppId, id: &str) -> bool {
        self.storage
            .namespace(zeroship_storage::Namespace::platform("workflow").unwrap())
            .get(app.as_str(), id)
            .await
            .unwrap()
            .is_some()
    }
    pub async fn page(&self, job: &JobSpec) -> Page {
        use crate::service::models::collection_pages;
        let tx = self.store.begin().await.unwrap();
        let mut rows = tx
            .database()
            .entity::<collection_pages::Entity>()
            .unwrap()
            .find::<Page>(
                collection_pages::id.eq(job.id.as_str()).unwrap(),
                FindOptions::default(),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(rows.len(), 1);
        rows.remove(0)
    }
    pub async fn scan(&self) -> Scan {
        use crate::service::models::collection_scans;
        let tx = self.store.begin().await.unwrap();
        let mut rows = tx
            .database()
            .entity::<collection_scans::Entity>()
            .unwrap()
            .find::<Scan>(
                collection_scans::id
                    .eq(self.scope.app_id().as_str())
                    .unwrap(),
                FindOptions::default(),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(rows.len(), 1);
        rows.remove(0)
    }
    pub async fn reopen(&self, storage: bool) -> AppWorkflows {
        let mut service = WorkflowService::open(self.store.clone(), self.service.policies.clone())
            .await
            .unwrap();
        if storage {
            service = service.with_payload_storage(self.storage.clone()).unwrap();
        }
        service.fixture_app(self.scope.app_id().clone())
    }
}

#[derive(Debug, PartialEq, Eq, FromRow)]
#[orm(entity = crate::service::models::payloads)]
pub(super) struct Payload {
    pub id: String,
    pub state: String,
    pub expires_at: i64,
}
#[derive(FromRow, Insertable)]
#[orm(entity = crate::service::models::payloads)]
struct PayloadRow {
    id: String,
    app_id: String,
    run_id: String,
    generation: i64,
    task_id: String,
    request_id: String,
    hash: String,
    size: i64,
    content_type: Option<String>,
    state: String,
    created_at: i64,
    expires_at: i64,
}
#[derive(Debug, PartialEq, Eq, FromRow)]
#[orm(entity = crate::service::models::collection_pages)]
pub(super) struct Page {
    pub plan: String,
    pub next_index: i64,
}
#[derive(Debug, PartialEq, Eq, FromRow)]
#[orm(entity = crate::service::models::collection_scans)]
pub(super) struct Scan {
    pub revision: i64,
    pub after_id: Option<String>,
    pub upper_id: Option<String>,
    pub observed_at: Option<i64>,
}

#[derive(Debug)]
pub(super) enum Fault {
    Fail(String),
    LostDeleteReply(String),
    Hang(String),
    Gate(String, flume::Sender<()>, flume::Receiver<()>),
}
#[derive(Debug)]
pub(super) struct Objects {
    inner: LocalFs,
    pub calls: Mutex<Vec<String>>,
    pub faults: Mutex<Vec<Fault>>,
}
impl Objects {
    pub fn gate(&self, id: &str) -> (flume::Receiver<()>, flume::Sender<()>) {
        let (entered, waiting) = flume::bounded(1);
        let (resume, gate) = flume::bounded(1);
        self.faults
            .lock()
            .unwrap()
            .push(Fault::Gate(id.into(), entered, gate));
        (waiting, resume)
    }
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}
#[async_trait::async_trait(?Send)]
impl Backend for Objects {
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
        self.inner.get_stream(app, bucket, key).await
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
        self.calls.lock().unwrap().push(key.to_owned());
        let fault = {
            let mut faults = self.faults.lock().unwrap();
            faults
                .iter()
                .position(|fault| match fault {
                    Fault::Fail(id)
                    | Fault::LostDeleteReply(id)
                    | Fault::Hang(id)
                    | Fault::Gate(id, _, _) => id == key,
                })
                .map(|index| faults.remove(index))
        };
        match fault {
            Some(Fault::Fail(_)) => {
                return Err(StorageError::InvalidArgument(
                    "injected delete failure".into(),
                ))
            }
            Some(Fault::LostDeleteReply(_)) => {
                self.inner.delete(app, bucket, key).await?;
                return Err(StorageError::InvalidArgument(
                    "injected lost delete reply".into(),
                ));
            }
            Some(Fault::Hang(_)) => std::future::pending::<()>().await,
            Some(Fault::Gate(_, entered, resume)) => {
                entered.send_async(()).await.unwrap();
                resume.recv_async().await.unwrap();
            }
            None => {}
        }
        self.inner.delete(app, bucket, key).await
    }
}
