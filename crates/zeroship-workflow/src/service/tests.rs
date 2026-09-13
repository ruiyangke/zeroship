use super::{
    schema,
    store::{OrmStore, Transaction},
};
use super::{
    AppPolicy, DeployRegistration, HostPolicies, PolicySnapshot, RequestId, WorkflowService,
};
use crate::operations::{ConflictPolicy, SignalOptions, StartOptions};
use crate::WorkflowServiceError;
use compio_postgres::NoTls;
use serde_json::json;
use std::{path::Path, process::Command, rc::Rc, sync::Arc};
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};
use zeroship_core::{app_id::AppId, typed_id};

fn configured_policy(revision: i64, policy: AppPolicy) -> PolicySnapshot {
    PolicySnapshot::configuration(revision.try_into().unwrap(), policy).unwrap()
}

mod activation;
mod background_scope;
mod delivery;
#[path = "../../../../tests/fixtures/workflow_deployments.rs"]
pub(super) mod deployment_fixture;
mod deployment_retention;
mod deployments;
mod frontier_models;
mod graph;
mod ingress_models;
mod journal_models;
mod management;
mod orm;
mod output_reads;
mod output_writes;
mod payload_models;
mod payloads;
mod policy;
pub(super) mod publication;
mod reconciliation;
mod requests;
mod restart_models;
mod runner;
#[path = "../../../../tests/fixtures/s3.rs"]
mod s3_fixture;
mod schedule_models;
mod schema_binding;
mod schema_metadata;
mod signal_models;
mod task_models;
mod topic_initialization;
mod worker;
use deployment_fixture::{Deployments, Sources};

async fn journal_rows(
    tx: &Transaction,
    table: &str,
    filter: serde_json::Value,
) -> Vec<super::store::Row> {
    use zeroship_data_orm::{orm::Output, value};
    let collection = tx
        .database()
        .collection(&format!("__zeroship_workflow_{table}"))
        .unwrap();
    let mut rows = Vec::new();
    loop {
        let Output::Rows { rows: page, .. } = collection
            .find(
                filter.clone().into(),
                value!({"offset":rows.len(), "limit":zeroship_data_orm::sql::MAX_ROW_LIMIT, "orderBy":{"id":1}}),
            )
            .await
            .unwrap()
        else {
            panic!("expected journal rows")
        };
        if page.is_empty() {
            break;
        }
        rows.extend(page.into_iter().map(super::store::Row));
    }
    rows
}

async fn journal_insert(
    tx: &Transaction,
    table: &str,
    document: serde_json::Value,
) -> Result<(), WorkflowServiceError> {
    tx.database()
        .collection(&format!("__zeroship_workflow_{table}"))?
        .insert(document.into())
        .await?;
    Ok(())
}

async fn journal_count(tx: &Transaction, table: &str, filter: serde_json::Value) -> i64 {
    let zeroship_data_orm::orm::Output::Count(count) = tx
        .database()
        .collection(&format!("__zeroship_workflow_{table}"))
        .unwrap()
        .count(filter.into(), zeroship_data_orm::value!({}))
        .await
        .unwrap()
    else {
        panic!("expected journal count")
    };
    count
}

async fn journal_update(
    tx: &Transaction,
    table: &str,
    filter: serde_json::Value,
    patch: serde_json::Value,
) {
    tx.database()
        .collection(&format!("__zeroship_workflow_{table}"))
        .unwrap()
        .execute(zeroship_data_orm::orm::Operation::Update {
            filter: filter.into(),
            patch: patch.into(),
            many: true,
        })
        .await
        .unwrap();
}

fn storage_id() -> String {
    typed_id::generate("wfj")
}

async fn sqlite_store(path: &Path) -> OrmStore {
    let store = orm_store(
        &format!(
            "sqlite:{}",
            path.parent().unwrap().join("orm.sqlite").display()
        ),
        super::store::SchemaName::new("workflow").unwrap(),
    )
    .await;
    schema::initialize_local(&store).await.unwrap();
    store
}

async fn orm_store(url: &str, schema: super::store::SchemaName) -> OrmStore {
    OrmStore::connect(
        zeroship_data_orm::binding::DbBinding::new("workflow", "test-deployment", schema),
        &zeroship_data_orm::connection::ConnectionFactory::for_url(url).unwrap(),
        zeroship_data_orm::encryption::ProjectKeySource::unavailable(),
    )
    .await
    .unwrap()
}

#[compio::test]
async fn sqlite_app_operations_are_scoped_and_retryable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    app_contract(Rc::new(sqlite_store(&path).await)).await;
}

#[compio::test]
async fn postgres_app_operations_are_scoped_and_retryable() {
    let fixture = PostgresFixture::start().await;
    app_contract(Rc::new(fixture.store.clone())).await;
}

async fn registered_service(store: Rc<OrmStore>) -> (WorkflowService, AppId, AppId, Deployments) {
    registered_with_deployments(store, Deployments::new().await).await
}

async fn registered_with_deployments(
    store: Rc<OrmStore>,
    deployments: Deployments,
) -> (WorkflowService, AppId, AppId, Deployments) {
    let a = AppId::mint();
    let b = AppId::mint();
    let service = WorkflowService::open(store, Arc::new(HostPolicies::default()))
        .await
        .unwrap()
        .with_deployments(deployments.binding(&[&a, &b]));
    for app in [&a, &b] {
        service
            .register_app(app, configured_policy(1, AppPolicy::default()))
            .await
            .unwrap();
        service
            .register_app(app, configured_policy(1, AppPolicy::default()))
            .await
            .unwrap();
        deployments
            .activate(
                &service,
                app,
                &DeployRegistration {
                    id: typed_id::generate("dep"),
                    hash: "a".repeat(64),
                    workflows: ["Example".into(), "Child".into()].into(),
                    schedules: Vec::new(),
                },
            )
            .await
            .unwrap();
    }
    (service, a, b, deployments)
}

async fn app_contract(store: Rc<OrmStore>) {
    let (service, a, b, _deployments) = registered_service(store).await;
    let a = service.for_app(a);
    let b = service.for_app(b);
    let request = RequestId::mint();
    let options = StartOptions {
        input: json!({"hello": "world"}),
        key: Some("invoice".into()),
        on_conflict: ConflictPolicy::Join,
    };
    let first = a.start(&request, "Example", options.clone()).await.unwrap();
    assert_eq!(
        first,
        a.start(&request, "Example", options.clone()).await.unwrap()
    );
    assert_eq!(
        first.id,
        a.start(&RequestId::mint(), "Example", options.clone())
            .await
            .unwrap()
            .id
    );
    assert_ne!(
        first.id,
        b.start(&request, "Example", options.clone())
            .await
            .unwrap()
            .id
    );
    assert!(matches!(
        b.status(&first.id).await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert!(matches!(
        a.start(&request, "Child", options.clone()).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    let signal_request = RequestId::mint();
    let signal = SignalOptions {
        signal_type: "approved".into(),
        payload: json!(true),
    };
    assert!(matches!(
        b.signal(&signal_request, &first.id, signal.clone()).await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    let delivered = a
        .signal(&signal_request, &first.id, signal.clone())
        .await
        .unwrap();
    assert_eq!(
        delivered,
        a.signal(&signal_request, &first.id, signal).await.unwrap()
    );
    let policy = AppPolicy {
        admission: false,
        ..AppPolicy::default()
    };
    service
        .register_app(a.app_id(), configured_policy(2, policy))
        .await
        .unwrap();
    assert_eq!(
        first,
        a.start(&request, "Example", options.clone()).await.unwrap()
    );
    assert!(matches!(
        a.start(&RequestId::mint(), "Example", options).await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
}

struct PostgresFixture {
    _container: Container<GenericImage>,
    store: OrmStore,
    admin_url: String,
}
impl PostgresFixture {
    async fn start() -> Self {
        let container = GenericImage::new("postgres", "18")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_HOST_AUTH_METHOD", "trust")
            .start()
            .expect("workflow PostgreSQL container");
        let host = container.get_host().unwrap();
        let port = container.get_host_port_ipv4(5432).unwrap();
        let admin_url = format!("postgres://postgres@{host}:{port}/postgres");
        let admin = connect(&admin_url).await;
        admin
            .batch_execute(
                "CREATE ROLE customer_migrator NOLOGIN; \
             CREATE ROLE customer_worker LOGIN; CREATE ROLE app_customer_role NOLOGIN; \
             GRANT app_customer_role TO customer_worker; CREATE ROLE zeroship_worker LOGIN; \
             CREATE ROLE zeroship_gateway LOGIN; CREATE ROLE zeroship_app LOGIN; \
             CREATE ROLE zeroship_control LOGIN; CREATE ROLE zeroship_workflow LOGIN; \
             CREATE SCHEMA customer AUTHORIZATION customer_migrator; \
             SET ROLE customer_migrator;",
            )
            .await
            .unwrap();
        let schema = super::store::SchemaName::new("customer").unwrap();
        admin
            .batch_execute(&schema::postgres_sql(&schema))
            .await
            .unwrap();
        admin.batch_execute(
            "RESET ROLE; GRANT USAGE ON SCHEMA customer TO app_customer_role; \
             GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA customer TO app_customer_role;"
        ).await.unwrap();
        Self {
            _container: container,
            store: orm_store(
                &format!("postgres://customer_worker@{host}:{port}/postgres"),
                schema,
            )
            .await,
            admin_url,
        }
    }
}
async fn connect(url: &str) -> compio_postgres::Client {
    let (client, connection) = compio_postgres::connect(url, NoTls).await.unwrap();
    compio::runtime::spawn(async move {
        connection.run().await.unwrap();
    })
    .detach();
    client
}

#[test]
fn generated_schema_matches_the_shared_migration_definition() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new("node")
        .arg("crates/zeroship-workflow/schema/generate.mjs")
        .arg("--check")
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "schema compiler check failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[compio::test]
async fn sqlite_schema_constraints_and_transaction_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    let store = sqlite_store(&path).await;
    storage_contract(&store).await;
}

#[compio::test]
async fn postgres_schema_constraints_and_transaction_rollback() {
    let fixture = PostgresFixture::start().await;
    storage_contract(&fixture.store).await;
}

async fn storage_contract(store: &OrmStore) {
    store.verify().await.unwrap();
    let tx = store.begin().await.unwrap();
    journal_insert(
        &tx,
        "app_state",
        json!({"id":storage_id(), "app_id":"app_rollback", "signal_epoch":0}),
    )
    .await
    .unwrap();
    drop(tx);
    let mut tx = store.begin().await.unwrap();
    assert!(
        journal_rows(&tx, "app_state", json!({"app_id":"app_rollback"}))
            .await
            .is_empty()
    );
    for app in ["app_a", "app_b"] {
        journal_insert(
            &tx,
            "app_state",
            json!({"id":storage_id(), "app_id":app, "signal_epoch":0}),
        )
        .await
        .unwrap();
        journal_insert(&tx, "deploys", json!({"app_id":app, "id":format!("deploy_{app}"), "hash":format!("deploy_{app}"), "manifest":"{}", "created_at":0, "active":1, "state":"available", "availability_epoch":0})).await.unwrap();
    }
    insert_run(&mut tx, "app_a", "run_a", "deploy_app_a", None)
        .await
        .unwrap();
    insert_run(&mut tx, "app_b", "run_b", "deploy_app_b", None)
        .await
        .unwrap();
    assert!(tx.now().await.unwrap() > 0);
    tx.commit().await.unwrap();
    for (app, run, deploy, parent) in [
        ("app_a", "bad_deploy", "deploy_app_b", None),
        ("app_a", "bad_parent", "deploy_app_a", Some("run_b")),
        ("app_missing", "bad_app", "deploy_app_a", None),
    ] {
        let mut tx = store.begin().await.unwrap();
        assert!(insert_run(&mut tx, app, run, deploy, parent).await.is_err());
    }
    let tx = store.begin().await.unwrap();
    assert_eq!(journal_rows(&tx, "runs", json!({})).await.len(), 2);
    assert!(journal_insert(&tx, "signals", json!({"app_id":"app_a", "run_id":"run_b", "id":"bad_signal", "signal_type":"approval", "payload":"null", "created_at":0})).await.is_err());
}

async fn insert_run(
    tx: &mut Transaction,
    app: &str,
    id: &str,
    deploy: &str,
    parent: Option<&str>,
) -> Result<(), WorkflowServiceError> {
    journal_insert(tx, "runs", json!({"app_id":app, "id":id, "workflow_name":"Checkout", "deploy_id":deploy, "generation":0, "state":"queued", "control":"none", "lease_epoch":0, "cascade":0, "depth":0, "created_at":0, "signal_epoch":0, "parent_id":parent})).await
}

#[compio::test]
async fn customer_runtime_has_dml_without_ddl_and_platform_roles_have_no_access() {
    let fixture = PostgresFixture::start().await;
    fixture.store.verify().await.unwrap();
    let admin = connect(&fixture.admin_url).await;
    for role in [
        "zeroship_worker",
        "zeroship_gateway",
        "zeroship_app",
        "zeroship_control",
        "zeroship_workflow",
    ] {
        let url = fixture
            .admin_url
            .replacen("postgres@", &format!("{role}@"), 1);
        let client = connect(&url).await;
        assert!(client
            .batch_execute("SELECT * FROM customer.__zeroship_workflow_runs")
            .await
            .is_err());
        assert!(client
            .batch_execute("DELETE FROM customer.__zeroship_workflow_runs")
            .await
            .is_err());
        assert!(client
            .batch_execute("SET ROLE customer_migrator")
            .await
            .is_err());
        let member: bool = admin
            .query_one(
                "SELECT pg_has_role($1, 'customer_migrator', 'MEMBER')",
                &[&role],
            )
            .await
            .unwrap()
            .get(0);
        assert!(!member);
    }
    let runtime = connect(
        &fixture
            .admin_url
            .replacen("postgres@", "customer_worker@", 1),
    )
    .await;
    assert!(runtime
        .batch_execute("CREATE TABLE customer.unauthorized (id text)")
        .await
        .is_err());
    assert!(runtime
        .batch_execute("ALTER TABLE customer.__zeroship_workflow_runs ADD COLUMN unauthorized text")
        .await
        .is_err());
    assert!(runtime
        .batch_execute("SET ROLE customer_migrator")
        .await
        .is_err());
}

#[test]
fn local_initialization_never_resets_an_incompatible_journal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE __zeroship_workflow_runs (id text); INSERT INTO __zeroship_workflow_runs VALUES ('retained')",
    )
    .unwrap();
    assert!(schema::initialize_sqlite(&path).is_err());
    assert_eq!(
        conn.query_row("SELECT id FROM __zeroship_workflow_runs", [], |row| row
            .get::<_, String>(
            0
        ))
        .unwrap(),
        "retained"
    );
}

#[compio::test]
async fn sqlite_task_leases_and_receipts_preserve_the_frontier() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    task_contract(Rc::new(sqlite_store(&path).await)).await;
}

#[compio::test]
async fn postgres_task_leases_and_receipts_preserve_the_frontier() {
    let fixture = PostgresFixture::start().await;
    task_contract(Rc::new(fixture.store.clone())).await;
}

fn execution(value: serde_json::Value) -> crate::WorkflowExecution {
    crate::WorkflowExecution::from_runtime_value(json!({"outcomes":value})).unwrap()
}

async fn task_contract(store: Rc<OrmStore>) {
    use super::{TaskToken, WorkerIdentity};
    use crate::operations::RunState;
    let (service, app, _, _deployments) = registered_service(store.clone()).await;
    let scope = service.for_app(app.clone());
    let worker = WorkerIdentity::new("worker-a".into()).unwrap();
    let other = WorkerIdentity::new("worker-b".into()).unwrap();
    let request = RequestId::mint();
    let options = StartOptions {
        key: Some("invoice".into()),
        ..Default::default()
    };
    let run = scope
        .start(&request, "Example", options.clone())
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, run.id);
    assert!(task.invocation.journal.is_empty());
    assert_eq!(task.invocation.app_id, app.as_str());
    assert!(service.poll(&other).await.unwrap().is_none());
    assert!(matches!(
        service.heartbeat(&other, &task.id, &task.token).await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert!(matches!(
        service
            .heartbeat(&worker, &task.id, &TaskToken::mint())
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert!(
        service
            .heartbeat(&worker, &task.id, &task.token)
            .await
            .unwrap()
            .deadline
            >= task.deadline
    );
    let signal_request = RequestId::mint();
    scope
        .signal(
            &signal_request,
            &run.id,
            SignalOptions {
                signal_type: "approved".into(),
                payload: json!({"ok":true}),
            },
        )
        .await
        .unwrap();
    let wait =
        execution(json!([{"kind":"Wait","ordinal":0,"name":"approval","signalType":"approved"}]));
    let receipt = service
        .complete(&worker, &task.id, &task.token, wait.clone())
        .await
        .unwrap();
    assert_eq!(receipt.state, RunState::Queued);
    assert_eq!(
        receipt,
        service
            .complete(&worker, &task.id, &task.token, wait)
            .await
            .unwrap()
    );
    assert!(matches!(
        service
            .complete(
                &worker,
                &task.id,
                &task.token,
                execution(json!([{"kind":"RunCompleted","output":false}]))
            )
            .await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    let next = service.poll(&other).await.unwrap().unwrap();
    assert_eq!(next.invocation.journal[0].state, "completed");
    let envelope = next.invocation.journal[0].output.as_ref().unwrap();
    assert_eq!(envelope["payload"], json!({"ok":true}));
    assert_eq!(envelope["type"], json!("approved"));
    assert_eq!(envelope["origin"], json!("app"));
    assert_eq!(envelope["delivery"], json!("direct"));
    typed_id::parse_with_prefix(
        envelope["id"].as_str().unwrap(),
        typed_id::WORKFLOW_SIGNAL_PREFIX,
    )
    .unwrap();
    chrono::DateTime::parse_from_rfc3339(envelope["createdAt"].as_str().unwrap()).unwrap();
    let done = execution(json!([{"kind":"RunCompleted","output":{"paid":true}}]));
    service
        .complete(&other, &next.id, &next.token, done.clone())
        .await
        .unwrap();
    assert_eq!(
        scope.status(&run.id).await.unwrap().output,
        Some(json!({"paid":true}))
    );
    assert_eq!(
        run,
        scope
            .start(&request, "Example", options.clone())
            .await
            .unwrap()
    );
    let new = scope
        .start(&RequestId::mint(), "Example", options)
        .await
        .unwrap();
    assert_ne!(new.id, run.id);
    let abandoned = service.poll(&worker).await.unwrap().unwrap();
    let mut tx = store.begin().await.unwrap();
    let expired = tx.now().await.unwrap() - 1;
    journal_update(
        &tx,
        "tasks",
        json!({"id":abandoned.id}),
        json!({"deadline":expired}),
    )
    .await;
    journal_update(
        &tx,
        "runs",
        json!({"app_id":app.as_str(), "id":new.id}),
        json!({"due_at":expired}),
    )
    .await;
    tx.commit().await.unwrap();
    assert!(matches!(
        service
            .heartbeat(&worker, &abandoned.id, &abandoned.token)
            .await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    let policies = service.policies.clone();
    drop(service);
    let recovered = WorkflowService::open(store, policies).await.unwrap();
    let replacement = recovered.poll(&other).await.unwrap().unwrap();
    assert_eq!(replacement.invocation.run_id, new.id);
    assert!(replacement.epoch > abandoned.epoch);
    assert!(matches!(
        recovered
            .complete(&worker, &abandoned.id, &abandoned.token, done.clone())
            .await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    let receipt = recovered
        .complete(&other, &replacement.id, &replacement.token, done.clone())
        .await
        .unwrap();
    assert_eq!(
        receipt,
        recovered
            .complete(&other, &replacement.id, &replacement.token, done)
            .await
            .unwrap()
    );
    assert!(recovered.poll(&worker).await.unwrap().is_none());
}

#[compio::test]
async fn sqlite_lifecycle_children_and_restart_share_service_transitions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    behavior_contract(Rc::new(sqlite_store(&path).await)).await;
}
#[compio::test]
async fn postgres_lifecycle_children_and_restart_share_service_transitions() {
    let fixture = PostgresFixture::start().await;
    behavior_contract(Rc::new(fixture.store.clone())).await;
}
async fn behavior_contract(store: Rc<OrmStore>) {
    use super::{ControlIntent, WorkerIdentity};
    use crate::operations::{RestartOptions, RestartTarget, RunOperation, RunState};
    let (service, app, _, deployments) = registered_service(store.clone()).await;
    let scope = service.for_app(app.clone());
    let worker = WorkerIdentity::new("worker".into()).unwrap();
    let start = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert!(matches!(
        scope
            .restart(&RequestId::mint(), &start.id, RestartOptions::default())
            .await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    scope
        .transition(&RequestId::mint(), &start.id, RunOperation::Pause)
        .await
        .unwrap();
    assert_eq!(
        service
            .heartbeat(&worker, &task.id, &task.token)
            .await
            .unwrap()
            .control,
        ControlIntent::Pause
    );
    let mut tx = store.begin().await.unwrap();
    let before = super::app::lock_run(&mut tx, &app, &start.id)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    for (operation, control) in [
        (RunOperation::Resume, "none"),
        (RunOperation::Pause, "pause"),
    ] {
        scope
            .transition(&RequestId::mint(), &start.id, operation)
            .await
            .unwrap();
        let mut tx = store.begin().await.unwrap();
        let after = super::app::lock_run(&mut tx, &app, &start.id)
            .await
            .unwrap();
        assert_eq!(after.text("control").unwrap(), control);
        for field in ["task_id", "state"] {
            assert_eq!(after.text(field).unwrap(), before.text(field).unwrap());
        }
        for field in ["generation", "lease_epoch", "due_at"] {
            assert_eq!(
                after.integer(field).unwrap(),
                before.integer(field).unwrap()
            );
        }
        tx.commit().await.unwrap();
    }
    let receipt=service.complete(&worker,&task.id,&task.token,execution(json!([
        {"kind":"StepCompleted","ordinal":0,"name":"charge","output":{"charge":"accepted"},"compensable":true},
        {"kind":"Wait","ordinal":1,"name":"approval","signalType":"approved"}
    ]))).await.unwrap();
    assert_eq!(receipt.state, RunState::Paused);
    scope
        .signal(
            &RequestId::mint(),
            &start.id,
            SignalOptions {
                signal_type: "approved".into(),
                payload: json!(true),
            },
        )
        .await
        .unwrap();
    assert!(service.poll(&worker).await.unwrap().is_none());
    scope
        .transition(&RequestId::mint(), &start.id, RunOperation::Resume)
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(
        task.invocation.journal[0].output,
        Some(json!({"charge":"accepted"}))
    );
    assert_eq!(
        task.invocation.journal[1].output.as_ref().unwrap()["payload"],
        json!(true)
    );
    scope
        .transition(&RequestId::mint(), &start.id, RunOperation::Cancel)
        .await
        .unwrap();
    let receipt=service.complete(&worker,&task.id,&task.token,execution(json!([
        {"kind":"StepCompleted","ordinal":2,"name":"reserve","compensable":true,"output":"reservation"},
        {"kind":"RunCompleted","output":"must be cancelled"}
    ]))).await.unwrap();
    assert_eq!(receipt.state, RunState::Compensating);
    assert!(matches!(
        scope
            .restart(&RequestId::mint(), &start.id, RestartOptions::default())
            .await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    for (ordinal, name) in [(2, "reserve"), (0, "charge")] {
        let task = service.poll(&worker).await.unwrap().unwrap();
        assert_eq!(task.invocation.phase, "compensating");
        service
            .complete(
                &worker,
                &task.id,
                &task.token,
                execution(json!([{"kind":"CompensationCompleted","ordinal":ordinal,"name":name}])),
            )
            .await
            .unwrap();
    }
    assert_eq!(
        scope.status(&start.id).await.unwrap().state,
        RunState::Cancelled
    );
    assert!(matches!(
        scope
            .restart(
                &RequestId::mint(),
                &start.id,
                RestartOptions {
                    from: Some(RestartTarget {
                        name: "approval".into(),
                        occurrence: None
                    }),
                    deploy: None
                }
            )
            .await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    let restarted = scope
        .restart(&RequestId::mint(), &start.id, RestartOptions::default())
        .await
        .unwrap();
    assert_eq!(restarted.run_id, start.id);
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.generation, 1);
    assert!(task.invocation.journal.is_empty());
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([
                {"kind":"StepCompleted","ordinal":0,"name":"keep","output":42},
                {"kind":"StepCompleted","ordinal":1,"name":"redo","output":0},
                {"kind":"RunCompleted","output":"original"}
            ])),
        )
        .await
        .unwrap();
    scope
        .restart(
            &RequestId::mint(),
            &start.id,
            RestartOptions {
                from: Some(RestartTarget {
                    name: "redo".into(),
                    occurrence: None,
                }),
                deploy: None,
            },
        )
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.generation, 2);
    assert_eq!(task.invocation.journal.len(), 1);
    assert_eq!(task.invocation.journal[0].output, Some(json!(42)));
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([
                {"kind":"StepCompleted","ordinal":1,"name":"redo","output":1},
                {"kind":"RunCompleted","output":"restarted"}
            ])),
        )
        .await
        .unwrap();
    let tx = store.begin().await.unwrap();
    let history = journal_rows(
        &tx,
        "steps",
        json!({"app_id":app.as_str(), "run_id":start.id, "generation":1, "ordinal":1}),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&history[0].text("record").unwrap()).unwrap()
            ["output"],
        json!(0)
    );
    tx.commit().await.unwrap();

    let parent = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    let old_deploy = task.invocation.deploy_id.clone();
    let new_deploy = DeployRegistration {
        id: typed_id::generate("dep"),
        hash: "b".repeat(64),
        workflows: ["Example".into(), "Child".into()].into(),
        schedules: Vec::new(),
    };
    deployments
        .activate(&service, &app, &new_deploy)
        .await
        .unwrap();
    let invalid = execution(json!([
        {"kind":"Child","ordinal":0,"name":"child","childWorkflowName":"Child","input":{"task":true}},
        {"kind":"StepCompleted","ordinal":9,"name":"invalid"}
    ]));
    assert!(matches!(
        service
            .complete(&worker, &task.id, &task.token, invalid)
            .await,
        Err(WorkflowServiceError::InvalidRequest(_))
    ));
    let tx = store.begin().await.unwrap();
    assert!(journal_rows(
        &tx,
        "runs",
        json!({"app_id":app.as_str(), "parent_id":parent.id})
    )
    .await
    .is_empty());
    tx.commit().await.unwrap();
    service.complete(&worker,&task.id,&task.token,execution(json!([{ "kind":"Child","ordinal":0,"name":"child","childWorkflowName":"Child","input":{"task":true}}]))).await.unwrap();
    let child = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(child.invocation.workflow_name, "Child");
    assert_eq!(child.invocation.deploy_id, old_deploy);
    service
        .complete(
            &worker,
            &child.id,
            &child.token,
            execution(json!([{"kind":"ContinueAsNew","input":"child continuation"}])),
        )
        .await
        .unwrap();
    let continued_child = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(continued_child.invocation.workflow_name, "Child");
    assert_eq!(continued_child.invocation.deploy_id, new_deploy.id);
    service
        .complete(
            &worker,
            &continued_child.id,
            &continued_child.token,
            execution(json!([{"kind":"RunCompleted","output":"child result"}])),
        )
        .await
        .unwrap();
    let resumed = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(resumed.invocation.run_id, parent.id);
    assert_eq!(
        resumed.invocation.journal[0].output,
        Some(json!("child result"))
    );
    service
        .complete(
            &worker,
            &resumed.id,
            &resumed.token,
            execution(json!([{"kind":"ContinueAsNew","input":{"next":true}}])),
        )
        .await
        .unwrap();
    let successor = service.poll(&worker).await.unwrap().unwrap();
    assert_ne!(successor.invocation.run_id, parent.id);
    assert_eq!(successor.invocation.deploy_id, new_deploy.id);
    assert_eq!(
        successor.invocation.trigger.input,
        Some(json!({"next":true}))
    );
    service
        .complete(
            &worker,
            &successor.id,
            &successor.token,
            execution(json!([{"kind":"RunCompleted","output":"finished"}])),
        )
        .await
        .unwrap();
    assert!(service.poll(&worker).await.unwrap().is_none());
}

#[compio::test]
async fn sqlite_concurrent_admission_cycles_and_compensation_retries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    review_contract(Rc::new(sqlite_store(&path).await)).await;
}
#[compio::test]
async fn postgres_concurrent_admission_cycles_and_compensation_retries() {
    let fixture = PostgresFixture::start().await;
    review_contract(Rc::new(fixture.store.clone())).await;
}
async fn review_contract(store: Rc<OrmStore>) {
    use super::WorkerIdentity;
    use crate::operations::{RunOperation, RunState};
    let (service, app, other_app, _deployments) = registered_service(store.clone()).await;
    let scope = service.for_app(app.clone());
    let policy = AppPolicy {
        max_running: 1,
        compensation_retry_ms: 60_000,
        ..Default::default()
    };
    service
        .register_app(&app, configured_policy(2, policy))
        .await
        .unwrap();
    let request = RequestId::mint();
    let mut starts = Vec::new();
    for _ in 0..4 {
        let scope = scope.clone();
        let request = request.clone();
        starts.push(compio::runtime::spawn(async move {
            scope
                .start(
                    &request,
                    "Example",
                    StartOptions {
                        key: Some("concurrent".into()),
                        ..Default::default()
                    },
                )
                .await
        }));
    }
    let mut ids = std::collections::BTreeSet::new();
    for task in starts {
        ids.insert(task.await.unwrap().unwrap().id);
    }
    assert_eq!(ids.len(), 1);
    let mut polls = Vec::new();
    for i in 0..4 {
        let service = service.clone();
        polls.push(compio::runtime::spawn(async move {
            service
                .poll(&WorkerIdentity::new(format!("concurrent-{i}")).unwrap())
                .await
                .map(|task| (i, task))
        }));
    }
    let mut assignments = Vec::new();
    for poll in polls {
        let (i, task) = poll.await.unwrap().unwrap();
        if let Some(task) = task {
            assignments.push((i, task));
        }
    }
    assert_eq!(assignments.len(), 1);
    let (i, task) = assignments.remove(0);
    service
        .complete(
            &WorkerIdentity::new(format!("concurrent-{i}")).unwrap(),
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();

    let worker = WorkerIdentity::new("worker".into()).unwrap();
    let a = scope
        .start(
            &RequestId::mint(),
            "Example",
            StartOptions {
                key: Some("a".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let b = scope
        .start(
            &RequestId::mint(),
            "Child",
            StartOptions {
                key: Some("b".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, a.id);
    service.complete(&worker,&task.id,&task.token,execution(json!([{"kind":"Child","ordinal":0,"name":"b","childWorkflowName":"Child","options":{"key":"b"}}]))).await.unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, b.id);
    assert!(matches!(service.complete(&worker,&task.id,&task.token,execution(json!([{"kind":"Child","ordinal":0,"name":"a","childWorkflowName":"Example","options":{"key":"a"}}]))).await,Err(WorkflowServiceError::InvalidRequest(_))));
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();

    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    service.complete(&worker,&task.id,&task.token,execution(json!([
        {"kind":"StepCompleted","ordinal":0,"name":"older","compensable":true,"output":0},
        {"kind":"StepCompleted","ordinal":1,"name":"newer","compensable":true,"compensationMaxAttempts":2,"output":1},
        {"kind":"RunFailed","error":{"message":"trigger compensation"}}
    ]))).await.unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    let failed = execution(
        json!([{"kind":"CompensationFailed","ordinal":1,"name":"newer","error":{"message":"try again"}}]),
    );
    let receipt = service
        .complete(&worker, &task.id, &task.token, failed.clone())
        .await
        .unwrap();
    assert_eq!(receipt.state, RunState::Compensating);
    assert_eq!(
        receipt,
        service
            .complete(&worker, &task.id, &task.token, failed)
            .await
            .unwrap()
    );
    scope
        .transition(&RequestId::mint(), &run.id, RunOperation::Pause)
        .await
        .unwrap();
    scope
        .transition(&RequestId::mint(), &run.id, RunOperation::Resume)
        .await
        .unwrap();
    assert!(service.poll(&worker).await.unwrap().is_none());
    let mut tx = store.begin().await.unwrap();
    let now = tx.now().await.unwrap();
    journal_update(
        &tx,
        "steps",
        json!({"app_id":app.as_str(), "run_id":run.id}),
        json!({"compensation_due_at":now}),
    )
    .await;
    journal_update(
        &tx,
        "runs",
        json!({"app_id":app.as_str(), "id":run.id}),
        json!({"due_at":now}),
    )
    .await;
    tx.commit().await.unwrap();
    let recovered = WorkflowService::open(store.clone(), service.policies.clone())
        .await
        .unwrap();
    let task = recovered.poll(&worker).await.unwrap().unwrap();
    assert_eq!(
        task.invocation.journal[1].compensation_state.as_deref(),
        Some("pending")
    );
    recovered.complete(&worker,&task.id,&task.token,execution(json!([{"kind":"CompensationFailed","ordinal":1,"name":"newer","error":{"message":"exhausted"}}]))).await.unwrap();
    let task = recovered.poll(&worker).await.unwrap().unwrap();
    assert_eq!(
        task.invocation.journal[1].compensation_state.as_deref(),
        Some("failed")
    );
    recovered
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"CompensationCompleted","ordinal":0,"name":"older"}])),
        )
        .await
        .unwrap();
    let status = scope.status(&run.id).await.unwrap();
    assert_eq!(status.state, RunState::Failed);
    assert_eq!(
        status.error.as_ref().unwrap()["name"],
        json!("WorkflowCompensationError")
    );
    assert_eq!(
        status.error.as_ref().unwrap()["failures"][0]["ordinal"],
        json!(1)
    );

    // An app with a full execution allocation must not hide another app behind
    // its backlog in the candidate page.
    service
        .register_app(
            &app,
            configured_policy(
                3,
                AppPolicy {
                    max_running: 0,
                    ..Default::default()
                },
            ),
        )
        .await
        .unwrap();
    for _ in 0..130 {
        scope
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await
            .unwrap();
    }
    let other = service
        .for_app(other_app)
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, other.id);
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
}

#[compio::test]
async fn postgres_completion_that_outlives_its_lease_rolls_back() {
    delayed_lease_write("steps", "INSERT", false).await;
}

#[compio::test]
async fn postgres_receipt_write_that_outlives_its_lease_rolls_back() {
    delayed_lease_write("tasks", "UPDATE", false).await;
}

#[compio::test]
async fn postgres_heartbeat_write_that_outlives_its_lease_rolls_back() {
    delayed_lease_write("tasks", "UPDATE", true).await;
}

async fn delayed_lease_write(table: &str, operation: &str, heartbeat: bool) {
    let fixture = PostgresFixture::start().await;
    let store: Rc<OrmStore> = Rc::new(fixture.store.clone());
    let (service, app, _, _deployments) = registered_service(store.clone()).await;
    let scope = service.for_app(app.clone());
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    service
        .register_app(
            &app,
            configured_policy(
                2,
                AppPolicy {
                    lease_ms: 300,
                    ..Default::default()
                },
            ),
        )
        .await
        .unwrap();
    let admin = connect(&fixture.admin_url).await;
    admin.batch_execute(&format!("CREATE SEQUENCE customer.delayed_write_calls; GRANT USAGE ON SEQUENCE customer.delayed_write_calls TO app_customer_role; CREATE FUNCTION customer.delay_write() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM nextval('customer.delayed_write_calls'); PERFORM pg_sleep(0.5); RETURN NEW; END $$; CREATE TRIGGER delay_write BEFORE {operation} ON customer.__zeroship_workflow_{table} FOR EACH ROW EXECUTE FUNCTION customer.delay_write();")).await.unwrap();
    let worker = super::WorkerIdentity::new("worker".into()).unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    if heartbeat {
        let result = service.heartbeat(&worker, &task.id, &task.token).await;
        assert!(
            matches!(result, Err(WorkflowServiceError::Conflict(_))),
            "{result:?}"
        );
    } else {
        let result = service.complete(&worker,&task.id,&task.token,execution(json!([{"kind":"StepCompleted","ordinal":0,"name":"slow","output":"must roll back"},{"kind":"RunCompleted"}]))).await;
        assert!(
            matches!(result, Err(WorkflowServiceError::Conflict(_))),
            "{result:?}"
        );
    }
    // Sequences survive rollback, proving this reached the delayed database write.
    assert!(admin
        .query_one("SELECT is_called FROM customer.delayed_write_calls", &[])
        .await
        .unwrap()
        .get::<_, bool>(0));
    let tx = store.begin().await.unwrap();
    assert!(journal_rows(
        &tx,
        "steps",
        json!({"app_id":app.as_str(), "run_id":run.id})
    )
    .await
    .is_empty());
    tx.commit().await.unwrap();
    let tx = store.begin().await.unwrap();
    let persisted = tx
        .database()
        .entity::<super::models::tasks::Entity>()
        .unwrap()
        .find::<super::models::TaskRecord>(
            super::models::tasks::id.eq(task.id.as_str()).unwrap(),
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(persisted[0].state, "leased");
    assert_eq!(persisted[0].deadline, task.deadline);
    assert!(persisted[0].receipt.is_none());
    tx.commit().await.unwrap();
    assert_eq!(
        scope.status(&run.id).await.unwrap().state,
        crate::operations::RunState::Running
    );
    admin
        .batch_execute(&format!(
            "DROP TRIGGER delay_write ON customer.__zeroship_workflow_{table};"
        ))
        .await
        .unwrap();
    let replacement = service.poll(&worker).await.unwrap().unwrap();
    assert!(replacement.epoch > task.epoch);
    service
        .complete(
            &worker,
            &replacement.id,
            &replacement.token,
            execution(json!([{"kind":"RunCompleted","output":"recovered"}])),
        )
        .await
        .unwrap();
}

#[compio::test]
async fn sqlite_signal_completion_races_do_not_lose_wakeups() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    signal_race_contract(Rc::new(sqlite_store(&path).await)).await;
}
#[compio::test]
async fn postgres_signal_completion_races_do_not_lose_wakeups() {
    let fixture = PostgresFixture::start().await;
    signal_race_contract(Rc::new(fixture.store.clone())).await;
}
async fn signal_race_contract(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let scope = service.for_app(app);
    let worker = super::WorkerIdentity::new("worker".into()).unwrap();
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    let signal = {
        let scope = scope.clone();
        let id = run.id.clone();
        compio::runtime::spawn(async move {
            scope
                .signal(
                    &RequestId::mint(),
                    &id,
                    SignalOptions {
                        signal_type: "ready".into(),
                        payload: json!("delivered"),
                    },
                )
                .await
        })
    };
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"Wait","ordinal":0,"name":"wait","signalType":"ready"}])),
        )
        .await
        .unwrap();
    signal.await.unwrap().unwrap();
    let resumed = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(
        resumed.invocation.journal[0].output.as_ref().unwrap()["payload"],
        json!("delivered")
    );
    service
        .complete(
            &worker,
            &resumed.id,
            &resumed.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    scope
        .signal(
            &RequestId::mint(),
            &run.id,
            SignalOptions {
                signal_type: "late".into(),
                payload: json!("after timeout"),
            },
        )
        .await
        .unwrap();
    service.complete(&worker,&task.id,&task.token,execution(json!([{"kind":"Wait","ordinal":0,"name":"timed","signalType":"late","wakeAt":"2000-01-01T00:00:00Z"}]))).await.unwrap();
    let resumed = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(resumed.invocation.journal[0].state, "completed");
    assert_eq!(
        serde_json::to_value(&resumed.invocation.journal[0]).unwrap()["output"],
        json!(null)
    );
    assert!(resumed.invocation.journal[0].error.is_none());
    service
        .complete(
            &worker,
            &resumed.id,
            &resumed.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
}

#[compio::test]
async fn sqlite_schedules_commit_occurrences_with_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    schedule_contract(Rc::new(sqlite_store(&path).await)).await;
}
#[compio::test]
async fn postgres_schedules_commit_occurrences_with_runs() {
    let fixture = PostgresFixture::start().await;
    schedule_contract(Rc::new(fixture.store.clone())).await;
}
async fn schedule_contract(store: Rc<OrmStore>) {
    use super::{
        IntervalAnchor, ScheduleCatchUp, ScheduleOverlap, ScheduleRegistration, ScheduleTiming,
    };
    let (service, app, _, deployments) = registered_service(store.clone()).await;
    let deploy = DeployRegistration {
        id: typed_id::generate("dep"),
        hash: "c".repeat(64),
        workflows: ["Example".into()].into(),
        schedules: vec![ScheduleRegistration {
            name: "billing".into(),
            workflow_name: "Example".into(),
            schedule: ScheduleTiming::Interval {
                interval_ms: 60_000,
                anchor: IntervalAnchor::Epoch,
            },
            input: json!({"scheduled":true}),
            overlap: ScheduleOverlap::Allow,
            catch_up: ScheduleCatchUp::Backfill { max: 3 },
        }],
    };
    deployments.activate(&service, &app, &deploy).await.unwrap();
    assert_eq!(service.tick_schedules().await.unwrap(), 0);
    let mut tx = store.begin().await.unwrap();
    let now = tx.now().await.unwrap();
    let at = (now / 60_000 - 5) * 60_000;
    journal_update(
        &tx,
        "schedules",
        json!({"app_id":app.as_str()}),
        json!({"next_at":at}),
    )
    .await;
    tx.commit().await.unwrap();
    // Activation notification retries must not move a persisted due frontier.
    deployments.activate(&service, &app, &deploy).await.unwrap();
    let mut ticks = Vec::new();
    for _ in 0..3 {
        let service = service.clone();
        ticks.push(compio::runtime::spawn(async move {
            service.tick_schedules().await
        }));
    }
    let mut fired = 0;
    for tick in ticks {
        fired += tick.await.unwrap().unwrap();
    }
    assert_eq!(fired, 3);
    let tx = store.begin().await.unwrap();
    let mut rows = journal_rows(&tx, "occurrences", json!({"app_id":app.as_str()})).await;
    rows.sort_by_key(|row| row.integer("at").unwrap());
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].integer("at").unwrap(), at);
    let scheduled_ids: std::collections::BTreeSet<String> =
        rows.iter().map(|row| row.text("run_id").unwrap()).collect();
    tx.commit().await.unwrap();
    let worker = super::WorkerIdentity::new("schedule-worker".into()).unwrap();
    let mut executed = std::collections::BTreeSet::new();
    while let Some(task) = service.poll(&worker).await.unwrap() {
        assert_eq!(task.invocation.deploy_id, deploy.id);
        assert_eq!(
            task.invocation.trigger.input,
            Some(json!({"scheduled":true}))
        );
        executed.insert(task.invocation.run_id.clone());
        service
            .complete(
                &worker,
                &task.id,
                &task.token,
                execution(json!([{"kind":"RunCompleted"}])),
            )
            .await
            .unwrap();
    }
    assert_eq!(executed, scheduled_ids);
    let mut skip = deploy.clone();
    skip.id = typed_id::generate("dep");
    skip.hash = "d".repeat(64);
    skip.schedules[0].overlap = ScheduleOverlap::SkipIfRunning;
    deployments.activate(&service, &app, &skip).await.unwrap();
    let mut tx = store.begin().await.unwrap();
    let later = tx.now().await.unwrap();
    let first = at + 180_000 + 10;
    journal_update(
        &tx,
        "schedules",
        json!({"app_id":app.as_str()}),
        json!({"next_at":first}),
    )
    .await;
    tx.commit().await.unwrap();
    assert_eq!(service.tick_schedules().await.unwrap(), 1);
    let tx = store.begin().await.unwrap();
    let rows = journal_rows(
        &tx,
        "occurrences",
        json!({"app_id":app.as_str(), "at":{"$gte":first, "$lte":later}}),
    )
    .await;
    assert_eq!(
        rows.iter()
            .filter(|row| row.optional_text("run_id").unwrap().is_some())
            .count(),
        1
    );
    assert!(rows
        .iter()
        .any(|row| row.optional_text("run_id").unwrap().is_none()));
    tx.commit().await.unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"ContinueAsNew","input":"scheduled continuation"}])),
        )
        .await
        .unwrap();
    let tx = store.begin().await.unwrap();
    journal_update(
        &tx,
        "schedules",
        json!({"app_id":app.as_str()}),
        json!({"next_at":(first + 1)}),
    )
    .await;
    tx.commit().await.unwrap();
    assert_eq!(service.tick_schedules().await.unwrap(), 0);
    let successor = service.poll(&worker).await.unwrap().unwrap();
    assert_ne!(successor.invocation.run_id, task.invocation.run_id);
    service
        .complete(
            &worker,
            &successor.id,
            &successor.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    let mut disabled = skip.clone();
    disabled.id = typed_id::generate("dep");
    disabled.hash = "e".repeat(64);
    disabled.schedules.clear();
    deployments
        .activate(&service, &app, &disabled)
        .await
        .unwrap();
    let tx = store.begin().await.unwrap();
    assert!(journal_rows(
        &tx,
        "schedules",
        json!({"app_id":app.as_str(), "next_at":{"$exists":true}})
    )
    .await
    .is_empty());
    assert!(
        !journal_rows(&tx, "occurrences", json!({"app_id":app.as_str()}))
            .await
            .is_empty()
    );
    tx.commit().await.unwrap();
    assert_eq!(service.tick_schedules().await.unwrap(), 0);
}

#[compio::test]
async fn sqlite_topic_fanout_preserves_recipient_scope_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    broadcast_contract(Rc::new(sqlite_store(&path).await)).await;
}
#[compio::test]
async fn postgres_topic_fanout_preserves_recipient_scope_after_restart() {
    let fixture = PostgresFixture::start().await;
    broadcast_contract(Rc::new(fixture.store.clone())).await;
}
async fn wait_on_topic(
    service: &WorkflowService,
    scope: &super::AppWorkflows,
    worker: &super::WorkerIdentity,
) -> String {
    let run = scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = service.poll(worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, run.id);
    service.complete(worker,&task.id,&task.token,execution(json!([{"kind":"Wait","ordinal":0,"name":"event","signalType":"news","topic":"updates"}]))).await.unwrap();
    run.id
}
async fn broadcast_contract(store: Rc<OrmStore>) {
    let (service, app, other, _deployments) = registered_service(store.clone()).await;
    let scope = service.for_app(app.clone());
    let other_scope = service.for_app(other);
    let worker = super::WorkerIdentity::new("fanout-worker".into()).unwrap();
    let mut expected = std::collections::BTreeSet::new();
    for _ in 0..129 {
        expected.insert(wait_on_topic(&service, &scope, &worker).await);
    }
    let foreign = wait_on_topic(&service, &other_scope, &worker).await;
    let request = RequestId::mint();
    let message = SignalOptions {
        signal_type: "news".into(),
        payload: json!({"release":"ready"}),
    };
    let broadcast = scope
        .broadcast(&request, "updates", message.clone())
        .await
        .unwrap();
    assert_eq!(
        broadcast,
        scope.broadcast(&request, "updates", message).await.unwrap()
    );
    assert!(matches!(
        scope
            .broadcast(
                &request,
                "another",
                SignalOptions {
                    signal_type: "news".into(),
                    payload: json!(null)
                }
            )
            .await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    let late = wait_on_topic(&service, &scope, &worker).await;
    assert_eq!(service.tick_broadcasts().await.unwrap(), 128);
    let recovered = WorkflowService::open(store.clone(), service.policies.clone())
        .await
        .unwrap();
    assert_eq!(recovered.tick_broadcasts().await.unwrap(), 1);
    assert_eq!(recovered.tick_broadcasts().await.unwrap(), 0);
    let mut actual = std::collections::BTreeSet::new();
    let mut signals = std::collections::BTreeSet::new();
    while let Some(task) = recovered.poll(&worker).await.unwrap() {
        let envelope = task.invocation.journal[0].output.as_ref().unwrap();
        assert_eq!(envelope["payload"], json!({"release":"ready"}));
        assert_eq!(envelope["topic"], json!("updates"));
        assert_eq!(envelope["delivery"], json!("topic"));
        assert!(signals.insert(envelope["id"].as_str().unwrap().to_owned()));
        actual.insert(task.invocation.run_id.clone());
        recovered
            .complete(
                &worker,
                &task.id,
                &task.token,
                execution(json!([{"kind":"RunCompleted"}])),
            )
            .await
            .unwrap();
    }
    assert_eq!(actual, expected);
    assert!(!actual.contains(&foreign));
    assert!(!actual.contains(&late));
    // Restarting a subscriber invalidates a publication still awaiting fanout.
    scope
        .broadcast(
            &RequestId::mint(),
            "updates",
            SignalOptions {
                signal_type: "news".into(),
                payload: json!("old generation"),
            },
        )
        .await
        .unwrap();
    scope
        .restart(
            &RequestId::mint(),
            &late,
            crate::operations::RestartOptions::default(),
        )
        .await
        .unwrap();
    let task = recovered.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, late);
    recovered.complete(&worker,&task.id,&task.token,execution(json!([{"kind":"Wait","ordinal":0,"name":"event","signalType":"news","topic":"updates"}]))).await.unwrap();
    assert_eq!(recovered.tick_broadcasts().await.unwrap(), 0);
    assert!(recovered.poll(&worker).await.unwrap().is_none());
    scope
        .signal(
            &RequestId::mint(),
            &late,
            SignalOptions {
                signal_type: "news".into(),
                payload: json!("current generation"),
            },
        )
        .await
        .unwrap();
    let task = recovered.poll(&worker).await.unwrap().unwrap();
    assert_eq!(
        task.invocation.journal[0].output.as_ref().unwrap()["payload"],
        json!("current generation")
    );
    recovered
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    assert_eq!(
        other_scope.status(&foreign).await.unwrap().state,
        crate::operations::RunState::Waiting
    );
}

#[compio::test]
async fn sqlite_signal_ingress_enforces_scopes_epochs_and_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    ingress_contract(Rc::new(sqlite_store(&path).await)).await;
}
#[compio::test]
async fn postgres_signal_ingress_enforces_scopes_epochs_and_receipts() {
    let fixture = PostgresFixture::start().await;
    ingress_contract(Rc::new(fixture.store.clone())).await;
}
async fn ingress_contract(store: Rc<OrmStore>) {
    use super::{
        capability::{mint_signal_capability, SignalGrant, SignalTarget},
        IngressReceipt, SignalAuthority, SignalTokenRequest,
    };
    use zeroship_core::service_assertion::{ServiceSigningKey, ServiceTrustBundle};
    let (service, app, other, _deployments) = registered_service(store.clone()).await;
    let worker = super::WorkerIdentity::new("ingress-worker".into()).unwrap();
    let scope = service.for_app(app.clone());
    let run = wait_on_topic(&service, &scope, &worker).await;
    let target = SignalTarget::Run {
        run_id: run.clone(),
    };
    let options = SignalTokenRequest {
        target: target.clone(),
        types: ["news".into()].into(),
        lifetime_seconds: 60,
    };
    assert!(matches!(
        scope
            .issue_signal_token(&RequestId::mint(), options.clone())
            .await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    let key = Arc::new(ServiceSigningKey::generate());
    let authority = Arc::new(SignalAuthority::new(key.clone(), ServiceTrustBundle::new()).unwrap());
    let service = service.with_signal_authority(authority.clone());
    let scope = service.for_app(app.clone());
    let other_scope = service.for_app(other.clone());
    let foreign = wait_on_topic(&service, &other_scope, &worker).await;
    let issue = RequestId::mint();
    let token = scope
        .issue_signal_token(&issue, options.clone())
        .await
        .unwrap();
    assert_eq!(
        token.as_str(),
        scope
            .issue_signal_token(&issue, options.clone())
            .await
            .unwrap()
            .as_str()
    );
    let message = SignalOptions {
        signal_type: "news".into(),
        payload: json!({"approved":true}),
    };
    assert!(matches!(
        service
            .ingest_signal(
                &RequestId::mint(),
                token.as_str(),
                &other,
                &target,
                message.clone()
            )
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert!(matches!(
        service
            .ingest_signal(
                &RequestId::mint(),
                token.as_str(),
                &app,
                &SignalTarget::Run { run_id: foreign },
                message.clone()
            )
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert_eq!(
        service
            .ingest_signal(
                &RequestId::mint(),
                token.as_str(),
                &app,
                &target,
                SignalOptions {
                    signal_type: "unlisted".into(),
                    payload: json!(null)
                }
            )
            .await,
        Err(WorkflowServiceError::PermissionDenied)
    );
    let expired = mint_signal_capability(
        &key,
        SignalGrant {
            app_id: app.clone(),
            target: target.clone(),
            types: ["news".into()].into(),
            epoch: 0,
            app_epoch: 0,
        },
        1000,
        60,
    )
    .unwrap();
    assert_eq!(
        service
            .ingest_signal(
                &RequestId::mint(),
                expired.as_str(),
                &app,
                &target,
                message.clone()
            )
            .await,
        Err(WorkflowServiceError::Unauthenticated)
    );
    let request = RequestId::mint();
    let receipt = service
        .ingest_signal(&request, token.as_str(), &app, &target, message.clone())
        .await
        .unwrap();
    assert!(matches!(receipt, IngressReceipt::Direct { .. }));
    assert_eq!(
        receipt,
        service
            .ingest_signal(&request, token.as_str(), &app, &target, message.clone())
            .await
            .unwrap()
    );
    assert!(matches!(
        service
            .ingest_signal(
                &request,
                token.as_str(),
                &app,
                &target,
                SignalOptions {
                    signal_type: "news".into(),
                    payload: json!("changed")
                }
            )
            .await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    let revoke = RequestId::mint();
    let revoked = scope
        .revoke_signal_tokens(&revoke, Some(target.clone()))
        .await
        .unwrap();
    assert_eq!(
        revoked,
        scope
            .revoke_signal_tokens(&revoke, Some(target.clone()))
            .await
            .unwrap()
    );
    assert_eq!(
        service
            .ingest_signal(&request, token.as_str(), &app, &target, message.clone())
            .await,
        Err(WorkflowServiceError::Unauthenticated)
    );
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, run);
    assert_eq!(
        task.invocation.journal[0].output.as_ref().unwrap()["origin"],
        json!("ingress")
    );
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();

    let waiting = wait_on_topic(&service, &scope, &worker).await;
    let topic = SignalTarget::Topic {
        topic: "updates".into(),
    };
    let topic_options = SignalTokenRequest {
        target: topic.clone(),
        types: ["news".into()].into(),
        lifetime_seconds: 60,
    };
    let token = scope
        .issue_signal_token(&RequestId::mint(), topic_options.clone())
        .await
        .unwrap();
    let request = RequestId::mint();
    let receipt = service
        .ingest_signal(&request, token.as_str(), &app, &topic, message.clone())
        .await
        .unwrap();
    assert!(matches!(receipt, IngressReceipt::Topic { .. }));
    assert_eq!(
        receipt,
        service
            .ingest_signal(&request, token.as_str(), &app, &topic, message.clone())
            .await
            .unwrap()
    );
    scope
        .revoke_signal_tokens(&RequestId::mint(), Some(topic.clone()))
        .await
        .unwrap();
    assert_eq!(
        service
            .ingest_signal(
                &RequestId::mint(),
                token.as_str(),
                &app,
                &topic,
                message.clone()
            )
            .await,
        Err(WorkflowServiceError::Unauthenticated)
    );
    let recovered = WorkflowService::open(store.clone(), service.policies.clone())
        .await
        .unwrap()
        .with_signal_authority(authority);
    assert_eq!(recovered.tick_broadcasts().await.unwrap(), 1);
    let task = recovered.poll(&worker).await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, waiting);
    assert_eq!(
        task.invocation.journal[0].output.as_ref().unwrap()["delivery"],
        json!("topic")
    );
    assert_eq!(
        task.invocation.journal[0].output.as_ref().unwrap()["origin"],
        json!("ingress")
    );
    recovered
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunCompleted"}])),
        )
        .await
        .unwrap();
    let token = scope
        .issue_signal_token(&RequestId::mint(), topic_options)
        .await
        .unwrap();
    scope
        .revoke_signal_tokens(&RequestId::mint(), None)
        .await
        .unwrap();
    assert_eq!(
        recovered
            .ingest_signal(
                &RequestId::mint(),
                token.as_str(),
                &app,
                &topic,
                message.clone()
            )
            .await,
        Err(WorkflowServiceError::Unauthenticated)
    );

    let run = wait_on_topic(&service, &scope, &worker).await;
    let target = SignalTarget::Run {
        run_id: run.clone(),
    };
    let token = scope
        .issue_signal_token(
            &RequestId::mint(),
            SignalTokenRequest {
                target: target.clone(),
                ..options
            },
        )
        .await
        .unwrap();
    scope
        .restart(
            &RequestId::mint(),
            &run,
            crate::operations::RestartOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        service
            .ingest_signal(&RequestId::mint(), token.as_str(), &app, &target, message)
            .await,
        Err(WorkflowServiceError::Unauthenticated)
    );
}
