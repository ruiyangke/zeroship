//! A connect that runs out of time names the stage it had reached.
//!
//! The failure this covers reported only "pool warm-up exceeded its
//! `warm_up_timeout`", which does not say whether name resolution hung, the
//! `TCP` dial never completed, the `SSLRequest` went unanswered, the TLS
//! handshake stalled, the startup and authentication exchange stalled, or a
//! post-connect read stalled. Every one of those has a different owner and a
//! different remedy, so the report has to name the stage.
//!
//! Each test drives a scripted peer that stalls at exactly one stage. What the
//! signal proves depends on which side sends it, and only one case is a bound
//! rather than a proof:
//!
//! * A script that signals after READING the client's bytes for that stage
//!   proves the client reached it. The peer reads the startup packet (startup),
//!   the `SSLRequest` (the SSL request), or - after answering `S` - the
//!   `ClientHello` record header (the TLS handshake), and signals only once
//!   those bytes have arrived. The test waits on that signal, so the stage the
//!   driver had reached is the peer's progress rather than a side of a race.
//! * A script whose write TRIGGERS the stage but which reads no later client
//!   bytes can only bound the stage; the write does not prove the client
//!   consumed it. `answer_auth_ok_then_stall` is the one such script: it
//!   answers `AuthenticationOk`, after which the client reaches post-connect
//!   setup without sending a byte the peer could read, so its signal is a
//!   bound - the peer has read the startup packet, and the stage advances only
//!   when the client reads the `AuthenticationOk`.
//! * The two `after_connect` tests signal from INSIDE the hook, not from the
//!   peer. The hook runs only after its connect has completed and returned, so
//!   the signal proves the connect finished before the deadline expired inside
//!   the hook, which is not itself a connect stage.
//!
//! The assertions are on the STRUCTURED stage ([`Error::connect_phase`]) and on
//! the rendered message, so a stage recorded at the wrong step fails the test
//! that owns that step and leaves the others green.
//!
//! The stages are reached locally where a peer can express them: the SSL
//! request, the TLS handshake, startup and authentication, and post-connect
//! setup. Name resolution and the `TCP` dial are not scripted here; their stage
//! is pinned by the unit tests in `src/connect.rs`.
//!
//! Every deadline here only has to expire: the peer signals the stage it
//! reached and then stalls there, so the deadline reports that stage. Load
//! slows the peer, which can only make the reached stage earlier, never later;
//! the deadline is a bound far longer than the local work of reaching a stage,
//! not a value tuned to the peer's speed.

use compio_postgres::config::SslMode;
use compio_postgres::error::{ConnectPhase, PoolBudget};
use compio_postgres::{Config, Error, NoTls, Pool, PoolConfig};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::support;

/// Bounds the scripted peer's accept and reads, so a client that never arrives
/// ends the peer instead of leaking its thread. It is not a deadline under
/// test: every deadline under test is shorter.
const PEER_WATCHDOG: Duration = Duration::from_secs(5);
const THREAD_WATCHDOG: Duration = Duration::from_secs(5);
/// Bounds a test against a client-side hang that is not the deadline under
/// test.
const OPERATION_WATCHDOG: Duration = Duration::from_secs(15);
/// The deadline under test. It only has to outlast the local work of reaching
/// the peer's stall point, which the peer signals before this can matter, and
/// then expire while the peer stalls. It is never the subject.
const STALL_DEADLINE: Duration = Duration::from_secs(1);

#[cfg(feature = "tls")]
const SSL_REQUEST_CODE: u32 = 80_877_103;

/// A scripted peer running one connection script on a private thread.
struct Peer {
    port: u16,
    stalled: Option<futures_channel::oneshot::Receiver<()>>,
    done: mpsc::Receiver<()>,
    thread: thread::JoinHandle<()>,
}

/// The script's way to say it has reached the point it will stall at. The
/// sender is consumed by the first signal; a script signals once.
struct StallSignal(Option<futures_channel::oneshot::Sender<()>>);

impl StallSignal {
    fn reached(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

impl Peer {
    fn spawn(script: fn(&TcpListener, &mut StallSignal) -> std::io::Result<()>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind scripted peer");
        let port = listener.local_addr().expect("scripted peer address").port();
        let (stall_tx, stalled) = futures_channel::oneshot::channel();
        let (done_tx, done) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut signal = StallSignal(Some(stall_tx));
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                script(&listener, &mut signal)
                    .unwrap_or_else(|error| panic!("scripted peer failed: {error}"));
            }));
            let _ = done_tx.send(());
            if let Err(panic) = outcome {
                std::panic::resume_unwind(panic);
            }
        });
        Self {
            port,
            stalled: Some(stalled),
            done,
            thread,
        }
    }

    /// Wait until the script reports it has reached the point it stalls at.
    /// Awaiting the signal keeps the runtime free to drive the client; the
    /// caller's operation watchdog bounds a peer that never gets there.
    async fn reached_stall(&mut self) {
        let stalled = self.stalled.take().expect("stall signal already consumed");
        stalled
            .await
            .expect("the scripted peer dropped its stall signal");
    }

    fn finish(self) {
        self.done
            .recv_timeout(THREAD_WATCHDOG)
            .expect("scripted peer exceeded its thread watchdog");
        self.thread
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    }
}

/// Accept the client this script is waiting for. Every script is reached only
/// after the client has connected, so the accept returns promptly; a client
/// that never arrives leaves the peer parked here and [`Peer::finish`] fails
/// the test on its thread watchdog rather than the script racing a deadline.
fn accept_bounded(listener: &TcpListener) -> std::io::Result<TcpStream> {
    let (stream, _) = listener.accept()?;
    stream.set_read_timeout(Some(PEER_WATCHDOG))?;
    stream.set_write_timeout(Some(PEER_WATCHDOG))?;
    Ok(stream)
}

/// Read one length-prefixed packet without interpreting it. The SSL request
/// and the plaintext startup message share this framing.
fn read_packet(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if !(8..=10_000).contains(&length) {
        return Err(std::io::Error::other(format!(
            "implausible startup length {length}"
        )));
    }
    let mut body = vec![0u8; length - 4];
    stream.read_exact(&mut body)?;
    Ok(())
}

#[cfg(feature = "tls")]
fn read_ssl_request(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut request = [0u8; 8];
    stream.read_exact(&mut request)?;
    if u32::from_be_bytes(request[..4].try_into().unwrap()) != 8
        || u32::from_be_bytes(request[4..].try_into().unwrap()) != SSL_REQUEST_CODE
    {
        return Err(std::io::Error::other("the client did not send SSLRequest"));
    }
    Ok(())
}

fn backend_frame(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + body.len());
    frame.push(tag);
    frame.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
    frame.extend_from_slice(body);
    frame
}

/// `AuthenticationOk` + `BackendKeyData` + `ReadyForQuery`, the shortest
/// successful startup a scripted peer can send.
fn write_startup_ok(stream: &mut TcpStream, process_id: i32) -> std::io::Result<()> {
    let mut response = backend_frame(b'R', &0u32.to_be_bytes());
    let mut key = process_id.to_be_bytes().to_vec();
    key.extend_from_slice(&1234i32.to_be_bytes());
    response.extend_from_slice(&backend_frame(b'K', &key));
    response.extend_from_slice(&backend_frame(b'Z', b"I"));
    stream.write_all(&response)?;
    stream.flush()
}

/// Keep the connection open until the client hangs up. A reset is the client
/// leaving with bytes it never read, not a peer failure.
fn hold(mut stream: TcpStream) -> std::io::Result<()> {
    let mut scratch = [0u8; 256];
    loop {
        match stream.read(&mut scratch) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::ConnectionReset | ErrorKind::BrokenPipe | ErrorKind::TimedOut
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        }
    }
}

// -- scripts ---------------------------------------------------------------

/// Accept, read the startup packet, then never answer. The client stalls in
/// the startup and authentication exchange.
fn stall_after_startup(listener: &TcpListener, signal: &mut StallSignal) -> std::io::Result<()> {
    let mut stream = accept_bounded(listener)?;
    read_packet(&mut stream)?;
    signal.reached();
    hold(stream)
}

/// Accept, answer `AuthenticationOk`, then never send the startup parameters
/// or `ReadyForQuery`. The client stalls in post-connect setup, but the signal
/// only bounds the reached stage: the client sends nothing after
/// `AuthenticationOk`, so the peer cannot read its way past startup.
fn answer_auth_ok_then_stall(
    listener: &TcpListener,
    signal: &mut StallSignal,
) -> std::io::Result<()> {
    let mut stream = accept_bounded(listener)?;
    read_packet(&mut stream)?;
    stream.write_all(&backend_frame(b'R', &0u32.to_be_bytes()))?;
    stream.flush()?;
    signal.reached();
    hold(stream)
}

/// Connection one completes startup and stays open; connection two reads its
/// startup packet and stalls. Lets a warm pool hand out the first and stall
/// opening the second on demand.
fn first_ok_then_second_stalls(
    listener: &TcpListener,
    signal: &mut StallSignal,
) -> std::io::Result<()> {
    let mut first = accept_bounded(listener)?;
    read_packet(&mut first)?;
    write_startup_ok(&mut first, 7)?;
    let mut second = accept_bounded(listener)?;
    read_packet(&mut second)?;
    signal.reached();
    hold(second)?;
    hold(first)
}

/// Answer one startup exchange and stay open. The client's post-connect work,
/// including its `after_connect` hook, follows the startup. This script does
/// not signal: the caller signals from inside the hook, which runs only after
/// the connect has completed, so that signal proves the connect finished.
fn answer_startup_then_hold(
    listener: &TcpListener,
    _signal: &mut StallSignal,
) -> std::io::Result<()> {
    let mut stream = accept_bounded(listener)?;
    read_packet(&mut stream)?;
    write_startup_ok(&mut stream, 7)?;
    hold(stream)
}

/// Answer startup on the first two connections, then keep both open. Lets a
/// warm pool hand out the first while the second's `after_connect` hook stalls.
/// This script does not signal: the caller signals from inside the hook, which
/// runs only after the second connect has completed, so that signal proves the
/// on-demand connect finished.
fn answer_two_startups_then_hold(
    listener: &TcpListener,
    _signal: &mut StallSignal,
) -> std::io::Result<()> {
    let mut first = accept_bounded(listener)?;
    read_packet(&mut first)?;
    write_startup_ok(&mut first, 7)?;
    let mut second = accept_bounded(listener)?;
    read_packet(&mut second)?;
    write_startup_ok(&mut second, 8)?;
    hold(second)?;
    hold(first)
}

/// Accept, read the `SSLRequest`, then never answer. The client stalls in the
/// SSL request negotiation.
#[cfg(feature = "tls")]
fn stall_ssl_request(listener: &TcpListener, signal: &mut StallSignal) -> std::io::Result<()> {
    let mut stream = accept_bounded(listener)?;
    read_ssl_request(&mut stream)?;
    signal.reached();
    hold(stream)
}

/// Accept, answer `S` to the `SSLRequest`, read the `ClientHello` the client
/// opens the handshake with, then never speak TLS. The client stalls partway
/// through the TLS handshake, and reading the record header before signalling
/// proves it reached the handshake.
#[cfg(feature = "tls")]
fn accept_tls_then_stall(listener: &TcpListener, signal: &mut StallSignal) -> std::io::Result<()> {
    let mut stream = accept_bounded(listener)?;
    read_ssl_request(&mut stream)?;
    stream.write_all(b"S")?;
    stream.flush()?;
    read_client_hello_record(&mut stream)?;
    signal.reached();
    hold(stream)
}

/// Read the five-byte TLS record header the client sends after the server
/// answers `S`, and require it to be a handshake record. The `ClientHello`
/// is the first record of the handshake, so its header arriving proves the
/// client left the `SSLRequest` negotiation and entered the TLS handshake.
#[cfg(feature = "tls")]
fn read_client_hello_record(stream: &mut TcpStream) -> std::io::Result<()> {
    const HANDSHAKE_CONTENT_TYPE: u8 = 0x16;
    let mut header = [0u8; 5];
    stream.read_exact(&mut header)?;
    if header[0] != HANDSHAKE_CONTENT_TYPE {
        return Err(std::io::Error::other(format!(
            "the client did not open a TLS handshake after `S`: record content type {:#04x}",
            header[0]
        )));
    }
    Ok(())
}

// -- helpers ---------------------------------------------------------------

fn config(port: u16, ssl_mode: SslMode) -> Config {
    let mut config = Config::new();
    config
        .user("scripted-user")
        .hostaddr([127, 0, 0, 1].into())
        .port(port)
        .ssl_mode(ssl_mode);
    config
}

fn assert_phase(error: &Error, expected: ConnectPhase, stage: &str) {
    assert_eq!(
        error.connect_phase(),
        Some(expected),
        "{stage}: the connect phase was not recorded: {error:?}"
    );
    assert!(
        support::error_chain(error).contains(expected.as_str()),
        "{stage}: the rendered message does not name {expected:?}: {}",
        support::error_chain(error)
    );
}

fn assert_connect_timeout(error: &Error, expected: ConnectPhase, stage: &str) {
    assert_phase(error, expected, stage);
    assert!(
        !error.is_pool_timeout(),
        "{stage}: a Config::connect_timeout expiry was classified as a pool timeout: {error:?}"
    );
    let timed_out = support::error_chain(error).contains("timed out")
        || support::error_chain(error).contains("timeout");
    assert!(timed_out, "{stage}: the error is not a timeout: {error:?}");
}

fn assert_warm_up_timeout(error: &Error, expected: ConnectPhase, stage: &str) {
    assert_phase(error, expected, stage);
    assert!(
        error.is_pool_timeout(),
        "{stage}: a warm-up expiry lost its pool-timeout classification: {error:?}"
    );
    assert_eq!(
        error.pool_timeout_budget(),
        Some(PoolBudget::WarmUp),
        "{stage}: the warm-up expiry was classified as another budget: {error:?}"
    );
}

fn assert_acquire_timeout(error: &Error, expected: ConnectPhase, stage: &str) {
    assert_phase(error, expected, stage);
    assert!(
        error.is_pool_timeout(),
        "{stage}: an acquire expiry lost its pool-timeout classification: {error:?}"
    );
    assert_eq!(
        error.pool_timeout_budget(),
        Some(PoolBudget::Acquire),
        "{stage}: the acquire expiry was classified as another budget: {error:?}"
    );
}

// -- startup and authentication --------------------------------------------

/// A peer that reads the startup packet and never answers: `Config::connect`
/// with a `connect_timeout` names startup and authentication.
#[compio::test]
async fn a_connect_timeout_names_startup_when_the_server_never_answers() {
    compio::time::timeout(
        OPERATION_WATCHDOG,
        Box::pin(async {
            let mut peer = Peer::spawn(stall_after_startup);
            let mut config = config(peer.port, SslMode::Disable);
            config.connect_timeout(STALL_DEADLINE);

            let (result, ()) = futures_util::join!(config.connect(NoTls), peer.reached_stall());
            let error = result.expect_err("a peer that never answered completed startup");
            assert_connect_timeout(&error, ConnectPhase::Startup, "startup/connect_timeout");
            peer.finish();
        }),
    )
    .await
    .expect("startup connect-timeout test exceeded its watchdog");
}

/// The same peer with a `warm_up_timeout`: the pool constructor names startup
/// and authentication and keeps the warm-up classification.
#[compio::test]
async fn a_warm_up_timeout_names_startup_when_the_server_never_answers() {
    compio::time::timeout(OPERATION_WATCHDOG, async {
        let mut peer = Peer::spawn(stall_after_startup);
        let connection_config = config(peer.port, SslMode::Disable);
        let mut pool_config = PoolConfig::new();
        pool_config
            .max_size(1)
            .min_idle(0)
            .warm_up_timeout(STALL_DEADLINE);

        let (result, ()) = futures_util::join!(
            Pool::connect_with_config(connection_config, pool_config),
            peer.reached_stall(),
        );
        let error = result.expect_err("a peer that never answered completed pool warm-up");
        assert_warm_up_timeout(&error, ConnectPhase::Startup, "startup/warm_up_timeout");
        peer.finish();
    })
    .await
    .expect("startup warm-up-timeout test exceeded its watchdog");
}

// -- post-connect setup ----------------------------------------------------

/// A peer that authenticates but never sends the startup parameters or
/// `ReadyForQuery`: the read that follows authentication names post-connect
/// setup.
#[compio::test]
async fn a_connect_timeout_names_post_connect_setup_after_authentication() {
    compio::time::timeout(
        OPERATION_WATCHDOG,
        Box::pin(async {
            let mut peer = Peer::spawn(answer_auth_ok_then_stall);
            let mut config = config(peer.port, SslMode::Disable);
            config.connect_timeout(STALL_DEADLINE);

            let (result, ()) = futures_util::join!(config.connect(NoTls), peer.reached_stall());
            let error = result.expect_err("a peer that never sent ReadyForQuery completed startup");
            assert_connect_timeout(
                &error,
                ConnectPhase::PostConnectSetup,
                "post-connect/connect_timeout",
            );
            peer.finish();
        }),
    )
    .await
    .expect("post-connect connect-timeout test exceeded its watchdog");
}

/// The same peer through pool construction, so the stage survives the warm-up
/// deadline as well.
#[compio::test]
async fn a_warm_up_timeout_names_post_connect_setup_after_authentication() {
    compio::time::timeout(OPERATION_WATCHDOG, async {
        let mut peer = Peer::spawn(answer_auth_ok_then_stall);
        let connection_config = config(peer.port, SslMode::Disable);
        let mut pool_config = PoolConfig::new();
        pool_config
            .max_size(1)
            .min_idle(0)
            .warm_up_timeout(STALL_DEADLINE);

        let (result, ()) = futures_util::join!(
            Pool::connect_with_config(connection_config, pool_config),
            peer.reached_stall(),
        );
        let error =
            result.expect_err("a peer that never sent ReadyForQuery completed pool warm-up");
        assert_warm_up_timeout(
            &error,
            ConnectPhase::PostConnectSetup,
            "post-connect/warm_up_timeout",
        );
        peer.finish();
    })
    .await
    .expect("post-connect warm-up-timeout test exceeded its watchdog");
}

// -- the acquire deadline --------------------------------------------------

/// A full pool that must open a connection on demand, against a peer that
/// stalls its second connection: the checkout names the stage the on-demand
/// connect had reached and keeps the acquire classification.
#[compio::test]
async fn an_acquire_timeout_names_the_stage_of_an_on_demand_connect() {
    compio::time::timeout(OPERATION_WATCHDOG, async {
        let mut peer = Peer::spawn(first_ok_then_second_stalls);
        let connection_config = config(peer.port, SslMode::Disable);
        let mut pool_config = PoolConfig::new();
        pool_config
            .max_size(2)
            .min_idle(1)
            .warm_up_timeout(Duration::from_secs(20))
            .validation_bypass(Duration::from_secs(20))
            .acquire_timeout(STALL_DEADLINE);

        let pool = Pool::connect_with_config(connection_config, pool_config)
            .await
            .expect("the peer completes the first connection");
        let first = pool.acquire().await.expect("the warm connection is idle");
        let (result, ()) = futures_util::join!(pool.acquire(), peer.reached_stall());
        let error = result.expect_err("opening the second connection must time out");
        assert_acquire_timeout(&error, ConnectPhase::Startup, "startup/acquire_timeout");

        drop(first);
        compio::time::timeout(OPERATION_WATCHDOG, pool.close())
            .await
            .expect("closing the pool exceeded its watchdog");
        peer.finish();
    })
    .await
    .expect("acquire-timeout test exceeded its watchdog");
}

// -- hooks are not a connect stage -----------------------------------------

/// A warm-up deadline that expires while an `after_connect` hook runs names NO
/// phase: the connect had already completed before the hook started, so the
/// hook is not a stage of the connect. The hook itself signals, so the deadline
/// is known to have expired inside the hook after its connect returned.
#[compio::test]
async fn a_warm_up_deadline_inside_after_connect_names_no_phase() {
    compio::time::timeout(OPERATION_WATCHDOG, async {
        let peer = Peer::spawn(answer_startup_then_hold);
        let connection_config = config(peer.port, SslMode::Disable);
        let mut pool_config = PoolConfig::new();
        pool_config
            .max_size(1)
            .min_idle(1)
            .warm_up_timeout(STALL_DEADLINE);
        let (hook_ran_tx, mut hook_ran) = futures_channel::mpsc::unbounded();
        pool_config.after_connect(move |_client| {
            let _ = hook_ran_tx.unbounded_send(());
            Box::pin(async { std::future::pending::<Result<(), Error>>().await })
        });

        let (result, ()) = futures_util::join!(
            Pool::connect_with_config(connection_config, pool_config),
            async {
                hook_ran
                    .recv()
                    .await
                    .expect("the after_connect hook never ran");
            },
        );
        let error = result.expect_err("a pool whose hook never returned warmed up");
        assert!(
            error.is_pool_timeout(),
            "a warm-up expiry lost its pool-timeout classification: {error:?}"
        );
        assert_eq!(
            error.pool_timeout_budget(),
            Some(PoolBudget::WarmUp),
            "the warm-up expiry was classified as another budget: {error:?}"
        );
        assert_eq!(
            error.connect_phase(),
            None,
            "a deadline that expired inside after_connect named a connect stage: {error:?}"
        );
        peer.finish();
    })
    .await
    .expect("after_connect warm-up-timeout test exceeded its watchdog");
}

/// An acquire deadline that expires while the `after_connect` hook of an
/// on-demand connection runs names NO phase: that connect completed before its
/// hook started. The hook stalls only on its second call, so the first
/// connection is warm and the on-demand connect is the one that reaches it; the
/// hook signals on that second call, proving the on-demand connect returned.
#[compio::test]
async fn an_acquire_deadline_inside_after_connect_names_no_phase() {
    compio::time::timeout(OPERATION_WATCHDOG, async {
        let peer = Peer::spawn(answer_two_startups_then_hold);
        let connection_config = config(peer.port, SslMode::Disable);
        let calls = Arc::new(AtomicUsize::new(0));
        let hook_calls = Arc::clone(&calls);
        let mut pool_config = PoolConfig::new();
        pool_config
            .max_size(2)
            .min_idle(1)
            .warm_up_timeout(Duration::from_secs(20))
            .validation_bypass(Duration::from_secs(20))
            .acquire_timeout(STALL_DEADLINE);
        let (hook_ran_tx, mut hook_ran) = futures_channel::mpsc::unbounded();
        pool_config.after_connect(move |_client| {
            let calls = Arc::clone(&hook_calls);
            let hook_ran = hook_ran_tx.clone();
            Box::pin(async move {
                if calls.fetch_add(1, Ordering::Relaxed) + 1 >= 2 {
                    let _ = hook_ran.unbounded_send(());
                    std::future::pending::<Result<(), Error>>().await
                } else {
                    Ok(())
                }
            })
        });

        let pool = Pool::connect_with_config(connection_config, pool_config)
            .await
            .expect("the peer completes the first connection");
        let first = pool.acquire().await.expect("the warm connection is idle");
        let (result, ()) = futures_util::join!(pool.acquire(), async {
            hook_ran
                .recv()
                .await
                .expect("the stalling after_connect hook never ran");
        });
        let error = result.expect_err("opening the second connection must time out");
        assert!(
            error.is_pool_timeout(),
            "an acquire expiry lost its pool-timeout classification: {error:?}"
        );
        assert_eq!(
            error.pool_timeout_budget(),
            Some(PoolBudget::Acquire),
            "the acquire expiry was classified as another budget: {error:?}"
        );
        assert_eq!(
            error.connect_phase(),
            None,
            "a deadline that expired inside after_connect named a connect stage: {error:?}"
        );
        assert_eq!(
            calls.load(Ordering::Relaxed),
            2,
            "the hook did not stall on its second call"
        );

        drop(first);
        compio::time::timeout(OPERATION_WATCHDOG, pool.close())
            .await
            .expect("closing the pool exceeded its watchdog");
        peer.finish();
    })
    .await
    .expect("after_connect acquire-timeout test exceeded its watchdog");
}

// -- SSL request and TLS handshake -----------------------------------------

/// A peer that accepts TCP and never answers the `SSLRequest`: the stage is
/// the SSL request, under a `connect_timeout`.
#[cfg(feature = "tls")]
#[compio::test]
async fn a_connect_timeout_names_the_ssl_request_when_it_goes_unanswered() {
    compio::time::timeout(
        OPERATION_WATCHDOG,
        Box::pin(async {
            let mut peer = Peer::spawn(stall_ssl_request);
            let mut config = config(peer.port, SslMode::Require);
            config.connect_timeout(STALL_DEADLINE);
            let tls = compio_postgres::MakeRustlsConnect::from_config(&config)
                .expect("build the TLS connector");

            let (result, ()) = futures_util::join!(config.connect(tls), peer.reached_stall());
            let error =
                result.expect_err("a peer that never answered SSLRequest completed a connection");
            assert_connect_timeout(
                &error,
                ConnectPhase::SslRequest,
                "ssl-request/connect_timeout",
            );
            let report = support::connection_failure_report(
                &format!("postgres://u@127.0.0.1:{}/d?sslmode=require", peer.port),
                &error,
            );
            assert!(
                report.contains("the request is sent"),
                "the SSL-request phase must read as a request sent and unanswered:\n{report}"
            );
            assert!(
                !report.contains("WITHOUT a phase line"),
                "a report that printed the SSL-request phase described the no-phase case:\n{report}"
            );
            peer.finish();
        }),
    )
    .await
    .expect("ssl-request connect-timeout test exceeded its watchdog");
}

/// The same peer through pool construction: the warm-up deadline names the
/// SSL request too.
#[cfg(feature = "tls")]
#[compio::test]
async fn a_warm_up_timeout_names_the_ssl_request_when_it_goes_unanswered() {
    compio::time::timeout(OPERATION_WATCHDOG, async {
        let mut peer = Peer::spawn(stall_ssl_request);
        let connection_config = config(peer.port, SslMode::Require);
        let mut pool_config = PoolConfig::new();
        pool_config
            .max_size(1)
            .min_idle(0)
            .warm_up_timeout(STALL_DEADLINE);

        let (result, ()) = futures_util::join!(
            Pool::connect_with_config(connection_config, pool_config),
            peer.reached_stall(),
        );
        let error =
            result.expect_err("a peer that never answered SSLRequest completed pool warm-up");
        assert_warm_up_timeout(
            &error,
            ConnectPhase::SslRequest,
            "ssl-request/warm_up_timeout",
        );
        peer.finish();
    })
    .await
    .expect("ssl-request warm-up-timeout test exceeded its watchdog");
}

/// A peer that answers `S` and then never speaks TLS: the stage is the TLS
/// handshake, under a `connect_timeout`.
#[cfg(feature = "tls")]
#[compio::test]
async fn a_connect_timeout_names_the_tls_handshake_when_it_stalls() {
    compio::time::timeout(
        OPERATION_WATCHDOG,
        Box::pin(async {
            let mut peer = Peer::spawn(accept_tls_then_stall);
            let mut config = config(peer.port, SslMode::Require);
            config.connect_timeout(STALL_DEADLINE);
            let tls = compio_postgres::MakeRustlsConnect::from_config(&config)
                .expect("build the TLS connector");

            let (result, ()) = futures_util::join!(config.connect(tls), peer.reached_stall());
            let error = result.expect_err("a peer that never spoke TLS completed a handshake");
            assert_connect_timeout(
                &error,
                ConnectPhase::TlsHandshake,
                "tls-handshake/connect_timeout",
            );
            peer.finish();
        }),
    )
    .await
    .expect("tls-handshake connect-timeout test exceeded its watchdog");
}

/// The same peer through pool construction: the warm-up deadline names the TLS
/// handshake.
#[cfg(feature = "tls")]
#[compio::test]
async fn a_warm_up_timeout_names_the_tls_handshake_when_it_stalls() {
    compio::time::timeout(OPERATION_WATCHDOG, async {
        let mut peer = Peer::spawn(accept_tls_then_stall);
        let connection_config = config(peer.port, SslMode::Require);
        let mut pool_config = PoolConfig::new();
        pool_config
            .max_size(1)
            .min_idle(0)
            .warm_up_timeout(STALL_DEADLINE);

        let (result, ()) = futures_util::join!(
            Pool::connect_with_config(connection_config, pool_config),
            peer.reached_stall(),
        );
        let error = result.expect_err("a peer that never spoke TLS completed pool warm-up");
        assert_warm_up_timeout(
            &error,
            ConnectPhase::TlsHandshake,
            "tls-handshake/warm_up_timeout",
        );
        peer.finish();
    })
    .await
    .expect("tls-handshake warm-up-timeout test exceeded its watchdog");
}

// -- a deadline that fires between attempts --------------------------------

/// A pool whose warm-up dials a port nothing listens on: the first attempt is
/// refused at once, the marker is cleared when that attempt returns, and the
/// warm-up deadline expires during the retry backoff, when no connect is in
/// flight. The error must name NO stage, not the `TCP dial` of the attempt that
/// already failed.
///
/// The deadline is the pool's own value and the refused dials are local, so it
/// lands in the pause between attempts rather than inside one.
#[compio::test]
async fn a_warm_up_timeout_between_attempts_names_no_phase() {
    compio::time::timeout(OPERATION_WATCHDOG, async {
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind a closed port");
            listener.local_addr().expect("closed port address").port()
        };
        let mut pool_config = PoolConfig::new();
        pool_config
            .max_size(1)
            .min_idle(0)
            .warm_up_timeout(Duration::from_millis(300));

        let error = Pool::connect_with_config(config(port, SslMode::Disable), pool_config)
            .await
            .expect_err("a refused connection warmed a pool");
        assert!(
            error.is_pool_timeout(),
            "a warm-up expiry lost its pool-timeout classification: {error:?}"
        );
        assert_eq!(
            error.pool_timeout_budget(),
            Some(PoolBudget::WarmUp),
            "the warm-up expiry was classified as another budget: {error:?}"
        );
        assert_eq!(
            error.connect_phase(),
            None,
            "a deadline that expired between attempts named a stale stage: {error:?}"
        );
        assert!(
            !support::error_chain(&error).contains("TCP dial"),
            "the error named the stage of the attempt that already failed: {}",
            support::error_chain(&error)
        );
    })
    .await
    .expect("retry-backoff phase test exceeded its watchdog");
}
