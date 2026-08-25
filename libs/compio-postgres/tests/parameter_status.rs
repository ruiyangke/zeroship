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
//! that change during the session. Four tests, each answering something the
//! others cannot:
//!
//!   1. a value fixed at startup is readable at all,
//!   2. the WHOLE startup run is folded, not just one frame of it,
//!   3. a LATER server report reaches the same accessor,
//!   4. the one-variable control -- a `SET` of a parameter PostgreSQL does not
//!      report must leave the accessor empty.
//!
//! Without (4), an implementation that secretly ran `SHOW` would pass the rest
//! and be wrong in the way that matters. Without (2), one that kept only the
//! last `ParameterStatus` of the startup batch would pass (1) and (3); that
//! mutation was run, and it leaves ten of the eleven names missing.

use compio_postgres::{Client, Error};

#[allow(dead_code)]
mod common;

fn test_url() -> String {
    common::test_url()
}

async fn connect(url: &str) -> Result<Client, Error> {
    let (client, connection) = compio_postgres::connect(url, common::suite_tls()).await?;
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

/// EVERY long-stable reported parameter survives startup, not just one.
///
/// The startup sequence delivers these as a run of `ParameterStatus` frames,
/// usually inside a single batch alongside `AuthenticationOk` and
/// `BackendKeyData`. A driver that folded only the first or only the last of
/// that run would still pass
/// [`a_startup_reported_parameter_is_readable_through_the_client`], because
/// `server_version` alone proves nothing about the rest. This is the arity
/// check for that fold.
///
/// The names below are deliberately the ones PostgreSQL has reported for many
/// major versions. The reported set GROWS -- `search_path` arrives only in
/// PostgreSQL 18, `in_hot_standby` and `default_transaction_read_only` in 14,
/// `scram_iterations` in 16 -- so pinning the full 15 would make this a test of
/// which server happens to be running. These eleven are stable, which is what
/// makes a missing one a driver defect rather than a version difference.
#[compio::test]
async fn every_long_stable_reported_parameter_survives_startup() {
    const ALWAYS_REPORTED: &[&str] = &[
        "application_name",
        "client_encoding",
        "DateStyle",
        "integer_datetimes",
        "IntervalStyle",
        "is_superuser",
        "server_encoding",
        "server_version",
        "session_authorization",
        "standard_conforming_strings",
        "TimeZone",
    ];

    let url = test_url();
    let client = match connect(&url).await {
        Ok(client) => client,
        Err(error) => common::postgres_unreachable(&url, &error),
    };

    let missing: Vec<&str> = ALWAYS_REPORTED
        .iter()
        .copied()
        .filter(|name| client.parameter(name).is_none())
        .collect();
    assert!(
        missing.is_empty(),
        "the startup ParameterStatus run was not folded whole; missing: {missing:?}"
    );
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
