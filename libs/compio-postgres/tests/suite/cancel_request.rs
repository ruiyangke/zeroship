//! Live PostgreSQL coverage for the out-of-band CancelRequest protocol.
//!
//! These are deliberately separate from tests which cancel by dropping a Rust
//! future. Every cancellation here opens a second connection and sends the
//! backend PID and secret key captured from the target connection's startup.

use compio::buf::{BufResult, IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite};
use compio::net::{TcpListener, TcpStream};
use compio_postgres::config::Host;
use compio_postgres::error::SqlState;
use compio_postgres::{CancelToken, Client, Config, Error, NoTls};
use std::cell::RefCell;
use std::rc::Rc;
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
async fn raw_cancel_interrupts_running_query_and_preserves_session() {
    const MARKER: &str = "cpg_cancel_raw_running_query";
    const QUERY: &str = "SELECT pg_sleep(30) /* cpg_cancel_raw_running_query */";

    let url = plaintext_url();
    let endpoint = tcp_endpoint(&url);
    let client = connect(&url).await.unwrap();
    let observer = connect(&url).await.unwrap();
    let pid = client.process_id();
    let token = client.cancel_token();
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
}
