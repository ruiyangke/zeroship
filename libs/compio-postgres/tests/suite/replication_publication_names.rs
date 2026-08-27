//! A publication name is an SQL identifier, and `START_REPLICATION` must put
//! it on the wire as one.
//!
//! `publication_names` is not a free-text field. The walsender parses the
//! option value with `SplitIdentifierString` - the same routine that reads
//! `search_path` - so an unquoted element is DOWNCASED, and a comma ENDS it.
//! A publication legitimately named `Orders`, or `eu,us`, therefore never
//! reaches pgoutput under the name its owner gave it. Both names are legal:
//! `CREATE PUBLICATION "eu,us"` and `CREATE PUBLICATION "Orders"` both
//! succeed on PostgreSQL 16.14.
//!
//! The damage does not land at `START_REPLICATION`. pgoutput resolves
//! publications LAZILY, in the change callback, so the command succeeds, the
//! CopyBoth channel opens, the stream looks healthy - and the session dies at
//! the first change with `publication "orders" does not exist`, naming a
//! publication the caller never asked for. Measured directly against the
//! server before these tests were written:
//!
//! ```text
//! publication_names 'MyPub'    -> ERROR: publication "mypub" does not exist
//! publication_names '"MyPub"'  -> 4 messages decoded
//! publication_names 'a,b'      -> ERROR: publication "a" does not exist
//! publication_names '"a,b"'    -> 4 messages decoded
//! publication_names 'plain'    -> 4 messages decoded
//! publication_names '"plain"'  -> 4 messages decoded
//! ```
//!
//! The last two lines are why the fix is to quote ALWAYS rather than to quote
//! only when a name "looks like it needs it": quoting an ordinary lowercase
//! name costs nothing, and every rule for deciding "needs it" is one more
//! place to be wrong.
//!
//! These are the crate's first live tests that stream a decoded change. Every
//! other live replication test stops at `IDENTIFY_SYSTEM` or talks to a
//! scripted peer, which is exactly how a defect in what we SEND survived: a
//! scripted peer asserts the bytes we chose to send, and agrees with us.

use compio_postgres::Client;
use compio_postgres::replication::pgoutput::{self, PgOutputMessage};
use compio_postgres::replication::{ReplicationMessage, StartReplicationOptions};
use std::time::Duration;

#[allow(unused_imports)]
use crate::common;

/// Bounds the whole publish-then-stream exchange. A live walsender that never
/// answers must fail the test rather than hang the suite.
const WATCHDOG: Duration = Duration::from_secs(20);

fn test_url() -> String {
    common::test_url()
}

async fn client() -> Client {
    let url = test_url();
    match compio_postgres::connect(&url, common::suite_tls()).await {
        Ok((client, connection)) => {
            compio::runtime::spawn(async move {
                if let Err(error) = connection.run().await {
                    eprintln!("connection error: {}", common::error_chain(&error));
                }
            })
            .detach();
            client
        }
        Err(error) => common::postgres_unreachable(&url, &error),
    }
}

/// `"` doubled, wrapped - the quoting the SERVER expects, written out
/// independently here so the test does not check the fix against itself.
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Publish one row through `publication`, then stream until the Insert
/// arrives.
///
/// Returns `Err` with the server's message if the stream fails first - which
/// is the pre-fix behaviour this file is about.
async fn stream_one_insert(logical: &str, publication: &str) -> Result<PgOutputMessage, String> {
    let base = common::test_object_name(logical);
    let table = format!("{base}_t");
    let slot = format!("{base}_s");
    let setup = client().await;

    // Order matters: pgoutput resolves a publication in the snapshot of the
    // change it is decoding, so a publication created AFTER the insert is
    // invisible to it. Publication first, then slot, then the row.
    setup
        .batch_execute(&format!(
            "DROP PUBLICATION IF EXISTS {pub_q};
             DROP TABLE IF EXISTS {table};
             CREATE TABLE {table}(id int primary key);
             CREATE PUBLICATION {pub_q} FOR TABLE {table};",
            pub_q = quoted(publication),
        ))
        .await
        .expect("publication setup failed");
    setup
        .batch_execute(&format!(
            "SELECT pg_create_logical_replication_slot('{slot}', 'pgoutput');"
        ))
        .await
        .expect("slot setup failed");
    setup
        .batch_execute(&format!("INSERT INTO {table} VALUES (1);"))
        .await
        .expect("insert failed");

    let result = read_first_insert(&slot, publication).await;

    common::drop_replication_slot(&setup, &slot).await;
    let _ = setup
        .batch_execute(&format!(
            "DROP PUBLICATION IF EXISTS {pub_q};
             DROP TABLE IF EXISTS {table};",
            pub_q = quoted(publication),
        ))
        .await;

    result
}

async fn read_first_insert(slot: &str, publication: &str) -> Result<PgOutputMessage, String> {
    let mut replication = compio_postgres::replication::connect_replication(
        common::suite_tls(),
        &common::replication_config("cpg_publication_names"),
    )
    .await
    .map_err(|error| {
        format!(
            "replication connect failed: {}",
            common::error_chain(&error)
        )
    })?;

    let mut stream = replication
        .start_logical_replication(StartReplicationOptions {
            slot_name: slot,
            start_lsn: "0/0",
            proto_version: 1,
            publication_names: &[publication],
            ..Default::default()
        })
        .await
        .map_err(|error| format!("START_REPLICATION failed: {}", common::error_chain(&error)))?;

    // A keepalive can precede the data, so read until the Insert, the error,
    // or the watchdog.
    loop {
        let message = stream
            .next()
            .await
            .map_err(|error| common::error_chain(&error))?;
        match message {
            Some(ReplicationMessage::XLogData { body, .. }) => {
                match pgoutput::decode(&body)
                    .map_err(|error| format!("decode failed: {error:?}"))?
                {
                    insert @ PgOutputMessage::Insert { .. } => return Ok(insert),
                    _ => continue,
                }
            }
            Some(ReplicationMessage::PrimaryKeepalive { .. }) => continue,
            None => return Err("the stream ended before the insert arrived".to_owned()),
        }
    }
}

/// The other half of the escaping, and the only one with a security shape:
/// the quoted list is embedded in a SINGLE-QUOTED literal, so a `'` inside a
/// name closes that literal early and the rest of the name lands on the
/// walsender as command text. Quoting each name as an identifier does not
/// address this - `"it's"` still carries a bare `'` - which is why the two
/// escapes compose rather than either one standing in for the other.
///
/// `CREATE PUBLICATION "it's"` is legal, so this is reachable without anyone
/// being adversarial; a name with an apostrophe in it is just a name.
#[compio::test]
async fn a_publication_name_containing_a_quote_still_streams_its_changes() {
    let publication = format!("{}_it's", common::test_object_name("cpg quote pub"));
    let outcome = compio::time::timeout(WATCHDOG, stream_one_insert("cpg quote pub", &publication))
        .await
        .expect("the quote-named publication exchange exceeded its watchdog");

    match outcome {
        Ok(PgOutputMessage::Insert { .. }) => {}
        Ok(other) => panic!("expected an Insert, got {other:?}"),
        Err(error) => panic!(
            "a publication whose name legally contains a quote must reach pgoutput \
             whole; the quote escaped its literal instead: {error}"
        ),
    }
}

/// RED before the fix: the comma inside a legal publication name is read as a
/// separator, so pgoutput is asked for two publications that do not exist.
#[compio::test]
async fn a_publication_name_containing_a_comma_still_streams_its_changes() {
    let publication = format!("{},us", common::test_object_name("cpg comma pub"));
    let outcome = compio::time::timeout(WATCHDOG, stream_one_insert("cpg comma pub", &publication))
        .await
        .expect("the comma-named publication exchange exceeded its watchdog");

    match outcome {
        Ok(PgOutputMessage::Insert { .. }) => {}
        Ok(other) => panic!("expected an Insert, got {other:?}"),
        Err(error) => panic!(
            "a publication whose name legally contains a comma must reach pgoutput \
             whole; the comma was read as a separator instead: {error}"
        ),
    }
}

/// RED before the fix: an unquoted element is downcased, so a mixed-case
/// publication is looked up under a name that does not exist.
#[compio::test]
async fn a_publication_name_containing_upper_case_still_streams_its_changes() {
    let publication = format!("{}_MixedCase", common::test_object_name("cpg case pub"));
    let outcome = compio::time::timeout(WATCHDOG, stream_one_insert("cpg case pub", &publication))
        .await
        .expect("the mixed-case publication exchange exceeded its watchdog");

    match outcome {
        Ok(PgOutputMessage::Insert { .. }) => {}
        Ok(other) => panic!("expected an Insert, got {other:?}"),
        Err(error) => panic!(
            "a publication whose name legally contains upper case must reach pgoutput \
             with its case intact; it was downcased instead: {error}"
        ),
    }
}

/// The one-variable control: an ordinary lowercase name has no comma and no
/// case to lose, so it must stream both before and after the fix. Without it,
/// the two tests above would also pass if quoting broke the ordinary path and
/// the panic messages happened to be reached some other way.
#[compio::test]
async fn an_ordinary_publication_name_still_streams_its_changes() {
    let publication = common::test_object_name("cpg plain pub");
    let outcome = compio::time::timeout(WATCHDOG, stream_one_insert("cpg plain pub", &publication))
        .await
        .expect("the ordinary publication exchange exceeded its watchdog");

    match outcome {
        Ok(PgOutputMessage::Insert { .. }) => {}
        Ok(other) => panic!("expected an Insert, got {other:?}"),
        Err(error) => panic!("an ordinary publication name must stream its changes: {error}"),
    }
}

/// A backslash in the name is what drives `quote_literal` onto its `E'...'`
/// branch, so this is the only case in the suite that puts an escape-string
/// literal in front of a real walsender.
///
/// That branch exists because doubling the quote alone is correct only while
/// `standard_conforming_strings` is on, which the driver does not own.
/// Switching to `E'...'` fixed that - and introduced a new way to be wrong:
/// if `START_REPLICATION`'s option parser did not accept escape-string
/// syntax, replication would break outright for every caller. Nothing else
/// here would notice, because every other name in this file takes the plain
/// branch.
///
/// A publication may legally be named with a backslash:
/// `CREATE PUBLICATION "back\slash"` succeeds.
#[compio::test]
async fn a_publication_name_containing_a_backslash_still_streams_its_changes() {
    let publication = format!("{}_back\\slash", common::test_object_name("cpg bs pub"));
    let outcome = compio::time::timeout(WATCHDOG, stream_one_insert("cpg bs pub", &publication))
        .await
        .expect("the backslash-named publication exchange exceeded its watchdog");

    match outcome {
        Ok(PgOutputMessage::Insert { .. }) => {}
        Ok(other) => panic!("expected an Insert, got {other:?}"),
        Err(error) => panic!(
            "a publication whose name legally contains a backslash must reach \
             pgoutput whole. If this says syntax error, the walsender does not \
             accept the E'...' literal `quote_literal` now emits, and every \
             replication caller is affected, not just this name: {error}"
        ),
    }
}
