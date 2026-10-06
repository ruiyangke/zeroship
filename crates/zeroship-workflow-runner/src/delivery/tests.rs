use super::*;
use crate::journal_fixture::PostgresFixture;
mod activation;
mod collection;
mod consumer;
mod cron;
mod fanout;
mod management;
mod propagation;
use zeroship_workflow::{
    operations::{RunOperation, RunState, StartOptions},
    service::{
        collection::CollectionOptions,
        maintenance::{MaintenanceOptions, MaintenanceOutcome},
        schema,
        store::{HostStorage, OrmStore},
        AppPolicy, DeployRegistration, HostPolicies, PolicySnapshot, RequestId, TaskAssignment,
        WorkflowService,
    },
    WorkflowExecution,
};
use async_trait::async_trait;
use futures::channel::oneshot;
use serde_json::json;
use std::{cell::Cell, collections::BTreeMap, sync::Arc};
use zeroship_core::{
    app_id::AppId,
    typed_id,
    workflow_coordination::{Revision, WorkerId},
    workflow_jobs::{JobOperation, JobOutcome, JobReceipt, JobSpec, JournalSettlement},
};
use zeroship_data_orm::{connection::ConnectionFactory, orm::Output, value};

use crate::{deployment_fixture as deployments, service_binding::ServiceFixture};

/// The delivery lease this fixture's manager transport grants and renews.
/// Tests that turn on the ratio between a lease and an execution bound derive
/// their bound from it rather than restating it.
const MANAGER_LEASE: Duration = Duration::from_secs(20);

/// The attempt the fixture's delivery carries: far past every lease and bound a
/// case uses, unless the case shortens it.
const MANAGER_ATTEMPT: Duration = Duration::from_hours(1);

/// The operation bound of the fixture's slot. Every settlement, release and
/// renewal under it is real journal I/O, and this is a bound that I/O cannot
/// plausibly miss; a case whose subject is a bound derives it from this one
/// rather than shrinking this one to fit.
const OPERATION_BOUND: Duration = Duration::from_secs(5);

/// The operation bound of a case whose attempt ends at an eighth of the lease:
/// the settlement an attempt reserves for itself has to fit inside it, with
/// most of the attempt left for the execution.
const SETTLEMENT_INSIDE_A_SHORT_ATTEMPT: Duration = MANAGER_LEASE.checked_div(32).unwrap();

/// The claim a transport would have answered for this lease.
///
/// A slot receives both halves of one exchange, so a test handing it a lease has
/// to hand it the acceptance the same exchange would have carried.
///
/// FALLIBLE, because the acceptance is part of the exchange now: a journal that
/// refuses to accept refuses the CLAIM, so a test expecting that refusal asserts
/// it here rather than on a slot that never receives a delivery.
async fn claimed<L: JobLease + Clone>(
    app: &AppWorkflows,
    lease: L,
) -> Result<Claimed<L>, WorkflowServiceError> {
    let accepted = if lease.delivery().job.operation.accepts_execution() {
        Some(app.accept_job(&lease).await?)
    } else {
        None
    };
    Ok(Claimed { lease, accepted })
}

/// The publisher a sweep in this fixture runs under.
///
/// The fixture's transport refuses an independent publication, and a sweep
/// driven here must not reach one either: a test that expects successors asserts
/// them on the settlements the transport records.
struct NoPublication(AppId);
impl zeroship_workflow::service::publication::JobPublisher for NoPublication {
    fn app_id(&self) -> &AppId {
        &self.0
    }
    async fn submit(&self, _: &JobSpec) -> Result<JobSpec, WorkflowServiceError> {
        panic!("a sweep fixture must not publish independently")
    }
}

#[derive(Clone)]
struct Lease {
    delivery: Delivery,
    expires: Instant,
    /// The end of the attempt the manager delivered, which no renewal moves.
    attempt_ends: Instant,
}
impl JobLease for Lease {
    fn delivery(&self) -> &Delivery {
        &self.delivery
    }
    fn remaining(&self) -> Option<Duration> {
        self.expires
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
    }
    fn attempt_remaining(&self) -> Option<Duration> {
        self.attempt_ends
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
    }
}

#[derive(Default)]
struct Metadata {
    settlements: RefCell<BTreeMap<i64, JournalSettlement>>,
    requests: RefCell<Vec<JournalSettlement>>,
    releases: Cell<usize>,
    lose_ack: Cell<bool>,
    /// Every acknowledgement is lost, not just the first, so a retry loop has
    /// nothing that will ever let it finish.
    lose_every_ack: Cell<bool>,
    reject_renewal: Cell<bool>,
    stall_renewal: Cell<bool>,
    substitute_renewal: Cell<bool>,
    /// Answer a settlement with a different family-valid outcome than the one
    /// the journal committed, the way a peer that settled another request would.
    substitute_outcome: Cell<bool>,
    renewals: Cell<usize>,
    /// The execution this attempt started has resolved, set by the executor the
    /// moment `wait` answers. A renewal after that is the settlement keeping its
    /// lease, not the execution reporting that it began.
    resolved: Rc<Cell<bool>>,
    /// Renewals made before the execution resolved: the ones that report an
    /// execution began rather than a delivery was made.
    renewals_before_resolution: Cell<usize>,
    /// A terminal reply -- a manager refusal or a substituted delivery -- has
    /// been given; a call after it would be a retry of what must not be retried.
    terminal: Cell<bool>,
    /// Renewal calls made after a terminal reply.
    calls_after_terminal: Cell<usize>,
    renewal_deadline: Cell<Option<Instant>>,
    renewed_late: Cell<bool>,
    /// How long the next renewal call takes before it answers; taken by the
    /// call it delays, so a retry of the same attempt answers at once.
    renewal_delay: Cell<Option<Duration>>,
    /// The gate the case releases to issue a renewal, or `None` to keep the
    /// transport's lease fraction. Taken by the one renewal it places.
    renewal_gate: RefCell<Option<oneshot::Receiver<()>>>,
    /// How far one renewal extends the manager lease; the fixture lease when
    /// unset. Never past the attempt the delivery carries, as the manager caps it.
    renewal: Cell<Option<Duration>>,
}
impl JobTransport for Metadata {
    /// Asked of the journal this host holds, the way the crossed transport asks
    /// the service that holds it.
    async fn release(
        &self,
        journal: &Self::Journal,
        lease: &Self::Lease,
        task: &zeroship_workflow::service::delivery::DeliveredTask,
    ) -> Result<(), WorkflowServiceError> {
        self.releases.set(self.releases.get() + 1);
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
    /// This host holds the journal in its own process.
    type Journal = AppWorkflows;
    async fn claim(&self, _: &ClaimJobs) -> Result<ClaimedBatch<Lease>, WorkflowServiceError> {
        panic!("a delivered slot must not claim or discover work")
    }
    async fn give_back(&self, _: &Claimed<Lease>, _: Unstarted) -> Result<(), WorkflowServiceError> {
        panic!("a delivered slot prepares nothing, so it never gives a claim back")
    }
    /// Both halves, in the order a served renewal keeps: the queue's lease
    /// first, then the journal task under it.
    ///
    /// A SUBSTITUTED DELIVERY IS SUBSTITUTED IN THE REPLY, not in the journal
    /// call. The journal half runs under the delivery this attempt really holds,
    /// so what the caller then sees is a reply naming a delivery it never asked
    /// about -- which is the case its own comparison exists to refuse. Renewing
    /// the journal under the substituted one instead would make the journal
    /// refuse first and leave that comparison unmeasured.
    async fn heartbeat(
        &self,
        journal: &AppWorkflows,
        lease: &Lease,
        task: &DeliveredTask,
    ) -> Result<Renewed<Lease>, WorkflowServiceError> {
        self.renewals.set(self.renewals.get() + 1);
        if !self.resolved.get() {
            self.renewals_before_resolution
                .set(self.renewals_before_resolution.get() + 1);
        }
        if self.terminal.get() {
            self.calls_after_terminal
                .set(self.calls_after_terminal.get() + 1);
        }
        if let Some(delay) = self.renewal_delay.take() {
            compio::time::sleep(delay).await;
        }
        if self.stall_renewal.get() {
            std::future::pending::<()>().await;
        }
        if self
            .renewal_deadline
            .get()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.renewed_late.set(true);
        }
        if self.reject_renewal.get() {
            self.terminal.set(true);
            return Err(WorkflowServiceError::PermissionDenied);
        }
        let mut renewed = lease.clone();
        renewed.expires = (Instant::now() + self.renewal.get().unwrap_or(MANAGER_LEASE))
            .min(lease.attempt_ends);
        let renewal = journal.heartbeat_job(task, &renewed).await?;
        if self.substitute_renewal.get() {
            self.terminal.set(true);
            renewed.delivery.worker_id = WorkerId::mint();
        }
        Ok(Renewed {
            lease: renewed,
            renewal,
        })
    }
    /// Settled from the journal this host holds, as every in-process transport
    /// settles, and recorded so a retry is seen to settle the same thing.
    async fn settle(
        &self,
        journal: &AppWorkflows,
        lease: &Lease,
    ) -> Result<SettlementReceipt, WorkflowServiceError> {
        let settlement = &committed_settlement(journal, lease).await?;
        self.requests.borrow_mut().push(settlement.clone());
        let mut settlements = self.settlements.borrow_mut();
        let previous = settlements
            .entry(settlement.delivery().attempt.get())
            .or_insert_with(|| settlement.clone());
        assert_eq!(
            previous, settlement,
            "a retry changed immutable settlement metadata"
        );
        if self.lose_every_ack.get() || self.lose_ack.replace(false) {
            return Err(WorkflowServiceError::Timeout);
        }
        let outcome = if self.substitute_outcome.get() {
            JobOutcome::Waiting {}
        } else {
            settlement.outcome().clone()
        };
        Ok(SettlementReceipt {
            job_id: settlement.delivery().job.id.clone(),
            app_id: settlement.delivery().job.app_id.clone(),
            attempt: settlement.delivery().attempt,
            outcome,
        })
    }
    /// The journal commits first, because its commit is what decides the outcome
    /// the queue is then settled with. Routed through `settle` so a lost ACK and
    /// the immutable-metadata assertion still observe the merged path.
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
        Ok(Completed {
            settlement: JobTransport::settle(self, journal, lease).await?,
            receipt,
        })
    }
    /// The case's gate, or the transport's lease fraction when none is set.
    ///
    /// A case that races a short lease or phase installs a gate and releases
    /// it at the point it chooses, so the renewal lands where the case places
    /// it rather than where a loaded host schedules the wait. Every renewal
    /// after the gated one keeps the lease fraction.
    async fn wait_for_renewal(&self, remaining: Duration) {
        let gate = self.renewal_gate.borrow_mut().take();
        match gate {
            Some(gate) => {
                let _ = gate.await;
            }
            None => super::default_renewal_wait(remaining).await,
        }
    }
}

#[derive(Clone, Copy, Default)]
enum Mode {
    #[default]
    Complete,
    Failure,
    Pending,
    AfterCreatorRenewal,
    HardTimeout,
    /// Checks its budget until `Probe::hold` has passed since it started, then
    /// completes: an execution that needs longer than one lease.
    CompleteAfter,
    /// Waits on `Probe::release` and completes once the case opens it. A gate
    /// the case never opens is an execution only a bound can end.
    Gated,
    /// One compensable step, then a failure: the run owes a rollback.
    CompensableFailure,
    /// The compensating effect reaches the outside world, and the thread is held
    /// past the execution budget before the resolved outcome is returned.
    Compensate,
}

#[derive(Default)]
struct Probe {
    /// How long `Mode::Compensate` holds the thread, as synchronous app code
    /// does, after the compensating effect has already landed.
    overrun: Cell<Duration>,
    compensations: Cell<usize>,
    starts: Cell<usize>,
    cancels: Cell<usize>,
    stops: Cell<usize>,
    mode: Cell<Mode>,
    started: RefCell<Option<oneshot::Sender<()>>>,
    stopping: RefCell<Option<oneshot::Sender<()>>>,
    stop_gate: RefCell<Option<oneshot::Receiver<()>>>,
    creator_renewed: Cell<bool>,
    /// The execution this attempt started has resolved, shared with the
    /// transport so a renewal it makes can tell reporting from settlement.
    resolved: Rc<Cell<bool>>,
    /// How long `Mode::CompleteAfter` runs before it completes.
    hold: Cell<Duration>,
    /// The gate `Mode::Gated` waits on, taken by the execution it starts.
    release: RefCell<Option<oneshot::Receiver<()>>>,
}

struct Executor {
    probe: Rc<Probe>,
    service: WorkflowService,
}
impl TaskExecutor for Executor {
    fn start(
        &self,
        assignment: &TaskAssignment,
        budget: crate::ExecutionBudget,
    ) -> Result<Box<dyn TaskExecution>, WorkflowServiceError> {
        self.probe.starts.set(self.probe.starts.get() + 1);
        if let Some(started) = self.probe.started.borrow_mut().take() {
            let _ = started.send(());
        }
        let (interrupt, interrupted) = flume::bounded(1);
        budget.on_interrupt(move || {
            let _ = interrupt.try_send(());
        })?;
        Ok(Box::new(Execution {
            probe: self.probe.clone(),
            service: self.service.clone(),
            task: assignment.id.clone(),
            initial_deadline: assignment.deadline,
            interrupted,
            budget,
            stopped: false,
            gate: self.probe.stop_gate.borrow_mut().take(),
            release: self.probe.release.borrow_mut().take(),
        }))
    }
}

struct Execution {
    probe: Rc<Probe>,
    service: WorkflowService,
    task: String,
    initial_deadline: i64,
    interrupted: flume::Receiver<()>,
    budget: crate::ExecutionBudget,
    stopped: bool,
    gate: Option<oneshot::Receiver<()>>,
    release: Option<oneshot::Receiver<()>>,
}
#[async_trait(?Send)]
impl TaskExecution for Execution {
    async fn wait(&mut self) -> Result<WorkflowExecution, WorkflowServiceError> {
        match self.probe.mode.get() {
            Mode::Complete => {}
            Mode::Failure => {
                return Err(WorkflowServiceError::InvalidRequest(
                    "injected execution failure".into(),
                ));
            }
            Mode::Pending => std::future::pending().await,
            Mode::Gated => match self.release.take() {
                Some(release) => {
                    if release.await.is_err() {
                        std::future::pending::<()>().await;
                    }
                }
                None => std::future::pending().await,
            },
            Mode::CompensableFailure => {
                return WorkflowExecution::from_runtime_value(json!({"outcomes":[
                    {"kind":"StepCompleted","ordinal":0,"name":"reserve",
                     "compensable":true,"output":0},
                    {"kind":"RunFailed",
                     "error":{"type":"Error","message":"intentional failure"}}
                ]}));
            }
            Mode::Compensate => {
                self.probe.compensations.set(self.probe.compensations.get() + 1);
                std::thread::sleep(self.probe.overrun.get());
                return WorkflowExecution::from_runtime_value(json!({"outcomes":[
                    {"kind":"CompensationCompleted","ordinal":0,"name":"reserve"}
                ]}));
            }
            Mode::CompleteAfter => {
                let started = Instant::now();
                while started.elapsed() < self.probe.hold.get() {
                    self.budget.check()?;
                    compio::time::sleep(Duration::from_millis(5)).await;
                }
            }
            Mode::AfterCreatorRenewal | Mode::HardTimeout => {
                loop {
                    self.budget.check()?;
                    let tx = self.service.begin().await?;
                    let Output::Rows(rows) = tx
                        .database()
                        .collection("__zeroship_workflow_tasks")?
                        .find(value!({"id":self.task.clone()}), value!({}))
                        .await?
                    else {
                        panic!("task rows")
                    };
                    let renewed = rows[0]["deadline"].as_i64().unwrap() > self.initial_deadline;
                    tx.commit().await?;
                    if renewed {
                        self.probe.creator_renewed.set(true);
                        break;
                    }
                    compio::time::sleep(Duration::from_millis(5)).await;
                }
                if matches!(self.probe.mode.get(), Mode::HardTimeout) {
                    self.interrupted.recv_async().await.unwrap();
                    self.budget.check()?;
                    unreachable!("hard budget must be exhausted");
                }
            }
        }
        // The execution has now resolved; a renewal after this point is the
        // settlement keeping its lease alive, not the execution reporting that
        // it began.
        self.probe.resolved.set(true);
        // The canned dispatch closes its run and returns nothing. A result
        // would have to be a staged payload object, and a fixture that stands
        // in for the runtime never reaches the transport that stages one.
        WorkflowExecution::from_runtime_value(json!({"outcomes":[{"kind":"RunCompleted"}]}))
    }
    fn cancel(&mut self) {
        self.probe.cancels.set(self.probe.cancels.get() + 1);
    }
    async fn stop(&mut self) {
        if self.stopped {
            return;
        }
        if let Some(stopping) = self.probe.stopping.borrow_mut().take() {
            let _ = stopping.send(());
        }
        if let Some(gate) = self.gate.as_mut() {
            let _ = gate.await;
        }
        self.gate = None;
        self.stopped = true;
        self.probe.stops.set(self.probe.stops.get() + 1);
    }
}

/// The host's configured policy at `revision`, with the open ingress epoch the
/// manager established.
fn snapshot(policy: &AppPolicy, revision: i64) -> PolicySnapshot {
    PolicySnapshot::configuration(Revision::try_from(revision).unwrap(), policy.clone())
        .unwrap()
        .with_ingress_epoch(Some(crate::manager_queue::open_epoch()))
}

struct Fixture {
    _directory: tempfile::TempDir,
    objects: crate::PayloadObjects,
    deployments: deployments::Deployments,
    service: WorkflowService,
    app: AppWorkflows,
    job: JobSpec,
    lease: Lease,
    metadata: Rc<Metadata>,
    probe: Rc<Probe>,
}
impl Fixture {
    async fn new(policy: AppPolicy) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let factory = ConnectionFactory::for_platform_url(&format!(
            "sqlite:{}",
            directory.path().join("creator.sqlite").display()
        ))
        .unwrap();
        let store = HostStorage::new(factory).open().await.unwrap();
        schema::initialize_local(&store).await.unwrap();
        Box::pin(Self::build_over(Rc::new(store), directory, policy)).await
    }

    /// The same fixture over a store the caller opened, so a case can put this
    /// journal on `PostgreSQL`.
    ///
    /// `initialize_local` above is the `SQLite` half of schema installation, so a
    /// caller arriving here has provisioned its own.
    async fn build_over(
        store: Rc<OrmStore>,
        directory: tempfile::TempDir,
        policy: AppPolicy,
    ) -> Self {
        let deployments = deployments::Deployments::new().await;
        let app_id = AppId::mint();
        let service = WorkflowService::open(store, Arc::new(HostPolicies::default()))
            .await
            .unwrap()
            .with_deployments(deployments.binding(&[&app_id]));
        service
            .fixture_register(&app_id, snapshot(&policy, 1))
            .await
            .unwrap();
        Box::pin(deployments.activate(
            &service,
            &app_id,
            &DeployRegistration {
                id: typed_id::generate("dep"),
                hash: "a".repeat(64),
                workflows: ["Example".into()].into(),
                schedules: Vec::new(),
            },
        ))
        .await
            .unwrap();
        let app = service.fixture_app(app_id);
        app.start(&RequestId::mint(), "Example", StartOptions::default())
            .await
            .unwrap();
        let job = app.pending_jobs(None, 1).await.unwrap().remove(0);
        let lease = Lease {
            delivery: Delivery {
                job: job.clone(),
                worker_id: WorkerId::mint(),
                attempt: Revision::try_from(1).unwrap(),
                deadline: 1.try_into().unwrap(),
            },
            expires: Instant::now() + MANAGER_LEASE,
            attempt_ends: Instant::now() + MANAGER_ATTEMPT,
        };
        let objects = crate::PayloadObjects::open(zeroship_storage::StorageStore::from_backend(
            Arc::new(zeroship_storage::LocalFs::new(directory.path().join("payloads"))),
        ))
        .unwrap();
        let resolved = Rc::new(Cell::new(false));
        Self {
            _directory: directory,
            objects,
            deployments,
            service,
            app,
            job,
            lease,
            metadata: Rc::new(Metadata {
                resolved: resolved.clone(),
                ..Metadata::default()
            }),
            probe: Rc::new(Probe {
                resolved,
                ..Probe::default()
            }),
        }
    }

    /// Reissue this fixture's policy at a later revision, as the service does
    /// when an operator changes it while an attempt runs.
    fn reissue(&self, policy: &AppPolicy, revision: i64) {
        self.service
            .fixture_install(self.app.app_id(), snapshot(policy, revision))
            .unwrap();
    }

    fn slot(&self, execution_timeout: Duration) -> DeliverySlot<Metadata> {
        self.slot_bounding_operations(execution_timeout, OPERATION_BOUND)
    }
    /// The same slot with the per-operation bound named, for the cases that
    /// assert what ends an attempt when a manager exchange never answers.
    fn slot_bounding_operations(
        &self,
        execution_timeout: Duration,
        operation_timeout: Duration,
    ) -> DeliverySlot<Metadata> {
        DeliverySlot::new(
            self.metadata.clone(),
            Rc::new(Executor {
                probe: self.probe.clone(),
                service: self.service.clone(),
            }),
            DeliveryOptions {
                execution_timeout,
                operation_timeout,
                retry_delay: Duration::from_millis(5),
            },
        )
        .unwrap()
    }

    /// Run one maintenance row the way its lane does: dispatch the sweep over
    /// this fixture's journal and payload store, then settle the delivery with
    /// the receipt that dispatch committed.
    ///
    /// The lane, not a delivery slot, is what claims these rows -
    /// `Claimant::admits` in `zeroship-workflow-manager` pairs each work class
    /// with exactly one claimant - so the retry around the settlement is this
    /// helper's own, matching what a lane owes a lost acknowledgement.
    async fn sweep(&self, lease: Lease) -> Result<DeliveryOutcome, WorkflowServiceError> {
        self.sweep_with(lease, MaintenanceOptions::default()).await
    }

    /// As [`Self::sweep`], under dispatch bounds the caller chooses.
    async fn sweep_with(
        &self,
        lease: Lease,
        options: MaintenanceOptions,
    ) -> Result<DeliveryOutcome, WorkflowServiceError> {
        let publisher = NoPublication(self.app.app_id().clone());
        let receipt = match self
            .app
            .maintenance_job(&lease, &publisher, &self.objects, &self.objects, options)
            .await?
        {
            MaintenanceOutcome::Settled(receipt) => *receipt,
            MaintenanceOutcome::Deferred => return Ok(DeliveryOutcome::Deferred),
            MaintenanceOutcome::Unclaimed => {
                panic!("a sweep fixture must not hand the lane creator work")
            }
        };
        receipt.settlement(&lease)?;
        let manager = loop {
            match JobTransport::settle(&*self.metadata, &self.app, &lease).await {
                Ok(observed) => break observed,
                Err(WorkflowServiceError::Timeout) => {
                    compio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(error) => return Err(error),
            }
        };
        Ok(DeliveryOutcome::Settled {
            creator: Box::new(receipt),
            manager,
        })
    }
    async fn task_state(&self) -> String {
        let tx = self.service.begin().await.unwrap();
        let Output::Rows(rows) = tx
            .database()
            .collection("__zeroship_workflow_tasks")
            .unwrap()
            .find(value!({"app_id":self.app.app_id().as_str()}), value!({}))
            .await
            .unwrap()
        else {
            panic!("task rows")
        };
        assert_eq!(rows.len(), 1);
        let state = rows[0]["state"].as_str().unwrap().to_owned();
        tx.commit().await.unwrap();
        state
    }
}

/// A slot runs creator work and nothing else, so a sweep handed to one is
/// refused instead of dispatched.
///
/// The queue is what makes this unreachable in production: `collect` is
/// `Work::Maintenance` and `Claimant::Worker` denies that whole class, so a
/// worker is never offered the row. This binds the slot's own half of it --
/// handed the delivery anyway, it commits nothing and settles nothing rather
/// than sweeping the journal under a worker's delivery.
///
/// TWO CONTROLS, because the refusal could otherwise be explained two ways.
/// The row IS sweepable: dispatched as its lane dispatches it, the same lease
/// settles and commits a receipt, so the absence asserted above is a real
/// absence and not an operation that would have failed anyway. And the same
/// slot, journal and claim path run the app's own advance to a settlement, so
/// the refusal is attributable to the kind the delivery names.
#[compio::test]
async fn a_slot_refuses_a_sweep_rather_than_dispatching_it() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let mut sweep = fixture.lease.clone();
    sweep.delivery.job.id = zeroship_core::workflow_jobs::JobId::mint();
    sweep.delivery.job.operation = JobOperation::Collect {};
    // A distinct attempt number, because the transport holds one settlement per
    // attempt and asserts a retry cannot change it: the advance control settles
    // under this lease's original attempt.
    sweep.delivery.attempt = 2.try_into().unwrap();
    let claim = claimed(&fixture.app, sweep.clone()).await.unwrap();
    assert!(
        claim.accepted.is_none(),
        "the journal accepts execution for creator work alone"
    );
    let mut slot = fixture.slot(Duration::from_secs(5));
    let refused = Box::pin(slot.run(&fixture.app, claim)).await;
    assert!(
        matches!(refused, Err(WorkflowServiceError::Unavailable(_))),
        "{refused:?}"
    );
    assert!(
        fixture
            .app
            .job_receipt(&sweep.delivery.job)
            .await
            .unwrap()
            .is_none(),
        "a refused sweep commits no receipt"
    );
    assert!(
        fixture.metadata.requests.borrow().is_empty(),
        "a refused sweep settles nothing"
    );
    assert_eq!(fixture.probe.starts.get(), 0);

    // The row was sweepable all along; only the host that took it was wrong.
    let DeliveryOutcome::Settled { creator, .. } = fixture.sweep(sweep.clone()).await.unwrap()
    else {
        panic!("the lane's dispatch settles the row the slot refused")
    };
    assert_eq!(creator.outcome, JobOutcome::Completed {});
    assert_eq!(
        fixture.app.job_receipt(&sweep.delivery.job).await.unwrap(),
        Some(*creator)
    );

    let advance = fixture.lease.clone();
    let DeliveryOutcome::Settled { creator, .. } = Box::pin(slot.run(
        &fixture.app,
        claimed(&fixture.app, advance.clone()).await.unwrap(),
    ))
    .await
    .unwrap() else {
        panic!("the control's creator work settles through the same slot")
    };
    assert_eq!(creator.job, advance.delivery.job);
    assert_eq!(fixture.probe.starts.get(), 1);
}

#[compio::test]
async fn unrepresentable_retry_delay_is_rejected_before_execution() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let result = DeliverySlot::new(
        fixture.metadata.clone(),
        Rc::new(Executor {
            probe: fixture.probe.clone(),
            service: fixture.service.clone(),
        }),
        DeliveryOptions {
            execution_timeout: Duration::from_secs(5),
            operation_timeout: Duration::from_secs(1),
            retry_delay: Duration::MAX,
        },
    );
    assert!(matches!(
        result,
        Err(WorkflowServiceError::InvalidRequest(_))
    ));
    assert_eq!(fixture.probe.starts.get(), 0);
}

/// A renewal that extends nothing ends the attempt at that renewal.
///
/// The service's policy turns dispatch off while the attempt runs. The journal
/// half of the next renewal extends nothing and answers with a pause, so the
/// execution is cancelled and joined at that one renewal -- long before the
/// lease or the execution bound would end it -- and nothing is settled. The
/// control reissues the same policy with dispatch still on: the execution is
/// still running, renewed again, past the point where the other one ended.
#[compio::test]
async fn a_renewal_that_extends_nothing_interrupts_the_execution_at_that_renewal() {
    for dispatch in [false, true] {
        let policy = AppPolicy {
            lease_ms: 900,
            ..AppPolicy::default()
        };
        let fixture = Fixture::new(policy.clone()).await;
        fixture.probe.mode.set(Mode::Pending);
        let (started, running) = oneshot::channel();
        fixture.probe.started.replace(Some(started));
        let mut slot = fixture.slot(Duration::from_secs(30));
        // Hold the first renewal until the policy is reissued, so which policy
        // the renewal answers is the case's choice and not a race with the
        // wall clock.
        let (open, gate) = oneshot::channel();
        *fixture.metadata.renewal_gate.borrow_mut() = Some(gate);
        let claim = claimed(&fixture.app, fixture.lease.clone()).await.unwrap();
        let run = slot.run(&fixture.app, claim).boxed_local();
        let Either::Left((Ok(()), run)) = futures::future::select(running, run).await else {
            panic!("the execution must start")
        };
        fixture.reissue(
            &AppPolicy {
                dispatch,
                ..policy.clone()
            },
            2,
        );
        open.send(()).unwrap();
        // The renewal delay is a third of the task lease, so two whole leases
        // hold at least one renewal for either case.
        let window = Duration::from_millis(u64::try_from(policy.lease_ms).unwrap()) * 2;
        let changed = Instant::now();
        let leftover = match futures::future::select(run, compio::time::sleep(window).boxed_local()).await {
            Either::Left((outcome, _)) => {
                assert!(!dispatch, "an attempt under dispatch ended: {outcome:?}");
                assert!(
                    matches!(outcome, Ok(DeliveryOutcome::Interrupted(ControlIntent::Pause))),
                    "{outcome:?}"
                );
                assert!(changed.elapsed() < window);
                assert_eq!(
                    fixture.metadata.renewals.get(),
                    1,
                    "the attempt outlived the renewal that extended nothing"
                );
                assert_eq!(fixture.probe.stops.get(), 1);
                assert!(fixture.metadata.requests.borrow().is_empty());
                assert!(fixture.app.job_receipt(&fixture.job).await.unwrap().is_none());
                None
            }
            Either::Right(((), run)) => {
                assert!(dispatch, "a renewal that extended nothing left the execution running");
                assert!(fixture.metadata.renewals.get() > 1);
                assert_eq!(fixture.probe.stops.get(), 0);
                Some(run)
            }
        };
        drop(leftover);
        slot.drain_interrupted().await;
    }
}

/// An in-process transport settles a committed delivery from its journal's own
/// receipt: refused while nothing has committed, and once the execution commits,
/// settled with exactly the receipt's outcome and no successors.
#[compio::test]
async fn an_in_process_settlement_is_read_from_the_journal() {
    let fixture = Box::pin(Fixture::new(AppPolicy::default())).await;
    assert!(matches!(
        committed_settlement(&fixture.app, &fixture.lease).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    let mut slot = fixture.slot(Duration::from_secs(5));
    let DeliveryOutcome::Settled { creator, .. } = Box::pin(slot.run(
        &fixture.app,
        claimed(&fixture.app, fixture.lease.clone()).await.unwrap(),
    ))
    .await
    .unwrap() else {
        panic!("the advance completes")
    };
    assert_eq!(
        committed_settlement(&fixture.app, &fixture.lease)
            .await
            .unwrap(),
        JournalSettlement::from_receipt(
            &JobReceipt {
                job: fixture.lease.delivery.job.clone(),
                outcome: creator.outcome.clone(),
            },
            &fixture.lease.delivery,
        )
        .unwrap()
    );
}

/// A manager half that reports a different family-valid outcome than the one the
/// journal committed is a peer contract violation, not a settlement: the slot
/// refuses it rather than reporting it as this delivery's result.
#[compio::test]
async fn a_settlement_with_another_outcome_is_refused() {
    let fixture = Box::pin(Fixture::new(AppPolicy::default())).await;
    fixture.metadata.substitute_outcome.set(true);
    let mut slot = fixture.slot(Duration::from_secs(5));
    let error = Box::pin(slot.run(
        &fixture.app,
        claimed(&fixture.app, fixture.lease.clone()).await.unwrap(),
    ))
    .await
    .unwrap_err();
    assert!(
        matches!(error, WorkflowServiceError::InvalidResponse(_)),
        "{error}"
    );
}

#[compio::test]
async fn corrupted_management_outcome_cannot_reexecute_or_acknowledge_advance() {
    use zeroship_core::workflow_coordination::ManagementOutcome;

    let fixture = Fixture::new(AppPolicy::default()).await;
    let mut slot = fixture.slot(Duration::from_secs(5));
    let DeliveryOutcome::Settled { creator, .. } =
        Box::pin(slot.run(&fixture.app, claimed(&fixture.app, fixture.lease.clone()).await.unwrap()))
            .await
            .unwrap()
    else {
        panic!("original advance must complete")
    };
    assert_eq!(fixture.probe.starts.get(), 1);
    let acknowledgements = fixture.metadata.requests.borrow().len();
    let tx = fixture.service.begin().await.unwrap();
    tx.database().collection("__zeroship_workflow_job_receipts").unwrap().update(
        value!({"id":fixture.job.id.as_str(),"app_id":fixture.job.app_id.as_str()}),
        value!({"outcome":serde_json::to_string(&JobOutcome::Management { outcome: ManagementOutcome::Denied {} }).unwrap()}),
    ).await.unwrap();
    tx.commit().await.unwrap();
    // THE REFUSAL IS THE CLAIM'S, because the acceptance rides it. A receipt
    // whose stored outcome does not answer its operation is refused before any
    // delivery exists, so no slot ever sees one: what the counters below prove is
    // that nothing executed and nothing was acknowledged on the strength of it.
    assert!(matches!(
        claimed(&fixture.app, fixture.lease.clone()).await,
        Err(WorkflowServiceError::Internal(_))
    ));
    assert_eq!(fixture.probe.starts.get(), 1);
    assert_eq!(fixture.probe.stops.get(), 1);
    assert_eq!(fixture.metadata.requests.borrow().len(), acknowledgements);
    let tx = fixture.service.begin().await.unwrap();
    tx.database()
        .collection("__zeroship_workflow_job_receipts")
        .unwrap()
        .update(
            value!({"id":fixture.job.id.as_str(),"app_id":fixture.job.app_id.as_str()}),
            value!({"outcome":serde_json::to_string(&creator.outcome).unwrap()}),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let DeliveryOutcome::Settled {
        creator: replay, ..
    } = Box::pin(slot.run(&fixture.app, claimed(&fixture.app, fixture.lease.clone()).await.unwrap()))
        .await
        .unwrap()
    else {
        panic!("repaired receipt must replay")
    };
    assert_eq!(replay, creator);
    assert_eq!(fixture.probe.starts.get(), 1);
    assert_eq!(
        fixture.metadata.requests.borrow().len(),
        acknowledgements + 1
    );
}

/// A lost acknowledgement is recovered rather than re-executed, and a
/// redelivery of the same work replays the committed receipt.
///
/// Run against both journals the service supports. The recovery reads a receipt
/// the journal committed, so the dialect that commits it is part of the claim: a
/// contract asserted on `SQLite` alone would say nothing about the one a
/// deployment runs.
async fn replay_contract(fixture: Fixture) {
    fixture.metadata.lose_ack.set(true);
    let mut slot = fixture.slot(Duration::from_secs(5));
    let DeliveryOutcome::Settled { creator, manager } =
        slot.run(&fixture.app, claimed(&fixture.app, fixture.lease.clone()).await.unwrap()).await.unwrap()
    else {
        panic!("expected committed settlement")
    };
    assert_eq!(creator.outcome, JobOutcome::Completed {});
    assert_eq!(manager.job_id, fixture.job.id);
    assert_eq!(fixture.metadata.requests.borrow().len(), 2);
    assert_eq!(fixture.probe.starts.get(), 1);
    assert_eq!(fixture.probe.stops.get(), 1);
    let mut redelivery = fixture.lease.clone();
    redelivery.delivery.attempt = Revision::try_from(2).unwrap();
    assert!(matches!(
        slot.run(&fixture.app, claimed(&fixture.app, redelivery).await.unwrap()).await.unwrap(),
        DeliveryOutcome::Settled { .. }
    ));
    assert_eq!(fixture.probe.starts.get(), 1);
    assert_eq!(fixture.task_state().await, "completed");
    assert_eq!(
        fixture.app.job_receipt(&fixture.job).await.unwrap(),
        Some(*creator)
    );
}

/// An applied compensation survives local expiry and is published once.
///
/// The compensating effect has already reached the outside world by the time the
/// budget ends, so withdrawing the right to publish it would strand the effect:
/// the journal would owe a rollback that had in fact run. The claim is therefore
/// never handed back for reclaim, and the journal accounts for exactly the one
/// compensation that ran.
#[compio::test]
async fn a_compensation_resolved_after_expiry_publishes_once_and_keeps_its_claim() {
    let fixture = Fixture::new(AppPolicy {
        lease_ms: 5_000,
        ..AppPolicy::default()
    })
    .await;
    // The forward attempt: one compensable step, then a failure.
    fixture.probe.mode.set(Mode::CompensableFailure);
    let mut forward = fixture.slot(Duration::from_secs(5));
    assert!(matches!(
        Box::pin(forward.run(
            &fixture.app,
            claimed(&fixture.app, fixture.lease.clone()).await.unwrap(),
        ))
        .await
        .unwrap(),
        DeliveryOutcome::Settled { .. }
    ));
    let JobOperation::Advance { run_id, .. } = fixture.job.operation.clone() else {
        panic!("the started run advances")
    };
    assert_eq!(
        fixture.app.status(run_id.as_str()).await.unwrap().state,
        RunState::Compensating,
        "a compensable step and a failure leave a rollback owed"
    );
    // The rollback arrives as the next advance of the same run, claimed the way
    // the manager delivers it.
    let owed = fixture.app.pending_jobs(None, 8).await.unwrap();
    let rollback_job = owed
        .into_iter()
        .max_by_key(|job| match &job.operation {
            JobOperation::Advance { revision, .. } => revision.get(),
            _ => 0,
        })
        .expect("the owed rollback is pending");
    assert_ne!(
        rollback_job.id, fixture.job.id,
        "the rollback is later work than the attempt that owed it"
    );
    let mut rollback = fixture.lease.clone();
    rollback.delivery.job = rollback_job;
    // A distinct delivery of distinct work, so its settlement is its own.
    rollback.delivery.attempt = Revision::try_from(2).unwrap();
    fixture.probe.mode.set(Mode::Compensate);
    fixture.probe.overrun.set(Duration::from_millis(900));
    let bound = Duration::from_millis(500);
    let mut slot = fixture.slot(bound);
    let started = Instant::now();
    Box::pin(slot.run(
        &fixture.app,
        claimed(&fixture.app, rollback).await.unwrap(),
    ))
    .await
    .unwrap();
    // Without this the case would pass over an attempt that never reached
    // expiry, and "survives expiry" would be asserting nothing.
    assert!(
        started.elapsed() > bound,
        "the attempt did not outlive its execution bound, so the publish never \
         had to survive an expired budget"
    );
    assert_eq!(
        fixture.probe.compensations.get(),
        1,
        "the compensating effect ran more than once"
    );
    assert_eq!(
        fixture.metadata.releases.get(),
        0,
        "a published compensation was handed back for reclaim"
    );
    let settled = fixture.app.status(run_id.as_str()).await.unwrap();
    assert_eq!(settled.state, RunState::Failed);
    assert_eq!(
        settled.error.unwrap()["compensation"],
        json!({"total":1, "completed":1, "failed":0, "outcome":"completed"}),
        "the journal accounts for exactly the one compensation that ran"
    );
}

#[compio::test]
async fn sqlite_lost_ack_and_new_attempt_replay_without_executing_again() {
    Box::pin(replay_contract(Fixture::new(AppPolicy::default()).await)).await;
}

#[compio::test]
async fn postgres_lost_ack_and_new_attempt_replay_without_executing_again() {
    let postgres = Box::pin(PostgresFixture::start()).await;
    let fixture = Box::pin(Fixture::build_over(
        Rc::new(postgres.store.clone()),
        tempfile::tempdir().unwrap(),
        AppPolicy::default(),
    ))
    .await;
    Box::pin(replay_contract(fixture)).await;
}

#[compio::test]
async fn paired_renewal_reaches_creator_before_execution_continues() {
    let fixture = Fixture::new(AppPolicy {
        lease_ms: 1000,
        ..AppPolicy::default()
    })
    .await;
    fixture.probe.mode.set(Mode::AfterCreatorRenewal);
    let mut slot = fixture.slot(Duration::from_secs(5));
    // The renewal is placed by the case, not by the wall clock: the first
    // renewal fires as soon as the claim is accepted, so a loaded host cannot
    // push it past the creator lease and turn the attempt into a timeout.
    let (open, gate) = oneshot::channel();
    *fixture.metadata.renewal_gate.borrow_mut() = Some(gate);
    open.send(()).unwrap();
    assert!(matches!(
        slot.run(&fixture.app, claimed(&fixture.app, fixture.lease.clone()).await.unwrap()).await.unwrap(),
        DeliveryOutcome::Settled { .. }
    ));
    assert!(fixture.metadata.renewals.get() > 0);
    assert!(fixture.probe.creator_renewed.get());
    assert_eq!(fixture.probe.stops.get(), 1);
}

/// The manager counts an attempt into its delivery ceiling on that attempt's
/// first renewal, so a renewal has to land inside every attempt that outlives
/// its own renewal delay, whatever execution bound the host was configured
/// with. It does, because the delay is a fraction of the smallest bound that
/// can end the attempt and the execution bound is one of them. A delay derived
/// from the lease alone would fall past the end of an attempt whose execution
/// bound is the shorter of the two, and the ceiling would stop advancing while
/// redelivery continued.
#[compio::test]
async fn an_execution_bound_below_the_lease_still_renews_inside_the_attempt() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    fixture.probe.mode.set(Mode::Pending);
    // Far below the fraction of the lease at which a lease-derived delay would
    // put the first renewal, and below the creator task lease as well.
    let mut slot = fixture.slot(MANAGER_LEASE / 32);
    // Place the first renewal inside the short bound instead of letting the
    // bound race the wall clock.
    let (open, gate) = oneshot::channel();
    *fixture.metadata.renewal_gate.borrow_mut() = Some(gate);
    open.send(()).unwrap();
    assert_eq!(
        slot.run(&fixture.app, claimed(&fixture.app, fixture.lease.clone()).await.unwrap())
            .await
            .unwrap_err(),
        WorkflowServiceError::Timeout
    );
    assert!(
        fixture.metadata.renewals_before_resolution.get() > 0,
        "an attempt that outlived its renewal delay reported nothing to the manager"
    );
    assert!(fixture.metadata.requests.borrow().is_empty());
}

/// The control for the renewal above, differing only in whether the attempt
/// outlives its renewal delay. An attempt that resolves first reports nothing
/// before it resolves, which is what makes a renewal evidence that an execution
/// began rather than evidence that a delivery was made.
#[compio::test]
async fn an_attempt_resolved_before_its_renewal_delay_reports_no_renewal() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    fixture.probe.mode.set(Mode::Complete);
    let mut slot = fixture.slot(MANAGER_LEASE / 32);
    assert!(matches!(
        slot.run(&fixture.app, claimed(&fixture.app, fixture.lease.clone()).await.unwrap()).await.unwrap(),
        DeliveryOutcome::Settled { .. }
    ));
    assert_eq!(
        fixture.metadata.renewals_before_resolution.get(),
        0,
        "a renewal reported an execution before the execution resolved"
    );
}

/// A control intent on a renewal interrupts the attempt, and settles nothing.
///
/// The manager's delivery ceiling and the creator's lease each end an attempt by
/// expiring. This is the third way and the only one an operator drives: the
/// renewal carries the run's effective control intent, so a run paused while a
/// host is executing it comes back as `Interrupted` with its claim released
/// rather than as a settlement.
///
/// The pause is recorded through the ordinary lifecycle call, so
/// `effective_control` is what reports it. A test that fabricated a renewal
/// carrying the intent would assert its own construction instead.
#[compio::test]
async fn a_control_intent_on_a_renewal_interrupts_without_settling() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    fixture.probe.mode.set(Mode::Pending);
    let JobOperation::Advance { run_id, .. } = fixture.job.operation.clone() else {
        panic!("the started run's first job advances it");
    };
    let claim = claimed(&fixture.app, fixture.lease.clone()).await.unwrap();
    fixture
        .app
        .transition(&RequestId::mint(), run_id.as_str(), RunOperation::Pause)
        .await
        .unwrap();
    let mut slot = fixture.slot(MANAGER_LEASE / 32);
    // Place the renewal, rather than let the short phase race it: the intent
    // rides the first renewal, which fires as soon as the claim is accepted.
    let (open, gate) = oneshot::channel();
    *fixture.metadata.renewal_gate.borrow_mut() = Some(gate);
    open.send(()).unwrap();
    assert!(matches!(
        Box::pin(slot.run(&fixture.app, claim))
            .await
            .unwrap(),
        DeliveryOutcome::Interrupted(ControlIntent::Pause)
    ));
    assert!(
        fixture.metadata.requests.borrow().is_empty(),
        "an interrupted attempt reported a settlement to the manager"
    );
}

/// The delivered attempt bound can end an attempt before either the manager
/// lease or the configured execution bound would, and the manager counts an
/// attempt into its delivery ceiling only on that attempt's first renewal. A
/// renewal therefore has to land inside an attempt the attempt bound shortens,
/// or the ceiling stops advancing while redelivery continues and nothing bounds
/// the retries. It does, because the phase this delay is a fraction of is built
/// from the same bound the guard holds.
#[compio::test]
async fn an_attempt_bound_ending_before_the_lease_still_renews_inside_the_attempt() {
    let mut fixture = Fixture::new(AppPolicy::default()).await;
    fixture.probe.mode.set(Mode::Pending);
    // Well under the fraction of the lease, and of the execution bound below,
    // at which a delay blind to the attempt bound would place the first
    // renewal.
    fixture.lease.attempt_ends = Instant::now() + MANAGER_LEASE / 8;
    fixture
        .metadata
        .renewal_deadline
        .set(Some(fixture.lease.attempt_ends));
    let mut slot = fixture.slot_bounding_operations(MANAGER_LEASE, SETTLEMENT_INSIDE_A_SHORT_ATTEMPT);
    // Place the first renewal inside the attempt bound rather than race the
    // bound that has to count it.
    let (open, gate) = oneshot::channel();
    *fixture.metadata.renewal_gate.borrow_mut() = Some(gate);
    open.send(()).unwrap();
    let started = Instant::now();
    slot.run(&fixture.app, claimed(&fixture.app, fixture.lease.clone()).await.unwrap())
        .await
        .unwrap_err();
    // An execution that never resolves on its own ends on the attempt bound
    // here, not on the execution bound the slot was configured with. Without
    // this the case would still pass while the attempt stopped binding anything.
    let elapsed = started.elapsed();
    assert!(
        elapsed < MANAGER_LEASE / 2,
        "the attempt outlived the attempt bound that had to end it: {elapsed:?}"
    );
    assert!(
        fixture.metadata.renewals_before_resolution.get() > 0,
        "an attempt that outlived its renewal delay reported nothing to the manager"
    );
    assert!(
        !fixture.metadata.renewed_late.get(),
        "the first renewal landed after the attempt it had to count"
    );
    assert!(fixture.metadata.requests.borrow().is_empty());
}

/// The control for the renewal above, differing only in whether the attempt
/// outlives its renewal delay. An attempt that resolves first reports nothing
/// before it resolves, which is what keeps a renewal evidence that an execution
/// began rather than evidence that a delivery was made.
///
/// A renewal made after the execution resolves is the settlement keeping its
/// lease alive, not the execution reporting; the assertion is the ordering, so
/// a loaded host that lets one land during a slow settlement does not turn it
/// into execution evidence.
#[compio::test]
async fn an_attempt_bound_resolved_first_reports_no_renewal() {
    let mut fixture = Fixture::new(AppPolicy::default()).await;
    fixture.probe.mode.set(Mode::Complete);
    fixture.lease.attempt_ends = Instant::now() + MANAGER_LEASE / 8;
    let mut slot = fixture.slot_bounding_operations(MANAGER_LEASE, SETTLEMENT_INSIDE_A_SHORT_ATTEMPT);
    assert!(matches!(
        slot.run(&fixture.app, claimed(&fixture.app, fixture.lease.clone()).await.unwrap()).await.unwrap(),
        DeliveryOutcome::Settled { .. }
    ));
    assert_eq!(
        fixture.metadata.renewals_before_resolution.get(),
        0,
        "a renewal reported an execution before the execution resolved"
    );
}

/// An execution is cut at the attempt bound its delivery carries, whatever
/// longer bound the host was configured with.
///
/// The lease here is left uncapped, longer than the attempt, so the attempt is
/// the only short bound: the guard and nothing else ends the execution. The
/// execution waits on a gate the cut arm never opens, so the bound is the only
/// thing that can end it. That attempt is the slot's operation bound, which it
/// reserves for the release, and a short window after it, so the release keeps
/// the bound real journal I/O cannot plausibly miss. The attempt is stamped
/// once the claim is accepted, so the window is the slot's alone.
///
/// The control differs only in the attempt bound: under an attempt far past the
/// window, the same execution is still running once the window has passed
/// twice, and when the case then opens its gate it completes and settles.
#[compio::test]
async fn an_execution_is_cut_at_the_delivered_attempt_bound_under_a_longer_local_bound() {
    let window = Duration::from_millis(250);
    let local = Duration::from_secs(30);
    for within in [false, true] {
        let fixture = Fixture::new(AppPolicy::default()).await;
        fixture.probe.mode.set(Mode::Gated);
        let (open, gate) = oneshot::channel();
        fixture.probe.release.replace(Some(gate));
        let (started, running) = oneshot::channel();
        fixture.probe.started.replace(Some(started));
        let attempt = if within {
            local
        } else {
            OPERATION_BOUND + window
        };
        let mut slot = fixture.slot(local);
        let mut claim = claimed(&fixture.app, fixture.lease.clone()).await.unwrap();
        claim.lease.attempt_ends = Instant::now() + attempt;
        let begun = Instant::now();
        let driver = async {
            running.await.unwrap();
            if within {
                compio::time::sleep(window * 2).await;
                assert_eq!(
                    fixture.probe.cancels.get(),
                    0,
                    "an attempt far past the window cut the execution inside it"
                );
                open.send(()).unwrap();
            } else {
                drop(open);
            }
        };
        let (outcome, ()) = futures::join!(Box::pin(slot.run(&fixture.app, claim)), driver);
        let elapsed = begun.elapsed();
        if within {
            assert!(
                matches!(outcome, Ok(DeliveryOutcome::Settled { .. })),
                "an execution inside its attempt bound did not settle: {outcome:?}"
            );
            assert!(elapsed >= window * 2);
        } else {
            assert_eq!(outcome.unwrap_err(), WorkflowServiceError::Timeout);
            assert!(
                elapsed < attempt,
                "the execution and its release outlived the attempt bound it was delivered \
                 with: {elapsed:?}"
            );
            assert!(fixture.probe.cancels.get() > 0, "the attempt bound cut the execution");
            assert_eq!(fixture.metadata.releases.get(), 1, "the cut delivery went back");
            assert!(fixture.metadata.requests.borrow().is_empty());
            assert!(fixture.app.job_receipt(&fixture.job).await.unwrap().is_none());
        }
        assert_eq!(fixture.probe.starts.get(), 1);
        assert_eq!(fixture.probe.stops.get(), 1);
    }
}

/// An execution that would run to the end of its attempt is stopped one
/// operation bound before it, so what follows still holds a live lease: here
/// the release, which the journal refuses under a spent lease.
///
/// The manager caps every lease at the attempt, so the lease ends with it and
/// nothing but the reserve leaves the release any authority. The execution
/// never resolves on its own, so the attempt is the only thing that ends it.
#[compio::test]
async fn an_execution_cut_at_its_attempt_still_releases_inside_its_lease() {
    let mut fixture = Fixture::new(AppPolicy::default()).await;
    fixture.probe.mode.set(Mode::Pending);
    let attempt = Duration::from_millis(1500);
    let operation = attempt / 3;
    fixture.lease.attempt_ends = Instant::now() + attempt;
    fixture.lease.expires = fixture.lease.attempt_ends;
    let mut slot = fixture.slot_bounding_operations(Duration::from_secs(30), operation);
    let started = Instant::now();
    let outcome = Box::pin(slot.run(
        &fixture.app,
        claimed(&fixture.app, fixture.lease.clone()).await.unwrap(),
    ))
    .await;
    let elapsed = started.elapsed();
    assert_eq!(outcome.unwrap_err(), WorkflowServiceError::Timeout);
    assert!(
        elapsed < attempt,
        "the release waited for the end of the attempt: {elapsed:?}"
    );
    assert_eq!(fixture.probe.starts.get(), 1);
    assert_eq!(fixture.metadata.releases.get(), 1);
    assert_eq!(
        fixture.task_state().await,
        "released",
        "the task went back under a live lease"
    );
}

/// A delivery whose attempt has less left than one operation bound when it
/// reaches the slot has no room to execute and settle inside the attempt, so it
/// never starts: it ends with `Timeout` and goes back explicitly, its journal
/// task released under the lease it still holds. No execution bound of zero or
/// less is built for it.
///
/// The control is the same delivery with the attempt one operation bound and
/// more ahead of it, which starts and settles. The operation bound is the
/// fixture's own, which the release and the settlement cannot plausibly miss,
/// and the attempt is stamped once the claim is accepted, so what the slot sees
/// left of it is what the case chose.
#[compio::test]
async fn an_attempt_shorter_than_one_operation_bound_is_released_unstarted() {
    for room in [false, true] {
        let fixture = Fixture::new(AppPolicy::default()).await;
        let attempt = if room {
            OPERATION_BOUND * 3
        } else {
            OPERATION_BOUND / 2
        };
        let mut slot = fixture.slot(Duration::from_secs(30));
        let mut claim = claimed(&fixture.app, fixture.lease.clone()).await.unwrap();
        claim.lease.attempt_ends = Instant::now() + attempt;
        let outcome = Box::pin(slot.run(&fixture.app, claim)).await;
        if room {
            assert!(
                matches!(outcome, Ok(DeliveryOutcome::Settled { .. })),
                "an attempt with room for its execution and settlement did not settle: \
                 {outcome:?}"
            );
            assert_eq!(fixture.probe.starts.get(), 1);
            assert_eq!(fixture.metadata.releases.get(), 0);
        } else {
            assert_eq!(outcome.unwrap_err(), WorkflowServiceError::Timeout);
            assert_eq!(fixture.probe.starts.get(), 0, "no execution was started");
            assert_eq!(
                fixture.metadata.releases.get(),
                1,
                "the delivery went back explicitly"
            );
            assert_eq!(fixture.task_state().await, "released");
            assert!(fixture.app.job_receipt(&fixture.job).await.unwrap().is_none());
        }
    }
}

/// A renewed lease carries an execution past the lease it started under.
///
/// The lease is renewable and the attempt is not, so the guard's hard bound is
/// the local ceiling and the attempt alone; the lease holds the guard through
/// each renewal instead. An execution needing several leases completes while
/// every renewal answers.
#[compio::test]
async fn a_renewed_lease_carries_an_execution_past_the_lease_it_started_under() {
    let mut fixture = Fixture::new(AppPolicy::default()).await;
    let lease = Duration::from_secs(2);
    fixture.lease.expires = Instant::now() + lease;
    fixture.metadata.renewal.set(Some(lease));
    fixture.probe.mode.set(Mode::CompleteAfter);
    fixture.probe.hold.set(lease * 5 / 2);
    let mut slot = fixture.slot(Duration::from_secs(30));
    let started = Instant::now();
    let outcome = Box::pin(slot.run(
        &fixture.app,
        claimed(&fixture.app, fixture.lease.clone()).await.unwrap(),
    ))
    .await;
    assert!(
        matches!(outcome, Ok(DeliveryOutcome::Settled { .. })),
        "a renewed execution ended at the lease it started under: {outcome:?}"
    );
    assert!(started.elapsed() > lease * 2);
    assert!(fixture.metadata.renewals.get() > 1);
}

/// A heartbeat reply that lands after one operation bound, but inside the lease
/// and phase, does not end the attempt: the renewal is retried and the execution
/// finishes once it lands, settling exactly once.
///
/// The first call's reply misses the operation bound while the attempt still has
/// most of its lease and phase left. A renewal the attempt could not retry would
/// end it at that one call -- the `?` on the bounded heartbeat -- and the turn
/// the journal was about to commit would be dropped for redelivery instead. The
/// execution completes only after it sees the deadline the retry wrote, so the
/// settlement is caused by the renewal that landed rather than by a call that
/// never answered.
#[compio::test]
async fn a_renewal_delayed_past_its_bound_but_within_the_lease_still_commits_once() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    fixture.probe.mode.set(Mode::AfterCreatorRenewal);
    let operation = Duration::from_millis(500);
    fixture.metadata.renewal_delay.set(Some(operation * 2));
    let mut slot = fixture.slot_bounding_operations(Duration::from_secs(3), operation);
    let outcome = Box::pin(slot.run(
        &fixture.app,
        claimed(&fixture.app, fixture.lease.clone()).await.unwrap(),
    ))
    .await;
    assert!(
        matches!(outcome, Ok(DeliveryOutcome::Settled { .. })),
        "an attempt whose first renewal missed its bound did not settle: {outcome:?}"
    );
    assert!(
        fixture.probe.creator_renewed.get(),
        "the execution finished without a renewal landing"
    );
    assert!(
        fixture.metadata.renewals.get() > 1,
        "the delayed renewal was never retried"
    );
    assert_eq!(fixture.probe.starts.get(), 1, "the execution ran once");
    assert_eq!(
        fixture.metadata.releases.get(),
        0,
        "an attempt that settled was handed back for redelivery"
    );
    assert_eq!(
        fixture.metadata.requests.borrow().len(),
        1,
        "the attempt settled more than once"
    );
}

/// A renewal that never answers is retried while its renewal budget lasts, and
/// the attempt ends when that budget runs out.
///
/// The manager accepts each connection and then says nothing. One call does not
/// end the attempt: each is bounded by the operation bound so it cannot hold the
/// slot, and the whole renewal is bounded by the grant and the phase, so the
/// attempt runs until the remaining lease cannot cover another renewal. The
/// lease is deliberately shorter than the phase, so the grant is the budget that
/// ends the attempt and not the execution bound. Several calls are made inside
/// it, and the attempt commits nothing.
#[compio::test]
async fn a_renewal_that_never_answers_is_retried_until_its_renewal_budget_runs_out() {
    let mut fixture = Fixture::new(AppPolicy::default()).await;
    fixture.probe.mode.set(Mode::Pending);
    fixture.metadata.stall_renewal.set(true);
    let lease = Duration::from_secs(1);
    fixture.lease.expires = Instant::now() + lease;
    let execution = Duration::from_secs(30);
    let mut slot = fixture.slot_bounding_operations(execution, Duration::from_millis(100));
    let started = Instant::now();
    let outcome = Box::pin(slot.run(
        &fixture.app,
        claimed(&fixture.app, fixture.lease.clone()).await.unwrap(),
    ))
    .await;
    let elapsed = started.elapsed();
    assert_eq!(outcome.unwrap_err(), WorkflowServiceError::Timeout);
    assert!(
        elapsed < execution / 2,
        "the attempt ran to its execution bound instead of its renewal budget: {elapsed:?}"
    );
    assert!(
        elapsed > lease / 2,
        "the attempt ended before its renewal budget was spent: {elapsed:?}"
    );
    assert!(
        fixture.metadata.renewals.get() > 1,
        "a stalled renewal ended the attempt after one call instead of retrying"
    );
    assert!(
        fixture.metadata.requests.borrow().is_empty(),
        "an attempt ended by a stalled renewal reported a settlement"
    );
    assert!(fixture
        .app
        .job_receipt(&fixture.job)
        .await
        .unwrap()
        .is_none());
}

/// Completion retries stay bounded while renewal keeps succeeding.
///
/// A settlement the manager never acknowledges is retried, and renewal is the
/// thing that would otherwise keep the attempt alive to retry in: a host whose
/// renewals all succeed has no expiring authority to stop it. So the operation
/// bound is what has to, and this is the case that says so - the retries run
/// until that bound and end there, the claim is not settled, and the attempt
/// reports the failure rather than looping.
///
/// The operation bound is the fixture's own, which the journal commit and the
/// read every retry makes cannot plausibly miss, so the retries counted are the
/// loop's and not a measure of how fast the journal answered. The lease the
/// delivery holds outlasts every bound the attempt runs under, so an attempt
/// that ends inside it was ended by a bound and not by expiring authority, and
/// the case is cut at that lease rather than left to loop.
#[compio::test]
async fn completion_retries_end_with_the_phase_while_renewal_keeps_succeeding() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    fixture.probe.mode.set(Mode::Complete);
    fixture.metadata.lose_every_ack.set(true);
    let mut slot = fixture.slot(MANAGER_LEASE);
    let claim = claimed(&fixture.app, fixture.lease.clone()).await.unwrap();
    let authority = claim.lease.remaining().expect("the delivery holds a live lease");
    // The commit, the recovery read and the acknowledgement each end on one
    // operation bound at the latest.
    assert!(
        authority > OPERATION_BOUND * 3,
        "the premise: the lease outlasts every bound the attempt runs under"
    );
    let started = Instant::now();
    let outcome = compio::time::timeout(authority, Box::pin(slot.run(&fixture.app, claim)))
        .await
        .unwrap_or_else(|_| {
            panic!("the retry loop outlived the lease of {authority:?} it ran under")
        });
    let elapsed = started.elapsed();
    assert_eq!(outcome.unwrap_err(), WorkflowServiceError::Timeout);
    assert!(
        elapsed >= OPERATION_BOUND,
        "the retries ended before the operation bound that ends them: {elapsed:?}"
    );
    assert!(
        fixture.metadata.requests.borrow().len() > 1,
        "an acknowledgement lost every time was never retried"
    );
    let settlements = fixture.metadata.settlements.borrow();
    assert_eq!(
        settlements.len(),
        1,
        "a retry addressed an attempt other than the one it was settling"
    );
}

#[compio::test]
async fn renewed_authority_does_not_extend_hard_execution_budget() {
    let fixture = Fixture::new(AppPolicy {
        lease_ms: 1000,
        ..AppPolicy::default()
    })
    .await;
    fixture.probe.mode.set(Mode::HardTimeout);
    let mut slot = fixture.slot(Duration::from_secs(2));
    // The first renewal is placed by the case so the execution is known to have
    // renewed before the hard budget ends it; later renewals keep the
    // transport's lease fraction, so the budget is still the bound that fires.
    let (open, gate) = oneshot::channel();
    *fixture.metadata.renewal_gate.borrow_mut() = Some(gate);
    open.send(()).unwrap();
    assert!(matches!(
        slot.run(&fixture.app, claimed(&fixture.app, fixture.lease.clone()).await.unwrap()).await,
        Err(WorkflowServiceError::Timeout)
    ));
    assert!(fixture.metadata.renewals.get() > 0);
    assert!(fixture.probe.creator_renewed.get());
    assert_eq!(fixture.probe.stops.get(), 1);
    assert!(fixture.metadata.requests.borrow().is_empty());
    assert!(fixture
        .app
        .job_receipt(&fixture.job)
        .await
        .unwrap()
        .is_none());
}

#[compio::test]
async fn cancellation_retains_slot_until_stop_joins_before_release() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    fixture.probe.mode.set(Mode::Pending);
    let (started, observed_start) = oneshot::channel();
    let (stopping, observed_stop) = oneshot::channel();
    let (release_stop, gate) = oneshot::channel();
    *fixture.probe.started.borrow_mut() = Some(started);
    *fixture.probe.stopping.borrow_mut() = Some(stopping);
    *fixture.probe.stop_gate.borrow_mut() = Some(gate);
    let mut slot = fixture.slot(Duration::from_secs(10));
    let run = slot.run(&fixture.app, claimed(&fixture.app, fixture.lease.clone()).await.unwrap()).boxed_local();
    let Either::Left((Ok(()), run)) = futures::future::select(observed_start, run).await else {
        panic!("execution must start")
    };
    drop(run);
    assert!(fixture.probe.cancels.get() > 0);
    assert!(slot.active.is_some());
    let drain = slot.drain_interrupted().boxed_local();
    let Either::Left((Ok(()), drain)) = futures::future::select(observed_stop, drain).await else {
        panic!("stop must block")
    };
    assert_eq!(fixture.probe.stops.get(), 0);
    assert_eq!(fixture.task_state().await, "leased");
    assert!(fixture.metadata.requests.borrow().is_empty());
    release_stop.send(()).unwrap();
    drain.await;
    assert_eq!(fixture.probe.stops.get(), 1);
    assert_eq!(fixture.task_state().await, "released");
    assert!(slot.active.is_none());
}

#[compio::test]
async fn substituted_renewal_stops_without_ack_or_checkpoint() {
    let fixture = Fixture::new(AppPolicy {
        lease_ms: 1000,
        ..AppPolicy::default()
    })
    .await;
    fixture.probe.mode.set(Mode::Pending);
    fixture.metadata.substitute_renewal.set(true);
    let mut slot = fixture.slot(Duration::from_secs(5));
    // Place the substituting renewal rather than race the bound against it.
    let (open, gate) = oneshot::channel();
    *fixture.metadata.renewal_gate.borrow_mut() = Some(gate);
    open.send(()).unwrap();
    assert!(matches!(
        slot.run(&fixture.app, claimed(&fixture.app, fixture.lease.clone()).await.unwrap()).await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert_eq!(
        fixture.metadata.calls_after_terminal.get(),
        0,
        "a non-retryable refusal was retried"
    );
    assert_eq!(fixture.probe.stops.get(), 1);
    assert!(fixture.metadata.requests.borrow().is_empty());
    assert!(fixture
        .app
        .job_receipt(&fixture.job)
        .await
        .unwrap()
        .is_none());
    let zeroship_core::workflow_jobs::JobOperation::Advance { run_id, .. } = &fixture.job.operation
    else {
        unreachable!()
    };
    assert_ne!(
        fixture.app.status(run_id.as_str()).await.unwrap().state,
        RunState::Completed
    );
}

/// A manager refusal on renewal is not retried and ends the attempt at once.
///
/// The transport answers `PermissionDenied` -- the manager refusing an identity
/// that cannot renew -- and the retry around a renewal must not turn that
/// durable refusal into a loop. The attempt ends at that refusal, reports it,
/// and settles nothing.
#[compio::test]
async fn a_refused_renewal_ends_the_attempt_without_retrying() {
    let fixture = Fixture::new(AppPolicy {
        lease_ms: 1000,
        ..AppPolicy::default()
    })
    .await;
    fixture.probe.mode.set(Mode::Pending);
    fixture.metadata.reject_renewal.set(true);
    let mut slot = fixture.slot(Duration::from_secs(5));
    // Place the refused renewal rather than race the bound against it.
    let (open, gate) = oneshot::channel();
    *fixture.metadata.renewal_gate.borrow_mut() = Some(gate);
    open.send(()).unwrap();
    assert!(matches!(
        slot.run(&fixture.app, claimed(&fixture.app, fixture.lease.clone()).await.unwrap()).await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert_eq!(
        fixture.metadata.calls_after_terminal.get(),
        0,
        "a manager refusal was retried"
    );
    assert_eq!(fixture.probe.stops.get(), 1);
    assert!(fixture.metadata.requests.borrow().is_empty());
    assert!(fixture
        .app
        .job_receipt(&fixture.job)
        .await
        .unwrap()
        .is_none());
}

#[compio::test]
async fn stalled_native_stop_exhausts_renewal_budget_without_reusing_slot() {
    for mode in [Mode::Complete, Mode::Failure] {
        let policy = AppPolicy {
            lease_ms: 1000,
            ..AppPolicy::default()
        };
        let fixture = Fixture::new(policy.clone()).await;
        fixture.probe.mode.set(mode);
        let (stopping, observed_stop) = oneshot::channel();
        let (release_stop, gate) = oneshot::channel();
        *fixture.probe.stopping.borrow_mut() = Some(stopping);
        *fixture.probe.stop_gate.borrow_mut() = Some(gate);
        let mut slot = fixture.slot(Duration::from_secs(10));
        let finalization = Duration::from_millis(800);
        slot.options.operation_timeout = finalization;
        let run = slot.run(&fixture.app, claimed(&fixture.app, fixture.lease.clone()).await.unwrap()).boxed_local();
        let Either::Left((Ok(()), run)) = futures::future::select(observed_stop, run).await else {
            panic!("native shutdown must reach its explicit barrier")
        };
        fixture
            .metadata
            .renewal_deadline
            .set(Some(Instant::now() + finalization));
        // Keep driving the actual slot beyond its finalization budget while
        // shutdown is explicitly blocked and renewal replies remain successful.
        let after_budget = compio::time::sleep(
            finalization + Duration::from_millis(u64::try_from(policy.lease_ms).unwrap()),
        )
        .boxed_local();
        let Either::Left(((), run)) = futures::future::select(after_budget, run).await else {
            panic!("a blocked shutdown must retain execution capacity")
        };
        assert!(fixture.metadata.renewals.get() > 0);
        assert!(!fixture.metadata.renewed_late.get());
        assert_eq!(fixture.probe.starts.get(), 1);
        assert_eq!(fixture.probe.stops.get(), 0);
        assert!(fixture.metadata.requests.borrow().is_empty());
        // The claim is still held while shutdown is blocked. Paired with the
        // release asserted below, this is what orders the two: a release that
        // ran before the executor stopped would show up here.
        assert_eq!(
            fixture.task_state().await,
            "leased",
            "the creator claim was given back before the executor stopped"
        );
        release_stop.send(()).unwrap();
        let error = run.await.unwrap_err();
        match mode {
            Mode::Complete => assert_eq!(error, WorkflowServiceError::Timeout),
            Mode::Failure => assert_eq!(
                error,
                WorkflowServiceError::InvalidRequest("injected execution failure".into())
            ),
            _ => unreachable!(),
        }
        assert_eq!(fixture.probe.stops.get(), 1);
        // The claim is NOT given back here, and that is the contract rather
        // than an omission: the error arm does attempt a release, bounded by the
        // same operation timeout this case exhausted, and that helper swallows a
        // failed attempt. So an attempt whose shutdown outlived its bound leaves
        // the claim to expire, which is what lets the manager redeliver it. The
        // drain path is where a release is joined -
        // `cancellation_retains_slot_until_stop_joins_before_release` holds that.
        assert_eq!(
            fixture.task_state().await,
            "leased",
            "a claim left to expire was instead given back inside a bound the \
             attempt had already exhausted"
        );
        assert!(slot.active.is_none());
        assert!(fixture
            .app
            .job_receipt(&fixture.job)
            .await
            .unwrap()
            .is_none());
    }
}

/// A corrupted child output is a permanent data fault, not a transient one, so
/// the retry predicate refuses it: a retry would read the same bytes and fail
/// the same way, spending the run's delivery budget without a chance to
/// recover. The control is an unavailable read, which stays retryable.
#[test]
fn a_corrupt_child_output_is_not_retryable() {
    assert!(!retryable(&WorkflowServiceError::Internal(
        "workflow child output payload is not valid JSON".into()
    )));
    assert!(retryable(&WorkflowServiceError::Unavailable(
        "workflow payload is unavailable".into()
    )));
}
