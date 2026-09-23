use super::*;
use zeroship_data_orm::orm::Insertable;

pub(super) use crate::service::tests::objects::Objects;

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
    pub objects: Objects,
    pub task: TaskAssignment,
    pub worker: WorkerIdentity,
    _deployments: Deployments,
}
impl Fixture {
    pub async fn new(store: Rc<OrmStore>) -> Self {
        let (service, app, other, deployments) = registered_service(store.clone()).await;
        let objects = Objects::new();
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
            objects,
            task,
            worker,
            _deployments: deployments,
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
                self.objects.upload(b"collect-me"),
            )
            .await
            .unwrap()
            .id
    }
    pub async fn expire(&self, id: &str) {
        self.set_expiry(id, 0).await;
    }
    /// Strip every staging-location column, the shape `stage_app_payload`
    /// writes for bytes that belong to an app and to no run. Both composite
    /// foreign keys then carry a NULL, which both dialects satisfy without a
    /// lookup, so expiry is the whole eligibility test that remains.
    pub async fn disown(&self, id: &str) {
        use crate::service::models::payloads;
        let tx = self.store.begin().await.unwrap();
        assert_eq!(
            tx.database()
                .entity::<payloads::Entity>()
                .unwrap()
                .update_many(
                    payloads::id.eq(id).unwrap(),
                    payloads::task_id
                        .set(None::<&str>)
                        .unwrap()
                        .and(payloads::run_id.set(None::<&str>).unwrap())
                        .unwrap()
                        .and(payloads::generation.set(None::<i64>).unwrap())
                        .unwrap()
                )
                .await
                .unwrap(),
            1
        );
        tx.commit().await.unwrap();
    }
    /// The journal's own clock, which is what collection compares against.
    pub async fn now(&self) -> i64 {
        let mut tx = self.store.begin().await.unwrap();
        let now = tx.now().await.unwrap();
        tx.commit().await.unwrap();
        now
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
    pub fn exists(&self, app: &AppId, id: &str) -> bool {
        self.objects.exists(app, id)
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
        self.scan_for(self.scope.app_id()).await
    }
    pub async fn scan_for(&self, app: &AppId) -> Scan {
        use crate::service::models::app_state;
        let tx = self.store.begin().await.unwrap();
        let mut rows = tx
            .database()
            .entity::<app_state::Entity>()
            .unwrap()
            .find::<Scan>(
                app_state::app_id.eq(app.as_str()).unwrap(),
                FindOptions::default(),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(rows.len(), 1);
        rows.remove(0)
    }
    /// A second host over the same journal and policy registry.
    pub async fn reopen(&self) -> AppWorkflows {
        let service = WorkflowService::open(self.store.clone(), self.service.policies.clone())
            .await
            .unwrap();
        service.fixture_app(self.scope.app_id().clone())
    }
}

#[derive(Debug, PartialEq, Eq, FromRow)]
#[orm(entity = crate::service::models::payloads)]
pub(super) struct Payload {
    pub id: String,
    pub state: String,
    pub expires_at: i64,
    pub run_id: Option<String>,
    pub generation: Option<i64>,
    pub task_id: Option<String>,
}
#[derive(FromRow, Insertable)]
#[orm(entity = crate::service::models::payloads)]
struct PayloadRow {
    id: String,
    app_id: String,
    run_id: Option<String>,
    generation: Option<i64>,
    task_id: Option<String>,
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
#[orm(entity = crate::service::models::app_state)]
pub(super) struct Scan {
    pub collection_revision: i64,
    pub collection_after_id: Option<String>,
    pub collection_upper_id: Option<String>,
    pub collection_observed_at: Option<i64>,
}
