#![expect(clippy::future_not_send, reason = "native delivery tests use compio")]

use super::{
    publication::{Manager, Publisher},
    *,
};
use crate::service::{
    delivery::{
        attempt_budget, creator_deadline, CapturedLease, DeliveredTask, JobAcceptance,
        ATTEMPT_IO_CEILING,
    },
    AppWorkflows, WorkerIdentity,
};
use std::time::{Duration, Instant};
use zeroship_core::{
    workflow_coordination::{Assignment, WorkerId},
    workflow_jobs::{Delivery, JobLease, JobOperation, JobOutcome, JobSpec},
};
use zeroship_workflow_manager::Options;

#[compio::test]
async fn postgres_delivery_receipt_failure_rolls_back_checkpoint() {
    let fixture = PostgresFixture::start().await;
    let admin = connect(&fixture.admin_url).await;
    let (service, app, _, _deployments) = registered_service(Rc::new(fixture.store.clone())).await;
    let scope = service.fixture_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let manager = Manager::new(&app).await;
    let owner = assignment(&app);
    let job = publish(&scope, &manager).await;
    let grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    let claimed = task(scope.accept_job(&grant).await.unwrap());
    admin.batch_execute("CREATE FUNCTION customer.fail_receipt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected receipt failure'; END $$; CREATE TRIGGER fail_receipt BEFORE UPDATE ON customer.__zeroship_workflow_job_receipts FOR EACH ROW EXECUTE FUNCTION customer.fail_receipt();").await.unwrap();
    let checkpoint = execution(
        json!([{"kind":"Sleep", "ordinal":0, "name":"delay", "nameOccurrence":0, "wakeAt":chrono::Utc::now()+chrono::Duration::hours(1)}]),
    );
    assert!(scope
        .complete_job(&claimed, &grant, checkpoint.clone())
        .await
        .is_err());
    admin.batch_execute("DROP TRIGGER fail_receipt ON customer.__zeroship_workflow_job_receipts; DROP FUNCTION customer.fail_receipt();").await.unwrap();
    assert!(scope.job_receipt(&job).await.unwrap().is_none());
    assert!(scope.pending_jobs(None, 10).await.unwrap().is_empty());
    let tx = service.begin().await.unwrap();
    for table in ["steps", "waits"] {
        assert_eq!(
            journal_count(&tx, table, json!({"app_id":app.as_str()})).await,
            0
        );
    }
    let tasks = journal_rows(&tx, "tasks", json!({"id":claimed.assignment().id})).await;
    assert_eq!(tasks[0].text("state").unwrap(), "leased");
    tx.commit().await.unwrap();
    assert_eq!(
        scope
            .complete_job(&claimed, &grant, checkpoint)
            .await
            .unwrap()
            .outcome,
        JobOutcome::Waiting {}
    );
    assert_eq!(scope.pending_jobs(None, 10).await.unwrap().len(), 1);
}

macro_rules! case {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[compio::test]
        async fn $sqlite() {
            let dir = tempfile::tempdir().unwrap();
            $contract(Rc::new(
                sqlite_store(&dir.path().join("zs-workflow.sqlite")).await,
            ))
            .await;
        }
        #[compio::test]
        async fn $postgres() {
            let fixture = PostgresFixture::start().await;
            $contract(Rc::new(fixture.store.clone())).await;
        }
    };
}

case!(
    sqlite_delivery_receipts_survive_history,
    postgres_delivery_receipts_survive_history,
    receipts
);
case!(
    sqlite_delivery_fences_attempts_and_legacy_tasks,
    postgres_delivery_fences_attempts_and_legacy_tasks,
    attempts
);
case!(
    sqlite_delivery_checkpoint_publishes_successor,
    postgres_delivery_checkpoint_publishes_successor,
    checkpoint
);
case!(
    sqlite_delivery_creator_policy_bounds_execution,
    postgres_delivery_creator_policy_bounds_execution,
    policy_bounds
);

case!(
    sqlite_delivery_caps_a_task_at_its_authority_window,
    postgres_delivery_caps_a_task_at_its_authority_window,
    authority_window
);

case!(
    sqlite_delivery_caps_a_creator_deadline_at_its_authority,
    postgres_delivery_caps_a_creator_deadline_at_its_authority,
    authority_caps_creator_deadline
);

case!(
    sqlite_delivery_rejects_competing_frontiers,
    postgres_delivery_rejects_competing_frontiers,
    competing
);

case!(
    sqlite_delivery_stalls_a_run_whose_dispatches_never_report,
    postgres_delivery_stalls_a_run_whose_dispatches_never_report,
    stalling
);

case!(
    sqlite_delivery_keeps_a_run_that_supersedes_its_frontier,
    postgres_delivery_keeps_a_run_that_supersedes_its_frontier,
    progress_clears_strikes
);

/// A dispatch that never reports leaves the frontier exactly where it was, so
/// the next delivery hands out the same work again. Counting those reclaimed
/// dispatches is what lets such a run reach a resting state at all, and only a
/// resting run settles its delivery job and gives the deployment back.
///
/// The two halves are asserted against each other: the same `release_deployment`
/// call that the hold refuses while the run is live must succeed once the stall
/// has settled the job, so an assertion cannot pass by never pinning anything.
async fn stalling(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    service
        .policies
        .fixture_install(
            &app,
            leased_policy(
                2,
                AppPolicy {
                    max_stuck_dispatches: 2,
                    ..Default::default()
                },
            ),
        )
        .unwrap();
    let manager = Manager::new(&app).await;
    let owner = assignment(&app);
    let job = publish(&scope, &manager).await;
    let deployment = job
        .deployment_id()
        .expect("an advance job names its deployment")
        .clone();

    let mut grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    for _ in 0..2 {
        let claimed = task(scope.accept_job(&grant).await.unwrap());
        assert_eq!(run_state(&service, &app, &run.id).await, "running");
        assert_eq!(
            manager.queue.release_deployment(&app, &deployment).await,
            Err(zeroship_workflow_manager::Error::Conflict),
            "an unsettled delivery job must keep its deployment"
        );
        scope.release_job(&claimed, &grant).await.unwrap();
        lapse_lease(&manager, &grant);
        grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    }

    let JobAcceptance::Settled(receipt) = scope.accept_job(&grant).await.unwrap() else {
        panic!("a spent strike budget must settle the delivery instead of dispatching again");
    };
    assert_eq!(
        receipt.outcome,
        JobOutcome::Completed {},
        "a stalled run is at rest, so its job is done rather than waiting"
    );
    assert_eq!(run_state(&service, &app, &run.id).await, "stalled");

    let tx = service.begin().await.unwrap();
    let runs = journal_rows(&tx, "runs", json!({"app_id":app.as_str(), "id":run.id.clone()})).await;
    assert!(
        runs[0].optional_integer("terminal_at").unwrap().is_some(),
        "a stalled run must record when it came to rest"
    );
    let generations = journal_rows(
        &tx,
        "generations",
        json!({"app_id":app.as_str(), "run_id":run.id.clone()}),
    )
    .await;
    let error: serde_json::Value =
        serde_json::from_str(&generations[0].text("error").unwrap()).unwrap();
    assert_eq!(error["type"], "StalledError");
    assert_eq!(error["stuckDispatches"], 2);
    assert_eq!(error["maxStuckDispatches"], 2);
    tx.commit().await.unwrap();

    manager
        .queue
        .settle(&owner, &receipt.settlement(&grant).unwrap())
        .await
        .unwrap();
    manager
        .queue
        .release_deployment(&app, &deployment)
        .await
        .unwrap();
}

/// Strikes belong to the frontier they were counted against, not to the run.
///
/// The run here accumulates as many reclaimed dispatches as the stalling case
/// does, but commits a frontier transition between them, so it must keep being
/// dispatched. Without this the strike count could ignore the frontier entirely
/// and the stalling case above would still pass, because nothing there ever
/// makes progress for a revision to supersede.
async fn progress_clears_strikes(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    service
        .policies
        .fixture_install(
            &app,
            leased_policy(
                2,
                AppPolicy {
                    max_stuck_dispatches: 2,
                    ..Default::default()
                },
            ),
        )
        .unwrap();
    let manager = Manager::new(&app).await;
    let owner = assignment(&app);
    publish(&scope, &manager).await;
    let mut grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    let mut claimed = task(scope.accept_job(&grant).await.unwrap());

    // One reclaimed dispatch, then a reported one that supersedes the frontier.
    scope.release_job(&claimed, &grant).await.unwrap();
    lapse_lease(&manager, &grant);
    grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    claimed = task(scope.accept_job(&grant).await.unwrap());
    let receipt = scope
        .complete_job(
            &claimed,
            &grant,
            execution(json!([{"kind":"RunFailed", "ordinal":0, "name":"charge",
                "nameOccurrence":0, "error":{"type":"Error", "message":"retry"}}])),
        )
        .await
        .unwrap();
    assert_eq!(receipt.outcome, JobOutcome::Waiting {});
    manager
        .queue
        .settle(&owner, &receipt.settlement(&grant).unwrap())
        .await
        .unwrap();

    // One more reclaimed dispatch, against the superseding frontier.
    publish(&scope, &manager).await;
    grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    claimed = task(scope.accept_job(&grant).await.unwrap());
    scope.release_job(&claimed, &grant).await.unwrap();
    lapse_lease(&manager, &grant);
    grant = manager.queue.claim(&owner).await.unwrap().unwrap();

    task(scope.accept_job(&grant).await.unwrap());
    assert_eq!(
        run_state(&service, &app, &run.id).await,
        "running",
        "strikes carried across a committed frontier transition would stall a live run"
    );
}

async fn run_state(service: &WorkflowService, app: &AppId, run: &str) -> String {
    let tx = service.begin().await.unwrap();
    let rows = journal_rows(&tx, "runs", json!({"app_id":app.as_str(), "id":run})).await;
    let state = rows[0].text("state").unwrap();
    tx.commit().await.unwrap();
    state
}

case!(
    sqlite_delivery_abandons_a_rollback_that_never_reports,
    postgres_delivery_abandons_a_rollback_that_never_reports,
    abandoning
);

case!(
    sqlite_delivery_keeps_a_rollback_that_supersedes_its_frontier,
    postgres_delivery_keeps_a_rollback_that_supersedes_its_frontier,
    rollback_progress_clears_strikes
);

/// A compensator that never returns is the forward stall one phase later, and
/// the same strikes bound it. The run rests at the failure it was rolling back
/// rather than at a stall, because that verdict is the creator's and a rollback
/// the host gave up on does not overturn it. The obligation it abandoned is
/// named on the summary, which is the only place a creator can learn that an
/// undo may have half-applied.
///
/// The deployment assertions refute each other: the hold that `release_deployment`
/// refuses while the rollback is live must be released once the abandonment has
/// settled the job, so this cannot pass by never pinning a deployment at all.
async fn abandoning(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    service
        .policies
        .fixture_install(
            &app,
            leased_policy(
                2,
                AppPolicy {
                    max_stuck_dispatches: 2,
                    ..Default::default()
                },
            ),
        )
        .unwrap();
    let manager = Manager::new(&app).await;
    let owner = assignment(&app);
    let job = publish(&scope, &manager).await;
    let deployment = job
        .deployment_id()
        .expect("an advance job names its deployment")
        .clone();
    let mut grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    let claimed = task(scope.accept_job(&grant).await.unwrap());
    let receipt = scope
        .complete_job(&claimed, &grant, compensable_failure())
        .await
        .unwrap();
    manager
        .queue
        .settle(&owner, &receipt.settlement(&grant).unwrap())
        .await
        .unwrap();
    assert_eq!(run_state(&service, &app, &run.id).await, "compensating");

    publish(&scope, &manager).await;
    grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    for _ in 0..2 {
        let claimed = task(scope.accept_job(&grant).await.unwrap());
        assert_eq!(
            claimed.assignment().invocation.phase,
            "compensating",
            "a run in rollback must be dispatched its compensators"
        );
        assert_eq!(
            manager.queue.release_deployment(&app, &deployment).await,
            Err(zeroship_workflow_manager::Error::Conflict),
            "an unsettled rollback delivery must keep its deployment"
        );
        scope.release_job(&claimed, &grant).await.unwrap();
        lapse_lease(&manager, &grant);
        grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    }

    let JobAcceptance::Settled(receipt) = scope.accept_job(&grant).await.unwrap() else {
        panic!("a spent strike budget must settle the delivery instead of dispatching again");
    };
    assert_eq!(
        receipt.outcome,
        JobOutcome::Completed {},
        "an abandoned rollback leaves the run at rest, so its job is done"
    );
    assert_eq!(
        run_state(&service, &app, &run.id).await,
        "failed",
        "the run keeps the verdict its own code produced"
    );
    let error = scope.status(&run.id).await.unwrap().error.unwrap();
    assert_eq!(error["type"], json!("Error"));
    assert_eq!(error["message"], json!("intentional failure"));
    assert_eq!(
        error["compensation"],
        json!({
            "total":0, "completed":0, "failed":0, "outcome":"abandoned",
            "abandoned":[{"ordinal":0, "name":"reserve"}],
            "reason":{"type":"StalledError",
                "message":"workflow made no durable progress before the liveness dispatch limit",
                "stuckDispatches":2, "maxStuckDispatches":2},
        }),
        "an obligation nobody reported on is named, not counted as a compensator that ran"
    );

    let tx = service.begin().await.unwrap();
    let steps = journal_rows(&tx, "steps", json!({"app_id":app.as_str()})).await;
    let record: serde_json::Value =
        serde_json::from_str(&steps[0].text("record").unwrap()).unwrap();
    assert_eq!(
        record["step"]["compensationState"],
        json!("abandoned"),
        "a run at rest must not leave its journal claiming a pending compensator"
    );
    tx.commit().await.unwrap();

    manager
        .queue
        .settle(&owner, &receipt.settlement(&grant).unwrap())
        .await
        .unwrap();
    manager
        .queue
        .release_deployment(&app, &deployment)
        .await
        .unwrap();
}

/// Rollback strikes belong to the frontier they were counted against, exactly
/// as forward strikes do.
///
/// This run accumulates as many reclaimed dispatches as the abandoning case
/// does, but discharges one obligation between them, so it must keep being
/// dispatched for the obligation that is left. Without this the strike count
/// could ignore the frontier and the abandoning case above would still pass,
/// because nothing there ever discharges anything.
async fn rollback_progress_clears_strikes(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    service
        .policies
        .fixture_install(
            &app,
            leased_policy(
                2,
                AppPolicy {
                    max_stuck_dispatches: 2,
                    ..Default::default()
                },
            ),
        )
        .unwrap();
    let manager = Manager::new(&app).await;
    let owner = assignment(&app);
    publish(&scope, &manager).await;
    let mut grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    let mut claimed = task(scope.accept_job(&grant).await.unwrap());
    let mut receipt = scope
        .complete_job(&claimed, &grant, two_compensable_failure())
        .await
        .unwrap();
    manager
        .queue
        .settle(&owner, &receipt.settlement(&grant).unwrap())
        .await
        .unwrap();

    // One reclaimed rollback dispatch, then a reported one that discharges the
    // newest obligation and supersedes the frontier.
    publish(&scope, &manager).await;
    grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    claimed = task(scope.accept_job(&grant).await.unwrap());
    scope.release_job(&claimed, &grant).await.unwrap();
    lapse_lease(&manager, &grant);
    grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    claimed = task(scope.accept_job(&grant).await.unwrap());
    receipt = scope
        .complete_job(
            &claimed,
            &grant,
            execution(
                json!([{"kind":"CompensationCompleted", "ordinal":1, "name":"charge",
                    "nameOccurrence":0}]),
            ),
        )
        .await
        .unwrap();
    assert_eq!(receipt.outcome, JobOutcome::Waiting {});
    manager
        .queue
        .settle(&owner, &receipt.settlement(&grant).unwrap())
        .await
        .unwrap();

    // One more reclaimed dispatch, against the superseding frontier.
    publish(&scope, &manager).await;
    grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    claimed = task(scope.accept_job(&grant).await.unwrap());
    scope.release_job(&claimed, &grant).await.unwrap();
    lapse_lease(&manager, &grant);
    grant = manager.queue.claim(&owner).await.unwrap().unwrap();

    task(scope.accept_job(&grant).await.unwrap());
    assert_eq!(
        run_state(&service, &app, &run.id).await,
        "compensating",
        "strikes carried across a discharged obligation would abandon a live rollback"
    );
}

/// A run that completes one compensable step and then fails, so the engine owes
/// exactly one compensator.
fn compensable_failure() -> crate::WorkflowExecution {
    execution(json!([
        {"kind":"StepCompleted", "ordinal":0, "name":"reserve", "nameOccurrence":0,
            "compensable":true, "output":0},
        {"kind":"RunFailed", "error":{"type":"Error", "message":"intentional failure"}},
    ]))
}

/// The same, owing two compensators, so discharging one leaves the run in
/// rollback rather than settling it.
fn two_compensable_failure() -> crate::WorkflowExecution {
    execution(json!([
        {"kind":"StepCompleted", "ordinal":0, "name":"reserve", "nameOccurrence":0,
            "compensable":true, "output":0},
        {"kind":"StepCompleted", "ordinal":1, "name":"charge", "nameOccurrence":0,
            "compensable":true, "output":0},
        {"kind":"RunFailed", "error":{"type":"Error", "message":"intentional failure"}},
    ]))
}

async fn competing(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let manager = Manager::new(&app).await;
    let owner = assignment(&app);
    let job = publish(&scope, &manager).await;
    let grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    let claimed = task(scope.accept_job(&grant).await.unwrap());
    let competing = JobSpec {
        id: zeroship_core::workflow_jobs::JobId::mint(),
        ..job.clone()
    };
    manager.queue.submit(&competing).await.unwrap();
    let duplicate = manager.queue.claim(&owner).await.unwrap().unwrap();
    assert_eq!(duplicate.delivery().job, competing);
    assert!(matches!(
        scope.accept_job(&duplicate).await.unwrap(),
        JobAcceptance::Deferred
    ));
    scope
        .complete_job(
            &claimed,
            &grant,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    let JobAcceptance::Settled(stale) = scope.accept_job(&duplicate).await.unwrap() else {
        panic!("expected obsolete frontier rejection")
    };
    assert_eq!(stale.outcome, JobOutcome::Rejected {});
    manager
        .queue
        .settle(&owner, &stale.settlement(&duplicate).unwrap())
        .await
        .unwrap();
    let tx = service.begin().await.unwrap();
    assert_eq!(
        journal_count(&tx, "tasks", json!({"app_id":app.as_str()})).await,
        1
    );
    tx.commit().await.unwrap();
}

case!(
    sqlite_delivery_cancels_lock_waits,
    postgres_delivery_cancels_lock_waits,
    lock_waits
);

async fn lock_waits(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let manager = Manager::new(&app).await;
    let owner = assignment(&app);
    publish(&scope, &manager).await;
    let grant = manager.queue.claim(&owner).await.unwrap().unwrap();

    for expiry in [true, false] {
        let mut held = service.begin().await.unwrap();
        super::super::app::lock_app(&mut held, &app).await.unwrap();
        let revision = if expiry { 2 } else { 3 };
        let policy = AppPolicy {
            lease_ms: if expiry { 200 } else { 60_000 },
            ..Default::default()
        };
        service
            .policies
            .fixture_install(&app, leased_policy(revision, policy))
            .unwrap();
        let mut accepting = Box::pin(scope.accept_job(&grant));
        assert!(futures::poll!(accepting.as_mut()).is_pending());
        if expiry {
            // The operation must cancel while the creator lock is still held.
            let result = compio::time::timeout(Duration::from_secs(1), accepting)
                .await
                .expect("delivery must cancel while the app lock is held");
            assert!(
                matches!(result, Err(WorkflowServiceError::Timeout)),
                "{result:?}"
            );
            held.commit().await.unwrap();
        } else {
            service
                .policies
                .fixture_install(
                    &app,
                    leased_policy(
                        4,
                        AppPolicy {
                            dispatch: false,
                            ..Default::default()
                        },
                    ),
                )
                .unwrap();
            held.commit().await.unwrap();
            let result = accepting.await;
            assert!(
                matches!(result, Err(WorkflowServiceError::Unavailable(_))),
                "{result:?}"
            );
        }
        let tx = service.begin().await.unwrap();
        for table in ["tasks", "job_receipts"] {
            assert_eq!(
                journal_count(&tx, table, json!({"app_id":app.as_str()})).await,
                0
            );
        }
        tx.commit().await.unwrap();
    }
    assert!(grant.remaining().is_some());
    service
        .policies
        .fixture_install(&app, leased_policy(5, AppPolicy::default()))
        .unwrap();
    task(scope.accept_job(&grant).await.unwrap());
}

/// The app lease of the case the ceiling has to bound. Wider than the ceiling,
/// so an attempt that took its whole authority would be visibly longer.
const UNBOUNDED_LEASE: Duration = Duration::from_mins(1);
/// The control's app lease. Narrower than the ceiling, so the authority bounds
/// that attempt and the ceiling never becomes its binding term.
const BOUNDED_LEASE: Duration = Duration::from_secs(1);

/// Install `lease` as the app's authority and report the budget one attempt
/// gets under it, beside the authority that attempt still holds. Only the
/// lease moves between the two calls below: the revision advances because
/// installing requires it, and the policy is otherwise identical.
fn budget_under(
    service: &WorkflowService,
    scope: &AppWorkflows,
    app: &AppId,
    grant: &impl JobLease,
    revision: i64,
    lease: Duration,
) -> (Duration, Duration) {
    service
        .policies
        .fixture_install(
            app,
            leased_policy(
                revision,
                AppPolicy {
                    lease_ms: i64::try_from(lease.as_millis()).unwrap(),
                    ..AppPolicy::default()
                },
            ),
        )
        .unwrap();
    let captured = CapturedLease::capture(scope, grant).unwrap();
    let budget = attempt_budget(Some(&captured), None);
    (budget, captured.remaining().unwrap())
}

case!(
    sqlite_delivery_bounds_one_attempt_below_its_authority,
    postgres_delivery_bounds_one_attempt_below_its_authority,
    io_ceiling
);

/// An attempt never gets the whole authority it holds: the journal I/O ceiling
/// caps it, so a stalled attempt ends while its grant is still live instead of
/// holding that grant until it lapses.
///
/// This pins the composition, not the magnitude. Both cases move with
/// [`ATTEMPT_IO_CEILING`], so retuning the ceiling keeps them green; what fails
/// is dropping the ceiling term, or letting a lease wider than it through.
async fn io_ceiling(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let manager = Manager::new(&app).await;
    let owner = assignment(&app);
    publish(&scope, &manager).await;
    let grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    assert!(
        grant.remaining().unwrap() > ATTEMPT_IO_CEILING,
        "the manager grant is narrower than the ceiling, so it would bound both cases"
    );

    let (unbounded, authority) = budget_under(&service, &scope, &app, &grant, 2, UNBOUNDED_LEASE);
    assert_eq!(
        unbounded, ATTEMPT_IO_CEILING,
        "an attempt under an authority wider than the ceiling was not capped by it: {unbounded:?}"
    );
    assert!(
        unbounded < authority,
        "one attempt was handed the whole remaining authority: {unbounded:?} of {authority:?}"
    );

    let (bounded, _) = budget_under(&service, &scope, &app, &grant, 3, BOUNDED_LEASE);
    assert!(
        bounded < ATTEMPT_IO_CEILING,
        "the control was capped by the ceiling too, so the wide case proves nothing: {bounded:?}"
    );
    assert!(
        !bounded.is_zero(),
        "the control spent its whole authority before it was measured, so it bounded nothing"
    );
}

fn assignment(app: &AppId) -> Assignment {
    Assignment {
        app_id: app.clone(),
        worker_id: WorkerId::mint(),
        revision: 1.try_into().unwrap(),
        expires_at: (chrono::Utc::now().timestamp_millis() + 120_000)
            .try_into()
            .unwrap(),
    }
}

async fn publish(scope: &AppWorkflows, manager: &Manager) -> JobSpec {
    let job = scope.pending_jobs(None, 1).await.unwrap().remove(0);
    assert_eq!(
        scope
            .publish_job(&job.id, &Publisher::new(&scope.app, manager.queue.clone()))
            .await
            .unwrap(),
        job
    );
    job
}

fn task(acceptance: JobAcceptance) -> DeliveredTask {
    match acceptance {
        JobAcceptance::Execute(task) => *task,
        other => panic!("expected execution, got {other:?}"),
    }
}

// A deliberately malformed trusted-host implementation probes immutable journal
// identity. Production callers use the native manager or authenticated client.
struct ProbeLease {
    delivery: Delivery,
    expires: Instant,
}
impl ProbeLease {
    fn copy(grant: &impl JobLease) -> Self {
        let now = Instant::now();
        Self {
            delivery: grant.delivery().clone(),
            expires: now + grant.remaining().unwrap(),
        }
    }
}
impl JobLease for ProbeLease {
    fn delivery(&self) -> &Delivery {
        &self.delivery
    }
    fn remaining(&self) -> Option<Duration> {
        self.expires
            .checked_duration_since(Instant::now())
            .filter(|time| !time.is_zero())
    }
}

async fn receipts(store: Rc<OrmStore>) {
    let (service, app, other, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let run = scope
        .start(
            &RequestId::mint(),
            "Example",
            StartOptions {
                input: json!({"private":"creator-input"}),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let manager = Manager::new(&app).await;
    let owner = assignment(&app);
    let job = publish(&scope, &manager).await;
    let grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    assert!(matches!(
        service.fixture_app(other).accept_job(&grant).await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    let claimed = task(scope.accept_job(&grant).await.unwrap());
    assert_eq!(claimed.assignment().invocation.run_id, run.id);
    assert!(matches!(
        scope.accept_job(&grant).await.unwrap(),
        JobAcceptance::Deferred
    ));
    let mut changed = ProbeLease::copy(&grant);
    changed.delivery.job.available_at = (job.available_at.get() + 1).try_into().unwrap();
    assert!(matches!(
        scope.accept_job(&changed).await,
        Err(WorkflowServiceError::Conflict(_))
    ));

    let execution =
        execution(json!([{"kind":"RunCompleted", "output":{"private":"creator-output"}}]));
    let receipt = scope
        .complete_job(&claimed, &grant, execution.clone())
        .await
        .unwrap();
    assert_eq!(receipt.job, job);
    assert_eq!(receipt.outcome, JobOutcome::Completed {});
    assert!(!serde_json::to_string(&receipt)
        .unwrap()
        .contains("creator-output"));
    let mut expired = ProbeLease::copy(&grant);
    expired.expires = Instant::now();
    assert_eq!(
        scope
            .complete_job(&claimed, &expired, execution)
            .await
            .unwrap(),
        receipt
    );
    assert!(matches!(
        scope
            .complete_job(
                &claimed,
                &expired,
                super::execution(json!([{"kind":"RunCompleted", "output":"changed"}]))
            )
            .await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    // Model a committed creator result whose manager ACK was never delivered.
    lapse_lease(&manager, &grant);
    let redelivered = manager.queue.claim(&owner).await.unwrap().unwrap();
    assert!(redelivered.delivery().attempt > grant.delivery().attempt);
    let JobAcceptance::Settled(recovered) = scope.accept_job(&redelivered).await.unwrap() else {
        panic!("redelivery must read the committed result")
    };
    assert_eq!(recovered, receipt);
    let command = receipt.settlement(&redelivered).unwrap();
    let settled = manager.queue.settle(&owner, &command).await.unwrap();
    assert_eq!(
        manager.queue.settle(&owner, &command).await.unwrap(),
        settled
    );
    assert!(manager.queue.claim(&owner).await.unwrap().is_none());

    retained_history(&scope, &job, &receipt, &expired, &changed.delivery.job).await;
}

async fn retained_history(
    scope: &AppWorkflows,
    job: &JobSpec,
    receipt: &crate::service::delivery::JobReceipt,
    expired: &ProbeLease,
    changed: &JobSpec,
) {
    let service = &scope.service;
    let tx = service.begin().await.unwrap();
    for table in [
        "tasks",
        "continuation_members",
        "continuation_heads",
        "generations",
        "runs",
    ] {
        tx.database()
            .collection(&format!("__zeroship_workflow_{table}"))
            .unwrap()
            .execute(zeroship_data_orm::orm::Operation::Purge {
                filter: zeroship_data_orm::value!({"app_id":scope.app.as_str()}),
                many: true,
            })
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    let reopened = WorkflowService::open(service.store.clone(), service.policies.clone())
        .await
        .unwrap()
        .fixture_app(scope.app.clone());
    assert_eq!(
        reopened.job_receipt(job).await.unwrap(),
        Some(receipt.clone())
    );
    let JobAcceptance::Settled(replayed) = reopened.accept_job(expired).await.unwrap() else {
        panic!("expected retained receipt")
    };
    assert_eq!(&replayed, receipt);
    assert!(reopened.job_receipt(changed).await.is_err());
}

async fn attempts(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let manager = Manager::new(&app).await;
    let owner = assignment(&app);
    publish(&scope, &manager).await;
    let grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    let claimed = task(scope.accept_job(&grant).await.unwrap());
    let worker = WorkerIdentity::new(owner.worker_id.as_str().into()).unwrap();
    let a = claimed.assignment();
    assert_eq!(
        service.heartbeat(&worker, &a.id, &a.token).await,
        Err(WorkflowServiceError::PermissionDenied)
    );
    assert_eq!(
        service
            .complete(
                &worker,
                &a.id,
                &a.token,
                execution(json!([{"kind":"RunCompleted"}]))
            )
            .await,
        Err(WorkflowServiceError::PermissionDenied)
    );
    assert_eq!(
        service.release(&worker, &a.id, &a.token).await,
        Err(WorkflowServiceError::PermissionDenied)
    );
    scope.release_job(&claimed, &grant).await.unwrap();
    scope.release_job(&claimed, &grant).await.unwrap();
    assert!(service.poll(&worker).await.unwrap().is_none());
    lapse_lease(&manager, &grant);
    let next = manager.queue.claim(&owner).await.unwrap().unwrap();
    assert!(next.delivery().attempt > grant.delivery().attempt);
    let replacement = task(scope.accept_job(&next).await.unwrap());
    assert_ne!(
        replacement.assignment().token.as_str(),
        claimed.assignment().token.as_str()
    );
    assert!(scope.release_job(&claimed, &next).await.is_err());
    assert!(scope
        .complete_job(&claimed, &next, execution(json!([{"kind":"RunCompleted"}])))
        .await
        .is_err());
    assert!(scope
        .complete_job(
            &claimed,
            &grant,
            execution(json!([{"kind":"RunCompleted"}]))
        )
        .await
        .is_err());
    let receipt = scope
        .complete_job(
            &replacement,
            &next,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    manager
        .queue
        .settle(&owner, &receipt.settlement(&next).unwrap())
        .await
        .unwrap();
}

/// Lapse a delivery's queue lease in place, so the next claim reclaims it.
///
/// The manager redelivers a leased job once its lease deadline has passed, and
/// that deadline is a row the queue owns rather than an interval this process
/// has to outlive. Writing it is what keeps a reclaimed dispatch free of the
/// grant's width: waiting the lease out instead would force every test that
/// wants a reclaim to hold a lease short enough to wait for, and the same width
/// is the budget `accept_job` gets to answer under, so the wait a fixture can
/// afford and the time the operation needs would be one number.
fn lapse_lease(manager: &Manager, grant: &impl JobLease) {
    let changed = rusqlite::Connection::open(&manager.path)
        .unwrap()
        .execute(
            "UPDATE jobs SET lease_deadline = 0 WHERE id = ?1 AND state = 'leased'",
            [grant.delivery().job.id.as_str()],
        )
        .unwrap();
    assert_eq!(
        changed, 1,
        "a delivery must hold a live queue lease for the next claim to reclaim it"
    );
}

async fn checkpoint(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let manager = Manager::new(&app).await;
    let owner = assignment(&app);
    let original = publish(&scope, &manager).await;
    let grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    let claimed = task(scope.accept_job(&grant).await.unwrap());
    let receipt = scope
        .complete_job(
            &claimed,
            &grant,
            execution(json!([{
                "kind":"Sleep", "ordinal":0, "name":"delay", "nameOccurrence":0,
                "wakeAt":chrono::Utc::now() + chrono::Duration::hours(1)
            }])),
        )
        .await
        .unwrap();
    assert_eq!(receipt.outcome, JobOutcome::Waiting {});
    let pending = scope.pending_jobs(None, 10).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert!(
        matches!(&pending[0].operation, JobOperation::Advance {run_id, revision, ..} if run_id.as_str()==run.id && revision.get()==2)
    );
    assert_ne!(pending[0].id, original.id);
    assert!(pending[0].available_at > original.available_at);
    let worker = WorkerIdentity::new(owner.worker_id.as_str().into()).unwrap();
    let tx = service.begin().await.unwrap();
    journal_update(&tx, "runs", json!({"id":run.id}), json!({"due_at":0})).await;
    tx.commit().await.unwrap();
    assert!(service.poll(&worker).await.unwrap().is_none());
    assert!(matches!(
        scope.accept_job(&grant).await.unwrap(),
        JobAcceptance::Settled(_)
    ));
    manager
        .queue
        .settle(&owner, &receipt.settlement(&grant).unwrap())
        .await
        .unwrap();
    assert!(manager.queue.claim(&owner).await.unwrap().is_none());
    assert_eq!(publish(&scope, &manager).await, pending[0]);
}

/// The authority window the narrowed case runs under. Both the manager grant
/// and the app lease below sit far outside it, so a task this short cannot be
/// mistaken for one either of those bounded.
const AUTHORITY_WINDOW: Duration = Duration::from_secs(5);
/// The app lease every case here runs under, and the bound the control expects.
const WINDOWED_LEASE: Duration = Duration::from_secs(60);
/// The manager grant, outlasting both of the above so it bounds neither.
const WINDOWED_GRANT: Duration = Duration::from_secs(120);
/// The control's window, wide enough that the app lease bounds it instead.
const WIDE_WINDOW: Duration = Duration::from_secs(3_600);

/// Reissue an app's policy with a lease window ending `window` from now. Only
/// the window moves: revision and policy content are identical for the narrowed
/// case and its control.
fn windowed(window: Duration) -> PolicySnapshot {
    PolicySnapshot::lease(
        2.try_into().unwrap(),
        AppPolicy {
            lease_ms: i64::try_from(WINDOWED_LEASE.as_millis()).unwrap(),
            ..AppPolicy::default()
        },
        Instant::now() + window,
    )
    .unwrap()
    .with_ingress_epoch(Some(super::open_epoch()))
}

/// Publish one job, claim it under a grant that outlasts every other bound
/// here, narrow the app's authority to `window`, and report how long the task
/// the delivery hands back says it may run.
async fn accept_within(service: &WorkflowService, app: &AppId, window: Duration) -> Duration {
    let scope = service.fixture_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let manager = Manager::with_options(
        app,
        Options {
            lease: WINDOWED_GRANT,
            ..Default::default()
        },
    )
    .await;
    let owner = assignment(app);
    publish(&scope, &manager).await;
    let grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    service.fixture_install(app, windowed(window)).unwrap();
    task(scope.accept_job(&grant).await.unwrap())
        .remaining()
        .unwrap()
}

/// Captured delivery authority is the window the host can still stand behind,
/// and both the manager grant and the app's own lease may outlast it. The task
/// a delivery hands back must expire no later than that window, because the
/// executor reads the task's own expiry and nothing rechecks the authority once
/// the execution has begun.
///
/// This binds the clamp in `CapturedLease::capture`, the one place an accepted
/// delivery's expiry meets the authority deadline. The delivery runner carries
/// the same property for an attempt it drives, through the bound it builds its
/// execution guard and renewal phase from - but a task accepted here is handed
/// back with no guard around it, so that enforcement point cannot stand in for
/// this one, and a case placed there passes whether or not this clamp is there.
///
/// The two apps differ in exactly one thing, the width of the window. The
/// control is what stops the narrowed assertion from passing because something
/// unrelated shortens every delivery.
async fn authority_window(store: Rc<OrmStore>) {
    let (service, narrowed, control, _deployments) = registered_service(store).await;
    let narrow = accept_within(&service, &narrowed, AUTHORITY_WINDOW).await;
    assert!(
        narrow <= AUTHORITY_WINDOW,
        "an accepted task outlived the authority window that had to bound it: {narrow:?}"
    );
    let wide = accept_within(&service, &control, WIDE_WINDOW).await;
    assert!(
        wide > AUTHORITY_WINDOW,
        "the control was shortened too, so the narrowed case proves nothing: {wide:?}"
    );
    assert!(
        wide <= WINDOWED_LEASE,
        "the control outlived the app lease that had to bound it: {wide:?}"
    );
}

/// A creator-clock deadline is converted back to the monotonic clock before an
/// executor is handed it, and captured authority is the ceiling that conversion
/// may not cross. Only a direct call reaches the ceiling: on the accept and
/// heartbeat paths the deadline handed over is itself derived from the captured
/// lease, and the database clock read that converts it back is taken after the
/// read it was built from, so an agreeing clock lands inside the ceiling on its
/// own and the clamp is never the binding term there. A creator clock that ran
/// slow or stepped back across the transaction is the case the clamp is for,
/// and no fixture here can produce one, so the contract is stated where it is
/// owned instead of through a path that cannot reach it.
///
/// The two calls differ in exactly one thing, the ceiling. Without the far one
/// the near assertion would also pass if the creator deadline were ignored
/// altogether.
async fn authority_caps_creator_deadline(store: Rc<OrmStore>) {
    let (service, _, _, _deployments) = registered_service(store).await;
    let mut tx = service.begin().await.unwrap();
    let creator_window = Duration::from_secs(600);
    let deadline = tx.now().await.unwrap() + i64::try_from(creator_window.as_millis()).unwrap();
    let near = Instant::now() + AUTHORITY_WINDOW;
    let capped = creator_deadline(&mut tx, deadline, near).await.unwrap();
    assert!(
        capped <= near,
        "a creator deadline re-anchored past the captured authority was handed out"
    );
    let far = Instant::now() + creator_window * 2;
    let uncapped = creator_deadline(&mut tx, deadline, far).await.unwrap();
    assert!(
        uncapped > near,
        "the control was capped too, so the near case proves nothing"
    );
    assert!(
        uncapped < far,
        "the control ignored the creator deadline and took its ceiling instead"
    );
    tx.commit().await.unwrap();
}

async fn policy_bounds(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let manager = Manager::new(&app).await;
    let owner = assignment(&app);
    publish(&scope, &manager).await;
    let grant = manager.queue.claim(&owner).await.unwrap().unwrap();
    service
        .policies
        .fixture_install(
            &app,
            leased_policy(
                2,
                AppPolicy {
                    dispatch: false,
                    ..Default::default()
                },
            ),
        )
        .unwrap();
    assert!(matches!(
        scope.accept_job(&grant).await.unwrap(),
        JobAcceptance::Deferred
    ));
    assert!(scope
        .job_receipt(&grant.delivery().job)
        .await
        .unwrap()
        .is_none());
    service
        .policies
        .fixture_install(
            &app,
            leased_policy(
                3,
                AppPolicy {
                    lease_ms: 700,
                    ..Default::default()
                },
            ),
        )
        .unwrap();
    let claimed = task(scope.accept_job(&grant).await.unwrap());
    assert!(claimed.remaining().unwrap() < grant.remaining().unwrap());
    assert!(claimed.assignment().lease_ms <= 700);
    let renewed_grant = manager
        .queue
        .heartbeat(&owner, grant.delivery())
        .await
        .unwrap();
    let mut renewed = claimed.clone();
    let renewal = scope.heartbeat_job(&renewed, &renewed_grant).await.unwrap();
    assert_eq!(renewal.control(), crate::service::ControlIntent::None);
    let previous = claimed.remaining().unwrap();
    renewed.renew(renewal);
    assert_eq!(
        renewed.assignment().token.as_str(),
        claimed.assignment().token.as_str()
    );
    assert!(
        renewed.remaining().unwrap() > previous,
        "the applied renewal left the task on its original creator expiration, \
         so the refreshed bound below proves nothing"
    );
    compio::time::sleep(renewed.remaining().unwrap() + Duration::from_millis(10)).await;
    assert!(renewed_grant.remaining().is_some());
    assert_eq!(
        scope
            .heartbeat_job(&renewed, &renewed_grant)
            .await
            .unwrap_err(),
        WorkflowServiceError::Timeout
    );
    assert_eq!(
        scope
            .complete_job(
                &renewed,
                &renewed_grant,
                execution(json!([{"kind":"RunCompleted"}]))
            )
            .await,
        Err(WorkflowServiceError::Timeout)
    );
    assert_eq!(
        scope.release_job(&renewed, &renewed_grant).await,
        Err(WorkflowServiceError::Timeout)
    );
    let fresh = task(scope.accept_job(&renewed_grant).await.unwrap());
    assert_ne!(
        fresh.assignment().token.as_str(),
        claimed.assignment().token.as_str()
    );
}
