use super::*;
use std::future::ready;
use zeroship_core::workflow_coordination::{AssignScope, AssignedScope, RegisterWorker, WorkerState};
use zeroship_workflow_manager::{
    coordinator::{self, Coordinator},
    recovery::{self, DutyKind, Recovery, ScopeState},
    DeliveryGrant,
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

/// The manager's queue in its own database, with one worker placed on the app.
struct Manager {
    _database: crate::service::tests::publication::Manager,
    coordinator: Coordinator,
    recovery: Recovery,
    worker: WorkerId,
    scope: AssignedScope,
}

impl Manager {
    async fn new(app: &AppId) -> Self {
        let database = crate::service::tests::publication::Manager::new(app).await;
        let coordinator =
            Coordinator::new(database.queue.clone(), coordinator::Options::default()).unwrap();
        // Duties fall due at once, so each Collect below is delivered on demand.
        let recovery = Recovery::new(
            database.queue.clone(),
            recovery::Options {
                interval: Duration::from_millis(1),
                ..recovery::Options::default()
            },
        )
        .unwrap();
        let worker = WorkerId::mint();
        coordinator
            .register(
                &worker,
                &RegisterWorker {
                    capacity: 1.try_into().unwrap(),
                    state: WorkerState::Ready,
                },
            )
            .await
            .unwrap();
        let assignment = coordinator
            .assign(&AssignScope {
                request_id: RequestId::mint(),
                app_id: app.clone(),
                worker_id: worker.clone(),
                expected_revision: None,
            })
            .await
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
            _database: database,
            coordinator,
            recovery,
            worker,
            scope: AssignedScope {
                app_id: assignment.app_id,
                assignment_revision: assignment.revision,
            },
        }
    }

    async fn claim(&self, expected: &JobSpec) -> DeliveryGrant {
        let grant = self
            .coordinator
            .claim_job(&self.worker, &self.scope, || ready(Ok(self.worker.clone())))
            .await
            .unwrap()
            .expect("the manager delivers its maintenance job");
        assert_eq!(&grant.delivery().job, expected);
        grant
    }

    async fn settle(&self, receipt: &crate::service::delivery::JobReceipt, grant: &DeliveryGrant) {
        self.coordinator
            .settle_job(&self.worker, &receipt.settlement(grant).unwrap(), || {
                ready(Ok(self.worker.clone()))
            })
            .await
            .unwrap();
    }

    /// Deliver one Collect page to the creator and settle it.
    async fn collect(&self, scope: &AppWorkflows) {
        let job = self
            .recovery
            .dispatch(&self.scope.app_id, DutyKind::Collect)
            .await
            .unwrap()
            .unwrap();
        let grant = self.claim(&job).await;
        let receipt = scope.collect_job(&grant, options(8)).await.unwrap();
        assert_eq!(receipt.outcome, JobOutcome::Completed {});
        self.settle(&receipt, &grant).await;
    }

    /// Deliver a closing attempt to the creator and settle its evidence.
    async fn close(&self, scope: &AppWorkflows) -> bool {
        let job = self
            .recovery
            .begin_close(&self.scope.app_id)
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
            .responsibility(&self.scope.app_id)
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
            execution(json!([{"kind":"RunCompleted", "output":"done"}])),
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
    manager.collect(&fixture.scope).await;
    assert_eq!(fixture.payload(&abandoned).await.state, "deleted");
    assert!(
        !manager.close(&fixture.scope).await,
        "the tombstone is owed its resweep"
    );
    assert_eq!(manager.state().await, ScopeState::Open);

    // The resweep window passes; the next sweep deletes once more, finally.
    fixture.expire(&abandoned).await;
    manager.collect(&fixture.scope).await;
    let purged = fixture.payload(&abandoned).await;
    assert_eq!(purged.state, "purged");
    assert_eq!(fixture.backend.calls(), [abandoned.clone(), abandoned.clone()]);
    manager.collect(&fixture.scope).await;
    assert_eq!(fixture.payload(&abandoned).await, purged);
    assert_eq!(
        fixture.backend.calls().len(),
        2,
        "collection never sweeps a final tombstone again"
    );

    assert!(manager.close(&fixture.scope).await);
    assert_eq!(manager.state().await, ScopeState::Retired);
}
