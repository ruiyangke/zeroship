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
use std::{num::NonZeroU32, sync::Arc, time::Duration};
use zeroship_core::{
    service_assertion::{
        ServiceIssuer, ServiceSigningKey, ServiceTrustBundle, TransportAssertionVerifier,
    },
    service_identity::endpoints,
    service_peers::{ServiceAuth, ServiceKeyring},
    workflow_coordination::{RegisterWorker, RegisteredWorker, WorkerId, WorkerState},
};
use zeroship_workflow::coordination::{Error, Options, WorkerCoordinator};

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

const fn registration() -> RegisterWorker {
    RegisterWorker {
        capacity: NonZeroU32::new(3).unwrap(),
        state: WorkerState::Ready,
    }
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

fn registration_assertion(request: Request) -> String {
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, endpoints::WORKFLOW_REGISTER.path_template());
    assert_eq!(
        serde_json::from_slice::<RegisterWorker>(&request.body).unwrap(),
        registration()
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
    // Match worker runtime bootstrap when test dependencies enable another provider.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
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
            client.register(&registration()).await.unwrap_err(),
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
    let registered = RegisteredWorker {
        worker_id: client.worker_id().clone(),
        capacity: registration().capacity,
        state: registration().state,
        expires_at: 1.try_into().unwrap(),
    };
    let complete_reply = response(&serde_json::to_vec(&registered).unwrap());
    let (partial_sent, partial_received) = oneshot::channel();
    let peer = async {
        let mut connections = 0;
        let mut requests = 0;
        let (mut first, _) = listener.accept().await.unwrap();
        connections += 1;
        let first_assertion = registration_assertion(request(&mut first).await);
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
            let next_assertion = registration_assertion(request(&mut second).await);
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
        let registration = registration();
        let mut pending = Box::pin(client.register(&registration));
        match futures::future::select(&mut pending, partial_received).await {
            Either::Left(_) => panic!("request completed before the partial response"),
            Either::Right((ready, _)) => ready.unwrap(),
        }
        match interruption {
            Interruption::Timeout => assert_eq!(pending.await.unwrap_err(), Error::Timeout),
            Interruption::CallerCancellation => drop(pending),
        }
        assert_eq!(client.register(&registration).await.unwrap(), registered);
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
