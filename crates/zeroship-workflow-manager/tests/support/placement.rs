//! Controllable eligibility facts and capacity providers for placement contracts.
#![allow(
    clippy::future_not_send,
    reason = "fixture providers run on their compio runtime"
)]

use crate::support::{synthetic_holds, Fixture};
use futures::channel::oneshot;
use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    num::NonZeroU32,
    rc::Rc,
    time::Duration,
};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{RegisterWorker, Revision, WorkerId, WorkerState},
    workflow_jobs::{BroadcastId, JobId, JobOperation, JobSpec},
};
use zeroship_workflow_manager::{
    capacity::{
        self, CapacityFuture, CapacityProvider, CapacityReply, CapacityRequest, Contract,
        ProvisionFuture, ProvisionReply, ProvisionRequest, ProvisioningProvider, Refusal,
    },
    coordinator::{self, Coordinator},
    driver::{self, Driver},
    eligibility::{AppFacts, EligibilityFuture, EligibilitySource, WorkerFacts, ZoneId},
    recovery, Error, Queue,
};

/// Control's facts as a test controls them. Revocation can be injected right
/// after a chosen read of a worker, which models a revocation that commits
/// between two reads of one placement transaction.
#[derive(Debug, Default)]
pub struct Facts {
    apps: RefCell<BTreeMap<String, AppFacts>>,
    workers: RefCell<BTreeMap<String, WorkerFacts>>,
    reads: RefCell<BTreeMap<String, usize>>,
    revoke_after: RefCell<Option<(String, usize)>>,
}

impl Facts {
    pub fn app(&self, app: &AppId, zone: &ZoneId) {
        self.apps.borrow_mut().insert(
            app.as_str().to_owned(),
            AppFacts {
                zone: zone.clone(),
                deleted: false,
            },
        );
    }

    /// Terminal deletion, which Control records only for an archived app.
    pub fn delete(&self, app: &AppId) {
        self.apps
            .borrow_mut()
            .get_mut(app.as_str())
            .expect("known app")
            .deleted = true;
    }

    pub fn enroll(&self, worker: &WorkerId, zone: &ZoneId) {
        self.workers.borrow_mut().insert(
            worker.as_str().to_owned(),
            WorkerFacts {
                zone: zone.clone(),
                active: true,
            },
        );
    }

    /// The enroller revocation cascade: the instance is no longer active.
    pub fn revoke(&self, worker: &WorkerId) {
        self.workers
            .borrow_mut()
            .get_mut(worker.as_str())
            .expect("enrolled worker")
            .active = false;
    }

    /// Reads of this worker's facts so far.
    pub fn reads(&self, worker: &WorkerId) -> usize {
        self.reads
            .borrow()
            .get(worker.as_str())
            .copied()
            .unwrap_or_default()
    }

    /// Revoke the worker immediately after the read numbered `read` returns.
    pub fn revoke_after_read(&self, worker: &WorkerId, read: usize) {
        *self.revoke_after.borrow_mut() = Some((worker.as_str().to_owned(), read));
    }

    /// The host's key-exact enrollment check, as delivery callbacks run it.
    pub fn authorize(&self, worker: &WorkerId) -> Result<WorkerId, Error> {
        match self.workers.borrow().get(worker.as_str()) {
            Some(facts) if facts.active => Ok(worker.clone()),
            _ => Err(Error::Denied),
        }
    }
}

impl EligibilitySource for Facts {
    fn app<'a>(&'a self, app: &'a AppId) -> EligibilityFuture<'a, Option<AppFacts>> {
        Box::pin(async move { Ok(self.apps.borrow().get(app.as_str()).cloned()) })
    }

    fn worker<'a>(&'a self, worker: &'a WorkerId) -> EligibilityFuture<'a, Option<WorkerFacts>> {
        Box::pin(async move {
            let current = self.workers.borrow().get(worker.as_str()).cloned();
            let read = {
                let mut reads = self.reads.borrow_mut();
                let count = reads.entry(worker.as_str().to_owned()).or_default();
                *count += 1;
                *count
            };
            let due = self
                .revoke_after
                .borrow()
                .as_ref()
                .is_some_and(|(id, at)| id == worker.as_str() && *at == read);
            if due {
                self.revoke_after.borrow_mut().take();
                self.revoke(worker);
            }
            Ok(current)
        })
    }
}

pub const LONG: Duration = Duration::from_secs(3600);
pub const SOON: Duration = Duration::from_millis(1);

pub fn revision(value: i64) -> Revision {
    value.try_into().unwrap()
}

/// Driver pacing for capacity contracts: duties recur rarely, the hold-down
/// never lapses during a test, and `retry` paces requests after a reply.
pub fn options(retry: Duration) -> driver::Options {
    driver::Options {
        page_limit: 16,
        recovery: recovery::Options {
            interval: LONG,
            page_size: 16,
            closing_timeout: LONG,
        },
        capacity: capacity::Options {
            idle_hold_down: LONG,
            request_timeout: Duration::from_secs(10),
            retry_interval: retry,
        },
        ..driver::Options::default()
    }
}

/// An intent-producing job that needs no deployment.
pub fn fanout(app: &AppId) -> JobSpec {
    JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation: JobOperation::Fanout {
            broadcast_id: BroadcastId::mint(),
            revision: revision(1),
        },
        available_at: 0.try_into().unwrap(),
    }
}

/// One manager replica: its own queue connections over the shared database,
/// reading the shared Control facts.
#[derive(Debug, Clone)]
pub struct Host {
    pub queue: Queue,
    pub coordinator: Coordinator,
    pub facts: Rc<Facts>,
}

impl Host {
    pub async fn new(fixture: &Fixture, facts: Rc<Facts>) -> Self {
        let queue = Queue::connect(
            fixture.binding(),
            fixture.url(),
            zeroship_workflow_manager::Options::default(),
            synthetic_holds(),
        )
        .await
        .unwrap();
        let coordinator =
            Coordinator::new(queue.clone(), coordinator::Options::default(), facts.clone())
                .unwrap();
        Self {
            queue,
            coordinator,
            facts,
        }
    }

    /// Enroll and register a ready worker in `zone`.
    pub async fn worker(&self, zone: &ZoneId, capacity: u32) -> WorkerId {
        let worker = WorkerId::mint();
        self.facts.enroll(&worker, zone);
        self.coordinator
            .register(&worker, &ready(capacity))
            .await
            .unwrap();
        worker
    }

    /// An app in `zone` with claimable work.
    pub async fn due(&self, app: &AppId, zone: &ZoneId) -> JobSpec {
        self.facts.app(app, zone);
        self.queue.register_scope(app).await.unwrap();
        self.queue.submit(&fanout(app)).await.unwrap()
    }

    pub fn driver(&self, retry: Duration, contract: Contract) -> Driver {
        Driver::new(self.coordinator.clone(), options(retry), contract).unwrap()
    }

    /// Apps a worker currently holds, in identity order.
    pub async fn placed(&self, worker: &WorkerId) -> Vec<AppId> {
        self.coordinator
            .assignments(worker, None)
            .await
            .unwrap()
            .into_iter()
            .map(|assignment| assignment.app_id)
            .collect()
    }
}

pub const fn ready(capacity: u32) -> RegisterWorker {
    RegisterWorker {
        capacity: NonZeroU32::new(capacity).unwrap(),
        state: WorkerState::Ready,
    }
}

/// Starts workers the way an orchestrator would: a new enrolled instance in
/// the zone, which then registers its capacity.
#[derive(Debug, Clone)]
pub struct Starter {
    host: Host,
    capacity: u32,
}

impl Starter {
    pub const fn new(host: Host, capacity: u32) -> Self {
        Self { host, capacity }
    }

    async fn start(&self, zone: &ZoneId) -> WorkerId {
        self.host.worker(zone, self.capacity).await
    }
}

/// A scripted deviation for one provider call, consumed in order.
#[derive(Debug)]
pub enum Step {
    /// Apply the request and reply normally.
    Proceed,
    /// Apply the request, then lose the reply as a crash or timeout would.
    Lose,
    /// Fail without applying, as an unreachable provider does.
    Fail,
    /// Refuse without applying.
    Refuse(Refusal),
    /// Announce entry, then wait for the test to release the step to take.
    Gate {
        entered: oneshot::Sender<()>,
        release: oneshot::Receiver<Self>,
    },
}

/// A declarative provider like an orchestrator's replica count: it converges
/// each zone on enough workers for the desired slots, so a repeated request
/// starts nothing.
#[derive(Debug)]
pub struct Pool {
    starter: Starter,
    pub calls: RefCell<Vec<CapacityRequest>>,
    pub started: RefCell<Vec<(ZoneId, WorkerId)>>,
    pub script: RefCell<VecDeque<Step>>,
}

impl Pool {
    pub fn new(starter: Starter) -> Rc<Self> {
        Rc::new(Self {
            starter,
            calls: RefCell::default(),
            started: RefCell::default(),
            script: RefCell::default(),
        })
    }

    pub fn starts(&self) -> usize {
        self.started.borrow().len()
    }

    pub fn calls(&self) -> usize {
        self.calls.borrow().len()
    }

    async fn apply(&self, request: &CapacityRequest) -> CapacityReply {
        let per_worker = u64::from(self.starter.capacity);
        let needed = usize::try_from(request.desired_slots.div_ceil(per_worker)).unwrap();
        let running = || {
            self.started
                .borrow()
                .iter()
                .filter(|(zone, _)| zone == &request.zone)
                .count()
        };
        while running() < needed {
            let worker = self.starter.start(&request.zone).await;
            self.started
                .borrow_mut()
                .push((request.zone.clone(), worker));
        }
        CapacityReply::Progress {
            ready_slots: u64::try_from(running()).unwrap() * per_worker,
        }
    }
}

impl CapacityProvider for Pool {
    fn ensure<'a>(&'a self, request: &'a CapacityRequest) -> CapacityFuture<'a> {
        Box::pin(async move {
            self.calls.borrow_mut().push(request.clone());
            let step = self.script.borrow_mut().pop_front();
            let step = match step {
                Some(Step::Gate { entered, release }) => {
                    let _ = entered.send(());
                    Some(release.await.expect("gate released"))
                }
                step => step,
            };
            match step {
                Some(Step::Refuse(refusal)) => Ok(CapacityReply::Refused(refusal)),
                Some(Step::Fail) => Err(Error::Unavailable),
                Some(Step::Lose) => {
                    self.apply(request).await;
                    Err(Error::Unavailable)
                }
                Some(Step::Gate { .. }) => unreachable!("gates release a step"),
                Some(Step::Proceed) | None => Ok(self.apply(request).await),
            }
        })
    }
}

/// An imperative provider: every request starts a worker for its app. With
/// `dedupe`, a repeated intent identity starts nothing.
#[derive(Debug)]
pub struct Starts {
    starter: Starter,
    dedupe: bool,
    pub calls: RefCell<Vec<ProvisionRequest>>,
    pub started: RefCell<Vec<(AppId, Revision, WorkerId)>>,
    pub script: RefCell<VecDeque<Step>>,
}

impl Starts {
    pub fn new(starter: Starter, dedupe: bool) -> Rc<Self> {
        Rc::new(Self {
            starter,
            dedupe,
            calls: RefCell::default(),
            started: RefCell::default(),
            script: RefCell::default(),
        })
    }

    pub fn starts(&self) -> usize {
        self.started.borrow().len()
    }

    pub fn calls(&self) -> usize {
        self.calls.borrow().len()
    }

    async fn apply(&self, request: &ProvisionRequest) {
        let seen = self
            .started
            .borrow()
            .iter()
            .any(|(app, generation, _)| app == &request.app && *generation == request.generation);
        if self.dedupe && seen {
            return;
        }
        let worker = self.starter.start(&request.zone).await;
        self.started
            .borrow_mut()
            .push((request.app.clone(), request.generation, worker));
    }
}

impl ProvisioningProvider for Starts {
    fn provision<'a>(&'a self, request: &'a ProvisionRequest) -> ProvisionFuture<'a> {
        Box::pin(async move {
            self.calls.borrow_mut().push(request.clone());
            let step = self.script.borrow_mut().pop_front();
            let step = match step {
                Some(Step::Gate { entered, release }) => {
                    let _ = entered.send(());
                    Some(release.await.expect("gate released"))
                }
                step => step,
            };
            match step {
                Some(Step::Refuse(refusal)) => Ok(ProvisionReply::Refused(refusal)),
                Some(Step::Fail) => Err(Error::Unavailable),
                Some(Step::Lose) => {
                    self.apply(request).await;
                    Err(Error::Unavailable)
                }
                Some(Step::Gate { .. }) => unreachable!("gates release a step"),
                Some(Step::Proceed) | None => {
                    self.apply(request).await;
                    Ok(ProvisionReply::Provisioned)
                }
            }
        })
    }
}

/// Wait until `waiters` manager sessions queue behind a lock the
/// administrator holds. Polls from inside the administrator's transaction.
pub async fn blocked_manager(admin: &compio_postgres::Client, waiters: i64) {
    let sql = "SELECT count(DISTINCT a.pid) FROM pg_locks l \
         JOIN pg_stat_activity a ON a.pid=l.pid \
         WHERE a.usename='workflow_manager_test' AND NOT l.granted \
         AND cardinality(pg_blocking_pids(a.pid)) > 0";
    compio::time::timeout(Duration::from_secs(10), async {
        loop {
            admin
                .batch_execute("SELECT pg_stat_clear_snapshot()")
                .await
                .unwrap();
            if admin.query(sql, &[]).await.unwrap()[0].get::<_, i64>(0) >= waiters {
                return;
            }
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("manager operations must reach the held lock");
}

/// Counts requests to a provider that records nothing itself.
#[derive(Debug)]
pub struct Counted<P> {
    inner: P,
    calls: std::cell::Cell<usize>,
}

impl<P> Counted<P> {
    pub fn new(inner: P) -> Rc<Self> {
        Rc::new(Self {
            inner,
            calls: std::cell::Cell::new(0),
        })
    }

    pub const fn calls(&self) -> usize {
        self.calls.get()
    }
}

impl<P: CapacityProvider> CapacityProvider for Counted<P> {
    fn ensure<'a>(&'a self, request: &'a CapacityRequest) -> CapacityFuture<'a> {
        self.calls.set(self.calls.get() + 1);
        self.inner.ensure(request)
    }
}
