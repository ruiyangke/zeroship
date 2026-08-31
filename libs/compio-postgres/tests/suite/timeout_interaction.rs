//! Interaction coverage for the driver's four client-owned timeout clocks.
//!
//! The scripted peer is used where transport silence and recovery ordering
//! must be controlled. It speaks only enough plaintext PostgreSQL to complete
//! startup, inspect simple-query/Sync/CancelRequest frames, and send selected
//! responses. It does not simulate TLS, authentication, actual SQL execution,
//! packet loss, or a postmaster applying cancellation. The command-only test
//! therefore uses live PostgreSQL and `pg_sleep` to exercise a real backend.

// Compio's test/timeout wrappers nest enough generic futures that rustc's
// default query-depth limit is exhausted before the interaction tests build.

use compio_postgres::config::SslMode;
use compio_postgres::{Client, Config, Error, Pool, PoolConfig};
use futures_channel::oneshot;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[allow(unused_imports)]
use crate::common;

/// Bounds a connect that must SUCCEED -- every scripted peer in this file
/// accepts, and no test here expects a connect to fail. So this is a hang
/// detector, and its shortness buys nothing.
///
/// It was 1s, the shortest budget in the suite, for a loopback connect to an
/// in-process stub that normally completes in microseconds. That is the same
/// shape as the 5s `ADMIN_STATEMENT_TIMEOUT` in `integration.rs`, which lost a
/// `CREATE SCHEMA` at load 16.4 on 2026-08-23 while a peer project's suite ran.
///
/// THE DISTINCTION THAT MATTERS, when reading the other budgets here: a test
/// asserting a timeout FIRES is robust under load, because load only makes it
/// fire sooner. One bounding an operation that must COMPLETE is fragile. Only
/// the second kind should be widened, which is why this changed and the
/// read/command deadlines below did not -- those are the subject.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const ACQUIRE_TIMEOUT: Duration = Duration::from_millis(150);
const READ_FIRST_TIMEOUT: Duration = Duration::from_millis(100);
const COMMAND_FIRST_TIMEOUT: Duration = Duration::from_millis(100);
const READ_DURING_RECOVERY_TIMEOUT: Duration = Duration::from_millis(750);
const LATE_COMMAND_TIMEOUT: Duration = Duration::from_millis(500);
const ASYNC_WATCHDOG: Duration = Duration::from_secs(8);
const OPERATION_WATCHDOG: Duration = Duration::from_secs(2);
const LIVE_OPERATION_WATCHDOG: Duration = Duration::from_secs(4);
const SOCKET_WATCHDOG: Duration = Duration::from_secs(2);
const THREAD_WATCHDOG: Duration = Duration::from_secs(3);
const CANCEL_REQUEST_CODE: u32 = 80_877_102;
const CANCEL_SECRET: i32 = 1234;

struct StubServer {
    addr: SocketAddr,
    done: mpsc::Receiver<()>,
    thread: thread::JoinHandle<()>,
}

impl StubServer {
    fn spawn(script: impl FnOnce(TcpListener) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind scripted PostgreSQL peer");
        listener
            .set_nonblocking(true)
            .expect("make scripted listener bounded");
        let addr = listener.local_addr().expect("scripted listener address");
        let (done_tx, done) = mpsc::channel();
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
    assert_eq!(
        &body[..4],
        &[0, 3, 0, 2],
        "client did not request protocol 3.2"
    );

    let mut response = backend_frame(b'R', &0u32.to_be_bytes());
    let mut key_data = Vec::with_capacity(8);
    key_data.extend_from_slice(&process_id.to_be_bytes());
    key_data.extend_from_slice(&CANCEL_SECRET.to_be_bytes());
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

fn expect_sync(stream: &mut TcpStream) {
    let (tag, body) = read_frontend_frame(stream);
    assert_eq!(tag, b'S', "command recovery did not send its Sync barrier");
    assert!(body.is_empty(), "Sync carried an unexpected body");
}

fn expect_cancel_request(stream: &mut TcpStream, process_id: i32) {
    let mut packet = [0u8; 16];
    stream
        .read_exact(&mut packet)
        .expect("read the complete CancelRequest");
    assert_eq!(u32::from_be_bytes(packet[0..4].try_into().unwrap()), 16);
    assert_eq!(
        u32::from_be_bytes(packet[4..8].try_into().unwrap()),
        CANCEL_REQUEST_CODE
    );
    assert_eq!(
        i32::from_be_bytes(packet[8..12].try_into().unwrap()),
        process_id
    );
    assert_eq!(
        i32::from_be_bytes(packet[12..16].try_into().unwrap()),
        CANCEL_SECRET
    );
}

fn answer_ready(stream: &mut TcpStream) {
    stream
        .write_all(&backend_frame(b'Z', b"I"))
        .expect("write ReadyForQuery");
    stream.flush().expect("flush ReadyForQuery");
}

fn answer_empty_query(stream: &mut TcpStream) {
    let mut response = backend_frame(b'I', &[]);
    response.extend_from_slice(&backend_frame(b'Z', b"I"));
    stream
        .write_all(&response)
        .expect("write empty-query response");
    stream.flush().expect("flush empty-query response");
}

fn answer_cancelled_query(stream: &mut TcpStream) {
    let mut response = backend_frame(
        b'E',
        b"SERROR\0C57014\0Mcanceling statement due to user request\0\0",
    );
    response.extend_from_slice(&backend_frame(b'Z', b"I"));
    stream
        .write_all(&response)
        .expect("write cancelled-query response");
    stream.flush().expect("flush cancelled-query response");
}

fn write_row_description(stream: &mut TcpStream) {
    let mut body = Vec::new();
    body.extend_from_slice(&1u16.to_be_bytes());
    body.extend_from_slice(b"v\0");
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&0u16.to_be_bytes());
    body.extend_from_slice(&25u32.to_be_bytes());
    body.extend_from_slice(&(-1i16).to_be_bytes());
    body.extend_from_slice(&(-1i32).to_be_bytes());
    body.extend_from_slice(&0u16.to_be_bytes());
    stream
        .write_all(&backend_frame(b'T', &body))
        .expect("write RowDescription before scripted silence");
    stream
        .flush()
        .expect("flush RowDescription before scripted silence");
}

fn write_partial_data_row(stream: &mut TcpStream) {
    // A read timeout must retire rather than resume after bytes were already
    // removed from the socket but the rest of this valid frame never arrived.
    stream
        .write_all(b"D\0\0\0\x0a\0\x01")
        .expect("write partial DataRow frame");
    stream.flush().expect("flush partial DataRow frame");
}

fn expect_disconnect(stream: &mut TcpStream) {
    let deadline = Instant::now() + SOCKET_WATCHDOG;
    let mut bytes = [0u8; 256];
    loop {
        match stream.read(&mut bytes) {
            Ok(0) => return,
            // A graceful close can send Terminate first; physical EOF/reset is
            // the proof that this particular scripted session was retired.
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
                panic!("client kept the scripted session open after its watchdog: {error}")
            }
            Err(error) => panic!("reading scripted disconnect failed: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "client kept sending bytes without retiring its connection"
        );
    }
}

fn disconnect_observed_within(stream: &mut TcpStream, timeout: Duration) -> bool {
    stream
        .set_read_timeout(Some(timeout))
        .expect("set scripted disconnect probe timeout");
    let deadline = Instant::now() + timeout;
    let mut bytes = [0u8; 256];
    loop {
        match stream.read(&mut bytes) {
            Ok(0) => return true,
            Ok(_) => {
                if Instant::now() >= deadline {
                    stream
                        .set_read_timeout(Some(SOCKET_WATCHDOG))
                        .expect("restore scripted peer read watchdog");
                    return false;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe
                        | ErrorKind::NotConnected
                ) =>
            {
                return true;
            }
            Err(error) if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                stream
                    .set_read_timeout(Some(SOCKET_WATCHDOG))
                    .expect("restore scripted peer read watchdog");
                return false;
            }
            Err(error) => panic!("scripted disconnect probe failed: {error}"),
        }
    }
}

fn stub_config(addr: SocketAddr, read_timeout: Option<Duration>) -> Config {
    let mut config = Config::new();
    config
        .user("scripted-user")
        .hostaddr(addr.ip())
        .port(addr.port())
        .ssl_mode(SslMode::Disable)
        .connect_timeout(CONNECT_TIMEOUT);
    if let Some(read_timeout) = read_timeout {
        config.read_timeout(read_timeout);
    }
    config
}

fn pool_config(command_timeout: Option<Duration>, acquire_timeout: Duration) -> PoolConfig {
    let mut config = PoolConfig::new();
    config
        .max_size(1)
        .min_idle(0)
        .acquire_timeout(acquire_timeout)
        .validation_bypass(Duration::from_secs(5));
    if let Some(command_timeout) = command_timeout {
        config.command_timeout(command_timeout);
    }
    config
}

async fn wait_for_client_close(client: &Client) {
    compio::time::timeout(OPERATION_WATCHDOG, async {
        while !client.is_closed() {
            compio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("retired client did not close before its watchdog");
}

fn assert_single_retirement(pool: &Pool) {
    assert_eq!(pool.active_count(), 0, "retired lease remained active");
    assert_eq!(pool.idle_count(), 0, "retired session became idle");
    assert_eq!(pool.total_count(), 0, "retired session kept its pool slot");
    assert_eq!(
        pool.metrics.evictions.get(),
        1,
        "pool did not record exactly one eviction for the retired session"
    );
}

fn live_url() -> String {
    let url = common::env::get(common::env::TestEnvKey::PgTestUrl)
        .unwrap_or_else(|| "postgres://postgres:zeroship@127.0.0.1:5455/zeroship".to_string());
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}sslmode=disable")
}

/// Scripted peer: a real backend cannot stop after RowDescription on demand;
/// this does not exercise SQL execution or a real postmaster cancellation.
#[compio::test]
async fn read_timeout_wins_deterministically_when_both_clocks_are_eligible() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = StubServer::spawn(|listener| {
            let mut stream = accept_bounded(&listener);
            complete_startup(&mut stream, 101);
            assert_eq!(expect_simple_query(&mut stream), b"SELECT 1\0");
            write_row_description(&mut stream);
            // The peer stays open, so only a client-owned clock can end this
            // response after the valid first frame.
            expect_disconnect(&mut stream);
        });

        let connection_config = stub_config(server.addr, Some(READ_FIRST_TIMEOUT));
        let pool_config = pool_config(Some(LATE_COMMAND_TIMEOUT), Duration::from_secs(1));
        let pool = Pool::connect_with_config(connection_config, pool_config)
            .await
            .expect("open pool against mid-response scripted peer");
        let mut client = pool.get().await.expect("check out scripted session");

        let error = compio::time::timeout(
            OPERATION_WATCHDOG,
            client.command(async |client| client.simple_query("SELECT 1").await),
        )
        .await
        .expect("dual-clock command exceeded its operation watchdog")
        .expect_err("silent mid-response peer completed the command");
        assert!(
            error.is_read_timeout(),
            "shorter read clock lost its stable classification: {error:?}"
        );
        assert!(
            !error.is_command_timeout(),
            "the later whole-command clock replaced the earlier read timeout"
        );

        wait_for_client_close(&client).await;
        drop(client);
        assert_single_retirement(&pool);
        compio::time::timeout(OPERATION_WATCHDOG, pool.close())
            .await
            .expect("dual-clock pool close exceeded its watchdog");
        server.finish();
    })
    .await
    .expect("read-first timeout interaction exceeded its outer watchdog");
}

/// Scripted peer: withholding the cancelled response makes recovery silence
/// deterministic; this does not claim a real postmaster ignores CancelRequest.
#[compio::test]
async fn read_timeout_during_command_recovery_keeps_command_classification() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = StubServer::spawn(|listener| {
            let mut primary = accept_bounded(&listener);
            complete_startup(&mut primary, 202);
            assert_eq!(expect_simple_query(&mut primary), b"SELECT 1\0");
            write_row_description(&mut primary);

            let mut cancel = accept_bounded(&listener);
            expect_cancel_request(&mut cancel, 202);
            // EOF confirms the CancelRequest to the client. Withholding both
            // the cancelled response and ReadyForQuery parks recovery on its
            // FIFO Sync barrier until the current socket-read budget expires.
            drop(cancel);
            expect_sync(&mut primary);
            expect_disconnect(&mut primary);
        });

        let connection_config = stub_config(server.addr, Some(READ_DURING_RECOVERY_TIMEOUT));
        let pool_config = pool_config(Some(COMMAND_FIRST_TIMEOUT), Duration::from_secs(1));
        let pool = Pool::connect_with_config(connection_config, pool_config)
            .await
            .expect("open pool against recovery-silence peer");
        let mut client = pool.get().await.expect("check out scripted session");

        let error = compio::time::timeout(
            OPERATION_WATCHDOG,
            client.command(async |client| client.simple_query("SELECT 1").await),
        )
        .await
        .expect("recovery/read interaction exceeded its operation watchdog")
        .expect_err("recovery-silence peer completed the command");
        assert!(
            error.is_command_timeout(),
            "the command clock lost ownership after recovery began: {error:?}"
        );
        assert!(
            !error.is_read_timeout(),
            "a recovery failure replaced the already-expired command clock"
        );
        let recovery_error = error
            .into_source()
            .expect("command timeout omitted its recovery failure")
            .downcast::<Error>()
            .expect("command timeout recovery source was not a driver Error");
        assert!(
            recovery_error.is_read_timeout(),
            "Sync recovery did not surface the read deadline: {recovery_error:?}"
        );

        wait_for_client_close(&client).await;
        drop(client);
        assert_single_retirement(&pool);
        compio::time::timeout(OPERATION_WATCHDOG, pool.close())
            .await
            .expect("recovery/read pool close exceeded its watchdog");
        server.finish();
    })
    .await
    .expect("read-during-recovery interaction exceeded its outer watchdog");
}

/// Live PostgreSQL: `pg_sleep` proves real CancelRequest recovery; a healthy
/// backend cannot simulate partial-frame transport silence for the read clock.
#[compio::test]
async fn command_timeout_recovers_without_a_read_timeout() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let url = live_url();
        let transport = common::test_transport(&url, common::suite_tls()).await;
        let mut connection_config: Config = url.parse().expect("parse PG_TEST_URL");
        connection_config
            .connect_timeout(CONNECT_TIMEOUT)
            .options("-c statement_timeout=0");
        assert_eq!(
            connection_config.get_read_timeout(),
            None,
            "command-only fixture accidentally enabled the read clock"
        );

        let pool_config = pool_config(Some(COMMAND_FIRST_TIMEOUT), Duration::from_secs(1));
        let pool = Pool::connect_with_config(connection_config, pool_config)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let mut client = pool.get().await.expect("check out live PostgreSQL session");
        let announced_pid = client.process_id();

        let error = compio::time::timeout(
            LIVE_OPERATION_WATCHDOG,
            client.command(async |client| {
                client
                    .query_one("SELECT pg_backend_pid(), 42::int4 FROM pg_sleep(3)", &[])
                    .await
            }),
        )
        .await
        .expect("live command cancellation exceeded its operation watchdog")
        .expect_err("pg_sleep outlived the configured command timeout");
        assert!(error.is_command_timeout());
        assert!(!error.is_read_timeout());

        let row = compio::time::timeout(
            OPERATION_WATCHDOG,
            client.query_one("SELECT pg_backend_pid(), 42::int4", &[]),
        )
        .await
        .expect("same-client follow-up exceeded its watchdog")
        .expect("command timeout did not recover the held client");
        transport.assert_backend_pid(announced_pid, row.get(0), "command/read-timeout recovery");
        assert_eq!(row.get::<_, i32>(1), 42);
        assert!(!client.is_closed());

        drop(client);
        assert_eq!(pool.idle_count(), 1);
        assert_eq!(pool.total_count(), 1);
        assert_eq!(pool.metrics.evictions.get(), 0);
        compio::time::timeout(OPERATION_WATCHDOG, pool.close())
            .await
            .expect("command-only pool close exceeded its watchdog");
    })
    .await
    .expect("command-only timeout interaction exceeded its outer watchdog");
}

/// Scripted peer: a withheld DataRow tail proves transport retirement; it does
/// not execute the SQL or model PostgreSQL's server-side statement timeout.
#[compio::test]
async fn read_timeout_retires_an_entry_without_a_command_timeout() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = StubServer::spawn(|listener| {
            let mut stream = accept_bounded(&listener);
            complete_startup(&mut stream, 303);
            assert_eq!(expect_simple_query(&mut stream), b"SELECT 1\0");
            write_partial_data_row(&mut stream);
            expect_disconnect(&mut stream);
        });

        let connection_config = stub_config(server.addr, Some(READ_FIRST_TIMEOUT));
        let pool_config = pool_config(None, Duration::from_secs(1));
        assert_eq!(pool_config.get_command_timeout(), None);
        let pool = Pool::connect_with_config(connection_config, pool_config)
            .await
            .expect("open pool against partial-frame peer");
        let mut client = pool.get().await.expect("check out scripted session");

        let error = compio::time::timeout(
            OPERATION_WATCHDOG,
            client.command(async |client| client.simple_query("SELECT 1").await),
        )
        .await
        .expect("read-only command exceeded its operation watchdog")
        .expect_err("partial-frame peer completed the command");
        assert!(error.is_read_timeout());
        assert!(!error.is_command_timeout());

        wait_for_client_close(&client).await;
        drop(client);
        assert_single_retirement(&pool);
        compio::time::timeout(OPERATION_WATCHDOG, pool.close())
            .await
            .expect("read-only pool close exceeded its watchdog");
        server.finish();
    })
    .await
    .expect("read-only timeout interaction exceeded its outer watchdog");
}

/// Scripted peer: gating CancelRequest EOF exposes recovery occupancy; it does
/// not prove how quickly a real postmaster would consume or apply cancellation.
#[compio::test]
async fn acquire_timeout_cannot_steal_a_connection_in_command_recovery() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let (cancel_seen_tx, cancel_seen_rx) = oneshot::channel();
        let (release_cancel_tx, release_cancel_rx) = mpsc::channel();
        let server = StubServer::spawn(move |listener| {
            let mut primary = accept_bounded(&listener);
            complete_startup(&mut primary, 404);
            assert_eq!(expect_simple_query(&mut primary), b"SELECT pg_sleep(30)\0");

            let mut cancel = accept_bounded(&listener);
            expect_cancel_request(&mut cancel, 404);
            cancel_seen_tx
                .send(())
                .expect("acquire test dropped its CancelRequest signal");
            // Confirmed cancellation waits for EOF. Keeping this socket open
            // until the competing get() finishes makes recovery occupancy an
            // observed protocol state rather than a scheduling guess.
            release_cancel_rx
                .recv_timeout(SOCKET_WATCHDOG)
                .expect("competing acquire did not release CancelRequest gate");
            drop(cancel);

            answer_cancelled_query(&mut primary);
            expect_sync(&mut primary);
            answer_ready(&mut primary);

            assert_eq!(expect_simple_query(&mut primary), b"\0");
            answer_empty_query(&mut primary);
            expect_disconnect(&mut primary);
        });

        let connection_config = stub_config(server.addr, Some(Duration::from_secs(2)));
        let pool_config = pool_config(Some(COMMAND_FIRST_TIMEOUT), ACQUIRE_TIMEOUT);
        let pool = Pool::connect_with_config(connection_config, pool_config)
            .await
            .expect("open pool against gated-recovery peer");
        let mut client = pool.get().await.expect("check out recovery session");
        assert_eq!(client.process_id(), 404);

        // Both pool futures contain the full connection/recovery state
        // machine. Boxing keeps their joined state off the executor stack.
        let command = Box::pin(
            client.command(async |client| client.batch_execute("SELECT pg_sleep(30)").await),
        );
        let contender = Box::pin(async {
            cancel_seen_rx
                .await
                .expect("server ended before observing the CancelRequest");
            assert_eq!(pool.active_count(), 1, "recovering lease was not active");
            assert_eq!(pool.idle_count(), 0, "recovering lease became idle");
            assert_eq!(pool.total_count(), 1);

            let acquire = compio::time::timeout(OPERATION_WATCHDOG, pool.get())
                .await
                .expect("competing get() exceeded its operation watchdog");
            // Unblock the command on either outcome so a failed assertion
            // cannot strand the scripted server behind its recovery gate.
            release_cancel_tx
                .send(())
                .expect("scripted recovery dropped its release receiver");
            // THIS FAILURE IS OVER-DETERMINED, and the count assertions around
            // it are what carry the test's name. `pool_config` sets
            // `max_size(1)` and `client` holds a live lease across the whole
            // join, so a competing `get()` must time out whether or not
            // "recovery" is a distinguished state at all -- a plain saturated
            // pool produces this same `connection timeout after`. Read it as
            // "the waiter was refused cleanly and classified as a pool acquire
            // timeout", not as evidence about recovery.
            //
            // What IS about recovery: `active_count() == 1` and
            // `idle_count() == 0` here and above, taken after the CancelRequest
            // has been observed. A pool that released the entry when the
            // command future entered recovery would show it idle and would hand
            // it over. Proving the stronger claim needs the slot to be
            // available in principle -- `max_size(2)`, a listener that accepts
            // the second dial, and `assert_ne!(second.process_id(), 404)` --
            // which is a larger change to a delicately scripted peer than this
            // is worth; the counts already fail on the bug this guards.
            let error = acquire.expect_err("competing caller stole the recovering entry");
            assert!(
                common::error_chain(&error).contains("connection timeout after"),
                "competing caller got the wrong pool error: {error:?}"
            );
            assert_eq!(pool.metrics.timeouts.get(), 1);
            assert_eq!(pool.pending_count(), 0, "timed-out waiter remained queued");
            assert_eq!(pool.active_count(), 1, "recovery lease was returned early");
            assert_eq!(pool.idle_count(), 0, "recovery entry was handed off early");
            assert_eq!(pool.total_count(), 1);
        });

        let (command_result, ()) = compio::time::timeout(
            LIVE_OPERATION_WATCHDOG,
            futures_util::future::join(command, contender),
        )
        .await
        .expect("command recovery/acquire join exceeded its watchdog");
        let command_error = command_result.expect_err("scripted long command was not cancelled");
        assert!(command_error.is_command_timeout());
        assert!(!command_error.is_read_timeout());

        assert_eq!(pool.active_count(), 1);
        assert_eq!(pool.idle_count(), 0);
        assert_eq!(pool.total_count(), 1);
        assert_eq!(pool.metrics.evictions.get(), 0);
        assert_eq!(
            client.process_id(),
            404,
            "pool replaced the physical session after successful recovery"
        );
        compio::time::timeout(OPERATION_WATCHDOG, client.simple_query(""))
            .await
            .expect("same-session query exceeded its watchdog")
            .expect("recovered physical session was not usable");
        assert!(!client.is_closed());
        drop(client);
        assert_eq!(pool.active_count(), 0);
        assert_eq!(pool.idle_count(), 1);

        compio::time::timeout(OPERATION_WATCHDOG, pool.close())
            .await
            .expect("acquire/recovery pool close exceeded its watchdog");
        server.finish();
    })
    .await
    .expect("acquire/recovery timeout interaction exceeded its outer watchdog");
}

/// The pool must use confirmed cancellation, not the public fire-and-forget
/// primitive. Holding the dedicated cancel socket open while making the
/// cancelled response available proves that recovery does not send its Sync
/// barrier until postmaster-style EOF establishes cross-connection ordering.
#[compio::test]
async fn command_recovery_waits_for_cancel_eof_before_sync_and_reuse() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let (sync_before_eof_tx, sync_before_eof_rx) = oneshot::channel();
        let (release_cancel_tx, release_cancel_rx) = mpsc::channel();
        let server = StubServer::spawn(move |listener| {
            let mut primary = accept_bounded(&listener);
            complete_startup(&mut primary, 505);
            assert_eq!(expect_simple_query(&mut primary), b"SELECT pg_sleep(30)\0");

            let mut cancel = accept_bounded(&listener);
            expect_cancel_request(&mut cancel, 505);

            // Make the original request fully drainable while the dedicated
            // cancel connection remains open. A fire-and-forget pool call can
            // now advance to Sync; confirmed cancellation cannot.
            answer_cancelled_query(&mut primary);
            primary
                .set_read_timeout(Some(Duration::from_millis(200)))
                .expect("set pre-EOF Sync probe timeout");
            let mut tag = [0u8; 1];
            let sync_before_eof = match primary.peek(&mut tag) {
                Ok(1) => {
                    assert_eq!(tag[0], b'S', "unexpected frontend frame before cancel EOF");
                    expect_sync(&mut primary);
                    answer_ready(&mut primary);
                    true
                }
                Err(error)
                    if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) =>
                {
                    false
                }
                Ok(0) => panic!("primary connection closed during the pre-EOF Sync probe"),
                Ok(_) => unreachable!("the Sync probe buffer holds one byte"),
                Err(error) => panic!("pre-EOF Sync probe failed: {error}"),
            };
            sync_before_eof_tx
                .send(sync_before_eof)
                .expect("command test dropped its pre-EOF Sync report");

            release_cancel_rx
                .recv_timeout(SOCKET_WATCHDOG)
                .expect("command test did not release the cancel EOF gate");
            drop(cancel);

            if !sync_before_eof {
                expect_sync(&mut primary);
                answer_ready(&mut primary);
            }
            primary
                .set_read_timeout(Some(SOCKET_WATCHDOG))
                .expect("restore scripted primary watchdog");
            assert_eq!(expect_simple_query(&mut primary), b"\0");
            answer_empty_query(&mut primary);
            expect_disconnect(&mut primary);
        });

        let connection_config = stub_config(server.addr, None);
        let pool_config = pool_config(Some(COMMAND_FIRST_TIMEOUT), Duration::from_secs(1));
        let pool = Pool::connect_with_config(connection_config, pool_config)
            .await
            .expect("open pool against cancel-EOF-gated peer");
        let mut client = pool
            .get()
            .await
            .expect("check out cancel-EOF-gated session");

        let command = Box::pin(
            client.command(async |client| client.batch_execute("SELECT pg_sleep(30)").await),
        );
        let gate = Box::pin(async {
            let sync_before_eof = sync_before_eof_rx
                .await
                .expect("server ended before reporting its pre-EOF Sync probe");
            release_cancel_tx
                .send(())
                .expect("scripted cancel connection dropped its EOF gate");
            sync_before_eof
        });

        let (command_result, sync_before_eof) = compio::time::timeout(
            LIVE_OPERATION_WATCHDOG,
            futures_util::future::join(command, gate),
        )
        .await
        .expect("cancel EOF recovery join exceeded its watchdog");
        assert!(
            !sync_before_eof,
            "pool sent recovery Sync before confirmed cancellation EOF"
        );
        let error = command_result.expect_err("scripted long command was not cancelled");
        assert!(error.is_command_timeout());
        assert!(!error.is_read_timeout());

        compio::time::timeout(OPERATION_WATCHDOG, client.simple_query(""))
            .await
            .expect("same-session post-EOF query exceeded its watchdog")
            .expect("same session was not reusable after confirmed cancellation EOF");
        assert_eq!(client.process_id(), 505);
        assert!(!client.is_closed());
        drop(client);

        compio::time::timeout(OPERATION_WATCHDOG, pool.close())
            .await
            .expect("cancel-EOF-gated pool close exceeded its watchdog");
        server.finish();
    })
    .await
    .expect("cancel EOF ordering test exceeded its outer watchdog");
}

/// Once timeout recovery has opened its out-of-band cancellation connection,
/// abandoning the command future must retire the original physical session.
/// Otherwise the unconfirmed `CancelRequest` can arrive after a later command
/// starts and cancel work belonging to the next operation or pool borrower.
#[compio::test]
async fn dropping_command_during_timeout_recovery_retires_session_before_reuse() {
    compio::time::timeout(
        ASYNC_WATCHDOG,
        Box::pin(async {
            let (cancel_seen_tx, cancel_seen_rx) = oneshot::channel();
            let (retired_tx, retired_rx) = oneshot::channel();
            let (continue_tx, continue_rx) = mpsc::channel::<bool>();
            let server = StubServer::spawn(move |listener| {
                let mut primary = accept_bounded(&listener);
                complete_startup(&mut primary, 606);
                assert_eq!(expect_simple_query(&mut primary), b"SELECT pg_sleep(30)\0");

                let mut cancel = accept_bounded(&listener);
                expect_cancel_request(&mut cancel, 606);
                cancel_seen_tx
                    .send(())
                    .expect("abandonment test dropped its CancelRequest signal");

                let retired = disconnect_observed_within(&mut primary, Duration::from_millis(500));
                retired_tx
                    .send(retired)
                    .expect("abandonment test dropped its retirement probe");

                let continue_with_replacement = continue_rx
                    .recv_timeout(SOCKET_WATCHDOG)
                    .expect("abandonment test did not release the scripted peer");
                drop(cancel);

                if !continue_with_replacement {
                    expect_disconnect(&mut primary);
                    return;
                }
                assert!(
                    retired,
                    "test advanced after retaining the poisoned session"
                );
                drop(primary);

                let mut replacement = accept_bounded(&listener);
                complete_startup(&mut replacement, 607);
                assert_eq!(expect_simple_query(&mut replacement), b"\0");
                answer_empty_query(&mut replacement);
                expect_disconnect(&mut replacement);
            });

            let connection_config = stub_config(server.addr, None);
            let pool_config = pool_config(Some(COMMAND_FIRST_TIMEOUT), Duration::from_secs(1));
            let pool = Pool::connect_with_config(connection_config, pool_config)
                .await
                .expect("open pool against abandonment peer");
            let mut client = Box::pin(pool.get())
                .await
                .expect("check out abandonment session");
            assert_eq!(client.process_id(), 606);

            let command = Box::pin(
                client.command(async |client| client.batch_execute("SELECT pg_sleep(30)").await),
            );
            let command = match compio::time::timeout(
                OPERATION_WATCHDOG,
                futures_util::future::select(command, cancel_seen_rx),
            )
            .await
            .expect("command did not reach timeout recovery before its watchdog")
            {
                futures_util::future::Either::Right((seen, command)) => {
                    seen.expect("server ended before observing the CancelRequest");
                    command
                }
                futures_util::future::Either::Left((result, _)) => {
                    panic!("command recovery completed before its cancel EOF gate: {result:?}")
                }
            };

            drop(command);
            let retired = compio::time::timeout(OPERATION_WATCHDOG, retired_rx)
                .await
                .expect("scripted peer did not report retirement before its watchdog")
                .expect("scripted peer dropped its retirement report");

            if !retired {
                continue_tx
                    .send(false)
                    .expect("scripted peer dropped its cleanup control");
                drop(client);
                compio::time::timeout(OPERATION_WATCHDOG, pool.close())
                    .await
                    .expect("failed-arm pool close exceeded its watchdog");
                server.finish();
                assert!(
                    retired,
                    "dropping timeout recovery left the old physical session reusable"
                );
                return;
            }

            drop(client);
            assert_single_retirement(&pool);
            continue_tx
                .send(true)
                .expect("scripted peer dropped its replacement control");

            let replacement = Box::pin(compio::time::timeout(OPERATION_WATCHDOG, pool.get()))
                .await
                .expect("replacement acquisition exceeded its watchdog")
                .expect("pool could not replace the abandoned recovery session");
            assert_eq!(
                replacement.process_id(),
                607,
                "pool reused the retired session"
            );
            compio::time::timeout(OPERATION_WATCHDOG, replacement.simple_query(""))
                .await
                .expect("replacement query exceeded its watchdog")
                .expect("replacement session was not usable");
            drop(replacement);

            compio::time::timeout(OPERATION_WATCHDOG, pool.close())
                .await
                .expect("abandonment pool close exceeded its watchdog");
            server.finish();
        }),
    )
    .await
    .expect("command-recovery abandonment test exceeded its outer watchdog");
}
