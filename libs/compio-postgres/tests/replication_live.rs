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
//! The crate had NO live coverage of `src/replication.rs` before this file.
//! Its unit tests build `IDENTIFY_SYSTEM` row bodies by hand, and that is
//! exactly how the defect below survived: the hand-built shape is not the one
//! `DataRowBody::buffer()` produces, so parser and fixture agreed on a row
//! layout the server never sends.

use compio_postgres::{Config, NoTls};

mod common;

fn test_url() -> String {
    common::env::get(common::env::TestEnvKey::PgTestUrl)
        .unwrap_or_else(|| "postgres://postgres:zeroship@localhost:5440/zeroship".to_string())
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
    let mut replication = match compio_postgres::replication::connect_replication(NoTls, &config)
        .await
    {
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
