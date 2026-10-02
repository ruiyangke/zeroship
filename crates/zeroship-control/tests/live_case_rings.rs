//! A live case gives back its `io_uring` ring, whatever it left running.
//!
//! This is its own binary because it counts every `io_uring` descriptor the
//! process holds: a test running beside it would move the count. The cases in
//! it take one lock so they run one at a time even under a parallel harness.

#![allow(clippy::future_not_send, reason = "the cases run on one compio thread")]

mod common;

use std::any::Any;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::sync::Mutex;
use std::sync::mpsc;

use compio::io::AsyncWriteExt;
use compio::net::{TcpListener, TcpStream};
use futures::channel::oneshot;

static SERIAL: Mutex<()> = Mutex::new(());

thread_local! {
    /// A client kept per thread, the way production keeps its JWKS client.
    static CLIENT: cyper::Client = cyper::Client::new();
    /// A `PostgreSQL` client kept past the case that opened it.
    static KEPT: std::cell::RefCell<Option<compio_postgres::Client>> =
        const { std::cell::RefCell::new(None) };
}

/// The `io_uring` rings this process holds open.
fn rings() -> usize {
    let descriptors = std::fs::read_dir("/proc/self/fd").expect("list this process's descriptors");
    descriptors
        .filter_map(Result::ok)
        .filter(|entry| {
            std::fs::read_link(entry.path())
                .is_ok_and(|target| target.as_os_str() == "anon_inode:[io_uring]")
        })
        .count()
}

/// A task that binds a listener, reports its address on `ready`, then parks on
/// an accept no client completes. The report is sent after the bind and before
/// the accept; the accept is submitted before the sending task yields, so a
/// receiver that has the address knows the accept is in flight.
async fn parked_accept(ready: oneshot::Sender<SocketAddr>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a listener nobody connects to");
    let _ = ready.send(listener.local_addr().expect("listener address"));
    let _ = listener.accept().await;
}

/// Wait for a spawned task's ready signal, which it sends once its I/O is
/// issued.
async fn await_signal<T>(ready: oneshot::Receiver<T>) -> T {
    ready.await.expect("the spawned task reports itself before it parks")
}

/// The control the other cases need: a task left parked on I/O when a plain
/// runtime is dropped keeps that runtime's ring open, and the count sees it.
/// Without this, a count that never moved would pass every case below.
#[test]
fn a_task_parked_on_io_strands_its_runtimes_ring() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let before = rings();
    let (ready_tx, ready_rx) = oneshot::channel();
    compio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(async {
            compio::runtime::spawn(parked_accept(ready_tx)).detach();
            await_signal(ready_rx).await;
        });
    assert_eq!(
        rings(),
        before + 1,
        "a runtime dropped under a task parked on accept must keep its ring open"
    );
}

/// A `PostgreSQL` client dropped at the end of a plain runtime's body leaves its
/// detached driver parked on the socket, which strands the ring the same way.
/// This is the shape production code gives its own connection drivers.
#[test]
fn a_detached_connection_driver_strands_its_runtimes_ring() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let url = common::require_control_db();
    let before = rings();
    compio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(detached_connection(url));
    assert_eq!(
        rings(),
        before + 1,
        "a runtime dropped under a connection driver parked on its socket must keep its ring open"
    );
}

/// Connect, query once and drop the client, with the driver detached.
async fn detached_connection(url: String) {
    let (client, connection) = compio_postgres::connect(&url, compio_postgres::NoTls)
        .await
        .expect("connect the detached-driver client");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
        .simple_query("SELECT 1")
        .await
        .expect("query the client");
}

/// The tasks a live case owns - an accept loop, and the Stripe mock with a
/// client connection still open on it - leave the ring count where it was.
#[test]
fn a_live_case_cancels_the_tasks_it_owns() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let before = rings();
    let (ready_tx, ready_rx) = oneshot::channel();
    common::live::runtime::Runtime::new()
        .expect("runtime")
        .block_on(async {
            common::live::spawn(parked_accept(ready_tx));
            await_signal(ready_rx).await;
            let stripe = common::stripe_mock::start_mock_stripe().await;
            let address = stripe.base_url.trim_start_matches("http://").to_owned();
            let mut client = TcpStream::connect(address.as_str())
                .await
                .expect("connect to the Stripe mock");
            let compio::BufResult(sent, _) = client
                .write_all(b"GET /v1/balance HTTP/1.1\r\nHost: stripe\r\n\r\n")
                .await;
            sent.expect("send a request the mock serves and then waits past");
            assert!(
                rings() > before,
                "the case's own runtime is open while its body runs"
            );
        });
    assert_eq!(
        rings(),
        before,
        "a live case left a ring open behind its own tasks"
    );
}

/// A connection driver the case does not own - detached, the way production
/// code detaches it - is closed before the runtime goes.
#[test]
fn a_live_case_closes_the_connections_it_opened() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let url = common::require_control_db();
    let before = rings();
    common::live::runtime::Runtime::new()
        .expect("runtime")
        .block_on(detached_connection(url));
    assert_eq!(
        rings(),
        before,
        "a live case left a ring open behind a connection it opened"
    );
}

/// A `PostgreSQL` client the case keeps past its body never closes its
/// connection, and the case fails naming the connection rather than leaving
/// it, and the ring its driver is parked in, behind.
#[test]
fn a_connection_held_past_the_case_fails_naming_it() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let url = common::require_control_db();
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
        common::live::runtime::Runtime::new()
            .expect("runtime")
            .block_on(async move {
                let (client, connection) = compio_postgres::connect(&url, compio_postgres::NoTls)
                    .await
                    .expect("connect the kept client");
                compio::runtime::spawn(async move {
                    let _ = connection.run().await;
                })
                .detach();
                KEPT.with(|kept| *kept.borrow_mut() = Some(client));
            });
    }));
    let panic = outcome.expect_err("a case that kept a connection fails");
    assert!(
        panic_message(panic.as_ref()).contains("PostgreSQL connection(s) this case opened"),
        "the failure names the connection: {}",
        panic_message(panic.as_ref())
    );
}

/// A body that panics gets the same teardown before its panic resumes.
#[test]
fn a_live_case_that_panics_still_gives_back_its_ring() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let before = rings();
    let (ready_tx, ready_rx) = oneshot::channel();
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
        common::live::runtime::Runtime::new()
            .expect("runtime")
            .block_on(async {
                common::live::spawn(parked_accept(ready_tx));
                await_signal(ready_rx).await;
                panic!("the case's own failure");
            });
    }));
    let panic = outcome.expect_err("the body's panic reaches the caller");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"the case's own failure"),
        "the caller sees the body's panic, not a teardown's"
    );
    assert_eq!(rings(), before, "a panicked live case left a ring open");
}

/// The ntex-system flavour tears down the same way.
#[test]
fn a_live_case_in_an_ntex_system_gives_back_its_ring() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let before = rings();
    let (ready_tx, ready_rx) = oneshot::channel();
    common::live::system::runtime::Runtime::new()
        .expect("runtime")
        .block_on(async {
            common::live::spawn(parked_accept(ready_tx));
            await_signal(ready_rx).await;
        });
    assert_eq!(
        rings(),
        before,
        "a finished ntex live case left a ring open"
    );
}

/// An HTTP server on its own thread that answers every request with `ok` and
/// keeps the connection open until the client closes it, as a long-lived
/// server that honours keep-alive would. Returns the bound address and a URL
/// for it.
fn keep_alive_server_on(bind: &str) -> (SocketAddr, String) {
    let listener = std::net::TcpListener::bind(bind).expect("bind the HTTP server");
    let address = listener.local_addr().expect("HTTP server address");
    common::live::register_listener(address);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                return;
            };
            std::thread::spawn(move || {
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => return,
                        Ok(read) => request.extend_from_slice(&buffer[..read]),
                    }
                }
                let response =
                    b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: keep-alive\r\n\r\nok";
                if stream.write_all(response).is_ok() {
                    let _ = stream.read(&mut buffer);
                }
            });
        }
    });
    (address, format!("http://{address}/"))
}

/// An HTTP server bound to loopback.
fn keep_alive_server() -> String {
    keep_alive_server_on("127.0.0.1:0").1
}

/// Fetch `url` with this thread's kept client and read the whole body.
async fn fetch(url: &str) {
    let client = CLIENT.with(Clone::clone);
    let response = client
        .get(url)
        .expect("a request to the HTTP server")
        .send()
        .await
        .expect("the HTTP server answers");
    assert_eq!(response.status(), 200, "the HTTP server answers OK");
    let body = response.text().await.expect("the response body");
    assert!(!body.is_empty(), "the HTTP server sent a body");
}

fn panic_message(panic: &(dyn Any + Send)) -> &str {
    panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or_default()
}

/// The reason servers close their connections: a kept client's pool holds a
/// connection to a server that keeps it open, parked on a read in the case's
/// runtime, and the case fails naming the connection.
#[test]
fn a_kept_http_connection_fails_the_case_that_left_it() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let url = keep_alive_server();
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
        common::live::runtime::Runtime::new()
            .expect("runtime")
            .block_on(fetch(&url));
    }));
    let panic = outcome.expect_err("a case that left a connection parked fails");
    assert!(
        panic_message(panic.as_ref()).contains("TCP connection(s) opened while this case ran"),
        "the failure names the kept connection: {}",
        panic_message(panic.as_ref())
    );
}

/// A case's server may bind a wildcard address while its client reaches it on
/// loopback; the wait still names the kept connection rather than falling
/// through to the ring backstop.
#[test]
fn a_wildcard_bound_servers_connection_is_named() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (address, _) = keep_alive_server_on("0.0.0.0:0");
    let url = format!("http://127.0.0.1:{}/", address.port());
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
        common::live::runtime::Runtime::new()
            .expect("runtime")
            .block_on(fetch(&url));
    }));
    let panic = outcome.expect_err("a case that left a wildcard-server connection parked fails");
    assert!(
        panic_message(panic.as_ref()).contains("TCP connection(s) opened while this case ran"),
        "the failure names the kept connection, not the ring: {}",
        panic_message(panic.as_ref())
    );
}

/// The backstop behind every other step: a task the case did not start
/// through `live::spawn`, parked on an accept - no connection, so nothing the
/// connection waits see - still holds the runtime, and the case fails naming
/// the ring it kept.
#[test]
fn a_ring_held_by_an_unowned_task_fails_the_case() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
        common::live::runtime::Runtime::new()
            .expect("runtime")
            .block_on(async {
                let (ready_tx, ready_rx) = oneshot::channel();
                compio::runtime::spawn(parked_accept(ready_tx)).detach();
                await_signal(ready_rx).await;
            });
    }));
    let panic = outcome.expect_err("a case whose runtime is still held fails");
    assert!(
        panic_message(panic.as_ref()).contains("kept its io_uring ring"),
        "the failure names the kept ring: {}",
        panic_message(panic.as_ref())
    );
}

/// The same kept client against the platform JWKS server every control case
/// verifies bearers with - which closes after each response - leaves nothing
/// parked, and the case gives its ring back.
#[test]
fn a_closed_http_connection_leaves_nothing_parked() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let url = common::platform_jwks_url();
    let before = rings();
    common::live::runtime::Runtime::new()
        .expect("runtime")
        .block_on(fetch(&url));
    assert_eq!(rings(), before, "a closed HTTP connection left a ring open");
}

/// The platform JWKS server every bearer check reaches says it closes each
/// connection, so a client kept per thread has nothing to pool. Without it a
/// case would wait on the server's own idle timeout instead.
#[test]
fn the_platform_jwks_server_closes_each_connection() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let url = common::platform_jwks_url();
    common::live::runtime::Runtime::new()
        .expect("runtime")
        .block_on(async move {
            let response = cyper::Client::new()
                .get(url.as_str())
                .expect("a request to the JWKS server")
                .send()
                .await
                .expect("the JWKS server answers");
            assert_eq!(
                response
                    .headers()
                    .get("connection")
                    .and_then(|value| value.to_str().ok()),
                Some("close"),
                "the JWKS server must end the connection after its response"
            );
            let _ = response.text().await;
        });
}

/// A task the case owns that ended in a panic fails the case, rather than
/// being dropped unseen with the rest.
#[test]
fn an_owned_tasks_panic_fails_the_case() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
        common::live::runtime::Runtime::new()
            .expect("runtime")
            .block_on(async {
                let (ready_tx, ready_rx) = oneshot::channel();
                common::live::spawn(async move {
                    let _ = ready_tx.send(());
                    panic!("the owned task's own failure");
                });
                await_signal(ready_rx).await;
            });
    }));
    let panic = outcome.expect_err("the owned task's panic fails the case");
    assert_eq!(
        panic_message(panic.as_ref()),
        "the owned task's own failure",
        "the case fails with the owned task's panic"
    );
}

/// Outside a live case there is nothing to cancel a task, so starting one is
/// refused rather than left to strand the ring.
#[test]
fn an_owned_task_outside_a_live_case_is_refused() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let refused = std::panic::catch_unwind(|| {
        compio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(async { common::live::spawn(async {}) });
    });
    let panic = refused.expect_err("an owned task outside a live case is refused");
    let message = panic_message(panic.as_ref());
    assert!(
        message.contains("outside a live case"),
        "the refusal names why: {message}"
    );
}

/// A case's connection wait is scoped to the servers it registered: a case that
/// keeps a connection to its own keep-alive server open fails naming that
/// connection, while a case running beside it in the same process reaches its
/// own teardown without waiting for the first case's connection.
#[test]
fn a_case_waits_only_for_the_servers_it_registered() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (b_started_tx, b_started_rx) = mpsc::channel::<()>();
    let (a_ready_tx, a_ready_rx) = mpsc::channel::<()>();
    let (release_a_tx, release_a_rx) = mpsc::channel::<()>();
    std::thread::scope(|scope| {
        let a = scope.spawn(move || {
            let url = keep_alive_server();
            std::panic::catch_unwind(AssertUnwindSafe(|| {
                common::live::runtime::Runtime::new()
                    .expect("runtime")
                    .block_on(async {
                        b_started_rx.recv().expect("wait until case B is running");
                        fetch(&url).await;
                        a_ready_tx.send(()).expect("report the open connection");
                        release_a_rx
                            .recv()
                            .expect("wait for the other case to finish");
                    });
            }))
        });
        let b = scope.spawn(move || {
            std::panic::catch_unwind(AssertUnwindSafe(|| {
                common::live::runtime::Runtime::new()
                    .expect("runtime")
                    .block_on(async {
                        b_started_tx.send(()).expect("report case B is running");
                        a_ready_rx.recv().expect("wait for case A's connection");
                    });
            }))
        });
        let b_outcome = b.join().expect("case B's thread");
        release_a_tx.send(()).expect("release case A");
        let a_outcome = a.join().expect("case A's thread");
        assert!(
            b_outcome.is_ok(),
            "case B waited for case A's connection: {}",
            panic_message(b_outcome.expect_err("case B must not fail").as_ref())
        );
        let a_panic = a_outcome.expect_err("case A must fail naming its kept connection");
        assert!(
            panic_message(a_panic.as_ref())
                .contains("TCP connection(s) opened while this case ran"),
            "case A names its kept connection: {}",
            panic_message(a_panic.as_ref())
        );
    });
}
