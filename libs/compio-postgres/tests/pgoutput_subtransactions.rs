//! A streamed transaction that opens a SUBTRANSACTION must still decode.
//!
//! Inside a stream chunk every transactional message repeats an xid. It is
//! tempting to treat that as a redundant copy of the chunk's own xid and check
//! the two agree. They do not have to agree: a change made after a SAVEPOINT
//! carries the SUBTRANSACTION's xid, and the enclosing StreamStart carries the
//! top-level one. Measured on 16.14, one streamed transaction with a savepoint
//! in the middle:
//!
//! ```text
//! S (StreamStart)   21 messages, 1 distinct xid: 000ab400
//! I (Insert)      4000 messages, 2 distinct xids: 000ab400, 000ab401
//! ```
//!
//! So the repeated xid is INFORMATION, not a checksum. A decoder that compares
//! it to the chunk's xid and refuses a mismatch rejects a completely ordinary
//! transaction - and this is not an exotic shape: a PL/pgSQL block with an
//! EXCEPTION handler opens an implicit subtransaction, so it reaches code that
//! never types the word SAVEPOINT.
//!
//! This file exists because that check shipped. `17c09e7ce` added
//! `DecodeError::StreamXidMismatch` and errored on it.

use compio_postgres::replication::pgoutput::{self, PgOutputMessage};
use compio_postgres::replication::{ReplicationMessage, StartReplicationOptions, Streaming};
use compio_postgres::{Client, NoTls};
use std::time::Duration;

#[allow(dead_code)]
mod common;

const WATCHDOG: Duration = Duration::from_secs(120);
const DECODING_WORK_MEM: &str = "64kB";
/// Enough rows on each side of the savepoint to spill past the work mem, so
/// the transaction really streams rather than arriving whole at commit.
const ROWS_PER_HALF: i32 = 2000;

async fn client() -> Client {
    let url = common::test_url();
    match compio_postgres::connect(&url, NoTls).await {
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

#[compio::test]
async fn a_streamed_transaction_with_a_savepoint_decodes() {
    compio::time::timeout(WATCHDOG, async {
        let base = common::test_object_name("cpg subtxn");
        let table = format!("{base}_t");
        let publication = format!("{base}_p");
        let slot = format!("{base}_s");
        let setup = client().await;
        common::sweep_stale_replication_slots(&setup).await;

        setup
            .batch_execute(&format!(
                "DROP PUBLICATION IF EXISTS {publication};
                 DROP TABLE IF EXISTS {table};
                 CREATE TABLE {table}(id int primary key, pad text);
                 CREATE PUBLICATION {publication} FOR TABLE {table};"
            ))
            .await
            .expect("fixture setup failed");
        setup
            .batch_execute(&format!(
                "SELECT pg_create_logical_replication_slot('{slot}', 'pgoutput');"
            ))
            .await
            .expect("slot setup failed");

        // The savepoint is the whole point: rows after it carry a different
        // xid from the ones before it, and from the chunk that contains both.
        setup
            .batch_execute(&format!(
                "BEGIN;
                 INSERT INTO {table} SELECT g, repeat('q', 200)
                   FROM generate_series(1, {ROWS_PER_HALF}) g;
                 SAVEPOINT sp1;
                 INSERT INTO {table} SELECT g + 100000, repeat('w', 200)
                   FROM generate_series(1, {ROWS_PER_HALF}) g;
                 COMMIT;"
            ))
            .await
            .expect("the savepoint transaction failed");

        let mut config = common::replication_config("cpg_subtxn");
        config.options(format!("-c logical_decoding_work_mem={DECODING_WORK_MEM}"));
        let mut replication = compio_postgres::replication::connect_replication(NoTls, &config)
            .await
            .expect("replication connect failed");
        let mut stream = replication
            .start_logical_replication(StartReplicationOptions {
                slot_name: &slot,
                publication_names: &[&publication],
                proto_version: 2,
                streaming: Streaming::On,
                ..Default::default()
            })
            .await
            .expect("START_REPLICATION failed");

        let mut decoder = pgoutput::Decoder::new();
        let mut inserts = 0usize;
        let mut chunks = 0usize;
        loop {
            match stream.next().await.expect("replication stream failed") {
                Some(ReplicationMessage::XLogData { body, .. }) => {
                    // The assertion is here, not below: before the fix this
                    // decode FAILED with StreamXidMismatch on the first
                    // message that followed the savepoint.
                    let message = decoder.decode(&body).unwrap_or_else(|error| {
                        panic!(
                            "a streamed transaction with a savepoint must decode; \
                             the repeated xid is the SUBTRANSACTION's and need not \
                             equal the chunk's: {error:?}"
                        )
                    });
                    match message {
                        PgOutputMessage::Insert { .. } => inserts += 1,
                        PgOutputMessage::StreamStart { .. } => chunks += 1,
                        PgOutputMessage::StreamCommit { .. } => break,
                        _ => {}
                    }
                }
                Some(ReplicationMessage::PrimaryKeepalive { .. }) => continue,
                None => break,
            }
        }

        common::drop_replication_slot(&setup, &slot).await;
        let _ = setup
            .batch_execute(&format!(
                "DROP PUBLICATION IF EXISTS {publication};
                 DROP TABLE IF EXISTS {table};"
            ))
            .await;

        assert_eq!(
            inserts,
            (ROWS_PER_HALF * 2) as usize,
            "every row on both sides of the savepoint must arrive"
        );
        assert!(
            chunks > 1,
            "the transaction must actually stream, or the savepoint xid never \
             reaches the in-chunk prefix this test is about; got {chunks} chunk(s)"
        );
    })
    .await
    .expect("the savepoint streaming test exceeded its watchdog");
}
