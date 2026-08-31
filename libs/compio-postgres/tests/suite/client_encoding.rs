//! What happens when the session's `client_encoding` stops being UTF8.
//!
//! `Config::param` refuses a `client_encoding` it cannot decode, and says why:
//! Rust strings are UTF-8, so another encoding would be "silently decoded as
//! something it is not". A mid-session `SET client_encoding` reached the same
//! state through a door with no check on it, and the consequence was not an
//! error but WRONG TEXT. Measured against the live server before the fix, same
//! statement, same server value:
//!
//! ```text
//! UTF8 session:   "\u{c3}\u{a9}"  bytes c3 83 c2 a9
//! LATIN1 session: "\u{e9}"        bytes c3 a9
//! ```
//!
//! The LATIN1 bytes of one string are a valid UTF-8 encoding of a different
//! one, so nothing failed -- the caller was handed a string the server does not
//! hold. Where the bytes are NOT valid UTF-8 the decoder already errored
//! safely; it is only the overlapping case that was silent, and silence is why
//! this is caught at the protocol layer rather than left to the decoder.
//!
//! The connection is now retired instead. That is harsh -- a caller who sets
//! the encoding deliberately loses the connection -- and it is the only option
//! that never returns text the server did not send.
//!
//! WHERE THE CAUSE LANDS, and it is a real limitation: the naming error is the
//! CONNECTION TASK's return value. The caller's in-flight command fails with
//! the generic "connection closed", because the request channel closes under
//! it. So a caller who does not keep the `Connection` join handle sees only
//! that the session died. Both halves are asserted below so the split is
//! visible rather than discovered.

use compio::runtime::JoinHandle;
use compio_postgres::{Client, Error};
use std::time::Duration;

#[allow(unused_imports)]
use crate::common;

const WATCHDOG: Duration = Duration::from_secs(30);

fn test_url() -> String {
    common::test_url()
}

/// Keeps the driver's join handle, because the cause of a retirement is its
/// return value and not the caller's error.
async fn connect_keeping_driver() -> (Client, JoinHandle<Result<(), Error>>) {
    let url = test_url();
    // No type annotation: the transport is chosen by `suite_tls`, and naming
    // one here would pin the file to plaintext.
    let (client, connection) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    let driver = compio::runtime::spawn(async move { connection.run().await });
    (client, driver)
}

/// A session that stops being UTF8 retires the connection, and the driver says
/// which setting did it.
///
/// The `SET` itself is where this lands: PostgreSQL reports `client_encoding`
/// with a `ParameterStatus` ahead of that command's `ReadyForQuery`, so the
/// connection task sees the change while the command is still in flight. The
/// driver therefore never reaches a state where it would decode a byte it
/// cannot vouch for.
#[compio::test]
async fn a_non_utf8_session_encoding_retires_the_connection() {
    compio::time::timeout(WATCHDOG, async {
        let (client, driver) = connect_keeping_driver().await;

        client
            .batch_execute("SET client_encoding TO 'LATIN1'")
            .await
            .expect_err("changing the session encoding must not be accepted silently");

        // The connection is gone, not merely this command.
        assert!(
            client.batch_execute("SELECT 1").await.is_err(),
            "the session kept serving queries after its encoding changed"
        );

        // The CAUSE is the driver's return value. Asserted separately because
        // the caller above cannot see it -- that split is the limitation.
        drop(client);
        let outcome = driver.await.expect("the driver task panicked");
        let error = outcome.expect_err("the driver must report why it retired");
        let chain = common::error_chain(&error);
        assert!(
            chain.contains("client_encoding"),
            "the driver's error must name the setting that caused it, got: {chain}"
        );
    })
    .await
    .expect("encoding-retirement test exceeded its watchdog");
}

#[compio::test]
async fn encoding_retirement_preserves_a_pipelined_server_error() {
    compio::time::timeout(WATCHDOG, async {
        let (client, driver) = connect_keeping_driver().await;

        let changing_encoding =
            client.batch_execute("SELECT pg_sleep(0.2); SET client_encoding TO 'LATIN1'");
        let division_by_zero = client.batch_execute("SELECT 1 / 0");
        let (_, division_by_zero) =
            futures_util::future::join(changing_encoding, division_by_zero).await;

        let error =
            division_by_zero.expect_err("the pipelined division by zero unexpectedly succeeded");
        assert_eq!(
            error.code().map(compio_postgres::error::SqlState::code),
            Some("22012"),
            "client_encoding retirement discarded pipelined SQLSTATE 22012: {error}"
        );

        drop(client);
        let outcome = driver.await.expect("the driver task panicked");
        let driver_error = outcome.expect_err("the encoding change did not retire the session");
        assert!(
            common::error_chain(&driver_error).contains("client_encoding"),
            "the driver did not retain the encoding-retirement cause: {driver_error}"
        );
    })
    .await
    .expect("pipelined encoding-retirement test exceeded its watchdog");
}

/// THE ONE-VARIABLE CONTROL. Naming the encoding the driver already announced
/// is a no-op and must NOT retire anything -- otherwise "retire when
/// client_encoding is reported" would be satisfied by retiring on every report,
/// and PostgreSQL reports this parameter at startup on every connection.
#[compio::test]
async fn setting_the_encoding_to_utf8_changes_nothing() {
    compio::time::timeout(WATCHDOG, async {
        let (client, _driver) = connect_keeping_driver().await;
        client
            .batch_execute("SET client_encoding TO 'UTF8'")
            .await
            .expect("UTF8 is what the driver already announced");
        let alive: i32 = client
            .query_one("SELECT 1::int4", &[])
            .await
            .expect("the session survives a no-op encoding change")
            .get(0);
        assert_eq!(alive, 1);
    })
    .await
    .expect("encoding-control test exceeded its watchdog");
}

/// The driver ANNOUNCES UTF8 in its startup packet, rather than inheriting it.
///
/// `connect_raw` puts `client_encoding=UTF8` in the startup parameters
/// unconditionally, which is what makes the rest of this file's reasoning hold:
/// the driver decodes text as UTF-8, so it must not accept whatever the server
/// happens to default to.
///
/// Dropping that parameter would be INVISIBLE on any server whose own default
/// is already UTF8 -- every test in this file would still pass, because the
/// session would still be UTF8, just by luck of the server's configuration
/// rather than by the driver's insistence. `pg_settings.source` is what tells
/// the two apart: a value the CLIENT sent in its startup packet reports
/// `client`, while an inherited default reports `default`. Measured on this
/// server, a `psql` session does not report `client` for `client_encoding` at
/// all, which is exactly the state this driver must not be in when connected
/// directly. A transaction pooler owns the backend session, so its provenance
/// can be `default` or `session`; there the frontend `ParameterStatus` is the
/// endpoint-visible UTF8 contract instead.
#[compio::test]
async fn the_startup_packet_announces_utf8_rather_than_inheriting_it() {
    let (client, _driver) = connect_keeping_driver().await;
    let transport = common::test_transport(&test_url(), common::suite_tls()).await;

    let row = client
        .query_one(
            "SELECT setting, source FROM pg_settings WHERE name = 'client_encoding'",
            &[],
        )
        .await
        .expect("read client_encoding's provenance");
    let setting: String = row.get(0);
    let source: String = row.get(1);

    assert_eq!(setting, "UTF8", "the session must be UTF8");
    match transport {
        common::TestTransport::Direct => assert_eq!(
            source, "client",
            "client_encoding must come from the driver's STARTUP PACKET, not from \
             the server's default -- if this reads `default`, the driver stopped \
             announcing it and is merely lucky that this server agrees"
        ),
        common::TestTransport::TransactionPooler => assert_eq!(
            client.parameter("client_encoding"),
            Some("UTF8".to_string()),
            "the pooler must expose the UTF8 frontend setting it tracks independently of \
             whichever backend session serves this query (backend source was {source:?})"
        ),
    }
}
