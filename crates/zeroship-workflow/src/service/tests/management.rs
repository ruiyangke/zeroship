use super::*;
use crate::service::{app, frontier};
use futures::{
    future::{select, Either},
    stream::FuturesUnordered,
    FutureExt, StreamExt,
};
use std::time::{Duration, Instant};
use zeroship_core::workflow_coordination::{
    ManagementOutcome, RestartOptions, RestartTarget, RunId, RunOperation, RunState,
};
use zeroship_core::workflow_jobs::{DeploymentId, ManagementCommand};
use zeroship_data_orm::{orm::Operation, value};

mod atomic_application;
pub(super) mod fixture;
mod latest;
mod ordering;
mod readback;
use fixture::{active, restarted, started, transition, Grant};

#[compio::test]
async fn sqlite_management_receipts_survive_app_receipt_loss_and_worker_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    replay_contract(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_management_receipts_survive_app_receipt_loss_and_worker_reopen() {
    let fixture = PostgresFixture::start().await;
    replay_contract(Rc::new(fixture.store.clone())).await;
}

#[compio::test]
async fn sqlite_management_preserves_rejections_and_retries_transient_failures() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    outcome_contract(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_management_preserves_rejections_and_retries_transient_failures() {
    let fixture = PostgresFixture::start().await;
    outcome_contract(Rc::new(fixture.store.clone())).await;
}

#[compio::test]
async fn sqlite_management_receipt_failure_rolls_back_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("zs-workflow.sqlite");
    let store = sqlite_store(&path).await;
    atomicity_contract(Rc::new(store), ReceiptFault::Sqlite(path)).await;
}

#[compio::test]
async fn postgres_management_receipt_failure_rolls_back_restart() {
    let fixture = PostgresFixture::start().await;
    atomicity_contract(
        Rc::new(fixture.store.clone()),
        ReceiptFault::Postgres(fixture.admin_url.clone()),
    )
    .await;
}

#[compio::test]
async fn postgres_management_receipt_wait_keeps_original_authority_after_refresh() {
    let fixture = PostgresFixture::start().await;
    let (service, local, _, _deployments) =
        registered_service(Rc::new(fixture.store.clone())).await;
    let run = start(&service, &local).await;
    let scope = service.fixture_app(local.clone());
    let request = started(&local, &run, 1);
    let barrier = PgBarrier::install(&fixture.admin_url, BarrierSite::Receipt).await;
    service
        .fixture_register(
            &local,
            PolicySnapshot::lease(
                2.try_into().unwrap(),
                AppPolicy::default(),
                Instant::now() + Duration::from_secs(3),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let (worker, pending) = match select(
        barrier.blocked_worker().boxed_local(),
        scope.management_outcome(&request).boxed_local(),
    )
    .await
    {
        Either::Left(result) => result,
        Either::Right((result, _)) => {
            panic!("management ended before the receipt barrier: {result:?}")
        }
    };
    service
        .policies
        .fixture_install(
            &local,
            PolicySnapshot::lease(
                2.try_into().unwrap(),
                AppPolicy::default(),
                Instant::now() + Duration::from_secs(30),
            )
            .unwrap(),
        )
        .unwrap();
    let result = compio::time::timeout(Duration::from_secs(10), pending)
        .await
        .expect("host authority expiry must cancel its blocked receipt write");
    assert!(
        matches!(result, Err(WorkflowServiceError::Unavailable(_))),
        "{result:?}"
    );
    service
        .policies
        .fixture_authority(&local)
        .unwrap()
        .check()
        .unwrap();
    barrier.wait_for_rollback(worker).await;
    barrier.remove().await;
    assert_rolled_back(&service, &local, &run, &request).await;
    service
        .fixture_register(&local, leased_policy(3, AppPolicy::default()))
        .await
        .unwrap();
    assert_eq!(
        scope.management_outcome(&request).await.unwrap(),
        restarted(&active(&service, &local).await.id)
    );
    assert_eq!(head(&service, &local, &run).await, (1, "queued".into()));
    assert_eq!(receipt_count(&service, &request).await, 1);
}

#[compio::test]
async fn postgres_cancelling_management_receipt_wait_rolls_back_restart() {
    cancellation_contract(BarrierSite::Receipt).await;
}

#[compio::test]
async fn postgres_cancelling_management_lifecycle_wait_rolls_back_restart() {
    cancellation_contract(BarrierSite::Lifecycle).await;
}

#[compio::test]
async fn postgres_management_database_denial_remains_retryable() {
    let fixture = PostgresFixture::start().await;
    let (service, local, _, _deployments) =
        registered_service(Rc::new(fixture.store.clone())).await;
    let run = start(&service, &local).await;
    let scope = service.fixture_app(local.clone());
    let request = started(&local, &run, 1);
    let admin = connect(&fixture.admin_url).await;
    admin.batch_execute("REVOKE INSERT ON customer.__zeroship_workflow_management_receipts FROM app_customer_role")
        .await.unwrap();
    let result = scope.management_outcome(&request).await;
    assert!(
        result.is_err(),
        "database authority failure became a durable outcome: {result:?}"
    );
    assert_rolled_back(&service, &local, &run, &request).await;
    admin
        .batch_execute(
            "GRANT INSERT ON customer.__zeroship_workflow_management_receipts TO app_customer_role",
        )
        .await
        .unwrap();
    assert_eq!(
        scope.management_outcome(&request).await.unwrap(),
        restarted(&active(&service, &local).await.id)
    );
    assert_eq!(receipt_count(&service, &request).await, 1);
    assert_eq!(head(&service, &local, &run).await, (1, "queued".into()));
}

async fn start(service: &WorkflowService, app: &AppId) -> String {
    service
        .fixture_app(app.clone())
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap()
        .id
}

async fn generation_steps(
    service: &WorkflowService,
    app_id: &AppId,
    run: &str,
    generation: i64,
) -> i64 {
    let tx = service.begin().await.unwrap();
    let count = journal_count(
        &tx,
        "steps",
        json!({"app_id":app_id.as_str(), "run_id":run, "generation":generation}),
    )
    .await;
    tx.commit().await.unwrap();
    count
}

async fn head(service: &WorkflowService, app_id: &AppId, run: &str) -> (i64, String) {
    let mut tx = service.begin().await.unwrap();
    app::lock_app(&mut tx, app_id).await.unwrap();
    let row = app::lock_run(&mut tx, app_id, run).await.unwrap();
    let result = (
        row.integer("generation").unwrap(),
        row.text("state").unwrap(),
    );
    tx.commit().await.unwrap();
    result
}

#[expect(
    clippy::future_not_send,
    reason = "management fixtures use the owning compio journal"
)]
async fn receipt_count(service: &WorkflowService, command: &Grant) -> i64 {
    let tx = service.begin().await.unwrap();
    let count = journal_count(
        &tx,
        "management_receipts",
        json!({
            "app_id":command.delivery.job.app_id.as_str(), "request_id":command.request_id().as_str(),
        }),
    )
    .await;
    tx.commit().await.unwrap();
    count
}

#[compio::test]
async fn sqlite_partial_restart_receipt_carries_its_prefix_and_source_pin() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    prefix_receipt_contract(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_partial_restart_receipt_carries_its_prefix_and_source_pin() {
    let fixture = PostgresFixture::start().await;
    prefix_receipt_contract(Rc::new(fixture.store.clone())).await;
}

/// A restart decides two things a transition does not: how much of the journal
/// it keeps, and which deployment the new generation replays against. Both ride
/// the receipt, so this drives a partial restart whose prefix is not the whole
/// run and whose source deployment is no longer the active one, then replays
/// the command so the assertion crosses the journal's encode and decode instead
/// of only reading the value the transaction just built.
///
/// The whole-run restart beside it is the control: same run, same source pin,
/// and only the target differs, so a prefix reported from a constant fails one
/// of the two. A third restart names the head generation's first step, which
/// retains nothing and is therefore the whole restart under another spelling;
/// the receipts of the two must agree.
///
/// This does NOT catch a receipt that reports the right pair while the journal
/// restarted somewhere else - the head generation and the retained step count
/// are asserted separately for that - and it does not exercise a restart onto
/// the latest deployment, which `latest::exact_target` binds.
async fn prefix_receipt_contract(store: Rc<OrmStore>) {
    use super::super::WorkerIdentity;
    let (service, local, _, deployments) = registered_service(store).await;
    let scope = service.fixture_app(local.clone());
    let worker = WorkerIdentity::new("worker".into()).unwrap();
    let source = active(&service, &local).await;
    let run = start(&service, &local).await;
    let task = service.poll(&worker).await.unwrap().unwrap();
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([
                {"kind":"StepCompleted","ordinal":0,"name":"keep","output":1},
                {"kind":"StepCompleted","ordinal":1,"name":"redo","output":2},
                {"kind":"RunCompleted"}
            ])),
        )
        .await
        .unwrap();
    let replacement = deployments.deploy(&local).await;
    service.activate_deploy(&local, &replacement).await.unwrap();
    assert_ne!(active(&service, &local).await.id, source.id);

    let partial = Grant::new(
        &local,
        &run,
        1,
        ManagementCommand::RestartStarted {
            from: Some(RestartTarget {
                name: "redo".into(),
                occurrence: None,
            }),
        },
    );
    let expected = ManagementOutcome::Restarted {
        state: RunState::Queued,
        restarted_from_ordinal: Some(1),
        pinned_to: DeploymentId::parse(&source.id).unwrap(),
    };
    assert_eq!(scope.management_outcome(&partial).await.unwrap(), expected);
    assert_eq!(
        scope.management_outcome(&partial.retry()).await.unwrap(),
        expected
    );
    assert_eq!(head(&service, &local, &run).await, (1, "queued".into()));
    assert_eq!(generation_steps(&service, &local, &run, 1).await, 1);

    // Naming the head generation's first step keeps none of it, so the receipt
    // reports the empty prefix the way a whole restart does. The retained step
    // count beside it is what makes the two the same restart rather than two
    // spellings this assertion merely agrees to call equal.
    let first = Grant::new(
        &local,
        &run,
        2,
        ManagementCommand::RestartStarted {
            from: Some(RestartTarget {
                name: "keep".into(),
                occurrence: None,
            }),
        },
    );
    assert_eq!(
        scope.management_outcome(&first).await.unwrap(),
        restarted(&source.id)
    );
    assert_eq!(head(&service, &local, &run).await, (2, "queued".into()));
    assert_eq!(generation_steps(&service, &local, &run, 2).await, 0);

    let whole = started(&local, &run, 3);
    assert_eq!(
        scope.management_outcome(&whole).await.unwrap(),
        restarted(&source.id)
    );
    assert_eq!(head(&service, &local, &run).await, (3, "queued".into()));
    assert_eq!(generation_steps(&service, &local, &run, 3).await, 0);
}

async fn replay_contract(store: Rc<OrmStore>) {
    let (service, local, foreign, deployments) = registered_service(store.clone()).await;
    let run = start(&service, &local).await;
    let scope = service.fixture_app(local.clone());
    let request = started(&local, &run, 1);
    let original = scope.management_outcome(&request).await.unwrap();
    assert_eq!(original, restarted(&active(&service, &local).await.id));
    assert_eq!(head(&service, &local, &run).await, (1, "queued".into()));
    assert_eq!(receipt_count(&service, &request).await, 1);

    let mut mismatch = request.clone();
    *mismatch.command_mut() = ManagementCommand::Transition {
        operation: RunOperation::Pause,
    };
    assert!(matches!(
        scope.management_outcome(&mismatch).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    mismatch = request.clone();
    if let zeroship_core::workflow_jobs::JobOperation::Management { run_id, .. } =
        &mut mismatch.delivery.job.operation
    {
        *run_id = RunId::mint();
    }
    assert!(matches!(
        scope.management_outcome(&mismatch).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    mismatch = request.clone();
    mismatch.delivery.job.app_id = foreign.clone();
    assert_eq!(
        scope.management_outcome(&mismatch).await,
        Err(WorkflowServiceError::PermissionDenied)
    );
    assert_eq!(
        service
            .fixture_app(foreign.clone())
            .management_outcome(&request)
            .await,
        Err(WorkflowServiceError::PermissionDenied)
    );
    assert_eq!(receipt_count(&service, &mismatch).await, 0);

    let tx = service.begin().await.unwrap();
    assert!(journal_count(&tx, "requests", json!({"app_id":local.as_str()})).await > 0);
    // Simulate app-receipt loss to check separate management identity. Normal
    // journal operations cannot retire accepted requests by age.
    tx.database()
        .collection("__zeroship_workflow_requests")
        .unwrap()
        .execute(Operation::Purge {
            filter: value!({"app_id":local.as_str()}),
            many: true,
        })
        .await
        .unwrap();
    assert_eq!(
        journal_count(&tx, "requests", json!({"app_id":local.as_str()})).await,
        0
    );
    tx.commit().await.unwrap();
    drop(scope);
    drop(service);

    let service = WorkflowService::open(store, Arc::new(HostPolicies::default()))
        .await
        .unwrap()
        .with_deployments(deployments.binding(&[&local, &foreign]));
    unconfigured_replay(&service, &request, &original).await;
    for app_id in [&local, &foreign] {
        service
            .fixture_register(app_id, leased_policy(2, AppPolicy::default()))
            .await
            .unwrap();
    }
    let scope = service.fixture_app(local.clone());
    assert_eq!(scope.management_outcome(&request).await.unwrap(), original);
    assert_eq!(head(&service, &local, &run).await, (1, "queued".into()));
    scope
        .transition(&RequestId::mint(), &run, RunOperation::Pause)
        .await
        .unwrap();
    assert_eq!(scope.management_outcome(&request).await.unwrap(), original);
    assert_eq!(head(&service, &local, &run).await, (1, "paused".into()));
    assert_eq!(receipt_count(&service, &request).await, 1);

    let concurrent_run = start(&service, &local).await;
    let concurrent = started(&local, &concurrent_run, 1);
    let mut requests = FuturesUnordered::new();
    for _ in 0..4 {
        requests.push(scope.management_outcome(&concurrent));
    }
    let mut completed = Vec::new();
    while let Some(result) = requests.next().await {
        completed.push(result.unwrap());
    }
    assert!(!completed.is_empty());
    assert!(completed.iter().all(|result| *result == original));
    assert_eq!(
        head(&service, &local, &concurrent_run).await,
        (1, "queued".into())
    );
    assert_eq!(receipt_count(&service, &concurrent).await, 1);
}

#[expect(
    clippy::future_not_send,
    reason = "receipt replay uses the owning compio journal"
)]
async fn unconfigured_replay(
    service: &WorkflowService,
    request: &Grant,
    original: &ManagementOutcome,
) {
    let local = &request.delivery.job.app_id;
    let unconfigured = service.fixture_app(local.clone());
    assert_eq!(
        &unconfigured.management_outcome(request).await.unwrap(),
        original
    );
    let mut changed = request.clone();
    *changed.command_mut() = ManagementCommand::Transition {
        operation: RunOperation::Pause,
    };
    assert!(matches!(
        unconfigured.management_outcome(&changed).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    let fresh = started(local, request.run_id().as_str(), 2);
    assert!(matches!(
        unconfigured.management_outcome(&fresh).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_eq!(receipt_count(service, &fresh).await, 0);
    service
        .policies
        .fixture_install(
            local,
            PolicySnapshot::lease(1.try_into().unwrap(), AppPolicy::default(), Instant::now())
                .unwrap(),
        )
        .unwrap();
    assert_eq!(
        &unconfigured.management_outcome(request).await.unwrap(),
        original
    );
    assert!(matches!(
        unconfigured.management_outcome(&fresh).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_eq!(receipt_count(service, &fresh).await, 0);
}

async fn outcome_contract(store: Rc<OrmStore>) {
    let (service, local, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(local.clone());
    let absent = RunId::mint();
    let missing = started(&local, absent.as_str(), 1);
    assert_eq!(
        scope.management_outcome(&missing).await.unwrap(),
        ManagementOutcome::NotFound {}
    );
    assert_eq!(receipt_count(&service, &missing).await, 1);
    let mut tx = service.begin().await.unwrap();
    app::lock_app(&mut tx, &local).await.unwrap();
    let deployment = app::active_deploy(&mut tx, &local).await.unwrap();
    let now = tx.now().await.unwrap();
    app::insert_root_run(
        &mut tx,
        &local,
        &app::NewRun {
            id: absent.as_str(),
            name: "Example",
            deploy: &deployment.id,
            options: &StartOptions::default(),
            input_source: None,
            max_input_bytes: AppPolicy::default().max_input_bytes,
        },
        now,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        scope.management_outcome(&missing).await.unwrap(),
        ManagementOutcome::NotFound {}
    );
    assert_eq!(
        head(&service, &local, absent.as_str()).await,
        (0, "queued".into())
    );

    let run = start(&service, &local).await;
    let invalid = Grant::new(
        &local,
        &run,
        1,
        ManagementCommand::RestartStarted {
            from: Some(RestartTarget {
                name: String::new(),
                occurrence: None,
            }),
        },
    );
    assert_eq!(
        scope.management_outcome(&invalid).await.unwrap(),
        ManagementOutcome::Conflict {}
    );
    assert_eq!(receipt_count(&service, &invalid).await, 1);
    assert_eq!(head(&service, &local, &run).await, (0, "queued".into()));

    finish(&service, &local, &run).await;
    let conflict = transition(&local, &run, 2, RunOperation::Pause);
    assert_eq!(
        scope.management_outcome(&conflict).await.unwrap(),
        ManagementOutcome::Conflict {}
    );
    scope
        .restart(&RequestId::mint(), &run, RestartOptions::default())
        .await
        .unwrap();
    assert_eq!(
        scope.management_outcome(&conflict).await.unwrap(),
        ManagementOutcome::Conflict {}
    );
    assert_eq!(head(&service, &local, &run).await, (1, "queued".into()));
    assert_eq!(receipt_count(&service, &conflict).await, 1);

    service
        .fixture_register(
            &local,
            leased_policy(
                2,
                AppPolicy {
                    admission: false,
                    ..Default::default()
                },
            ),
        )
        .await
        .unwrap();
    let denied = started(&local, &run, 3);
    assert_eq!(
        scope.management_outcome(&denied).await.unwrap(),
        ManagementOutcome::Denied {}
    );
    service
        .fixture_register(&local, leased_policy(3, AppPolicy::default()))
        .await
        .unwrap();
    assert_eq!(
        scope.management_outcome(&denied).await.unwrap(),
        ManagementOutcome::Denied {}
    );
    assert_eq!(head(&service, &local, &run).await, (1, "queued".into()));
    assert_eq!(receipt_count(&service, &denied).await, 1);
    let accepted = started(&local, &run, 4);
    assert_eq!(
        scope.management_outcome(&accepted).await.unwrap(),
        restarted(&active(&service, &local).await.id)
    );
    assert_eq!(head(&service, &local, &run).await, (2, "queued".into()));

    finish(&service, &local, &run).await;
    service
        .fixture_register(
            &local,
            leased_policy(
                4,
                AppPolicy {
                    max_live_runs: 0,
                    ..Default::default()
                },
            ),
        )
        .await
        .unwrap();
    let capacity = started(&local, &run, 5);
    assert!(matches!(
        scope.management_outcome(&capacity).await,
        Err(WorkflowServiceError::ResourceExhausted(_))
    ));
    assert_eq!(receipt_count(&service, &capacity).await, 0);
    service
        .fixture_register(&local, leased_policy(5, AppPolicy::default()))
        .await
        .unwrap();
    assert_eq!(
        scope.management_outcome(&capacity).await.unwrap(),
        restarted(&active(&service, &local).await.id)
    );
    assert_eq!(head(&service, &local, &run).await, (3, "queued".into()));

    service
        .policies
        .fixture_install(
            &local,
            PolicySnapshot::lease(6.try_into().unwrap(), AppPolicy::default(), Instant::now())
                .unwrap(),
        )
        .unwrap();
    // An acknowledged decision remains readable after authority expires. Only
    // previously unseen commands need renewed mutation authority.
    assert_eq!(
        scope.management_outcome(&capacity).await.unwrap(),
        restarted(&active(&service, &local).await.id)
    );
    let expired = transition(&local, &run, 6, RunOperation::Pause);
    assert!(matches!(
        scope.management_outcome(&expired).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_eq!(receipt_count(&service, &expired).await, 0);
    assert_eq!(head(&service, &local, &run).await, (3, "queued".into()));
    service
        .fixture_register(&local, leased_policy(7, AppPolicy::default()))
        .await
        .unwrap();
    assert_eq!(
        scope.management_outcome(&expired).await.unwrap(),
        ManagementOutcome::Applied {
            state: RunState::Paused
        }
    );
    assert_eq!(receipt_count(&service, &expired).await, 1);
}

async fn finish(service: &WorkflowService, app_id: &AppId, run_id: &str) {
    let mut tx = service.begin().await.unwrap();
    app::lock_app(&mut tx, app_id).await.unwrap();
    let run = app::lock_run(&mut tx, app_id, run_id).await.unwrap();
    let now = tx.now().await.unwrap();
    frontier::finish(&mut tx, app_id, &run, RunState::Completed, None, now)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

async fn atomicity_contract(store: Rc<OrmStore>, fault: ReceiptFault) {
    let (service, local, _, _deployments) = registered_service(store).await;
    let run = start(&service, &local).await;
    let scope = service.fixture_app(local.clone());
    let request = started(&local, &run, 1);
    let before = atomic_application::persisted(&service, &local).await;
    fault.install().await;
    assert!(matches!(
        scope.management_outcome(&request).await,
        Err(WorkflowServiceError::Internal(_) | WorkflowServiceError::Unavailable(_))
    ));
    assert_rolled_back(&service, &local, &run, &request).await;
    assert_eq!(
        atomic_application::persisted(&service, &local).await,
        before
    );
    fault.remove().await;
    let outcome = scope.management_outcome(&request).await.unwrap();
    assert_eq!(outcome, restarted(&active(&service, &local).await.id));
    assert_eq!(scope.management_outcome(&request).await.unwrap(), outcome);
    assert_eq!(head(&service, &local, &run).await, (1, "queued".into()));
    assert_eq!(receipt_count(&service, &request).await, 1);
    let tx = service.begin().await.unwrap();
    assert_eq!(
        journal_count(
            &tx,
            "outbox",
            json!({"app_id":local.as_str(), "kind":"workflow.restart"})
        )
        .await,
        1
    );
    tx.commit().await.unwrap();
}

#[expect(
    clippy::future_not_send,
    reason = "management fixtures use the owning compio journal"
)]
async fn assert_rolled_back(service: &WorkflowService, local: &AppId, run: &str, request: &Grant) {
    assert_eq!(head(service, local, run).await, (0, "queued".into()));
    assert_eq!(receipt_count(service, request).await, 0);
    let tx = service.begin().await.unwrap();
    for table in ["management_receipts", "job_receipts"] {
        assert_eq!(
            journal_count(&tx, table, json!({"app_id":local.as_str()})).await,
            0
        );
    }
    assert_eq!(
        journal_count(
            &tx,
            "generations",
            json!({"app_id":local.as_str(), "run_id":run, "generation":1})
        )
        .await,
        0
    );
    assert!(advance_intents(&tx, local, run, Some(1)).await.is_empty());
    let generation = journal_rows(
        &tx,
        "generations",
        json!({"app_id":local.as_str(), "run_id":run, "generation":0}),
    )
    .await;
    assert_eq!(generation[0].text("state").unwrap(), "queued");
    assert_eq!(
        journal_count(
            &tx,
            "outbox",
            json!({"app_id":local.as_str(), "kind":"workflow.restart"})
        )
        .await,
        0
    );
    tx.commit().await.unwrap();
}

enum ReceiptFault {
    Sqlite(std::path::PathBuf),
    Postgres(String),
}

impl ReceiptFault {
    async fn install(&self) {
        match self {
            Self::Sqlite(path) => {
                assert!(path.is_file(), "fault target must be the existing attached app database");
                let connection = rusqlite::Connection::open(path).unwrap();
                assert!(connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='__zeroship_workflow_management_receipts')",
                    [], |row| row.get::<_, bool>(0),
                ).unwrap());
                connection.execute_batch(
                    "CREATE TRIGGER management_receipt_fault BEFORE UPDATE OF outcome ON __zeroship_workflow_job_receipts
                     BEGIN SELECT RAISE(ABORT,'management receipt fault'); END;",
                ).unwrap();
            },
            Self::Postgres(url) => connect(url).await.batch_execute(
                "CREATE FUNCTION customer.management_receipt_fault() RETURNS trigger LANGUAGE plpgsql AS $$
                 BEGIN RAISE EXCEPTION 'management receipt fault'; END $$;
                 CREATE TRIGGER management_receipt_fault BEFORE UPDATE OF outcome ON customer.__zeroship_workflow_job_receipts
                 FOR EACH ROW EXECUTE FUNCTION customer.management_receipt_fault();",
            ).await.unwrap(),
        }
    }

    async fn remove(&self) {
        match self {
            Self::Sqlite(path) => rusqlite::Connection::open(path).unwrap()
                .execute_batch("DROP TRIGGER management_receipt_fault").unwrap(),
            Self::Postgres(url) => connect(url).await.batch_execute(
                "DROP TRIGGER management_receipt_fault ON customer.__zeroship_workflow_job_receipts;
                 DROP FUNCTION customer.management_receipt_fault();",
            ).await.unwrap(),
        }
    }
}

#[derive(Clone, Copy)]
enum BarrierSite {
    AppLock,
    Receipt,
    Lifecycle,
}

struct PgBarrier {
    blocker: compio_postgres::Client,
    observer: compio_postgres::Client,
    blocker_pid: i32,
    site: BarrierSite,
}

impl PgBarrier {
    async fn install(url: &str, site: BarrierSite) -> Self {
        let blocker = connect(url).await;
        blocker.batch_execute(
            "CREATE FUNCTION customer.management_barrier() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN PERFORM pg_advisory_xact_lock(73921861); RETURN NEW; END $$;",
        ).await.unwrap();
        blocker.batch_execute(match site {
            BarrierSite::AppLock => "CREATE TRIGGER management_barrier BEFORE UPDATE ON customer.__zeroship_workflow_app_state FOR EACH ROW EXECUTE FUNCTION customer.management_barrier()",
            BarrierSite::Receipt => "CREATE TRIGGER management_barrier BEFORE UPDATE OF outcome ON customer.__zeroship_workflow_job_receipts FOR EACH ROW EXECUTE FUNCTION customer.management_barrier()",
            BarrierSite::Lifecycle => "CREATE TRIGGER management_barrier BEFORE UPDATE ON customer.__zeroship_workflow_runs FOR EACH ROW WHEN (OLD.generation IS DISTINCT FROM NEW.generation) EXECUTE FUNCTION customer.management_barrier()",
        }).await.unwrap();
        let blocker_pid = blocker
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        blocker
            .query_one("SELECT pg_advisory_lock(73921861)", &[])
            .await
            .unwrap();
        Self {
            blocker,
            observer: connect(url).await,
            blocker_pid,
            site,
        }
    }

    async fn blocked_worker(&self) -> i32 {
        compio::time::timeout(Duration::from_secs(10), async {
            loop {
                let waiting = self.observer.query(
                    "SELECT pid FROM pg_stat_activity WHERE usename='customer_worker' AND wait_event_type='Lock' AND $1=ANY(pg_blocking_pids(pid))",
                    &[&self.blocker_pid],
                ).await.unwrap();
                if let Some(worker) = waiting.first() {
                    assert_eq!(waiting.len(), 1);
                    return worker.get(0);
                }
                compio::time::sleep(Duration::from_millis(1)).await;
            }
        }).await.expect("management must reach the selected database barrier")
    }

    async fn wait_for_rollback(&self, worker: i32) {
        let rollback_budget = Duration::from_secs(2);
        assert!(
            rollback_budget.as_millis()
                < u128::from(zeroship_data_orm::budgets::DB_LOCK_TIMEOUT_MS),
            "server lock timeout must not satisfy the cancellation oracle"
        );
        compio::time::timeout(rollback_budget, async {
            loop {
                let active: bool = self.observer.query_one(
                    "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND xact_start IS NOT NULL)",
                    &[&worker],
                ).await.unwrap().get(0);
                if !active { return; }
                compio::time::sleep(Duration::from_millis(1)).await;
            }
        }).await.expect("cancelled management must release its transaction while the barrier is still held");
    }

    async fn release(&self) {
        assert!(self
            .blocker
            .query_one("SELECT pg_advisory_unlock(73921861)", &[])
            .await
            .unwrap()
            .get::<_, bool>(0));
    }

    async fn remove(self) {
        self.release().await;
        self.remove_trigger().await;
    }

    async fn remove_trigger(self) {
        self.blocker
            .batch_execute(match self.site {
                BarrierSite::AppLock => {
                    "DROP TRIGGER management_barrier ON customer.__zeroship_workflow_app_state"
                }
                BarrierSite::Receipt => {
                    "DROP TRIGGER management_barrier ON customer.__zeroship_workflow_job_receipts"
                }
                BarrierSite::Lifecycle => {
                    "DROP TRIGGER management_barrier ON customer.__zeroship_workflow_runs"
                }
            })
            .await
            .unwrap();
        self.blocker
            .batch_execute("DROP FUNCTION customer.management_barrier()")
            .await
            .unwrap();
    }
}

async fn cancellation_contract(site: BarrierSite) {
    let fixture = PostgresFixture::start().await;
    let (service, local, _, _deployments) =
        registered_service(Rc::new(fixture.store.clone())).await;
    let run = start(&service, &local).await;
    let scope = service.fixture_app(local.clone());
    let request = started(&local, &run, 1);
    let barrier = PgBarrier::install(&fixture.admin_url, site).await;
    let (worker, pending) = match select(
        barrier.blocked_worker().boxed_local(),
        scope.management_outcome(&request).boxed_local(),
    )
    .await
    {
        Either::Left(result) => result,
        Either::Right((result, _)) => {
            panic!("management ended before the cancellation barrier: {result:?}")
        }
    };
    drop(pending);
    barrier.wait_for_rollback(worker).await;
    barrier.remove().await;
    assert_rolled_back(&service, &local, &run, &request).await;
    let outcome = scope.management_outcome(&request).await.unwrap();
    assert_eq!(outcome, restarted(&active(&service, &local).await.id));
    assert_eq!(scope.management_outcome(&request).await.unwrap(), outcome);
    assert_eq!(head(&service, &local, &run).await, (1, "queued".into()));
    assert_eq!(receipt_count(&service, &request).await, 1);
}

#[compio::test]
async fn postgres_management_revocation_during_app_lock_is_retryable() {
    let fixture = PostgresFixture::start().await;
    let (service, local, _, _deployments) =
        registered_service(Rc::new(fixture.store.clone())).await;
    let run = start(&service, &local).await;
    let scope = service.fixture_app(local.clone());
    let request = started(&local, &run, 1);
    let barrier = PgBarrier::install(&fixture.admin_url, BarrierSite::AppLock).await;
    let pending = match select(
        barrier.blocked_worker().boxed_local(),
        scope.management_outcome(&request).boxed_local(),
    )
    .await
    {
        Either::Left((_, pending)) => pending,
        Either::Right((result, _)) => panic!("management ended before its app lock: {result:?}"),
    };
    service
        .policies
        .fixture_install(
            &local,
            leased_policy(
                2,
                AppPolicy {
                    admission: false,
                    ..Default::default()
                },
            ),
        )
        .unwrap();
    barrier.release().await;
    assert!(matches!(
        pending.await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    barrier.remove_trigger().await;
    assert_rolled_back(&service, &local, &run, &request).await;
    assert_eq!(
        scope.management_outcome(&request).await.unwrap(),
        ManagementOutcome::Denied {}
    );
    assert_eq!(receipt_count(&service, &request).await, 1);
    service
        .policies
        .fixture_install(&local, leased_policy(3, AppPolicy::default()))
        .unwrap();
    assert_eq!(
        scope.management_outcome(&request).await.unwrap(),
        ManagementOutcome::Denied {}
    );
    assert_eq!(
        scope
            .management_outcome(&started(&local, &run, 2))
            .await
            .unwrap(),
        restarted(&active(&service, &local).await.id)
    );
}

#[compio::test]
async fn postgres_management_app_lock_wait_keeps_original_policy_deadline() {
    let fixture = PostgresFixture::start().await;
    let (service, local, _, _deployments) =
        registered_service(Rc::new(fixture.store.clone())).await;
    let run = start(&service, &local).await;
    let scope = service.fixture_app(local.clone());
    let request = started(&local, &run, 1);
    let barrier = PgBarrier::install(&fixture.admin_url, BarrierSite::AppLock).await;
    let deadline = Instant::now() + Duration::from_secs(3);
    service
        .policies
        .fixture_install(
            &local,
            PolicySnapshot::lease(2.try_into().unwrap(), AppPolicy::default(), deadline).unwrap(),
        )
        .unwrap();
    let (worker, pending) = match select(
        barrier.blocked_worker().boxed_local(),
        scope.management_outcome(&request).boxed_local(),
    )
    .await
    {
        Either::Left(result) => result,
        Either::Right((result, _)) => panic!("management ended before its app lock: {result:?}"),
    };
    service
        .policies
        .fixture_install(
            &local,
            PolicySnapshot::lease(
                2.try_into().unwrap(),
                AppPolicy::default(),
                deadline + Duration::from_secs(30),
            )
            .unwrap(),
        )
        .unwrap();
    let budget = Duration::from_secs(5);
    assert!(budget.as_millis() < u128::from(zeroship_data_orm::budgets::DB_LOCK_TIMEOUT_MS));
    let result = compio::time::timeout(budget, pending)
        .await
        .expect("the original policy deadline must cancel app-lock waiting");
    assert!(
        matches!(result, Err(WorkflowServiceError::Unavailable(_))),
        "{result:?}"
    );
    assert!(service.policies.fixture_authority(&local).is_ok());
    barrier.wait_for_rollback(worker).await;
    barrier.remove().await;
    assert_rolled_back(&service, &local, &run, &request).await;
    assert_eq!(
        scope.management_outcome(&request).await.unwrap(),
        restarted(&active(&service, &local).await.id)
    );
    assert_eq!(receipt_count(&service, &request).await, 1);
}

#[compio::test]
async fn sqlite_management_captures_policy_before_waiting_for_journal() {
    let directory = tempfile::tempdir().unwrap();
    let store = sqlite_store(&directory.path().join("zs-workflow.sqlite")).await;
    waiting_authority_contract(Rc::new(store)).await;
}

#[compio::test]
async fn postgres_management_captures_policy_before_waiting_for_journal() {
    let fixture = PostgresFixture::start().await;
    waiting_authority_contract(Rc::new(fixture.store.clone())).await;
}

#[expect(
    clippy::future_not_send,
    reason = "native journal waits stay on the owning compio thread"
)]
async fn waiting_authority_contract(store: Rc<OrmStore>) {
    let (service, local, _, _deployments) = registered_service(store).await;
    let run = start(&service, &local).await;
    let scope = service.fixture_app(local.clone());
    let request = started(&local, &run, 1);
    let mut blocker = service.begin().await.unwrap();
    app::lock_app(&mut blocker, &local).await.unwrap();
    let mut pending = scope.management_outcome(&request).boxed_local();
    assert!(matches!(
        futures::poll!(&mut pending),
        std::task::Poll::Pending
    ));
    service
        .policies
        .fixture_install(
            &local,
            leased_policy(
                2,
                AppPolicy {
                    admission: false,
                    ..Default::default()
                },
            ),
        )
        .unwrap();
    blocker.commit().await.unwrap();
    assert!(matches!(
        pending.await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_rolled_back(&service, &local, &run, &request).await;

    service
        .policies
        .fixture_install(&local, leased_policy(3, AppPolicy::default()))
        .unwrap();
    let mut blocker = service.begin().await.unwrap();
    app::lock_app(&mut blocker, &local).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    service
        .policies
        .fixture_install(
            &local,
            PolicySnapshot::lease(4.try_into().unwrap(), AppPolicy::default(), deadline).unwrap(),
        )
        .unwrap();
    let mut pending = scope.management_outcome(&request).boxed_local();
    assert!(matches!(
        futures::poll!(&mut pending),
        std::task::Poll::Pending
    ));
    service
        .policies
        .fixture_install(
            &local,
            PolicySnapshot::lease(
                4.try_into().unwrap(),
                AppPolicy::default(),
                deadline + Duration::from_secs(30),
            )
            .unwrap(),
        )
        .unwrap();
    let budget = Duration::from_secs(3);
    assert!(budget.as_millis() < u128::from(zeroship_data_orm::budgets::DB_LOCK_TIMEOUT_MS));
    let result = compio::time::timeout(budget, pending)
        .await
        .expect("management must keep its original deadline before the journal opens");
    assert!(
        matches!(result, Err(WorkflowServiceError::Unavailable(_))),
        "{result:?}"
    );
    assert!(service.policies.fixture_authority(&local).is_ok());
    blocker.commit().await.unwrap();
    assert_rolled_back(&service, &local, &run, &request).await;
    assert_eq!(
        scope.management_outcome(&request).await.unwrap(),
        restarted(&active(&service, &local).await.id)
    );
    assert_eq!(receipt_count(&service, &request).await, 1);
}
