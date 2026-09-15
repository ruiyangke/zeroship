//! Queue delivery crosses the authenticated process boundary using metadata only.
#![allow(
    clippy::future_not_send,
    reason = "HTTP and database fixtures stay on the ntex compio runtime"
)]

#[path = "support/platform.rs"]
mod platform;
#[allow(
    dead_code,
    reason = "shared process fixture also supports host failure tests"
)]
#[path = "support/server_process.rs"]
mod server_process;

#[path = "http_jobs/fanout.rs"]
mod fanout;
#[path = "http_jobs/propagation.rs"]
mod propagation;

use compio::io::{AsyncRead, AsyncWriteExt};
use ntex::{client::Client, http::StatusCode};
use serde::Serialize;
use serde_json::{json, Value};
use std::time::Duration;
use zeroship_core::{
    app_id::AppId,
    service_assertion::{ServiceAssertionMinter, ServiceIssuer, ServiceSigningKey},
    service_identity::{endpoints, ServiceEndpoint},
    service_peers::{service_issuer, CONTROL_SERVICE_NAME},
    workflow_coordination::{
        AssignedScope, Assignment, RequestId, RunId, RunOperation, WorkerId, AUDIENCE,
    },
    workflow_jobs::{
        Delivery, DeliveryLease, DeploymentId, JobId, JobOperation, JobOutcome, JobSpec,
        ManagementCommand, Settlement, SettlementReceipt, SubmitJob,
    },
    workflow_schedules::ScheduleId,
};

struct Worker {
    id: WorkerId,
    issuer: ServiceIssuer,
    key: ServiceSigningKey,
}
impl Worker {
    fn new() -> Self {
        let id = WorkerId::mint();
        Self {
            issuer: ServiceIssuer::parse(&format!(
                "spiffe://zeroship.ai/svc/worker/{}",
                id.as_str()
            ))
            .unwrap(),
            id,
            key: ServiceSigningKey::generate(),
        }
    }
    fn assertion(&self) -> String {
        assertion(&self.issuer, &self.key, AUDIENCE)
    }
}

struct Fixture {
    platform: platform::Platform,
    http: Client,
    server: server_process::ServerProcess,
    worker: Worker,
    assignment: Assignment,
    control: ServiceIssuer,
    control_key: ServiceSigningKey,
}
impl Fixture {
    async fn new() -> Self {
        let platform = platform::Platform::new().await;
        let control = service_issuer(CONTROL_SERVICE_NAME).unwrap();
        let control_key = ServiceSigningKey::generate();
        let peers = platform.work.path().join("queue-peers.json");
        platform::write_private(
            &peers,
            serde_json::to_vec(&json!({"keys":[{
                "iss":control.as_str(),"x":control_key.public_jwk_x()
            }]}))
            .unwrap(),
        );
        let http = Client::new().await;
        let server = server_process::ServerProcess::start(
            &platform.runtime_url,
            &peers,
            platform.work.path(),
            "jobs",
            &http,
        )
        .await;
        let worker = Worker::new();
        enroll(&platform, &http, &server.url, &worker).await;
        let app = AppId::mint();
        platform.seed_app(&app).await;
        let assignment = platform
            .seed_placement(&app, &worker.id, Duration::from_secs(30))
            .await;
        Self {
            platform,
            http,
            server,
            worker,
            assignment,
            control,
            control_key,
        }
    }
    fn scope(&self) -> AssignedScope {
        AssignedScope {
            app_id: self.assignment.app_id.clone(),
            assignment_revision: self.assignment.revision,
        }
    }
    fn job(&self) -> JobSpec {
        JobSpec {
            id: JobId::mint(),
            app_id: self.assignment.app_id.clone(),
            operation: JobOperation::Advance {
                deployment_id: DeploymentId::mint(),
                run_id: RunId::mint(),
                generation: 0,
                revision: 1.try_into().unwrap(),
            },
            available_at: 1.try_into().unwrap(),
        }
    }
    async fn post<T: Serialize>(&self, endpoint: ServiceEndpoint, body: &T) -> (StatusCode, Value) {
        post(
            &self.http,
            &self.server.url,
            endpoint,
            &self.worker.assertion(),
            body,
        )
        .await
    }
    async fn submit(&self, job: &JobSpec) {
        let (status, body) = self
            .post(
                endpoints::WORKFLOW_JOB_SUBMIT,
                &SubmitJob {
                    scope: self.scope(),
                    job: job.clone(),
                },
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(serde_json::from_value::<JobSpec>(body).unwrap(), *job);
    }
    async fn claim(&self, job: &JobSpec) -> Delivery {
        let (status, body) = self
            .post(endpoints::WORKFLOW_JOB_CLAIM, &self.scope())
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let lease: DeliveryLease = serde_json::from_value(body).unwrap();
        assert!(lease.remaining_ms.get() > 0);
        assert_eq!(lease.delivery.job, *job);
        assert_eq!(lease.delivery.worker_id, self.worker.id);
        assert_eq!(lease.delivery.assignment_revision, self.assignment.revision);
        assert!(lease.delivery.deadline <= self.assignment.expires_at);
        lease.delivery
    }
    async fn job_snapshot(&self, job: &JobSpec) -> Vec<String> {
        self.platform
            .admin
            .query(
                "SELECT to_jsonb(j)::text FROM workflow_manager.jobs j WHERE app_id=$1 AND id=$2",
                &[&job.app_id.as_str(), &job.id.as_str()],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect()
    }
}

fn assertion(issuer: &ServiceIssuer, key: &ServiceSigningKey, audience: &str) -> String {
    format!(
        "Bearer {}",
        ServiceAssertionMinter::new(issuer.clone(), key.key_id(), key)
            .unwrap()
            .mint(&ServiceIssuer::parse(audience).unwrap())
            .unwrap()
    )
}

async fn post<T: Serialize>(
    client: &Client,
    url: &str,
    endpoint: ServiceEndpoint,
    token: &str,
    body: &T,
) -> (StatusCode, Value) {
    let response = client
        .post(format!("{url}{}", endpoint.path_template()))
        .header("authorization", token)
        .send_json(body)
        .await
        .unwrap();
    let status = response.status();
    let body = response.body().await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

async fn enroll(platform: &platform::Platform, http: &Client, url: &str, worker: &Worker) {
    platform.admin.execute(
        "INSERT INTO zeroship.worker_instances(id,ring_key,public_key,advertise_host,advertise_port,status,enroller_id) \
         VALUES($1,$2,$3,'127.0.0.1',8080,'active',$4)",
        &[&worker.id.as_str(), &vec![1_u8], &worker.key.verifying_key_bytes().to_vec(), &platform.default_enroller_id],
    ).await.unwrap();
    let (status, body) = post(
        http,
        url,
        endpoints::WORKFLOW_REGISTER,
        &worker.assertion(),
        &json!({"capacity":1,"state":"ready"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

fn settlement(delivery: &Delivery, successors: Vec<JobSpec>) -> Settlement {
    Settlement {
        delivery: delivery.clone(),
        outcome: JobOutcome::Completed {},
        successors,
    }
}

#[ntex::test]
async fn delivery_and_receipts_remain_scoped_across_process_restart_and_placement_expiry() {
    let mut fixture = Fixture::new().await;
    let job = fixture.job();
    fixture.submit(&job).await;
    fixture.submit(&job).await;
    let original = fixture.claim(&job).await;
    assert_eq!(original.attempt.get(), 1);
    assert_eq!(
        fixture
            .post(endpoints::WORKFLOW_JOB_CLAIM, &fixture.scope())
            .await,
        (StatusCode::OK, Value::Null)
    );
    let (status, body) = fixture
        .post(endpoints::WORKFLOW_JOB_HEARTBEAT, &original)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let renewed: DeliveryLease = serde_json::from_value(body).unwrap();
    assert_eq!(renewed.delivery.job, original.job);
    assert_eq!(renewed.delivery.attempt, original.attempt);
    assert!(renewed.delivery.deadline >= original.deadline);
    let successor = fixture.job();
    let command = settlement(&original, vec![successor.clone()]);
    let (status, body) = fixture.post(endpoints::WORKFLOW_JOB_SETTLE, &command).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let receipt: SettlementReceipt = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(body["outcome"], json!({"kind":"completed"}));
    assert_eq!(receipt.job_id, job.id);
    assert_eq!(receipt.app_id, job.app_id);
    assert_eq!(receipt.attempt, original.attempt);
    assert_eq!(receipt.outcome, JobOutcome::Completed {});
    assert_foreign_worker_denied(&fixture, &command).await;
    assert_receipt_replay(&mut fixture, &command, &successor, body).await;
}

async fn assert_receipt_replay(
    fixture: &mut Fixture,
    command: &Settlement,
    successor: &JobSpec,
    body: Value,
) {
    let job = &command.delivery.job;
    let before_job = fixture.job_snapshot(job).await;
    let before_successor = fixture.job_snapshot(successor).await;
    assert_eq!(before_job.len(), 1);
    assert_eq!(before_successor.len(), 1);
    fixture.server.restart(&fixture.http).await;
    fixture
        .platform
        .admin
        .execute(
            "UPDATE workflow_manager.assignments SET expires_at=0 WHERE app_id=$1",
            &[&job.app_id.as_str()],
        )
        .await
        .unwrap();
    fixture
        .platform
        .admin
        .execute(
            "UPDATE workflow_manager.workers SET expires_at=0 WHERE id=$1",
            &[&fixture.worker.id.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(
        fixture.post(endpoints::WORKFLOW_JOB_SETTLE, command).await,
        (StatusCode::OK, body)
    );
    assert_eq!(fixture.job_snapshot(job).await, before_job);
    assert_eq!(fixture.job_snapshot(successor).await, before_successor);
    let changed = Settlement {
        outcome: JobOutcome::Rejected {},
        ..command.clone()
    };
    assert_eq!(
        fixture
            .post(endpoints::WORKFLOW_JOB_SETTLE, &changed)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        fixture
            .post(endpoints::WORKFLOW_JOB_CLAIM, &fixture.scope())
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    fixture
        .platform
        .admin
        .execute(
            "UPDATE zeroship.worker_instances SET status='draining' WHERE id=$1",
            &[&fixture.worker.id.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(
        fixture
            .post(endpoints::WORKFLOW_JOB_SETTLE, command)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

async fn assert_foreign_worker_denied(fixture: &Fixture, command: &Settlement) {
    let foreign = Worker::new();
    enroll(
        &fixture.platform,
        &fixture.http,
        &fixture.server.url,
        &foreign,
    )
    .await;
    for (endpoint, body) in [
        (
            endpoints::WORKFLOW_JOB_SUBMIT,
            serde_json::to_value(SubmitJob {
                scope: fixture.scope(),
                job: fixture.job(),
            })
            .unwrap(),
        ),
        (
            endpoints::WORKFLOW_JOB_CLAIM,
            serde_json::to_value(fixture.scope()).unwrap(),
        ),
        (
            endpoints::WORKFLOW_JOB_HEARTBEAT,
            serde_json::to_value(&command.delivery).unwrap(),
        ),
        (
            endpoints::WORKFLOW_JOB_SETTLE,
            serde_json::to_value(command).unwrap(),
        ),
    ] {
        let (status, body) = post(
            &fixture.http,
            &fixture.server.url,
            endpoint,
            &foreign.assertion(),
            &body,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }
}

#[ntex::test]
async fn queue_refuses_foreign_scope_and_platform_job_origins() {
    let fixture = Fixture::new().await;
    let job = fixture.job();
    fixture.submit(&job).await;
    let delivery = fixture.claim(&job).await;
    let before = fixture.job_snapshot(&job).await;
    let foreign = JobSpec {
        app_id: AppId::mint(),
        ..fixture.job()
    };
    let management = [
        ManagementCommand::Transition {
            operation: RunOperation::Pause,
        },
        ManagementCommand::RestartStarted { from: None },
        ManagementCommand::RestartLatest {
            deployment_id: DeploymentId::mint(),
        },
    ]
    .into_iter()
    .map(|command| JobSpec {
        operation: JobOperation::Management {
            request_id: RequestId::mint(),
            run_id: RunId::mint(),
            revision: 1.try_into().unwrap(),
            command,
        },
        ..fixture.job()
    });
    let cron = JobSpec {
        operation: JobOperation::Cron {
            deployment_id: DeploymentId::mint(),
            schedule_id: ScheduleId::mint(),
            schedule_name: "daily-report".into(),
            request_id: RequestId::mint(),
            run_id: RunId::mint(),
            revision: 1.try_into().unwrap(),
            scheduled_at: 1.try_into().unwrap(),
        },
        ..fixture.job()
    };
    let activation = JobSpec {
        operation: JobOperation::Activate {
            deployment_id: DeploymentId::mint(),
            revision: 1.try_into().unwrap(),
        },
        ..fixture.job()
    };
    for denied in [foreign, cron, activation].into_iter().chain(management) {
        let submission = SubmitJob {
            scope: fixture.scope(),
            job: denied.clone(),
        };
        assert_eq!(
            fixture
                .post(endpoints::WORKFLOW_JOB_SUBMIT, &submission)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            fixture
                .post(
                    endpoints::WORKFLOW_JOB_SETTLE,
                    &settlement(&delivery, vec![denied.clone()])
                )
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert!(fixture.job_snapshot(&denied).await.is_empty());
        assert_eq!(fixture.job_snapshot(&job).await, before);
    }
    assert_eq!(
        fixture
            .post(
                endpoints::WORKFLOW_JOB_SETTLE,
                &settlement(&delivery, Vec::new())
            )
            .await
            .0,
        StatusCode::OK
    );
}

async fn rejects_incomplete_job_envelopes(fixture: &Fixture) {
    let candidate = fixture.job();
    let submitted = serde_json::to_value(SubmitJob {
        scope: fixture.scope(),
        job: candidate.clone(),
    })
    .unwrap();
    let mut bodies = Vec::new();
    let mut missing = submitted.clone();
    missing["job"]["operation"]
        .as_object_mut()
        .unwrap()
        .remove("deploymentId");
    bodies.push(missing);
    let mut misplaced = submitted.clone();
    misplaced["job"]["deploymentId"] = json!(candidate.deployment_id().unwrap());
    bodies.push(misplaced);
    for operation in [
        json!({"kind":"activate", "revision":1}),
        json!({"kind":"reconcile", "deploymentId":DeploymentId::mint()}),
        json!({"kind":"collect", "deploymentId":DeploymentId::mint()}),
        json!({"kind":"cron", "scheduleId":ScheduleId::mint(), "scheduleName":"daily-report",
            "requestId":RequestId::mint(), "runId":RunId::mint(), "revision":1, "scheduledAt":0}),
        json!({"kind":"management", "requestId":RequestId::mint(), "runId":RunId::mint(), "revision":1}),
        json!({"kind":"management", "requestId":RequestId::mint(), "runId":RunId::mint(),
            "command":{"kind":"transition","operation":"pause"}}),
        json!({"kind":"management", "requestId":RequestId::mint(), "runId":RunId::mint(), "revision":1,
            "command":{"kind":"restart_latest"}}),
        json!({"kind":"management", "requestId":RequestId::mint(), "runId":RunId::mint(), "revision":1,
            "command":{"kind":"transition","operation":"pause","input":"private"}}),
        json!({"kind":"management", "requestId":RequestId::mint(), "runId":RunId::mint(), "revision":1,
            "command":{"kind":"restart_started","deploymentId":DeploymentId::mint()}}),
        json!({"kind":"management", "requestId":RequestId::mint(), "runId":RunId::mint(), "revision":1,
            "command":{"kind":"restart_started","from":{"name":"step","input":"private"}}}),
    ] {
        let mut malformed = submitted.clone();
        malformed["job"]["operation"] = operation;
        bodies.push(malformed);
    }
    assert!(!bodies.is_empty());
    for body in bodies {
        let (status, failure) = fixture.post(endpoints::WORKFLOW_JOB_SUBMIT, &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{failure}");
        assert_eq!(failure, json!({"code":"invalid"}));
    }
    assert!(fixture.job_snapshot(&candidate).await.is_empty());
}

async fn rejects_invalid_settlement_outcomes(fixture: &Fixture, delivery: &Delivery) {
    let command = settlement(delivery, Vec::new());
    let before = fixture.job_snapshot(&delivery.job).await;
    assert_eq!(before.len(), 1);
    for outcome in [
        json!("completed"),
        json!("waiting"),
        json!("rejected"),
        json!({"kind":"completed","result":"private"}),
        json!({"kind":"waiting","history":[]}),
        json!({"kind":"rejected","error":"private"}),
        json!({"kind":"completed","outcome":{"kind":"denied"}}),
        json!({"kind":"management"}),
        json!({"kind":"management","outcome":null}),
        json!({"kind":"management","outcome":{"kind":"applied"}}),
        json!({"kind":"management","outcome":{"kind":"applied","state":"invented"}}),
        json!({"kind":"management","outcome":{"kind":"denied","input":"private"}}),
        // A closed management result still cannot settle an execution job.
        json!({"kind":"management","outcome":{"kind":"denied"}}),
    ] {
        let mut body = json!(command);
        body["outcome"] = outcome;
        assert_eq!(
            fixture.post(endpoints::WORKFLOW_JOB_SETTLE, &body).await,
            (StatusCode::BAD_REQUEST, json!({"code":"invalid"})),
        );
        assert_eq!(fixture.job_snapshot(&delivery.job).await, before);
    }
    let mut secondary = json!(command);
    secondary["managementOutcome"] = json!({"kind":"denied"});
    assert_eq!(
        fixture
            .post(endpoints::WORKFLOW_JOB_SETTLE, &secondary)
            .await,
        (StatusCode::BAD_REQUEST, json!({"code":"invalid"})),
    );
    assert_eq!(fixture.job_snapshot(&delivery.job).await, before);
}

#[ntex::test]
async fn queue_routes_authenticate_before_body_and_reject_open_metadata() {
    let fixture = Fixture::new().await;
    let job = fixture.job();
    fixture.submit(&job).await;
    let delivery = fixture.claim(&job).await;
    rejects_incomplete_job_envelopes(&fixture).await;
    rejects_invalid_settlement_outcomes(&fixture, &delivery).await;
    for (endpoint, mut body) in [
        (
            endpoints::WORKFLOW_JOB_SUBMIT,
            serde_json::to_value(SubmitJob {
                scope: fixture.scope(),
                job: fixture.job(),
            })
            .unwrap(),
        ),
        (
            endpoints::WORKFLOW_JOB_CLAIM,
            serde_json::to_value(fixture.scope()).unwrap(),
        ),
        (
            endpoints::WORKFLOW_JOB_HEARTBEAT,
            serde_json::to_value(&delivery).unwrap(),
        ),
        (
            endpoints::WORKFLOW_JOB_SETTLE,
            serde_json::to_value(settlement(&delivery, Vec::new())).unwrap(),
        ),
    ] {
        rejects_before_body(fixture.server.address, endpoint).await;
        for token in [
            assertion(&fixture.control, &fixture.control_key, AUDIENCE),
            assertion(
                &fixture.worker.issuer,
                &ServiceSigningKey::generate(),
                AUDIENCE,
            ),
            assertion(
                &fixture.worker.issuer,
                &fixture.worker.key,
                fixture.control.as_str(),
            ),
        ] {
            assert_eq!(
                post(&fixture.http, &fixture.server.url, endpoint, &token, &body)
                    .await
                    .0,
                StatusCode::UNAUTHORIZED
            );
        }
        body["customerPayload"] = json!("must stay in creator storage");
        assert_eq!(
            fixture.post(endpoint, &body).await.0,
            StatusCode::BAD_REQUEST
        );
        body["customerPayload"] = json!("x".repeat(2048));
        assert_eq!(
            fixture.post(endpoint, &body).await.0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }
    let scope = AssignedScope {
        app_id: AppId::mint(),
        ..fixture.scope()
    };
    assert_eq!(
        fixture.post(endpoints::WORKFLOW_JOB_CLAIM, &scope).await.0,
        StatusCode::FORBIDDEN
    );
    let token = fixture.worker.assertion();
    let body = serde_json::to_value(fixture.scope()).unwrap();
    assert_eq!(
        post(
            &fixture.http,
            &fixture.server.url,
            endpoints::WORKFLOW_JOB_CLAIM,
            &token,
            &body
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        post(
            &fixture.http,
            &fixture.server.url,
            endpoints::WORKFLOW_JOB_CLAIM,
            &token,
            &body
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let command = settlement(&delivery, Vec::new());
    let (status, body) = fixture.post(endpoints::WORKFLOW_JOB_SETTLE, &command).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let receipt: SettlementReceipt = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(receipt.job_id, job.id);
    assert_eq!(receipt.outcome, JobOutcome::Completed {});
    assert_eq!(body["outcome"], json!({"kind":"completed"}));
}

async fn rejects_before_body(address: std::net::SocketAddr, endpoint: ServiceEndpoint) {
    compio::time::timeout(Duration::from_secs(3), async {
        let mut stream = compio::net::TcpStream::connect(address).await.unwrap();
        stream.write_all(format!(
            "POST {} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 1024\r\nConnection: close\r\n\r\n",
            endpoint.path_template(),
        ).into_bytes()).await.0.unwrap();
        let mut response = Vec::new();
        loop {
            let compio::BufResult(read, bytes) = stream.read(vec![0;1024]).await;
            let read = read.unwrap();
            assert_ne!(read,0);
            response.extend_from_slice(&bytes[..read]);
            if response.windows(2).any(|bytes| bytes==b"\r\n") {break;}
        }
        assert!(String::from_utf8(response).unwrap().starts_with("HTTP/1.1 401"));
    }).await.expect("authentication must reject without waiting for request bytes");
}

#[ntex::test]
async fn enrollment_revocation_and_key_replacement_fence_blocked_queue_operations() {
    let fixture = Fixture::new().await;
    for replace_key in [false, true] {
        for operation in [
            RequestKind::Submit,
            RequestKind::Claim,
            RequestKind::Heartbeat,
            RequestKind::Settle,
            RequestKind::Replay,
        ] {
            deny_blocked_operation(&fixture, operation, replace_key).await;
        }
    }
}

/// Success criterion 3 (the manager half) of the option-1A worker-enrollment
/// `PoC`: revoking enroller E's row - `SELECT zeroship.revoke_worker_enroller(E)`,
/// the same explicit operator database operation
/// `db/migrations-ts/20260914000500_worker_instances_enroller_binding.ts`
/// defines - cascades to `WorkflowAuth::worker`
/// (crates/zeroship-workflow-server/src/auth.rs) refusing E's instance, while
/// a DIFFERENT enroller F's instance is unaffected.
///
/// `WorkflowAuth::worker` and `PostgresWorkerRegistry::active_key` are
/// UNCHANGED by option 1A: they already read
/// `zeroship.worker_instances.status = 'active'`, and revocation writes
/// exactly that column for every instance the enroller admitted - see the
/// module header of `crates/zeroship-control/src/worker_enrolment.rs`. This
/// test is what binds that claim for the manager reader specifically, the way
/// [`enrollment_revocation_and_key_replacement_fence_blocked_queue_operations`]
/// already binds it for a direct instance-status transition.
#[ntex::test]
async fn revoking_an_enroller_denies_its_worker_while_a_sibling_enroller_stays_active() {
    let fixture = Fixture::new().await;

    // A second enroller F, and a second worker enrolled under it, assigned to
    // its own app - independent of the fixture's default enroller and worker.
    // Reuse WorkerId's own base36 body under the enroller prefix - this crate
    // has no direct dependency on a UUID generator, and the typed-id shape
    // constraints only care about the body's alphabet and width, not which
    // entity minted it.
    let enroller_f = format!("wen_{}", &WorkerId::mint().as_str()[4..]);
    fixture
        .platform
        .admin
        .execute(
            "INSERT INTO zeroship.worker_enrollers (id, public_key, execution_zone_id, status) \
             VALUES ($1, $2, 'ezn_default000000000000000000', 'active')",
            &[&enroller_f, &vec![5_u8; 32]],
        )
        .await
        .unwrap();
    let worker_g = Worker::new();
    fixture
        .platform
        .admin
        .execute(
            "INSERT INTO zeroship.worker_instances(id,ring_key,public_key,advertise_host,advertise_port,status,enroller_id) \
             VALUES($1,$2,$3,'127.0.0.1',8081,'active',$4)",
            &[
                &worker_g.id.as_str(),
                &vec![2_u8],
                &worker_g.key.verifying_key_bytes().to_vec(),
                &enroller_f,
            ],
        )
        .await
        .unwrap();
    // Registration, like `enroll()` does for the fixture's own worker: the
    // coordinator must know G is a live, ready worker before it is assignable.
    let (register_status, body) = post(
        &fixture.http,
        &fixture.server.url,
        endpoints::WORKFLOW_REGISTER,
        &worker_g.assertion(),
        &json!({"capacity":1,"state":"ready"}),
    )
    .await;
    assert_eq!(register_status, StatusCode::OK, "{body}");
    let app_g = AppId::mint();
    fixture.platform.seed_app(&app_g).await;
    let assignment_g = fixture
        .platform
        .seed_placement(&app_g, &worker_g.id, Duration::from_secs(30))
        .await;
    let scope_g = AssignedScope {
        app_id: assignment_g.app_id.clone(),
        assignment_revision: assignment_g.revision,
    };
    let job_g = JobSpec {
        id: JobId::mint(),
        app_id: assignment_g.app_id.clone(),
        operation: JobOperation::Advance {
            deployment_id: DeploymentId::mint(),
            run_id: RunId::mint(),
            generation: 0,
            revision: 1.try_into().unwrap(),
        },
        available_at: 1.try_into().unwrap(),
    };

    // BEFORE: both workers can submit and claim from their own scope.
    let job_e = fixture.job();
    fixture.submit(&job_e).await;
    let claimed_e_before = fixture.claim(&job_e).await;
    let settle_e_before = fixture
        .post(
            endpoints::WORKFLOW_JOB_SETTLE,
            &settlement(&claimed_e_before, vec![]),
        )
        .await;
    assert_eq!(settle_e_before.0, StatusCode::OK, "{:?}", settle_e_before.1);

    let (submit_g_status, body) = post(
        &fixture.http,
        &fixture.server.url,
        endpoints::WORKFLOW_JOB_SUBMIT,
        &worker_g.assertion(),
        &SubmitJob {
            scope: scope_g.clone(),
            job: job_g.clone(),
        },
    )
    .await;
    assert_eq!(submit_g_status, StatusCode::OK, "{body}");
    let (claim_g_before_status, _) = post(
        &fixture.http,
        &fixture.server.url,
        endpoints::WORKFLOW_JOB_CLAIM,
        &worker_g.assertion(),
        &scope_g,
    )
    .await;
    assert_eq!(claim_g_before_status, StatusCode::OK);

    // Revoke ONLY the fixture's default enroller.
    fixture
        .platform
        .admin
        .execute(
            "SELECT zeroship.revoke_worker_enroller($1)",
            &[&fixture.platform.default_enroller_id],
        )
        .await
        .unwrap();

    // AFTER: E's worker is refused at the manager; F's worker is unaffected.
    let job_e_after = fixture.job();
    let (submit_e_after_status, _) = post(
        &fixture.http,
        &fixture.server.url,
        endpoints::WORKFLOW_JOB_SUBMIT,
        &fixture.worker.assertion(),
        &SubmitJob {
            scope: fixture.scope(),
            job: job_e_after,
        },
    )
    .await;
    assert_eq!(
        submit_e_after_status,
        StatusCode::UNAUTHORIZED,
        "a revoked enroller's worker must lose manager access"
    );

    let job_g_2 = JobSpec {
        id: JobId::mint(),
        app_id: assignment_g.app_id.clone(),
        operation: JobOperation::Advance {
            deployment_id: DeploymentId::mint(),
            run_id: RunId::mint(),
            generation: 0,
            revision: 1.try_into().unwrap(),
        },
        available_at: 1.try_into().unwrap(),
    };
    let (submit_g_after_status, body) = post(
        &fixture.http,
        &fixture.server.url,
        endpoints::WORKFLOW_JOB_SUBMIT,
        &worker_g.assertion(),
        &SubmitJob {
            scope: scope_g.clone(),
            job: job_g_2,
        },
    )
    .await;
    assert_eq!(
        submit_g_after_status,
        StatusCode::OK,
        "an untouched enroller's worker must be unaffected by a sibling's revocation: {body}"
    );
}

#[ntex::test]
async fn management_receipt_replay_rechecks_enrollment_after_linkage_reads() {
    use zeroship_core::workflow_coordination::{ManageRun, ManagementOperation, ManagementOutcome};

    let fixture = Fixture::new().await;
    let request = ManageRun {
        app_id: fixture.assignment.app_id.clone(),
        request_id: RequestId::mint(),
        run_id: RunId::mint(),
        command: ManagementOperation::Transition {
            operation: RunOperation::Pause,
        },
    };
    let accepted = post(
        &fixture.http,
        &fixture.server.url,
        endpoints::WORKFLOW_MANAGE,
        &assertion(&fixture.control, &fixture.control_key, AUDIENCE),
        &request,
    )
    .await;
    assert_eq!(accepted.0, StatusCode::OK, "{:?}", accepted.1);
    let (status, body) = fixture
        .post(endpoints::WORKFLOW_JOB_CLAIM, &fixture.scope())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let lease: DeliveryLease = serde_json::from_value(body).unwrap();
    assert_eq!(
        lease.delivery.job.operation,
        JobOperation::Management {
            request_id: request.request_id,
            run_id: request.run_id,
            revision: 1.try_into().unwrap(),
            command: ManagementCommand::Transition {
                operation: RunOperation::Pause
            },
        }
    );
    let settlement = Settlement {
        delivery: lease.delivery,
        outcome: JobOutcome::Management {
            outcome: ManagementOutcome::NotFound {},
        },
        successors: vec![],
    };
    let receipt = fixture
        .post(endpoints::WORKFLOW_JOB_SETTLE, &settlement)
        .await;
    assert_eq!(receipt.0, StatusCode::OK, "{:?}", receipt.1);
    // Receipt recovery depends on enrollment rather than renewed placement.
    fixture
        .platform
        .admin
        .execute(
            "UPDATE workflow_manager.assignments SET expires_at=0 WHERE app_id=$1",
            &[&settlement.delivery.job.app_id.as_str()],
        )
        .await
        .unwrap();
    let before = management_snapshot(&fixture).await;
    for replace_key in [false, true] {
        deny_management_replay_after_linkage_wait(&fixture, &settlement, replace_key).await;
        assert_eq!(management_snapshot(&fixture).await, before);
        assert_eq!(
            fixture
                .post(endpoints::WORKFLOW_JOB_SETTLE, &settlement)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        replace_enrollment(&fixture, fixture.worker.key.verifying_key_bytes()).await;
        assert_eq!(
            fixture
                .post(endpoints::WORKFLOW_JOB_SETTLE, &settlement)
                .await,
            receipt
        );
        assert_eq!(management_snapshot(&fixture).await, before);
    }
}

async fn management_snapshot(fixture: &Fixture) -> Vec<(String, String)> {
    let rows = fixture.platform.admin.query(
        "SELECT 'job' AS kind,to_jsonb(j)::text AS body FROM workflow_manager.jobs j WHERE app_id=$1 \
         UNION ALL SELECT 'command',to_jsonb(m)::text FROM workflow_manager.management m WHERE app_id=$1 \
         UNION ALL SELECT 'order',to_jsonb(o)::text FROM workflow_manager.management_scopes o WHERE app_id=$1 \
         UNION ALL SELECT 'queue_scope',to_jsonb(s)::text FROM workflow_manager.queue_scopes s WHERE id=$1 \
         UNION ALL SELECT 'assignment',to_jsonb(a)::text FROM workflow_manager.assignments a WHERE app_id=$1 \
         ORDER BY kind,body",
        &[&fixture.assignment.app_id.as_str()],
    ).await.unwrap();
    assert_eq!(
        rows.len(),
        5,
        "fixture must include all linked receipt and scope records"
    );
    rows.iter().map(|row| (row.get(0), row.get(1))).collect()
}

async fn deny_management_replay_after_linkage_wait(
    fixture: &Fixture,
    settlement: &Settlement,
    replace_key: bool,
) {
    let admin_url = fixture
        .platform
        .runtime_url
        .replacen("zeroship_workflow@", "postgres@", 1);
    let mut blocker = platform::connect(&admin_url).await;
    let pid: i32 = blocker
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let lock = blocker.transaction().await.unwrap();
    lock.batch_execute("LOCK TABLE workflow_manager.management IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    let request = fixture.post(endpoints::WORKFLOW_JOB_SETTLE, settlement);
    let revoke = async {
        compio::time::timeout(Duration::from_secs(3), async {
            loop {
                let waiting: bool = fixture.platform.admin.query_one(
                    "SELECT EXISTS(SELECT 1 FROM pg_stat_activity a JOIN pg_locks l ON l.pid=a.pid \
                     WHERE a.usename='zeroship_workflow' AND a.state='active' \
                     AND $1=ANY(pg_blocking_pids(a.pid)) AND NOT l.granted \
                     AND l.relation='workflow_manager.management'::regclass \
                     AND l.mode='AccessShareLock' AND a.query ILIKE '%management%')", &[&pid],
                ).await.unwrap().get(0);
                if waiting {
                    break;
                }
                compio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("exact settlement must reach the blocked management linkage SELECT");
        change_enrollment(fixture, replace_key).await;
        lock.commit().await.unwrap();
    };
    let ((status, body), ()) = futures::join!(Box::pin(request), Box::pin(revoke));
    assert_eq!(
        (status, body),
        (StatusCode::FORBIDDEN, json!({"code":"denied"}))
    );
}

#[derive(Clone, Copy)]
enum RequestKind {
    Submit,
    Claim,
    Heartbeat,
    Settle,
    Replay,
}

impl RequestKind {
    const fn endpoint(self) -> ServiceEndpoint {
        match self {
            Self::Submit => endpoints::WORKFLOW_JOB_SUBMIT,
            Self::Claim => endpoints::WORKFLOW_JOB_CLAIM,
            Self::Heartbeat => endpoints::WORKFLOW_JOB_HEARTBEAT,
            Self::Settle | Self::Replay => endpoints::WORKFLOW_JOB_SETTLE,
        }
    }
}

async fn blocked_request(fixture: &Fixture, job: &JobSpec, kind: RequestKind) -> Value {
    if matches!(kind, RequestKind::Submit) {
        return serde_json::to_value(SubmitJob {
            scope: fixture.scope(),
            job: job.clone(),
        })
        .unwrap();
    }
    fixture.submit(job).await;
    if matches!(kind, RequestKind::Claim) {
        return serde_json::to_value(fixture.scope()).unwrap();
    }
    let delivery = fixture.claim(job).await;
    if matches!(kind, RequestKind::Heartbeat) {
        return serde_json::to_value(delivery).unwrap();
    }
    let command = settlement(&delivery, Vec::new());
    if matches!(kind, RequestKind::Replay) {
        assert_eq!(
            fixture.post(kind.endpoint(), &command).await.0,
            StatusCode::OK
        );
    }
    serde_json::to_value(command).unwrap()
}

async fn deny_blocked_operation(fixture: &Fixture, kind: RequestKind, replace_key: bool) {
    let job = fixture.job();
    let command = blocked_request(fixture, &job, kind).await;
    let before = fixture.job_snapshot(&job).await;
    let mut blocker = platform::connect(&fixture.platform.runtime_url).await;
    let pid: i32 = blocker
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let lock = blocker.transaction().await.unwrap();
    lock.query(
        "SELECT id FROM workflow_manager.queue_scopes WHERE id=$1 FOR UPDATE",
        &[&job.app_id.as_str()],
    )
    .await
    .unwrap();
    let request = fixture.post(kind.endpoint(), &command);
    let revoke = async {
        compio::time::timeout(Duration::from_secs(3),async {
            loop {
                let waiting:bool = fixture.platform.admin.query_one(
                    "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE usename='zeroship_workflow' \
                     AND $1=ANY(pg_blocking_pids(pid)))",&[&pid],
                ).await.unwrap().get(0);
                if waiting {break;}
                compio::time::sleep(Duration::from_millis(1)).await;
            }
        }).await.expect("authenticated queue request must reach the held app lock");
        change_enrollment(fixture, replace_key).await;
        lock.commit().await.unwrap();
    };
    let ((status, body), ()) = futures::join!(request, revoke);
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(fixture.job_snapshot(&job).await, before);
    replace_enrollment(fixture, fixture.worker.key.verifying_key_bytes()).await;
    fixture
        .platform
        .admin
        .execute(
            "DELETE FROM workflow_manager.jobs WHERE app_id=$1 AND id=$2",
            &[&job.app_id.as_str(), &job.id.as_str()],
        )
        .await
        .unwrap();
}

async fn change_enrollment(fixture: &Fixture, replace_key: bool) {
    if replace_key {
        replace_enrollment(fixture, ServiceSigningKey::generate().verifying_key_bytes()).await;
    } else {
        fixture
            .platform
            .admin
            .execute(
                "UPDATE zeroship.worker_instances SET status='draining' WHERE id=$1",
                &[&fixture.worker.id.as_str()],
            )
            .await
            .unwrap();
    }
}

async fn replace_enrollment(fixture: &Fixture, public_key: [u8; 32]) {
    // Normal enrollment freezes keys. Model an out-of-band administrator row
    // replacement without disabling the production immutability trigger.
    assert_eq!(
        fixture.platform.admin.execute(
            "WITH previous AS (DELETE FROM zeroship.worker_instances WHERE id=$1 \
             RETURNING id,ring_key,advertise_host,advertise_port,registered_at,enroller_id) \
             INSERT INTO zeroship.worker_instances(id,ring_key,public_key,advertise_host,advertise_port,registered_at,status,enroller_id) \
             SELECT id,ring_key,$2,advertise_host,advertise_port,registered_at,'active',enroller_id FROM previous",
            &[&fixture.worker.id.as_str(), &public_key.to_vec()],
        ).await.unwrap(),
        1
    );
}
