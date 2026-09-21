use super::*;
use crate::deployment_holds::{
    DeploymentHoldClient, HoldGeneration, HoldReceipt, HoldScope, HoldState,
};
use crate::service::AppDeployments;
use futures::{channel::oneshot, future::Either};

struct HoldClient {
    inner: deployment_fixture::OwnedClient,
    calls: RefCell<Vec<(String, HoldState, i64)>>,
    lose_acquire: Cell<bool>,
    lose_release: Cell<bool>,
    hang: RefCell<Option<String>>,
    barrier: RefCell<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
}

impl HoldClient {
    fn new(platform: &Deployments, app: &AppId) -> Rc<Self> {
        Rc::new(Self {
            inner: platform.client(app),
            calls: RefCell::new(Vec::new()),
            lose_acquire: Cell::new(false),
            lose_release: Cell::new(false),
            hang: RefCell::new(None),
            barrier: RefCell::new(None),
        })
    }

    fn gate(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, observed) = oneshot::channel();
        let (release, blocked) = oneshot::channel();
        assert!(self
            .barrier
            .borrow_mut()
            .replace((entered, blocked))
            .is_none());
        (observed, release)
    }

    async fn finish(
        &self,
        receipt: HoldReceipt,
        lost: &Cell<bool>,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        let barrier = self.barrier.borrow_mut().take();
        if let Some((entered, resume)) = barrier {
            entered.send(()).unwrap();
            resume.await.unwrap();
        }
        if lost.replace(false) {
            Err(WorkflowServiceError::Unavailable(
                "lost hold acknowledgement".into(),
            ))
        } else {
            Ok(receipt)
        }
    }
}

#[async_trait::async_trait(?Send)]
impl DeploymentHoldClient for HoldClient {
    fn scope(&self) -> &HoldScope {
        self.inner.scope()
    }

    async fn acquire(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.calls
            .borrow_mut()
            .push((deployment.into(), HoldState::Held, generation.get()));
        if self.hang.borrow().as_deref() == Some(deployment) {
            std::future::pending::<()>().await;
        }
        self.finish(
            self.inner.acquire(deployment, generation).await?,
            &self.lose_acquire,
        )
        .await
    }

    async fn release(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.calls
            .borrow_mut()
            .push((deployment.into(), HoldState::Released, generation.get()));
        self.finish(
            self.inner.release(deployment, generation).await?,
            &self.lose_release,
        )
        .await
    }
}

fn with_client(
    service: WorkflowService,
    platform: &Deployments,
    client: Rc<HoldClient>,
) -> WorkflowService {
    service.with_deployments(
        AppDeployments::new(platform.source.clone(), 1024 * 1024)
            .unwrap()
            .with_hold_client(client),
    )
}

async fn pending(
    service: &WorkflowService,
    app: &AppId,
    platform: &Deployments,
    client: &HoldClient,
) -> DeployRegistration {
    let deployment = platform.deploy(app).await;
    client.lose_acquire.set(true);
    assert!(matches!(
        service
            .acquire_deployment_hold(app, &deployment.id, &deployment.hash, client)
            .await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    deployment
}

async fn intent(
    service: &WorkflowService,
    app: &AppId,
    deployment: &str,
) -> super::super::super::store::Row {
    let tx = service.begin().await.unwrap();
    let mut rows = journal_rows(
        &tx,
        "deployment_holds",
        json!({"app_id":app.as_str(), "deploy_id":deployment}),
    )
    .await;
    assert_eq!(rows.len(), 1);
    tx.commit().await.unwrap();
    rows.remove(0)
}

async fn holds_count(service: &WorkflowService, app: &AppId) -> i64 {
    let tx = service.begin().await.unwrap();
    let count = journal_count(&tx, "deployment_holds", json!({"app_id":app.as_str()})).await;
    tx.commit().await.unwrap();
    count
}

async fn page(
    scope: &AppWorkflows,
    seed: &JobSpec,
    publisher: &Publisher,
    size: u32,
    expected: JobOutcome,
) {
    assert_eq!(
        scope
            .reconcile_job(&Grant::new(seed), publisher, options(size))
            .await
            .unwrap()
            .outcome,
        expected
    );
}

case!(
    sqlite_delivered_reconciliation_recovers_hold_replies_and_preserves_scope,
    postgres_delivered_reconciliation_recovers_hold_replies_and_preserves_scope,
    lost_replies
);
case!(
    sqlite_delivered_hold_reconciliation_passes_failures_and_publication_churn,
    postgres_delivered_hold_reconciliation_passes_failures_and_publication_churn,
    fairness
);
case!(
    sqlite_delivered_hold_reconciliation_preserves_intent_on_authority_loss,
    postgres_delivered_hold_reconciliation_preserves_intent_on_authority_loss,
    authority_loss
);
case!(
    sqlite_delivered_hold_reconciliation_refuses_a_stale_generation_reply,
    postgres_delivered_hold_reconciliation_refuses_a_stale_generation_reply,
    generation_race
);
case!(
    sqlite_delivered_hold_page_refuses_equal_revision_phase_substitution,
    postgres_delivered_hold_page_refuses_equal_revision_phase_substitution,
    phase_substitution
);
case!(
    sqlite_completed_hold_page_replays_without_live_authority_or_deployment_client,
    postgres_completed_hold_page_replays_without_live_authority_or_deployment_client,
    completed_replay
);
case!(
    sqlite_delivered_hold_page_passes_malformed_pending_intent,
    postgres_delivered_hold_page_passes_malformed_pending_intent,
    malformed_intent
);

async fn completed_replay(store: Rc<OrmStore>) {
    let (service, app, foreign, platform) = registered_service(store.clone()).await;
    let client = HoldClient::new(&platform, &app);
    let deployment = pending(&service, &app, &platform, &client).await;
    let service = with_client(service, &platform, client.clone());
    let scope = service.fixture_app(app.clone());
    let seed = start(&scope, 1).await.remove(0);
    let publisher = Publisher::new(&app).await;
    page(&scope, &seed, &publisher, 1, JobOutcome::Waiting {}).await;
    let grant = Grant::new(&seed);
    let receipt = scope
        .reconcile_job(&grant, &publisher, options(1))
        .await
        .unwrap();
    assert_eq!(receipt.outcome, JobOutcome::Completed {});
    let held = intent(&service, &app, &deployment.id).await.0;
    let count = holds_count(&service, &app).await;
    let scan = scans(&service, &app).await.remove(0).0;
    client.calls.borrow_mut().clear();
    publisher.calls.borrow_mut().clear();
    drop(scope);
    drop(service);

    let policies = Arc::new(HostPolicies::default());
    let binding = policies.bind(app.clone()).unwrap();
    binding
        .begin_refresh()
        .unwrap()
        .install(
            PolicySnapshot::lease(1.try_into().unwrap(), AppPolicy::default(), Instant::now())
                .unwrap(),
        )
        .unwrap();
    assert!(binding.authority().is_err());
    let reopened = WorkflowService::open(store, policies).await.unwrap();
    let scope = reopened.bind_app(&binding).unwrap();
    let mut retry = grant.retry();
    retry.expires = Instant::now();
    assert!(retry.remaining().is_none());
    assert_eq!(
        scope
            .reconcile_job(&retry, &publisher, options(1))
            .await
            .unwrap(),
        receipt
    );
    let missing = Grant::new(&seed);
    assert!(scope
        .reconcile_job(&missing, &publisher, options(1))
        .await
        .is_err());
    assert!(scope
        .job_receipt(&missing.delivery.job)
        .await
        .unwrap()
        .is_none());
    retry.delivery.job.available_at = (retry.delivery.job.available_at.get() + 1)
        .try_into()
        .unwrap();
    assert!(scope
        .reconcile_job(&retry, &publisher, options(1))
        .await
        .is_err());
    retry.delivery.job = grant.delivery.job.clone();
    retry.delivery.job.app_id = foreign;
    assert!(matches!(
        scope.reconcile_job(&retry, &publisher, options(1)).await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert!(client.calls.borrow().is_empty());
    assert!(publisher.calls.borrow().is_empty());
    assert_eq!(intent(&reopened, &app, &deployment.id).await.0, held);
    assert_eq!(holds_count(&reopened, &app).await, count);
    assert_eq!(scans(&reopened, &app).await.remove(0).0, scan);
}

async fn malformed_intent(store: Rc<OrmStore>) {
    let (service, app, _, platform) = registered_service(store).await;
    let client = HoldClient::new(&platform, &app);
    let mut deployments = Vec::new();
    for _ in 0..2 {
        deployments.push(pending(&service, &app, &platform, &client).await);
    }
    deployments.sort_by(|left, right| left.id.cmp(&right.id));
    let tx = service.begin().await.unwrap();
    journal_update(
        &tx,
        "deployment_holds",
        json!({"app_id":app.as_str(), "deploy_id":deployments[0].id}),
        json!({"deploy_hash":"malformed-hash"}),
    )
    .await;
    tx.commit().await.unwrap();
    let malformed = intent(&service, &app, &deployments[0].id).await.0;
    let count = holds_count(&service, &app).await;
    client.calls.borrow_mut().clear();
    let service = with_client(service, &platform, client.clone());
    let scope = service.fixture_app(app.clone());
    let seed = start(&scope, 1).await.remove(0);
    let publisher = Publisher::new(&app).await;
    page(&scope, &seed, &publisher, 2, JobOutcome::Waiting {}).await;
    page(&scope, &seed, &publisher, 2, JobOutcome::Completed {}).await;
    assert_eq!(
        intent(&service, &app, &deployments[0].id).await.0,
        malformed
    );
    assert_eq!(
        intent(&service, &app, &deployments[1].id)
            .await
            .text("state")
            .unwrap(),
        "held"
    );
    assert_eq!(
        client.calls.borrow().as_slice(),
        &[(deployments[1].id.clone(), HoldState::Held, 1)]
    );
    assert_eq!(holds_count(&service, &app).await, count);
    let tx = service.begin().await.unwrap();
    journal_update(
        &tx,
        "deployment_holds",
        json!({"app_id":app.as_str(), "deploy_id":deployments[0].id}),
        json!({"deploy_hash":deployments[0].hash}),
    )
    .await;
    tx.commit().await.unwrap();
    page(&scope, &seed, &publisher, 2, JobOutcome::Waiting {}).await;
    page(&scope, &seed, &publisher, 2, JobOutcome::Completed {}).await;
    assert_eq!(
        client.calls.borrow().as_slice(),
        &[
            (deployments[1].id.clone(), HoldState::Held, 1),
            (deployments[0].id.clone(), HoldState::Held, 1),
        ]
    );
    assert!(service
        .pending_deployment_holds(&app, None, 10)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(holds_count(&service, &app).await, count);
}

async fn lost_replies(store: Rc<OrmStore>) {
    let (service, app, foreign, platform) = registered_service(store.clone()).await;
    let client = HoldClient::new(&platform, &app);
    let foreign_client = HoldClient::new(&platform, &foreign);
    let acquiring = pending(&service, &app, &platform, &client).await;
    let foreign_deployment = pending(&service, &foreign, &platform, &foreign_client).await;
    let untouched = platform.deploy(&app).await;
    let before = holds_count(&service, &app).await;
    client.calls.borrow_mut().clear();
    foreign_client.calls.borrow_mut().clear();
    let service = with_client(
        WorkflowService::open(store, service.policies.clone())
            .await
            .unwrap(),
        &platform,
        client.clone(),
    );
    let scope = service.fixture_app(app.clone());
    let seed = start(&scope, 1).await.remove(0);
    let publisher = Publisher::new(&app).await;
    page(&scope, &seed, &publisher, 1, JobOutcome::Waiting {}).await;
    assert!(client.calls.borrow().is_empty());
    let grant = Grant::new(&seed);
    let receipt = scope
        .reconcile_job(&grant, &publisher, options(1))
        .await
        .unwrap();
    assert_eq!(receipt.outcome, JobOutcome::Completed {});
    assert_eq!(
        intent(&service, &app, &acquiring.id)
            .await
            .text("state")
            .unwrap(),
        "held"
    );
    assert_eq!(
        client.calls.borrow().as_slice(),
        &[(acquiring.id.clone(), HoldState::Held, 1)]
    );
    assert_eq!(
        scope
            .reconcile_job(&grant.retry(), &publisher, options(1))
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(client.calls.borrow().len(), 1);
    assert_eq!(holds_count(&service, &app).await, before);
    assert_eq!(
        intent(&service, &foreign, &foreign_deployment.id)
            .await
            .text("state")
            .unwrap(),
        "acquiring"
    );
    assert!(foreign_client.calls.borrow().is_empty());
    let tx = service.begin().await.unwrap();
    assert_eq!(
        journal_count(
            &tx,
            "deployment_holds",
            json!({"app_id":app.as_str(), "deploy_id":untouched.id})
        )
        .await,
        0
    );
    tx.commit().await.unwrap();
    platform.assert_held(&app, &acquiring.id).await;

    recover_lost_release(&service, &scope, &acquiring.id, &client, &seed, &publisher).await;
    assert_eq!(holds_count(&service, &app).await, before);
}

async fn recover_lost_release(
    service: &WorkflowService,
    scope: &AppWorkflows,
    deployment: &str,
    client: &HoldClient,
    seed: &JobSpec,
    publisher: &Publisher,
) {
    let app = scope.app_id();
    client.lose_release.set(true);
    assert!(matches!(
        service
            .release_deployment_hold(app, deployment, client)
            .await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_eq!(
        intent(service, app, deployment)
            .await
            .text("state")
            .unwrap(),
        "releasing"
    );
    client.calls.borrow_mut().clear();
    page(scope, seed, publisher, 1, JobOutcome::Waiting {}).await;
    page(scope, seed, publisher, 1, JobOutcome::Completed {}).await;
    let released = intent(service, app, deployment).await;
    assert_eq!(released.text("state").unwrap(), "released");
    assert_eq!(released.integer("generation").unwrap(), 1);
    assert_eq!(
        client.calls.borrow().as_slice(),
        &[(deployment.to_owned(), HoldState::Released, 1)]
    );
    assert_eq!(
        client
            .inner
            .release(deployment, 1.try_into().unwrap())
            .await
            .unwrap()
            .state,
        HoldState::Released
    );
}

async fn pending_publication_ids(scope: &AppWorkflows) -> Vec<JobId> {
    scope
        .pending_jobs(None, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|job| job.id)
        .collect()
}

/// Start one workflow and answer the publication intent it added.
async fn arrival(scope: &AppWorkflows, before: &[JobId]) -> JobId {
    let mut added = start(scope, 1)
        .await
        .into_iter()
        .map(|job| job.id)
        .filter(|id| !before.contains(id))
        .collect::<Vec<_>>();
    assert_eq!(added.len(), 1, "one publication intent must arrive");
    added.remove(0)
}

async fn fairness(store: Rc<OrmStore>) {
    let (service, app, _, platform) = registered_service(store).await;
    let client = HoldClient::new(&platform, &app);
    let mut deployments = Vec::new();
    for _ in 0..2 {
        deployments.push(pending(&service, &app, &platform, &client).await);
    }
    deployments.sort_by(|left, right| left.id.cmp(&right.id));
    client.calls.borrow_mut().clear();
    *client.hang.borrow_mut() = Some(deployments[0].id.clone());
    let service = with_client(service, &platform, client.clone());
    let scope = service.fixture_app(app.clone());
    let jobs = start(&scope, 2).await;
    let publisher = Publisher::new(&app).await;
    let opened = pending_publication_ids(&scope).await;
    assert_eq!(opened.len(), 2, "the cycle must open over pending work");
    page(&scope, &jobs[0], &publisher, 1, JobOutcome::Waiting {}).await;
    // A publication id is derived from the work it names, so one arriving
    // mid-cycle sorts where its own content puts it: inside the window this
    // cycle bounded at its first page, or past it. Both are correct and the
    // cycle drains either way, so what is asserted here is that the window is
    // finite and the phase leaves it. The number of pages that takes is not a
    // property of the journal and reading creation order off id order is the
    // one inference `JobId::derived` refuses.
    let churned = arrival(&scope, &opened).await;
    let mut pages = 1;
    while reconciliation_phase(&service, &app).await == "publications" {
        page(&scope, &jobs[0], &publisher, 1, JobOutcome::Waiting {}).await;
        pages += 1;
        assert!(
            pages <= opened.len() + 1,
            "the publications phase must drain the window it bounded"
        );
    }
    let published = publisher.calls.borrow().clone();
    for opened in &opened {
        assert!(
            published.contains(opened),
            "a publication pending when the cycle opened must be published before the phase leaves it"
        );
    }
    assert_eq!(
        published.len(),
        pages,
        "a page of one publishes exactly one intent"
    );
    // This one arrives after the phase left, so no window can hold it and the
    // hold page below is measured against a journal that certainly has
    // publication work waiting on it.
    let waiting = arrival(&scope, &pending_publication_ids(&scope).await).await;
    assert_ne!(waiting, churned, "the two arrivals must be distinct intents");
    assert_eq!(
        scope
            .reconcile_job(&Grant::new(&jobs[0]), &publisher, options(2))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    assert_eq!(
        intent(&service, &app, &deployments[0].id)
            .await
            .text("state")
            .unwrap(),
        "acquiring"
    );
    assert_eq!(
        intent(&service, &app, &deployments[1].id)
            .await
            .text("state")
            .unwrap(),
        "held"
    );
    assert_eq!(
        client
            .calls
            .borrow()
            .iter()
            .map(|call| call.0.clone())
            .collect::<Vec<_>>(),
        deployments
            .iter()
            .map(|deployment| deployment.id.clone())
            .collect::<Vec<_>>()
    );
    assert!(
        scope
            .pending_jobs(None, 10)
            .await
            .unwrap()
            .iter()
            .any(|job| job.id == waiting),
        "new publication must wait while holds receive their turn"
    );
    assert!(
        !publisher.calls.borrow().contains(&waiting),
        "a publication arriving after the phase left is not published by the hold page"
    );
    client.hang.borrow_mut().take();
    page(&scope, &jobs[0], &publisher, 2, JobOutcome::Waiting {}).await;
    page(&scope, &jobs[0], &publisher, 2, JobOutcome::Completed {}).await;
    assert!(service
        .pending_deployment_holds(&app, None, 10)
        .await
        .unwrap()
        .is_empty());
    assert!(scope.pending_jobs(None, 10).await.unwrap().is_empty());
    assert!(
        publisher.calls.borrow().contains(&churned),
        "a publication arriving mid-cycle is published rather than lost"
    );
}

async fn authority_loss(store: Rc<OrmStore>) {
    for expire_delivery in [false, true] {
        let (service, app, _, platform) = registered_service(store.clone()).await;
        let client = HoldClient::new(&platform, &app);
        let deployment = pending(&service, &app, &platform, &client).await;
        client.calls.borrow_mut().clear();
        let service = with_client(service, &platform, client.clone());
        let scope = service.fixture_app(app.clone());
        let seed = start(&scope, 1).await.remove(0);
        let publisher = Publisher::new(&app).await;
        page(&scope, &seed, &publisher, 1, JobOutcome::Waiting {}).await;
        let (observed, release) = client.gate();
        let mut grant = Grant::new(&seed);
        if expire_delivery {
            grant.expires = Instant::now() + Duration::from_secs(1);
        }
        let mut recovery = Box::pin(scope.reconcile_job(&grant, &publisher, options(1)));
        assert!(matches!(
            futures::future::select(observed, recovery.as_mut()).await,
            Either::Left((Ok(()), _))
        ));
        if !expire_delivery {
            service
                .policies
                .fixture_install(
                    &app,
                    PolicySnapshot::lease(
                        2.try_into().unwrap(),
                        AppPolicy::default(),
                        Instant::now(),
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        assert!(recovery.await.is_err());
        assert!(
            release.send(()).is_err(),
            "exhausted original authority must cancel the hold exchange"
        );
        assert_eq!(
            intent(&service, &app, &deployment.id)
                .await
                .text("state")
                .unwrap(),
            "acquiring"
        );
        assert!(scope
            .job_receipt(&grant.delivery.job)
            .await
            .unwrap()
            .is_none());
        platform.assert_held(&app, &deployment.id).await;
        service
            .policies
            .fixture_install(&app, leased_policy(3, AppPolicy::default()))
            .unwrap();
        assert_eq!(
            scope
                .reconcile_job(&grant.retry(), &publisher, options(1))
                .await
                .unwrap()
                .outcome,
            JobOutcome::Completed {}
        );
        assert_eq!(
            client.calls.borrow().len(),
            1,
            "an interrupted reserved item is retried in a later sweep"
        );
        page(&scope, &seed, &publisher, 1, JobOutcome::Waiting {}).await;
        page(&scope, &seed, &publisher, 1, JobOutcome::Completed {}).await;
        assert_eq!(
            intent(&service, &app, &deployment.id)
                .await
                .text("state")
                .unwrap(),
            "held"
        );
        assert_eq!(
            client.calls.borrow().as_slice(),
            &[
                (deployment.id.clone(), HoldState::Held, 1),
                (deployment.id, HoldState::Held, 1)
            ]
        );
    }
}

async fn generation_race(store: Rc<OrmStore>) {
    let (service, app, _, platform) = registered_service(store).await;
    let client = HoldClient::new(&platform, &app);
    let deployment = pending(&service, &app, &platform, &client).await;
    client.calls.borrow_mut().clear();
    let service = with_client(service, &platform, client.clone());
    let scope = service.fixture_app(app.clone());
    let seed = start(&scope, 1).await.remove(0);
    let publisher = Publisher::new(&app).await;
    page(&scope, &seed, &publisher, 1, JobOutcome::Waiting {}).await;
    let (observed, release) = client.gate();
    let grant = Grant::new(&seed);
    let mut recovering = Box::pin(scope.reconcile_job(&grant, &publisher, options(1)));
    assert!(matches!(
        futures::future::select(observed, recovering.as_mut()).await,
        Either::Left((Ok(()), _))
    ));
    service
        .reconcile_deployment_hold(&app, &deployment.id, &client.inner)
        .await
        .unwrap();
    service
        .release_deployment_hold(&app, &deployment.id, &client.inner)
        .await
        .unwrap();
    client.lose_acquire.set(true);
    assert!(service
        .acquire_deployment_hold(&app, &deployment.id, &deployment.hash, client.as_ref())
        .await
        .is_err());
    assert_eq!(
        intent(&service, &app, &deployment.id)
            .await
            .integer("generation")
            .unwrap(),
        2
    );
    release.send(()).unwrap();
    assert_eq!(recovering.await.unwrap().outcome, JobOutcome::Completed {});
    let pending = intent(&service, &app, &deployment.id).await;
    assert_eq!(pending.text("state").unwrap(), "acquiring");
    assert_eq!(pending.integer("generation").unwrap(), 2);
    page(&scope, &seed, &publisher, 1, JobOutcome::Waiting {}).await;
    page(&scope, &seed, &publisher, 1, JobOutcome::Completed {}).await;
    let held = intent(&service, &app, &deployment.id).await;
    assert_eq!(held.text("state").unwrap(), "held");
    assert_eq!(held.integer("generation").unwrap(), 2);
    assert_eq!(
        client
            .calls
            .borrow()
            .iter()
            .map(|call| call.2)
            .collect::<Vec<_>>(),
        vec![1, 2, 2]
    );
}

async fn phase_substitution(store: Rc<OrmStore>) {
    let (service, app, _, platform) = registered_service(store).await;
    let client = HoldClient::new(&platform, &app);
    let mut deployments = Vec::new();
    for _ in 0..2 {
        deployments.push(pending(&service, &app, &platform, &client).await);
    }
    deployments.sort_by(|left, right| left.id.cmp(&right.id));
    client.calls.borrow_mut().clear();
    let service = with_client(service, &platform, client.clone());
    let scope = service.fixture_app(app.clone());
    let seed = start(&scope, 1).await.remove(0);
    let publisher = Publisher::new(&app).await;
    page(&scope, &seed, &publisher, 1, JobOutcome::Waiting {}).await;
    let (observed, release) = client.gate();
    let grant = Grant::new(&seed);
    let mut recovering = Box::pin(scope.reconcile_job(&grant, &publisher, options(2)));
    assert!(matches!(
        futures::future::select(observed, recovering.as_mut()).await,
        Either::Left((Ok(()), _))
    ));
    let tx = service.begin().await.unwrap();
    journal_update(
        &tx,
        "app_state",
        json!({"app_id":app.as_str()}),
        json!({"reconciliation_phase":"publications"}),
    )
    .await;
    tx.commit().await.unwrap();
    release.send(()).unwrap();
    assert!(recovering.await.is_err());
    assert_eq!(
        client.calls.borrow().len(),
        1,
        "a substituted shared scan must not authorize the next hold RPC"
    );
    assert_eq!(
        intent(&service, &app, &deployments[1].id)
            .await
            .text("state")
            .unwrap(),
        "acquiring"
    );
    assert!(scope
        .job_receipt(&grant.delivery.job)
        .await
        .unwrap()
        .is_none());
    let tx = service.begin().await.unwrap();
    journal_update(
        &tx,
        "app_state",
        json!({"app_id":app.as_str()}),
        json!({"reconciliation_phase":"deployment_holds"}),
    )
    .await;
    tx.commit().await.unwrap();
    assert_eq!(
        scope
            .reconcile_job(&grant.retry(), &publisher, options(2))
            .await
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    assert_eq!(
        client
            .calls
            .borrow()
            .iter()
            .map(|call| call.0.clone())
            .collect::<Vec<_>>(),
        deployments
            .iter()
            .map(|deployment| deployment.id.clone())
            .collect::<Vec<_>>()
    );
    assert!(service
        .pending_deployment_holds(&app, None, 10)
        .await
        .unwrap()
        .is_empty());
}
