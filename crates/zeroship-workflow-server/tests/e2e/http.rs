//! Real coordinator processes share metadata and assertion replay state.
#![allow(
    clippy::future_not_send,
    reason = "native HTTP clients run on the ntex compio test runtime"
)]

use crate::support::{holds, platform, policy as policy_fixture, provision, server_process};

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
        Assignment, ManageRun, ManagementOperation, ManagementOutcome, RequestId, RunId,
        RunOperation, WorkerId, AUDIENCE,
    },
    workflow_jobs::{
        Delivery, JobOperation, JobOutcome, JobReceipt, JournalSettlement, ManagementCommand,
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
/// `Claimant::Placed` (`Coordinator::claim_job`), and that claimant admits
/// `advance` alone, so a management row is the lane's, and so is its settlement:
/// an outcome reaches the queue from the journal that applied the command, never
/// from the worker a lease names.
///
/// A case that shows the worker refused at the settle route builds its lane with
/// the placed worker's own id, so the request is refused by the route rather
/// than by this client's own identity check. Its asserted revision is `1`, which
/// is the revision `seed_placement` records.
async fn sweep(
    queue: &Queue,
    lane: &MaintenanceAuthority,
    scope: &zeroship_core::workflow_coordination::AssignedScope,
) -> Delivery {
    let delivery = claimed_sweep(queue, lane)
        .await
        .expect("the queue holds a sweep for the lane to claim");
    assert_eq!(delivery.job.app_id, scope.app_id);
    assert_eq!(delivery.assignment_revision, scope.assignment_revision);
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

#[ntex::test]
async fn native_worker_client_uses_the_authenticated_coordinator_api() {
    use std::{num::NonZeroU32, sync::Arc};
    use zeroship_core::{
        service_assertion::{ServiceTrustBundle, TransportAssertionVerifier},
        service_peers::{ServiceAuth, ServiceKeyring},
        workflow_coordination::{
            AssignedScope, FailureCode, RegisterWorker, ReleaseReason, ReleaseScope, ScopePage,
            WorkerState,
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
    // No sweep lane on this host. The subject is the native client's own use of
    // the authenticated protocol, and it asserts what the queue offers a placed
    // worker -- including that a leased sweep is redelivered to nobody. The lane
    // claims under an authority no placement expiry fences, so a running one
    // would answer those reads instead of the client.
    let mut server = server_process::ServerProcess::without_maintenance_sweeps(
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
    // The client receives only an endpoint and the joined host's signer.
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
    fixture.admin.execute("INSERT INTO zeroship.worker_instances(id,ring_key,public_key,advertise_host,advertise_port,status,join_signer_id,join_token_id,execution_zone_id,expires_at) VALUES($1,$2,$3,'127.0.0.1',8080,'active',$4,'tok_testfixturedefault','ezn_default000000000000000000',now() + interval '1 hour')",
        &[&worker.as_str(), &vec![7u8], &public, &platform::DEFAULT_JOIN_SIGNER_ID]).await.unwrap();
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
        fixture.seed_app(app).await;
    }
    for app in &apps {
        fixture
            .seed_placement(app, &worker, Duration::from_secs(30))
            .await;
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
    verify_native_policy_source(&fixture, &client, &scope).await;
    fixture
        .admin
        .batch_execute("REVOKE UPDATE ON workflow_manager.assignments FROM zeroship_workflow")
        .await
        .unwrap();
    let refused = client.renew(&scope).await;
    fixture
        .admin
        .batch_execute("GRANT UPDATE ON workflow_manager.assignments TO zeroship_workflow")
        .await
        .unwrap();
    assert_eq!(refused, Err(Error::Refused(FailureCode::Unavailable)));
    let restored = client.renew(&scope).await.unwrap();
    assert_eq!(restored.app_id, scope.app_id);
    assert_eq!(restored.worker_id, worker);
    assert_eq!(restored.revision, scope.assignment_revision);
    // A worker claiming for an app it holds no placement on is denied, not told
    // the service is unavailable. `claim` in
    // `crates/zeroship-workflow-server/src/api/jobs.rs` hands `claim_job` an
    // unresolved ceiling so authorization settles first; an `Unavailable` here
    // would tell the worker to retry a scope it can never hold.
    assert_eq!(
        client
            .claim_job::<AppJournal>(&AssignedScope {
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
    server.restart(&http).await;
    let queue = queue(&fixture.runtime_url).await;
    let lane = MaintenanceAuthority::new(scope.app_id.clone(), worker.clone());
    let delivery = sweep(&queue, &lane, &scope).await;
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
    assert!(client.claim_job::<AppJournal>(&scope).await.unwrap().is_none());
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
    assert!(client.claim_job::<AppJournal>(&scope).await.unwrap().is_none());
    let (status, receipt) = post(
        &http,
        &server.url,
        endpoints::WORKFLOW_MANAGEMENT_STATUS.path_template(),
        &assertion(&control, &control_key),
        &json!({"appId":scope.app_id,"requestId":command.request_id}),
    )
    .await;
    assert_eq!(
        (status, receipt),
        (
            StatusCode::OK,
            json!({"appId":scope.app_id,"requestId":command.request_id,"outcome":{"kind":"not_found"}})
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
        &scope,
    )
    .await;

    // Release carries no wake hint and needs no responsible peer: the manager
    // keeps recovery responsibility for the app.
    let release = ReleaseScope {
        request_id: RequestId::mint(),
        app_id: scope.app_id.clone(),
        assignment_revision: scope.assignment_revision,
        reason: ReleaseReason::Relinquished,
    };
    client.release(&release).await.unwrap();
    client.release(&release).await.unwrap();
    assert_eq!(
        client
            .release(&ReleaseScope {
                reason: ReleaseReason::Refused,
                ..release.clone()
            })
            .await
            .unwrap_err(),
        Error::Refused(FailureCode::Conflict)
    );
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
    // Another enrolled instance of the same zone can take the released app.
    let backup = WorkerId::mint();
    let key = ServiceSigningKey::generate();
    fixture.admin.execute("INSERT INTO zeroship.worker_instances(id,ring_key,public_key,advertise_host,advertise_port,status,join_signer_id,join_token_id,execution_zone_id,expires_at) VALUES($1,$2,$3,'127.0.0.1',8080,'active',$4,'tok_testfixturedefault','ezn_default000000000000000000',now() + interval '1 hour')",
        &[&backup.as_str(), &vec![8u8], &key.verifying_key_bytes().to_vec(), &platform::DEFAULT_JOIN_SIGNER_ID]).await.unwrap();
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
    fixture
        .seed_placement(&scope.app_id, &backup, Duration::from_secs(30))
        .await;
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

/// An instance whose Control lease has run out stops authenticating, and the
/// readiness probe covers the column that decides it.
///
/// `assignments` is the endpoint under test because the registry key lookup is
/// its only possible refusal: `api.rs::assignments` authenticates and then
/// lists, reading no zone, app or eligibility row on the way. `register`
/// refuses a lapsed instance through placement eligibility whatever the
/// registry reads, so a refusal there would say nothing about this query.
///
/// Two variables, each moved alone and each with its control. The lease is
/// moved with Control's own column while `status` stays `active` and the
/// registration and assignment rows stay live, so restoring it must bring the
/// same placements back. The grant is then revoked on the lease column alone:
/// `WorkflowAuth::ready` projects exactly what `active_instance` reads, so a
/// revoked column grant must fail readiness rather than pass it and refuse
/// every authenticated worker afterwards.
#[ntex::test]
async fn a_lapsed_instance_lease_refuses_a_worker_and_is_covered_by_readiness() {
    use std::{num::NonZeroU32, sync::Arc};
    use zeroship_core::{
        service_assertion::{ServiceTrustBundle, TransportAssertionVerifier},
        service_peers::{ServiceAuth, ServiceKeyring},
        workflow_coordination::{FailureCode, RegisterWorker, ScopePage, WorkerState},
    };
    use zeroship_workflow_client::{Error, Options, WorkerCoordinator};

    let fixture = platform::Platform::new().await;
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
        &fixture.runtime_url,
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
    fixture.admin.execute("INSERT INTO zeroship.worker_instances(id,ring_key,public_key,advertise_host,advertise_port,status,join_signer_id,join_token_id,execution_zone_id,expires_at) VALUES($1,$2,$3,'127.0.0.1',8080,'active',$4,'tok_testfixturedefault','ezn_default000000000000000000',now() + interval '1 hour')",
        &[&worker.as_str(), &vec![9u8], &public, &platform::DEFAULT_JOIN_SIGNER_ID]).await.unwrap();
    client
        .register(&RegisterWorker {
            capacity: NonZeroU32::new(1).unwrap(),
            state: WorkerState::Ready,
        })
        .await
        .unwrap();
    let app = AppId::mint();
    fixture.seed_app(&app).await;
    fixture
        .seed_placement(&app, &worker, Duration::from_secs(30))
        .await;
    let held = client.assignments(&ScopePage { after: None }).await.unwrap();
    assert_eq!(
        held.iter().map(|a| &a.app_id).collect::<Vec<_>>(),
        vec![&app],
        "a live instance holds the seeded placement"
    );

    // THE VARIABLE: the lease runs out. Nothing else about the row changes -
    // it is still `active`, still registered, still holding the assignment.
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
            .assignments(&ScopePage { after: None })
            .await
            .unwrap_err(),
        Error::Refused(FailureCode::Unauthenticated),
        "a lapsed instance must not authenticate"
    );

    // THE CONTROL, one renewal apart: the same rows answer the same call.
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
    assert_eq!(
        client.assignments(&ScopePage { after: None }).await.unwrap(),
        held,
        "renewing the lease restores the placements the refusal hid"
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
    scope: &zeroship_core::workflow_coordination::AssignedScope,
) {
    use zeroship_core::{
        workflow_coordination::{RestartDeploy, RestartDeployment, RestartOptions},
        workflow_jobs::DeploymentId,
    };
    let deployment = DeploymentId::mint();
    let hash = "a".repeat(64);
    fixture.admin.execute(
        "INSERT INTO zeroship.app_deploys(id,app_id,deploy_hash,manifest_json) VALUES($1,$2,$3,'{}')",
        &[&deployment.as_str(), &scope.app_id.as_str(), &hash],
    ).await.unwrap();
    let command = ManageRun {
        request_id: RequestId::mint(),
        app_id: scope.app_id.clone(),
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
    let lane = MaintenanceAuthority::new(scope.app_id.clone(), client.worker_id().clone());
    let delivery = sweep(&queue, &lane, scope).await;
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

async fn verify_native_policy_source(
    fixture: &platform::Platform,
    client: &zeroship_workflow_client::WorkerCoordinator,
    scope: &zeroship_core::workflow_coordination::AssignedScope,
) {
    use std::time::Instant;
    assert!(matches!(
        client.policy_lease(&plain_request(scope)).await,
        Err(zeroship_workflow_client::Error::Refused(
            zeroship_core::workflow_coordination::FailureCode::Unavailable
        ))
    ));
    let plan = policy_fixture::seed_app(fixture, &scope.app_id).await;
    let operator = policy_fixture::operator(fixture).await;
    let plans = policy_fixture::plan_admin(fixture).await;
    let policy = AppPolicy {
        admission: false,
        ..AppPolicy::default()
    };
    plans.set_plan_policy(&plan, &policy).await.unwrap();
    operator.set_rollout(policy_fixture::rollout()).await.unwrap();
    let leased = client.policy_lease(&plain_request(scope)).await.unwrap();
    assert_eq!(leased.policy(), &policy);
    assert_eq!(leased.app_id(), &scope.app_id);
    assert_eq!(leased.worker_id(), client.worker_id());
    assert_eq!(leased.signing_key_id(), client.signing_key_id());
    assert!(leased.expires_at() > Instant::now());
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
    // No sweep lane on either replica. The subject is that two hosts authenticate
    // and serve the SAME metadata for the same delivery, so the case has to hold
    // one delivery and present it to both. Each replica's lane would claim under
    // its own identity, and the settle route authorizes on the delivery's worker
    // id, so a running lane makes the delivery unavailable to the case at all.
    let mut first = server_process::ServerProcess::without_maintenance_sweeps(
        &fixture.runtime_url,
        &peers,
        fixture.work.path(),
        "first",
        &client,
    )
    .await;
    let mut second = server_process::ServerProcess::without_maintenance_sweeps(
        &fixture.runtime_url,
        &peers,
        fixture.work.path(),
        "second",
        &client,
    )
    .await;
    for endpoint in [
        endpoints::WORKFLOW_VERIFY_ASSIGNMENT,
        endpoints::WORKFLOW_MANAGE,
        endpoints::WORKFLOW_MANAGEMENT_STATUS,
        endpoints::WORKFLOW_REGISTER,
        endpoints::WORKFLOW_ASSIGNMENTS,
        endpoints::WORKFLOW_RENEW,
        endpoints::WORKFLOW_RELEASE,
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
    fixture.admin.execute("INSERT INTO zeroship.worker_instances(id,ring_key,public_key,advertise_host,advertise_port,status,join_signer_id,join_token_id,execution_zone_id,expires_at) VALUES($1,$2,$3,'127.0.0.1',8080,'active',$4,'tok_testfixturedefault','ezn_default000000000000000000',now() + interval '1 hour')",
        &[&worker.as_str(),&vec![1u8],&worker_key.verifying_key_bytes().to_vec(),&platform::DEFAULT_JOIN_SIGNER_ID]).await.unwrap();
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
    provision::provision(&fixture, &app, &AppPolicy::default()).await;
    let assignment = fixture
        .seed_placement(&app, &worker, Duration::from_secs(30))
        .await;
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
    // Same contract as the earlier claim_job case in this file, over HTTP: a
    // foreign scope is 403, never 503.
    assert_eq!(
        post(
            &client,
            &first.url,
            endpoints::WORKFLOW_JOB_CLAIM.path_template(),
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
    // Management is a sweep, so the lane claims it; the wire claim offers this
    // placed worker nothing. What the replicas are measured on starts at the
    // settle route below, which either of them serves for the same delivery.
    assert_eq!(
        post(
            &client,
            &first.url,
            endpoints::WORKFLOW_JOB_CLAIM.path_template(),
            &assertion(&worker_issuer, &worker_key),
            &scope,
        )
        .await,
        (StatusCode::OK, Value::Null)
    );
    let assigned = zeroship_core::workflow_coordination::AssignedScope {
        app_id: app.clone(),
        assignment_revision: assignment.revision,
    };
    let queue = queue(&fixture.runtime_url).await;
    let lane = MaintenanceAuthority::new(app.clone(), worker.clone());
    let delivery = sweep(&queue, &lane, &assigned).await;
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
    for path in ["/v1/management/poll", "/v1/management/acknowledge"] {
        let response = client
            .post(format!("{}{path}", first.url))
            .header("authorization", assertion(&worker_issuer, &worker_key))
            .send_json(&scope)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    // The wake-hint route is gone, and a release carries a closed reason
    // instead of a hint; the last owner may release.
    assert_eq!(
        post(
            &client,
            &first.url,
            "/v1/wake-hints/publish",
            &assertion(&worker_issuer, &worker_key),
            &json!({"appId":app,"assignmentRevision":assignment.revision,"revision":1})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let hinted = json!({"appId":app,"requestId":RequestId::mint(),"assignmentRevision":assignment.revision,"wakeRevision":1});
    assert_eq!(
        post(
            &client,
            &first.url,
            endpoints::WORKFLOW_RELEASE.path_template(),
            &assertion(&worker_issuer, &worker_key),
            &hinted
        )
        .await,
        (StatusCode::BAD_REQUEST, json!({"code":"invalid"}))
    );
    let release = json!({"appId":app,"requestId":RequestId::mint(),"assignmentRevision":assignment.revision,"reason":"relinquished"});
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
        StatusCode::OK
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

fn plain_request(
    scope: &zeroship_core::workflow_coordination::AssignedScope,
) -> zeroship_core::workflow_policy::PolicyLeaseRequest {
    zeroship_core::workflow_policy::PolicyLeaseRequest {
        scope: scope.clone(),
        establish: None,
        ingress_used: false,
    }
}
