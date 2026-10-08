//! The zone claim over the wire, through real instance authentication, against
//! this service's own queue and journal.
//!
//! Each case works in a database of its own, declares a zone there and enrolls
//! one instance in it, so a claim pages exactly the apps the case seeded. A
//! declared zone is deployment-global, so the case never declares it on the
//! process-shared database.
#![expect(
    clippy::future_not_send,
    reason = "HTTP fixtures use the owning ntex compio runtime"
)]

use crate::support::{
    holds, journal, platform,
    policies::{self, GrantedPolicies},
    zone,
};

use ntex::{
    http::{Request, StatusCode},
    service::{Pipeline, Service},
    web::{self, test, WebResponse},
};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    num::{NonZeroU32, NonZeroU64},
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use zeroship_core::{
    app_id::AppId,
    service_assertion::{InMemoryReplayStore, ServiceAssertionVerifier, ServiceTrustBundle},
    service_identity::{endpoints, ServiceEndpoint},
    workflow_coordination::RunId,
    workflow_jobs::{ClaimJobs, ClaimedJobs, Delivery, DeploymentId, JobId, JobOperation, JobSpec},
    workflow_policy::AppPolicy,
    ZoneId,
};
use zeroship_storage::StorageBackendConfig;
use zeroship_workflow::service::delivery::{AcceptedJob, ClaimedTask, ReportedExecution};
use zeroship_workflow_manager::{policy::PolicySource, recovery::Options as RecoveryOptions};
use zeroship_workflow_server::{
    auth::{PostgresWorkerRegistry, WorkflowAuth},
    coordinator::{Coordinator, Options},
    payloads::ServicePayloads,
    runs::RunService,
    SharedState, WorkflowHttpState,
};

struct Fixture {
    platform: platform::Platform,
    state: SharedState,
    enrolled: zone::Enrolled,
    zone: ZoneId,
    policies: Rc<GrantedPolicies>,
}

impl Fixture {
    /// A case on a database of its own, under the default options.
    async fn new() -> Self {
        Self::with_options(Options::default()).await
    }

    /// A case on a database of its own, under `options`. The case declares an
    /// execution zone, which is deployment-global, so it works in a clone no
    /// sibling observes.
    async fn with_options(options: Options) -> Self {
        Box::pin(Self::compose(platform::Platform::fresh_database().await, options)).await
    }

    /// The production composition of one HTTP thread's state, under `options`.
    async fn compose(platform: platform::Platform, options: Options) -> Self {
        let (zone, signer) = zone::declare_zone(&platform).await;
        let enrolled = zone::Enrolled::join(&platform, &signer, zone.as_str()).await;
        let service = Coordinator::connect(&platform.runtime_url, options, holds::client())
            .await
            .unwrap();
        // The worker's key is read from its own instance row, so the peer
        // bundle here is unused by `WorkflowAuth::worker`.
        let replay = Arc::new(InMemoryReplayStore::default());
        let auth = Arc::new(WorkflowAuth::new(
            Arc::new(ServiceAssertionVerifier::new(
                ServiceTrustBundle::new(),
                replay.clone(),
            )),
            Arc::new(PostgresWorkerRegistry::new(Arc::new(
                platform::connect(&platform.runtime_url).await,
            ))),
            replay,
        ));
        let policies = Rc::new(GrantedPolicies::default());
        let runs = Rc::new(
            RunService::connect_over(
                &platform.runtime_url,
                &service,
                service.recovery(RecoveryOptions::default()).unwrap(),
                options.startup_timeout(),
            )
            .await
            .unwrap(),
        );
        let payloads = ServicePayloads::open(&StorageBackendConfig::Local(
            platform.work.path().join("payloads"),
        ))
        .unwrap();
        let state = Rc::new(WorkflowHttpState {
            service,
            auth,
            policy_source: Some(policies.clone() as Rc<dyn PolicySource>),
            runs,
            payloads,
        });
        Self {
            platform,
            state,
            enrolled,
            zone,
            policies,
        }
    }

    /// An app of this case's zone: its Control row, the queue scope Control's
    /// lifecycle publication creates, and an observed default policy.
    async fn app(&self) -> AppId {
        let app = AppId::mint();
        self.seed(&app).await;
        app
    }

    /// A cursor that sorts before `count` apps of this case's zone, and the
    /// apps in the order a lap from that cursor visits them.
    async fn apps(&self, count: usize) -> (AppId, Vec<AppId>) {
        let mut minted: Vec<AppId> = (0..=count).map(|_| AppId::mint()).collect();
        minted.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        let cursor = minted.remove(0);
        for app in &minted {
            self.seed(app).await;
        }
        (cursor, minted)
    }

    async fn seed(&self, app: &AppId) {
        self.platform
            .seed_app_in(app, Some(self.zone.as_str()))
            .await;
        self.platform.seed_scope(app, self.zone.as_str()).await;
        self.policies
            .grant(app, &self.zone, AppPolicy::default());
    }

    /// A runnable run in `app`'s journal and the advance job for it, submitted
    /// the way the service's own publication submits committed intent.
    async fn executable(&self, app: &AppId) -> (RunId, JobSpec) {
        let run = journal::seed_run(&self.platform, app).await;
        let job = JobSpec {
            id: JobId::mint(),
            app_id: app.clone(),
            operation: JobOperation::Advance {
                deployment_id: DeploymentId::parse_owned(app.as_str().replacen("app_", "dep_", 1))
                    .unwrap(),
                run_id: run.clone(),
                generation: 0,
                revision: 1.try_into().unwrap(),
            },
            available_at: 1.try_into().unwrap(),
        };
        let queue = self.state.service.manager.queue();
        assert_eq!(queue.submit(&job).await.unwrap(), job);
        (run, job)
    }

    async fn service(
        &self,
    ) -> Pipeline<impl Service<Request, Response = WebResponse, Error = web::Error>> {
        test::init_service(
            web::App::new()
                .state(self.state.clone())
                .configure(zeroship_workflow_server::configure),
        )
        .await
    }

    /// The queue row of `job`, as text, read from the database.
    async fn row(&self, job: &JobSpec) -> Row {
        let row = self
            .platform
            .admin
            .query_one(
                "SELECT state, worker_id, attempt, deferred_until, deferrals, lease_deadline \
                 FROM workflow_manager.jobs WHERE app_id=$1 AND id=$2",
                &[&job.app_id.as_str(), &job.id.as_str()],
            )
            .await
            .unwrap();
        Row {
            state: row.get(0),
            worker: row.get(1),
            attempt: row.get(2),
            deferred_until: row.get(3),
            deferrals: row.get(4),
            lease_deadline: row.get(5),
        }
    }

    /// The database clock the queue reads, in its own unit.
    async fn now(&self) -> i64 {
        self.platform
            .admin
            .query_one(
                "SELECT CAST(FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000) AS BIGINT)",
                &[],
            )
            .await
            .unwrap()
            .get(0)
    }

    /// Wait, on the database clock, until `instant` has passed.
    async fn until_past(&self, instant: i64) {
        compio::time::timeout(Duration::from_secs(30), async {
            while self.now().await <= instant {
                compio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the database clock passes the instant");
    }

    /// Journal tasks dispatched for `run`.
    async fn tasks(&self, run: &RunId) -> i64 {
        self.platform
            .admin
            .query_one(
                "SELECT count(*) FROM workflow_manager.__zeroship_workflow_tasks \
                 WHERE run_id=$1",
                &[&run.as_str()],
            )
            .await
            .unwrap()
            .get(0)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Row {
    state: String,
    worker: Option<String>,
    attempt: i64,
    deferred_until: Option<i64>,
    deferrals: i64,
    lease_deadline: Option<i64>,
}

const fn claim(max: u32) -> ClaimJobs {
    ClaimJobs {
        max: NonZeroU32::new(max).unwrap(),
        wait_ms: NonZeroU64::new(5_000).unwrap(),
        after: None,
        exclude: Vec::new(),
    }
}

async fn post<S>(
    app: &Pipeline<S>,
    authorization: &str,
    endpoint: ServiceEndpoint,
    body: &impl Serialize,
) -> (StatusCode, Value)
where
    S: Service<Request, Response = WebResponse, Error = web::Error>,
{
    let response = test::call_service(
        app,
        test::TestRequest::post()
            .uri(endpoint.path_template())
            .header("authorization", authorization)
            .set_json(body)
            .to_request(),
    )
    .await;
    let status = response.status();
    let body = test::read_body(response).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// One claim, decoded, with the status it answered.
async fn claimed<S>(fixture: &Fixture, app: &Pipeline<S>, max: u32) -> ClaimedJobs<AcceptedJob>
where
    S: Service<Request, Response = WebResponse, Error = web::Error>,
{
    let (status, body) = post(
        app,
        &fixture.enrolled.authorization(),
        endpoints::WORKFLOW_JOB_CLAIM,
        &claim(max),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_value(body).unwrap()
}

/// The task an executable delivery's journal acceptance handed out.
fn task(accepted: Option<&AcceptedJob>) -> ClaimedTask {
    let Some(AcceptedJob::Execute {
        assignment,
        remaining_ms,
    }) = accepted
    else {
        panic!("the journal holds this run, so its acceptance hands out a task: {accepted:?}")
    };
    ClaimedTask {
        id: assignment.id.clone(),
        token: assignment.token.clone(),
        remaining_ms: *remaining_ms,
    }
}

/// One batch, two apps, through the routes a worker drives: the claim hands out
/// one delivery per app with its journal task, each renews both halves, and
/// each settles with the outcome the journal commits.
#[ntex::test]
async fn a_batch_claim_heartbeat_and_settle_round_trip() {
    let fixture = Box::pin(Fixture::new()).await;
    let (left, right) = (fixture.app().await, fixture.app().await);
    let (_, left_job) = fixture.executable(&left).await;
    let (_, right_job) = fixture.executable(&right).await;
    let app = fixture.service().await;

    let batch = claimed(&fixture, &app, 2).await;
    assert_eq!(batch.deliveries.len(), 2, "{batch:?}");
    let mut jobs = batch
        .deliveries
        .iter()
        .map(|claimed| claimed.lease.delivery.job.id.clone())
        .collect::<Vec<_>>();
    jobs.sort();
    let mut expected = vec![left_job.id.clone(), right_job.id.clone()];
    expected.sort();
    assert_eq!(jobs, expected, "one delivery per app");

    for claimed in &batch.deliveries {
        let delivery = &claimed.lease.delivery;
        assert_eq!(delivery.worker_id, fixture.enrolled.instance);
        assert_eq!(delivery.attempt.get(), 1);
        let task = task(claimed.accepted.as_ref());
        let (status, renewed) = post(
            &app,
            &fixture.enrolled.authorization(),
            endpoints::WORKFLOW_JOB_HEARTBEAT,
            &json!({"delivery": delivery, "task": task}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{renewed}");
        let extended = &renewed["renewal"]["extended"];
        assert!(extended.is_object(), "the renewal extended the task: {renewed}");
        let remaining: NonZeroU64 = serde_json::from_value(extended["remainingMs"].clone()).unwrap();
        let (status, settled) = post(
            &app,
            &fixture.enrolled.authorization(),
            endpoints::WORKFLOW_JOB_SETTLE,
            &json!({
                "delivery": delivery,
                "execution": ReportedExecution {
                    grant_ms: Some(remaining),
                    task: ClaimedTask { remaining_ms: remaining, ..task },
                    confirmed: Vec::new(),
                    execution: zeroship_workflow::WorkflowExecution::from_runtime_value(
                        json!({"outcomes":[{"kind":"RunCompleted"}]}),
                    )
                    .unwrap(),
                },
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{settled}");
        assert_eq!(settled["outcome"], json!({"kind":"completed"}));
        assert_eq!(settled["jobId"], json!(delivery.job.id));
    }
    for job in [&left_job, &right_job] {
        assert_eq!(fixture.row(job).await.state, "settled");
    }
}

/// A journal deferral on the claim route is given back in the same request: the
/// row returns to `ready`, holds nobody, and waits exactly until the run is due.
/// The control is the accepted claim in the same batch, which keeps its row.
#[ntex::test]
async fn a_deferred_claim_gives_its_row_back_in_the_same_request() {
    let fixture = Box::pin(Fixture::new()).await;
    let (waiting, ready) = (fixture.app().await, fixture.app().await);
    let (run, deferred) = fixture.executable(&waiting).await;
    let (_, accepted) = fixture.executable(&ready).await;
    let due = fixture.now().await + 3_600_000;
    assert_eq!(
        fixture
            .platform
            .admin
            .execute(
                "UPDATE workflow_manager.__zeroship_workflow_runs SET due_at=$3 \
                 WHERE app_id=$1 AND id=$2",
                &[&waiting.as_str(), &run.as_str(), &due],
            )
            .await
            .unwrap(),
        1
    );
    let app = fixture.service().await;

    let batch = claimed(&fixture, &app, 2).await;
    let [delivered] = <[_; 1]>::try_from(batch.deliveries)
        .unwrap_or_else(|deliveries| panic!("only the due run is handed out: {deliveries:?}"));
    assert_eq!(delivered.lease.delivery.job, accepted);

    let given_back = fixture.row(&deferred).await;
    assert_eq!(
        given_back,
        Row {
            state: "ready".to_owned(),
            worker: None,
            attempt: 1,
            deferred_until: Some(due),
            deferrals: 0,
            lease_deadline: None,
        },
        "a deferred claim keeps nothing leased"
    );
    let kept = fixture.row(&accepted).await;
    assert_eq!(kept.state, "leased");
    assert_eq!(
        kept.worker.as_deref(),
        Some(fixture.enrolled.instance.as_str())
    );
    assert_eq!(kept.deferred_until, None);

    // The given-back row is not offered again before it is due.
    let again = claimed(&fixture, &app, 2).await;
    assert!(again.deliveries.is_empty(), "{again:?}");
    assert_eq!(fixture.row(&deferred).await, given_back);
}

/// A delivery its holder could not prepare is released with that reason, and
/// the row comes back with a pause that grows with every consecutive back-off.
///
/// Each pause is read against the database clock on both sides of the request,
/// so the window holds the instant the queue stamped whatever the request's own
/// latency was.
#[ntex::test]
async fn a_preparation_failure_backs_off_more_with_each_give_back() {
    let fixture = Box::pin(Fixture::new()).await;
    let owner = fixture.app().await;
    let (_, job) = fixture.executable(&owner).await;
    let app = fixture.service().await;

    for (deferrals, pause) in [(1_i64, 100_i64), (2, 200)] {
        let batch = claimed(&fixture, &app, 1).await;
        let [claimed] = <[_; 1]>::try_from(batch.deliveries)
            .unwrap_or_else(|deliveries| panic!("the job is claimable again: {deliveries:?}"));
        let delivery: &Delivery = &claimed.lease.delivery;
        assert_eq!(delivery.job, job);
        let before = fixture.now().await;
        let (status, body) = post(
            &app,
            &fixture.enrolled.authorization(),
            endpoints::WORKFLOW_JOB_RELEASE,
            &json!({
                "delivery": delivery,
                "task": task(claimed.accepted.as_ref()),
                "reason": "preparation_failed",
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let after = fixture.now().await;
        let row = fixture.row(&job).await;
        assert_eq!(row.state, "ready");
        assert_eq!(row.worker, None);
        assert_eq!(row.deferrals, deferrals);
        let until = row.deferred_until.expect("a given-back row waits");
        assert!(
            until - after <= pause && pause <= until - before,
            "give-back {deferrals} paused {} to {}ms, not {pause}ms",
            until - after,
            until - before
        );
        fixture.until_past(until).await;
    }
}

/// A claim does not wait for an app whose scope another session holds locked:
/// it passes that app and serves the next one while the lock is still held,
/// leaving the locked app's row untouched. Released, the same app is served.
#[ntex::test]
async fn a_claim_passes_an_app_whose_scope_is_locked() {
    let fixture = Box::pin(Fixture::new()).await;
    let (locked, free) = (fixture.app().await, fixture.app().await);
    let (_, held) = fixture.executable(&locked).await;
    let (_, open) = fixture.executable(&free).await;
    let app = fixture.service().await;
    let mut blocker = platform::connect(fixture.platform.admin_url.as_str()).await;
    let lock = blocker.transaction().await.unwrap();
    lock.query(
        "SELECT id FROM workflow_manager.queue_scopes WHERE id=$1 FOR UPDATE",
        &[&locked.as_str()],
    )
    .await
    .unwrap();
    let before = fixture.row(&held).await;

    let batch = compio::time::timeout(Duration::from_secs(5), claimed(&fixture, &app, 2))
        .await
        .expect("a claim waited on a locked scope");
    let [served] = <[_; 1]>::try_from(batch.deliveries)
        .unwrap_or_else(|deliveries| panic!("only the free app is served: {deliveries:?}"));
    assert_eq!(served.lease.delivery.job, open);
    assert_eq!(fixture.row(&held).await, before);

    lock.commit().await.unwrap();
    let batch = claimed(&fixture, &app, 2).await;
    let [served] = <[_; 1]>::try_from(batch.deliveries)
        .unwrap_or_else(|deliveries| panic!("the released app is served: {deliveries:?}"));
    assert_eq!(served.lease.delivery.job, held);
}

/// A claim passes a deleted app without leasing anything of it, and serves a
/// live app of the same zone in the same request.
#[ntex::test]
async fn a_claim_passes_a_deleted_app() {
    let fixture = Box::pin(Fixture::new()).await;
    let (gone, live) = (fixture.app().await, fixture.app().await);
    let (_, orphan) = fixture.executable(&gone).await;
    let (_, work) = fixture.executable(&live).await;
    fixture.policies.delete(&gone);
    let before = fixture.row(&orphan).await;
    let app = fixture.service().await;

    let batch = claimed(&fixture, &app, 2).await;
    let [served] = <[_; 1]>::try_from(batch.deliveries)
        .unwrap_or_else(|deliveries| panic!("only the live app is served: {deliveries:?}"));
    assert_eq!(served.lease.delivery.job, work);
    assert_eq!(fixture.row(&orphan).await, before);
    assert_eq!(before.state, "ready");
}

/// A grant whose lease runs out while the journal half of the claim is still
/// working is not handed to the caller, and the job is redelivered afterwards.
///
/// The journal's app state is held locked while the claim waits on it, past the
/// lease the queue granted, read off the database clock against the row's own
/// deadline. The control is the next claim, which delivers the job again with a
/// task once nothing is in its way.
#[ntex::test]
async fn a_grant_exhausted_before_the_reply_is_not_returned() {
    let lease = Duration::from_secs(2);
    let fixture = Box::pin(Fixture::with_options(Options {
        lease,
        ..Options::default()
    }))
    .await;
    let owner = fixture.app().await;
    let (run, job) = fixture.executable(&owner).await;
    let app = fixture.service().await;
    let mut blocker = platform::connect(fixture.platform.admin_url.as_str()).await;
    let pid: i32 = blocker
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let lock = blocker.transaction().await.unwrap();
    lock.batch_execute(
        "LOCK TABLE workflow_manager.__zeroship_workflow_app_state IN ACCESS EXCLUSIVE MODE",
    )
    .await
    .unwrap();

    let held = async {
        compio::time::timeout(Duration::from_secs(10), async {
            loop {
                let waiting: bool = fixture
                    .platform
                    .admin
                    .query_one(
                        "SELECT EXISTS(SELECT 1 FROM pg_stat_activity \
                         WHERE usename='zeroship_workflow' AND datname=current_database() \
                         AND $1=ANY(pg_blocking_pids(pid)))",
                        &[&pid],
                    )
                    .await
                    .unwrap()
                    .get(0);
                if waiting {
                    break;
                }
                compio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the claim's journal half must reach the locked app state");
        let leased = fixture.row(&job).await;
        assert_eq!(leased.state, "leased", "the queue half committed first");
        fixture
            .until_past(leased.lease_deadline.expect("a leased row has a deadline"))
            .await;
        lock.commit().await.unwrap();
    };
    let (batch, ()) = futures::join!(claimed(&fixture, &app, 1), held);
    assert!(
        batch.deliveries.is_empty(),
        "an exhausted grant reached the caller: {batch:?}"
    );
    assert_eq!(
        fixture.tasks(&run).await,
        0,
        "the journal accepted work under an exhausted grant"
    );

    let batch = claimed(&fixture, &app, 1).await;
    let [redelivered] = <[_; 1]>::try_from(batch.deliveries)
        .unwrap_or_else(|deliveries| panic!("the job is redelivered: {deliveries:?}"));
    assert_eq!(redelivered.lease.delivery.job, job);
    assert_eq!(redelivered.lease.delivery.attempt.get(), 2);
    task(redelivered.accepted.as_ref());
}

/// A grant the rest of its batch outlasted is neither handed to the caller nor
/// given back: the reply finds its lease spent and leaves the row leased to
/// lapse, counting no back-off, with the journal task accepted under it.
///
/// The batch's second app never has its policy answered, so its visit lasts
/// until the claim's deadline, past the first app's lease. While it waits, the
/// first row's stored deadline is moved an hour on, as a slow clock read leaves
/// the queue's deadline after the grant's own, so a give-back would certainly
/// find the lease live. The journal's task for the first run is the control
/// that its grant was admitted before the batch outlasted it.
#[ntex::test]
async fn a_grant_the_batch_outlasted_is_left_to_lapse() {
    let fixture = Box::pin(Fixture::with_options(Options {
        lease: Duration::from_secs(2),
        ..Options::default()
    }))
    .await;
    let (cursor, apps) = fixture.apps(2).await;
    let [served, stalled] = <[AppId; 2]>::try_from(apps).unwrap();
    let (run, job) = fixture.executable(&served).await;
    fixture.executable(&stalled).await;
    fixture.policies.stall(&stalled);
    let app = fixture.service().await;
    let request = ClaimJobs {
        after: Some(cursor),
        ..claim(2)
    };

    let moved = async {
        compio::time::timeout(Duration::from_secs(10), async {
            while fixture.row(&job).await.state != "leased" {
                compio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the claim leases the first app's row");
        let moved = fixture
            .platform
            .admin
            .execute(
                "UPDATE workflow_manager.jobs SET lease_deadline = lease_deadline + 3600000 \
                 WHERE app_id=$1 AND id=$2",
                &[&job.app_id.as_str(), &job.id.as_str()],
            )
            .await
            .unwrap();
        assert_eq!(moved, 1, "the leased row's deadline moved");
    };
    let authorization = fixture.enrolled.authorization();
    let ((status, body), ()) = futures::join!(
        post(
            &app,
            &authorization,
            endpoints::WORKFLOW_JOB_CLAIM,
            &request,
        ),
        moved
    );
    assert_eq!(status, StatusCode::OK, "{body}");
    let batch: ClaimedJobs<AcceptedJob> = serde_json::from_value(body).unwrap();
    assert!(batch.deliveries.is_empty(), "a spent grant reached the caller: {batch:?}");
    assert_eq!(fixture.tasks(&run).await, 1, "the grant was admitted with its task");
    let row = fixture.row(&job).await;
    assert_eq!(
        (row.state.as_str(), row.worker.as_deref(), row.deferrals, row.deferred_until),
        ("leased", Some(fixture.enrolled.instance.as_str()), 0, None),
        "a spent grant is left to lapse, not given back"
    );
}

/// A claim whose journal half finds dispatch switched off gives the row back
/// until the policy observation that switched it off lapses, because nothing
/// reads that app's policy again before then.
///
/// The switch lands between the claim's own policy check and the journal's
/// read of the same app, which is the one window in which a claim reaches the
/// journal for an app whose dispatch is off.
#[ntex::test]
async fn a_claim_finding_dispatch_off_waits_out_the_observation() {
    let fixture = Box::pin(Fixture::new()).await;
    let owner = fixture.app().await;
    let (_, job) = fixture.executable(&owner).await;
    fixture.policies.change_after_next_read(
        &owner,
        AppPolicy {
            dispatch: false,
            ..AppPolicy::default()
        },
    );
    let app = fixture.service().await;

    let batch = claimed(&fixture, &app, 1).await;
    let after = fixture.now().await;
    assert!(batch.deliveries.is_empty(), "{batch:?}");
    let row = fixture.row(&job).await;
    assert_eq!(row.state, "ready");
    assert_eq!(row.worker, None);
    let validity = i64::try_from(policies::VALIDITY.as_millis()).unwrap();
    let pause = row.deferred_until.expect("a given-back row waits") - after;
    assert!(
        validity - 60_000 <= pause && pause <= validity,
        "dispatch off paused the row {pause}ms, not the observation's remaining validity"
    );
}

/// How many attempts of `job` the queue has counted toward its delivery budget.
async fn counted(fixture: &Fixture, job: &JobSpec) -> i64 {
    fixture
        .platform
        .admin
        .query_one(
            "SELECT execution_attempts FROM workflow_manager.jobs WHERE app_id=$1 AND id=$2",
            &[&job.app_id.as_str(), &job.id.as_str()],
        )
        .await
        .unwrap()
        .get(0)
}

/// A release reporting an interrupted attempt returns the row claimable at
/// once and counts that attempt; the next claim redelivers the job with a new
/// task. The control is a release reporting a preparation failure, which counts
/// nothing and leaves its row waiting out a back-off.
#[ntex::test]
async fn an_interrupted_release_returns_the_row_at_once_with_the_attempt_counted() {
    let fixture = Box::pin(Fixture::new()).await;
    let (stopped, unprepared) = (fixture.app().await, fixture.app().await);
    let (_, interrupted) = fixture.executable(&stopped).await;
    let (_, deferred) = fixture.executable(&unprepared).await;
    let app = fixture.service().await;

    let batch = claimed(&fixture, &app, 2).await;
    assert_eq!(batch.deliveries.len(), 2, "{batch:?}");
    for claimed in &batch.deliveries {
        let reason = if claimed.lease.delivery.job == interrupted {
            "interrupted"
        } else {
            "preparation_failed"
        };
        let (status, body) = post(
            &app,
            &fixture.enrolled.authorization(),
            endpoints::WORKFLOW_JOB_RELEASE,
            &json!({
                "delivery": claimed.lease.delivery,
                "task": task(claimed.accepted.as_ref()),
                "reason": reason,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    let row = fixture.row(&interrupted).await;
    assert_eq!(
        (row.state.as_str(), row.worker, row.deferred_until, row.deferrals),
        ("ready", None, None, 0),
        "an interrupted attempt is claimable at once"
    );
    assert_eq!(counted(&fixture, &interrupted).await, 1);
    let control = fixture.row(&deferred).await;
    assert_eq!(control.state, "ready");
    assert!(control.deferred_until.is_some(), "an unprepared app waits");
    assert_eq!(counted(&fixture, &deferred).await, 0);

    let batch = claimed(&fixture, &app, 2).await;
    let again = batch
        .deliveries
        .iter()
        .find(|claimed| claimed.lease.delivery.job == interrupted)
        .unwrap_or_else(|| panic!("the interrupted job is redelivered: {batch:?}"));
    assert_eq!(again.lease.delivery.attempt.get(), 2);
    task(again.accepted.as_ref());
}

/// A grant whose journal half stalls is cut by the batch's deadline and goes
/// back in the same request under a back-off, even when storage holds that
/// give-back back for half a queue transaction's budget: it is not handed out,
/// and the journal holds no task for it.
///
/// The journal's app state is held locked for the whole claim, on a database of
/// the case's own because every app's journal work queues behind that lock.
/// The claim budget ends the journal step, well inside the journal attempt's
/// own I/O ceiling. The give-back is one queue transaction, which the fixture
/// bounds by its command timeout, so the request's wait is the shortest
/// multiple of that budget whose give-back window, as this coordinator derives
/// it, holds a whole one. Once the journal half is seen waiting - the queue
/// half committed - another session locks the queue's job table; once the
/// give-back is itself seen waiting on that lock, the session holds it for
/// half the give-back window the coordinator computed, so the give-back
/// answers well after it starts and still inside its window.
///
/// A give-back that misses its window leaves its row to lapse; that order is
/// `a_give_back_past_the_claims_give_back_deadline_leaves_its_row_to_lapse` in
/// the manager. This order also asserts that its own reply still leaves by
/// the give-back deadline the coordinator computed for it, even though the
/// give-back genuinely blocks on a real lock first. The sibling order
/// `a_give_back_never_holds_the_reply_past_the_callers_wait` covers a
/// different give-back, one refused at once because it cannot take its scope
/// lock, so it never blocks for real time.
#[ntex::test]
async fn a_grant_the_batch_deadline_cuts_from_a_stalled_journal_goes_back() {
    let options = Options {
        claim_budget: Duration::from_secs(1),
        ..Options::default()
    };
    let fixture = Box::pin(Fixture::with_options(options)).await;
    let owner = fixture.app().await;
    let (run, job) = fixture.executable(&owner).await;
    let app = fixture.service().await;
    let transaction = options.command_timeout;
    let arrival = std::time::Instant::now();
    let (request, deadline) = (1..=64)
        .map(|multiple| {
            let wait = u64::try_from((transaction * multiple).as_millis()).unwrap();
            let request = ClaimJobs {
                wait_ms: NonZeroU64::new(wait).unwrap(),
                ..claim(1)
            };
            let deadline = fixture
                .state
                .service
                .manager
                .claim_deadline(arrival, &request)
                .unwrap();
            (request, deadline)
        })
        .find(|(_, deadline)| deadline.give_backs - deadline.attempts >= transaction)
        .expect("some wait leaves a give-back a whole queue transaction");
    assert_eq!(
        deadline.attempts - arrival,
        options.claim_budget,
        "the claim budget, not the wait, ends the journal step"
    );
    let mut blocker = platform::connect(fixture.platform.admin_url.as_str()).await;
    let pid: i32 = blocker
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let lock = blocker.transaction().await.unwrap();
    lock.batch_execute(
        "LOCK TABLE workflow_manager.__zeroship_workflow_app_state IN ACCESS EXCLUSIVE MODE",
    )
    .await
    .unwrap();
    let mut rows_blocker = platform::connect(fixture.platform.admin_url.as_str()).await;
    let rows_pid: i32 = rows_blocker
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);

    let held = async {
        compio::time::timeout(Duration::from_secs(10), async {
            loop {
                let waiting: bool = fixture
                    .platform
                    .admin
                    .query_one(
                        "SELECT EXISTS(SELECT 1 FROM pg_stat_activity \
                         WHERE usename='zeroship_workflow' AND datname=current_database() \
                         AND $1=ANY(pg_blocking_pids(pid)))",
                        &[&pid],
                    )
                    .await
                    .unwrap()
                    .get(0);
                if waiting {
                    break;
                }
                compio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the claim's journal half must reach the locked app state");
        let rows = rows_blocker.transaction().await.unwrap();
        rows.batch_execute("LOCK TABLE workflow_manager.jobs IN ACCESS EXCLUSIVE MODE")
            .await
            .unwrap();
        compio::time::timeout(Duration::from_secs(10), async {
            loop {
                let waiting: bool = fixture
                    .platform
                    .admin
                    .query_one(
                        "SELECT EXISTS(SELECT 1 FROM pg_stat_activity \
                         WHERE usename='zeroship_workflow' AND datname=current_database() \
                         AND $1=ANY(pg_blocking_pids(pid)))",
                        &[&rows_pid],
                    )
                    .await
                    .unwrap()
                    .get(0);
                if waiting {
                    break;
                }
                compio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the claim's give-back must reach the locked job row");
        // The give-back is now genuinely blocked on this lock, an observed
        // signal rather than a wall-clock instant computed from the test's own
        // early `arrival`. Hold it for half the give-back window the
        // coordinator actually computed (the search above only accepted a
        // wait whose window covers a whole queue transaction), counted from
        // this observation: the other half of the window still remains for
        // the give-back's own write once released, inside the server's own
        // `deadline.give_backs`.
        compio::time::sleep((deadline.give_backs - deadline.attempts) / 2).await;
        rows.commit().await.unwrap();
    };
    let authorization = fixture.enrolled.authorization();
    // The give-back deadline below is anchored at `started`, taken right
    // before the request is sent rather than the early `arrival` the window
    // search used: the server's own arrival can only be later than this, by
    // no more than the dispatch through the test's HTTP client, so this
    // deadline cannot be later than the real one the server computes, and a
    // reply observed after it proves the give-back held the reply too long.
    let started = std::time::Instant::now();
    let reply_deadline = fixture
        .state
        .service
        .manager
        .claim_deadline(started, &request)
        .unwrap()
        .give_backs;
    let ((status, body), ()) = futures::join!(
        post(&app, &authorization, endpoints::WORKFLOW_JOB_CLAIM, &request),
        held
    );
    assert!(
        std::time::Instant::now() <= reply_deadline,
        "the reply left after the give-back deadline the coordinator computed for it"
    );
    assert_eq!(status, StatusCode::OK, "{body}");
    let batch: ClaimedJobs<AcceptedJob> = serde_json::from_value(body).unwrap();
    assert!(batch.deliveries.is_empty(), "{batch:?}");
    let row = fixture.row(&job).await;
    assert_eq!(
        (row.state.as_str(), row.worker, row.deferrals),
        ("ready", None, 1),
        "the grant went back under a back-off"
    );
    assert!(row.deferred_until.is_some(), "the back-off holds the row back");
    lock.commit().await.unwrap();
    assert_eq!(fixture.tasks(&run).await, 0, "the journal accepted nothing");
}

/// The service accepts a grant into its journal only after it has admitted the
/// verified worker's zone for that app itself. An app deleted after the claim
/// observed it, before the journal half, is given back and the journal holds
/// no task for it.
#[ntex::test]
async fn a_claim_hands_the_journal_nothing_of_an_app_deleted_mid_claim() {
    let fixture = Box::pin(Fixture::new()).await;
    let owner = fixture.app().await;
    let (run, job) = fixture.executable(&owner).await;
    fixture.policies.delete_after_next_read(&owner);
    let app = fixture.service().await;

    let batch = claimed(&fixture, &app, 1).await;
    assert!(batch.deliveries.is_empty(), "{batch:?}");
    assert_eq!(fixture.tasks(&run).await, 0, "the journal accepted a deleted app's job");
    let row = fixture.row(&job).await;
    assert_eq!(
        (row.state.as_str(), row.worker, row.attempt, row.deferrals),
        ("ready", None, 1, 1),
        "the grant went back"
    );
}

/// The exclusion bound is the protocol constant on the wire too: a claim at it
/// is served, one more entry is refused as an invalid request.
#[ntex::test]
async fn a_claim_above_the_exclusion_bound_is_refused() {
    let fixture = Box::pin(Fixture::new()).await;
    let owner = fixture.app().await;
    let (_, job) = fixture.executable(&owner).await;
    let app = fixture.service().await;
    let mut exclude: Vec<AppId> = (0..=ClaimJobs::MAX_EXCLUDE).map(|_| AppId::mint()).collect();

    let (status, body) = post(
        &app,
        &fixture.enrolled.authorization(),
        endpoints::WORKFLOW_JOB_CLAIM,
        &ClaimJobs {
            exclude: exclude.clone(),
            ..claim(1)
        },
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(fixture.row(&job).await.state, "ready");

    exclude.pop();
    let (status, body) = post(
        &app,
        &fixture.enrolled.authorization(),
        endpoints::WORKFLOW_JOB_CLAIM,
        &ClaimJobs { exclude, ..claim(1) },
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let batch: ClaimedJobs<AcceptedJob> = serde_json::from_value(body).unwrap();
    let [served] = <[_; 1]>::try_from(batch.deliveries)
        .unwrap_or_else(|deliveries| panic!("a claim at the bound is served: {deliveries:?}"));
    assert_eq!(served.lease.delivery.job, job);
}

/// A delivery the worker received and began nothing of - its journal task
/// reached it already spent - is given back as `unsent`: the row is claimable
/// at once, counts no attempt, and its back-off is unchanged. The control is
/// the release of an app the worker could not prepare, which waits out a
/// back-off.
#[ntex::test]
async fn an_unsent_release_returns_the_row_at_once_and_counts_nothing() {
    let fixture = Box::pin(Fixture::new()).await;
    let (spent_app, unprepared) = (fixture.app().await, fixture.app().await);
    let (_, spent) = fixture.executable(&spent_app).await;
    let (_, deferred) = fixture.executable(&unprepared).await;
    let app = fixture.service().await;

    let batch = claimed(&fixture, &app, 2).await;
    assert_eq!(batch.deliveries.len(), 2, "{batch:?}");
    for claimed in &batch.deliveries {
        let reason = if claimed.lease.delivery.job == spent {
            "unsent"
        } else {
            "preparation_failed"
        };
        let (status, body) = post(
            &app,
            &fixture.enrolled.authorization(),
            endpoints::WORKFLOW_JOB_RELEASE,
            &json!({"delivery": claimed.lease.delivery, "task": null, "reason": reason}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    let row = fixture.row(&spent).await;
    assert_eq!(
        (row.state.as_str(), row.worker, row.deferred_until, row.deferrals),
        ("ready", None, None, 0),
        "a spent delivery is claimable at once"
    );
    assert_eq!(counted(&fixture, &spent).await, 0);
    let control = fixture.row(&deferred).await;
    assert_eq!(control.state, "ready");
    assert!(control.deferred_until.is_some(), "an unprepared app waits");
    assert_eq!(control.deferrals, 1);
}

/// The most deliveries a claim may ask for is the protocol constant on the
/// wire too: one above it is refused as an invalid request before anything is
/// claimed, and a claim at it is served.
#[ntex::test]
async fn a_claim_above_the_delivery_bound_is_refused() {
    let fixture = Box::pin(Fixture::new()).await;
    let owner = fixture.app().await;
    let (_, job) = fixture.executable(&owner).await;
    let app = fixture.service().await;

    let (status, body) = post(
        &app,
        &fixture.enrolled.authorization(),
        endpoints::WORKFLOW_JOB_CLAIM,
        &claim(ClaimJobs::MAX_DELIVERIES + 1),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(fixture.row(&job).await.state, "ready");

    let batch = claimed(&fixture, &app, ClaimJobs::MAX_DELIVERIES).await;
    let [served] = <[_; 1]>::try_from(batch.deliveries)
        .unwrap_or_else(|deliveries| panic!("a claim at the bound is served: {deliveries:?}"));
    assert_eq!(served.lease.delivery.job, job);
}

/// A grant whose journal half the batch deadline cut goes back without waiting
/// for its app's lock, so no give-back holds the reply past the caller's wait.
///
/// The journal's app state is held locked so the claim's journal half stalls,
/// and once it is seen waiting - its queue half committed - another session
/// takes the app's queue lock too. The give-back is refused at once and the
/// row is left leased to lapse, which redelivers it; the journal holds no task.
/// On a database of the case's own because every app's journal work queues
/// behind the app-state lock.
#[ntex::test]
async fn a_give_back_never_holds_the_reply_past_the_callers_wait() {
    let fixture = Box::pin(Fixture::new()).await;
    let owner = fixture.app().await;
    let (run, job) = fixture.executable(&owner).await;
    let app = fixture.service().await;
    let mut journal_blocker = platform::connect(fixture.platform.admin_url.as_str()).await;
    let pid: i32 = journal_blocker
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let journal_lock = journal_blocker.transaction().await.unwrap();
    journal_lock
        .batch_execute(
            "LOCK TABLE workflow_manager.__zeroship_workflow_app_state IN ACCESS EXCLUSIVE MODE",
        )
        .await
        .unwrap();
    let mut scope_blocker = platform::connect(fixture.platform.admin_url.as_str()).await;

    let wait = Duration::from_secs(2);
    let request = ClaimJobs {
        wait_ms: NonZeroU64::new(u64::try_from(wait.as_millis()).unwrap()).unwrap(),
        ..claim(1)
    };
    let held = async {
        compio::time::timeout(Duration::from_secs(10), async {
            loop {
                let waiting: bool = fixture
                    .platform
                    .admin
                    .query_one(
                        "SELECT EXISTS(SELECT 1 FROM pg_stat_activity \
                         WHERE usename='zeroship_workflow' AND datname=current_database() \
                         AND $1=ANY(pg_blocking_pids(pid)))",
                        &[&pid],
                    )
                    .await
                    .unwrap()
                    .get(0);
                if waiting {
                    break;
                }
                compio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the claim's journal half must reach the locked app state");
        let scope_lock = scope_blocker.transaction().await.unwrap();
        let locked = scope_lock
            .query(
                "SELECT id FROM workflow_manager.queue_scopes WHERE id=$1 FOR UPDATE",
                &[&owner.as_str()],
            )
            .await
            .unwrap();
        assert_eq!(locked.len(), 1, "the blocker holds the app's queue lock");
        scope_lock
    };
    let authorization = fixture.enrolled.authorization();
    let started = std::time::Instant::now();
    let ((status, body), scope_lock) = futures::join!(
        post(
            &app,
            &authorization,
            endpoints::WORKFLOW_JOB_CLAIM,
            &request,
        ),
        held
    );
    let elapsed = started.elapsed();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(elapsed < wait, "the reply took {elapsed:?} of a {wait:?} wait");
    let batch: ClaimedJobs<AcceptedJob> = serde_json::from_value(body).unwrap();
    assert!(batch.deliveries.is_empty(), "{batch:?}");
    let row = fixture.row(&job).await;
    assert_eq!(
        (row.state.as_str(), row.worker.as_deref()),
        ("leased", Some(fixture.enrolled.instance.as_str())),
        "the give-back could not take the lock, so the row waits for its lease to lapse"
    );
    scope_lock.commit().await.unwrap();
    journal_lock.commit().await.unwrap();
    assert_eq!(fixture.tasks(&run).await, 0, "the journal accepted nothing");
}
