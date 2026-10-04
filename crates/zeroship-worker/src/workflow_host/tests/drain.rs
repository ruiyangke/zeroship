//! A stopping worker drains its workflow host beside its HTTP server.
//!
//! The host here is a real [`WorkflowHost`] thread running the runner's
//! [`JobConsumer`] against a manager queue and a journal of its own, with an
//! executor whose one execution the case releases. The HTTP drain is a stand-in
//! future, so the case decides how long it lasts and observes what the host
//! does while it runs.

use super::*;
use zeroship_workflow_fixtures::deployment::{Deployments, Sources};
use std::{cell::RefCell, sync::Mutex, time::Instant};
use zeroship_core::{
    typed_id,
    workflow_coordination::WorkerId,
    workflow_jobs::{ClaimJobs, JobSpec, SettlementReceipt},
    ZoneId,
};
use zeroship_data_orm::connection::ConnectionFactory;
use zeroship_workflow::{
    operations::{RunState, StartOptions},
    service::{
        delivery::{
            DeliveredTask, JobAcceptance, JobReceipt, PayloadConfirmation,
        },
        publication::JobPublisher,
        schema,
        store::HostStorage,
        AppPolicy, AppWorkflows, DeployRegistration, HostPolicies, PolicySnapshot, RequestId,
        TaskAssignment, WorkflowService,
    },
    WorkflowExecution,
};
use zeroship_workflow_manager::{
    coordinator::Coordinator, local::ConfiguredPolicies, DeliveryGrant, GiveBack,
};
use zeroship_workflow_runner::{
    consumer::{ConsumerOptions, JobConsumer},
    delivery::{
        committed_settlement, Claimed, ClaimedBatch, Completed, DeliveryOptions, JobTransport,
        Renewed, Unstarted,
    },
    prepared::{CreatorFactory, CreatorRuntime, PreparedApps, PreparedOptions},
    ExecutionBudget, TaskExecution, TaskExecutor,
};

/// What the host did, as the case reads it from its own thread.
#[derive(Default)]
struct Events {
    /// When each claim was made.
    claims: Vec<Instant>,
    /// When the delivered execution's settlement was acknowledged.
    settled: Option<Instant>,
    /// When a cancelled delivery's task was handed back to the journal.
    released: Option<Instant>,
    /// When the consumer returned from its run.
    consumer_returned: Option<Instant>,
    /// The run's state once the consumer had returned.
    final_state: Option<RunState>,
}

type Shared = Arc<Mutex<Events>>;

fn record(events: &Shared, write: impl FnOnce(&mut Events)) {
    write(&mut events.lock().expect("the event log"));
}

/// The one execution the case drives: it reports that it started, then runs
/// until the case releases it, and completes the run. One that does not `join`
/// never finishes stopping once cancelled, as a stuck native operation would.
struct HeldExecutor {
    started: RefCell<Option<oneshot::Sender<()>>>,
    release: RefCell<Option<oneshot::Receiver<()>>>,
    joins: bool,
}

impl TaskExecutor for HeldExecutor {
    fn start(
        &self,
        _assignment: &TaskAssignment,
        _budget: ExecutionBudget,
    ) -> Result<Box<dyn TaskExecution>, WorkflowServiceError> {
        Ok(Box::new(HeldExecution {
            started: self.started.borrow_mut().take(),
            release: self.release.borrow_mut().take(),
            joins: self.joins,
        }))
    }
}

struct HeldExecution {
    started: Option<oneshot::Sender<()>>,
    release: Option<oneshot::Receiver<()>>,
    joins: bool,
}

#[async_trait::async_trait(?Send)]
impl TaskExecution for HeldExecution {
    async fn wait(&mut self) -> Result<WorkflowExecution, WorkflowServiceError> {
        if let Some(started) = self.started.take() {
            let _ = started.send(());
        }
        match self.release.take() {
            Some(release) => {
                let _ = release.await;
            }
            None => std::future::pending::<()>().await,
        }
        WorkflowExecution::from_runtime_value(
            serde_json::json!({"outcomes": [{"kind": "RunCompleted"}]}),
        )
    }

    fn cancel(&mut self) {}

    async fn stop(&mut self) {
        if !self.joins {
            std::future::pending::<()>().await;
        }
    }
}

/// Prepares the one app whose journal this host holds.
struct Creator {
    journal: AppWorkflows,
    executor: Rc<HeldExecutor>,
}

impl CreatorFactory for Creator {
    type Journal = AppWorkflows;

    fn open<'a>(
        &'a self,
        app: &'a AppId,
    ) -> LocalBoxFuture<'a, Result<CreatorRuntime<AppWorkflows>, WorkflowServiceError>> {
        Box::pin(async move {
            if app != self.journal.app_id() {
                return Err(WorkflowServiceError::PermissionDenied);
            }
            Ok(CreatorRuntime {
                app: self.journal.clone(),
                executor: self.executor.clone(),
                residency: Rc::new(()),
            })
        })
    }
}

/// The manager queue a pulling worker claims from, held in process beside the
/// journal, recording every claim and settlement the host makes.
struct Queue {
    coordinator: Coordinator,
    worker: WorkerId,
    app: AppId,
    policies: ConfiguredPolicies,
    journal: AppWorkflows,
    events: Shared,
}

fn queue_error(error: zeroship_workflow_manager::Error) -> WorkflowServiceError {
    WorkflowServiceError::Unavailable(error.to_string())
}

impl Queue {
    /// Both halves of a claim this host began nothing of: the journal task this
    /// process holds the journal for, then the queue row as `defer` says.
    async fn give_back_as(
        &self,
        claimed: &Claimed<DeliveryGrant>,
        defer: GiveBack,
    ) -> Result<(), WorkflowServiceError> {
        if let Some(task) = claimed.task() {
            self.journal.release_job(task, &claimed.lease).await?;
        }
        self.coordinator
            .give_back_job(
                &self.worker,
                claimed.lease.delivery(),
                defer,
                || async { Ok(self.worker.clone()) },
            )
            .await
            .map_err(queue_error)
    }
}

impl JobTransport for Queue {
    type Lease = DeliveryGrant;
    type Journal = AppWorkflows;

    async fn claim(
        &self,
        request: &ClaimJobs,
    ) -> Result<ClaimedBatch<DeliveryGrant>, WorkflowServiceError> {
        record(&self.events, |events| events.claims.push(Instant::now()));
        let zone = ZoneId::default_zone();
        let claim = zeroship_workflow_manager::coordinator::ZoneClaim {
            worker: &self.worker,
            zone: &zone,
            request,
            deadline: self
                .coordinator
                .claim_deadline(Instant::now(), request)
                .map_err(queue_error)?,
        };
        let (batch, _) = self
            .coordinator
            .claim_in_zone(
                &claim,
                &self.policies,
                || async { Ok(self.worker.clone()) },
                |_| std::future::ready(zeroship_workflow_manager::coordinator::Admission::Deliver(())),
            )
            .await
            .map_err(queue_error)?;
        let mut deliveries = Vec::with_capacity(batch.grants.len());
        for (lease, ()) in batch.grants {
            let claimed = Claimed {
                accepted: Some(self.journal.accept_job(&lease).await?),
                lease,
            };
            if matches!(claimed.accepted, Some(JobAcceptance::Deferred { .. })) {
                self.give_back_as(&claimed, GiveBack::Backoff).await?;
                continue;
            }
            deliveries.push(claimed);
        }
        Ok(ClaimedBatch {
            deliveries,
            after: batch.after,
            lap_complete: batch.lap_complete,
        })
    }

    async fn heartbeat(
        &self,
        journal: &AppWorkflows,
        lease: &DeliveryGrant,
        task: &DeliveredTask,
    ) -> Result<Renewed<DeliveryGrant>, WorkflowServiceError> {
        let lease = self
            .coordinator
            .heartbeat_job(&self.worker, lease.delivery(), || async {
                Ok(self.worker.clone())
            })
            .await
            .map_err(queue_error)?;
        let renewal = journal.heartbeat_job(task, &lease).await?;
        Ok(Renewed { lease, renewal })
    }

    async fn settle(
        &self,
        journal: &AppWorkflows,
        lease: &DeliveryGrant,
    ) -> Result<SettlementReceipt, WorkflowServiceError> {
        let settlement = committed_settlement(journal, lease).await?;
        let receipt = self
            .coordinator
            .settle_job(&self.worker, &settlement, || async {
                Ok(self.worker.clone())
            })
            .await
            .map_err(queue_error)?;
        record(&self.events, |events| events.settled = Some(Instant::now()));
        Ok(receipt)
    }

    async fn complete(
        &self,
        journal: &AppWorkflows,
        lease: &DeliveryGrant,
        task: &DeliveredTask,
        execution: WorkflowExecution,
        confirmed: Vec<PayloadConfirmation>,
    ) -> Result<Completed, WorkflowServiceError> {
        let receipt = journal
            .complete_reported_job(task, lease, execution, &confirmed)
            .await?;
        Ok(Completed {
            settlement: JobTransport::settle(self, journal, lease).await?,
            receipt,
        })
    }

    async fn release(
        &self,
        journal: &AppWorkflows,
        lease: &DeliveryGrant,
        task: &DeliveredTask,
    ) -> Result<(), WorkflowServiceError> {
        journal.release_job(task, lease).await?;
        record(&self.events, |events| events.released = Some(Instant::now()));
        Ok(())
    }

    async fn give_back(
        &self,
        claimed: &Claimed<DeliveryGrant>,
        why: Unstarted,
    ) -> Result<(), WorkflowServiceError> {
        let defer = match why {
            Unstarted::Unprepared => GiveBack::Backoff,
            Unstarted::Stopped => GiveBack::Unsent,
        };
        self.give_back_as(claimed, defer).await
    }

    async fn receipt(
        &self,
        journal: &AppWorkflows,
        job: &JobSpec,
    ) -> Result<Option<JobReceipt>, WorkflowServiceError> {
        journal.job_receipt(job).await
    }
}

impl JobPublisher for Queue {
    fn app_id(&self) -> &AppId {
        &self.app
    }

    async fn submit(&self, job: &JobSpec) -> Result<JobSpec, WorkflowServiceError> {
        self.coordinator.queue().submit(job).await.map_err(queue_error)
    }
}

/// One app's journal, its deployment active, over a session file in
/// `directory`.
async fn one_app_journal(directory: &std::path::Path, deployments: &Deployments) -> AppWorkflows {
    let app = AppId::mint();
    let policies = Arc::new(HostPolicies::default());
    let binding = policies.bind(app.clone()).expect("bind the app's policy");
    binding
        .begin_refresh()
        .expect("refresh the app's policy")
        .install(
            PolicySnapshot::configuration(1.try_into().unwrap(), AppPolicy::default())
                .expect("a configured policy")
                .with_ingress_epoch(Some(1.try_into().unwrap())),
        )
        .expect("install the app's policy");
    let store = HostStorage::new(
        ConnectionFactory::for_platform_url(&format!(
            "sqlite:{}",
            directory.join("app.sqlite").display()
        ))
        .expect("a journal location"),
    )
    .open()
    .await
    .expect("open the journal");
    schema::initialize_local(&store).await.expect("the journal schema");
    let service = WorkflowService::open(Rc::new(store), policies)
        .await
        .expect("the journal service")
        .with_deployments(deployments.binding(&[&app]));
    let journal = service.register_app(&binding).await.expect("register the app");
    let declaration = deployments
        .publish(
            &app,
            &DeployRegistration {
                id: typed_id::generate("dep"),
                hash: "a".repeat(64),
                workflows: ["Example".into()].into(),
                schedules: Vec::new(),
            },
            &Sources::single("export class Example { run() { return 'unused'; } }"),
        )
        .await
        .expect("publish the deployment");
    Box::pin(service.activate_deploy(&app, &declaration))
        .await
        .expect("activate the deployment");
    journal
}

/// The pulling consumer a worker host runs, over `transport` and preparing
/// `journal`'s one app with `executor`, giving the deliveries running at
/// a stop `drain` to finish.
fn consumer(
    transport: Rc<Queue>,
    journal: AppWorkflows,
    executor: HeldExecutor,
    drain: Duration,
) -> JobConsumer<Queue, Creator> {
    let operation_timeout = Duration::from_secs(2);
    let worker = transport.worker.clone();
    JobConsumer::new(
        transport,
        worker,
        Rc::new(
            PreparedApps::new(
                Creator {
                    journal,
                    executor: Rc::new(executor),
                },
                Rc::new(|_: &AppId| true),
                PreparedOptions {
                    capacity: 1,
                    operation_timeout,
                },
            )
            .expect("prepared app bounds"),
        ),
        ConsumerOptions {
            slots: 1,
            idle_poll: Duration::from_millis(5),
            error_backoff: Duration::from_millis(10),
            drain,
            delivery: DeliveryOptions {
                execution_timeout: Duration::from_mins(1),
                operation_timeout,
                retry_delay: Duration::from_millis(5),
            },
        },
    )
    .expect("a consumer")
}

/// The host thread's body: one app with one published run, and a consumer
/// pulling it until the host is told to stop.
async fn pulling_one_run(
    directory: std::path::PathBuf,
    events: Shared,
    executor: HeldExecutor,
    drain: Duration,
    stopped: oneshot::Receiver<()>,
) -> Result<(), String> {
    let deployments = Deployments::new().await;
    let journal = one_app_journal(&directory, &deployments).await;
    let app = journal.app_id().clone();
    let queue = deployments
        .platform
        .queue(zeroship_workflow_manager::Options::default())
        .await
        .expect("open the queue");
    queue
        .register_scope(&app, &ZoneId::default_zone())
        .await
        .expect("register the app's scope");
    let transport = Rc::new(Queue {
        coordinator: Coordinator::new(
            queue,
            zeroship_workflow_manager::coordinator::Options::default(),
        )
        .expect("a coordinator"),
        worker: WorkerId::mint(),
        app: app.clone(),
        policies: ConfiguredPolicies::new(app, AppPolicy::default()).expect("the app's policy"),
        journal: journal.clone(),
        events: events.clone(),
    });
    let run = journal
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .expect("start a run");
    for job in journal.pending_jobs(None, 64).await.expect("pending jobs") {
        journal
            .publish_job(&job.id, transport.as_ref())
            .await
            .expect("publish the run's job");
    }
    consumer(transport, journal.clone(), executor, drain)
        .run_until(stopped.map(|_| ()))
        .await;
    let state = journal.status(&run.id).await.expect("the run's status").state;
    record(&events, |events| {
        events.consumer_returned = Some(Instant::now());
        events.final_state = Some(state);
    });
    Ok(())
}

/// A host pulling one run, whose execution has started.
struct Running {
    host: WorkflowHost,
    events: Shared,
    /// Releases the execution; held, it never finishes.
    release: oneshot::Sender<()>,
    _directory: tempfile::TempDir,
}

/// Start a host whose running deliveries get `drain` to finish after a stop,
/// and wait until its one execution is running. An execution that does not
/// `join` never finishes stopping once cancelled.
async fn running(drain: Duration, joins: bool) -> Running {
    let directory = tempfile::tempdir().expect("private journal storage");
    let events = Shared::default();
    let (started, has_started) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let executor = HeldExecutor {
        started: RefCell::new(Some(started)),
        release: RefCell::new(Some(released)),
        joins,
    };
    let host = {
        let path = directory.path().to_owned();
        let events = events.clone();
        WorkflowHost::spawn(move |stopped| {
            pulling_one_run(path, events, executor, drain, stopped)
        })
        .expect("the host thread starts")
    };
    compio::time::timeout(Duration::from_secs(30), has_started)
        .await
        .expect("the host claims the run and starts its execution")
        .expect("the execution reports its start");
    Running {
        host,
        events,
        release,
        _directory: directory,
    }
}

/// How many claims the host made after `signal`.
fn claimed_after(events: &Events, signal: Instant) -> usize {
    events.claims.iter().filter(|claim| **claim > signal).count()
}

/// On the stop signal the host stops claiming at once, and the execution it
/// was running when the signal arrived finishes and settles WHILE HTTP drains,
/// all within the drain budget.
///
/// The HTTP drain here lasts until the host has finished, or until the host
/// claims again: the one would show the two drains overlapping, the other is
/// the failure this case exists to catch. A host told to stop only once HTTP
/// had drained would still be pulling during the drain, and would claim.
#[compio::test]
async fn a_stop_drains_the_running_execution_beside_http_and_claims_nothing_after_it() {
    let budget = Duration::from_secs(20);
    let Running {
        host,
        events,
        release,
        _directory,
    } = running(budget.saturating_sub(Duration::from_secs(5)), true).await;
    let signal = Instant::now();
    let http = async {
        release.send(()).expect("the execution waits to be released");
        compio::time::timeout(budget, async {
            loop {
                {
                    let events = events.lock().expect("the event log");
                    if events.consumer_returned.is_some() || claimed_after(&events, signal) > 0 {
                        break;
                    }
                }
                compio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the host finishes or claims inside the drain budget");
        Instant::now()
    };

    let (http_drained, drained) = compio::time::timeout(
        budget * 2,
        drain(std::future::ready(()), http, Some(host), budget),
    )
    .await
    .expect("the drain ends");
    drained.expect("the host drained");

    let events = std::mem::take(&mut *events.lock().expect("the event log"));
    assert!(
        !events.claims.is_empty(),
        "the premise: the host claimed the run before the signal"
    );
    assert_eq!(
        claimed_after(&events, signal),
        0,
        "a stopped host claims nothing after the signal"
    );
    let settled = events
        .settled
        .expect("the execution running at the signal settled");
    assert!(
        settled > signal && settled <= http_drained,
        "the execution settled while HTTP drained"
    );
    assert_eq!(events.final_state, Some(RunState::Completed));
    assert!(events.released.is_none(), "nothing was cancelled");
    assert!(signal.elapsed() < budget, "the drain fits its budget");
}

/// A delivery still running when the host's grace runs out is cancelled then,
/// not at the stop, and released inside the drain budget; the host then ends
/// on its own.
#[compio::test]
async fn a_delivery_the_grace_cannot_finish_is_released_inside_the_drain() {
    let grace = Duration::from_millis(300);
    let budget = Duration::from_secs(10);
    let Running {
        host,
        events,
        release: _held,
        _directory,
    } = running(grace, true).await;
    let signal = Instant::now();
    let (served, drained) = compio::time::timeout(
        budget * 2,
        drain(
            std::future::ready(()),
            std::future::ready("served"),
            Some(host),
            budget,
        ),
    )
    .await
    .expect("the drain ends");
    assert_eq!(served, "served");
    drained.expect("the host drained after releasing what it could not finish");
    assert!(signal.elapsed() < budget, "the drain fits its budget");

    let events = std::mem::take(&mut *events.lock().expect("the event log"));
    let released = events
        .released
        .expect("the delivery the grace could not finish was released");
    assert!(
        released >= signal + grace,
        "the delivery was cancelled when the grace ran out, not at the stop"
    );
    assert!(events.settled.is_none());
    assert_ne!(events.final_state, Some(RunState::Completed));
    assert_eq!(claimed_after(&events, signal), 0);
}

/// A host that cannot drain is left at the budget: the drain reports it and
/// returns, rather than holding the worker past the bound its termination grace
/// is stated from.
///
/// The execution here never finishes stopping once its grace cancels it. The
/// control is the case above, where the same cancellation joins and the host
/// drains well inside its budget.
#[compio::test]
async fn a_host_that_cannot_drain_is_left_at_the_budget() {
    let Running {
        host,
        events: _events,
        release: _held,
        _directory,
    } = running(Duration::from_millis(100), false).await;
    let budget = Duration::from_secs(1);
    let signal = Instant::now();
    let (served, drained) = compio::time::timeout(
        Duration::from_secs(10),
        drain(
            std::future::ready(()),
            std::future::ready("served"),
            Some(host),
            budget,
        ),
    )
    .await
    .expect("the drain returns at its budget instead of waiting for the host");
    assert_eq!(served, "served");
    let refusal = drained.expect_err("the cancelled execution never finished stopping");
    assert!(refusal.contains("did not drain within 1s"), "{refusal}");
    assert!(signal.elapsed() >= budget, "the host had its whole budget");
}
