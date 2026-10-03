//! Typed job clients verify metadata identity and transfer monotonic authority.
#![allow(
    clippy::future_not_send,
    reason = "native HTTP fixtures stay on their compio runtime"
)]

use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use futures::{channel::oneshot, future::Either};
use serde_json::{json, Value};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use zeroship_core::{
    app_id::AppId,
    service_assertion::{
        InMemoryReplayStore, ServiceAssertionVerifier, ServiceIssuer, ServiceSigningKey,
        ServiceTrustBundle, TransportAssertionVerifier,
    },
    service_identity::{endpoints, verify_service_call, ServiceEndpoint},
    service_peers::{ServiceAuth, ServiceKeyring},
    workflow_coordination::{
        AssignedScope, FailureCode, ManagementOutcome, RequestId, RestartTarget, RunId,
        RunOperation, RunState, WorkerId, AUDIENCE,
    },
    workflow_jobs::{
        BroadcastId, Delivery, DeploymentId, JobId, JobOperation, JobOutcome, JobSpec,
        ManagementCommand, PropagationId, SettlementReceipt,
    },
    workflow_schedules::ScheduleId,
};
use zeroship_workflow_client::{Error, JobJournal, Options, WorkerCoordinator};

struct Fixture {
    auth: Arc<ServiceAuth>,
    scope: AssignedScope,
    spec: JobSpec,
    delivery: Delivery,
}

impl Fixture {
    fn new() -> Self {
        let worker = WorkerId::mint();
        let issuer = ServiceIssuer::parse(&format!(
            "spiffe://zeroship.ai/svc/worker/{}",
            worker.as_str()
        ))
        .unwrap();
        let auth = Arc::new(ServiceAuth::new(
            ServiceKeyring::from_parts(
                issuer,
                ServiceSigningKey::generate(),
                ServiceTrustBundle::new(),
            )
            .unwrap(),
            Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
        ));
        let scope = AssignedScope {
            app_id: AppId::mint(),
            assignment_revision: 1.try_into().unwrap(),
        };
        let spec = JobSpec {
            id: JobId::mint(),
            app_id: scope.app_id.clone(),
            operation: JobOperation::Advance {
                deployment_id: DeploymentId::mint(),
                run_id: RunId::mint(),
                generation: 0,
                revision: 1.try_into().unwrap(),
            },
            available_at: 0.try_into().unwrap(),
        };
        let delivery = Delivery {
            job: spec.clone(),
            worker_id: worker,
            assignment_revision: scope.assignment_revision,
            attempt: 1.try_into().unwrap(),
            // Deliberately unrelated to the receiving worker's wall clock.
            deadline: 1.try_into().unwrap(),
        };
        Self {
            auth,
            scope,
            spec,
            delivery,
        }
    }

    fn fanout() -> Self {
        let mut fixture = Self::new();
        fixture.spec.operation = JobOperation::Fanout {
            broadcast_id: BroadcastId::mint(),
            revision: 1.try_into().unwrap(),
        };
        fixture.delivery.job = fixture.spec.clone();
        fixture
    }

    fn propagation() -> Self {
        let mut fixture = Self::new();
        fixture.spec.operation = JobOperation::Propagate {
            propagation_id: PropagationId::mint(),
            revision: 1.try_into().unwrap(),
        };
        fixture.delivery.job = fixture.spec.clone();
        fixture
    }

    /// An outcome the service's journal could have committed for this job.
    ///
    /// The result has to answer the command the job carries, so a restart job
    /// settles restarted and a transition applied.
    fn outcome(&self) -> JobOutcome {
        match &self.delivery.job.operation {
            JobOperation::Management {
                command: ManagementCommand::Transition { .. },
                ..
            } => JobOutcome::Management {
                outcome: ManagementOutcome::Applied {
                    state: RunState::Paused,
                },
            },
            JobOperation::Management {
                command:
                    ManagementCommand::RestartStarted { .. } | ManagementCommand::RestartLatest { .. },
                ..
            } => JobOutcome::Management {
                outcome: ManagementOutcome::Restarted {
                    state: RunState::Queued,
                    restarted_from_ordinal: Some(2),
                    pinned_to: DeploymentId::mint(),
                },
            },
            JobOperation::Close { .. } => JobOutcome::Closed { drained: true },
            _ => JobOutcome::Waiting {},
        }
    }

    /// The service's reply to this fixture's committed settlement.
    fn receipt(&self) -> SettlementReceipt {
        receipt(&self.delivery, self.outcome())
    }
}

/// What a committed settlement sends: the delivery, and nothing that names an
/// outcome or a successor.
fn committed(delivery: &Delivery) -> Value {
    json!({"delivery": delivery})
}

fn receipt(delivery: &Delivery, outcome: JobOutcome) -> SettlementReceipt {
    SettlementReceipt {
        job_id: delivery.job.id.clone(),
        app_id: delivery.job.app_id.clone(),
        attempt: delivery.attempt,
        outcome,
    }
}

fn lease(delivery: &Delivery, remaining_ms: u64) -> Value {
    json!({"delivery":delivery,"remainingMs":remaining_ms})
}

/// A renewal request naming no journal task, so these cases exercise the queue
/// half exactly as they did when it was the only half.
fn renewal(delivery: &Delivery) -> Value {
    json!({"delivery":delivery})
}

/// A journal whose halves are opaque JSON.
///
/// That this compiles is the boundary's own proof: the port's bounds are serde
/// and nothing else, so this crate carries no dependency on the engine that owns
/// the real payloads and cannot name them even in a test.
struct AnyJournal;
impl JobJournal for AnyJournal {
    type Receipt = serde_json::Value;
    type Claim = Value;
    type Acceptance = Value;
    type Renewal = Value;
    type Execution = Value;
}

struct Exchange {
    endpoint: ServiceEndpoint,
    request: Value,
    response: Value,
    status: u16,
    delay: Duration,
}

impl Exchange {
    fn new(endpoint: ServiceEndpoint, request: &impl serde::Serialize, response: Value) -> Self {
        Self {
            endpoint,
            request: serde_json::to_value(request).unwrap(),
            response,
            status: 200,
            delay: Duration::ZERO,
        }
    }

    /// A renewal reply carrying the queue lease alone, which is what a renewal
    /// that named no journal task answers.
    fn granted(endpoint: ServiceEndpoint, request: &impl serde::Serialize, lease: Value) -> Self {
        Self::new(endpoint, request, json!({"lease": lease}))
    }

    /// A claim reply: the queue lease, and the journal acceptance the same
    /// exchange carries for the one operation that hands out a task. Derived from
    /// the lease rather than passed in, so a case that alters the operation in a
    /// reply body alters which halves that reply is allowed to carry.
    fn claimed(request: &impl serde::Serialize, lease: Value) -> Self {
        let mut body = json!({"lease": lease});
        if body["lease"]["delivery"]["job"]["operation"]["kind"] == json!("advance") {
            body["accepted"] = json!({"kind": "deferred"});
        }
        Self::new(endpoints::WORKFLOW_JOB_CLAIM, request, body)
    }

    /// A settlement reply. It carries the queue receipt alone, whichever half
    /// produced the outcome, because the journal receipt would repeat the job the
    /// caller sent and the outcome this receipt already names.
    fn settled(request: &impl serde::Serialize, receipt: Value) -> Self {
        Self::new(endpoints::WORKFLOW_JOB_SETTLE, request, receipt)
    }
}

fn peer<'a>(
    fixture: &'a Fixture,
    exchanges: Vec<Exchange>,
    test: impl AsyncFnOnce(WorkerCoordinator) + 'a,
) -> impl std::future::Future<Output = ()> + 'a {
    Box::pin(async move {
        let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = WorkerCoordinator::new(
            &format!("http://{}", listener.local_addr().unwrap()),
            fixture.auth.clone(),
            Options::default(),
        )
        .unwrap();
        let (issuer, key) = fixture.auth.signing_identity().unwrap();
        let mut trust = ServiceTrustBundle::new();
        trust.trust_signing_key(issuer, key.key_id(), key).unwrap();
        let verifier = ServiceAssertionVerifier::new(trust, Arc::new(InMemoryReplayStore::new()));
        let (done, completed) = oneshot::channel();
        let server = async {
            let mut previous = None;
            for exchange in exchanges {
                let (mut stream, _) = listener.accept().await.unwrap();
                let observed = request(&mut stream).await;
                assert_eq!(observed.path, exchange.endpoint.path_template());
                assert_eq!(observed.body, exchange.request);
                assert_ne!(previous.as_ref(), Some(&observed.authorization));
                verify_service_call(
                    &verifier,
                    Some(&observed.authorization),
                    AUDIENCE,
                    exchange.endpoint,
                )
                .await
                .unwrap();
                assert!(verify_service_call(
                    &verifier,
                    Some(&observed.authorization),
                    AUDIENCE,
                    exchange.endpoint,
                )
                .await
                .is_err());
                previous = Some(observed.authorization);
                if !exchange.delay.is_zero() {
                    let received = Instant::now();
                    compio::time::sleep(exchange.delay).await;
                    assert!(received.elapsed() >= exchange.delay);
                }
                let body = serde_json::to_vec(&exchange.response).unwrap();
                let mut response = format!(
                "HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                exchange.status, body.len()
            )
            .into_bytes();
                response.extend(body);
                stream.write_all(response).await.0.unwrap();
                stream.flush().await.unwrap();
            }
            match futures::future::select(completed, Box::pin(listener.accept())).await {
                Either::Left((result, _)) => result.unwrap(),
                Either::Right(_) => panic!("client sent an unexpected HTTP request"),
            }
        };
        compio::time::timeout(Duration::from_secs(10), async {
            futures::join!(server, async {
                test(client).await;
                done.send(()).unwrap();
            });
        })
        .await
        .expect("job client contract hung");
    })
}

struct Request {
    path: String,
    authorization: String,
    body: Value,
}

async fn request(stream: &mut compio::net::TcpStream) -> Request {
    let mut bytes = Vec::new();
    loop {
        let compio::BufResult(read, buffer) = stream.read(vec![0; 1024]).await;
        let read = read.unwrap();
        assert_ne!(read, 0, "peer closed before sending its request");
        bytes.extend_from_slice(&buffer[..read]);
        assert!(bytes.len() <= 16 * 1024);
        let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let header = std::str::from_utf8(&bytes[..end]).unwrap();
        let mut lines = header.lines();
        let mut operation = lines.next().unwrap().split_ascii_whitespace();
        assert_eq!(operation.next(), Some("POST"));
        let path = operation.next().unwrap().to_owned();
        assert_eq!(operation.next(), Some("HTTP/1.1"));
        let mut length = None;
        let mut authorization = None;
        for line in lines {
            let (name, value) = line.split_once(':').unwrap();
            if name.eq_ignore_ascii_case("content-length") {
                assert!(length.is_none());
                length = Some(value.trim().parse::<usize>().unwrap());
            } else if name.eq_ignore_ascii_case("authorization") {
                assert!(authorization.is_none());
                authorization = Some(value.trim().to_owned());
            }
        }
        let length = length.unwrap();
        if bytes.len() < end + 4 + length {
            continue;
        }
        assert_eq!(bytes.len(), end + 4 + length);
        return Request {
            path,
            authorization: authorization.unwrap(),
            body: serde_json::from_slice(&bytes[end + 4..]).unwrap(),
        };
    }
}

#[compio::test]
async fn job_methods_preserve_identity_and_use_remaining_authority() {
    exercise_job_methods(&Fixture::new()).await;
}

async fn exercise_job_methods(fixture: &Fixture) {
    let mut renewed = fixture.delivery.clone();
    renewed.deadline = 0.try_into().unwrap();
    let settled = receipt(&renewed, fixture.outcome());
    peer(
        fixture,
        vec![
            Exchange::claimed(&fixture.scope, lease(&fixture.delivery, 60_000)),
            Exchange::granted(endpoints::WORKFLOW_JOB_HEARTBEAT, &renewal(&fixture.delivery), lease(&renewed, 60_000)),
            Exchange::settled(&committed(&renewed), json!(settled)),
            Exchange::settled(&committed(&renewed), json!(settled)),
            Exchange::new(endpoints::WORKFLOW_JOB_CLAIM, &fixture.scope, Value::Null),
        ],
        async |client| {
            let claimed = client.claim_job::<AnyJournal>(&fixture.scope).await.unwrap().unwrap();
            assert_eq!(claimed.lease.delivery(), &fixture.delivery);
            assert!(claimed.lease.remaining().unwrap() <= Duration::from_secs(60));
            let heartbeat = client.heartbeat_job::<AnyJournal>(&claimed.lease, None).await.unwrap();
            assert_eq!(heartbeat.lease.delivery(), &renewed);
            assert!(heartbeat.lease.remaining().unwrap() <= Duration::from_secs(60));
            assert_eq!(client.settle_committed(&renewed).await.unwrap(), settled);
            assert_eq!(client.settle_committed(&renewed).await.unwrap(), settled);
            assert!(client.claim_job::<AnyJournal>(&fixture.scope).await.unwrap().is_none());
        },
    )
    .await;
}

fn manager_operations() -> Vec<JobOperation> {
    let commands = [
        ManagementCommand::Transition {
            operation: RunOperation::Pause,
        },
        ManagementCommand::RestartStarted {
            from: Some(RestartTarget {
                name: "retained-step".into(),
                occurrence: Some(1),
            }),
        },
        ManagementCommand::RestartLatest {
            deployment_id: DeploymentId::mint(),
        },
    ];
    let mut operations = vec![
        JobOperation::Activate {
            deployment_id: DeploymentId::mint(),
            revision: 1.try_into().unwrap(),
        },
        // A worker answers a release; it never asks for one.
        JobOperation::ReleaseHold {
            deployment_id: DeploymentId::mint(),
        },
        JobOperation::Cron {
            deployment_id: DeploymentId::mint(),
            schedule_id: ScheduleId::mint(),
            schedule_name: "daily-report".into(),
            request_id: RequestId::mint(),
            run_id: RunId::mint(),
            revision: 1.try_into().unwrap(),
            scheduled_at: 0.try_into().unwrap(),
        },
        JobOperation::Close {
            epoch: 1.try_into().unwrap(),
        },
        JobOperation::Reconcile {},
        JobOperation::Collect {},
    ];
    operations.extend(
        commands
            .into_iter()
            .map(|command| JobOperation::Management {
                request_id: RequestId::mint(),
                run_id: RunId::mint(),
                revision: 1.try_into().unwrap(),
                command,
            }),
    );
    operations
}

#[compio::test]
async fn manager_dispatched_jobs_can_be_claimed_and_settled() {
    for operation in manager_operations() {
        let mut fixture = Fixture::new();
        fixture.spec.operation = operation;
        fixture.delivery.job = fixture.spec.clone();
        let settled = fixture.receipt();
        peer(
            &fixture,
            vec![
                Exchange::claimed(&fixture.scope, lease(&fixture.delivery, 60_000)),
                Exchange::settled(&committed(&fixture.delivery), json!(settled)),
            ],
            async |client| {
                let claimed = client.claim_job::<AnyJournal>(&fixture.scope).await.unwrap().unwrap();
                assert_eq!(claimed.lease.delivery(), &fixture.delivery);
                assert_eq!(client.settle_committed(&fixture.delivery).await.unwrap(), settled);
            },
        )
        .await;
    }
}

#[compio::test]
async fn settlement_receipts_cannot_substitute_metadata() {
    let fixture = Fixture::new();
    let settled = fixture.receipt();
    for (field, value) in [
        ("jobId", json!(JobId::mint())),
        ("appId", json!(AppId::mint())),
        ("attempt", json!(2)),
        ("input", json!("private")),
    ] {
        let mut altered = json!(settled);
        altered[field] = value;
        peer(
            &fixture,
            vec![Exchange::settled(&committed(&fixture.delivery), altered)],
            async |client| {
                assert_eq!(
                    client.settle_committed(&fixture.delivery).await.unwrap_err(),
                    Error::InvalidResponse
                );
            },
        )
        .await;
    }
}

/// A settlement's outcome arrives rather than being sent, so the only check this
/// side can make on it is that the operation admits its family -- and it makes
/// that check for every operation, on the reply path both settlement shapes share.
#[compio::test]
async fn a_settlement_reply_in_a_family_the_operation_refuses_is_invalid() {
    let operations = [
        Fixture::new().spec.operation,
        JobOperation::Reconcile {},
        JobOperation::Collect {},
        JobOperation::Fanout {
            broadcast_id: BroadcastId::mint(),
            revision: 1.try_into().unwrap(),
        },
        JobOperation::Propagate {
            propagation_id: PropagationId::mint(),
            revision: 1.try_into().unwrap(),
        },
    ]
    .into_iter()
    .chain(manager_operations());
    for operation in operations {
        let mut fixture = Fixture::new();
        fixture.spec.operation = operation;
        fixture.delivery.job = fixture.spec.clone();
        let refused = if matches!(fixture.spec.operation, JobOperation::Management { .. }) {
            vec![
                JobOutcome::Completed {},
                JobOutcome::Waiting {},
                JobOutcome::Rejected {},
            ]
        } else {
            vec![JobOutcome::Management {
                outcome: ManagementOutcome::Denied {},
            }]
        };
        assert!(!refused.is_empty());
        let mut exchanges: Vec<Exchange> = refused
            .iter()
            .map(|outcome| {
                Exchange::settled(
                    &committed(&fixture.delivery),
                    json!(receipt(&fixture.delivery, outcome.clone())),
                )
            })
            .collect();
        // The control: the same exchange answering a family the operation
        // admits is accepted, so each refusal above measures the family alone.
        let accepted = fixture.receipt();
        exchanges.push(Exchange::settled(
            &committed(&fixture.delivery),
            json!(accepted),
        ));
        peer(&fixture, exchanges, async |client| {
            for _ in &refused {
                assert_eq!(
                    client.settle_committed(&fixture.delivery).await,
                    Err(Error::InvalidResponse)
                );
            }
            assert_eq!(
                client.settle_committed(&fixture.delivery).await.unwrap(),
                accepted
            );
        })
        .await;
    }
}

/// A settlement reply's outcome is a closed shape in a family the operation
/// admits, and within that family it is the service's to choose: this side sent
/// no outcome, so it has none to hold the reply to.
#[compio::test]
async fn settlement_receipts_reject_open_outcomes_and_foreign_families() {
    let mut fixture = Fixture::new();
    for outcome in [
        json!("completed"),
        json!("waiting"),
        json!("rejected"),
        json!({"kind":"waiting","result":"private"}),
        json!({"kind":"waiting","outcome":{"kind":"denied"}}),
        json!({"kind":"management","outcome":{"kind":"denied"}}),
        json!({"kind":"management"}),
        json!({"kind":"management","outcome":null}),
        json!({"kind":"management","outcome":{"kind":"applied","state":"invented"}}),
        json!({"kind":"management","outcome":{"kind":"denied","history":[]}}),
    ] {
        let mut response = json!(fixture.receipt());
        response["outcome"] = outcome;
        peer(
            &fixture,
            vec![Exchange::settled(&committed(&fixture.delivery), response)],
            async |client| {
                assert_eq!(
                    client.settle_committed(&fixture.delivery).await,
                    Err(Error::InvalidResponse)
                );
            },
        )
        .await;
    }
    fixture.spec.operation = JobOperation::Management {
        request_id: RequestId::mint(),
        run_id: RunId::mint(),
        revision: 1.try_into().unwrap(),
        command: ManagementCommand::Transition {
            operation: RunOperation::Pause,
        },
    };
    fixture.delivery.job = fixture.spec.clone();
    for outcome in [
        json!({"kind":"completed"}),
        json!({"kind":"management","outcome":{"kind":"applied","state":"invented"}}),
        json!({"kind":"management","outcome":{"kind":"denied","history":[]}}),
    ] {
        let mut response = json!(fixture.receipt());
        response["outcome"] = outcome;
        peer(
            &fixture,
            vec![Exchange::settled(&committed(&fixture.delivery), response)],
            async |client| {
                assert_eq!(
                    client.settle_committed(&fixture.delivery).await,
                    Err(Error::InvalidResponse)
                );
            },
        )
        .await;
    }
    for outcome in [
        ManagementOutcome::Applied {
            state: RunState::Running,
        },
        ManagementOutcome::NotFound {},
        ManagementOutcome::Conflict {},
        ManagementOutcome::Denied {},
    ] {
        let answered = receipt(&fixture.delivery, JobOutcome::Management { outcome });
        peer(
            &fixture,
            vec![Exchange::settled(&committed(&fixture.delivery), json!(answered))],
            async |client| {
                assert_eq!(
                    client.settle_committed(&fixture.delivery).await.unwrap(),
                    answered
                );
            },
        )
        .await;
    }
    let mut response = json!(fixture.receipt());
    response["managementOutcome"] = json!({"kind":"denied"});
    peer(
        &fixture,
        vec![Exchange::settled(&committed(&fixture.delivery), response)],
        async |client| {
            assert_eq!(
                client.settle_committed(&fixture.delivery).await,
                Err(Error::InvalidResponse)
            );
        },
    )
    .await;
}

#[compio::test]
async fn claim_rejects_foreign_and_malformed_lease_metadata() {
    let fixture = Fixture::new();
    let valid = lease(&fixture.delivery, 60_000);
    let mut cases = Vec::new();
    for (field, value) in [
        ("workerId", json!(WorkerId::mint())),
        ("assignmentRevision", json!(2)),
        ("attempt", json!(0)),
        ("input", json!("private")),
    ] {
        let mut altered = valid.clone();
        altered["delivery"][field] = value;
        cases.push(altered);
    }
    let mut foreign = valid.clone();
    foreign["delivery"]["job"]["appId"] = json!(AppId::mint());
    cases.push(foreign);
    let mut nested = valid.clone();
    nested["delivery"]["job"]["operation"]["history"] = json!(["private"]);
    cases.push(nested);
    let mut missing_deployment = valid.clone();
    missing_deployment["delivery"]["job"]["operation"]
        .as_object_mut()
        .unwrap()
        .remove("deploymentId");
    cases.push(missing_deployment);
    let mut moved_deployment = valid.clone();
    let deployment = moved_deployment["delivery"]["job"]["operation"]
        .as_object_mut()
        .unwrap()
        .remove("deploymentId")
        .unwrap();
    moved_deployment["delivery"]["job"]["deploymentId"] = deployment;
    cases.push(moved_deployment);
    for operation in [
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
        let mut malformed = valid.clone();
        malformed["delivery"]["job"]["operation"] = operation;
        cases.push(malformed);
    }
    for remaining in [json!(0), json!(-1), json!(u64::MAX)] {
        let mut altered = valid.clone();
        altered["remainingMs"] = remaining;
        cases.push(altered);
    }
    for body in cases {
        peer(
            &fixture,
            vec![Exchange::claimed(&fixture.scope, body)],
            async |client| {
                assert_eq!(
                    client.claim_job::<AnyJournal>(&fixture.scope).await.unwrap_err(),
                    Error::InvalidResponse
                );
            },
        )
        .await;
    }
}

#[compio::test]
async fn heartbeat_preserves_the_full_immutable_delivery() {
    let fixture = Fixture::new();
    let valid = lease(&fixture.delivery, 60_000);
    let mut cases = Vec::new();
    for (field, value) in [
        ("workerId", json!(WorkerId::mint())),
        ("assignmentRevision", json!(2)),
        ("attempt", json!(2)),
    ] {
        let mut altered = valid.clone();
        altered["delivery"][field] = value;
        cases.push(altered);
    }
    for (field, value) in [
        ("id", json!(JobId::mint())),
        ("appId", json!(AppId::mint())),
        ("deploymentId", json!(DeploymentId::mint())),
        ("availableAt", json!(2)),
        ("operation", json!({"kind":"collect"})),
    ] {
        let mut altered = valid.clone();
        altered["delivery"]["job"][field] = value;
        cases.push(altered);
    }
    let mut changed_deployment = valid.clone();
    changed_deployment["delivery"]["job"]["operation"]["deploymentId"] =
        json!(DeploymentId::mint());
    cases.push(changed_deployment);
    for body in cases {
        peer(
            &fixture,
            vec![
                Exchange::claimed(&fixture.scope, valid.clone()),
                Exchange::granted(endpoints::WORKFLOW_JOB_HEARTBEAT, &renewal(&fixture.delivery), body),
            ],
            async |client| {
                let claimed = client.claim_job::<AnyJournal>(&fixture.scope).await.unwrap().unwrap();
                assert_eq!(
                    client.heartbeat_job::<AnyJournal>(&claimed.lease, None).await.unwrap_err(),
                    Error::InvalidResponse
                );
            },
        )
        .await;
    }
}

#[compio::test]
async fn a_foreign_worker_delivery_is_rejected_without_http() {
    let fixture = Fixture::new();
    peer(&fixture, Vec::new(), async |client| {
        let mut delivery = fixture.delivery.clone();
        delivery.worker_id = WorkerId::mint();
        assert_eq!(
            client.settle_committed(&delivery).await.unwrap_err(),
            Error::Refused(FailureCode::Denied)
        );
    })
    .await;
}

#[compio::test]
async fn delayed_claim_reply_cannot_reset_the_grant_clock() {
    let fixture = Fixture::new();
    let mut exchange = Exchange::claimed(&fixture.scope, lease(&fixture.delivery, 50));
    exchange.delay = Duration::from_millis(100);
    peer(&fixture, vec![exchange], async |client| {
        assert_eq!(
            client.claim_job::<AnyJournal>(&fixture.scope).await.unwrap_err(),
            Error::Timeout
        );
    })
    .await;
}

#[compio::test]
async fn heartbeat_reply_cannot_revive_expired_local_authority() {
    let fixture = Fixture::new();
    let mut heartbeat = Exchange::granted(endpoints::WORKFLOW_JOB_HEARTBEAT, &renewal(&fixture.delivery), lease(&fixture.delivery, 60_000));
    heartbeat.delay = Duration::from_millis(700);
    peer(
        &fixture,
        vec![
            Exchange::claimed(&fixture.scope, lease(&fixture.delivery, 500)),
            heartbeat,
        ],
        async |client| {
            let claimed = client.claim_job::<AnyJournal>(&fixture.scope).await.unwrap().unwrap();
            assert_eq!(
                client.heartbeat_job::<AnyJournal>(&claimed.lease, None).await.unwrap_err(),
                Error::Timeout
            );
            assert_eq!(claimed.lease.remaining(), Err(Error::Timeout));
        },
    )
    .await;
}

#[compio::test]
async fn expired_handle_refuses_heartbeat_but_allows_receipt_replay() {
    let fixture = Fixture::new();
    let settled = fixture.receipt();
    peer(
        &fixture,
        vec![
            Exchange::claimed(&fixture.scope, lease(&fixture.delivery, 200)),
            Exchange::settled(&committed(&fixture.delivery), json!(settled)),
        ],
        async |client| {
            let claimed = client.claim_job::<AnyJournal>(&fixture.scope).await.unwrap().unwrap();
            let cloned = claimed.lease.clone();
            compio::time::sleep(claimed.lease.remaining().unwrap()).await;
            assert_eq!(claimed.lease.remaining(), Err(Error::Timeout));
            assert_eq!(cloned.remaining(), Err(Error::Timeout));
            assert_eq!(
                client.heartbeat_job::<AnyJournal>(&cloned, None).await.unwrap_err(),
                Error::Timeout
            );
            assert_eq!(client.settle_committed(&fixture.delivery).await.unwrap(), settled);
        },
    )
    .await;
}

#[compio::test]
async fn job_refusals_keep_the_closed_error_contract() {
    let fixture = Fixture::new();
    for (body, expected) in [
        (
            json!({"code":"unavailable"}),
            Error::Refused(FailureCode::Unavailable),
        ),
        (json!({"code":"denied"}), Error::InvalidResponse),
        (
            json!({"code":"unavailable","input":"private"}),
            Error::InvalidResponse,
        ),
    ] {
        // A refusal body is not a reply envelope: it is read by status and code.
        let mut exchange = Exchange::new(endpoints::WORKFLOW_JOB_CLAIM, &fixture.scope, body);
        exchange.status = 503;
        peer(&fixture, vec![exchange], async |client| {
            assert_eq!(
                client.claim_job::<AnyJournal>(&fixture.scope).await.unwrap_err(),
                expected
            );
        })
        .await;
    }
}

#[compio::test]
async fn fanout_publication_preserves_remaining_authority() {
    exercise_job_methods(&Fixture::fanout()).await;
}

#[compio::test]
async fn fanout_replies_cannot_substitute_broadcast_or_revision() {
    let fixture = Fixture::fanout();
    let JobOperation::Fanout { broadcast_id, .. } = &fixture.spec.operation else {
        unreachable!()
    };
    let changes = [
        JobOperation::Fanout {
            broadcast_id: BroadcastId::mint(),
            revision: 1.try_into().unwrap(),
        },
        JobOperation::Fanout {
            broadcast_id: broadcast_id.clone(),
            revision: 2.try_into().unwrap(),
        },
    ];
    let mut exchanges = Vec::new();
    exchanges.push(Exchange::claimed(&fixture.scope, lease(&fixture.delivery, 60_000)));
    for operation in &changes {
        let changed = Delivery {
            job: JobSpec {
                operation: operation.clone(),
                ..fixture.spec.clone()
            },
            ..fixture.delivery.clone()
        };
        exchanges.push(Exchange::granted(endpoints::WORKFLOW_JOB_HEARTBEAT, &renewal(&fixture.delivery), lease(&changed, 60_000)));
    }
    peer(&fixture, exchanges, async |client| {
        let job = client.claim_job::<AnyJournal>(&fixture.scope).await.unwrap().unwrap();
        for _ in &changes {
            assert_eq!(
                client.heartbeat_job::<AnyJournal>(&job.lease, None).await.unwrap_err(),
                Error::InvalidResponse
            );
        }
    })
    .await;
}

#[compio::test]
async fn propagation_publication_preserves_remaining_authority() {
    exercise_job_methods(&Fixture::propagation()).await;
}
