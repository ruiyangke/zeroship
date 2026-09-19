use super::*;
mod activation;
mod collection;
mod consumer;
mod cron;
mod fanout;
mod management;
mod propagation;
use crate::{
    operations::{RunState, StartOptions},
    service::{
        schema, store::OrmStore, AppPolicy, DeployRegistration, HostPolicies, PolicySnapshot,
        RequestId, TaskAssignment, WorkflowService,
    },
    WorkflowExecution,
};
use async_trait::async_trait;
use futures::channel::oneshot;
use serde_json::json;
use std::{cell::Cell, collections::BTreeMap, sync::Arc};
use zeroship_core::{
    app_id::AppId,
    schema_name::SchemaName,
    typed_id,
    workflow_coordination::{Revision, WorkerId},
    workflow_jobs::{JobOutcome, JobSpec},
};
use zeroship_data_orm::{
    binding::DbBinding, connection::ConnectionFactory, encryption::ProjectKeySource, orm::Output,
    value,
};

use crate::service::tests::deployment_fixture as deployments;

/// The delivery lease this fixture's manager transport grants and renews.
/// Tests that turn on the ratio between a lease and an execution bound derive
/// their bound from it rather than restating it.
const MANAGER_LEASE: Duration = Duration::from_secs(20);

#[derive(Clone)]
struct Lease {
    delivery: Delivery,
    expires: Instant,
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
}

#[derive(Default)]
struct Metadata {
    settlements: RefCell<BTreeMap<i64, Settlement>>,
    requests: RefCell<Vec<Settlement>>,
    lose_ack: Cell<bool>,
    reject_renewal: Cell<bool>,
    substitute_renewal: Cell<bool>,
    renewals: Cell<usize>,
    renewal_deadline: Cell<Option<Instant>>,
    renewed_late: Cell<bool>,
}
impl JobTransport for Metadata {
    type Lease = Lease;
    async fn submit(
        &self,
        _: &AssignedScope,
        _: &JobSpec,
    ) -> Result<JobSpec, WorkflowServiceError> {
        panic!("advance fixture must not publish independently")
    }
    async fn claim(&self, _: &AssignedScope) -> Result<Option<Lease>, WorkflowServiceError> {
        panic!("a delivered slot must not claim or discover work")
    }
    async fn heartbeat(&self, lease: &Lease) -> Result<Lease, WorkflowServiceError> {
        self.renewals.set(self.renewals.get() + 1);
        if self
            .renewal_deadline
            .get()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.renewed_late.set(true);
        }
        if self.reject_renewal.get() {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        let mut renewed = lease.clone();
        renewed.expires = Instant::now() + MANAGER_LEASE;
        if self.substitute_renewal.get() {
            renewed.delivery.worker_id = WorkerId::mint();
        }
        Ok(renewed)
    }
    async fn settle(
        &self,
        settlement: &Settlement,
    ) -> Result<SettlementReceipt, WorkflowServiceError> {
        self.requests.borrow_mut().push(settlement.clone());
        let mut settlements = self.settlements.borrow_mut();
        let previous = settlements
            .entry(settlement.delivery.attempt.get())
            .or_insert_with(|| settlement.clone());
        assert_eq!(
            previous, settlement,
            "a retry changed immutable settlement metadata"
        );
        if self.lose_ack.replace(false) {
            return Err(WorkflowServiceError::Timeout);
        }
        Ok(SettlementReceipt {
            job_id: settlement.delivery.job.id.clone(),
            app_id: settlement.delivery.job.app_id.clone(),
            attempt: settlement.delivery.attempt,
            outcome: settlement.outcome,
        })
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
    RevokedFrontier,
}

#[derive(Default)]
struct Probe {
    starts: Cell<usize>,
    cancels: Cell<usize>,
    stops: Cell<usize>,
    mode: Cell<Mode>,
    started: RefCell<Option<oneshot::Sender<()>>>,
    stopping: RefCell<Option<oneshot::Sender<()>>>,
    stop_gate: RefCell<Option<oneshot::Receiver<()>>>,
    creator_renewed: Cell<bool>,
    /// Withdrawn from inside app code, so the frontier reaches the runner on
    /// the same poll and the delivery authority's own arm cannot preempt it.
    revoke: RefCell<Option<crate::service::policy::PolicyBinding>>,
}

struct Executor {
    probe: Rc<Probe>,
    service: WorkflowService,
}
impl TaskExecutor for Executor {
    fn start(
        &self,
        assignment: &TaskAssignment,
        budget: super::super::ExecutionBudget,
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
        }))
    }
}

struct Execution {
    probe: Rc<Probe>,
    service: WorkflowService,
    task: String,
    initial_deadline: i64,
    interrupted: flume::Receiver<()>,
    budget: super::super::ExecutionBudget,
    stopped: bool,
    gate: Option<oneshot::Receiver<()>>,
}
#[async_trait(?Send)]
impl TaskExecution for Execution {
    async fn wait(&mut self) -> Result<WorkflowExecution, WorkflowServiceError> {
        match self.probe.mode.get() {
            Mode::Complete => {}
            Mode::Failure => {
                return Err(WorkflowServiceError::InvalidRequest(
                    "injected execution failure".into(),
                ))
            }
            Mode::Pending => std::future::pending().await,
            Mode::RevokedFrontier => {
                self.probe
                    .revoke
                    .borrow_mut()
                    .take()
                    .expect("policy binding")
                    .revoke()?;
                // Hold the thread, as synchronous app code does, until the
                // watchdog has observed the withdrawal on its own thread.
                let mut observed = self.budget.check();
                while observed.is_ok() {
                    std::hint::spin_loop();
                    observed = self.budget.check();
                }
                assert_eq!(observed, Err(crate::service::runner::BudgetEnd::Revoked));
            }
            Mode::AfterCreatorRenewal | Mode::HardTimeout => {
                loop {
                    self.budget.check()?;
                    let tx = self.service.begin().await?;
                    let Output::Rows { rows, .. } = tx
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
        WorkflowExecution::from_runtime_value(
            json!({"outcomes":[{"kind":"RunCompleted","output":{"done":true}}]}),
        )
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

/// The grant a leased fixture is built under. Host configuration carries no
/// deadline at all, so a fixture that needs one registers a remote-style lease
/// instead, long enough that registration, activation and the first claim run
/// under it. Tests that turn on a shorter window reissue the same policy
/// through [`Fixture::shorten_authority`].
const SETUP_GRANT: Duration = Duration::from_secs(3600);

/// Configured policy when `leased` names no window, a remote-style lease ending
/// that far out when it does. The revision and content are held constant so a
/// reissue narrows the window and nothing else.
fn snapshot(policy: &AppPolicy, leased: Option<Duration>) -> PolicySnapshot {
    let revision = Revision::try_from(1).unwrap();
    match leased {
        None => PolicySnapshot::configuration(revision, policy.clone()),
        Some(remaining) => {
            PolicySnapshot::lease(revision, policy.clone(), Instant::now() + remaining)
        }
    }
    .unwrap()
    .with_ingress_epoch(Some(crate::service::tests::open_epoch()))
}

struct Fixture {
    directory: tempfile::TempDir,
    deployments: deployments::Deployments,
    service: WorkflowService,
    app: AppWorkflows,
    job: JobSpec,
    lease: Lease,
    policy: AppPolicy,
    metadata: Rc<Metadata>,
    probe: Rc<Probe>,
}
impl Fixture {
    async fn new(policy: AppPolicy) -> Self {
        Self::build(policy, None).await
    }

    /// A fixture whose host authority carries a deadline, as a worker's does
    /// once its policy comes from the manager rather than from configuration.
    async fn leased(policy: AppPolicy) -> Self {
        Self::build(policy, Some(SETUP_GRANT)).await
    }

    async fn build(policy: AppPolicy, leased: Option<Duration>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let factory = ConnectionFactory::for_url(&format!(
            "sqlite:{}",
            directory.path().join("creator.sqlite").display()
        ))
        .unwrap();
        let store = OrmStore::connect(
            DbBinding::platform("workflow", "fixture", SchemaName::new("workflow").unwrap()),
            &factory,
            ProjectKeySource::unavailable(),
        )
        .await
        .unwrap();
        schema::initialize_local(&store).await.unwrap();
        let deployments = deployments::Deployments::new().await;
        let app_id = AppId::mint();
        let service = WorkflowService::open(Rc::new(store), Arc::new(HostPolicies::default()))
            .await
            .unwrap()
            .with_deployments(deployments.binding(&[&app_id]));
        service
            .fixture_register(&app_id, snapshot(&policy, leased))
            .await
            .unwrap();
        deployments
            .activate(
                &service,
                &app_id,
                &DeployRegistration {
                    id: typed_id::generate("dep"),
                    hash: "a".repeat(64),
                    workflows: ["Example".into()].into(),
                    schedules: Vec::new(),
                },
            )
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
                assignment_revision: Revision::try_from(1).unwrap(),
                attempt: Revision::try_from(1).unwrap(),
                deadline: 1.try_into().unwrap(),
            },
            expires: Instant::now() + MANAGER_LEASE,
        };
        Self {
            directory,
            deployments,
            service,
            app,
            job,
            lease,
            policy,
            metadata: Rc::new(Metadata::default()),
            probe: Rc::new(Probe::default()),
        }
    }

    /// Reissue this fixture's policy with a shorter window, as a manager does
    /// when the grant it can still stand behind has narrowed. Only a leased
    /// fixture can: a binding cannot switch between configured and leased
    /// authority, and configuration has no window to narrow.
    fn shorten_authority(&self, remaining: Duration) {
        self.service
            .fixture_install(
                self.app.app_id(),
                snapshot(&self.policy, Some(remaining)),
            )
            .unwrap();
    }

    fn slot(&self, execution_timeout: Duration) -> DeliverySlot<Metadata> {
        DeliverySlot::new(
            self.metadata.clone(),
            Rc::new(Executor {
                probe: self.probe.clone(),
                service: self.service.clone(),
            }),
            DeliveryOptions {
                execution_timeout,
                operation_timeout: Duration::from_secs(5),
                retry_delay: Duration::from_millis(5),
                reconciliation: ReconciliationOptions::default(),
                collection: crate::service::collection::CollectionOptions::default(),
                fanout: crate::service::fanout::FanoutOptions::default(),
                propagation: crate::service::propagation::PropagationOptions::default(),
            },
        )
        .unwrap()
    }
    async fn task_state(&self) -> String {
        let tx = self.service.begin().await.unwrap();
        let Output::Rows { rows, .. } = tx
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
            reconciliation: ReconciliationOptions::default(),
            collection: crate::service::collection::CollectionOptions::default(),
            fanout: crate::service::fanout::FanoutOptions::default(),
            propagation: crate::service::propagation::PropagationOptions::default(),
        },
    );
    assert!(matches!(
        result,
        Err(WorkflowServiceError::InvalidRequest(_))
    ));
    assert_eq!(fixture.probe.starts.get(), 0);
}

/// Expiry is waived when a resolved frontier is published, revocation is not.
/// App code resolves a complete frontier after its delivery authority has been
/// withdrawn; nothing from that attempt may reach the journal or the manager.
#[compio::test]
async fn revoked_authority_discards_a_resolved_frontier_instead_of_publishing_it() {
    let fixture = Fixture::new(AppPolicy {
        lease_ms: 5_000,
        ..AppPolicy::default()
    })
    .await;
    let binding = fixture
        .service
        .policies
        .current_binding(fixture.app.app_id())
        .unwrap();
    fixture.probe.revoke.replace(Some(binding));
    fixture.probe.mode.set(Mode::RevokedFrontier);
    let mut slot = fixture.slot(Duration::from_secs(5));
    assert!(Box::pin(slot.run(&fixture.app, fixture.lease.clone()))
        .await
        .is_err());
    assert_eq!(fixture.probe.starts.get(), 1);
    assert_eq!(fixture.probe.stops.get(), 1);
    assert_ne!(fixture.task_state().await, "completed");
    assert!(fixture.app.job_receipt(&fixture.job).await.unwrap().is_none());
    assert!(fixture.metadata.requests.borrow().is_empty());
}

#[compio::test]
async fn corrupted_management_outcome_cannot_reexecute_or_acknowledge_advance() {
    use zeroship_core::workflow_coordination::ManagementOutcome;

    let fixture = Fixture::new(AppPolicy::default()).await;
    let mut slot = fixture.slot(Duration::from_secs(5));
    let DeliveryOutcome::Settled { creator, .. } =
        Box::pin(slot.run(&fixture.app, fixture.lease.clone()))
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
    assert!(matches!(
        Box::pin(slot.run(&fixture.app, fixture.lease.clone())).await,
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
    } = Box::pin(slot.run(&fixture.app, fixture.lease.clone()))
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

#[compio::test]
async fn lost_ack_and_new_attempt_replay_without_executing_again() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    fixture.metadata.lose_ack.set(true);
    let mut slot = fixture.slot(Duration::from_secs(5));
    let DeliveryOutcome::Settled { creator, manager } =
        slot.run(&fixture.app, fixture.lease.clone()).await.unwrap()
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
        slot.run(&fixture.app, redelivery).await.unwrap(),
        DeliveryOutcome::Settled { .. }
    ));
    assert_eq!(fixture.probe.starts.get(), 1);
    assert_eq!(fixture.task_state().await, "completed");
    assert_eq!(
        fixture.app.job_receipt(&fixture.job).await.unwrap(),
        Some(*creator)
    );
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
    assert!(matches!(
        slot.run(&fixture.app, fixture.lease.clone()).await.unwrap(),
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
    assert_eq!(
        slot.run(&fixture.app, fixture.lease.clone())
            .await
            .unwrap_err(),
        WorkflowServiceError::Timeout
    );
    assert!(
        fixture.metadata.renewals.get() > 0,
        "an attempt that outlived its renewal delay reported nothing to the manager"
    );
    assert!(fixture.metadata.requests.borrow().is_empty());
}

/// The control for the renewal above, differing only in whether the attempt
/// outlives its renewal delay. An attempt that resolves first reports nothing,
/// which is what makes a renewal evidence that an execution began rather than
/// evidence that a delivery was made.
#[compio::test]
async fn an_attempt_resolved_before_its_renewal_delay_reports_no_renewal() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    fixture.probe.mode.set(Mode::Complete);
    let mut slot = fixture.slot(MANAGER_LEASE / 32);
    assert!(matches!(
        slot.run(&fixture.app, fixture.lease.clone()).await.unwrap(),
        DeliveryOutcome::Settled { .. }
    ));
    assert_eq!(fixture.metadata.renewals.get(), 0);
}

/// Captured host authority can end an attempt before either the manager lease
/// or the configured execution bound would, and the manager counts an attempt
/// into its delivery ceiling only on that attempt's first renewal. A renewal
/// therefore has to land inside an attempt the authority window shortens, or
/// the ceiling stops advancing while redelivery continues and nothing bounds
/// the retries. Two independent caps hold it: the creator task lease is
/// capped by the same deadline when the delivery is accepted, and the phase
/// this delay is a fraction of is built from the capped execution bound.
#[compio::test]
async fn authority_ending_before_the_lease_still_renews_inside_the_attempt() {
    let fixture = Fixture::leased(AppPolicy::default()).await;
    fixture.probe.mode.set(Mode::Pending);
    // Well under the fraction of the lease, and of the execution bound below,
    // at which a delay blind to the authority window would place the first
    // renewal.
    let window = MANAGER_LEASE / 8;
    fixture.shorten_authority(window);
    let mut slot = fixture.slot(MANAGER_LEASE);
    let started = Instant::now();
    slot.run(&fixture.app, fixture.lease.clone())
        .await
        .unwrap_err();
    // An execution that never resolves on its own ends on the authority window
    // here, not on the execution bound the slot was configured with. Without
    // this the case would still pass while the window stopped binding anything.
    let elapsed = started.elapsed();
    assert!(
        elapsed < MANAGER_LEASE / 2,
        "the attempt outlived the authority window that had to end it: {elapsed:?}"
    );
    assert!(
        fixture.metadata.renewals.get() > 0,
        "an attempt that outlived its renewal delay reported nothing to the manager"
    );
    assert!(fixture.metadata.requests.borrow().is_empty());
}

/// The control for the renewal above, differing only in whether the attempt
/// outlives its renewal delay. An attempt that resolves first reports nothing,
/// which is what keeps a renewal evidence that an execution began rather than
/// evidence that a delivery was made.
#[compio::test]
async fn authority_shortened_attempt_resolved_first_reports_no_renewal() {
    let fixture = Fixture::leased(AppPolicy::default()).await;
    fixture.probe.mode.set(Mode::Complete);
    fixture.shorten_authority(MANAGER_LEASE / 8);
    let mut slot = fixture.slot(MANAGER_LEASE);
    assert!(matches!(
        slot.run(&fixture.app, fixture.lease.clone()).await.unwrap(),
        DeliveryOutcome::Settled { .. }
    ));
    assert_eq!(fixture.metadata.renewals.get(), 0);
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
    assert!(matches!(
        slot.run(&fixture.app, fixture.lease.clone()).await,
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
    let run = slot.run(&fixture.app, fixture.lease.clone()).boxed_local();
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
    assert!(matches!(
        slot.run(&fixture.app, fixture.lease.clone()).await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert!(fixture.metadata.renewals.get() > 0);
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
        let run = slot.run(&fixture.app, fixture.lease.clone()).boxed_local();
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
        assert!(slot.active.is_none());
        assert!(fixture
            .app
            .job_receipt(&fixture.job)
            .await
            .unwrap()
            .is_none());
    }
}
