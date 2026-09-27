use super::*;
use zeroship_workflow_manager::{
    maintenance::MaintenanceAuthority,
    recovery::{self, DutyKind, Recovery, ScopeState},
    DeliveryGrant, Queue,
};

/// Every committed intent was delivered and settled by the manager.
struct Settled(AppId);
impl crate::service::publication::JobPublisher for Settled {
    fn app_id(&self) -> &AppId {
        &self.0
    }
    async fn submit(&self, job: &JobSpec) -> Result<JobSpec, WorkflowServiceError> {
        Ok(job.clone())
    }
}

/// The manager's queue in its own database, swept by the lane that owns it.
///
/// No worker is registered and no placement exists. Collection and closure are
/// sweeps, so the host that owns the journal is what claims them, and it asserts
/// its own authority rather than reading one: a claim that succeeds here with no
/// `assignments` row is that authority working.
struct Manager {
    _database: crate::service::tests::publication::Manager,
    queue: Queue,
    recovery: Recovery,
    authority: MaintenanceAuthority,
}

impl Manager {
    async fn new(app: &AppId) -> Self {
        let database = crate::service::tests::publication::Manager::new(app).await;
        // Duties fall due at once, so each Collect below is delivered on demand.
        let recovery = Recovery::new(
            database.queue.clone(),
            recovery::Options {
                interval: Duration::from_millis(1),
                ..recovery::Options::default()
            },
        )
        .unwrap();
        recovery
            .ensure(
                app,
                &zeroship_core::workflow_jobs::DeploymentId::mint(),
                1.try_into().unwrap(),
            )
            .await
            .unwrap();
        Self {
            queue: database.queue.clone(),
            _database: database,
            recovery,
            authority: MaintenanceAuthority::new(app.clone(), WorkerId::mint()),
        }
    }

    async fn claim(&self, expected: &JobSpec) -> DeliveryGrant {
        let grant = self
            .authority
            .claim(
                &self.queue,
                Ok(AppPolicy::default().max_delivery_attempts),
            )
            .await
            .unwrap()
            .expect("the manager delivers its maintenance job");
        assert_eq!(&grant.delivery().job, expected);
        grant
    }

    async fn settle(&self, receipt: &crate::service::delivery::JobReceipt, grant: &DeliveryGrant) {
        self.authority
            .settle(&self.queue, &receipt.settlement(grant).unwrap())
            .await
            .unwrap();
    }

    /// Deliver one Collect page to the creator and settle it.
    async fn collect(&self, scope: &AppWorkflows, objects: &Objects) {
        let job = self
            .recovery
            .dispatch(self.authority.app(), DutyKind::Collect)
            .await
            .unwrap()
            .unwrap();
        let grant = self.claim(&job).await;
        let receipt = scope
            .collect_job(&grant, options(8), objects)
            .await
            .unwrap();
        assert_eq!(receipt.outcome, JobOutcome::Completed {});
        self.settle(&receipt, &grant).await;
    }

    /// Deliver a closing attempt to the creator and settle its evidence.
    async fn close(&self, scope: &AppWorkflows) -> bool {
        let job = self
            .recovery
            .begin_close(self.authority.app())
            .await
            .unwrap()
            .unwrap();
        let grant = self.claim(&job).await;
        let receipt = scope.close_job(&grant).await.unwrap();
        self.settle(&receipt, &grant).await;
        let JobOutcome::Closed { drained } = receipt.outcome else {
            panic!("closure settles with closed evidence: {receipt:?}");
        };
        drained
    }

    async fn state(&self) -> ScopeState {
        self.recovery
            .responsibility(self.authority.app())
            .await
            .unwrap()
            .unwrap()
            .state
    }
}

/// An app that deleted a payload retires. Collection first leaves a tombstone
/// owed one resweep, so a closing attempt reports undrained evidence; the
/// resweep after the tombstone's window makes it final, later sweeps leave it
/// alone, and the next attempt retires the manager's responsibility.
pub(super) async fn retirement(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let app = fixture.scope.app_id().clone();
    let abandoned = fixture.stage().await;
    fixture
        .service
        .complete(
            &fixture.worker,
            &fixture.task.id,
            &fixture.task.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    for job in fixture.scope.pending_jobs(None, 100).await.unwrap() {
        fixture
            .scope
            .publish_job(&job.id, &Settled(app.clone()))
            .await
            .unwrap();
    }
    let manager = Manager::new(&app).await;
    assert!(!manager.close(&fixture.scope).await, "the upload is in preparation");
    assert_eq!(manager.state().await, ScopeState::Open);

    fixture.expire(&abandoned).await;
    manager.collect(&fixture.scope, &fixture.objects).await;
    assert_eq!(fixture.payload(&abandoned).await.state, "deleted");
    assert!(
        !manager.close(&fixture.scope).await,
        "the tombstone is owed its resweep"
    );
    assert_eq!(manager.state().await, ScopeState::Open);

    // The resweep window passes; the next sweep deletes once more, finally.
    fixture.expire(&abandoned).await;
    manager.collect(&fixture.scope, &fixture.objects).await;
    let purged = fixture.payload(&abandoned).await;
    assert_eq!(purged.state, "purged");
    assert_eq!(
        fixture.objects.deletes(),
        [abandoned.clone(), abandoned.clone()]
    );
    manager.collect(&fixture.scope, &fixture.objects).await;
    assert_eq!(fixture.payload(&abandoned).await, purged);
    assert_eq!(
        fixture.objects.deletes().len(),
        2,
        "collection never sweeps a final tombstone again"
    );

    assert!(manager.close(&fixture.scope).await);
    assert_eq!(manager.state().await, ScopeState::Retired);
}

/// Two resweeps of one tombstone race. The attempt that fenced the tombstone
/// first makes it final; the later one, which fenced the row the first left
/// in deletion, finds it purged and settles without error.
pub(super) async fn concurrent_resweep(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let app = fixture.scope.app_id().clone();
    let id = fixture.stage().await;
    fixture.expire(&id).await;
    fixture
        .scope
        .collect_job(&Grant::new(&app), options(1), &fixture.objects)
        .await
        .unwrap();
    assert_eq!(fixture.payload(&id).await.state, "deleted");
    fixture.expire(&id).await;
    let (first_entered, first_resume) = fixture.objects.gate(&id);
    let (second_entered, second_resume) = fixture.objects.gate(&id);
    let other_host = fixture.reopen().await;
    let (finished, first_done) = flume::bounded(1);
    let first = async {
        let receipt = fixture
            .scope
            .collect_job(&Grant::new(&app), options(1), &fixture.objects)
            .await;
        finished.send_async(()).await.unwrap();
        receipt
    };
    let second = async {
        first_entered.recv_async().await.unwrap();
        other_host
            .collect_job(&Grant::new(&app), options(1), &fixture.objects)
            .await
    };
    let order = async {
        second_entered.recv_async().await.unwrap();
        first_resume.send_async(()).await.unwrap();
        first_done.recv_async().await.unwrap();
        second_resume.send_async(()).await.unwrap();
    };
    let (first, second, ()) = futures::join!(first, second, order);
    assert_eq!(first.unwrap().outcome, JobOutcome::Completed {});
    assert_eq!(second.unwrap().outcome, JobOutcome::Completed {});
    assert_eq!(fixture.payload(&id).await.state, "purged");
    assert_eq!(fixture.objects.deletes(), [id.clone(), id.clone(), id]);
}

/// A final tombstone no longer counts toward the app's payload quota: once the
/// only permitted object is purged, the app can stage another.
pub(super) async fn quota(store: Rc<OrmStore>) {
    let fixture = Fixture::new(store).await;
    let app = fixture.scope.app_id().clone();
    fixture
        .service
        .policies
        .fixture_install(
            &app,
            leased_policy(
                2,
                AppPolicy {
                    max_payload_objects: 1,
                    ..AppPolicy::default()
                },
            ),
        )
        .unwrap();
    let first = fixture.stage().await;
    let stage = async || {
        fixture
            .service
            .stage_payload(
                &fixture.worker,
                &fixture.task.id,
                &fixture.task.token,
                &RequestId::mint(),
                reference(b"collect-me"),
                fixture.objects.upload(b"collect-me"),
            )
            .await
    };
    assert!(matches!(
        stage().await,
        Err(WorkflowServiceError::ResourceExhausted(_))
    ));
    for expected in ["deleted", "purged"] {
        fixture.expire(&first).await;
        fixture
            .scope
            .collect_job(&Grant::new(&app), options(1), &fixture.objects)
            .await
            .unwrap();
        assert_eq!(fixture.payload(&first).await.state, expected);
    }
    stage().await.unwrap();
}
