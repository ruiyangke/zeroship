use ntex::{http::StatusCode, web};
use serde_json::json;
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    GenericImage, ImageExt,
};
use zeroship_core::{
    app_id::AppId,
    service_assertion::{
        InMemoryReplayStore, ServiceAssertionVerifier, ServiceIssuer, ServiceSigningKey,
        ServiceTrustBundle,
    },
    service_peers::{ServiceAuth, ServiceKeyring},
    typed_id,
};
use zeroship_workflow::{
    operations::{RunOperation, RunState, SignalOptions, StartOptions},
    service::{
        capability::{mint_app_capability, AppGrant, AppOperation, WORKFLOW_AUDIENCE},
        schema,
        store::PostgresStore,
        AppPolicy, DeployRegistration, RequestId, WorkflowEndpoint, WorkflowService,
    },
    WorkflowExecution, WorkflowServiceError,
};
use zeroship_workflow_server::{
    auth::{PostgresWorkerRegistry, WorkflowAuth},
    WorkflowHttpState,
};

fn peer(issuer: &str, key: ServiceSigningKey, keys: ServiceTrustBundle) -> ServiceAuth {
    let issuer = ServiceIssuer::parse(issuer).unwrap();
    let mut keyring = ServiceKeyring::from_parts(issuer, key, keys).unwrap();
    let verifier = ServiceAssertionVerifier::new(
        keyring.take_bundle().unwrap(),
        Arc::new(InMemoryReplayStore::new()),
    );
    ServiceAuth::new(keyring, Arc::new(verifier))
}

#[ntex::test]
async fn remote_clients_obey_app_capabilities_and_registered_task_ownership() {
    let container = GenericImage::new("postgres", "18")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_HOST_AUTH_METHOD", "trust")
        .start()
        .unwrap();
    let dsn = format!(
        "postgres://postgres@{}:{}/postgres",
        container.get_host().unwrap(),
        container.get_host_port_ipv4(5432).unwrap()
    );
    let (pg, connection) = compio_postgres::connect(&dsn, compio_postgres::NoTls)
        .await
        .unwrap();
    compio::runtime::spawn(async move {
        connection.run().await.unwrap();
    })
    .detach();
    pg.batch_execute("CREATE SCHEMA workflow; CREATE SCHEMA zeroship; CREATE TABLE zeroship.worker_instances (id TEXT PRIMARY KEY, public_key BYTEA NOT NULL, status TEXT NOT NULL)").await.unwrap();
    pg.batch_execute(schema::POSTGRES_SQL).await.unwrap();
    let pg = Arc::new(pg);
    let service = WorkflowService::open(Arc::new(PostgresStore::new(dsn)))
        .await
        .unwrap();
    let a = AppId::mint();
    let b = AppId::mint();
    for app in [&a, &b] {
        service
            .register_app(app, &AppPolicy::default())
            .await
            .unwrap();
        service
            .activate_deploy(
                app,
                &DeployRegistration {
                    id: typed_id::generate("dep"),
                    hash: "a".repeat(64),
                    workflows: ["Example".into()].into(),
                    schedules: Vec::new(),
                },
            )
            .await
            .unwrap();
    }
    let control_key = ServiceSigningKey::generate();
    let mut keys = ServiceTrustBundle::new();
    keys.trust_signing_key(
        &ServiceIssuer::parse("spiffe://zeroship.ai/svc/control").unwrap(),
        control_key.key_id(),
        &control_key,
    )
    .unwrap();
    let mut peer_keys = ServiceTrustBundle::new();
    peer_keys
        .trust_signing_key(
            &ServiceIssuer::parse("spiffe://zeroship.ai/svc/control").unwrap(),
            control_key.key_id(),
            &control_key,
        )
        .unwrap();
    let server_identity = peer(WORKFLOW_AUDIENCE, ServiceSigningKey::generate(), peer_keys);
    let state = Arc::new(WorkflowHttpState {
        service,
        auth: WorkflowAuth::new(
            keys,
            server_identity,
            Arc::new(PostgresWorkerRegistry::new(pg.clone())),
            Arc::new(InMemoryReplayStore::new()),
        ),
    });
    let state_a = state.clone();
    let server = web::test::server(move || {
        let state = state_a.clone();
        async move {
            web::App::new()
                .state(state)
                .configure(zeroship_workflow_server::configure)
        }
    })
    .await;
    let state_b = state.clone();
    let replica = web::test::server(move || {
        let state = state_b.clone();
        async move {
            web::App::new()
                .state(state)
                .configure(zeroship_workflow_server::configure)
        }
    })
    .await;
    let endpoint = WorkflowEndpoint::new(&server.url("")).unwrap();
    let other_endpoint = WorkflowEndpoint::new(&replica.url("")).unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let token = mint_app_capability(
        &control_key,
        AppGrant {
            app_id: a.clone(),
            operations: [
                AppOperation::Start,
                AppOperation::Status,
                AppOperation::Signal,
                AppOperation::Control,
            ]
            .into(),
        },
        now,
        120,
    )
    .unwrap();
    let app = endpoint.for_app(a.clone(), token.clone());
    let other_replica = other_endpoint.for_app(a.clone(), token.clone());
    let foreign = endpoint.for_app(b, token.clone());
    let request = RequestId::mint();
    let run = app
        .start(&request, "Example", StartOptions::default())
        .await
        .unwrap();
    assert_eq!(
        run,
        other_replica
            .start(&request, "Example", StartOptions::default())
            .await
            .unwrap()
    );
    assert!(matches!(
        foreign.status(&run.id).await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert!(matches!(
        foreign
            .start(&RequestId::mint(), "Example", StartOptions::default())
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert!(matches!(
        app.broadcast(
            &RequestId::mint(),
            "news",
            SignalOptions {
                signal_type: "ready".into(),
                payload: json!(null)
            }
        )
        .await,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    let anonymous = server
        .get(format!("/v1/apps/{}/workflow-runs/{}", a.as_str(), run.id))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let worker_key = ServiceSigningKey::generate();
    let instance = typed_id::generate(typed_id::WORKER_INSTANCE_PREFIX);
    let worker_issuer = format!("spiffe://zeroship.ai/svc/worker/{instance}");
    let public = worker_key.verifying_key_bytes().to_vec();
    let worker_auth = Arc::new(peer(&worker_issuer, worker_key, ServiceTrustBundle::new()));
    let tasks = endpoint.tasks(worker_auth.clone());
    let replica_tasks = other_endpoint.tasks(worker_auth.clone());
    assert!(matches!(
        tasks.poll().await,
        Err(WorkflowServiceError::Unauthenticated)
    ));
    pg.execute(
        "INSERT INTO zeroship.worker_instances (id,public_key,status) VALUES ($1,$2,'active')",
        &[&instance, &public],
    )
    .await
    .unwrap();
    let scoped_poll = server
        .post("/v1/tasks/poll")
        .header(
            "authorization",
            worker_auth
                .authorization_for(&ServiceIssuer::parse(WORKFLOW_AUDIENCE).unwrap())
                .unwrap(),
        )
        .send_json(&json!({"appId":a,"runId":run.id}))
        .await
        .unwrap();
    assert_eq!(scoped_poll.status(), StatusCode::BAD_REQUEST);
    let task = tasks.poll().await.unwrap().unwrap();
    assert_eq!(task.invocation.run_id, run.id);
    let other_key = ServiceSigningKey::generate();
    let other_id = typed_id::generate(typed_id::WORKER_INSTANCE_PREFIX);
    let other_public = other_key.verifying_key_bytes().to_vec();
    pg.execute(
        "INSERT INTO zeroship.worker_instances (id,public_key,status) VALUES ($1,$2,'active')",
        &[&other_id, &other_public],
    )
    .await
    .unwrap();
    let stranger = endpoint.tasks(Arc::new(peer(
        &format!("spiffe://zeroship.ai/svc/worker/{other_id}"),
        other_key,
        ServiceTrustBundle::new(),
    )));
    assert!(matches!(
        stranger.heartbeat(&task.id, &task.token).await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    let role_worker = endpoint.tasks(Arc::new(peer(
        "spiffe://zeroship.ai/svc/worker",
        ServiceSigningKey::generate(),
        ServiceTrustBundle::new(),
    )));
    assert!(matches!(
        role_worker.poll().await,
        Err(WorkflowServiceError::Unauthenticated)
    ));
    let auth = worker_auth
        .authorization_for(&ServiceIssuer::parse(WORKFLOW_AUDIENCE).unwrap())
        .unwrap();
    let first = server
        .post("/v1/tasks/poll")
        .header("authorization", auth.clone())
        .send_json(&json!({}))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let replay = replica
        .post("/v1/tasks/poll")
        .header("authorization", auth)
        .send_json(&json!({}))
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
    app.transition(&RequestId::mint(), &run.id, RunOperation::Pause)
        .await
        .unwrap();
    assert_eq!(
        tasks
            .heartbeat(&task.id, &task.token)
            .await
            .unwrap()
            .control,
        zeroship_workflow::service::ControlIntent::Pause
    );
    tasks.release(&task.id, &task.token).await.unwrap();
    assert!(tasks.poll().await.unwrap().is_none());
    app.transition(&RequestId::mint(), &run.id, RunOperation::Resume)
        .await
        .unwrap();
    let task = tasks.poll().await.unwrap().unwrap();
    pg.execute(
        "UPDATE zeroship.worker_instances SET status='draining' WHERE id=$1",
        &[&instance],
    )
    .await
    .unwrap();
    assert!(matches!(
        tasks.heartbeat(&task.id, &task.token).await,
        Err(WorkflowServiceError::Unauthenticated)
    ));
    pg.execute(
        "UPDATE zeroship.worker_instances SET status='active' WHERE id=$1",
        &[&instance],
    )
    .await
    .unwrap();
    let execution: WorkflowExecution = serde_json::from_value(
        json!({"outcomes":[{"kind":"RunCompleted","output":{"done":true}}]}),
    )
    .unwrap();
    let completed = tasks
        .complete(&task.id, &task.token, execution.clone())
        .await
        .unwrap();
    assert_eq!(
        completed,
        replica_tasks
            .complete(&task.id, &task.token, execution)
            .await
            .unwrap()
    );
    assert_eq!(
        app.status(&run.id).await.unwrap().state,
        RunState::Completed
    );
    assert_eq!(
        app.status(&run.id).await.unwrap().output,
        Some(json!({"done":true}))
    );
    assert!(!format!("{app:?}").contains(token.as_str()));
}
