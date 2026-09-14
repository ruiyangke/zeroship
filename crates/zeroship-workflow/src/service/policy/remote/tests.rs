#![expect(
    clippy::future_not_send,
    reason = "policy refresh and native sockets stay on their owning compio runtime"
)]

use super::*;
use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use futures::{channel::oneshot, future::Either};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use zeroship_core::{
    app_id::AppId,
    service_assertion::{
        ServiceIssuer, ServiceSigningKey, ServiceTrustBundle, TransportAssertionVerifier,
    },
    service_peers::{ServiceAuth, ServiceKeyring},
    workflow_coordination::{Revision, WorkerId},
    workflow_policy::{AppPolicy, PolicyLease, PolicyLeaseRequest},
};
use zeroship_workflow_client::Options;

struct Fixture {
    registry: Arc<HostPolicies>,
    auth: Arc<ServiceAuth>,
    scope: AssignedScope,
    worker: WorkerId,
    policy: AppPolicy,
    revision: Revision,
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
            registry: Arc::new(HostPolicies::default()),
            auth,
            scope: AssignedScope {
                app_id: AppId::mint(),
                assignment_revision: 3.try_into().unwrap(),
            },
            worker,
            policy: AppPolicy::default(),
            revision: 7.try_into().unwrap(),
        }
    }

    fn reply(&self, scope: &AssignedScope, remaining_ms: u64) -> Exchange {
        Exchange {
            request: json!(PolicyLeaseRequest {
                scope: scope.clone(),
                establish_after: None,
                ingress_used: false,
            }),
            response: json!(PolicyLease {
                app_id: scope.app_id.clone(),
                worker_id: self.worker.clone(),
                signing_key_id: self.auth.signing_identity().unwrap().1.key_id(),
                assignment_revision: scope.assignment_revision,
                policy_revision: self.revision,
                policy: self.policy.clone(),
                ingress_epoch: Some(1.try_into().unwrap()),
                remaining_ms: remaining_ms.try_into().unwrap(),
            }),
            status: 200,
            gate: None,
        }
    }

    fn assigned(&self, client: WorkerCoordinator) -> AssignedPolicies {
        AssignedPolicies::new(&self.registry, client, self.scope.clone()).unwrap()
    }
}

struct Gate {
    arrived: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

struct Exchange {
    request: Value,
    response: Value,
    status: u16,
    gate: Option<Gate>,
}

impl Exchange {
    fn unavailable(mut self) -> Self {
        self.response = json!({"code":"unavailable"});
        self.status = 503;
        self
    }

    fn gated(mut self) -> (Self, oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (arrived, observed) = oneshot::channel();
        let (release, blocked) = oneshot::channel();
        self.gate = Some(Gate {
            arrived,
            release: blocked,
        });
        (self, observed, release)
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
        let (done, completed) = oneshot::channel();
        let server = async {
            let mut responses = Vec::new();
            for exchange in exchanges {
                let (mut stream, _) = listener.accept().await.unwrap();
                assert_eq!(request(&mut stream).await, exchange.request);
                responses.push(compio::runtime::spawn(async move {
                if let Some(gate) = exchange.gate {
                    gate.arrived.send(()).unwrap();
                    gate.release.await.unwrap();
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
            }));
            }
            for response in responses {
                response.await.unwrap();
            }
            let tail = futures::future::select(completed, Box::pin(listener.accept())).await;
            match tail {
                Either::Left((result, _)) => result.unwrap(),
                Either::Right(_) => panic!("unexpected policy refresh request"),
            }
        };
        compio::time::timeout(Duration::from_secs(10), async {
            futures::join!(server, async {
                test(client).await;
                done.send(()).unwrap();
            });
        })
        .await
        .expect("assigned policy exchange hung");
    })
}

async fn request(stream: &mut compio::net::TcpStream) -> Value {
    let mut bytes = Vec::new();
    loop {
        let compio::BufResult(read, buffer) = stream.read(vec![0; 1024]).await;
        let read = read.unwrap();
        assert_ne!(read, 0, "refresh closed before its request was complete");
        bytes.extend_from_slice(&buffer[..read]);
        assert!(bytes.len() <= 16 * 1024);
        let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let header = std::str::from_utf8(&bytes[..end]).unwrap();
        let mut length = None;
        for line in header.lines().skip(1) {
            let (name, value) = line.split_once(':').unwrap();
            if name.eq_ignore_ascii_case("content-length") {
                assert!(length.is_none());
                length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
        let length = length.unwrap();
        if bytes.len() < end + 4 + length {
            continue;
        }
        assert_eq!(bytes.len(), end + 4 + length);
        return serde_json::from_slice(&bytes[end + 4..]).unwrap();
    }
}

fn assert_unavailable<T>(result: Result<T, WorkflowServiceError>) {
    assert!(matches!(
        result.err(),
        Some(WorkflowServiceError::Unavailable(_))
    ));
}

#[compio::test]
async fn delayed_refresh_cannot_reverse_a_newer_shorter_grant() {
    let fixture = Fixture::new();
    let (first, arrived, release) = fixture.reply(&fixture.scope, 60_000).gated();
    peer(
        &fixture,
        vec![first, fixture.reply(&fixture.scope, 30_000)],
        async |client| {
            let assigned = fixture.assigned(client);
            let newer = assigned.clone();
            let driver = async {
                arrived.await.unwrap();
                newer.refresh().await.unwrap();
                let authority = newer.binding().authority().unwrap();
                release.send(()).unwrap();
                authority
            };
            let (stale, current) = futures::join!(assigned.refresh(), driver);
            assert_unavailable(stale);
            current.check().unwrap();
            assert_eq!(
                assigned.binding().authority().unwrap().deadline,
                current.deadline
            );
            assert!(assigned.binding().same_binding(newer.binding()));
        },
    )
    .await;
}

#[compio::test]
async fn delayed_failure_cannot_revoke_a_newer_successful_refresh() {
    let fixture = Fixture::new();
    let (first, arrived, release) = fixture.reply(&fixture.scope, 60_000).unavailable().gated();
    peer(
        &fixture,
        vec![first, fixture.reply(&fixture.scope, 30_000)],
        async |client| {
            let assigned = fixture.assigned(client);
            let driver = async {
                arrived.await.unwrap();
                assigned.refresh().await.unwrap();
                let authority = assigned.binding().authority().unwrap();
                release.send(()).unwrap();
                authority
            };
            let (failed, current) = futures::join!(assigned.refresh(), driver);
            assert_unavailable(failed);
            current.check().unwrap();
            assert_eq!(
                assigned.binding().authority().unwrap().deadline,
                current.deadline
            );
        },
    )
    .await;
}

#[compio::test]
async fn replacement_rejects_a_pending_response_for_the_old_assignment() {
    let fixture = Fixture::new();
    let replacement_scope = AssignedScope {
        assignment_revision: 9.try_into().unwrap(),
        ..fixture.scope.clone()
    };
    let (first, arrived, release) = fixture.reply(&fixture.scope, 60_000).gated();
    peer(
        &fixture,
        vec![first, fixture.reply(&replacement_scope, 30_000)],
        async |client| {
            let assigned = fixture.assigned(client.clone());
            let driver = async {
                arrived.await.unwrap();
                let replacement =
                    AssignedPolicies::new(&fixture.registry, client, replacement_scope.clone())
                        .unwrap();
                assert!(!assigned.binding().same_binding(replacement.binding()));
                assert_eq!(replacement.scope(), &replacement_scope);
                replacement.refresh().await.unwrap();
                release.send(()).unwrap();
                replacement
            };
            let (stale, replacement) = futures::join!(assigned.refresh(), driver);
            assert_unavailable(stale);
            assert_unavailable(assigned.binding().authority());
            replacement.binding().authority().unwrap().check().unwrap();
            assert_eq!(assigned.scope(), &fixture.scope);
        },
    )
    .await;
}

#[compio::test]
async fn healthy_refresh_keeps_binding_and_raw_source_revision() {
    let fixture = Fixture::new();
    peer(
        &fixture,
        vec![
            fixture.reply(&fixture.scope, 30_000),
            fixture.reply(&fixture.scope, 60_000),
        ],
        async |client| {
            let assigned = fixture.assigned(client);
            let original_binding = assigned.binding().clone();
            assigned.refresh().await.unwrap();
            let original = original_binding.authority().unwrap();
            assigned.clone().refresh().await.unwrap();
            let current = assigned.binding().authority().unwrap();
            assert!(original_binding.same_binding(assigned.binding()));
            assert_eq!(current.revision, fixture.revision);
            assert_eq!(current.revision, original.revision);
            assert_eq!(current.policy, fixture.policy);
            assert!(current.deadline > original.deadline);
            original.check().unwrap();
        },
    )
    .await;
}

#[compio::test]
async fn remote_assignment_explicitly_replaces_configured_generation() {
    let fixture = Fixture::new();
    let configured = fixture.registry.bind(fixture.scope.app_id.clone()).unwrap();
    configured
        .begin_refresh()
        .unwrap()
        .install(PolicySnapshot::configuration(fixture.revision, fixture.policy.clone()).unwrap())
        .unwrap();
    let configured_authority = configured.authority().unwrap();
    peer(
        &fixture,
        vec![fixture.reply(&fixture.scope, 30_000)],
        async |client| {
            let assigned = fixture.assigned(client);
            assert!(!assigned.binding().same_binding(&configured));
            assert_unavailable(configured_authority.check());
            assert_unavailable(assigned.binding().authority());
            assigned.refresh().await.unwrap();
            let remote = assigned.binding().authority().unwrap();
            assert_eq!(remote.revision, fixture.revision);
            assert_eq!(remote.policy, fixture.policy);
            assert!(remote.deadline.is_some());
        },
    )
    .await;
}

#[compio::test]
async fn installation_preserves_the_original_validated_client_deadline() {
    let fixture = Fixture::new();
    peer(
        &fixture,
        vec![fixture.reply(&fixture.scope, 30_000)],
        async |client| {
            let assigned = fixture.assigned(client.clone());
            let ticket = assigned.binding().begin_refresh().unwrap();
            let lease = client
                .policy_lease(&PolicyLeaseRequest {
                    scope: assigned.scope().clone(),
                    establish_after: None,
                    ingress_used: false,
                })
                .await
                .unwrap();
            let original = lease.expires_at();
            assert!(original > Instant::now());
            assigned.install(ticket, &lease).unwrap();
            assert_eq!(
                assigned.binding().authority().unwrap().deadline,
                Some(original)
            );
        },
    )
    .await;
}

#[compio::test]
async fn source_failure_leaves_previous_authority_at_its_original_deadline() {
    let fixture = Fixture::new();
    peer(
        &fixture,
        vec![
            fixture.reply(&fixture.scope, 30_000),
            fixture.reply(&fixture.scope, 60_000).unavailable(),
        ],
        async |client| {
            let assigned = fixture.assigned(client);
            assigned.refresh().await.unwrap();
            let original = assigned.binding().authority().unwrap();
            assert_unavailable(assigned.refresh().await);
            original.check().unwrap();
            assert_eq!(
                assigned.binding().authority().unwrap().deadline,
                original.deadline
            );
        },
    )
    .await;
}
