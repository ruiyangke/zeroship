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

use compio_postgres::Client;
use compio_postgres::replication::pgoutput::{self, PgOutputMessage};
use compio_postgres::replication::{ReplicationMessage, StartReplicationOptions, Streaming};
use std::time::Duration;

#[allow(unused_imports)]
use crate::common;

const WATCHDOG: Duration = Duration::from_secs(120);
const DECODING_WORK_MEM: &str = "64kB";
/// Enough rows on each side of the savepoint to spill past the work mem, so
/// the transaction really streams rather than arriving whole at commit.
const ROWS_PER_HALF: i32 = 2000;

async fn client() -> Client {
    let url = common::test_url();
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

#[compio::test]
async fn a_streamed_transaction_with_a_savepoint_decodes() {
    compio::time::timeout(WATCHDOG, async {
        let base = common::test_object_name("cpg subtxn");
        let table = format!("{base}_t");
        let publication = format!("{base}_p");
        let slot = format!("{base}_s");
        let setup = client().await;
        common::sweep_stale_test_objects(&setup).await;

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
        let mut replication =
            compio_postgres::replication::connect_replication(common::suite_tls(), &config)
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

        drop(stream);
        common::drop_replication_slot(&setup, &slot)
            .await
            .unwrap_or_else(|error| eprintln!("could not drop slot {slot}: {error}"));
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

/// A `StreamAbort` must not arrive while a stream chunk is OPEN.
///
/// `Decoder::decode` clears `stream_xid` on StreamAbort, and `stream_xid`
/// decides a WIRE-FORMAT question: between StreamStart and StreamStop every
/// `R/Y/I/U/D/T/M` frame carries a leading u32 xid, and outside a chunk it does
/// not. So clearing it early does not merely lose a label - the next frame is
/// then read four bytes out of phase, taking the relation oid from the xid
/// bytes. That is silent misattribution, not a decode error.
///
/// The clear is UNCONDITIONAL: it does not check that the aborted xid is the
/// open chunk's. A StreamAbort names a subtransaction independently of the
/// chunk's top-level xid (see this file's header), so "it names a different
/// xid" cannot be used to tell an in-chunk abort from an out-of-chunk one.
///
/// That makes the placement of StreamAbort load-bearing, and it was NOT
/// measured anywhere: the census above is a COMMITTED transaction and contains
/// no aborts at all. This rolls a subtransaction back inside a streamed
/// transaction and records where the abort actually lands.
///
/// MEASURED on 16.15, 2026-08-26: 20 chunks, 1 StreamAbort, 0 of them inside a
/// chunk. So the unconditional clear is safe on a well-formed stream, and this
/// test exists to say so out loud and to notice if a later server changes it.
#[compio::test]
async fn a_stream_abort_never_lands_inside_an_open_chunk() {
    compio::time::timeout(WATCHDOG, async {
        let base = common::test_object_name("cpg subabort");
        let table = format!("{base}_t");
        let publication = format!("{base}_p");
        let slot = format!("{base}_s");
        let setup = client().await;
        common::sweep_stale_test_objects(&setup).await;

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

        // Both halves spill, then the second half is ABORTED. The transaction
        // still commits, so the stream carries a subtransaction abort rather
        // than a whole-transaction one.
        setup
            .batch_execute(&format!(
                "BEGIN;
                 INSERT INTO {table} SELECT g, repeat('q', 200)
                   FROM generate_series(1, {ROWS_PER_HALF}) g;
                 SAVEPOINT sp1;
                 INSERT INTO {table} SELECT g + 100000, repeat('w', 200)
                   FROM generate_series(1, {ROWS_PER_HALF}) g;
                 ROLLBACK TO SAVEPOINT sp1;
                 COMMIT;"
            ))
            .await
            .expect("the aborted-savepoint transaction failed");

        let mut config = common::replication_config("cpg_subabort");
        config.options(format!("-c logical_decoding_work_mem={DECODING_WORK_MEM}"));
        let mut replication =
            compio_postgres::replication::connect_replication(common::suite_tls(), &config)
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
        let mut aborts = 0usize;
        let mut aborts_inside_a_chunk = 0usize;
        let mut chunks = 0usize;
        loop {
            match stream.next().await.expect("replication stream failed") {
                Some(ReplicationMessage::XLogData { body, .. }) => {
                    // Sampled BEFORE decoding, because decode is what clears it.
                    let open_before = decoder.stream_xid().is_some();
                    let message = decoder.decode(&body).expect("decode failed");
                    match message {
                        PgOutputMessage::StreamStart { .. } => chunks += 1,
                        PgOutputMessage::StreamAbort { .. } => {
                            aborts += 1;
                            if open_before {
                                aborts_inside_a_chunk += 1;
                            }
                        }
                        PgOutputMessage::StreamCommit { .. } | PgOutputMessage::Commit { .. } => {
                            break;
                        }
                        _ => {}
                    }
                }
                Some(ReplicationMessage::PrimaryKeepalive { .. }) => continue,
                Some(_) => {}
                None => break,
            }
        }
        drop(stream);

        // Floors first: a run that streamed nothing, or aborted nothing, would
        // satisfy the real assertion vacuously.
        assert!(
            chunks > 0,
            "the transaction never streamed, so this measured nothing"
        );
        assert!(
            aborts > 0,
            "no StreamAbort was seen, so the placement below was never tested"
        );
        assert_eq!(
            aborts_inside_a_chunk, 0,
            "{aborts_inside_a_chunk} of {aborts} StreamAbort messages arrived \
             between StreamStart and StreamStop. Decoder::decode clears \
             stream_xid there, so every following frame is parsed without its \
             leading xid and takes the relation oid four bytes early"
        );

        common::drop_replication_slot(&setup, &slot)
            .await
            .unwrap_or_else(|error| panic!("fixture teardown failed: {error}"));
        setup
            .batch_execute(&format!(
                "DROP PUBLICATION IF EXISTS {publication};
                 DROP TABLE IF EXISTS {table};"
            ))
            .await
            .expect("fixture teardown failed");
    })
    .await
    .expect("stream-abort placement test exceeded its watchdog");
}
