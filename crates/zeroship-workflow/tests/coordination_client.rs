//! Exercise the worker client against deliberately malformed HTTP peers.
#![allow(
    clippy::future_not_send,
    reason = "test sockets stay on the compio runtime"
)]

use compio::io::{AsyncRead, AsyncWriteExt};
use serde_json::json;
use std::{num::NonZeroU32, sync::Arc, time::Duration};
use zeroship_core::{
    app_id::AppId,
    service_assertion::{
        ServiceIssuer, ServiceSigningKey, ServiceTrustBundle, TransportAssertionVerifier,
    },
    service_peers::{ServiceAuth, ServiceKeyring},
    workflow_coordination::{
        AcknowledgeManagement, AssignedScope, FailureCode, ManagementOutcome, PublishWakeHint,
        RegisterWorker, RequestId, RunId, ScopePage, WorkerId, WorkerState,
    },
};
use zeroship_workflow::coordination::{Error, Options, WorkerCoordinator};

fn auth(issuer: ServiceIssuer) -> Arc<ServiceAuth> {
    let keyring = ServiceKeyring::from_parts(
        issuer,
        ServiceSigningKey::generate(),
        ServiceTrustBundle::new(),
    )
    .unwrap();
    Arc::new(ServiceAuth::new(
        keyring,
        Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
    ))
}

fn worker_auth() -> Arc<ServiceAuth> {
    auth(
        ServiceIssuer::parse(&format!(
            "spiffe://zeroship.ai/svc/worker/{}",
            WorkerId::mint().as_str()
        ))
        .unwrap(),
    )
}

const fn registration() -> RegisterWorker {
    RegisterWorker {
        capacity: NonZeroU32::new(3).unwrap(),
        state: WorkerState::Ready,
    }
}

fn response(status: u16, body: &impl serde::Serialize) -> Vec<u8> {
    let body = serde_json::to_vec(body).unwrap();
    let mut bytes = format!(
        "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend(body);
    bytes
}

fn reply<'a>(
    bytes: Vec<u8>,
    options: Options,
    test: impl AsyncFnOnce(WorkerCoordinator) + 'a,
) -> futures::future::LocalBoxFuture<'a, ()> {
    reply_as(bytes, worker_auth(), options, test)
}

fn reply_as<'a>(
    bytes: Vec<u8>,
    auth: Arc<ServiceAuth>,
    options: Options,
    test: impl AsyncFnOnce(WorkerCoordinator) + 'a,
) -> futures::future::LocalBoxFuture<'a, ()> {
    Box::pin(async move {
        let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let client = WorkerCoordinator::new(&url, auth, options).unwrap();
        let (done, stop) = futures::channel::oneshot::channel();
        let server = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let compio::BufResult(read, bytes) = stream.read(vec![0; 1024]).await;
                let read = read.unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&bytes[..read]);
                assert!(request.len() < 16 * 1024);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let header = std::str::from_utf8(&request[..end])
                        .unwrap()
                        .to_ascii_lowercase();
                    assert!(header.contains("authorization: bearer "));
                    let length: usize = header
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            // A body-size rejection may close the connection before all bytes send.
            let _ = stream.write_all(bytes).await;
            let _ = stop.await;
        };
        compio::time::timeout(Duration::from_secs(10), async {
            futures::join!(server, async {
                test(client).await;
                let _ = done.send(());
            });
        })
        .await
        .expect("client contract hung");
    })
}

#[compio::test]
async fn assignment_pages_and_renewals_preserve_current_authority() {
    let auth = worker_auth();
    let worker = auth.signing_identity().unwrap().0.instance().unwrap();
    let mut apps = [AppId::mint(), AppId::mint()];
    apps.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let first = json!({"appId":apps[0],"workerId":worker,"revision":1,"expiresAt":1});
    let second = json!({"appId":apps[1],"workerId":worker,"revision":1,"expiresAt":1});
    for page in [json!([second, first]), json!([first, first])] {
        reply_as(
            response(200, &page),
            auth.clone(),
            Options::default(),
            async |client| {
                assert_eq!(
                    client
                        .assignments(&ScopePage { after: None })
                        .await
                        .unwrap_err(),
                    Error::InvalidResponse
                );
            },
        )
        .await;
    }
    reply_as(
        response(200, &json!([first])),
        auth.clone(),
        Options::default(),
        async |client| {
            assert_eq!(
                client
                    .assignments(&ScopePage {
                        after: Some(apps[0].clone())
                    })
                    .await
                    .unwrap_err(),
                Error::InvalidResponse
            );
        },
    )
    .await;
    let scope = AssignedScope {
        app_id: apps[0].clone(),
        assignment_revision: 1.try_into().unwrap(),
    };
    for assignment in [
        second,
        json!({"appId":apps[0],"workerId":worker,"revision":2,"expiresAt":1}),
    ] {
        reply_as(
            response(200, &assignment),
            auth.clone(),
            Options::default(),
            async |client| {
                assert_eq!(
                    client.renew(&scope).await.unwrap_err(),
                    Error::InvalidResponse
                );
            },
        )
        .await;
    }
    for registered in [
        json!({"workerId":worker,"capacity":2,"state":"ready","expiresAt":1}),
        json!({"workerId":worker,"capacity":3,"state":"draining","expiresAt":1}),
    ] {
        reply_as(
            response(200, &registered),
            auth.clone(),
            Options::default(),
            async |client| {
                assert_eq!(
                    client.register(&registration()).await.unwrap_err(),
                    Error::InvalidResponse
                );
            },
        )
        .await;
    }
}

#[compio::test]
async fn configuration_requires_a_worker_identity_and_an_unambiguous_secure_origin() {
    for issuer in [
        "spiffe://zeroship.ai/svc/control",
        "spiffe://zeroship.ai/svc/worker",
        "spiffe://zeroship.ai/svc/worker/not-an-instance",
    ] {
        assert_eq!(
            WorkerCoordinator::new(
                "https://coordinator.example",
                auth(ServiceIssuer::parse(issuer).unwrap()),
                Options::default()
            )
            .unwrap_err(),
            Error::Unauthenticated
        );
    }
    assert_eq!(
        WorkerCoordinator::new(
            "https://coordinator.example",
            Arc::new(ServiceAuth::unconfigured()),
            Options::default()
        )
        .unwrap_err(),
        Error::Unauthenticated
    );
    let auth = worker_auth();
    for url in [
        "https://coordinator.example",
        "https://coordinator.example:8443/",
        "http://127.0.0.1:8080",
        "http://[::1]:8080",
    ] {
        WorkerCoordinator::new(url, auth.clone(), Options::default()).unwrap();
    }
    for url in [
        "http://coordinator.example",
        "http://localhost",
        "http://192.0.2.1",
        "file:///secret",
        "https://user:secret@coordinator.example",
        "https://coordinator.example/prefix",
        "https://coordinator.example?secret=x",
        "https://coordinator.example#fragment",
    ] {
        assert_eq!(
            WorkerCoordinator::new(url, auth.clone(), Options::default()).unwrap_err(),
            Error::InvalidConfig
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
        assert_eq!(
            WorkerCoordinator::new("https://coordinator.example", auth.clone(), options)
                .unwrap_err(),
            Error::InvalidConfig
        );
    }
    // Serialization is bounded before a connection or assertion is sent.
    let client = WorkerCoordinator::new(
        "http://127.0.0.1:1",
        auth,
        Options {
            max_request_bytes: 1,
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(
        client.register(&registration()).await.unwrap_err(),
        Error::RequestTooLarge
    );
}

#[compio::test]
async fn transport_bounds_bodies_and_time_and_never_follows_redirects() {
    let options = Options {
        max_response_bytes: 128,
        ..Options::default()
    };
    let oversized = json!({"secret":"x".repeat(256)});
    for status in [200, 503] {
        reply(response(status, &oversized), options, async |client| {
            assert_eq!(
                client.register(&registration()).await.unwrap_err(),
                Error::ResponseTooLarge
            );
        })
        .await;
    }
    let chunk = "x".repeat(256);
    let bytes = format!(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{chunk}\r\n0\r\n\r\n",
        chunk.len()
    )
    .into_bytes();
    reply(bytes, options, async |client| {
        assert_eq!(
            client.register(&registration()).await.unwrap_err(),
            Error::ResponseTooLarge
        );
    })
    .await;
    for bytes in [
        b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{".to_vec(),
        b"HTTP/1.1 200 OK\r\n".to_vec(),
    ] {
        reply(
            bytes,
            Options {
                timeout: Duration::from_millis(100),
                ..options
            },
            async |client| {
                assert_eq!(
                    client.register(&registration()).await.unwrap_err(),
                    Error::Timeout
                );
            },
        )
        .await;
    }
    let sink = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    for status in [301, 302, 303, 307, 308] {
        let bytes = format!(
            "HTTP/1.1 {status} Redirect\r\nLocation: http://{}/stolen\r\nContent-Length: 0\r\n\r\n",
            sink.local_addr().unwrap()
        )
        .into_bytes();
        reply(bytes, options, async |client| {
            assert_eq!(
                client.register(&registration()).await.unwrap_err(),
                Error::InvalidResponse
            );
        })
        .await;
    }
    assert!(
        compio::time::timeout(Duration::from_millis(100), sink.accept())
            .await
            .is_err()
    );
}

#[compio::test]
async fn errors_are_closed_metadata_and_never_echo_remote_data() {
    for (status, code) in [
        (400, FailureCode::Invalid),
        (401, FailureCode::Unauthenticated),
        (403, FailureCode::Denied),
        (409, FailureCode::Conflict),
        (413, FailureCode::RequestTooLarge),
        (429, FailureCode::Capacity),
        (503, FailureCode::Unavailable),
    ] {
        reply(
            response(status, &json!({"code":code})),
            Options::default(),
            async |client| {
                assert_eq!(
                    client.register(&registration()).await.unwrap_err(),
                    Error::Refused(code)
                );
            },
        )
        .await;
    }
    for (status, body, expected) in [
        (401, json!({"code":"denied"}), Error::InvalidResponse),
        (
            409,
            json!({"code":"conflict","password":"customer-secret"}),
            Error::InvalidResponse,
        ),
        (
            500,
            json!({"password":"customer-secret"}),
            Error::Unavailable,
        ),
        (
            200,
            json!({"password":"customer-secret"}),
            Error::InvalidResponse,
        ),
        (204, json!(null), Error::InvalidResponse),
    ] {
        reply(
            response(status, &body),
            Options::default(),
            async |client| {
                let error = client.register(&registration()).await.unwrap_err();
                assert_eq!(error, expected);
                assert!(!format!("{error:?} {error}").contains("customer-secret"));
            },
        )
        .await;
    }
}

#[compio::test]
async fn scope_and_receipt_substitution_are_rejected() {
    let app = AppId::mint();
    let other = AppId::mint();
    let scope = AssignedScope {
        app_id: app.clone(),
        assignment_revision: 1.try_into().unwrap(),
    };
    let foreign_assignment =
        json!({"appId":app,"workerId":WorkerId::mint(),"revision":1,"expiresAt":1});
    reply(
        response(200, &vec![foreign_assignment.clone()]),
        Options::default(),
        async |client| {
            assert_eq!(
                client
                    .assignments(&ScopePage { after: None })
                    .await
                    .unwrap_err(),
                Error::InvalidResponse
            );
        },
    )
    .await;
    reply(
        response(200, &foreign_assignment),
        Options::default(),
        async |client| {
            assert_eq!(
                client.renew(&scope).await.unwrap_err(),
                Error::InvalidResponse
            );
        },
    )
    .await;
    reply(
        response(
            200,
            &json!({"workerId":WorkerId::mint(),"capacity":3,"state":"ready","expiresAt":1}),
        ),
        Options::default(),
        async |client| {
            assert_eq!(
                client.register(&registration()).await.unwrap_err(),
                Error::InvalidResponse
            );
        },
    )
    .await;
    for receipt in [
        json!({"appId":other,"assignmentRevision":1,"revision":1}),
        json!({"appId":app,"assignmentRevision":2,"revision":1}),
        json!({"appId":app,"assignmentRevision":1,"revision":2}),
    ] {
        reply(
            response(200, &receipt),
            Options::default(),
            async |client| {
                let request = PublishWakeHint {
                    app_id: app.clone(),
                    assignment_revision: scope.assignment_revision,
                    revision: 1.try_into().unwrap(),
                    next_due_at: None,
                };
                assert_eq!(
                    client.publish_wake(&request).await.unwrap_err(),
                    Error::InvalidResponse
                );
            },
        )
        .await;
    }
    let request_id = RequestId::mint();
    for commands in [
        json!([{"appId":other,"requestId":request_id,"runId":RunId::mint(),"command":{"kind":"transition","operation":"cancel"}}]),
        json!([{"appId":app,"requestId":request_id,"runId":RunId::mint(),"command":{"kind":"transition","operation":"pause"}},{"appId":app,"requestId":request_id,"runId":RunId::mint(),"command":{"kind":"transition","operation":"cancel"}}]),
        json!([{"appId":app,"requestId":request_id,"runId":RunId::mint(),"command":{"kind":"transition","operation":"cancel","input":{"secret":"customer-data"}}}]),
    ] {
        reply(
            response(200, &commands),
            Options::default(),
            async |client| {
                assert_eq!(
                    client.pending_management(&scope).await.unwrap_err(),
                    Error::InvalidResponse
                );
            },
        )
        .await;
    }
    let ack = AcknowledgeManagement {
        request_id: request_id.clone(),
        app_id: app.clone(),
        assignment_revision: scope.assignment_revision,
        outcome: ManagementOutcome::Conflict {},
    };
    for receipt in [
        json!({"appId":other,"requestId":request_id,"outcome":{"kind":"conflict"}}),
        json!({"appId":app,"requestId":RequestId::mint(),"outcome":{"kind":"conflict"}}),
        json!({"appId":app,"requestId":request_id,"outcome":null}),
        json!({"appId":app,"requestId":request_id,"outcome":{"kind":"denied"}}),
    ] {
        reply(
            response(200, &receipt),
            Options::default(),
            async |client| {
                assert_eq!(
                    client.acknowledge_management(&ack).await.unwrap_err(),
                    Error::InvalidResponse
                );
            },
        )
        .await;
    }
}
