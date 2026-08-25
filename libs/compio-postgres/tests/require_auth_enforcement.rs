//! `require_auth` enforcement, for the policy shapes the in-src tests do not
//! cover.
//!
//! THIS FILE'S ORIGINAL HEADER CLAIMED `require_auth` HAD ZERO TESTS. That was
//! wrong, and the mistake is worth recording because it is cheap to repeat: a
//! grep of `tests/` found nothing, and I concluded untested without looking in
//! `src/`. `src/connect_raw.rs` already carries
//! `require_scram_refuses_cleartext_without_sending_the_password` and
//! `require_scram_refuses_authentication_ok_without_an_exchange` - and the
//! second one covers the downgrade-to-trust case this file was written for,
//! under `require_auth=scram-sha-256`.
//!
//! What is genuinely added here is the OTHER policy shapes. The in-src pair
//! exercises `Require(scram-sha-256)`; these exercise `Require(password)` and
//! `Reject(password)`, so the "policy names a method the server did not offer"
//! and "policy rejects the method the server DID offer" arms both run. Each is
//! paired with a control differing only in the policy, which the in-src tests do
//! not have.
//!
//! `tests/libpq_parameter_parity.rs` proves the DSN key is accepted, which is a
//! different claim again from the policy being applied. For a security setting
//! "accepted but not enforced" is the dangerous state: the connection string
//! looks hardened, the driver reports success, and nothing was checked.
//! `check_require_auth` has seven call sites in `src/connect_raw.rs`.
//!
//! THE CASE THAT MATTERS is `AuthMethod::None`, whose failure text is "server
//! did not complete authentication". A hostile or man-in-the-middle server can
//! answer the startup packet with `AuthenticationOk` and never issue a
//! challenge. The client then holds a session it believes is authenticated
//! while having proved nothing to anybody. libpq grew `require_auth` in 16 for
//! precisely this, and a client that accepts the connection anyway has silently
//! downgraded to trust.
//!
//! Each test pairs a hostile server with a ONE-VARIABLE CONTROL: the same
//! scripted bytes, the same client, differing only in the policy. Without the
//! control a passing test cannot distinguish "the policy rejected this" from
//! "the connection failed for some unrelated reason", which is the failure mode
//! that makes security tests worthless.
//!
//! NOT covered here: SCRAM channel binding, TLS, GSSAPI or SSPI (the driver
//! cannot perform the latter two and accepts them only in negative policies).

use compio_postgres::Config;
use compio_postgres::config::{AuthMethod, AuthMethods, RequireAuth, SslMode};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

#[allow(dead_code)]
mod common;

const ASYNC_WATCHDOG: Duration = Duration::from_secs(5);
const SOCKET_WATCHDOG: Duration = Duration::from_secs(2);
const THREAD_WATCHDOG: Duration = Duration::from_secs(3);
const CONNECT_WATCHDOG: Duration = Duration::from_secs(2);

struct StubServer {
    addr: SocketAddr,
    done: std::sync::mpsc::Receiver<()>,
    thread: thread::JoinHandle<()>,
}

impl StubServer {
    fn spawn(script: impl FnOnce(TcpListener) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind scripted PostgreSQL peer");
        listener
            .set_nonblocking(true)
            .expect("make scripted listener bounded");
        let addr = listener.local_addr().expect("scripted listener address");
        let (done_tx, done) = std::sync::mpsc::channel();
        let thread = thread::spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                script(listener);
            }));
            let _ = done_tx.send(());
            if let Err(panic) = outcome {
                std::panic::resume_unwind(panic);
            }
        });
        Self { addr, done, thread }
    }

    fn finish(self) {
        self.done
            .recv_timeout(THREAD_WATCHDOG)
            .expect("scripted PostgreSQL peer exceeded its thread watchdog");
        self.thread
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    }
}

fn accept_bounded(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + SOCKET_WATCHDOG;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_read_timeout(Some(SOCKET_WATCHDOG))
                    .expect("set scripted peer read watchdog");
                stream
                    .set_write_timeout(Some(SOCKET_WATCHDOG))
                    .expect("set scripted peer write watchdog");
                return stream;
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "client did not connect before the scripted accept watchdog"
                );
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("scripted accept failed: {error}"),
        }
    }
}

fn backend_frame(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + body.len());
    frame.push(tag);
    frame.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
    frame.extend_from_slice(body);
    frame
}

fn read_startup(stream: &mut TcpStream) {
    let mut length = [0u8; 4];
    stream
        .read_exact(&mut length)
        .expect("read startup packet length");
    let length = u32::from_be_bytes(length) as usize;
    assert!(length >= 8, "startup packet is shorter than its header");
    let mut body = vec![0u8; length - 4];
    stream
        .read_exact(&mut body)
        .expect("read startup packet body");
}

/// The tail every successful handshake ends with, after whatever authentication
/// exchange preceded it.
fn session_established(process_id: i32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut key_data = Vec::with_capacity(8);
    key_data.extend_from_slice(&process_id.to_be_bytes());
    key_data.extend_from_slice(&1234i32.to_be_bytes());
    out.extend_from_slice(&backend_frame(b'K', &key_data));
    out.extend_from_slice(&backend_frame(b'Z', b"I"));
    out
}

/// A server that declares the client authenticated without ever challenging it.
/// `AuthenticationOk` is message type `R` with a zero body.
fn trust_server(process_id: i32) -> impl FnOnce(TcpListener) + Send + 'static {
    move |listener| {
        let mut stream = accept_bounded(&listener);
        read_startup(&mut stream);
        let mut response = backend_frame(b'R', &0u32.to_be_bytes());
        response.extend_from_slice(&session_established(process_id));
        let _ = stream.write_all(&response);
        let _ = stream.flush();
        thread::sleep(Duration::from_millis(200));
    }
}

/// A server that demands a cleartext password. `AuthenticationCleartextPassword`
/// is `R` with a body of 3.
///
/// REPORTS WHAT THE CLIENT SENT, which is the security half of a `Reject`
/// policy and was unmeasurable here until 2026-08-23. The peer read the
/// client's reply into a local and dropped it, so a driver that wrote the
/// password to the socket and only THEN consulted the policy produced exactly
/// the same refusal and passed. `src/connect_raw.rs` already holds the right
/// shape for this in
/// `require_scram_refuses_cleartext_without_sending_the_password`, which
/// asserts the wire was empty; this is that assertion for the integration-level
/// twin.
fn cleartext_password_server(
    process_id: i32,
) -> (
    impl FnOnce(TcpListener) + Send + 'static,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    let (sent_tx, sent_rx) = std::sync::mpsc::channel();
    let script = move |listener: TcpListener| {
        let mut stream = accept_bounded(&listener);
        read_startup(&mut stream);
        let _ = stream.write_all(&backend_frame(b'R', &3u32.to_be_bytes()));
        let _ = stream.flush();
        // Read whatever the client sends back, then accept it. A real server
        // would verify; this one does not, because the claim under test is what
        // the CLIENT enforces before it gets here.
        let mut sent = Vec::new();
        let mut tag = [0u8; 1];
        if stream.read_exact(&mut tag).is_ok() {
            sent.extend_from_slice(&tag);
            let mut length = [0u8; 4];
            if stream.read_exact(&mut length).is_ok() {
                sent.extend_from_slice(&length);
                let length = u32::from_be_bytes(length) as usize;
                let mut body = vec![0u8; length.saturating_sub(4)];
                if stream.read_exact(&mut body).is_ok() {
                    sent.extend_from_slice(&body);
                }
            }
        }
        let _ = sent_tx.send(sent);
        let mut response = backend_frame(b'R', &0u32.to_be_bytes());
        response.extend_from_slice(&session_established(process_id));
        let _ = stream.write_all(&response);
        let _ = stream.flush();
        thread::sleep(Duration::from_millis(200));
    };
    (script, sent_rx)
}

fn base_config(addr: SocketAddr) -> Config {
    let mut config = Config::new();
    config
        .user("scripted-user")
        .password("scripted-password")
        .hostaddr(addr.ip())
        .port(addr.port())
        .ssl_mode(SslMode::Disable)
        .connect_timeout(Duration::from_secs(1));
    config
}

fn require_password() -> RequireAuth {
    RequireAuth::Require(AuthMethods::new(AuthMethod::Password))
}

/// A server that skips authentication entirely must be refused when the policy
/// demands a password. This is the downgrade-to-trust case.
#[compio::test]
async fn a_server_that_never_authenticates_is_refused_under_require_password() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = StubServer::spawn(trust_server(401));
        let mut config = base_config(server.addr);
        config.require_auth(require_password());

        // `let ... else` rather than `expect_err`: the Ok half carries a
        // `Connection`, which is not `Debug`, so `expect_err` does not apply.
        let Err(error) =
            compio::time::timeout(CONNECT_WATCHDOG, config.connect(common::suite_tls()))
                .await
                .expect("connect hung instead of refusing an unauthenticated server")
        else {
            panic!("the driver accepted a server that never authenticated it")
        };

        let chain = common::error_chain(&error);
        assert!(
            chain.contains("did not complete authentication"),
            "refusal did not name the missing authentication: {chain}"
        );
        server.finish();
    })
    .await
    .expect("require_auth trust-downgrade test exceeded its outer watchdog");
}

/// THE CONTROL for the test above: the same scripted bytes and the same client,
/// differing only in the policy. It must CONNECT. Without this, the refusal
/// above could be any handshake failure rather than the policy firing.
#[compio::test]
async fn the_same_server_is_accepted_when_no_policy_is_set() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = StubServer::spawn(trust_server(402));
        let config = base_config(server.addr);

        let (client, connection) =
            compio::time::timeout(CONNECT_WATCHDOG, config.connect(common::suite_tls()))
                .await
                .expect("connect hung against a trust server")
                .expect("the default policy rejected a server it should accept");
        let driver = compio::runtime::spawn(async move { connection.run().await });
        drop(client);
        let _ = compio::time::timeout(CONNECT_WATCHDOG, driver).await;
        server.finish();
    })
    .await
    .expect("require_auth control test exceeded its outer watchdog");
}

/// A policy naming one method must reject a DIFFERENT method, not merely the
/// absence of one. `Reject(password)` against a server demanding exactly that
/// separates "the policy is consulted" from "the policy only catches None".
#[compio::test]
async fn a_rejected_method_is_refused_even_though_the_server_offers_it() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let (script, sent) = cleartext_password_server(403);
        let server = StubServer::spawn(script);
        let mut config = base_config(server.addr);
        config.require_auth(RequireAuth::Reject(AuthMethods::new(AuthMethod::Password)));

        let Err(error) =
            compio::time::timeout(CONNECT_WATCHDOG, config.connect(common::suite_tls()))
                .await
                .expect("connect hung instead of refusing a rejected method")
        else {
            panic!("the driver used an authentication method its policy rejects")
        };

        let chain = common::error_chain(&error);
        assert!(
            chain.contains("cleartext password"),
            "refusal did not name the rejected method: {chain}"
        );

        // THE SECURITY CLAIM, not just the error. A `Reject(password)` policy
        // exists so the secret never reaches the wire; a driver that sent it
        // and then refused would produce the identical error above. The peer
        // reports what it received, and it must have received nothing before
        // the client hung up.
        server.finish();
        let sent = sent
            .try_recv()
            .expect("the scripted peer did not report what the client sent");
        assert!(
            sent.is_empty(),
            "the driver put {} bytes on the wire for a method its policy rejects, and \
             {:?} is the password among them",
            sent.len(),
            String::from_utf8_lossy(&sent)
        );
    })
    .await
    .expect("require_auth rejected-method test exceeded its outer watchdog");
}

/// The control for the rejected-method test: identical server, policy that
/// permits the method, so the handshake completes.
#[compio::test]
async fn the_same_method_is_accepted_when_the_policy_requires_it() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let (script, sent) = cleartext_password_server(404);
        let server = StubServer::spawn(script);
        let mut config = base_config(server.addr);
        config.require_auth(require_password());

        let (client, connection) =
            compio::time::timeout(CONNECT_WATCHDOG, config.connect(common::suite_tls()))
                .await
                .expect("connect hung against a password server")
                .expect("require_auth=password rejected a password handshake");
        let driver = compio::runtime::spawn(async move { connection.run().await });
        drop(client);
        let _ = compio::time::timeout(CONNECT_WATCHDOG, driver).await;
        server.finish();

        // THE OTHER HALF OF THE WIRE ASSERTION. Its twin requires the peer to
        // have received NOTHING when the policy rejects the method; on its own
        // that is satisfied by a peer that never reports anything, or by a
        // client that cannot reach this server at all. Here the same peer, the
        // same client and the same password must produce a PasswordMessage
        // carrying the secret - so "nothing was sent" over there is a fact
        // about the policy rather than about the fixture.
        let sent = sent
            .try_recv()
            .expect("the scripted peer did not report what the client sent");
        assert_eq!(
            sent.first(),
            Some(&b'p'),
            "a permitted cleartext method must send a PasswordMessage, got {sent:02x?}"
        );
        assert!(
            sent.windows(17).any(|w| w == b"scripted-password"),
            "the permitted handshake did not carry the configured password: {:?}",
            String::from_utf8_lossy(&sent)
        );
    })
    .await
    .expect("require_auth accepted-method test exceeded its outer watchdog");
}
