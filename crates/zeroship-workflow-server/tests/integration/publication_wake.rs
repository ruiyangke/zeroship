//! A committed publication intent wakes this service's own drain, so a start,
//! a signal and a settle publish without waiting for the manager's
//! reconciliation.
//!
//! The manager's reconciliation still runs and still recovers a missed intent.
//! These cases run no driver, so nothing but the wake can publish in them: a
//! successor that becomes claimable within the bound is the wake's own, and its
//! absence before the wake existed is the regression.
#![expect(
    clippy::future_not_send,
    reason = "journal, queue and wake fixtures stay on their compio runtime"
)]

use crate::integration::http_runs::Fixture;

use std::{rc::Rc, time::Duration};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{RequestId, Revision, RunId, SignalOptions, WorkerId},
    workflow_jobs::{DeploymentId, JobId, JobOperation, JobSpec},
};
use zeroship_workflow::{
    operations::StartOptions,
    service::{
        delivery::{DeliveredTask, JobAcceptance},
        maintenance::MaintenanceOptions,
        AppWorkflows,
    },
    WorkflowExecution,
};
use zeroship_workflow_manager::{recovery::Options as RecoveryOptions, DeliveryGrant};
use zeroship_workflow_server::{runs::RunService, sweeps::MaintenanceLane};

/// A bound comfortably below the reconciliation cadence, so a pass that only
/// reconciliation could produce fails here rather than passes late.
const WAKE_BOUND: Duration = Duration::from_secs(10);

/// A start commits an Advance intent, and the wake publishes it before any
/// reconciliation could.
#[compio::test]
async fn a_start_publishes_its_advance_before_reconciliation() {
    let fixture = Box::pin(Fixture::new()).await;
    let _seeded = fixture.seed_run().await;
    fixture.ensure_recovery().await;
    let api = fixture.api().await;

    let started = api
        .start(&RequestId::mint(), "demo", StartOptions::default())
        .await
        .unwrap();
    let run = RunId::parse(&started.id).unwrap();

    let published = until("publish the started run's advance", async || {
        job_rows(&fixture, &run).await.first().cloned()
    })
    .await;
    assert_eq!(published.1, "ready", "{published:?}");
    assert_eq!(
        published.2.as_deref(),
        Some("advance"),
        "the wake must publish the run's own advance: {published:?}"
    );
}

/// A signal that wakes a waiting run commits an Advance intent, and the wake
/// publishes it before any reconciliation could.
#[compio::test]
async fn a_signal_publishes_its_advance_before_reconciliation() {
    let fixture = Box::pin(Fixture::new()).await;
    let run = fixture.seed_run().await;
    fixture.ensure_recovery().await;
    let api = fixture.api().await;
    seed_wait(&fixture, &run, "approved").await;

    api.signal(
        &RequestId::mint(),
        run.as_str(),
        SignalOptions {
            signal_type: "approved".to_owned(),
            payload: serde_json::json!({}),
        },
    )
    .await
    .unwrap();

    let published = until("publish the signalled run's advance", async || {
        job_rows(&fixture, &run).await.first().cloned()
    })
    .await;
    assert_eq!(published.1, "ready", "{published:?}");
    assert_eq!(
        published.2.as_deref(),
        Some("advance"),
        "the wake must publish the woken run's advance: {published:?}"
    );
}

/// A signal of a type the run is NOT waiting on wakes nothing and commits no
/// advance, which is the control for the case above.
#[compio::test]
async fn a_signal_the_run_is_not_waiting_on_publishes_nothing() {
    let fixture = Box::pin(Fixture::new()).await;
    let run = fixture.seed_run().await;
    fixture.ensure_recovery().await;
    let api = fixture.api().await;
    seed_wait(&fixture, &run, "approved").await;
    let wake = fixture
        .state
        .runs
        .publication_wake(&fixture.app)
        .expect("binding the app composes its wake");
    let before = wake.passes();

    api.signal(
        &RequestId::mint(),
        run.as_str(),
        SignalOptions {
            signal_type: "declined".to_owned(),
            payload: serde_json::json!({}),
        },
    )
    .await
    .unwrap();

    // Wait for the wake's own pass rather than a fixed window: once it has run,
    // an advance a late reconcile would have produced is not what this checks.
    until("run the unmatched signal's pass", async || {
        (wake.passes() > before).then_some(())
    })
    .await;
    assert!(
        job_rows(&fixture, &run).await.is_empty(),
        "an unmatched signal must not publish a runnable advance"
    );
}

/// A settle commits its successor intent, and the wake publishes it at once,
/// including a sleeping run's future wake time.
///
/// The run is driven through the manager's own claim and the journal's own
/// completion - the same two calls the wire settle serves - and the successor
/// is the timer the sleep owes. Its `available_at` near the requested wake shows
/// the queue holds the wake before it is due, rather than the run resuming at
/// the reconciliation cadence.
#[compio::test]
async fn a_settle_publishes_its_successor_before_reconciliation() {
    let fixture = Box::pin(Fixture::new()).await;
    let run = fixture.seed_run().await;
    fixture.ensure_recovery().await;
    fixture.set_frontier(&run, 1).await;
    let initial = seed_advance(&fixture, &run, 1).await;
    let api = fixture.api().await;

    let grant = fixture.claim().await;
    // The acceptance runs on a handle with no wake, so the only signal that can
    // publish the successor is the completion's own.
    let accepting = fixture.plain_api().await;
    let JobAcceptance::Execute(task) = accepting.accept_job(&grant).await.unwrap() else {
        panic!("a placed worker must be handed the runnable advance's task");
    };
    let execution = WorkflowExecution::from_runtime_value(serde_json::json!({"outcomes": [{
        "kind": "Sleep", "ordinal": 0, "name": "delay", "nameOccurrence": 0, "wakeAt": "2s"
    }]}))
    .unwrap();
    api.complete_reported_job(&task, &grant, execution, &[])
        .await
        .unwrap();

    let successor = until("publish the settled run's successor", async || {
        job_rows(&fixture, &run)
            .await
            .into_iter()
            .find(|row| row.0 != initial.as_str())
    })
    .await;
    assert_eq!(successor.1, "ready", "{successor:?}");
    assert_eq!(successor.2.as_deref(), Some("advance"), "{successor:?}");

    // The successor is the sleep's timer, still ahead of the wall clock rather
    // than a due-now resume produced at the reconciliation cadence.
    let due = successor
        .3
        .expect("a sleeping run's advance carries its wake time");
    let now = fixture.now_millis().await;
    assert!(
        due > now,
        "the successor must be scheduled for the sleep's due time, not now: {successor:?}"
    );
    assert!(
        due <= now + 10_000,
        "the successor's wake must be near the requested two seconds: {successor:?}"
    );
}

/// One wake drains every pending intent this app holds, not only the one the
/// committing call added.
#[compio::test]
async fn a_single_wake_publishes_every_pending_intent() {
    const INTENTS: usize = 4;
    let fixture = Box::pin(Fixture::new()).await;
    let _seeded = fixture.seed_run().await;
    fixture.ensure_recovery().await;

    // A journal handle over the same app with no queue: its starts commit
    // intents nothing drains.
    let plain_api = fixture.plain_api().await;
    for _ in 0..INTENTS {
        plain_api
            .start(&RequestId::mint(), "demo", StartOptions::default())
            .await
            .unwrap();
    }
    assert_eq!(
        pending_count(&fixture, &fixture.app).await,
        i64::try_from(INTENTS).unwrap(),
        "the plain handle must have committed every intent without draining"
    );

    // The waking handle binds the app and commits one more intent; the wake it
    // now carries drains the whole backlog, not only its own commit.
    let waking = fixture.api().await;
    waking
        .start(&RequestId::mint(), "demo", StartOptions::default())
        .await
        .unwrap();
    until("publish the whole backlog", async || {
        (pending_count(&fixture, &fixture.app).await == 0).then_some(())
    })
    .await;
}

/// A journal binding whose policy source refuses every app, the shape a
/// deleted app's observation takes.
#[derive(Debug)]
struct Refusing;
impl zeroship_workflow_manager::policy::PolicySource for Refusing {
    fn observe<'a>(
        &'a self,
        _: &'a AppId,
    ) -> futures::future::LocalBoxFuture<
        'a,
        Result<zeroship_workflow_manager::policy::PolicyObservation, zeroship_workflow_manager::Error>,
    > {
        Box::pin(async move { Err(zeroship_workflow_manager::Error::Denied) })
    }
    fn revalidate(
        &self,
        _: &zeroship_workflow_manager::policy::PolicyObservation,
    ) -> Result<std::time::Instant, zeroship_workflow_manager::Error> {
        Err(zeroship_workflow_manager::Error::Denied)
    }
}

/// An app the policy source refuses loses its wake, so a deleted app leaves no
/// per-thread map entry and no drain task behind.
#[compio::test]
async fn a_refused_observation_evicts_the_apps_wake() {
    let fixture = Box::pin(Fixture::new()).await;
    let _binding = fixture.api().await;
    assert!(
        fixture.state.runs.publication_wake(&fixture.app).is_some(),
        "binding the app composes its wake"
    );
    assert!(fixture
        .state
        .runs
        .app(&Refusing, &fixture.app)
        .await
        .is_err());
    assert!(
        fixture.state.runs.publication_wake(&fixture.app).is_none(),
        "a refused observation evicts the deleted app's wake"
    );
}

/// A child's completion wakes its waiting parent through the maintenance lane,
/// and the lane's own wake publishes the parent's advance before reconciliation
/// could.
///
/// No driver and no reconciliation run here, so the parent's resumed advance
/// can only reach the queue through the lane's publication wake. The lane is
/// built over the service's own queue-bound journal, which is what a host that
/// forgot to bind the queue would not compose.
#[compio::test]
async fn a_completed_child_resumes_its_waiting_parent_before_reconciliation() {
    let fixture = Box::pin(Fixture::new()).await;
    let _seeded = fixture.seed_run().await;
    fixture.ensure_recovery().await;
    let api = fixture.api().await;

    let started = api
        .start(&RequestId::mint(), "demo", StartOptions::default())
        .await
        .unwrap();
    let parent = RunId::parse(&started.id).unwrap();
    until("publish the parent's advance", async || {
        (advance_count(&fixture, &parent).await >= 1).then_some(())
    })
    .await;
    let (task, grant) = claim_and_accept(&fixture).await;
    let child_outcome = WorkflowExecution::from_runtime_value(serde_json::json!({
        "outcomes": [{
            "kind": "Child", "ordinal": 0, "name": "child",
            "childWorkflowName": "demo", "options": {}
        }]
    }))
    .unwrap();
    api.complete_reported_job(&task, &grant, child_outcome, &[])
        .await
        .unwrap();

    let child = until("create the child run", async || {
        child_run(&fixture, &parent).await
    })
    .await;
    until("publish the child's advance", async || {
        (advance_count(&fixture, &child).await >= 1).then_some(())
    })
    .await;
    let (task, grant) = claim_and_accept(&fixture).await;
    let completed = WorkflowExecution::from_runtime_value(
        serde_json::json!({"outcomes": [{"kind": "RunCompleted"}]}),
    )
    .unwrap();
    api.complete_reported_job(&task, &grant, completed, &[])
        .await
        .unwrap();

    let before = advance_count(&fixture, &parent).await;
    let lane = maintenance_lane(&fixture);
    until("resume the waiting parent", async || {
        let _ = lane.sweep(&fixture.app).await;
        (advance_count(&fixture, &parent).await > before).then_some(())
    })
    .await;
    assert!(
        child.as_str() != parent.as_str(),
        "the child is a run of its own"
    );
}

impl Fixture {
    /// The `AppWorkflows` handle this service binds for the app, wake attached.
    async fn api(&self) -> AppWorkflows {
        self.state
            .runs
            .app(self.source(), &self.app)
            .await
            .unwrap()
    }

    /// The app's journal on a handle with no publication wake, so an intent it
    /// commits stays pending until a waking handle drains it.
    async fn plain_api(&self) -> AppWorkflows {
        let plain = RunService::connect(
            &self.platform.runtime_url,
            self.state
                .service
                .recovery(RecoveryOptions::default())
                .unwrap(),
        )
        .await
        .unwrap();
        plain.app(self.source(), &self.app).await.unwrap()
    }

    /// The policy source the fixture observes the app under.
    fn source(&self) -> &dyn zeroship_workflow_manager::policy::PolicySource {
        self.state
            .policy_source
            .as_ref()
            .expect("the fixture observes a policy")
            .as_ref()
    }

    /// Set the run's frontier revision, which the journal would have advanced
    /// when it recorded the run's own advance.
    async fn set_frontier(&self, run: &RunId, revision: i64) {
        let updated = self
            .platform
            .admin
            .execute(
                "UPDATE workflow_manager.__zeroship_workflow_runs SET frontier_revision=$3 \
                 WHERE app_id=$1 AND id=$2",
                &[&self.app.as_str(), &run.as_str(), &revision],
            )
            .await
            .unwrap();
        assert_eq!(updated, 1, "no seeded run to set a frontier on");
    }

    /// Claim the app's next placed creator job, the manager half the wire
    /// claim serves.
    async fn claim(&self) -> zeroship_workflow_manager::DeliveryGrant {
        let worker = self.worker.clone();
        self.state
            .service
            .manager
            .claim_job(&self.worker, &self.scope, Ok(i64::MAX), || {
                let worker = worker.clone();
                async move { Ok(worker) }
            })
            .await
            .unwrap()
            .expect("a placed worker claims the app's ready creator job")
    }

    async fn now_millis(&self) -> i64 {
        self.platform
            .admin
            .query_one("SELECT (extract(epoch from now())*1000)::bigint", &[])
            .await
            .unwrap()
            .get(0)
    }
}

/// Claim the app's next placed creator job and accept it, returning the task
/// and the grant that authorizes completing it.
async fn claim_and_accept(fixture: &Fixture) -> (Box<DeliveredTask>, DeliveryGrant) {
    let grant = fixture.claim().await;
    let accepting = fixture.plain_api().await;
    let JobAcceptance::Execute(task) = accepting.accept_job(&grant).await.unwrap() else {
        panic!("a placed worker must be handed the runnable advance's task");
    };
    (task, grant)
}

/// The child run `parent` created, once the journal has it.
async fn child_run(fixture: &Fixture, parent: &RunId) -> Option<RunId> {
    fixture
        .platform
        .admin
        .query_opt(
            "SELECT id FROM workflow_manager.__zeroship_workflow_runs \
             WHERE app_id=$1 AND parent_id=$2 ORDER BY id LIMIT 1",
            &[&fixture.app.as_str(), &parent.as_str()],
        )
        .await
        .unwrap()
        .map(|row| RunId::parse(&row.get::<_, String>(0)).unwrap())
}

/// The queue rows naming `run`'s advance, settled or ready.
async fn advance_count(fixture: &Fixture, run: &RunId) -> usize {
    job_rows(fixture, run)
        .await
        .into_iter()
        .filter(|row| row.2.as_deref() == Some("advance"))
        .count()
}

/// The service's maintenance lane over its own queue-bound journal, the way the
/// process composes it: same queue, same journal, same policy source.
fn maintenance_lane(fixture: &Fixture) -> MaintenanceLane {
    MaintenanceLane::new(
        fixture.state.service.manager.queue().clone(),
        Rc::clone(&fixture.state.runs),
        fixture
            .state
            .policy_source
            .clone()
            .expect("the fixture observes a policy"),
        WorkerId::mint(),
        fixture.state.payloads.clone(),
        MaintenanceOptions::default(),
    )
    .unwrap()
}

/// Put a matching wait on the run, so a signal of `signal_type` wakes it.
///
/// The step row comes first because the wait's foreign key references it: a
/// wait a step does not own is not a journal state the engine can write either.
async fn seed_wait(fixture: &Fixture, run: &RunId, signal_type: &str) {
    let updated = fixture
        .platform
        .admin
        .execute(
            "UPDATE workflow_manager.__zeroship_workflow_runs SET state='waiting' \
             WHERE app_id=$1 AND id=$2",
            &[&fixture.app.as_str(), &run.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(updated, 1, "no seeded run to park on a wait");
    fixture
        .platform
        .admin
        .execute(
            "INSERT INTO workflow_manager.__zeroship_workflow_steps\
             (id,app_id,run_id,generation,ordinal,name,occurrence,origin_generation,kind,state,record) \
             VALUES($1,$2,$3,0,0,'approval',0,0,'signal','waiting','{}')",
            &[
                &zeroship_core::typed_id::generate("wjr"),
                &fixture.app.as_str(),
                &run.as_str(),
            ],
        )
        .await
        .unwrap();
    fixture
        .platform
        .admin
        .execute(
            "INSERT INTO workflow_manager.__zeroship_workflow_waits\
             (id,app_id,run_id,generation,ordinal,kind,signal_type) \
             VALUES($1,$2,$3,0,0,'signal',$4)",
            &[
                &zeroship_core::typed_id::generate("wjw"),
                &fixture.app.as_str(),
                &run.as_str(),
                &signal_type,
            ],
        )
        .await
        .unwrap();
}

/// Submit the run's own advance the way the journal would have, so a settle can
/// be driven without depending on the start wake.
async fn seed_advance(fixture: &Fixture, run: &RunId, revision: i64) -> JobId {
    let deploy: String = fixture
        .platform
        .admin
        .query_one(
            "SELECT id FROM workflow_manager.__zeroship_workflow_deploys \
             WHERE app_id=$1 AND active=1",
            &[&fixture.app.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    let job = JobSpec {
        id: JobId::mint(),
        app_id: fixture.app.clone(),
        operation: JobOperation::Advance {
            deployment_id: DeploymentId::parse(&deploy).unwrap(),
            run_id: run.clone(),
            generation: 0,
            revision: Revision::try_from(revision).unwrap(),
        },
        available_at: 0.try_into().unwrap(),
    };
    fixture
        .state
        .service
        .manager
        .queue()
        .submit(&job)
        .await
        .unwrap();
    job.id
}

/// Every queue row for `run`, oldest first, as (id, state, operation kind,
/// `available_at`).
async fn job_rows(fixture: &Fixture, run: &RunId) -> Vec<(String, String, Option<String>, Option<i64>)> {
    fixture
        .platform
        .admin
        .query(
            "SELECT id,state,operation_kind,available_at FROM workflow_manager.jobs \
             WHERE app_id=$1 AND run_id=$2 ORDER BY id",
            &[&fixture.app.as_str(), &run.as_str()],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3)))
        .collect()
}

/// Unconfirmed publication intents this app holds.
async fn pending_count(fixture: &Fixture, app: &AppId) -> i64 {
    fixture
        .platform
        .admin
        .query_one(
            "SELECT count(*) FROM workflow_manager.__zeroship_workflow_job_publications \
             WHERE app_id=$1 AND confirmed_at IS NULL",
            &[&app.as_str()],
        )
        .await
        .unwrap()
        .get(0)
}

/// Wait for `probe` to answer, or fail the case. The observable outcome is the
/// queue row, not a fixed window.
async fn until<T>(description: &str, mut probe: impl std::ops::AsyncFnMut() -> Option<T>) -> T {
    compio::time::timeout(WAKE_BOUND, async {
        loop {
            if let Some(answer) = probe().await {
                return answer;
            }
            compio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the publication wake did not {description}"))
}
