//! Exercise native coordinator clients against deliberately malformed HTTP peers.
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
        InMemoryReplayStore, ServiceAssertionVerifier, ServiceIssuer, ServiceSigningKey,
        ServiceTrustBundle, TransportAssertionVerifier,
    },
    service_identity::{endpoints, verify_identity, PeerCredentials, ServiceEndpoint},
    service_peers::{ServiceAuth, ServiceKeyring},
    workflow_coordination::{
        AssignedScope, DeploymentId, FailureCode, ManageRun, ManagementOperation, ManagementStatus,
        RegisterWorker, RequestId, RestartDeploy, RestartOptions, RestartTarget, RunId,
        RunOperation, ScopePage, VerifyAssignment, WorkerId, WorkerState, AUDIENCE,
    },
};
use zeroship_workflow_client::{ControlCoordinator, Error, Options, WorkerCoordinator};

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

fn control_auth() -> Arc<ServiceAuth> {
    auth(ServiceIssuer::parse("spiffe://zeroship.ai/svc/control/control-a").unwrap())
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
    reply_client(bytes, auth, options, WorkerCoordinator::new, None, test)
}

fn control_reply<'a>(
    bytes: Vec<u8>,
    endpoint: ServiceEndpoint,
    request: &impl serde::Serialize,
    test: impl AsyncFnOnce(ControlCoordinator) + 'a,
) -> futures::future::LocalBoxFuture<'a, ()> {
    reply_client(
        bytes,
        control_auth(),
        Options::default(),
        ControlCoordinator::new,
        Some((endpoint, serde_json::to_value(request).unwrap())),
        test,
    )
}

fn reply_client<'a, Client: 'a>(
    bytes: Vec<u8>,
    auth: Arc<ServiceAuth>,
    options: Options,
    build: impl FnOnce(&str, Arc<ServiceAuth>, Options) -> Result<Client, Error> + 'a,
    expected: Option<(ServiceEndpoint, serde_json::Value)>,
    test: impl AsyncFnOnce(Client) + 'a,
) -> futures::future::LocalBoxFuture<'a, ()> {
    Box::pin(async move {
        let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let client = build(&url, auth, options).unwrap();
        let (done, stop) = futures::channel::oneshot::channel();
        let server = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            assert!(request.bearer().is_some());
            if let Some((endpoint, body)) = expected {
                request.assert_operation(endpoint, &body);
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

struct ObservedRequest {
    header: String,
    body: serde_json::Value,
}

impl ObservedRequest {
    fn bearer(&self) -> Option<&str> {
        self.header.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("authorization")
                .then(|| value.trim().strip_prefix("Bearer "))
                .flatten()
        })
    }

    fn assert_operation(&self, endpoint: ServiceEndpoint, body: &serde_json::Value) {
        assert_eq!(
            self.header.lines().next().unwrap(),
            format!("POST {} HTTP/1.1", endpoint.path_template())
        );
        assert_eq!(&self.body, body);
    }
}

async fn read_request(stream: &mut compio::net::TcpStream) -> ObservedRequest {
    let mut request = Vec::new();
    loop {
        let compio::BufResult(read, bytes) = stream.read(vec![0; 1024]).await;
        let read = read.unwrap();
        assert_ne!(read, 0);
        request.extend_from_slice(&bytes[..read]);
        assert!(request.len() < 16 * 1024);
        if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            let header = std::str::from_utf8(&request[..end]).unwrap();
            let length: usize = header
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            if request.len() >= end + 4 + length {
                return ObservedRequest {
                    header: header.to_owned(),
                    body: serde_json::from_slice(&request[end + 4..end + 4 + length]).unwrap(),
                };
            }
        }
    }
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
        reply(response(status, &oversized), options.clone(), async |client| {
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
    reply(bytes, options.clone(), async |client| {
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
                ..options.clone()
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
        reply(bytes, options.clone(), async |client| {
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
}

#[compio::test]
async fn control_configuration_requires_the_control_principal_and_secure_origin() {
    for signer in [
        worker_auth(),
        auth(ServiceIssuer::parse("spiffe://zeroship.ai/svc/worker").unwrap()),
        auth(ServiceIssuer::parse("spiffe://zeroship.ai/svc/gateway").unwrap()),
        auth(ServiceIssuer::parse("spiffe://other.example/svc/control").unwrap()),
        Arc::new(ServiceAuth::unconfigured()),
    ] {
        assert_eq!(
            ControlCoordinator::new("https://coordinator.example", signer, Options::default())
                .unwrap_err(),
            Error::Unauthenticated
        );
    }
    for issuer in [
        "spiffe://zeroship.ai/svc/control",
        "spiffe://zeroship.ai/svc/control/control-a",
    ] {
        ControlCoordinator::new(
            "https://coordinator.example",
            auth(ServiceIssuer::parse(issuer).unwrap()),
            Options::default(),
        )
        .unwrap();
    }
    for url in [
        "http://coordinator.example",
        "http://localhost",
        "https://user:secret@coordinator.example",
        "https://coordinator.example/prefix",
        "https://coordinator.example?token=secret",
    ] {
        assert_eq!(
            ControlCoordinator::new(url, control_auth(), Options::default()).unwrap_err(),
            Error::InvalidConfig
        );
    }
    let client = ControlCoordinator::new(
        "http://127.0.0.1:1",
        control_auth(),
        Options {
            max_request_bytes: 1,
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(
        client
            .verify_assignment(&VerifyAssignment {
                app_id: AppId::mint(),
                worker_id: WorkerId::mint(),
                assignment_revision: 1.try_into().unwrap(),
            })
            .await
            .unwrap_err(),
        Error::RequestTooLarge
    );
}

#[compio::test]
async fn control_assignment_verification_cannot_change_the_requested_authority() {
    let request = VerifyAssignment {
        app_id: AppId::mint(),
        worker_id: WorkerId::mint(),
        assignment_revision: 3.try_into().unwrap(),
    };
    let valid =
        json!({"appId":request.app_id,"workerId":request.worker_id,"revision":3,"expiresAt":1});
    control_reply(
        response(200, &valid),
        endpoints::WORKFLOW_VERIFY_ASSIGNMENT,
        &request,
        async |client| {
            assert_eq!(
                serde_json::to_value(client.verify_assignment(&request).await.unwrap()).unwrap(),
                valid
            );
        },
    )
    .await;
    for (field, changed) in [
        ("appId", json!(AppId::mint())),
        ("workerId", json!(WorkerId::mint())),
        ("revision", json!(2)),
        ("revision", json!(4)),
        ("expiresAt", json!(-1)),
        ("history", json!(["customer-secret"])),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = changed;
        control_reply(
            response(200, &invalid),
            endpoints::WORKFLOW_VERIFY_ASSIGNMENT,
            &request,
            async |client| {
                assert_eq!(
                    client.verify_assignment(&request).await.unwrap_err(),
                    Error::InvalidResponse
                );
            },
        )
        .await;
    }
}

#[compio::test]
async fn control_management_receipts_are_bound_and_closed() {
    let request = ManageRun {
        request_id: RequestId::mint(),
        app_id: AppId::mint(),
        run_id: RunId::mint(),
        command: ManagementOperation::Transition {
            operation: RunOperation::Cancel,
        },
    };
    let status = ManagementStatus {
        app_id: request.app_id.clone(),
        request_id: request.request_id.clone(),
    };
    for outcome in [
        json!(null),
        json!({"kind":"conflict"}),
        json!({"kind":"applied","state":"cancelled"}),
    ] {
        let receipt =
            json!({"appId":request.app_id,"requestId":request.request_id,"outcome":outcome});
        control_reply(
            response(200, &receipt),
            endpoints::WORKFLOW_MANAGE,
            &request,
            async |client| {
                assert_eq!(
                    serde_json::to_value(client.manage(&request).await.unwrap()).unwrap(),
                    receipt
                );
            },
        )
        .await;
        control_reply(
            response(200, &receipt),
            endpoints::WORKFLOW_MANAGEMENT_STATUS,
            &status,
            async |client| {
                assert_eq!(
                    serde_json::to_value(client.management_status(&status).await.unwrap().unwrap())
                        .unwrap(),
                    receipt
                );
            },
        )
        .await;
    }
    control_reply(
        response(200, &json!(null)),
        endpoints::WORKFLOW_MANAGEMENT_STATUS,
        &status,
        async |client| {
            assert!(client.management_status(&status).await.unwrap().is_none());
        },
    )
    .await;
    control_reply(
        response(200, &json!(null)),
        endpoints::WORKFLOW_MANAGE,
        &request,
        async |client| {
            assert_eq!(
                client.manage(&request).await.unwrap_err(),
                Error::InvalidResponse
            );
        },
    )
    .await;
    for receipt in [
        json!({"appId":AppId::mint(),"requestId":request.request_id,"outcome":null}),
        json!({"appId":request.app_id,"requestId":RequestId::mint(),"outcome":null}),
        json!({"appId":request.app_id,"requestId":request.request_id,"outcome":{"kind":"denied","secret":"customer-data"}}),
        json!({"appId":request.app_id,"requestId":request.request_id,"outcome":null,"output":"customer-data"}),
    ] {
        control_reply(
            response(200, &receipt),
            endpoints::WORKFLOW_MANAGE,
            &request,
            async |client| {
                assert_eq!(
                    client.manage(&request).await.unwrap_err(),
                    Error::InvalidResponse
                );
            },
        )
        .await;
        control_reply(
            response(200, &receipt),
            endpoints::WORKFLOW_MANAGEMENT_STATUS,
            &status,
            async |client| {
                assert_eq!(
                    client.management_status(&status).await.unwrap_err(),
                    Error::InvalidResponse
                );
            },
        )
        .await;
    }
    for (status, body, expected) in [
        (
            409,
            json!({"code":"conflict"}),
            Error::Refused(FailureCode::Conflict),
        ),
        (
            503,
            json!({"code":"unavailable"}),
            Error::Refused(FailureCode::Unavailable),
        ),
        (
            409,
            json!({"code":"conflict","secret":"customer-data"}),
            Error::InvalidResponse,
        ),
        (401, json!({"code":"denied"}), Error::InvalidResponse),
    ] {
        control_reply(
            response(status, &body),
            endpoints::WORKFLOW_MANAGE,
            &request,
            async |client| {
                let error = client.manage(&request).await.unwrap_err();
                assert_eq!(error, expected);
                assert!(!format!("{error:?} {error}").contains("customer-data"));
            },
        )
        .await;
    }
}

#[compio::test]
async fn native_clients_mint_fresh_full_assertions_for_the_workflow_audience() {
    for (auth, is_control) in [(worker_auth(), false), (control_auth(), true)] {
        let (issuer, key) = auth.signing_identity().unwrap();
        let mut trust = ServiceTrustBundle::new();
        trust.trust_signing_key(issuer, key.key_id(), key).unwrap();
        let issuer = issuer.clone();
        let verifier = ServiceAssertionVerifier::new(trust, Arc::new(InMemoryReplayStore::new()));
        let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let worker = (!is_control)
            .then(|| WorkerCoordinator::new(&url, auth.clone(), Options::default()).unwrap());
        let control =
            is_control.then(|| ControlCoordinator::new(&url, auth, Options::default()).unwrap());
        let (app, placed) = (AppId::mint(), WorkerId::mint());
        let (endpoint, body, reply) = if is_control {
            (
                endpoints::WORKFLOW_VERIFY_ASSIGNMENT,
                json!({"appId":app,"workerId":placed,"assignmentRevision":1}),
                json!({"appId":app,"workerId":placed,"revision":1,"expiresAt":1}),
            )
        } else {
            (
                endpoints::WORKFLOW_ASSIGNMENTS,
                json!({"after":null}),
                json!([]),
            )
        };
        let encoded = serde_json::to_vec(&reply).unwrap();
        let server = async {
            let mut prior = None;
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                request.assert_operation(endpoint, &body);
                let assertion = request.bearer().unwrap();
                assert!(
                    prior.as_deref() != Some(assertion),
                    "client reused a service assertion"
                );
                let wrong_audience =
                    PeerCredentials::new(Some(assertion), None, "spiffe://zeroship.ai/svc/control");
                assert!(verify_identity(&verifier, &wrong_audience).await.is_err());
                let observed = PeerCredentials::new(Some(assertion), None, AUDIENCE);
                let identity = verify_identity(&verifier, &observed).await.unwrap();
                assert!(identity.matches_principal(issuer.principal()));
                assert!(
                    verify_identity(&verifier, &observed).await.is_err(),
                    "full assertions must reject replay"
                );
                prior = Some(assertion.to_owned());
                let mut response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                    encoded.len()
                )
                .into_bytes();
                response.extend_from_slice(&encoded);
                stream.write_all(response).await.0.unwrap();
            }
        };
        let calls = async {
            for _ in 0..2 {
                if let Some(client) = &worker {
                    assert!(client
                        .assignments(&ScopePage { after: None })
                        .await
                        .unwrap()
                        .is_empty());
                } else {
                    let assignment = control
                        .as_ref()
                        .unwrap()
                        .verify_assignment(&VerifyAssignment {
                            app_id: app.clone(),
                            worker_id: placed.clone(),
                            assignment_revision: 1.try_into().unwrap(),
                        })
                        .await
                        .unwrap();
                    assert_eq!(assignment.worker_id, placed);
                }
            }
        };
        Box::pin(compio::time::timeout(Duration::from_secs(10), async {
            futures::join!(server, calls);
        }))
        .await
        .expect("signed coordinator exchanges completed");
    }
}

/// `transition` is `manage` with the command assembled for the caller: the same
/// endpoint and the same body. The harness asserts both, so if this ever grows a
/// route of its own the test fails - which is the property the relocation plan
/// warns about at step 4, where a parallel endpoint doubles the manager's
/// request rate for no new capability.
///
/// Every outcome arm a transition can be answered with is exercised, plus the
/// `null` outcome that means the manager accepted the command and has not
/// applied it yet.
#[compio::test]
async fn control_transition_reuses_the_manage_exchange() {
    let request_id = RequestId::mint();
    let app_id = AppId::mint();
    let run_id = RunId::mint();
    let expected = ManageRun {
        request_id: request_id.clone(),
        app_id: app_id.clone(),
        run_id: run_id.clone(),
        command: ManagementOperation::Transition {
            operation: RunOperation::Pause,
        },
    };
    for outcome in [
        json!(null),
        json!({"kind":"applied","state":"paused"}),
        json!({"kind":"not_found"}),
        json!({"kind":"conflict"}),
        json!({"kind":"denied"}),
    ] {
        let receipt = json!({"appId":app_id,"requestId":request_id,"outcome":outcome});
        control_reply(
            response(200, &receipt),
            endpoints::WORKFLOW_MANAGE,
            &expected,
            async |client| {
                assert_eq!(
                    serde_json::to_value(
                        client
                            .transition(&request_id, &app_id, &run_id, RunOperation::Pause)
                            .await
                            .unwrap()
                    )
                    .unwrap(),
                    receipt
                );
            },
        )
        .await;
    }
}

/// `restart` is `manage` with the command assembled for the caller: the same
/// endpoint and the same body. The harness asserts both, so if this ever grows
/// a route of its own the test fails - which is the property the relocation
/// plan warns about at step 4, where a parallel endpoint doubles the manager's
/// request rate for no new capability.
///
/// The restart receipt carries what a transition receipt cannot: the retained
/// prefix and the deployment the new generation replays against. Both arrive
/// here as JSON and leave as JSON, so a field dropped from the variant stops
/// round-tripping and fails.
///
/// This does NOT catch a manager that answers a restart with a transition
/// receipt: the peer is a fixed reply, so only the command this client sends
/// and the receipt it decodes are bound here.
#[compio::test]
async fn control_restart_reuses_the_manage_exchange() {
    let request_id = RequestId::mint();
    let app_id = AppId::mint();
    let run_id = RunId::mint();
    let pinned_to = DeploymentId::mint();
    let options = RestartOptions {
        from: Some(RestartTarget {
            name: "checkpoint".into(),
            occurrence: Some(2),
        }),
        deploy: Some(RestartDeploy::Started),
    };
    let expected = ManageRun {
        request_id: request_id.clone(),
        app_id: app_id.clone(),
        run_id: run_id.clone(),
        command: ManagementOperation::Restart {
            options: options.clone(),
        },
    };
    for outcome in [
        json!(null),
        json!({"kind":"restarted","state":"queued","restartedFromOrdinal":2,
            "pinnedTo":pinned_to}),
        json!({"kind":"restarted","state":"queued","restartedFromOrdinal":null,
            "pinnedTo":pinned_to}),
        json!({"kind":"not_found"}),
        json!({"kind":"conflict"}),
        json!({"kind":"denied"}),
    ] {
        let receipt = json!({"appId":app_id,"requestId":request_id,"outcome":outcome});
        control_reply(
            response(200, &receipt),
            endpoints::WORKFLOW_MANAGE,
            &expected,
            async |client| {
                assert_eq!(
                    serde_json::to_value(
                        client
                            .restart(&request_id, &app_id, &run_id, options.clone())
                            .await
                            .unwrap()
                    )
                    .unwrap(),
                    receipt
                );
            },
        )
        .await;
    }
}
