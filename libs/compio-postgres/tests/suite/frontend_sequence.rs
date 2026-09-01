//! What this driver PUTS ON THE WIRE, message by message.
//!
//! Two campaigns have compared values - this driver against tokio-postgres,
//! and this driver against `PostgreSQL`'s own binary COPY bytes. Neither can see
//! the shape of the conversation. A value encoded perfectly can still travel
//! inside a wrong sequence: an extra Sync ends an implicit transaction early,
//! a Sync where Flush belongs forces a needless round trip, and a Describe the
//! caller never asked for is wasted work. All of that is invisible to a test
//! that only inspects results.
//!
//! Across the whole suite exactly two places assert a frontend tag sequence,
//! both inside `read_timeout.rs` and both incidental scaffolding for a
//! deadline test rather than the point of it. This module makes the sequence
//! itself the artifact.
//!
//! The peer here is a plain scripted socket, so these tests state what the
//! driver SENDS. They deliberately do not judge whether the server likes it -
//! that is what the live suite is for.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

#[allow(unused_imports)]
use crate::common;

const SOCKET_WATCHDOG: Duration = Duration::from_secs(2);
const THREAD_WATCHDOG: Duration = Duration::from_secs(3);
const ASYNC_WATCHDOG: Duration = Duration::from_secs(5);

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

fn complete_startup(stream: &mut TcpStream, process_id: i32) {
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

fn read_frontend_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut tag = [0u8; 1];
    stream.read_exact(&mut tag).expect("read frontend tag");
    let mut length = [0u8; 4];
    stream
        .read_exact(&mut length)
        .expect("read frontend frame length");
    let length = u32::from_be_bytes(length) as usize;
    assert!(length >= 4, "frontend frame length is below its header");
    assert!(length <= 1024 * 1024, "frontend frame is implausibly large");
    let mut body = vec![0u8; length - 4];
    stream
        .read_exact(&mut body)
        .expect("read frontend frame body");
    (tag[0], body)
}

/// One `int4` column named `n`, in text format.
fn one_int4_row_description() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&1u16.to_be_bytes());
    body.extend_from_slice(b"n\0");
    body.extend_from_slice(&0u32.to_be_bytes()); // table oid
    body.extend_from_slice(&0u16.to_be_bytes()); // column id
    body.extend_from_slice(&23u32.to_be_bytes()); // int4
    body.extend_from_slice(&4i16.to_be_bytes());
    body.extend_from_slice(&(-1i32).to_be_bytes());
    body.extend_from_slice(&0u16.to_be_bytes()); // text format
    backend_frame(b'T', &body)
}

fn one_int4_data_row(text: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&1u16.to_be_bytes());
    body.extend_from_slice(&u32::try_from(text.len()).unwrap().to_be_bytes());
    body.extend_from_slice(text);
    backend_frame(b'D', &body)
}

fn command_complete(tag: &[u8]) -> Vec<u8> {
    let mut body = tag.to_vec();
    body.push(0);
    backend_frame(b'C', &body)
}

fn ready_for_query() -> Vec<u8> {
    backend_frame(b'Z', b"I")
}

fn stub_url(addr: SocketAddr) -> String {
    format!(
        "postgres://postgres@{}:{}/scripted?sslmode=disable",
        addr.ip(),
        addr.port()
    )
}

async fn connect_scripted(addr: SocketAddr) -> compio_postgres::Client {
    let (client, connection) = compio_postgres::connect(&stub_url(addr), compio_postgres::NoTls)
        .await
        .expect("connect to the scripted peer");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}

/// Serve a generic extended-query conversation, recording every frontend tag.
///
/// Responses are buffered and flushed on `Sync`, which is what a real backend
/// does: the frontend is entitled to pipeline `P B D E` and only then wait.
/// A responder that answered each frame immediately would let a driver that
/// serialises round trips look identical to one that pipelines, and the whole
/// point of this module is to tell those apart.
///
/// Returns when the client disconnects.
fn serve_recording(stream: &mut TcpStream, tags: &std::sync::mpsc::Sender<(u8, Vec<u8>)>) {
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let mut tag = [0u8; 1];
        if stream.read_exact(&mut tag).is_err() {
            return; // client hung up; nothing further to record
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
        let tag = tag[0];
        let _ = tags.send((tag, body.clone()));

        match tag {
            b'Q' => {
                pending.extend_from_slice(&command_complete(b"BEGIN"));
                pending.extend_from_slice(&ready_for_query());
            }
            b'P' => pending.extend_from_slice(&backend_frame(b'1', b"")),
            b'B' => pending.extend_from_slice(&backend_frame(b'2', b"")),
            // Describe on a statement answers ParameterDescription + NoData;
            // these fixtures never select rows through the extended path.
            b'D' => {
                let mut parameters = Vec::new();
                parameters.extend_from_slice(&0u16.to_be_bytes());
                pending.extend_from_slice(&backend_frame(b't', &parameters));
                pending.extend_from_slice(&backend_frame(b'n', b""));
            }
            b'E' => pending.extend_from_slice(&command_complete(b"BEGIN")),
            b'C' => pending.extend_from_slice(&backend_frame(b'3', b"")),
            b'X' => return,
            _ => {}
        }

        if tag == b'Q' || tag == b'S' {
            if tag == b'S' {
                pending.extend_from_slice(&ready_for_query());
            }
            if stream.write_all(&pending).is_err() {
                return;
            }
            let _ = stream.flush();
            pending.clear();
        }
    }
}

/// The simple-query protocol is exactly one frame.
///
/// `simple_query` must not reach for Parse or Bind: the whole point of `Q` is
/// that it carries the SQL itself. A driver that quietly routed it through the
/// extended protocol would still return the right rows, so only the tag
/// sequence can tell.
#[compio::test]
async fn a_simple_query_sends_one_query_frame_and_nothing_else() {
    let (tags_tx, tags_rx) = std::sync::mpsc::channel();
    let server = StubServer::spawn(move |listener| {
        let mut stream = accept_bounded(&listener);
        complete_startup(&mut stream, 601);

        let (tag, body) = read_frontend_frame(&mut stream);
        let _ = tags_tx.send(tag);
        assert_eq!(
            &body[..body.len() - 1],
            b"SELECT 1",
            "the Query frame did not carry the SQL verbatim"
        );

        let mut response = one_int4_row_description();
        response.extend_from_slice(&one_int4_data_row(b"1"));
        response.extend_from_slice(&command_complete(b"SELECT 1"));
        response.extend_from_slice(&ready_for_query());
        stream.write_all(&response).expect("write query response");
        stream.flush().expect("flush query response");

        // Whatever the client sends on drop is not part of the claim.
        let mut rest = Vec::new();
        let _ = stream.read_to_end(&mut rest);
    });

    let addr = server.addr;
    compio::time::timeout(ASYNC_WATCHDOG, async move {
        let client = connect_scripted(addr).await;
        client
            .simple_query("SELECT 1")
            .await
            .expect("scripted simple query");
        drop(client);
    })
    .await
    .expect("the scripted simple query exceeded its watchdog");
    server.finish();

    let tags: Vec<u8> = tags_rx.try_iter().collect();
    assert_eq!(
        tags,
        vec![b'Q'],
        "simple_query sent {:?}, not a single Query frame",
        tags.iter().map(|t| *t as char).collect::<Vec<_>>()
    );
}

/// The extended-query sequence for a first-time `query`, pinned.
///
/// MEASURED, not assumed: the driver prepares and executes in two
/// Sync-terminated round trips rather than one. Each `S` is a round trip and
/// ends any implicit transaction block, so this shape is a real behavioural
/// commitment - if it ever collapses to a single `P B D E S`, error recovery
/// between the two halves changes and this test is the thing that says so.
#[compio::test]
async fn a_prepared_query_parses_and_executes_in_two_sync_round_trips() {
    let (tags_tx, tags_rx) = std::sync::mpsc::channel();
    let server = StubServer::spawn(move |listener| {
        let mut stream = accept_bounded(&listener);
        complete_startup(&mut stream, 602);

        // First round trip: everything up to and including the first Sync.
        loop {
            let (tag, _) = read_frontend_frame(&mut stream);
            let _ = tags_tx.send(tag);
            if tag == b'S' {
                break;
            }
        }
        let mut response = backend_frame(b'1', b""); // ParseComplete
        let mut parameter_description = Vec::new();
        parameter_description.extend_from_slice(&0u16.to_be_bytes());
        response.extend_from_slice(&backend_frame(b't', &parameter_description));
        response.extend_from_slice(&one_int4_row_description());
        response.extend_from_slice(&ready_for_query());
        stream.write_all(&response).expect("write prepare response");
        stream.flush().expect("flush prepare response");

        // Second round trip: bind, execute, sync.
        loop {
            let (tag, _) = read_frontend_frame(&mut stream);
            let _ = tags_tx.send(tag);
            if tag == b'S' {
                break;
            }
        }
        let mut response = backend_frame(b'2', b""); // BindComplete
        response.extend_from_slice(&one_int4_data_row(b"1"));
        response.extend_from_slice(&command_complete(b"SELECT 1"));
        response.extend_from_slice(&ready_for_query());
        stream.write_all(&response).expect("write execute response");
        stream.flush().expect("flush execute response");

        let mut rest = Vec::new();
        let _ = stream.read_to_end(&mut rest);
    });

    let addr = server.addr;
    compio::time::timeout(ASYNC_WATCHDOG, async move {
        let client = connect_scripted(addr).await;
        let rows = client
            .query("SELECT 1", &[])
            .await
            .expect("scripted prepared query");
        assert_eq!(rows.len(), 1, "the scripted peer returned one row");
        drop(client);
    })
    .await
    .expect("the scripted prepared query exceeded its watchdog");
    server.finish();

    let tags: String = tags_rx.try_iter().map(|t| t as char).collect();
    assert_eq!(
        tags, "PDSBES",
        "the extended-query sequence changed shape (got {tags})"
    );
}

/// Transaction control travels on the SIMPLE query protocol, one frame each.
///
/// MEASURED, not assumed, and the measurement corrected a wrong guess: the
/// statement is `START TRANSACTION`, not `BEGIN`. Both are legal and mean the
/// same thing to `PostgreSQL`, which is exactly why nothing else notices - the
/// only other place that spelling is written down is a log-matching constant
/// in `transaction_claims.rs`. Each is a single `Q`. That is
/// the right shape - neither takes a parameter, so preparing them would buy
/// nothing and cost a Parse plus a second round trip - but nothing else in the
/// suite says so, and a change to the extended path would still pass every
/// value-level test. The SQL text is asserted too, because a `Q` carrying the
/// wrong statement is exactly as wrong as the wrong tag.
#[compio::test]
async fn begin_and_commit_are_one_simple_query_frame_each() {
    assert_eq!(
        Box::pin(transaction_frames(621, true)).await,
        vec![
            ("Q".to_owned(), "START TRANSACTION".to_owned()),
            ("Q".to_owned(), "COMMIT".to_owned())
        ],
    );
}

/// The rollback arm, differing from the commit arm in exactly one call.
#[compio::test]
async fn begin_and_rollback_are_one_simple_query_frame_each() {
    assert_eq!(
        Box::pin(transaction_frames(622, false)).await,
        vec![
            ("Q".to_owned(), "START TRANSACTION".to_owned()),
            ("Q".to_owned(), "ROLLBACK".to_owned())
        ],
    );
}

/// Drive one transaction against a recording peer and return `(tag, sql)` per
/// frontend frame.
async fn transaction_frames(process_id: i32, commit: bool) -> Vec<(String, String)> {
    let (tags_tx, tags_rx) = std::sync::mpsc::channel();
    let server = StubServer::spawn(move |listener| {
        let mut stream = accept_bounded(&listener);
        complete_startup(&mut stream, process_id);
        serve_recording(&mut stream, &tags_tx);
    });
    let addr = server.addr;
    compio::time::timeout(ASYNC_WATCHDOG, async move {
        let mut client = connect_scripted(addr).await;
        let transaction = client.transaction().await.expect("begin the transaction");
        if commit {
            transaction.commit().await.expect("commit");
        } else {
            transaction.rollback().await.expect("rollback");
        }
        drop(client);
    })
    .await
    .expect("the scripted transaction exceeded its watchdog");
    server.finish();

    tags_rx
        .try_iter()
        .filter(|(tag, _)| *tag != b'X')
        .map(|(tag, body)| {
            let sql =
                String::from_utf8_lossy(body.strip_suffix(&[0]).unwrap_or(&body)).into_owned();
            ((tag as char).to_string(), sql)
        })
        .collect()
}
