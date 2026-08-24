//! Live replication-protocol tests against a real walsender.
//!
//! What these need is `max_wal_senders > 0`, and a spare slot in it. NOT
//! `wal_level=logical`: `IDENTIFY_SYSTEM` answers on any `replication=database`
//! connection, and only `CREATE_REPLICATION_SLOT ... LOGICAL` and
//! `START_REPLICATION ... LOGICAL` need the logical level. `wal_level=minimal`
//! is the case that breaks them, because it forces `max_wal_senders` to 0.
//! The `replication=database` startup parameter puts the backend in walsender
//! mode, where the regular query grammar is gone and only the replication
//! commands answer.
//!
//! Deadline stalls use a bounded scripted peer: PostgreSQL cannot be told to
//! stop at a chosen byte inside a protocol frame, and its keepalives make a
//! measured zero-byte streaming interval nondeterministic.
//!
//! The crate had NO live coverage of `src/replication.rs` before this file.
//! Its unit tests build `IDENTIFY_SYSTEM` row bodies by hand, and that is
//! exactly how the defect below survived: the hand-built shape is not the one
//! `DataRowBody::buffer()` produces, so parser and fixture agreed on a row
//! layout the server never sends.

use compio_postgres::config::SslMode;
use compio_postgres::replication::{ReplicationMessage, StartReplicationOptions};
use compio_postgres::{Config, NoTls};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

mod common;

const READ_TIMEOUT: Duration = Duration::from_millis(75);
const IDLE_EXPOSURE: Duration = Duration::from_millis(225);
const OPERATION_WATCHDOG: Duration = Duration::from_secs(1);
const SOCKET_WATCHDOG: Duration = Duration::from_secs(2);
const ASYNC_WATCHDOG: Duration = Duration::from_secs(5);
const THREAD_WATCHDOG: Duration = Duration::from_secs(3);

/// A bounded plaintext peer for states a real walsender cannot be instructed
/// to enter, such as stopping after an exact frame prefix.
struct ReplicationStub {
    addr: SocketAddr,
    done: std::sync::mpsc::Receiver<()>,
    thread: thread::JoinHandle<()>,
}

impl ReplicationStub {
    fn spawn(script: impl FnOnce(TcpListener) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind scripted replication peer");
        listener
            .set_nonblocking(true)
            .expect("make scripted replication listener bounded");
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
            .expect("scripted replication peer exceeded its thread watchdog");
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
                    "replication client missed the scripted accept watchdog"
                );
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("scripted replication accept failed: {error}"),
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

fn complete_replication_startup(stream: &mut TcpStream, delay: Duration) {
    let mut length = [0u8; 4];
    stream
        .read_exact(&mut length)
        .expect("read replication startup length");
    let length = u32::from_be_bytes(length) as usize;
    assert!(
        length >= 8,
        "replication startup is shorter than its header"
    );
    assert!(
        length <= 1024 * 1024,
        "replication startup is implausibly large"
    );
    let mut body = vec![0u8; length - 4];
    stream
        .read_exact(&mut body)
        .expect("read replication startup body");
    assert_eq!(&body[..4], &[0, 3, 0, 0]);
    assert!(
        body.windows(b"replication\0database\0".len())
            .any(|window| window == b"replication\0database\0"),
        "client startup did not request logical replication mode"
    );

    // Startup is deliberately delayed in one test to prove read_timeout is
    // not installed until authentication completes.
    thread::sleep(delay);
    let mut response = backend_frame(b'R', &0u32.to_be_bytes());
    response.extend_from_slice(&backend_frame(b'K', &[0; 8]));
    response.extend_from_slice(&backend_frame(b'Z', b"I"));
    stream
        .write_all(&response)
        .expect("write scripted replication startup response");
    stream
        .flush()
        .expect("flush scripted replication startup response");
}

fn expect_simple_query(stream: &mut TcpStream) -> Vec<u8> {
    let mut tag = [0u8; 1];
    stream.read_exact(&mut tag).expect("read simple-query tag");
    assert_eq!(tag[0], b'Q');
    let mut length = [0u8; 4];
    stream
        .read_exact(&mut length)
        .expect("read simple-query length");
    let length = u32::from_be_bytes(length) as usize;
    assert!(length >= 5, "simple query has no NUL-terminated body");
    assert!(length <= 1024 * 1024, "simple query is implausibly large");
    let mut body = vec![0u8; length - 4];
    stream
        .read_exact(&mut body)
        .expect("read simple-query body");
    assert_eq!(body.last(), Some(&0), "simple query is not NUL terminated");
    body
}

/// A complete, well-formed `IDENTIFY_SYSTEM` response: one row, its completion,
/// and the `ReadyForQuery` that closes the phase.
fn identify_system_response() -> Vec<u8> {
    let mut row = Vec::new();
    row.extend_from_slice(&4u16.to_be_bytes());
    for field in [
        b"scripted-system".as_slice(),
        b"1".as_slice(),
        b"0/10".as_slice(),
        b"scripted-db".as_slice(),
    ] {
        row.extend_from_slice(&i32::try_from(field.len()).unwrap().to_be_bytes());
        row.extend_from_slice(field);
    }

    let mut response = backend_frame(b'D', &row);
    response.extend_from_slice(&backend_frame(b'C', b"IDENTIFY_SYSTEM\0"));
    response.extend_from_slice(&backend_frame(b'Z', b"I"));
    response
}

fn send_identify_system(stream: &mut TcpStream) {
    stream
        .write_all(&identify_system_response())
        .expect("write IDENTIFY_SYSTEM response");
    stream.flush().expect("flush IDENTIFY_SYSTEM response");
}

fn send_copy_both(stream: &mut TcpStream) {
    // Overall format byte + zero columns. The driver intentionally ignores
    // these fields, but this is the real CopyBothResponse shape.
    stream
        .write_all(&backend_frame(b'W', &[0, 0, 0]))
        .expect("write CopyBothResponse");
    stream.flush().expect("flush CopyBothResponse");
}

fn send_keepalive(stream: &mut TcpStream, wal_end: u64) {
    let mut body = vec![b'k'];
    body.extend_from_slice(&wal_end.to_be_bytes());
    body.extend_from_slice(&700_000_000_000i64.to_be_bytes());
    body.push(0);
    stream
        .write_all(&backend_frame(b'd', &body))
        .expect("write PrimaryKeepalive");
    stream.flush().expect("flush PrimaryKeepalive");
}

fn send_notice(stream: &mut TcpStream) {
    stream
        .write_all(&backend_frame(b'N', b"Mscripted notice\0\0"))
        .expect("write replication NoticeResponse");
    stream.flush().expect("flush replication NoticeResponse");
}

fn expect_disconnect(stream: &mut TcpStream) {
    let deadline = Instant::now() + SOCKET_WATCHDOG;
    let mut bytes = [0u8; 256];
    loop {
        match stream.read(&mut bytes) {
            Ok(0) => return,
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
                panic!("replication client kept its timed-out session open: {error}")
            }
            Err(error) => panic!("reading replication disconnect failed: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "replication client kept sending without retiring its session"
        );
    }
}

fn stub_config(addr: SocketAddr) -> Config {
    let mut config = Config::new();
    config
        .user("scripted-replication-user")
        .hostaddr(addr.ip())
        .port(addr.port())
        .ssl_mode(SslMode::Disable)
        .connect_timeout(Duration::from_secs(1))
        .read_timeout(READ_TIMEOUT);
    config
}

fn start_options() -> StartReplicationOptions<'static> {
    StartReplicationOptions {
        slot_name: "deadline_slot",
        start_lsn: "0/0",
        proto_version: 1,
        publication_names: &["deadline_publication"],
        ..Default::default()
    }
}

fn test_url() -> String {
    common::test_url()
}

/// The credentials and database from the test DSN, with no host or port.
///
/// The multi-host tests below need to place the live endpoint at a chosen
/// position in a list, which a DSN's single host cannot express.
fn credentials_only(url: &str) -> Config {
    let parsed: Config = url.parse().expect("test DSN did not parse");
    let mut config = Config::new();
    if let Some(user) = parsed.get_user() {
        config.user(user);
    }
    if let Some(password) = parsed.get_password() {
        config.password(password);
    }
    if let Some(dbname) = parsed.get_dbname() {
        config.dbname(dbname);
    }
    config
}

/// The host and port the test DSN names.
fn live_endpoint(url: &str) -> (String, u16) {
    let parsed: Config = url.parse().expect("test DSN did not parse");
    let host = match parsed.get_hosts().first().expect("test DSN names no host") {
        compio_postgres::config::Host::Tcp(host) => host.clone(),
        #[cfg(unix)]
        compio_postgres::config::Host::Unix(path) => {
            panic!(
                "this test needs a TCP endpoint, got the socket {}",
                path.display()
            )
        }
    };
    let port = parsed.get_ports().first().copied().unwrap_or(5432);
    (host, port)
}

/// A TCP port on loopback with nothing listening on it.
///
/// Bound and released rather than picked from thin air: the kernel hands out
/// a port it knows is free, and it does not hand the same one out again while
/// this test runs. A port nobody listens on REFUSES, which is what makes the
/// first attempt below fail fast instead of hanging on a backlog.
async fn closed_port() -> u16 {
    let listener = compio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let port = listener.local_addr().expect("listener address").port();
    drop(listener);
    port
}

/// A replication connection must try every host in the configuration.
///
/// `connect_replication` took `get_hosts().first()` and stopped there, so a
/// two-endpoint configuration had no failover at all: if the first host was
/// down, the call failed while a healthy second host sat unused. The same
/// `first()` was applied to DNS resolution, which is the more common way to
/// meet this - a name that resolves to both an AAAA and an A record against a
/// server bound only to IPv4 fails on the first address every time.
#[compio::test]
async fn replication_connect_tries_every_configured_host() {
    let url = test_url();
    let (live_host, live_port) = live_endpoint(&url);
    let dead_port = closed_port().await;

    let mut config = credentials_only(&url);
    config.host("127.0.0.1");
    config.port(dead_port);
    config.host(live_host);
    config.port(live_port);
    config.application_name("cpg_replication_failover");

    let mut replication =
        match compio_postgres::replication::connect_replication(NoTls, &config).await {
            Ok(connection) => connection,
            Err(e) if common::server_answered(&e) => panic!(
                "the server refused a replication connection: {}",
                common::error_chain(&e)
            ),
            Err(e) => panic!(
                "a live host listed after a dead one was never tried: {}",
                common::error_chain(&e)
            ),
        };

    let identity = replication
        .identify_system()
        .await
        .expect("IDENTIFY_SYSTEM failed on the host that was reached");
    assert!(
        identity.xlogpos.contains('/'),
        "xlogpos {:?} is not an LSN",
        identity.xlogpos
    );
}

/// The control for the host walk: one port covers every host.
///
/// libpq broadcasts a single `port` across all hosts and only requires a
/// one-per-host list when more than one is given. A host walk that demands
/// `ports.len() == hosts.len()`, or that indexes the port list by host
/// position without falling back to the first entry, refuses this
/// configuration - which is a valid one.
#[compio::test]
async fn replication_connect_broadcasts_a_single_port_across_hosts() {
    let url = test_url();
    let (live_host, live_port) = live_endpoint(&url);

    let mut config = credentials_only(&url);
    config.host(live_host.clone());
    config.host(live_host);
    config.port(live_port);
    config.application_name("cpg_replication_one_port");

    let mut replication =
        match compio_postgres::replication::connect_replication(NoTls, &config).await {
            Ok(connection) => connection,
            Err(e) if common::server_answered(&e) => panic!(
                "the server refused a replication connection: {}",
                common::error_chain(&e)
            ),
            Err(e) => panic!(
                "two hosts sharing one port must connect: {}",
                common::error_chain(&e)
            ),
        };

    replication
        .identify_system()
        .await
        .expect("IDENTIFY_SYSTEM failed");
}

/// The control for the failure path: when no host answers, the error is the
/// last endpoint's, not a success and not a hang.
#[compio::test]
async fn replication_connect_reports_the_error_when_no_host_answers() {
    let url = test_url();
    let first_dead = closed_port().await;
    let second_dead = closed_port().await;

    let mut config = credentials_only(&url);
    config.host("127.0.0.1");
    config.port(first_dead);
    config.host("127.0.0.1");
    config.port(second_dead);

    let err = compio_postgres::replication::connect_replication(NoTls, &config)
        .await
        .err()
        .expect("no host was listening, so this cannot succeed");
    assert!(
        !common::server_answered(&err),
        "nothing answered, so this must be a connect failure: {}",
        common::error_chain(&err)
    );
}

/// `IDENTIFY_SYSTEM` must return the server's real identity.
///
/// `postgres-protocol` consumes the `DataRow`'s `u16` field count during
/// `Message::parse` and keeps only the length-prefixed fields in
/// `DataRowBody::storage`, which is what `buffer()` hands back. The parser
/// read a `u16` count of its own as its first action, so it consumed the top
/// two bytes of the FIRST FIELD'S `i32` length instead. A 19-character
/// systemid is length `00 00 00 13`, whose leading two bytes are zero, so the
/// count read as 0, no fields were parsed, and the call returned empty
/// strings and a zero timeline while reporting success.
///
/// Asserted against the server's own `pg_control_system()` rather than
/// against a non-empty string, so the test pins the VALUE and not merely the
/// absence of the default.
#[compio::test]
async fn identify_system_returns_the_servers_real_identity() {
    let url = test_url();

    // The expected identity, read over an ordinary connection.
    //
    // Routed through `postgres_unreachable` rather than `expect`, because a
    // bare message here would diagnose the wrong cause: a `53300`
    // too_many_connections refusal is a server that ANSWERED, and printing a
    // replication-configuration hint for it is the exact mistake
    // `tests/common/mod.rs` records a measured incident of.
    let (client, connection) = match compio_postgres::connect(&url, NoTls).await {
        Ok(pair) => pair,
        Err(e) => common::postgres_unreachable(&url, &e),
    };
    let driver = compio::runtime::spawn(async move { connection.run().await });
    let expected_systemid: String = client
        .query_one_scalar(
            "SELECT system_identifier::text FROM pg_control_system()",
            &[],
        )
        .await
        .expect("pg_control_system() failed");
    drop(client);
    let _ = driver.await;

    let mut config: Config = url.parse().expect("test DSN did not parse");
    config.application_name("cpg_identify_system");
    // Same reasoning as above: let the shared helper decide whether the server
    // answered before it names a remedy.
    let mut replication =
        match compio_postgres::replication::connect_replication(NoTls, &config).await {
            Ok(connection) => connection,
            Err(e) if common::server_answered(&e) => panic!(
                "the server refused a replication connection: {}",
                common::error_chain(&e)
            ),
            Err(e) => common::postgres_unreachable(&url, &e),
        };

    let identity = replication
        .identify_system()
        .await
        .expect("IDENTIFY_SYSTEM failed");

    assert_eq!(
        identity.systemid, expected_systemid,
        "IDENTIFY_SYSTEM reported a system identifier the server does not have"
    );
    assert!(
        identity.timeline >= 1,
        "timeline {} is not a real timeline",
        identity.timeline
    );
    assert!(
        identity.xlogpos.contains('/'),
        "xlogpos {:?} is not an LSN",
        identity.xlogpos
    );
}

/// `connect_replication`'s TLS refusal is keyed to the CONTRADICTION, not to
/// the endpoint.
///
/// A matched pair built from ONE config, differing in exactly one call:
/// `ssl_mode`. `sslrootcert=system` is a contradiction under `Prefer` and is
/// fine under `VerifyFull`, so the same endpoint must be refused in the first
/// case and dialled in the second. Both arms are constructed here rather than
/// leaning on the unit test in `src/replication.rs`, which differs from this
/// one in host form, listener, timeout and credentials -- pairing against it
/// would have varied five things at once and proved nothing about which one
/// mattered.
#[compio::test]
async fn replication_tls_refusal_is_keyed_to_the_contradiction_not_the_endpoint() {
    // One dead port for both arms: the endpoint is held fixed by construction.
    let port = closed_port().await;
    let config_with = |mode| {
        let mut config = credentials_only(&test_url());
        config.host("127.0.0.1");
        config.port(port);
        config.ssl_mode(mode);
        config.ssl_root_cert(compio_postgres::config::SslRootCert::System);
        config
    };

    let refused = compio_postgres::replication::connect_replication(
        NoTls,
        &config_with(compio_postgres::config::SslMode::Prefer),
    )
    .await
    .err()
    .expect("a weak sslmode with sslrootcert=system is a contradiction");
    let refused_chain = common::error_chain(&refused);
    assert!(
        refused_chain.contains("sslrootcert=system"),
        "the contradiction must be named by the refusal, got: {refused_chain}"
    );

    let dialled = compio_postgres::replication::connect_replication(
        NoTls,
        &config_with(compio_postgres::config::SslMode::VerifyFull),
    )
    .await
    .err()
    .expect("nothing is listening on that port, so this cannot succeed");
    let dialled_chain = common::error_chain(&dialled);
    assert!(
        !dialled_chain.contains("sslrootcert=system"),
        "verify-full is not a contradiction, so validation must let it through: {dialled_chain}"
    );
    // Asserted POSITIVELY: without this, any unrelated early failure that
    // merely lacks the literal would satisfy the check above.
    assert!(
        !common::server_answered(&dialled),
        "the second arm must reach the socket and be refused there: {dialled_chain}"
    );
}

/// Authentication is still governed by `connect_timeout`; `read_timeout`
/// starts only on the post-startup `IDENTIFY_SYSTEM` exchange.
///
/// One peer holds startup longer than three read budgets, then starts but does
/// not finish an IDENTIFY_SYSTEM frame. Pairing both phases on one socket makes
/// it impossible for a disabled deadline to satisfy the startup assertion and
/// masquerade as coverage of the command deadline.
#[compio::test]
async fn replication_read_timeout_starts_after_startup_and_poisons_identify_system() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = ReplicationStub::spawn(|listener| {
            let mut stream = accept_bounded(&listener);
            complete_replication_startup(&mut stream, IDLE_EXPOSURE);
            assert_eq!(expect_simple_query(&mut stream), b"IDENTIFY_SYSTEM\0");

            // Valid RowDescription tag and declared length, but only one of
            // the body bytes. The peer stays connected at an unknown frame
            // boundary until the client retires it.
            stream
                .write_all(&[b'T', 0, 0, 0, 29, 0])
                .expect("write partial IDENTIFY_SYSTEM response");
            stream
                .flush()
                .expect("flush partial IDENTIFY_SYSTEM response");
            expect_disconnect(&mut stream);
        });

        let startup_started = Instant::now();
        let mut replication = compio::time::timeout(
            OPERATION_WATCHDOG,
            compio_postgres::replication::connect_replication(NoTls, &stub_config(server.addr)),
        )
        .await
        .expect("replication startup exceeded its outer watchdog")
        .expect("read_timeout incorrectly covered replication startup");
        assert!(
            startup_started.elapsed() >= IDLE_EXPOSURE,
            "scripted startup did not expose three read budgets"
        );

        let first = compio::time::timeout(OPERATION_WATCHDOG, replication.identify_system())
            .await
            .expect("IDENTIFY_SYSTEM exceeded its outer watchdog")
            .expect_err("partial IDENTIFY_SYSTEM response completed");
        assert!(
            first.is_read_timeout(),
            "IDENTIFY_SYSTEM lost its socket-read timeout: {first}"
        );

        let second = compio::time::timeout(OPERATION_WATCHDOG, replication.identify_system())
            .await
            .expect("poisoned IDENTIFY_SYSTEM retry exceeded its watchdog")
            .expect_err("timed-out replication connection was reused");
        assert!(
            second.is_cancelled(),
            "IDENTIFY_SYSTEM timeout did not poison the session: {second}"
        );
        server.finish();
    })
    .await
    .expect("replication startup/IDENTIFY timeout test exceeded its outer watchdog");
}

/// `START_REPLICATION` has its own response obligation before CopyBoth mode.
/// The call consumes the connection, so physical disconnect is the observable
/// retirement proof; there is deliberately no same-object retry API.
#[compio::test]
async fn a_stalled_start_replication_exchange_times_out_and_retires_its_session() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = ReplicationStub::spawn(|listener| {
            let mut stream = accept_bounded(&listener);
            complete_replication_startup(&mut stream, Duration::ZERO);
            assert_eq!(
                expect_simple_query(&mut stream),
                b"START_REPLICATION SLOT \"deadline_slot\" LOGICAL 0/0 (\"proto_version\" '1', \"publication_names\" '\"deadline_publication\"')\0"
            );

            // CopyBothResponse declares its three-byte body, but only its
            // format byte arrives. This cannot be reproduced with PostgreSQL.
            stream
                .write_all(&[b'W', 0, 0, 0, 7, 0])
                .expect("write partial CopyBothResponse");
            stream.flush().expect("flush partial CopyBothResponse");
            expect_disconnect(&mut stream);
        });

        let replication = compio::time::timeout(
            OPERATION_WATCHDOG,
            compio_postgres::replication::connect_replication(
                NoTls,
                &stub_config(server.addr),
            ),
        )
        .await
        .expect("scripted replication startup exceeded its watchdog")
        .expect("connect to scripted replication peer");
        let start_result = compio::time::timeout(
            OPERATION_WATCHDOG,
            replication.start_logical_replication(start_options()),
        )
        .await
        .expect("START_REPLICATION exceeded its outer watchdog");
        let error = match start_result {
            Ok(_) => panic!("partial CopyBothResponse started a stream"),
            Err(error) => error,
        };
        assert!(
            error.is_read_timeout(),
            "START_REPLICATION lost its socket-read timeout: {error}"
        );
        server.finish();
    })
    .await
    .expect("START_REPLICATION timeout test exceeded its outer watchdog");
}

/// Waiting for the first byte of a CopyBoth frame is legitimate idle time.
/// Keep one `next()` future alive across three read budgets, then require the
/// exact later keepalive. A second interval follows a skipped NoticeResponse,
/// proving every complete frame disarms the clock before the next idle wait.
#[compio::test]
async fn an_awaited_idle_replication_stream_survives_repeated_read_budgets() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let (begin_idle_tx, begin_idle_rx) = std::sync::mpsc::channel();
        let server = ReplicationStub::spawn(move |listener| {
            let mut stream = accept_bounded(&listener);
            complete_replication_startup(&mut stream, Duration::ZERO);
            assert_eq!(expect_simple_query(&mut stream), b"IDENTIFY_SYSTEM\0");
            send_identify_system(&mut stream);
            assert_eq!(
                expect_simple_query(&mut stream),
                b"START_REPLICATION SLOT \"deadline_slot\" LOGICAL 0/0 (\"proto_version\" '1', \"publication_names\" '\"deadline_publication\"')\0"
            );
            send_copy_both(&mut stream);

            begin_idle_rx
                .recv_timeout(SOCKET_WATCHDOG)
                .expect("client never began the first idle read");
            thread::sleep(IDLE_EXPOSURE);
            send_keepalive(&mut stream, 0x100);

            begin_idle_rx
                .recv_timeout(SOCKET_WATCHDOG)
                .expect("client never began the post-notice idle read");
            send_notice(&mut stream);
            thread::sleep(IDLE_EXPOSURE);
            send_keepalive(&mut stream, 0x200);
            expect_disconnect(&mut stream);
        });

        let mut replication = compio::time::timeout(
            OPERATION_WATCHDOG,
            compio_postgres::replication::connect_replication(
                NoTls,
                &stub_config(server.addr),
            ),
        )
        .await
        .expect("scripted replication startup exceeded its watchdog")
        .expect("connect to scripted replication peer");
        let identity = compio::time::timeout(OPERATION_WATCHDOG, replication.identify_system())
            .await
            .expect("IDENTIFY_SYSTEM exceeded its watchdog")
            .expect("scripted IDENTIFY_SYSTEM failed");
        assert_eq!(identity.systemid, "scripted-system");
        assert_eq!(identity.timeline, 1);
        assert_eq!(identity.xlogpos, "0/10");
        assert_eq!(identity.dbname.as_deref(), Some("scripted-db"));
        let mut stream = compio::time::timeout(
            OPERATION_WATCHDOG,
            replication.start_logical_replication(start_options()),
        )
        .await
        .expect("START_REPLICATION exceeded its watchdog")
        .expect("scripted peer refused CopyBoth mode");

        for (iteration, expected_wal_end) in [("first", 0x100), ("post-notice", 0x200)] {
            let started = Instant::now();
            let message = {
                let mut next = std::pin::pin!(stream.next());
                assert!(
                    futures_util::poll!(next.as_mut()).is_pending(),
                    "{iteration} idle read completed before the peer was released"
                );
                begin_idle_tx
                    .send(())
                    .expect("scripted peer dropped its idle trigger");
                compio::time::timeout(OPERATION_WATCHDOG, next.as_mut())
                    .await
                    .unwrap_or_else(|_| panic!("{iteration} idle read exceeded its watchdog"))
                    .unwrap_or_else(|error| {
                        panic!("{iteration} idle read spent the read budget: {error}")
                    })
                    .unwrap_or_else(|| panic!("{iteration} idle stream ended without a frame"))
            };
            assert!(
                started.elapsed() >= IDLE_EXPOSURE,
                "{iteration} idle interval did not expose three read budgets"
            );
            match message {
                ReplicationMessage::PrimaryKeepalive { wal_end, .. } => {
                    assert_eq!(wal_end, expected_wal_end);
                }
                other => panic!("{iteration} idle read returned {other:?}"),
            }
        }

        drop(stream);
        server.finish();
    })
    .await
    .expect("idle replication deadline test exceeded its outer watchdog");
}

/// Once any frame byte arrives, a quiet peer is no longer merely idle: the
/// driver has lost a frame boundary if that read is cancelled. The timeout is
/// therefore terminal, observable both as logical poison and physical EOF.
#[compio::test]
async fn a_mid_frame_replication_stall_times_out_and_poisons_the_stream() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let (send_prefix_tx, send_prefix_rx) = std::sync::mpsc::channel();
        let server = ReplicationStub::spawn(move |listener| {
            let mut stream = accept_bounded(&listener);
            complete_replication_startup(&mut stream, Duration::ZERO);
            assert_eq!(
                expect_simple_query(&mut stream),
                b"START_REPLICATION SLOT \"deadline_slot\" LOGICAL 0/0 (\"proto_version\" '1', \"publication_names\" '\"deadline_publication\"')\0"
            );
            send_copy_both(&mut stream);
            send_prefix_rx
                .recv_timeout(SOCKET_WATCHDOG)
                .expect("client never started the bounded frame read");

            // CopyData length 22 declares an 18-byte PrimaryKeepalive body.
            // The sub-tag and four WAL bytes prove the frame began, but its
            // remaining fields never arrive.
            stream
                .write_all(&[b'd', 0, 0, 0, 22, b'k', 0, 0, 0, 1])
                .expect("write partial PrimaryKeepalive");
            stream
                .flush()
                .expect("flush partial PrimaryKeepalive");
            expect_disconnect(&mut stream);
        });

        let replication = compio::time::timeout(
            OPERATION_WATCHDOG,
            compio_postgres::replication::connect_replication(
                NoTls,
                &stub_config(server.addr),
            ),
        )
        .await
        .expect("scripted replication startup exceeded its watchdog")
        .expect("connect to scripted replication peer");
        let mut stream = compio::time::timeout(
            OPERATION_WATCHDOG,
            replication.start_logical_replication(start_options()),
        )
        .await
        .expect("START_REPLICATION exceeded its watchdog")
        .expect("scripted peer refused CopyBoth mode");

        let first = {
            let mut next = std::pin::pin!(stream.next());
            assert!(
                futures_util::poll!(next.as_mut()).is_pending(),
                "stream produced a frame before the peer sent its prefix"
            );
            send_prefix_tx
                .send(())
                .expect("scripted peer dropped its frame-prefix trigger");
            compio::time::timeout(OPERATION_WATCHDOG, next.as_mut())
                .await
                .expect("partial replication frame exceeded its outer watchdog")
                .expect_err("partial PrimaryKeepalive completed")
        };
        assert!(
            first.is_read_timeout(),
            "mid-frame stall lost its socket-read timeout: {first}"
        );

        let second = compio::time::timeout(OPERATION_WATCHDOG, stream.next())
            .await
            .expect("poisoned stream retry exceeded its watchdog")
            .expect_err("timed-out replication framing was reused");
        assert!(
            second.is_cancelled(),
            "mid-frame timeout did not poison the stream: {second}"
        );
        server.finish();
    })
    .await
    .expect("mid-frame replication timeout test exceeded its outer watchdog");
}

/// An out-of-range `start_lsn` must be REFUSED, not silently replaced by 0.
///
/// MEASURED against the test server on 2026-08-23, because the two parsers
/// involved disagree and neither is obvious. `START_REPLICATION ... LOGICAL
/// 0/100000000` -- nine hex digits in the low half -- is ACCEPTED by the
/// replication grammar: the command reaches the slot lookup and fails with
/// `replication slot "..." does not exist`, i.e. never on the LSN. But
/// `SELECT '0/100000000'::pg_lsn` is REFUSED with `invalid input syntax for
/// type pg_lsn`. So a server accepts a value that does not fit this driver's
/// u32-per-half representation.
///
/// `parse_lsn` returns `None` there, and the call site paired that with
/// `unwrap_or(0)`. Zero is not a neutral default for an LSN -- it is the start
/// of WAL. The server would have begun streaming from wherever it read the
/// oversized value while `LsnTracker` reported position 0, so every standby
/// status update afterwards acknowledged a position the stream had never been
/// at, and nothing anywhere reported a problem.
///
/// The refusal happens BEFORE the command is sent, so a start position this
/// driver cannot track never starts a replication stream on the server.
#[compio::test]
async fn an_unrepresentable_start_lsn_is_refused_before_replication_starts() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = ReplicationStub::spawn(move |listener| {
            let mut stream = accept_bounded(&listener);
            complete_replication_startup(&mut stream, Duration::ZERO);
            // No START_REPLICATION is expected: the driver must refuse the LSN
            // without issuing a command. If it issues one anyway, this peer
            // never answers and the watchdog fires -- a distinguishable
            // failure from the assertion below.
            expect_disconnect(&mut stream);
        });

        let replication = compio::time::timeout(
            OPERATION_WATCHDOG,
            compio_postgres::replication::connect_replication(NoTls, &stub_config(server.addr)),
        )
        .await
        .expect("scripted replication startup exceeded its watchdog")
        .expect("connect to scripted replication peer");

        let error = replication
            .start_logical_replication(StartReplicationOptions {
                slot_name: "deadline_slot",
                start_lsn: "0/100000000",
                proto_version: 1,
                publication_names: &["deadline_publication"],
                ..Default::default()
            })
            .await
            .err()
            .expect("an LSN this driver cannot represent must not start a stream");

        let rendered = format!("{error}");
        let chain = std::iter::successors(std::error::Error::source(&error), |error| {
            std::error::Error::source(*error)
        })
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join("; ");
        assert!(
            rendered.contains("start_lsn") || chain.contains("start_lsn"),
            "the refusal must name start_lsn so the caller knows which value is \
             wrong: {rendered} / {chain}"
        );

        server.finish();
    })
    .await
    .expect("unrepresentable start_lsn test exceeded its outer watchdog");
}

/// One variable away: a well-formed LSN must still start a stream, or the
/// refusal above could be satisfied by refusing every start_lsn.
#[compio::test]
async fn a_representable_start_lsn_still_starts_replication() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = ReplicationStub::spawn(move |listener| {
            let mut stream = accept_bounded(&listener);
            complete_replication_startup(&mut stream, Duration::ZERO);
            assert_eq!(
                expect_simple_query(&mut stream),
                b"START_REPLICATION SLOT \"deadline_slot\" LOGICAL 0/16B3750 (\"proto_version\" '1', \"publication_names\" '\"deadline_publication\"')\0"
            );
            send_copy_both(&mut stream);
            expect_disconnect(&mut stream);
        });

        let replication = compio::time::timeout(
            OPERATION_WATCHDOG,
            compio_postgres::replication::connect_replication(NoTls, &stub_config(server.addr)),
        )
        .await
        .expect("scripted replication startup exceeded its watchdog")
        .expect("connect to scripted replication peer");

        let stream = compio::time::timeout(
            OPERATION_WATCHDOG,
            replication.start_logical_replication(StartReplicationOptions {
                slot_name: "deadline_slot",
                start_lsn: "0/16B3750",
                proto_version: 1,
                publication_names: &["deadline_publication"],
                ..Default::default()
            }),
        )
        .await
        .expect("START_REPLICATION exceeded its watchdog")
        .expect("a representable start_lsn must start a stream");

        drop(stream);
        server.finish();
    })
    .await
    .expect("representable start_lsn test exceeded its outer watchdog");
}

/// A malformed `IDENTIFY_SYSTEM` row must be refused, not filled in with
/// plausible-looking defaults.
///
/// Every field was defaulted: a missing or NULL `systemid` became `""`, a
/// missing, NULL or unparseable `timeline` became `0`, and `xlogpos` became
/// `""`. Those are not neutral values. `systemid` is the CLUSTER identity, and
/// callers compare it to notice they have been failed over onto a different
/// cluster -- two empty strings compare equal, so the check silently passes
/// exactly when it should fire. `0` is not a valid timeline either;
/// PostgreSQL numbers them from 1.
///
/// A conforming server always sends all three as non-NULL, so reaching this
/// needs a hostile or broken peer -- the threat model `tests/hostile_peer.rs`
/// and the stubs in this file already work in. `dbname` is deliberately NOT in
/// this test: it is genuinely NULL on a non-database-specific replication
/// connection, which is why it alone is modelled as an `Option`.
#[compio::test]
async fn a_null_field_in_identify_system_is_refused_rather_than_defaulted() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = ReplicationStub::spawn(move |listener| {
            let mut stream = accept_bounded(&listener);
            complete_replication_startup(&mut stream, Duration::ZERO);
            assert_eq!(expect_simple_query(&mut stream), b"IDENTIFY_SYSTEM\0");

            // Four fields, but systemid and timeline are NULL (length -1).
            let mut row = Vec::new();
            row.extend_from_slice(&4u16.to_be_bytes());
            row.extend_from_slice(&(-1i32).to_be_bytes()); // systemid NULL
            row.extend_from_slice(&(-1i32).to_be_bytes()); // timeline NULL
            for field in [b"0/10".as_slice(), b"scripted-db".as_slice()] {
                row.extend_from_slice(&i32::try_from(field.len()).unwrap().to_be_bytes());
                row.extend_from_slice(field);
            }

            let mut response = backend_frame(b'D', &row);
            response.extend_from_slice(&backend_frame(b'C', b"IDENTIFY_SYSTEM\0"));
            response.extend_from_slice(&backend_frame(b'Z', b"I"));
            let _ = stream.write_all(&response);
            let _ = stream.flush();
            expect_disconnect(&mut stream);
        });

        let mut replication = compio::time::timeout(
            OPERATION_WATCHDOG,
            compio_postgres::replication::connect_replication(NoTls, &stub_config(server.addr)),
        )
        .await
        .expect("scripted replication startup exceeded its watchdog")
        .expect("connect to scripted replication peer");

        let outcome =
            compio::time::timeout(OPERATION_WATCHDOG, replication.identify_system()).await;
        let error = match outcome {
            Err(_) => panic!("identify_system hung on a malformed row"),
            Ok(Ok(identity)) => panic!(
                "a NULL systemid and timeline were accepted as {:?} / {}",
                identity.systemid, identity.timeline
            ),
            Ok(Err(error)) => error,
        };

        let rendered = format!("{error}");
        let chain = std::iter::successors(std::error::Error::source(&error), |error| {
            std::error::Error::source(*error)
        })
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join("; ");
        assert!(
            rendered.to_lowercase().contains("identify_system")
                || chain.to_lowercase().contains("identify_system"),
            "the refusal should name IDENTIFY_SYSTEM: {rendered} / {chain}"
        );

        // The peer waits for a close; the refusal leaves the connection in the
        // caller's hands, so this test has to end the session itself.
        drop(replication);

        server.finish();
    })
    .await
    .expect("malformed IDENTIFY_SYSTEM test exceeded its outer watchdog");
}

/// The reply a walsender sends when `IDENTIFY_SYSTEM` produced no row at all:
/// a completion and a `ReadyForQuery`, and nothing in between.
fn send_identify_system_without_a_row(stream: &mut TcpStream) {
    let mut response = backend_frame(b'C', b"IDENTIFY_SYSTEM\0");
    response.extend_from_slice(&backend_frame(b'Z', b"I"));
    stream
        .write_all(&response)
        .expect("write rowless IDENTIFY_SYSTEM response");
    stream
        .flush()
        .expect("flush rowless IDENTIFY_SYSTEM response");
}

/// An `IDENTIFY_SYSTEM` response that carried NO `DataRow` must be refused.
///
/// The "refused rather than defaulted" rule was applied inside
/// `parse_identify_system_row`, which only runs when a row arrives. The
/// response loop above it seeded `systemid = String::new()`, `timeline = 0` and
/// `xlogpos = String::new()` and returned them at `ReadyForQuery`, so a
/// response with no row returned `Ok` carrying exactly the three sentinels the
/// row parser exists to reject. `systemid` is the CLUSTER identity a caller
/// compares to notice a failover, and two empty strings compare EQUAL: the
/// check passes silently in precisely the case it exists to catch.
#[compio::test]
async fn identify_system_refuses_a_response_that_carried_no_row() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = ReplicationStub::spawn(move |listener| {
            let mut stream = accept_bounded(&listener);
            complete_replication_startup(&mut stream, Duration::ZERO);
            assert_eq!(expect_simple_query(&mut stream), b"IDENTIFY_SYSTEM\0");
            send_identify_system_without_a_row(&mut stream);
            expect_disconnect(&mut stream);
        });

        let mut replication = compio::time::timeout(
            OPERATION_WATCHDOG,
            compio_postgres::replication::connect_replication(NoTls, &stub_config(server.addr)),
        )
        .await
        .expect("scripted replication startup exceeded its watchdog")
        .expect("connect to scripted replication peer");

        let outcome =
            compio::time::timeout(OPERATION_WATCHDOG, replication.identify_system()).await;
        let error = match outcome {
            Err(_) => panic!("identify_system hung on a rowless response"),
            Ok(Ok(identity)) => panic!(
                "a response carrying no row was accepted as systemid {:?}, timeline {}, \
                 xlogpos {:?}",
                identity.systemid, identity.timeline, identity.xlogpos
            ),
            Ok(Err(error)) => error,
        };

        let rendered = common::error_chain(&error).to_lowercase();
        assert!(
            rendered.contains("identify_system"),
            "the refusal should name IDENTIFY_SYSTEM: {rendered}"
        );

        drop(replication);
        server.finish();
    })
    .await
    .expect("rowless IDENTIFY_SYSTEM test exceeded its outer watchdog");
}

/// One variable away from the test above: the SAME script with a row in it must
/// still return the identity. A refusal that fired on every response would
/// satisfy the assertion above and turn this red.
#[compio::test]
async fn identify_system_accepts_a_response_that_carried_a_row() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = ReplicationStub::spawn(move |listener| {
            let mut stream = accept_bounded(&listener);
            complete_replication_startup(&mut stream, Duration::ZERO);
            assert_eq!(expect_simple_query(&mut stream), b"IDENTIFY_SYSTEM\0");
            send_identify_system(&mut stream);
            expect_disconnect(&mut stream);
        });

        let mut replication = compio::time::timeout(
            OPERATION_WATCHDOG,
            compio_postgres::replication::connect_replication(NoTls, &stub_config(server.addr)),
        )
        .await
        .expect("scripted replication startup exceeded its watchdog")
        .expect("connect to scripted replication peer");

        let identity = compio::time::timeout(OPERATION_WATCHDOG, replication.identify_system())
            .await
            .expect("identify_system exceeded its watchdog")
            .expect("a well-formed IDENTIFY_SYSTEM row must be accepted");

        assert_eq!(identity.systemid, "scripted-system");
        assert_eq!(identity.timeline, 1);
        assert_eq!(identity.xlogpos, "0/10");
        assert_eq!(identity.dbname.as_deref(), Some("scripted-db"));

        drop(replication);
        server.finish();
    })
    .await
    .expect("well-formed IDENTIFY_SYSTEM test exceeded its outer watchdog");
}

/// A `ParameterStatus` inside the response must not fail the command.
///
/// `ParameterStatus`, `NoticeResponse` and `NotificationResponse` are
/// asynchronous: the protocol lets the backend interleave them into any
/// response, and `connect_raw.rs` / `connection.rs` both fold them out of the
/// query path for exactly that reason. This loop skipped `NoticeResponse` only,
/// so a walsender that reported a changed GUC mid-`IDENTIFY_SYSTEM` -- a
/// conforming server doing a conforming thing -- fell into the
/// unexpected-message arm and failed the command.
#[compio::test]
async fn identify_system_tolerates_an_asynchronous_parameter_status() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = ReplicationStub::spawn(move |listener| {
            let mut stream = accept_bounded(&listener);
            complete_replication_startup(&mut stream, Duration::ZERO);
            assert_eq!(expect_simple_query(&mut stream), b"IDENTIFY_SYSTEM\0");
            stream
                .write_all(&backend_frame(b'S', b"TimeZone\0UTC\0"))
                .expect("write asynchronous ParameterStatus");
            stream.flush().expect("flush asynchronous ParameterStatus");
            send_identify_system(&mut stream);
            expect_disconnect(&mut stream);
        });

        let mut replication = compio::time::timeout(
            OPERATION_WATCHDOG,
            compio_postgres::replication::connect_replication(NoTls, &stub_config(server.addr)),
        )
        .await
        .expect("scripted replication startup exceeded its watchdog")
        .expect("connect to scripted replication peer");

        let identity = compio::time::timeout(OPERATION_WATCHDOG, replication.identify_system())
            .await
            .expect("identify_system exceeded its watchdog")
            .expect("an asynchronous ParameterStatus must not fail IDENTIFY_SYSTEM");
        assert_eq!(identity.systemid, "scripted-system");

        drop(replication);
        server.finish();
    })
    .await
    .expect("asynchronous ParameterStatus test exceeded its outer watchdog");
}

/// A message this phase cannot account for retires the session.
///
/// An `ErrorResponse` and a rejected row both carry their reason to
/// `ReadyForQuery`, because PostgreSQL guarantees one arrives. A message the
/// phase cannot account for carries no such guarantee, so the driver gives up
/// where it stands and the response is left undrained -- precisely the state
/// the `ErrorResponse` test below proves is dangerous. The connection therefore
/// has to refuse everything afterwards rather than answer the next command from
/// a frame belonging to this one.
#[compio::test]
async fn an_unaccountable_message_retires_the_replication_session() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = ReplicationStub::spawn(move |listener| {
            let mut stream = accept_bounded(&listener);
            complete_replication_startup(&mut stream, Duration::ZERO);
            assert_eq!(expect_simple_query(&mut stream), b"IDENTIFY_SYSTEM\0");
            // EmptyQueryResponse: a real backend message, well framed, and one
            // no IDENTIFY_SYSTEM response can contain. The rest of a complete
            // response follows it, so the failure is the message and not a
            // short read.
            let mut response = backend_frame(b'I', b"");
            response.extend_from_slice(&identify_system_response());
            stream
                .write_all(&response)
                .expect("write unaccountable IDENTIFY_SYSTEM response");
            stream
                .flush()
                .expect("flush unaccountable IDENTIFY_SYSTEM response");
            expect_disconnect(&mut stream);
        });

        let mut replication = compio::time::timeout(
            OPERATION_WATCHDOG,
            compio_postgres::replication::connect_replication(NoTls, &stub_config(server.addr)),
        )
        .await
        .expect("scripted replication startup exceeded its watchdog")
        .expect("connect to scripted replication peer");

        let first = compio::time::timeout(OPERATION_WATCHDOG, replication.identify_system())
            .await
            .expect("identify_system exceeded its watchdog")
            .expect_err("a message this phase cannot account for must fail the command");
        assert!(
            !first.is_cancelled(),
            "the first call must report the message, not the refusal: {}",
            common::error_chain(&first)
        );

        let second = compio::time::timeout(OPERATION_WATCHDOG, replication.identify_system())
            .await
            .expect("the retried identify_system exceeded its watchdog")
            .expect_err("a retired session must not answer a second command");
        assert!(
            second.is_cancelled(),
            "the session was left undrained, so it must be refused rather than \
             answered from the stale frames: {}",
            common::error_chain(&second)
        );

        drop(replication);
        server.finish();
    })
    .await
    .expect("unaccountable message test exceeded its outer watchdog");
}

/// A server-sent `ErrorResponse` must not leave the session one frame behind.
///
/// The response loop returned the moment it saw the `ErrorResponse`, leaving
/// the `ReadyForQuery` that closes every simple-query response sitting in the
/// read buffer. The NEXT command on that connection then read the stale frame
/// as its own reply: a second `IDENTIFY_SYSTEM` parsed the leftover
/// `ReadyForQuery`, broke out of its loop before its own response arrived, and
/// -- with the sentinels above still in place -- reported `Ok` with an empty
/// identity for a command the server had not answered yet.
#[compio::test]
async fn an_identify_system_error_leaves_the_session_able_to_answer_the_next_command() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = ReplicationStub::spawn(move |listener| {
            let mut stream = accept_bounded(&listener);
            complete_replication_startup(&mut stream, Duration::ZERO);

            assert_eq!(expect_simple_query(&mut stream), b"IDENTIFY_SYSTEM\0");
            // A complete simple-query response: the failure, then the
            // ReadyForQuery that ends the phase. Written as ONE flush so the
            // driver's read pulls both into its buffer, which is what makes the
            // stale frame reachable by the next command.
            let mut refusal =
                backend_frame(b'E', b"SERROR\0C57P03\0Mscripted identify refusal\0\0");
            refusal.extend_from_slice(&backend_frame(b'Z', b"I"));
            stream
                .write_all(&refusal)
                .expect("write scripted IDENTIFY_SYSTEM refusal");
            stream
                .flush()
                .expect("flush scripted IDENTIFY_SYSTEM refusal");

            assert_eq!(expect_simple_query(&mut stream), b"IDENTIFY_SYSTEM\0");
            send_identify_system(&mut stream);
            expect_disconnect(&mut stream);
        });

        let mut replication = compio::time::timeout(
            OPERATION_WATCHDOG,
            compio_postgres::replication::connect_replication(NoTls, &stub_config(server.addr)),
        )
        .await
        .expect("scripted replication startup exceeded its watchdog")
        .expect("connect to scripted replication peer");

        let refusal = compio::time::timeout(OPERATION_WATCHDOG, replication.identify_system())
            .await
            .expect("the refused identify_system exceeded its watchdog")
            .expect_err("the server refused IDENTIFY_SYSTEM, so this must be an error");
        let rendered = common::error_chain(&refusal);
        assert!(
            rendered.contains("57P03"),
            "the server's SQLSTATE must survive: {rendered}"
        );

        let identity = compio::time::timeout(OPERATION_WATCHDOG, replication.identify_system())
            .await
            .expect("the retried identify_system exceeded its watchdog")
            .expect("the session was left in step, so the retry must answer");
        assert_eq!(
            identity.systemid, "scripted-system",
            "the retry answered from a stale frame rather than from the server's reply"
        );

        drop(replication);
        server.finish();
    })
    .await
    .expect("IDENTIFY_SYSTEM resynchronisation test exceeded its outer watchdog");
}

/// A FATAL `ErrorResponse` must reach the caller even though no `ReadyForQuery`
/// follows it.
///
/// The response loop drains to `ReadyForQuery` and only THEN reports the
/// `ErrorResponse` it stashed. PostgreSQL does not send a `ReadyForQuery` after
/// a FATAL error - it writes the `ErrorResponse` and closes the connection - so
/// the next read fails, and the `?` on it discarded the server's diagnostic and
/// reported the transport error in its place. `57P01`, `57P03` and
/// `idle_session_timeout` on a walsender all take this exact path.
///
/// The pairing with the test above is the one variable that matters: there the
/// same `ErrorResponse` is followed by a `ReadyForQuery` and the SQLSTATE
/// already survived. Here it is not, and only the read-error arm can carry it.
#[compio::test]
async fn a_fatal_identify_system_error_survives_the_close_that_follows_it() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let server = ReplicationStub::spawn(move |listener| {
            let mut stream = accept_bounded(&listener);
            complete_replication_startup(&mut stream, Duration::ZERO);

            assert_eq!(expect_simple_query(&mut stream), b"IDENTIFY_SYSTEM\0");
            // NO ReadyForQuery: a FATAL is the whole response, and the server
            // hangs up behind it. That is what makes the driver's next read
            // fail rather than deliver a terminator.
            stream
                .write_all(&backend_frame(
                    b'E',
                    b"SFATAL\0C57P01\0Mterminating connection due to administrator command\0\0",
                ))
                .expect("write scripted FATAL refusal");
            stream.flush().expect("flush scripted FATAL refusal");
            stream
                .shutdown(std::net::Shutdown::Both)
                .expect("close the scripted connection behind the FATAL");
        });

        let mut replication = compio::time::timeout(
            OPERATION_WATCHDOG,
            compio_postgres::replication::connect_replication(NoTls, &stub_config(server.addr)),
        )
        .await
        .expect("scripted replication startup exceeded its watchdog")
        .expect("connect to scripted replication peer");

        let refusal = compio::time::timeout(OPERATION_WATCHDOG, replication.identify_system())
            .await
            .expect("the refused identify_system exceeded its watchdog")
            .expect_err("the server refused IDENTIFY_SYSTEM, so this must be an error");
        let rendered = common::error_chain(&refusal);
        assert!(
            rendered.contains("57P01"),
            "the server's SQLSTATE was replaced by the close that followed it: {rendered}"
        );
        assert!(
            rendered.contains("terminating connection due to administrator command"),
            "the server's message was replaced by the close that followed it: {rendered}"
        );

        // The response ended at an unknown frame boundary, so the session must
        // be retired rather than reused - the same ruling the unaccountable
        // message test makes, and the reason the read-error arm cannot simply
        // swap the error it returns.
        let second = compio::time::timeout(OPERATION_WATCHDOG, replication.identify_system())
            .await
            .expect("the retried identify_system exceeded its watchdog")
            .expect_err("a retired session must not answer a second command");
        assert!(
            second.is_cancelled(),
            "the session survived a transport error mid-response: {}",
            common::error_chain(&second)
        );

        drop(replication);
        server.finish();
    })
    .await
    .expect("FATAL IDENTIFY_SYSTEM test exceeded its outer watchdog");
}
