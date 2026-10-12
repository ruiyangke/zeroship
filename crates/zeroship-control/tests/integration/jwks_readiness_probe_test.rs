//! The JWKS readiness probe reads one HTTP response and stops.
//!
//! `PlatformJwks::await_key` is what a control fixture waits on before a
//! provider may fetch the mock's key. A probe that reads to EOF depends on the
//! server closing the connection, so a server that answers and keeps the socket
//! open parks the probe until its fixture guard expires and then fails with
//! `WouldBlock` - a failure that names the probe, not the server. These cases
//! pin the probe to the response it was answered with: one that carries a
//! `Content-Length`, after which the server holds the connection open.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use crate::support::{PlatformJwks, JWKS_READINESS_BOUND};

/// The key id the loopback server publishes and the probe looks for.
const KID: &str = "jwks-readiness-probe-kid";

/// A loopback server that answers ONE valid JWKS response with a
/// `Content-Length`, then holds the connection open without closing it.
///
/// The returned sender releases the held connection; the join handle is what
/// proves the server thread ended. Nothing closes the connection until the
/// caller sends, so a probe that reads to EOF cannot return.
fn answering_server_that_stays_open() -> (SocketAddr, mpsc::Sender<()>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback readiness server");
    let address = listener.local_addr().expect("the loopback server's address");
    let (release_tx, release_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept the readiness probe");
        let mut request = Vec::new();
        let mut scratch = [0_u8; 1024];
        loop {
            let read = socket.read(&mut scratch).expect("read the probe request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&scratch[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let body = format!("{{\"keys\":[{{\"kid\":\"{KID}\"}}]}}");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        socket
            .write_all(response.as_bytes())
            .expect("write the readiness response");
        socket.flush().expect("flush the readiness response");
        // The connection stays open: the probe must not need EOF to return.
        let _ = release_rx.recv();
        drop(socket);
    });
    (address, release_tx, server)
}

/// The probe returns against a server that answered and never closed, and it
/// returns within its guard rather than waiting for an EOF that never comes.
#[test]
fn the_readiness_probe_reads_one_response_from_a_server_that_stays_open() {
    let (address, release, server) = answering_server_that_stays_open();
    let started = Instant::now();
    PlatformJwks::await_key(address, "/.well-known/jwks.json", KID);
    let elapsed = started.elapsed();
    assert!(
        elapsed < JWKS_READINESS_BOUND,
        "the readiness probe took {elapsed:?} against a server that had already answered; \
         it waited for the connection to close instead of reading one response"
    );
    let _ = release.send(());
    server.join().expect("the loopback readiness server joins");
}
