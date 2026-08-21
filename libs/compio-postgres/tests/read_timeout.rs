//! Socket-read deadline coverage.
//!
//! The scripted peer completes a plaintext PostgreSQL startup, consumes one
//! complete simple-query frame, and then stops either before the first response
//! byte or midway through a valid protocol frame. A real PostgreSQL backend
//! cannot be asked to become transport-silent on demand. This fixture does not
//! simulate TLS, authentication, packet loss, a peer that keeps trickling
//! bytes, or a blocked socket write.

use compio_postgres::config::SslMode;
use compio_postgres::{AsyncMessage, Config, NoTls, Pool, PoolConfig};
use futures_util::{StreamExt, TryStreamExt};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

#[allow(dead_code)]
mod common;

const READ_TIMEOUT: Duration = Duration::from_millis(75);
const ASYNC_WATCHDOG: Duration = Duration::from_secs(5);
const SOCKET_WATCHDOG: Duration = Duration::from_secs(2);
const THREAD_WATCHDOG: Duration = Duration::from_secs(3);

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

fn expect_simple_query(stream: &mut TcpStream) -> Vec<u8> {
    let (tag, body) = read_frontend_frame(stream);
    assert_eq!(tag, b'Q', "fixture expects one simple-query frame");
    assert_eq!(body.last(), Some(&0), "simple query is not NUL terminated");
    body
}

fn answer_empty_query(stream: &mut TcpStream) {
    let mut response = backend_frame(b'I', &[]);
    response.extend_from_slice(&backend_frame(b'Z', b"I"));
    stream
        .write_all(&response)
        .expect("write empty-query response");
    stream.flush().expect("flush empty-query response");
}

fn send_notification(stream: &mut TcpStream, process_id: i32, channel: &str, payload: &str) {
    let mut body = Vec::with_capacity(4 + channel.len() + payload.len() + 2);
    body.extend_from_slice(&process_id.to_be_bytes());
    body.extend_from_slice(channel.as_bytes());
    body.push(0);
    body.extend_from_slice(payload.as_bytes());
    body.push(0);
    stream
        .write_all(&backend_frame(b'A', &body))
        .expect("write scripted notification");
    stream.flush().expect("flush scripted notification");
}

fn answer_large_row_prefix(stream: &mut TcpStream, complete: bool) {
    let mut row_description = Vec::new();
    row_description.extend_from_slice(&1u16.to_be_bytes());
    row_description.extend_from_slice(b"v\0");
    row_description.extend_from_slice(&0u32.to_be_bytes());
    row_description.extend_from_slice(&0u16.to_be_bytes());
    row_description.extend_from_slice(&25u32.to_be_bytes());
    row_description.extend_from_slice(&(-1i16).to_be_bytes());
    row_description.extend_from_slice(&(-1i32).to_be_bytes());
    row_description.extend_from_slice(&0u16.to_be_bytes());

    let mut data_row = Vec::with_capacity(2 + 4 + 32 * 1024);
    data_row.extend_from_slice(&1u16.to_be_bytes());
    data_row.extend_from_slice(&(32i32 * 1024).to_be_bytes());
    data_row.resize(data_row.len() + 32 * 1024, b'x');

    // One write still yields two decoder batches deterministically: the data
    // frame exceeds the driver's 16 KiB read chunk, so RowDescription is
    // returned before the incomplete DataRow can be filled.
    let mut response = backend_frame(b'T', &row_description);
    response.extend_from_slice(&backend_frame(b'D', &data_row));
    if complete {
        response.extend_from_slice(&backend_frame(b'C', b"SELECT 1\0"));
        response.extend_from_slice(&backend_frame(b'Z', b"I"));
    }
    stream
        .write_all(&response)
        .expect("write scripted large-row response");
    stream.flush().expect("flush scripted large-row response");
}

fn answer_two_phased_rows_and_complete(stream: &mut TcpStream, phase_delay: Duration) {
    let mut row_description = Vec::new();
    row_description.extend_from_slice(&1u16.to_be_bytes());
    row_description.extend_from_slice(b"v\0");
    row_description.extend_from_slice(&0u32.to_be_bytes());
    row_description.extend_from_slice(&0u16.to_be_bytes());
    row_description.extend_from_slice(&25u32.to_be_bytes());
    row_description.extend_from_slice(&(-1i16).to_be_bytes());
    row_description.extend_from_slice(&(-1i32).to_be_bytes());
    row_description.extend_from_slice(&0u16.to_be_bytes());

    let mut data_row = Vec::with_capacity(2 + 4 + 32 * 1024);
    data_row.extend_from_slice(&1u16.to_be_bytes());
    data_row.extend_from_slice(&(32i32 * 1024).to_be_bytes());
    data_row.resize(data_row.len() + 32 * 1024, b'x');

    // Separate writes make three decoder batches independent of kernel packet
    // coalescing. RowDescription and the first DataRow exhaust the futures-mpsc
    // channel's two effective slots (buffer + sender); the final batch reaches
    // dispatch with ReadyForQuery while its consumer is backpressured.
    stream
        .write_all(&backend_frame(b'T', &row_description))
        .expect("write phased RowDescription");
    stream.flush().expect("flush phased RowDescription");
    thread::sleep(phase_delay);
    stream
        .write_all(&backend_frame(b'D', &data_row))
        .expect("write first phased DataRow");
    stream.flush().expect("flush first phased DataRow");
    thread::sleep(phase_delay);

    let mut response = backend_frame(b'D', &data_row);
    response.extend_from_slice(&backend_frame(b'C', b"SELECT 2\0"));
    response.extend_from_slice(&backend_frame(b'Z', b"I"));
    stream
        .write_all(&response)
        .expect("write final phased response");
    stream.flush().expect("flush final phased response");
}

fn expect_disconnect(stream: &mut TcpStream) {
    let deadline = Instant::now() + SOCKET_WATCHDOG;
    let mut bytes = [0u8; 256];
    loop {
        match stream.read(&mut bytes) {
            Ok(0) => return,
            // A graceful path may send Terminate before shutting down. Keep
            // reading because physical EOF/reset, not that courtesy frame, is
            // the retirement proof.
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe
                        | ErrorKind::NotConnected
                ) =>
            {
                return;
            }
            Err(error) if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                panic!("client kept the scripted server session open after its watchdog: {error}")
            }
            Err(error) => panic!("reading scripted disconnect failed: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "client kept sending bytes without retiring its connection"
        );
    }
}

fn stub_config_without_read_timeout(addr: SocketAddr) -> Config {
    let mut config = Config::new();
    config
        .user("scripted-user")
        .hostaddr(addr.ip())
        .port(addr.port())
        .ssl_mode(SslMode::Disable)
        .connect_timeout(Duration::from_secs(1));
    config
}

fn stub_config(addr: SocketAddr) -> Config {
    let mut config = stub_config_without_read_timeout(addr);
    config.read_timeout(READ_TIMEOUT);
    config
}

fn live_url() -> String {
    let url = common::env::get(common::env::TestEnvKey::PgTestUrl)
        .unwrap_or_else(|| "postgres://postgres:zeroship@localhost:5440/zeroship".to_string());
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}sslmode=disable")
}

#[test]
fn read_timeout_is_opt_in_connection_policy() {
    let mut config = Config::new();
    assert_eq!(config.get_read_timeout(), None);

    config.read_timeout(Duration::from_millis(250));
    assert_eq!(
        config.get_read_timeout(),
        Some(&Duration::from_millis(250))
    );

    assert!(
        "host=localhost read_timeout=1".parse::<Config>().is_err(),
        "programmatic read policy became a libpq-looking DSN parameter"
    );
}

#[compio::test]
async fn a_silent_server_trips_a_distinguishable_read_timeout() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = StubServer::spawn(|listener| {
            let mut stream = accept_bounded(&listener);
            complete_startup(&mut stream, 101);
            let query = expect_simple_query(&mut stream);
            assert_eq!(query, b"SELECT 1\0");
            // Emit nothing: this is the transport silence under test.
            expect_disconnect(&mut stream);
        });

        let (client, connection) = stub_config(server.addr)
            .connect(NoTls)
            .await
            .expect("connect to scripted PostgreSQL peer");
        let driver = compio::runtime::spawn(async move { connection.run().await });
        let started = Instant::now();
        let query_error = compio::time::timeout(
            Duration::from_secs(2),
            client.simple_query("SELECT 1"),
        )
        .await
        .expect("stalled query exceeded its outer watchdog")
        .expect_err("a silent server answered the query");
        assert!(
            query_error.is_read_timeout(),
            "stalled operation lost its read-timeout classification: {query_error:?}"
        );

        let driver_error = compio::time::timeout(Duration::from_secs(2), driver)
            .await
            .expect("connection driver exceeded its outer watchdog")
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            .expect_err("a silent server ended the connection cleanly");
        assert!(
            driver_error.is_read_timeout(),
            "read deadline needs its own error classification, got {driver_error:?}"
        );
        assert!(client.is_closed(), "timed-out client remained usable");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the configured read deadline did not bound transport silence"
        );
        drop(client);
        server.finish();
    })
    .await
    .expect("silent-server read-timeout test exceeded its outer watchdog");
}

#[compio::test]
async fn silence_mid_frame_trips_the_deadline_and_retires_the_session() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = StubServer::spawn(|listener| {
            let mut stream = accept_bounded(&listener);
            complete_startup(&mut stream, 102);
            let query = expect_simple_query(&mut stream);
            assert_eq!(query, b"SELECT 1\0");

            // Valid DataRow prefix: tag, total length 10, and one-column count.
            // The four-byte column length is deliberately withheld so the
            // driver's submitted read is parked halfway through one frame.
            stream
                .write_all(b"D\0\0\0\x0a\0\x01")
                .expect("write partial DataRow frame");
            stream.flush().expect("flush partial DataRow frame");
            expect_disconnect(&mut stream);
        });

        let (client, connection) = stub_config(server.addr)
            .connect(NoTls)
            .await
            .expect("connect to partial-frame peer");
        let driver = compio::runtime::spawn(async move { connection.run().await });

        let operation_error = compio::time::timeout(
            Duration::from_secs(2),
            client.simple_query("SELECT 1"),
        )
        .await
        .expect("partial-frame query exceeded its outer watchdog")
        .expect_err("partial-frame peer completed the query");
        assert!(
            operation_error.is_read_timeout(),
            "partial-frame stall lost its timeout classification: {operation_error:?}"
        );

        let driver_error = compio::time::timeout(Duration::from_secs(2), driver)
            .await
            .expect("partial-frame driver exceeded its outer watchdog")
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            .expect_err("partial-frame stall ended the connection cleanly");
        assert!(driver_error.is_read_timeout());
        drop(client);
        server.finish();
    })
    .await
    .expect("partial-frame timeout test exceeded its outer watchdog");
}

#[compio::test]
async fn an_idle_connection_does_not_spend_the_read_budget() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let (begin_idle_tx, begin_idle_rx) = std::sync::mpsc::channel();
        let server = StubServer::spawn(move |listener| {
            let mut stream = accept_bounded(&listener);
            complete_startup(&mut stream, 111);
            begin_idle_rx
                .recv_timeout(SOCKET_WATCHDOG)
                .expect("idle client did not start its bounded exposure window");
            // The notification proves the plaintext driver's background read
            // stayed submitted while its zero-obligation deadline was disarmed.
            thread::sleep(READ_TIMEOUT * 3);
            send_notification(&mut stream, 111, "read_budget", "still-reading");
            let query = expect_simple_query(&mut stream);
            assert_eq!(query, b"\0");
            answer_empty_query(&mut stream);
            expect_disconnect(&mut stream);
        });

        let (client, mut connection) = stub_config(server.addr)
            .connect(NoTls)
            .await
            .expect("connect to scripted PostgreSQL peer");
        let mut notifications = connection.notifications();
        let driver = compio::runtime::spawn(async move { connection.run().await });
        let idle_started = Instant::now();
        begin_idle_tx
            .send(())
            .expect("scripted idle peer dropped its exposure trigger");
        let message = compio::time::timeout(SOCKET_WATCHDOG, notifications.next())
            .await
            .expect("idle background read did not deliver its notification")
            .expect("idle notification channel closed without a message");
        assert!(
            idle_started.elapsed() >= READ_TIMEOUT * 3,
            "idle exemption was not exposed for the scripted three budgets"
        );
        match message {
            AsyncMessage::Notification(notification) => {
                assert_eq!(notification.process_id(), 111);
                assert_eq!(notification.channel(), "read_budget");
                assert_eq!(notification.payload(), "still-reading");
            }
            other => panic!("idle peer sent an unexpected async message: {other:?}"),
        }
        client
            .simple_query("")
            .await
            .expect("idle time incorrectly consumed the socket-read budget");
        assert!(!client.is_closed());

        drop(client);
        compio::time::timeout(Duration::from_secs(2), driver)
            .await
            .expect("idle-control connection did not shut down")
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            .expect("idle-control connection driver failed");
        server.finish();
    })
    .await
    .expect("idle read-deadline control exceeded its outer watchdog");
}

#[compio::test]
async fn a_timed_out_pool_entry_is_retired_before_return() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = StubServer::spawn(|listener| {
            let mut first = accept_bounded(&listener);
            complete_startup(&mut first, 201);
            let query = expect_simple_query(&mut first);
            assert_eq!(query, b"SELECT 1\0");
            // First session goes silent and must physically close before the
            // listener accepts the replacement.
            expect_disconnect(&mut first);

            let mut second = accept_bounded(&listener);
            complete_startup(&mut second, 202);
            let query = expect_simple_query(&mut second);
            assert_eq!(query, b"\0");
            answer_empty_query(&mut second);
            expect_disconnect(&mut second);
        });

        let mut pool_config = PoolConfig::new();
        pool_config
            .max_size(1)
            .min_idle(0)
            .acquire_timeout(Duration::from_secs(2))
            .validation_bypass(Duration::from_secs(5));
        let pool = Pool::connect_with_config(stub_config(server.addr), pool_config)
            .await
            .expect("warm pool against scripted PostgreSQL peer");

        let first = pool.get().await.expect("check out warm scripted session");
        assert_eq!(first.process_id(), 201);
        let error = compio::time::timeout(
            Duration::from_secs(2),
            first.simple_query("SELECT 1"),
        )
        .await
        .expect("pooled stalled query exceeded its outer watchdog")
        .expect_err("silent pooled session answered the query");
        assert!(error.is_read_timeout());
        // Drop immediately: the read task must have published pool poison
        // before it delivered the operation error, even if Connection::run has
        // not yet closed the client request channel.
        drop(first);

        assert_eq!(pool.idle_count(), 0, "timed-out session became idle");
        assert_eq!(pool.total_count(), 0, "timed-out session kept its pool slot");
        assert_eq!(pool.metrics.evictions.get(), 1);

        let second = compio::time::timeout(Duration::from_secs(2), pool.get())
            .await
            .expect("replacement checkout exceeded its watchdog")
            .expect("pool did not replace the timed-out session");
        assert_eq!(second.process_id(), 202, "pool reused the silent session");
        second
            .simple_query("")
            .await
            .expect("replacement session could not complete a normal query");
        assert!(!second.is_closed());
        assert_eq!(pool.metrics.connections_created.get(), 2);
        drop(second);

        compio::time::timeout(Duration::from_secs(2), pool.close())
            .await
            .expect("pool close exceeded its watchdog");
        server.finish();
    })
    .await
    .expect("pool retirement test exceeded its outer watchdog");
}

#[compio::test]
async fn backpressure_cannot_hide_retirement_from_the_pool() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = StubServer::spawn(|listener| {
            let mut stream = accept_bounded(&listener);
            complete_startup(&mut stream, 301);
            let query = expect_simple_query(&mut stream);
            assert_eq!(query, b"SELECT repeat('x', 32768)\0");
            answer_large_row_prefix(&mut stream, false);
            // The client deliberately does not poll either response batch.
            // Retirement must bypass that consumer backpressure.
            expect_disconnect(&mut stream);
        });

        let mut pool_config = PoolConfig::new();
        pool_config
            .max_size(1)
            .min_idle(0)
            .acquire_timeout(Duration::from_secs(2))
            .validation_bypass(Duration::from_secs(5));
        let pool = Pool::connect_with_config(stub_config(server.addr), pool_config)
            .await
            .expect("open pool against backpressure peer");

        let first = pool.get().await.expect("check out scripted session");
        let retained = first
            .simple_query_raw("SELECT repeat('x', 32768)")
            .await
            .expect("enqueue large scripted query");
        compio::time::timeout(Duration::from_secs(2), async {
            while !first.is_closed() {
                compio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("read timeout was trapped behind response backpressure");

        drop(first);
        assert_eq!(pool.idle_count(), 0, "poisoned session became idle");
        assert_eq!(pool.total_count(), 0, "poisoned session kept its slot");
        assert_eq!(pool.metrics.evictions.get(), 1);
        // Retain the unpolled response through every pool assertion. Dropping
        // it earlier would remove the backpressure this regression requires.
        drop(retained);

        compio::time::timeout(Duration::from_secs(2), pool.close())
            .await
            .expect("backpressure pool close exceeded its watchdog");
        server.finish();
    })
    .await
    .expect("backpressure retirement test exceeded its outer watchdog");
}

#[compio::test]
async fn a_complete_backpressured_response_does_not_arm_an_idle_read() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let (response_flushed_tx, response_flushed_rx) = futures_channel::oneshot::channel();
        let server = StubServer::spawn(move |listener| {
            let mut stream = accept_bounded(&listener);
            complete_startup(&mut stream, 311);
            let query = expect_simple_query(&mut stream);
            assert_eq!(query, b"SELECT repeat('x', 32768)\0");
            // Receiving the query proves its bytes reached the peer. Give the
            // driver one bounded scheduling window to observe flush completion
            // and arm the response obligation before the fast reply arrives.
            thread::sleep(READ_TIMEOUT / 3);
            answer_two_phased_rows_and_complete(&mut stream, READ_TIMEOUT / 8);
            response_flushed_tx
                .send(())
                .expect("complete-response client dropped its flush signal");
            let query = expect_simple_query(&mut stream);
            assert_eq!(query, b"\0");
            answer_empty_query(&mut stream);
            expect_disconnect(&mut stream);
        });

        let (client, connection) = stub_config(server.addr)
            .connect(NoTls)
            .await
            .expect("connect to complete-response peer");
        let driver = compio::runtime::spawn(async move { connection.run().await });
        let retained = client
            .simple_query_raw("SELECT repeat('x', 32768)")
            .await
            .expect("enqueue complete large response");
        compio::time::timeout(SOCKET_WATCHDOG, response_flushed_rx)
            .await
            .expect("scripted complete response was not flushed before its watchdog")
            .expect("scripted complete-response peer dropped its flush signal");
        let idle_started = Instant::now();
        compio::time::sleep(READ_TIMEOUT * 3).await;
        assert!(
            idle_started.elapsed() >= READ_TIMEOUT * 3,
            "completed response was not exposed for the scripted three budgets"
        );
        assert!(
            !client.is_closed(),
            "read-ahead timed out after ReadyForQuery was already decoded"
        );
        drop(retained);
        client
            .simple_query("")
            .await
            .expect("completed backpressured response poisoned the session");

        drop(client);
        compio::time::timeout(Duration::from_secs(2), driver)
            .await
            .expect("complete-response driver did not shut down")
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            .expect("complete-response driver failed");
        server.finish();
    })
    .await
    .expect("complete backpressure control exceeded its outer watchdog");
}

#[compio::test]
async fn a_complete_response_is_delivered_before_the_peers_eof() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = StubServer::spawn(|listener| {
            let mut stream = accept_bounded(&listener);
            complete_startup(&mut stream, 312);
            let query = expect_simple_query(&mut stream);
            assert_eq!(query, b"SELECT repeat('x', 32768)\0");
            answer_two_phased_rows_and_complete(&mut stream, Duration::from_millis(100));
            // Close immediately after the complete response. The connection
            // must preserve wire order even though its reader can observe EOF
            // before the backpressured response consumer drains every batch.
        });

        let (client, connection) = stub_config_without_read_timeout(server.addr)
            .connect(NoTls)
            .await
            .expect("connect to response-then-EOF peer");
        let driver = compio::runtime::spawn(async move { connection.run().await });
        let response = client
            .simple_query_raw("SELECT repeat('x', 32768)")
            .await
            .expect("enqueue response-then-EOF query");
        // Leave the capacity-one operation channel full long enough for the
        // connection reader to queue a later response batch and observe EOF.
        compio::time::sleep(Duration::from_millis(500)).await;
        let messages = response
            .try_collect::<Vec<_>>()
            .await
            .expect("peer EOF overtook its complete response");
        assert_eq!(
            messages.len(),
            4,
            "response stream ended before RowDescription, two DataRows, and CommandComplete"
        );

        drop(client);
        compio::time::timeout(Duration::from_secs(2), driver)
            .await
            .expect("response-then-EOF driver exceeded its watchdog")
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            .expect("complete response followed by EOF failed the driver");
        server.finish();
    })
    .await
    .expect("response-then-EOF regression exceeded its outer watchdog");
}

#[compio::test]
async fn a_normal_live_query_inside_the_deadline_is_untouched() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let url = live_url();
        let mut config: Config = url.parse().expect("parse PG_TEST_URL");
        config.read_timeout(Duration::from_secs(1));
        let (client, connection) = config
            .connect(NoTls)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let driver = compio::runtime::spawn(async move { connection.run().await });

        let row = client
            .query_one("SELECT 42::int4 FROM pg_sleep(0.01)", &[])
            .await
            .expect("an in-budget live query hit the socket-read deadline");
        assert_eq!(row.get::<_, i32>(0), 42);
        assert!(!client.is_closed());

        drop(client);
        compio::time::timeout(Duration::from_secs(2), driver)
            .await
            .expect("live control connection did not shut down")
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            .expect("live control connection driver failed");
    })
    .await
    .expect("normal live query exceeded its outer watchdog");
}

#[compio::test]
async fn copy_input_time_is_not_charged_as_server_read_silence() {
    use bytes::Bytes;
    use futures_util::SinkExt;

    compio::time::timeout(ASYNC_WATCHDOG, async {
        let url = live_url();
        let mut config: Config = url.parse().expect("parse PG_TEST_URL");
        config.read_timeout(Duration::from_millis(100));
        let (client, connection) = config
            .connect(NoTls)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let driver = compio::runtime::spawn(async move { connection.run().await });

        client
            .batch_execute("CREATE TEMPORARY TABLE cpg_read_timeout_copy (n int)")
            .await
            .expect("create COPY deadline fixture");
        let sink = client
            .copy_in::<_, Bytes>("COPY cpg_read_timeout_copy (n) FROM STDIN")
            .await
            .expect("enter COPY input mode");
        let mut sink = std::pin::pin!(sink);

        // PostgreSQL is healthy but deliberately silent here because it is
        // waiting for caller input. This is not a stalled server read.
        let producer_started = Instant::now();
        compio::time::sleep(Duration::from_millis(350)).await;
        assert!(
            producer_started.elapsed() >= Duration::from_millis(350),
            "COPY exemption was not exposed for the scripted producer delay"
        );
        assert!(!client.is_closed(), "COPY producer time spent the read budget");
        sink.as_mut()
            .send(Bytes::from_static(b"7\n"))
            .await
            .expect("send COPY row after producer delay");
        assert_eq!(
            sink.as_mut().finish().await.expect("finish delayed COPY"),
            1
        );
        let row = client
            .query_one("SELECT n FROM cpg_read_timeout_copy", &[])
            .await
            .expect("query delayed COPY result");
        assert_eq!(row.get::<_, i32>(0), 7);

        drop(client);
        compio::time::timeout(Duration::from_secs(2), driver)
            .await
            .expect("COPY control driver did not shut down")
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            .expect("COPY control driver failed");
    })
    .await
    .expect("COPY input deadline control exceeded its outer watchdog");
}
