//! The zone claim: which apps one batch visits and in what order, which it
//! passes without taking their lock, and what it returns at its deadline.
//!
//! Every case mints a zone of its own. Apps are minted together and sorted, so
//! a case knows the order a lap visits them in, and the lowest minted id is
//! kept unregistered as a cursor that sorts before every app of the case.
#![expect(
    clippy::future_not_send,
    reason = "native queue fixtures stay on their compio runtime"
)]

use crate::support::{self, policies::Policies, Admin, Backend, Fixture, Owner, QueueCalls};

use std::{
    future::{pending, ready, Future},
    pin::Pin,
    rc::Rc,
    time::{Duration, Instant},
};
use zeroship_core::{
    app_id::AppId,
    service_peers::{service_issuer, CONTROL_SERVICE_NAME},
    workflow_coordination::{
        ManageRun, ManagementOperation, ManagementOutcome, RequestId, RunId, RunOperation,
        WorkerId,
    },
    workflow_jobs::{ClaimJobs, DeploymentId, JobId, JobLease, JobOperation, JobOutcome, JobSpec},
    workflow_policy::AppPolicy,
    zone_id::ZoneId,
};
use zeroship_data_orm::{
    orm::{Operation, Output},
    value, Value,
};
use zeroship_workflow_manager::{
    coordinator::{
        self, Admission, ClaimBatch, ClaimDeadline, ClaimReport, ClaimSkip, ClaimSkipReason,
        Coordinator, ZoneClaim,
    },
    policy::{PolicyObservation, PolicySource},
    Claimant, Error, Options, Queue,
};

macro_rules! case {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[compio::test]
        async fn $sqlite() {
            Box::pin($contract(&Fixture::new(Backend::Sqlite).await)).await;
        }
        #[compio::test]
        async fn $postgres() {
            Box::pin($contract(&Fixture::new(Backend::Postgres).await)).await;
        }
    };
}

case!(
    sqlite_a_zone_claim_takes_only_its_zones_apps,
    postgres_a_zone_claim_takes_only_its_zones_apps,
    only_its_zone
);
case!(
    sqlite_a_hot_app_cannot_starve_a_cold_one,
    postgres_a_hot_app_cannot_starve_a_cold_one,
    hot_and_cold
);
case!(
    sqlite_apps_with_nothing_to_give_are_passed_without_their_lock,
    postgres_apps_with_nothing_to_give_are_passed_without_their_lock,
    passed_without_a_lock
);
case!(
    sqlite_an_excluded_app_is_not_offered,
    postgres_an_excluded_app_is_not_offered,
    excluded_app
);
case!(
    sqlite_a_refused_kind_at_the_head_does_not_hide_the_creator_row_behind_it,
    postgres_a_refused_kind_at_the_head_does_not_hide_the_creator_row_behind_it,
    refused_kind_at_the_head
);
case!(
    sqlite_a_claim_without_a_cursor_starts_at_an_app_the_worker_names,
    postgres_a_claim_without_a_cursor_starts_at_an_app_the_worker_names,
    hashed_start
);
case!(
    sqlite_a_cursor_at_the_last_app_wraps_to_the_apps_before_it,
    postgres_a_cursor_at_the_last_app_wraps_to_the_apps_before_it,
    wraps_round
);
case!(
    sqlite_a_batch_stops_at_its_deadline_and_returns_what_it_committed,
    postgres_a_batch_stops_at_its_deadline_and_returns_what_it_committed,
    stops_at_the_deadline
);
case!(
    sqlite_an_empty_reply_cut_by_the_deadline_carries_a_moved_cursor,
    postgres_an_empty_reply_cut_by_the_deadline_carries_a_moved_cursor,
    empty_reply_moves_the_cursor
);
case!(
    sqlite_a_failed_app_is_skipped_and_the_batch_continues_past_it,
    postgres_a_failed_app_is_skipped_and_the_batch_continues_past_it,
    failure_moves_the_cursor
);
case!(
    sqlite_the_exclusion_bound_is_the_protocol_constant,
    postgres_the_exclusion_bound_is_the_protocol_constant,
    exclusion_bound
);
case!(
    sqlite_the_delivery_bound_is_the_protocol_constant,
    postgres_the_delivery_bound_is_the_protocol_constant,
    delivery_bound
);
case!(
    sqlite_an_enrollment_check_that_cannot_be_answered_skips_only_its_app,
    postgres_an_enrollment_check_that_cannot_be_answered_skips_only_its_app,
    unanswered_enrollment
);
case!(
    sqlite_a_give_back_past_the_claims_give_back_deadline_leaves_its_row_to_lapse,
    postgres_a_give_back_past_the_claims_give_back_deadline_leaves_its_row_to_lapse,
    give_back_past_its_deadline
);
case!(
    sqlite_a_grant_its_admission_spent_is_left_to_lapse,
    postgres_a_grant_its_admission_spent_is_left_to_lapse,
    spent_grant_lapses
);

/// The deadline is arithmetic over the request and the options, the same on
/// either backend, so one backend states it.
#[compio::test]
async fn the_batch_deadline_leaves_the_reply_part_of_the_wait() {
    Box::pin(deadline_inside_the_wait(&Fixture::new(Backend::Sqlite).await)).await;
}

/// Another session holds one app's scope row `FOR UPDATE` for the whole claim.
/// The claim passes that app as contended, without waiting for the lock, and
/// serves the next app while the holder is still open. Released, the same app
/// is served.
#[compio::test]
async fn postgres_a_locked_app_is_passed_as_contended_and_the_next_is_served() {
    let fixture = Fixture::new(Backend::Postgres).await;
    let zone = Zone::new(&fixture).await;
    let (cursor, apps) = zone.apps(2).await;
    let [locked, free] = <[AppId; 2]>::try_from(apps).unwrap();
    let held = zone.job(&locked).await;
    let open = zone.job(&free).await;

    hold(&fixture, &[&locked]).await;
    let (batch, report) = zone
        .claim(&WorkerId::mint(), &request(1, Some(&cursor), Vec::new()))
        .await;
    assert_eq!(delivered(&batch), [open], "{report:?}");
    assert_eq!(report.skipped, [skip(&locked, ClaimSkipReason::Contended)]);
    assert_eq!(stored(&fixture, &held).await["state"], value!("ready"));
    // Still open: the claim above returned while the holder held the row.
    release(&fixture).await;

    let (batch, report) = zone
        .claim(&WorkerId::mint(), &request(2, Some(&cursor), Vec::new()))
        .await;
    assert_eq!(delivered(&batch), [held], "{report:?}");
}

/// SQLite has no row locks: another connection holding the database's write
/// lock leaves a claimable app unavailable to this batch, never contended. The
/// control is the same claim once the lock is released.
#[compio::test]
async fn sqlite_a_held_write_lock_leaves_the_app_unavailable_never_contended() {
    let fixture = Fixture::new(Backend::Sqlite).await;
    let zone = Zone::new(&fixture).await;
    let (cursor, apps) = zone.apps(1).await;
    let job = zone.job(&apps[0]).await;

    hold(&fixture, &[&apps[0]]).await;
    let (batch, report) = zone
        .claim_until(
            &WorkerId::mint(),
            &request(1, Some(&cursor), Vec::new()),
            Instant::now() + Duration::from_secs(1),
            zone.policies.as_ref(),
        )
        .await
        .unwrap();
    release(&fixture).await;
    assert!(batch.grants.is_empty(), "{batch:?}");
    assert_eq!(report.skipped, [skip(&apps[0], ClaimSkipReason::Unavailable)]);

    let (batch, report) = zone
        .claim(&WorkerId::mint(), &request(1, Some(&cursor), Vec::new()))
        .await;
    assert_eq!(delivered(&batch), [job], "{report:?}");
}

/// A deadline no case reaches.
fn unbounded() -> Instant {
    Instant::now() + Duration::from_hours(1)
}

fn request(max: u32, after: Option<&AppId>, exclude: Vec<AppId>) -> ClaimJobs {
    ClaimJobs {
        max: max.try_into().unwrap(),
        wait_ms: 3_600_000.try_into().unwrap(),
        after: after.cloned(),
        exclude,
    }
}

fn advance(app: &AppId, run: RunId) -> JobSpec {
    JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation: JobOperation::Advance {
            deployment_id: DeploymentId::mint(),
            run_id: run,
            generation: 0,
            revision: 1.try_into().unwrap(),
        },
        available_at: 0.try_into().unwrap(),
    }
}

/// One zone of a case, its queue and the policy it observes.
struct Zone {
    queue: Queue,
    coordinator: Coordinator,
    policies: Rc<Policies>,
    id: ZoneId,
}

impl Zone {
    async fn new(fixture: &Fixture) -> Self {
        Self::with(fixture, coordinator::Options::default()).await
    }

    async fn with(fixture: &Fixture, options: coordinator::Options) -> Self {
        Self::build(fixture, Options::default(), options).await
    }

    /// A zone whose queue grants leases of `lease`.
    async fn leasing(fixture: &Fixture, lease: Duration) -> Self {
        Self::build(
            fixture,
            Options {
                lease,
                ..Options::default()
            },
            coordinator::Options::default(),
        )
        .await
    }

    async fn build(fixture: &Fixture, queue: Options, options: coordinator::Options) -> Self {
        let queue = Queue::connect(
            fixture.binding(),
            fixture.url(),
            queue,
            support::synthetic_holds(),
        )
        .await
        .unwrap();
        let coordinator = support::coordinator(&queue, options);
        Self {
            queue,
            coordinator,
            policies: Policies::new(),
            id: ZoneId::mint(),
        }
    }

    /// The same queue and policy, in another zone.
    fn beside(&self) -> Self {
        Self {
            queue: self.queue.clone(),
            coordinator: self.coordinator.clone(),
            policies: self.policies.clone(),
            id: ZoneId::mint(),
        }
    }

    /// A cursor that sorts before `count` apps of this zone, and the apps in
    /// the order a lap visits them, each observed under the default policy.
    async fn apps(&self, count: usize) -> (AppId, Vec<AppId>) {
        let mut minted: Vec<AppId> = (0..=count).map(|_| AppId::mint()).collect();
        minted.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        let cursor = minted.remove(0);
        for app in &minted {
            self.policies.app(app, &self.id);
            self.queue.register_scope(app, &self.id).await.unwrap();
        }
        (cursor, minted)
    }

    async fn job(&self, app: &AppId) -> JobSpec {
        self.run_job(app, RunId::mint()).await
    }

    async fn run_job(&self, app: &AppId, run: RunId) -> JobSpec {
        let job = advance(app, run);
        assert_eq!(self.queue.submit(&job).await.unwrap(), job);
        job
    }

    /// A claim no deadline cuts, admitting every grant as it stands.
    async fn claim(&self, worker: &WorkerId, request: &ClaimJobs) -> (ClaimBatch<()>, ClaimReport) {
        self.claim_until(worker, request, unbounded(), self.policies.as_ref())
            .await
            .unwrap()
    }

    async fn claim_until(
        &self,
        worker: &WorkerId,
        request: &ClaimJobs,
        deadline: Instant,
        policies: &dyn PolicySource,
    ) -> Result<(ClaimBatch<()>, ClaimReport), Error> {
        let claim = ZoneClaim {
            worker,
            zone: &self.id,
            request,
            deadline: ClaimDeadline::at(deadline),
        };
        self.coordinator
            .claim_in_zone(
                &claim,
                policies,
                || ready(Ok(worker.clone())),
                |_| ready(Admission::Deliver(())),
            )
            .await
    }
}

/// The jobs a batch delivered, in claim order.
fn delivered(batch: &ClaimBatch<()>) -> Vec<JobSpec> {
    batch
        .grants
        .iter()
        .map(|(grant, ())| grant.delivery().job.clone())
        .collect()
}

fn skip(app: &AppId, reason: ClaimSkipReason) -> ClaimSkip {
    ClaimSkip {
        app_id: app.clone(),
        reason,
    }
}

async fn stored(fixture: &Fixture, job: &JobSpec) -> Value {
    let Output::Rows(rows) = fixture
        .database()
        .await
        .collection("jobs")
        .unwrap()
        .find(
            value!({"id":job.id.as_str(),"app_id":job.app_id.as_str()}),
            value!({"limit":1}),
        )
        .await
        .unwrap()
    else {
        panic!("job query returned a count");
    };
    rows.into_iter().next().expect("the job was submitted")
}

async fn patch(fixture: &Fixture, job: &JobSpec, changes: Value) {
    let updated = fixture
        .database()
        .await
        .collection("jobs")
        .unwrap()
        .execute(Operation::Update {
            filter: value!({"id":job.id.as_str(),"app_id":job.app_id.as_str()}),
            patch: changes,
            many: true,
        })
        .await
        .unwrap();
    assert!(matches!(updated, Output::Count(1)), "{updated:?}");
}

/// Another session holds what a claim would lock: on PostgreSQL each app's
/// scope row `FOR UPDATE`, on SQLite the database-wide write lock. Held until
/// [`release`].
async fn hold(fixture: &Fixture, apps: &[&AppId]) {
    match &fixture.admin {
        Admin::Postgres(admin) => {
            admin.batch_execute("BEGIN").await.unwrap();
            for app in apps {
                let held = admin
                    .query(
                        "SELECT id FROM workflow_manager.queue_scopes WHERE id=$1 FOR UPDATE",
                        &[&app.as_str()],
                    )
                    .await
                    .unwrap();
                assert_eq!(held.len(), 1, "the blocker holds the scope row");
            }
        }
        Admin::Sqlite(admin) => admin.execute_batch("BEGIN IMMEDIATE").unwrap(),
    }
}

async fn release(fixture: &Fixture) {
    match &fixture.admin {
        Admin::Postgres(admin) => admin.batch_execute("COMMIT").await.unwrap(),
        Admin::Sqlite(admin) => admin.execute_batch("COMMIT").unwrap(),
    }
}

/// Policy that never answers for one app, so a visit to it lasts until the
/// claim's deadline cuts it. Every other app is answered as `inner` answers.
#[derive(Debug)]
struct Stalled {
    inner: Rc<Policies>,
    app: AppId,
}

impl PolicySource for Stalled {
    fn observe<'a>(
        &'a self,
        app: &'a AppId,
    ) -> Pin<Box<dyn Future<Output = Result<PolicyObservation, Error>> + 'a>> {
        if app == &self.app {
            return Box::pin(pending());
        }
        self.inner.observe(app)
    }
}

/// A claim pages only the zone its credential names. The control is the same
/// worker claiming in the other zone, which takes that zone's app and only it.
async fn only_its_zone(fixture: &Fixture) {
    let near = Zone::new(fixture).await;
    let far = near.beside();
    let (_, near_apps) = near.apps(1).await;
    let (_, far_apps) = far.apps(1).await;
    let near_job = near.job(&near_apps[0]).await;
    let far_job = far.job(&far_apps[0]).await;
    let worker = WorkerId::mint();

    let (batch, report) = near.claim(&worker, &request(2, None, Vec::new())).await;
    assert_eq!(delivered(&batch), [near_job], "{report:?}");
    assert_eq!(report, ClaimReport::default(), "the other zone's app is never visited");
    assert_eq!(stored(fixture, &far_job).await["state"], value!("ready"));

    let (batch, report) = far.claim(&worker, &request(2, None, Vec::new())).await;
    assert_eq!(delivered(&batch), [far_job], "{report:?}");
}

/// One batch from a cursor serves both a hot app and a cold one: a lap takes
/// one job per app. The control is a hot app alone, which fills the batch
/// across laps in its own ticket order.
async fn hot_and_cold(fixture: &Fixture) {
    let mixed = Zone::new(fixture).await;
    let (cursor, apps) = mixed.apps(2).await;
    let (hot, cold) = (&apps[0], &apps[1]);
    let mut hot_jobs = Vec::new();
    for _ in 0..3 {
        hot_jobs.push(mixed.job(hot).await);
    }
    let cold_job = mixed.job(cold).await;

    let (batch, report) = mixed
        .claim(&WorkerId::mint(), &request(2, Some(&cursor), Vec::new()))
        .await;
    assert_eq!(
        delivered(&batch),
        [hot_jobs[0].clone(), cold_job],
        "{report:?}"
    );

    let alone = mixed.beside();
    let (cursor, apps) = alone.apps(1).await;
    let mut tickets = Vec::new();
    for _ in 0..3 {
        tickets.push(alone.job(&apps[0]).await);
    }
    let (batch, report) = alone
        .claim(&WorkerId::mint(), &request(3, Some(&cursor), Vec::new()))
        .await;
    assert_eq!(delivered(&batch), tickets, "{report:?}");
}

/// An app at its concurrency cap, one whose dispatch is off, a deleted one, one
/// whose only row has spent its delivery budget and one whose only row waits
/// behind a management barrier are each passed with that reason while another
/// session holds what their claim would lock - so none of them was locked -
/// and the cursor moves past every one. Released, each app's control differs
/// from it in exactly the property that passed it, and every one is served.
async fn passed_without_a_lock(fixture: &Fixture) {
    let zone = Zone::new(fixture).await;
    let (cursor, apps) = zone.apps(5).await;
    let passed = Passed::arrange(fixture, &zone, apps).await;
    let Passed {
        capped,
        disabled,
        deleted,
        exhausted,
        blocked,
        jobs,
    } = &passed;

    hold(fixture, &[capped, disabled, deleted, exhausted, blocked]).await;
    let (batch, report) = zone
        .claim(&WorkerId::mint(), &request(5, Some(&cursor), Vec::new()))
        .await;
    release(fixture).await;
    assert!(batch.grants.is_empty(), "{batch:?}");
    assert_eq!(
        report.skipped,
        [
            skip(capped, ClaimSkipReason::AtCap),
            skip(disabled, ClaimSkipReason::PolicyOff),
            skip(deleted, ClaimSkipReason::Denied),
            skip(exhausted, ClaimSkipReason::NoCandidate),
            skip(blocked, ClaimSkipReason::NoCandidate),
        ]
    );
    assert_eq!(batch.after.as_ref(), Some(blocked), "the cursor passed every app");
    assert!(batch.lap_complete);
    for job in jobs {
        assert_eq!(stored(fixture, job).await["state"], value!("ready"));
    }

    passed.lift(&zone).await;
    let (batch, report) = zone
        .claim(&WorkerId::mint(), &request(5, Some(&cursor), Vec::new()))
        .await;
    assert_eq!(delivered(&batch), *jobs, "{report:?}");
}

/// Five apps of one zone, each with nothing to give for one reason, and the
/// ready job each would deliver once that reason is lifted, in the same order.
struct Passed {
    capped: AppId,
    disabled: AppId,
    deleted: AppId,
    exhausted: AppId,
    blocked: AppId,
    jobs: [JobSpec; 5],
}

impl Passed {
    async fn arrange(fixture: &Fixture, zone: &Zone, apps: Vec<AppId>) -> Self {
        let [capped, disabled, deleted, exhausted, blocked] =
            <[AppId; 5]>::try_from(apps).unwrap();
        zone.policies.with_policy(
            &capped,
            &zone.id,
            AppPolicy {
                max_running: 1,
                ..AppPolicy::default()
            },
        );
        let running = zone.job(&capped).await;
        let waiting = zone.job(&capped).await;
        let holder = Owner::new(capped.clone(), WorkerId::mint());
        let held = zone.queue.claim(&holder).await.unwrap().unwrap();
        assert_eq!(held.delivery().job, running);
        zone.policies.with_policy(
            &disabled,
            &zone.id,
            AppPolicy {
                dispatch: false,
                ..AppPolicy::default()
            },
        );
        let disabled_job = zone.job(&disabled).await;
        let deleted_job = zone.job(&deleted).await;
        zone.policies.delete(&deleted);
        let exhausted_job = zone.job(&exhausted).await;
        patch(
            fixture,
            &exhausted_job,
            value!({"execution_attempts":AppPolicy::default().max_delivery_attempts}),
        )
        .await;
        let run = RunId::mint();
        let blocked_job = zone.run_job(&blocked, run.clone()).await;
        zone.coordinator
            .manage(
                &service_issuer(CONTROL_SERVICE_NAME).unwrap(),
                &ManageRun {
                    request_id: RequestId::mint(),
                    app_id: blocked.clone(),
                    run_id: run,
                    command: ManagementOperation::Transition {
                        operation: RunOperation::Pause,
                    },
                },
            )
            .await
            .unwrap();
        Self {
            capped,
            disabled,
            deleted,
            exhausted,
            blocked,
            jobs: [waiting, disabled_job, deleted_job, exhausted_job, blocked_job],
        }
    }

    /// Lift each app's reason and nothing else: room under the cap, dispatch
    /// on, the deletion undone, one more attempt in the budget, and the
    /// barrier's command settled by the lane that takes it.
    async fn lift(&self, zone: &Zone) {
        zone.policies.with_policy(
            &self.capped,
            &zone.id,
            AppPolicy {
                max_running: 2,
                ..AppPolicy::default()
            },
        );
        zone.policies.app(&self.disabled, &zone.id);
        zone.policies.app(&self.deleted, &zone.id);
        zone.policies.with_policy(
            &self.exhausted,
            &zone.id,
            AppPolicy {
                max_delivery_attempts: AppPolicy::default().max_delivery_attempts + 1,
                ..AppPolicy::default()
            },
        );
        let lane = Owner::new(self.blocked.clone(), WorkerId::mint());
        let barrier = zone
            .queue
            .claim_authorized(
                &lane.app_id,
                &lane.worker_id,
                Claimant::Maintenance,
                Ok(support::delivery_ceiling()),
                |_| ready(Ok(lane.worker_id.clone())),
            )
            .await
            .unwrap()
            .expect("the lane takes the management command");
        zone.queue
            .settle(
                &lane,
                &support::settlement(
                    barrier.delivery(),
                    JobOutcome::Management {
                        outcome: ManagementOutcome::Conflict {},
                    },
                ),
            )
            .await
            .unwrap();
    }
}

/// An app the caller excludes is not visited at all. The control is the same
/// claim with nothing excluded, which serves it.
async fn excluded_app(fixture: &Fixture) {
    let zone = Zone::new(fixture).await;
    let (cursor, apps) = zone.apps(2).await;
    let (left, right) = (&apps[0], &apps[1]);
    let excluded_job = zone.job(left).await;
    let first = zone.job(right).await;
    let second = zone.job(right).await;

    let (batch, report) = zone
        .claim(
            &WorkerId::mint(),
            &request(1, Some(&cursor), vec![left.clone()]),
        )
        .await;
    assert_eq!(delivered(&batch), [first], "{report:?}");
    assert_eq!(report, ClaimReport::default(), "an excluded app is not offered");
    assert_eq!(stored(fixture, &excluded_job).await["state"], value!("ready"));

    let (batch, report) = zone
        .claim(&WorkerId::mint(), &request(2, Some(&cursor), Vec::new()))
        .await;
    assert_eq!(delivered(&batch), [excluded_job, second], "{report:?}");
}

/// A sweep at the head of an app's dispatch order does not hide the creator row
/// behind it from a worker: the kinds a worker refuses are excluded while its
/// candidates are chosen, not after one is loaded.
///
/// The control differs only in the claimant: the lane that takes sweeps answers
/// the head of the same queue. And the refusal itself: an app whose only row is
/// a sweep is not offered to a worker at all, and the lane takes that row.
async fn refused_kind_at_the_head(fixture: &Fixture) {
    let zone = Zone::new(fixture).await;
    let (cursor, apps) = zone.apps(2).await;
    let (mixed, sweeps_only) = (&apps[0], &apps[1]);
    let head = sweep(mixed);
    zone.queue.submit(&head).await.unwrap();
    let behind = zone.job(mixed).await;
    assert!(
        stored(fixture, &head).await["dispatch_order"].as_i64()
            < stored(fixture, &behind).await["dispatch_order"].as_i64(),
        "the sweep must sit at the head for this to measure anything"
    );
    let only_sweep = sweep(sweeps_only);
    zone.queue.submit(&only_sweep).await.unwrap();

    let (batch, report) = zone
        .claim(&WorkerId::mint(), &request(2, Some(&cursor), Vec::new()))
        .await;
    assert_eq!(delivered(&batch), [behind], "{report:?}");
    assert_eq!(
        report,
        ClaimReport::default(),
        "an app whose only row is a sweep is not offered to a worker"
    );

    for (app, expected) in [(mixed, head), (sweeps_only, only_sweep)] {
        let lane = Owner::new(app.clone(), WorkerId::mint());
        let taken = zone
            .queue
            .claim_authorized(
                &lane.app_id,
                &lane.worker_id,
                Claimant::Maintenance,
                Ok(support::delivery_ceiling()),
                |_| ready(Ok(lane.worker_id.clone())),
            )
            .await
            .unwrap()
            .expect("the lane takes the sweep a worker refused");
        assert_eq!(taken.delivery().job, expected);
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

/// Without a cursor a claim starts at an app of the zone's candidates that the
/// worker's identity selects: always one that exists, the same one for the
/// same worker, and not the same one for every worker.
async fn hashed_start(fixture: &Fixture) {
    let zone = Zone::new(fixture).await;
    let (_, apps) = zone.apps(8).await;
    // Enough rows, and room under the concurrency cap, that no app is passed
    // or leaves the candidate page while the workers below claim.
    for app in &apps {
        zone.policies.with_policy(
            app,
            &zone.id,
            AppPolicy {
                max_running: 64,
                ..AppPolicy::default()
            },
        );
        for _ in 0..19 {
            zone.job(app).await;
        }
    }
    let start = |batch: &ClaimBatch<()>| {
        let [(grant, ())] = batch.grants.as_slice() else {
            panic!("a claim of one delivers one job: {batch:?}");
        };
        let app = grant.delivery().job.app_id.clone();
        assert!(apps.contains(&app), "the start is an app of the zone");
        assert_eq!(batch.after.as_ref(), Some(&app), "the cursor is where it started");
        app
    };

    let worker = WorkerId::mint();
    let (first, _) = zone.claim(&worker, &request(1, None, Vec::new())).await;
    let (again, _) = zone.claim(&worker, &request(1, None, Vec::new())).await;
    assert_eq!(start(&first), start(&again), "one worker starts at one app");

    let mut starts = std::collections::BTreeSet::new();
    for _ in 0..16 {
        let (batch, _) = zone
            .claim(&WorkerId::mint(), &request(1, None, Vec::new()))
            .await;
        starts.insert(start(&batch).as_str().to_owned());
    }
    assert!(
        starts.len() > 1,
        "workers without a cursor all started at {starts:?}"
    );
}

/// A cursor at the zone's last app continues from the start of the zone, so a
/// worker whose cursor reached the end still reaches the apps before it.
async fn wraps_round(fixture: &Fixture) {
    let zone = Zone::new(fixture).await;
    let (_, apps) = zone.apps(2).await;
    let first = zone.job(&apps[0]).await;
    let last = zone.job(&apps[1]).await;

    let (batch, report) = zone
        .claim(&WorkerId::mint(), &request(2, Some(&apps[1]), Vec::new()))
        .await;
    assert_eq!(delivered(&batch), [first, last], "{report:?}");
    assert_eq!(batch.after.as_ref(), Some(&apps[1]));
}

/// A batch whose deadline passes during one app's visit starts no attempt
/// after it, and returns the delivery it committed before. The app whose visit
/// the deadline cut is recorded and the cursor rests on it; the app after it
/// is untouched. The control is the same claim with nothing stalled, which
/// serves the apps the cut one left.
async fn stops_at_the_deadline(fixture: &Fixture) {
    let zone = Zone::new(fixture).await;
    let (cursor, apps) = zone.apps(3).await;
    let [first, stalled, after] = <[AppId; 3]>::try_from(apps).unwrap();
    let served = zone.job(&first).await;
    let cut = zone.job(&stalled).await;
    let untouched = zone.job(&after).await;
    let policies = Stalled {
        inner: zone.policies.clone(),
        app: stalled.clone(),
    };

    let deadline = Instant::now() + Duration::from_secs(2);
    let (batch, report) = zone
        .claim_until(
            &WorkerId::mint(),
            &request(3, Some(&cursor), Vec::new()),
            deadline,
            &policies,
        )
        .await
        .unwrap();
    assert!(Instant::now() >= deadline);
    assert_eq!(delivered(&batch), [served], "{report:?}");
    assert!(batch.grants[0].0.lease().is_ok(), "the committed grant is live");
    assert_eq!(report.skipped, [skip(&stalled, ClaimSkipReason::Unavailable)]);
    assert_eq!(batch.after.as_ref(), Some(&stalled));
    assert!(!batch.lap_complete, "a batch cut by its deadline did not finish its lap");
    assert_eq!(stored(fixture, &untouched).await["attempt"], value!(0));

    let (batch, report) = zone
        .claim(&WorkerId::mint(), &request(3, Some(&cursor), Vec::new()))
        .await;
    assert_eq!(delivered(&batch), [cut, untouched], "{report:?}");
}

/// A claim that delivers nothing before its deadline still moves its cursor
/// past the app it visited, and says it did not finish its lap, so the worker
/// asks again at once and continues after that app.
async fn empty_reply_moves_the_cursor(fixture: &Fixture) {
    let zone = Zone::new(fixture).await;
    let (cursor, apps) = zone.apps(2).await;
    let [stalled, next] = <[AppId; 2]>::try_from(apps).unwrap();
    zone.job(&stalled).await;
    let waiting = zone.job(&next).await;
    let policies = Stalled {
        inner: zone.policies.clone(),
        app: stalled.clone(),
    };
    let worker = WorkerId::mint();

    let (batch, report) = zone
        .claim_until(
            &worker,
            &request(2, Some(&cursor), Vec::new()),
            Instant::now() + Duration::from_secs(1),
            &policies,
        )
        .await
        .unwrap();
    assert!(batch.grants.is_empty(), "{batch:?}");
    assert!(!batch.lap_complete);
    assert_eq!(batch.after.as_ref(), Some(&stalled), "{report:?}");

    let (batch, report) = zone
        .claim(&worker, &request(1, batch.after.as_ref(), Vec::new()))
        .await;
    assert_eq!(delivered(&batch), [waiting], "{report:?}");
}

/// An app whose claim fails is a skip, not a failed batch: the delivery the
/// batch committed before it is returned, and the app after it is served.
async fn failure_moves_the_cursor(fixture: &Fixture) {
    let zone = Zone::new(fixture).await;
    let (cursor, apps) = zone.apps(3).await;
    let [first, broken, last] = <[AppId; 3]>::try_from(apps).unwrap();
    let before = zone.job(&first).await;
    let damaged = zone.job(&broken).await;
    let after = zone.job(&last).await;
    // A dispatch ticket the scope never issued, which the claim refuses as a
    // storage contract failure.
    patch(fixture, &damaged, value!({"dispatch_order":0})).await;

    let (batch, report) = zone
        .claim(&WorkerId::mint(), &request(2, Some(&cursor), Vec::new()))
        .await;
    assert_eq!(delivered(&batch), [before, after], "{report:?}");
    assert_eq!(
        report.skipped,
        [skip(&broken, ClaimSkipReason::Failed(Error::Storage))]
    );
    assert_eq!(batch.after.as_ref(), Some(&last));
    assert_eq!(stored(fixture, &damaged).await["state"], value!("ready"));
}

/// The longest exclusion list a claim may carry is the protocol constant,
/// whatever page size the service is configured with: a list at the bound is
/// served under the smallest page, and one entry more is refused.
async fn exclusion_bound(fixture: &Fixture) {
    let zone = Zone::with(
        fixture,
        coordinator::Options {
            batch_limit: 1,
            ..coordinator::Options::default()
        },
    )
    .await;
    let (_, apps) = zone.apps(1).await;
    let job = zone.job(&apps[0]).await;
    let mut exclude: Vec<AppId> = (0..ClaimJobs::MAX_EXCLUDE).map(|_| AppId::mint()).collect();
    assert!(!exclude.contains(&apps[0]));
    let worker = WorkerId::mint();

    exclude.push(AppId::mint());
    let refused = zone
        .claim_until(
            &worker,
            &request(1, None, exclude.clone()),
            unbounded(),
            zone.policies.as_ref(),
        )
        .await;
    assert_eq!(refused.err(), Some(Error::Invalid));
    assert_eq!(stored(fixture, &job).await["state"], value!("ready"));

    exclude.pop();
    let (batch, report) = zone.claim(&worker, &request(1, None, exclude)).await;
    assert_eq!(delivered(&batch), [job], "{report:?}");
}

/// The batch deadline is inside the caller's wait and never later than the
/// service's own claim budget, and every give-back ends after the attempts
/// and still inside the wait, so part of the wait is left for the reply.
async fn deadline_inside_the_wait(fixture: &Fixture) {
    let budget = Duration::from_secs(5);
    let zone = Zone::with(
        fixture,
        coordinator::Options {
            claim_budget: budget,
            ..coordinator::Options::default()
        },
    )
    .await;
    let arrival = Instant::now();
    for wait in [Duration::from_millis(400), Duration::from_secs(4), Duration::from_mins(1)] {
        let ask = ClaimJobs {
            wait_ms: u64::try_from(wait.as_millis()).unwrap().try_into().unwrap(),
            ..request(1, None, Vec::new())
        };
        let deadline = zone.coordinator.claim_deadline(arrival, &ask).unwrap();
        assert!(deadline.attempts > arrival, "a wait of {wait:?} leaves the batch nothing");
        assert!(
            deadline.give_backs > deadline.attempts,
            "a wait of {wait:?} leaves a cut grant no time to go back"
        );
        assert!(
            deadline.give_backs < arrival + wait,
            "a wait of {wait:?} leaves the reply nothing"
        );
        assert!(
            deadline.attempts <= arrival + budget,
            "a wait of {wait:?} outlasts the budget"
        );
    }
}

case!(
    sqlite_a_full_reply_ends_the_batch_and_its_unsent_row_comes_back_first,
    postgres_a_full_reply_ends_the_batch_and_its_unsent_row_comes_back_first,
    full_reply
);

/// A host whose reply has no room for a grant ends the batch there: the grant's
/// row goes back unsent - claimable at once, no attempt counted - the app after
/// it is not visited, and the cursor rests before the unsent app so the next
/// claim begins with it. The control is the same batch with room for all.
async fn full_reply(fixture: &Fixture) {
    let zone = Zone::new(fixture).await;
    let (cursor, apps) = zone.apps(3).await;
    let [first, unsent, after] = <[AppId; 3]>::try_from(apps).unwrap();
    let sent = zone.job(&first).await;
    let refused = zone.job(&unsent).await;
    let untouched = zone.job(&after).await;
    let worker = WorkerId::mint();
    let ask = request(3, Some(&cursor), Vec::new());
    let admitted = std::cell::Cell::new(0);
    let claim = ZoneClaim {
        worker: &worker,
        zone: &zone.id,
        request: &ask,
        deadline: ClaimDeadline::at(unbounded()),
    };
    let (batch, report) = zone
        .coordinator
        .claim_in_zone(
            &claim,
            zone.policies.as_ref(),
            || ready(Ok(worker.clone())),
            |_| {
                admitted.set(admitted.get() + 1);
                ready(if admitted.get() == 1 {
                    Admission::Deliver(())
                } else {
                    Admission::Full
                })
            },
        )
        .await
        .unwrap();
    assert_eq!(delivered(&batch), [sent], "{report:?}");
    assert_eq!(report.skipped, [skip(&unsent, ClaimSkipReason::Full)]);
    assert_eq!(batch.after.as_ref(), Some(&first), "the cursor rests before the unsent app");
    assert!(!batch.lap_complete);
    let row = stored(fixture, &refused).await;
    assert_eq!(row["state"], value!("ready"));
    assert_eq!(row["worker_id"], Value::Null);
    assert_eq!(row["deferred_until"], Value::Null);
    assert_eq!(row["execution_attempts"], value!(0));
    assert_eq!(stored(fixture, &untouched).await["attempt"], value!(0));

    let (batch, report) = zone
        .claim(&worker, &request(3, batch.after.as_ref(), Vec::new()))
        .await;
    assert_eq!(delivered(&batch), [refused, untouched], "{report:?}");
}

case!(
    sqlite_an_app_passed_at_its_cap_is_offered_again_once_the_cap_lifts,
    postgres_an_app_passed_at_its_cap_is_offered_again_once_the_cap_lifts,
    passed_at_cap_comes_round_again
);

/// An app the claim passed at its concurrency cap is offered again by the
/// worker's next claim once the cap lifts, although the cursor that claim
/// returned rests on that very app: the next claim comes round to it rather
/// than continuing past the end of the zone.
async fn passed_at_cap_comes_round_again(fixture: &Fixture) {
    let zone = Zone::new(fixture).await;
    let (_, apps) = zone.apps(1).await;
    let app = &apps[0];
    zone.policies.with_policy(
        app,
        &zone.id,
        AppPolicy {
            max_running: 1,
            ..AppPolicy::default()
        },
    );
    let first = zone.job(app).await;
    let second = zone.job(app).await;
    let worker = WorkerId::mint();

    let (batch, report) = zone.claim(&worker, &request(2, None, Vec::new())).await;
    assert_eq!(delivered(&batch), [first], "{report:?}");
    assert_eq!(report.skipped, [skip(app, ClaimSkipReason::AtCap)]);
    assert_eq!(batch.after.as_ref(), Some(app));

    zone.policies.app(app, &zone.id);
    let (batch, report) = zone
        .claim(&worker, &request(2, batch.after.as_ref(), Vec::new()))
        .await;
    assert_eq!(delivered(&batch), [second], "{report:?}");
}

/// An app passed because another session held its lock is offered again by
/// the worker's next claim once the lock is released, although the cursor the
/// first claim returned rests on it.
#[compio::test]
async fn postgres_an_app_passed_as_contended_is_offered_again_once_released() {
    let fixture = Fixture::new(Backend::Postgres).await;
    let zone = Zone::new(&fixture).await;
    let (_, apps) = zone.apps(1).await;
    let app = &apps[0];
    let job = zone.job(app).await;
    let worker = WorkerId::mint();

    hold(&fixture, &[app]).await;
    let (batch, report) = zone.claim(&worker, &request(1, None, Vec::new())).await;
    release(&fixture).await;
    assert!(batch.grants.is_empty(), "{batch:?}");
    assert_eq!(report.skipped, [skip(app, ClaimSkipReason::Contended)]);
    assert_eq!(batch.after.as_ref(), Some(app));

    let (batch, report) = zone
        .claim(&worker, &request(1, batch.after.as_ref(), Vec::new()))
        .await;
    assert_eq!(delivered(&batch), [job], "{report:?}");
}

/// The most deliveries a claim may ask for is the protocol constant: a request
/// one above it, or at the largest number the wire can carry, is refused before
/// anything is claimed, and nothing is sized from it. The control is a request
/// at the bound, which is served.
async fn delivery_bound(fixture: &Fixture) {
    let zone = Zone::new(fixture).await;
    let (_, apps) = zone.apps(1).await;
    let job = zone.job(&apps[0]).await;
    let worker = WorkerId::mint();

    for max in [ClaimJobs::MAX_DELIVERIES + 1, u32::MAX] {
        let refused = zone
            .claim_until(
                &worker,
                &request(max, None, Vec::new()),
                unbounded(),
                zone.policies.as_ref(),
            )
            .await;
        assert_eq!(refused.err(), Some(Error::Invalid), "max {max}");
        assert_eq!(stored(fixture, &job).await["state"], value!("ready"));
    }

    let (batch, report) = zone
        .claim(&worker, &request(ClaimJobs::MAX_DELIVERIES, None, Vec::new()))
        .await;
    assert_eq!(delivered(&batch), [job], "{report:?}");
}

/// An enrollment check that cannot be answered while one app is claimed is that
/// app's skip, not the batch's failure: the grant committed before it still
/// goes out and the app after it is served. Only a refusal of the enrollment
/// fails the request.
async fn unanswered_enrollment(fixture: &Fixture) {
    let zone = Zone::new(fixture).await;
    let (cursor, apps) = zone.apps(3).await;
    let [first, unanswered, last] = <[AppId; 3]>::try_from(apps).unwrap();
    let before = zone.job(&first).await;
    let skipped = zone.job(&unanswered).await;
    let after = zone.job(&last).await;
    let worker = WorkerId::mint();
    // Two deliveries, so the batch ends with the lap that skipped the app.
    let ask = request(2, Some(&cursor), Vec::new());
    let claim = ZoneClaim {
        worker: &worker,
        zone: &zone.id,
        request: &ask,
        deadline: ClaimDeadline::at(unbounded()),
    };
    // The first grant's admission arms the failure, so the next check - the
    // one the second app's claim makes under its lock - cannot be answered.
    let fail_next = std::cell::Cell::new(false);
    let (batch, report) = zone
        .coordinator
        .claim_in_zone(
            &claim,
            zone.policies.as_ref(),
            || {
                ready(if fail_next.replace(false) {
                    Err(Error::Unavailable)
                } else {
                    Ok(worker.clone())
                })
            },
            |grant| {
                if grant.delivery().job.app_id == first {
                    fail_next.set(true);
                }
                ready(Admission::Deliver(()))
            },
        )
        .await
        .expect("an unanswered check fails its app, not the request");
    assert_eq!(delivered(&batch), [before, after], "{report:?}");
    assert_eq!(
        report.skipped,
        [skip(&unanswered, ClaimSkipReason::Unavailable)]
    );
    assert_eq!(stored(fixture, &skipped).await["state"], value!("ready"));

    // The control: the same check answered with a refusal of the enrollment
    // fails the whole request. Nothing is delivered, the app the refusal came
    // for keeps its row claimable, and the batch visits no app after it.
    let (cursor, apps) = zone.apps(3).await;
    let [first, refused, last] = <[AppId; 3]>::try_from(apps).unwrap();
    zone.job(&first).await;
    let denied = zone.job(&refused).await;
    let untouched = zone.job(&last).await;
    let ask = request(2, Some(&cursor), Vec::new());
    let claim = ZoneClaim {
        worker: &worker,
        zone: &zone.id,
        request: &ask,
        deadline: ClaimDeadline::at(unbounded()),
    };
    let fail_next = std::cell::Cell::new(false);
    let outcome = zone
        .coordinator
        .claim_in_zone(
            &claim,
            zone.policies.as_ref(),
            || {
                ready(if fail_next.replace(false) {
                    Err(Error::Denied)
                } else {
                    Ok(worker.clone())
                })
            },
            |grant| {
                if grant.delivery().job.app_id == first {
                    fail_next.set(true);
                }
                ready(Admission::Deliver(()))
            },
        )
        .await;
    assert_eq!(
        outcome.err(),
        Some(Error::Denied),
        "a refused enrollment fails the request"
    );
    assert_eq!(stored(fixture, &denied).await["state"], value!("ready"));
    assert_eq!(stored(fixture, &untouched).await["state"], value!("ready"));
}

/// The concurrency cap is counted again under the app's lock. A claim whose
/// lock-free count found the app's last slot free is refused it when another
/// claim leased that slot before this one took the lock.
///
/// The other lease is committed from another session while this claim holds
/// the app's lock, after its lock-free count and before the count it makes
/// under the lock: the window two workers' claims race in. The control is the
/// same claim with nothing committed in that window, which takes the slot.
#[compio::test]
async fn postgres_a_slot_leased_after_the_lock_free_count_is_not_leased_twice() {
    let fixture = Fixture::new(Backend::Postgres).await;
    let Admin::Postgres(admin) = &fixture.admin else {
        unreachable!("a PostgreSQL fixture has a PostgreSQL admin")
    };
    let zone = Zone::new(&fixture).await;
    for raced in [true, false] {
        let (cursor, apps) = zone.apps(1).await;
        let app = &apps[0];
        zone.policies.with_policy(
            app,
            &zone.id,
            AppPolicy {
                max_running: 1,
                ..AppPolicy::default()
            },
        );
        let head = zone.job(app).await;
        let rival = zone.job(app).await;
        let worker = WorkerId::mint();
        let ask = request(1, Some(&cursor), Vec::new());
        let claim = ZoneClaim {
            worker: &worker,
            zone: &zone.id,
            request: &ask,
            deadline: ClaimDeadline::at(unbounded()),
        };
        // The batch checks the caller once before any app, then again under
        // each app's lock: the second check is inside the window.
        let checks = std::cell::Cell::new(0);
        let (batch, report) = zone
            .coordinator
            .claim_in_zone(
                &claim,
                zone.policies.as_ref(),
                || {
                    checks.set(checks.get() + 1);
                    let race = raced && checks.get() == 2;
                    let (rival, worker) = (&rival, &worker);
                    async move {
                        if race {
                            let leased = admin
                                .execute(
                                    "UPDATE workflow_manager.jobs SET state='leased', attempt=1, \
                                     worker_id=$3, leased_at=CAST(FLOOR(EXTRACT(EPOCH FROM \
                                     clock_timestamp()) * 1000) AS BIGINT), lease_deadline=\
                                     CAST(FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000) \
                                     AS BIGINT) + 3600000 WHERE app_id=$1 AND id=$2",
                                    &[
                                        &rival.app_id.as_str(),
                                        &rival.id.as_str(),
                                        &WorkerId::mint().as_str(),
                                    ],
                                )
                                .await
                                .unwrap();
                            assert_eq!(leased, 1, "the rival lease committed");
                        }
                        Ok(worker.clone())
                    }
                },
                |_| ready(Admission::Deliver(())),
            )
            .await
            .unwrap();
        if raced {
            assert!(batch.grants.is_empty(), "the last slot was leased twice: {batch:?}");
            assert_eq!(report.skipped, [skip(app, ClaimSkipReason::AtCap)]);
            assert_eq!(stored(&fixture, &head).await["state"], value!("ready"));
        } else {
            assert_eq!(delivered(&batch), [head], "{report:?}");
        }
    }
}

/// A grant its host turns away goes back without waiting for the app's lock,
/// so a give-back never holds the reply. With another session holding one
/// app's lock, that app's give-back is refused at once as contended and its row
/// is left to lapse; the other app's give-back, with its lock free, returns its
/// row. The claim returns while the lock is still held.
#[compio::test]
async fn postgres_a_give_back_inside_a_claim_does_not_wait_for_the_apps_lock() {
    let fixture = Fixture::new(Backend::Postgres).await;
    let zone = Zone::new(&fixture).await;
    let (cursor, apps) = zone.apps(2).await;
    let [busy, free] = <[AppId; 2]>::try_from(apps).unwrap();
    let stranded = zone.job(&busy).await;
    let returned = zone.job(&free).await;
    let worker = WorkerId::mint();
    let ask = request(2, Some(&cursor), Vec::new());
    let claim = ZoneClaim {
        worker: &worker,
        zone: &zone.id,
        request: &ask,
        deadline: ClaimDeadline::at(unbounded()),
    };
    let started = Instant::now();
    let (batch, report): (ClaimBatch<()>, ClaimReport) = zone
        .coordinator
        .claim_in_zone(
            &claim,
            zone.policies.as_ref(),
            || ready(Ok(worker.clone())),
            |grant| {
                let held = grant.delivery().job.app_id == busy;
                let (fixture, busy) = (&fixture, &busy);
                async move {
                    if held {
                        hold(fixture, &[busy]).await;
                    }
                    Admission::GiveBack {
                        reason: ClaimSkipReason::Deferred,
                        defer: zeroship_workflow_manager::GiveBack::Unsent,
                    }
                }
            },
        )
        .await
        .unwrap();
    let elapsed = started.elapsed();
    release(&fixture).await;
    assert!(batch.grants.is_empty(), "{batch:?}");
    assert_eq!(report.lapsing, [(busy.clone(), Error::Contended)]);
    assert!(
        elapsed < Options::default().transaction_timeout,
        "a give-back waited for the app's lock: {elapsed:?}"
    );
    let row = stored(&fixture, &stranded).await;
    assert_eq!(row["state"], value!("leased"));
    assert_eq!(row["worker_id"], value!(worker.as_str()));
    let row = stored(&fixture, &returned).await;
    assert_eq!(row["state"], value!("ready"));
    assert_eq!(row["worker_id"], Value::Null);
}

/// A maintenance claim waits for an app whose lock another session holds, and
/// takes the row once the holder commits: the maintenance lane, management
/// commands and closing claim one app at a time, and a busy app is only ever
/// passed over by a worker's zone claim. The holder commits once the claim is
/// observed waiting behind it.
#[compio::test]
async fn postgres_a_maintenance_claim_waits_for_a_locked_app() {
    let fixture = Fixture::new(Backend::Postgres).await;
    let Admin::Postgres(admin) = &fixture.admin else {
        unreachable!("a PostgreSQL fixture has a PostgreSQL admin")
    };
    let zone = Zone::new(&fixture).await;
    let (_, apps) = zone.apps(1).await;
    let app = &apps[0];
    let job = sweep(app);
    assert_eq!(zone.queue.submit(&job).await.unwrap(), job);
    let holder: i32 = admin
        .query("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()[0]
        .get(0);
    hold(&fixture, &[app]).await;
    let authority = zeroship_workflow_manager::maintenance::MaintenanceAuthority::new(
        app.clone(),
        WorkerId::mint(),
    );
    let waited = async {
        compio::time::timeout(Duration::from_secs(10), async {
            loop {
                let blocked: bool = admin
                    .query(
                        "SELECT EXISTS(SELECT 1 FROM pg_stat_activity \
                         WHERE $1 = ANY(pg_blocking_pids(pid)))",
                        &[&holder],
                    )
                    .await
                    .unwrap()[0]
                    .get(0);
                if blocked {
                    break;
                }
                compio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the maintenance claim waits behind the held lock");
        release(&fixture).await;
    };
    let (claimed, ()) = futures::join!(
        authority.claim(&zone.queue, Ok(support::delivery_ceiling())),
        waited
    );
    let grant = claimed
        .expect("a maintenance claim waits for the lock rather than passing the app")
        .expect("the released app's maintenance row is taken");
    assert_eq!(grant.delivery().job, job);
}

/// Every give-back a claim makes ends by the claim's give-back deadline, so
/// none can push the reply past the caller's wait. Here that deadline is the
/// attempt deadline itself, and the host's admission never answers: the
/// deadline cuts the admission, no time is left for the give-back, and the
/// row is left to lapse rather than returned late. The control is the same
/// grant turned away at once, whose give-back returns the row.
async fn give_back_past_its_deadline(fixture: &Fixture) {
    let zone = Zone::new(fixture).await;
    let (cursor, apps) = zone.apps(2).await;
    let [cut, prompt] = <[AppId; 2]>::try_from(apps).unwrap();
    let stranded = zone.job(&cut).await;
    let returned = zone.job(&prompt).await;
    let worker = WorkerId::mint();
    let ask = request(2, Some(&cursor), Vec::new());
    for (app, job, stalls) in [(&cut, &stranded, true), (&prompt, &returned, false)] {
        let claim = ZoneClaim {
            worker: &worker,
            zone: &zone.id,
            request: &ask,
            deadline: ClaimDeadline::at(Instant::now() + Duration::from_millis(500)),
        };
        let (batch, report): (ClaimBatch<()>, ClaimReport) = zone
            .coordinator
            .claim_in_zone(
                &claim,
                zone.policies.as_ref(),
                || ready(Ok(worker.clone())),
                |grant| {
                    let stall = stalls && grant.delivery().job.app_id == *app;
                    async move {
                        if stall {
                            pending::<()>().await;
                        }
                        Admission::GiveBack {
                            reason: ClaimSkipReason::Deferred,
                            defer: zeroship_workflow_manager::GiveBack::Unsent,
                        }
                    }
                },
            )
            .await
            .unwrap();
        assert!(batch.grants.is_empty(), "{batch:?}");
        let row = stored(fixture, job).await;
        if stalls {
            assert_eq!(report.lapsing, [(app.clone(), Error::Timeout)], "{report:?}");
            assert_eq!(row["state"], value!("leased"));
        } else {
            assert!(report.lapsing.is_empty(), "{report:?}");
            assert_eq!(row["state"], value!("ready"));
        }
    }
}

/// A grant whose lease its admission spent is not given back, even when the
/// admission asks for a back-off: the row stays leased to lapse, counting no
/// back-off, and once its deadline passes the next claim delivers the job at
/// its next delivery attempt with no execution attempt counted for the one that
/// lapsed. The control is a grant turned away with time left, which
/// goes back under the back-off its admission named.
///
/// The spent grant's stored deadline is moved an hour on before its admission
/// answers, as a slow clock read leaves the queue's deadline after the grant's
/// own, so a give-back would certainly find the lease live rather than only
/// when it beat the margin the anchoring leaves.
async fn spent_grant_lapses(fixture: &Fixture) {
    let zone = Zone::leasing(fixture, Duration::from_secs(2)).await;
    let (cursor, apps) = zone.apps(2).await;
    let [spent, live] = <[AppId; 2]>::try_from(apps).unwrap();
    let lapsing = zone.job(&spent).await;
    let returned = zone.job(&live).await;
    let worker = WorkerId::mint();
    let ask = request(2, Some(&cursor), Vec::new());
    let claim = ZoneClaim {
        worker: &worker,
        zone: &zone.id,
        request: &ask,
        deadline: ClaimDeadline::at(unbounded()),
    };
    let (batch, report): (ClaimBatch<()>, ClaimReport) = zone
        .coordinator
        .claim_in_zone(
            &claim,
            zone.policies.as_ref(),
            || ready(Ok(worker.clone())),
            |grant| {
                let outlived = grant.delivery().job.app_id == spent;
                let (zone, lapsing) = (&zone, &lapsing);
                async move {
                    if outlived {
                        let now = zone.queue.now().await.unwrap();
                        patch(fixture, lapsing, value!({"lease_deadline": now + 3_600_000})).await;
                        compio::time::sleep(grant.remaining().unwrap_or_default()).await;
                        assert!(grant.lease().is_err(), "the admission outlived its grant");
                    }
                    Admission::GiveBack {
                        reason: ClaimSkipReason::Refused,
                        defer: zeroship_workflow_manager::GiveBack::Backoff,
                    }
                }
            },
        )
        .await
        .unwrap();
    assert!(batch.grants.is_empty(), "{batch:?}");
    let row = stored(fixture, &lapsing).await;
    assert_eq!(row["state"], value!("leased"), "a spent grant went back: {row:?}");
    assert_eq!(report.lapsing, [(spent.clone(), Error::Timeout)], "{report:?}");
    assert_eq!(row["worker_id"], value!(worker.as_str()));
    assert_eq!(row["deferrals"], value!(0));
    assert_eq!(row["deferred_until"], Value::Null);
    assert_eq!(row["execution_attempts"], value!(0));
    let row = stored(fixture, &returned).await;
    assert_eq!(row["state"], value!("ready"));
    assert_eq!(row["worker_id"], Value::Null);
    assert_eq!(row["deferrals"], value!(1));
    assert_ne!(row["deferred_until"], Value::Null);

    let now = zone.queue.now().await.unwrap();
    patch(fixture, &lapsing, value!({"lease_deadline": now})).await;
    let (batch, report) = zone
        .claim(&worker, &request(1, Some(&cursor), Vec::new()))
        .await;
    let [(grant, ())] = <[_; 1]>::try_from(batch.grants)
        .unwrap_or_else(|grants| panic!("the lapsed job is redelivered: {grants:?} {report:?}"));
    assert_eq!(grant.delivery().job, lapsing);
    assert_eq!(grant.delivery().attempt.get(), 2);
    let row = stored(fixture, &lapsing).await;
    assert_eq!(row["execution_attempts"], value!(0), "the lapse counted an attempt");
    assert_eq!(row["deferrals"], value!(0));
}
