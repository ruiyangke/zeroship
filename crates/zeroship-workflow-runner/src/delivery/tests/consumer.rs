use super::*;
use crate::consumer::{ConsumerBindings, ConsumerOptions, ConsumerScope, JobConsumer};
use crate::PayloadObjects;
use std::collections::VecDeque;
use zeroship_core::workflow_jobs::DeploymentId;

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
    claim_gates: RefCell<VecDeque<oneshot::Receiver<()>>>,
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
            claim_gates: RefCell::new(VecDeque::new()),
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
    /// Asked of the journal this host holds, the way the crossed transport asks
    /// the service that holds it.
    async fn release(
        &self,
        journal: &Self::Journal,
        lease: &Self::Lease,
        task: &zeroship_workflow::service::delivery::DeliveredTask,
    ) -> Result<(), WorkflowServiceError> {
        journal.release_job(task, lease).await
    }
    async fn receipt(
        &self,
        journal: &Self::Journal,
        job: &zeroship_core::workflow_jobs::JobSpec,
    ) -> Result<Option<zeroship_workflow::service::delivery::JobReceipt>, WorkflowServiceError> {
        journal.job_receipt(job).await
    }
    type Lease = Lease;
    /// This host holds the journal, so an attempt is scoped here rather than
    /// server-side.
    type Journal = AppWorkflows;
    fn scope(
        &self,
        journal: &Self::Journal,
        authority: &zeroship_workflow::service::PolicyAuthority,
    ) -> Result<Self::Journal, WorkflowServiceError> {
        scope_journal(journal, authority)
    }
    async fn claim(
        &self,
        journal: &AppWorkflows,
        scope: &AssignedScope,
    ) -> Result<Option<Claimed<Lease>>, WorkflowServiceError> {
        self.claims.borrow_mut().push(scope.clone());
        self.events.send(Event::Claimed).unwrap();
        let gate = self.claim_gates.borrow_mut().pop_front();
        if let Some(gate) = gate {
            let _ = gate.await;
        }
        if let Some(error) = self.claim_error.borrow_mut().take() {
            return Err(error);
        }
        let Some(lease) = self
            .jobs
            .borrow_mut()
            .get_mut(&scope.app_id)
            .and_then(VecDeque::pop_front)
        else {
            return Ok(None);
        };
        let accepted = if lease.delivery().job.operation.accepts_execution() {
            Some(journal.accept_job(&lease).await?)
        } else {
            None
        };
        Ok(Some(Claimed { lease, accepted }))
    }
    async fn heartbeat(
        &self,
        journal: &AppWorkflows,
        lease: &Lease,
        task: &DeliveredTask,
    ) -> Result<Renewed<Lease>, WorkflowServiceError> {
        self.metadata.heartbeat(journal, lease, task).await
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
            outcome: settlement.outcome.clone(),
        })
    }
    async fn complete(
        &self,
        journal: &AppWorkflows,
        lease: &Lease,
        task: &DeliveredTask,
        execution: WorkflowExecution,
        confirmed: Vec<zeroship_workflow::service::delivery::PayloadConfirmation>,
    ) -> Result<Completed, WorkflowServiceError> {
        assert!(confirmed.is_empty(), "an in-process store confirms its own uploads");
        let receipt = journal.complete_job(task, lease, execution).await?;
        let settlement = receipt.settlement(lease)?;
        Ok(Completed {
            settlement: JobTransport::settle(self, &settlement).await?,
            receipt,
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
        },
    }
}
fn scope(fixture: &Fixture, revision: i64) -> ConsumerScope<AppWorkflows> {
    ConsumerScope::new(
        fixture.app.clone(),
        fixture.app.binding().clone(),
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
    scopes: Vec<ConsumerScope<AppWorkflows>>,
) -> (JobConsumer<Queue>, ConsumerBindings<AppWorkflows>) {
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
        .policies()
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
    queue.claim_gates.borrow_mut().push_back(gate);
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
    // Every claim stalls, and the operation timeout is what releases the stall,
    // so the release is observed on the queue's claim stream. Waiting for a
    // later settlement instead would tie the assertion to the same bound that
    // commits an execution, and under load that commit can outlive the bound
    // the stall is measured against.
    let (_release_first, first_gate) = oneshot::channel();
    let (_release_second, second_gate) = oneshot::channel();
    let (_release_retry, retry_gate) = oneshot::channel();
    queue
        .claim_gates
        .borrow_mut()
        .extend([first_gate, second_gate, retry_gate]);
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
    // The stalled first claim times out, the released capacity is offered to
    // the second app, and the first app's per-app reservation is released for a
    // later attempt. Each is a claim the host issues, and each is observed
    // before this host is stopped.
    let released = async {
        for _ in 0..3 {
            assert!(matches!(
                queue.responses.recv_async().await.unwrap(),
                Event::Claimed
            ));
        }
    };
    finished(host.run_until(released)).await;
    assert_eq!(first.probe.starts.get(), 0);
    assert_eq!(second.probe.starts.get(), 0);
    assert_eq!(queue.claims.borrow().len(), 3);
    assert_eq!(queue.claims.borrow()[0].app_id, *first.app.app_id());
    assert_eq!(queue.claims.borrow()[1].app_id, *second.app.app_id());
    assert_eq!(queue.claims.borrow()[2].app_id, *first.app.app_id());
    assert!(first.app.job_receipt(&first.job).await.unwrap().is_none());
    // No claim popped a job while stalled, so a host under ordinary bounds runs
    // both apps to completion.
    drop(host);
    let mut host = JobConsumer::new(
        queue.clone(),
        first.lease.delivery.worker_id.clone(),
        options(1),
    )
    .unwrap();
    host.bindings()
        .replace(vec![scope(&first, 1), scope(&second, 1)])
        .unwrap();
    finished(host.run_until(queue.settlements(2))).await;
    assert_eq!(first.probe.starts.get(), 1);
    assert_eq!(second.probe.starts.get(), 1);
    assert_eq!(first.task_state().await, "completed");
    assert_eq!(second.task_state().await, "completed");
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
    database: crate::manager_queue::Manager,
    coordinator: zeroship_workflow_manager::coordinator::Coordinator,
    worker: WorkerId,
    scope: AssignedScope,
    lose_ack: Cell<bool>,
    requests: RefCell<Vec<Settlement>>,
    settled: flume::Sender<()>,
    completion: flume::Receiver<()>,
}

impl NativeManager {
    /// Take the next maintenance row of this app's queue as the lane that owns
    /// it, and run it over the fixture's journal.
    ///
    /// Claim through the maintenance lane, under this fixture's own worker id.
    ///
    /// `Claimant::Placed` admits `Work::Creator` alone, so every other class is
    /// reachable only here. This is the claim half of [`Self::sweep`], for the
    /// cases that dispatch and settle by hand.
    async fn lane_claim(
        &self,
        journal: &AppWorkflows,
    ) -> Option<zeroship_workflow_manager::DeliveryGrant> {
        zeroship_workflow_manager::maintenance::MaintenanceAuthority::new(
            journal.app_id().clone(),
            self.worker.clone(),
        )
        .claim(
            self.coordinator.queue(),
            Ok(AppPolicy::default().max_delivery_attempts),
        )
        .await
        .unwrap()
    }

    /// The lane asserts its own authority: `Claimant::Placed` denies every sweep,
    /// so a host claiming a placement can never be handed one, and the retention
    /// duty this fixture publishes is claimable only here.
    async fn sweep(
        &self,
        journal: &AppWorkflows,
        objects: &PayloadObjects,
    ) -> (JobSpec, JobReceipt, SettlementReceipt) {
        self.sweep_publishing(journal, objects, &super::NoPublication(journal.app_id().clone()))
            .await
    }

    /// As [`Self::sweep`], with the publisher the dispatch's own intents reach.
    ///
    /// A duty that commits creator work publishes it through a publisher rather
    /// than on its settlement, so a case that wants the consumer to go on and
    /// execute that work has to hand the lane one that really submits.
    async fn sweep_publishing(
        &self,
        journal: &AppWorkflows,
        objects: &PayloadObjects,
        publisher: &impl zeroship_workflow::service::publication::JobPublisher,
    ) -> (JobSpec, JobReceipt, SettlementReceipt) {
        let lane = zeroship_workflow_manager::maintenance::MaintenanceAuthority::new(
            journal.app_id().clone(),
            self.worker.clone(),
        );
        let queue = self.coordinator.queue();
        let grant = self
            .lane_claim(journal)
            .await
            .expect("the lane takes the published maintenance row");
        let job = grant.delivery().job.clone();
        let MaintenanceOutcome::Settled(receipt) = journal
            .maintenance_job(
                &grant,
                publisher,
                objects,
                objects,
                MaintenanceOptions::default(),
            )
            .await
            .unwrap()
        else {
            panic!("the lane's dispatch settles the row it claimed")
        };
        // Construct once and retry the same request, the way a lane owes a lost
        // acknowledgement: the queue commits the first attempt, the reply is
        // dropped, and the retry must present identical metadata.
        let settlement = receipt.settlement(&grant).unwrap();
        let acknowledged = loop {
            self.requests.borrow_mut().push(settlement.clone());
            let observed = lane.settle(queue, &settlement).await.unwrap();
            if !self.lose_ack.replace(false) {
                break observed;
            }
        };
        (job, *receipt, acknowledged)
    }

    async fn new(fixture: &Fixture) -> Rc<Self> {
        use zeroship_core::workflow_coordination::{RegisterWorker, WorkerState};
        let database = crate::manager_queue::Manager::new(fixture.app.app_id()).await;
        // The trusted in-process worker of a local host: one zone, no enrollment.
        let coordinator = zeroship_workflow_manager::coordinator::Coordinator::new(
            database.queue.clone(),
            zeroship_workflow_manager::coordinator::Options::default(),
            Rc::new(zeroship_workflow_manager::eligibility::LocalEligibility::new(
                zeroship_workflow_manager::eligibility::ZoneId::default_zone(),
            )),
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
        // The manager places the app; this worker is its only capacity.
        let zeroship_workflow_manager::coordinator::Placed::Assigned(assignment) = coordinator
            .place(fixture.app.app_id())
            .await
            .unwrap()
        else {
            panic!("the local host is the app's only eligible worker");
        };
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
    /// Asked of the journal this host holds, the way the crossed transport asks
    /// the service that holds it.
    async fn release(
        &self,
        journal: &Self::Journal,
        lease: &Self::Lease,
        task: &zeroship_workflow::service::delivery::DeliveredTask,
    ) -> Result<(), WorkflowServiceError> {
        journal.release_job(task, lease).await
    }
    async fn receipt(
        &self,
        journal: &Self::Journal,
        job: &zeroship_core::workflow_jobs::JobSpec,
    ) -> Result<Option<zeroship_workflow::service::delivery::JobReceipt>, WorkflowServiceError> {
        journal.job_receipt(job).await
    }
    type Lease = zeroship_workflow_manager::DeliveryGrant;
    /// This host holds the journal, so an attempt is scoped here rather than
    /// server-side.
    type Journal = AppWorkflows;
    fn scope(
        &self,
        journal: &Self::Journal,
        authority: &zeroship_workflow::service::PolicyAuthority,
    ) -> Result<Self::Journal, WorkflowServiceError> {
        scope_journal(journal, authority)
    }
    async fn claim(
        &self,
        journal: &AppWorkflows,
        scope: &AssignedScope,
    ) -> Result<Option<Claimed<Self::Lease>>, WorkflowServiceError> {
        let granted = self
            .coordinator
            .claim_job(&self.worker, scope, Ok(AppPolicy::default().max_delivery_attempts), || async { Ok(self.worker.clone()) })
            .await
            .map_err(manager_error)?;
        let Some(lease) = granted else {
            return Ok(None);
        };
        let accepted = if lease.delivery().job.operation.accepts_execution() {
            Some(journal.accept_job(&lease).await?)
        } else {
            None
        };
        Ok(Some(Claimed { lease, accepted }))
    }
    async fn heartbeat(
        &self,
        journal: &AppWorkflows,
        lease: &Self::Lease,
        task: &DeliveredTask,
    ) -> Result<Renewed<Self::Lease>, WorkflowServiceError> {
        let lease = self
            .coordinator
            .heartbeat_job(&self.worker, lease.delivery(), || async {
                Ok(self.worker.clone())
            })
            .await
            .map_err(manager_error)?;
        let renewal = journal.heartbeat_job(task, &lease).await?;
        Ok(Renewed { lease, renewal })
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
    async fn complete(
        &self,
        journal: &AppWorkflows,
        lease: &Self::Lease,
        task: &DeliveredTask,
        execution: WorkflowExecution,
        confirmed: Vec<zeroship_workflow::service::delivery::PayloadConfirmation>,
    ) -> Result<Completed, WorkflowServiceError> {
        assert!(confirmed.is_empty(), "an in-process store confirms its own uploads");
        let receipt = journal.complete_job(task, lease, execution).await?;
        let settlement = receipt.settlement(lease)?;
        Ok(Completed {
            settlement: JobTransport::settle(self, &settlement).await?,
            receipt,
        })
    }
}

impl zeroship_workflow::service::publication::JobPublisher for NativeManager {
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
    assert!(manager.claim(&fixture.app, &manager.scope).await.unwrap().is_none());
    assert!(fixture.app.pending_jobs(None, 1).await.unwrap().is_empty());
}

#[compio::test]
async fn manager_collect_duty_settles_without_publishing_or_executing_creator_work() {
    let fixture = Fixture::new(AppPolicy::default()).await;
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
    // The consumer runs THROUGHOUT, bound to the placement, and is offered
    // nothing: `Claimant::Placed` admits `Work::Creator` alone, so a row of any
    // other class is the lane's. Keeping it live is what makes the executor
    // assertion below say something rather than hold vacuously.
    let swept = RefCell::new(None);
    finished(consumer.run_until(async {
        *swept.borrow_mut() = Some(Box::pin(manager.sweep(&fixture.app, &fixture.objects)).await);
    }))
    .await;
    let (settled_job, settled_receipt, acknowledged) =
        swept.into_inner().expect("the lane settled the row it claimed");
    assert_eq!(settled_job, collect, "the lane claimed the published row");
    assert_eq!(settled_receipt.outcome, JobOutcome::Completed {});
    assert_eq!(acknowledged.outcome, JobOutcome::Completed {});
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
        std::slice::from_ref(&fixture.job)
    );
    assert!(manager.claim(&fixture.app, &manager.scope).await.unwrap().is_none());
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
    // The consumer runs THROUGHOUT, bound to the placement, and is offered
    // nothing: `Claimant::Placed` admits `Work::Creator` alone, so a row of any
    // other class is the lane's. Keeping it live is what makes the executor
    // assertion below say something rather than hold vacuously.
    let swept = RefCell::new(None);
    finished(consumer.run_until(async {
        *swept.borrow_mut() = Some(Box::pin(manager.sweep(&fixture.app, &fixture.objects)).await);
    }))
    .await;
    let (settled_job, settled_receipt, acknowledged) =
        swept.into_inner().expect("the lane settled the row it claimed");
    assert_eq!(settled_job, fanout, "the lane claimed the published row");
    assert_eq!(settled_receipt.outcome, JobOutcome::Completed {});
    assert_eq!(acknowledged.outcome, JobOutcome::Completed {});
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
    assert!(manager.claim(&fixture.app, &manager.scope).await.unwrap().is_none());
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
    // The consumer runs THROUGHOUT, bound to the placement, and is offered
    // nothing: `Claimant::Placed` admits `Work::Creator` alone, so a row of any
    // other class is the lane's. Keeping it live is what makes the executor
    // assertion below say something rather than hold vacuously.
    let swept = RefCell::new(None);
    finished(consumer.run_until(async {
        *swept.borrow_mut() = Some(Box::pin(manager.sweep(&fixture.app, &fixture.objects)).await);
    }))
    .await;
    let (settled_job, settled_receipt, acknowledged) =
        swept.into_inner().expect("the lane settled the row it claimed");
    assert_eq!(settled_job, page, "the lane claimed the published row");
    assert_eq!(settled_receipt.outcome, JobOutcome::Completed {});
    assert_eq!(acknowledged.outcome, JobOutcome::Completed {});
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
    assert!(manager.claim(&fixture.app, &manager.scope).await.unwrap().is_none());
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
    // Two claimants, one consumer run. The reconciliation is `Work::Maintenance`,
    // which `Claimant::Placed` denies, so the lane takes it; its settlement
    // carries the creator Advance, and THAT is what the consumer executes. The
    // order is the point: the duty publishes creator work before the placement
    // can be handed any.
    let swept = RefCell::new(None);
    finished(consumer.run_until(async {
        *swept.borrow_mut() = Some(
            Box::pin(manager.sweep_publishing(
                &fixture.app,
                &fixture.objects,
                manager.as_ref(),
            ))
            .await,
        );
        manager.completion.recv_async().await.unwrap();
    }))
    .await;
    let (settled_job, settled_receipt, acknowledged) =
        swept.into_inner().expect("the lane settled the duty it claimed");
    assert_eq!(settled_job, reconciliation);
    assert_eq!(settled_receipt.outcome, JobOutcome::Waiting {});
    assert_eq!(acknowledged.outcome, JobOutcome::Waiting {});
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
    assert!(manager.claim(&fixture.app, &manager.scope).await.unwrap().is_none());
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
            establish: after.map(|epoch| zeroship_core::workflow_policy::EstablishIngress {
                after: Some(Revision::try_from(epoch).unwrap()),
            }),
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
impl zeroship_workflow::service::publication::JobPublisher for Settled {
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

/// Close delivered to a journal fences ingress under a still-valid
/// epoch, and a propagation page claimed after closing began commits intents
/// after that fence. Its dispatch ticket above the closing watermark keeps the
/// manager's responsibility open over separate journal and manager databases.
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
    // Both rows are `Work::Maintenance`, so both claims are the lane's: a
    // placement admits `Work::Creator` alone and would be handed neither.
    let page_grant = manager
        .lane_claim(&fixture.app)
        .await
        .expect("the lane takes the published page");
    assert_eq!(page_grant.delivery().job, page);
    let close_grant = manager
        .lane_claim(&fixture.app)
        .await
        .expect("the lane takes the closure alongside it");
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
            zeroship_workflow::service::propagation::PropagationOptions::default(),
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
        "retired while the journal holds {} unconfirmed intents",
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
async fn the_lane_closes_and_retires_once_after_lost_ack_and_redelivery() {
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
    // A first lane worker commits the creator receipt, then crashes before
    // settling. The claim is the lane's because a closure is `Work::Maintenance`,
    // which `Claimant::Placed` denies.
    let crashed = manager
        .lane_claim(&fixture.app)
        .await
        .expect("the lane takes the closure it published");
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
    // The expired row is redelivered to the lane, whose dispatch replays the
    // receipt the crashed attempt already committed rather than closing twice.
    let (redelivered, replayed, _) =
        Box::pin(manager.sweep(&fixture.app, &fixture.objects)).await;
    assert_eq!(redelivered, close);
    assert_eq!(replayed, committed);
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
    // Settled once: neither claimant is offered it again.
    assert!(manager.lane_claim(&fixture.app).await.is_none());
    assert!(manager.claim(&fixture.app, &manager.scope).await.unwrap().is_none());
}

/// Archive masks admission, dispatch and ingress. The manager refuses to
/// establish ingress, yet the consumer still delivers the manager-origin Close
/// to the creator handler, the evidence drains and the scope retires.
#[compio::test]
async fn the_lane_delivers_close_under_archived_policy_and_the_scope_retires() {
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
    for job in fixture.app.pending_jobs(None, 100).await.unwrap() {
        fixture
            .app
            .publish_job(&job.id, &Settled(app.clone()))
            .await
            .unwrap();
    }
    let archived = AppPolicy {
        admission: false,
        dispatch: false,
        ingress: false,
        ..AppPolicy::default()
    };
    let source = Policies(
        zeroship_workflow_manager::policy::PolicyObservation::new(
            app.clone(),
            Revision::try_from(2).unwrap(),
            archived.clone(),
            Instant::now() + Duration::from_secs(600),
        )
        .unwrap(),
    );
    let refused = manager
        .coordinator
        .policy_lease(
            &manager.worker,
            "enrolled-key",
            &zeroship_core::workflow_policy::PolicyLeaseRequest {
                scope: manager.scope.clone(),
                establish: Some(zeroship_core::workflow_policy::EstablishIngress {
                    after: Some(Revision::try_from(1).unwrap()),
                }),
                ingress_used: false,
            },
            &source,
            || async { Ok(manager.worker.clone()) },
        )
        .await
        .map(|grant| grant.ingress_epoch());
    assert_eq!(refused, Err(zeroship_workflow_manager::Error::Denied));
    // The host installs the archived policy with the epoch it still holds.
    fixture
        .service
        .fixture_install(
            &app,
            PolicySnapshot::configuration(Revision::try_from(2).unwrap(), archived)
                .unwrap()
                .with_ingress_epoch(Some(Revision::try_from(1).unwrap())),
        )
        .unwrap();
    let close = recovery.begin_close(&app).await.unwrap().unwrap();
    let mut consumer =
        JobConsumer::new(manager.clone(), manager.worker.clone(), options(1)).unwrap();
    consumer
        .bindings()
        .replace(vec![scope(
            &fixture,
            manager.scope.assignment_revision.get(),
        )])
        .unwrap();
    // A closure is `Work::Maintenance`, so the placement this consumer holds is
    // offered nothing while the lane claims and settles it. The consumer stays
    // live for exactly that reason.
    let swept = RefCell::new(None);
    finished(consumer.run_until(async {
        *swept.borrow_mut() = Some(Box::pin(manager.sweep(&fixture.app, &fixture.objects)).await);
    }))
    .await;
    let (settled_job, settled_receipt, acknowledged) =
        swept.into_inner().expect("the lane settled the closure it claimed");
    assert_eq!(settled_job, close);
    assert_eq!(settled_receipt.outcome, JobOutcome::Closed { drained: true });
    assert_eq!(acknowledged.outcome, JobOutcome::Closed { drained: true });
    assert_eq!(
        fixture.app.job_receipt(&close).await.unwrap().unwrap().outcome,
        JobOutcome::Closed { drained: true }
    );
    assert_eq!(fixture.probe.starts.get(), 0);
    let retired = recovery.responsibility(&app).await.unwrap().unwrap();
    assert_eq!(retired.state, ScopeState::Retired);
    assert_eq!(
        fixture
            .app
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await
            .unwrap_err(),
        WorkflowServiceError::PermissionDenied,
        "archive refuses admission itself"
    );
}

/// Move a confirmed queue hold past any release grace, as the manager's own
/// retention suite does. The lane only considers a deployment whose hold was
/// confirmed long enough ago that its acquirer has committed its dependency.
fn age_queue_hold(manager: &NativeManager, app: &AppId, deployment: &DeploymentId) {
    let connection = rusqlite::Connection::open(&manager.database.path).unwrap();
    assert_eq!(
        connection
            .execute(
                "UPDATE deployment_holds SET held_at=0 \
                 WHERE app_id=?1 AND deployment_id=?2 AND state='held'",
                [app.as_str(), deployment.as_str()],
            )
            .unwrap(),
        1
    );
}

/// The retention lane's journal release duty reaches the creator engine's hold
/// release through the maintenance dispatch.
///
/// The operation is never constructed here: the manager's own lane mints it from
/// a released queue hold, and the sweep lane claims what it published. That is
/// what this binds. Every sweep revalidates its own operation kind, so an arm
/// pointed at another sweep refuses the delivery instead of releasing the hold,
/// and the journal keeps the deployment.
///
/// It is the LANE that claims it, not a host holding a placement: `release_hold`
/// is `Work::Maintenance`, and `Claimant::Placed` denies every kind in that
/// class, so a placed claim answers nothing here at all.
#[compio::test]
async fn retention_release_duty_dispatches_to_the_creator_hold_release() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let app = fixture.app.app_id().clone();
    let manager = NativeManager::new(&fixture).await;

    // A deployment this app never selected, whose journal hold the creator
    // engine holds and nothing in its journal depends on.
    let superseded = fixture.deployments.deploy(&app).await;
    let client = fixture.deployments.client(&app);
    fixture
        .service
        .acquire_deployment_hold(&app, &superseded.id, &superseded.hash, &client)
        .await
        .unwrap();
    fixture.deployments.assert_held(&app, &superseded.id).await;

    // The manager's own queue hold on it, aged past the release grace.
    let deployment = DeploymentId::parse(&superseded.id).unwrap();
    manager
        .database
        .queue
        .ensure_deployment(&app, &deployment)
        .await
        .unwrap();
    age_queue_hold(&manager, &app, &deployment);

    // Two retention turns: the first gives the queue hold back, the second
    // publishes the journal release duty the creator engine answers.
    let mut driver = zeroship_workflow_manager::driver::Driver::new(
        manager.coordinator.clone(),
        zeroship_workflow_manager::driver::Options::default(),
        Rc::new(zeroship_workflow_manager::lifecycle::Undeletable),
        Rc::new(zeroship_workflow_manager::capacity::LocalCapacity),
    )
    .unwrap();
    for turn in 0..2 {
        let retention = driver.tick().await.retention;
        assert!(
            retention.failures.is_empty(),
            "turn {turn}: {:?}",
            retention.failures
        );
        assert_eq!(retention.completed, 1, "turn {turn}: {retention:?}");
    }

    // A host holding a placement answers nothing here: the class the row carries
    // is one `Claimant::Placed` denies, which is what leaves the row to the lane.
    assert!(
        manager
            .claim(&fixture.app, &manager.scope)
            .await
            .unwrap()
            .is_none(),
        "a placed claim must not reach a release duty"
    );

    let (job, creator, acknowledged) =
        Box::pin(manager.sweep(&fixture.app, &fixture.objects)).await;
    assert_eq!(
        job.operation,
        JobOperation::ReleaseHold {
            deployment_id: deployment.clone()
        },
        "the lane minted the operation this dispatch runs"
    );
    assert_eq!(job.deployment_id(), None, "a release needs no hold");
    assert_eq!(creator.job, job);
    assert_eq!(creator.outcome, JobOutcome::Completed {});
    assert_eq!(acknowledged.outcome, JobOutcome::Completed {});
    // The hold release is the sweep that ran: the journal gave the
    // deployment back, so the platform collector's fence now commits.
    fixture
        .deployments
        .assert_reclaimable(&app, &superseded.id)
        .await;
    assert_eq!(
        fixture.app.job_receipt(&job).await.unwrap(),
        Some(creator),
        "the committed receipt is the release's own"
    );
    assert_eq!(fixture.probe.starts.get(), 0, "a release runs no app code");
    let requests = manager.requests.borrow();
    assert_eq!(requests.len(), 2, "the lost acknowledgement was retried");
    assert_eq!(requests[0], requests[1]);
    assert_eq!(requests[0].delivery.job, job);
    assert!(requests[0].successors.is_empty());
}
