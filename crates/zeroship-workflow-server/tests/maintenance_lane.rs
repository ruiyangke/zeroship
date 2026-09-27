//! The service's maintenance lane, end to end against its own journal and the
//! queue it owns.
//!
//! Nothing here places the app on a worker. That is the point: the lane asserts
//! its own authority, so a claim that succeeds without any `assignments` row is
//! what the authority seam decided.
#![expect(
    clippy::future_not_send,
    reason = "journal and queue fixtures stay on their compio runtime"
)]

#[path = "support/holds.rs"]
mod holds;
#[path = "support/journal.rs"]
mod journal;
#[allow(
    dead_code,
    reason = "the shared platform fixture also supports process tests"
)]
#[path = "support/platform.rs"]
mod platform;

use futures::{
    channel::oneshot,
    future::{select, Either, LocalBoxFuture},
};
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{RunId, WorkerId},
    workflow_jobs::{DeploymentId, JobId, JobOperation, JobOutcome, JobSpec},
    workflow_policy::AppPolicy,
};
use zeroship_workflow::{
    service::{maintenance::MaintenanceOptions, publication::JobPublisher},
    WorkflowServiceError,
};
use zeroship_workflow_manager::{
    capacity::StaticPool,
    driver::{Driver, Options as DriverOptions},
    lifecycle::Undeletable,
    policy::{PolicyObservation, PolicySource},
    recovery::Options as RecoveryOptions,
    Error as ManagerError, Queue,
};
use zeroship_workflow_server::{
    coordinator::{connect_eligibility, Coordinator, Options},
    runs::RunService,
    server::drive,
    sweeps::{LaneOptions, LanePublisher, MaintenanceDriver, MaintenanceLane, SweepError, Swept},
};

/// The observations journals are bound under, one per app the fixture has
/// granted a policy.
///
/// It answers for those apps and refuses every other, so a visit to an app this
/// fixture never seeded is refused rather than served by a stub that answers for
/// anything the lane happens to enumerate.
#[derive(Debug, Default)]
struct Source(RefCell<Vec<PolicyObservation>>);
impl Source {
    fn grant(&self, app: &AppId) {
        self.0.borrow_mut().push(
            PolicyObservation::new(
                app.clone(),
                7.try_into().unwrap(),
                AppPolicy::default(),
                Instant::now() + Duration::from_mins(10),
            )
            .unwrap(),
        );
    }
}
impl PolicySource for Source {
    fn observe<'a>(
        &'a self,
        app: &'a AppId,
    ) -> LocalBoxFuture<'a, Result<PolicyObservation, ManagerError>> {
        Box::pin(async move {
            self.0
                .borrow()
                .iter()
                .find(|observed| observed.app_id() == app)
                .cloned()
                .ok_or(ManagerError::Denied)
        })
    }
    fn revalidate(&self, observation: &PolicyObservation) -> Result<Instant, ManagerError> {
        Ok(observation.expires_at())
    }
}

struct Fixture {
    platform: platform::Platform,
    service: Coordinator,
    queue: Queue,
    runs: Rc<RunService>,
    policies: Rc<Source>,
    lane: MaintenanceLane,
    app: AppId,
}

/// A lane of this fixture's own, with an identity nothing else holds.
fn lane(queue: &Queue, runs: &Rc<RunService>, policies: &Rc<Source>) -> MaintenanceLane {
    MaintenanceLane::new(
        queue.clone(),
        Rc::clone(runs),
        Rc::clone(policies) as Rc<dyn PolicySource>,
        WorkerId::mint(),
        MaintenanceOptions::default(),
    )
    .unwrap()
}

impl Fixture {
    async fn new() -> Self {
        let platform = platform::Platform::new().await;
        let eligibility = Rc::new(
            connect_eligibility(&platform.runtime_url, Options::default())
                .await
                .unwrap(),
        );
        let service = Coordinator::connect(
            &platform.runtime_url,
            Options::default(),
            holds::client(),
            eligibility,
        )
        .await
        .unwrap();
        let queue = service.manager.queue().clone();
        let runs = Rc::new(
            RunService::connect(
                &platform.runtime_url,
                service.recovery(RecoveryOptions::default()).unwrap(),
            )
            .await
            .unwrap(),
        );
        let policies = Rc::new(Source::default());
        let app = AppId::mint();
        let fixture = Self {
            lane: lane(&queue, &runs, &policies),
            platform,
            service,
            queue,
            runs,
            policies,
            app: app.clone(),
        };
        fixture.seed(&app).await;
        fixture
    }

    /// Everything an app needs before the lane can sweep it: a live Control
    /// row, a registered queue scope, the journal rows an app needs to exist at
    /// all, and an observed policy to bind its journal under.
    ///
    /// The lane's operation acts on what this does NOT seed: there is no pending
    /// publication, so the reconciliation page is empty and its phase advances.
    async fn seed(&self, app: &AppId) {
        self.platform.seed_app(app).await;
        self.queue.register_scope(app).await.unwrap();
        journal::seed_run(&self.platform, app).await;
        self.policies.grant(app);
    }

    /// A second app this lane can sweep, for cases about which apps a turn
    /// reaches.
    async fn sweepable(&self) -> AppId {
        let app = AppId::mint();
        self.seed(&app).await;
        app
    }

    /// Turns over this fixture's queue, under the bounds a host configures.
    fn sweeps(&self, options: LaneOptions) -> MaintenanceDriver {
        MaintenanceDriver::new(lane(&self.queue, &self.runs, &self.policies), options).unwrap()
    }

    /// The manager driver this process composes beside the lane.
    fn driver(&self) -> Driver {
        Driver::new(
            self.service.manager.clone(),
            DriverOptions::default(),
            Rc::new(Undeletable),
            Rc::new(StaticPool),
        )
        .unwrap()
    }

    /// Placements recorded for any app. The lane asserts its own authority, so
    /// this stays empty however long the drive path runs.
    async fn placements(&self) -> i64 {
        self.platform
            .admin
            .query_one("SELECT COUNT(*) FROM workflow_manager.assignments", &[])
            .await
            .unwrap()
            .get(0)
    }

    async fn row(&self, job: &JobId) -> (String, Option<String>, Option<String>) {
        let row = self
            .platform
            .admin
            .query_one(
                "SELECT state, outcome, worker_id FROM workflow_manager.jobs WHERE id=$1",
                &[&job.as_str()],
            )
            .await
            .unwrap();
        (row.get(0), row.get(1), row.get(2))
    }

    /// Whether the queue holds a row for `job` at all.
    async fn stored(&self, job: &JobId) -> bool {
        !self
            .platform
            .admin
            .query(
                "SELECT 1 FROM workflow_manager.jobs WHERE id=$1",
                &[&job.as_str()],
            )
            .await
            .unwrap()
            .is_empty()
    }

    /// A second app with a registered queue scope, so a job naming it is one
    /// `Queue::submit` would accept.
    async fn neighbour(&self) -> AppId {
        let app = AppId::mint();
        self.platform.seed_app(&app).await;
        self.queue.register_scope(&app).await.unwrap();
        app
    }
}

fn sweep(app: &AppId) -> JobSpec {
    JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation: JobOperation::Reconcile {},
        available_at: 0.try_into().unwrap(),
    }
}

fn creator_work(app: &AppId) -> JobSpec {
    JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation: JobOperation::Advance {
            deployment_id: DeploymentId::mint(),
            run_id: RunId::mint(),
            generation: 0,
            revision: 1.try_into().unwrap(),
        },
        available_at: 0.try_into().unwrap(),
    }
}

/// Publication refuses a job naming an app other than the publisher's, and
/// writes nothing for it.
///
/// The job is hand-constructed, so this says the check refuses such a job. It
/// does not say anything can produce one: the sweep that holds this publisher
/// publishes from its own app's journal rows. What makes the check worth having
/// anyway is that one journal serves every app and its `app_id` columns are the
/// only thing separating them, so this is where that boundary is asserted.
///
/// The neighbour's scope is registered, which is what makes the control mean
/// something: `Queue::submit` would accept this job, so a refusal is the
/// publisher's own and the pass-through arm beside it proves the publisher is
/// not refusing everything.
#[compio::test]
async fn publication_refuses_a_job_belonging_to_another_app() {
    let fixture = Box::pin(Fixture::new()).await;
    let neighbour = fixture.neighbour().await;
    let publisher = LanePublisher::new(&fixture.queue, fixture.app.clone());

    let foreign = sweep(&neighbour);
    assert!(matches!(
        publisher.submit(&foreign).await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert!(
        !fixture.stored(&foreign.id).await,
        "a refused publication must leave the neighbour's queue untouched"
    );

    // The control, differing only in the job's app.
    let own = sweep(&fixture.app);
    assert_eq!(publisher.submit(&own).await.unwrap(), own);
    assert!(
        fixture.stored(&own.id).await,
        "the publisher's own app must reach the queue, or the refusal above \
         proves nothing about the app comparison"
    );
}

/// The lane claims a maintenance row without a placement, runs it against the
/// service's own journal and records what it committed.
///
/// The creator row submitted first is the control for the claim: it sits ahead
/// in the dispatch order and is still `ready` afterwards, so what the lane took
/// was chosen rather than simply first.
#[compio::test]
async fn the_lane_claims_and_settles_a_journal_only_maintenance_job() {
    let fixture = Box::pin(Fixture::new()).await;
    let creator = creator_work(&fixture.app);
    let maintenance = sweep(&fixture.app);
    fixture.queue.submit(&creator).await.unwrap();
    fixture.queue.submit(&maintenance).await.unwrap();
    assert_eq!(fixture.row(&creator.id).await.0, "ready");
    assert_eq!(fixture.row(&maintenance.id).await.0, "ready");

    let swept = Box::pin(fixture.lane.sweep(&fixture.app)).await.unwrap();
    let Swept::Settled(receipt) = swept else {
        panic!("the lane settles a reconciliation page: {swept:?}");
    };
    assert_eq!(receipt.job_id, maintenance.id);
    // The publications phase always asks for another page, so the first sweep
    // of an app settles as waiting rather than completed.
    assert_eq!(receipt.outcome, JobOutcome::Waiting {});

    let (state, outcome, worker) = fixture.row(&maintenance.id).await;
    assert_eq!(state, "settled");
    assert_eq!(
        outcome,
        Some(serde_json::to_string(&JobOutcome::Waiting {}).unwrap())
    );
    assert_eq!(
        worker.as_deref(),
        Some(fixture.lane.identity().as_str()),
        "the row must carry the identity the lane asserted for itself"
    );
    assert_eq!(
        fixture.row(&creator.id).await.0,
        "ready",
        "creator work stays for the claimant that runs it"
    );

    // Nothing else the lane can take: the creator row at the head is not it.
    assert!(matches!(
        Box::pin(fixture.lane.sweep(&fixture.app)).await.unwrap(),
        Swept::Idle
    ));
}

/// A maintenance row whose operation needs an artifact source is claimed and
/// then refused by name.
///
/// The lane takes every journal-only kind the dispatch runs, and the ones
/// reaching `self.service.deployments` have no source here. The refusal must
/// reach the caller rather than be absorbed: the row stays unsettled and
/// redeliverable, and what is missing is named.
#[compio::test]
async fn an_operation_without_an_artifact_source_is_refused_by_name() {
    let fixture = Box::pin(Fixture::new()).await;
    let release = JobSpec {
        id: JobId::mint(),
        app_id: fixture.app.clone(),
        operation: JobOperation::ReleaseHold {
            deployment_id: DeploymentId::mint(),
        },
        available_at: 0.try_into().unwrap(),
    };
    fixture.queue.submit(&release).await.unwrap();
    assert_eq!(fixture.row(&release.id).await.0, "ready");

    let refused = Box::pin(fixture.lane.sweep(&fixture.app)).await;
    let Err(SweepError::Journal(WorkflowServiceError::Unavailable(reason))) = refused else {
        panic!("the release names its missing source: {refused:?}");
    };
    assert!(
        reason.contains("deployment"),
        "the refusal must name what is missing, not just fail: {reason}"
    );
    let (state, outcome, worker) = fixture.row(&release.id).await;
    assert_eq!(
        state, "leased",
        "a refused row keeps its lease until it lapses"
    );
    assert_eq!(outcome, None);
    assert_eq!(worker.as_deref(), Some(fixture.lane.identity().as_str()));
}

/// Wait for `probe` to answer, or fail the case.
///
/// A lane the drive path never reaches never settles the row, so the expiry
/// here is the failure the cases below are built to see.
async fn until<T>(description: &str, mut probe: impl std::ops::AsyncFnMut() -> Option<T>) -> T {
    compio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(answer) = probe().await {
                return answer;
            }
            compio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the drive path did not {description}"))
}

/// The running service settles a due maintenance row on its own, and no worker
/// is ever placed for it.
///
/// This drives `drive`, the cadence the process itself runs, rather than calling
/// the lane: what it binds is that the drive path REACHES the lane. The manager
/// driver handed to it is the real one the process composes beside the lane, so
/// the pass this observes is the pass production performs.
///
/// The creator row submitted beside the maintenance one is the control for the
/// claim: it sits ahead in the dispatch order and stays `ready`, so what the
/// drive path swept was chosen rather than whatever came first.
#[compio::test]
async fn the_drive_path_settles_a_due_maintenance_row_without_a_placement() {
    let fixture = Box::pin(Fixture::new()).await;
    let creator = creator_work(&fixture.app);
    let maintenance = sweep(&fixture.app);
    fixture.queue.submit(&creator).await.unwrap();
    fixture.queue.submit(&maintenance).await.unwrap();
    assert_eq!(fixture.row(&maintenance.id).await.0, "ready");
    assert_eq!(fixture.placements().await, 0);

    let sweeps = fixture.sweeps(LaneOptions {
        page_limit: 8,
        lane_timeout: Duration::from_secs(10),
    });
    let identity = sweeps.lane().identity().clone();
    // Held for the whole case: a dropped sender reads as a stop, and the drive
    // path would then return before taking a single turn.
    let (_stop, stopped) = oneshot::channel();
    let driven = select(
        Box::pin(drive(
            fixture.driver(),
            sweeps,
            Duration::from_millis(20),
            stopped,
        )),
        Box::pin(until("settle the maintenance row", async || {
            (fixture.row(&maintenance.id).await.0 == "settled").then_some(())
        })),
    )
    .await;
    assert!(
        matches!(driven, Either::Right(_)),
        "the drive path returned before the lane settled anything"
    );

    let (state, outcome, worker) = fixture.row(&maintenance.id).await;
    assert_eq!(state, "settled");
    assert_eq!(
        outcome,
        Some(serde_json::to_string(&JobOutcome::Waiting {}).unwrap())
    );
    assert_eq!(
        worker.as_deref(),
        Some(identity.as_str()),
        "the row must carry the identity of the lane the drive path drove"
    );
    assert_eq!(
        fixture.row(&creator.id).await.0,
        "ready",
        "creator work stays for the claimant that runs it"
    );
    assert_eq!(
        fixture.placements().await,
        0,
        "the lane asserts its own authority, so nothing may have placed a worker"
    );
}

/// A turn visits at most its page limit, and the next turn continues from where
/// it stopped.
///
/// Two apps each hold one maintenance row and the limit is one. Without the
/// bound one turn would sweep both, so the second row still being `ready` is
/// what the limit buys; the second turn settling it says the bound is a page
/// rather than a ceiling on the work, and reports the pass as complete because
/// it reached the upper bound the first turn started against.
#[compio::test]
async fn a_turn_visits_at_most_its_page_limit_and_the_next_resumes() {
    let fixture = Box::pin(Fixture::new()).await;
    let neighbour = fixture.sweepable().await;
    let mut apps = [fixture.app.clone(), neighbour];
    apps.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    let rows = [sweep(&apps[0]), sweep(&apps[1])];
    for row in &rows {
        fixture.queue.submit(row).await.unwrap();
    }

    let mut sweeps = fixture.sweeps(LaneOptions {
        page_limit: 1,
        lane_timeout: Duration::from_secs(10),
    });
    let first = Box::pin(sweeps.tick()).await;
    assert_eq!(first.visited, 1, "{first:?}");
    assert_eq!(first.settled, 1, "{first:?}");
    assert!(first.failures.is_empty(), "{first:?}");
    assert!(!first.sweep_complete, "{first:?}");
    assert_eq!(fixture.row(&rows[0].id).await.0, "settled");
    assert_eq!(
        fixture.row(&rows[1].id).await.0,
        "ready",
        "the page limit must leave the second app for a later turn"
    );

    let second = Box::pin(sweeps.tick()).await;
    assert_eq!(second.visited, 1, "{second:?}");
    assert_eq!(second.settled, 1, "{second:?}");
    assert!(second.sweep_complete, "{second:?}");
    assert_eq!(fixture.row(&rows[1].id).await.0, "settled");
}

/// A turn whose deadline is already spent visits nothing and reports why.
///
/// The control differs only in the deadline: the same turn over the same row,
/// given a deadline it can meet, sweeps it. Without that arm an empty turn would
/// read as a bound working when it could equally be a lane that sweeps nothing.
#[compio::test]
async fn a_turn_whose_deadline_is_spent_visits_nothing() {
    let fixture = Box::pin(Fixture::new()).await;
    let row = sweep(&fixture.app);
    fixture.queue.submit(&row).await.unwrap();

    let mut spent = fixture.sweeps(LaneOptions {
        page_limit: 8,
        lane_timeout: Duration::from_nanos(1),
    });
    let turn = Box::pin(spent.tick()).await;
    assert_eq!(turn.visited, 0, "{turn:?}");
    assert!(turn.timed_out, "{turn:?}");
    assert_eq!(turn.scan_error, Some(ManagerError::Timeout), "{turn:?}");
    assert_eq!(
        fixture.row(&row.id).await.0,
        "ready",
        "a turn that never ran must leave the row for the next one"
    );

    let mut afforded = fixture.sweeps(LaneOptions {
        page_limit: 8,
        lane_timeout: Duration::from_secs(10),
    });
    let turn = Box::pin(afforded.tick()).await;
    assert_eq!(turn.settled, 1, "{turn:?}");
    assert!(!turn.timed_out, "{turn:?}");
    assert_eq!(fixture.row(&row.id).await.0, "settled");
}
