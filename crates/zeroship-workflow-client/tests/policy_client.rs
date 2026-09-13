//! Policy leases bind the enrolled signer and retain their original clock budget.
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
    service_identity::{endpoints, verify_service_call},
    service_peers::{ServiceAuth, ServiceKeyring},
    workflow_coordination::{AssignedScope, FailureCode, Revision, WorkerId, AUDIENCE},
    workflow_policy::{AppPolicy, PolicyLease},
};
use zeroship_workflow_client::{Error, Options, WorkerCoordinator};

struct Fixture {
    auth: Arc<ServiceAuth>,
    scope: AssignedScope,
    worker: WorkerId,
    revision: Revision,
    policy: AppPolicy,
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
        Self {
            auth,
            scope: AssignedScope {
                app_id: AppId::mint(),
                assignment_revision: 3.try_into().unwrap(),
            },
            worker,
            revision: 7.try_into().unwrap(),
            policy: AppPolicy {
                admission: false,
                dispatch: false,
                ..AppPolicy::default()
            },
        }
    }

    fn reply(&self, remaining_ms: u64) -> Value {
        json!(PolicyLease {
            app_id: self.scope.app_id.clone(),
            worker_id: self.worker.clone(),
            signing_key_id: self.auth.signing_identity().unwrap().1.key_id(),
            assignment_revision: self.scope.assignment_revision,
            policy_revision: self.revision,
            policy: self.policy.clone(),
            remaining_ms: remaining_ms.try_into().unwrap(),
        })
    }
}

struct Exchange {
    response: Value,
    status: u16,
    delay: Duration,
    received: Option<oneshot::Sender<Instant>>,
}

impl Exchange {
    const fn new(response: Value) -> Self {
        Self {
            response,
            status: 200,
            delay: Duration::ZERO,
            received: None,
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
                let arrived = Instant::now();
                assert_eq!(
                    observed.path,
                    endpoints::WORKFLOW_POLICY_LEASE.path_template()
                );
                assert_eq!(observed.body, json!(fixture.scope));
                assert_ne!(previous.as_ref(), Some(&observed.authorization));
                verify_service_call(
                    &verifier,
                    Some(&observed.authorization),
                    AUDIENCE,
                    endpoints::WORKFLOW_POLICY_LEASE,
                )
                .await
                .unwrap();
                assert!(verify_service_call(
                    &verifier,
                    Some(&observed.authorization),
                    AUDIENCE,
                    endpoints::WORKFLOW_POLICY_LEASE,
                )
                .await
                .is_err());
                previous = Some(observed.authorization);
                if let Some(received) = exchange.received {
                    received.send(arrived).unwrap();
                }
                if !exchange.delay.is_zero() {
                    compio::time::sleep(exchange.delay).await;
                    assert!(arrived.elapsed() >= exchange.delay);
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
            let tail = futures::future::select(completed, Box::pin(listener.accept())).await;
            match tail {
                Either::Left((result, _)) => result.unwrap(),
                Either::Right(_) => panic!("client sent an unexpected policy request"),
            }
        };
        compio::time::timeout(Duration::from_secs(10), async {
            futures::join!(server, async {
                test(client).await;
                done.send(()).unwrap();
            });
        })
        .await
        .expect("policy client contract hung");
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
async fn leases_preserve_raw_policy_identity_and_original_deadline() {
    let fixture = Fixture::new();
    let (received, observed) = oneshot::channel();
    let mut exchange = Exchange::new(fixture.reply(60_000));
    exchange.delay = Duration::from_millis(100);
    exchange.received = Some(received);
    peer(
        &fixture,
        vec![exchange, Exchange::new(fixture.reply(30_000))],
        async |client| {
            let lease = client.policy_lease(&fixture.scope).await.unwrap();
            let arrived = observed.await.unwrap();
            assert_eq!(lease.app_id(), &fixture.scope.app_id);
            assert_eq!(lease.worker_id(), &fixture.worker);
            assert_eq!(lease.signing_key_id(), client.signing_key_id());
            assert_eq!(
                lease.signing_key_id(),
                fixture.auth.signing_identity().unwrap().1.key_id()
            );
            assert_eq!(
                lease.assignment_revision(),
                fixture.scope.assignment_revision
            );
            assert_eq!(lease.revision(), fixture.revision);
            assert_ne!(lease.revision(), lease.assignment_revision());
            assert_eq!(lease.policy(), &fixture.policy);
            assert!(!lease.policy().admission);
            assert!(!lease.policy().dispatch);
            assert!(lease.expires_at() <= arrived + Duration::from_secs(60));
            assert!(lease.remaining().unwrap() < Duration::from_secs(60));
            let cloned = lease.clone();
            assert_eq!(cloned.expires_at(), lease.expires_at());
            let next = client.clone().policy_lease(&fixture.scope).await.unwrap();
            assert_eq!(next.revision(), lease.revision());
            assert_eq!(next.policy(), lease.policy());
            assert!(next.remaining().unwrap() < Duration::from_secs(30));
            assert_eq!(cloned.expires_at(), lease.expires_at());
        },
    )
    .await;
}

#[compio::test]
async fn policy_lease_refuses_substituted_scope_and_signer() {
    let fixture = Fixture::new();
    let substitutions = [
        ("appId", json!(AppId::mint())),
        ("workerId", json!(WorkerId::mint())),
        (
            "signingKeyId",
            json!(ServiceSigningKey::generate().key_id()),
        ),
        ("assignmentRevision", json!(9)),
    ];
    let mut exchanges = Vec::new();
    for (field, value) in &substitutions {
        let mut reply = fixture.reply(60_000);
        reply[*field] = value.clone();
        exchanges.push(Exchange::new(reply));
    }
    exchanges.push(Exchange::new(fixture.reply(60_000)));
    peer(&fixture, exchanges, async |client| {
        for (field, _) in substitutions {
            assert_eq!(
                client.policy_lease(&fixture.scope).await.unwrap_err(),
                Error::InvalidResponse,
                "substituted {field}"
            );
        }
        assert_eq!(
            client.policy_lease(&fixture.scope).await.unwrap().policy(),
            &fixture.policy
        );
    })
    .await;
}

#[compio::test]
async fn policy_lease_rejects_open_incomplete_and_invalid_policy() {
    let fixture = Fixture::new();
    let mut replies = Vec::new();
    for (field, value) in [
        ("credentials", json!({"password":"untrusted"})),
        ("history", json!([])),
        ("input", json!({"arbitrary":true})),
    ] {
        let mut outer = fixture.reply(60_000);
        outer[field] = value.clone();
        replies.push(outer);
        let mut nested = fixture.reply(60_000);
        nested["policy"][field] = value;
        replies.push(nested);
    }
    let mut missing = fixture.reply(60_000);
    missing["policy"]
        .as_object_mut()
        .unwrap()
        .remove("admission");
    replies.push(missing);
    for (field, value) in [
        ("leaseMs", json!(0)),
        ("maxInputBytes", json!(0)),
        ("maxLiveRuns", json!(-1)),
        ("maxPayloadStorageBytes", json!(1)),
        ("maxChildDepth", json!(u64::MAX)),
        ("admission", json!({"enabled":true})),
    ] {
        let mut invalid = fixture.reply(60_000);
        invalid["policy"][field] = value;
        replies.push(invalid);
    }
    let mut revision = fixture.reply(60_000);
    revision["policyRevision"] = json!(0);
    replies.push(revision);
    let rejected = replies.len();
    let exchanges = replies.into_iter().map(Exchange::new).collect();
    peer(&fixture, exchanges, async |client| {
        assert_ne!(rejected, 0);
        for _ in 0..rejected {
            assert_eq!(
                client.policy_lease(&fixture.scope).await.unwrap_err(),
                Error::InvalidResponse
            );
        }
    })
    .await;
}

#[compio::test]
async fn policy_lease_rejects_unrepresentable_or_missing_duration() {
    let fixture = Fixture::new();
    let durations = [
        json!(0),
        json!(-1),
        json!(u64::MAX),
        json!(1.5),
        Value::Null,
    ];
    let mut exchanges = Vec::new();
    for duration in &durations {
        let mut reply = fixture.reply(60_000);
        reply["remainingMs"] = duration.clone();
        exchanges.push(Exchange::new(reply));
    }
    let mut missing = fixture.reply(60_000);
    missing.as_object_mut().unwrap().remove("remainingMs");
    exchanges.push(Exchange::new(missing));
    peer(&fixture, exchanges, async |client| {
        for _ in 0..=durations.len() {
            assert_eq!(
                client.policy_lease(&fixture.scope).await.unwrap_err(),
                Error::InvalidResponse
            );
        }
    })
    .await;
}

#[compio::test]
async fn a_delayed_policy_reply_cannot_restart_exhausted_authority() {
    let fixture = Fixture::new();
    let mut exchange = Exchange::new(fixture.reply(50));
    exchange.delay = Duration::from_millis(100);
    peer(&fixture, vec![exchange], async |client| {
        assert_eq!(
            client.policy_lease(&fixture.scope).await.unwrap_err(),
            Error::Timeout
        );
    })
    .await;
}

#[compio::test]
async fn policy_lease_refuses_authority_beyond_its_raw_policy_ceiling() {
    let fixture = Fixture::new();
    let ceiling = u64::try_from(fixture.policy.lease_ms).unwrap();
    peer(
        &fixture,
        vec![
            Exchange::new(fixture.reply(ceiling + 1)),
            Exchange::new(fixture.reply(ceiling)),
        ],
        async |client| {
            assert_eq!(
                client.policy_lease(&fixture.scope).await.unwrap_err(),
                Error::InvalidResponse
            );
            assert_eq!(
                client.policy_lease(&fixture.scope).await.unwrap().policy(),
                &fixture.policy
            );
        },
    )
    .await;
}

#[compio::test]
async fn cloning_policy_never_renews_its_original_expiration() {
    let fixture = Fixture::new();
    peer(
        &fixture,
        vec![Exchange::new(fixture.reply(500))],
        async |client| {
            let lease = client.policy_lease(&fixture.scope).await.unwrap();
            let copy = lease.clone();
            compio::time::sleep(lease.remaining().unwrap()).await;
            assert_eq!(lease.remaining(), Err(Error::Timeout));
            assert_eq!(copy.remaining(), Err(Error::Timeout));
            assert_eq!(lease.expires_at(), copy.expires_at());
        },
    )
    .await;
}

#[compio::test]
async fn missing_source_authority_stays_unavailable_without_default_policy() {
    let fixture = Fixture::new();
    let mut exchange = Exchange::new(json!({"code":"unavailable"}));
    exchange.status = 503;
    peer(&fixture, vec![exchange], async |client| {
        assert_eq!(
            client.policy_lease(&fixture.scope).await.unwrap_err(),
            Error::Refused(FailureCode::Unavailable)
        );
    })
    .await;
}
