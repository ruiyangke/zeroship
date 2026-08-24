//! Regression coverage for every connection-side socket release path.

use compio_postgres::{Client, Error, NoTls};
use log::{Level, LevelFilter, Log, Metadata, Record};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

mod common;

fn test_url() -> String {
    common::test_url()
}

async fn connect(url: &str) -> Result<Client, Error> {
    let (client, connection) = compio_postgres::connect(url, NoTls).await?;
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {}", common::error_chain(&error));
        }
    })
    .detach();
    Ok(client)
}

async fn backend_exists(observer: &Client, pid: i32) -> bool {
    observer
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1)",
            &[&pid],
        )
        .await
        .expect("inspect pg_stat_activity")
        .get(0)
}

async fn wait_until_backend_is_gone(observer: &Client, pid: i32) {
    loop {
        if !backend_exists(observer, pid).await {
            return;
        }
        compio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn assert_backend_dies_before_client_drop(
    observer: &Client,
    pid: i32,
    client: Client,
    teardown: &str,
) {
    let gone_while_client_lives = compio::time::timeout(
        Duration::from_secs(2),
        wait_until_backend_is_gone(observer, pid),
    )
    .await
    .is_ok();

    if !gone_while_client_lives {
        assert!(
            backend_exists(observer, pid).await,
            "backend disappeared between the bounded wait and diagnosis",
        );

        // This control also cleans up the deliberately stranded backend before
        // failing: the duplicate really is the descriptor keeping it alive.
        drop(client);
        compio::time::timeout(
            Duration::from_secs(5),
            wait_until_backend_is_gone(observer, pid),
        )
        .await
        .expect("dropping the client did not release its duplicated socket");

        panic!(
            "backend {pid} survived after {teardown}; only dropping the still-live Client \
             released it"
        );
    }

    drop(client);
}

const PANIC_NOTICE: &str = "cpg_connection_run_panic";

struct PanicOnTaggedNotice {
    armed: AtomicBool,
}

impl Log for PanicOnTaggedNotice {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata())
            && self.armed.load(Ordering::SeqCst)
            && record.args().to_string().contains(PANIC_NOTICE)
        {
            // Disarm before unwinding so later tests cannot trip over the
            // process-global logger after this connection has been isolated.
            self.armed.store(false, Ordering::SeqCst);
            panic!("panic requested while Connection::run routed a notice");
        }
    }

    fn flush(&self) {}
}

static PANIC_LOGGER: PanicOnTaggedNotice = PanicOnTaggedNotice {
    armed: AtomicBool::new(false),
};
static INSTALL_PANIC_LOGGER: Once = Once::new();

fn arm_notice_panic() {
    INSTALL_PANIC_LOGGER.call_once(|| {
        log::set_logger(&PANIC_LOGGER).expect("install the socket-release test logger");
        log::set_max_level(LevelFilter::Info);
    });
    PANIC_LOGGER.armed.store(true, Ordering::SeqCst);
}

/// Discarding the connection half before its driver starts must release the
/// server session even while its client half remains available to the caller.
#[compio::test]
async fn dropping_an_unrun_connection_does_not_leave_the_backend_alive() {
    let url = test_url();
    let (client, connection) = compio_postgres::connect(&url, NoTls)
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    let pid = client.process_id();
    let observer = connect(&url)
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));

    drop(connection);

    assert_backend_dies_before_client_drop(
        &observer,
        pid,
        client,
        "an unrun Connection was dropped",
    )
    .await;
}

/// A caller timeout drops a pending `run()` future without giving its async
/// teardown another poll, so the connection-side release must be synchronous.
#[compio::test]
async fn cancelling_the_run_future_does_not_leave_the_backend_alive() {
    let url = test_url();
    let (client, connection) = compio_postgres::connect(&url, NoTls)
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    let pid = client.process_id();
    let observer = connect(&url)
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));

    let timed_out = compio::time::timeout(Duration::from_millis(50), connection.run()).await;
    assert!(timed_out.is_err(), "an idle Connection::run returned before cancellation");

    assert_backend_dies_before_client_drop(
        &observer,
        pid,
        client,
        "the pending Connection::run future was cancelled",
    )
    .await;
}

/// Logging is user-installed process code and can panic. A PostgreSQL notice
/// reaches that code from inside `run()`, making unwind teardown observable
/// without adding a test-only panic arm to the driver.
#[compio::test]
async fn panicking_run_does_not_leave_the_backend_alive() {
    let url = test_url();
    let (client, connection) = compio_postgres::connect(&url, NoTls)
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    let pid = client.process_id();
    let observer = connect(&url)
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    let driver = compio::runtime::spawn(async move { connection.run().await });

    arm_notice_panic();
    let query = compio::time::timeout(
        Duration::from_secs(5),
        client.batch_execute(&format!(
            "DO $$ BEGIN RAISE NOTICE '{PANIC_NOTICE}'; END $$"
        )),
    )
    .await
    .expect("the notice query stayed blocked after its connection task panicked");
    assert!(query.is_err(), "the notice did not panic inside Connection::run");

    let driver_result = compio::time::timeout(Duration::from_secs(5), driver)
        .await
        .expect("the connection task did not unwind after the notice panic");
    assert!(
        driver_result.is_err(),
        "Connection::run returned instead of unwinding through the notice logger"
    );

    assert_backend_dies_before_client_drop(
        &observer,
        pid,
        client,
        "Connection::run panicked",
    )
    .await;
}

/// A local framing refusal ends the connection task, so its server backend
/// must end even while the now-unusable `Client` handle remains in scope.
///
/// `Socket::release_handle` gives the client a duplicated descriptor. Closing
/// the connection task's original descriptor is therefore not enough: unless
/// an error exit first shuts down the shared socket, the duplicate becomes its
/// last live descriptor and PostgreSQL remains blocked sending the refused
/// frame. Waiting on the connection task proves the driver has exited before
/// the observer asks about the exact backend PID; dropping the client only
/// after that observation keeps the condition under test intact.
#[compio::test]
async fn a_local_protocol_failure_does_not_leave_the_backend_alive() {
    let url = test_url();
    let (broken_client, broken_connection) =
        compio_postgres::connect(&url, NoTls)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    let broken_pid = broken_client.process_id();
    let driver = compio::runtime::spawn(async move { broken_connection.run().await });

    let observer = connect(&url)
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));

    // One DataRow larger than the 64 MiB framing ceiling makes the driver
    // reject locally while PostgreSQL is still trying to write that session.
    let query = compio::time::timeout(
        Duration::from_secs(15),
        broken_client.simple_query("SELECT repeat('x', 67108864)"),
    )
    .await
    .expect("oversize response did not reach the framing refusal");
    assert!(query.is_err(), "the oversize DataRow was unexpectedly accepted");

    let driver_error = compio::time::timeout(Duration::from_secs(5), driver)
        .await
        .expect("connection task did not exit after the framing refusal")
        .expect("connection task panicked")
        .expect_err("the local framing refusal was reported as a clean close");
    assert!(
        common::error_chain(&driver_error).contains("message too large"),
        "the test missed the intended local refusal: {}",
        common::error_chain(&driver_error),
    );

    assert_backend_dies_before_client_drop(
        &observer,
        broken_pid,
        broken_client,
        "Connection::run returned its local framing error",
    )
    .await;
}
