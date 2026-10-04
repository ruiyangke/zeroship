//! Native transport checks for certificate rejection and interrupted exchanges.
#![allow(
    clippy::future_not_send,
    reason = "test sockets stay on their compio runtime"
)]

use compio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use compio_tls::{TlsAcceptor, TlsConnector};
use futures::{channel::oneshot, future::Either};
use rustls::pki_types::PrivateKeyDer;
use std::{num::{NonZeroU32, NonZeroU64}, sync::Arc, time::Duration};
use zeroship_core::{
    service_assertion::{
        ServiceIssuer, ServiceSigningKey, ServiceTrustBundle, TransportAssertionVerifier,
    },
    service_identity::endpoints,
    service_peers::{ServiceAuth, ServiceKeyring},
    workflow_coordination::WorkerId,
    workflow_jobs::{ClaimJobs, ClaimedJobs},
};
use zeroship_workflow_client::{Error, JobJournal, Options, WorkerCoordinator};

/// The default exchange bounds derive from the platform ceilings for the
/// quantities this transport carries, so a host taking them can carry anything
/// admission admits without a literal repeated on either side: a request one
/// creator input, a settlement one journal, and a response the largest claim
/// reply.
#[test]
fn default_exchange_bounds_derive_from_the_platform_ceilings() {
    use zeroship_core::workflow_policy::{MAX_INPUT_BYTES_CEILING, MAX_JOURNAL_BYTES_CEILING};
    let options = Options::default();
    assert_eq!(options.max_request_bytes, MAX_INPUT_BYTES_CEILING);
    assert_eq!(options.max_journal_request_bytes, MAX_JOURNAL_BYTES_CEILING);
    assert_eq!(options.max_response_bytes, ClaimJobs::MAX_REPLY_BYTES);
}

/// A worker accepts a claim reply of the size the claim contract lets the
/// service send, so a worker configured to refuse one is refused when it is
/// built; the control is the same configuration at the bound.
#[test]
fn a_worker_accepts_the_largest_claim_reply() {
    let at = Options {
        max_response_bytes: ClaimJobs::MAX_REPLY_BYTES,
        ..Options::default()
    };
    assert!(WorkerCoordinator::new("http://127.0.0.1:1", worker_auth(), at).is_ok());
    let below = Options {
        max_response_bytes: ClaimJobs::MAX_REPLY_BYTES - 1,
        ..Options::default()
    };
    assert_eq!(
        WorkerCoordinator::new("http://127.0.0.1:1", worker_auth(), below).err(),
        Some(Error::InvalidConfig)
    );
}

fn worker_auth() -> Arc<ServiceAuth> {
    let issuer = ServiceIssuer::parse(&format!(
        "spiffe://zeroship.ai/svc/worker/{}",
        WorkerId::mint().as_str()
    ))
    .unwrap();
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

fn claim() -> ClaimJobs {
    ClaimJobs {
        max: NonZeroU32::new(3).unwrap(),
        wait_ms: NonZeroU64::new(100).unwrap(),
        after: None,
        exclude: Vec::new(),
    }
}

struct JsonJournal;
impl JobJournal for JsonJournal {
    type Receipt = serde_json::Value;
    type Claim = serde_json::Value;
    type Acceptance = serde_json::Value;
    type Renewal = serde_json::Value;
    type Execution = serde_json::Value;
}

struct Request {
    method: String,
    path: String,
    authorization: Option<String>,
    body: Vec<u8>,
}

async fn request(stream: &mut impl AsyncRead) -> Request {
    let mut bytes = Vec::new();
    loop {
        let compio::BufResult(read, buffer) = stream.read(vec![0; 1024]).await;
        let read = read.unwrap();
        assert_ne!(read, 0, "peer closed before its request completed");
        bytes.extend_from_slice(&buffer[..read]);
        assert!(
            bytes.len() <= 16 * 1024,
            "fixture request exceeded its bound"
        );
        let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&bytes[..end]).unwrap();
        let mut lines = headers.lines();
        let mut start = lines.next().unwrap().split_ascii_whitespace();
        let method = start.next().unwrap().to_owned();
        let path = start.next().unwrap().to_owned();
        let mut length = None;
        let mut authorization = None;
        for line in lines {
            let (name, value) = line.split_once(':').unwrap();
            if name.eq_ignore_ascii_case("content-length") {
                assert!(length.is_none(), "duplicate request length");
                length = Some(value.trim().parse::<usize>().unwrap());
            } else if name.eq_ignore_ascii_case("authorization") {
                assert!(authorization.is_none(), "duplicate request authorization");
                authorization = Some(value.trim().to_owned());
            }
        }
        let length = length.expect("fixture requests declare their length");
        assert!(length <= 16 * 1024, "fixture body exceeded its bound");
        if bytes.len() < end + 4 + length {
            continue;
        }
        assert_eq!(
            bytes.len(),
            end + 4 + length,
            "unexpected pipelined request"
        );
        return Request {
            method,
            path,
            authorization,
            body: bytes[end + 4..].to_vec(),
        };
    }
}

fn claim_assertion(request: Request) -> String {
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, endpoints::WORKFLOW_JOB_CLAIM.path_template());
    assert_eq!(
        serde_json::from_slice::<ClaimJobs>(&request.body).unwrap(),
        claim()
    );
    let token = request
        .authorization
        .expect("worker request has an assertion");
    assert!(token.starts_with("Bearer "));
    token
}

fn response(body: &[u8]) -> Vec<u8> {
    let mut bytes = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

#[compio::test]
async fn untrusted_tls_certificate_is_rejected_before_http_authorization() {
    let certificate = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![certificate.cert.der().clone()],
            PrivateKeyDer::Pkcs8(certificate.signing_key.serialize_der().into()),
        )
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(server));
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certificate.cert.der().clone()).unwrap();
    let trusted = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let trusted = TlsConnector::from(Arc::new(trusted));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let client = WorkerCoordinator::new(
        &format!("https://{address}"),
        worker_auth(),
        Options::default(),
    )
    .unwrap();
    let reply = response(br#""ready""#);
    let peer = async {
        let mut connections = 0;
        let mut http_requests = 0;
        let (stream, _) = listener.accept().await.unwrap();
        connections += 1;
        let mut tls = acceptor.accept(stream).await.unwrap();
        let probe = request(&mut tls).await;
        http_requests += 1;
        assert_eq!(probe.method, "GET");
        assert_eq!(probe.path, "/probe");
        assert!(probe.authorization.is_none());
        tls.write_all(reply.clone()).await.0.unwrap();
        tls.flush().await.unwrap();
        drop(tls);

        let (stream, _) = listener.accept().await.unwrap();
        connections += 1;
        assert!(
            acceptor.accept(stream).await.is_err(),
            "untrusted certificate was accepted before HTTP authorization"
        );
        (connections, http_requests)
    };
    let caller = async {
        // A trusted probe establishes that this certificate and listener work.
        let stream = TcpStream::connect(address).await.unwrap();
        let mut tls = trusted.connect("127.0.0.1", stream).await.unwrap();
        tls.write_all(
            b"GET /probe HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n".to_vec(),
        )
        .await
        .0
        .unwrap();
        tls.flush().await.unwrap();
        let compio::BufResult(read, received) = tls.read_exact(vec![0; reply.len()]).await;
        read.unwrap();
        assert_eq!(received, reply);
        drop(tls);
        assert_eq!(
            client.claim_jobs::<JsonJournal>(&claim()).await.unwrap_err(),
            Error::Unavailable
        );
    };
    let (observed, ()) = Box::pin(compio::time::timeout(Duration::from_secs(10), async {
        futures::join!(peer, caller)
    }))
    .await
    .expect("TLS verification contract hung");
    assert_eq!(observed, (2, 1));
}

#[derive(Clone, Copy)]
enum Interruption {
    Timeout,
    CallerCancellation,
}

async fn reuse_after_interruption(interruption: Interruption) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = WorkerCoordinator::new(
        &format!("http://{}", listener.local_addr().unwrap()),
        worker_auth(),
        Options {
            timeout: Duration::from_secs(1),
            ..Options::default()
        },
    )
    .unwrap();
    let claimed = ClaimedJobs::<serde_json::Value> {
        deliveries: Vec::new(),
        after: None,
        lap_complete: true,
    };
    let complete_reply = response(&serde_json::to_vec(&claimed).unwrap());
    let (partial_sent, partial_received) = oneshot::channel();
    let peer = async {
        let mut connections = 0;
        let mut requests = 0;
        let (mut first, _) = listener.accept().await.unwrap();
        connections += 1;
        let first_assertion = claim_assertion(request(&mut first).await);
        requests += 1;
        first
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\n{\r\n".to_vec())
            .await
            .0
            .unwrap();
        first.flush().await.unwrap();
        partial_sent.send(()).unwrap();
        let abandoned = async {
            let compio::BufResult(read, _) = first.read(vec![0; 1024]).await;
            match read {
                Ok(0) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                _ => panic!("interrupted HTTP exchange was not closed"),
            }
        };
        let next = async {
            let (mut second, _) = listener.accept().await.unwrap();
            connections += 1;
            let next_assertion = claim_assertion(request(&mut second).await);
            requests += 1;
            assert!(
                first_assertion != next_assertion,
                "retry reused an assertion"
            );
            second.write_all(complete_reply).await.0.unwrap();
            second.flush().await.unwrap();
        };
        futures::join!(abandoned, next);
        (connections, requests)
    };
    let caller = async {
        let claim = claim();
        let mut pending = Box::pin(client.claim_jobs::<JsonJournal>(&claim));
        match futures::future::select(&mut pending, partial_received).await {
            Either::Left(_) => panic!("request completed before the partial response"),
            Either::Right((ready, _)) => ready.unwrap(),
        }
        match interruption {
            Interruption::Timeout => assert_eq!(pending.await.unwrap_err(), Error::Timeout),
            Interruption::CallerCancellation => drop(pending),
        }
        let observed = client.claim_jobs::<JsonJournal>(&claim).await.unwrap();
        assert!(observed.deliveries.is_empty());
        assert!(observed.lap_complete);
    };
    let (observed, ()) = Box::pin(compio::time::timeout(Duration::from_secs(10), async {
        futures::join!(peer, caller)
    }))
    .await
    .expect("interrupted client failed to recover");
    assert_eq!(observed, (2, 2));
}

#[compio::test]
async fn client_recovers_after_a_partial_response_times_out() {
    Box::pin(reuse_after_interruption(Interruption::Timeout)).await;
}

#[compio::test]
async fn client_recovers_after_the_caller_cancels_a_partial_response() {
    Box::pin(reuse_after_interruption(Interruption::CallerCancellation)).await;
}

/// The claim reply `client` would accept for one delivery of an advance job to
/// it, with `accepted` as the journal half, encoded.
fn claim_reply(client: &WorkerCoordinator, accepted: &serde_json::Value) -> Vec<u8> {
    use zeroship_core::{app_id::AppId, workflow_coordination::RunId};
    use zeroship_core::workflow_jobs::{DeploymentId, JobId};
    serde_json::to_vec(&serde_json::json!({
        "deliveries": [{
            "lease": {
                "delivery": {
                    "job": {
                        "id": JobId::mint().as_str(),
                        "appId": AppId::mint().as_str(),
                        "operation": {
                            "kind": "advance",
                            "deploymentId": DeploymentId::mint().as_str(),
                            "runId": RunId::mint().as_str(),
                            "generation": 0,
                            "revision": 1,
                        },
                        "availableAt": 1,
                    },
                    "workerId": client.worker_id().as_str(),
                    "attempt": 1,
                    "deadline": 1,
                },
                "remainingMs": 60_000,
                "attemptRemainingMs": 60_000,
            },
            "accepted": accepted,
        }],
        "after": null,
        "lapComplete": false,
    }))
    .unwrap()
}

/// Serve one claim with `body`, after reading the request in full. A client
/// that refuses the reply closes the connection while it is being written, so
/// the write's outcome is the caller's to judge.
async fn serve_claim(listener: &TcpListener, body: Vec<u8>) -> std::io::Result<()> {
    let (mut stream, _) = listener.accept().await.unwrap();
    let received = request(&mut stream).await;
    assert_eq!(received.path, endpoints::WORKFLOW_JOB_CLAIM.path_template());
    stream.write_all(response(&body)).await.0?;
    stream.flush().await
}

/// A claim reply of exactly the protocol bound - room for one maximal delivery
/// - is received by a client on its default options, the ones a worker builds
/// its manager client from. The control is the same reply one byte longer,
/// which is refused.
#[compio::test]
async fn a_reply_at_the_protocol_bound_is_received_and_one_byte_more_is_not() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = WorkerCoordinator::new(
        &format!("http://{}", listener.local_addr().unwrap()),
        worker_auth(),
        Options::default(),
    )
    .unwrap();
    let framing = claim_reply(&client, &serde_json::json!("")).len();
    for (extra, received) in [(0, true), (1, false)] {
        let padding = ClaimJobs::MAX_REPLY_BYTES - framing + extra;
        let body = claim_reply(&client, &serde_json::Value::String("x".repeat(padding)));
        assert_eq!(body.len(), ClaimJobs::MAX_REPLY_BYTES + extra);
        let ask = ClaimJobs {
            wait_ms: NonZeroU64::new(30_000).unwrap(),
            ..claim()
        };
        let (claimed, served) = Box::pin(compio::time::timeout(Duration::from_secs(30), async {
            futures::join!(
                client.claim_jobs::<JsonJournal>(&ask),
                serve_claim(&listener, body)
            )
        }))
        .await
        .expect("the claim exchange finished");
        if received {
            served.unwrap();
            assert_eq!(claimed.unwrap().deliveries.len(), 1);
        } else {
            assert_eq!(claimed.unwrap_err(), Error::ResponseTooLarge);
        }
    }
}

/// One claim against a service that accepts it and never answers: how long the
/// client waited before it gave up, and what it reported.
async fn silent_claim(
    listener: &TcpListener,
    client: &WorkerCoordinator,
    wait: Duration,
) -> (Result<(), Error>, Duration) {
    let ask = ClaimJobs {
        wait_ms: NonZeroU64::new(u64::try_from(wait.as_millis()).unwrap()).unwrap(),
        ..claim()
    };
    let (gave_up, seen) = oneshot::channel::<()>();
    let silent = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        request(&mut stream).await;
        // Hold the connection open and say nothing until the client gives up.
        let _ = seen.await;
        drop(stream);
    };
    let started = std::time::Instant::now();
    let caller = async {
        let outcome = client.claim_jobs::<JsonJournal>(&ask).await.map(|_| ());
        let elapsed = started.elapsed();
        gave_up.send(()).unwrap();
        (outcome, elapsed)
    };
    let (answer, ()) = futures::join!(caller, silent);
    answer
}

/// A claim's exchange lasts exactly the wait the claim states, whatever the
/// client's generic exchange bound. On the client's default options - the ones
/// a worker builds its manager client from - a claim stating a wait longer than
/// that bound is still waited on to its end, because the service works for it
/// until a deadline inside that wait; giving up earlier would strand what the
/// service committed. The control is a claim stating a wait shorter than the
/// generic bound, which is given up on at its own wait.
#[compio::test]
async fn a_claim_waits_exactly_the_wait_it_states() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = WorkerCoordinator::new(
        &format!("http://{}", listener.local_addr().unwrap()),
        worker_auth(),
        Options::default(),
    )
    .unwrap();
    let generic = Options::default().timeout;
    for stated in [generic + Duration::from_secs(1), Duration::from_millis(300)] {
        let (outcome, elapsed) = silent_claim(&listener, &client, stated).await;
        assert_eq!(outcome.unwrap_err(), Error::Timeout);
        assert!(
            elapsed >= stated,
            "a claim stating {stated:?} gave up after {elapsed:?}"
        );
        assert!(
            elapsed < stated.max(generic) + Duration::from_secs(1),
            "a claim stating {stated:?} waited {elapsed:?}"
        );
    }
}
