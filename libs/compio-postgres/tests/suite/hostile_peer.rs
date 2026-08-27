//! What the driver does when the peer violates the protocol.
//!
//! `src/` carries 43 `Error::unexpected_message()` sites across nine files -
//! grepping the bare name reports 44, but one of those is a COMMENT at
//! connect_raw.rs:146, and three are `ok_or_else(Error::unexpected_message)`,
//! which ARE sites despite not looking like calls -
//! every one a place the driver says "a PostgreSQL server cannot have sent that
//! here". Before this file nothing drove any of them: a grep of `tests/` for a
//! bad length, an unknown tag, or `unexpected_message` returned nothing. The
//! oversize case is the exception and is already covered elsewhere against
//! `MAX_MESSAGE_SIZE`, so nothing here re-tests it.
//!
//! The peer is on a network. A mangling proxy, a transaction pooler that
//! rewrites framing, or an outright hostile server can put bytes on the wire
//! that PostgreSQL never would. Each test below asserts THREE things:
//!
//!   1. the operation returns an `Err` naming THE VIOLATION rather than a
//!      wrong-but-plausible `Ok`,
//!   2. the poisoned session does not go on to serve the next caller,
//!   3. it happened inside a watchdog, because several of these shapes are
//!      built to hang an implementation that waits for bytes that never come.
//!
//! ASSERTION 1 SAID ONLY `!chain.is_empty()` UNTIL 2026-08-23 - "an error
//! occurred". Every wrong answer in this file satisfies that, including an
//! error the HARNESS caused, which is how the COPY family below passed with
//! their violations removed. Each test now names the diagnosis it expects, and
//! [`HostileOutcome::names`] looks in both places one can land.
//!
//! ASSERTION 2 IS WEAKER THAN "THE SESSION IS RETIRED", and the difference is
//! measured, not hedged. This header used to claim retirement outright and call
//! it the point. It is not what happens: instrumenting the helper with
//! `client.is_closed()` on 2026-08-23 reported FALSE half a second after the
//! error for the unknown-tag case and both DataRow cases - the driver treats
//! those as belonging to one response, not to the session. What makes the
//! follow-up query fail there is the scripted peer hanging up 300ms later.
//! Raise that sleep past `OPERATION_WATCHDOG` and all three fail on the
//! "reusing the poisoned session hung" arm. Only
//! `a_length_below_its_own_header_is_refused` retires by the driver's own doing
//! (`is_closed` true, `Connection::run` returning the length error), and only
//! `a_truncated_frame_followed_by_silence_does_not_hang` asserts retirement
//! directly - it can, because `read_timeout` retires on expiry by contract.
//! Whether the other three SHOULD retire a stream the driver has lost sync with
//! is a driver question, not a test one; it is open.
//!
//! WHAT THESE ACTUALLY REACH, measured rather than assumed. The unknown-tag
//! case reports `error parsing response from server: unknown message tag
//! \`127\``, so it is rejected by the CODEC while decoding the frame - it never
//! reaches an `unexpected_message` site at all. The lying-length cases are the
//! same story one layer down. But the prepare and COPY families below DO reach
//! the state-machine cluster: all seven report `unexpected message from server`
//! (measured 2026-08-23), as does
//! `a_data_row_without_a_row_description_is_refused`. Eight tests on those 43
//! sites, five on the framing and decode defences.
//!
//! These tests discriminate, and that was checked rather than hoped: feeding a
//! WELL-FORMED `CommandComplete` + `ReadyForQuery` through the same helper
//! fails with "the driver accepted a malformed frame as a valid response:
//! [CommandComplete(1)]". A version of this file that passed no matter what the
//! peer sent would be worse than nothing, because it would read as coverage.
//!
//! THE COPY TESTS DID EXACTLY THAT UNTIL 2026-08-23, which is why
//! [`well_formed_copy_out`] exists as a named control rather than a habit. All
//! three sent a `ParseComplete` the COPY batch never asked for -- it is `B E S`,
//! measured by logging the frontend tags -- and then fell silent instead of
//! finishing the conversation. Either one is a violation by itself, so the
//! driver errored whatever else the response said and all three passed with
//! their own violation REMOVED. One of them was additionally misnamed: it
//! claimed to test a missing `ParseComplete` in a batch that contains no Parse,
//! and what it actually drives is a missing `CopyOutResponse`.
//!
//! So: substitute the well-formed response and require the test to FAIL. That
//! check is cheap, it is the only thing that catches this, and it belongs on
//! every test in this file.
//!
//! ALL FOUR HELPERS HAVE NOW BEEN THROUGH IT, so this need not be redone:
//! `hostile_response_retires_session` (the `CommandComplete` + `ReadyForQuery`
//! result quoted above), `hostile_copy_out_retires_session` and
//! `hostile_copy_in_retires_session` (both fixed to earn it, 2026-08-23), and
//! `hostile_prepare_retires_session`, whose three tests were checked the same
//! day against a well-formed `ParseComplete` + `ParameterDescription` +
//! `NoData` + `ReadyForQuery` and all three duly failed. The prepare helper
//! needed no change: a correct prepare reply ends in `ReadyForQuery`, so it
//! never had the truncation problem the COPY helper did.
//!
//! AND THE COPY HALF OF IT IS NOW A TEST, not a procedure. Re-running the
//! substitution on 2026-08-23 found `well_formed_copy_out` was DEAD CODE - it
//! had never had a caller, so the check it documents had to be performed by
//! hand by someone who had read the paragraph, which is how the note below it
//! came to say the control "could not be built" and was "INCONCLUSIVE" while
//! the function's own doc said substituting it must fail each test. Both were
//! written on the same day and they cannot both be right; the measurement says
//! the function's doc is. [`a_well_formed_copy_out_is_accepted`] now runs it on
//! every `cargo test`, and the four COPY tests were re-checked by substitution:
//! all four fail, each at its own `expect_err`.
//!
//! The `close_notify` regressions below are the TLS cases. A peer that sends
//! the RIGHT bytes one at a time is covered too, since "wrong bytes, promptly"
//! and "right bytes, slowly" break different things: the first tests framing
//! validation, the second tests reassembly across short reads.
//!
//! NOT covered here: TLS policy, authentication, or replication framing.

use compio_postgres::Config;
use compio_postgres::config::SslMode;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

#[allow(unused_imports)]
use crate::common;

const ASYNC_WATCHDOG: Duration = Duration::from_secs(5);
const SOCKET_WATCHDOG: Duration = Duration::from_secs(2);
const THREAD_WATCHDOG: Duration = Duration::from_secs(3);
/// Bounds the shapes whose failure mode is "wait forever for bytes the peer
/// will never send". Without it those tests would hang instead of failing.
const OPERATION_WATCHDOG: Duration = Duration::from_secs(2);

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

/// A frame whose declared length is a lie. `declared` replaces the real one, so
/// a value below the payload truncates it and a value above it makes the driver
/// wait for bytes that are never coming.
fn lying_frame(tag: u8, body: &[u8], declared: u32) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + body.len());
    frame.push(tag);
    frame.extend_from_slice(&declared.to_be_bytes());
    frame.extend_from_slice(body);
    frame
}

fn complete_startup(stream: &mut (impl Read + Write), process_id: i32) {
    let mut length = [0u8; 4];
    stream
        .read_exact(&mut length)
        .expect("read startup packet length");
    let length = u32::from_be_bytes(length) as usize;
    assert!(length >= 8, "startup packet is shorter than its header");
    assert!(length <= 1024 * 1024, "startup packet is implausibly large");
    let mut body = vec![0u8; length - 4];
    stream
        .read_exact(&mut body)
        .expect("read startup packet body");
    assert_eq!(
        &body[..4],
        &[0, 3, 0, 2],
        "client did not request protocol 3.2"
    );

    let mut response = backend_frame(b'R', &0u32.to_be_bytes());
    let mut key_data = Vec::with_capacity(8);
    key_data.extend_from_slice(&process_id.to_be_bytes());
    key_data.extend_from_slice(&1234i32.to_be_bytes());
    response.extend_from_slice(&backend_frame(b'K', &key_data));
    response.extend_from_slice(&backend_frame(b'Z', b"I"));
    stream
        .write_all(&response)
        .expect("write scripted startup response");
    stream.flush().expect("flush scripted startup response");
}

fn expect_simple_query(stream: &mut (impl Read + Write)) -> Vec<u8> {
    let mut tag = [0u8; 1];
    stream.read_exact(&mut tag).expect("read frontend tag");
    assert_eq!(tag[0], b'Q', "expected a simple-query frame");
    let mut length = [0u8; 4];
    stream
        .read_exact(&mut length)
        .expect("read frontend frame length");
    let length = u32::from_be_bytes(length) as usize;
    assert!(length >= 4, "frontend frame length is below its header");
    let mut body = vec![0u8; length - 4];
    stream
        .read_exact(&mut body)
        .expect("read frontend frame body");
    body
}

fn stub_config(addr: SocketAddr) -> Config {
    let mut config = Config::new();
    config
        .user("scripted-user")
        .hostaddr(addr.ip())
        .port(addr.port())
        .ssl_mode(SslMode::Disable)
        .connect_timeout(Duration::from_secs(1));
    config
}

/// What one hostile exchange reported, from BOTH places an error can land.
///
/// Two fields rather than one because a framing violation does not always reach
/// the caller. Measured on 2026-08-23: the sub-header-length case hands the
/// query the generic `connection closed` while the real diagnosis - `invalid
/// message length: header length < 4` - is what the connection task returns. A
/// test that asserted only on `query` there would be asserting on a PROXY, and
/// `connection closed` is exactly the string a peer that simply hung up
/// produces, so it distinguishes nothing.
struct HostileOutcome {
    /// What the caller's own operation reported.
    query: String,
    /// What `Connection::run` returned, when it returned an error at all. It is
    /// `None` when the connection task is still running - which is itself worth
    /// knowing, and is the case for every violation the driver treats as
    /// belonging to one response rather than to the session.
    connection: Option<String>,
}

impl HostileOutcome {
    /// Require `needle` to appear in one of the two places an error can land.
    ///
    /// The assertion these tests carried until 2026-08-23 was
    /// `!chain.is_empty()` - "an error occurred", which every wrong answer in
    /// this file also satisfies, including an error the HARNESS caused. Naming
    /// the diagnosis is what makes the test about the violation it substitutes.
    fn names(&self, needle: &str) {
        let connection = self.connection.as_deref().unwrap_or("<still running>");
        assert!(
            self.query.contains(needle) || connection.contains(needle),
            "expected the driver to report {needle:?}; the query said {:?} and the connection \
             task said {connection:?}",
            self.query
        );
    }
}

/// Run one simple query against a peer that answers it with `response`, and
/// report the error plus whether the session was retired.
///
/// Every caller goes through here so the three assertions cannot drift apart
/// between tests, and so "the query failed" and "the session was retired" are
/// always measured on the same connection.
async fn hostile_response_retires_session(process_id: i32, response: Vec<u8>) -> HostileOutcome {
    let server = StubServer::spawn(move |listener| {
        let mut stream = accept_bounded(&listener);
        complete_startup(&mut stream, process_id);
        let query = expect_simple_query(&mut stream);
        assert_eq!(query, b"SELECT 1\0");
        // A short write is fine: the point is what the client does with what
        // it got, not that the peer stayed well-behaved afterwards.
        let _ = stream.write_all(&response);
        let _ = stream.flush();
        // Hold the socket open so a hang is the driver's doing, not an EOF
        // that would let it conclude "connection closed" for the wrong reason.
        thread::sleep(Duration::from_millis(300));
    });

    let (client, connection) = stub_config(server.addr)
        .connect(common::suite_tls())
        .await
        .expect("connect to scripted PostgreSQL peer");
    let driver = compio::runtime::spawn(async move { connection.run().await });

    let error = compio::time::timeout(OPERATION_WATCHDOG, client.simple_query("SELECT 1"))
        .await
        .expect("the query hung instead of rejecting a malformed frame")
        .expect_err("the driver accepted a malformed frame as a valid response");

    // (2) The session must not be handed to the next caller. READ THE LIMIT OF
    // THIS, measured 2026-08-23: for the unknown-tag case and both DataRow
    // cases the driver does NOT retire the session -- `client.is_closed()` is
    // still false half a second after the error -- and what makes the follow-up
    // fail is the peer above hanging up at 300ms. Raise that sleep past
    // `OPERATION_WATCHDOG` and those three fail here instead, on the `Err` arm.
    // So this arm says "the poisoned session does not serve a second query",
    // which is true, and NOT "the driver retired it", which is the stronger
    // claim the file header used to make on its behalf. Only
    // `a_length_below_its_own_header_is_refused` retires by the driver's own
    // doing, and only `a_truncated_frame_followed_by_silence_does_not_hang`
    // (which scripts its own peer) asserts retirement directly.
    let reuse = compio::time::timeout(OPERATION_WATCHDOG, client.simple_query("SELECT 2")).await;
    match reuse {
        Err(_) => panic!("reusing the poisoned session hung instead of failing"),
        Ok(Ok(_)) => panic!("the driver reused a session after a malformed frame"),
        Ok(Err(_)) => {}
    }

    let connection = match compio::time::timeout(OPERATION_WATCHDOG, driver).await {
        Ok(Ok(Err(error))) => Some(common::error_chain(&error)),
        _ => None,
    };
    drop(client);
    server.finish();
    HostileOutcome {
        query: common::error_chain(&error),
        connection,
    }
}

/// A byte that is not any backend message type must be refused, not skipped.
///
/// Skipping it would be the dangerous outcome: the driver would resynchronise
/// on whatever followed and hand the caller rows from a frame it never parsed.
#[compio::test]
async fn an_unknown_message_tag_is_refused_and_retires_the_session() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let outcome =
            hostile_response_retires_session(201, backend_frame(b'\x7f', b"nonsense")).await;
        outcome.names("unknown message tag `127`");
    })
    .await
    .expect("unknown-tag test exceeded its outer watchdog");
}

/// `DataRow` before any `RowDescription` is a real PostgreSQL message arriving
/// where the protocol forbids it - the shape a mangling proxy produces.
#[compio::test]
async fn a_data_row_without_a_row_description_is_refused() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let mut body = Vec::new();
        body.extend_from_slice(&1u16.to_be_bytes());
        body.extend_from_slice(&1u32.to_be_bytes());
        body.push(b'x');
        let outcome = hostile_response_retires_session(202, backend_frame(b'D', &body)).await;
        outcome.names("unexpected message from server");
    })
    .await
    .expect("out-of-place DataRow test exceeded its outer watchdog");
}

/// A `RowDescription` declaring more columns than the `DataRow` then carries.
///
/// This is the one malformed shape whose consequence was a PANIC rather than an
/// error, and in the accessor whose entire contract is not panicking. The row
/// accessors bounds-check the caller's index against the COLUMNS and then index
/// the FIELDS, so the two lists agreeing is load-bearing; when they disagreed,
/// `try_get` -- which returns a `Result` precisely so a caller need not trust
/// the server -- panicked with an out-of-bounds index instead.
///
/// `src/row.rs` grew an arity check for it and carries a comment explaining the
/// panic. Nothing drove that check from outside: it is enforced in two
/// independent constructors and no test in the tree reached either. So this
/// pins the one that the simple-query path uses.
fn row_description(columns: &[&str]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(
        &u16::try_from(columns.len())
            .expect("column count")
            .to_be_bytes(),
    );
    for name in columns {
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(&0u32.to_be_bytes()); // table oid: not from a table
        body.extend_from_slice(&0i16.to_be_bytes()); // column id
        body.extend_from_slice(&25u32.to_be_bytes()); // text
        body.extend_from_slice(&(-1i16).to_be_bytes()); // varlena
        body.extend_from_slice(&(-1i32).to_be_bytes()); // no type modifier
        body.extend_from_slice(&0u16.to_be_bytes()); // text format
    }
    body
}

/// Dropping the last client handle ends TLS with the authenticated alert, not
/// by cutting off the transport underneath rustls.
///
/// This peer inspects rustls' processed record state. A bare EOF, a reset, or
/// arbitrary bytes cannot set `peer_has_closed`, so the assertion observes the
/// real encrypted `close_notify` rather than a teardown helper being called.
/// A cancel key whose length the server chose must be REFUSED when it is
/// outside what the protocol allows.
///
/// Protocol 3.2 made the cancel key variable-length, so its size now comes
/// from server input rather than being fixed at four bytes. That is the one
/// field in startup a hostile peer gets to size, and `CancelKey::new` bounds
/// it at 4..=256. The frame limit alone is not the answer: `max_message_size`
/// defaults far above 256, so a 300-byte key arrives intact and is rejected
/// only if something checks the key itself.
///
/// The fuzzer reaches this shape too, but asserts termination without a panic;
/// this asserts the driver REFUSES and says why, which is the standard the
/// rest of this file holds malformed input to.
#[compio::test]
async fn a_cancel_key_outside_the_allowed_length_is_refused() {
    for (label, key_len) in [("over-long", 300usize), ("too-short", 3usize)] {
        let server = StubServer::spawn(move |listener| {
            let mut stream = accept_bounded(&listener);

            let mut length = [0u8; 4];
            stream.read_exact(&mut length).expect("startup length");
            let length = u32::from_be_bytes(length) as usize;
            let mut body = vec![0u8; length - 4];
            stream.read_exact(&mut body).expect("startup body");

            let mut key_data = Vec::new();
            key_data.extend_from_slice(&99i32.to_be_bytes());
            key_data.extend(std::iter::repeat_n(0xABu8, key_len));

            let mut response = backend_frame(b'R', &0u32.to_be_bytes());
            response.extend_from_slice(&backend_frame(b'K', &key_data));
            response.extend_from_slice(&backend_frame(b'Z', b"I"));
            let _ = stream.write_all(&response);
            let _ = stream.flush();

            // Stay open so a hang is the driver's doing, not an EOF it could
            // read as the session simply ending.
            thread::sleep(Duration::from_millis(300));
        });

        let error = compio::time::timeout(
            OPERATION_WATCHDOG,
            stub_config(server.addr).connect(common::suite_tls()),
        )
        .await
        .unwrap_or_else(|_| panic!("{label} cancel key hung the handshake"))
        .err()
        .unwrap_or_else(|| panic!("{label} cancel key was accepted"));

        let mut chain = error.to_string();
        let mut source = std::error::Error::source(&error);
        while let Some(cause) = source {
            chain.push_str(" | ");
            chain.push_str(&cause.to_string());
            source = std::error::Error::source(cause);
        }
        // Either wording satisfies the contract, which is that the refusal
        // NAMES the length rather than looking like any other handshake
        // failure. Both frames here arrive behind `AuthenticationOk`, and since
        // the per-tag startup limit began applying to batched frames too they
        // are refused from the header - "invalid BackendKeyData length 308;
        // expected 12 to 264" - instead of reaching the cancel-key parser,
        // which said "cancel key length" only after buffering the whole body.
        // Earlier, and equally specific.
        assert!(
            chain.contains("cancel key length") || chain.contains("BackendKeyData length"),
            "the {label} key was refused without saying the length was wrong, \
             so a caller cannot tell it from any other handshake failure: {chain}"
        );

        server.finish();
    }
}

/// A peer that sends the RIGHT bytes, one at a time.
///
/// Every other case in this file sends wrong bytes promptly. This one is the
/// mirror: nothing is malformed, the frames simply arrive in as many pieces as
/// they have bytes, so each socket read returns a fragment of a frame and
/// several reads land mid-header. The header of this file named it as the gap.
///
/// What it rules out is a read loop that treats a short read as a framing
/// error, or that only makes progress when a whole frame arrives at once. The
/// value is asserted, not merely the absence of an error, so a driver that
/// reassembles the bytes in the wrong order fails here too.
#[compio::test]
async fn a_peer_that_trickles_a_frame_one_byte_at_a_time_is_understood() {
    let mut response = Vec::new();
    let mut row = Vec::new();
    row.extend_from_slice(&1u16.to_be_bytes());
    row.extend_from_slice(&1u32.to_be_bytes());
    row.push(b'7');
    response.extend_from_slice(&backend_frame(b'T', &row_description(&["?column?"])));
    response.extend_from_slice(&backend_frame(b'D', &row));
    response.extend_from_slice(&backend_frame(b'C', b"SELECT 1\0"));
    response.extend_from_slice(&backend_frame(b'Z', b"I"));

    let server = StubServer::spawn(move |listener| {
        let mut stream = accept_bounded(&listener);
        complete_startup(&mut stream, 217);
        let query = expect_simple_query(&mut stream);
        assert_eq!(query, b"SELECT 7\0");

        // One byte per write, each flushed, so the client cannot receive two
        // together through this side's buffering. A sleep every few bytes
        // keeps the total well inside OPERATION_WATCHDOG while still forcing
        // the reads to be genuinely separate.
        for (index, byte) in response.iter().enumerate() {
            stream
                .write_all(std::slice::from_ref(byte))
                .expect("trickle one byte");
            stream.flush().expect("flush one byte");
            if index % 8 == 0 {
                thread::sleep(Duration::from_millis(1));
            }
        }

        // Stay open, so a hang is the driver's doing rather than an EOF it
        // could read as "the session ended".
        thread::sleep(Duration::from_millis(200));
    });

    let (client, connection) = stub_config(server.addr)
        .connect(common::suite_tls())
        .await
        .expect("connect to scripted PostgreSQL peer");
    let driver = compio::runtime::spawn(async move { connection.run().await });

    let messages = compio::time::timeout(OPERATION_WATCHDOG, client.simple_query("SELECT 7"))
        .await
        .expect("a trickled response hung the driver")
        .expect("a trickled but well-formed response was rejected");

    let value = messages.iter().find_map(|message| match message {
        compio_postgres::SimpleQueryMessage::Row(row) => row.get(0).map(str::to_owned),
        _ => None,
    });
    assert_eq!(
        value.as_deref(),
        Some("7"),
        "the reassembled row does not carry the value the peer sent"
    );

    drop(client);
    server.finish();
    let _ = compio::time::timeout(OPERATION_WATCHDOG, driver).await;
}

/// A self-signed TLS server configuration for the scripted peers below.
#[cfg(feature = "tls")]
fn scripted_tls_server_config() -> std::sync::Arc<rustls::ServerConfig> {
    let issued = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("generate test certificate");
    let cert = rustls::pki_types::CertificateDer::from(issued.cert.der().to_vec());
    let key = rustls::pki_types::PrivateKeyDer::try_from(issued.signing_key.serialize_der())
        .expect("serialize test key");
    let server_config = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("safe TLS protocol versions")
    .with_no_client_auth()
    .with_single_cert(vec![cert], key)
    .expect("build TLS server config");
    std::sync::Arc::new(server_config)
}

/// Read TLS records until the peer's `close_notify` arrives.
///
/// Fails on EOF rather than treating it as an ending: a transport that just
/// stops is exactly the unclean shutdown these tests exist to catch.
#[cfg(feature = "tls")]
fn expect_close_notify(tls: &mut rustls::ServerConnection, socket: &mut TcpStream) {
    loop {
        let read = tls.read_tls(socket).unwrap_or_else(|error| {
            panic!("TLS transport failed before close_notify arrived: {error}")
        });
        assert_ne!(
            read, 0,
            "TLS transport reached EOF before close_notify arrived"
        );
        let state = tls
            .process_new_packets()
            .expect("process client TLS shutdown records");
        let peer_has_closed = state.peer_has_closed();

        let mut plaintext = [0u8; 64];
        loop {
            match tls.reader().read(&mut plaintext) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => panic!("read client shutdown plaintext: {error}"),
            }
        }
        if peer_has_closed {
            break;
        }
    }
}

/// The CONNECTION half going away must also close the session cleanly.
///
/// `ConnectionRelease` (the client half) was taught to send `close_notify`
/// first; `ConnectionDropRelease` is the guard for every connection-side exit -
/// discarded without being run, `run` cancelled, the future unwound, the task
/// returned - and the read-timeout recovery path calls its `shutdown()`
/// deliberately. MEASURED 2026-08-25 against a live TLS server, one connection
/// per case: dropping the CLIENT logged nothing, dropping an unrun CONNECTION
/// logged `could not receive data from client: Connection reset by peer`.
///
/// The client is kept alive here on purpose. Dropping it would fire the
/// already-fixed client-side release and this test would pass without the
/// guard doing anything.
#[cfg(feature = "tls")]
#[compio::test]
async fn dropping_an_unrun_tls_connection_sends_close_notify() {
    let server_config = scripted_tls_server_config();

    let server = StubServer::spawn(move |listener| {
        let mut socket = accept_bounded(&listener);

        let mut ssl_request = [0u8; 8];
        socket
            .read_exact(&mut ssl_request)
            .expect("read PostgreSQL SSLRequest");
        socket.write_all(b"S").expect("accept TLS negotiation");
        socket.flush().expect("flush TLS negotiation response");

        let mut tls =
            rustls::ServerConnection::new(server_config).expect("build TLS server session");
        {
            let mut stream = rustls::Stream::new(&mut tls, &mut socket);
            complete_startup(&mut stream, 215);
        }

        expect_close_notify(&mut tls, &mut socket);
    });

    let dsn = format!(
        "host=localhost hostaddr={} port={} user=scripted-user sslmode=require connect_timeout=2",
        server.addr.ip(),
        server.addr.port()
    );
    let config = dsn.parse::<Config>().expect("parse TLS peer config");
    let tls = compio_postgres::MakeRustlsConnect::from_config(&config)
        .expect("build unverified rustls connector");
    let (client, connection) = config
        .connect(tls)
        .await
        .expect("connect to scripted TLS PostgreSQL peer");

    // The guard fires here, with the client still holding its own dup.
    drop(connection);

    server.finish();
    drop(client);
}

/// A command timeout retires the session through `Client::force_close`, which
/// calls `ConnectionRelease::shutdown()` DIRECTLY rather than dropping it.
///
/// That direct path is the one the fixes above did not reach: `Drop` sent the
/// alert, `shutdown()` did not, and six production sites call `shutdown()`
/// without dropping - pool command-timeout recovery here, plus four
/// replication cleanup paths. So a pool that times out a query over TLS ended
/// the session with no alert, which is the case a real deployment hits most.
#[cfg(feature = "tls")]
#[compio::test]
async fn a_command_timeout_closes_the_tls_session_cleanly() {
    let server_config = scripted_tls_server_config();

    let server = StubServer::spawn(move |listener| {
        let mut socket = accept_bounded(&listener);
        // Expiry attempts a REAL cancel first, on a second connection this
        // scripted peer never accepts, so recovery takes about as long as the
        // shared socket watchdog. Wait longer than that here, or the peer
        // stops reading just before the alert and the test measures its own
        // patience instead of the driver.
        socket
            .set_read_timeout(Some(Duration::from_secs(8)))
            .expect("extend this peer's read watchdog");

        let mut ssl_request = [0u8; 8];
        socket
            .read_exact(&mut ssl_request)
            .expect("read PostgreSQL SSLRequest");
        socket.write_all(b"S").expect("accept TLS negotiation");
        socket.flush().expect("flush TLS negotiation response");

        let mut tls =
            rustls::ServerConnection::new(server_config).expect("build TLS server session");
        {
            let mut stream = rustls::Stream::new(&mut tls, &mut socket);
            complete_startup(&mut stream, 216);
            // Take the query and never answer it. The client's command timeout
            // is what ends this session.
            let _ = expect_simple_query(&mut stream);
        }

        expect_close_notify(&mut tls, &mut socket);
    });

    let dsn = format!(
        "host=localhost hostaddr={} port={} user=scripted-user sslmode=require connect_timeout=2",
        server.addr.ip(),
        server.addr.port()
    );
    let config = dsn.parse::<Config>().expect("parse TLS peer config");
    let mut pool_config = compio_postgres::PoolConfig::new();
    // ONE connection: the scripted peer accepts exactly one, and a warm-up
    // that opens a second fails the test for a reason that is not the claim.
    pool_config.max_size(1);
    pool_config.min_idle(0);
    pool_config.command_timeout(Duration::from_millis(300));

    let pool = compio_postgres::Pool::connect_with_config(config, pool_config)
        .await
        .expect("build a pool against the scripted TLS peer");
    let mut client = pool.get().await.expect("lease a pooled connection");

    // `command` is what arms the recovery guard; a bare `simple_query` derefs
    // straight to `Client` and never reaches the pool's timeout at all.
    let outcome = client
        .command(async |client| client.simple_query("SELECT 1").await.map(|_| ()))
        .await;
    assert!(
        outcome.is_err(),
        "the scripted peer never answered, so this must time out"
    );

    // Order matters: the lease and the pool must go away while the scripted
    // peer is still reading, or it stops waiting before the alert is sent and
    // the test fails for its own reason rather than the driver's.
    drop(client);
    drop(pool);
    server.finish();
}

#[cfg(feature = "tls")]
#[compio::test]
async fn dropping_a_tls_client_sends_close_notify() {
    let server_config = scripted_tls_server_config();

    let server = StubServer::spawn(move |listener| {
        let mut socket = accept_bounded(&listener);

        let mut ssl_request = [0u8; 8];
        socket
            .read_exact(&mut ssl_request)
            .expect("read PostgreSQL SSLRequest");
        assert_eq!(
            u32::from_be_bytes(ssl_request[..4].try_into().unwrap()),
            8,
            "SSLRequest length"
        );
        assert_eq!(
            u32::from_be_bytes(ssl_request[4..].try_into().unwrap()),
            80_877_103,
            "SSLRequest code"
        );
        socket.write_all(b"S").expect("accept TLS negotiation");
        socket.flush().expect("flush TLS negotiation response");

        let mut tls =
            rustls::ServerConnection::new(server_config).expect("build TLS server session");

        {
            let mut stream = rustls::Stream::new(&mut tls, &mut socket);
            complete_startup(&mut stream, 214);
            let query = expect_simple_query(&mut stream);
            assert_eq!(query, b"SELECT 1\0");

            let mut row = Vec::new();
            row.extend_from_slice(&1u16.to_be_bytes());
            row.extend_from_slice(&1u32.to_be_bytes());
            row.push(b'1');
            let mut response = backend_frame(b'T', &row_description(&["?column?"]));
            response.extend_from_slice(&backend_frame(b'D', &row));
            response.extend_from_slice(&backend_frame(b'C', b"SELECT 1\0"));
            response.extend_from_slice(&backend_frame(b'Z', b"I"));
            stream
                .write_all(&response)
                .expect("write SELECT 1 response");
            stream.flush().expect("flush SELECT 1 response");
        }

        expect_close_notify(&mut tls, &mut socket);
    });

    let dsn = format!(
        "host=localhost hostaddr={} port={} user=scripted-user sslmode=require connect_timeout=2",
        server.addr.ip(),
        server.addr.port()
    );
    let config = dsn.parse::<Config>().expect("parse TLS peer config");
    let tls = compio_postgres::MakeRustlsConnect::from_config(&config)
        .expect("build unverified rustls connector");
    let (client, connection) = config
        .connect(tls)
        .await
        .expect("connect to scripted TLS PostgreSQL peer");
    let driver = compio::runtime::spawn(async move { connection.run().await });

    let messages = compio::time::timeout(OPERATION_WATCHDOG, client.simple_query("SELECT 1"))
        .await
        .expect("SELECT 1 over TLS exceeded its watchdog")
        .expect("run SELECT 1 over TLS");
    assert!(
        messages.iter().any(|message| matches!(
            message,
            compio_postgres::SimpleQueryMessage::Row(row) if row.get(0) == Some("1")
        )),
        "the scripted SELECT 1 response did not reach the client"
    );
    drop(client);

    server.finish();
    let _ = compio::time::timeout(OPERATION_WATCHDOG, driver)
        .await
        .expect("connection task did not exit after client release");
}

#[compio::test]
async fn a_data_row_with_fewer_fields_than_its_description_is_refused() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let mut short_row = Vec::new();
        short_row.extend_from_slice(&1u16.to_be_bytes()); // one field ...
        short_row.extend_from_slice(&1u32.to_be_bytes());
        short_row.push(b'x');

        // THE RESPONSE MUST OTHERWISE BE COMPLETE, and this is the whole
        // difficulty of the test. Ending it after the DataRow makes the driver
        // fail on the truncation instead, so the test passed with the arity
        // check REMOVED -- verified by mutation, which is the only reason this
        // is written the hard way. With CommandComplete and ReadyForQuery
        // present the only thing wrong is the arity, so the error can only come
        // from the check under test.
        let mut response = backend_frame(b'T', &row_description(&["a", "b"])); // ... two columns
        response.extend_from_slice(&backend_frame(b'D', &short_row));
        response.extend_from_slice(&backend_frame(b'C', b"SELECT 1\0"));
        response.extend_from_slice(&backend_frame(b'Z', b"I"));

        let outcome = hostile_response_retires_session(210, response).await;
        outcome.names("DataRow carries 1 fields but its RowDescription declared 2 columns");
    })
    .await
    .expect("short-DataRow test exceeded its outer watchdog");
}

/// A length shorter than the payload leaves trailing bytes the driver will read
/// as the start of the next frame. It must not resynchronise onto them.
#[compio::test]
async fn a_length_shorter_than_its_payload_is_refused() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let outcome =
            hostile_response_retires_session(203, lying_frame(b'C', b"SELECT 1\0", 6)).await;
        outcome.names("unexpected EOF");
    })
    .await
    .expect("short-length test exceeded its outer watchdog");
}

/// A length below the 4-byte header itself is unrepresentable, not merely
/// wrong: there is no body length that satisfies it.
#[compio::test]
async fn a_length_below_its_own_header_is_refused() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let outcome = hostile_response_retires_session(204, lying_frame(b'C', b"", 3)).await;
        outcome.names("invalid message length: header length < 4");
    })
    .await
    .expect("sub-header-length test exceeded its outer watchdog");
}

/// Half a frame followed by silence, from a peer that KEEPS THE SOCKET OPEN.
/// The driver must give up on its own clock rather than waiting for the rest
/// forever.
///
/// THIS TEST COULD NOT MEASURE ITS OWN NAME UNTIL 2026-08-23, and the reason is
/// the one this file exists to catch. It went through
/// `hostile_response_retires_session`, whose peer sleeps 300ms and then drops
/// the socket - so the error was the EOF, arriving well inside the 2s
/// `OPERATION_WATCHDOG`, and the test passed exactly as well for a driver with
/// no clock of its own at all. Measured, by raising that sleep to 3000ms: the
/// query then hung and the test failed at "the query hung instead of rejecting
/// a malformed frame". `stub_config` sets no `read_timeout`, so the FIXTURE
/// could not represent the difference the name asserts.
///
/// So this one configures clock (3) - `Config::read_timeout`, the post-startup
/// socket-read inactivity deadline - and holds the socket open far past it. The
/// peer never closes and never sends another byte, so an EOF cannot be what
/// ends the wait: the only thing that can is the driver's own deadline. The
/// error is required to BE that deadline (`is_read_timeout`), not merely to
/// exist, and it must arrive before the peer would have hung up anyway.
#[compio::test]
async fn a_truncated_frame_followed_by_silence_does_not_hang() {
    /// The driver's own deadline. Short so the test is quick; the point is that
    /// it fires long before `PEER_HOLD`.
    const READ_TIMEOUT: Duration = Duration::from_millis(400);
    /// How long the peer stays connected and silent. Must exceed `READ_TIMEOUT`
    /// by enough that an error arriving on the peer's clock is unmistakable.
    const PEER_HOLD: Duration = Duration::from_millis(2500);

    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = StubServer::spawn(move |listener| {
            let mut stream = accept_bounded(&listener);
            complete_startup(&mut stream, 205);
            let query = expect_simple_query(&mut stream);
            assert_eq!(query, b"SELECT 1\0");
            // Declares 64 bytes of body and sends four.
            let _ = stream.write_all(&lying_frame(b'D', b"abcd", 68));
            let _ = stream.flush();
            thread::sleep(PEER_HOLD);
        });

        let mut config = stub_config(server.addr);
        config.read_timeout(READ_TIMEOUT);
        let (client, connection) = config
            .connect(common::suite_tls())
            .await
            .expect("connect to scripted PostgreSQL peer");
        let driver = compio::runtime::spawn(async move { connection.run().await });

        let started = Instant::now();
        let error = compio::time::timeout(OPERATION_WATCHDOG, client.simple_query("SELECT 1"))
            .await
            .expect("the query hung instead of giving up on the driver's own clock")
            .expect_err("the driver accepted a truncated frame as a valid response");
        let elapsed = started.elapsed();

        // (1) THE DRIVER'S CLOCK, not the peer's. Both halves matter: the right
        // error kind, and an arrival time that rules the peer out as its cause.
        assert!(
            error.is_read_timeout(),
            "a truncated frame followed by silence ended with {} rather than the read deadline",
            common::error_chain(&error)
        );
        assert!(
            elapsed < PEER_HOLD,
            "the query took {elapsed:?}, which is not distinguishable from the peer hanging up \
             at {PEER_HOLD:?}"
        );

        // (2) Retired, not returned to service. `read_timeout` retires the
        // connection on expiry because a cancelled, possibly partial read
        // cannot be resumed, so this is the driver's doing and not an EOF.
        assert!(
            client.is_closed(),
            "the driver kept the session usable after its read deadline expired"
        );
        let reuse =
            compio::time::timeout(OPERATION_WATCHDOG, client.simple_query("SELECT 2")).await;
        match reuse {
            Err(_) => panic!("reusing the timed-out session hung instead of failing"),
            Ok(Ok(_)) => panic!("the driver reused a session after its read deadline expired"),
            Ok(Err(_)) => {}
        }

        let _ = compio::time::timeout(OPERATION_WATCHDOG, driver).await;
        drop(client);
        server.finish();
    })
    .await
    .expect("truncated-frame test exceeded its outer watchdog");
}

// ---------------------------------------------------------------------------
// Out-of-order WELL-FORMED messages.
//
// Everything above is refused while the frame is being DECODED - a bad tag, a
// lying length. Those never reach the driver's protocol state machine, which is
// where the 44 `unexpected_message()` sites live. The tests below send messages
// PostgreSQL really emits, correctly framed, in positions the protocol forbids.
// That is the only way to drive those branches, and it is the shape a mangling
// proxy or a confused pooler actually produces.
//
// `Client::prepare` drives the extended query protocol and expects, in order:
// ParseComplete, then ParameterDescription, then RowDescription or NoData.
// Each of the three tests below substitutes a legitimate message at one of
// those positions, so exactly one expectation is violated per test.
// ---------------------------------------------------------------------------

fn expect_frontend_until_sync(stream: &mut TcpStream) {
    // prepare() writes Parse + Describe + Sync as one batch. Drain to the Sync
    // so the scripted reply cannot race the client's own write.
    loop {
        let mut tag = [0u8; 1];
        if stream.read_exact(&mut tag).is_err() {
            return;
        }
        let mut length = [0u8; 4];
        if stream.read_exact(&mut length).is_err() {
            return;
        }
        let length = u32::from_be_bytes(length) as usize;
        assert!(length >= 4, "frontend frame length is below its header");
        let mut body = vec![0u8; length - 4];
        if stream.read_exact(&mut body).is_err() {
            return;
        }
        if tag[0] == b'S' {
            return;
        }
    }
}

/// Drive `prepare` against a peer that answers with `response`, and require the
/// same three properties the simple-query helper does.
async fn hostile_prepare_retires_session(process_id: i32, response: Vec<u8>) -> String {
    let server = StubServer::spawn(move |listener| {
        let mut stream = accept_bounded(&listener);
        complete_startup(&mut stream, process_id);
        expect_frontend_until_sync(&mut stream);
        let _ = stream.write_all(&response);
        let _ = stream.flush();
        thread::sleep(Duration::from_millis(300));
    });

    let (client, connection) = stub_config(server.addr)
        .connect(common::suite_tls())
        .await
        .expect("connect to scripted PostgreSQL peer");
    let driver = compio::runtime::spawn(async move { connection.run().await });

    let error = compio::time::timeout(OPERATION_WATCHDOG, client.prepare("SELECT $1::int4"))
        .await
        .expect("prepare hung instead of rejecting an out-of-order message")
        .expect_err("the driver accepted an out-of-order message as a valid response");

    let reuse = compio::time::timeout(OPERATION_WATCHDOG, client.simple_query("SELECT 2")).await;
    match reuse {
        Err(_) => panic!("reusing the poisoned session hung instead of failing"),
        Ok(Ok(_)) => panic!("the driver reused a session after an out-of-order message"),
        Ok(Err(_)) => {}
    }

    let _ = compio::time::timeout(OPERATION_WATCHDOG, driver).await;
    drop(client);
    server.finish();
    common::error_chain(&error)
}

/// The extended-protocol twin of
/// [`a_data_row_with_fewer_fields_than_its_description_is_refused`].
///
/// The arity check lives in TWO constructors -- `Row::new` for the extended
/// path and `SimpleQueryRow::new` for the simple one -- and neither was driven
/// by a test. The simple-query version needs one scripted reply; this one needs
/// two, because the column count is fixed by the RowDescription that `prepare`
/// receives and the offending DataRow only arrives on the later execute. That
/// split is the whole reason the two constructors exist separately, so covering
/// one says nothing about the other.
#[compio::test]
async fn an_extended_query_data_row_shorter_than_its_description_is_refused() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        // prepare: ParseComplete, no parameters, two columns, ready.
        let mut prepare_reply = backend_frame(b'1', b"");
        prepare_reply.extend_from_slice(&backend_frame(b't', &0u16.to_be_bytes()));
        prepare_reply.extend_from_slice(&backend_frame(b'T', &row_description(&["a", "b"])));
        prepare_reply.extend_from_slice(&backend_frame(b'Z', b"I"));

        // execute: a row carrying one field where two columns were declared.
        // Complete in every other respect, for the reason recorded on the
        // simple-query twin -- truncating it here would make the driver fail on
        // the truncation and the test would pass with the check removed.
        let mut short_row = Vec::new();
        short_row.extend_from_slice(&1u16.to_be_bytes());
        short_row.extend_from_slice(&1u32.to_be_bytes());
        short_row.push(b'x');
        let mut query_reply = backend_frame(b'2', b"");
        query_reply.extend_from_slice(&backend_frame(b'D', &short_row));
        query_reply.extend_from_slice(&backend_frame(b'C', b"SELECT 1\0"));
        query_reply.extend_from_slice(&backend_frame(b'Z', b"I"));

        let server = StubServer::spawn(move |listener| {
            let mut stream = accept_bounded(&listener);
            complete_startup(&mut stream, 211);
            expect_frontend_until_sync(&mut stream);
            let _ = stream.write_all(&prepare_reply);
            let _ = stream.flush();
            expect_frontend_until_sync(&mut stream);
            let _ = stream.write_all(&query_reply);
            let _ = stream.flush();
            thread::sleep(Duration::from_millis(300));
        });

        let (client, connection) = stub_config(server.addr)
            .connect(common::suite_tls())
            .await
            .expect("connect to scripted PostgreSQL peer");
        let driver = compio::runtime::spawn(async move { connection.run().await });

        let statement = compio::time::timeout(OPERATION_WATCHDOG, client.prepare("SELECT 1"))
            .await
            .expect("prepare hung against the scripted peer")
            .expect("the scripted prepare reply is well formed");

        let error = compio::time::timeout(OPERATION_WATCHDOG, client.query(&statement, &[]))
            .await
            .expect("the query hung instead of rejecting a short DataRow")
            .expect_err("the driver built a row with fewer fields than columns");
        assert!(
            common::error_chain(&error)
                .contains("DataRow carries 1 fields but its RowDescription declared 2 columns"),
            "an extended-protocol short DataRow reported {:?} rather than the arity check",
            common::error_chain(&error)
        );

        let _ = compio::time::timeout(OPERATION_WATCHDOG, driver).await;
        drop(client);
        server.finish();
    })
    .await
    .expect("extended short-DataRow test exceeded its outer watchdog");
}

/// `BindComplete` is a real message, correctly framed - but Parse was what was
/// owed. Accepting it would leave the driver believing a statement is prepared
/// that the server never parsed.
#[compio::test]
async fn a_bind_complete_where_parse_complete_is_owed_is_refused() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let chain = hostile_prepare_retires_session(301, backend_frame(b'2', b"")).await;
        assert!(
            chain.contains("unexpected message from server"),
            "a misplaced BindComplete reported {chain:?} rather than an out-of-order message"
        );
    })
    .await
    .expect("misplaced BindComplete test exceeded its outer watchdog");
}

/// ParseComplete lands correctly, then `NoData` arrives where the parameter
/// description is owed. The statement's parameter types would otherwise be
/// silently unknown.
#[compio::test]
async fn a_missing_parameter_description_is_refused() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let mut response = backend_frame(b'1', b"");
        response.extend_from_slice(&backend_frame(b'n', b""));
        let chain = hostile_prepare_retires_session(302, response).await;
        assert!(
            chain.contains("unexpected message from server"),
            "a missing ParameterDescription reported {chain:?} rather than an out-of-order message"
        );
    })
    .await
    .expect("missing ParameterDescription test exceeded its outer watchdog");
}

/// The first two steps land, then `BindComplete` arrives where the row
/// description or `NoData` is owed - the last of prepare's three expectations.
#[compio::test]
async fn a_misplaced_message_after_the_parameter_description_is_refused() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let mut response = backend_frame(b'1', b"");
        let mut params = Vec::new();
        params.extend_from_slice(&1u16.to_be_bytes());
        params.extend_from_slice(&23u32.to_be_bytes());
        response.extend_from_slice(&backend_frame(b't', &params));
        response.extend_from_slice(&backend_frame(b'2', b""));
        let chain = hostile_prepare_retires_session(303, response).await;
        assert!(
            chain.contains("unexpected message from server"),
            "a misplaced post-ParameterDescription message reported {chain:?} rather than an \
             out-of-order message"
        );
    })
    .await
    .expect("misplaced post-ParameterDescription test exceeded its outer watchdog");
}

// ---------------------------------------------------------------------------
// COPY sub-protocol violations.
//
// `src/copy_out.rs` and `src/copy_in.rs` hold 8 of the 43 refusal sites, and
// this file's header listed them as NOT covered. They are the last named
// cluster. COPY has its own sub-protocol on top of the extended query one:
// ParseComplete, BindComplete, then CopyOutResponse or CopyInResponse, and only
// then a CopyData stream. Each step is a place a mangling proxy can substitute
// something legitimate-looking.
//
// The mid-stream case matters most. Once a COPY is established the driver is
// reading a data stream, and a non-CopyData message there is the point at which
// a driver that resynchronises would start handing the caller bytes from frames
// it never parsed as data.
//
// WHAT THESE THREE REACH, and it took a correction to get right. The first
// version of this helper answered the FIRST frontend batch - but `copy_out(&str)`
// prepares before it copies, so that batch is Parse, Describe, Sync. Logging the
// frontend tags showed "PDS": the violation was answering the PREPARE, the error
// came from prepare.rs, and no COPY code ran at all. The helper now answers the
// prepare correctly and applies the violation to the SECOND batch, which logs as
// "BES" - Bind, Execute, Sync. That is the copy batch.
//
// ESTABLISHED, and this paragraph used to say the opposite. It recorded that the
// one-variable control "could not be built", that a scripted CopyOutResponse plus
// CopyData, CopyDone, CommandComplete and ReadyForQuery "still errors", and that
// the control was therefore "INCONCLUSIVE rather than negative" - so read as
// written it told the next person not to bother trying. Re-run on 2026-08-23
// against that exact sequence ([`well_formed_copy_out`], through the same
// [`copy_stub_server`]) it SUCCEEDS and yields `row-one\n`. All four COPY tests
// fail when it is substituted, each at its own `expect_err`. The refusals are
// proven necessary; the happy path is also covered against a real server by the
// copy family in integration.rs.
// ---------------------------------------------------------------------------

/// The peer every COPY OUT test talks to: it answers the prepare batch
/// correctly and applies `response` to the copy batch.
///
/// Shared by the hostile helper and by [`a_well_formed_copy_out_is_accepted`],
/// so the positive control cannot drift into scripting a DIFFERENT peer from
/// the one the negative tests use. A control against a different fixture proves
/// nothing about them.
fn copy_stub_server(process_id: i32, response: Vec<u8>) -> StubServer {
    StubServer::spawn(move |listener| {
        let mut stream = accept_bounded(&listener);
        complete_startup(&mut stream, process_id);

        // `copy_out(&str)` PREPARES first: the client sends Parse, Describe,
        // Sync as one batch, and only then Bind/Execute for the copy itself.
        // Answering that first batch with the violation answers the PREPARE,
        // so the error comes from prepare.rs and no COPY code runs at all.
        // An earlier version of this helper did exactly that; logging the
        // frontend tags showed "PDS" and settled it.
        expect_frontend_until_sync(&mut stream);
        let mut prepared = backend_frame(b'1', b"");
        let mut params = Vec::new();
        params.extend_from_slice(&0u16.to_be_bytes());
        prepared.extend_from_slice(&backend_frame(b't', &params));
        prepared.extend_from_slice(&backend_frame(b'n', b""));
        prepared.extend_from_slice(&backend_frame(b'Z', b"I"));
        let _ = stream.write_all(&prepared);
        let _ = stream.flush();

        // The copy batch arrives second, and that is what the violation answers.
        expect_frontend_until_sync(&mut stream);
        let _ = stream.write_all(&response);
        let _ = stream.flush();
        thread::sleep(Duration::from_millis(300));
    })
}

/// Drive `copy_out` against a peer answering with `response`, requiring the same
/// three properties as the other helpers: an error, a retired session, bounded.
async fn hostile_copy_out_retires_session(process_id: i32, response: Vec<u8>) -> String {
    use futures_util::TryStreamExt;

    let server = copy_stub_server(process_id, response);

    let (client, connection) = stub_config(server.addr)
        .connect(common::suite_tls())
        .await
        .expect("connect to scripted PostgreSQL peer");
    let driver = compio::runtime::spawn(async move { connection.run().await });

    let error = compio::time::timeout(OPERATION_WATCHDOG, async {
        let stream = client.copy_out("COPY t TO STDOUT").await?;
        let mut stream = Box::pin(stream);
        while stream.try_next().await?.is_some() {}
        Ok::<(), compio_postgres::Error>(())
    })
    .await
    .expect("copy_out hung instead of rejecting a malformed COPY response")
    .expect_err("the driver accepted a malformed COPY response");

    let reuse = compio::time::timeout(OPERATION_WATCHDOG, client.simple_query("SELECT 2")).await;
    match reuse {
        Err(_) => panic!("reusing the poisoned session hung instead of failing"),
        Ok(Ok(_)) => panic!("the driver reused a session after a malformed COPY response"),
        Ok(Err(_)) => {}
    }

    let _ = compio::time::timeout(OPERATION_WATCHDOG, driver).await;
    drop(client);
    server.finish();
    common::error_chain(&error)
}

/// Drive `copy_in` against a peer that answers the COPY batch with `response`.
///
/// The COPY IN direction had NO hostile coverage: all three tests below drive
/// COPY OUT. That is the wrong way round for where the machinery is. The IN
/// direction is the one with the `CopyInReceiver`, the read obligation's
/// paused/terminal states and `copy_initial_flushed`, and it is where the
/// producer-teardown and guard-window defects were found.
///
/// Same two-batch shape as [`hostile_copy_out_retires_session`], for the same
/// measured reason: `copy_in(&str)` PREPARES first, so answering the first
/// batch with the violation would only exercise `prepare.rs`.
async fn hostile_copy_in_retires_session(process_id: i32, response: Vec<u8>) -> String {
    use bytes::Bytes;
    use futures_util::SinkExt;

    let server = StubServer::spawn(move |listener| {
        let mut stream = accept_bounded(&listener);
        complete_startup(&mut stream, process_id);

        expect_frontend_until_sync(&mut stream);
        let mut prepared = backend_frame(b'1', b"");
        prepared.extend_from_slice(&backend_frame(b't', &0u16.to_be_bytes()));
        prepared.extend_from_slice(&backend_frame(b'n', b""));
        prepared.extend_from_slice(&backend_frame(b'Z', b"I"));
        let _ = stream.write_all(&prepared);
        let _ = stream.flush();

        expect_frontend_until_sync(&mut stream);
        let _ = stream.write_all(&response);
        let _ = stream.flush();

        // COMPLETE THE CONVERSATION, so the only thing wrong is `response`.
        // Without this the peer just falls silent, the client fails on the
        // truncation rather than on the violation, and the test passes even
        // when `response` is the CORRECT `CopyInResponse` -- measured, by
        // running exactly that as a control. The short read timeout keeps the
        // violation path fast: there the client has already errored and will
        // send nothing, so this drain is expected to time out.
        let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
        expect_frontend_until_sync(&mut stream);
        let mut completion = backend_frame(b'C', b"COPY 1\0");
        completion.extend_from_slice(&backend_frame(b'Z', b"I"));
        let _ = stream.write_all(&completion);
        let _ = stream.flush();
        thread::sleep(Duration::from_millis(300));
    });

    let (client, connection) = stub_config(server.addr)
        .connect(common::suite_tls())
        .await
        .expect("connect to scripted PostgreSQL peer");
    let driver = compio::runtime::spawn(async move { connection.run().await });

    let error = compio::time::timeout(OPERATION_WATCHDOG, async {
        let sink = client.copy_in::<_, Bytes>("COPY t FROM STDIN").await?;
        let mut sink = Box::pin(sink);
        sink.as_mut().send(Bytes::from_static(b"1\n")).await?;
        sink.as_mut().finish().await?;
        Ok::<(), compio_postgres::Error>(())
    })
    .await
    .expect("copy_in hung instead of rejecting a malformed COPY response")
    .expect_err("the driver accepted a malformed COPY IN response");

    let reuse = compio::time::timeout(OPERATION_WATCHDOG, client.simple_query("SELECT 2")).await;
    match reuse {
        Err(_) => panic!("reusing the poisoned session hung instead of failing"),
        Ok(Ok(_)) => panic!("the driver reused a session after a malformed COPY response"),
        Ok(Err(_)) => {}
    }

    let _ = compio::time::timeout(OPERATION_WATCHDOG, driver).await;
    drop(client);
    server.finish();
    common::error_chain(&error)
}

/// `CopyOutResponse` where the IN direction was requested -- the mirror of
/// [`a_copy_in_response_to_a_copy_out_request_is_refused`], and the more
/// dangerous half. A driver that ignored the direction here would hold a sink
/// the caller is about to write into while the server believes it is sending;
/// the caller's rows would go to a server that never entered copy-in mode.
#[compio::test]
async fn a_copy_out_response_to_a_copy_in_request_is_refused() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        // BindComplete, then the WRONG direction. Exactly the frames a correct
        // reply carries, with `H` where `G` belongs, so the direction is the
        // only variable. Sending a `ParseComplete` here as well -- which the
        // COPY OUT helper below does -- would be a second violation, and the
        // measured control showed it masks this one entirely: with it present
        // the test passed even when the direction was RIGHT.
        let mut response = backend_frame(b'2', b"");
        response.extend_from_slice(&backend_frame(b'H', b"\x00\x00\x00"));
        let chain = hostile_copy_in_retires_session(511, response).await;
        assert!(
            chain.contains("answered a COPY IN request with COPY OUT")
                && chain.contains("violated the")
                && chain.contains("copy_out"),
            "a wrong-direction COPY IN response did not name BOTH causes: {chain:?}"
        );
    })
    .await
    .expect("wrong-direction COPY IN test exceeded its outer watchdog");
}

/// The frames a HEALTHY server sends for `COPY t TO STDOUT`, after the prepare
/// batch has already been answered.
///
/// Every COPY OUT test below is this sequence with exactly one thing wrong, and
/// that is not cosmetic. Each of these tests used to send a `ParseComplete` the
/// COPY batch never asked for (it is `B E S`; measured by logging the frontend
/// tags) and then fall silent instead of finishing the conversation. Either one
/// is a violation in its own right, so the driver errored whatever else the
/// response said, and the tests passed WITH THE VIOLATION REMOVED -- verified by
/// substituting this function for the response and watching them keep passing.
///
/// Substituting this must FAIL each test. That is the check that separates a
/// hostile-peer test from a test of its own harness.
fn well_formed_copy_out() -> Vec<u8> {
    let mut response = backend_frame(b'2', b"");
    // CopyOutResponse: overall format byte, then a column count of zero.
    response.extend_from_slice(&backend_frame(b'H', b"\x00\x00\x00"));
    response.extend_from_slice(&backend_frame(b'd', b"row-one\n"));
    response.extend_from_slice(&backend_frame(b'c', b""));
    response.extend_from_slice(&backend_frame(b'C', b"COPY 1\0"));
    response.extend_from_slice(&backend_frame(b'Z', b"I"));
    response
}

/// THE CONTROL, RUN RATHER THAN REMEMBERED.
///
/// Until 2026-08-23 [`well_formed_copy_out`] was dead code: the substitution it
/// documents was a procedure someone was supposed to perform by hand, and the
/// only trace of it was a paragraph. A procedure nobody runs is a claim, not a
/// check - and the paragraph immediately above these tests said the control
/// "could not be built" and was "INCONCLUSIVE", which was already false by the
/// time it was written. Both failure modes have the same cure: run it.
///
/// This is the same peer, the same call, the same frames as the three tests
/// below, with nothing wrong. It must SUCCEED, and it must yield the bytes the
/// peer sent - not merely return `Ok`, because a driver that silently dropped
/// the payload would satisfy that. Every one of those three ends in an `Err`
/// only because of its single substituted violation.
#[compio::test]
async fn a_well_formed_copy_out_is_accepted() {
    use futures_util::TryStreamExt;

    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = copy_stub_server(520, well_formed_copy_out());

        let (client, connection) = stub_config(server.addr)
            .connect(common::suite_tls())
            .await
            .expect("connect to scripted PostgreSQL peer");
        let driver = compio::runtime::spawn(async move { connection.run().await });

        let collected = compio::time::timeout(OPERATION_WATCHDOG, async {
            let stream = client.copy_out("COPY t TO STDOUT").await?;
            assert_eq!(stream.format(), compio_postgres::CopyFormat::Text);
            assert!(stream.column_formats().is_empty());
            let mut stream = Box::pin(stream);
            let mut collected: Vec<u8> = Vec::new();
            while let Some(chunk) = stream.try_next().await? {
                collected.extend_from_slice(&chunk);
            }
            Ok::<Vec<u8>, compio_postgres::Error>(collected)
        })
        .await
        .expect("a well-formed COPY OUT hung")
        .expect(
            "the driver rejected a well-formed COPY OUT, so the hostile COPY tests below \
                 cannot be attributing their errors to the violation they substitute",
        );

        assert_eq!(
            collected, b"row-one\n",
            "the driver accepted the COPY but did not deliver what the peer sent"
        );

        let _ = compio::time::timeout(OPERATION_WATCHDOG, driver).await;
        drop(client);
        server.finish();
    })
    .await
    .expect("well-formed COPY OUT control exceeded its outer watchdog");
}

/// `CopyInResponse` where the OUT direction was requested. Both are real
/// messages; only the direction is wrong, and a driver that ignored the
/// distinction would wait for input on a stream the caller means to read.
#[compio::test]
async fn a_copy_in_response_to_a_copy_out_request_is_refused() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        // `G` where `H` belongs; the rest of the conversation is intact.
        let mut response = backend_frame(b'2', b"");
        response.extend_from_slice(&backend_frame(b'G', b"\x00\x00\x00"));
        response.extend_from_slice(&backend_frame(b'c', b""));
        response.extend_from_slice(&backend_frame(b'C', b"COPY 1\0"));
        response.extend_from_slice(&backend_frame(b'Z', b"I"));
        let chain = hostile_copy_out_retires_session(501, response).await;
        assert!(
            chain.contains("unexpected message from server"),
            "a wrong-direction COPY response reported {chain:?} rather than an out-of-order message"
        );
    })
    .await
    .expect("wrong-direction COPY test exceeded its outer watchdog");
}

/// A COPY that is established correctly and then delivers a `DataRow` mid
/// stream. This is the site at `copy_out.rs:88`, the one where resynchronising
/// would hand the caller unparsed bytes as if they were copy data.
#[compio::test]
async fn a_non_copy_data_message_mid_stream_is_refused() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        // A DataRow spliced into an otherwise complete stream, between the
        // legitimate CopyData and the CopyDone that ends it.
        let mut response = backend_frame(b'2', b"");
        response.extend_from_slice(&backend_frame(b'H', b"\x00\x00\x00"));
        response.extend_from_slice(&backend_frame(b'd', b"row-one\n"));
        let mut data_row = Vec::new();
        data_row.extend_from_slice(&1u16.to_be_bytes());
        data_row.extend_from_slice(&1u32.to_be_bytes());
        data_row.push(b'x');
        response.extend_from_slice(&backend_frame(b'D', &data_row));
        response.extend_from_slice(&backend_frame(b'c', b""));
        response.extend_from_slice(&backend_frame(b'C', b"COPY 1\0"));
        response.extend_from_slice(&backend_frame(b'Z', b"I"));
        let chain = hostile_copy_out_retires_session(502, response).await;
        assert!(
            chain.contains("unexpected message from server"),
            "a mid-stream non-CopyData message reported {chain:?} rather than an out-of-order \
             message"
        );
    })
    .await
    .expect("mid-stream COPY test exceeded its outer watchdog");
}

/// `BindComplete` where `ParseComplete` is owed, inside the COPY path rather
/// than the plain prepare path - the same violation one layer along.
#[compio::test]
async fn a_copy_out_whose_copy_out_response_never_arrives_is_refused() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let mut response = backend_frame(b'2', b"");
        response.extend_from_slice(&backend_frame(b'C', b"COPY 0\0"));
        response.extend_from_slice(&backend_frame(b'Z', b"I"));
        let chain = hostile_copy_out_retires_session(503, response).await;
        assert!(
            chain.contains("unexpected message from server"),
            "a COPY OUT without its CopyOutResponse reported {chain:?} rather than an \
             out-of-order message"
        );
    })
    .await
    .expect("missing-CopyOutResponse test exceeded its outer watchdog");
}

// ---------------------------------------------------------------------------
// Binary COPY OUT framing.
//
// `BinaryCopyOutStream` parses ONE tuple out of each chunk `CopyOutStream`
// hands it and then throws the rest of that chunk away. That is sound only
// because of a guarantee the PEER makes: the PostgreSQL protocol's COPY
// Operations section says the backend sends "zero or more CopyData messages
// (always one per row)" in copy-out mode. The reverse direction is explicitly
// NOT bound that way -- "the message boundaries are not required to have
// anything to do with row boundaries" -- so the assumption is one-directional
// and rests entirely on the peer conforming.
//
// A peer that does not is the case below. It is the quiet member of this file:
// nothing is malformed, no length lies, every tuple parses. The rows simply do
// not arrive, and a short answer with no error is the one failure a caller has
// no way to detect.
// ---------------------------------------------------------------------------

/// A 19-byte binary-COPY file header followed by `tuples`, as one `CopyData`
/// body. PostgreSQL merges the header into the first row's message, so a
/// single tuple here is exactly what a conforming backend sends first.
fn binary_copy_chunk(tuples: &[&[u8]]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(b"PGCOPY\n\xff\r\n\0");
    body.extend_from_slice(&0i32.to_be_bytes()); // flags
    body.extend_from_slice(&0u32.to_be_bytes()); // header extension length
    for tuple in tuples {
        body.extend_from_slice(tuple);
    }
    backend_frame(b'd', &body)
}

/// One tuple carrying a single non-NULL `int4`.
fn binary_int4_tuple(value: i32) -> Vec<u8> {
    let mut tuple = Vec::new();
    tuple.extend_from_slice(&1i16.to_be_bytes()); // field count
    tuple.extend_from_slice(&4i32.to_be_bytes()); // field length
    tuple.extend_from_slice(&value.to_be_bytes());
    tuple
}

/// The frames around the data: BindComplete and a one-column binary
/// `CopyOutResponse` before, the -1 trailer and the wrap-up after.
fn binary_copy_out_frames(data: Vec<Vec<u8>>) -> Vec<u8> {
    binary_copy_out_frames_with_trailer(data, &(-1i16).to_be_bytes())
}

fn binary_copy_out_frames_with_trailer(data: Vec<Vec<u8>>, trailer: &[u8]) -> Vec<u8> {
    binary_copy_out_frames_with_suffix(data, trailer, &[])
}

fn binary_copy_out_frames_with_suffix(
    data: Vec<Vec<u8>>,
    trailer: &[u8],
    suffix: &[u8],
) -> Vec<u8> {
    let mut response = backend_frame(b'2', b"");
    // CopyOutResponse: overall format 1 (binary), one column, that column
    // binary too.
    response.extend_from_slice(&backend_frame(b'H', b"\x01\x00\x01\x00\x01"));
    for chunk in data {
        response.extend_from_slice(&chunk);
    }
    response.extend_from_slice(&backend_frame(b'd', trailer));
    response.extend_from_slice(suffix);
    response.extend_from_slice(&backend_frame(b'c', b""));
    response.extend_from_slice(&backend_frame(b'C', b"COPY 2\0"));
    response.extend_from_slice(&backend_frame(b'Z', b"I"));
    response
}

/// Drain a binary COPY OUT of `int4` against `response` and report what came
/// back.
async fn binary_copy_out_against(
    process_id: i32,
    response: Vec<u8>,
) -> Result<Vec<i32>, compio_postgres::Error> {
    use compio_postgres::binary_copy::BinaryCopyOutStream;
    use compio_postgres::types::Type;
    use futures_util::TryStreamExt;

    let server = copy_stub_server(process_id, response);
    let (client, connection) = stub_config(server.addr)
        .connect(common::suite_tls())
        .await
        .expect("connect to scripted PostgreSQL peer");
    let driver = compio::runtime::spawn(async move { connection.run().await });

    let collected = compio::time::timeout(OPERATION_WATCHDOG, async {
        let stream = client.copy_out("COPY t TO STDOUT BINARY").await?;
        assert_eq!(stream.format(), compio_postgres::CopyFormat::Binary);
        assert_eq!(
            stream.column_formats(),
            &[compio_postgres::CopyFormat::Binary]
        );
        let mut rows = Box::pin(BinaryCopyOutStream::new(stream, &[Type::INT4]));
        let mut values = Vec::new();
        while let Some(row) = rows.try_next().await? {
            values.push(row.try_get::<i32>(0)?);
        }
        Ok::<Vec<i32>, compio_postgres::Error>(values)
    })
    .await
    .expect("binary COPY OUT hung");

    let _ = compio::time::timeout(OPERATION_WATCHDOG, driver).await;
    drop(client);
    server.finish();
    collected
}

/// THE CONTROL, and it runs first for a reason: it is the same two tuples, the
/// same values, the same everything, framed the way a conforming backend frames
/// them -- one `CopyData` per row. It must yield BOTH.
///
/// Without it the test below could be satisfied by a driver that refused binary
/// COPY OUT outright.
#[compio::test]
async fn two_binary_tuples_in_two_messages_are_both_delivered() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let response = binary_copy_out_frames(vec![
            binary_copy_chunk(&[&binary_int4_tuple(7)]),
            backend_frame(b'd', &binary_int4_tuple(9)),
        ]);
        let values = binary_copy_out_against(530, response)
            .await
            .expect("a conforming binary COPY OUT was rejected");
        assert_eq!(
            values,
            vec![7, 9],
            "the driver lost a tuple from a correctly framed binary COPY OUT"
        );
    })
    .await
    .expect("binary COPY OUT control exceeded its outer watchdog");
}

/// THE ONE VARIABLE: both tuples in ONE `CopyData`.
///
/// Every byte is identical to the control; only the message boundary moved.
/// The driver parses the first tuple and the second is still sitting in the
/// chunk when the row is returned -- the next poll reads the NEXT message, so
/// those bytes are gone. The caller gets `[7]` and no error at all.
///
/// Refusing is the answer rather than parsing on, because a peer free to pack
/// two tuples into a message is equally free to split one ACROSS messages, and
/// no amount of parsing within a chunk recovers that. What the caller needs is
/// to be told the framing is not what this parser requires.
#[compio::test]
async fn two_binary_tuples_in_one_message_are_refused_rather_than_silently_halved() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let response = binary_copy_out_frames(vec![binary_copy_chunk(&[
            &binary_int4_tuple(7),
            &binary_int4_tuple(9),
        ])]);
        let outcome = binary_copy_out_against(531, response).await;
        let error = match outcome {
            Ok(values) => panic!(
                "the driver returned {values:?} for a chunk carrying TWO tuples, dropping the \
                 rest of the message with no error"
            ),
            Err(error) => error,
        };
        let chain = common::error_chain(&error);
        assert!(
            chain.contains("trailing bytes"),
            "a coalesced binary COPY chunk reported {chain:?} rather than naming the leftover \
             bytes"
        );
    })
    .await
    .expect("coalesced binary COPY test exceeded its outer watchdog");
}

/// The binary trailer is a complete tuple-count field of -1, not a prefix
/// after which arbitrary bytes can be ignored. PostgreSQL's own reader probes
/// once beyond it and reports "received copy data after EOF marker" when data
/// remains. Silently accepting that suffix would let a peer append a tuple or
/// corrupt bytes while presenting the caller with a clean end of stream.
#[compio::test]
async fn bytes_after_the_binary_copy_trailer_are_refused() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let mut trailer = (-1i16).to_be_bytes().to_vec();
        trailer.push(0);
        let response = binary_copy_out_frames_with_trailer(
            vec![binary_copy_chunk(&[&binary_int4_tuple(7)])],
            &trailer,
        );
        let outcome = binary_copy_out_against(532, response).await;
        let error = match outcome {
            Ok(values) => {
                panic!("the driver returned {values:?} after a binary trailer carrying extra bytes")
            }
            Err(error) => error,
        };
        let chain = common::error_chain(&error);
        assert!(
            chain.contains("trailing bytes after the binary COPY trailer"),
            "binary trailer garbage reported {chain:?} instead of naming the trailer suffix"
        );
    })
    .await
    .expect("binary trailer suffix test exceeded its outer watchdog");
}

/// The EOF rule spans `CopyData` boundaries. Returning `None` as soon as the
/// trailer's own frame ends lets a later data frame disappear into the raw
/// stream's drop-time drain, so exercise the transition separately from the
/// same-frame suffix above.
#[compio::test]
async fn copy_data_after_the_binary_copy_trailer_is_refused() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let suffix = backend_frame(b'd', &[0]);
        let response = binary_copy_out_frames_with_suffix(
            vec![binary_copy_chunk(&[&binary_int4_tuple(7)])],
            &(-1i16).to_be_bytes(),
            &suffix,
        );
        let outcome = binary_copy_out_against(533, response).await;
        let error = match outcome {
            Ok(values) => {
                panic!("the driver returned {values:?} after CopyData followed the binary trailer")
            }
            Err(error) => error,
        };
        let chain = common::error_chain(&error);
        assert!(
            chain.contains("CopyData after the binary COPY trailer"),
            "cross-frame binary trailer garbage was not refused by name: {chain}"
        );
    })
    .await
    .expect("cross-frame binary trailer test exceeded its outer watchdog");
}
