use super::*;
use crate::service::{app, frontier};
use futures::{
    future::{select, Either},
    stream::FuturesUnordered,
    FutureExt, StreamExt,
};
use std::time::{Duration, Instant};
use zeroship_core::workflow_coordination::{
    ManageRun, ManagementOperation, ManagementOutcome, RestartOptions, RestartTarget, RunId,
    RunOperation, RunState,
};
use zeroship_data_orm::{orm::Operation, value};

mod atomic_application;

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
async fn postgres_management_receipt_wait_cannot_outlive_host_authority() {
    let fixture = PostgresFixture::start().await;
    let (service, local, _, _deployments) =
        registered_service(Rc::new(fixture.store.clone())).await;
    let run = start(&service, &local).await;
    let scope = service.fixture_app(local.clone());
    let request = restart(&local, &run);
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
        scope.apply_management(&request).boxed_local(),
    )
    .await
    {
        Either::Left(result) => result,
        Either::Right((result, _)) => {
            panic!("management ended before the receipt barrier: {result:?}")
        }
    };
    let result = compio::time::timeout(Duration::from_secs(10), pending)
        .await
        .expect("host authority expiry must cancel its blocked receipt write");
    assert!(
        matches!(result, Err(WorkflowServiceError::Unavailable(_))),
        "{result:?}"
    );
    barrier.wait_for_rollback(worker).await;
    barrier.remove().await;
    assert_rolled_back(&service, &local, &run, &request).await;
    service
        .fixture_register(&local, leased_policy(3, AppPolicy::default()))
        .await
        .unwrap();
    assert_eq!(
        scope.apply_management(&request).await.unwrap(),
        ManagementOutcome::Applied {
            state: RunState::Queued
        }
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
    let request = restart(&local, &run);
    let admin = connect(&fixture.admin_url).await;
    admin.batch_execute("REVOKE INSERT ON customer.__zeroship_workflow_management_receipts FROM app_customer_role")
        .await.unwrap();
    let result = scope.apply_management(&request).await;
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
        scope.apply_management(&request).await.unwrap(),
        ManagementOutcome::Applied {
            state: RunState::Queued
        }
    );
    assert_eq!(receipt_count(&service, &request).await, 1);
    assert_eq!(head(&service, &local, &run).await, (1, "queued".into()));
}

fn command(app: &AppId, run: &str, operation: ManagementOperation) -> ManageRun {
    ManageRun {
        request_id: RequestId::mint(),
        app_id: app.clone(),
        run_id: RunId::parse(run).unwrap(),
        command: operation,
    }
}

fn restart(app: &AppId, run: &str) -> ManageRun {
    command(
        app,
        run,
        ManagementOperation::Restart {
            options: RestartOptions::default(),
        },
    )
}

fn transition(app: &AppId, run: &str, operation: RunOperation) -> ManageRun {
    command(app, run, ManagementOperation::Transition { operation })
}

async fn start(service: &WorkflowService, app: &AppId) -> String {
    service
        .fixture_app(app.clone())
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap()
        .id
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

async fn receipt_count(service: &WorkflowService, command: &ManageRun) -> i64 {
    let tx = service.begin().await.unwrap();
    let count = journal_count(
        &tx,
        "management_receipts",
        json!({
            "app_id":command.app_id.as_str(), "request_id":command.request_id.as_str(),
        }),
    )
    .await;
    tx.commit().await.unwrap();
    count
}

async fn replay_contract(store: Rc<OrmStore>) {
    let (service, local, foreign, deployments) = registered_service(store.clone()).await;
    let run = start(&service, &local).await;
    let scope = service.fixture_app(local.clone());
    let request = restart(&local, &run);
    let original = scope.apply_management(&request).await.unwrap();
    assert_eq!(
        original,
        ManagementOutcome::Applied {
            state: RunState::Queued
        }
    );
    assert_eq!(head(&service, &local, &run).await, (1, "queued".into()));
    assert_eq!(receipt_count(&service, &request).await, 1);

    let mut mismatch = request.clone();
    mismatch.command = ManagementOperation::Transition {
        operation: RunOperation::Pause,
    };
    assert!(matches!(
        scope.apply_management(&mismatch).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    mismatch = request.clone();
    mismatch.run_id = RunId::mint();
    assert!(matches!(
        scope.apply_management(&mismatch).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    mismatch = request.clone();
    mismatch.app_id = foreign.clone();
    assert_eq!(
        scope.apply_management(&mismatch).await,
        Err(WorkflowServiceError::PermissionDenied)
    );
    assert_eq!(
        service
            .fixture_app(foreign.clone())
            .apply_management(&request)
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
    unconfigured_replay(&service, &request, original).await;
    for app_id in [&local, &foreign] {
        service
            .fixture_register(app_id, leased_policy(2, AppPolicy::default()))
            .await
            .unwrap();
    }
    let scope = service.fixture_app(local.clone());
    assert_eq!(scope.apply_management(&request).await.unwrap(), original);
    assert_eq!(head(&service, &local, &run).await, (1, "queued".into()));
    scope
        .transition(&RequestId::mint(), &run, RunOperation::Pause)
        .await
        .unwrap();
    assert_eq!(scope.apply_management(&request).await.unwrap(), original);
    assert_eq!(head(&service, &local, &run).await, (1, "paused".into()));
    assert_eq!(receipt_count(&service, &request).await, 1);

    let concurrent_run = start(&service, &local).await;
    let concurrent = restart(&local, &concurrent_run);
    let mut requests = FuturesUnordered::new();
    for _ in 0..4 {
        requests.push(scope.apply_management(&concurrent));
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
    request: &ManageRun,
    original: ManagementOutcome,
) {
    let local = &request.app_id;
    let unconfigured = service.fixture_app(local.clone());
    assert_eq!(
        unconfigured.apply_management(request).await.unwrap(),
        original
    );
    let mut changed = request.clone();
    changed.command = ManagementOperation::Transition {
        operation: RunOperation::Pause,
    };
    assert!(matches!(
        unconfigured.apply_management(&changed).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    let fresh = restart(local, request.run_id.as_str());
    assert!(matches!(
        unconfigured.apply_management(&fresh).await,
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
        unconfigured.apply_management(request).await.unwrap(),
        original
    );
    assert!(matches!(
        unconfigured.apply_management(&fresh).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_eq!(receipt_count(service, &fresh).await, 0);
}

async fn outcome_contract(store: Rc<OrmStore>) {
    let (service, local, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(local.clone());
    let absent = RunId::mint();
    let missing = restart(&local, absent.as_str());
    assert_eq!(
        scope.apply_management(&missing).await.unwrap(),
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
        absent.as_str(),
        "Example",
        &deployment.id,
        &StartOptions::default(),
        now,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        scope.apply_management(&missing).await.unwrap(),
        ManagementOutcome::NotFound {}
    );
    assert_eq!(
        head(&service, &local, absent.as_str()).await,
        (0, "queued".into())
    );

    let run = start(&service, &local).await;
    let invalid = command(
        &local,
        &run,
        ManagementOperation::Restart {
            options: RestartOptions {
                from: Some(RestartTarget {
                    name: String::new(),
                    occurrence: None,
                }),
                ..Default::default()
            },
        },
    );
    assert_eq!(
        scope.apply_management(&invalid).await.unwrap(),
        ManagementOutcome::Conflict {}
    );
    assert_eq!(receipt_count(&service, &invalid).await, 1);
    assert_eq!(head(&service, &local, &run).await, (0, "queued".into()));

    finish(&service, &local, &run).await;
    let conflict = transition(&local, &run, RunOperation::Pause);
    assert_eq!(
        scope.apply_management(&conflict).await.unwrap(),
        ManagementOutcome::Conflict {}
    );
    scope
        .restart(&RequestId::mint(), &run, RestartOptions::default())
        .await
        .unwrap();
    assert_eq!(
        scope.apply_management(&conflict).await.unwrap(),
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
    let denied = restart(&local, &run);
    assert_eq!(
        scope.apply_management(&denied).await.unwrap(),
        ManagementOutcome::Denied {}
    );
    service
        .fixture_register(&local, leased_policy(3, AppPolicy::default()))
        .await
        .unwrap();
    assert_eq!(
        scope.apply_management(&denied).await.unwrap(),
        ManagementOutcome::Denied {}
    );
    assert_eq!(head(&service, &local, &run).await, (1, "queued".into()));
    assert_eq!(receipt_count(&service, &denied).await, 1);
    let accepted = restart(&local, &run);
    assert_eq!(
        scope.apply_management(&accepted).await.unwrap(),
        ManagementOutcome::Applied {
            state: RunState::Queued
        }
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
    let capacity = restart(&local, &run);
    assert!(matches!(
        scope.apply_management(&capacity).await,
        Err(WorkflowServiceError::ResourceExhausted(_))
    ));
    assert_eq!(receipt_count(&service, &capacity).await, 0);
    service
        .fixture_register(&local, leased_policy(5, AppPolicy::default()))
        .await
        .unwrap();
    assert_eq!(
        scope.apply_management(&capacity).await.unwrap(),
        ManagementOutcome::Applied {
            state: RunState::Queued
        }
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
        scope.apply_management(&capacity).await.unwrap(),
        ManagementOutcome::Applied {
            state: RunState::Queued
        }
    );
    let expired = transition(&local, &run, RunOperation::Pause);
    assert!(matches!(
        scope.apply_management(&expired).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_eq!(receipt_count(&service, &expired).await, 0);
    assert_eq!(head(&service, &local, &run).await, (3, "queued".into()));
    service
        .fixture_register(&local, leased_policy(7, AppPolicy::default()))
        .await
        .unwrap();
    assert_eq!(
        scope.apply_management(&expired).await.unwrap(),
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
    frontier::finish(&mut tx, app_id, &run, RunState::Completed, None, None, now)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

async fn atomicity_contract(store: Rc<OrmStore>, fault: ReceiptFault) {
    let (service, local, _, _deployments) = registered_service(store).await;
    let run = start(&service, &local).await;
    let scope = service.fixture_app(local.clone());
    let request = restart(&local, &run);
    fault.install().await;
    assert!(matches!(
        scope.apply_management(&request).await,
        Err(WorkflowServiceError::Internal(_) | WorkflowServiceError::Unavailable(_))
    ));
    assert_rolled_back(&service, &local, &run, &request).await;
    fault.remove().await;
    let outcome = scope.apply_management(&request).await.unwrap();
    assert_eq!(
        outcome,
        ManagementOutcome::Applied {
            state: RunState::Queued
        }
    );
    assert_eq!(scope.apply_management(&request).await.unwrap(), outcome);
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

async fn assert_rolled_back(
    service: &WorkflowService,
    local: &AppId,
    run: &str,
    request: &ManageRun,
) {
    assert_eq!(head(service, local, run).await, (0, "queued".into()));
    assert_eq!(receipt_count(service, request).await, 0);
    let tx = service.begin().await.unwrap();
    assert_eq!(
        journal_count(
            &tx,
            "generations",
            json!({"app_id":local.as_str(), "run_id":run, "generation":1})
        )
        .await,
        0
    );
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
                    "CREATE TRIGGER management_receipt_fault BEFORE INSERT ON __zeroship_workflow_management_receipts
                     BEGIN SELECT RAISE(ABORT,'management receipt fault'); END;",
                ).unwrap();
            },
            Self::Postgres(url) => connect(url).await.batch_execute(
                "CREATE FUNCTION customer.management_receipt_fault() RETURNS trigger LANGUAGE plpgsql AS $$
                 BEGIN RAISE EXCEPTION 'management receipt fault'; END $$;
                 CREATE TRIGGER management_receipt_fault BEFORE INSERT ON customer.__zeroship_workflow_management_receipts
                 FOR EACH ROW EXECUTE FUNCTION customer.management_receipt_fault();",
            ).await.unwrap(),
        }
    }

    async fn remove(&self) {
        match self {
            Self::Sqlite(path) => rusqlite::Connection::open(path).unwrap()
                .execute_batch("DROP TRIGGER management_receipt_fault").unwrap(),
            Self::Postgres(url) => connect(url).await.batch_execute(
                "DROP TRIGGER management_receipt_fault ON customer.__zeroship_workflow_management_receipts;
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
            BarrierSite::Receipt => "CREATE TRIGGER management_barrier BEFORE INSERT ON customer.__zeroship_workflow_management_receipts FOR EACH ROW EXECUTE FUNCTION customer.management_barrier()",
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
        self.blocker.batch_execute(match self.site {
            BarrierSite::AppLock => "DROP TRIGGER management_barrier ON customer.__zeroship_workflow_app_state",
            BarrierSite::Receipt => "DROP TRIGGER management_barrier ON customer.__zeroship_workflow_management_receipts",
            BarrierSite::Lifecycle => "DROP TRIGGER management_barrier ON customer.__zeroship_workflow_runs",
        }).await.unwrap();
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
    let request = restart(&local, &run);
    let barrier = PgBarrier::install(&fixture.admin_url, site).await;
    let (worker, pending) = match select(
        barrier.blocked_worker().boxed_local(),
        scope.apply_management(&request).boxed_local(),
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
    let outcome = scope.apply_management(&request).await.unwrap();
    assert_eq!(
        outcome,
        ManagementOutcome::Applied {
            state: RunState::Queued
        }
    );
    assert_eq!(scope.apply_management(&request).await.unwrap(), outcome);
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
    let request = restart(&local, &run);
    let barrier = PgBarrier::install(&fixture.admin_url, BarrierSite::AppLock).await;
    let pending = match select(
        barrier.blocked_worker().boxed_local(),
        scope.apply_management(&request).boxed_local(),
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
        scope.apply_management(&request).await.unwrap(),
        ManagementOutcome::Denied {}
    );
    assert_eq!(receipt_count(&service, &request).await, 1);
    service
        .policies
        .fixture_install(&local, leased_policy(3, AppPolicy::default()))
        .unwrap();
    assert_eq!(
        scope.apply_management(&request).await.unwrap(),
        ManagementOutcome::Denied {}
    );
    assert_eq!(
        scope
            .apply_management(&restart(&local, &run))
            .await
            .unwrap(),
        ManagementOutcome::Applied {
            state: RunState::Queued
        }
    );
}

#[compio::test]
async fn postgres_management_app_lock_wait_keeps_original_policy_deadline() {
    let fixture = PostgresFixture::start().await;
    let (service, local, _, _deployments) =
        registered_service(Rc::new(fixture.store.clone())).await;
    let run = start(&service, &local).await;
    let scope = service.fixture_app(local.clone());
    let request = restart(&local, &run);
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
        scope.apply_management(&request).boxed_local(),
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
        scope.apply_management(&request).await.unwrap(),
        ManagementOutcome::Applied {
            state: RunState::Queued
        }
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
    let request = restart(&local, &run);
    let mut blocker = service.begin().await.unwrap();
    app::lock_app(&mut blocker, &local).await.unwrap();
    let mut pending = scope.apply_management(&request).boxed_local();
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
    let mut pending = scope.apply_management(&request).boxed_local();
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
        scope.apply_management(&request).await.unwrap(),
        ManagementOutcome::Applied {
            state: RunState::Queued
        }
    );
    assert_eq!(receipt_count(&service, &request).await, 1);
}
