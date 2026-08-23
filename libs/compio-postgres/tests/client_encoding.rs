//! What happens when the session's `client_encoding` stops being UTF8.
//!
//! This driver announces `client_encoding=UTF8` in its startup packet and
//! REFUSES any other value in a connection string -- `config.rs` has
//! `client_encoding_accepts_utf8_and_refuses_an_encoding_we_cannot_decode`,
//! and the reason is stated there: Rust strings are UTF-8, so an encoding we
//! cannot decode would be "silently decoded as something it is not".
//!
//! A mid-session `SET client_encoding` walks straight past that guard, and the
//! consequence is exactly the one the DSN check exists to prevent. MEASURED
//! against the live server, same statement, same server value:
//!
//! ```text
//! UTF8 session:   "\u{c3}\u{a9}"  bytes c3 83 c2 a9
//! LATIN1 session: "\u{e9}"        bytes c3 a9
//! ```
//!
//! The bytes `c3 a9` are the LATIN1 encoding of U+00C3 U+00A9 AND a valid
//! UTF-8 encoding of U+00E9, so nothing fails -- the caller is handed a
//! different string than the server holds. Where the bytes are not valid UTF-8
//! (`chr(233)` alone, byte `e9`) the driver does the safe thing and errors with
//! "error deserializing column 0", leaving the session usable; it is only the
//! overlapping case that is silent.
//!
//! THE TEST BELOW IS `#[ignore]`d AND RED ON PURPOSE. It asserts the behaviour
//! this driver should have -- refusing to hand back text it cannot vouch for --
//! and is the ready-made regression test for that fix. Drop the `#[ignore]`
//! when the connection task acts on a `client_encoding` it did not ask for.

use compio_postgres::{Client, NoTls};
use std::time::Duration;

#[allow(dead_code)]
mod common;

const WATCHDOG: Duration = Duration::from_secs(30);

fn test_url() -> String {
    common::env::get(common::env::TestEnvKey::PgTestUrl)
        .unwrap_or_else(|| "postgres://postgres:zeroship@localhost:5440/zeroship".to_string())
}

async fn connect() -> Client {
    let url = test_url();
    let (client, connection) = compio_postgres::connect(&url, NoTls)
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {error}");
        }
    })
    .detach();
    client
}

/// Bytes that are invalid UTF-8 are refused rather than mangled. This half
/// ALREADY HOLDS and is pinned so a future encoding change cannot quietly
/// weaken it into the silent case below.
#[compio::test]
async fn undecodable_text_is_an_error_and_the_session_survives() {
    compio::time::timeout(WATCHDOG, async {
        let client = connect().await;
        client
            .batch_execute("SET client_encoding TO 'LATIN1'")
            .await
            .expect("the server accepts the encoding change");

        // chr(233) is U+00E9; LATIN1 encodes it as the single byte 0xE9, which
        // is not valid UTF-8 on its own.
        let row = client
            .query_one("SELECT chr(233)::text", &[])
            .await
            .expect("the row itself arrives");
        row.try_get::<_, String>(0)
            .expect_err("undecodable bytes must not be handed back as a String");

        // The session is not poisoned by one undecodable value.
        let alive: i32 = client
            .query_one("SELECT 1::int4", &[])
            .await
            .expect("the session survives an undecodable column")
            .get(0);
        assert_eq!(alive, 1);
    })
    .await
    .expect("undecodable-text test exceeded its watchdog");
}

/// The silent case: LATIN1 bytes that happen to be valid UTF-8.
///
/// RED ON PURPOSE -- this asserts the behaviour the driver SHOULD have. Today
/// it returns "\u{e9}" for a server value of "\u{c3}\u{a9}" with no error at
/// all, which is the failure the DSN-level `client_encoding` check exists to
/// prevent, reached through a door that has no check on it.
#[compio::test]
#[ignore = "known defect: a mid-session client_encoding change is not acted on"]
async fn a_non_utf8_session_encoding_does_not_silently_change_text() {
    compio::time::timeout(WATCHDOG, async {
        let client = connect().await;
        let before: String = client
            .query_one("SELECT chr(195) || chr(169)", &[])
            .await
            .expect("baseline query")
            .get(0);

        client
            .batch_execute("SET client_encoding TO 'LATIN1'")
            .await
            .expect("the server accepts the encoding change");

        // Either answer is acceptable: refuse the session, or keep decoding
        // correctly. Returning a DIFFERENT string with no error is not.
        match client.query_one("SELECT chr(195) || chr(169)", &[]).await {
            Err(_) => {}
            Ok(row) => {
                let after: String = row.get(0);
                assert_eq!(
                    after, before,
                    "the same server value decoded differently after the encoding \
                     changed, and nothing reported it"
                );
            }
        }
    })
    .await
    .expect("encoding-change test exceeded its watchdog");
}
