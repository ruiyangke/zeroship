use super::*;
use crate::deployment_holds::{DeploymentHoldClient, HoldGeneration, HoldReceipt, HoldScope};
use std::time::Instant;
use zeroship_data_orm::{orm::Output, value};

struct Client {
    inner: deployment_fixture::OwnedClient,
    lose_reply: Cell<bool>,
    stall: Cell<bool>,
    entered: Cell<usize>,
    cancelled: Cell<usize>,
    calls: RefCell<Vec<String>>,
}
impl Client {
    fn new(deployments: &Deployments, app: &AppId) -> Self {
        Self {
            inner: deployments.client(app),
            lose_reply: Cell::new(false),
            stall: Cell::new(false),
            entered: Cell::new(0),
            cancelled: Cell::new(0),
            calls: RefCell::new(Vec::new()),
        }
    }
    async fn reply(&self, receipt: HoldReceipt) -> Result<HoldReceipt, WorkflowServiceError> {
        if self.lose_reply.replace(false) {
            return Err(WorkflowServiceError::Unavailable(
                "injected lost hold reply".into(),
            ));
        }
        if self.stall.get() {
            self.entered.set(self.entered.get() + 1);
            let _guard = Cancelled(&self.cancelled);
            std::future::pending::<()>().await;
        }
        Ok(receipt)
    }
}
struct Cancelled<'a>(&'a Cell<usize>);
impl Drop for Cancelled<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}
#[async_trait(?Send)]
impl DeploymentHoldClient for Client {
    fn scope(&self) -> &HoldScope {
        self.inner.scope()
    }
    async fn acquire(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.calls.borrow_mut().push(deployment.into());
        self.reply(self.inner.acquire(deployment, generation).await?)
            .await
    }
    async fn release(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.calls.borrow_mut().push(deployment.into());
        self.reply(self.inner.release(deployment, generation).await?)
            .await
    }
}

async fn lose_acquisition(
    service: &WorkflowService,
    app: &AppId,
    deployment: &DeployRegistration,
    client: &Client,
) {
    client.lose_reply.set(true);
    assert!(matches!(
        service
            .acquire_deployment_hold(app, &deployment.id, &deployment.hash, client)
            .await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
}

async fn state(service: &WorkflowService, app: &AppId, deployment: &str) -> String {
    let tx = service.begin().await.unwrap();
    let Output::Rows { rows, .. } = tx
        .database()
        .collection("__zeroship_workflow_deployment_holds")
        .unwrap()
        .find(
            value!({"app_id":app.as_str(), "deploy_id":deployment}),
            value!({}),
        )
        .await
        .unwrap()
    else {
        panic!("hold rows");
    };
    let result = rows[0]["state"].as_str().unwrap().to_owned();
    tx.commit().await.unwrap();
    result
}

async fn until_held(service: &WorkflowService, app: &AppId, deployment: &str) {
    compio::time::timeout(Duration::from_secs(5), async {
        while state(service, app, deployment).await != "held" {
            compio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("hold recovery did not settle");
}

fn worker(
    service: &WorkflowService,
    dir: &Path,
    probe: Rc<Probe>,
    timeout_ms: u64,
) -> WorkflowWorker {
    let service = service
        .clone()
        .with_payload_storage(zeroship_storage::StorageStore::from_backend(Arc::new(
            zeroship_storage::LocalFs::new(dir),
        )))
        .unwrap();
    WorkflowWorker::new(
        Rc::new(service.tasks(WorkerIdentity::new("hold-worker".into()).unwrap())),
        Rc::new(Executor(probe)),
        WorkerOptions {
            maintenance_timeout_ms: timeout_ms,
            ..options()
        },
    )
    .unwrap()
}

#[compio::test]
async fn sqlite_background_holds_recover_after_restart_and_skip_invalid_or_unassigned_intents() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    Box::pin(recovery_contract(
        Rc::new(sqlite_store(&path).await),
        dir.path(),
    ))
    .await;
}

#[compio::test]
async fn postgres_background_holds_recover_after_restart_and_skip_invalid_or_unassigned_intents() {
    let fixture = PostgresFixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    Box::pin(recovery_contract(
        Rc::new(fixture.store.clone()),
        dir.path(),
    ))
    .await;
}

#[expect(
    clippy::too_many_lines,
    reason = "follow interrupted acquisitions and releases across host restart"
)]
async fn recovery_contract(store: Rc<OrmStore>, dir: &Path) {
    let (service, a, b, deployments) = registered_service(store.clone()).await;
    let first = Rc::new(Client::new(&deployments, &a));
    let second = Rc::new(Client::new(&deployments, &b));
    let mut pending = [deployments.deploy(&a).await, deployments.deploy(&a).await];
    pending.sort_by(|a, b| a.id.cmp(&b.id));
    let [late, acquire] = pending;
    let release = deployments.deploy(&a).await;
    lose_acquisition(&service, &a, &acquire, &first).await;
    service
        .acquire_deployment_hold(&a, &release.id, &release.hash, first.as_ref())
        .await
        .unwrap();
    first.lose_reply.set(true);
    assert!(service
        .release_deployment_hold(&a, &release.id, first.as_ref())
        .await
        .is_err());
    let expired = deployments.deploy(&b).await;
    lose_acquisition(&service, &b, &expired, &second).await;

    // Corrupt identities and values must not prevent discovery of later intents.
    let mut tx = service.begin().await.unwrap();
    tx.database()
        .collection("__zeroship_workflow_deployment_holds")
        .unwrap()
        .insert(value!({
            "app_id":a.as_str(), "deploy_id":"!invalid", "deploy_hash":"invalid",
            "holder_id":first.scope().holder(), "generation":-1, "state":"acquiring"
        }))
        .await
        .unwrap();
    if tx.dialect() == "sqlite" {
        tx.execute(
            &format!(
                "UPDATE {} SET generation='corrupt' WHERE app_id=$1 AND deploy_id='!invalid'",
                tx.table("deployment_holds")
            ),
            &[a.as_str().into()],
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();

    let foreign = AppId::mint();
    let foreign_client = Rc::new(Client::new(&deployments, &foreign));
    let foreign_owner = WorkflowService::open(store.clone(), Arc::new(HostPolicies::default()))
        .await
        .unwrap();
    foreign_owner
        .register_app(&foreign, configured_policy(1, AppPolicy::default()))
        .await
        .unwrap();
    let foreign_deploy = deployments.deploy(&foreign).await;
    lose_acquisition(&foreign_owner, &foreign, &foreign_deploy, &foreign_client).await;
    foreign_client.calls.borrow_mut().clear();
    drop(service);

    let reopened = WorkflowService::open(store, Arc::new(HostPolicies::default()))
        .await
        .unwrap()
        .with_deployments(
            deployments
                .binding(&[&a, &b, &foreign])
                .with_hold_client(first.clone())
                .with_hold_client(second.clone())
                .with_hold_client(foreign_client.clone()),
        );
    reopened
        .register_app(&a, configured_policy(1, AppPolicy::default()))
        .await
        .unwrap();
    reopened
        .register_app(
            &b,
            PolicySnapshot::lease(
                1_i64.try_into().unwrap(),
                AppPolicy::default(),
                Instant::now(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let probe = Rc::new(Probe::default());
    let mut worker = worker(&reopened, dir, probe.clone(), 1_000);
    worker
        .run_until(async {
            until_held(&reopened, &a, &acquire.id).await;
            until_held(&reopened, &b, &expired.id).await;
            compio::time::timeout(Duration::from_secs(5), async {
                while state(&reopened, &a, &release.id).await != "released" {
                    compio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
        })
        .await;
    deployments.assert_held(&a, &acquire.id).await;
    deployments.assert_held(&b, &expired.id).await;
    let tx = deployments.database.begin_transaction().await.unwrap();
    crate::deployment_holds::fence_reclamation(&tx, &a, &release.id)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(
        state(&foreign_owner, &foreign, &foreign_deploy.id).await,
        "acquiring"
    );
    assert!(foreign_client.calls.borrow().is_empty());
    assert!(!first.calls.borrow().iter().any(|id| id == "!invalid"));
    assert!(probe.started.borrow().is_empty());
    let tx = reopened.begin().await.unwrap();
    assert!(crate::service::deploys::read(&tx, &a, &acquire.id)
        .await
        .unwrap()
        .is_none());
    tx.commit().await.unwrap();

    // A later intent can sort behind the saved cursor and must still be found.
    lose_acquisition(&reopened, &a, &late, &first).await;
    worker.run_until(until_held(&reopened, &a, &late.id)).await;
    deployments.assert_held(&a, &late.id).await;
}

#[compio::test]
async fn sqlite_stalled_hold_recovery_preserves_execution_fairness_and_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    Box::pin(stalled_contract(
        Rc::new(sqlite_store(&path).await),
        dir.path(),
    ))
    .await;
}

#[compio::test]
async fn postgres_stalled_hold_recovery_preserves_execution_fairness_and_shutdown() {
    let fixture = PostgresFixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    Box::pin(stalled_contract(Rc::new(fixture.store.clone()), dir.path())).await;
}

async fn stalled_contract(store: Rc<OrmStore>, dir: &Path) {
    let (service, a, b, deployments) = registered_service(store).await;
    let [a, b] = if a < b { [a, b] } else { [b, a] };
    let stalled = Rc::new(Client::new(&deployments, &a));
    let healthy = Rc::new(Client::new(&deployments, &b));
    let deployment = deployments.deploy(&a).await;
    let other = deployments.deploy(&b).await;
    lose_acquisition(&service, &a, &deployment, &stalled).await;
    lose_acquisition(&service, &b, &other, &healthy).await;
    stalled.stall.set(true);
    let service = service.with_deployments(
        deployments
            .binding(&[&a, &b])
            .with_hold_client(stalled.clone())
            .with_hold_client(healthy),
    );
    let api = service.for_app(a.clone());
    let run = api
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let probe = Rc::new(Probe::default());
    let mut worker = worker(&service, dir, probe.clone(), 250);
    worker
        .run_until(async {
            wait_for(|| stalled.entered.get() > 0).await;
            compio::time::timeout(Duration::from_secs(5), async {
                while api.status(&run.id).await.unwrap().state != RunState::Completed {
                    compio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
        })
        .await;
    assert_eq!(probe.active.get(), 0);
    assert_eq!(stalled.cancelled.get(), stalled.entered.get());
    assert_eq!(state(&service, &a, &deployment.id).await, "acquiring");
    deployments.assert_held(&a, &deployment.id).await;
    let cancelled = stalled.cancelled.get();
    worker
        .run_until(async {
            until_held(&service, &b, &other.id).await;
            wait_for(|| stalled.cancelled.get() > cancelled).await;
        })
        .await;
    assert_eq!(state(&service, &a, &deployment.id).await, "acquiring");
    stalled.stall.set(false);
    worker
        .run_until(until_held(&service, &a, &deployment.id))
        .await;
    deployments.assert_held(&a, &deployment.id).await;
}
