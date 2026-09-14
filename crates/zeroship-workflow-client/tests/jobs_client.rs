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
        ManagementCommand, PropagationId, Settlement, SettlementReceipt, SubmitJob,
    },
    workflow_schedules::ScheduleId,
};
use zeroship_workflow_client::{Error, Options, WorkerCoordinator};

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

    fn submission(&self) -> SubmitJob {
        SubmitJob {
            scope: self.scope.clone(),
            job: self.spec.clone(),
        }
    }

    fn settlement(&self) -> Settlement {
        Settlement {
            delivery: self.delivery.clone(),
            outcome: match self.delivery.job.operation {
                JobOperation::Management { .. } => JobOutcome::Management {
                    outcome: ManagementOutcome::Applied {
                        state: RunState::Paused,
                    },
                },
                JobOperation::Close { .. } => JobOutcome::Closed { drained: true },
                _ => JobOutcome::Waiting {},
            },
            successors: Vec::new(),
        }
    }
}

fn receipt(command: &Settlement) -> SettlementReceipt {
    SettlementReceipt {
        job_id: command.delivery.job.id.clone(),
        app_id: command.delivery.job.app_id.clone(),
        attempt: command.delivery.attempt,
        outcome: command.outcome,
    }
}

fn lease(delivery: &Delivery, remaining_ms: u64) -> Value {
    json!({"delivery":delivery,"remainingMs":remaining_ms})
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
    exercise_job_methods(&Fixture::new(), Vec::new()).await;
}

async fn exercise_job_methods(fixture: &Fixture, successors: Vec<JobSpec>) {
    let submit = fixture.submission();
    let mut renewed = fixture.delivery.clone();
    renewed.deadline = 0.try_into().unwrap();
    let settlement = Settlement {
        delivery: renewed.clone(),
        successors,
        ..fixture.settlement()
    };
    let settled = receipt(&settlement);
    peer(
        fixture,
        vec![
            Exchange::new(endpoints::WORKFLOW_JOB_SUBMIT, &submit, json!(fixture.spec)),
            Exchange::new(
                endpoints::WORKFLOW_JOB_CLAIM,
                &fixture.scope,
                lease(&fixture.delivery, 60_000),
            ),
            Exchange::new(
                endpoints::WORKFLOW_JOB_HEARTBEAT,
                &fixture.delivery,
                lease(&renewed, 60_000),
            ),
            Exchange::new(endpoints::WORKFLOW_JOB_SETTLE, &settlement, json!(settled)),
            Exchange::new(endpoints::WORKFLOW_JOB_SETTLE, &settlement, json!(settled)),
            Exchange::new(endpoints::WORKFLOW_JOB_CLAIM, &fixture.scope, Value::Null),
        ],
        async |client| {
            assert_eq!(client.submit_job(&submit).await.unwrap(), fixture.spec);
            let claimed = client.claim_job(&fixture.scope).await.unwrap().unwrap();
            assert_eq!(claimed.delivery(), &fixture.delivery);
            assert!(claimed.remaining().unwrap() <= Duration::from_secs(60));
            let heartbeat = client.heartbeat_job(&claimed).await.unwrap();
            assert_eq!(heartbeat.delivery(), &renewed);
            assert!(heartbeat.remaining().unwrap() <= Duration::from_secs(60));
            assert_eq!(client.settle_job(&settlement).await.unwrap(), settled);
            assert_eq!(client.settle_job(&settlement).await.unwrap(), settled);
            assert!(client.claim_job(&fixture.scope).await.unwrap().is_none());
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
        let command = fixture.settlement();
        let settled = receipt(&command);
        peer(
            &fixture,
            vec![
                Exchange::new(
                    endpoints::WORKFLOW_JOB_CLAIM,
                    &fixture.scope,
                    lease(&fixture.delivery, 60_000),
                ),
                Exchange::new(endpoints::WORKFLOW_JOB_SETTLE, &command, json!(settled)),
            ],
            async |client| {
                let claimed = client.claim_job(&fixture.scope).await.unwrap().unwrap();
                assert_eq!(claimed.delivery(), &fixture.delivery);
                assert_eq!(client.settle_job(&command).await.unwrap(), settled);
            },
        )
        .await;
    }
}

#[compio::test]
async fn submission_and_settlement_receipts_cannot_substitute_metadata() {
    let fixture = Fixture::new();
    let submit = fixture.submission();
    let mut altered_job = json!(fixture.spec);
    altered_job["operation"]["deploymentId"] = json!(DeploymentId::mint());
    peer(
        &fixture,
        vec![Exchange::new(
            endpoints::WORKFLOW_JOB_SUBMIT,
            &submit,
            altered_job,
        )],
        async |client| {
            assert_eq!(
                client.submit_job(&submit).await.unwrap_err(),
                Error::InvalidResponse
            );
        },
    )
    .await;
    let command = fixture.settlement();
    let settled = receipt(&command);
    for (field, value) in [
        ("jobId", json!(JobId::mint())),
        ("appId", json!(AppId::mint())),
        ("attempt", json!(2)),
        ("outcome", json!({"kind":"completed"})),
        ("input", json!("private")),
    ] {
        let mut altered = json!(settled);
        altered[field] = value;
        peer(
            &fixture,
            vec![Exchange::new(
                endpoints::WORKFLOW_JOB_SETTLE,
                &command,
                altered,
            )],
            async |client| {
                assert_eq!(
                    client.settle_job(&command).await.unwrap_err(),
                    Error::InvalidResponse
                );
            },
        )
        .await;
    }
}

#[compio::test]
async fn incompatible_outcome_families_refuse_before_http() {
    let fixture = Fixture::new();
    let valid = fixture.settlement();
    peer(
        &fixture,
        vec![Exchange::new(
            endpoints::WORKFLOW_JOB_SETTLE,
            &valid,
            json!(receipt(&valid)),
        )],
        async |client| {
            let operations = [
                fixture.spec.operation.clone(),
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
                let mut invalid = fixture.settlement();
                invalid.delivery.job.operation = operation;
                let outcomes = if matches!(
                    invalid.delivery.job.operation,
                    JobOperation::Management { .. }
                ) {
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
                for outcome in outcomes {
                    invalid.outcome = outcome;
                    assert_eq!(
                        client.settle_job(&invalid).await,
                        Err(Error::Refused(FailureCode::Invalid))
                    );
                }
            }
            assert_eq!(client.settle_job(&valid).await.unwrap(), receipt(&valid));
        },
    )
    .await;
}

#[compio::test]
async fn settlement_receipts_reject_open_outcomes_and_changed_management_results() {
    let mut fixture = Fixture::new();
    let command = fixture.settlement();
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
        let mut response = json!(receipt(&command));
        response["outcome"] = outcome;
        peer(
            &fixture,
            vec![Exchange::new(
                endpoints::WORKFLOW_JOB_SETTLE,
                &command,
                response,
            )],
            async |client| {
                assert_eq!(
                    client.settle_job(&command).await,
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
    let command = fixture.settlement();
    for outcome in [
        json!({"kind":"completed"}),
        json!({"kind":"management","outcome":{"kind":"applied","state":"running"}}),
        json!({"kind":"management","outcome":{"kind":"not_found"}}),
        json!({"kind":"management","outcome":{"kind":"conflict"}}),
        json!({"kind":"management","outcome":{"kind":"denied"}}),
    ] {
        let mut response = json!(receipt(&command));
        response["outcome"] = outcome;
        peer(
            &fixture,
            vec![Exchange::new(
                endpoints::WORKFLOW_JOB_SETTLE,
                &command,
                response,
            )],
            async |client| {
                assert_eq!(
                    client.settle_job(&command).await,
                    Err(Error::InvalidResponse)
                );
            },
        )
        .await;
    }
    let mut response = json!(receipt(&command));
    response["managementOutcome"] = json!({"kind":"denied"});
    peer(
        &fixture,
        vec![Exchange::new(
            endpoints::WORKFLOW_JOB_SETTLE,
            &command,
            response,
        )],
        async |client| {
            assert_eq!(
                client.settle_job(&command).await,
                Err(Error::InvalidResponse)
            );
        },
    )
    .await;
    peer(
        &fixture,
        vec![Exchange::new(
            endpoints::WORKFLOW_JOB_SETTLE,
            &command,
            json!(receipt(&command)),
        )],
        async |client| {
            assert_eq!(
                client.settle_job(&command).await.unwrap(),
                receipt(&command)
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
            vec![Exchange::new(
                endpoints::WORKFLOW_JOB_CLAIM,
                &fixture.scope,
                body,
            )],
            async |client| {
                assert_eq!(
                    client.claim_job(&fixture.scope).await.unwrap_err(),
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
                Exchange::new(endpoints::WORKFLOW_JOB_CLAIM, &fixture.scope, valid.clone()),
                Exchange::new(endpoints::WORKFLOW_JOB_HEARTBEAT, &fixture.delivery, body),
            ],
            async |client| {
                let claimed = client.claim_job(&fixture.scope).await.unwrap().unwrap();
                assert_eq!(
                    client.heartbeat_job(&claimed).await.unwrap_err(),
                    Error::InvalidResponse
                );
            },
        )
        .await;
    }
}

#[compio::test]
async fn forbidden_publication_is_rejected_without_http() {
    let fixture = Fixture::new();
    peer(&fixture, Vec::new(), async |client| {
        let denied = Error::Refused(FailureCode::Denied);
        let mut foreign = fixture.submission();
        foreign.scope.app_id = AppId::mint();
        assert_eq!(client.submit_job(&foreign).await.unwrap_err(), denied);
        for operation in manager_operations() {
            let mut command = fixture.submission();
            command.job.operation = operation;
            assert_eq!(client.submit_job(&command).await.unwrap_err(), denied);
            let command = Settlement {
                successors: vec![command.job],
                ..fixture.settlement()
            };
            assert_eq!(client.settle_job(&command).await.unwrap_err(), denied);
        }
        let mut command = fixture.settlement();
        let mut foreign = fixture.spec.clone();
        foreign.app_id = AppId::mint();
        command.successors.push(foreign);
        assert_eq!(client.settle_job(&command).await.unwrap_err(), denied);
        let mut command = fixture.settlement();
        command.delivery.worker_id = WorkerId::mint();
        assert_eq!(client.settle_job(&command).await.unwrap_err(), denied);
    })
    .await;
}

#[compio::test]
async fn delayed_claim_reply_cannot_reset_the_grant_clock() {
    let fixture = Fixture::new();
    let mut exchange = Exchange::new(
        endpoints::WORKFLOW_JOB_CLAIM,
        &fixture.scope,
        lease(&fixture.delivery, 50),
    );
    exchange.delay = Duration::from_millis(100);
    peer(&fixture, vec![exchange], async |client| {
        assert_eq!(
            client.claim_job(&fixture.scope).await.unwrap_err(),
            Error::Timeout
        );
    })
    .await;
}

#[compio::test]
async fn heartbeat_reply_cannot_revive_expired_local_authority() {
    let fixture = Fixture::new();
    let mut heartbeat = Exchange::new(
        endpoints::WORKFLOW_JOB_HEARTBEAT,
        &fixture.delivery,
        lease(&fixture.delivery, 60_000),
    );
    heartbeat.delay = Duration::from_millis(700);
    peer(
        &fixture,
        vec![
            Exchange::new(
                endpoints::WORKFLOW_JOB_CLAIM,
                &fixture.scope,
                lease(&fixture.delivery, 500),
            ),
            heartbeat,
        ],
        async |client| {
            let claimed = client.claim_job(&fixture.scope).await.unwrap().unwrap();
            assert_eq!(
                client.heartbeat_job(&claimed).await.unwrap_err(),
                Error::Timeout
            );
            assert_eq!(claimed.remaining(), Err(Error::Timeout));
        },
    )
    .await;
}

#[compio::test]
async fn expired_handle_refuses_heartbeat_but_allows_receipt_replay() {
    let fixture = Fixture::new();
    let command = fixture.settlement();
    let settled = receipt(&command);
    peer(
        &fixture,
        vec![
            Exchange::new(
                endpoints::WORKFLOW_JOB_CLAIM,
                &fixture.scope,
                lease(&fixture.delivery, 200),
            ),
            Exchange::new(endpoints::WORKFLOW_JOB_SETTLE, &command, json!(settled)),
        ],
        async |client| {
            let claimed = client.claim_job(&fixture.scope).await.unwrap().unwrap();
            let cloned = claimed.clone();
            compio::time::sleep(claimed.remaining().unwrap()).await;
            assert_eq!(claimed.remaining(), Err(Error::Timeout));
            assert_eq!(cloned.remaining(), Err(Error::Timeout));
            assert_eq!(
                client.heartbeat_job(&cloned).await.unwrap_err(),
                Error::Timeout
            );
            assert_eq!(client.settle_job(&command).await.unwrap(), settled);
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
        let mut exchange = Exchange::new(endpoints::WORKFLOW_JOB_CLAIM, &fixture.scope, body);
        exchange.status = 503;
        peer(&fixture, vec![exchange], async |client| {
            assert_eq!(
                client.claim_job(&fixture.scope).await.unwrap_err(),
                expected
            );
        })
        .await;
    }
}

#[compio::test]
async fn fanout_publication_and_successors_preserve_remaining_authority() {
    let fixture = Fixture::fanout();
    let JobOperation::Fanout { broadcast_id, .. } = &fixture.spec.operation else {
        unreachable!()
    };
    let successor = JobSpec {
        id: JobId::mint(),
        operation: JobOperation::Fanout {
            broadcast_id: broadcast_id.clone(),
            revision: 2.try_into().unwrap(),
        },
        ..fixture.spec.clone()
    };
    exercise_job_methods(&fixture, vec![successor]).await;
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
    let submit = fixture.submission();
    let mut exchanges = Vec::new();
    for operation in &changes {
        exchanges.push(Exchange::new(
            endpoints::WORKFLOW_JOB_SUBMIT,
            &submit,
            json!(JobSpec {
                operation: operation.clone(),
                ..fixture.spec.clone()
            }),
        ));
    }
    exchanges.push(Exchange::new(
        endpoints::WORKFLOW_JOB_CLAIM,
        &fixture.scope,
        lease(&fixture.delivery, 60_000),
    ));
    for operation in &changes {
        let changed = Delivery {
            job: JobSpec {
                operation: operation.clone(),
                ..fixture.spec.clone()
            },
            ..fixture.delivery.clone()
        };
        exchanges.push(Exchange::new(
            endpoints::WORKFLOW_JOB_HEARTBEAT,
            &fixture.delivery,
            lease(&changed, 60_000),
        ));
    }
    peer(&fixture, exchanges, async |client| {
        for _ in &changes {
            assert_eq!(
                client.submit_job(&submit).await,
                Err(Error::InvalidResponse)
            );
        }
        let job = client.claim_job(&fixture.scope).await.unwrap().unwrap();
        for _ in &changes {
            assert_eq!(
                client.heartbeat_job(&job).await.unwrap_err(),
                Error::InvalidResponse
            );
        }
    })
    .await;
}

#[compio::test]
async fn propagation_publication_and_successors_preserve_remaining_authority() {
    let fixture = Fixture::propagation();
    let JobOperation::Propagate { propagation_id, .. } = &fixture.spec.operation else {
        unreachable!()
    };
    let successor = JobSpec {
        id: JobId::mint(),
        operation: JobOperation::Propagate {
            propagation_id: propagation_id.clone(),
            revision: 2.try_into().unwrap(),
        },
        ..fixture.spec.clone()
    };
    exercise_job_methods(&fixture, vec![successor]).await;
}
