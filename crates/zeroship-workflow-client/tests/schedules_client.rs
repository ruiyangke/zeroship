//! Schedule acknowledgements are bound to the exact Control publication request.
#![allow(
    clippy::future_not_send,
    reason = "HTTP fixtures use native compio sockets"
)]

use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use futures::{channel::oneshot, future::Either};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use zeroship_core::{
    app_id::AppId,
    service_assertion::{
        InMemoryReplayStore, ServiceAssertionVerifier, ServiceIssuer, ServiceSigningKey,
        ServiceTrustBundle, TransportAssertionVerifier,
    },
    service_identity::{endpoints, verify_service_call, ServiceEndpoint},
    service_peers::{service_issuer, ServiceAuth, ServiceKeyring, CONTROL_SERVICE_NAME},
    workflow_coordination::{FailureCode, AUDIENCE},
    workflow_jobs::{DeploymentId, JobId, JobOperation, JobSpec},
    workflow_schedules::{
        ActivateSchedules, DisableSchedules, RegisterSchedules, ScheduleCatchUp,
        ScheduleDescriptor, ScheduleOverlap, ScheduleTiming,
    },
};
use zeroship_workflow_client::{ControlCoordinator, Error, Options};

fn signer(issuer: ServiceIssuer) -> Arc<ServiceAuth> {
    Arc::new(ServiceAuth::new(
        ServiceKeyring::from_parts(
            issuer,
            ServiceSigningKey::generate(),
            ServiceTrustBundle::new(),
        )
        .unwrap(),
        Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
    ))
}

fn registration() -> RegisterSchedules {
    RegisterSchedules {
        app_id: AppId::mint(),
        deployment_id: DeploymentId::mint(),
        schedules: ["later", "earlier"]
            .into_iter()
            .map(|name| ScheduleDescriptor {
                name: name.into(),
                workflow_name: "scheduled-work".into(),
                schedule: ScheduleTiming::Cron {
                    cron_expr: "0 0 1 1 *".into(),
                    tz: "UTC".into(),
                },
                overlap: ScheduleOverlap::Allow,
                catch_up: ScheduleCatchUp::Skip,
            })
            .collect(),
    }
}

fn activation(registration: &RegisterSchedules) -> ActivateSchedules {
    ActivateSchedules {
        app_id: registration.app_id.clone(),
        deployment_id: registration.deployment_id.clone(),
        revision: 1.try_into().unwrap(),
    }
}

fn receipt(command: &ActivateSchedules) -> JobSpec {
    JobSpec {
        id: JobId::mint(),
        app_id: command.app_id.clone(),
        operation: JobOperation::Activate {
            deployment_id: command.deployment_id.clone(),
            revision: command.revision,
        },
        available_at: 0.try_into().unwrap(),
    }
}

struct Exchange {
    endpoint: ServiceEndpoint,
    request: Value,
    response: Value,
    status: u16,
}

impl Exchange {
    fn register(command: &RegisterSchedules, response: Value) -> Self {
        Self {
            endpoint: endpoints::WORKFLOW_SCHEDULE_REGISTER,
            request: json!(command),
            response,
            status: 200,
        }
    }
    fn activate(command: &ActivateSchedules, response: Value) -> Self {
        Self {
            endpoint: endpoints::WORKFLOW_SCHEDULE_ACTIVATE,
            request: json!(command),
            response,
            status: 200,
        }
    }
    fn disable(command: &DisableSchedules, response: Value) -> Self {
        Self {
            endpoint: endpoints::WORKFLOW_SCHEDULE_DISABLE,
            request: json!(command),
            response,
            status: 200,
        }
    }
}

fn peer(
    exchanges: Vec<Exchange>,
    test: impl std::ops::AsyncFnOnce(ControlCoordinator),
) -> impl std::future::Future<Output = ()> {
    Box::pin(async move {
        let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let auth = signer(service_issuer(CONTROL_SERVICE_NAME).unwrap());
        let client = ControlCoordinator::new(
            &format!("http://{}", listener.local_addr().unwrap()),
            auth.clone(),
            Options::default(),
        )
        .unwrap();
        let (issuer, key) = auth.signing_identity().unwrap();
        let mut trust = ServiceTrustBundle::new();
        trust.trust_signing_key(issuer, key.key_id(), key).unwrap();
        let verifier = ServiceAssertionVerifier::new(trust, Arc::new(InMemoryReplayStore::new()));
        let (done, completed) = oneshot::channel();
        let server = async {
            let mut previous = None;
            for exchange in exchanges {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (path, authorization, body) = request(&mut stream).await;
                assert_eq!(path, exchange.endpoint.path_template());
                assert_eq!(body, exchange.request);
                assert_ne!(previous.as_ref(), Some(&authorization));
                verify_service_call(&verifier, Some(&authorization), AUDIENCE, exchange.endpoint)
                    .await
                    .unwrap();
                assert!(verify_service_call(
                    &verifier,
                    Some(&authorization),
                    AUDIENCE,
                    exchange.endpoint,
                )
                .await
                .is_err());
                previous = Some(authorization);
                let bytes = serde_json::to_vec(&exchange.response).unwrap();
                let mut response = format!(
                "HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                exchange.status, bytes.len(),
            ).into_bytes();
                response.extend(bytes);
                stream.write_all(response).await.0.unwrap();
                stream.flush().await.unwrap();
            }
            match futures::future::select(completed, Box::pin(listener.accept())).await {
                Either::Left((result, _)) => result.unwrap(),
                Either::Right(_) => panic!("unexpected schedule request"),
            };
        };
        compio::time::timeout(Duration::from_secs(10), async {
            futures::join!(server, async {
                test(client).await;
                done.send(()).unwrap();
            });
        })
        .await
        .expect("schedule client fixture hung");
    })
}

async fn request(stream: &mut compio::net::TcpStream) -> (String, String, Value) {
    let mut bytes = Vec::new();
    loop {
        let compio::BufResult(read, buffer) = stream.read(vec![0; 1024]).await;
        let read = read.unwrap();
        assert_ne!(read, 0, "request ended before its body");
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
        return (
            path,
            authorization.unwrap(),
            serde_json::from_slice(&bytes[end + 4..]).unwrap(),
        );
    }
}

#[compio::test]
async fn schedule_calls_preserve_metadata_and_refresh_control_assertions() {
    let registration = registration();
    let activation = activation(&registration);
    let job = receipt(&activation);
    peer(
        vec![
            Exchange::register(&registration, json!(registration)),
            Exchange::activate(&activation, json!(job)),
            Exchange::activate(&activation, json!(job)),
        ],
        async |client| {
            assert_eq!(
                client.register_schedules(&registration).await.unwrap(),
                registration
            );
            assert_eq!(client.activate_schedules(&activation).await.unwrap(), job);
            assert_eq!(client.activate_schedules(&activation).await.unwrap(), job);
        },
    )
    .await;
}

#[compio::test]
async fn registration_rejects_foreign_or_changed_echoes_and_open_metadata() {
    let command = registration();
    let original = json!(command);
    let mut replies = Vec::new();
    for (field, value) in [
        ("appId", json!(AppId::mint())),
        ("deploymentId", json!(DeploymentId::mint())),
        ("credentials", json!({"token":"not schedule metadata"})),
    ] {
        let mut reply = original.clone();
        reply[field] = value;
        replies.push(reply);
    }
    let mut reordered = original.clone();
    reordered["schedules"].as_array_mut().unwrap().reverse();
    replies.push(reordered);
    let mut changed = original.clone();
    changed["schedules"][0]["workflowName"] = json!("other-workflow");
    replies.push(changed);
    let mut nested = original;
    nested["schedules"][0]["schedule"]["input"] = json!({"business":"data"});
    replies.push(nested);
    let count = replies.len();
    let exchanges = replies
        .into_iter()
        .map(|reply| Exchange::register(&command, reply))
        .collect();
    peer(exchanges, async |client| {
        for _ in 0..count {
            assert_eq!(
                client.register_schedules(&command).await,
                Err(Error::InvalidResponse)
            );
        }
    })
    .await;
}

#[compio::test]
async fn activation_rejects_foreign_scope_revision_kind_and_open_receipts() {
    let command = activation(&registration());
    let original = json!(receipt(&command));
    let mut replies = Vec::new();
    for (field, value) in [
        ("id", json!("not-a-job-id")),
        ("appId", json!(AppId::mint())),
        ("deploymentId", json!(DeploymentId::mint())),
        (
            "operation",
            json!({"kind":"activate","deploymentId":command.deployment_id,"revision":2}),
        ),
        (
            "operation",
            json!({"kind":"activate","deploymentId":DeploymentId::mint(),"revision":command.revision}),
        ),
        (
            "operation",
            json!({"kind":"activate","revision":command.revision}),
        ),
        ("operation", json!({"kind":"reconcile"})),
        (
            "operation",
            json!({"kind":"activate","deploymentId":command.deployment_id,"revision":command.revision,"input":null}),
        ),
        ("history", json!([])),
    ] {
        let mut reply = original.clone();
        reply[field] = value;
        replies.push(Exchange::activate(&command, reply));
    }
    let count = replies.len();
    peer(replies, async |client| {
        for _ in 0..count {
            assert_eq!(
                client.activate_schedules(&command).await,
                Err(Error::InvalidResponse)
            );
        }
    })
    .await;
}

#[compio::test]
async fn schedule_failures_keep_the_closed_transport_contract() {
    let command = registration();
    let activation = activation(&command);
    let mut conflict = Exchange::register(&command, json!({"code":"conflict"}));
    conflict.status = 409;
    let mut unavailable = Exchange::activate(&activation, json!({"code":"unavailable"}));
    unavailable.status = 503;
    let mut open_failure =
        Exchange::activate(&activation, json!({"code":"conflict","detail":"private"}));
    open_failure.status = 409;
    peer(vec![conflict, unavailable, open_failure], async |client| {
        assert_eq!(
            client.register_schedules(&command).await,
            Err(Error::Refused(FailureCode::Conflict))
        );
        assert_eq!(
            client.activate_schedules(&activation).await,
            Err(Error::Refused(FailureCode::Unavailable))
        );
        assert_eq!(
            client.activate_schedules(&activation).await,
            Err(Error::InvalidResponse)
        );
    })
    .await;
}

#[compio::test]
async fn disable_preserves_revision_and_rejects_changed_or_open_receipts() {
    let command = DisableSchedules {
        app_id: AppId::mint(),
        revision: 7.try_into().unwrap(),
    };
    let mut foreign = json!(command);
    foreign["appId"] = json!(AppId::mint());
    let mut newer = json!(command);
    newer["revision"] = json!(8);
    let mut open = json!(command);
    open["input"] = json!({"private":"customer data"});
    let mut unavailable = Exchange::disable(&command, json!({"code":"unavailable"}));
    unavailable.status = 503;
    peer(
        vec![
            Exchange::disable(&command, json!(command)),
            Exchange::disable(&command, json!(command)),
            Exchange::disable(&command, foreign),
            Exchange::disable(&command, newer),
            Exchange::disable(&command, open),
            unavailable,
        ],
        async |client| {
            assert_eq!(client.disable_schedules(&command).await.unwrap(), command);
            assert_eq!(client.disable_schedules(&command).await.unwrap(), command);
            for _ in 0..3 {
                assert_eq!(
                    client.disable_schedules(&command).await,
                    Err(Error::InvalidResponse)
                );
            }
            assert_eq!(
                client.disable_schedules(&command).await,
                Err(Error::Refused(FailureCode::Unavailable))
            );
        },
    )
    .await;
}

#[compio::test]
async fn control_instance_credentials_cannot_publish_schedules() {
    let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = ControlCoordinator::new(
        &format!("http://{}", listener.local_addr().unwrap()),
        signer(ServiceIssuer::parse("spiffe://zeroship.ai/svc/control/control-a").unwrap()),
        Options::default(),
    )
    .unwrap();
    let command = registration();
    let attempt = async {
        assert_eq!(
            client.register_schedules(&command).await,
            Err(Error::Unauthenticated)
        );
        assert_eq!(
            client.activate_schedules(&activation(&command)).await,
            Err(Error::Unauthenticated)
        );
        assert_eq!(
            client
                .disable_schedules(&DisableSchedules {
                    app_id: command.app_id.clone(),
                    revision: 1.try_into().unwrap(),
                })
                .await,
            Err(Error::Unauthenticated)
        );
    };
    match futures::future::select(Box::pin(listener.accept()), Box::pin(attempt)).await {
        Either::Right(_) => {}
        Either::Left(_) => panic!("instance credentials reached the schedule endpoint"),
    };
}
