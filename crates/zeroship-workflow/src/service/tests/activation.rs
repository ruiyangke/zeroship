#![expect(
    clippy::future_not_send,
    reason = "creator activation tests use compio"
)]

use super::*;
use crate::{
    deployment_holds::{DeploymentHoldClient, HoldGeneration, HoldReceipt, HoldScope},
    service::{delivery::ATTEMPT_IO_CEILING, AppWorkflows},
};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};
use zeroship_bundle::{BlobError, BlobStore, LocalDiskBlobStore, PutOutcome};
use zeroship_core::{
    workflow_coordination::WorkerId,
    workflow_jobs::{Delivery, DeploymentId, JobId, JobLease, JobOperation, JobOutcome, JobSpec},
};
use zeroship_data_orm::{value, Value};

#[derive(Clone)]
struct Grant {
    delivery: Delivery,
    expires: Instant,
}
impl Grant {
    fn new(app: &AppId, deployment: &DeployRegistration, revision: i64) -> Self {
        Self {
            delivery: Delivery {
                job: JobSpec {
                    id: JobId::mint(),
                    app_id: app.clone(),
                    operation: JobOperation::Activate {
                        deployment_id: DeploymentId::parse(&deployment.id).unwrap(),
                        revision: revision.try_into().unwrap(),
                    },
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
    fn retry(&self) -> Self {
        let mut retry = self.clone();
        retry.delivery.attempt = (self.delivery.attempt.get() + 1).try_into().unwrap();
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
            .filter(|time| !time.is_zero())
    }
}

macro_rules! case {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[compio::test]
        async fn $sqlite() {
            let directory = tempfile::tempdir().unwrap();
            Box::pin($contract(Rc::new(
                sqlite_store(&directory.path().join("creator.sqlite")).await,
            )))
            .await;
        }
        #[compio::test]
        async fn $postgres() {
            let fixture = PostgresFixture::start().await;
            Box::pin($contract(Rc::new(fixture.store.clone()))).await;
        }
    };
}

case!(
    sqlite_activation_receipts_and_pins_survive_late_delivery,
    postgres_activation_receipts_and_pins_survive_late_delivery,
    replay_and_order
);
case!(
    sqlite_activation_rejects_reused_revision_or_job,
    postgres_activation_rejects_reused_revision_or_job,
    conflicts
);
case!(
    sqlite_activation_receipt_requires_matching_readiness_history,
    postgres_activation_receipt_requires_matching_readiness_history,
    receipt_history
);
case!(
    sqlite_activation_requires_the_exact_available_bundle,
    postgres_activation_requires_the_exact_available_bundle,
    artifacts
);
case!(
    sqlite_activation_rejects_foreign_and_expired_authority,
    postgres_activation_rejects_foreign_and_expired_authority,
    authority
);
case!(
    sqlite_activation_rechecks_policy_after_app_lock,
    postgres_activation_rechecks_policy_after_app_lock,
    policy_lock
);
case!(
    sqlite_activation_rechecks_policy_after_hold_response,
    postgres_activation_rechecks_policy_after_hold_response,
    policy_hold
);
case!(
    sqlite_activation_rechecks_policy_after_artifact_read,
    postgres_activation_rechecks_policy_after_artifact_read,
    policy_artifact
);
case!(
    sqlite_activation_recovers_lost_or_mismatched_unknown_hash_receipts,
    postgres_activation_recovers_lost_or_mismatched_unknown_hash_receipts,
    hold_replies
);
case!(
    sqlite_activation_ends_a_stalled_attempt_at_the_io_ceiling,
    postgres_activation_ends_a_stalled_attempt_at_the_io_ceiling,
    io_ceiling
);

async fn empty_service(
    store: Rc<OrmStore>,
    platform: Deployments,
) -> (WorkflowService, AppId, AppId, Deployments) {
    let app = AppId::mint();
    let other = AppId::mint();
    let service = WorkflowService::open(store, Arc::new(HostPolicies::default()))
        .await
        .unwrap()
        .with_deployments(platform.binding(&[&app, &other]));
    for identity in [&app, &other] {
        service
            .fixture_register(identity, leased_policy(1, AppPolicy::default()))
            .await
            .unwrap();
    }
    (service, app, other, platform)
}

async fn snapshot(service: &WorkflowService, table: &str, app: &AppId) -> Vec<Value> {
    let tx = service.begin().await.unwrap();
    let filter = if table == "activation_scopes" {
        json!({"id":app.as_str()})
    } else {
        json!({"app_id":app.as_str()})
    };
    let rows = journal_rows(&tx, table, filter)
        .await
        .into_iter()
        .map(|row| row.0)
        .collect();
    tx.commit().await.unwrap();
    rows
}

async fn assert_unaccepted(service: &WorkflowService, scope: &AppWorkflows, grant: &Grant) {
    assert!(scope
        .job_receipt(&grant.delivery.job)
        .await
        .unwrap()
        .is_none());
    for table in ["activations", "activation_scopes", "runs", "tasks"] {
        assert!(
            snapshot(service, table, scope.app_id()).await.is_empty(),
            "{table}"
        );
    }
}

async fn selected(service: &WorkflowService, app: &AppId) -> String {
    let active: Vec<_> = snapshot(service, "deploys", app)
        .await
        .into_iter()
        .filter(|row| row["active"] == value!(1))
        .collect();
    assert_eq!(active.len(), 1);
    active[0]["id"].as_str().unwrap().to_owned()
}

async fn reopened(service: &WorkflowService) -> WorkflowService {
    WorkflowService::open(service.store.clone(), service.policies.clone())
        .await
        .unwrap()
        .with_deployments(service.deployments.clone().unwrap())
}

async fn replay_and_order(store: Rc<OrmStore>) {
    let (service, app, _, platform) = empty_service(store, Deployments::new().await).await;
    let old = platform.deploy(&app).await;
    let new = platform.deploy(&app).await;
    let scope = service.fixture_app(app.clone());
    let newest = Grant::new(&app, &new, 5);
    let newest_receipt = scope.activate_job(&newest).await.unwrap();
    assert_eq!(newest_receipt.job, newest.delivery.job);
    assert_eq!(newest_receipt.outcome, JobOutcome::Completed {});
    let previous = Grant::new(&app, &old, 3);
    let old_receipt = scope.activate_job(&previous).await.unwrap();
    assert_eq!(old_receipt.outcome, JobOutcome::Completed {});
    assert_eq!(selected(&service, &app).await, new.id);
    assert_eq!(snapshot(&service, "activations", &app).await.len(), 2);
    assert_eq!(
        snapshot(&service, "activation_scopes", &app).await[0]["revision"],
        value!(5)
    );
    platform.assert_held(&app, &old.id).await;
    platform.assert_held(&app, &new.id).await;
    for table in ["runs", "tasks", "schedules", "job_publications"] {
        assert!(snapshot(&service, table, &app).await.is_empty());
    }
    let current = snapshot(&service, "activation_scopes", &app).await;
    platform
        .source
        .delete_manifest(&app, &old.hash)
        .await
        .unwrap();
    platform
        .source
        .delete_manifest(&app, &new.hash)
        .await
        .unwrap();
    let service = reopened(&service).await;
    let scope = service.fixture_app(app.clone());
    let mut retry = previous.retry();
    retry.expires = Instant::now();
    assert_eq!(scope.activate_job(&retry).await.unwrap(), old_receipt);
    assert_eq!(
        scope.activate_job(&newest.retry()).await.unwrap(),
        newest_receipt
    );
    assert_eq!(snapshot(&service, "activation_scopes", &app).await, current);
    assert_eq!(selected(&service, &app).await, new.id);
    let settlement = old_receipt.settlement(&retry).unwrap();
    assert_eq!(settlement.delivery.attempt, retry.delivery.attempt);
    assert_eq!(settlement.outcome, JobOutcome::Completed {});
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let tx = service.begin().await.unwrap();
    let run = journal_rows(&tx, "runs", json!({"id":run.id})).await;
    assert_eq!(run[0].text("deploy_id").unwrap(), new.id);
    tx.commit().await.unwrap();
}

async fn conflicts(store: Rc<OrmStore>) {
    let (service, app, _, platform) = empty_service(store, Deployments::new().await).await;
    let first = platform.deploy(&app).await;
    let other = platform.deploy(&app).await;
    let scope = service.fixture_app(app.clone());
    let grant = Grant::new(&app, &first, 1);
    let receipt = scope.activate_job(&grant).await.unwrap();
    let history = snapshot(&service, "activations", &app).await;
    let selection = snapshot(&service, "activation_scopes", &app).await;
    let mut changed_job = grant.retry();
    changed_job.delivery.job.operation = JobOperation::Activate {
        deployment_id: DeploymentId::parse(&other.id).unwrap(),
        revision: 1.try_into().unwrap(),
    };
    let mut changed_revision = grant.retry();
    changed_revision.delivery.job.operation = JobOperation::Activate {
        deployment_id: DeploymentId::parse(&first.id).unwrap(),
        revision: 2.try_into().unwrap(),
    };
    for conflicting in [
        changed_job,
        changed_revision,
        Grant::new(&app, &first, 1),
        Grant::new(&app, &other, 1),
    ] {
        assert!(matches!(
            scope.activate_job(&conflicting).await,
            Err(WorkflowServiceError::Conflict(_))
        ));
        assert_eq!(snapshot(&service, "activations", &app).await, history);
        assert_eq!(
            snapshot(&service, "activation_scopes", &app).await,
            selection
        );
    }
    assert_eq!(scope.activate_job(&grant.retry()).await.unwrap(), receipt);
    assert!(matches!(
        service.activate_deploy(&app, &other).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    assert_eq!(selected(&service, &app).await, first.id);
}

async fn artifacts(store: Rc<OrmStore>) {
    let (service, app, _, platform) = empty_service(store, Deployments::new().await).await;
    let deployment = platform.deploy(&app).await;
    let scope = service.fixture_app(app.clone());
    let grant = Grant::new(&app, &deployment, 1);
    let manifest = platform
        .source
        .get_manifest(&app, &deployment.hash)
        .await
        .unwrap();
    platform
        .source
        .delete_manifest(&app, &deployment.hash)
        .await
        .unwrap();
    assert!(matches!(
        scope.activate_job(&grant).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_unaccepted(&service, &scope, &grant).await;
    platform
        .source
        .put_manifest(&app, &deployment.hash, b"{\"corrupt\":true}")
        .await
        .unwrap();
    assert!(matches!(
        scope.activate_job(&grant.retry()).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_unaccepted(&service, &scope, &grant).await;
    platform
        .source
        .delete_manifest(&app, &deployment.hash)
        .await
        .unwrap();
    platform
        .source
        .put_manifest(&app, &deployment.hash, &manifest)
        .await
        .unwrap();
    assert_eq!(
        scope.activate_job(&grant.retry()).await.unwrap().outcome,
        JobOutcome::Completed {}
    );
    assert_eq!(selected(&service, &app).await, deployment.id);
}

async fn receipt_history(store: Rc<OrmStore>) {
    let (service, app, _, platform) = empty_service(store, Deployments::new().await).await;
    let older = platform.deploy(&app).await;
    let newer = platform.deploy(&app).await;
    let scope = service.fixture_app(app.clone());
    let previous = Grant::new(&app, &older, 1);
    let current = Grant::new(&app, &newer, 2);
    let previous_receipt = scope.activate_job(&previous).await.unwrap();
    let current_receipt = scope.activate_job(&current).await.unwrap();
    let selection = snapshot(&service, "activation_scopes", &app).await;
    let receipts = snapshot(&service, "job_receipts", &app).await;

    let tx = service.begin().await.unwrap();
    journal_update(
        &tx,
        "activations",
        json!({"app_id":app.as_str(), "id":previous.delivery.job.id.as_str()}),
        json!({"deploy_id":newer.id}),
    )
    .await;
    tx.commit().await.unwrap();
    let substituted = snapshot(&service, "activations", &app).await;
    assert!(matches!(
        scope.activate_job(&previous.retry()).await,
        Err(WorkflowServiceError::Internal(_))
    ));
    assert_eq!(snapshot(&service, "activations", &app).await, substituted);
    assert_eq!(
        snapshot(&service, "activation_scopes", &app).await,
        selection
    );
    assert_eq!(snapshot(&service, "job_receipts", &app).await, receipts);

    let tx = service.begin().await.unwrap();
    journal_update(
        &tx,
        "activations",
        json!({"app_id":app.as_str(), "id":previous.delivery.job.id.as_str()}),
        json!({"deploy_id":older.id}),
    )
    .await;
    tx.commit().await.unwrap();
    assert_eq!(
        scope.activate_job(&previous.retry()).await.unwrap(),
        previous_receipt
    );

    let tx = service.begin().await.unwrap();
    tx.database()
        .collection("__zeroship_workflow_activations")
        .unwrap()
        .delete(value!({"app_id":app.as_str(), "id":previous.delivery.job.id.as_str()}))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let remaining = snapshot(&service, "activations", &app).await;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0]["id"], value!(current.delivery.job.id.as_str()));
    let mut retry = previous.retry();
    retry.expires = Instant::now();
    assert!(matches!(
        scope.activate_job(&retry).await,
        Err(WorkflowServiceError::Internal(_))
    ));
    assert_eq!(snapshot(&service, "activations", &app).await, remaining);
    assert_eq!(
        snapshot(&service, "activation_scopes", &app).await,
        selection
    );
    assert_eq!(snapshot(&service, "job_receipts", &app).await, receipts);
    assert_eq!(selected(&service, &app).await, newer.id);
    assert_eq!(
        scope.activate_job(&current.retry()).await.unwrap(),
        current_receipt
    );
}

async fn authority(store: Rc<OrmStore>) {
    let (service, app, other, platform) = empty_service(store, Deployments::new().await).await;
    let deployment = platform.deploy(&app).await;
    let scope = service.fixture_app(app.clone());
    let grant = Grant::new(&app, &deployment, 1);
    assert!(matches!(
        service
            .fixture_app(other.clone())
            .activate_job(&grant)
            .await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert!(snapshot(&service, "deployment_holds", &other)
        .await
        .is_empty());
    let mut expired = grant.clone();
    expired.expires = Instant::now();
    assert!(matches!(
        scope.activate_job(&expired).await,
        Err(WorkflowServiceError::Timeout)
    ));
    assert_unaccepted(&service, &scope, &grant).await;
    let mut held = service.begin().await.unwrap();
    super::super::app::lock_app(&mut held, &app).await.unwrap();
    let mut short = grant.clone();
    short.expires = Instant::now() + Duration::from_millis(100);
    assert!(matches!(
        compio::time::timeout(Duration::from_secs(2), scope.activate_job(&short))
            .await
            .unwrap(),
        Err(WorkflowServiceError::Timeout)
    ));
    held.commit().await.unwrap();
    assert_unaccepted(&service, &scope, &grant).await;
    assert_eq!(
        scope.activate_job(&grant.retry()).await.unwrap().outcome,
        JobOutcome::Completed {}
    );
}

async fn policy_lock(store: Rc<OrmStore>) {
    let (service, app, _, platform) = empty_service(store, Deployments::new().await).await;
    let deployment = platform.deploy(&app).await;
    let scope = service.fixture_app(app.clone());
    let grant = Grant::new(&app, &deployment, 1);
    let mut held = service.begin().await.unwrap();
    super::super::app::lock_app(&mut held, &app).await.unwrap();
    let mut work = Box::pin(scope.activate_job(&grant));
    assert!(futures::poll!(work.as_mut()).is_pending());
    service
        .policies
        .fixture_install(&app, leased_policy(2, AppPolicy::default()))
        .unwrap();
    held.commit().await.unwrap();
    assert!(matches!(
        work.await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_unaccepted(&service, &scope, &grant).await;
    assert_eq!(
        scope.activate_job(&grant.retry()).await.unwrap().outcome,
        JobOutcome::Completed {}
    );
}

#[derive(Debug)]
struct Gate {
    entered: flume::Sender<()>,
    resume: flume::Receiver<()>,
}
impl Gate {
    async fn wait(self) {
        self.entered.send_async(()).await.unwrap();
        self.resume.recv_async().await.unwrap();
    }
}
fn gate() -> (Gate, flume::Receiver<()>, flume::Sender<()>) {
    let (entered, observe) = flume::bounded(1);
    let (resume, wait) = flume::bounded(1);
    (
        Gate {
            entered,
            resume: wait,
        },
        observe,
        resume,
    )
}

#[derive(Clone, Copy)]
enum Reply {
    Honest,
    Lost,
    Foreign,
}
struct Holds {
    inner: deployment_fixture::OwnedClient,
    gate: RefCell<Option<Gate>>,
    reply: Cell<Reply>,
    calls: RefCell<Vec<HoldGeneration>>,
}
impl Holds {
    fn new(platform: &Deployments, app: &AppId) -> Self {
        Self {
            inner: platform.client(app),
            gate: RefCell::new(None),
            reply: Cell::new(Reply::Honest),
            calls: RefCell::new(vec![]),
        }
    }
}
#[async_trait::async_trait(?Send)]
impl DeploymentHoldClient for Holds {
    fn scope(&self) -> &HoldScope {
        self.inner.scope()
    }
    async fn acquire(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.calls.borrow_mut().push(generation);
        let mut receipt = self.inner.acquire(deployment, generation).await?;
        let wait = self.gate.borrow_mut().take();
        if let Some(gate) = wait {
            gate.wait().await;
        }
        match self.reply.replace(Reply::Honest) {
            Reply::Honest => Ok(receipt),
            Reply::Lost => Err(WorkflowServiceError::Unavailable("lost hold reply".into())),
            Reply::Foreign => {
                receipt.app_id = AppId::mint();
                Ok(receipt)
            }
        }
    }
    async fn release(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.inner.release(deployment, generation).await
    }
}

async fn policy_hold(store: Rc<OrmStore>) {
    let (service, app, other, platform) = empty_service(store, Deployments::new().await).await;
    let client = Rc::new(Holds::new(&platform, &app));
    let service = service.with_deployments(
        platform
            .binding(&[&app, &other])
            .with_hold_client(client.clone()),
    );
    let deployment = platform.deploy(&app).await;
    let scope = service.fixture_app(app.clone());
    let grant = Grant::new(&app, &deployment, 1);
    let (wait, entered, resume) = gate();
    *client.gate.borrow_mut() = Some(wait);
    let (result, ()) = futures::join!(scope.activate_job(&grant), async {
        entered.recv_async().await.unwrap();
        service
            .policies
            .fixture_install(&app, leased_policy(2, AppPolicy::default()))
            .unwrap();
        resume.send_async(()).await.unwrap();
    });
    assert!(matches!(result, Err(WorkflowServiceError::Unavailable(_))));
    assert_unaccepted(&service, &scope, &grant).await;
    let holds = snapshot(&service, "deployment_holds", &app).await;
    assert_eq!(holds.len(), 1);
    assert_eq!(holds[0]["state"], value!("acquiring"));
    assert_eq!(holds[0]["deploy_hash"], value!(null));
    assert_eq!(
        scope.activate_job(&grant.retry()).await.unwrap().outcome,
        JobOutcome::Completed {}
    );
    let calls = client.calls.borrow();
    assert!(calls.len() >= 2);
    assert!(calls.iter().all(|generation| *generation == calls[0]));
}

async fn hold_replies(store: Rc<OrmStore>) {
    let (service, app, other, platform) = empty_service(store, Deployments::new().await).await;
    let client = Rc::new(Holds::new(&platform, &app));
    let service = service.with_deployments(
        platform
            .binding(&[&app, &other])
            .with_hold_client(client.clone()),
    );
    let deployment = platform.deploy(&app).await;
    let scope = service.fixture_app(app.clone());
    let grant = Grant::new(&app, &deployment, 1);
    for reply in [Reply::Lost, Reply::Foreign] {
        client.reply.set(reply);
        assert!(scope.activate_job(&grant.retry()).await.is_err());
        assert_unaccepted(&service, &scope, &grant).await;
        let holds = snapshot(&service, "deployment_holds", &app).await;
        assert_eq!(holds.len(), 1);
        assert_eq!(holds[0]["state"], value!("acquiring"));
        assert_eq!(holds[0]["deploy_hash"], value!(null));
        platform.assert_held(&app, &deployment.id).await;
    }
    let reopened = reopened(&service).await;
    assert_eq!(
        reopened
            .fixture_app(app.clone())
            .activate_job(&grant.retry())
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    let holds = snapshot(&reopened, "deployment_holds", &app).await;
    assert_eq!(holds[0]["state"], value!("held"));
    assert_eq!(holds[0]["deploy_hash"], value!(deployment.hash));
    let calls = client.calls.borrow();
    assert!(calls.len() >= 3);
    assert!(calls.iter().all(|generation| *generation == calls[0]));
}

#[derive(Debug)]
struct Artifacts {
    local: LocalDiskBlobStore,
    gate: Mutex<Option<Gate>>,
    reads: AtomicUsize,
}
#[async_trait::async_trait(?Send)]
impl BlobStore for Artifacts {
    async fn get_blob(&self, hash: &str) -> Result<bytes::Bytes, BlobError> {
        self.local.get_blob(hash).await
    }
    fn local_path(&self, hash: &str) -> Option<PathBuf> {
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
    async fn put_manifest(&self, app: &AppId, hash: &str, bytes: &[u8]) -> Result<(), BlobError> {
        self.local.put_manifest(app, hash, bytes).await
    }
    async fn get_manifest(&self, app: &AppId, hash: &str) -> Result<bytes::Bytes, BlobError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let bytes = self.local.get_manifest(app, hash).await?;
        let wait = self.gate.lock().unwrap().take();
        if let Some(gate) = wait {
            gate.wait().await;
        }
        Ok(bytes)
    }
    async fn delete_manifest(&self, app: &AppId, hash: &str) -> Result<bool, BlobError> {
        self.local.delete_manifest(app, hash).await
    }
    async fn delete_app_manifests(&self, app: &AppId) -> Result<(), BlobError> {
        self.local.delete_app_manifests(app).await
    }
}

async fn policy_artifact(store: Rc<OrmStore>) {
    let directory = Arc::new(tempfile::tempdir().unwrap());
    let objects = Arc::new(Artifacts {
        local: LocalDiskBlobStore::new(directory.path().join("artifacts")).unwrap(),
        gate: Mutex::new(None),
        reads: AtomicUsize::new(0),
    });
    let platform = Deployments::with_source(directory, objects.clone()).await;
    let (service, app, _, platform) = empty_service(store, platform).await;
    let deployment = platform.deploy(&app).await;
    let scope = service.fixture_app(app.clone());
    let grant = Grant::new(&app, &deployment, 1);
    let (wait, entered, resume) = gate();
    *objects.gate.lock().unwrap() = Some(wait);
    let (result, ()) = futures::join!(scope.activate_job(&grant), async {
        entered.recv_async().await.unwrap();
        service
            .policies
            .fixture_install(&app, leased_policy(2, AppPolicy::default()))
            .unwrap();
        resume.send_async(()).await.unwrap();
    });
    assert!(matches!(result, Err(WorkflowServiceError::Unavailable(_))));
    assert_unaccepted(&service, &scope, &grant).await;
    assert!(snapshot(&service, "deploys", &app).await.is_empty());
    let receipt = scope.activate_job(&grant.retry()).await.unwrap();
    let reads = objects.reads.load(Ordering::SeqCst);
    assert!(reads > 0);
    objects
        .local
        .delete_manifest(&app, &deployment.hash)
        .await
        .unwrap();
    assert_eq!(scope.activate_job(&grant.retry()).await.unwrap(), receipt);
    assert_eq!(objects.reads.load(Ordering::SeqCst), reads);
}

enum ReceiptFault {
    Sqlite(PathBuf),
    Postgres(Box<compio_postgres::Client>),
}
impl ReceiptFault {
    async fn set(&self, enabled: bool) {
        match self {
            Self::Sqlite(path) => {
                let path = path.clone();
                compio::runtime::spawn_blocking(move || {
                    let connection = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
                    connection.execute_batch(if enabled {
                        "CREATE TRIGGER activation_receipt_fault BEFORE UPDATE OF outcome ON __zeroship_workflow_job_receipts BEGIN SELECT RAISE(ABORT, 'activation receipt fault'); END"
                    } else { "DROP TRIGGER activation_receipt_fault" }).unwrap();
                }).await.unwrap();
            }
            Self::Postgres(client) => client.batch_execute(if enabled {
                "CREATE FUNCTION customer.activation_receipt_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'activation receipt fault'; END $$; CREATE TRIGGER activation_receipt_fault BEFORE UPDATE OF outcome ON customer.__zeroship_workflow_job_receipts FOR EACH ROW EXECUTE FUNCTION customer.activation_receipt_fault();"
            } else { "DROP TRIGGER activation_receipt_fault ON customer.__zeroship_workflow_job_receipts; DROP FUNCTION customer.activation_receipt_fault();" }).await.unwrap(),
        }
    }
}

#[compio::test]
async fn sqlite_activation_receipt_rolls_back_history_and_selection() {
    let directory = tempfile::tempdir().unwrap();
    let store = Rc::new(sqlite_store(&directory.path().join("creator.sqlite")).await);
    let attached = store
        .backend
        .query(
            &store.binding,
            "PRAGMA database_list",
            &[],
        )
        .await
        .unwrap();
    let namespace = store
        .backend
        .namespace(&store.binding);
    let path = attached
        .iter()
        .find(|row| row.get("name").and_then(Value::as_str) == Some(namespace))
        .and_then(|row| row.get("file"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap();
    Box::pin(rollback(store, ReceiptFault::Sqlite(path))).await;
}
#[compio::test]
async fn postgres_activation_receipt_rolls_back_history_and_selection() {
    let fixture = PostgresFixture::start().await;
    let admin = connect(&fixture.admin_url).await;
    Box::pin(rollback(
        Rc::new(fixture.store.clone()),
        ReceiptFault::Postgres(Box::new(admin)),
    ))
    .await;
}

async fn rollback(store: Rc<OrmStore>, fault: ReceiptFault) {
    let (service, app, _, platform) = empty_service(store, Deployments::new().await).await;
    let first = platform.deploy(&app).await;
    let second = platform.deploy(&app).await;
    let scope = service.fixture_app(app.clone());
    scope
        .activate_job(&Grant::new(&app, &first, 1))
        .await
        .unwrap();
    let grant = Grant::new(&app, &second, 2);
    let history = snapshot(&service, "activations", &app).await;
    let selection = snapshot(&service, "activation_scopes", &app).await;
    fault.set(true).await;
    assert!(scope.activate_job(&grant).await.is_err());
    assert!(scope
        .job_receipt(&grant.delivery.job)
        .await
        .unwrap()
        .is_none());
    assert_eq!(snapshot(&service, "activations", &app).await, history);
    assert_eq!(
        snapshot(&service, "activation_scopes", &app).await,
        selection
    );
    assert_eq!(selected(&service, &app).await, first.id);
    assert!(snapshot(&service, "deploys", &app)
        .await
        .iter()
        .all(|row| row["id"] != value!(second.id)));
    fault.set(false).await;
    assert_eq!(
        scope.activate_job(&grant.retry()).await.unwrap().outcome,
        JobOutcome::Completed {}
    );
    assert_eq!(selected(&service, &app).await, second.id);
}

/// One activation attempt ends at the journal I/O ceiling, not at the end of
/// the authority it captured. The stall sits in the platform hold client, so
/// the window measured here is the attempt's own budget and not a journal wait.
///
/// This pins the composition, not the magnitude. Both arms move with
/// [`ATTEMPT_IO_CEILING`], so retuning the ceiling keeps them green; what fails
/// is dropping the ceiling term and handing one attempt its whole authority.
async fn io_ceiling(store: Rc<OrmStore>) {
    let (service, app, other, platform) = empty_service(store, Deployments::new().await).await;
    let client = Rc::new(Holds::new(&platform, &app));
    let service = service.with_deployments(
        platform
            .binding(&[&app, &other])
            .with_hold_client(client.clone()),
    );
    let deployment = platform.deploy(&app).await;
    let scope = service.fixture_app(app.clone());

    let mut grant = Grant::new(&app, &deployment, 1);
    grant.expires = Instant::now() + ATTEMPT_IO_CEILING * 6;
    assert!(
        grant.remaining().unwrap() > ATTEMPT_IO_CEILING * 3,
        "the fixture authority is narrower than the window asserted below, so it \
         would bound this attempt instead of the ceiling"
    );
    let (wait, _entered, _resume) = gate();
    *client.gate.borrow_mut() = Some(wait);
    let started = Instant::now();
    let result = scope.activate_job(&grant).await;
    let capped = started.elapsed();
    assert!(
        matches!(result, Err(WorkflowServiceError::Timeout)),
        "{result:?}"
    );
    assert!(
        capped >= ATTEMPT_IO_CEILING,
        "the attempt ended before the ceiling, so something other than its budget \
         stopped it and this measures nothing: {capped:?}"
    );
    assert!(
        capped < ATTEMPT_IO_CEILING * 3,
        "one attempt was handed authority beyond the ceiling: {capped:?}"
    );

    // The control moves one variable. An authority narrower than the ceiling
    // binds the same stalled attempt instead, so the arm above is not a fixed
    // wait that would pass with the ceiling term removed.
    let narrow = ATTEMPT_IO_CEILING / 5;
    let mut short = grant.retry();
    short.expires = Instant::now() + narrow;
    let (wait, _entered, _resume) = gate();
    *client.gate.borrow_mut() = Some(wait);
    let started = Instant::now();
    let result = scope.activate_job(&short).await;
    let bounded = started.elapsed();
    assert!(
        matches!(result, Err(WorkflowServiceError::Timeout)),
        "{result:?}"
    );
    assert!(
        bounded >= narrow,
        "the control ended before its own authority, so it bounded nothing: {bounded:?}"
    );
    assert!(
        bounded < ATTEMPT_IO_CEILING,
        "the control was capped by the ceiling too, so the arm above proves \
         nothing: {bounded:?}"
    );
    assert_unaccepted(&service, &scope, &grant).await;
}
