//! Streaming of in-progress transactions, against a real walsender.
//!
//! With `streaming` on, a transaction the walsender decides to stream is sent
//! BEFORE it commits, and the framing changes shape: `Begin`/`Commit` are
//! replaced, not supplemented - `StreamStart`/`StreamStop` around each chunk
//! and one `StreamCommit` at the end. A consumer written against the
//! non-streaming shape therefore waits for a `Commit` that never arrives. That
//! is why this is not merely an extra option: turning it on changes the
//! contract.
//!
//! Every walsender here runs with `support::STREAM_EVERY_CHANGE`, so the
//! decision to stream is the session's rather than memory pressure's: each
//! change is streamed as it is decoded, every transaction arrives in chunks,
//! and a subtransaction's changes reach the stream before its abort does.
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

use crate::support;

const WATCHDOG: Duration = Duration::from_secs(120);
const ABORT_OBSERVATION: Duration = Duration::from_secs(3);

/// Rows in the streamed transaction. Each is streamed as it is decoded, so a
/// few dozen give every transaction many chunks.
const ROWS: i32 = 40;

async fn client() -> Client {
    let url = support::test_url();
    match compio_postgres::connect(&url, support::suite_tls()).await {
        Ok((client, connection)) => {
            compio::runtime::spawn(async move {
                if let Err(error) = connection.run().await {
                    eprintln!("connection error: {}", support::error_chain(&error));
                }
            })
            .detach();
            client
        }
        Err(error) => support::postgres_unreachable(&url, &error),
    }
}

/// Collect messages until the transaction ends, however it ends: a
/// non-streamed `Commit`, a streamed `StreamCommit`, or a `StreamAbort`.
///
/// `write` is run AFTER the stream is started, so the walsender decodes the
/// transaction live. Writing first and reading afterwards lets a committed
/// transaction's rolled-back subtransaction be discarded in memory without
/// ever being streamed, which leaves no `StreamAbort` to observe.
async fn collect<F, Fut>(
    slot: &str,
    streaming: Streaming,
    publication: &str,
    write: F,
) -> Vec<PgOutputMessage>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = u32>,
{
    try_collect(slot, streaming, publication, write)
        .await
        .unwrap_or_else(|error| panic!("a live frame failed to decode: {error}"))
}

async fn try_collect<F, Fut>(
    slot: &str,
    streaming: Streaming,
    publication: &str,
    write: F,
) -> Result<Vec<PgOutputMessage>, String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = u32>,
{
    let mut stream = start_stream(slot, streaming, publication).await?;

    // The write runs while the stream is live, so the walsender decodes the
    // transaction as it is written.
    let xid = write().await;

    let mut read = Read::new(xid);
    read.until(&mut stream, |_| false).await?;
    Ok(read.messages)
}

/// Open a replication stream on `slot` whose walsender streams every change
/// as it is decoded.
async fn start_stream(
    slot: &str,
    streaming: Streaming,
    publication: &str,
) -> Result<
    compio_postgres::replication::ReplicationStream<
        compio_postgres::Socket,
        impl compio::io::AsyncRead + compio::io::AsyncWrite + Unpin,
    >,
    String,
> {
    let mut config = support::replication_config("cpg_streaming");
    config.options(support::STREAM_EVERY_CHANGE.to_owned());

    let replication =
        compio_postgres::replication::connect_replication(support::suite_tls(), &config)
            .await
            .map_err(|error| {
                format!(
                    "replication connect failed: {}",
                    support::error_chain(&error)
                )
            })?;

    let proto_version = match streaming {
        Streaming::Parallel => 4,
        Streaming::Off | Streaming::On => 2,
    };

    replication
        .start_logical_replication(StartReplicationOptions {
            slot_name: slot,
            publication_names: &[publication],
            proto_version,
            streaming,
            ..Default::default()
        })
        .await
        .map_err(|error| format!("START_REPLICATION failed: {}", support::error_chain(&error)))
}

/// The frames one transaction has delivered so far.
struct Read {
    decoder: pgoutput::Decoder,
    // A concurrent transaction's frames reach this slot too; they are dropped
    // instead of being allowed to end the read (`support::StreamOwnership`).
    ownership: support::StreamOwnership,
    messages: Vec<PgOutputMessage>,
}

impl Read {
    fn new(xid: u32) -> Self {
        Self {
            decoder: pgoutput::Decoder::new(),
            ownership: support::StreamOwnership::new(xid),
            messages: Vec::new(),
        }
    }

    /// Read until the transaction ends or `enough` holds for what it has
    /// delivered. Returns whether the transaction ended.
    async fn until<T>(
        &mut self,
        stream: &mut compio_postgres::replication::ReplicationStream<compio_postgres::Socket, T>,
        enough: impl Fn(&[PgOutputMessage]) -> bool,
    ) -> Result<bool, String>
    where
        T: compio::io::AsyncRead + compio::io::AsyncWrite + Unpin,
    {
        loop {
            match stream.next().await.map_err(|error| {
                format!(
                    "replication stream failed: {}",
                    support::error_chain(&error)
                )
            })? {
                Some(ReplicationMessage::XLogData { body, .. }) => {
                    let message = self
                        .decoder
                        .decode(&body)
                        .map_err(|error| format!("{error:?}"))?;
                    let (keep, terminal) = self.ownership.classify(&message);
                    if keep {
                        self.messages.push(message);
                    }
                    if terminal {
                        return Ok(true);
                    }
                    if keep && enough(&self.messages) {
                        return Ok(false);
                    }
                }
                Some(ReplicationMessage::PrimaryKeepalive { .. }) => continue,
                None => return Ok(true),
            }
        }
    }
}

/// Read a transaction whose child subtransaction is rolled back only once
/// every row the child inserted has arrived on the stream.
///
/// The order is the subject, and PostgreSQL decides it two ways. A logical
/// walsender decodes only flushed WAL (`WalSndWaitForWal` waits on
/// `GetFlushRecPtr`), and an open transaction's records stay unflushed until
/// something flushes past them. And a change decoded after its subtransaction
/// has aborted is dropped from the stream rather than sent: streaming sets
/// `CheckXidAlive` to the change's xid, any catalog scan the decode needs then
/// raises `ERRCODE_TRANSACTION_ROLLBACK` for an aborted xid
/// (`HandleConcurrentAbort`), and `ReorderBufferProcessTXN` handles that by
/// discarding the rest of the (sub)transaction's changes. Whether a scan is
/// needed depends on the walsender's caches, which other sessions' catalog
/// changes invalidate. A child rolled back in the same batch that wrote it
/// therefore reached the stream only when the walsender happened to decode it
/// first - and otherwise arrived as an abort with no rows of its own.
///
/// So the child stays open until its rows are on the stream. Its WAL is
/// flushed from another session, whose transactional logical message makes its
/// commit flush the WAL through its own commit record. The parent alters the
/// table's catalog entry before the savepoint, so decoding the child's first
/// row always needs a catalog scan: rolled back before the walsender reaches
/// it, the child loses every row on every run, not only when other sessions'
/// catalog changes happen to have emptied the walsender's caches.
async fn collect_child_abort(fixture: &Fixture) -> Result<Vec<PgOutputMessage>, String> {
    let mut stream =
        start_stream(&fixture.slot, Streaming::On, &fixture.publication).await?;
    let top = fixture.begin_and_xid().await;
    fixture
        .setup
        .batch_execute(&format!(
            "INSERT INTO {t} VALUES (1, 'top before');
             ALTER TABLE {t} ALTER COLUMN pad SET STATISTICS 100;
             SAVEPOINT streamed_child;
             INSERT INTO {t}
                SELECT g, repeat('x', 200)
                  FROM generate_series(2, {ROWS}) g;",
            t = fixture.table,
        ))
        .await
        .expect("the child subtransaction's inserts failed");
    client()
        .await
        .batch_execute("SELECT pg_logical_emit_message(true, 'compio-postgres-flush', '')")
        .await
        .expect("flush the open transaction's WAL from another session");

    let mut read = Read::new(top);
    let child_rows = usize::try_from(ROWS - 1).expect("the child inserts a positive row count");
    let ended = read
        .until(&mut stream, |messages| {
            messages
                .iter()
                .filter(|message| {
                    matches!(message, PgOutputMessage::Insert { xid: Some(xid), .. } if *xid != top)
                })
                .count()
                == child_rows
        })
        .await?;
    if ended {
        return Err(format!(
            "the transaction ended before the child's rows arrived: {:?}",
            read.messages
        ));
    }

    fixture
        .setup
        .batch_execute(&format!(
            "ROLLBACK TO SAVEPOINT streamed_child;
             INSERT INTO {t} VALUES ({after}, 'top after');
             COMMIT;",
            t = fixture.table,
            after = ROWS + 1,
        ))
        .await
        .expect("rolling back the child and committing the parent failed");
    read.until(&mut stream, |_| false).await?;
    Ok(read.messages)
}

struct Fixture {
    table: String,
    publication: String,
    slot: String,
    setup: Client,
}

impl Fixture {
    async fn create(logical: &str) -> Self {
        let base = support::test_object_name(logical);
        let fixture = Self {
            table: format!("{base}_t"),
            publication: format!("{base}_p"),
            slot: format!("{base}_s"),
            setup: client().await,
        };
        support::sweep_stale_test_objects(&fixture.setup).await;
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

    /// Open an explicit transaction and return the xid `PostgreSQL` assigned.
    ///
    /// THE XID IS WHAT TELLS A TEST'S TRANSACTION FROM A CONCURRENT ONE. A
    /// walsender that streams every change streams every concurrent
    /// transaction too, and `PostgreSQL` 16 sends the `StreamStart`/`StreamCommit`
    /// framing for it to every slot whose stream is reading that LSN - including
    /// slots whose publication filters out every row it contains. A reader that
    /// stopped at the first `StreamCommit` could therefore stop on another
    /// test's empty stream and see a fraction of its own transaction. Keeping
    /// only frames carrying this xid is what scopes the read.
    async fn begin_and_xid(&self) -> u32 {
        self.setup
            .batch_execute("BEGIN")
            .await
            .expect("begin the fixture transaction");
        let xid: i64 = self
            .setup
            .query_one_scalar("SELECT txid_current()", &[])
            .await
            .expect("read the transaction id");
        xid as u32
    }

    async fn write_big_transaction(&self, ending: &str) -> u32 {
        let xid = self.begin_and_xid().await;
        self.setup
            .batch_execute(&format!(
                "INSERT INTO {t} SELECT g, repeat('x', 200) FROM generate_series(1, {ROWS}) g;
                 {ending};",
                t = self.table,
            ))
            .await
            .expect("bulk transaction failed");
        xid
    }

    async fn write_big_subtransaction(&self) -> u32 {
        let xid = self.begin_and_xid().await;
        self.setup
            .batch_execute(&format!(
                "INSERT INTO {t}
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
        xid
    }

    async fn drop_all(&self) {
        support::drop_replication_slot(&self.setup, &self.slot)
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

async fn observe_abort(
    slot: &str,
    streaming: Streaming,
    publication: &str,
    xid: u32,
) -> AbortOutcome {
    // The bound is the CALLER's, not the walsender's. A walsender owes no bytes
    // at a frame boundary, so PostgreSQL 18 can remain healthily silent for
    // this rollback and the observation has to give up on its own clock. A dead
    // stream is a different condition and still surfaces: an unreadable socket
    // returns an error here, which is a failure, while silence expires the
    // bound. Whether the driver answers a reply-requested keepalive is pinned
    // separately by a scripted peer with no server and no clock; this
    // observation must not arm a server-side deadline, because a CPU-starved
    // client that missed it would have the walsender closed and report that as
    // a dead stream.
    //
    // The transaction is written BEFORE this call, and the stream starts at the
    // slot's confirmed_flush_lsn, so it replays the rollback from a committed
    // WAL record. Starting the read first would only decode it live and buy
    // nothing while shortening the window the bound covers.
    let messages = match compio::time::timeout(
        ABORT_OBSERVATION,
        try_collect(slot, streaming, publication, || async { xid }),
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
        let messages = collect(&fixture.slot, Streaming::On, &fixture.publication, || {
            fixture.write_big_transaction("COMMIT")
        })
        .await;
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
            "a walsender streaming every change must deliver the transaction in MORE \
             than one chunk, got {starts}; if it is 1 the walsender ignored the streaming \
             mode sent in its startup options and this test proves nothing"
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
        let decoded = try_collect(&fixture.slot, Streaming::On, &fixture.publication, || {
            fixture.write_big_subtransaction()
        })
        .await;
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
        let decoded = collect_child_abort(&fixture).await;
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
        let child_rows_before_abort = messages[..abort_index]
            .iter()
            .filter(|message| {
                matches!(message, PgOutputMessage::Insert { xid: Some(xid), .. } if *xid == child_xid)
            })
            .count();
        assert_eq!(
            child_rows_before_abort,
            usize::try_from(ROWS - 1).expect("the child inserts a positive row count"),
            "every row the child inserted must arrive, carrying the child's xid, before \
             the abort that invalidates them"
        );
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
        let messages = collect(&fixture.slot, Streaming::Off, &fixture.publication, || {
            fixture.write_big_transaction("COMMIT")
        })
        .await;
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
        let xid = fixture.write_big_transaction("ROLLBACK").await;
        let outcome = observe_abort(&fixture.slot, Streaming::On, &fixture.publication, xid).await;
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
        let xid = fixture.write_big_transaction("ROLLBACK").await;
        let outcome = observe_abort(
            &fixture.slot,
            Streaming::Parallel,
            &fixture.publication,
            xid,
        )
        .await;
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
            support::suite_tls(),
            &support::replication_config("cpg_streaming_refusal"),
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
            .expect_err("an option the proto_version cannot carry must be refused");

        assert!(
            error.as_db_error().is_none(),
            "the invalid combination reached PostgreSQL instead of being refused locally: {}",
            support::error_chain(&error)
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
        support::suite_tls(),
        &support::replication_config("cpg_protocol_ceiling"),
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
        .expect_err("proto_version 5 must be refused before START_REPLICATION");
    let chain = support::error_chain(&error);

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
        support::suite_tls(),
        &support::replication_config("cpg_protocol_floor"),
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
        .expect_err("proto_version 0 must be refused before START_REPLICATION");
    let chain = support::error_chain(&error);

    assert!(
        error.as_db_error().is_none(),
        "the invalid version reached PostgreSQL instead of being refused locally: {chain}"
    );
    assert!(
        chain.contains("proto_version") && chain.contains("1 or higher"),
        "the refusal must name the supported minimum: {chain}"
    );
}
