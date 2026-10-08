//! Queue delivery crosses the authenticated process boundary using metadata only.
#![allow(
    clippy::future_not_send,
    reason = "HTTP and database fixtures stay on the ntex compio runtime"
)]

use crate::support::{
    holds, journal, platform, provision, server_process,
};

mod authority;
mod fanout;
mod propagation;

use compio::io::{AsyncRead, AsyncWriteExt};
use ntex::{client::Client, http::StatusCode};
use serde::Serialize;
use serde_json::{json, Value};
use std::time::Duration;
use zeroship_workflow::service::delivery::{
    AcceptedJob, ClaimedTask, RenewedTask, ReportedExecution,
};
use zeroship_workflow_client::{RenewDelivery, RenewedDelivery, SettleDelivery};
use zeroship_data_orm::binding::DbBinding;
use zeroship_workflow_manager::{
    maintenance::MaintenanceAuthority, Options as QueueOptions, Queue,
};
use zeroship_core::{
    app_id::AppId,
    schema_name::SchemaName,
    service_assertion::{ServiceAssertionMinter, ServiceIssuer, ServiceSigningKey},
    service_identity::{endpoints, ServiceEndpoint},
    service_peers::{service_issuer, CONTROL_SERVICE_NAME},
    workflow_coordination::{RequestId, RunId, RunOperation, WorkerId, AUDIENCE},
    workflow_jobs::{
        ClaimJobs, ClaimedJobs, Delivery, DeploymentId, JobId, JobOperation, JobOutcome, JobSpec,
        ManagementCommand, SettlementReceipt,
    },
    workflow_policy::AppPolicy,
};

/// A renewal request naming no journal task: the caller holds a queue lease and
/// no task under it, which is every maintenance operation.
fn renewal(delivery: &Delivery) -> RenewDelivery<ClaimedTask> {
    RenewDelivery {
        delivery: delivery.clone(),
        task: None,
    }
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

/// The deliveries a claim reply carries, read from the reply rather than
/// trusted to its status.
fn delivered(body: &Value) -> Vec<Value> {
    body["deliveries"]
        .as_array()
        .unwrap_or_else(|| panic!("a claim reply carries a delivery list: {body}"))
        .clone()
}

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
    /// Declared before `platform` so it drops first: a failing case reports the
    /// server's log before the platform removes the work directory holding it.
    server: server_process::ServerProcess,
    platform: platform::Platform,
    http: Client,
    worker: Worker,
    /// The app this case's jobs belong to, in the deployment's one zone, which
    /// is the zone the fixture's worker enrolled in.
    app: AppId,
    control: ServiceIssuer,
    control_key: ServiceSigningKey,
    /// The journal run and deployment this service's own journal holds for the
    /// app.
    ///
    /// A claim accepts into that journal in the same exchange, so the app has to
    /// exist there for any advance job to be claimable at all -- the journal's
    /// `lock_app_state` refuses an app it has never seen. Seeding is the ARRANGE
    /// step: nothing asserted below is written by it.
    run: RunId,
    deploy: DeploymentId,
    /// The queue the spawned service owns, opened a second time in this process
    /// so a sweep can be claimed the way the service's own lane claims one.
    queue: Queue,
    /// The in-process authority that stands in for the service's maintenance
    /// lane. See [`Fixture::swept`] for why a sweep cannot be claimed over HTTP.
    lane: MaintenanceAuthority,
}
impl Fixture {
    async fn new() -> Self {
        // The spawned process composes the manager driver, which enumerates the
        // whole queue, and a zone claim pages every app of its zone, so this case
        // gets a database of its own.
        let platform = platform::Platform::fresh_database().await;
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
        // No sweep lane on this host. Every case in this target is about a queue
        // ROUTE -- what claim, renew and settle accept, authenticate and record
        // -- and the sweep it arranges is arranged so the settle route has a
        // delivery to discharge. The lane claims under an authority of its own,
        // so a running one would take that row first and the route under test
        // would never see it.
        let server = server_process::ServerProcess::without_maintenance_sweeps(
            &platform,
            &peers,
            platform.work.path(),
            "jobs",
            &http,
        )
        .await;
        let worker = Worker::new();
        enroll(&platform, &worker).await;
        let app = AppId::mint();
        provision::provision(&platform, &app, &AppPolicy::default()).await;
        // The scope Control's lifecycle publication creates, in the app's zone.
        platform.seed_scope(&app, platform::DEFAULT_ZONE_ID).await;
        let run = journal::seed_run(&platform, &app).await;
        let deploy = DeploymentId::parse(&app.as_str().replacen("app_", "dep_", 1)).unwrap();
        let queue = Queue::connect(
            DbBinding::platform(
                "workflow_manager",
                "workflow_manager",
                SchemaName::new("workflow_manager").unwrap(),
            ),
            &platform.runtime_url,
            QueueOptions::default(),
            holds::client(),
        )
        .await
        .unwrap();
        let lane = MaintenanceAuthority::new(app.clone(), worker.id.clone());
        Self {
            server,
            platform,
            http,
            worker,
            app,
            control,
            control_key,
            run,
            deploy,
            queue,
            lane,
        }
    }
    fn job(&self) -> JobSpec {
        JobSpec {
            id: JobId::mint(),
            app_id: self.app.clone(),
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
        // A committed creator intent reaches the queue through the same trusted
        // submission the service's own publication uses; no worker request path
        // publishes any more.
        assert_eq!(self.queue.submit(job).await.unwrap(), *job);
    }
    async fn claim(&self, job: &JobSpec) -> Delivery {
        self.claimed(job).await.0
    }

    /// One exchange, both halves: the queue lease and, for the one operation that
    /// hands out a task, the journal acceptance that authorizes executing it.
    async fn claimed(&self, job: &JobSpec) -> (Delivery, Option<AcceptedJob>) {
        let (status, body) = self.post(endpoints::WORKFLOW_JOB_CLAIM, &claim_one()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let claimed: ClaimedJobs<AcceptedJob> = serde_json::from_value(body).unwrap();
        let [claimed] = <[_; 1]>::try_from(claimed.deliveries)
            .unwrap_or_else(|deliveries| panic!("one claim, one delivery: {deliveries:?}"));
        let lease = claimed.lease;
        assert!(lease.remaining_ms.get() > 0);
        assert!(lease.attempt_remaining_ms >= lease.remaining_ms);
        assert_eq!(lease.delivery.job, *job);
        assert_eq!(lease.delivery.worker_id, self.worker.id);
        assert_eq!(
            claimed.accepted.is_some(),
            job.operation.accepts_execution(),
            "a claim carries a journal acceptance for exactly the executable kind"
        );
        (lease.delivery, claimed.accepted)
    }

    /// Take the next sweep off this app's queue, in process, under the authority
    /// the service's own maintenance lane asserts.
    ///
    /// THERE IS NO WIRE CLAIM FOR A SWEEP. `WORKFLOW_JOB_CLAIM` claims as
    /// `Claimant::Worker`, and that claimant admits `advance` alone, so every
    /// journal sweep belongs to the lane. The cases below are about what the
    /// SETTLE route does with a sweep's delivery, and `Queue::settle` is not
    /// claimant-scoped: it authorizes on `settlement.delivery.worker_id`. So the
    /// delivery is arranged here and exercised over HTTP from there.
    ///
    /// The authority carries this fixture's own worker id rather than a fresh
    /// one, because that is the identity the settle route authenticates.
    async fn swept(&self) -> Delivery {
        let (status, body) = self.post(endpoints::WORKFLOW_JOB_CLAIM, &claim_one()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            delivered(&body).is_empty(),
            "the wire claim offers a worker no sweep: {body}"
        );
        let delivery = self
            .lane
            .claim(&self.queue, Ok(AppPolicy::default().max_delivery_attempts))
            .await
            .unwrap()
            .expect("the queue holds a sweep for the lane to claim")
            .delivery()
            .clone();
        assert_eq!(delivery.worker_id, self.worker.id);
        delivery
    }

    /// As [`Self::swept`], for a case that published the sweep it expects back.
    async fn sweep(&self, job: &JobSpec) -> Delivery {
        let delivery = self.swept().await;
        assert_eq!(delivery.job, *job);
        delivery
    }

    /// An advance job for the run this service's journal actually holds, so the
    /// journal half of its claim hands out a task rather than settling a frontier
    /// it has never seen.
    fn executable(&self) -> JobSpec {
        JobSpec {
            id: JobId::mint(),
            app_id: self.app.clone(),
            operation: JobOperation::Advance {
                deployment_id: self.deploy.clone(),
                run_id: self.run.clone(),
                generation: 0,
                revision: 1.try_into().unwrap(),
            },
            available_at: 1.try_into().unwrap(),
        }
    }
    /// The journal state of one dispatched task, read from the row rather than
    /// from a reply, so a merged exchange is measured by what it wrote.
    async fn task_state(&self, task: &str) -> Vec<String> {
        self.platform
            .admin
            .query(
                "SELECT state FROM workflow_manager.__zeroship_workflow_tasks WHERE id=$1",
                &[&task],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect()
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

    async fn job_column(&self, job: &JobSpec, column: &str) -> Option<String> {
        self.platform
            .admin
            .query_one(
                &format!(
                    "SELECT {column}::text FROM workflow_manager.jobs WHERE app_id=$1 AND id=$2"
                ),
                &[&job.app_id.as_str(), &job.id.as_str()],
            )
            .await
            .unwrap()
            .get(0)
    }

    /// An advance job for a run of its own, so its claim hands out a task and its
    /// execution can complete without another case's run being in the way.
    async fn fresh_executable(&self) -> JobSpec {
        let run = journal::seed_run(&self.platform, &self.app).await;
        JobSpec {
            id: JobId::mint(),
            app_id: self.app.clone(),
            operation: JobOperation::Advance {
                deployment_id: self.deploy.clone(),
                run_id: run,
                generation: 0,
                revision: 1.try_into().unwrap(),
            },
            available_at: 1.try_into().unwrap(),
        }
    }

    /// Submit and claim `job` as this fixture's worker, returning the delivery and
    /// the task the journal handed out under it.
    async fn held(&self, job: &JobSpec) -> (Delivery, ClaimedTask) {
        self.submit(job).await;
        let (delivery, accepted) = self.claimed(job).await;
        let Some(AcceptedJob::Execute {
            assignment,
            remaining_ms,
        }) = accepted
        else {
            panic!("the journal holds this run, so its acceptance hands out a task")
        };
        (
            delivery,
            ClaimedTask {
                id: assignment.id.clone(),
                token: assignment.token.clone(),
                remaining_ms,
            },
        )
    }

    /// A second enrolled worker of the same zone, holding no delivery.
    async fn other_worker(&self) -> Worker {
        let worker = Worker::new();
        enroll(&self.platform, &worker).await;
        worker
    }

    async fn post_as<T: Serialize>(
        &self,
        worker: &Worker,
        endpoint: ServiceEndpoint,
        body: &T,
    ) -> (StatusCode, Value) {
        post(
            &self.http,
            &self.server.url,
            endpoint,
            &worker.assertion(),
            body,
        )
        .await
    }
}

/// The settle body that reports an execution: the batch a run that completes
/// produces, under the task and the creator authority the claim handed out.
///
/// Built as JSON rather than through the client's envelope, like every settle
/// body in this target: what is under test is what the ROUTE does with a body a
/// worker chose, and a typed envelope only lets a test send what it can express.
fn executed(delivery: &Delivery, task: &ClaimedTask) -> Value {
    json!({
        "delivery": delivery,
        "execution": ReportedExecution {
            grant_ms: Some(task.remaining_ms),
            task: task.clone(),
            confirmed: Vec::new(),
            execution: zeroship_workflow::WorkflowExecution::from_runtime_value(
                json!({"outcomes":[{"kind":"RunCompleted"}]}),
            )
            .unwrap(),
        },
    })
}

/// The settle body that reports no execution: the delivery alone, which the
/// service settles from the receipt its journal holds.
fn committed(delivery: &Delivery) -> Value {
    json!({"delivery": delivery})
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

/// Enroll `worker` the way Control's join leaves an instance: an `active` row in
/// the deployment's one zone, holding the key its assertions verify under. There
/// is no registration step after it; the row is the whole of what a claim needs.
async fn enroll(platform: &platform::Platform, worker: &Worker) {
    platform.admin.execute(
        "INSERT INTO zeroship.worker_instances(id,ring_key,public_key,advertise_host,advertise_port,status,join_signer_id,join_token_id,execution_zone_id,expires_at) \
         VALUES($1,$2,$3,'127.0.0.1',8080,'active',$4,'tok_testfixturedefault',$5,now() + interval '1 hour')",
        &[&worker.id.as_str(), &vec![1_u8], &worker.key.verifying_key_bytes().to_vec(), &platform::DEFAULT_JOIN_SIGNER_ID, &platform::DEFAULT_ZONE_ID],
    ).await.unwrap();
}

/// A worker cannot publish a job over HTTP: a committed creator intent reaches
/// the queue through `Queue::submit` inside the process that owns the journal,
/// and no request path accepts a submission.
#[ntex::test]
async fn a_worker_cannot_submit_a_job() {
    let fixture = Fixture::new().await;
    let response = fixture
        .http
        .post(format!("{}/v1/jobs/submit", fixture.server.url))
        .header("authorization", fixture.worker.assertion())
        .send_json(&json!({}))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[ntex::test]
async fn delivery_and_receipts_remain_scoped_across_process_restart() {
    let mut fixture = Fixture::new().await;
    let job = fixture.fresh_executable().await;
    // `held` submits it again: the second submission of one job is idempotent.
    fixture.submit(&job).await;
    let (original, task) = fixture.held(&job).await;
    assert_eq!(original.attempt.get(), 1);
    let (status, body) = fixture
        .post(endpoints::WORKFLOW_JOB_CLAIM, &claim_one())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(delivered(&body).is_empty(), "a leased job is offered again: {body}");
    let (status, body) = fixture
        .post(endpoints::WORKFLOW_JOB_HEARTBEAT, &renewal(&original))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let renewed: RenewedDelivery<RenewedTask> = serde_json::from_value(body).unwrap();
    assert!(
        renewed.renewal.is_none(),
        "a renewal that named no journal task answers with the queue half alone"
    );
    let renewed = renewed.lease;
    assert_eq!(renewed.delivery.job, original.job);
    assert_eq!(renewed.delivery.attempt, original.attempt);
    assert!(renewed.delivery.deadline >= original.deadline);
    let execution = executed(&original, &task);
    let (status, body) = fixture
        .post(endpoints::WORKFLOW_JOB_SETTLE, &execution)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let receipt: SettlementReceipt = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(body["outcome"], json!({"kind":"completed"}));
    assert_eq!(receipt.job_id, job.id);
    assert_eq!(receipt.app_id, job.app_id);
    assert_eq!(receipt.attempt, original.attempt);
    assert_eq!(receipt.outcome, JobOutcome::Completed {});
    assert_foreign_worker_denied(&fixture, &original).await;
    assert_receipt_replay(&mut fixture, &original, &execution, body).await;
}

/// A settled delivery's exact retry replays its receipt across a restart, and
/// stops once the worker's enrollment does. Both retries a holder can send
/// replay: the delivery alone, and the same execution again, which the journal
/// answers from the task it already completed rather than committing twice.
async fn assert_receipt_replay(
    fixture: &mut Fixture,
    delivery: &Delivery,
    execution: &Value,
    body: Value,
) {
    let job = &delivery.job;
    let before = fixture.job_snapshot(job).await;
    assert_eq!(before.len(), 1);
    fixture.server.restart(&fixture.http).await;
    for retry in [&committed(delivery), execution] {
        assert_eq!(
            fixture.post(endpoints::WORKFLOW_JOB_SETTLE, retry).await,
            (StatusCode::OK, body.clone()),
            "{retry}"
        );
        assert_eq!(fixture.job_snapshot(job).await, before);
    }
    let (status, reply) = fixture
        .post(endpoints::WORKFLOW_JOB_CLAIM, &claim_one())
        .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert!(delivered(&reply).is_empty(), "a settled job is offered again: {reply}");
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
            .post(endpoints::WORKFLOW_JOB_SETTLE, &committed(delivery))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

/// Another worker of the same zone is offered nothing the holder holds, and may
/// neither renew nor settle the holder's delivery.
async fn assert_foreign_worker_denied(fixture: &Fixture, delivery: &Delivery) {
    let foreign = fixture.other_worker().await;
    let before = fixture.job_snapshot(&delivery.job).await;
    let (status, body) = fixture
        .post_as(&foreign, endpoints::WORKFLOW_JOB_CLAIM, &claim_one())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(delivered(&body).is_empty(), "a held job was offered again: {body}");
    for (endpoint, body) in [
        (
            endpoints::WORKFLOW_JOB_HEARTBEAT,
            serde_json::to_value(renewal(delivery)).unwrap(),
        ),
        (endpoints::WORKFLOW_JOB_SETTLE, committed(delivery)),
    ] {
        let (status, body) = fixture.post_as(&foreign, endpoint, &body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }
    assert_eq!(fixture.job_snapshot(&delivery.job).await, before);
}

#[ntex::test]
async fn queue_routes_authenticate_before_body_and_reject_open_metadata() {
    let fixture = Fixture::new().await;
    let job = fixture.job();
    fixture.submit(&job).await;
    let delivery = fixture.claim(&job).await;
    for (endpoint, mut body) in [
        (
            endpoints::WORKFLOW_JOB_CLAIM,
            serde_json::to_value(claim_one()).unwrap(),
        ),
        (
            endpoints::WORKFLOW_JOB_HEARTBEAT,
            serde_json::to_value(renewal(&delivery)).unwrap(),
        ),
        (endpoints::WORKFLOW_JOB_SETTLE, committed(&delivery)),
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
        // EACH ROUTE IS PROBED AGAINST ITS OWN BUDGET. The settlement route
        // answers to the journal ceiling its outcome batch belongs to rather than
        // to the service metadata budget, so one shared pad would measure the
        // metadata budget three times and this one not at all.
        let settle = endpoint.path_template() == endpoints::WORKFLOW_JOB_SETTLE.path_template();
        if settle {
            // IN BOTH DIRECTIONS, because the refusal below cannot tell the two
            // budgets apart: a body over the larger one is over the smaller one
            // too. This arm is the one that fails if the settlement route ever
            // falls back to the service-wide budget -- a pad past THAT budget and
            // short of this route's is refused for what it says, not its size.
            body["customerPayload"] = json!("x".repeat(server_process::MAX_REQUEST_BYTES + 1));
            assert_eq!(
                fixture.post(endpoint, &body).await.0,
                StatusCode::BAD_REQUEST
            );
        }
        let budget = if settle {
            zeroship_workflow_server::api::SETTLE_BODY_BYTES
        } else {
            server_process::MAX_REQUEST_BYTES
        };
        body["customerPayload"] = json!("x".repeat(budget + 1));
        assert_eq!(
            fixture.post(endpoint, &body).await.0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }
    // A claim names no app at all: a body that tries to is refused as a body,
    // so a worker cannot steer the claim to an app of its choosing.
    let mut steered = serde_json::to_value(claim_one()).unwrap();
    steered["appId"] = json!(AppId::mint());
    assert_eq!(
        fixture.post(endpoints::WORKFLOW_JOB_CLAIM, &steered).await,
        (StatusCode::BAD_REQUEST, json!({"code":"invalid"}))
    );
    let token = fixture.worker.assertion();
    let body = serde_json::to_value(claim_one()).unwrap();
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
    // A fresh assertion is decided on what the body says, and the outcome is the
    // journal's: it rejected an advance for a run it does not hold at the claim.
    let (status, body) = fixture
        .post(endpoints::WORKFLOW_JOB_SETTLE, &committed(&delivery))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let receipt: SettlementReceipt = serde_json::from_value(body).unwrap();
    assert_eq!(receipt.job_id, job.id);
    assert_eq!(receipt.outcome, JobOutcome::Rejected {});
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

/// A renewal or settlement that waits on the app lock rechecks enrollment once
/// it holds the lock, so a revocation or a key replacement that lands during
/// the wait refuses it and changes nothing.
///
/// A claim is not among them: it never waits on an app lock, and the claim's
/// own contention contract is `a_claim_passes_an_app_whose_scope_is_locked`
/// in `crates/zeroship-workflow-server/tests/integration/http_claims.rs`.
#[ntex::test]
async fn enrollment_revocation_and_key_replacement_fence_blocked_queue_operations() {
    let fixture = Fixture::new().await;
    for replace_key in [false, true] {
        for operation in [
            RequestKind::Heartbeat,
            RequestKind::Settle,
            RequestKind::Replay,
        ] {
            deny_blocked_operation(&fixture, operation, replace_key).await;
        }
    }
}

/// Success criterion 3 (the manager half) of the worker-join `PoC`: purging
/// signer E's row - `SELECT zeroship.purge_worker_join_signer(E)`, the same
/// explicit operator database operation
/// `db/migrations-ts/20260914000500_worker_join_bindings.ts` defines -
/// cascades to `WorkflowAuth::worker`
/// (crates/zeroship-workflow-server/src/auth.rs) refusing E's instance, while
/// a DIFFERENT signer F's instance is unaffected.
///
/// `WorkflowAuth::worker` and `PostgresWorkerRegistry::active_instance` read
/// `zeroship.worker_instances.status = 'active'`, and the purge writes
/// exactly that column for every instance the signer admitted - see the
/// module header of `crates/zeroship-control/src/worker_join.rs`. This
/// test is what binds that claim for the manager reader specifically, the way
/// [`enrollment_revocation_and_key_replacement_fence_blocked_queue_operations`]
/// already binds it for a direct instance-status transition.
#[ntex::test]
async fn revoking_a_join_signer_denies_its_worker_while_a_sibling_signer_stays_active() {
    let fixture = Fixture::new().await;

    // A second signer F, and a second worker G joined under it in the same
    // zone - independent of the fixture's default signer and worker. Reuse
    // WorkerId's own base36 body under the join signer prefix - this crate has
    // no direct dependency on a UUID generator, and the typed-id shape
    // constraints only care about the body's alphabet and width, not which
    // entity minted it.
    let signer_f = format!("wjs_{}", &WorkerId::mint().as_str()[4..]);
    fixture
        .platform
        .admin
        .execute(
            "INSERT INTO zeroship.worker_join_signers (id, public_key, status) \
             VALUES ($1, $2, 'active')",
            &[&signer_f, &vec![5_u8; 32]],
        )
        .await
        .unwrap();
    fixture
        .platform
        .admin
        .execute(
            "INSERT INTO zeroship.worker_join_signer_zones (signer_id, execution_zone_id) \
             VALUES ($1, 'ezn_default000000000000000000')",
            &[&signer_f],
        )
        .await
        .unwrap();
    let worker_g = Worker::new();
    fixture
        .platform
        .admin
        .execute(
            "INSERT INTO zeroship.worker_instances(id,ring_key,public_key,advertise_host,advertise_port,status,join_signer_id,join_token_id,execution_zone_id,expires_at) \
             VALUES($1,$2,$3,'127.0.0.1',8081,'active',$4,'tok_testfixturesignerf','ezn_default000000000000000000',now() + interval '1 hour')",
            &[
                &worker_g.id.as_str(),
                &vec![2_u8],
                &worker_g.key.verifying_key_bytes().to_vec(),
                &signer_f,
            ],
        )
        .await
        .unwrap();
    let claimed = async |worker: &Worker| {
        post(
            &fixture.http,
            &fixture.server.url,
            endpoints::WORKFLOW_JOB_CLAIM,
            &worker.assertion(),
            &claim_one(),
        )
        .await
    };

    // BEFORE: both workers claim from their zone, each taking the one ready job.
    let job_e = fixture.job();
    fixture.submit(&job_e).await;
    let claimed_e_before = fixture.claim(&job_e).await;
    let renew_e_before = fixture
        .post(endpoints::WORKFLOW_JOB_HEARTBEAT, &renewal(&claimed_e_before))
        .await;
    assert_eq!(renew_e_before.0, StatusCode::OK, "{:?}", renew_e_before.1);
    let job_g = fixture.job();
    fixture.submit(&job_g).await;
    let (status, body) = claimed(&worker_g).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(delivered(&body).len(), 1, "{body}");

    // Purge ONLY the fixture's default signer.
    fixture
        .platform
        .admin
        .execute(
            "SELECT zeroship.purge_worker_join_signer($1)",
            &[&platform::DEFAULT_JOIN_SIGNER_ID],
        )
        .await
        .unwrap();

    // AFTER: E's worker is refused at the manager; F's worker is unaffected.
    let job_g_2 = fixture.job();
    fixture.submit(&job_g_2).await;
    let (status, _) = claimed(&fixture.worker).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a revoked signer's worker must lose manager access"
    );
    let (status, body) = claimed(&worker_g).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an untouched signer's worker must be unaffected by a sibling's revocation: {body}"
    );
    assert_eq!(delivered(&body).len(), 1, "{body}");
}

/// A worker cannot decide the outcome of a management command Control reads.
///
/// Management is a sweep, so the service's own lane claims it; this case arranges
/// that claim under the worker's own identity, which is the most a worker could
/// ever hold of one. A settlement naming the outcome is refused as a body, and one
/// naming no execution finds no receipt, because no journal applied the command.
/// Either way the command, its job and its ordering are left as they were.
#[ntex::test]
async fn a_worker_cannot_decide_a_management_outcome() {
    use zeroship_core::workflow_coordination::{ManageRun, ManagementOperation};

    let fixture = Fixture::new().await;
    let request = ManageRun {
        app_id: fixture.app.clone(),
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
    let delivery = fixture.swept().await;
    assert_eq!(
        delivery.job.operation,
        JobOperation::Management {
            request_id: request.request_id,
            run_id: request.run_id,
            revision: 1.try_into().unwrap(),
            command: ManagementCommand::Transition {
                operation: RunOperation::Pause
            },
        }
    );
    let before = management_snapshot(&fixture).await;
    let forged = json!({
        "delivery": delivery,
        "outcome": {"kind":"management","outcome":{"kind":"not_found"}},
    });
    let answer = fixture.post(endpoints::WORKFLOW_JOB_SETTLE, &forged).await;
    assert_eq!(
        management_snapshot(&fixture).await,
        before,
        "a worker's settlement decided the command: {answer:?}"
    );
    assert_eq!(answer, (StatusCode::BAD_REQUEST, json!({"code":"invalid"})));
    assert_eq!(
        fixture
            .post(endpoints::WORKFLOW_JOB_SETTLE, &committed(&delivery))
            .await,
        (StatusCode::CONFLICT, json!({"code":"conflict"}))
    );
    assert_eq!(management_snapshot(&fixture).await, before);
}

async fn management_snapshot(fixture: &Fixture) -> Vec<(String, String)> {
    let rows = fixture.platform.admin.query(
        "SELECT 'job' AS kind,to_jsonb(j)::text AS body FROM workflow_manager.jobs j WHERE app_id=$1 \
         UNION ALL SELECT 'command',to_jsonb(m)::text FROM workflow_manager.management m WHERE app_id=$1 \
         UNION ALL SELECT 'order',to_jsonb(o)::text FROM workflow_manager.management_scopes o WHERE app_id=$1 \
         UNION ALL SELECT 'queue_scope',to_jsonb(s)::text FROM workflow_manager.queue_scopes s WHERE id=$1 \
         ORDER BY kind,body",
        &[&fixture.app.as_str()],
    ).await.unwrap();
    assert_eq!(
        rows.len(),
        4,
        "fixture must include all linked receipt and scope records"
    );
    rows.iter().map(|row| (row.get(0), row.get(1))).collect()
}

#[derive(Clone, Copy)]
enum RequestKind {
    Heartbeat,
    Settle,
    Replay,
}

impl RequestKind {
    const fn endpoint(self) -> ServiceEndpoint {
        match self {
            Self::Heartbeat => endpoints::WORKFLOW_JOB_HEARTBEAT,
            Self::Settle | Self::Replay => endpoints::WORKFLOW_JOB_SETTLE,
        }
    }
}

async fn blocked_request(fixture: &Fixture, job: &JobSpec, kind: RequestKind) -> Value {
    if matches!(kind, RequestKind::Settle | RequestKind::Replay) {
        // A settlement reaches the queue only with an outcome the journal decided:
        // the execution it commits first, or, for a replay, the receipt that
        // commit left behind.
        let (delivery, task) = fixture.held(job).await;
        let executed = executed(&delivery, &task);
        if matches!(kind, RequestKind::Settle) {
            return executed;
        }
        assert_eq!(
            fixture.post(kind.endpoint(), &executed).await.0,
            StatusCode::OK
        );
        return committed(&delivery);
    }
    fixture.submit(job).await;
    let delivery = fixture.claim(job).await;
    serde_json::to_value(renewal(&delivery)).unwrap()
}

async fn deny_blocked_operation(fixture: &Fixture, kind: RequestKind, replace_key: bool) {
    let job = if matches!(kind, RequestKind::Settle | RequestKind::Replay) {
        fixture.fresh_executable().await
    } else {
        fixture.job()
    };
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
    // Normal joining freezes keys. Model an out-of-band administrator row
    // replacement without disabling the production immutability trigger.
    assert_eq!(
        fixture.platform.admin.execute(
            "WITH previous AS (DELETE FROM zeroship.worker_instances WHERE id=$1 \
             RETURNING id,ring_key,advertise_host,advertise_port,registered_at,join_signer_id,join_token_id,execution_zone_id,expires_at) \
             INSERT INTO zeroship.worker_instances(id,ring_key,public_key,advertise_host,advertise_port,registered_at,status,join_signer_id,join_token_id,execution_zone_id,expires_at) \
             SELECT id,ring_key,$2,advertise_host,advertise_port,registered_at,'active',join_signer_id,join_token_id,execution_zone_id,expires_at FROM previous",
            &[&fixture.worker.id.as_str(), &public_key.to_vec()],
        ).await.unwrap(),
        1
    );
}

/// One exchange carries both halves, at every stage of one delivery.
///
/// This is asserted against a live service rather than inferred from the halves:
/// the claim answers a queue lease AND the task the journal accepted under it,
/// one renewal extends both leases, and one settlement commits the execution and
/// answers with the outcome that commit decided. The journal rows read back are
/// written by the endpoints; the fixture seeds only the run they act on.
#[ntex::test]
async fn one_exchange_carries_the_queue_and_journal_halves_of_a_delivery() {
    let fixture = Fixture::new().await;
    let job = fixture.executable();
    fixture.submit(&job).await;
    let (delivery, accepted) = fixture.claimed(&job).await;
    let AcceptedJob::Execute {
        assignment,
        remaining_ms,
    } = accepted.expect("an advance job carries a journal acceptance")
    else {
        panic!("the journal holds this run, so its acceptance hands out a task")
    };
    assert_eq!(assignment.invocation.run_id, fixture.run.as_str());
    assert_eq!(assignment.invocation.deploy_id, fixture.deploy.as_str());
    let task = ClaimedTask {
        id: assignment.id.clone(),
        token: assignment.token.clone(),
        remaining_ms,
    };
    assert_eq!(
        fixture.task_state(&assignment.id).await,
        vec!["leased".to_owned()],
        "the journal half of the claim wrote the task the reply describes"
    );

    // ONE RENEWAL, BOTH LEASES. The queue's deadline moves and the journal's
    // does too, in the one exchange.
    // A renewal recomputes the deadline from the journal's database clock, so a
    // heartbeat that lands in the claim's own millisecond answers the same
    // instant and extends nothing. Wait for that clock to leave the claim's
    // millisecond before the heartbeat, so the strict extension below is a real
    // one. The polling is bounded and fails loudly if the clock never moves.
    let claimed_at = assignment.deadline - assignment.lease_ms;
    let wait_until = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let now = fixture
            .platform
            .admin
            .query_one(
                "SELECT floor(extract(epoch FROM clock_timestamp())*1000)::bigint",
                &[],
            )
            .await
            .unwrap()
            .get::<_, i64>(0);
        if now > claimed_at {
            break;
        }
        assert!(
            std::time::Instant::now() < wait_until,
            "the database clock never advanced past the claim"
        );
        compio::time::sleep(Duration::from_millis(1)).await;
    }
    let (status, body) = fixture
        .post(
            endpoints::WORKFLOW_JOB_HEARTBEAT,
            &RenewDelivery {
                delivery: delivery.clone(),
                task: Some(task.clone()),
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let renewed: RenewedDelivery<RenewedTask> = serde_json::from_value(body).unwrap();
    assert_eq!(renewed.lease.delivery.job, delivery.job);
    assert_eq!(renewed.lease.delivery.attempt, delivery.attempt);
    let renewal = renewed
        .renewal
        .expect("a renewal that named a task answers for it");
    assert_eq!(
        renewal.control,
        zeroship_workflow::service::ControlIntent::None,
        "the renewal carries the run control intent read in its own transaction"
    );
    let extended = renewal
        .extended
        .expect("admission and dispatch are on, so the renewal extended the task");
    assert!(extended.deadline > assignment.deadline);

    // THE FRONTIER RIDES THE SETTLEMENT. The outcome is not sent: the journal
    // decides it when it commits the batch, and it comes back on the receipt.
    let (status, body) = fixture
        .post(
            endpoints::WORKFLOW_JOB_SETTLE,
            &SettleDelivery {
                delivery: delivery.clone(),
                execution: Some(ReportedExecution {
                    grant_ms: Some(extended.remaining_ms),
                    task: ClaimedTask {
                        remaining_ms: extended.remaining_ms,
                        ..task
                    },
                    confirmed: Vec::new(),
                    execution: zeroship_workflow::WorkflowExecution::from_runtime_value(
                        json!({"outcomes":[{"kind":"RunCompleted"}]}),
                    )
                    .unwrap(),
                }),
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let receipt: SettlementReceipt = serde_json::from_value(body).unwrap();
    assert_eq!(receipt.job_id, job.id);
    assert_eq!(receipt.attempt, delivery.attempt);
    assert_eq!(receipt.outcome, JobOutcome::Completed {});
    assert_eq!(
        fixture.task_state(&assignment.id).await,
        vec!["completed".to_owned()],
        "the journal half committed before the queue was settled with its outcome"
    );
}
