//! Real coordinator processes share metadata and assertion replay state.
#![allow(
    clippy::future_not_send,
    reason = "native HTTP clients run on the ntex compio test runtime"
)]

#[path = "support/platform.rs"]
mod platform;
#[path = "support/server_process.rs"]
mod server_process;

use compio::io::{AsyncRead, AsyncWriteExt};
use ntex::{client::Client, http::StatusCode};
use serde_json::{json, Value};
use std::time::Duration;
use zeroship_core::{
    app_id::AppId,
    service_assertion::{ServiceAssertionMinter, ServiceIssuer, ServiceSigningKey},
    service_identity::endpoints,
    service_peers::{service_issuer, CONTROL_SERVICE_NAME},
    workflow_coordination::{
        Assignment, ManageRun, ManagementOperation, RequestId, RunId, RunOperation, WorkerId,
        AUDIENCE,
    },
};

fn assertion(issuer: &ServiceIssuer, key: &ServiceSigningKey) -> String {
    format!(
        "Bearer {}",
        ServiceAssertionMinter::new(issuer.clone(), key.key_id(), key)
            .unwrap()
            .mint(&ServiceIssuer::parse(AUDIENCE).unwrap())
            .unwrap()
    )
}

#[ntex::test]
async fn native_worker_client_uses_the_authenticated_coordinator_api() {
    use std::{num::NonZeroU32, sync::Arc};
    use zeroship_core::{
        service_assertion::{ServiceTrustBundle, TransportAssertionVerifier},
        service_peers::{ServiceAuth, ServiceKeyring},
        workflow_coordination::{
            AcknowledgeManagement, AssignedScope, FailureCode, ManagementOutcome, PublishWakeHint,
            RegisterWorker, ReleaseScope, ScopePage, WorkerState,
        },
    };
    use zeroship_workflow_client::{Error, Options, WorkerCoordinator};

    let fixture = platform::Platform::new().await;
    let control = service_issuer(CONTROL_SERVICE_NAME).unwrap();
    let control_key = ServiceSigningKey::generate();
    let peers = fixture.work.path().join("native-client-peers.json");
    platform::write_private(
        &peers,
        serde_json::to_vec(&json!({"keys":[{
            "iss":control.as_str(),"x":control_key.public_jwk_x()
        }]}))
        .unwrap(),
    );
    let http = Client::new().await;
    let mut server = server_process::ServerProcess::start(
        &fixture.runtime_url,
        &peers,
        fixture.work.path(),
        "native-client",
        &http,
    )
    .await;

    let worker = WorkerId::mint();
    let key = ServiceSigningKey::generate();
    let public = key.verifying_key_bytes().to_vec();
    let issuer = ServiceIssuer::parse(&format!(
        "spiffe://zeroship.ai/svc/worker/{}",
        worker.as_str()
    ))
    .unwrap();
    let keyring = ServiceKeyring::from_parts(issuer, key, ServiceTrustBundle::new()).unwrap();
    let auth = Arc::new(ServiceAuth::new(
        keyring,
        Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
    ));
    // The client receives only an endpoint and the enrolled host's signer.
    let client = WorkerCoordinator::new(&server.url, auth, Options::default()).unwrap();
    assert_eq!(client.worker_id(), &worker);
    let registration = RegisterWorker {
        capacity: NonZeroU32::new(3).unwrap(),
        state: WorkerState::Ready,
    };
    assert_eq!(
        client.register(&registration).await.unwrap_err(),
        Error::Refused(FailureCode::Unauthenticated)
    );
    fixture.admin.execute("INSERT INTO zeroship.worker_instances(id,ring_key,public_key,advertise_host,advertise_port,status) VALUES($1,$2,$3,'127.0.0.1',8080,'active')",
        &[&worker.as_str(), &vec![7u8], &public]).await.unwrap();
    client.register(&registration).await.unwrap();
    // Independent requests must mint fresh assertions despite sharing a signer.
    client.register(&registration).await.unwrap();
    assert!(client
        .assignments(&ScopePage { after: None })
        .await
        .unwrap()
        .is_empty());

    let mut apps = [AppId::mint(), AppId::mint()];
    apps.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    for app in &apps {
        let (status, _) = post(
            &http,
            &server.url,
            endpoints::WORKFLOW_ASSIGN.path_template(),
            &assertion(&control, &control_key),
            &json!({
                "requestId":RequestId::mint(),"appId":app,"workerId":worker,"expectedRevision":null
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let assignments = client
        .assignments(&ScopePage { after: None })
        .await
        .unwrap();
    assert_eq!(
        assignments.iter().map(|a| &a.app_id).collect::<Vec<_>>(),
        apps.iter().collect::<Vec<_>>()
    );
    let page = client
        .assignments(&ScopePage {
            after: Some(apps[0].clone()),
        })
        .await
        .unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0], assignments[1]);
    let scope = AssignedScope {
        app_id: apps[0].clone(),
        assignment_revision: assignments[0].revision,
    };
    client.renew(&scope).await.unwrap();
    assert_eq!(
        client
            .pending_management(&AssignedScope {
                app_id: AppId::mint(),
                assignment_revision: scope.assignment_revision
            })
            .await
            .unwrap_err(),
        Error::Refused(FailureCode::Denied)
    );

    let command = ManageRun {
        request_id: RequestId::mint(),
        app_id: scope.app_id.clone(),
        run_id: RunId::mint(),
        command: ManagementOperation::Transition {
            operation: RunOperation::Cancel,
        },
    };
    let (status, _) = post(
        &http,
        &server.url,
        endpoints::WORKFLOW_MANAGE.path_template(),
        &assertion(&control, &control_key),
        &serde_json::to_value(&command).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        client.pending_management(&scope).await.unwrap(),
        vec![command.clone()]
    );
    server.restart(&http).await;
    assert_eq!(
        client.pending_management(&scope).await.unwrap(),
        vec![command.clone()]
    );
    let ack = AcknowledgeManagement {
        request_id: command.request_id,
        app_id: scope.app_id.clone(),
        assignment_revision: scope.assignment_revision,
        outcome: ManagementOutcome::NotFound {},
    };
    let receipt = client.acknowledge_management(&ack).await.unwrap();
    assert_eq!(client.acknowledge_management(&ack).await.unwrap(), receipt);
    assert!(client.pending_management(&scope).await.unwrap().is_empty());
    let changed = AcknowledgeManagement {
        outcome: ManagementOutcome::Conflict {},
        ..ack
    };
    assert_eq!(
        client.acknowledge_management(&changed).await.unwrap_err(),
        Error::Refused(FailureCode::Conflict)
    );

    let hint = PublishWakeHint {
        app_id: scope.app_id.clone(),
        assignment_revision: scope.assignment_revision,
        revision: 1.try_into().unwrap(),
        next_due_at: None,
    };
    let receipt = client.publish_wake(&hint).await.unwrap();
    assert_eq!(client.publish_wake(&hint).await.unwrap(), receipt);
    let release = ReleaseScope {
        request_id: RequestId::mint(),
        app_id: scope.app_id.clone(),
        assignment_revision: scope.assignment_revision,
        wake_revision: receipt.revision,
    };
    assert_eq!(
        client.release(&release).await.unwrap_err(),
        Error::Refused(FailureCode::Conflict)
    );
    let backup = WorkerId::mint();
    let key = ServiceSigningKey::generate();
    fixture.admin.execute("INSERT INTO zeroship.worker_instances(id,ring_key,public_key,advertise_host,advertise_port,status) VALUES($1,$2,$3,'127.0.0.1',8080,'active')",
        &[&backup.as_str(), &vec![8u8], &key.verifying_key_bytes().to_vec()]).await.unwrap();
    let issuer = ServiceIssuer::parse(&format!(
        "spiffe://zeroship.ai/svc/worker/{}",
        backup.as_str()
    ))
    .unwrap();
    let keyring = ServiceKeyring::from_parts(issuer, key, ServiceTrustBundle::new()).unwrap();
    let auth = Arc::new(ServiceAuth::new(
        keyring,
        Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
    ));
    let backup_client = WorkerCoordinator::new(&server.url, auth, Options::default()).unwrap();
    backup_client.register(&registration).await.unwrap();
    let (status,_) = post(&http,&server.url,endpoints::WORKFLOW_ASSIGN.path_template(),&assertion(&control,&control_key),&json!({
        "requestId":RequestId::mint(),"appId":scope.app_id,"workerId":backup,"expectedRevision":null
    })).await;
    assert_eq!(status, StatusCode::OK);
    client.release(&release).await.unwrap();
    client.release(&release).await.unwrap();
    assert_eq!(
        client.renew(&scope).await.unwrap_err(),
        Error::Refused(FailureCode::Denied)
    );
    assert_eq!(
        client
            .assignments(&ScopePage { after: None })
            .await
            .unwrap()
            .len(),
        1
    );
    client
        .register(&RegisterWorker {
            state: WorkerState::Draining,
            ..registration
        })
        .await
        .unwrap();

    fixture
        .admin
        .execute(
            "UPDATE zeroship.worker_instances SET status='gone' WHERE id=$1",
            &[&worker.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(
        client
            .assignments(&ScopePage { after: None })
            .await
            .unwrap_err(),
        Error::Refused(FailureCode::Unauthenticated)
    );
}
async fn post(
    client: &Client,
    url: &str,
    path: &str,
    token: &str,
    body: &Value,
) -> (StatusCode, Value) {
    let response = client
        .post(format!("{url}{path}"))
        .header("authorization", token)
        .send_json(body)
        .await
        .unwrap();
    let status = response.status();
    let body = response.body().await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}
async fn rejects_before_body(address: std::net::SocketAddr, path: &str) {
    compio::time::timeout(Duration::from_secs(5),async {
        let mut stream = compio::net::TcpStream::connect(address).await.unwrap();
        stream.write_all(format!("POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 1024\r\nConnection: close\r\n\r\n").into_bytes()).await.0.unwrap();
        let mut response = Vec::new();
        loop {
            let compio::BufResult(read, bytes) = stream.read(vec![0;1024]).await;
            let read = read.unwrap();
            assert_ne!(read,0,"connection ended without an HTTP status");
            response.extend_from_slice(&bytes[..read]);
            if response.windows(2).any(|value| value==b"\r\n") { break; }
        }
        assert!(response.starts_with(b"HTTP/1.1 401 "),"{response:?}");
    }).await.expect("authentication waited for the withheld body");
}

#[ntex::test]
async fn replicas_authenticate_metadata_and_keep_customer_execution_off_the_protocol() {
    let fixture = platform::Platform::new().await;
    let control = service_issuer(CONTROL_SERVICE_NAME).unwrap();
    let control_key = ServiceSigningKey::generate();
    let peers = fixture.work.path().join("peers.json");
    platform::write_private(
        &peers,
        serde_json::to_vec(
            &json!({"keys":[{"iss":control.as_str(),"x":control_key.public_jwk_x()}]}),
        )
        .unwrap(),
    );
    let client = Client::new().await;
    let mut first = server_process::ServerProcess::start(
        &fixture.runtime_url,
        &peers,
        fixture.work.path(),
        "first",
        &client,
    )
    .await;
    let mut second = server_process::ServerProcess::start(
        &fixture.runtime_url,
        &peers,
        fixture.work.path(),
        "second",
        &client,
    )
    .await;
    for endpoint in [
        endpoints::WORKFLOW_WORKERS,
        endpoints::WORKFLOW_ASSIGN,
        endpoints::WORKFLOW_VERIFY_ASSIGNMENT,
        endpoints::WORKFLOW_RECOVERY,
        endpoints::WORKFLOW_MANAGE,
        endpoints::WORKFLOW_MANAGEMENT_STATUS,
        endpoints::WORKFLOW_REGISTER,
        endpoints::WORKFLOW_ASSIGNMENTS,
        endpoints::WORKFLOW_RENEW,
        endpoints::WORKFLOW_RELEASE,
        endpoints::WORKFLOW_WAKE,
        endpoints::WORKFLOW_MANAGEMENT_POLL,
        endpoints::WORKFLOW_MANAGEMENT_ACK,
    ] {
        rejects_before_body(first.address, endpoint.path_template()).await;
    }

    let worker = WorkerId::mint();
    let worker_key = ServiceSigningKey::generate();
    let worker_issuer = ServiceIssuer::parse(&format!(
        "spiffe://zeroship.ai/svc/worker/{}",
        worker.as_str()
    ))
    .unwrap();
    let registration = json!({"capacity":4,"state":"ready"});
    let register = endpoints::WORKFLOW_REGISTER.path_template();
    assert_eq!(
        post(
            &client,
            &first.url,
            register,
            &assertion(&worker_issuer, &worker_key),
            &registration
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    fixture.admin.execute("INSERT INTO zeroship.worker_instances(id,ring_key,public_key,advertise_host,advertise_port,status) VALUES($1,$2,$3,'127.0.0.1',8080,'active')",
        &[&worker.as_str(),&vec![1u8],&worker_key.verifying_key_bytes().to_vec()]).await.unwrap();
    assert_eq!(
        post(
            &client,
            &first.url,
            register,
            &assertion(&worker_issuer, &control_key),
            &registration
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post(
            &client,
            &first.url,
            register,
            &assertion(&control, &control_key),
            &registration
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let token = assertion(&worker_issuer, &worker_key);
    let (status, registered) = post(&client, &first.url, register, &token, &registration).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(registered["workerId"], json!(worker));
    assert_eq!(
        post(&client, &second.url, register, &token, &registration)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let mut injected = registration.clone();
    injected["workerId"] = json!(WorkerId::mint());
    assert_eq!(
        post(
            &client,
            &first.url,
            register,
            &assertion(&worker_issuer, &worker_key),
            &injected
        )
        .await,
        (StatusCode::BAD_REQUEST, json!({"code":"invalid"}))
    );

    let app = AppId::mint();
    let assign = json!({"requestId":RequestId::mint(),"appId":app,"workerId":worker,"expectedRevision":null});
    let path = endpoints::WORKFLOW_ASSIGN.path_template();
    assert_eq!(
        post(
            &client,
            &first.url,
            path,
            &assertion(&worker_issuer, &worker_key),
            &assign
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, value) = post(
        &client,
        &first.url,
        path,
        &assertion(&control, &control_key),
        &assign,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let assignment: Assignment = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(
        post(
            &client,
            &second.url,
            path,
            &assertion(&control, &control_key),
            &assign
        )
        .await,
        (StatusCode::OK, value)
    );
    let verification =
        json!({"appId":app,"workerId":worker,"assignmentRevision":assignment.revision});
    let verify = endpoints::WORKFLOW_VERIFY_ASSIGNMENT.path_template();
    assert_eq!(
        post(
            &client,
            &first.url,
            verify,
            &assertion(&worker_issuer, &worker_key),
            &verification
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, verified) = post(
        &client,
        &second.url,
        verify,
        &assertion(&control, &control_key),
        &verification,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let verified: Assignment = serde_json::from_value(verified).unwrap();
    assert_eq!(verified.app_id, assignment.app_id);
    assert_eq!(verified.worker_id, assignment.worker_id);
    assert_eq!(verified.revision, assignment.revision);
    assert!(verified.expires_at <= assignment.expires_at);
    for (field, value) in [
        ("appId", json!(AppId::mint())),
        ("workerId", json!(WorkerId::mint())),
        ("assignmentRevision", json!(assignment.revision.get() + 1)),
    ] {
        let mut foreign = verification.clone();
        foreign[field] = value;
        assert_eq!(
            post(
                &client,
                &second.url,
                verify,
                &assertion(&control, &control_key),
                &foreign
            )
            .await,
            (StatusCode::FORBIDDEN, json!({"code":"denied"}))
        );
    }
    let mut injected = verification.clone();
    injected["holderId"] = json!("caller-selected");
    assert_eq!(
        post(
            &client,
            &first.url,
            verify,
            &assertion(&control, &control_key),
            &injected
        )
        .await,
        (StatusCode::BAD_REQUEST, json!({"code":"invalid"}))
    );
    let scope = json!({"appId":app,"assignmentRevision":assignment.revision});
    assert_eq!(
        post(
            &client,
            &second.url,
            endpoints::WORKFLOW_RENEW.path_template(),
            &assertion(&worker_issuer, &worker_key),
            &scope
        )
        .await
        .0,
        StatusCode::OK
    );
    let foreign = json!({"appId":AppId::mint(),"assignmentRevision":assignment.revision});
    assert_eq!(
        post(
            &client,
            &first.url,
            endpoints::WORKFLOW_MANAGEMENT_POLL.path_template(),
            &assertion(&worker_issuer, &worker_key),
            &foreign
        )
        .await,
        (StatusCode::FORBIDDEN, json!({"code":"denied"}))
    );
    for field in ["input", "history", "databaseUrl", "payloadUrl", "taskToken"] {
        let mut injected = scope.clone();
        injected[field] = json!("private-customer-data");
        assert_eq!(
            post(
                &client,
                &first.url,
                endpoints::WORKFLOW_RENEW.path_template(),
                &assertion(&worker_issuer, &worker_key),
                &injected
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    let oversized = json!({"capacity":1,"state":"ready","input":"x".repeat(2048)});
    assert_eq!(
        post(
            &client,
            &first.url,
            register,
            &assertion(&worker_issuer, &worker_key),
            &oversized
        )
        .await,
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({"code":"request_too_large"})
        )
    );

    let request = ManageRun {
        request_id: RequestId::mint(),
        app_id: app.clone(),
        run_id: RunId::mint(),
        command: ManagementOperation::Transition {
            operation: RunOperation::Pause,
        },
    };
    let management = serde_json::to_value(&request).unwrap();
    assert_eq!(
        post(
            &client,
            &first.url,
            endpoints::WORKFLOW_MANAGE.path_template(),
            &assertion(&control, &control_key),
            &management
        )
        .await
        .0,
        StatusCode::OK
    );
    first.restart(&client).await;
    let (status, pending) = post(
        &client,
        &first.url,
        endpoints::WORKFLOW_MANAGEMENT_POLL.path_template(),
        &assertion(&worker_issuer, &worker_key),
        &scope,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pending, json!([request]));
    let ack = json!({"appId":app,"requestId":request.request_id,"assignmentRevision":assignment.revision,"outcome":{"kind":"applied","state":"paused"}});
    let (status, receipt) = post(
        &client,
        &second.url,
        endpoints::WORKFLOW_MANAGEMENT_ACK.path_template(),
        &assertion(&worker_issuer, &worker_key),
        &ack,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        post(
            &client,
            &first.url,
            endpoints::WORKFLOW_MANAGE.path_template(),
            &assertion(&control, &control_key),
            &management
        )
        .await,
        (StatusCode::OK, receipt.clone())
    );
    assert_eq!(
        post(
            &client,
            &first.url,
            endpoints::WORKFLOW_MANAGEMENT_STATUS.path_template(),
            &assertion(&control, &control_key),
            &json!({"appId":app,"requestId":request.request_id})
        )
        .await,
        (StatusCode::OK, receipt)
    );
    let hint =
        json!({"appId":app,"assignmentRevision":assignment.revision,"revision":1,"nextDueAt":null});
    assert_eq!(
        post(
            &client,
            &first.url,
            endpoints::WORKFLOW_WAKE.path_template(),
            &assertion(&worker_issuer, &worker_key),
            &hint
        )
        .await
        .0,
        StatusCode::OK
    );
    let release = json!({"appId":app,"requestId":RequestId::mint(),"assignmentRevision":assignment.revision,"wakeRevision":1});
    assert_eq!(
        post(
            &client,
            &first.url,
            endpoints::WORKFLOW_RELEASE.path_template(),
            &assertion(&worker_issuer, &worker_key),
            &release
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    for path in [
        "/v1/tasks/poll".into(),
        format!("/v1/apps/{}/workflows/Example/runs", app.as_str()),
        format!("/v1/apps/{}/workflow-deploy", app.as_str()),
    ] {
        assert_eq!(
            post(
                &client,
                &first.url,
                &path,
                &assertion(&control, &control_key),
                &json!({"input":"private"})
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }
    fixture
        .admin
        .execute(
            "UPDATE zeroship.worker_instances SET status='draining' WHERE id=$1",
            &[&worker.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(
        post(
            &client,
            &second.url,
            register,
            &assertion(&worker_issuer, &worker_key),
            &registration
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );

    fixture.admin.query("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename='zeroship_workflow'",&[]).await.unwrap();
    first.expect_failure().await;
    second.expect_failure().await;
    first.restart(&client).await;
    assert_eq!(
        post(
            &client,
            &first.url,
            endpoints::WORKFLOW_MANAGEMENT_STATUS.path_template(),
            &assertion(&control, &control_key),
            &json!({"appId":app,"requestId":request.request_id})
        )
        .await
        .0,
        StatusCode::OK
    );
}
