use super::*;
use crate::{
    operations::RunState,
    service::{ControlIntent, WorkerIdentity},
};
use std::time::{Duration, Instant};

#[test]
fn host_policy_revisions_reject_conflicting_limits_and_accept_authorized_refreshes() {
    let app = AppId::mint();
    let policies = HostPolicies::default();
    let until = Instant::now() + Duration::from_secs(30);
    let snapshot =
        PolicySnapshot::lease(1.try_into().unwrap(), AppPolicy::default(), until).unwrap();
    policies.install(&app, snapshot.clone()).unwrap();
    let conflicting = PolicySnapshot::lease(
        1.try_into().unwrap(),
        AppPolicy {
            admission: false,
            ..AppPolicy::default()
        },
        until,
    )
    .unwrap();
    assert!(matches!(
        policies.install(&app, conflicting),
        Err(WorkflowServiceError::Conflict(_))
    ));
    assert!(policies.resolve(&app).unwrap().admission);
    let refresh = PolicySnapshot::lease(
        1.try_into().unwrap(),
        AppPolicy::default(),
        until + Duration::from_secs(30),
    )
    .unwrap();
    policies.install(&app, refresh).unwrap();
    policies.install(&app, snapshot).unwrap();
    let original_remaining = until.saturating_duration_since(Instant::now()).as_millis();
    assert!(u128::try_from(policies.resolve(&app).unwrap().lease_ms).unwrap() > original_remaining);
    assert!(matches!(
        policies.resolve(&AppId::mint()),
        Err(WorkflowServiceError::PermissionDenied)
    ));
}

#[compio::test]
async fn policy_revocation_while_waiting_for_customer_lock_prevents_admission() {
    let fixture = PostgresFixture::start().await;
    let (service, app, _, _deployments) = registered_service(Rc::new(fixture.store.clone())).await;
    let blocker = connect(&fixture.admin_url).await;
    blocker.batch_execute("BEGIN").await.unwrap();
    blocker
        .query_one(
            "SELECT app_id FROM customer.__zeroship_workflow_app_state WHERE app_id=$1 FOR UPDATE",
            &[&app.as_str()],
        )
        .await
        .unwrap();
    let scope = service.for_app(app.clone());
    let starting = compio::runtime::spawn(async move {
        scope
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await
    });
    let observer = connect(&fixture.admin_url).await;
    compio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = observer.query_one("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE usename='customer_worker' AND wait_event_type='Lock' AND query LIKE 'SELECT app_id FROM %')", &[]).await.unwrap().get(0);
            if waiting { break; }
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("start reached the customer app lock");
    service
        .policies
        .install(
            &app,
            PolicySnapshot::lease(2.try_into().unwrap(), AppPolicy::default(), Instant::now())
                .unwrap(),
        )
        .unwrap();
    blocker.batch_execute("COMMIT").await.unwrap();
    assert!(matches!(
        starting.await.unwrap(),
        Err(WorkflowServiceError::PermissionDenied)
    ));
    let runs: i64 = observer
        .query_one(
            "SELECT count(*) FROM customer.__zeroship_workflow_runs",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(runs, 0);
}

#[compio::test]
async fn sqlite_host_policy_expiry_preserves_history_and_stops_new_execution() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    host_policy_contract(Rc::new(sqlite_store(&path).await)).await;
}

#[compio::test]
async fn postgres_host_policy_needs_no_platform_database() {
    let fixture = PostgresFixture::start().await;
    host_policy_contract(Rc::new(fixture.store.clone())).await;
    let admin = connect(&fixture.admin_url).await;
    assert!(admin
        .query(
            "SELECT nspname FROM pg_namespace WHERE nspname IN ('workflow','zeroship')",
            &[]
        )
        .await
        .unwrap()
        .is_empty());
    assert!(admin.query("SELECT column_name FROM information_schema.columns WHERE table_schema='customer' AND column_name IN ('policy','platform_app_id','deploy_revision')", &[]).await.unwrap().is_empty());
}

#[expect(
    clippy::future_not_send,
    reason = "The fixture drives a thread-local compio journal"
)]
async fn host_policy_contract(store: Rc<OrmStore>) {
    let (service, app, other, _deployments) = registered_service(store.clone()).await;
    let scope = service.for_app(app.clone());
    let request = RequestId::mint();
    let run = scope
        .start(&request, "Example", StartOptions::default())
        .await
        .unwrap();
    let worker = WorkerIdentity::new("customer-worker".into()).unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    let expired =
        PolicySnapshot::lease(2.try_into().unwrap(), AppPolicy::default(), Instant::now()).unwrap();
    service.register_app(&app, expired.clone()).await.unwrap();
    assert!(matches!(
        scope
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert_eq!(
        scope
            .start(&request, "Example", StartOptions::default())
            .await
            .unwrap(),
        run
    );
    let heartbeat = service
        .heartbeat(&worker, &task.id, &task.token)
        .await
        .unwrap();
    assert_eq!(heartbeat.control, ControlIntent::Pause);
    assert_eq!(
        heartbeat.deadline, task.deadline,
        "expired policy renewed execution authority"
    );
    service
        .complete(
            &worker,
            &task.id,
            &task.token,
            execution(json!([{"kind":"RunCompleted","output":{"customer":"retained"}}])),
        )
        .await
        .unwrap();
    let status = scope.status(&run.id).await.unwrap();
    assert_eq!(status.state, RunState::Completed);
    assert_eq!(status.output, Some(json!({"customer":"retained"})));
    assert!(service
        .for_app(other)
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .is_ok());
    assert!(matches!(
        service
            .register_app(&app, configured_policy(1, AppPolicy::default()))
            .await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    assert!(matches!(
        service
            .register_app(&app, configured_policy(2, AppPolicy::default()))
            .await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    service.register_app(&app, expired).await.unwrap();
    assert!(matches!(
        scope
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await,
        Err(WorkflowServiceError::PermissionDenied)
    ));

    // Journal contents cannot repopulate host authorization after a restart.
    let unconfigured = WorkflowService::open(store.clone(), Arc::new(HostPolicies::default()))
        .await
        .unwrap();
    assert!(matches!(
        unconfigured
            .for_app(app.clone())
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    let reopened = WorkflowService::open(store, service.policies.clone())
        .await
        .unwrap();
    assert!(matches!(
        reopened
            .for_app(app.clone())
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    reopened
        .register_app(&app, configured_policy(3, AppPolicy::default()))
        .await
        .unwrap();
    assert!(scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .is_ok());
}

#[compio::test]
async fn metadata_lease_bounds_grants_and_duplicate_delivery_keeps_its_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("zs-workflow.sqlite");
    schema::initialize_sqlite(&path).unwrap();
    let (service, app, _, _deployments) =
        registered_service(Rc::new(sqlite_store(&path).await)).await;
    let lifetime = Duration::from_millis(250);
    let until = Instant::now() + lifetime;
    let snapshot =
        PolicySnapshot::lease(2.try_into().unwrap(), AppPolicy::default(), until).unwrap();
    service.register_app(&app, snapshot.clone()).await.unwrap();
    let scope = service.for_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let worker = WorkerIdentity::new("customer-worker".into()).unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert!(u128::try_from(task.lease_ms).unwrap() <= lifetime.as_millis());
    compio::time::sleep(until.saturating_duration_since(Instant::now())).await;
    service.register_app(&app, snapshot).await.unwrap();
    assert!(matches!(
        scope
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert!(service.poll(&worker).await.unwrap().is_none());
    assert!(matches!(
        service.heartbeat(&worker, &task.id, &task.token).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
}

#[compio::test]
async fn customer_schema_binding_is_explicit_and_independent_of_app_identity() {
    let fixture = PostgresFixture::start().await;
    let (first, app, _, deployments) = registered_service(Rc::new(fixture.store.clone())).await;
    let admin = connect(&fixture.admin_url).await;
    let other = super::super::store::SchemaName::new("customer-other").unwrap();
    admin.batch_execute("CREATE SCHEMA \"customer-other\" AUTHORIZATION customer_migrator; CREATE ROLE other_customer_worker LOGIN; CREATE ROLE \"app_customer-other_role\" NOLOGIN; GRANT \"app_customer-other_role\" TO other_customer_worker; SET ROLE customer_migrator;").await.unwrap();
    admin
        .batch_execute(&schema::postgres_sql(&other))
        .await
        .unwrap();
    admin.batch_execute("RESET ROLE; GRANT USAGE ON SCHEMA \"customer-other\" TO \"app_customer-other_role\"; GRANT SELECT,INSERT,UPDATE,DELETE ON ALL TABLES IN SCHEMA \"customer-other\" TO \"app_customer-other_role\";").await.unwrap();
    let wrong = Rc::new(
        orm_store(
            &fixture.admin_url.replace("postgres@", "customer_worker@"),
            other.clone(),
        )
        .await,
    );
    assert!(
        WorkflowService::open(wrong, Arc::new(HostPolicies::default()))
            .await
            .is_err()
    );
    let second = WorkflowService::open(
        Rc::new(
            orm_store(
                &fixture
                    .admin_url
                    .replace("postgres@", "other_customer_worker@"),
                other,
            )
            .await,
        ),
        Arc::new(HostPolicies::default()),
    )
    .await
    .unwrap();
    let second = second.with_deployments(deployments.binding(&[&app]));
    second
        .register_app(&app, configured_policy(1, AppPolicy::default()))
        .await
        .unwrap();
    deployments
        .activate(
            &second,
            &app,
            &DeployRegistration {
                id: typed_id::generate("dep"),
                hash: "b".repeat(64),
                workflows: ["Example".into()].into(),
                schedules: Vec::new(),
            },
        )
        .await
        .unwrap();
    let first = first.for_app(app.clone());
    let second = second.for_app(app);
    let request = RequestId::mint();
    let a = first
        .start(&request, "Example", StartOptions::default())
        .await
        .unwrap();
    let b = second
        .start(&request, "Example", StartOptions::default())
        .await
        .unwrap();
    assert_ne!(a.id, b.id);
    assert!(matches!(
        first.status(&b.id).await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert!(matches!(
        second.status(&a.id).await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    for invalid in ["", "customer.other", "customer\"; SELECT 'untrusted'"] {
        assert!(super::super::store::SchemaName::new(invalid).is_err());
    }
    let names = admin.query("SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname IN ('customer','customer-other')", &[]).await.unwrap();
    assert!(!names.is_empty());
    for row in names {
        let name: &str = row.get(0);
        assert!(
            name.starts_with("__zeroship_workflow_"),
            "unreserved journal relation {name}"
        );
    }
}
