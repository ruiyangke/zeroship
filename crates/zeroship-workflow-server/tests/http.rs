use compio::io::{AsyncRead, AsyncWriteExt};
use ntex::{http::StatusCode, web};
use serde_json::json;
use sha2::{Digest, Sha256};
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

#[path = "http/capability_refresh.rs"]
mod capability_refresh;
#[path = "http/runner.rs"]
mod runner;

fn peer(issuer: &str, key: ServiceSigningKey, keys: ServiceTrustBundle) -> ServiceAuth {
    let issuer = ServiceIssuer::parse(issuer).unwrap();
    let mut keyring = ServiceKeyring::from_parts(issuer, key, keys).unwrap();
    let verifier = ServiceAssertionVerifier::new(
        keyring.take_bundle().unwrap(),
        Arc::new(InMemoryReplayStore::new()),
    );
    ServiceAuth::new(keyring, Arc::new(verifier))
}

async fn rejects_before_reading_body(address: std::net::SocketAddr, path: &str) {
    compio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut stream = compio::net::TcpStream::connect(address).await.unwrap();
        let headers = format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 1024\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(headers.into_bytes()).await.0.unwrap();
        // Deliberately withhold the declared body. Authentication must finish
        // without waiting for the buffered JSON extractor.
        let mut response = Vec::new();
        loop {
            let compio::BufResult(read, bytes) = stream.read(vec![0; 1024]).await;
            let read = read.unwrap();
            assert_ne!(read, 0, "connection closed without an HTTP status");
            response.extend_from_slice(&bytes[..read]);
            if response.windows(2).any(|window| window == b"\r\n") {
                break;
            }
        }
        assert!(response.starts_with(b"HTTP/1.1 401 "), "{response:?}");
    })
    .await
    .expect("unauthenticated request waited for its body");
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
    let objects = tempfile::tempdir().unwrap();
    let storage = zeroship_storage::StorageStore::from_backend(Arc::new(
        zeroship_storage::LocalFs::new(objects.path()),
    ));
    let service = service.with_payload_storage(storage.clone()).unwrap();
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
    let server_identity = Arc::new(ServiceAssertionVerifier::new(
        peer_keys,
        Arc::new(InMemoryReplayStore::new()),
    ));
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
    for path in [
        format!("/v1/apps/{}/workflows/Example/runs", a.as_str()),
        zeroship_core::service_identity::endpoints::WORKFLOW_TASK_POLL
            .path_template()
            .to_owned(),
    ] {
        rejects_before_reading_body(server.addr(), &path).await;
    }
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
                AppOperation::ReadOutput,
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

    let run = app
        .start(&RequestId::mint(), "Example", StartOptions::default())
        .await
        .unwrap();
    let task = tasks.poll().await.unwrap().unwrap();
    let data = bytes::Bytes::from(vec![42; 128 * 1024]);
    let reference = zeroship_workflow::engine::WorkflowOutputRef {
        hash: format!("{:x}", Sha256::digest(&data)),
        size: data.len() as i64,
        content_type: Some("application/octet-stream".into()),
    };
    let request = RequestId::mint();
    let staged = tasks
        .stage_payload(
            &task.id,
            &task.token,
            &request,
            reference.clone(),
            Box::new(zeroship_storage::backend::OnceChunk::new(data.clone())),
        )
        .await
        .unwrap();
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let retry = compio::time::timeout(
        std::time::Duration::from_secs(5),
        replica_tasks.stage_payload(
            &task.id,
            &task.token,
            &request,
            reference.clone(),
            Box::new(NeverSource(dropped.clone())),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(retry, staged);
    compio::time::timeout(std::time::Duration::from_secs(5), async {
        while !dropped.load(std::sync::atomic::Ordering::SeqCst) {
            compio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        stranger
            .read_payload(&task.id, &task.token, &reference)
            .await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    let mut download = tasks
        .read_payload(&task.id, &task.token, &reference)
        .await
        .unwrap();
    let mut received = Vec::new();
    while let Some(chunk) = download.body.next_chunk().await {
        received.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(received, data);
    let done: WorkflowExecution =
        serde_json::from_value(json!({"outcomes":[{"kind":"RunCompleted","outputRef":reference}]}))
            .unwrap();
    tasks.complete(&task.id, &task.token, done).await.unwrap();
    let mut download = app
        .read_payload(
            &run.id,
            task.generation,
            zeroship_workflow::service::PayloadSlot::Output,
        )
        .await
        .unwrap();
    let mut received = Vec::new();
    while let Some(chunk) = download.body.next_chunk().await {
        received.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(received, data);
    assert!(matches!(
        tasks.read_payload(&task.id, &task.token, &reference).await,
        Err(WorkflowServiceError::Conflict(_))
    ));
    runner::check(tasks.clone(), &app).await;
    capability_refresh::check(&endpoint, &a, &control_key, worker_auth, &task, &data).await;
    // A Content-Length response may finish before the server can report a
    // trailing stream error. The client independently checks the digest.
    let objects = storage.namespace(zeroship_storage::Namespace::platform("workflow").unwrap());
    objects
        .put(
            a.as_str(),
            &staged.id,
            &vec![43; data.len()],
            Some("application/octet-stream"),
        )
        .await
        .unwrap();
    let mut corrupt = app
        .read_payload(
            &run.id,
            task.generation,
            zeroship_workflow::service::PayloadSlot::Output,
        )
        .await
        .unwrap();
    let mut rejected = false;
    while let Some(chunk) = corrupt.body.next_chunk().await {
        if chunk.is_err() {
            rejected = true;
            break;
        }
    }
    assert!(
        rejected,
        "corrupt bytes must not finish as a verified payload"
    );
}

struct NeverSource(Arc<std::sync::atomic::AtomicBool>);
impl Drop for NeverSource {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}
#[async_trait::async_trait(?Send)]
impl zeroship_storage::backend::ChunkSource for NeverSource {
    async fn next_chunk(&mut self) -> Option<zeroship_storage::backend::ChunkResult> {
        std::future::pending().await
    }
}
