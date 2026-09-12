use super::*;
use crate::deployment_holds::{
    self, DeploymentHoldClient, DeploymentHolds, HoldGeneration, HoldReceipt, HoldScope, HoldState,
    ScopedDeploymentHolds,
};
use crate::operations::{RestartDeploy, RestartOptions, RunOperation};
use std::{cell::Cell, time::Duration};
use zeroship_data_orm::{
    binding::DbBinding, encryption::ProjectKeySource, orm::Database, value, ConnectOptions, Value,
};

struct Platform {
    _directory: tempfile::TempDir,
    database: Database,
    ledger: DeploymentHolds,
}
impl Platform {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("platform.sqlite");
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch(deployment_holds::SQLITE_SCHEMA)
            .unwrap();
        let database = Database::connect(
            DbBinding::new(
                "platform",
                "metadata-test",
                super::super::store::SchemaName::new("main").unwrap(),
            ),
            ConnectOptions::new(
                format!("sqlite:{}", path.display()),
                ProjectKeySource::unavailable(),
            ),
            deployment_holds::collections().unwrap(),
        )
        .await
        .unwrap();
        Self {
            _directory: directory,
            ledger: DeploymentHolds::new(database.clone()).unwrap(),
            database,
        }
    }
    fn client(&self, app: &AppId) -> ScopedDeploymentHolds {
        self.ledger
            .for_scope(HoldScope::new(app.clone(), typed_id::generate("dhl")).unwrap())
    }
    async fn deploy(&self, app: &AppId, hash: char) -> DeployRegistration {
        let deploy = DeployRegistration {
            id: typed_id::generate("dep"),
            hash: hash.to_string().repeat(64),
            workflows: ["Example".into()].into(),
            schedules: vec![],
        };
        let mut document = value!({"id":deploy.id, "app_id":app.uuid().to_string(), "deploy_hash":deploy.hash,
            "manifest_json":"{}", "activated_at":null, "retention_state":"available", "retention_lock":0});
        document["created_at"] = Value::Timestamp(0);
        self.database
            .collection("app_deploys")
            .unwrap()
            .insert(document)
            .await
            .unwrap();
        deploy
    }
    async fn assert_held(&self, app: &AppId, deployment: &str) {
        let tx = self.database.begin_transaction().await.unwrap();
        assert!(matches!(
            deployment_holds::fence_reclamation(&tx, app, deployment).await,
            Err(WorkflowServiceError::Conflict(_))
        ));
        tx.rollback().await.unwrap();
    }
}

struct LostReplies {
    inner: ScopedDeploymentHolds,
    acquire: Cell<bool>,
    release: Cell<bool>,
}
#[async_trait::async_trait(?Send)]
impl DeploymentHoldClient for LostReplies {
    fn scope(&self) -> &HoldScope {
        self.inner.scope()
    }
    async fn acquire(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        let receipt = self.inner.acquire(deployment, generation).await?;
        if self.acquire.replace(false) {
            Err(WorkflowServiceError::Unavailable(
                "lost acquire reply".into(),
            ))
        } else {
            Ok(receipt)
        }
    }
    async fn release(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        let receipt = self.inner.release(deployment, generation).await?;
        if self.release.replace(false) {
            Err(WorkflowServiceError::Unavailable(
                "lost release reply".into(),
            ))
        } else {
            Ok(receipt)
        }
    }
}

#[compio::test]
async fn sqlite_deployment_intents_recover_lost_replies_and_close_admission() {
    let dir = tempfile::tempdir().unwrap();
    recovery_contract(Rc::new(
        sqlite_store(&dir.path().join("customer.sqlite")).await,
    ))
    .await;
}
#[compio::test]
async fn postgres_deployment_intents_recover_lost_replies_and_close_admission() {
    let fixture = PostgresFixture::start().await;
    recovery_contract(Rc::new(fixture.store.clone())).await;
}

async fn recovery_contract(store: Rc<OrmStore>) {
    let (service, app, other) = registered_service(store.clone()).await;
    let platform = Platform::new().await;
    let deploy = platform.deploy(&app, 'b').await;
    let client = LostReplies {
        inner: platform.client(&app),
        acquire: Cell::new(true),
        release: Cell::new(true),
    };
    assert!(matches!(
        service
            .acquire_deployment_hold(&app, &deploy.id, &deploy.hash, &client)
            .await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    platform.assert_held(&app, &deploy.id).await;
    assert_eq!(
        service
            .pending_deployment_holds(&app, None, 1)
            .await
            .unwrap(),
        [deploy.id.clone()]
    );
    assert!(service
        .pending_deployment_holds(&other, None, 1)
        .await
        .unwrap()
        .is_empty());
    assert!(service
        .pending_deployment_holds(&app, Some(&deploy.id), 1)
        .await
        .unwrap()
        .is_empty());
    assert!(matches!(
        service
            .acquire_deployment_hold(&other, &deploy.id, &deploy.hash, &client)
            .await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert!(service
        .acquire_deployment_hold(&app, &deploy.id, &deploy.hash, &platform.client(&app))
        .await
        .is_err());
    assert!(service
        .acquire_deployment_hold(&app, &deploy.id, &"c".repeat(64), &client)
        .await
        .is_err());
    assert!(service
        .activate_deploy(&app, &deploy, &test_snapshot())
        .await
        .is_err());
    let reopened = WorkflowService::open(store, service.policies.clone())
        .await
        .unwrap()
        .with_snapshots(fixture_snapshot_store());
    let held = reopened
        .reconcile_deployment_hold(&app, &deploy.id, &client)
        .await
        .unwrap();
    assert_eq!(held.state, HoldState::Held);
    assert_eq!(held.generation.get(), 1);
    assert!(reopened
        .pending_deployment_holds(&app, None, 1)
        .await
        .unwrap()
        .is_empty());
    reopened
        .activate_deploy(&other, &deploy, &test_snapshot())
        .await
        .unwrap();
    reopened
        .for_app(other.clone())
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    assert!(matches!(
        reopened
            .release_deployment_hold(&app, &deploy.id, &client)
            .await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_eq!(
        reopened
            .pending_deployment_holds(&app, None, 1)
            .await
            .unwrap(),
        [deploy.id.clone()]
    );
    assert!(reopened
        .acquire_deployment_hold(&app, &deploy.id, &deploy.hash, &client)
        .await
        .is_err());
    assert!(reopened
        .activate_deploy(&app, &deploy, &test_snapshot())
        .await
        .is_err());
    let released = reopened
        .reconcile_deployment_hold(&app, &deploy.id, &client)
        .await
        .unwrap();
    assert_eq!(released.state, HoldState::Released);
    assert_eq!(
        reopened
            .release_deployment_hold(&app, &deploy.id, &client)
            .await
            .unwrap(),
        released
    );
    assert!(reopened
        .activate_deploy(&app, &deploy, &test_snapshot())
        .await
        .is_err());
    let reacquired = reopened
        .acquire_deployment_hold(&app, &deploy.id, &deploy.hash, &client)
        .await
        .unwrap();
    assert_eq!(reacquired.generation.get(), 2);
    assert!(client
        .inner
        .release(&deploy.id, held.generation)
        .await
        .is_err());
    platform.assert_held(&app, &deploy.id).await;
    reopened
        .activate_deploy(&app, &deploy, &test_snapshot())
        .await
        .unwrap();
    assert!(
        reopened
            .release_deployment_hold(&app, &deploy.id, &client)
            .await
            .is_err(),
        "active deployment cannot be released"
    );
    let replacement = platform.deploy(&app, 'c').await;
    reopened
        .activate_deploy(&app, &replacement, &test_snapshot())
        .await
        .unwrap();
    reopened
        .release_deployment_hold(&app, &deploy.id, &client)
        .await
        .unwrap();
    assert!(reopened
        .activate_deploy(&app, &deploy, &test_snapshot())
        .await
        .is_err());
    reopened
        .acquire_deployment_hold(&app, &deploy.id, &deploy.hash, &client)
        .await
        .unwrap();
    reopened
        .activate_deploy(&app, &deploy, &test_snapshot())
        .await
        .unwrap();
}

#[compio::test]
async fn sqlite_retained_generations_keep_their_deployment_hold() {
    let dir = tempfile::tempdir().unwrap();
    dependencies_contract(Rc::new(
        sqlite_store(&dir.path().join("customer.sqlite")).await,
    ))
    .await;
}
#[compio::test]
async fn postgres_retained_generations_keep_their_deployment_hold() {
    let fixture = PostgresFixture::start().await;
    dependencies_contract(Rc::new(fixture.store.clone())).await;
}
async fn dependencies_contract(store: Rc<OrmStore>) {
    let (service, app, _) = registered_service(store).await;
    let platform = Platform::new().await;
    let first = platform.deploy(&app, 'b').await;
    let second = platform.deploy(&app, 'c').await;
    let client = platform.client(&app);
    service
        .acquire_deployment_hold(&app, &first.id, &first.hash, &client)
        .await
        .unwrap();
    service
        .activate_deploy(&app, &first, &test_snapshot())
        .await
        .unwrap();
    let scope = service.for_app(app.clone());
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    service
        .activate_deploy(&app, &second, &test_snapshot())
        .await
        .unwrap();
    assert!(service
        .release_deployment_hold(&app, &first.id, &client)
        .await
        .is_err());
    scope
        .transition(&RequestId::mint(), &run.id, RunOperation::Cancel)
        .await
        .unwrap();
    assert!(service
        .release_deployment_hold(&app, &first.id, &client)
        .await
        .is_err());
    scope
        .restart(
            &RequestId::mint(),
            &run.id,
            RestartOptions {
                from: None,
                deploy: Some(RestartDeploy::Latest),
            },
        )
        .await
        .unwrap();
    // The run head moved, but its retained previous generation still needs code.
    assert!(service
        .release_deployment_hold(&app, &first.id, &client)
        .await
        .is_err());
    platform.assert_held(&app, &first.id).await;
    assert!(service
        .pending_deployment_holds(&app, None, 1)
        .await
        .unwrap()
        .is_empty());
    // Failed release rolled back its admission fence.
    service
        .activate_deploy(&app, &first, &test_snapshot())
        .await
        .unwrap();
}

struct GatedReply {
    inner: ScopedDeploymentHolds,
    ready: flume::Sender<()>,
    resume: flume::Receiver<()>,
}
impl GatedReply {
    async fn deliver(&self, receipt: HoldReceipt) -> Result<HoldReceipt, WorkflowServiceError> {
        self.ready.send_async(()).await.unwrap();
        self.resume.recv_async().await.unwrap();
        Ok(receipt)
    }
}
#[async_trait::async_trait(?Send)]
impl DeploymentHoldClient for GatedReply {
    fn scope(&self) -> &HoldScope {
        self.inner.scope()
    }
    async fn acquire(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.deliver(self.inner.acquire(deployment, generation).await?)
            .await
    }
    async fn release(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.deliver(self.inner.release(deployment, generation).await?)
            .await
    }
}

#[compio::test]
async fn sqlite_old_acknowledgements_cannot_reopen_reacquired_holds() {
    let dir = tempfile::tempdir().unwrap();
    stale_contract(Rc::new(
        sqlite_store(&dir.path().join("customer.sqlite")).await,
    ))
    .await;
}
#[compio::test]
async fn postgres_old_acknowledgements_cannot_reopen_reacquired_holds() {
    let fixture = PostgresFixture::start().await;
    stale_contract(Rc::new(fixture.store.clone())).await;
}
async fn stale_contract(store: Rc<OrmStore>) {
    let (service, app, _) = registered_service(store).await;
    let platform = Platform::new().await;
    let deployment = platform.deploy(&app, 'b').await;
    let client = platform.client(&app);
    for release in [false, true] {
        let (ready, reached) = flume::bounded(1);
        let (resume, resumed) = flume::bounded(1);
        let gated = GatedReply {
            inner: client.clone(),
            ready,
            resume: resumed,
        };
        let running = service.clone();
        let running_app = app.clone();
        let deploy = deployment.clone();
        let attempt = compio::runtime::spawn(async move {
            if release {
                running
                    .release_deployment_hold(&running_app, &deploy.id, &gated)
                    .await
            } else {
                running
                    .acquire_deployment_hold(&running_app, &deploy.id, &deploy.hash, &gated)
                    .await
            }
        });
        compio::time::timeout(Duration::from_secs(10), reached.recv_async())
            .await
            .unwrap()
            .unwrap();
        // A different journal operation completes while the platform reply is
        // blocked, proving the outbound call has released the app transaction.
        compio::time::timeout(
            Duration::from_secs(10),
            service.pending_deployment_holds(&app, None, 1),
        )
        .await
        .unwrap()
        .unwrap();
        service
            .reconcile_deployment_hold(&app, &deployment.id, &client)
            .await
            .unwrap();
        if !release {
            service
                .release_deployment_hold(&app, &deployment.id, &client)
                .await
                .unwrap();
        }
        service
            .acquire_deployment_hold(&app, &deployment.id, &deployment.hash, &client)
            .await
            .unwrap();
        resume.send_async(()).await.unwrap();
        assert!(matches!(
            attempt.await.unwrap(),
            Err(WorkflowServiceError::Conflict(_))
        ));
        platform.assert_held(&app, &deployment.id).await;
        assert!(service
            .pending_deployment_holds(&app, None, 1)
            .await
            .unwrap()
            .is_empty());
    }
    service
        .release_deployment_hold(&app, &deployment.id, &client)
        .await
        .unwrap();
    let (ready, reached) = flume::bounded(1);
    let (_resume, resumed) = flume::bounded(1);
    let gated = GatedReply {
        inner: client.clone(),
        ready,
        resume: resumed,
    };
    let running = service.clone();
    let running_app = app.clone();
    let deploy = deployment.clone();
    let (abort, registration) = futures::future::AbortHandle::new_pair();
    let attempt = compio::runtime::spawn(futures::future::Abortable::new(
        async move {
            running
                .acquire_deployment_hold(&running_app, &deploy.id, &deploy.hash, &gated)
                .await
        },
        registration,
    ));
    compio::time::timeout(Duration::from_secs(10), reached.recv_async())
        .await
        .unwrap()
        .unwrap();
    abort.abort();
    assert!(attempt.await.unwrap().is_err());
    platform.assert_held(&app, &deployment.id).await;
    let reopened = WorkflowService::open(service.store.clone(), service.policies.clone())
        .await
        .unwrap();
    assert_eq!(
        reopened
            .pending_deployment_holds(&app, None, 1)
            .await
            .unwrap(),
        [deployment.id.clone()]
    );
    reopened
        .reconcile_deployment_hold(&app, &deployment.id, &client)
        .await
        .unwrap();
}

struct MismatchedReceipt {
    inner: ScopedDeploymentHolds,
    corrupt: fn(&mut HoldReceipt),
}
#[async_trait::async_trait(?Send)]
impl DeploymentHoldClient for MismatchedReceipt {
    fn scope(&self) -> &HoldScope {
        self.inner.scope()
    }
    async fn acquire(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        let mut receipt = self.inner.acquire(deployment, generation).await?;
        (self.corrupt)(&mut receipt);
        Ok(receipt)
    }
    async fn release(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.inner.release(deployment, generation).await
    }
}

#[compio::test]
async fn sqlite_receipts_require_complete_intent_identity() {
    let dir = tempfile::tempdir().unwrap();
    receipt_contract(Rc::new(
        sqlite_store(&dir.path().join("customer.sqlite")).await,
    ))
    .await;
}
#[compio::test]
async fn postgres_receipts_require_complete_intent_identity() {
    let fixture = PostgresFixture::start().await;
    receipt_contract(Rc::new(fixture.store.clone())).await;
}
async fn receipt_contract(store: Rc<OrmStore>) {
    let (service, app, _) = registered_service(store).await;
    let platform = Platform::new().await;
    let client = platform.client(&app);
    let corruptions: [fn(&mut HoldReceipt); 6] = [
        |r| r.app_id = AppId::mint(),
        |r| r.deploy_id = typed_id::generate("dep"),
        |r| r.holder_id = typed_id::generate("dhl"),
        |r| r.generation = r.generation.next().unwrap(),
        |r| r.deploy_hash = "f".repeat(64),
        |r| r.state = HoldState::Released,
    ];
    for (corrupt, hash) in corruptions.into_iter().zip('0'..='5') {
        let deploy = platform.deploy(&app, hash).await;
        let bad = MismatchedReceipt {
            inner: client.clone(),
            corrupt,
        };
        assert!(matches!(
            service
                .acquire_deployment_hold(&app, &deploy.id, &deploy.hash, &bad)
                .await,
            Err(WorkflowServiceError::Conflict(_))
        ));
        assert_eq!(
            service
                .pending_deployment_holds(&app, None, 1)
                .await
                .unwrap(),
            [deploy.id.clone()]
        );
        assert!(service
            .activate_deploy(&app, &deploy, &test_snapshot())
            .await
            .is_err());
        service
            .reconcile_deployment_hold(&app, &deploy.id, &client)
            .await
            .unwrap();
        service
            .release_deployment_hold(&app, &deploy.id, &client)
            .await
            .unwrap();
    }
}

#[compio::test]
async fn sqlite_schedule_references_prevent_deployment_release() {
    let dir = tempfile::tempdir().unwrap();
    schedule_contract(Rc::new(
        sqlite_store(&dir.path().join("customer.sqlite")).await,
    ))
    .await;
}
#[compio::test]
async fn postgres_schedule_references_prevent_deployment_release() {
    let fixture = PostgresFixture::start().await;
    schedule_contract(Rc::new(fixture.store.clone())).await;
}
async fn schedule_contract(store: Rc<OrmStore>) {
    use crate::service::{IntervalAnchor, ScheduleRegistration, ScheduleTiming};
    let (service, app, _) = registered_service(store).await;
    let platform = Platform::new().await;
    let client = platform.client(&app);
    let mut first = platform.deploy(&app, 'b').await;
    first.schedules.push(ScheduleRegistration {
        name: "periodic".into(),
        workflow_name: "Example".into(),
        schedule: ScheduleTiming::Interval {
            interval_ms: 3_600_000,
            anchor: IntervalAnchor::Deploy,
        },
        input: json!(null),
        overlap: Default::default(),
        catch_up: Default::default(),
    });
    let second = platform.deploy(&app, 'c').await;
    service
        .acquire_deployment_hold(&app, &first.id, &first.hash, &client)
        .await
        .unwrap();
    service
        .activate_deploy(&app, &first, &test_snapshot())
        .await
        .unwrap();
    service
        .activate_deploy(&app, &second, &test_snapshot())
        .await
        .unwrap();
    assert!(service
        .release_deployment_hold(&app, &first.id, &client)
        .await
        .is_err());
    platform.assert_held(&app, &first.id).await;
    assert!(service
        .pending_deployment_holds(&app, None, 1)
        .await
        .unwrap()
        .is_empty());
}
