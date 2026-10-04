//! The service's maintenance lane, end to end against its own journal and the
//! queue it owns.
//!
//! No worker instance exists here. That is the point: the lane asserts its own
//! authority, so a claim that succeeds with no worker enrolled is what the
//! authority seam decided.
#![expect(
    clippy::future_not_send,
    reason = "journal and queue fixtures stay on their compio runtime"
)]

use crate::support::{deployments, holds, journal, platform};

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
    workflow_coordination::{RequestId, RunId, WorkerId},
    workflow_jobs::{DeploymentId, JobId, JobOperation, JobOutcome, JobSpec},
    workflow_policy::AppPolicy,
    workflow_schedules::{ActivateSchedules, RegisterSchedules},
};
use zeroship_storage::{backend::ListRequest, Namespace, StorageBackendConfig, StorageStore};
use zeroship_workflow::{
    service::{
        maintenance::MaintenanceOptions, publication::JobPublisher, DeployRegistration,
        PAYLOAD_NAMESPACE,
    },
    InputStager, WorkflowServiceError,
};
use zeroship_workflow_manager::{
    capacity::StaticPool,
    driver::{Driver, Options as DriverOptions},
    lifecycle::Undeletable,
    policy::{PolicyObservation, PolicySource},
    recovery::Options as RecoveryOptions,
    scheduling::{Options as SchedulerOptions, Scheduler},
    Error as ManagerError, Queue,
};
use zeroship_workflow_server::{
    coordinator::{Coordinator, Options},
    payloads::ServicePayloads,
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
                zeroship_core::ZoneId::default_zone(),
                false,
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
    }}

struct Fixture {
    platform: platform::Platform,
    service: Coordinator,
    queue: Queue,
    runs: Rc<RunService>,
    policies: Rc<Source>,
    lane: MaintenanceLane,
    /// The payload store's root. It outlives every lane this fixture builds, so
    /// the objects one lane wrote are the objects the next one reads.
    objects: tempfile::TempDir,
    app: AppId,
}

/// A lane of this fixture's own, with an identity nothing else holds, over the
/// payload store rooted at `objects`.
fn lane(
    queue: &Queue,
    runs: &Rc<RunService>,
    policies: &Rc<Source>,
    objects: &tempfile::TempDir,
) -> MaintenanceLane {
    MaintenanceLane::new(
        queue.clone(),
        Rc::clone(runs),
        Rc::clone(policies) as Rc<dyn PolicySource>,
        WorkerId::mint(),
        ServicePayloads::open(&StorageBackendConfig::Local(objects.path().to_owned())).unwrap(),
        MaintenanceOptions::default(),
    )
    .unwrap()
}

impl Fixture {
    async fn new() -> Self {
        // The lane's turns enumerate every claimable app in the queue, a
    // queue-global subject, so this case gets a database no other case shares.
    let platform = platform::Platform::fresh_database().await;
        let service =
            Coordinator::connect(&platform.runtime_url, Options::default(), holds::client())
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
        let objects = tempfile::tempdir().unwrap();
        let app = AppId::mint();
        let fixture = Self {
            lane: lane(&queue, &runs, &policies, &objects),
            platform,
            service,
            queue,
            runs,
            policies,
            objects,
            app: app.clone(),
        };
        fixture.seed(&app).await;
        fixture
    }

    /// Everything an app needs before the lane can sweep it: what [`Self::admit`]
    /// grants, and the journal rows an app needs to exist at all.
    ///
    /// The lane's operation acts on what this does NOT seed: there is no pending
    /// publication, so the reconciliation page is empty and its phase advances.
    async fn seed(&self, app: &AppId) {
        self.admit(app).await;
        journal::seed_run(&self.platform, app).await;
    }

    /// Everything outside the journal an app needs before the lane can visit
    /// it: a live Control row, a registered queue scope and an observed policy
    /// to bind its journal under.
    async fn admit(&self, app: &AppId) {
        self.platform.seed_app(app).await;
        self.queue
            .register_scope(app, &zeroship_core::ZoneId::default_zone())
            .await
            .unwrap();
        self.policies.grant(app);
    }

    /// A lane whose journal activates `registration` for `app`, built the way
    /// the process builds its own: the retention authority and the asserted
    /// manifest summary, and no artifact store.
    async fn hosting(&self, app: &AppId, registration: &DeployRegistration) -> MaintenanceLane {
        let runs = Rc::new(
            RunService::connect(
                &self.platform.runtime_url,
                self.service.recovery(RecoveryOptions::default()).unwrap(),
            )
            .await
            .unwrap()
            .with_deployments(deployments::asserted(app, registration)),
        );
        lane(&self.queue, &runs, &self.policies, &self.objects)
    }

    /// The app state rows the journal holds for `app`, and the deployment it
    /// holds as active.
    async fn journal_app(&self, app: &AppId) -> (i64, Option<String>) {
        let entered: i64 = self
            .platform
            .admin
            .query_one(
                "SELECT COUNT(*) FROM workflow_manager.__zeroship_workflow_app_state \
                 WHERE app_id=$1",
                &[&app.as_str()],
            )
            .await
            .unwrap()
            .get(0);
        let active = self
            .platform
            .admin
            .query(
                "SELECT id FROM workflow_manager.__zeroship_workflow_deploys \
                 WHERE app_id=$1 AND active=1",
                &[&app.as_str()],
            )
            .await
            .unwrap()
            .first()
            .map(|row| row.get(0));
        (entered, active)
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
        MaintenanceDriver::new(
            lane(&self.queue, &self.runs, &self.policies, &self.objects),
            options,
        )
        .unwrap()
    }

    /// The payload objects an app holds, read through a store of the test's own
    /// over the same root. Reading them back through the lane's own handle would
    /// say nothing about where it put them.
    async fn stored_objects(&self, app: &AppId) -> Vec<(String, Vec<u8>)> {
        let store =
            StorageStore::open(&StorageBackendConfig::Local(self.objects.path().to_owned()))
                .unwrap()
                .namespace(Namespace::platform(PAYLOAD_NAMESPACE).unwrap());
        let page = store
            .list(
                app.as_str(),
                ListRequest {
                    prefix: "",
                    cursor: None,
                    limit: 16,
                },
            )
            .await
            .unwrap();
        assert!(
            page.cursor.is_none(),
            "a case holding more objects than one page has outgrown this probe"
        );
        let mut objects = Vec::new();
        for entry in page.entries {
            let (bytes, _) = store
                .get(app.as_str(), &entry.key)
                .await
                .unwrap()
                .expect("a listed object must be readable");
            objects.push((entry.key, bytes));
        }
        objects
    }

    /// The manager driver this process composes beside the lane, reading the
    /// same policy source.
    fn driver(&self) -> Driver {
        Driver::new(
            self.service.manager.clone(),
            DriverOptions::default(),
            Rc::new(Undeletable),
            Rc::clone(&self.policies) as Rc<dyn PolicySource>,
            Rc::new(StaticPool { pool_slots: 1024 }),
        )
        .unwrap()
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
        self.queue
            .register_scope(&app, &zeroship_core::ZoneId::default_zone())
            .await
            .unwrap();
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

/// The lane claims a maintenance row with no worker enrolled, runs it against the
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

/// The lane claims and settles a sweep that moves payload objects.
///
/// Collection is one of the two sweeps that reach the payload store, so this is
/// the claim the lane could not make while the store was somewhere else. The
/// creator row submitted first is the control: it sits ahead in the dispatch
/// order and stays `ready`, so the lane still refuses the one kind it should.
#[compio::test]
async fn the_lane_claims_and_settles_a_sweep_that_moves_payload_objects() {
    let fixture = Box::pin(Fixture::new()).await;
    let creator = creator_work(&fixture.app);
    let collection = JobSpec {
        id: JobId::mint(),
        app_id: fixture.app.clone(),
        operation: JobOperation::Collect {},
        available_at: 0.try_into().unwrap(),
    };
    fixture.queue.submit(&creator).await.unwrap();
    fixture.queue.submit(&collection).await.unwrap();

    let swept = Box::pin(fixture.lane.sweep(&fixture.app)).await.unwrap();
    let Swept::Settled(receipt) = swept else {
        panic!("the lane settles a collection page: {swept:?}");
    };
    assert_eq!(receipt.job_id, collection.id);

    let (state, _, worker) = fixture.row(&collection.id).await;
    assert_eq!(state, "settled");
    assert_eq!(worker.as_deref(), Some(fixture.lane.identity().as_str()));
    assert_eq!(
        fixture.row(&creator.id).await.0,
        "ready",
        "creator work stays for the claimant that runs it"
    );
}

/// The lane settles an activation for an app its journal has never seen, and
/// the app is in the journal afterwards.
///
/// Every other case here seeds its app through `journal::seed_run`, which writes
/// the app's state row by hand. This one does not, because nothing on this host
/// registers an app: Control publishes an activation for every deploy of a live
/// app, and that activation is the journal's only door. An activation that needed the app
/// already entered would refuse every deploy of every app this service holds.
///
/// The app is admitted everywhere but the journal, and the empty journal read
/// before the sweep is the control that the row found afterwards is the
/// activation's own.
#[compio::test]
async fn the_lane_activates_an_app_its_journal_has_never_seen() {
    let fixture = Box::pin(Fixture::new()).await;
    let app = AppId::mint();
    fixture.admit(&app).await;
    let deployment = DeploymentId::mint();
    let registration = DeployRegistration {
        id: deployment.as_str().to_owned(),
        hash: "a".repeat(64),
        workflows: ["demo".to_owned()].into(),
        schedules: Vec::new(),
    };
    let lane = fixture.hosting(&app, &registration).await;
    assert_eq!(
        fixture.journal_app(&app).await,
        (0, None),
        "the case is about an app the journal has never seen"
    );
    // Enqueued the way Control's publication has the manager enqueue it: the
    // deployment's calendars prepared, then selected at a revision.
    let scheduler = Scheduler::new(fixture.queue.clone(), SchedulerOptions::default()).unwrap();
    scheduler
        .prepare(&RegisterSchedules {
            app_id: app.clone(),
            execution_zone_id: zeroship_core::ZoneId::default_zone(),
            deployment_id: deployment.clone(),
            schedules: Vec::new(),
        })
        .await
        .unwrap();
    let activation = scheduler
        .activate(&ActivateSchedules {
            app_id: app.clone(),
            execution_zone_id: zeroship_core::ZoneId::default_zone(),
            deployment_id: deployment,
            revision: 1.try_into().unwrap(),
        })
        .await
        .unwrap();
    assert!(matches!(
        activation.operation,
        JobOperation::Activate { .. }
    ));

    let swept = Box::pin(lane.sweep(&app)).await.unwrap();
    let Swept::Settled(receipt) = swept else {
        panic!("the lane settles the activation: {swept:?}");
    };
    assert_eq!(receipt.job_id, activation.id);
    assert_eq!(receipt.outcome, JobOutcome::Completed {});
    let (state, _, worker) = fixture.row(&activation.id).await;
    assert_eq!(state, "settled");
    assert_eq!(worker.as_deref(), Some(lane.identity().as_str()));
    assert_eq!(
        fixture.journal_app(&app).await,
        (1, Some(registration.id)),
        "the activation enters the app and selects the deployment it verified"
    );
}

/// The store the lane holds is where a staged run input lands, and the object
/// carries the value the caller handed it.
///
/// `cron_job` is the production caller and it takes this capability as an
/// argument, so this drives the argument the lane supplies. The object is read
/// back through a store of the test's own over the same root: what it proves is
/// that the writer put the bytes where the namespace says they go, which reading
/// through the lane's own handle could not.
#[compio::test]
async fn the_lanes_store_holds_the_run_input_it_staged() {
    let fixture = Box::pin(Fixture::new()).await;
    assert!(
        fixture.stored_objects(&fixture.app).await.is_empty(),
        "the case measures what staging wrote, so the store must start empty"
    );
    let engine = fixture
        .runs
        .app(fixture.policies.as_ref(), &fixture.app)
        .await
        .unwrap();
    let input = serde_json::json!({"order": "annual", "items": [1, 2, 3]});
    let expected = serde_json::to_vec(&input).unwrap();

    let reference = Box::pin(fixture.lane.payloads().stage_input(
        &engine,
        &RequestId::mint(),
        &input,
    ))
    .await
    .unwrap();
    assert_eq!(reference.size, i64::try_from(expected.len()).unwrap());
    assert_eq!(reference.content_type.as_deref(), Some("application/json"));

    let objects = fixture.stored_objects(&fixture.app).await;
    let [(_, bytes)] = objects.as_slice() else {
        panic!("staging writes exactly one object: {objects:?}");
    };
    assert_eq!(bytes, &expected);
    assert!(
        fixture
            .stored_objects(&fixture.neighbour().await)
            .await
            .is_empty(),
        "the object belongs to the app that staged it and to no other"
    );
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
/// ever claims it.
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
async fn the_drive_path_settles_a_due_maintenance_row_with_no_worker_enrolled() {
    let fixture = Box::pin(Fixture::new()).await;
    let creator = creator_work(&fixture.app);
    let maintenance = sweep(&fixture.app);
    fixture.queue.submit(&creator).await.unwrap();
    fixture.queue.submit(&maintenance).await.unwrap();
    assert_eq!(fixture.row(&maintenance.id).await.0, "ready");

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
            Some(sweeps),
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
