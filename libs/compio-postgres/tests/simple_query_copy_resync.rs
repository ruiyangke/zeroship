//! A simple query that puts the session into COPY-IN mode must not desynchronise it.
//!
//! `COPY <table> FROM STDIN` sent through the simple query protocol - by
//! `batch_execute`, by `simple_query`, or by either of their `Transaction`
//! forwarders - makes PostgreSQL answer `CopyInResponse` and then WAIT for copy
//! data. This driver's simple-query drain cannot supply any: the request was
//! encoded as one pre-built buffer, so there is no channel to push `CopyData`
//! through. It reported `unexpected message from server` and dropped the
//! response stream.
//!
//! That left the SESSION in copy mode. The next frontend message was read as
//! copy data, and PostgreSQL answered (measured 2026-08-23 from the review
//! server's own log)
//!
//!   ERROR:  unexpected message type 0x50 during COPY from stdin
//!   FATAL:  terminating connection because protocol synchronization was lost
//!
//! so the connection died. The failure landed on whatever ran NEXT, which in a
//! pool is the next borrower: after the failing `batch_execute` the client still
//! reported `is_closed() == false` and `is_dirty() == false`, so nothing marked
//! it for eviction, and `transaction_status()` was stuck at `None` because the
//! copy's `ReadyForQuery` was never coming.
//!
//! The fix ends the copy with `CopyFail`, so the caller gets PostgreSQL's own
//! diagnostic for the copy it could not feed and the session stays usable.
//!
//! `COPY ... TO STDOUT` is the CONTROL. It reaches the same
//! `unexpected message from server` arm of the same drain loop, but PostgreSQL
//! streams the whole result unprompted and returns to `ReadyForQuery` on its
//! own, so that session was never desynchronised and the fix does not touch it.
//! A test asserting only "the client still works" would pass on the control
//! whatever the STDIN case did.
//!
//! EVERY TEST HERE IS UNDER A WATCHDOG because the regression this guards is a
//! HANG as often as an error: `abort_copy_in` sends `CopyFail` AND a `Sync`, and
//! the `Sync` is what balances the connection task's response accounting.
//! Removing just that one line (measured) does not fail the assertions - it
//! leaves the follow-up query waiting for a reply that was handed to the
//! abort's own orphaned response slot, and the test never returns.

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
const ABORT_MARKER: &str = "simple query execution cannot supply COPY data";

fn test_url() -> Option<String> {
    common::env::get(common::env::TestEnvKey::PgTestUrl)
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
    let name = common::test_object_name(&format!("cpg_copy_resync_{suffix}"));
    client
        .batch_execute(&format!("CREATE TEMPORARY TABLE {name} (v int)"))
        .await
        .expect("create the probe table");
    name
}

/// The session survives a `COPY ... FROM STDIN` sent through `batch_execute`.
#[compio::test]
async fn batch_execute_of_copy_from_stdin_leaves_the_session_usable() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let Some(url) = test_url() else {
            return;
        };
        let client = connect_client(&url).await;
        let table = probe_table(&client, "batch").await;

        let failure = client
            .batch_execute(&format!("COPY {table} FROM STDIN"))
            .await
            .expect_err("batch_execute fed a COPY it has no data channel for");
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
    })
    .await
    .expect("batch COPY resync test exceeded its watchdog");
}

/// Same claim for the streaming `simple_query` entry point, which drains
/// through `SimpleQueryStream` rather than through `finish_batch_execute`.
#[compio::test]
async fn simple_query_of_copy_from_stdin_leaves_the_session_usable() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let Some(url) = test_url() else {
            return;
        };
        let client = connect_client(&url).await;
        let table = probe_table(&client, "stream").await;

        let failure = client
            .simple_query(&format!("COPY {table} FROM STDIN"))
            .await
            .expect_err("simple_query fed a COPY it has no data channel for");
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
    })
    .await
    .expect("streaming COPY resync test exceeded its watchdog");
}

/// Inside a transaction the abandoned COPY must leave a session the
/// transaction's own rollback can still reach. Before the fix the `ROLLBACK`
/// was itself read as copy data.
///
/// The `Idle` assertion at the end is not decoration: it is the only cheap
/// witness that the connection task's in-flight accounting came back to zero.
/// `transaction_status()` answers `None` while any transaction-capable request
/// has not reached its `ReadyForQuery`, so an abort that consumed one terminator
/// too few would report `None` here forever.
#[compio::test]
async fn a_transaction_survives_an_abandoned_copy_from_stdin() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let Some(url) = test_url() else {
            return;
        };
        let mut client = connect_client(&url).await;
        let table = probe_table(&client, "txn").await;

        {
            let transaction = client.transaction().await.expect("begin");
            let failure = transaction
                .batch_execute(&format!("COPY {table} FROM STDIN"))
                .await
                .expect_err("batch_execute fed a COPY it has no data channel for");
            assert_eq!(
                failure.code(),
                Some(&SqlState::QUERY_CANCELED),
                "the copy was not aborted by this driver: {}",
                common::error_chain(&failure)
            );
            // The COPY error aborted the transaction block; rolling back is the
            // only legal move, and it has to reach the server.
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
    .expect("transactional COPY resync test exceeded its watchdog");
}

/// CONTROL. `COPY ... TO STDOUT` reaches the same rejection arm but never puts
/// the session into copy mode, so it was already usable afterwards and must
/// stay that way. This is what separates "the drain rejects COPY" from "the
/// session was desynchronised", and it must stay green through every mutation
/// of the abort path.
#[compio::test]
async fn batch_execute_of_copy_to_stdout_was_never_desynchronised() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let Some(url) = test_url() else {
            return;
        };
        let client = connect_client(&url).await;
        let table = probe_table(&client, "out").await;
        client
            .batch_execute(&format!("INSERT INTO {table} VALUES (1), (2)"))
            .await
            .expect("seed the probe table");

        let failure = client
            .batch_execute(&format!("COPY {table} TO STDOUT"))
            .await
            .expect_err("batch_execute decoded a COPY OUT result set");
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
    .expect("COPY TO STDOUT control exceeded its watchdog");
}
