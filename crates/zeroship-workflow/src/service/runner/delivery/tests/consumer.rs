use super::*;
use crate::service::runner::consumer::{
    ConsumerBindings, ConsumerOptions, ConsumerScope, JobConsumer,
};
use std::collections::VecDeque;

enum Event {
    Claimed,
    Settled,
}

struct Queue {
    metadata: Metadata,
    jobs: RefCell<BTreeMap<AppId, VecDeque<Lease>>>,
    claims: RefCell<Vec<AssignedScope>>,
    events: flume::Sender<Event>,
    responses: flume::Receiver<Event>,
    claim_gate: RefCell<Option<oneshot::Receiver<()>>>,
    claim_error: RefCell<Option<WorkflowServiceError>>,
}
impl Queue {
    fn new(jobs: impl IntoIterator<Item = Lease>) -> Rc<Self> {
        let (events, responses) = flume::unbounded();
        let queue = Rc::new(Self {
            metadata: Metadata::default(),
            jobs: RefCell::new(BTreeMap::new()),
            claims: RefCell::new(Vec::new()),
            events,
            responses,
            claim_gate: RefCell::new(None),
            claim_error: RefCell::new(None),
        });
        for job in jobs {
            queue.push(job);
        }
        queue
    }
    fn push(&self, job: Lease) {
        self.jobs
            .borrow_mut()
            .entry(job.delivery.job.app_id.clone())
            .or_default()
            .push_back(job);
    }
    async fn settlements(&self, count: usize) {
        for _ in 0..count {
            while !matches!(self.responses.recv_async().await.unwrap(), Event::Settled) {}
        }
    }
}
impl JobTransport for Queue {
    type Lease = Lease;
    async fn submit(
        &self,
        _: &AssignedScope,
        _: &JobSpec,
    ) -> Result<JobSpec, WorkflowServiceError> {
        panic!("advance fixture must not publish independently")
    }
    async fn claim(&self, scope: &AssignedScope) -> Result<Option<Lease>, WorkflowServiceError> {
        self.claims.borrow_mut().push(scope.clone());
        self.events.send(Event::Claimed).unwrap();
        let gate = self.claim_gate.borrow_mut().take();
        if let Some(gate) = gate {
            let _ = gate.await;
        }
        if let Some(error) = self.claim_error.borrow_mut().take() {
            return Err(error);
        }
        Ok(self
            .jobs
            .borrow_mut()
            .get_mut(&scope.app_id)
            .and_then(VecDeque::pop_front))
    }
    async fn heartbeat(&self, lease: &Lease) -> Result<Lease, WorkflowServiceError> {
        self.metadata.heartbeat(lease).await
    }
    async fn settle(
        &self,
        settlement: &Settlement,
    ) -> Result<SettlementReceipt, WorkflowServiceError> {
        self.events.send(Event::Settled).unwrap();
        Ok(SettlementReceipt {
            job_id: settlement.delivery.job.id.clone(),
            app_id: settlement.delivery.job.app_id.clone(),
            attempt: settlement.delivery.attempt,
            outcome: settlement.outcome,
        })
    }
}
fn options(slots: usize) -> ConsumerOptions {
    ConsumerOptions {
        slots,
        max_scopes: 8,
        idle_poll: Duration::from_millis(5),
        error_backoff: Duration::from_millis(10),
        delivery: DeliveryOptions {
            execution_timeout: Duration::from_secs(10),
            operation_timeout: Duration::from_secs(1),
            retry_delay: Duration::from_millis(5),
            reconciliation: ReconciliationOptions::default(),
            collection: crate::service::collection::CollectionOptions::default(),
            fanout: crate::service::fanout::FanoutOptions::default(),
            propagation: crate::service::propagation::PropagationOptions::default(),
        },
    }
}
fn scope(fixture: &Fixture, revision: i64) -> ConsumerScope {
    ConsumerScope::new(
        fixture.app.clone(),
        AssignedScope {
            app_id: fixture.app.app_id().clone(),
            assignment_revision: revision.try_into().unwrap(),
        },
        Rc::new(Executor {
            probe: fixture.probe.clone(),
            service: fixture.service.clone(),
        }),
    )
    .unwrap()
}
fn consumer(
    queue: Rc<Queue>,
    worker: &WorkerId,
    slots: usize,
    scopes: Vec<ConsumerScope>,
) -> (JobConsumer<Queue>, ConsumerBindings) {
    let consumer = JobConsumer::new(queue, worker.clone(), options(slots)).unwrap();
    let bindings = consumer.bindings();
    bindings.replace(scopes).unwrap();
    (consumer, bindings)
}
async fn finished(future: impl Future<Output = ()>) {
    compio::time::timeout(Duration::from_secs(15), future.boxed_local())
        .await
        .expect("consumer test finished");
}

#[compio::test]
async fn only_supplied_apps_are_claimed_and_each_uses_its_own_creator_database() {
    let left = Fixture::new(AppPolicy::default()).await;
    let mut right = Fixture::new(AppPolicy::default()).await;
    let foreign = Fixture::new(AppPolicy::default()).await;
    right.lease.delivery.worker_id = left.lease.delivery.worker_id.clone();
    let queue = Queue::new([
        left.lease.clone(),
        right.lease.clone(),
        foreign.lease.clone(),
    ]);
    let (mut host, _) = consumer(
        queue.clone(),
        &left.lease.delivery.worker_id,
        1,
        vec![scope(&left, 1), scope(&right, 1)],
    );
    finished(host.run_until(queue.settlements(2))).await;
    assert_eq!(left.probe.starts.get(), 1);
    assert_eq!(right.probe.starts.get(), 1);
    assert_eq!(foreign.probe.starts.get(), 0);
    assert!(queue
        .claims
        .borrow()
        .iter()
        .all(|claim| claim.app_id != *foreign.app.app_id()));
    assert_eq!(left.task_state().await, "completed");
    assert_eq!(right.task_state().await, "completed");
    assert!(foreign
        .app
        .job_receipt(&foreign.job)
        .await
        .unwrap()
        .is_none());
}

#[compio::test]
async fn replacing_scope_cancels_and_joins_before_reusing_its_slot() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    fixture.probe.mode.set(Mode::Pending);
    let (started, running) = oneshot::channel();
    fixture.probe.started.replace(Some(started));
    let (stopping, stopped) = oneshot::channel();
    fixture.probe.stopping.replace(Some(stopping));
    let (release, gate) = oneshot::channel();
    fixture.probe.stop_gate.replace(Some(gate));
    let queue = Queue::new([fixture.lease.clone()]);
    let (mut host, bindings) = consumer(
        queue.clone(),
        &fixture.lease.delivery.worker_id,
        1,
        vec![scope(&fixture, 1)],
    );
    let (stop_host, shutdown) = oneshot::channel();
    let update = async {
        running.await.unwrap();
        let mut replacement = fixture.lease.clone();
        replacement.delivery.assignment_revision = 2.try_into().unwrap();
        replacement.delivery.attempt = 2.try_into().unwrap();
        queue.push(replacement);
        bindings.replace(vec![scope(&fixture, 2)]).unwrap();
        stopped.await.unwrap();
        assert_eq!(
            queue.claims.borrow().len(),
            1,
            "old execution still owns the slot"
        );
        assert_eq!(fixture.probe.stops.get(), 0);
        fixture.probe.mode.set(Mode::Complete);
        release.send(()).unwrap();
        queue.settlements(1).await;
        stop_host.send(()).unwrap();
    };
    finished(async {
        futures::join!(
            host.run_until(async {
                let _ = shutdown.await;
            }),
            update
        );
    })
    .await;
    assert_eq!(fixture.probe.starts.get(), 2);
    assert_eq!(fixture.probe.stops.get(), 2);
    let tx = fixture.service.begin().await.unwrap();
    let Output::Rows { rows, .. } = tx
        .database()
        .collection("__zeroship_workflow_tasks")
        .unwrap()
        .find(value!({"app_id":fixture.app.app_id().as_str()}), value!({}))
        .await
        .unwrap()
    else {
        panic!("task rows");
    };
    let mut states: Vec<_> = rows
        .iter()
        .map(|row| row["state"].as_str().unwrap())
        .collect();
    states.sort_unstable();
    assert_eq!(states, ["completed", "expired"]);
    tx.commit().await.unwrap();
    assert_eq!(queue.claims.borrow()[1].assignment_revision.get(), 2);
}

#[compio::test]
async fn policy_revocation_keeps_consumer_scope_and_joins_before_reusing_capacity() {
    let policy = AppPolicy {
        lease_ms: 1000,
        ..AppPolicy::default()
    };
    let mut first = Fixture::new(policy.clone()).await;
    let mut second = Fixture::new(policy).await;
    if first.app.app_id() > second.app.app_id() {
        std::mem::swap(&mut first, &mut second);
    }
    second.lease.delivery.worker_id = first.lease.delivery.worker_id.clone();
    first.probe.mode.set(Mode::Pending);
    let (started, running) = oneshot::channel();
    first.probe.started.replace(Some(started));
    let (stopping, stopped) = oneshot::channel();
    first.probe.stopping.replace(Some(stopping));
    let (release, gate) = oneshot::channel();
    first.probe.stop_gate.replace(Some(gate));
    let policy_binding = first
        .service
        .policies
        .current_binding(first.app.app_id())
        .unwrap();
    let queue = Queue::new([first.lease.clone(), second.lease.clone()]);
    let unchanged = scope(&first, 1);
    let (mut host, bindings) = consumer(
        queue.clone(),
        &first.lease.delivery.worker_id,
        1,
        vec![unchanged.clone(), scope(&second, 1)],
    );
    let (stop_host, shutdown) = oneshot::channel();
    let update = async {
        running.await.unwrap();
        while queue.metadata.renewals.get() == 0 {
            compio::time::sleep(Duration::from_millis(5)).await;
        }
        policy_binding.revoke().unwrap();
        stopped.await.unwrap();
        assert!(first.probe.cancels.get() > 0);
        assert_eq!(first.probe.stops.get(), 0);
        compio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            queue.claims.borrow().len(),
            1,
            "native shutdown still owns execution capacity"
        );
        assert_eq!(second.probe.starts.get(), 0);
        release.send(()).unwrap();
        queue.settlements(1).await;
        stop_host.send(()).unwrap();
    };
    finished(async {
        futures::join!(
            host.run_until(async {
                let _ = shutdown.await;
            }),
            update
        );
    })
    .await;
    assert_eq!(first.probe.starts.get(), 1);
    assert_eq!(first.probe.stops.get(), 1);
    assert_eq!(second.probe.starts.get(), 1);
    assert_eq!(second.task_state().await, "completed");
    assert_ne!(first.task_state().await, "completed");
    assert!(first.app.job_receipt(&first.job).await.unwrap().is_none());
    drop(bindings);
    drop(unchanged);
}

#[compio::test]
async fn unchanged_snapshot_does_not_cancel_execution_and_invalid_snapshot_is_atomic() {
    let fixture = Fixture::new(AppPolicy {
        lease_ms: 1000,
        ..AppPolicy::default()
    })
    .await;
    fixture.probe.mode.set(Mode::AfterCreatorRenewal);
    let (started, running) = oneshot::channel();
    fixture.probe.started.replace(Some(started));
    let queue = Queue::new([fixture.lease.clone()]);
    let original = scope(&fixture, 1);
    let (mut host, bindings) = consumer(
        queue.clone(),
        &fixture.lease.delivery.worker_id,
        1,
        vec![original.clone()],
    );
    let shutdown = async {
        running.await.unwrap();
        bindings.replace(vec![original.clone()]).unwrap();
        assert!(matches!(
            bindings.replace(vec![original.clone(), original]),
            Err(WorkflowServiceError::InvalidRequest(_))
        ));
        queue.settlements(1).await;
    };
    finished(host.run_until(shutdown)).await;
    assert!(fixture.probe.creator_renewed.get());
    assert_eq!(fixture.probe.starts.get(), 1);
    assert_eq!(fixture.task_state().await, "completed");
}

#[compio::test]
async fn cached_snapshots_cannot_restore_retired_assignment_authority() {
    for error in [
        WorkflowServiceError::PermissionDenied,
        WorkflowServiceError::Unauthenticated,
        WorkflowServiceError::Conflict("assignment retired".into()),
    ] {
        let first = Fixture::new(AppPolicy::default()).await;
        let mut second = Fixture::new(AppPolicy::default()).await;
        second.lease.delivery.worker_id = first.lease.delivery.worker_id.clone();
        let queue = Queue::new([first.lease.clone(), second.lease.clone()]);
        queue.claim_error.replace(Some(error));
        let original = scope(&first, 1);
        let survivor = scope(&second, 1);
        let (mut host, bindings) = consumer(
            queue.clone(),
            &first.lease.delivery.worker_id,
            1,
            vec![original.clone()],
        );
        let shutdown = async {
            while !original.is_retired() {
                compio::time::sleep(Duration::from_millis(1)).await;
            }
            bindings
                .replace(vec![original.clone(), survivor.clone()])
                .unwrap();
            queue.settlements(1).await;
            assert_eq!(first.probe.starts.get(), 0);
            assert_eq!(second.probe.starts.get(), 1);
            assert!(original.is_retired());

            // A host cache can outlive removal from the consumer's snapshot.
            bindings.replace(vec![survivor.clone()]).unwrap();
            bindings
                .replace(vec![original.clone(), survivor.clone()])
                .unwrap();
            let claims_before = queue.claims.borrow().len();
            while queue.claims.borrow().len() < claims_before + 2 {
                compio::time::sleep(Duration::from_millis(1)).await;
            }
            assert_eq!(
                queue
                    .claims
                    .borrow()
                    .iter()
                    .filter(|claim| &claim.app_id == first.app.app_id())
                    .count(),
                1,
                "cached retired bindings cannot claim again"
            );
            assert_eq!(first.probe.starts.get(), 0);

            // Trusted host acceptance creates a new local binding even when
            // the manager still assigns the same placement revision.
            let refreshed = scope(&first, 1);
            assert!(!refreshed.is_retired());
            bindings.replace(vec![refreshed, survivor.clone()]).unwrap();
            queue.settlements(1).await;
        };
        finished(host.run_until(shutdown)).await;
        assert_eq!(first.probe.starts.get(), 1);
        assert_eq!(first.task_state().await, "completed");
        assert!(original.is_retired());
    }
}

#[compio::test]
async fn shutdown_cancels_a_pending_claim_without_admitting_creator_work() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let queue = Queue::new([fixture.lease.clone()]);
    let (_release, gate) = oneshot::channel();
    queue.claim_gate.replace(Some(gate));
    let (mut host, _) = consumer(
        queue.clone(),
        &fixture.lease.delivery.worker_id,
        1,
        vec![scope(&fixture, 1)],
    );
    let shutdown = async {
        assert!(matches!(
            queue.responses.recv_async().await.unwrap(),
            Event::Claimed
        ));
    };
    finished(host.run_until(shutdown)).await;
    assert_eq!(fixture.probe.starts.get(), 0);
    assert!(fixture
        .app
        .job_receipt(&fixture.job)
        .await
        .unwrap()
        .is_none());
    assert_eq!(queue.jobs.borrow()[fixture.app.app_id()].len(), 1);
    finished(host.run_until(queue.settlements(1))).await;
    assert_eq!(fixture.probe.starts.get(), 1);
}

#[compio::test]
async fn a_stalled_claim_times_out_and_releases_capacity_for_another_app() {
    let mut first = Fixture::new(AppPolicy::default()).await;
    let mut second = Fixture::new(AppPolicy::default()).await;
    if first.app.app_id() > second.app.app_id() {
        std::mem::swap(&mut first, &mut second);
    }
    second.lease.delivery.worker_id = first.lease.delivery.worker_id.clone();
    let queue = Queue::new([first.lease.clone(), second.lease.clone()]);
    let (_release, gate) = oneshot::channel();
    queue.claim_gate.replace(Some(gate));
    let mut bounds = options(1);
    bounds.delivery.operation_timeout = Duration::from_millis(50);
    bounds.error_backoff = Duration::from_millis(100);
    let mut host = JobConsumer::new(
        queue.clone(),
        first.lease.delivery.worker_id.clone(),
        bounds,
    )
    .unwrap();
    host.bindings()
        .replace(vec![scope(&first, 1), scope(&second, 1)])
        .unwrap();
    finished(host.run_until(queue.settlements(1))).await;
    assert_eq!(first.probe.starts.get(), 0);
    assert_eq!(second.probe.starts.get(), 1);
    assert_eq!(queue.claims.borrow()[0].app_id, *first.app.app_id());
    assert_eq!(queue.claims.borrow()[1].app_id, *second.app.app_id());
    assert!(first.app.job_receipt(&first.job).await.unwrap().is_none());
    // Timeout released the per-app reservation; a later attempt can proceed.
    finished(host.run_until(queue.settlements(1))).await;
    assert_eq!(first.probe.starts.get(), 1);
    assert_eq!(first.task_state().await, "completed");
}

#[compio::test]
async fn exhausted_manager_grant_cannot_admit_a_creator_task() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let mut expired = fixture.lease.clone();
    expired.expires = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    let queue = Queue::new([expired]);
    let (mut host, _) = consumer(
        queue.clone(),
        &fixture.lease.delivery.worker_id,
        1,
        vec![scope(&fixture, 1)],
    );
    let shutdown = async {
        queue.responses.recv_async().await.unwrap();
        // Backoff must return control to the host while the grant remains unusable.
        compio::time::sleep(Duration::from_millis(1)).await;
    };
    finished(host.run_until(shutdown)).await;
    assert_eq!(queue.claims.borrow().len(), 1);
    assert_eq!(fixture.probe.starts.get(), 0);
    assert!(fixture
        .app
        .job_receipt(&fixture.job)
        .await
        .unwrap()
        .is_none());
}

#[compio::test]
async fn substituted_delivery_revokes_scope_before_creator_acceptance() {
    for substitution in ["worker", "revision", "app"] {
        let fixture = Fixture::new(AppPolicy::default()).await;
        let mut lease = fixture.lease.clone();
        match substitution {
            "worker" => lease.delivery.worker_id = WorkerId::mint(),
            "revision" => lease.delivery.assignment_revision = 2.try_into().unwrap(),
            "app" => lease.delivery.job.app_id = AppId::mint(),
            _ => unreachable!(),
        }
        let queue = Queue::new([]);
        queue
            .jobs
            .borrow_mut()
            .insert(fixture.app.app_id().clone(), [lease].into());
        let (mut host, _) = consumer(
            queue.clone(),
            &fixture.lease.delivery.worker_id,
            1,
            vec![scope(&fixture, 1)],
        );
        let shutdown = async {
            queue.responses.recv_async().await.unwrap();
            compio::time::sleep(Duration::from_millis(30)).await;
        };
        finished(host.run_until(shutdown)).await;
        assert_eq!(queue.claims.borrow().len(), 1);
        assert_eq!(fixture.probe.starts.get(), 0);
        assert!(fixture
            .app
            .job_receipt(&fixture.job)
            .await
            .unwrap()
            .is_none());
    }
}

#[compio::test]
async fn capacity_is_shared_across_apps_and_busy_apps_do_not_starve_other_scopes() {
    let worker = WorkerId::mint();
    let mut fixtures = Vec::new();
    for _ in 0..3 {
        let mut fixture = Fixture::new(AppPolicy::default()).await;
        fixture.lease.delivery.worker_id = worker.clone();
        fixture.probe.mode.set(Mode::Pending);
        fixtures.push(fixture);
    }
    fixtures.sort_by(|left, right| left.app.app_id().cmp(right.app.app_id()));
    let mut running = Vec::new();
    for fixture in &fixtures {
        let (started, receiver) = oneshot::channel();
        fixture.probe.started.replace(Some(started));
        running.push(receiver);
    }
    let queue = Queue::new(fixtures.iter().map(|fixture| fixture.lease.clone()));
    let scopes: Vec<_> = fixtures.iter().map(|fixture| scope(fixture, 1)).collect();
    let (mut host, bindings) = consumer(queue.clone(), &worker, 2, scopes.clone());
    let shutdown = async {
        running.remove(0).await.unwrap();
        running.remove(0).await.unwrap();
        assert_eq!(queue.claims.borrow().len(), 2);
        assert_eq!(fixtures[2].probe.starts.get(), 0);
        bindings.replace(scopes[1..].to_vec()).unwrap();
        running.remove(0).await.unwrap();
        assert_eq!(fixtures[0].probe.stops.get(), 1);
        assert_eq!(fixtures[1].probe.stops.get(), 0);
        assert_eq!(queue.claims.borrow()[2].app_id, *fixtures[2].app.app_id());
    };
    finished(host.run_until(shutdown)).await;
    assert!(fixtures
        .iter()
        .all(|fixture| fixture.probe.stops.get() == 1));
    assert!(fixtures
        .iter()
        .all(|fixture| fixture.probe.starts.get() == 1));
}

#[compio::test]
async fn abandoned_host_retains_active_execution_until_drain_finishes() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    fixture.probe.mode.set(Mode::Pending);
    let (started, running) = oneshot::channel();
    fixture.probe.started.replace(Some(started));
    let (stopping, stopped) = oneshot::channel();
    fixture.probe.stopping.replace(Some(stopping));
    let (release, gate) = oneshot::channel();
    fixture.probe.stop_gate.replace(Some(gate));
    let queue = Queue::new([fixture.lease.clone()]);
    let (mut host, _) = consumer(
        queue.clone(),
        &fixture.lease.delivery.worker_id,
        1,
        vec![scope(&fixture, 1)],
    );
    let task = host.run_until(std::future::pending()).boxed_local();
    let Either::Left((Ok(()), task)) = futures::future::select(running, task).await else {
        panic!("host must remain active");
    };
    drop(task);
    assert!(fixture.probe.cancels.get() > 0);
    assert_eq!(fixture.probe.stops.get(), 0);
    let drain = host.drain().boxed_local();
    let Either::Left((Ok(()), drain)) = futures::future::select(stopped, drain).await else {
        panic!("drain must join execution");
    };
    assert_eq!(queue.claims.borrow().len(), 1);
    release.send(()).unwrap();
    finished(drain).await;
    assert_eq!(fixture.probe.stops.get(), 1);
    assert_eq!(fixture.task_state().await, "released");
}

struct NativeManager {
    database: crate::service::tests::publication::Manager,
    coordinator: zeroship_workflow_manager::coordinator::Coordinator,
    worker: WorkerId,
    scope: AssignedScope,
    lose_ack: Cell<bool>,
    requests: RefCell<Vec<Settlement>>,
    settled: flume::Sender<()>,
    completion: flume::Receiver<()>,
}

impl NativeManager {
    async fn new(fixture: &Fixture) -> Rc<Self> {
        use zeroship_core::workflow_coordination::{AssignScope, RegisterWorker, WorkerState};
        let database = crate::service::tests::publication::Manager::new(fixture.app.app_id()).await;
        let coordinator = zeroship_workflow_manager::coordinator::Coordinator::new(
            database.queue.clone(),
            zeroship_workflow_manager::coordinator::Options::default(),
        )
        .unwrap();
        let worker = fixture.lease.delivery.worker_id.clone();
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
                app_id: fixture.app.app_id().clone(),
                worker_id: worker.clone(),
                expected_revision: None,
            })
            .await
            .unwrap();
        let (settled, completion) = flume::bounded(1);
        Rc::new(Self {
            database,
            coordinator,
            worker,
            scope: AssignedScope {
                app_id: assignment.app_id,
                assignment_revision: assignment.revision,
            },
            lose_ack: Cell::new(true),
            requests: RefCell::new(Vec::new()),
            settled,
            completion,
        })
    }
}

fn manager_error(error: zeroship_workflow_manager::Error) -> WorkflowServiceError {
    WorkflowServiceError::Unavailable(error.to_string())
}

impl JobTransport for NativeManager {
    type Lease = zeroship_workflow_manager::DeliveryGrant;
    async fn submit(
        &self,
        scope: &AssignedScope,
        job: &JobSpec,
    ) -> Result<JobSpec, WorkflowServiceError> {
        self.coordinator
            .submit_job(
                &self.worker,
                &SubmitJob {
                    scope: scope.clone(),
                    job: job.clone(),
                },
                || async { Ok(self.worker.clone()) },
            )
            .await
            .map_err(manager_error)
    }
    async fn claim(
        &self,
        scope: &AssignedScope,
    ) -> Result<Option<Self::Lease>, WorkflowServiceError> {
        self.coordinator
            .claim_job(&self.worker, scope, || async { Ok(self.worker.clone()) })
            .await
            .map_err(manager_error)
    }
    async fn heartbeat(&self, lease: &Self::Lease) -> Result<Self::Lease, WorkflowServiceError> {
        self.coordinator
            .heartbeat_job(&self.worker, lease.delivery(), || async {
                Ok(self.worker.clone())
            })
            .await
            .map_err(manager_error)
    }
    async fn settle(
        &self,
        request: &Settlement,
    ) -> Result<SettlementReceipt, WorkflowServiceError> {
        self.requests.borrow_mut().push(request.clone());
        let receipt = self
            .coordinator
            .settle_job(&self.worker, request, || async { Ok(self.worker.clone()) })
            .await
            .map_err(manager_error)?;
        if self.lose_ack.replace(false) {
            return Err(WorkflowServiceError::Timeout);
        }
        self.settled.send(()).unwrap();
        Ok(receipt)
    }
}

impl crate::service::publication::JobPublisher for NativeManager {
    fn app_id(&self) -> &AppId {
        &self.scope.app_id
    }
    async fn submit(&self, job: &JobSpec) -> Result<JobSpec, WorkflowServiceError> {
        self.coordinator
            .submit_job(
                &self.worker,
                &zeroship_core::workflow_jobs::SubmitJob {
                    scope: self.scope.clone(),
                    job: job.clone(),
                },
                || async { Ok(self.worker.clone()) },
            )
            .await
            .map_err(manager_error)
    }
}

#[compio::test]
async fn native_manager_delivery_and_lost_ack_finish_through_separate_orm_databases() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let manager = NativeManager::new(&fixture).await;
    fixture
        .app
        .publish_job(&fixture.job.id, manager.as_ref())
        .await
        .unwrap();
    let mut consumer =
        JobConsumer::new(manager.clone(), manager.worker.clone(), options(1)).unwrap();
    consumer
        .bindings()
        .replace(vec![scope(
            &fixture,
            manager.scope.assignment_revision.get(),
        )])
        .unwrap();
    finished(consumer.run_until(async {
        manager.completion.recv_async().await.unwrap();
    }))
    .await;
    assert_eq!(fixture.probe.starts.get(), 1);
    assert_eq!(fixture.probe.stops.get(), 1);
    assert_eq!(fixture.task_state().await, "completed");
    assert_eq!(
        fixture
            .app
            .job_receipt(&fixture.job)
            .await
            .unwrap()
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    {
        let requests = manager.requests.borrow();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], requests[1]);
    }
    assert!(manager.claim(&manager.scope).await.unwrap().is_none());
    assert!(fixture.app.pending_jobs(None, 1).await.unwrap().is_empty());
}

#[compio::test]
async fn manager_collect_duty_settles_without_publishing_or_executing_creator_work() {
    let mut fixture = Fixture::new(AppPolicy::default()).await;
    super::collection::attach_storage(&mut fixture);
    assert!(!super::collection::has_task(&fixture).await);
    let manager = NativeManager::new(&fixture).await;
    let recovery = zeroship_workflow_manager::recovery::Recovery::new(
        manager.database.queue.clone(),
        zeroship_workflow_manager::recovery::Options::default(),
    )
    .unwrap();
    recovery
        .ensure(
            fixture.app.app_id(),
            fixture.job.deployment_id().unwrap(),
            1.try_into().unwrap(),
        )
        .await
        .unwrap();
    let collect = recovery
        .dispatch(
            fixture.app.app_id(),
            zeroship_workflow_manager::recovery::DutyKind::Collect,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(collect.deployment_id().is_none());
    let mut consumer =
        JobConsumer::new(manager.clone(), manager.worker.clone(), options(1)).unwrap();
    consumer
        .bindings()
        .replace(vec![scope(
            &fixture,
            manager.scope.assignment_revision.get(),
        )])
        .unwrap();
    finished(consumer.run_until(async {
        manager.completion.recv_async().await.unwrap();
    }))
    .await;
    assert_eq!(fixture.probe.starts.get(), 0);
    assert!(!super::collection::has_task(&fixture).await);
    assert_eq!(
        fixture
            .app
            .job_receipt(&collect)
            .await
            .unwrap()
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    assert_eq!(
        fixture.app.pending_jobs(None, 1).await.unwrap(),
        [fixture.job.clone()]
    );
    assert!(manager.claim(&manager.scope).await.unwrap().is_none());
    let requests = manager.requests.borrow();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    assert_eq!(requests[0].delivery.job, collect);
    assert!(requests[0].successors.is_empty());
}

#[compio::test]
async fn manager_delivers_committed_fanout_publication_without_executor() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let fanout = super::fanout::accepted(&fixture).await;
    assert!(fanout.deployment_id().is_none());
    let manager = NativeManager::new(&fixture).await;
    fixture
        .app
        .publish_job(&fanout.id, manager.as_ref())
        .await
        .unwrap();
    let mut consumer =
        JobConsumer::new(manager.clone(), manager.worker.clone(), options(1)).unwrap();
    consumer
        .bindings()
        .replace(vec![scope(
            &fixture,
            manager.scope.assignment_revision.get(),
        )])
        .unwrap();
    finished(consumer.run_until(async {
        manager.completion.recv_async().await.unwrap();
    }))
    .await;
    assert_eq!(fixture.probe.starts.get(), 0);
    assert!(!super::collection::has_task(&fixture).await);
    assert_eq!(
        fixture
            .app
            .job_receipt(&fanout)
            .await
            .unwrap()
            .unwrap()
            .outcome,
        JobOutcome::Completed {}
    );
    assert_eq!(
        fixture.app.pending_jobs(None, 1).await.unwrap(),
        std::slice::from_ref(&fixture.job)
    );
    assert!(manager.claim(&manager.scope).await.unwrap().is_none());
    let requests = manager.requests.borrow();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    assert_eq!(requests[0].delivery.job, fanout);
}

#[compio::test]
async fn manager_delivers_committed_propagation_page_without_executor() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let (page, child) = super::propagation::cascade(&fixture).await;
    assert!(page.deployment_id().is_none());
    let manager = NativeManager::new(&fixture).await;
    fixture
        .app
        .publish_job(&page.id, manager.as_ref())
        .await
        .unwrap();
    let mut consumer =
        JobConsumer::new(manager.clone(), manager.worker.clone(), options(1)).unwrap();
    consumer
        .bindings()
        .replace(vec![scope(
            &fixture,
            manager.scope.assignment_revision.get(),
        )])
        .unwrap();
    finished(consumer.run_until(async {
        manager.completion.recv_async().await.unwrap();
    }))
    .await;
    assert_eq!(fixture.probe.starts.get(), 0);
    assert_eq!(
        super::propagation::control(&fixture, &child).await,
        "cancel"
    );
    let receipt = fixture.app.job_receipt(&page).await.unwrap().unwrap();
    assert_eq!(receipt.outcome, JobOutcome::Completed {});
    // The page's successor is the child's committed Advance intent at its new
    // frontier, which the creator outbox publishes independently of settlement.
    assert!(fixture
        .app
        .pending_jobs(None, 100)
        .await
        .unwrap()
        .iter()
        .any(
            |job| matches!(&job.operation, JobOperation::Advance { run_id, revision, .. }
            if run_id.as_str() == child && revision.get() == 2)
        ));
    assert!(manager.claim(&manager.scope).await.unwrap().is_none());
    let requests = manager.requests.borrow();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    assert_eq!(requests[0].delivery.job, page);
    assert!(requests[0].successors.is_empty());
}

#[compio::test]
async fn manager_reconciliation_publishes_creator_work_before_the_consumer_executes_it() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let manager = NativeManager::new(&fixture).await;
    let recovery = zeroship_workflow_manager::recovery::Recovery::new(
        manager.database.queue.clone(),
        zeroship_workflow_manager::recovery::Options::default(),
    )
    .unwrap();
    recovery
        .ensure(
            fixture.app.app_id(),
            fixture.job.deployment_id().unwrap(),
            1.try_into().unwrap(),
        )
        .await
        .unwrap();
    let reconciliation = recovery
        .dispatch(
            fixture.app.app_id(),
            zeroship_workflow_manager::recovery::DutyKind::Reconcile,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(reconciliation.deployment_id().is_none());
    let mut consumer =
        JobConsumer::new(manager.clone(), manager.worker.clone(), options(1)).unwrap();
    consumer
        .bindings()
        .replace(vec![scope(
            &fixture,
            manager.scope.assignment_revision.get(),
        )])
        .unwrap();
    finished(consumer.run_until(async {
        manager.completion.recv_async().await.unwrap();
        manager.completion.recv_async().await.unwrap();
    }))
    .await;
    assert_eq!(
        fixture.probe.starts.get(),
        1,
        "reconciliation must not load or execute app code"
    );
    assert_eq!(fixture.task_state().await, "completed");
    assert_eq!(
        fixture
            .app
            .job_receipt(&reconciliation)
            .await
            .unwrap()
            .unwrap()
            .outcome,
        JobOutcome::Waiting {}
    );
    assert!(fixture.app.pending_jobs(None, 1).await.unwrap().is_empty());
    assert!(manager.claim(&manager.scope).await.unwrap().is_none());
    let requests = manager.requests.borrow();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0], requests[1]);
    assert_eq!(requests[0].delivery.job.id, reconciliation.id);
    assert_eq!(requests[2].delivery.job.id, fixture.job.id);
}

/// The platform policy source composed beside the native manager.
#[derive(Debug)]
struct Policies(zeroship_workflow_manager::policy::PolicyObservation);

impl zeroship_workflow_manager::policy::PolicySource for Policies {
    fn observe<'a>(
        &'a self,
        _: &'a AppId,
    ) -> std::pin::Pin<
        Box<
            dyn Future<
                    Output = Result<
                        zeroship_workflow_manager::policy::PolicyObservation,
                        zeroship_workflow_manager::Error,
                    >,
                > + 'a,
        >,
    > {
        Box::pin(std::future::ready(Ok(self.0.clone())))
    }

    fn revalidate(
        &self,
        observation: &zeroship_workflow_manager::policy::PolicyObservation,
    ) -> Result<Instant, zeroship_workflow_manager::Error> {
        Ok(observation.expires_at())
    }
}

impl Policies {
    fn new(app: &AppId) -> Self {
        Self(
            zeroship_workflow_manager::policy::PolicyObservation::new(
                app.clone(),
                Revision::try_from(1).unwrap(),
                AppPolicy::default(),
                Instant::now() + Duration::from_secs(600),
            )
            .unwrap(),
        )
    }
}

impl NativeManager {
    /// The worker host's policy exchange; `after` names the epoch it was refused.
    async fn establish(&self, source: &Policies, after: Option<i64>) -> Option<Revision> {
        let request = zeroship_core::workflow_policy::PolicyLeaseRequest {
            scope: self.scope.clone(),
            establish_after: after.map(|epoch| Revision::try_from(epoch).unwrap()),
            ingress_used: true,
        };
        self.coordinator
            .policy_lease(&self.worker, "enrolled-key", &request, source, || async {
                Ok(self.worker.clone())
            })
            .await
            .unwrap()
            .lease()
            .unwrap()
            .ingress_epoch
    }

    async fn settle_directly(&self, settlement: &Settlement) -> SettlementReceipt {
        self.coordinator
            .settle_job(&self.worker, settlement, || async { Ok(self.worker.clone()) })
            .await
            .unwrap()
    }
}

/// Installs the manager's epoch beside the policy, as the host's refresh does.
fn install_epoch(fixture: &Fixture, epoch: Option<Revision>) {
    fixture
        .service
        .policies
        .fixture_install(
            fixture.app.app_id(),
            PolicySnapshot::configuration(Revision::try_from(1).unwrap(), AppPolicy::default())
                .unwrap()
                .with_ingress_epoch(epoch),
        )
        .unwrap();
}

/// Intents whose jobs the manager already delivered and settled.
struct Settled(AppId);
impl crate::service::publication::JobPublisher for Settled {
    fn app_id(&self) -> &AppId {
        &self.0
    }
    async fn submit(&self, job: &JobSpec) -> Result<JobSpec, WorkflowServiceError> {
        Ok(job.clone())
    }
}

fn recovery(manager: &NativeManager) -> zeroship_workflow_manager::recovery::Recovery {
    zeroship_workflow_manager::recovery::Recovery::new(
        manager.database.queue.clone(),
        zeroship_workflow_manager::recovery::Options::default(),
    )
    .unwrap()
}

/// Close delivered to a creator journal fences ingress under a still-valid
/// epoch, and a propagation page claimed after closing began commits intents
/// after that fence. Its dispatch ticket above the closing watermark keeps the
/// manager's responsibility open over separate creator and manager databases.
#[compio::test]
async fn closing_watermark_keeps_late_delivered_intents_across_separate_databases() {
    use zeroship_workflow_manager::recovery::{DutyKind, ScopeState};
    let fixture = Fixture::new(AppPolicy::default()).await;
    let app = fixture.app.app_id().clone();
    let (page, _child) = super::propagation::cascade(&fixture).await;
    let manager = NativeManager::new(&fixture).await;
    let recovery = recovery(&manager);
    recovery
        .ensure(
            &app,
            fixture.job.deployment_id().unwrap(),
            Revision::try_from(1).unwrap(),
        )
        .await
        .unwrap();
    let source = Policies::new(&app);
    let epoch = manager.establish(&source, None).await;
    assert_eq!(epoch, Some(Revision::try_from(1).unwrap()));
    install_epoch(&fixture, epoch);
    for job in fixture.app.pending_jobs(None, 100).await.unwrap() {
        if job.id == page.id {
            fixture
                .app
                .publish_job(&job.id, manager.as_ref())
                .await
                .unwrap();
        } else {
            fixture
                .app
                .publish_job(&job.id, &Settled(app.clone()))
                .await
                .unwrap();
        }
    }
    assert!(fixture.app.pending_jobs(None, 1).await.unwrap().is_empty());

    let close = recovery.begin_close(&app).await.unwrap().unwrap();
    let page_grant = manager.claim(&manager.scope).await.unwrap().unwrap();
    assert_eq!(page_grant.delivery().job, page);
    let close_grant = manager.claim(&manager.scope).await.unwrap().unwrap();
    assert_eq!(close_grant.delivery().job, close);
    let closed = fixture.app.close_job(&close_grant).await.unwrap();
    assert_eq!(
        closed.outcome,
        JobOutcome::Closed { drained: true },
        "every intent was confirmed when the fence committed"
    );
    // Delivered work runs under delivery authority, not the ingress fence.
    let applied = fixture
        .app
        .propagation_job(
            &page_grant,
            crate::service::propagation::PropagationOptions::default(),
        )
        .await
        .unwrap();
    let late = fixture.app.pending_jobs(None, 100).await.unwrap();
    assert!(!late.is_empty(), "the page committed intents after the fence");
    manager
        .settle_directly(&applied.settlement(&page_grant).unwrap())
        .await;
    manager
        .settle_directly(&closed.settlement(&close_grant).unwrap())
        .await;
    let kept = recovery.responsibility(&app).await.unwrap().unwrap();
    assert_eq!(
        kept.state,
        ScopeState::Open,
        "retired while the creator journal holds {} unconfirmed intents",
        late.len()
    );
    assert!(recovery
        .dispatch(&app, DutyKind::Reconcile)
        .await
        .unwrap()
        .is_some());

    // The creator refuses the still-valid epoch; the host establishes the next.
    assert_eq!(
        fixture
            .app
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await
            .unwrap_err(),
        WorkflowServiceError::IngressFenced(Some(Revision::try_from(1).unwrap()))
    );
    let next = manager.establish(&source, Some(1)).await;
    assert_eq!(next, Some(Revision::try_from(2).unwrap()));
    assert_eq!(manager.establish(&source, Some(1)).await, next);
    install_epoch(&fixture, next);
    fixture
        .app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    assert_eq!(
        recovery
            .responsibility(&app)
            .await
            .unwrap()
            .unwrap()
            .ingress_epoch,
        Revision::try_from(2).unwrap()
    );
}

/// The consumer dispatches Close to the creator handler. A lost settlement
/// acknowledgement is retried with the identical settlement, and the manager
/// retires exactly once. A crashed first attempt redelivered to another worker
/// replays the committed creator receipt.
#[compio::test]
async fn consumer_closes_retires_once_after_lost_ack_and_redelivery() {
    use zeroship_workflow_manager::recovery::ScopeState;
    let fixture = Fixture::new(AppPolicy::default()).await;
    let app = fixture.app.app_id().clone();
    let manager = NativeManager::new(&fixture).await;
    let recovery = recovery(&manager);
    recovery
        .ensure(
            &app,
            fixture.job.deployment_id().unwrap(),
            Revision::try_from(1).unwrap(),
        )
        .await
        .unwrap();
    install_epoch(&fixture, manager.establish(&Policies::new(&app), None).await);
    for job in fixture.app.pending_jobs(None, 100).await.unwrap() {
        fixture
            .app
            .publish_job(&job.id, &Settled(app.clone()))
            .await
            .unwrap();
    }
    let close = recovery.begin_close(&app).await.unwrap().unwrap();
    // A first worker commits the creator receipt, then crashes before settling.
    let crashed = manager.claim(&manager.scope).await.unwrap().unwrap();
    assert_eq!(crashed.delivery().job, close);
    let committed = fixture.app.close_job(&crashed).await.unwrap();
    assert_eq!(committed.outcome, JobOutcome::Closed { drained: true });
    let expire = rusqlite::Connection::open(&manager.database.path).unwrap();
    assert_eq!(
        expire
            .execute(
                "UPDATE jobs SET lease_deadline=0 WHERE id=?1",
                [close.id.as_str()],
            )
            .unwrap(),
        1
    );
    let mut consumer =
        JobConsumer::new(manager.clone(), manager.worker.clone(), options(1)).unwrap();
    consumer
        .bindings()
        .replace(vec![scope(
            &fixture,
            manager.scope.assignment_revision.get(),
        )])
        .unwrap();
    finished(consumer.run_until(async {
        manager.completion.recv_async().await.unwrap();
    }))
    .await;
    {
        let requests = manager.requests.borrow();
        assert_eq!(requests.len(), 2, "the lost acknowledgement was retried");
        assert_eq!(requests[0], requests[1]);
        assert_eq!(requests[0].delivery.job, close);
        assert_eq!(requests[0].delivery.attempt.get(), 2);
        assert_eq!(requests[0].outcome, committed.outcome);
    }
    assert_eq!(fixture.probe.starts.get(), 0, "closure runs no app code");
    assert_eq!(
        fixture.app.job_receipt(&close).await.unwrap(),
        Some(committed)
    );
    let retired = recovery.responsibility(&app).await.unwrap().unwrap();
    assert_eq!(retired.state, ScopeState::Retired);
    assert_eq!(retired.ingress_epoch, Revision::try_from(1).unwrap());
    assert!(manager.claim(&manager.scope).await.unwrap().is_none());
}
