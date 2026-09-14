//! The creator ingress fence and delivered closure evidence.

use super::*;
use crate::{
    operations::{SignalOptions, StartOptions},
    service::publication::JobPublisher,
};
use std::time::Duration;
use zeroship_core::{
    workflow_coordination::Revision,
    workflow_jobs::{JobId, JobOperation, JobOutcome, JobSpec},
};

fn epoch(value: i64) -> Revision {
    value.try_into().unwrap()
}

fn close(app: &AppId, value: i64) -> JobSpec {
    JobSpec {
        id: JobId::mint(),
        app_id: app.clone(),
        operation: JobOperation::Close {
            epoch: epoch(value),
        },
        available_at: 0.try_into().unwrap(),
    }
}

/// Install the app's policy with the ingress epoch the manager issued with it.
fn install(
    service: &WorkflowService,
    app: &AppId,
    revision: i64,
    policy: AppPolicy,
    ingress: Option<i64>,
) {
    service
        .policies
        .fixture_install(
            app,
            leased_policy(revision, policy).with_ingress_epoch(ingress.map(epoch)),
        )
        .unwrap();
}

async fn closed(service: &WorkflowService, app: &AppId) -> i64 {
    let tx = service.begin_history().await.unwrap();
    let closed = crate::service::app::closed_epoch(&tx, app).await.unwrap();
    tx.commit().await.unwrap();
    closed
}

fn fenced<T: std::fmt::Debug>(result: Result<T, WorkflowServiceError>, after: Option<i64>) {
    assert_eq!(
        result.unwrap_err(),
        WorkflowServiceError::IngressFenced(after.map(epoch))
    );
}

fn approved() -> SignalOptions {
    SignalOptions {
        signal_type: "approved".into(),
        payload: json!(true),
    }
}

/// The manager's queue accepted every publication.
struct Accepting(AppId);
impl JobPublisher for Accepting {
    fn app_id(&self) -> &AppId {
        &self.0
    }
    async fn submit(&self, job: &JobSpec) -> Result<JobSpec, WorkflowServiceError> {
        Ok(job.clone())
    }
}

/// Confirm every pending publication, as the worker's outbox would.
async fn confirm_all(scope: &super::super::AppWorkflows) {
    let publisher = Accepting(scope.app_id().clone());
    for job in scope.pending_jobs(None, 100).await.unwrap() {
        scope.publish_job(&job.id, &publisher).await.unwrap();
    }
    assert!(scope.pending_jobs(None, 1).await.unwrap().is_empty());
}

async fn drained(scope: &super::super::AppWorkflows, value: i64) -> bool {
    let job = close(scope.app_id(), value);
    let receipt = scope.close_job(&JobGrant::new(&job)).await.unwrap();
    assert_eq!(receipt.job, job);
    let JobOutcome::Closed { drained } = receipt.outcome else {
        panic!("closure settles with closed evidence: {receipt:?}");
    };
    drained
}

#[compio::test]
async fn sqlite_closed_epoch_fences_acceptance_under_a_still_valid_lease() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    Box::pin(fenced_acceptance(Rc::new(sqlite_store(&path).await))).await;
}

#[compio::test]
async fn postgres_closed_epoch_fences_acceptance_under_a_still_valid_lease() {
    let fixture = PostgresFixture::start().await;
    Box::pin(fenced_acceptance(Rc::new(fixture.store.clone()))).await;
}

/// A worker whose lease still carries epoch one is refused once Close(1)
/// commits, even though its policy has not expired. Epoch two is accepted.
async fn fenced_acceptance(store: Rc<OrmStore>) {
    let (service, app, other, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let accepted = RequestId::mint();
    let run = scope
        .start(&accepted, "Example", StartOptions::default())
        .await
        .unwrap();
    scope
        .signal(&RequestId::mint(), &run.id, approved())
        .await
        .unwrap();
    // The fence commits even though the accepted run's intent is unconfirmed.
    assert!(!drained(&scope, 1).await);
    assert_eq!(closed(&service, &app).await, 1);
    fenced(
        scope
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await,
        Some(1),
    );
    fenced(
        scope.signal(&RequestId::mint(), &run.id, approved()).await,
        Some(1),
    );
    // A committed acceptance still resolves through its request receipt.
    assert_eq!(
        scope
            .start(&accepted, "Example", StartOptions::default())
            .await
            .unwrap(),
        run
    );

    // Control: the manager's next epoch is accepted by the same binding.
    install(&service, &app, 1, AppPolicy::default(), Some(2));
    let second = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    scope
        .signal(&RequestId::mint(), &second.id, approved())
        .await
        .unwrap();

    // A snapshot without an epoch cannot accept; the refusal names the
    // journal's closed epoch, or nothing when the journal closed none.
    install(&service, &app, 1, AppPolicy::default(), None);
    fenced(
        scope
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await,
        Some(1),
    );
    install(&service, &other, 1, AppPolicy::default(), None);
    fenced(
        service
            .fixture_app(other.clone())
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await,
        None,
    );
    assert_eq!(closed(&service, &other).await, 0);
}

/// Wait until a creator session waits on a lock held by `blocker`.
async fn blocked_by(admin: &compio_postgres::Client, blocker: i32) -> i32 {
    compio::time::timeout(Duration::from_secs(10), async {
        loop {
            let row = admin
                .query_one(
                    "SELECT min(pid) FROM pg_stat_activity WHERE usename='customer_worker' \
                     AND wait_event_type='Lock' AND $1=ANY(pg_blocking_pids(pid))",
                    &[&blocker],
                )
                .await
                .unwrap();
            if let Some(pid) = row.get::<_, Option<i32>>(0) {
                return pid;
            }
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("creator operation reached the lock")
}

/// Acceptance and closure serialize on the app state row in both orders. The
/// closing worker is a second journal connection, as a second worker would be.
#[compio::test]
async fn postgres_acceptance_and_closure_serialize_on_the_app_state_row() {
    const GATE: i64 = 73_921_901;
    let fixture = PostgresFixture::start().await;
    let (service, app, _, _deployments) =
        registered_service(Rc::new(fixture.store.clone())).await;
    let closer = WorkflowService::open(
        Rc::new(
            orm_store(
                &fixture
                    .admin_url
                    .replacen("postgres@", "customer_worker@", 1),
                fixture.store.binding.schema().clone(),
            )
            .await,
        ),
        service.policies.clone(),
    )
    .await
    .unwrap();
    let admin = connect(&fixture.admin_url).await;
    let admin_pid: i32 = admin
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    admin
        .batch_execute(&format!(
            "CREATE FUNCTION customer.gate_closure() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN PERFORM pg_advisory_xact_lock({GATE}); RETURN NEW; END $$;"
        ))
        .await
        .unwrap();

    // Acceptance first: it holds the app state lock while its request receipt
    // waits on the gate, and Close waits behind it.
    admin
        .batch_execute(
            "CREATE TRIGGER gate_acceptance BEFORE INSERT ON customer.__zeroship_workflow_requests \
             FOR EACH ROW EXECUTE FUNCTION customer.gate_closure();",
        )
        .await
        .unwrap();
    admin
        .query_one(&format!("SELECT pg_advisory_lock({GATE})"), &[])
        .await
        .unwrap();
    let accepting = service.fixture_app(app.clone());
    let acceptance = compio::runtime::spawn(async move {
        accepting
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await
    });
    let accepting_pid = blocked_by(&admin, admin_pid).await;
    let closing = closer.fixture_app(app.clone());
    let first_close = close(&app, 1);
    let closure_job = first_close.clone();
    let closure =
        compio::runtime::spawn(async move { closing.close_job(&JobGrant::new(&closure_job)).await });
    blocked_by(&admin, accepting_pid).await;
    admin
        .query_one(&format!("SELECT pg_advisory_unlock({GATE})"), &[])
        .await
        .unwrap();
    let run = acceptance.await.unwrap().unwrap();
    let receipt = closure.await.unwrap().unwrap();
    assert_eq!(receipt.job, first_close);
    assert_eq!(
        receipt.outcome,
        JobOutcome::Closed { drained: false },
        "Close observed the acceptance that committed first"
    );
    assert_eq!(closed(&service, &app).await, 1);
    let scope = service.fixture_app(app.clone());
    fenced(
        scope.signal(&RequestId::mint(), &run.id, approved()).await,
        Some(1),
    );
    admin
        .batch_execute(
            "DROP TRIGGER gate_acceptance ON customer.__zeroship_workflow_requests;",
        )
        .await
        .unwrap();

    // Close first: it holds the app state lock while its receipt waits on the
    // gate, and the acceptance waits behind it and is then fenced.
    install(&service, &app, 1, AppPolicy::default(), Some(2));
    admin
        .batch_execute(
            "CREATE TRIGGER gate_receipt BEFORE INSERT ON customer.__zeroship_workflow_job_receipts \
             FOR EACH ROW EXECUTE FUNCTION customer.gate_closure();",
        )
        .await
        .unwrap();
    admin
        .query_one(&format!("SELECT pg_advisory_lock({GATE})"), &[])
        .await
        .unwrap();
    let closing = closer.fixture_app(app.clone());
    let second_close = close(&app, 2);
    let closure_job = second_close.clone();
    let closure =
        compio::runtime::spawn(async move { closing.close_job(&JobGrant::new(&closure_job)).await });
    let closing_pid = blocked_by(&admin, admin_pid).await;
    let accepting = service.fixture_app(app.clone());
    let acceptance = compio::runtime::spawn(async move {
        accepting
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await
    });
    blocked_by(&admin, closing_pid).await;
    admin
        .query_one(&format!("SELECT pg_advisory_unlock({GATE})"), &[])
        .await
        .unwrap();
    let receipt = closure.await.unwrap().unwrap();
    assert_eq!(receipt.job, second_close);
    fenced(acceptance.await.unwrap(), Some(2));
    assert_eq!(closed(&service, &app).await, 2);
    admin
        .batch_execute(
            "DROP TRIGGER gate_receipt ON customer.__zeroship_workflow_job_receipts; \
             DROP FUNCTION customer.gate_closure();",
        )
        .await
        .unwrap();
}

#[compio::test]
async fn sqlite_closure_reports_each_closed_drain_predicate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    Box::pin(drain_predicates(Rc::new(sqlite_store(&path).await))).await;
}

#[compio::test]
async fn postgres_closure_reports_each_closed_drain_predicate() {
    let fixture = PostgresFixture::start().await;
    Box::pin(drain_predicates(Rc::new(fixture.store.clone()))).await;
}

/// Each closed predicate alone makes the evidence undrained; its control
/// differs only in the one row that predicate reads.
async fn drain_predicates(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    // Each probe closes the current epoch; the manager's next epoch then covers
    // the following acceptance.
    let mut next = 0;
    let mut evidence = async || {
        next += 1;
        let drained = drained(&scope, next).await;
        install(&service, &app, 1, AppPolicy::default(), Some(next + 1));
        drained
    };
    assert!(evidence().await, "an activated app with no work is drained");

    // An unconfirmed publication intent.
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    assert!(!evidence().await);
    confirm_all(&scope).await;
    assert!(evidence().await);

    let app_id = app.as_str();
    let task = typed_id::new_workflow_dispatch_id();
    let payload = typed_id::generate(typed_id::WORKFLOW_PAYLOAD_PREFIX);
    let far = i64::MAX / 2;
    let hold = storage_id();
    let tx = service.begin().await.unwrap();
    journal_insert(
        &tx,
        "deployment_holds",
        json!({"id":hold, "app_id":app_id, "deploy_id":typed_id::generate("dep"),
            "holder_id":"closure-test", "generation":1, "state":"held"}),
    )
    .await
    .unwrap();
    journal_insert(
        &tx,
        "tasks",
        json!({"id":task, "app_id":app_id, "run_id":run.id, "generation":0,
            "worker":"closure-test", "epoch":999, "token_hash":"closure-test",
            "deadline":0, "state":"expired", "created_at":0, "frontier_revision":1}),
    )
    .await
    .unwrap();
    journal_insert(
        &tx,
        "payloads",
        json!({"id":payload, "app_id":app_id, "run_id":run.id, "generation":0,
            "task_id":task, "request_id":"closure-test", "hash":"0".repeat(64), "size":1,
            "state":"referenced", "created_at":0, "expires_at":far}),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(evidence().await, "expired claims and referenced payloads are drained");

    let set = async |table: &str, id: &str, patch: serde_json::Value| {
        let tx = service.begin().await.unwrap();
        journal_update(&tx, table, json!({"app_id":app_id, "id":id}), patch).await;
        tx.commit().await.unwrap();
    };
    // A hold in transition.
    for state in ["acquiring", "releasing"] {
        set("deployment_holds", &hold, json!({"state":state})).await;
        assert!(!evidence().await, "{state} hold");
    }
    set("deployment_holds", &hold, json!({"state":"held"})).await;
    assert!(evidence().await);

    // A live task claim.
    set("tasks", &task, json!({"state":"leased", "deadline":far})).await;
    assert!(!evidence().await, "live claim");
    set("tasks", &task, json!({"deadline":0})).await;
    assert!(evidence().await, "a lapsed claim is not live");
    set("tasks", &task, json!({"state":"completed"})).await;

    // A payload in preparation or deletion, and a tombstone in its window.
    for state in ["uploading", "staged", "deleting", "deleted"] {
        set("payloads", &payload, json!({"state":state, "expires_at":far})).await;
        assert!(!evidence().await, "{state} payload");
    }
    set("payloads", &payload, json!({"state":"deleted", "expires_at":0})).await;
    assert!(evidence().await, "a tombstone past its resweep window");
    assert_eq!(closed(&service, &app).await, next);
}

#[compio::test]
async fn sqlite_closure_runs_under_archived_policy_and_replays_its_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    Box::pin(archived_and_replayed(Rc::new(sqlite_store(&path).await))).await;
}

#[compio::test]
async fn postgres_closure_runs_under_archived_policy_and_replays_its_receipt() {
    let fixture = PostgresFixture::start().await;
    Box::pin(archived_and_replayed(Rc::new(fixture.store.clone()))).await;
}

/// Closure is journal-only: archive masks admission, dispatch and ingress, yet
/// Close is delivered and drains. Its committed receipt replays for a lost
/// acknowledgement and for redelivery to another worker, without re-evaluating
/// evidence and without live authority.
async fn archived_and_replayed(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.fixture_app(app.clone());
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let archive = AppPolicy {
        admission: false,
        dispatch: false,
        ingress: false,
        ..AppPolicy::default()
    };
    install(&service, &app, 2, archive.clone(), Some(1));
    assert_eq!(
        scope
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await
            .unwrap_err(),
        WorkflowServiceError::PermissionDenied,
        "archive refuses admission while the epoch is open"
    );
    // Signals to existing runs carry no admission check; the open epoch covers them.
    scope
        .signal(&RequestId::mint(), &run.id, approved())
        .await
        .unwrap();
    confirm_all(&scope).await;
    let job = close(&app, 1);
    let grant = JobGrant::new(&job);
    let receipt = scope.close_job(&grant).await.unwrap();
    assert_eq!(receipt.outcome, JobOutcome::Closed { drained: true });
    assert_eq!(closed(&service, &app).await, 1);
    assert_eq!(
        scope
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await
            .unwrap_err(),
        WorkflowServiceError::PermissionDenied
    );
    fenced(
        scope.signal(&RequestId::mint(), &run.id, approved()).await,
        Some(1),
    );

    // Work accepted under a later epoch cannot change the committed evidence.
    install(&service, &app, 3, AppPolicy::default(), Some(2));
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    assert_eq!(scope.close_job(&grant.retry()).await.unwrap(), receipt);
    let mut redelivered = grant.retry();
    redelivered.delivery.worker_id = zeroship_core::workflow_coordination::WorkerId::mint();
    assert_eq!(scope.close_job(&redelivered).await.unwrap(), receipt);
    assert_eq!(scope.job_receipt(&job).await.unwrap(), Some(receipt.clone()));
    // A stale closure keeps the newer fence and still reports current evidence.
    assert!(!drained(&scope, 1).await);
    assert_eq!(closed(&service, &app).await, 1);
    assert!(!drained(&scope, 2).await);
    assert!(!drained(&scope, 1).await);
    assert_eq!(closed(&service, &app).await, 2);

    // Replay needs no live authority; a fresh closure does.
    service
        .policies
        .fixture_binding(&app)
        .unwrap()
        .revoke()
        .unwrap();
    assert_eq!(scope.close_job(&grant.retry()).await.unwrap(), receipt);
    assert!(scope
        .close_job(&JobGrant::new(&close(&app, 3)))
        .await
        .is_err());
    assert_eq!(closed(&service, &app).await, 2);

    // Foreign and non-closure jobs are refused before journal I/O.
    let foreign = close(&AppId::mint(), 1);
    assert_eq!(
        scope.close_job(&JobGrant::new(&foreign)).await.unwrap_err(),
        WorkflowServiceError::PermissionDenied
    );
    let reconcile = JobSpec {
        operation: JobOperation::Reconcile {},
        ..close(&app, 1)
    };
    assert!(matches!(
        scope.close_job(&JobGrant::new(&reconcile)).await,
        Err(WorkflowServiceError::InvalidRequest(_))
    ));
}
