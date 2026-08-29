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
//! A  9 bytes  41 000a864b 000a864b   on: tag, xid u32, subxid u32
//! A 25 bytes  41 000a864b 000a864b ... parallel: plus abort LSN and timestamp
//! ```
//!
//! The 9-byte `A` was observed with streaming `on` at proto_version 2 through
//! 4, with `two_phase` both on and off. The 25-byte `A` was separately
//! observed with streaming `parallel` at proto_version 4. All observations
//! came from PostgreSQL 16.14 on 2026-08-24 while the decoder still returned
//! `DecodeError::UnknownTag` for these tags.

use compio_postgres::Client;
use compio_postgres::replication::pgoutput::{self, PgOutputMessage, TupleColumn};
use compio_postgres::replication::{ReplicationMessage, StartReplicationOptions, Streaming};
use std::collections::BTreeSet;
use std::time::Duration;

#[allow(unused_imports)]
use crate::common;

const WATCHDOG: Duration = Duration::from_secs(120);
const ABORT_OBSERVATION: Duration = Duration::from_secs(3);

/// Small enough that a few thousand rows spill, so the test does not have to
/// write the 64 MB the default would demand. This is the server's minimum.
const DECODING_WORK_MEM: &str = "64kB";

/// Rows in the streamed transaction. At ~200 bytes of payload each this is
/// comfortably past `DECODING_WORK_MEM` and produced 21 chunks when measured.
const ROWS: i32 = 4000;

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

/// Collect messages until the transaction ends, however it ends: a
/// non-streamed `Commit`, a streamed `StreamCommit`, or a `StreamAbort`.
async fn collect(slot: &str, streaming: Streaming, publication: &str) -> Vec<PgOutputMessage> {
    try_collect(slot, streaming, publication)
        .await
        .unwrap_or_else(|error| panic!("a live frame failed to decode: {error}"))
}

async fn try_collect(
    slot: &str,
    streaming: Streaming,
    publication: &str,
) -> Result<Vec<PgOutputMessage>, String> {
    try_collect_with_fast_keepalives(slot, streaming, publication, false).await
}

async fn try_collect_with_fast_keepalives(
    slot: &str,
    streaming: Streaming,
    publication: &str,
    fast_keepalives: bool,
) -> Result<Vec<PgOutputMessage>, String> {
    let mut config = common::replication_config("cpg_streaming");
    // The walsender is the process that decodes, so the limit has to be set
    // on ITS session. Startup options are the only channel a replication
    // connection has for that - it never runs a `SET`.
    let mut options = format!("-c logical_decoding_work_mem={DECODING_WORK_MEM}");
    if fast_keepalives {
        options.push_str(" -c wal_sender_timeout=1s");
    }
    config.options(options);

    let replication =
        compio_postgres::replication::connect_replication(common::suite_tls(), &config)
            .await
            .map_err(|error| {
                format!(
                    "replication connect failed: {}",
                    common::error_chain(&error)
                )
            })?;

    let proto_version = match streaming {
        Streaming::Parallel => 4,
        Streaming::Off | Streaming::On => 2,
    };

    let mut stream = replication
        .start_logical_replication(StartReplicationOptions {
            slot_name: slot,
            publication_names: &[publication],
            proto_version,
            streaming,
            ..Default::default()
        })
        .await
        .map_err(|error| format!("START_REPLICATION failed: {}", common::error_chain(&error)))?;

    let mut decoder = pgoutput::Decoder::new();
    let mut messages = Vec::new();
    loop {
        match stream.next().await.map_err(|error| {
            format!("replication stream failed: {}", common::error_chain(&error))
        })? {
            Some(ReplicationMessage::XLogData { body, .. }) => {
                let message = decoder
                    .decode(&body)
                    .map_err(|error| format!("{error:?}"))?;
                let done = match &message {
                    PgOutputMessage::Commit { .. } | PgOutputMessage::StreamCommit { .. } => true,
                    PgOutputMessage::StreamAbort { xid, subxid, .. } => xid == subxid,
                    _ => false,
                };
                messages.push(message);
                if done {
                    return Ok(messages);
                }
            }
            Some(ReplicationMessage::PrimaryKeepalive { .. }) => continue,
            None => return Ok(messages),
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
        common::sweep_stale_test_objects(&fixture.setup).await;
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
                "SELECT pg_create_logical_replication_slot('{s}', 'pgoutput');",
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

    async fn write_big_subtransaction(&self) {
        self.setup
            .batch_execute(&format!(
                "BEGIN;
                 INSERT INTO {t}
                    SELECT g, repeat('x', 200)
                      FROM generate_series(1, {half}) g;
                 SAVEPOINT streamed_child;
                 INSERT INTO {t}
                    SELECT g, repeat('x', 200)
                      FROM generate_series({child_start}, {ROWS}) g;
                 RELEASE SAVEPOINT streamed_child;
                 COMMIT;",
                t = self.table,
                half = ROWS / 2,
                child_start = ROWS / 2 + 1,
            ))
            .await
            .expect("bulk subtransaction failed");
    }

    async fn write_rolled_back_big_subtransaction(&self) {
        self.setup
            .batch_execute(&format!(
                "BEGIN;
                 INSERT INTO {t} VALUES (1, 'top before');
                 SAVEPOINT streamed_child;
                 INSERT INTO {t}
                    SELECT g, repeat('x', 200)
                      FROM generate_series(2, {ROWS}) g;
                 ROLLBACK TO SAVEPOINT streamed_child;
                 INSERT INTO {t} VALUES ({after}, 'top after');
                 COMMIT;",
                t = self.table,
                after = ROWS + 1,
            ))
            .await
            .expect("rolled-back bulk subtransaction failed");
    }

    async fn drop_all(&self) {
        common::drop_replication_slot(&self.setup, &self.slot)
            .await
            .unwrap_or_else(|error| panic!("replication slot did not detach for cleanup: {error}"));

        self.setup
            .batch_execute(&format!(
                "DROP PUBLICATION IF EXISTS {p};
                 DROP TABLE IF EXISTS {t};",
                p = self.publication,
                t = self.table,
            ))
            .await
            .expect("fixture cleanup failed");
    }
}

/// What a server reported for a streamed transaction that rolled back.
///
/// MEASURED ACROSS VERSIONS, 2026-08-24. PostgreSQL 16.14 streams the
/// transaction and then sends `StreamAbort`. PostgreSQL 18.4 sends NOTHING at
/// all for the same workload - no StreamStart, no rows, no abort - and leaves
/// the replication stream open. Confirmed on 18.4 by two independent
/// instruments (a real walsender and `pg_logical_slot_peek_binary_changes`) and
/// at 4000 and 40000 rows, so it is a behaviour difference and not a spill
/// threshold.
///
/// So `StreamAbort` cannot be REQUIRED without pinning the suite to one server
/// version. What holds on both is the invariant that matters to a consumer: an
/// aborted transaction is never reported as committed. The abort's SHAPE is
/// still checked whenever a server does send one.
struct AbortOutcome {
    abort: Option<(u32, u32, Option<u64>, Option<i64>)>,
    committed: bool,
}

async fn observe_abort(slot: &str, streaming: Streaming, publication: &str) -> AbortOutcome {
    // PostgreSQL 18 can remain healthily silent for this rollback, so bound the
    // observation rather than waiting for a transaction-terminal frame. A
    // one-second server timeout forces a demanded keepalive during the window:
    // with correct feedback the outer observation expires normally; without
    // it the walsender terminates and the inner future returns an error. The
    // old test folded that transport error into an empty result and therefore
    // mistook a dead stream for PostgreSQL 18's legitimate silence.
    let messages = match compio::time::timeout(
        ABORT_OBSERVATION,
        try_collect_with_fast_keepalives(slot, streaming, publication, true),
    )
    .await
    {
        Ok(Ok(messages)) => messages,
        Ok(Err(error)) => panic!("the walsender failed during abort observation: {error}"),
        Err(_) => Vec::new(),
    };
    AbortOutcome {
        abort: messages.iter().find_map(|message| match message {
            PgOutputMessage::StreamAbort {
                xid,
                subxid,
                abort_lsn,
                abort_timestamp,
            } => Some((*xid, *subxid, *abort_lsn, *abort_timestamp)),
            _ => None,
        }),
        committed: messages.iter().any(|message| {
            matches!(
                message,
                PgOutputMessage::StreamCommit { .. } | PgOutputMessage::Commit { .. }
            )
        }),
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
            .filter(|message| matches!(message, PgOutputMessage::Insert { .. }))
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

/// Changes made under a SAVEPOINT carry the subtransaction xid, not the
/// top-level xid in StreamStart. Both are valid members of the same streamed
/// transaction and must decode rather than being treated as corrupt framing.
#[compio::test]
async fn a_streamed_subtransaction_preserves_its_own_xid() {
    compio::time::timeout(WATCHDOG, async {
        let fixture = Fixture::create("cpg stream subxid").await;
        fixture.write_big_subtransaction().await;
        let decoded = try_collect(&fixture.slot, Streaming::On, &fixture.publication).await;
        fixture.drop_all().await;

        let messages = decoded.unwrap_or_else(|error| {
            panic!("a valid streamed subtransaction failed to decode: {error}")
        });
        let top_xid = messages
            .iter()
            .find_map(|message| match message {
                PgOutputMessage::StreamCommit { xid, .. } => Some(*xid),
                _ => None,
            })
            .expect("the streamed transaction had no StreamCommit");
        let mut carried_xids = BTreeSet::new();
        let mut ids = BTreeSet::new();
        for message in &messages {
            if let PgOutputMessage::Insert {
                xid: Some(xid),
                new_tuple,
                ..
            } = message
            {
                carried_xids.insert(*xid);
                let Some(TupleColumn::Text(id)) = new_tuple.columns.first() else {
                    panic!("streamed insert had no text id: {new_tuple:?}");
                };
                ids.insert(id.parse::<i32>().expect("streamed id was not an i32"));
            }
        }

        assert_eq!(ids.len(), ROWS as usize, "every streamed row must decode");
        assert_eq!(ids.first(), Some(&1));
        assert_eq!(ids.last(), Some(&ROWS));
        assert!(
            carried_xids.contains(&top_xid),
            "top-level changes must retain xid {top_xid}: {carried_xids:?}"
        );
        assert!(
            carried_xids.iter().any(|xid| *xid != top_xid),
            "SAVEPOINT changes must retain their subxid, not be relabelled as {top_xid}"
        );
    })
    .await
    .expect("the streamed-subtransaction test exceeded its watchdog");
}

/// A child StreamAbort invalidates only the messages carrying its `subxid`;
/// the parent transaction remains live and must still reach StreamCommit.
#[compio::test]
async fn a_child_stream_abort_does_not_end_its_parent_transaction() {
    compio::time::timeout(WATCHDOG, async {
        let fixture = Fixture::create("cpg stream child abort").await;
        fixture.write_rolled_back_big_subtransaction().await;
        let decoded = try_collect(&fixture.slot, Streaming::On, &fixture.publication).await;
        fixture.drop_all().await;

        let messages = decoded
            .unwrap_or_else(|error| panic!("a valid child abort failed to decode: {error}"));
        let (top_xid, child_xid, abort_index) = messages
            .iter()
            .enumerate()
            .find_map(|(index, message)| match message {
                PgOutputMessage::StreamAbort { xid, subxid, .. } if xid != subxid => {
                    Some((*xid, *subxid, index))
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no child StreamAbort among {messages:?}"));

        assert!(messages[abort_index + 1..].iter().any(
            |message| matches!(message, PgOutputMessage::StreamCommit { xid, .. } if *xid == top_xid)
        ));
        assert!(messages.iter().any(|message| {
            matches!(message, PgOutputMessage::Insert { xid: Some(xid), .. } if *xid == child_xid)
        }));
        assert!(messages.iter().any(|message| {
            matches!(message, PgOutputMessage::Insert { xid: Some(xid), .. } if *xid == top_xid)
        }));
    })
    .await
    .expect("the child stream-abort test exceeded its watchdog");
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

/// A streamed transaction that rolls back is never reported as committed.
/// When the server emits StreamAbort, the consumer can also name the
/// transaction whose delivered rows are now void.
#[compio::test]
async fn a_rolled_back_streamed_transaction_is_never_reported_as_committed() {
    compio::time::timeout(WATCHDOG, async {
        let fixture = Fixture::create("cpg stream abort").await;
        fixture.write_big_transaction("ROLLBACK").await;
        let outcome = observe_abort(&fixture.slot, Streaming::On, &fixture.publication).await;
        fixture.drop_all().await;

        assert!(
            !outcome.committed,
            "a rolled-back transaction was reported as committed"
        );
        if let Some((xid, subxid, _, _)) = outcome.abort {
            assert_ne!(xid, 0, "StreamAbort must name the transaction");
            assert_eq!(
                xid, subxid,
                "when the whole transaction aborts, subxid equals xid"
            );
        }
    })
    .await
    .expect("the stream-abort test exceeded its watchdog");
}

/// Parallel streaming adds the abort location and timestamp in protocol 4.
/// Keeping them lets a parallel apply worker order the rollback against work
/// it may already have scheduled.
#[compio::test]
async fn a_parallel_stream_abort_preserves_protocol_four_metadata() {
    compio::time::timeout(WATCHDOG, async {
        let fixture = Fixture::create("cpg parallel abort").await;
        fixture.write_big_transaction("ROLLBACK").await;
        let outcome = observe_abort(&fixture.slot, Streaming::Parallel, &fixture.publication).await;
        fixture.drop_all().await;

        assert!(
            !outcome.committed,
            "a rolled-back transaction was reported as committed"
        );
        // When a server does send the abort under parallel streaming, protocol 4
        // carries the location and timestamp with it - that is what lets a
        // parallel apply worker order the rollback against scheduled work.
        if let Some((xid, subxid, abort_lsn, abort_timestamp)) = outcome.abort {
            assert_ne!(xid, 0);
            assert_eq!(subxid, xid);
            assert!(abort_lsn.is_some_and(|lsn| lsn > 0));
            assert!(abort_timestamp.is_some_and(|timestamp| timestamp > 0));
        }
    })
    .await
    .expect("the parallel stream-abort test exceeded its watchdog");
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
            common::suite_tls(),
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

        assert!(
            error.as_db_error().is_none(),
            "the invalid combination reached PostgreSQL instead of being refused locally: {}",
            common::error_chain(&error)
        );
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

/// PostgreSQL 16 speaks no protocol above 4, and this driver cannot decode a
/// future shape merely because the caller supplied a larger number. Refuse it
/// locally rather than starting a stream whose next frame may be ambiguous.
#[compio::test]
async fn protocol_above_four_is_refused_locally() {
    let replication = compio_postgres::replication::connect_replication(
        common::suite_tls(),
        &common::replication_config("cpg_protocol_ceiling"),
    )
    .await
    .expect("replication connect failed");

    let error = replication
        .start_logical_replication(StartReplicationOptions {
            slot_name: "never_used",
            publication_names: &["never_used"],
            proto_version: 5,
            ..Default::default()
        })
        .await
        .err()
        .expect("proto_version 5 must be refused before START_REPLICATION");
    let chain = common::error_chain(&error);

    assert!(
        error.as_db_error().is_none(),
        "the invalid version reached PostgreSQL instead of being refused locally: {chain}"
    );
    assert!(
        chain.contains("proto_version") && chain.contains("4 or lower"),
        "the refusal must name the supported maximum: {chain}"
    );
}

/// pgoutput has no protocol zero and does not negotiate one upward. Refuse the
/// invalid request locally, just like a version above the decoder's ceiling.
#[compio::test]
async fn protocol_below_one_is_refused_locally() {
    let replication = compio_postgres::replication::connect_replication(
        common::suite_tls(),
        &common::replication_config("cpg_protocol_floor"),
    )
    .await
    .expect("replication connect failed");

    let error = replication
        .start_logical_replication(StartReplicationOptions {
            slot_name: "never_used",
            publication_names: &["never_used"],
            proto_version: 0,
            ..Default::default()
        })
        .await
        .err()
        .expect("proto_version 0 must be refused before START_REPLICATION");
    let chain = common::error_chain(&error);

    assert!(
        error.as_db_error().is_none(),
        "the invalid version reached PostgreSQL instead of being refused locally: {chain}"
    );
    assert!(
        chain.contains("proto_version") && chain.contains("1 or higher"),
        "the refusal must name the supported minimum: {chain}"
    );
}
