//! Streaming of large transactions, against a real walsender.
//!
//! With `streaming` on, a transaction that outgrows
//! `logical_decoding_work_mem` is sent BEFORE it commits, and the framing
//! changes shape: `Begin`/`Commit` are replaced, not supplemented. The same
//! 4000-row transaction, measured on 16.14:
//!
//! ```text
//! streaming off -> B:1     C:1  I:4000 R:1
//! streaming on  -> S:21 E:21 c:1 I:4000 R:1
//! ```
//!
//! A consumer written against the non-streaming shape therefore waits for a
//! `Commit` that never arrives. That is why this is not merely an extra
//! option: turning it on changes the contract.
//!
//! Payload layouts, all read off the wire rather than from a document
//! (xid 689737 = 0x000a8649):
//!
//! ```text
//! S  6 bytes  53 000a8649 01     tag, xid u32, first-segment flag u8
//! E  1 byte   45                 tag only, no payload
//! c 30 bytes  63 000a8649 00 ... tag, xid, flags u8, 3 x 8-byte fields
//! A  9 bytes  41 000a864b 000a864b   tag, xid u32, subxid u32
//! ```
//!
//! `A` measured identical at proto_version 2, 3 and 4, and with `two_phase`
//! both on and off.

use compio_postgres::replication::pgoutput::{self, PgOutputMessage};
use compio_postgres::replication::{ReplicationMessage, StartReplicationOptions, Streaming};
use compio_postgres::{Client, NoTls};
use std::time::Duration;

#[allow(dead_code)]
mod common;

const WATCHDOG: Duration = Duration::from_secs(120);

/// Small enough that a few thousand rows spill, so the test does not have to
/// write the 64 MB the default would demand. This is the server's minimum.
const DECODING_WORK_MEM: &str = "64kB";

/// Rows in the streamed transaction. At ~200 bytes of payload each this is
/// comfortably past `DECODING_WORK_MEM` and produced 21 chunks when measured.
const ROWS: i32 = 4000;

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

/// Collect messages until the transaction ends, however it ends: a
/// non-streamed `Commit`, a streamed `StreamCommit`, or a `StreamAbort`.
async fn collect(slot: &str, streaming: Streaming, publication: &str) -> Vec<PgOutputMessage> {
    let mut config = common::replication_config("cpg_streaming");
    // The walsender is the process that decodes, so the limit has to be set
    // on ITS session. Startup options are the only channel a replication
    // connection has for that - it never runs a `SET`.
    config.options(format!("-c logical_decoding_work_mem={DECODING_WORK_MEM}"));

    let mut replication = compio_postgres::replication::connect_replication(NoTls, &config)
        .await
        .expect("replication connect failed");

    let mut stream = replication
        .start_logical_replication(StartReplicationOptions {
            slot_name: slot,
            publication_names: &[publication],
            proto_version: 2,
            streaming,
            ..Default::default()
        })
        .await
        .unwrap_or_else(|error| {
            panic!("START_REPLICATION failed: {}", common::error_chain(&error))
        });

    let mut decoder = pgoutput::Decoder::new();
    let mut messages = Vec::new();
    loop {
        match stream.next().await.expect("replication stream failed") {
            Some(ReplicationMessage::XLogData { body, .. }) => {
                let message = decoder.decode(&body).unwrap_or_else(|error| {
                    panic!("a live frame failed to decode: {error:?}");
                });
                let done = matches!(
                    message,
                    PgOutputMessage::Commit { .. }
                        | PgOutputMessage::StreamCommit { .. }
                        | PgOutputMessage::StreamAbort { .. }
                );
                messages.push(message);
                if done {
                    return messages;
                }
            }
            Some(ReplicationMessage::PrimaryKeepalive { .. }) => continue,
            None => return messages,
        }
    }
}

struct Fixture {
    table: String,
    publication: String,
    slot: String,
    setup: Client,
}

impl Fixture {
    async fn create(logical: &str) -> Self {
        let base = common::test_object_name(logical);
        let fixture = Self {
            table: format!("{base}_t"),
            publication: format!("{base}_p"),
            slot: format!("{base}_s"),
            setup: client().await,
        };
        fixture
            .setup
            .batch_execute(&format!(
                "DROP PUBLICATION IF EXISTS {p};
                 DROP TABLE IF EXISTS {t};
                 CREATE TABLE {t}(id int primary key, pad text);
                 CREATE PUBLICATION {p} FOR TABLE {t};",
                p = fixture.publication,
                t = fixture.table,
            ))
            .await
            .expect("fixture setup failed");
        fixture
            .setup
            .batch_execute(&format!(
                "SELECT pg_drop_replication_slot('{s}')
                   FROM pg_replication_slots WHERE slot_name = '{s}';
                 SELECT pg_create_logical_replication_slot('{s}', 'pgoutput');",
                s = fixture.slot,
            ))
            .await
            .expect("slot setup failed");
        fixture
    }

    async fn write_big_transaction(&self, ending: &str) {
        self.setup
            .batch_execute(&format!(
                "BEGIN;
                 INSERT INTO {t} SELECT g, repeat('x', 200) FROM generate_series(1, {ROWS}) g;
                 {ending};",
                t = self.table,
            ))
            .await
            .expect("bulk transaction failed");
    }

    async fn drop_all(&self) {
        let _ = self
            .setup
            .batch_execute(&format!(
                "SELECT pg_drop_replication_slot('{s}')
                   FROM pg_replication_slots WHERE slot_name = '{s}';
                 DROP PUBLICATION IF EXISTS {p};
                 DROP TABLE IF EXISTS {t};",
                s = self.slot,
                p = self.publication,
                t = self.table,
            ))
            .await;
    }
}

/// `streaming: On` delivers the transaction in chunks, framed by
/// StreamStart/StreamStop and closed by StreamCommit.
#[compio::test]
async fn a_streamed_transaction_arrives_in_chunks_and_commits_as_a_stream() {
    compio::time::timeout(WATCHDOG, async {
        let fixture = Fixture::create("cpg stream on").await;
        fixture.write_big_transaction("COMMIT").await;
        let messages = collect(&fixture.slot, Streaming::On, &fixture.publication).await;
        fixture.drop_all().await;

        let starts = messages
            .iter()
            .filter(|m| matches!(m, PgOutputMessage::StreamStart { .. }))
            .count();
        let stops = messages
            .iter()
            .filter(|m| matches!(m, PgOutputMessage::StreamStop))
            .count();
        let inserts = messages
            .iter()
            .filter(|m| matches!(m, PgOutputMessage::Insert { .. }))
            .count();

        assert!(
            starts > 1,
            "a transaction past logical_decoding_work_mem must arrive in MORE than one \
             chunk, got {starts}; if it is 1 the walsender ignored the \
             logical_decoding_work_mem sent in startup options and this test proves nothing"
        );
        assert_eq!(stops, starts, "every StreamStart must be closed by a StreamStop");
        assert_eq!(inserts, ROWS as usize, "every row must still arrive");
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, PgOutputMessage::StreamCommit { .. })),
            "a streamed transaction ends with StreamCommit"
        );
        assert!(
            !messages
                .iter()
                .any(|m| matches!(m, PgOutputMessage::Begin { .. } | PgOutputMessage::Commit { .. })),
            "streaming REPLACES Begin/Commit; seeing them means the option did not take"
        );

        // Exactly one chunk is the transaction's first.
        let firsts = messages
            .iter()
            .filter(
                |m| matches!(m, PgOutputMessage::StreamStart { first_segment, .. } if *first_segment),
            )
            .count();
        assert_eq!(firsts, 1, "only the opening chunk carries first_segment");
    })
    .await
    .expect("the streaming test exceeded its watchdog");
}

/// The control: the same transaction, streaming off, is one Begin/Commit pair
/// and no stream framing. Without it, the assertions above could be satisfied
/// by a driver that streamed unconditionally.
#[compio::test]
async fn the_same_transaction_without_streaming_is_one_begin_and_commit() {
    compio::time::timeout(WATCHDOG, async {
        let fixture = Fixture::create("cpg stream off").await;
        fixture.write_big_transaction("COMMIT").await;
        let messages = collect(&fixture.slot, Streaming::Off, &fixture.publication).await;
        fixture.drop_all().await;

        assert!(
            messages
                .iter()
                .any(|m| matches!(m, PgOutputMessage::Commit { .. })),
            "without streaming the transaction ends with a plain Commit"
        );
        assert!(
            !messages
                .iter()
                .any(|m| matches!(m, PgOutputMessage::StreamStart { .. })),
            "without streaming there is no chunk framing"
        );
    })
    .await
    .expect("the non-streaming control exceeded its watchdog");
}

/// A streamed transaction that rolls back ends with StreamAbort, and the
/// consumer must be able to name the transaction whose delivered rows are now
/// void.
#[compio::test]
async fn a_rolled_back_streamed_transaction_ends_with_stream_abort() {
    compio::time::timeout(WATCHDOG, async {
        let fixture = Fixture::create("cpg stream abort").await;
        fixture.write_big_transaction("ROLLBACK").await;
        let messages = collect(&fixture.slot, Streaming::On, &fixture.publication).await;
        fixture.drop_all().await;

        let abort = messages
            .iter()
            .find_map(|m| match m {
                PgOutputMessage::StreamAbort { xid, subxid } => Some((*xid, *subxid)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no StreamAbort among {messages:?}"));
        assert_ne!(abort.0, 0, "StreamAbort must name the transaction");
        assert_eq!(
            abort.0, abort.1,
            "when the whole transaction aborts, subxid equals xid"
        );
    })
    .await
    .expect("the stream-abort test exceeded its watchdog");
}

/// Asking for a setting the running protocol version cannot carry is refused
/// before the command is sent, naming the version needed.
#[compio::test]
async fn streaming_below_its_minimum_proto_version_is_refused_locally() {
    for (streaming, two_phase, version, needed) in [
        (Streaming::On, false, 1, "2"),
        (Streaming::Parallel, false, 2, "4"),
        (Streaming::Off, true, 2, "3"),
    ] {
        // One connection per case: `start_logical_replication` consumes it.
        let replication = compio_postgres::replication::connect_replication(
            NoTls,
            &common::replication_config("cpg_streaming_refusal"),
        )
        .await
        .expect("replication connect failed");

        let error = replication
            .start_logical_replication(StartReplicationOptions {
                slot_name: "never_used",
                publication_names: &["never_used"],
                proto_version: version,
                streaming,
                two_phase,
                ..Default::default()
            })
            .await
            .err()
            .expect("an option the proto_version cannot carry must be refused");

        let rendered = format!("{error}");
        let chain = std::iter::successors(std::error::Error::source(&error), |error| {
            std::error::Error::source(*error)
        })
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join("; ");
        assert!(
            rendered.contains(needed) || chain.contains(needed),
            "the refusal must name the version needed ({needed}): {rendered} / {chain}"
        );
    }
}
