//! Real coordinator processes share metadata and assertion replay state.
#![allow(
    clippy::future_not_send,
    reason = "native HTTP clients run on the ntex compio test runtime"
)]

use crate::support::{holds, platform, provision, server_process};

use compio::io::{AsyncRead, AsyncWriteExt};
use ntex::{client::Client, http::StatusCode};
use serde_json::{json, Value};
use std::time::Duration;
use zeroship_data_orm::binding::DbBinding;
use zeroship_workflow::service::delivery::AppJournal;
use zeroship_workflow_manager::{
    maintenance::MaintenanceAuthority, Options as QueueOptions, Queue,
};
use zeroship_core::{
    app_id::AppId,
    schema_name::SchemaName,
    service_assertion::{ServiceAssertionMinter, ServiceIssuer, ServiceSigningKey},
    service_identity::endpoints,
    service_peers::{service_issuer, CONTROL_SERVICE_NAME},
    workflow_coordination::{
        ManageRun, ManagementOperation, ManagementOutcome, RequestId, RunId, RunOperation,
        WorkerId, AUDIENCE,
    },
    workflow_jobs::{
        ClaimJobs, Delivery, JobOperation, JobOutcome, JobReceipt, JournalSettlement,
        ManagementCommand,
    },
    workflow_policy::AppPolicy,
};

/// The settlement a management sweep's outcome produced.
fn decided(delivery: Delivery, outcome: JobOutcome) -> JournalSettlement {
    JournalSettlement::from_receipt(
        &JobReceipt {
            job: delivery.job.clone(),
            outcome,
        },
        &delivery,
    )
    .unwrap()
}

/// One zone claim for at most one delivery, from no cursor.
const fn claim_one() -> ClaimJobs {
    ClaimJobs {
        max: std::num::NonZeroU32::MIN,
        wait_ms: std::num::NonZeroU64::new(5_000).unwrap(),
        after: None,
        exclude: Vec::new(),
    }
}

/// The queue the spawned services own, opened a second time in this process so a
/// sweep can be claimed the way the service's own maintenance lane claims one.
async fn queue(url: &str) -> Queue {
    Queue::connect(
        DbBinding::platform(
            "workflow_manager",
            "workflow_manager",
            SchemaName::new("workflow_manager").unwrap(),
        ),
        url,
        QueueOptions::default(),
        holds::client(),
    )
    .await
    .unwrap()
}

/// Take the next sweep off this app's queue, in process, under the authority the
/// service's own maintenance lane asserts.
///
/// THERE IS NO WIRE CLAIM FOR A SWEEP. `WORKFLOW_JOB_CLAIM` claims as
/// `Claimant::Worker`, and that claimant admits `advance` alone, so a management
/// row is the lane's, and so is its settlement: an outcome reaches the queue from
/// the journal that applied the command, never from the worker a lease names.
///
/// A case that shows the worker refused at the settle route builds its lane with
/// the worker's own id, so the request is refused by the route rather than by
/// this client's own identity check.
async fn sweep(queue: &Queue, lane: &MaintenanceAuthority, app: &AppId) -> Delivery {
    let delivery = claimed_sweep(queue, lane)
        .await
        .expect("the queue holds a sweep for the lane to claim");
    assert_eq!(&delivery.job.app_id, app);
    delivery
}

async fn claimed_sweep(queue: &Queue, lane: &MaintenanceAuthority) -> Option<Delivery> {
    lane.claim(queue, Ok(AppPolicy::default().max_delivery_attempts))
        .await
        .unwrap()
        .map(|grant| grant.delivery().clone())
}

fn assertion(issuer: &ServiceIssuer, key: &ServiceSigningKey) -> String {
    format!(
        "Bearer {}",
        ServiceAssertionMinter::new(issuer.clone(), key.key_id(), key)
            .unwrap()
            .mint(&ServiceIssuer::parse(AUDIENCE).unwrap())
            .unwrap()
    )
}

/// Enroll an instance in the deployment's one zone under the fixture's join
/// signer, holding `public` as its key.
async fn enroll(fixture: &platform::Platform, worker: &WorkerId, ring: u8, public: &[u8]) {
    fixture.admin.execute("INSERT INTO zeroship.worker_instances(id,ring_key,public_key,advertise_host,advertise_port,status,join_signer_id,join_token_id,execution_zone_id,expires_at) VALUES($1,$2,$3,'127.0.0.1',8080,'active',$4,'tok_testfixturedefault',$5,now() + interval '1 hour')",
        &[&worker.as_str(), &vec![ring], &public.to_vec(), &platform::DEFAULT_JOIN_SIGNER_ID, &platform::DEFAULT_ZONE_ID]).await.unwrap();
}

#[ntex::test]
async fn native_worker_client_uses_the_authenticated_coordinator_api() {
    use std::sync::Arc;
    use zeroship_core::{
        service_assertion::{ServiceTrustBundle, TransportAssertionVerifier},
        service_peers::{ServiceAuth, ServiceKeyring},
        workflow_coordination::FailureCode,
    };
    use zeroship_workflow_client::{Error, Options, WorkerCoordinator};

    // The spawned process composes the manager driver, which enumerates the
    // whole queue, and a zone claim pages the whole zone, so this case gets a
    // database of its own.
    let fixture = platform::Platform::fresh_database().await;
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
    // No sweep lane on this host. The subject is the native client's own use of
    // the authenticated protocol, and it asserts what the queue offers a worker
    // -- including that a leased sweep is redelivered to nobody. The lane claims
    // under an authority of its own, so a running one would answer those reads
    // instead of the client.
    let mut server = server_process::ServerProcess::without_maintenance_sweeps(
        &fixture,
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
    // The client receives only an endpoint and the joined host's signer.
    let client = WorkerCoordinator::new(&server.url, auth, Options::default()).unwrap();
    assert_eq!(client.worker_id(), &worker);
    assert_eq!(
        client
            .claim_jobs::<AppJournal>(&claim_one())
            .await
            .unwrap_err(),
        Error::Refused(FailureCode::Unauthenticated)
    );
    enroll(&fixture, &worker, 7, &public).await;
    // Independent requests must mint fresh assertions despite sharing a signer.
    for _ in 0..2 {
        let batch = client.claim_jobs::<AppJournal>(&claim_one()).await.unwrap();
        assert!(batch.deliveries.is_empty());
        assert!(batch.lap_complete, "an empty zone is one complete lap");
    }

    let app = AppId::mint();
    // The app with its plan policy published, the way a deployment provisions
    // it: the settle route reaches the journal under the app's observed policy.
    provision::provision(&fixture, &app, &AppPolicy::default()).await;
    fixture.seed_scope(&app, platform::DEFAULT_ZONE_ID).await;
    let command = ManageRun {
        request_id: RequestId::mint(),
        app_id: app.clone(),
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
    server.restart(&http).await;
    let queue = queue(&fixture.runtime_url).await;
    let lane = MaintenanceAuthority::new(app.clone(), worker.clone());
    let delivery = sweep(&queue, &lane, &app).await;
    assert_eq!(
        delivery.job.operation,
        JobOperation::Management {
            request_id: command.request_id.clone(),
            run_id: command.run_id.clone(),
            revision: 1.try_into().unwrap(),
            command: ManagementCommand::Transition {
                operation: RunOperation::Cancel
            },
        }
    );
    // The client claims creator work, and this queue holds none. The leased
    // sweep is not redelivered to the lane that holds it either.
    assert!(client
        .claim_jobs::<AppJournal>(&claim_one())
        .await
        .unwrap()
        .deliveries
        .is_empty());
    assert!(claimed_sweep(&queue, &lane).await.is_none());
    // The worker the sweep was leased to cannot settle it: nothing has applied
    // the command, so the journal holds no receipt to settle it from, and the
    // settlement names no outcome of its own.
    assert_eq!(
        client.settle_committed(&delivery).await.unwrap_err(),
        Error::Refused(FailureCode::Conflict)
    );
    let settlement = decided(
        delivery,
        JobOutcome::Management {
            outcome: ManagementOutcome::NotFound {},
        },
    );
    let receipt = lane.settle(&queue, &settlement).await.unwrap();
    assert_eq!(lane.settle(&queue, &settlement).await.unwrap(), receipt);
    assert!(client
        .claim_jobs::<AppJournal>(&claim_one())
        .await
        .unwrap()
        .deliveries
        .is_empty());
    let (status, receipt) = post(
        &http,
        &server.url,
        endpoints::WORKFLOW_MANAGEMENT_STATUS.path_template(),
        &assertion(&control, &control_key),
        &json!({"appId":app,"requestId":command.request_id}),
    )
    .await;
    assert_eq!(
        (status, receipt),
        (
            StatusCode::OK,
            json!({"appId":app,"requestId":command.request_id,"outcome":{"kind":"not_found"}})
        )
    );
    let changed = decided(
        settlement.delivery().clone(),
        JobOutcome::Management {
            outcome: ManagementOutcome::Conflict {},
        },
    );
    assert_eq!(
        lane.settle(&queue, &changed).await.unwrap_err(),
        zeroship_workflow_manager::Error::Conflict
    );

    verify_latest_management(
        &fixture,
        &http,
        &server,
        &client,
        &control,
        &control_key,
        &app,
    )
    .await;

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
            .claim_jobs::<AppJournal>(&claim_one())
            .await
            .unwrap_err(),
        Error::Refused(FailureCode::Unauthenticated)
    );
}

/// An instance whose Control lease has run out stops authenticating, and the
/// readiness probe covers the column that decides it.
///
/// A claim over an empty zone is the call under test because the registry key
/// lookup is its only possible refusal: the claim authenticates and then pages
/// its zone, which holds nothing here, so it reads no app on the way.
///
/// Two variables, each moved alone and each with its control. The lease is
/// moved with Control's own column while `status` stays `active`, so restoring
/// it must bring the same answer back. The grant is then revoked on the lease
/// column alone: `WorkflowAuth::ready` projects exactly what `active_instance`
/// reads, so a revoked column grant must fail readiness rather than pass it and
/// refuse every authenticated worker afterwards.
#[ntex::test]
async fn a_lapsed_instance_lease_refuses_a_worker_and_is_covered_by_readiness() {
    use std::sync::Arc;
    use zeroship_core::{
        service_assertion::{ServiceTrustBundle, TransportAssertionVerifier},
        service_peers::{ServiceAuth, ServiceKeyring},
        workflow_coordination::FailureCode,
    };
    use zeroship_workflow_client::{Error, Options, WorkerCoordinator};

    // The spawned process composes the manager driver and sweep lane, which
    // enumerate the whole queue, so this case gets a database of its own.
    let fixture = platform::Platform::fresh_database().await;
    let control = service_issuer(CONTROL_SERVICE_NAME).unwrap();
    let control_key = ServiceSigningKey::generate();
    let peers = fixture.work.path().join("lease-fence-peers.json");
    platform::write_private(
        &peers,
        serde_json::to_vec(&json!({"keys":[{
            "iss":control.as_str(),"x":control_key.public_jwk_x()
        }]}))
        .unwrap(),
    );
    let http = Client::new().await;
    let server = server_process::ServerProcess::start(
        &fixture,
        &peers,
        fixture.work.path(),
        "lease-fence",
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
    let client = WorkerCoordinator::new(&server.url, auth, Options::default()).unwrap();
    enroll(&fixture, &worker, 9, &public).await;
    let held = client.claim_jobs::<AppJournal>(&claim_one()).await.unwrap();
    assert!(held.deliveries.is_empty(), "the zone holds no work");

    // THE VARIABLE: the lease runs out. Nothing else about the row changes -
    // it is still `active` and still in its zone.
    assert_eq!(
        fixture
            .admin
            .execute(
                "UPDATE zeroship.worker_instances SET expires_at=now() - interval '1 hour' \
                 WHERE id=$1",
                &[&worker.as_str()],
            )
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        client
            .claim_jobs::<AppJournal>(&claim_one())
            .await
            .unwrap_err(),
        Error::Refused(FailureCode::Unauthenticated),
        "a lapsed instance must not authenticate"
    );

    // THE CONTROL, one renewal apart: the same row answers the same call.
    assert_eq!(
        fixture
            .admin
            .execute(
                "UPDATE zeroship.worker_instances SET expires_at=now() + interval '1 hour' \
                 WHERE id=$1",
                &[&worker.as_str()],
            )
            .await
            .unwrap(),
        1
    );
    let restored = client.claim_jobs::<AppJournal>(&claim_one()).await.unwrap();
    assert!(
        restored.deliveries.is_empty() && restored.lap_complete,
        "renewing the lease restores the answer the refusal hid"
    );

    // THE SECOND VARIABLE: the grant on the lease column alone.
    fixture
        .admin
        .batch_execute("REVOKE SELECT (expires_at) ON zeroship.worker_instances FROM zeroship_workflow")
        .await
        .unwrap();
    let refused = http
        .get(format!("{}/readyz", server.url))
        .send()
        .await
        .unwrap()
        .status();
    fixture
        .admin
        .batch_execute("GRANT SELECT (expires_at) ON zeroship.worker_instances TO zeroship_workflow")
        .await
        .unwrap();
    assert_eq!(
        refused,
        StatusCode::SERVICE_UNAVAILABLE,
        "readiness must project the column authentication decides on"
    );
    assert_eq!(
        http.get(format!("{}/readyz", server.url))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

async fn verify_latest_management(
    fixture: &platform::Platform,
    http: &Client,
    server: &server_process::ServerProcess,
    client: &zeroship_workflow_client::WorkerCoordinator,
    control: &ServiceIssuer,
    key: &ServiceSigningKey,
    app: &AppId,
) {
    use zeroship_core::{
        workflow_coordination::{RestartDeploy, RestartDeployment, RestartOptions},
        workflow_jobs::DeploymentId,
    };
    let deployment = DeploymentId::mint();
    let hash = "a".repeat(64);
    fixture.admin.execute(
        "INSERT INTO zeroship.app_deploys(id,app_id,deploy_hash,manifest_json) VALUES($1,$2,$3,'{}')",
        &[&deployment.as_str(), &app.as_str(), &hash],
    ).await.unwrap();
    let command = ManageRun {
        request_id: RequestId::mint(),
        app_id: app.clone(),
        run_id: RunId::mint(),
        command: ManagementOperation::Restart {
            options: RestartOptions {
                from: None,
                deploy: Some(RestartDeploy::Latest),
            },
            deployment: Some(RestartDeployment {
                deployment_id: deployment.clone(),
                deploy_hash: hash.clone(),
            }),
        },
    };
    let request = serde_json::to_value(&command).unwrap();
    let accepted = post(
        http,
        &server.url,
        endpoints::WORKFLOW_MANAGE.path_template(),
        &assertion(control, key),
        &request,
    )
    .await;
    assert_eq!(accepted.0, StatusCode::OK);
    // The accepted command is frozen: closing the deployment to new holds does
    // not change the receipt an exact retry replays.
    fixture
        .admin
        .execute(
            "UPDATE zeroship.app_deploys SET retention_state='reclaiming' WHERE id=$1",
            &[&deployment.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(
        post(
            http,
            &server.url,
            endpoints::WORKFLOW_MANAGE.path_template(),
            &assertion(control, key),
            &request
        )
        .await,
        accepted
    );
    // Management is a sweep: the lane claims it and settles it in process, as the
    // service's own lane does.
    let queue = queue(&fixture.runtime_url).await;
    let lane = MaintenanceAuthority::new(app.clone(), client.worker_id().clone());
    let delivery = sweep(&queue, &lane, app).await;
    assert_eq!(
        delivery.job.operation,
        JobOperation::Management {
            request_id: command.request_id,
            run_id: command.run_id,
            revision: 1.try_into().unwrap(),
            command: ManagementCommand::RestartLatest {
                deployment_id: deployment
            },
        }
    );
    lane.settle(
        &queue,
        &decided(
            delivery,
            JobOutcome::Management {
                outcome: ManagementOutcome::Denied {},
            },
        ),
    )
    .await
    .unwrap();
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
    // The spawned process composes the manager driver and sweep lane, which
    // enumerate the whole queue, so this case gets a database of its own.
    let fixture = platform::Platform::fresh_database().await;
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
    // No sweep lane on either replica. The subject is that two hosts authenticate
    // and serve the SAME metadata for the same delivery, so the case has to hold
    // one delivery and present it to both. Each replica's lane would claim under
    // its own identity, and the settle route authorizes on the delivery's worker
    // id, so a running lane makes the delivery unavailable to the case at all.
    let mut first = server_process::ServerProcess::without_maintenance_sweeps(
        &fixture,
        &peers,
        fixture.work.path(),
        "first",
        &client,
    )
    .await;
    let mut second = server_process::ServerProcess::without_maintenance_sweeps(
        &fixture,
        &peers,
        fixture.work.path(),
        "second",
        &client,
    )
    .await;
    for endpoint in [
        endpoints::WORKFLOW_MANAGE,
        endpoints::WORKFLOW_MANAGEMENT_STATUS,
        endpoints::WORKFLOW_JOB_CLAIM,
        endpoints::WORKFLOW_JOB_SETTLE,
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
    let claim = serde_json::to_value(claim_one()).unwrap();
    let claim_path = endpoints::WORKFLOW_JOB_CLAIM.path_template();
    assert_eq!(
        post(
            &client,
            &first.url,
            claim_path,
            &assertion(&worker_issuer, &worker_key),
            &claim
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    enroll(&fixture, &worker, 1, &worker_key.verifying_key_bytes()).await;
    assert_eq!(
        post(
            &client,
            &first.url,
            claim_path,
            &assertion(&worker_issuer, &control_key),
            &claim
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post(
            &client,
            &first.url,
            claim_path,
            &assertion(&control, &control_key),
            &claim
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let token = assertion(&worker_issuer, &worker_key);
    let (status, claimed) = post(&client, &first.url, claim_path, &token, &claim).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(claimed["deliveries"], json!([]));
    // One assertion is one request, whichever replica it is presented to.
    assert_eq!(
        post(&client, &second.url, claim_path, &token, &claim)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let mut injected = claim.clone();
    injected["workerId"] = json!(WorkerId::mint());
    assert_eq!(
        post(
            &client,
            &first.url,
            claim_path,
            &assertion(&worker_issuer, &worker_key),
            &injected
        )
        .await,
        (StatusCode::BAD_REQUEST, json!({"code":"invalid"}))
    );

    let app = AppId::mint();
    provision::provision(&fixture, &app, &AppPolicy::default()).await;
    fixture.seed_scope(&app, platform::DEFAULT_ZONE_ID).await;
    let (status, claimed) = post(
        &client,
        &second.url,
        claim_path,
        &assertion(&worker_issuer, &worker_key),
        &claim,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(claimed["deliveries"], json!([]));
    for field in ["input", "history", "databaseUrl", "payloadUrl", "taskToken"] {
        let mut injected = claim.clone();
        injected[field] = json!("private-customer-data");
        assert_eq!(
            post(
                &client,
                &first.url,
                claim_path,
                &assertion(&worker_issuer, &worker_key),
                &injected
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    let mut oversized = claim.clone();
    oversized["input"] = json!("x".repeat(2048));
    assert_eq!(
        post(
            &client,
            &first.url,
            claim_path,
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
    // Management is a sweep, so the lane claims it; the wire claim offers this
    // worker nothing. What the replicas are measured on starts at the settle
    // route below, which either of them serves for the same delivery.
    let (status, claimed) = post(
        &client,
        &first.url,
        claim_path,
        &assertion(&worker_issuer, &worker_key),
        &claim,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(claimed["deliveries"], json!([]));
    let queue = queue(&fixture.runtime_url).await;
    let lane = MaintenanceAuthority::new(app.clone(), worker.clone());
    let delivery = sweep(&queue, &lane, &app).await;
    assert_eq!(
        delivery.job.operation,
        JobOperation::Management {
            request_id: request.request_id.clone(),
            run_id: request.run_id.clone(),
            revision: 1.try_into().unwrap(),
            command: ManagementCommand::Transition {
                operation: RunOperation::Pause
            },
        }
    );
    // The worker the sweep was leased to decides nothing about the command, on
    // either replica: a body naming an outcome is refused as a body, and one
    // naming none finds no receipt, because no journal applied the command.
    let settlement = decided(
        delivery,
        JobOutcome::Management {
            outcome: ManagementOutcome::Applied {
                state: zeroship_core::workflow_coordination::RunState::Paused,
            },
        },
    );
    let forged = serde_json::to_value(&settlement).unwrap();
    let mut generic = forged.clone();
    generic["outcome"] = json!({"kind":"completed"});
    for body in [&generic, &forged] {
        assert_eq!(
            post(
                &client,
                &second.url,
                endpoints::WORKFLOW_JOB_SETTLE.path_template(),
                &assertion(&worker_issuer, &worker_key),
                body
            )
            .await,
            (StatusCode::BAD_REQUEST, json!({"code":"invalid"}))
        );
    }
    let committed = json!({"delivery": settlement.delivery()});
    for replica in [&second.url, &first.url] {
        assert_eq!(
            post(
                &client,
                replica,
                endpoints::WORKFLOW_JOB_SETTLE.path_template(),
                &assertion(&worker_issuer, &worker_key),
                &committed
            )
            .await,
            (StatusCode::CONFLICT, json!({"code":"conflict"}))
        );
    }
    assert_eq!(
        post(
            &client,
            &first.url,
            endpoints::WORKFLOW_MANAGEMENT_STATUS.path_template(),
            &assertion(&control, &control_key),
            &json!({"appId":app,"requestId":request.request_id})
        )
        .await,
        (
            StatusCode::OK,
            json!({"appId":app,"requestId":request.request_id,"outcome":null})
        )
    );
    // The lane settles it in process, as the service's own lane does, and an
    // exact retry replays the receipt.
    let receipt = lane.settle(&queue, &settlement).await.unwrap();
    assert_eq!(lane.settle(&queue, &settlement).await.unwrap(), receipt);
    let management_receipt = json!({"appId":app,"requestId":request.request_id,"outcome":{"kind":"applied","state":"paused"}});
    assert_eq!(
        post(
            &client,
            &first.url,
            endpoints::WORKFLOW_MANAGE.path_template(),
            &assertion(&control, &control_key),
            &management
        )
        .await,
        (StatusCode::OK, management_receipt.clone())
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
        (StatusCode::OK, management_receipt)
    );
    // No route registers a worker, lists, renews or releases a placement, leases
    // a policy, polls or acknowledges management, or publishes a wake hint: an
    // authenticated worker reaches none of them.
    for path in [
        "/v1/workers/register",
        "/v1/assignments/list",
        "/v1/assignments/renew",
        "/v1/assignments/release",
        "/v1/policy/lease",
        "/v1/management/poll",
        "/v1/management/acknowledge",
        "/v1/wake-hints/publish",
    ] {
        let response = client
            .post(format!("{}{path}", first.url))
            .header("authorization", assertion(&worker_issuer, &worker_key))
            .send_json(&json!({"appId":app}))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
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
            claim_path,
            &assertion(&worker_issuer, &worker_key),
            &claim
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );

    // Scoped to this case's database: on the shared server an unscoped sweep
    // would terminate every other case's workflow connections too.
    fixture.admin.query("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename='zeroship_workflow' AND datname=current_database()",&[]).await.unwrap();
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
