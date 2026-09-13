use super::*;
use crate::service::policy::admit;
use crate::{
    operations::RunState,
    service::{ControlIntent, WorkerIdentity},
};
use std::time::{Duration, Instant};

mod lifetimes;

#[test]
fn snapshots_reject_invalid_shared_policy_values() {
    let policy = AppPolicy {
        lease_ms: 0,
        ..AppPolicy::default()
    };
    for snapshot in [
        PolicySnapshot::configuration(1.try_into().unwrap(), policy.clone()),
        PolicySnapshot::lease(1.try_into().unwrap(), policy, Instant::now()),
    ] {
        assert!(matches!(
            snapshot,
            Err(WorkflowServiceError::InvalidRequest(_))
        ));
    }
}

#[test]
fn authority_preserves_raw_admission_near_lease_expiry() {
    let app = AppId::mint();
    let policies = Arc::new(HostPolicies::default());
    let policy = AppPolicy::default();
    let stop = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            Instant::now() < stop,
            "could not capture a live management authority"
        );
        let until = Instant::now() + Duration::from_micros(900);
        policies
            .fixture_install(
                &app,
                PolicySnapshot::lease(1.try_into().unwrap(), policy.clone(), until).unwrap(),
            )
            .unwrap();
        let authority = match policies.fixture_authority(&app) {
            Ok(authority) => authority,
            Err(WorkflowServiceError::Unavailable(_)) if Instant::now() >= until => continue,
            Err(error) => panic!("unexpected management authority failure: {error}"),
        };
        assert_eq!(authority.deadline, Some(until));
        assert_eq!(authority.policy, policy);
        assert!(authority.policy.admission);
        assert!(admit(&authority.policy).is_ok());
        assert!(
            !policies.fixture_resolve(&app).unwrap().admission,
            "the effective execution policy must exercise its expiry rounding"
        );
        break;
    }
}

#[test]
fn authority_keeps_the_original_deadline_after_refresh() {
    let app = AppId::mint();
    let policies = Arc::new(HostPolicies::default());
    let until = Instant::now() + Duration::from_secs(30);
    policies
        .fixture_install(
            &app,
            PolicySnapshot::lease(1.try_into().unwrap(), AppPolicy::default(), until).unwrap(),
        )
        .unwrap();
    let mut authority = policies.fixture_authority(&app).unwrap();
    authority.check().unwrap();
    let refreshed_until = until + Duration::from_secs(30);
    policies
        .fixture_install(
            &app,
            PolicySnapshot::lease(1.try_into().unwrap(), AppPolicy::default(), refreshed_until)
                .unwrap(),
        )
        .unwrap();
    assert_eq!(authority.deadline, Some(until));
    authority.check().unwrap();
    let refreshed = policies.fixture_authority(&app).unwrap();
    assert_eq!(refreshed.deadline, Some(refreshed_until));
    refreshed.check().unwrap();

    // Simulate expiration of the captured budget while the refreshed host lease
    // remains valid, without waiting for wall-clock scheduling.
    authority.deadline = Some(Instant::now());
    assert!(matches!(
        authority.check(),
        Err(WorkflowServiceError::Unavailable(_))
    ));
    refreshed.check().unwrap();
}

#[test]
fn authority_retries_when_the_host_revision_changes() {
    let app = AppId::mint();
    let policies = Arc::new(HostPolicies::default());
    policies
        .fixture_install(&app, leased_policy(1, AppPolicy::default()))
        .unwrap();
    let authority = policies.fixture_authority(&app).unwrap();
    authority.check().unwrap();
    policies
        .fixture_install(&app, leased_policy(2, AppPolicy::default()))
        .unwrap();
    assert!(matches!(
        authority.check(),
        Err(WorkflowServiceError::Unavailable(_))
    ));
    policies.fixture_authority(&app).unwrap().check().unwrap();
}

#[test]
fn authority_requires_a_present_unexpired_host_snapshot() {
    let app = AppId::mint();
    let policies = Arc::new(HostPolicies::default());
    assert!(matches!(
        policies.fixture_authority(&app),
        Err(WorkflowServiceError::Unavailable(_))
    ));
    policies
        .fixture_install(&app, leased_policy(1, AppPolicy::default()))
        .unwrap();
    let authority = policies.fixture_authority(&app).unwrap();
    authority.check().unwrap();
    assert!(matches!(
        Arc::new(HostPolicies::default()).fixture_authority(&app),
        Err(WorkflowServiceError::Unavailable(_))
    ));
    policies
        .fixture_install(
            &app,
            PolicySnapshot::lease(2.try_into().unwrap(), AppPolicy::default(), Instant::now())
                .unwrap(),
        )
        .unwrap();
    assert!(matches!(
        policies.fixture_authority(&app),
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert!(matches!(
        authority.check(),
        Err(WorkflowServiceError::Unavailable(_))
    ));
}

#[test]
fn authority_preserves_an_explicit_configured_admission_denial() {
    let app = AppId::mint();
    let policies = Arc::new(HostPolicies::default());
    let policy = AppPolicy {
        admission: false,
        ..AppPolicy::default()
    };
    policies
        .fixture_install(
            &app,
            PolicySnapshot::configuration(1.try_into().unwrap(), policy.clone()).unwrap(),
        )
        .unwrap();
    let authority = policies.fixture_authority(&app).unwrap();
    assert_eq!(authority.deadline, None);
    assert_eq!(authority.policy, policy);
    authority.check().unwrap();
    assert!(matches!(
        admit(&authority.policy),
        Err(WorkflowServiceError::PermissionDenied)
    ));
}

#[test]
fn host_policy_revisions_reject_conflicting_limits_and_accept_authorized_refreshes() {
    let app = AppId::mint();
    let policies = Arc::new(HostPolicies::default());
    let until = Instant::now() + Duration::from_secs(30);
    let snapshot =
        PolicySnapshot::lease(1.try_into().unwrap(), AppPolicy::default(), until).unwrap();
    policies.fixture_install(&app, snapshot.clone()).unwrap();
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
        policies.fixture_install(&app, conflicting),
        Err(WorkflowServiceError::Conflict(_))
    ));
    assert!(policies.fixture_resolve(&app).unwrap().admission);
    let refresh = PolicySnapshot::lease(
        1.try_into().unwrap(),
        AppPolicy::default(),
        until + Duration::from_secs(30),
    )
    .unwrap();
    let delayed = policies
        .fixture_binding(&app)
        .unwrap()
        .begin_refresh()
        .unwrap();
    policies.fixture_install(&app, refresh).unwrap();
    assert!(matches!(
        delayed.install(snapshot),
        Err(WorkflowServiceError::Unavailable(_))
    ));
    let original_remaining = until.saturating_duration_since(Instant::now()).as_millis();
    assert!(
        u128::try_from(policies.fixture_resolve(&app).unwrap().lease_ms).unwrap()
            > original_remaining
    );
    assert!(matches!(
        policies.fixture_resolve(&AppId::mint()),
        Err(WorkflowServiceError::Unavailable(_))
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
    let scope = service.fixture_app(app.clone());
    let starting = compio::runtime::spawn(async move {
        scope
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await
    });
    let observer = connect(&fixture.admin_url).await;
    compio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = observer.query_one("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE usename='customer_worker' AND wait_event_type='Lock' AND position('__zeroship_workflow_app_state' in query) > 0)", &[]).await.unwrap().get(0);
            if waiting { break; }
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("start reached the customer app lock");
    service
        .policies
        .fixture_install(
            &app,
            PolicySnapshot::lease(2.try_into().unwrap(), AppPolicy::default(), Instant::now())
                .unwrap(),
        )
        .unwrap();
    blocker.batch_execute("COMMIT").await.unwrap();
    assert!(matches!(
        starting.await.unwrap(),
        Err(WorkflowServiceError::Unavailable(_))
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
    let scope = service.fixture_app(app.clone());
    let request = RequestId::mint();
    let run = scope
        .start(&request, "Example", StartOptions::default())
        .await
        .unwrap();
    let worker = WorkerIdentity::new("customer-worker".into()).unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    let expired =
        PolicySnapshot::lease(2.try_into().unwrap(), AppPolicy::default(), Instant::now()).unwrap();
    service
        .policies
        .fixture_install(&app, expired.clone())
        .unwrap();
    assert!(matches!(
        scope
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await,
        Err(WorkflowServiceError::Unavailable(_))
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
        .fixture_app(other)
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .is_ok());
    assert!(matches!(
        service
            .fixture_register(&app, leased_policy(1, AppPolicy::default()))
            .await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    assert!(matches!(
        service
            .fixture_register(
                &app,
                PolicySnapshot::configuration(2.try_into().unwrap(), AppPolicy::default()).unwrap()
            )
            .await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    service.policies.fixture_install(&app, expired).unwrap();
    assert!(matches!(
        scope
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await,
        Err(WorkflowServiceError::Unavailable(_))
    ));

    // Journal contents cannot repopulate host authorization after a restart.
    let unconfigured = WorkflowService::open(store.clone(), Arc::new(HostPolicies::default()))
        .await
        .unwrap();
    assert!(matches!(
        unconfigured
            .fixture_app(app.clone())
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    let reopened = WorkflowService::open(store, service.policies.clone())
        .await
        .unwrap();
    assert!(matches!(
        reopened
            .fixture_app(app.clone())
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    reopened
        .fixture_register(&app, leased_policy(3, AppPolicy::default()))
        .await
        .unwrap();
    assert!(scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .is_ok());
}

#[compio::test]
async fn sqlite_retired_app_handles_cannot_use_replacement_authority() {
    let directory = tempfile::tempdir().unwrap();
    retired_app_handles(Rc::new(
        sqlite_store(&directory.path().join("app.sqlite")).await,
    ))
    .await;
}

#[compio::test]
async fn postgres_retired_app_handles_cannot_use_replacement_authority() {
    let fixture = PostgresFixture::start().await;
    retired_app_handles(Rc::new(fixture.store.clone())).await;
}

#[expect(
    clippy::future_not_send,
    reason = "the fixture retains native handles on their owning compio thread"
)]
async fn retired_app_handles(store: Rc<OrmStore>) {
    use crate::backend::WorkflowBackend;

    let (service, app, other, _deployments) = registered_service(store).await;
    let binding = service.policies.current_binding(&app).unwrap();
    let scope = service.bind_app(&binding).unwrap();
    let backend = scope.clone().into_backend(1024).unwrap();
    let unrelated = service.fixture_app(other);
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    binding
        .begin_refresh()
        .unwrap()
        .install(leased_policy(2, AppPolicy::default()))
        .unwrap();
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    backend
        .start("Example".into(), StartOptions::default())
        .await
        .unwrap();

    let delayed = binding.begin_refresh().unwrap();
    binding.revoke().unwrap();
    let replacement = service.policies.bind(app.clone()).unwrap();
    replacement
        .begin_refresh()
        .unwrap()
        .install(leased_policy(2, AppPolicy::default()))
        .unwrap();
    let current = service.register_app(&replacement).await.unwrap();
    assert!(matches!(
        delayed.install(leased_policy(3, AppPolicy::default())),
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert!(matches!(
        binding.revoke(),
        Err(WorkflowServiceError::Unavailable(_))
    ));
    let before = ingress_state(&service, &app).await;
    assert!(matches!(
        scope
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert!(matches!(
        backend
            .start("Example".into(), StartOptions::default())
            .await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert_eq!(ingress_state(&service, &app).await, before);
    current
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    unrelated
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();

    let foreign = Arc::new(HostPolicies::default()).bind(app).unwrap();
    foreign
        .begin_refresh()
        .unwrap()
        .install(leased_policy(3, AppPolicy::default()))
        .unwrap();
    assert!(matches!(
        service.bind_app(&foreign),
        Err(WorkflowServiceError::PermissionDenied)
    ));
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
    service
        .policies
        .fixture_install(&app, snapshot.clone())
        .unwrap();
    let scope = service.fixture_app(app.clone());
    scope
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let worker = WorkerIdentity::new("customer-worker".into()).unwrap();
    let task = service.poll(&worker).await.unwrap().unwrap();
    assert!(u128::try_from(task.lease_ms).unwrap() <= lifetime.as_millis());
    compio::time::sleep(until.saturating_duration_since(Instant::now())).await;
    service.policies.fixture_install(&app, snapshot).unwrap();
    assert!(matches!(
        scope
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await,
        Err(WorkflowServiceError::Unavailable(_))
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
        .fixture_register(&app, leased_policy(1, AppPolicy::default()))
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
    let first = first.fixture_app(app.clone());
    let second = second.fixture_app(app);
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

#[derive(Clone, Copy, Debug)]
enum IngressOperation {
    Start,
    Signal,
    Broadcast,
    Resume,
    Restart,
    Ingest,
    IssueToken,
    RevokeRunTokens,
    RevokeTopicTokens,
    RevokeAppTokens,
}

const INGRESS_OPERATIONS: &[IngressOperation] = &[
    IngressOperation::Start,
    IngressOperation::Signal,
    IngressOperation::Broadcast,
    IngressOperation::Resume,
    IngressOperation::Restart,
    IngressOperation::Ingest,
    IngressOperation::IssueToken,
    IngressOperation::RevokeRunTokens,
    IngressOperation::RevokeTopicTokens,
    IngressOperation::RevokeAppTokens,
];

struct IngressCall {
    scope: crate::service::AppWorkflows,
    operation: IngressOperation,
    request: RequestId,
    run: String,
    token: crate::service::capability::CapabilityToken,
}

#[expect(
    clippy::future_not_send,
    reason = "native ingress fixtures stay on their owning compio thread"
)]
impl IngressCall {
    async fn prepare(service: &WorkflowService, app: &AppId, operation: IngressOperation) -> Self {
        use crate::service::{capability::SignalTarget, SignalTokenRequest};
        let scope = service.fixture_app(app.clone());
        let run = scope
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await
            .unwrap()
            .id;
        if matches!(operation, IngressOperation::Resume) {
            scope
                .transition(
                    &RequestId::mint(),
                    &run,
                    crate::operations::RunOperation::Pause,
                )
                .await
                .unwrap();
        }
        let token = scope
            .issue_signal_token(
                &RequestId::mint(),
                SignalTokenRequest {
                    target: SignalTarget::Run {
                        run_id: run.clone(),
                    },
                    types: ["ready".into()].into(),
                    lifetime_seconds: 60,
                },
            )
            .await
            .unwrap();
        if matches!(operation, IngressOperation::RevokeTopicTokens) {
            scope
                .issue_signal_token(
                    &RequestId::mint(),
                    SignalTokenRequest {
                        target: SignalTarget::Topic {
                            topic: format!("issued-{run}"),
                        },
                        types: ["ready".into()].into(),
                        lifetime_seconds: 60,
                    },
                )
                .await
                .unwrap();
        }
        Self {
            scope,
            operation,
            request: RequestId::mint(),
            run,
            token,
        }
    }

    async fn invoke(&self) -> Result<serde_json::Value, WorkflowServiceError> {
        use crate::{
            operations::{RestartOptions, RunOperation},
            service::{capability::SignalTarget, SignalTokenRequest},
        };
        let message = SignalOptions {
            signal_type: "ready".into(),
            payload: json!({"marker":"private"}),
        };
        let result = match self.operation {
            IngressOperation::Start => serde_json::to_value(
                self.scope
                    .start(&self.request, "Example", StartOptions::default())
                    .await?,
            ),
            IngressOperation::Signal => {
                serde_json::to_value(self.scope.signal(&self.request, &self.run, message).await?)
            }
            IngressOperation::Broadcast => serde_json::to_value(
                self.scope
                    .broadcast(&self.request, "updates", message)
                    .await?,
            ),
            IngressOperation::Resume => serde_json::to_value(
                self.scope
                    .transition(&self.request, &self.run, RunOperation::Resume)
                    .await?,
            ),
            IngressOperation::Restart => serde_json::to_value(
                self.scope
                    .restart(&self.request, &self.run, RestartOptions::default())
                    .await?,
            ),
            IngressOperation::Ingest => serde_json::to_value(
                self.scope
                    .ingest_signal(
                        &self.request,
                        self.token.as_str(),
                        &SignalTarget::Run {
                            run_id: self.run.clone(),
                        },
                        message,
                    )
                    .await?,
            ),
            IngressOperation::IssueToken => serde_json::to_value(
                self.scope
                    .issue_signal_token(
                        &self.request,
                        SignalTokenRequest {
                            target: SignalTarget::Topic {
                                topic: format!("issued-{}", self.run),
                            },
                            types: ["ready".into()].into(),
                            lifetime_seconds: 60,
                        },
                    )
                    .await?,
            ),
            IngressOperation::RevokeRunTokens => serde_json::to_value(
                self.scope
                    .revoke_signal_tokens(
                        &self.request,
                        Some(SignalTarget::Run {
                            run_id: self.run.clone(),
                        }),
                    )
                    .await?,
            ),
            IngressOperation::RevokeTopicTokens => serde_json::to_value(
                self.scope
                    .revoke_signal_tokens(
                        &self.request,
                        Some(SignalTarget::Topic {
                            topic: format!("issued-{}", self.run),
                        }),
                    )
                    .await?,
            ),
            IngressOperation::RevokeAppTokens => {
                serde_json::to_value(self.scope.revoke_signal_tokens(&self.request, None).await?)
            }
        };
        Ok(result.unwrap())
    }

    async fn epoch(&self) -> Option<i64> {
        let (table, filter) = match self.operation {
            IngressOperation::RevokeRunTokens => (
                "runs",
                json!({"app_id":self.scope.app_id().as_str(),"id":self.run}),
            ),
            IngressOperation::RevokeTopicTokens => (
                "topics",
                json!({"app_id":self.scope.app_id().as_str(),"topic":format!("issued-{}", self.run)}),
            ),
            IngressOperation::RevokeAppTokens => {
                ("app_state", json!({"app_id":self.scope.app_id().as_str()}))
            }
            _ => return None,
        };
        let tx = self.scope.service.begin().await.unwrap();
        let rows = journal_rows(&tx, table, filter).await;
        assert_eq!(rows.len(), 1);
        let epoch = rows[0].integer("signal_epoch").unwrap();
        tx.commit().await.unwrap();
        Some(epoch)
    }

    async fn assert_token_effect(&self, result: &serde_json::Value, previous_epoch: Option<i64>) {
        if let Some(previous) = previous_epoch {
            let expected = previous.checked_add(1).unwrap();
            assert_eq!(result["epoch"].as_i64(), Some(expected));
            assert_eq!(self.epoch().await, Some(expected));
        }
        if matches!(self.operation, IngressOperation::IssueToken) {
            assert!(!result.as_str().expect("issued capability token").is_empty());
            let tx = self.scope.service.begin().await.unwrap();
            let topics = journal_rows(&tx, "topics", json!({"app_id":self.scope.app_id().as_str(),"topic":format!("issued-{}", self.run)})).await;
            assert_eq!(topics.len(), 1);
            tx.commit().await.unwrap();
        }
    }
}

fn signed_ingress(service: WorkflowService) -> WorkflowService {
    use crate::service::SignalAuthority;
    use zeroship_core::service_assertion::{ServiceSigningKey, ServiceTrustBundle};
    service.with_signal_authority(Arc::new(
        SignalAuthority::new(
            Arc::new(ServiceSigningKey::generate()),
            ServiceTrustBundle::new(),
        )
        .unwrap(),
    ))
}

#[expect(
    clippy::future_not_send,
    reason = "journal snapshots stay on their owning compio thread"
)]
async fn ingress_state(
    service: &WorkflowService,
    app: &AppId,
) -> Vec<Vec<zeroship_data_orm::Value>> {
    let tx = service.begin().await.unwrap();
    let mut snapshot = Vec::new();
    for table in [
        "app_state",
        "runs",
        "generations",
        "steps",
        "signals",
        "topics",
        "broadcasts",
        "requests",
        "outbox",
        "job_publications",
    ] {
        snapshot.push(
            journal_rows(&tx, table, json!({"app_id":app.as_str()}))
                .await
                .into_iter()
                .map(|row| row.0)
                .collect(),
        );
    }
    tx.commit().await.unwrap();
    snapshot
}

#[compio::test]
async fn sqlite_ingress_receipts_replay_after_host_policy_expiry() {
    let directory = tempfile::tempdir().unwrap();
    ingress_receipt_expiry(Rc::new(
        sqlite_store(&directory.path().join("app.sqlite")).await,
    ))
    .await;
}

#[compio::test]
async fn postgres_ingress_receipts_replay_after_host_policy_expiry() {
    let fixture = PostgresFixture::start().await;
    ingress_receipt_expiry(Rc::new(fixture.store.clone())).await;
}

#[expect(
    clippy::future_not_send,
    reason = "native ingress fixtures stay on their owning compio thread"
)]
async fn ingress_receipt_expiry(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let service = signed_ingress(service);
    let mut revision = 1;
    for &operation in INGRESS_OPERATIONS {
        let call = IngressCall::prepare(&service, &app, operation).await;
        let previous_epoch = call.epoch().await;
        let accepted = call.invoke().await.unwrap();
        call.assert_token_effect(&accepted, previous_epoch).await;
        let before = ingress_state(&service, &app).await;
        revision += 1;
        service
            .policies
            .fixture_install(
                &app,
                PolicySnapshot::lease(
                    revision.try_into().unwrap(),
                    AppPolicy::default(),
                    Instant::now(),
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(call.invoke().await.unwrap(), accepted, "{operation:?}");
        call.assert_token_effect(&accepted, previous_epoch).await;
        assert_eq!(ingress_state(&service, &app).await, before, "{operation:?}");
        revision += 1;
        service
            .policies
            .fixture_install(&app, leased_policy(revision, AppPolicy::default()))
            .unwrap();
    }
}

struct IngressBarrier {
    blocker: compio_postgres::Client,
    observer: compio_postgres::Client,
    blocker_pid: i32,
}

#[expect(
    clippy::future_not_send,
    reason = "database barriers stay on their owning compio thread"
)]
impl IngressBarrier {
    async fn install(url: &str) -> Self {
        let blocker = connect(url).await;
        blocker
            .batch_execute(
                "CREATE FUNCTION customer.ingress_barrier() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN PERFORM pg_advisory_xact_lock(73921864); RETURN NEW; END $$;
             CREATE TRIGGER ingress_barrier BEFORE INSERT ON customer.__zeroship_workflow_requests
             FOR EACH ROW EXECUTE FUNCTION customer.ingress_barrier();",
            )
            .await
            .unwrap();
        let blocker_pid = blocker
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        blocker
            .query_one("SELECT pg_advisory_lock(73921864)", &[])
            .await
            .unwrap();
        Self {
            blocker,
            observer: connect(url).await,
            blocker_pid,
        }
    }

    async fn blocked(&self) -> i32 {
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
        }).await.expect("ingress must reach the receipt write after its app lock and mutations")
    }

    async fn rolled_back(&self, worker: i32) {
        let budget = Duration::from_secs(2);
        assert!(budget.as_millis() < u128::from(zeroship_data_orm::budgets::DB_LOCK_TIMEOUT_MS));
        compio::time::timeout(budget, async {
            loop {
                let active: bool = self.observer.query_one(
                    "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND xact_start IS NOT NULL)", &[&worker],
                ).await.unwrap().get(0);
                if !active { return; }
                compio::time::sleep(Duration::from_millis(1)).await;
            }
        }).await.expect("expired ingress must roll back while its receipt write remains blocked");
    }

    async fn unlock(&self) {
        assert!(self
            .blocker
            .query_one("SELECT pg_advisory_unlock(73921864)", &[])
            .await
            .unwrap()
            .get::<_, bool>(0));
    }

    async fn remove(&self) {
        self.blocker
            .batch_execute(
                "DROP TRIGGER ingress_barrier ON customer.__zeroship_workflow_requests;
             DROP FUNCTION customer.ingress_barrier();",
            )
            .await
            .unwrap();
    }
}

#[compio::test]
async fn postgres_ingress_revocation_after_writes_rolls_back() {
    use futures::{
        future::{select, Either},
        FutureExt,
    };
    let fixture = PostgresFixture::start().await;
    let (service, app, _, _deployments) = registered_service(Rc::new(fixture.store.clone())).await;
    let service = signed_ingress(service);
    let mut revision = 1;
    for &operation in INGRESS_OPERATIONS {
        let call = IngressCall::prepare(&service, &app, operation).await;
        let previous_epoch = call.epoch().await;
        let before = ingress_state(&service, &app).await;
        let barrier = IngressBarrier::install(&fixture.admin_url).await;
        let pending =
            match select(barrier.blocked().boxed_local(), call.invoke().boxed_local()).await {
                Either::Left((_, pending)) => pending,
                Either::Right((result, _)) => {
                    panic!("{operation:?} finished before its write barrier: {result:?}")
                }
            };
        revision += 1;
        service
            .policies
            .fixture_install(
                &app,
                leased_policy(
                    revision,
                    AppPolicy {
                        admission: false,
                        dispatch: false,
                        ingress: false,
                        ..AppPolicy::default()
                    },
                ),
            )
            .unwrap();
        barrier.unlock().await;
        assert!(
            matches!(pending.await, Err(WorkflowServiceError::Unavailable(_))),
            "{operation:?}"
        );
        barrier.remove().await;
        assert_eq!(ingress_state(&service, &app).await, before, "{operation:?}");
        assert_eq!(call.epoch().await, previous_epoch, "{operation:?}");
        revision += 1;
        service
            .policies
            .fixture_install(&app, leased_policy(revision, AppPolicy::default()))
            .unwrap();
        let accepted = call.invoke().await.unwrap();
        assert_eq!(call.invoke().await.unwrap(), accepted, "{operation:?}");
        call.assert_token_effect(&accepted, previous_epoch).await;
        assert_ne!(ingress_state(&service, &app).await, before, "{operation:?}");
    }
}

#[compio::test]
async fn postgres_ingress_binding_change_cancels_blocked_write() {
    use futures::{
        future::{select, Either},
        FutureExt,
    };
    let fixture = PostgresFixture::start().await;
    let (service, app, _, _deployments) = registered_service(Rc::new(fixture.store.clone())).await;
    let service = signed_ingress(service);
    for replace in [false, true] {
        let call = IngressCall::prepare(&service, &app, IngressOperation::Start).await;
        let binding = service.policies.current_binding(&app).unwrap();
        let delayed = binding.begin_refresh().unwrap();
        let before = ingress_state(&service, &app).await;
        let barrier = IngressBarrier::install(&fixture.admin_url).await;
        let (worker, pending) =
            match select(barrier.blocked().boxed_local(), call.invoke().boxed_local()).await {
                Either::Left(result) => result,
                Either::Right((result, _)) => {
                    panic!("ingress finished before its write barrier: {result:?}")
                }
            };
        let current = if replace {
            service.policies.bind(app.clone()).unwrap()
        } else {
            binding.clone()
        };
        current
            .begin_refresh()
            .unwrap()
            .install(
                PolicySnapshot::lease(
                    1.try_into().unwrap(),
                    AppPolicy::default(),
                    Instant::now() + Duration::from_secs(30),
                )
                .unwrap(),
            )
            .unwrap();
        assert!(matches!(
            delayed.install(leased_policy(1, AppPolicy::default())),
            Err(WorkflowServiceError::Unavailable(_))
        ));
        assert!(matches!(
            compio::time::timeout(Duration::from_secs(3), pending)
                .await
                .unwrap(),
            Err(WorkflowServiceError::Unavailable(_))
        ));
        barrier.rolled_back(worker).await;
        barrier.unlock().await;
        barrier.remove().await;
        assert_eq!(ingress_state(&service, &app).await, before);
        let accepted = service
            .bind_app(&current)
            .unwrap()
            .start(&call.request, "Example", StartOptions::default())
            .await
            .unwrap();
        assert_eq!(
            service
                .bind_app(&current)
                .unwrap()
                .start(&call.request, "Example", StartOptions::default())
                .await
                .unwrap(),
            accepted
        );
        current
            .begin_refresh()
            .unwrap()
            .install(leased_policy(1, AppPolicy::default()))
            .unwrap();
    }
}

#[compio::test]
async fn postgres_ingress_expiry_cancels_blocked_write_without_refreshing_the_attempt() {
    use futures::{
        future::{select, Either},
        FutureExt,
    };
    let fixture = PostgresFixture::start().await;
    let (service, app, _, _deployments) = registered_service(Rc::new(fixture.store.clone())).await;
    let service = signed_ingress(service);
    for (offset, (operation, refresh)) in [
        IngressOperation::Start,
        IngressOperation::IssueToken,
        IngressOperation::RevokeAppTokens,
    ]
    .into_iter()
    .flat_map(|operation| [(operation, false), (operation, true)])
    .enumerate()
    {
        let call = IngressCall::prepare(&service, &app, operation).await;
        let previous_epoch = call.epoch().await;
        let before = ingress_state(&service, &app).await;
        let barrier = IngressBarrier::install(&fixture.admin_url).await;
        let revision = 2 + i64::try_from(offset).unwrap() * 2;
        let deadline = Instant::now() + Duration::from_secs(3);
        service
            .policies
            .fixture_install(
                &app,
                PolicySnapshot::lease(revision.try_into().unwrap(), AppPolicy::default(), deadline)
                    .unwrap(),
            )
            .unwrap();
        let (worker, pending) =
            match select(barrier.blocked().boxed_local(), call.invoke().boxed_local()).await {
                Either::Left(result) => result,
                Either::Right((result, _)) => {
                    panic!("ingress finished before its write barrier: {result:?}")
                }
            };
        if refresh {
            service
                .policies
                .fixture_install(
                    &app,
                    PolicySnapshot::lease(
                        revision.try_into().unwrap(),
                        AppPolicy::default(),
                        deadline + Duration::from_secs(30),
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        assert!(matches!(
            compio::time::timeout(Duration::from_secs(5), pending)
                .await
                .unwrap(),
            Err(WorkflowServiceError::Unavailable(_))
        ));
        barrier.rolled_back(worker).await;
        barrier.unlock().await;
        barrier.remove().await;
        assert_eq!(ingress_state(&service, &app).await, before);
        assert_eq!(call.epoch().await, previous_epoch);
        if refresh {
            assert!(
                service.policies.fixture_authority(&app).is_ok(),
                "the refreshed host lease remains live"
            );
        }
        service
            .policies
            .fixture_install(&app, leased_policy(revision + 1, AppPolicy::default()))
            .unwrap();
        let accepted = call.invoke().await.unwrap();
        call.assert_token_effect(&accepted, previous_epoch).await;
    }
}

#[compio::test]
async fn sqlite_captured_ingress_deadline_rolls_back_native_transaction() {
    let directory = tempfile::tempdir().unwrap();
    captured_transaction_policy(
        Rc::new(sqlite_store(&directory.path().join("app.sqlite")).await),
        CapturedChange::Extend,
    )
    .await;
}

#[compio::test]
async fn postgres_captured_ingress_deadline_rolls_back_native_transaction() {
    let fixture = PostgresFixture::start().await;
    captured_transaction_policy(Rc::new(fixture.store.clone()), CapturedChange::Extend).await;
}

#[compio::test]
async fn sqlite_captured_ingress_binding_change_rolls_back_native_transaction() {
    let directory = tempfile::tempdir().unwrap();
    let store = Rc::new(sqlite_store(&directory.path().join("app.sqlite")).await);
    for change in [CapturedChange::Shorten, CapturedChange::Replace] {
        captured_transaction_policy(store.clone(), change).await;
    }
}

#[compio::test]
async fn postgres_captured_ingress_binding_change_rolls_back_native_transaction() {
    let fixture = PostgresFixture::start().await;
    for change in [CapturedChange::Shorten, CapturedChange::Replace] {
        captured_transaction_policy(Rc::new(fixture.store.clone()), change).await;
    }
}

#[derive(Clone, Copy)]
enum CapturedChange {
    Extend,
    Shorten,
    Replace,
}

#[expect(
    clippy::future_not_send,
    reason = "native transactions stay on their owning compio thread"
)]
async fn captured_transaction_policy(store: Rc<OrmStore>, change: CapturedChange) {
    use crate::service::{app::lock_app, policy::CapturedPolicy};
    use futures::{
        future::{select, Either},
        FutureExt,
    };
    let (service, app, _, _deployments) = registered_service(store).await;
    let before = ingress_state(&service, &app).await;
    let deadline = Instant::now()
        + Duration::from_secs(match change {
            CapturedChange::Extend => 1,
            _ => 60,
        });
    service
        .policies
        .fixture_install(
            &app,
            PolicySnapshot::lease(2.try_into().unwrap(), AppPolicy::default(), deadline).unwrap(),
        )
        .unwrap();
    let captured = CapturedPolicy::capture(&service.policies.fixture_binding(&app).unwrap());
    let (written, observe) = futures::channel::oneshot::channel();
    let operation = captured.run(async {
        let mut tx = service.begin().await?;
        lock_app(&mut tx, &app).await?;
        captured.check()?;
        journal_update(
            &tx,
            "app_state",
            json!({"app_id":app.as_str()}),
            json!({"signal_epoch":123}),
        )
        .await;
        assert_eq!(
            journal_rows(&tx, "app_state", json!({"app_id":app.as_str()})).await[0]
                .integer("signal_epoch")
                .unwrap(),
            123
        );
        written.send(()).unwrap();
        futures::future::pending::<()>().await;
        tx.commit().await
    });
    let pending = match select(observe.boxed_local(), operation.boxed_local()).await {
        Either::Left((observed, pending)) => {
            observed.unwrap();
            pending
        }
        Either::Right((result, _)) => {
            panic!("transaction ended before its pending continuation: {result:?}")
        }
    };
    let binding = match change {
        CapturedChange::Replace => service.policies.bind(app.clone()).unwrap(),
        _ => service.policies.current_binding(&app).unwrap(),
    };
    let refreshed_until = match change {
        CapturedChange::Extend => deadline + Duration::from_secs(30),
        _ => Instant::now() + Duration::from_secs(30),
    };
    binding
        .begin_refresh()
        .unwrap()
        .install(
            PolicySnapshot::lease(2.try_into().unwrap(), AppPolicy::default(), refreshed_until)
                .unwrap(),
        )
        .unwrap();
    assert!(matches!(
        compio::time::timeout(Duration::from_secs(3), pending)
            .await
            .unwrap(),
        Err(WorkflowServiceError::Unavailable(_))
    ));
    assert!(service.policies.fixture_authority(&app).is_ok());
    assert_eq!(ingress_state(&service, &app).await, before);
}

#[compio::test]
async fn sqlite_ingress_keeps_explicit_disabled_policy_semantics() {
    let directory = tempfile::tempdir().unwrap();
    disabled_ingress_semantics(Rc::new(
        sqlite_store(&directory.path().join("app.sqlite")).await,
    ))
    .await;
}

#[compio::test]
async fn postgres_ingress_keeps_explicit_disabled_policy_semantics() {
    let fixture = PostgresFixture::start().await;
    disabled_ingress_semantics(Rc::new(fixture.store.clone())).await;
}

#[expect(
    clippy::future_not_send,
    reason = "native ingress fixtures stay on their owning compio thread"
)]
async fn disabled_ingress_semantics(store: Rc<OrmStore>) {
    let (service, app, _, _deployments) = registered_service(store).await;
    let service = signed_ingress(service);
    let mut calls = Vec::new();
    for &operation in INGRESS_OPERATIONS {
        calls.push(IngressCall::prepare(&service, &app, operation).await);
    }
    let controls = IngressCall::prepare(&service, &app, IngressOperation::Start).await;
    service
        .policies
        .fixture_install(
            &app,
            leased_policy(
                2,
                AppPolicy {
                    admission: false,
                    dispatch: false,
                    ingress: false,
                    ..AppPolicy::default()
                },
            ),
        )
        .unwrap();
    for call in calls {
        let result = call.invoke().await;
        match call.operation {
            IngressOperation::Signal
            | IngressOperation::Broadcast
            | IngressOperation::RevokeRunTokens
            | IngressOperation::RevokeTopicTokens
            | IngressOperation::RevokeAppTokens => {
                assert!(result.is_ok(), "{:?}: {result:?}", call.operation);
            }
            _ => assert!(
                matches!(result, Err(WorkflowServiceError::PermissionDenied)),
                "{:?}: {result:?}",
                call.operation
            ),
        }
    }
    for operation in [
        crate::operations::RunOperation::Pause,
        crate::operations::RunOperation::Cancel,
    ] {
        assert!(controls
            .scope
            .transition(&RequestId::mint(), &controls.run, operation)
            .await
            .is_ok());
    }
}
