use super::*;
use crate::consumer::{ConsumerOptions, JobConsumer};
use crate::prepared::{AppFeed, CreatorFactory, CreatorRuntime, PreparedApps, PreparedOptions};
use crate::PayloadObjects;
use futures::future::LocalBoxFuture;
use std::collections::{BTreeSet, VecDeque};
use zeroship_core::{workflow_jobs::DeploymentId, ZoneId};
use zeroship_workflow_manager::{coordinator::Coordinator, local::ConfiguredPolicies};

enum Event {
    Claimed,
    Settled,
    GivenBack,
}

/// One reply a case scripts, answered before the queue's own.
struct Reply {
    deliveries: Vec<Lease>,
    after: Option<AppId>,
    lap_complete: bool,
    /// Each delivery is accepted under its lease, then handed over with that
    /// lease already spent, as a reply delayed past its grant arrives.
    spent: bool,
}

/// A zone queue in this process, holding each app's journal.
///
/// Unscripted, a claim walks the apps holding work in id order from the
/// request's cursor, wrapping once, takes at most one job per app and at most
/// `max` in all, skips excluded apps and reports whether it reached the end:
/// the batch the service answers, without its leases or its policy.
struct Queue {
    metadata: Metadata,
    journals: RefCell<BTreeMap<AppId, AppWorkflows>>,
    jobs: RefCell<BTreeMap<AppId, VecDeque<Lease>>>,
    script: RefCell<VecDeque<Reply>>,
    requests: RefCell<Vec<(Instant, ClaimJobs)>>,
    given_back: RefCell<Vec<(Instant, AppId, bool, Unstarted)>>,
    /// When each settlement reached the queue.
    settled: RefCell<Vec<Instant>>,
    /// How long each give-back takes before the queue takes the row back.
    give_back_delay: Cell<Duration>,
    /// Told when the first give-back reaches the queue, before its delay.
    give_back_started: RefCell<Option<oneshot::Sender<()>>>,
    events: flume::Sender<Event>,
    responses: flume::Receiver<Event>,
    claim_gates: RefCell<VecDeque<oneshot::Receiver<()>>>,
    claim_error: RefCell<Option<WorkflowServiceError>>,
}

impl Queue {
    fn new(fixtures: &[&Fixture]) -> Rc<Self> {
        let (events, responses) = flume::unbounded();
        let queue = Rc::new(Self {
            metadata: Metadata::default(),
            journals: RefCell::new(BTreeMap::new()),
            jobs: RefCell::new(BTreeMap::new()),
            script: RefCell::new(VecDeque::new()),
            requests: RefCell::new(Vec::new()),
            given_back: RefCell::new(Vec::new()),
            settled: RefCell::new(Vec::new()),
            give_back_delay: Cell::new(Duration::ZERO),
            give_back_started: RefCell::new(None),
            events,
            responses,
            claim_gates: RefCell::new(VecDeque::new()),
            claim_error: RefCell::new(None),
        });
        for fixture in fixtures {
            queue.register(fixture);
            queue.push(fixture.lease.clone());
        }
        queue
    }

    /// Hold `fixture`'s journal without queueing its job.
    fn register(&self, fixture: &Fixture) {
        self.journals
            .borrow_mut()
            .insert(fixture.app.app_id().clone(), fixture.app.clone());
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

    async fn claims(&self, count: usize) {
        for _ in 0..count {
            while !matches!(self.responses.recv_async().await.unwrap(), Event::Claimed) {}
        }
    }

    fn requests(&self) -> Vec<ClaimJobs> {
        self.requests
            .borrow()
            .iter()
            .map(|(_, request)| request.clone())
            .collect()
    }

    fn answer(&self, request: &ClaimJobs) -> (Vec<Lease>, Option<AppId>, bool) {
        let mut jobs = self.jobs.borrow_mut();
        let apps: Vec<AppId> = jobs
            .iter()
            .filter(|(_, queued)| !queued.is_empty())
            .map(|(app, _)| app.clone())
            .collect();
        let start = request
            .after
            .as_ref()
            .map_or(0, |after| apps.iter().position(|app| app > after).unwrap_or(apps.len()));
        let max = usize::try_from(request.max.get()).unwrap();
        let mut deliveries = Vec::new();
        let mut after = request.after.clone();
        let mut lap_complete = true;
        for app in apps[start..].iter().chain(&apps[..start]) {
            if deliveries.len() >= max {
                lap_complete = false;
                break;
            }
            after = Some(app.clone());
            if request.exclude.contains(app) {
                continue;
            }
            deliveries.extend(jobs.get_mut(app).and_then(VecDeque::pop_front));
        }
        (deliveries, after, lap_complete)
    }
}

impl JobTransport for Queue {
    type Lease = Lease;
    type Journal = AppWorkflows;

    async fn claim(&self, request: &ClaimJobs) -> Result<ClaimedBatch<Lease>, WorkflowServiceError> {
        self.requests
            .borrow_mut()
            .push((Instant::now(), request.clone()));
        self.events.send(Event::Claimed).unwrap();
        let gate = self.claim_gates.borrow_mut().pop_front();
        if let Some(gate) = gate {
            let _ = gate.await;
        }
        if let Some(error) = self.claim_error.borrow_mut().take() {
            return Err(error);
        }
        let scripted = self.script.borrow_mut().pop_front();
        let (leases, after, lap_complete, spent) = if let Some(reply) = scripted {
            (reply.deliveries, reply.after, reply.lap_complete, reply.spent)
        } else {
            let (leases, after, lap_complete) = self.answer(request);
            (leases, after, lap_complete, false)
        };
        let mut deliveries = Vec::new();
        for mut lease in leases {
            let journal = self
                .journals
                .borrow()
                .get(&lease.delivery.job.app_id)
                .cloned()
                .expect("a queued job's app has a registered journal");
            let accepted = Some(journal.accept_job(&lease).await?);
            if spent {
                lease.expires = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
            }
            deliveries.push(Claimed { lease, accepted });
        }
        Ok(ClaimedBatch {
            deliveries,
            after,
            lap_complete,
        })
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
        journal: &AppWorkflows,
        lease: &Lease,
    ) -> Result<SettlementReceipt, WorkflowServiceError> {
        let settlement = committed_settlement(journal, lease).await?;
        self.settled.borrow_mut().push(Instant::now());
        self.events.send(Event::Settled).unwrap();
        Ok(SettlementReceipt {
            job_id: settlement.delivery().job.id.clone(),
            app_id: settlement.delivery().job.app_id.clone(),
            attempt: settlement.delivery().attempt,
            outcome: settlement.outcome().clone(),
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
        Ok(Completed {
            settlement: JobTransport::settle(self, journal, lease).await?,
            receipt,
        })
    }

    async fn release(
        &self,
        journal: &AppWorkflows,
        lease: &Lease,
        task: &DeliveredTask,
    ) -> Result<(), WorkflowServiceError> {
        journal.release_job(task, lease).await
    }

    /// Both halves, as the service gives them back: the journal task this
    /// process holds the journal for, then the queue row.
    async fn give_back(
        &self,
        claimed: &Claimed<Lease>,
        why: Unstarted,
    ) -> Result<(), WorkflowServiceError> {
        if let Some(started) = self.give_back_started.borrow_mut().take() {
            let _ = started.send(());
        }
        compio::time::sleep(self.give_back_delay.get()).await;
        let app = claimed.lease.delivery.job.app_id.clone();
        if let Some(task) = claimed.task() {
            let journal = self.journals.borrow().get(&app).cloned().unwrap();
            journal.release_job(task, &claimed.lease).await?;
        }
        self.given_back
            .borrow_mut()
            .push((Instant::now(), app, claimed.task().is_some(), why));
        self.events.send(Event::GivenBack).unwrap();
        Ok(())
    }

    async fn receipt(
        &self,
        journal: &AppWorkflows,
        job: &JobSpec,
    ) -> Result<Option<JobReceipt>, WorkflowServiceError> {
        journal.job_receipt(job).await
    }
}

/// An app's journal and the executor that runs its tasks.
type Creator = (AppWorkflows, Rc<dyn TaskExecutor>);

/// Creator resources for the fixtures a case registers, opened on demand.
#[derive(Default)]
struct Creators {
    apps: RefCell<BTreeMap<AppId, Creator>>,
    opened: RefCell<BTreeMap<AppId, usize>>,
    failing: RefCell<BTreeSet<AppId>>,
    listed: RefCell<BTreeSet<AppId>>,
    resident: Rc<RefCell<BTreeMap<AppId, usize>>>,
}

impl Creators {
    fn new(fixtures: &[&Fixture]) -> Rc<Self> {
        let creators = Rc::new(Self::default());
        for fixture in fixtures {
            let app = fixture.app.app_id().clone();
            creators.apps.borrow_mut().insert(
                app.clone(),
                (
                    fixture.app.clone(),
                    Rc::new(Executor {
                        probe: fixture.probe.clone(),
                        service: fixture.service.clone(),
                    }),
                ),
            );
            creators.listed.borrow_mut().insert(app);
        }
        creators
    }

    fn feed(self: &Rc<Self>) -> AppFeed {
        let creators = self.clone();
        Rc::new(move |app: &AppId| creators.listed.borrow().contains(app))
    }

    fn opened(&self, app: &AppId) -> usize {
        self.opened.borrow().get(app).copied().unwrap_or(0)
    }

    fn resident(&self, app: &AppId) -> usize {
        self.resident.borrow().get(app).copied().unwrap_or(0)
    }
}

/// What a prepared app holds for as long as anything holds it.
#[derive(Debug)]
struct Resident {
    app: AppId,
    live: Rc<RefCell<BTreeMap<AppId, usize>>>,
}

impl Drop for Resident {
    fn drop(&mut self) {
        *self.live.borrow_mut().get_mut(&self.app).unwrap() -= 1;
    }
}

struct Opener(Rc<Creators>);

impl CreatorFactory for Opener {
    type Journal = AppWorkflows;

    fn open<'a>(
        &'a self,
        app: &'a AppId,
    ) -> LocalBoxFuture<'a, Result<CreatorRuntime<AppWorkflows>, WorkflowServiceError>> {
        async move {
            *self.0.opened.borrow_mut().entry(app.clone()).or_default() += 1;
            if self.0.failing.borrow().contains(app) {
                return Err(WorkflowServiceError::Unavailable(
                    "injected preparation failure".into(),
                ));
            }
            let (journal, executor) = self
                .0
                .apps
                .borrow()
                .get(app)
                .cloned()
                .ok_or(WorkflowServiceError::PermissionDenied)?;
            *self.0.resident.borrow_mut().entry(app.clone()).or_default() += 1;
            Ok(CreatorRuntime {
                app: journal,
                executor,
                residency: Rc::new(Resident {
                    app: app.clone(),
                    live: self.0.resident.clone(),
                }),
            })
        }
        .boxed_local()
    }
}

fn options(slots: usize) -> ConsumerOptions {
    ConsumerOptions {
        slots,
        idle_poll: Duration::from_millis(5),
        error_backoff: Duration::from_millis(10),
        drain: Duration::ZERO,
        delivery: DeliveryOptions {
            execution_timeout: Duration::from_secs(10),
            operation_timeout: Duration::from_secs(1),
            retry_delay: Duration::from_millis(5),
        },
    }
}

fn prepared(creators: &Rc<Creators>) -> Rc<PreparedApps<Opener>> {
    Rc::new(
        PreparedApps::new(
            Opener(creators.clone()),
            creators.feed(),
            PreparedOptions {
                capacity: 8,
                operation_timeout: Duration::from_secs(1),
            },
        )
        .unwrap(),
    )
}

fn consumer<T: JobTransport<Journal = AppWorkflows>>(
    transport: Rc<T>,
    worker: &WorkerId,
    creators: &Rc<Creators>,
    options: ConsumerOptions,
) -> JobConsumer<T, Opener> {
    JobConsumer::new(transport, worker.clone(), prepared(creators), options).unwrap()
}

async fn finished(future: impl Future<Output = ()>) {
    compio::time::timeout(Duration::from_secs(15), future.boxed_local())
        .await
        .expect("consumer test finished");
}

/// Two fixtures under one worker, ordered by app id so a batch's order is known.
async fn pair(policy: &AppPolicy) -> (Fixture, Fixture) {
    let mut first = Fixture::new(policy.clone()).await;
    let mut second = Fixture::new(policy.clone()).await;
    if first.app.app_id() > second.app.app_id() {
        std::mem::swap(&mut first, &mut second);
    }
    second.lease.delivery.worker_id = first.lease.delivery.worker_id.clone();
    (first, second)
}

/// A second job of `fixture`'s app, from a second run.
async fn another_lease(fixture: &Fixture) -> Lease {
    fixture
        .app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let job = fixture
        .app
        .pending_jobs(None, 8)
        .await
        .unwrap()
        .into_iter()
        .find(|job| job.id != fixture.job.id)
        .expect("the second run's first job");
    let mut lease = fixture.lease.clone();
    lease.delivery.job = job;
    lease
}

/// The claimer asks for exactly its free slots, sends back the cursor the
/// previous reply returned, claims again at once after an empty reply that did
/// not reach the end of the zone, and waits the idle interval after one that
/// did.
#[compio::test]
async fn claims_ask_for_the_free_slots_and_continue_from_the_cursor() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    fixture.probe.mode.set(Mode::Pending);
    let queue = Queue::new(&[]);
    queue.register(&fixture);
    let visited = AppId::mint();
    queue.script.borrow_mut().extend([
        Reply {
            deliveries: vec![fixture.lease.clone()],
            after: Some(fixture.app.app_id().clone()),
            lap_complete: false,
            spent: false,
        },
        Reply {
            deliveries: Vec::new(),
            after: Some(visited.clone()),
            lap_complete: false,
            spent: false,
        },
        Reply {
            deliveries: Vec::new(),
            after: None,
            lap_complete: true,
            spent: false,
        },
    ]);
    let creators = Creators::new(&[&fixture]);
    let idle = Duration::from_secs(2);
    let mut host = consumer(
        queue.clone(),
        &fixture.lease.delivery.worker_id,
        &creators,
        ConsumerOptions {
            idle_poll: idle,
            ..options(2)
        },
    );
    finished(host.run_until(queue.claims(4))).await;
    let requests = queue.requests();
    let at: Vec<Instant> = queue.requests.borrow().iter().map(|(at, _)| *at).collect();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].max.get(), 2, "both slots were free");
    assert_eq!(requests[0].after, None);
    assert_eq!(requests[1].max.get(), 1, "one slot runs the delivered job");
    assert_eq!(requests[1].after.as_ref(), Some(fixture.app.app_id()));
    assert_eq!(requests[2].max.get(), 1);
    assert_eq!(requests[2].after, Some(visited));
    assert!(
        at[2] - at[1] < idle / 2,
        "an empty reply short of the zone's end waited before claiming again"
    );
    assert!(
        at[3] - at[2] >= idle,
        "a reply that reached the zone's end did not wait the idle interval"
    );
    assert_eq!(requests[3].after, None);
    assert!(requests.iter().all(|request| request.exclude.is_empty()));
    assert_eq!(fixture.probe.starts.get(), 1);
    assert_eq!(fixture.probe.stops.get(), 1, "shutdown joins the running execution");
}

/// A slot that frees during the idle wait claims at once: the settlement that
/// freed it may have published the run's next job.
#[compio::test]
async fn a_slot_freed_during_the_idle_wait_claims_at_once() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    fixture.probe.mode.set(Mode::CompleteAfter);
    fixture.probe.hold.set(Duration::from_millis(100));
    let queue = Queue::new(&[]);
    queue.register(&fixture);
    queue.script.borrow_mut().push_back(Reply {
        deliveries: vec![fixture.lease.clone()],
        after: Some(fixture.app.app_id().clone()),
        lap_complete: true,
        spent: false,
    });
    let creators = Creators::new(&[&fixture]);
    let mut host = consumer(
        queue.clone(),
        &fixture.lease.delivery.worker_id,
        &creators,
        ConsumerOptions {
            idle_poll: Duration::from_mins(10),
            ..options(2)
        },
    );
    finished(host.run_until(async {
        // The first claim's event precedes the settlement and is consumed with
        // it, so the next claim is the one the freed slot makes.
        queue.settlements(1).await;
        queue.claims(1).await;
    }))
    .await;
    let requests = queue.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].max.get(), 2, "the freed slot is offered again");
}

/// The first delivery of an app prepares it; a later delivery of the same app
/// reuses the prepared entry rather than opening it again.
#[compio::test]
async fn a_second_delivery_of_an_app_reuses_its_prepared_entry() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let second = another_lease(&fixture).await;
    let queue = Queue::new(&[&fixture]);
    queue.push(second);
    let creators = Creators::new(&[&fixture]);
    let mut host = consumer(
        queue.clone(),
        &fixture.lease.delivery.worker_id,
        &creators,
        options(1),
    );
    finished(host.run_until(queue.settlements(2))).await;
    assert_eq!(fixture.probe.starts.get(), 2);
    assert_eq!(creators.opened(fixture.app.app_id()), 1);
    assert_eq!(creators.resident(fixture.app.app_id()), 1, "the entry stays cached");
}

/// One batch runs each app against its own journal, and an app the queue never
/// delivered is never prepared.
#[compio::test]
async fn each_delivery_runs_against_its_own_apps_journal() {
    let (left, right) = pair(&AppPolicy::default()).await;
    let foreign = Fixture::new(AppPolicy::default()).await;
    let queue = Queue::new(&[&left, &right]);
    let creators = Creators::new(&[&left, &right, &foreign]);
    let mut host = consumer(queue.clone(), &left.lease.delivery.worker_id, &creators, options(2));
    finished(host.run_until(queue.settlements(2))).await;
    assert_eq!(queue.requests()[0].max.get(), 2);
    assert_eq!(left.probe.starts.get(), 1);
    assert_eq!(right.probe.starts.get(), 1);
    assert_eq!(foreign.probe.starts.get(), 0);
    assert_eq!(creators.opened(foreign.app.app_id()), 0);
    assert_eq!(left.task_state().await, "completed");
    assert_eq!(right.task_state().await, "completed");
    assert!(foreign
        .app
        .job_receipt(&foreign.job)
        .await
        .unwrap()
        .is_none());
}

/// A failed preparation gives the claim back, journal task included, and puts
/// the app on this worker's skip list: the next claim excludes it, and once the
/// entry expires, claims stop excluding it.
#[compio::test]
async fn a_failed_preparation_gives_back_and_excludes_the_app_until_it_expires() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let queue = Queue::new(&[&fixture]);
    let creators = Creators::new(&[&fixture]);
    creators
        .failing
        .borrow_mut()
        .insert(fixture.app.app_id().clone());
    let backoff = Duration::from_millis(300);
    let mut host = consumer(
        queue.clone(),
        &fixture.lease.delivery.worker_id,
        &creators,
        ConsumerOptions {
            error_backoff: backoff,
            idle_poll: Duration::from_millis(20),
            ..options(1)
        },
    );
    let app = fixture.app.app_id().clone();
    finished(host.run_until(async {
        while !matches!(queue.responses.recv_async().await.unwrap(), Event::GivenBack) {}
        // Claims continue at the idle interval until one leaves out
        // the app; the entry's expiry is what ends the exclusion.
        loop {
            queue.claims(1).await;
            if queue
                .requests
                .borrow()
                .last()
                .is_some_and(|(_, request)| !request.exclude.contains(&app))
            {
                break;
            }
        }
    }))
    .await;
    let given_back = queue.given_back.borrow().clone();
    assert_eq!(given_back.len(), 1);
    let (returned_at, returned, with_task, why) = &given_back[0];
    assert_eq!(*why, Unstarted::Unprepared);
    assert_eq!(returned, &app);
    assert!(with_task, "the journal accepted execution, so its task went back too");
    assert_eq!(fixture.task_state().await, "released");
    assert_eq!(fixture.probe.starts.get(), 0);
    let after: Vec<_> = queue
        .requests
        .borrow()
        .iter()
        .filter(|(at, _)| at > returned_at)
        .cloned()
        .collect();
    assert!(
        after.first().is_some_and(|(_, request)| request.exclude == [app.clone()]),
        "the claim after the failure did not exclude the app"
    );
    let (released_at, _) = after.last().unwrap();
    assert!(
        *released_at - *returned_at >= backoff / 2,
        "the exclusion ended before the entry's expiry"
    );
}

/// An app the version feed stops listing leaves the prepared cache on the next
/// claim cycle, taking its residency with it. The control is a listed app
/// prepared in the same run, which stays.
#[compio::test]
async fn an_app_the_feed_stops_listing_is_dropped_on_the_next_claim_cycle() {
    let (deleted, kept) = pair(&AppPolicy::default()).await;
    let queue = Queue::new(&[&deleted, &kept]);
    let creators = Creators::new(&[&deleted, &kept]);
    let mut host = consumer(
        queue.clone(),
        &deleted.lease.delivery.worker_id,
        &creators,
        options(2),
    );
    finished(host.run_until(async {
        queue.settlements(2).await;
        assert_eq!(creators.resident(deleted.app.app_id()), 1);
        creators.listed.borrow_mut().remove(deleted.app.app_id());
        // Every event still queued predates the removal; the next claim is
        // sent after the cycle that begins with it prunes the cache.
        while queue.responses.try_recv().is_ok() {}
        queue.claims(1).await;
    }))
    .await;
    assert_eq!(
        creators.resident(deleted.app.app_id()),
        0,
        "an app absent from the feed stayed prepared"
    );
    assert_eq!(creators.resident(kept.app.app_id()), 1);
}

/// An execution interrupted by a renewal holds its slot until its native
/// shutdown joins, so the next delivery waits rather than sharing the slot.
#[compio::test]
async fn an_interrupted_execution_holds_its_slot_until_it_joins() {
    let policy = AppPolicy {
        lease_ms: 900,
        ..AppPolicy::default()
    };
    let (first, second) = pair(&policy).await;
    first.probe.mode.set(Mode::Pending);
    let (started, running) = oneshot::channel();
    first.probe.started.replace(Some(started));
    let (stopping, stopped) = oneshot::channel();
    first.probe.stopping.replace(Some(stopping));
    let (release, gate) = oneshot::channel();
    first.probe.stop_gate.replace(Some(gate));
    let queue = Queue::new(&[&first, &second]);
    let creators = Creators::new(&[&first, &second]);
    let mut host = consumer(queue.clone(), &first.lease.delivery.worker_id, &creators, options(1));
    let (stop_host, shutdown) = oneshot::channel();
    let update = async {
        running.await.unwrap();
        first.reissue(
            &AppPolicy {
                dispatch: false,
                ..policy.clone()
            },
            2,
        );
        stopped.await.unwrap();
        assert!(first.probe.cancels.get() > 0);
        assert_eq!(first.probe.stops.get(), 0);
        let claimed = queue.requests().len();
        compio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            queue.requests().len(),
            claimed,
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
}

/// A refused claim waits the error back-off before the next one.
#[compio::test]
async fn a_refused_claim_backs_off_before_claiming_again() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let queue = Queue::new(&[&fixture]);
    queue
        .claim_error
        .replace(Some(WorkflowServiceError::Unavailable("refused".into())));
    let creators = Creators::new(&[&fixture]);
    let backoff = Duration::from_millis(200);
    let mut host = consumer(
        queue.clone(),
        &fixture.lease.delivery.worker_id,
        &creators,
        ConsumerOptions {
            error_backoff: backoff,
            ..options(1)
        },
    );
    finished(host.run_until(queue.settlements(1))).await;
    let at: Vec<Instant> = queue.requests.borrow().iter().map(|(at, _)| *at).collect();
    assert!(at.len() >= 2);
    assert!(
        at[1] - at[0] >= backoff,
        "a refused claim was retried before its back-off"
    );
    assert_eq!(fixture.task_state().await, "completed");
}

/// A stop that arrives while a claim is pending does not drop the claim: the
/// service committed whatever its reply brings, so the claim runs to that reply
/// and each delivery goes back unstarted, with the journal task accepted for
/// it, and no creator work starts. The control is the next host, which claims
/// the given-back job and runs it.
///
/// The stop is sent before the claim's gate opens, so the reply can only
/// arrive after the stop.
#[compio::test]
async fn a_stop_during_a_claim_gives_back_what_the_claim_delivers_unstarted() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let queue = Queue::new(&[&fixture]);
    let (release, gate) = oneshot::channel();
    queue.claim_gates.borrow_mut().push_back(gate);
    let creators = Creators::new(&[&fixture]);
    let mut host = consumer(queue.clone(), &fixture.lease.delivery.worker_id, &creators, options(1));
    let (stop, stopped) = oneshot::channel::<()>();
    let driver = async {
        queue.claims(1).await;
        stop.send(()).unwrap();
        release.send(()).unwrap();
    };
    finished(async {
        futures::join!(host.run_until(stopped.map(|_| ())), driver);
    })
    .await;
    assert_eq!(fixture.probe.starts.get(), 0);
    let given_back = queue.given_back.borrow().clone();
    let [(_, returned, with_task, why)] = <[_; 1]>::try_from(given_back)
        .unwrap_or_else(|given_back| panic!("the delivered job goes back: {given_back:?}"));
    assert_eq!(&returned, fixture.app.app_id());
    assert!(with_task, "the journal accepted execution, so its task went back too");
    assert_eq!(why, Unstarted::Stopped);
    assert_eq!(fixture.task_state().await, "released");
    assert!(fixture.app.job_receipt(&fixture.job).await.unwrap().is_none());

    queue.push(fixture.lease.clone());
    drop(host);
    let mut host = consumer(queue.clone(), &fixture.lease.delivery.worker_id, &creators, options(1));
    finished(host.run_until(queue.settlements(1))).await;
    assert_eq!(fixture.probe.starts.get(), 1);
}

/// A stop during a claim that delivers several jobs gives every one of them
/// back unstarted, each with the journal task accepted for it, and starts no
/// creator work for any of them.
///
/// The stop is sent before the claim's gate opens, so the reply can only arrive
/// after the stop; the claim asks for both slots and the queue holds a job for
/// each app, so the one reply carries both. Each give-back releases a journal
/// task, so it runs under the fixture's operation bound.
#[compio::test]
async fn a_stop_during_a_claim_gives_back_every_delivery_the_claim_brings() {
    let (first, second) = pair(&AppPolicy::default()).await;
    let queue = Queue::new(&[&first, &second]);
    let (release, gate) = oneshot::channel();
    queue.claim_gates.borrow_mut().push_back(gate);
    let creators = Creators::new(&[&first, &second]);
    let bounds = ConsumerOptions {
        delivery: DeliveryOptions {
            operation_timeout: OPERATION_BOUND,
            ..options(2).delivery
        },
        ..options(2)
    };
    let mut host = consumer(queue.clone(), &first.lease.delivery.worker_id, &creators, bounds);
    let (stop, stopped) = oneshot::channel::<()>();
    let driver = async {
        queue.claims(1).await;
        stop.send(()).unwrap();
        release.send(()).unwrap();
    };
    finished(async {
        futures::join!(host.run_until(stopped.map(|_| ())), driver);
    })
    .await;
    assert_eq!(queue.requests().len(), 1, "nothing was claimed after the stop");
    assert_eq!(queue.requests()[0].max.get(), 2);
    let mut given_back: Vec<(AppId, bool, Unstarted)> = queue
        .given_back
        .borrow()
        .iter()
        .map(|(_, app, with_task, why)| (app.clone(), *with_task, *why))
        .collect();
    given_back.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(
        given_back,
        [
            (first.app.app_id().clone(), true, Unstarted::Stopped),
            (second.app.app_id().clone(), true, Unstarted::Stopped),
        ],
        "every delivery of the reply goes back, with its journal task"
    );
    for fixture in [&first, &second] {
        assert_eq!(fixture.probe.starts.get(), 0);
        assert_eq!(fixture.task_state().await, "released");
        assert!(fixture.app.job_receipt(&fixture.job).await.unwrap().is_none());
    }
}

/// The grace a stop leaves the running deliveries is one deadline counted from
/// the stop. A claim pending at the stop runs on to its reply and gives back
/// what it brings beside the running delivery, which is driven the whole time:
/// the delivery finishes and settles inside the drain, the claim's delivery
/// goes back, and the consumer returns inside the drain and one operation bound
/// of the stop.
///
/// The running delivery waits on a gate the case opens once the give-back has
/// reached the queue, and that give-back takes the whole drain. A consumer that
/// paused the running delivery while it gave back, or that started the grace
/// only once the give-backs ended, settles it no sooner than the drain after
/// the stop. The drain is the fixture's operation bound, so the settlement
/// that must land inside it is real journal I/O it cannot plausibly miss.
#[compio::test]
async fn a_claim_pending_at_the_stop_spends_none_of_the_running_deliverys_drain() {
    let (first, second) = pair(&AppPolicy::default()).await;
    let drain = OPERATION_BOUND;
    first.probe.mode.set(Mode::Gated);
    let (open, gate) = oneshot::channel();
    first.probe.release.replace(Some(gate));
    let (started, running) = oneshot::channel();
    first.probe.started.replace(Some(started));
    // Only the first app has a job at the start, so the first claim delivers it
    // alone and the next one, for the free slot, is the claim the stop finds.
    let queue = Queue::new(&[&first]);
    queue.register(&second);
    let (opened, ungated) = oneshot::channel();
    opened.send(()).unwrap();
    let (release, claim_gate) = oneshot::channel();
    queue.claim_gates.borrow_mut().extend([ungated, claim_gate]);
    queue.give_back_delay.set(drain);
    let (giving_back, given) = oneshot::channel();
    queue.give_back_started.replace(Some(giving_back));
    let creators = Creators::new(&[&first, &second]);
    let bounds = ConsumerOptions {
        drain,
        delivery: DeliveryOptions {
            // Above the give-back's delay, so the give-back runs to its end.
            operation_timeout: drain * 2,
            ..options(2).delivery
        },
        ..options(2)
    };
    let mut host = consumer(queue.clone(), &first.lease.delivery.worker_id, &creators, bounds);
    let (stop, stopped) = oneshot::channel::<()>();
    let stopped_at = Cell::new(None);
    let driver = async {
        running.await.unwrap();
        queue.claims(2).await;
        queue.push(second.lease.clone());
        stopped_at.set(Some(Instant::now()));
        stop.send(()).unwrap();
        release.send(()).unwrap();
        given.await.unwrap();
        open.send(()).unwrap();
    };
    finished(async {
        futures::join!(host.run_until(stopped.map(|_| ())), driver);
    })
    .await;
    let returned_at = Instant::now();
    let stopped_at = stopped_at.get().expect("the stop was sent");

    assert_eq!(first.task_state().await, "completed");
    let [settled_at] = <[Instant; 1]>::try_from(queue.settled.borrow().clone())
        .unwrap_or_else(|settled| panic!("the running delivery settles once: {settled:?}"));
    assert!(
        settled_at.saturating_duration_since(stopped_at) < drain,
        "the running delivery settled {:?} after the stop, past its {drain:?} drain",
        settled_at.saturating_duration_since(stopped_at)
    );

    assert_eq!(second.probe.starts.get(), 0);
    let given_back = queue.given_back.borrow().clone();
    let [(_, returned, with_task, why)] = <[_; 1]>::try_from(given_back)
        .unwrap_or_else(|given_back| panic!("the claim's delivery goes back: {given_back:?}"));
    assert_eq!(&returned, second.app.app_id());
    assert!(with_task, "the journal accepted execution, so its task went back too");
    assert_eq!(why, Unstarted::Stopped);
    assert_eq!(second.task_state().await, "released");

    assert_eq!(queue.requests().len(), 2, "nothing was claimed after the stop");
    let budget = drain + bounds.delivery.operation_timeout;
    assert!(
        returned_at.saturating_duration_since(stopped_at) < budget,
        "the consumer returned {:?} after the stop, past its {budget:?} budget",
        returned_at.saturating_duration_since(stopped_at)
    );
}

/// A stalled claim ends on its own bound - the wait it states and one
/// operation bound after it - and the claimer claims again after the error
/// back-off rather than holding its slots on the stall.
#[compio::test]
async fn a_stalled_claim_times_out_and_the_claimer_claims_again() {
    let (first, second) = pair(&AppPolicy::default()).await;
    let queue = Queue::new(&[&first, &second]);
    // Every claim stalls, and the operation timeout is what releases the stall,
    // so the release is observed on the queue's claim stream.
    let (_release_first, first_gate) = oneshot::channel();
    let (_release_second, second_gate) = oneshot::channel();
    let (_release_third, third_gate) = oneshot::channel();
    queue
        .claim_gates
        .borrow_mut()
        .extend([first_gate, second_gate, third_gate]);
    let creators = Creators::new(&[&first, &second]);
    let mut bounds = options(2);
    bounds.delivery.operation_timeout = Duration::from_millis(50);
    bounds.error_backoff = Duration::from_millis(100);
    let mut host = consumer(queue.clone(), &first.lease.delivery.worker_id, &creators, bounds);
    finished(host.run_until(queue.claims(3))).await;
    assert_eq!(first.probe.starts.get(), 0);
    assert_eq!(second.probe.starts.get(), 0);
    let requests = queue.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|request| request.max.get() == 2));
    assert!(first.app.job_receipt(&first.job).await.unwrap().is_none());
    // No stalled claim took a job, so a host under ordinary bounds runs both.
    drop(host);
    let mut host = consumer(queue.clone(), &first.lease.delivery.worker_id, &creators, options(2));
    finished(host.run_until(queue.settlements(2))).await;
    assert_eq!(first.probe.starts.get(), 1);
    assert_eq!(second.probe.starts.get(), 1);
    assert_eq!(first.task_state().await, "completed");
    assert_eq!(second.task_state().await, "completed");
}

#[compio::test]
async fn an_exhausted_manager_grant_cannot_admit_a_creator_task() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let expired = fixture.lease.clone();
    let queue = Queue::new(&[]);
    queue.register(&fixture);
    queue.script.borrow_mut().push_back(Reply {
        deliveries: vec![expired],
        after: Some(fixture.app.app_id().clone()),
        lap_complete: true,
        spent: true,
    });
    let creators = Creators::new(&[&fixture]);
    let mut host = consumer(queue.clone(), &fixture.lease.delivery.worker_id, &creators, options(1));
    finished(host.run_until(queue.claims(2))).await;
    assert_eq!(fixture.probe.starts.get(), 0);
    assert_eq!(creators.opened(fixture.app.app_id()), 0);
    assert!(fixture
        .app
        .job_receipt(&fixture.job)
        .await
        .unwrap()
        .is_none());
}

/// A delivery naming another worker is refused before its app is prepared.
#[compio::test]
async fn a_delivery_for_another_worker_is_refused_before_preparation() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let mut lease = fixture.lease.clone();
    lease.delivery.worker_id = WorkerId::mint();
    let queue = Queue::new(&[]);
    queue.register(&fixture);
    queue.script.borrow_mut().push_back(Reply {
        deliveries: vec![lease],
        after: Some(fixture.app.app_id().clone()),
        lap_complete: true,
        spent: false,
    });
    let creators = Creators::new(&[&fixture]);
    let mut host = consumer(queue.clone(), &fixture.lease.delivery.worker_id, &creators, options(1));
    finished(host.run_until(queue.claims(2))).await;
    assert_eq!(creators.opened(fixture.app.app_id()), 0);
    assert_eq!(fixture.probe.starts.get(), 0);
    assert!(fixture
        .app
        .job_receipt(&fixture.job)
        .await
        .unwrap()
        .is_none());
}

/// A batch fills the free slots across apps, one job each, and an app beyond
/// them waits for a slot rather than being claimed.
#[compio::test]
async fn a_batch_fills_the_free_slots_across_apps() {
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
    for fixture in &fixtures[..2] {
        let (started, receiver) = oneshot::channel();
        fixture.probe.started.replace(Some(started));
        running.push(receiver);
    }
    let all: Vec<&Fixture> = fixtures.iter().collect();
    let queue = Queue::new(&all);
    let creators = Creators::new(&all);
    let mut host = consumer(queue.clone(), &worker, &creators, options(2));
    finished(host.run_until(async {
        for started in running {
            started.await.unwrap();
        }
    }))
    .await;
    let requests = queue.requests();
    assert_eq!(requests.len(), 1, "a full batch leaves no free slot to claim for");
    assert_eq!(requests[0].max.get(), 2);
    assert_eq!(fixtures[0].probe.starts.get(), 1);
    assert_eq!(fixtures[1].probe.starts.get(), 1);
    assert_eq!(fixtures[2].probe.starts.get(), 0);
    assert_eq!(creators.opened(fixtures[2].app.app_id()), 0);
    assert!(fixtures[..2]
        .iter()
        .all(|fixture| fixture.probe.stops.get() == 1));
}

/// An abandoned host keeps its cancelled execution, and the app that execution
/// runs stays resident, until `drain` joins it -- even after the cache has
/// dropped the app.
#[compio::test]
async fn an_abandoned_host_keeps_its_execution_and_app_until_drain_finishes() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    fixture.probe.mode.set(Mode::Pending);
    let (started, running) = oneshot::channel();
    fixture.probe.started.replace(Some(started));
    let (stopping, stopped) = oneshot::channel();
    fixture.probe.stopping.replace(Some(stopping));
    let (release, gate) = oneshot::channel();
    fixture.probe.stop_gate.replace(Some(gate));
    let queue = Queue::new(&[&fixture]);
    let creators = Creators::new(&[&fixture]);
    let cache = prepared(&creators);
    let mut host = JobConsumer::new(
        queue.clone(),
        fixture.lease.delivery.worker_id.clone(),
        cache.clone(),
        options(1),
    )
    .unwrap();
    let task = host.run_until(std::future::pending()).boxed_local();
    let Either::Left((Ok(()), task)) = futures::future::select(running, task).await else {
        panic!("host must remain active");
    };
    drop(task);
    assert!(fixture.probe.cancels.get() > 0);
    assert_eq!(fixture.probe.stops.get(), 0);
    // The cache gives the app up; the abandoned slot still holds it.
    creators.listed.borrow_mut().clear();
    cache.prune();
    assert_eq!(creators.resident(fixture.app.app_id()), 1);
    let drain = host.drain().boxed_local();
    let Either::Left((Ok(()), drain)) = futures::future::select(stopped, drain).await else {
        panic!("drain must join execution");
    };
    assert_eq!(queue.requests().len(), 1);
    assert_eq!(
        creators.resident(fixture.app.app_id()),
        1,
        "the app was withdrawn while its execution was still stopping"
    );
    release.send(()).unwrap();
    finished(drain).await;
    assert_eq!(fixture.probe.stops.get(), 1);
    assert_eq!(fixture.task_state().await, "released");
    assert_eq!(creators.resident(fixture.app.app_id()), 0);
}

/// A stop ends claiming, and the execution already running finishes and
/// settles inside the drain bound instead of being cancelled at the stop.
///
/// The stop arrives once the execution has started; it needs a fraction of the
/// drain to complete, so only a consumer that cancels at the stop leaves it
/// unsettled.
#[compio::test]
async fn a_stop_lets_the_running_execution_finish_and_settle_within_the_drain() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    fixture.probe.mode.set(Mode::CompleteAfter);
    fixture.probe.hold.set(Duration::from_millis(300));
    let (started, running) = oneshot::channel();
    fixture.probe.started.replace(Some(started));
    let queue = Queue::new(&[&fixture]);
    let creators = Creators::new(&[&fixture]);
    let mut host = consumer(
        queue.clone(),
        &fixture.lease.delivery.worker_id,
        &creators,
        ConsumerOptions {
            drain: Duration::from_secs(10),
            ..options(1)
        },
    );
    let claims_at_stop = Cell::new(None);
    finished(host.run_until(async {
        running.await.unwrap();
        claims_at_stop.set(Some(queue.requests().len()));
    }))
    .await;
    assert_eq!(fixture.task_state().await, "completed");
    assert!(fixture
        .app
        .job_receipt(&fixture.job)
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        Some(queue.requests().len()),
        claims_at_stop.get(),
        "nothing was claimed after the stop"
    );
}

/// An execution still running when the drain bound runs out is cancelled
/// then, not at the stop, and its slot drains before the consumer returns.
///
/// The execution never completes on its own, so the bound is the only thing
/// that can end it inside its own timeout; the elapsed time says which bound
/// did.
#[compio::test]
async fn an_execution_the_drain_cannot_finish_is_cancelled_when_it_runs_out() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    fixture.probe.mode.set(Mode::Pending);
    let (started, running) = oneshot::channel();
    fixture.probe.started.replace(Some(started));
    let queue = Queue::new(&[&fixture]);
    let creators = Creators::new(&[&fixture]);
    let drain = Duration::from_millis(300);
    let mut host = consumer(
        queue.clone(),
        &fixture.lease.delivery.worker_id,
        &creators,
        ConsumerOptions {
            drain,
            // Far past the drain, so the drain bound is what ends the execution.
            delivery: DeliveryOptions {
                execution_timeout: Duration::from_mins(1),
                ..options(1).delivery
            },
            ..options(1)
        },
    );
    let stopped_at = Cell::new(None);
    finished(host.run_until(async {
        running.await.unwrap();
        stopped_at.set(Some(Instant::now()));
    }))
    .await;
    let stopped_at = stopped_at.get().expect("the stop arrived");
    assert!(
        stopped_at.elapsed() >= drain,
        "the running execution kept its slot for the whole drain"
    );
    assert!(
        stopped_at.elapsed() < drain + Duration::from_secs(5),
        "the drain bound, not the execution's own timeout, ended it"
    );
    assert_eq!(
        fixture.probe.stops.get(),
        1,
        "the drain bound cancelled it and its slot joined it"
    );
    assert_eq!(fixture.task_state().await, "released");
}

/// The manager queue of one app in its own database, claimed in the default
/// zone under the local host's configured policy.
struct NativeManager {
    database: crate::manager_queue::Manager,
    coordinator: Coordinator,
    policies: ConfiguredPolicies,
    worker: WorkerId,
    app: AppId,
    journal: AppWorkflows,
    /// The lane loses its first settlement acknowledgement.
    lane_loses_ack: Cell<bool>,
    /// The worker's transport loses its first settlement acknowledgement. Its
    /// own flag, because both claimants can settle in one case and their
    /// order is not the case's to decide.
    worker_loses_ack: Cell<bool>,
    /// Every settlement the manager committed for this worker, in order, so a
    /// lost acknowledgement's retry is seen to present identical metadata. A
    /// call the operation bound cancelled before it reached the manager is not
    /// one of them.
    requests: RefCell<Vec<JournalSettlement>>,
    settled: flume::Sender<()>,
    completion: flume::Receiver<()>,
    /// A claim waits on this before asking the queue, for a case that has to
    /// establish the lane's publication before a worker can be handed the
    /// work. Taken by the one claim it gates.
    claim_gate: RefCell<Option<oneshot::Receiver<()>>>,
    /// Opened by [`Self::sweep_publishing`] once the reconciliation has
    /// committed the work its dispatch publishes, releasing a claim gated on
    /// [`Self::claim_gate`].
    publish_gate: RefCell<Option<oneshot::Sender<()>>>,
}

impl NativeManager {
    /// Take the next maintenance row of this app's queue as the lane that owns
    /// it. `Claimant::Worker` admits `Work::Creator` alone, so every other class
    /// is reachable only here.
    async fn lane_claim(&self) -> Option<zeroship_workflow_manager::DeliveryGrant> {
        zeroship_workflow_manager::maintenance::MaintenanceAuthority::new(
            self.app.clone(),
            self.worker.clone(),
        )
        .claim(
            self.coordinator.queue(),
            Ok(AppPolicy::default().max_delivery_attempts),
        )
        .await
        .unwrap()
    }

    /// The lane asserts its own authority: `Claimant::Worker` denies every
    /// sweep, so a worker's claim is never handed one, and the duty this
    /// fixture publishes is claimable only here.
    async fn sweep(&self, objects: &PayloadObjects) -> (JobSpec, JobReceipt, SettlementReceipt) {
        self.sweep_publishing(objects, &super::NoPublication(self.app.clone()))
            .await
    }

    /// As [`Self::sweep`], with the publisher the dispatch's own intents reach.
    async fn sweep_publishing(
        &self,
        objects: &PayloadObjects,
        publisher: &impl zeroship_workflow::service::publication::JobPublisher,
    ) -> (JobSpec, JobReceipt, SettlementReceipt) {
        let lane = zeroship_workflow_manager::maintenance::MaintenanceAuthority::new(
            self.app.clone(),
            self.worker.clone(),
        );
        let queue = self.coordinator.queue();
        let grant = self
            .lane_claim()
            .await
            .expect("the lane takes the published maintenance row");
        let job = grant.delivery().job.clone();
        let MaintenanceOutcome::Settled(receipt) = self
            .journal
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
        // The dispatch has committed the work it publishes, including the
        // creator work a gated consumer waits to be handed. Opening the gate
        // only now makes the ordering the case names a fact: no worker claim
        // reaches the queue before the duty has published the creator work.
        if let Some(published) = self.publish_gate.borrow_mut().take() {
            let _ = published.send(());
        }
        // Construct once and retry the same request, the way a lane owes a lost
        // acknowledgement: the queue commits the first attempt, the reply is
        // dropped, and the retry must present identical metadata.
        let settlement = receipt.settlement(&grant).unwrap();
        let acknowledged = loop {
            self.requests.borrow_mut().push(settlement.clone());
            let observed = lane.settle(queue, &settlement).await.unwrap();
            if !self.lane_loses_ack.replace(false) {
                break observed;
            }
        };
        (job, *receipt, acknowledged)
    }

    async fn new(fixture: &Fixture) -> Rc<Self> {
        let app = fixture.app.app_id().clone();
        let database = crate::manager_queue::Manager::new(&app).await;
        let coordinator = Coordinator::new(
            database.queue.clone(),
            zeroship_workflow_manager::coordinator::Options::default(),
        )
        .unwrap();
        let (settled, completion) = flume::unbounded();
        Rc::new(Self {
            database,
            coordinator,
            policies: ConfiguredPolicies::new(app.clone(), AppPolicy::default()).unwrap(),
            worker: fixture.lease.delivery.worker_id.clone(),
            app,
            journal: fixture.app.clone(),
            lane_loses_ack: Cell::new(true),
            worker_loses_ack: Cell::new(true),
            requests: RefCell::new(Vec::new()),
            settled,
            completion,
            claim_gate: RefCell::new(None),
            publish_gate: RefCell::new(None),
        })
    }

    /// One zone claim for one slot, asserting it delivers nothing.
    async fn offers_nothing(&self) -> bool {
        JobTransport::claim(
            self,
            &ClaimJobs {
                max: 1.try_into().unwrap(),
                wait_ms: 1_000.try_into().unwrap(),
                after: None,
                exclude: Vec::new(),
            },
        )
        .await
        .unwrap()
        .deliveries
        .is_empty()
    }

    async fn settle_directly(&self, settlement: &JournalSettlement) -> SettlementReceipt {
        self.coordinator
            .settle_job(&self.worker, settlement, || async { Ok(self.worker.clone()) })
            .await
            .unwrap()
    }

    fn recovery(&self) -> zeroship_workflow_manager::recovery::Recovery {
        zeroship_workflow_manager::recovery::Recovery::new(
            self.database.queue.clone(),
            zeroship_workflow_manager::recovery::Options::default(),
        )
        .unwrap()
    }

    /// Responsibility for the app at the fixture's first deployment.
    async fn ensure(&self, fixture: &Fixture) -> zeroship_workflow_manager::recovery::Recovery {
        let recovery = self.recovery();
        recovery
            .ensure(
                &self.app,
                &ZoneId::default_zone(),
                fixture.job.deployment_id().unwrap(),
                Revision::try_from(1).unwrap(),
            )
            .await
            .unwrap();
        recovery
    }
}

fn manager_error(error: zeroship_workflow_manager::Error) -> WorkflowServiceError {
    WorkflowServiceError::Unavailable(error.to_string())
}

impl JobTransport for NativeManager {
    type Lease = zeroship_workflow_manager::DeliveryGrant;
    type Journal = AppWorkflows;

    async fn claim(
        &self,
        request: &ClaimJobs,
    ) -> Result<ClaimedBatch<Self::Lease>, WorkflowServiceError> {
        // A gated case establishes the lane's publication before any worker
        // claim reaches the queue, so the ordering it asserts is a fact rather
        // than a race. The gate is taken by the one claim it holds.
        let gated = self.claim_gate.borrow_mut().take();
        if let Some(gate) = gated {
            let _ = gate.await;
        }
        let claim = zeroship_workflow_manager::coordinator::ZoneClaim {
            worker: &self.worker,
            zone: &ZoneId::default_zone(),
            request,
            deadline: self
                .coordinator
                .claim_deadline(std::time::Instant::now(), request)
                .map_err(manager_error)?,
        };
        let (batch, _) = self
            .coordinator
            .claim_in_zone(
                &claim,
                &self.policies,
                || async { Ok(self.worker.clone()) },
                |_| async { zeroship_workflow_manager::coordinator::Admission::Deliver(()) },
            )
            .await
            .map_err(manager_error)?;
        let mut deliveries = Vec::new();
        for (lease, ()) in batch.grants {
            let accepted = if lease.delivery().job.operation.accepts_execution() {
                Some(self.journal.accept_job(&lease).await?)
            } else {
                None
            };
            deliveries.push(Claimed { lease, accepted });
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
        journal: &AppWorkflows,
        lease: &Self::Lease,
    ) -> Result<SettlementReceipt, WorkflowServiceError> {
        let request = committed_settlement(journal, lease).await?;
        let receipt = self
            .coordinator
            .settle_job(&self.worker, &request, || async { Ok(self.worker.clone()) })
            .await
            .map_err(manager_error)?;
        // RECORDED ONCE THE MANAGER COMMITTED, not when the call was made. A
        // settlement the operation bound cancels mid-flight never reached the
        // manager, and the retry that follows replays it; recording the
        // cancelled call would count the bound's timing as an acknowledgement
        // the transport lost.
        self.requests.borrow_mut().push(request);
        if self.worker_loses_ack.replace(false) {
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
        Ok(Completed {
            settlement: JobTransport::settle(self, journal, lease).await?,
            receipt,
        })
    }

    async fn release(
        &self,
        journal: &AppWorkflows,
        lease: &Self::Lease,
        task: &DeliveredTask,
    ) -> Result<(), WorkflowServiceError> {
        journal.release_job(task, lease).await
    }

    async fn give_back(
        &self,
        claimed: &Claimed<Self::Lease>,
        why: Unstarted,
    ) -> Result<(), WorkflowServiceError> {
        if let Some(task) = claimed.task() {
            self.journal.release_job(task, &claimed.lease).await?;
        }
        self.coordinator
            .give_back_job(
                &self.worker,
                claimed.lease.delivery(),
                match why {
                    Unstarted::Unprepared => zeroship_workflow_manager::GiveBack::Backoff,
                    Unstarted::Stopped => zeroship_workflow_manager::GiveBack::Unsent,
                },
                || async { Ok(self.worker.clone()) },
            )
            .await
            .map_err(manager_error)
    }

    async fn receipt(
        &self,
        journal: &AppWorkflows,
        job: &JobSpec,
    ) -> Result<Option<JobReceipt>, WorkflowServiceError> {
        journal.job_receipt(job).await
    }
}

impl zeroship_workflow::service::publication::JobPublisher for NativeManager {
    fn app_id(&self) -> &AppId {
        &self.app
    }
    async fn submit(&self, job: &JobSpec) -> Result<JobSpec, WorkflowServiceError> {
        self.coordinator
            .queue()
            .submit(job)
            .await
            .map_err(manager_error)
    }
}

fn native_consumer(manager: &Rc<NativeManager>, fixture: &Fixture) -> JobConsumer<NativeManager, Opener> {
    consumer(
        manager.clone(),
        &manager.worker,
        &Creators::new(&[fixture]),
        options(1),
    )
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
    let mut consumer = native_consumer(&manager, &fixture);
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
    assert!(manager.offers_nothing().await);
    assert!(fixture.app.pending_jobs(None, 1).await.unwrap().is_empty());
}

/// Each of these duties is `Work::Maintenance`, which `Claimant::Worker`
/// denies, so the lane claims and settles it while the worker's consumer runs
/// THROUGHOUT and is offered nothing. Keeping the consumer live is what makes
/// the executor assertions say something rather than hold vacuously.
async fn swept_beside_a_live_consumer(
    manager: &Rc<NativeManager>,
    fixture: &Fixture,
) -> (JobSpec, JobReceipt, SettlementReceipt) {
    let mut consumer = native_consumer(manager, fixture);
    let swept = RefCell::new(None);
    finished(consumer.run_until(async {
        *swept.borrow_mut() = Some(Box::pin(manager.sweep(&fixture.objects)).await);
    }))
    .await;
    swept.into_inner().expect("the lane settled the row it claimed")
}

#[compio::test]
async fn manager_collect_duty_settles_without_publishing_or_executing_creator_work() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    assert!(!super::collection::has_task(&fixture).await);
    let manager = NativeManager::new(&fixture).await;
    let recovery = manager.ensure(&fixture).await;
    let collect = recovery
        .dispatch(
            fixture.app.app_id(),
            zeroship_workflow_manager::recovery::DutyKind::Collect,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(collect.deployment_id().is_none());
    let (settled_job, settled_receipt, acknowledged) =
        swept_beside_a_live_consumer(&manager, &fixture).await;
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
    assert!(manager.offers_nothing().await);
    let requests = manager.requests.borrow();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    assert_eq!(requests[0].delivery().job, collect);
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
    let (settled_job, settled_receipt, acknowledged) =
        swept_beside_a_live_consumer(&manager, &fixture).await;
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
    assert!(manager.offers_nothing().await);
    let requests = manager.requests.borrow();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    assert_eq!(requests[0].delivery().job, fanout);
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
    let (settled_job, settled_receipt, acknowledged) =
        swept_beside_a_live_consumer(&manager, &fixture).await;
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
    assert!(manager.offers_nothing().await);
    let requests = manager.requests.borrow();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    assert_eq!(requests[0].delivery().job, page);
}

#[compio::test]
async fn manager_reconciliation_publishes_creator_work_before_the_consumer_executes_it() {
    let fixture = Fixture::new(AppPolicy::default()).await;
    let manager = NativeManager::new(&fixture).await;
    let recovery = manager.ensure(&fixture).await;
    let reconciliation = recovery
        .dispatch(
            fixture.app.app_id(),
            zeroship_workflow_manager::recovery::DutyKind::Reconcile,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(reconciliation.deployment_id().is_none());
    let mut consumer = native_consumer(&manager, &fixture);
    // Two claimants, one consumer run. The reconciliation is `Work::Maintenance`,
    // which `Claimant::Worker` denies, so the lane takes it; its settlement
    // carries the creator Advance, and THAT is what the consumer executes. The
    // order is the point: the consumer's claim is gated until the lane's
    // dispatch has committed the creator work, so the duty publishes that work
    // before any worker can be handed some rather than racing how a loaded host
    // interleaves the two claimants.
    let (published, publication) = oneshot::channel();
    *manager.publish_gate.borrow_mut() = Some(published);
    *manager.claim_gate.borrow_mut() = Some(publication);
    let swept = RefCell::new(None);
    finished(consumer.run_until(async {
        *swept.borrow_mut() = Some(
            Box::pin(manager.sweep_publishing(&fixture.objects, manager.as_ref())).await,
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
    assert!(manager.offers_nothing().await);
    // Each claimant's retried settlement is asserted on its own job, so the
    // assertions do not depend on their interleaving.
    let requests = manager.requests.borrow();
    let (lane, worker): (Vec<_>, Vec<_>) = requests
        .iter()
        .partition(|request| request.delivery().job.id == reconciliation.id);
    assert_eq!(lane.len(), 2, "the lane's lost acknowledgement was retried");
    assert_eq!(lane[0], lane[1]);
    assert_eq!(worker.len(), 2, "the worker's lost acknowledgement was retried");
    assert_eq!(worker[0], worker[1]);
    assert_eq!(worker[0].delivery().job.id, fixture.job.id);
}

/// Installs the manager's epoch beside the policy, as the service does.
fn install_epoch(fixture: &Fixture, epoch: Revision) {
    fixture
        .service
        .fixture_install(
            fixture.app.app_id(),
            PolicySnapshot::configuration(Revision::try_from(1).unwrap(), AppPolicy::default())
                .unwrap()
                .with_ingress_epoch(Some(epoch)),
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
    let recovery = manager.ensure(&fixture).await;
    let epoch = recovery.establish(&app, None, true).await.unwrap();
    assert_eq!(epoch, Revision::try_from(1).unwrap());
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
    // worker's claim admits `Work::Creator` alone and would be handed neither.
    let page_grant = manager
        .lane_claim()
        .await
        .expect("the lane takes the published page");
    assert_eq!(page_grant.delivery().job, page);
    let close_grant = manager
        .lane_claim()
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

    // The creator refuses the still-valid epoch; the service establishes the next.
    assert_eq!(
        fixture
            .app
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await
            .unwrap_err(),
        WorkflowServiceError::IngressFenced(Some(Revision::try_from(1).unwrap()))
    );
    let next = recovery.establish(&app, Some(epoch), true).await.unwrap();
    assert_eq!(next, Revision::try_from(2).unwrap());
    assert_eq!(recovery.establish(&app, Some(epoch), true).await.unwrap(), next);
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

/// The lane dispatches Close to the creator handler. A lost settlement
/// acknowledgement is retried with the identical settlement, and the manager
/// retires exactly once. A crashed first attempt redelivered to the lane
/// replays the committed creator receipt.
#[compio::test]
async fn the_lane_closes_and_retires_once_after_lost_ack_and_redelivery() {
    use zeroship_workflow_manager::recovery::ScopeState;
    let fixture = Fixture::new(AppPolicy::default()).await;
    let app = fixture.app.app_id().clone();
    let manager = NativeManager::new(&fixture).await;
    let recovery = manager.ensure(&fixture).await;
    install_epoch(&fixture, recovery.establish(&app, None, true).await.unwrap());
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
    // which `Claimant::Worker` denies.
    let crashed = manager
        .lane_claim()
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
    let (redelivered, replayed, _) = Box::pin(manager.sweep(&fixture.objects)).await;
    assert_eq!(redelivered, close);
    assert_eq!(replayed, committed);
    {
        let requests = manager.requests.borrow();
        assert_eq!(requests.len(), 2, "the lost acknowledgement was retried");
        assert_eq!(requests[0], requests[1]);
        assert_eq!(requests[0].delivery().job, close);
        assert_eq!(requests[0].delivery().attempt.get(), 2);
        assert_eq!(*requests[0].outcome(), committed.outcome);
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
    assert!(manager.lane_claim().await.is_none());
    assert!(manager.offers_nothing().await);
}

/// Archive masks admission, dispatch and ingress. The manager refuses to
/// establish ingress, yet the lane still delivers the manager-origin Close to
/// the creator handler, the evidence drains and the scope retires.
#[compio::test]
async fn the_lane_delivers_close_under_archived_policy_and_the_scope_retires() {
    use zeroship_workflow_manager::recovery::ScopeState;
    let fixture = Fixture::new(AppPolicy::default()).await;
    let app = fixture.app.app_id().clone();
    let manager = NativeManager::new(&fixture).await;
    let recovery = manager.ensure(&fixture).await;
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
    assert_eq!(
        recovery
            .establish(&app, Some(Revision::try_from(1).unwrap()), archived.admission)
            .await,
        Err(zeroship_workflow_manager::Error::Denied)
    );
    // The service installs the archived policy with the epoch it still holds.
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
    let (settled_job, settled_receipt, acknowledged) =
        swept_beside_a_live_consumer(&manager, &fixture).await;
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
/// It is the LANE that claims it, not a worker: `release_hold` is
/// `Work::Maintenance`, and `Claimant::Worker` denies every kind in that class,
/// so a worker's claim answers nothing here at all.
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
        Rc::new(manager.policies.clone()),
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

    // A worker's claim answers nothing here: the class the row carries is one
    // `Claimant::Worker` denies, which is what leaves the row to the lane.
    assert!(
        manager.offers_nothing().await,
        "a worker's claim must not reach a release duty"
    );

    let (job, creator, acknowledged) = Box::pin(manager.sweep(&fixture.objects)).await;
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
    assert_eq!(requests[0].delivery().job, job);
}
