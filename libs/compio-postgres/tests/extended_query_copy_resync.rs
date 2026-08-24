//! `COPY ... FROM STDIN` sent through the EXTENDED protocol must not desynchronise
//! the session.
//!
//! `simple_query_copy_resync.rs` rules on the same statement sent through the
//! simple query protocol. This file is its extended-protocol peer:
//! `Client::execute`, `Client::query` and the `RowStream` they hand back encode
//! `Parse + Bind + Describe + Execute + Sync` as one buffer, so - exactly as on
//! the simple path - there is no channel to push `CopyData` through and the
//! copy can never be fed.
//!
//! The two paths need SEPARATE rulings because the server answers the abort
//! differently. In simple query mode PostgreSQL answers `CopyFail` with
//! `ErrorResponse` AND its own `ReadyForQuery`; under an extended-protocol copy
//! the error sets `ignore_till_sync` and ONLY a `Sync` releases a
//! `ReadyForQuery`. A fix that assumed the simple-path accounting here would
//! hand the caller's terminator to the abort's own slot and hang.
//!
//! `COPY ... TO STDOUT` is the CONTROL, for the same reason it is on the simple
//! path: it reaches the same rejection arm but PostgreSQL streams the whole
//! result unprompted and returns to `ReadyForQuery` on its own, so that session
//! was never desynchronised. A test asserting only "the client still works"
//! would pass on the control whatever the STDIN case did.
//!
//! EVERY TEST HERE IS UNDER A WATCHDOG: the regression is a HANG as often as an
//! error, because it is response-slot accounting that goes wrong.

use compio_postgres::error::SqlState;
use compio_postgres::{Client, NoTls};
use std::time::Duration;

#[allow(dead_code)]
mod common;

const TEST_TIMEOUT: Duration = Duration::from_secs(20);

/// The marker this driver puts in the `CopyFail` message. Asserting on it,
/// rather than on "COPY from stdin failed" alone, is what separates OUR abort
/// from any other way the copy could have ended: an abandoned `copy_in` sink
/// aborts the identical statement with an EMPTY reason, so the server's prefix
/// is shared between the two and cannot tell them apart.
const ABORT_MARKER: &str = "cannot supply COPY data";

fn test_url() -> String {
    common::test_url()
}

async fn connect_client(url: &str) -> Client {
    let (client, connection) = compio_postgres::connect(url, NoTls)
        .await
        .expect("connect to PostgreSQL");
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {error}");
        }
    })
    .detach();
    client
}

/// Temporary, so nothing survives the session even if an assertion below fails
/// part way through - the review database is shared with other suites.
async fn probe_table(client: &Client, suffix: &str) -> String {
    let name = common::test_object_name(&format!("cpg_ext_copy_resync_{suffix}"));
    client
        .batch_execute(&format!("CREATE TEMPORARY TABLE {name} (v int)"))
        .await
        .expect("create the probe table");
    name
}

/// `execute` runs Parse/Bind/Describe/Execute/Sync and drains to
/// `ReadyForQuery`. The `Sync` it already sent is IGNORED by PostgreSQL while
/// the session is in copy mode, so without an abort the session stays there and
/// the NEXT request's `Parse` is read as copy data.
#[compio::test]
async fn execute_of_copy_from_stdin_leaves_the_session_usable() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let client = connect_client(&url).await;
        let table = probe_table(&client, "exec").await;

        let failure = client
            .execute(&format!("COPY {table} FROM STDIN"), &[])
            .await
            .expect_err("execute fed a COPY it has no data channel for");
        assert_eq!(
            failure.code(),
            Some(&SqlState::QUERY_CANCELED),
            "the copy was not aborted by this driver: {}",
            common::error_chain(&failure)
        );
        assert!(
            common::error_chain(&failure).contains(ABORT_MARKER),
            "the failure did not carry this driver's copy-abort reason: {}",
            common::error_chain(&failure)
        );

        let value: i32 = client
            .query_one_scalar("SELECT 42::int4", &[])
            .await
            .expect("the session was left desynchronised by the abandoned COPY");
        assert_eq!(value, 42);
        assert!(
            !client.is_closed(),
            "the abandoned COPY closed the connection"
        );
        assert_eq!(
            client.transaction_status(),
            Some(compio_postgres::TransactionStatus::Idle),
            "the connection task's in-flight accounting did not come back to zero"
        );
    })
    .await
    .expect("extended execute COPY resync test exceeded its watchdog");
}

/// Same claim for `query`, which drains through a different loop in `query.rs`
/// and hands back a `RowStream` rather than a row count.
#[compio::test]
async fn query_of_copy_from_stdin_leaves_the_session_usable() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let client = connect_client(&url).await;
        let table = probe_table(&client, "query").await;

        let failure = client
            .query(&format!("COPY {table} FROM STDIN"), &[])
            .await
            .expect_err("query fed a COPY it has no data channel for");
        assert!(
            common::error_chain(&failure).contains(ABORT_MARKER),
            "the failure did not carry this driver's copy-abort reason: {}",
            common::error_chain(&failure)
        );

        let value: i32 = client
            .query_one_scalar("SELECT 43::int4", &[])
            .await
            .expect("the session was left desynchronised by the abandoned COPY");
        assert_eq!(value, 43);
        assert_eq!(
            client.transaction_status(),
            Some(compio_postgres::TransactionStatus::Idle),
            "the connection task's in-flight accounting did not come back to zero"
        );
    })
    .await
    .expect("extended query COPY resync test exceeded its watchdog");
}

/// Inside a transaction the abandoned COPY must leave a session the
/// transaction's own rollback can still reach.
#[compio::test]
async fn a_transaction_survives_an_abandoned_extended_copy_from_stdin() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect_client(&url).await;
        let table = probe_table(&client, "txn").await;

        {
            let transaction = client.transaction().await.expect("begin");
            let failure = transaction
                .execute(&format!("COPY {table} FROM STDIN"), &[])
                .await
                .expect_err("execute fed a COPY it has no data channel for");
            assert_eq!(
                failure.code(),
                Some(&SqlState::QUERY_CANCELED),
                "the copy was not aborted by this driver: {}",
                common::error_chain(&failure)
            );
            transaction
                .rollback()
                .await
                .expect("rollback after the aborted copy");
        }

        let value: i32 = client
            .query_one_scalar("SELECT 44::int4", &[])
            .await
            .expect("the session was left desynchronised by the abandoned COPY");
        assert_eq!(value, 44);
        assert_eq!(
            client.transaction_status(),
            Some(compio_postgres::TransactionStatus::Idle),
            "the session did not return to idle after the aborted copy"
        );
    })
    .await
    .expect("transactional extended COPY resync test exceeded its watchdog");
}

/// CONTROL. `COPY ... TO STDOUT` through the extended protocol reaches the same
/// rejection arm but never puts the session into copy mode, so it was already
/// usable afterwards and must stay that way. This is what separates "the drain
/// rejects COPY" from "the session was desynchronised", and it must stay green
/// through every mutation of the abort path.
#[compio::test]
async fn execute_of_copy_to_stdout_was_never_desynchronised() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let client = connect_client(&url).await;
        let table = probe_table(&client, "out").await;
        client
            .batch_execute(&format!("INSERT INTO {table} VALUES (1), (2)"))
            .await
            .expect("seed the probe table");

        let failure = client
            .execute(&format!("COPY {table} TO STDOUT"), &[])
            .await
            .expect_err("execute decoded a COPY OUT result set");
        assert!(
            failure.code().is_none(),
            "COPY TO STDOUT was answered with a server error rather than \
             rejected locally: {}",
            common::error_chain(&failure)
        );
        assert!(
            !common::error_chain(&failure).contains(ABORT_MARKER),
            "the copy-abort path fired for a COPY that never entered copy mode: {}",
            common::error_chain(&failure)
        );

        let value: i32 = client
            .query_one_scalar("SELECT 45::int4", &[])
            .await
            .expect("COPY TO STDOUT left the session unusable");
        assert_eq!(value, 45);
    })
    .await
    .expect("extended COPY TO STDOUT control exceeded its watchdog");
}
