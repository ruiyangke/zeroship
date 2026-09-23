//! What one authenticated coordination round trip costs on the machine that
//! runs this file.
//!
//! WHY THIS EXISTS. A design that moves the workflow journal into the workflow
//! service turns calls that are in-process today into calls over this client.
//! The question that gates it is not "how fast is a socket" but "what does the
//! shipped exchange cost, framing and credential included, next to the work a
//! dispatch already does". This measures the shipped exchange: the real
//! [`Transport`], a real service assertion minted per call, a peer that really
//! verifies it against a trust bundle and a replay store, real HTTP framing and
//! real JSON on both sides.
//!
//! WHAT IT MEASURES
//! - `exchange`: client-observed latency of one `Transport::post`, end to end.
//!   Reported for a connection-per-exchange peer and for a keep-alive peer, and
//!   for a claim-shaped body and for the completion batch a whole dispatch
//!   folds at once, because the two protocol points that would cross carry
//!   very different payloads.
//! - `mint` and `verify`: the credential halves alone, so a reader can tell how
//!   much of an exchange is cryptography the worker pays per call and how much
//!   is transport.
//!
//! WHAT IT OMITS, and a reader must not forget
//! - TLS. The peer is loopback HTTP. `Transport` admits plaintext only for a
//!   literal loopback address; a remote peer is HTTPS, and a handshake plus
//!   record layer is not in these numbers. Connection reuse decides how much
//!   that matters, which is why the keep-alive column exists.
//! - Network. Loopback has no propagation delay. A zone-local peer adds its
//!   own; a cross-zone peer adds much more.
//! - The peer's own work. This peer verifies and replies from memory. A real
//!   service also does its database work - but that work exists today too, on
//!   whichever side of the boundary the journal sits, so the difference this
//!   measures is the transport and credential the relocation would add.
//!
//! HOW TO RUN IT
//!   cargo test -p zeroship-workflow-client --test `round_trip_cost` -- --ignored --nocapture
//! The report goes to stdout. It prints the machine's load average beside the
//! samples: a reading taken while the machine is busy is not a reading taken
//! while it is quiet, and the two must not be quoted as one number.
//!
//! The non-ignored test in this file exercises the harness itself and asserts
//! it really completed the exchanges it was asked for. It makes no claim about
//! time, because a timing assertion on a shared machine is a coin flip.

#![allow(
    clippy::future_not_send,
    reason = "loopback HTTP fixtures stay on their compio runtime"
)]

use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use serde_json::{json, Value};
use std::{
    cell::Cell,
    sync::Arc,
    time::{Duration, Instant},
};
use zeroship_core::{
    service_assertion::{
        InMemoryReplayStore, ServiceAssertionVerifier, ServiceIssuer, ServiceSigningKey,
        ServiceTrustBundle, TransportAssertionVerifier,
    },
    service_identity::{endpoints, verify_service_call, ServiceEndpoint},
    service_peers::{ServiceAuth, ServiceKeyring},
    workflow_coordination::{WorkerId, AUDIENCE},
};
use zeroship_workflow_client::{Options, Transport};

/// The endpoint the samples are taken on. A worker holds a grant on it, so the
/// peer's authorization check takes the same path a real claim does.
const ENDPOINT: ServiceEndpoint = endpoints::WORKFLOW_JOB_CLAIM;

/// Whether the peer keeps the connection open between exchanges.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Peer {
    /// One TCP connection per exchange: the shape a peer that closes forces.
    PerExchange,
    /// The connection stays open: the shape a pooling client gets from a peer
    /// that allows it.
    KeepAlive,
}
impl Peer {
    const fn label(self) -> &'static str {
        match self {
            Self::PerExchange => "connection per exchange",
            Self::KeepAlive => "keep-alive",
        }
    }
}

fn worker_auth() -> Arc<ServiceAuth> {
    let issuer = ServiceIssuer::parse(&format!(
        "spiffe://zeroship.ai/svc/worker/{}",
        WorkerId::mint().as_str()
    ))
    .expect("worker issuer");
    Arc::new(ServiceAuth::new(
        ServiceKeyring::from_parts(
            issuer,
            ServiceSigningKey::generate(),
            ServiceTrustBundle::new(),
        )
        .expect("worker keyring"),
        Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
    ))
}

/// A verifier that trusts `auth`'s signing key, with the replay store a real
/// peer runs. Every exchange therefore pays a replay claim as well as a
/// signature check.
fn peer_verifier(auth: &ServiceAuth) -> ServiceAssertionVerifier {
    let (issuer, key) = auth.signing_identity().expect("configured signer");
    let mut trust = ServiceTrustBundle::new();
    trust
        .trust_signing_key(issuer, key.key_id(), key)
        .expect("trust the worker key");
    ServiceAssertionVerifier::new(trust, Arc::new(InMemoryReplayStore::new()))
}

/// One HTTP/1.1 request read off a connection that may carry more after it.
struct Inbound {
    authorization: String,
}

/// Reads requests off one connection, keeping whatever followed the body so a
/// keep-alive peer can serve the next request from the same buffer.
struct Connection {
    stream: compio::net::TcpStream,
    buffered: Vec<u8>,
}
impl Connection {
    const fn new(stream: compio::net::TcpStream) -> Self {
        Self {
            stream,
            buffered: Vec::new(),
        }
    }

    /// `None` once the peer closed without starting another request.
    async fn read_request(&mut self) -> Option<Inbound> {
        loop {
            let parsed = parse(&self.buffered);
            match parsed {
                Parsed::Complete {
                    authorization,
                    consumed,
                } => {
                    self.buffered.drain(..consumed);
                    return Some(Inbound { authorization });
                }
                Parsed::Incomplete => {}
            }
            let compio::BufResult(read, buffer) = self.stream.read(vec![0; 16 * 1024]).await;
            let read = read.expect("read from the calling peer");
            if read == 0 {
                assert!(
                    self.buffered.is_empty(),
                    "peer closed mid-request with {} bytes buffered",
                    self.buffered.len()
                );
                return None;
            }
            self.buffered.extend_from_slice(&buffer[..read]);
        }
    }

    async fn reply(&mut self, body: &[u8], close: bool) {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}\r\n",
            body.len(),
            if close { "Connection: close\r\n" } else { "" }
        )
        .into_bytes();
        response.extend_from_slice(body);
        self.stream
            .write_all(response)
            .await
            .0
            .expect("write the reply");
        self.stream.flush().await.expect("flush the reply");
    }
}

enum Parsed {
    Incomplete,
    Complete {
        authorization: String,
        consumed: usize,
    },
}

/// Parse one request out of `bytes`, reporting how much of it the request used.
fn parse(bytes: &[u8]) -> Parsed {
    let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
        return Parsed::Incomplete;
    };
    let header = std::str::from_utf8(&bytes[..end]).expect("ASCII request head");
    let mut lines = header.lines();
    let mut operation = lines.next().expect("request line").split_ascii_whitespace();
    assert_eq!(operation.next(), Some("POST"));
    assert_eq!(operation.next(), Some(ENDPOINT.path_template()));
    let mut length = None;
    let mut authorization = None;
    for line in lines {
        let (name, value) = line.split_once(':').expect("header line");
        if name.eq_ignore_ascii_case("content-length") {
            length = Some(value.trim().parse::<usize>().expect("content length"));
        } else if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value.trim().to_owned());
        }
    }
    let length = length.expect("the client always sends a length");
    let consumed = end + 4 + length;
    if bytes.len() < consumed {
        return Parsed::Incomplete;
    }
    Parsed::Complete {
        authorization: authorization.expect("the client always sends a credential"),
        consumed,
    }
}

struct Samples {
    exchanges: Vec<Duration>,
    connections: usize,
}

/// Drive `iterations` real exchanges and return one sample per exchange.
///
/// The peer verifies every credential before replying, so a sample includes the
/// signature the caller minted and the check the callee ran.
async fn exchange(peer: Peer, iterations: usize, request: &Value, response: &Value) -> Samples {
    assert!(
        iterations > 0,
        "a measurement over zero exchanges is not one"
    );
    let auth = worker_auth();
    let verifier = peer_verifier(&auth);
    let listener = compio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback peer");
    let origin = format!("http://{}", listener.local_addr().expect("peer address"));
    let transport = Transport::new(
        &origin,
        auth.clone(),
        ServiceIssuer::parse(AUDIENCE).expect("audience"),
        Options::default(),
    )
    .expect("build the shipped client transport");

    let body = serde_json::to_vec(response).expect("encode the reply");
    let served = Cell::new(0usize);
    let connections = Cell::new(0usize);
    let serve = async {
        while served.get() < iterations {
            let (stream, _) = listener.accept().await.expect("accept");
            connections.set(connections.get() + 1);
            let mut connection = Connection::new(stream);
            while served.get() < iterations {
                let Some(inbound) = connection.read_request().await else {
                    break;
                };
                verify_service_call(&verifier, Some(&inbound.authorization), AUDIENCE, ENDPOINT)
                    .await
                    .expect("the peer accepts the caller's credential");
                served.set(served.get() + 1);
                connection
                    .reply(
                        &body,
                        peer == Peer::PerExchange || served.get() == iterations,
                    )
                    .await;
                if peer == Peer::PerExchange {
                    break;
                }
            }
        }
    };

    let call = async {
        let mut exchanges = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let started = Instant::now();
            let received: Value = transport
                .post(ENDPOINT, request)
                .await
                .expect("the exchange completes");
            exchanges.push(started.elapsed());
            assert_eq!(&received, response, "the peer's reply was substituted");
        }
        exchanges
    };

    let (exchanges, ()) = futures::future::join(call, serve).await;
    assert_eq!(
        exchanges.len(),
        iterations,
        "fewer samples than exchanges requested"
    );
    assert_eq!(served.get(), iterations, "the peer served fewer exchanges");
    Samples {
        exchanges,
        connections: connections.get(),
    }
}

/// A frontier batch of `outcomes` completed steps, the shape a `complete` call
/// would carry. Built here rather than imported so this file measures a wire
/// body without depending on the engine crate.
fn frontier(outcomes: usize, output_bytes: usize) -> Value {
    let output: String = "x".repeat(output_bytes);
    Value::Array(
        (0..outcomes)
            .map(|ordinal| {
                json!({
                    "kind": "StepCompleted",
                    "ordinal": ordinal,
                    "name": format!("step-{ordinal}"),
                    "nameOccurrence": 0,
                    "stepKind": "run",
                    "output": { "value": output },
                    "compensationMaxAttempts": 1,
                })
            })
            .collect::<Vec<_>>(),
    )
}

fn percentile(sorted: &[Duration], hundredths: usize) -> Duration {
    assert!(!sorted.is_empty(), "no samples to summarize");
    let index = (sorted.len() - 1) * hundredths / 100;
    sorted[index]
}

fn report(label: &str, peer: Peer, samples: &Samples, request_bytes: usize, reply_bytes: usize) {
    let mut sorted = samples.exchanges.clone();
    sorted.sort_unstable();
    println!(
        "{label} [{}] n={} connections={} request={request_bytes}B reply={reply_bytes}B\n  \
         min={:?} p50={:?} p90={:?} p99={:?} max={:?}",
        peer.label(),
        sorted.len(),
        samples.connections,
        sorted[0],
        percentile(&sorted, 50),
        percentile(&sorted, 90),
        percentile(&sorted, 99),
        sorted[sorted.len() - 1],
    );
}

fn load_average() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|line| {
            let mut parts = line.split_ascii_whitespace();
            Some(format!(
                "1m={} 5m={} 15m={}",
                parts.next()?,
                parts.next()?,
                parts.next()?
            ))
        })
        .unwrap_or_else(|| "unavailable".into())
}

/// Exercises the harness on every peer shape and body it reports on, and
/// asserts it really ran them. No timing is asserted: this test states that the
/// instrument works, not what it read.
#[compio::test]
async fn the_harness_completes_every_exchange_it_is_asked_for() {
    for peer in [Peer::PerExchange, Peer::KeepAlive] {
        let request = json!({"appId": "app_probe", "assignmentRevision": 1});
        let samples = Box::pin(exchange(peer, 4, &request, &frontier(2, 16))).await;
        assert_eq!(samples.exchanges.len(), 4);
        assert!(samples.connections >= 1, "no connection was opened");
        assert!(
            samples.exchanges.iter().all(|sample| !sample.is_zero()),
            "an exchange reported no elapsed time at all"
        );
    }
}

/// The report. Run with `--ignored --nocapture`.
#[compio::test]
#[ignore = "a latency measurement, not a contract; run it deliberately"]
async fn one_round_trip_costs() {
    const ITERATIONS: usize = 400;
    println!("load before: {}", load_average());
    // A claim-shaped request with a small reply: the cheapest crossing.
    let small_request = json!({"appId": "app_probe", "assignmentRevision": 1});
    // A completion-shaped request: the batch a whole dispatch folds at once.
    let batch_request = frontier(64, 256);
    // A reply the size of a replay journal handed back with an assignment.
    let large_reply = frontier(64, 4096);
    let small_reply = json!({"accepted": true});

    for peer in [Peer::PerExchange, Peer::KeepAlive] {
        let samples = Box::pin(exchange(peer, ITERATIONS, &small_request, &small_reply)).await;
        report(
            "small request, small reply",
            peer,
            &samples,
            serde_json::to_vec(&small_request).expect("encode").len(),
            serde_json::to_vec(&small_reply).expect("encode").len(),
        );
        let samples = Box::pin(exchange(peer, ITERATIONS, &batch_request, &small_reply)).await;
        report(
            "frontier batch request, small reply",
            peer,
            &samples,
            serde_json::to_vec(&batch_request).expect("encode").len(),
            serde_json::to_vec(&small_reply).expect("encode").len(),
        );
        let samples = Box::pin(exchange(peer, ITERATIONS, &small_request, &large_reply)).await;
        report(
            "small request, replay-journal reply",
            peer,
            &samples,
            serde_json::to_vec(&small_request).expect("encode").len(),
            serde_json::to_vec(&large_reply).expect("encode").len(),
        );
    }

    credential_halves(ITERATIONS).await;
    println!("load after: {}", load_average());
}

/// What the caller pays to mint a credential and the callee pays to check one,
/// with no socket between them. Their sum is the floor under every exchange
/// above, and it is the part a connection pool cannot remove.
async fn credential_halves(iterations: usize) {
    let auth = worker_auth();
    let audience = ServiceIssuer::parse(AUDIENCE).expect("audience");
    let verifier = peer_verifier(&auth);

    let mut minted = Vec::with_capacity(iterations);
    let mut mints = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        let header = auth
            .authorization_for(&audience)
            .expect("the worker holds a key");
        mints.push(started.elapsed());
        minted.push(header);
    }
    assert_eq!(minted.len(), iterations, "mint produced fewer credentials");

    let mut checked = Vec::with_capacity(iterations);
    for header in &minted {
        let started = Instant::now();
        verify_service_call(&verifier, Some(header), AUDIENCE, ENDPOINT)
            .await
            .expect("a freshly minted credential verifies");
        checked.push(started.elapsed());
    }
    assert_eq!(checked.len(), iterations, "verify ran fewer checks");

    for (label, mut samples) in [("mint", mints), ("verify", checked)] {
        samples.sort_unstable();
        println!(
            "{label} n={} min={:?} p50={:?} p90={:?} max={:?}",
            samples.len(),
            samples[0],
            percentile(&samples, 50),
            percentile(&samples, 90),
            samples[samples.len() - 1],
        );
    }
}
