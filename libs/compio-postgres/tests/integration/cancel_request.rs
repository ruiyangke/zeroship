//! Live PostgreSQL coverage for the out-of-band CancelRequest protocol.
//!
//! These are deliberately separate from tests which cancel by dropping a Rust
//! future. Every cancellation here opens a second connection and sends the
//! backend PID and secret key captured from the target connection's startup.

use bytes::Bytes;
use compio::buf::{BufResult, IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite};
use compio::net::{TcpListener, TcpStream};
use compio_postgres::config::{Host, ProtocolVersion};
use compio_postgres::error::SqlState;
use compio_postgres::{CancelToken, Client, Config, Error, NoTls, Pool};
use futures_util::{SinkExt, StreamExt};
use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;
use std::task::Poll;
use std::time::Duration;

#[allow(unused_imports)]
use crate::common;

const OPERATION_TIMEOUT: Duration = Duration::from_secs(5);

fn test_url() -> String {
    common::test_url()
}

fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut rendered = String::new();
    let mut link = Some(error);
    while let Some(current) = link {
        if !rendered.is_empty() {
            rendered.push_str(": ");
        }
        rendered.push_str(&current.to_string());
        if let Some(db) = current.downcast_ref::<compio_postgres::error::DbError>() {
            rendered.push_str(&format!(" (SQLSTATE {})", db.code().code()));
        }
        link = current.source();
    }
    rendered
}

fn plaintext_url() -> String {
    let url = test_url();
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}sslmode=disable")
}

async fn connect(url: &str) -> Result<Client, Error> {
    let (client, connection) = compio_postgres::connect(url, common::suite_tls()).await?;
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {}", error_chain(&error));
        }
    })
    .detach();
    Ok(client)
}

fn tcp_endpoint(url: &str) -> (String, u16) {
    let config: Config = url.parse().expect("parse PG_TEST_URL");
    let port = config.get_ports().first().copied().unwrap_or(5432);

    if let Some(Some(address)) = config.get_hostaddrs().first() {
        return (address.to_string(), port);
    }

    match config.get_hosts().first() {
        Some(Host::Tcp(host)) => (host.clone(), port),
        #[cfg(unix)]
        Some(Host::Unix(path)) => panic!(
            "the live cancel_query_raw test requires a TCP PG_TEST_URL, got {}",
            path.display()
        ),
        None => ("localhost".to_string(), port),
    }
}

async fn wait_until_pg_sleep_is_running(observer: &Client, pid: i32, marker: &str) {
    compio::time::timeout(OPERATION_TIMEOUT, async {
        loop {
            let running: bool = observer
                .query_one_scalar(
                    "SELECT EXISTS (\
                         SELECT 1 \
                         FROM pg_stat_activity \
                         WHERE pid = $1 \
                           AND state = 'active' \
                           AND wait_event_type = 'Timeout' \
                           AND wait_event = 'PgSleep' \
                           AND query LIKE '%' || $2 || '%'\
                     )",
                    &[&pid, &marker],
                )
                .await
                .expect("poll pg_stat_activity for the target query");
            if running {
                return;
            }
        }
    })
    .await
    .expect("target query never appeared as an executing pg_sleep in pg_stat_activity");
}

async fn wait_until_backend_is_gone(observer: &Client, pid: i32) {
    compio::time::timeout(OPERATION_TIMEOUT, async {
        loop {
            let exists: bool = observer
                .query_one_scalar(
                    "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1)",
                    &[&pid],
                )
                .await
                .expect("poll pg_stat_activity for the closed backend");
            if !exists {
                return;
            }
        }
    })
    .await
    .expect("target backend remained visible after its connection closed");
}

async fn wait_until_copy_progress(observer: &Client, pid: i32, expected: bool) {
    compio::time::timeout(OPERATION_TIMEOUT, async {
        loop {
            let present: bool = observer
                .query_one_scalar(
                    "SELECT EXISTS (SELECT 1 FROM pg_stat_progress_copy WHERE pid = $1)",
                    &[&pid],
                )
                .await
                .expect("inspect pg_stat_progress_copy");
            if present == expected {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "COPY progress for backend {pid} did not become {}",
            if expected { "visible" } else { "absent" }
        )
    });
}

async fn assert_client_still_works(client: &Client) {
    let answer: i32 = compio::time::timeout(
        OPERATION_TIMEOUT,
        client.query_one_scalar("SELECT 42::int4", &[]),
    )
    .await
    .expect("follow-up query timed out")
    .expect("follow-up query failed on the cancelled session");
    assert_eq!(answer, 42);
}

fn assert_query_canceled(result: Result<(), Error>) {
    let error = result.expect_err("pg_sleep unexpectedly completed instead of being cancelled");
    assert_eq!(
        error.code(),
        Some(&SqlState::QUERY_CANCELED),
        "expected 57014 query_canceled, got {}",
        error_chain(&error)
    );
}

struct RecordingStream<'a> {
    inner: &'a mut TcpStream,
    writes: Rc<RefCell<Vec<u8>>>,
}

impl AsyncRead for RecordingStream<'_> {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.inner.read(buf).await
    }
}

impl AsyncWrite for RecordingStream<'_> {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let BufResult(result, buf) = self.inner.write(buf).await;
        if let Ok(written) = result {
            self.writes
                .borrow_mut()
                .extend_from_slice(&buf.as_init()[..written]);
        }
        BufResult(result, buf)
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush().await
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        self.inner.shutdown().await
    }
}

async fn cancel_raw_and_wait_for_server_close(
    token: &CancelToken,
    endpoint: &(String, u16),
) -> Vec<u8> {
    let mut socket = TcpStream::connect((endpoint.0.as_str(), endpoint.1))
        .await
        .expect("open the caller-owned CancelRequest connection");
    let writes = Rc::new(RefCell::new(Vec::new()));
    token
        .cancel_query_raw(
            RecordingStream {
                inner: &mut socket,
                writes: Rc::clone(&writes),
            },
            NoTls,
        )
        .await
        .expect("send CancelRequest on the caller-owned connection");

    // CancelRequest has no response. EOF is the only server-side barrier: the
    // postmaster closes this connection after consuming the startup packet.
    let compio::BufResult(result, _) =
        compio::time::timeout(OPERATION_TIMEOUT, socket.read(vec![0; 1]))
            .await
            .expect("server did not close the CancelRequest connection");
    assert_eq!(
        result.expect("read CancelRequest connection EOF"),
        0,
        "PostgreSQL sent bytes in response to a fire-and-forget CancelRequest"
    );

    writes.borrow().clone()
}

/// Capture the token's real length-prefixed packet against a loopback peer.
/// Mutating that packet lets the wrong-key test vary exactly one byte while
/// preserving the 4-byte PG16 or 32-byte PG18 key length issued by the server.
async fn capture_cancel_request_packet(token: &CancelToken) -> Vec<u8> {
    use compio::io::AsyncReadExt;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind cancel capture peer");
    let address = listener.local_addr().expect("cancel capture peer address");
    let (packet_tx, packet_rx) = futures_channel::oneshot::channel();
    compio::runtime::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept captured cancel");
        let compio::BufResult(result, length) = socket.read_exact(vec![0u8; 4]).await;
        result.expect("read captured CancelRequest length");
        let length = u32::from_be_bytes(length.as_slice().try_into().unwrap()) as usize;
        assert!(length >= 16, "captured CancelRequest is too short");
        let compio::BufResult(result, body) = socket.read_exact(vec![0u8; length - 4]).await;
        result.expect("read captured CancelRequest body");

        let mut packet = (length as u32).to_be_bytes().to_vec();
        packet.extend_from_slice(&body);
        packet_tx
            .send(packet)
            .expect("report captured CancelRequest");
    })
    .detach();

    let socket = TcpStream::connect(address)
        .await
        .expect("connect cancel capture peer");
    token
        .cancel_query_raw(socket, NoTls)
        .await
        .expect("write captured CancelRequest");
    packet_rx.await.expect("cancel capture peer did not report")
}

async fn send_packet_and_wait_for_server_close(endpoint: &(String, u16), packet: Vec<u8>) {
    use compio::io::{AsyncWrite, AsyncWriteExt};

    let mut socket = TcpStream::connect((endpoint.0.as_str(), endpoint.1))
        .await
        .expect("open a CancelRequest connection");
    let compio::BufResult(result, _) = socket.write_all(packet).await;
    result.expect("write the hand-built CancelRequest");
    socket.flush().await.expect("flush the CancelRequest");

    // Same barrier the driver's confirmed cancel uses: the postmaster closes
    // this connection only after consuming the packet, so EOF means the server
    // has already decided what to do with it.
    let compio::BufResult(result, _) =
        compio::time::timeout(OPERATION_TIMEOUT, socket.read(vec![0; 1]))
            .await
            .expect("server did not close the CancelRequest connection");
    assert_eq!(
        result.expect("read CancelRequest connection EOF"),
        0,
        "PostgreSQL sent bytes in response to a CancelRequest"
    );
}

async fn pg_sleep_is_running(observer: &Client, pid: i32, marker: &str) -> bool {
    observer
        .query_one_scalar(
            "SELECT EXISTS (\
                 SELECT 1 \
                 FROM pg_stat_activity \
                 WHERE pid = $1 \
                   AND state = 'active' \
                   AND wait_event_type = 'Timeout' \
                   AND wait_event = 'PgSleep' \
                   AND query LIKE '%' || $2 || '%'\
             )",
            &[&pid, &marker],
        )
        .await
        .expect("inspect pg_stat_activity")
}

/// Bounds the hazard behind a long-lived [`CancelToken`]: the token names a
/// backend PID, PIDs are recycled, and nothing in the protocol reports that a
/// token has gone stale. What keeps a stale token from cancelling a stranger's
/// query is the secret key, so this measures whether the key is really checked
/// rather than assuming it.
///
/// One variable: the same connection, the same running statement, the same
/// delivery barrier, and only the key differs between the two arms. `pg_sleep`
/// still running after the wrong-key packet has been consumed is the negative;
/// 57014 after the token's own packet is the positive control that proves the
/// wrong-key arm was not simply a cancel that failed to arrive.
#[compio::test]
async fn a_cancel_request_with_the_wrong_secret_key_is_inert() {
    const MARKER: &str = "cpg_cancel_wrong_secret_key";
    const QUERY: &str = "SELECT pg_sleep(30) /* cpg_cancel_wrong_secret_key */";

    let url = plaintext_url();
    let endpoint = tcp_endpoint(&url);
    let client = connect(&url).await.unwrap();
    let observer = connect(&url).await.unwrap();
    let pid = client.process_id();
    let token = client.cancel_token();

    let cancel_task = compio::runtime::spawn(async move {
        wait_until_pg_sleep_is_running(&observer, pid, MARKER).await;

        // Arm one: the token's real packet, with exactly one key byte changed.
        // PID, framing and the server-issued key length remain identical.
        let mut wrong_key_packet = capture_cancel_request_packet(&token).await;
        wrong_key_packet[12] ^= 1;
        send_packet_and_wait_for_server_close(&endpoint, wrong_key_packet).await;
        assert!(
            pg_sleep_is_running(&observer, pid, MARKER).await,
            "PostgreSQL honoured a CancelRequest whose secret key did not match the backend; a \
             stale CancelToken landing on a recycled PID would cancel a stranger's query"
        );

        // Arm two: the same PID with the session's own key.
        token
            .cancel_query(common::suite_tls())
            .await
            .expect("send CancelRequest");
    });

    let (query_result, cancel_result) = compio::time::timeout(
        OPERATION_TIMEOUT,
        futures_util::future::join(client.batch_execute(QUERY), cancel_task),
    )
    .await
    .expect("CancelRequest did not interrupt pg_sleep before the test deadline");

    cancel_result.expect("CancelRequest task panicked or was cancelled");
    assert_query_canceled(query_result);
    assert_client_still_works(&client).await;
}

#[compio::test]
async fn running_query_cancel_returns_57014_and_preserves_session() {
    const MARKER: &str = "cpg_cancel_running_query";
    const QUERY: &str = "SELECT pg_sleep(30) /* cpg_cancel_running_query */";

    let url = plaintext_url();
    let client = connect(&url).await.unwrap();
    let observer = connect(&url).await.unwrap();
    let pid = client.process_id();
    let token = client.cancel_token();

    let cancel_task = compio::runtime::spawn(async move {
        wait_until_pg_sleep_is_running(&observer, pid, MARKER).await;
        token
            .cancel_query(common::suite_tls())
            .await
            .expect("send CancelRequest");
    });

    let (query_result, cancel_result) = compio::time::timeout(
        OPERATION_TIMEOUT,
        futures_util::future::join(client.batch_execute(QUERY), cancel_task),
    )
    .await
    .expect("CancelRequest did not interrupt pg_sleep before the test deadline");

    cancel_result.expect("CancelRequest task panicked or was cancelled");
    assert_query_canceled(query_result);
    assert_client_still_works(&client).await;
}

#[compio::test]
async fn pool_cancel_query_interrupts_a_running_query_and_preserves_the_session() {
    const MARKER: &str = "cpg_pool_cancel_running_query";
    const QUERY: &str = "SELECT pg_sleep(30) /* cpg_pool_cancel_running_query */";

    let url = plaintext_url();
    let pool = Pool::connect(&url, 1)
        .await
        .expect("connect one-slot cancellation pool");
    let client = Box::pin(pool.acquire())
        .await
        .expect("borrow the pool cancellation target");
    let observer = connect(&url).await.expect("connect cancellation observer");
    let pid = client.process_id();
    let token = client.cancel_token();

    let cancel = async {
        wait_until_pg_sleep_is_running(&observer, pid, MARKER).await;
        pool.cancel_query(&token).await
    };
    let (query_result, cancel_result) = compio::time::timeout(
        OPERATION_TIMEOUT,
        futures_util::future::join(client.batch_execute(QUERY), cancel),
    )
    .await
    .expect("pool cancellation did not finish before the test deadline");

    cancel_result.expect("the pool did not deliver its CancelRequest");
    assert_query_canceled(query_result);
    assert_client_still_works(&client).await;
}

#[cfg(feature = "suite-over-tls")]
#[compio::test]
async fn a_pool_can_cancel_tls_with_its_private_policy_lineage() {
    const MARKER: &str = "cpg_cancel_pool_tls_policy";
    const QUERY: &str = "SELECT pg_sleep(1) /* cpg_cancel_pool_tls_policy */";

    let url = test_url();
    let pool = Pool::connect(&url, 1)
        .await
        .expect("connect one-slot TLS pool");
    let client = pool.acquire().await.expect("borrow TLS pool connection");
    let observer = connect(&url).await.expect("connect cancellation observer");
    let pid = client.process_id();
    let token = client.cancel_token();

    let encrypted: bool = observer
        .query_one_scalar("SELECT ssl FROM pg_stat_ssl WHERE pid = $1", &[&pid])
        .await
        .expect("inspect the pooled connection's TLS transport");
    assert!(
        encrypted,
        "the pool cancellation test did not establish TLS"
    );

    let cancel = async {
        wait_until_pg_sleep_is_running(&observer, pid, MARKER).await;
        pool.cancel_query(&token).await
    };
    let (query_result, cancel_result) = compio::time::timeout(
        OPERATION_TIMEOUT,
        futures_util::future::join(client.batch_execute(QUERY), cancel),
    )
    .await
    .expect("pool-owned TLS cancellation did not finish before the test deadline");

    cancel_result.expect("the pool could not use its private TLS policy lineage");
    assert_query_canceled(query_result);
    assert_client_still_works(&client).await;
}

#[compio::test]
async fn cancel_twice_with_nothing_running_is_harmless() {
    let url = plaintext_url();
    let endpoint = tcp_endpoint(&url);
    let client = connect(&url).await.unwrap();
    let first_token = client.cancel_token();
    let second_token = first_token.clone();

    for (attempt, token) in [("first", first_token), ("second", second_token)] {
        let _packet = compio::time::timeout(
            OPERATION_TIMEOUT,
            cancel_raw_and_wait_for_server_close(&token, &endpoint),
        )
        .await
        .unwrap_or_else(|_| panic!("{attempt} idle CancelRequest timed out"));
    }

    assert_client_still_works(&client).await;
}

#[compio::test]
async fn two_cancels_for_one_running_query_leave_the_session_usable() {
    const MARKER: &str = "cpg_cancel_twice_running";
    const QUERY: &str = "SELECT pg_sleep(30) /* cpg_cancel_twice_running */";

    let url = plaintext_url();
    let client = connect(&url).await.unwrap();
    let observer = connect(&url).await.unwrap();
    let pid = client.process_id();
    let first = client.cancel_token();
    let second = first.clone();

    let cancel_task = compio::runtime::spawn(async move {
        wait_until_pg_sleep_is_running(&observer, pid, MARKER).await;
        futures_util::future::join(
            first.cancel_query(common::suite_tls()),
            second.cancel_query(common::suite_tls()),
        )
        .await
    });
    let (query_result, cancels) = compio::time::timeout(
        OPERATION_TIMEOUT,
        futures_util::future::join(client.batch_execute(QUERY), cancel_task),
    )
    .await
    .expect("two CancelRequests did not resolve before the test deadline");

    let (first_result, second_result) = cancels.expect("double-cancel task panicked");
    first_result.expect("first CancelRequest was not consumed");
    second_result.expect("second CancelRequest was not consumed");
    assert_query_canceled(query_result);
    assert_client_still_works(&client).await;
}

#[compio::test]
async fn cancel_after_query_finished_is_harmless() {
    let url = plaintext_url();
    let endpoint = tcp_endpoint(&url);
    let client = connect(&url).await.unwrap();
    let token = client.cancel_token();

    let finished: i32 = client
        .query_one_scalar("SELECT 7::int4", &[])
        .await
        .expect("finish the query before cancelling");
    assert_eq!(finished, 7);

    let _packet = compio::time::timeout(
        OPERATION_TIMEOUT,
        cancel_raw_and_wait_for_server_close(&token, &endpoint),
    )
    .await
    .expect("post-completion CancelRequest timed out");

    assert_client_still_works(&client).await;
}

#[compio::test]
async fn stale_cancel_token_completes_cleanly_without_hanging() {
    let url = plaintext_url();
    let observer = connect(&url).await.unwrap();
    let (client, connection) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap();
    let pid = client.process_id();
    let token = client.cancel_token();
    let driver = compio::runtime::spawn(async move { connection.run().await });

    drop(client);
    let driver_result = compio::time::timeout(OPERATION_TIMEOUT, driver)
        .await
        .expect("target connection driver did not stop after client drop")
        .expect("target connection driver task panicked");
    driver_result.expect("target connection did not close cleanly");
    wait_until_backend_is_gone(&observer, pid).await;

    // PostgreSQL sends no result for CancelRequest, so a reachable postmaster
    // cannot report that the PID/key pair is stale. Ok means the packet was
    // sent cleanly; it does not claim that a query was cancelled.
    compio::time::timeout(OPERATION_TIMEOUT, token.cancel_query(common::suite_tls()))
        .await
        .expect("stale CancelToken hung")
        .expect("stale CancelToken could not send its fire-and-forget packet");
}

#[compio::test]
async fn pool_cancel_query_refuses_a_token_from_a_returned_lease() {
    const MARKER: &str = "cpg_cancel_stale_pool_lease";
    const QUERY: &str = "SELECT pg_sleep(1) /* cpg_cancel_stale_pool_lease */";

    let url = plaintext_url();
    let pool = Pool::connect(&url, 1).await.expect("connect one-slot pool");
    let observer = connect(&url).await.unwrap();

    let first = Box::pin(pool.acquire()).await.expect("borrow first pool lease");
    let token = first.cancel_token();
    drop(first);

    let second = Box::pin(pool.acquire())
        .await
        .expect("borrow second pool lease");
    let second_pid = second.process_id();
    let cancel = async {
        wait_until_pg_sleep_is_running(&observer, second_pid, MARKER).await;
        pool.cancel_query(&token).await
    };

    let (query_result, cancel_result) = compio::time::timeout(
        OPERATION_TIMEOUT,
        futures_util::future::join(second.batch_execute(QUERY), cancel),
    )
    .await
    .expect("stale pool-token race did not finish before the test deadline");
    let cancel_error =
        cancel_result.expect_err("a token from the returned lease retained cancellation authority");

    assert!(
        error_chain(&cancel_error).contains("pool lease has ended"),
        "stale token refusal did not name the ended pool lease: {}",
        error_chain(&cancel_error)
    );
    query_result.expect("the stale token cancelled the next pool borrower's query");
    assert_client_still_works(&second).await;
}

#[compio::test]
async fn pool_cancel_query_racing_return_retires_the_physical_session() {
    let url = plaintext_url();
    let pool = Pool::connect(&url, 1)
        .await
        .expect("connect one-slot cancellation pool");
    let observer = connect(&url).await.expect("connect cancellation observer");
    let first = Box::pin(pool.acquire())
        .await
        .expect("borrow the cancellation target");
    let first_pid = first.process_id();
    let token = first.cancel_token();
    let evictions_before = pool.metrics().evictions.get();

    let mut cancel = Box::pin(pool.cancel_query(&token));
    futures_util::future::poll_fn(|context| match cancel.as_mut().poll(context) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(result) => {
            panic!("pool cancellation completed before it could race lease return: {result:?}")
        }
    })
    .await;

    drop(first);
    assert_eq!(pool.active_count(), 0);
    assert_eq!(pool.idle_count(), 0);
    assert_eq!(pool.total_count(), 0);
    assert_eq!(pool.metrics().evictions.get(), evictions_before + 1);

    compio::time::timeout(OPERATION_TIMEOUT, cancel)
        .await
        .expect("in-flight pool cancellation hung after lease return")
        .expect("PostgreSQL did not consume the in-flight CancelRequest");
    wait_until_backend_is_gone(&observer, first_pid).await;

    let replacement = Box::pin(pool.acquire())
        .await
        .expect("retiring the raced session did not release pool capacity");
    assert_client_still_works(&replacement).await;
}

#[compio::test]
async fn cancel_during_copy_in_surfaces_57014_and_preserves_session() {
    let url = plaintext_url();
    let client = connect(&url).await.unwrap();
    let observer = connect(&url).await.unwrap();
    let pid = client.process_id();
    let token = client.cancel_token();
    let table = common::test_object_name("cpg_cancel_copy_in");

    client
        .batch_execute(&format!("CREATE TEMPORARY TABLE {table} (v int4)"))
        .await
        .expect("create COPY IN cancellation fixture");
    let sink = client
        .copy_in::<_, Bytes>(&format!("COPY {table} FROM STDIN"))
        .await
        .expect("enter COPY IN");
    let mut sink = Box::pin(sink);
    sink.as_mut()
        .send(Bytes::from_static(b"7\n"))
        .await
        .expect("send one COPY row before cancellation");
    wait_until_copy_progress(&observer, pid, true).await;

    token
        .cancel_query(common::suite_tls())
        .await
        .expect("send CancelRequest during COPY IN");

    let error = compio::time::timeout(OPERATION_TIMEOUT, sink.as_mut().finish())
        .await
        .expect("COPY IN cancellation recovery timed out")
        .expect_err("cancelled COPY IN reported success");
    assert_eq!(
        error.code(),
        Some(&SqlState::QUERY_CANCELED),
        "COPY IN lost SQLSTATE 57014: {}",
        error_chain(&error)
    );
    wait_until_copy_progress(&observer, pid, false).await;
    assert_client_still_works(&client).await;
    let stored: i64 = client
        .query_one_scalar(&format!("SELECT count(*) FROM {table}"), &[])
        .await
        .expect("count COPY IN rows after cancellation");
    assert_eq!(stored, 0, "cancelled COPY IN committed a partial load");
}

#[compio::test]
async fn cancel_during_copy_out_surfaces_57014_and_preserves_session() {
    const QUERY: &str =
        "COPY (SELECT repeat('x', 1048576) FROM generate_series(1, 1000)) TO STDOUT";

    let url = plaintext_url();
    let client = connect(&url).await.unwrap();
    let observer = connect(&url).await.unwrap();
    let pid = client.process_id();
    let token = client.cancel_token();
    let stream = client.copy_out(QUERY).await.expect("enter COPY OUT");
    let mut stream = Box::pin(stream);
    wait_until_copy_progress(&observer, pid, true).await;
    let stream_result = async {
        loop {
            match stream.as_mut().next().await {
                Some(Ok(_)) => {}
                Some(Err(error)) => break Err(error),
                None => break Ok(()),
            }
        }
    };
    let (copy_result, cancel_result) = compio::time::timeout(
        OPERATION_TIMEOUT,
        futures_util::future::join(stream_result, token.cancel_query(common::suite_tls())),
    )
    .await
    .expect("COPY OUT cancellation did not resolve before the test deadline");
    cancel_result.expect("COPY OUT CancelRequest was not consumed");
    let error = copy_result.expect_err("cancelled COPY OUT ended successfully");
    assert_eq!(
        error.code(),
        Some(&SqlState::QUERY_CANCELED),
        "COPY OUT lost SQLSTATE 57014: {}",
        error_chain(&error)
    );
    wait_until_copy_progress(&observer, pid, false).await;
    assert_client_still_works(&client).await;
}

#[compio::test]
async fn cancel_inside_transaction_requires_rollback_then_preserves_session() {
    const MARKER: &str = "cpg_cancel_transaction";
    const QUERY: &str = "SELECT pg_sleep(30) /* cpg_cancel_transaction */";

    let url = plaintext_url();
    let client = connect(&url).await.unwrap();
    let observer = connect(&url).await.unwrap();
    let pid = client.process_id();
    let token = client.cancel_token();
    client
        .batch_execute("BEGIN")
        .await
        .expect("begin transaction");

    let cancel_task = compio::runtime::spawn(async move {
        wait_until_pg_sleep_is_running(&observer, pid, MARKER).await;
        token.cancel_query(common::suite_tls()).await
    });
    let (query_result, cancel_result) = compio::time::timeout(
        OPERATION_TIMEOUT,
        futures_util::future::join(client.batch_execute(QUERY), cancel_task),
    )
    .await
    .expect("transaction cancellation did not resolve before the test deadline");
    cancel_result
        .expect("transaction cancel task panicked")
        .expect("transaction CancelRequest was not consumed");
    assert_query_canceled(query_result);

    let aborted = client
        .query_one_scalar::<i32, _>("SELECT 1::int4", &[])
        .await
        .expect_err("cancelled transaction did not enter failed state");
    assert_eq!(
        aborted.code(),
        Some(&SqlState::IN_FAILED_SQL_TRANSACTION),
        "cancelled transaction did not report 25P02: {}",
        error_chain(&aborted)
    );
    client
        .batch_execute("ROLLBACK")
        .await
        .expect("roll back cancelled transaction");
    assert_client_still_works(&client).await;
}

#[compio::test]
async fn raw_cancel_interrupts_running_query_and_preserves_session() {
    const MARKER: &str = "cpg_cancel_raw_running_query";
    const QUERY: &str = "SELECT pg_sleep(30) /* cpg_cancel_raw_running_query */";

    let url = plaintext_url();
    let endpoint = tcp_endpoint(&url);
    let client = connect(&url).await.unwrap();
    let observer = connect(&url).await.unwrap();
    let pid = client.process_id();
    let token = client.cancel_token();
    let protocol_version = client.protocol_version();
    let server_version: i32 = client
        .query_one_scalar("SELECT current_setting('server_version_num')::int4", &[])
        .await
        .expect("ask PostgreSQL for its server version");

    let cancel_task = compio::runtime::spawn(async move {
        wait_until_pg_sleep_is_running(&observer, pid, MARKER).await;
        cancel_raw_and_wait_for_server_close(&token, &endpoint).await
    });

    let (query_result, cancel_result) = compio::time::timeout(
        OPERATION_TIMEOUT,
        futures_util::future::join(client.batch_execute(QUERY), cancel_task),
    )
    .await
    .expect("raw CancelRequest did not interrupt pg_sleep before the test deadline");

    let packet = cancel_result.expect("raw CancelRequest task panicked or was cancelled");
    let expected_len = if server_version >= 180_000 { 44 } else { 16 };
    let expected_protocol = if server_version >= 180_000 {
        ProtocolVersion::V3_2
    } else {
        ProtocolVersion::V3_0
    };
    assert_eq!(protocol_version, expected_protocol);
    assert_eq!(
        packet.len(),
        expected_len,
        "CancelRequest did not echo the key length issued by this PostgreSQL server"
    );
    assert_eq!(
        u32::from_be_bytes(packet[..4].try_into().unwrap()) as usize,
        expected_len,
        "CancelRequest's length field did not cover the server-issued key"
    );
    assert_query_canceled(query_result);
    assert_client_still_works(&client).await;
    eprintln!(
        "cancel oracle: server_version_num={server_version} protocol={protocol_version:?} \
         backend_key_len={} cancel_packet_len={} original_sqlstate=57014 session_reused=true",
        packet.len() - 12,
        packet.len()
    );
}
