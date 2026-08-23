//! Server parameter status (`ParameterStatus`) as the caller sees it.
//!
//! libpq exposes this as `PQparameterStatus` and tokio-postgres as
//! `Connection::parameter`; it is how a caller gates on `server_version` or
//! reads back what the server made of a `SET`. This driver captured the map
//! and then hung the accessor on `Connection`, which is moved into a spawned
//! task the moment a session becomes usable -- so no caller could reach it.
//!
//! These tests are built around SNAPSHOT vs LIVE, because a startup snapshot
//! is the easy half and the useless one: the values worth reading are the ones
//! that change during the session. So the first test reads a value fixed at
//! startup, the second proves a LATER server report reaches the same accessor,
//! and the third is the one-variable control -- a `SET` of a parameter
//! PostgreSQL does not report must leave the accessor empty. Without that
//! control, an implementation that secretly ran `SHOW` would pass the first two
//! and be wrong in the way that matters.

use compio_postgres::{Client, Error, NoTls};

#[allow(dead_code)]
mod common;

fn test_url() -> String {
    common::env::get(common::env::TestEnvKey::PgTestUrl)
        .unwrap_or_else(|| "postgres://postgres:zeroship@localhost:5440/zeroship".to_string())
}

async fn connect(url: &str) -> Result<Client, Error> {
    let (client, connection) = compio_postgres::connect(url, NoTls).await?;
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {error}");
        }
    })
    .detach();
    Ok(client)
}

/// The startup snapshot reaches the client.
///
/// `server_version` is sent once, during startup, before the caller ever holds
/// a `Client`. If it is not readable here then the map was captured and then
/// stranded inside the connection task.
#[compio::test]
async fn a_startup_reported_parameter_is_readable_through_the_client() {
    let url = test_url();
    let client = match connect(&url).await {
        Ok(client) => client,
        Err(error) => common::postgres_unreachable(&url, &error),
    };

    let version = client
        .parameter("server_version")
        .expect("every PostgreSQL server reports server_version at startup");

    // Compared against what the server itself answers, so a stubbed or
    // stale map cannot pass.
    let queried: String = client
        .query_one("SHOW server_version", &[])
        .await
        .expect("SHOW server_version")
        .get(0);
    assert_eq!(version, queried);
}

/// A parameter the server re-reports mid-session updates the client's view.
///
/// This is the half that makes the accessor worth having. `application_name`
/// carries `GUC_REPORT`, so the `SET` below makes the server push a
/// `ParameterStatus` frame ahead of that command's `ReadyForQuery`. The
/// connection task routes async frames in wire order, so the update has landed
/// by the time `batch_execute` returns -- no sleep and no polling. If this is
/// ever flaky, the ordering claim is wrong rather than the timing.
#[compio::test]
async fn a_reported_parameter_updates_after_the_server_reports_it() {
    let url = test_url();
    let client = match connect(&url).await {
        Ok(client) => client,
        Err(error) => common::postgres_unreachable(&url, &error),
    };

    let before = client.parameter("application_name");
    let chosen = common::test_object_name("param-status");
    client
        .batch_execute(&format!("SET application_name TO '{chosen}'"))
        .await
        .expect("set a reported parameter");

    assert_eq!(
        client.parameter("application_name").as_deref(),
        Some(chosen.as_str()),
        "the server reported application_name and the client did not take it up \
         (before the SET it read {before:?})"
    );
}

/// THE CONTROL. A `SET` of a parameter PostgreSQL does not report must leave
/// the accessor untouched.
///
/// `work_mem` is not `GUC_REPORT`: the server applies it and says nothing. If
/// `parameter()` were backed by a `SHOW`, or by anything other than the
/// `ParameterStatus` frames actually delivered, this would come back with the
/// new value and the two tests above would prove nothing.
#[compio::test]
async fn a_parameter_the_server_does_not_report_never_appears() {
    let url = test_url();
    let client = match connect(&url).await {
        Ok(client) => client,
        Err(error) => common::postgres_unreachable(&url, &error),
    };

    client
        .batch_execute("SET work_mem TO '5MB'")
        .await
        .expect("set a parameter the server does not report");

    assert_eq!(
        client.parameter("work_mem"),
        None,
        "work_mem is not GUC_REPORT, so nothing should have delivered it"
    );

    // The SET really did take effect. Without this the test would also pass on
    // a server that rejected the statement outright.
    let applied: String = client
        .query_one("SHOW work_mem", &[])
        .await
        .expect("SHOW work_mem")
        .get(0);
    assert_eq!(applied, "5MB");
}
