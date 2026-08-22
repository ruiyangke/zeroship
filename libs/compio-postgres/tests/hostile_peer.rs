//! What the driver does when the peer violates the protocol.
//!
//! `src/` carries 44 `Error::unexpected_message()` sites across nine files -
//! every one a place the driver says "a PostgreSQL server cannot have sent that
//! here". Before this file nothing drove any of them: a grep of `tests/` for a
//! bad length, an unknown tag, or `unexpected_message` returned nothing. The
//! oversize case is the exception and is already covered elsewhere against
//! `MAX_MESSAGE_SIZE`, so nothing here re-tests it.
//!
//! The peer is on a network. A mangling proxy, a transaction pooler that
//! rewrites framing, or an outright hostile server can put bytes on the wire
//! that PostgreSQL never would. Each test below asserts THREE things, and the
//! middle one is the point:
//!
//!   1. the operation returns an `Err` rather than a wrong-but-plausible `Ok`,
//!   2. the session is RETIRED rather than handed to the next caller,
//!   3. it happened inside a watchdog, because several of these shapes are
//!      built to hang an implementation that waits for bytes that never come.
//!
//! Assertion 2 is what a careless version of this file would omit. A driver
//! that reports an error and then leaves the poisoned session usable has the
//! defect this crate has had SIX separate confirmed times.
//!
//! WHAT THESE ACTUALLY REACH, measured rather than assumed. The unknown-tag
//! case reports `error parsing response from server: unknown message tag
//! \`127\``, so it is rejected by the CODEC while decoding the frame - it never
//! reaches an `unexpected_message` site at all. Read this file as coverage of
//! the framing and decode defences, NOT of those 44 state-machine branches;
//! driving those needs a peer that sends well-formed messages in a forbidden
//! ORDER, which only `a_data_row_without_a_row_description_is_refused` does.
//!
//! These tests discriminate, and that was checked rather than hoped: feeding a
//! WELL-FORMED `CommandComplete` + `ReadyForQuery` through the same helper
//! fails with "the driver accepted a malformed frame as a valid response:
//! [CommandComplete(1)]". A version of this file that passed no matter what the
//! peer sent would be worse than nothing, because it would read as coverage.
//!
//! NOT covered here: TLS, authentication, replication framing, COPY
//! sub-protocol violations, or a peer that trickles bytes slowly rather than
//! sending wrong ones.

use compio_postgres::config::SslMode;
use compio_postgres::{Config, NoTls};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

#[allow(dead_code)]
mod common;

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

fn complete_startup(stream: &mut TcpStream, process_id: i32) {
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
    assert_eq!(&body[..4], &[0, 3, 0, 0], "client did not request protocol 3");

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

fn expect_simple_query(stream: &mut TcpStream) -> Vec<u8> {
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

/// Run one simple query against a peer that answers it with `response`, and
/// report the error plus whether the session was retired.
///
/// Every caller goes through here so the three assertions cannot drift apart
/// between tests, and so "the query failed" and "the session was retired" are
/// always measured on the same connection.
async fn hostile_response_retires_session(process_id: i32, response: Vec<u8>) -> String {
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
        .connect(NoTls)
        .await
        .expect("connect to scripted PostgreSQL peer");
    let driver = compio::runtime::spawn(async move { connection.run().await });

    let error = compio::time::timeout(OPERATION_WATCHDOG, client.simple_query("SELECT 1"))
        .await
        .expect("the query hung instead of rejecting a malformed frame")
        .expect_err("the driver accepted a malformed frame as a valid response");

    // (2) The session must be retired, not returned to service. A follow-up on
    // the same client has to fail; if it succeeds the driver kept a connection
    // whose framing it has already lost track of.
    let reuse = compio::time::timeout(OPERATION_WATCHDOG, client.simple_query("SELECT 2")).await;
    match reuse {
        Err(_) => panic!("reusing the poisoned session hung instead of failing"),
        Ok(Ok(_)) => panic!("the driver reused a session after a malformed frame"),
        Ok(Err(_)) => {}
    }

    let _ = compio::time::timeout(OPERATION_WATCHDOG, driver).await;
    drop(client);
    server.finish();
    common::error_chain(&error)
}

/// A byte that is not any backend message type must be refused, not skipped.
///
/// Skipping it would be the dangerous outcome: the driver would resynchronise
/// on whatever followed and hand the caller rows from a frame it never parsed.
#[compio::test]
async fn an_unknown_message_tag_is_refused_and_retires_the_session() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let chain = hostile_response_retires_session(201, backend_frame(b'\x7f', b"nonsense")).await;
        assert!(
            !chain.is_empty(),
            "an unknown backend tag produced an error with no description"
        );
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
        let chain = hostile_response_retires_session(202, backend_frame(b'D', &body)).await;
        assert!(
            !chain.is_empty(),
            "an out-of-place DataRow produced an error with no description"
        );
    })
    .await
    .expect("out-of-place DataRow test exceeded its outer watchdog");
}

/// A length shorter than the payload leaves trailing bytes the driver will read
/// as the start of the next frame. It must not resynchronise onto them.
#[compio::test]
async fn a_length_shorter_than_its_payload_is_refused() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let chain =
            hostile_response_retires_session(203, lying_frame(b'C', b"SELECT 1\0", 6)).await;
        assert!(
            !chain.is_empty(),
            "an under-declared frame produced an error with no description"
        );
    })
    .await
    .expect("short-length test exceeded its outer watchdog");
}

/// A length below the 4-byte header itself is unrepresentable, not merely
/// wrong: there is no body length that satisfies it.
#[compio::test]
async fn a_length_below_its_own_header_is_refused() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let chain = hostile_response_retires_session(204, lying_frame(b'C', b"", 3)).await;
        assert!(
            !chain.is_empty(),
            "a sub-header frame length produced an error with no description"
        );
    })
    .await
    .expect("sub-header-length test exceeded its outer watchdog");
}

/// Half a frame followed by silence. The driver must give up on its own clock
/// rather than waiting for the rest forever - the failure here is a HANG, which
/// `OPERATION_WATCHDOG` converts into a test failure.
#[compio::test]
async fn a_truncated_frame_followed_by_silence_does_not_hang() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        // Declares 64 bytes of body and sends four.
        let chain = hostile_response_retires_session(205, lying_frame(b'D', b"abcd", 68)).await;
        assert!(
            !chain.is_empty(),
            "a truncated frame produced an error with no description"
        );
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
        .connect(NoTls)
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

/// `BindComplete` is a real message, correctly framed - but Parse was what was
/// owed. Accepting it would leave the driver believing a statement is prepared
/// that the server never parsed.
#[compio::test]
async fn a_bind_complete_where_parse_complete_is_owed_is_refused() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let chain = hostile_prepare_retires_session(301, backend_frame(b'2', b"")).await;
        assert!(
            !chain.is_empty(),
            "a misplaced BindComplete produced an error with no description"
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
            !chain.is_empty(),
            "a missing ParameterDescription produced an error with no description"
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
            !chain.is_empty(),
            "a misplaced message after ParameterDescription produced an error with no description"
        );
    })
    .await
    .expect("misplaced post-ParameterDescription test exceeded its outer watchdog");
}

// ---------------------------------------------------------------------------
// COPY sub-protocol violations.
//
// `src/copy_out.rs` and `src/copy_in.rs` hold 8 of the 44 refusal sites, and
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
// A LIMIT ON WHAT THESE THREE PROVE, stated because three green tests imply
// more than was established. The mid-stream case demonstrably REACHES its site:
// instrumenting it reports "unexpected message from server", which is
// `Error::unexpected_message()` at copy_out.rs:88. But the one-variable control
// used elsewhere in this file - feed the same helper a VALID stream and require
// the test to fail - could not be built here. A scripted CopyOutResponse plus
// CopyData, CopyDone, CommandComplete and ReadyForQuery still errors, so the
// fixture is not a valid COPY and the control is inconclusive rather than
// negative. The happy path IS covered, against a real server, by
// `copy_out_error_surfaces_and_recovers_the_same_connection` and the rest of the
// copy family in integration.rs. Treat these three as "the refusal path is
// reached", not as "the refusal is proven necessary".
// ---------------------------------------------------------------------------

/// Drive `copy_out` against a peer answering with `response`, requiring the same
/// three properties as the other helpers: an error, a retired session, bounded.
async fn hostile_copy_out_retires_session(process_id: i32, response: Vec<u8>) -> String {
    use futures_util::TryStreamExt;

    let server = StubServer::spawn(move |listener| {
        let mut stream = accept_bounded(&listener);
        complete_startup(&mut stream, process_id);
        expect_frontend_until_sync(&mut stream);
        let _ = stream.write_all(&response);
        let _ = stream.flush();
        thread::sleep(Duration::from_millis(300));
    });

    let (client, connection) = stub_config(server.addr)
        .connect(NoTls)
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

/// `CopyInResponse` where the OUT direction was requested. Both are real
/// messages; only the direction is wrong, and a driver that ignored the
/// distinction would wait for input on a stream the caller means to read.
#[compio::test]
async fn a_copy_in_response_to_a_copy_out_request_is_refused() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let mut response = backend_frame(b'1', b"");
        response.extend_from_slice(&backend_frame(b'2', b""));
        // CopyInResponse: overall format byte, then a column count of zero.
        response.extend_from_slice(&backend_frame(b'G', b"\x00\x00\x00"));
        let chain = hostile_copy_out_retires_session(501, response).await;
        assert!(
            !chain.is_empty(),
            "a wrong-direction COPY response produced an error with no description"
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
        let mut response = backend_frame(b'1', b"");
        response.extend_from_slice(&backend_frame(b'2', b""));
        // CopyOutResponse, then one legitimate CopyData, then a DataRow.
        response.extend_from_slice(&backend_frame(b'H', b"\x00\x00\x00"));
        response.extend_from_slice(&backend_frame(b'd', b"row-one\n"));
        let mut data_row = Vec::new();
        data_row.extend_from_slice(&1u16.to_be_bytes());
        data_row.extend_from_slice(&1u32.to_be_bytes());
        data_row.push(b'x');
        response.extend_from_slice(&backend_frame(b'D', &data_row));
        let chain = hostile_copy_out_retires_session(502, response).await;
        assert!(
            !chain.is_empty(),
            "a mid-stream non-CopyData message produced an error with no description"
        );
    })
    .await
    .expect("mid-stream COPY test exceeded its outer watchdog");
}

/// `BindComplete` where `ParseComplete` is owed, inside the COPY path rather
/// than the plain prepare path - the same violation one layer along.
#[compio::test]
async fn a_copy_out_missing_its_parse_complete_is_refused() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let chain = hostile_copy_out_retires_session(503, backend_frame(b'2', b"")).await;
        assert!(
            !chain.is_empty(),
            "a COPY missing ParseComplete produced an error with no description"
        );
    })
    .await
    .expect("COPY missing ParseComplete test exceeded its outer watchdog");
}
