//! Queue retention uses a distinct service role and validates closed receipts.
#![allow(clippy::future_not_send, reason = "native HTTP fixtures use compio")]

use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use futures::{channel::oneshot, future::Either};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use zeroship_core::{
    app_id::AppId,
    service_assertion::{
        InMemoryReplayStore, ServiceAssertionVerifier, ServiceIssuer, ServiceSigningKey,
        ServiceTrustBundle, TransportAssertionVerifier,
    },
    service_identity::{ServiceEndpoint, endpoints, verify_service_call},
    service_peers::{
        CONTROL_SERVICE_NAME, ServiceAuth, ServiceKeyring, WORKFLOW_SERVICE_NAME, service_issuer,
    },
    workflow_coordination::{FailureCode, WorkerId},
    workflow_deployments::{HoldReceipt, HoldScope, HoldState, QueueHoldRequest},
    workflow_jobs::DeploymentId,
};
use zeroship_workflow_client::{Error, Options, QueueDeploymentHolds, Transport};

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

fn command() -> QueueHoldRequest {
    QueueHoldRequest {
        app_id: AppId::mint(),
        deploy_id: DeploymentId::mint(),
        generation: 1.try_into().unwrap(),
    }
}

fn receipt(command: &QueueHoldRequest, state: HoldState) -> HoldReceipt {
    HoldReceipt {
        app_id: command.app_id.clone(),
        deploy_id: command.deploy_id.as_str().to_owned(),
        holder_id: HoldScope::for_queue(command.app_id.clone())
            .holder()
            .to_owned(),
        generation: command.generation,
        state,
        deploy_hash: "a".repeat(64),
    }
}

struct Exchange {
    endpoint: ServiceEndpoint,
    request: QueueHoldRequest,
    response: Value,
    status: u16,
}

impl Exchange {
    fn acquire(command: &QueueHoldRequest, response: Value) -> Self {
        Self {
            endpoint: endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_ACQUIRE,
            request: command.clone(),
            response,
            status: 200,
        }
    }
}

async fn peer(exchanges: Vec<Exchange>, test: impl AsyncFnOnce(QueueDeploymentHolds)) {
    let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let auth = signer(service_issuer(WORKFLOW_SERVICE_NAME).unwrap());
    let client = QueueDeploymentHolds::new(
        &format!("http://{}", listener.local_addr().unwrap()),
        auth.clone(),
        Options::default(),
    )
    .unwrap();
    let (issuer, key) = auth.signing_identity().unwrap();
    let mut trust = ServiceTrustBundle::new();
    trust.trust_signing_key(issuer, key.key_id(), key).unwrap();
    let verifier = ServiceAssertionVerifier::new(trust, Arc::new(InMemoryReplayStore::new()));
    let audience = service_issuer(CONTROL_SERVICE_NAME).unwrap();
    let (done, completed) = oneshot::channel();
    let server = async {
        let mut previous = None;
        for exchange in exchanges {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (path, authorization, body) = request(&mut stream).await;
            assert_eq!(path, exchange.endpoint.path_template());
            assert_eq!(body, json!(exchange.request));
            assert_ne!(previous.as_ref(), Some(&authorization));
            verify_service_call(
                &verifier,
                Some(&authorization),
                audience.as_str(),
                exchange.endpoint,
            )
            .await
            .unwrap();
            assert!(
                verify_service_call(
                    &verifier,
                    Some(&authorization),
                    audience.as_str(),
                    exchange.endpoint
                )
                .await
                .is_err()
            );
            previous = Some(authorization);
            let bytes = serde_json::to_vec(&exchange.response).unwrap();
            let mut response = format!("HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n", exchange.status, bytes.len()).into_bytes();
            response.extend(bytes);
            stream.write_all(response).await.0.unwrap();
            stream.flush().await.unwrap();
        }
        match futures::future::select(completed, Box::pin(listener.accept())).await {
            Either::Left((result, _)) => result.unwrap(),
            Either::Right(_) => panic!("unexpected hold request"),
        }
    };
    compio::time::timeout(Duration::from_secs(10), async {
        futures::join!(server, async {
            test(client).await;
            done.send(()).unwrap();
        });
    })
    .await
    .expect("queue hold client fixture hung");
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
async fn queue_hold_methods_preserve_receipts_and_refresh_assertions() {
    let command = command();
    let held = receipt(&command, HoldState::Held);
    let released = receipt(&command, HoldState::Released);
    peer(
        vec![
            Exchange::acquire(&command, json!(held)),
            Exchange::acquire(&command, json!(held)),
            Exchange {
                endpoint: endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_RELEASE,
                request: command.clone(),
                response: json!(released),
                status: 200,
            },
        ],
        async |client| {
            assert_eq!(client.acquire(&command).await.unwrap(), held);
            assert_eq!(client.clone().acquire(&command).await.unwrap(), held);
            assert_eq!(client.release(&command).await.unwrap(), released);
        },
    )
    .await;
}

#[compio::test]
async fn queue_hold_client_rejects_changed_or_open_receipts() {
    let command = command();
    let held = receipt(&command, HoldState::Held);
    let mut responses = Vec::new();
    for (field, value) in [
        ("appId", json!(AppId::mint())),
        ("deployId", json!(DeploymentId::mint())),
        (
            "holderId",
            json!(HoldScope::for_app(command.app_id.clone()).holder()),
        ),
        (
            "holderId",
            json!(HoldScope::for_queue(AppId::mint()).holder()),
        ),
        ("generation", json!(2)),
        ("generation", json!(0)),
        ("state", json!("released")),
        ("state", json!("unknown")),
        ("deployHash", json!("a".repeat(63))),
        ("deployHash", json!("A".repeat(64))),
        ("deployHash", json!("g".repeat(64))),
        ("input", json!({"private":"body"})),
    ] {
        let mut invalid = json!(held);
        invalid[field] = value;
        responses.push(Exchange::acquire(&command, invalid));
    }
    let expected = responses.len();
    peer(responses, async |client| {
        for _ in 0..expected {
            assert_eq!(client.acquire(&command).await, Err(Error::InvalidResponse));
        }
    })
    .await;
    peer(
        vec![Exchange {
            endpoint: endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_RELEASE,
            request: command.clone(),
            response: json!(held),
            status: 200,
        }],
        async |client| {
            assert_eq!(client.release(&command).await, Err(Error::InvalidResponse));
        },
    )
    .await;
}

#[compio::test]
async fn queue_hold_errors_use_the_closed_failure_contract() {
    let command = command();
    peer(
        vec![
            Exchange {
                status: 403,
                response: json!({"code":"denied"}),
                ..Exchange::acquire(&command, Value::Null)
            },
            Exchange {
                status: 503,
                response: json!({"code":"unavailable","details":"private"}),
                ..Exchange::acquire(&command, Value::Null)
            },
        ],
        async |client| {
            assert_eq!(
                client.acquire(&command).await,
                Err(Error::Refused(FailureCode::Denied))
            );
            assert_eq!(client.acquire(&command).await, Err(Error::InvalidResponse));
        },
    )
    .await;
}

#[compio::test]
async fn queue_hold_configuration_requires_the_workflow_role_and_secure_origin() {
    for issuer in [
        "spiffe://zeroship.ai/svc/control".to_owned(),
        "spiffe://zeroship.ai/svc/worker".to_owned(),
        format!(
            "spiffe://zeroship.ai/svc/worker/{}",
            WorkerId::mint().as_str()
        ),
        format!(
            "spiffe://zeroship.ai/svc/workflow/{}",
            WorkerId::mint().as_str()
        ),
        "spiffe://foreign.test/svc/workflow".to_owned(),
    ] {
        assert!(matches!(
            QueueDeploymentHolds::new(
                "https://control.example",
                signer(ServiceIssuer::parse(&issuer).unwrap()),
                Options::default()
            ),
            Err(Error::Unauthenticated)
        ));
    }
    assert!(matches!(
        QueueDeploymentHolds::new(
            "https://control.example",
            Arc::new(ServiceAuth::unconfigured()),
            Options::default()
        ),
        Err(Error::Unauthenticated)
    ));
    let auth = signer(service_issuer(WORKFLOW_SERVICE_NAME).unwrap());
    for origin in [
        "http://control.example",
        "http://localhost",
        "https://user@control.example",
        "https://control.example/prefix",
        "https://control.example/?query",
        "https://control.example/#fragment",
        "file:///control",
    ] {
        assert!(
            matches!(
                QueueDeploymentHolds::new(origin, auth.clone(), Options::default()),
                Err(Error::InvalidConfig)
            ),
            "{origin}"
        );
        assert_eq!(
            Transport::validate_config(origin, Options::default()),
            Err(Error::InvalidConfig)
        );
    }
    for options in [
        Options {
            timeout: Duration::ZERO,
            ..Options::default()
        },
        Options {
            max_request_bytes: 0,
            ..Options::default()
        },
        Options {
            max_response_bytes: 0,
            ..Options::default()
        },
    ] {
        assert!(matches!(
            QueueDeploymentHolds::new("https://control.example", auth.clone(), options),
            Err(Error::InvalidConfig)
        ));
        assert_eq!(
            Transport::validate_config("https://control.example", options),
            Err(Error::InvalidConfig)
        );
    }
    for origin in [
        "https://control.example",
        "http://127.0.0.1:9090",
        "http://[::1]:9090",
    ] {
        assert!(QueueDeploymentHolds::new(origin, auth.clone(), Options::default()).is_ok());
        assert_eq!(
            Transport::validate_config(origin, Options::default()),
            Ok(())
        );
    }
}
