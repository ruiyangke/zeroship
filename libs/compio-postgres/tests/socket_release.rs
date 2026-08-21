//! Regression coverage for the socket release handle's failure path.

use compio_postgres::{Client, Error, NoTls};
use std::time::Duration;

mod common;

fn test_url() -> String {
    common::env::get(common::env::TestEnvKey::PgTestUrl)
        .unwrap_or_else(|| "postgres://postgres:zeroship@localhost:5440/zeroship".to_string())
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

    let gone_while_client_lives = compio::time::timeout(
        Duration::from_secs(2),
        wait_until_backend_is_gone(&observer, broken_pid),
    )
    .await
    .is_ok();

    if !gone_while_client_lives {
        assert!(
            backend_exists(&observer, broken_pid).await,
            "backend disappeared between the bounded wait and diagnosis",
        );

        // This control also cleans up the deliberately wedged backend before
        // failing: the duplicate really is the descriptor keeping it alive.
        drop(broken_client);
        compio::time::timeout(
            Duration::from_secs(5),
            wait_until_backend_is_gone(&observer, broken_pid),
        )
        .await
        .expect("dropping the client did not release its duplicated socket");

        panic!(
            "backend {broken_pid} survived after Connection::run returned its local framing \
             error; only dropping the still-live Client released it"
        );
    }

    drop(broken_client);
}
