//! Two-phase pgoutput messages, against a real PostgreSQL walsender.
//!
//! Observed from PostgreSQL 16.14 on 2026-08-24, while the decoder still
//! returned `DecodeError::UnknownTag`: Begin Prepare was `0x62`, Prepare
//! `0x50`, Commit Prepared `0x4b`, Rollback Prepared `0x72`, and Stream
//! Prepare `0x70`. These bytes came from the live walsender, not a document.

use compio_postgres::Client;
use compio_postgres::replication::pgoutput::{self, PgOutputMessage};
use compio_postgres::replication::{ReplicationMessage, StartReplicationOptions, Streaming};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::support;

const WATCHDOG: Duration = Duration::from_secs(60);
const EARLY_DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);

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

async fn current_xid(client: &Client) -> u32 {
    let row = client
        .query_one("SELECT (txid_current() % 4294967296)::text", &[])
        .await
        .expect("txid_current failed");
    let xid: String = row.get(0);
    xid.parse().expect("the server returned a non-u32 xid")
}

#[test]
fn two_phase_defaults_off() {
    assert!(!StartReplicationOptions::default().two_phase);
}

/// `two_phase: true` can enable a slot that was not created with TWO_PHASE, so
/// PREPARE itself becomes visible. The larger decode test below deliberately
/// starts with a TWO_PHASE slot, as a subscriber normally would; this plain
/// slot is the control that proves the start option itself reached pgoutput.
#[compio::test]
async fn two_phase_start_option_enables_a_plain_slot_before_commit() {
    compio::time::timeout(WATCHDOG, async {
        let base = support::test_object_name("cpg two phase early");
        let table = format!("{base}_t");
        let publication = format!("{base}_p");
        let slot = format!("{base}_s");
        let gid = format!("{base}_gid");
        let setup = client().await;
        support::sweep_stale_replication_slots(&setup).await;

        setup
            .batch_execute(&format!(
                "DROP PUBLICATION IF EXISTS {publication};
                 DROP TABLE IF EXISTS {table};
                 CREATE TABLE {table}(id int primary key);
                 CREATE PUBLICATION {publication} FOR TABLE {table};"
            ))
            .await
            .expect("fixture setup failed");
        setup
            .batch_execute(&format!(
                "SELECT pg_create_logical_replication_slot('{slot}', 'pgoutput');"
            ))
            .await
            .expect("plain slot setup failed");

        let replication = compio_postgres::replication::connect_replication(
            support::suite_tls(),
            &support::replication_config("cpg_two_phase_early"),
        )
        .await
        .expect("replication connect failed");
        let mut stream = replication
            .start_logical_replication(StartReplicationOptions {
                slot_name: &slot,
                proto_version: 3,
                publication_names: &[&publication],
                two_phase: true,
                ..Default::default()
            })
            .await
            .expect("START_REPLICATION with two_phase failed");

        setup.batch_execute("BEGIN").await.expect("BEGIN failed");
        let xid = current_xid(&setup).await;
        setup
            .batch_execute(&format!("INSERT INTO {table} VALUES (1)"))
            .await
            .expect("INSERT failed");
        setup
            .batch_execute(&format!("PREPARE TRANSACTION '{gid}'"))
            .await
            .expect("PREPARE TRANSACTION failed");

        let delivered = compio::time::timeout(EARLY_DELIVERY_TIMEOUT, async {
            let mut decoder = pgoutput::Decoder::new();
            let mut begin = None;
            loop {
                match stream.next().await.map_err(|error| {
                    format!(
                        "replication stream failed: {}",
                        support::error_chain(&error)
                    )
                })? {
                    Some(ReplicationMessage::XLogData { body, .. }) => {
                        match decoder
                            .decode(&body)
                            .map_err(|error| format!("live frame did not decode: {error:?}"))?
                        {
                            PgOutputMessage::BeginPrepare {
                                prepare_lsn,
                                end_lsn,
                                prepare_timestamp,
                                xid,
                                gid: observed_gid,
                            } if observed_gid == gid => {
                                begin = Some((prepare_lsn, end_lsn, prepare_timestamp, xid));
                            }
                            PgOutputMessage::Prepare {
                                flags,
                                prepare_lsn,
                                end_lsn,
                                prepare_timestamp,
                                xid,
                                gid: observed_gid,
                            } if observed_gid == gid => {
                                return Ok::<_, String>((
                                    begin.ok_or_else(|| {
                                        "Prepare arrived without BeginPrepare".to_owned()
                                    })?,
                                    (flags, prepare_lsn, end_lsn, prepare_timestamp, xid),
                                ));
                            }
                            _ => {}
                        }
                    }
                    Some(ReplicationMessage::PrimaryKeepalive { .. }) => {}
                    None => return Err("replication ended before Prepare".to_owned()),
                }
            }
        })
        .await
        .map_err(|error| {
            format!(
                "PREPARE was not delivered within {EARLY_DELIVERY_TIMEOUT:?}; \
                 the two_phase start option was not honored: {error}"
            )
        })
        .and_then(|observed| observed);

        let finish = if delivered.is_ok() {
            "COMMIT PREPARED"
        } else {
            "ROLLBACK PREPARED"
        };
        let finish_result = setup.batch_execute(&format!("{finish} '{gid}'")).await;

        drop(stream);
        let slot_dropped = support::drop_replication_slot(&setup, &slot).await;
        let cleanup_result = setup
            .batch_execute(&format!(
                "DROP PUBLICATION {publication};
                 DROP TABLE {table};"
            ))
            .await;
        slot_dropped.unwrap_or_else(|error| panic!("slot cleanup failed: {error}"));
        finish_result.unwrap_or_else(|error| panic!("{finish} failed: {error}"));
        cleanup_result.expect("fixture cleanup failed");

        let (begin, prepare) =
            delivered.unwrap_or_else(|error| panic!("two-phase early delivery failed: {error}"));

        assert_eq!(prepare.0, 0);
        assert_eq!(begin, (prepare.1, prepare.2, prepare.3, prepare.4));
        assert_eq!(prepare.4, xid);
        assert!(prepare.1 > 0 && prepare.1 <= prepare.2);
        assert!(prepare.3 > 0);
    })
    .await
    .expect("two-phase early-delivery test exceeded its watchdog");
}

/// How a walsender session decides when to stream a transaction, fixed by the
/// session setting rather than by memory pressure (see
/// `support::STREAM_EVERY_CHANGE`).
#[derive(Clone, Copy)]
enum Decoding {
    /// Every transaction arrives whole: `BeginPrepare` ... `Prepare`.
    Buffered,
    /// Every transaction arrives in chunks: `StreamStart` ... `StreamPrepare`.
    Immediate,
}

impl Decoding {
    /// The walsender session's startup options.
    const fn options(self) -> &'static str {
        match self {
            Self::Buffered => support::STREAM_NOTHING,
            Self::Immediate => support::STREAM_EVERY_CHANGE,
        }
    }
}

/// Start a two-phase, parallel-streaming pgoutput stream on `slot`.
async fn start_stream(
    slot: &str,
    publication: &str,
    decoding: Decoding,
) -> compio_postgres::replication::ReplicationStream<
    compio_postgres::Socket,
    impl compio::io::AsyncRead + compio::io::AsyncWrite + Unpin,
> {
    let mut config = support::replication_config("cpg_two_phase_observe");
    config.options(decoding.options());
    let replication =
        compio_postgres::replication::connect_replication(support::suite_tls(), &config)
            .await
            .expect("replication connect failed");
    replication
        .start_logical_replication(StartReplicationOptions {
            slot_name: slot,
            proto_version: 4,
            publication_names: &[publication],
            streaming: Streaming::Parallel,
            two_phase: true,
            ..Default::default()
        })
        .await
        .expect("START_REPLICATION with two_phase failed")
}

/// What one stream delivered for this test's gids.
struct Observed {
    messages: Vec<PgOutputMessage>,
    decode_error: Option<(u8, pgoutput::DecodeError)>,
    stream_prepares_inside_a_chunk: usize,
}

/// Read `stream` until every gid in `expected_gids` has reached its commit or
/// rollback.
///
/// A prepared transaction is delivered to a `two_phase` slot WITHOUT regard to
/// the slot's publication filter, so a concurrent test that runs `PREPARE
/// TRANSACTION` puts its gid on this stream. Only the gids this test produced
/// are its own; a prepare family frame naming anything else belongs to another
/// session and is dropped, exactly as the sibling two-phase test ignores a
/// prepare it did not ask for. The stream is read until every one of this
/// test's gids has finished, not until the first `C` on the wire, because that
/// `C` can be another session's ordinary commit.
async fn observe<T>(
    stream: &mut compio_postgres::replication::ReplicationStream<compio_postgres::Socket, T>,
    expected_gids: &[&str],
) -> Observed
where
    T: compio::io::AsyncRead + compio::io::AsyncWrite + Unpin,
{
    let mut decoder = pgoutput::Decoder::new();
    let mut observed = Observed {
        messages: Vec::new(),
        decode_error: None,
        stream_prepares_inside_a_chunk: 0,
    };
    let mut finished: BTreeSet<String> = BTreeSet::new();
    loop {
        match stream.next().await.expect("replication stream failed") {
            Some(ReplicationMessage::XLogData { body, .. }) => {
                // Sampled BEFORE decoding, because decode is what clears it.
                // See the StreamPrepare placement assertion for why this is
                // worth recording.
                let chunk_open_before = decoder.stream_xid().is_some();
                match decoder.decode(&body) {
                    Ok(message) => {
                        let foreign = match &message {
                            PgOutputMessage::BeginPrepare { gid, .. }
                            | PgOutputMessage::Prepare { gid, .. }
                            | PgOutputMessage::StreamPrepare { gid, .. }
                            | PgOutputMessage::CommitPrepared { gid, .. }
                            | PgOutputMessage::RollbackPrepared { gid, .. } => {
                                !expected_gids.contains(&gid.as_str())
                            }
                            _ => false,
                        };
                        if !foreign {
                            if chunk_open_before
                                && matches!(message, PgOutputMessage::StreamPrepare { .. })
                            {
                                observed.stream_prepares_inside_a_chunk += 1;
                            }
                            match &message {
                                PgOutputMessage::CommitPrepared { gid, .. }
                                | PgOutputMessage::RollbackPrepared { gid, .. } => {
                                    finished.insert(gid.clone());
                                }
                                _ => {}
                            }
                            observed.messages.push(message);
                        }
                    }
                    Err(error) => {
                        eprintln!(
                            "OBSERVED two-phase tag 0x{:02x} ({:?}), len={}, error={error:?}",
                            body[0],
                            body[0] as char,
                            body.len()
                        );
                        if observed.decode_error.is_none() {
                            observed.decode_error = Some((body[0], error));
                        }
                    }
                }
                if finished.len() == expected_gids.len() {
                    return observed;
                }
            }
            Some(ReplicationMessage::PrimaryKeepalive { .. }) => {}
            None => return observed,
        }
    }
}

/// The two-phase frames one stream carried, keyed by gid, each checked for
/// the fields every frame of its kind must carry.
#[derive(Default)]
struct Frames {
    begins: BTreeMap<String, (u64, u64, i64, u32)>,
    prepares: BTreeMap<String, (u64, u64, i64, u32)>,
    stream_prepares: BTreeMap<String, (u64, u64, i64, u32)>,
    commits: BTreeSet<String>,
    rollbacks: BTreeSet<String>,
    streamed_xids: BTreeSet<u32>,
}

fn frames(messages: &[PgOutputMessage], expected: &BTreeMap<&str, u32>) -> Frames {
    let mut frames = Frames::default();
    for message in messages {
        match message {
            PgOutputMessage::BeginPrepare {
                prepare_lsn,
                end_lsn,
                prepare_timestamp,
                xid,
                gid,
            } => {
                assert_eq!(expected.get(gid.as_str()), Some(xid));
                assert!(*prepare_lsn > 0 && prepare_lsn <= end_lsn);
                assert!(*prepare_timestamp > 0);
                assert!(
                    frames
                        .begins
                        .insert(
                            gid.clone(),
                            (*prepare_lsn, *end_lsn, *prepare_timestamp, *xid)
                        )
                        .is_none(),
                    "duplicate BeginPrepare for {gid}"
                );
            }
            PgOutputMessage::Prepare {
                flags,
                prepare_lsn,
                end_lsn,
                prepare_timestamp,
                xid,
                gid,
            } => {
                assert_eq!(*flags, 0);
                assert_eq!(expected.get(gid.as_str()), Some(xid));
                assert!(*prepare_lsn > 0 && prepare_lsn <= end_lsn);
                assert!(*prepare_timestamp > 0);
                assert!(
                    frames
                        .prepares
                        .insert(
                            gid.clone(),
                            (*prepare_lsn, *end_lsn, *prepare_timestamp, *xid)
                        )
                        .is_none(),
                    "duplicate Prepare for {gid}"
                );
            }
            PgOutputMessage::StreamPrepare {
                flags,
                prepare_lsn,
                end_lsn,
                prepare_timestamp,
                xid,
                gid,
            } => {
                assert_eq!(*flags, 0);
                assert_eq!(expected.get(gid.as_str()), Some(xid));
                assert!(*prepare_lsn > 0 && prepare_lsn <= end_lsn);
                assert!(*prepare_timestamp > 0);
                assert!(
                    frames
                        .stream_prepares
                        .insert(
                            gid.clone(),
                            (*prepare_lsn, *end_lsn, *prepare_timestamp, *xid)
                        )
                        .is_none(),
                    "duplicate StreamPrepare for {gid}"
                );
            }
            PgOutputMessage::CommitPrepared {
                flags,
                commit_lsn,
                end_lsn,
                commit_timestamp,
                xid,
                gid,
            } => {
                assert_eq!(*flags, 0);
                assert_eq!(expected.get(gid.as_str()), Some(xid));
                assert!(*commit_lsn > 0 && commit_lsn <= end_lsn);
                assert!(*commit_timestamp > 0);
                assert!(
                    frames.commits.insert(gid.clone()),
                    "duplicate CommitPrepared for {gid}"
                );
            }
            PgOutputMessage::RollbackPrepared {
                flags,
                prepare_end_lsn,
                rollback_end_lsn,
                prepare_timestamp,
                rollback_timestamp,
                xid,
                gid,
            } => {
                assert_eq!(*flags, 0);
                assert_eq!(expected.get(gid.as_str()), Some(xid));
                assert!(*prepare_end_lsn > 0 && prepare_end_lsn <= rollback_end_lsn);
                assert!(*prepare_timestamp > 0 && prepare_timestamp <= rollback_timestamp);
                assert!(
                    frames.rollbacks.insert(gid.clone()),
                    "duplicate RollbackPrepared for {gid}"
                );
            }
            PgOutputMessage::StreamStart { xid, .. } => {
                frames.streamed_xids.insert(*xid);
            }
            _ => {}
        }
    }
    frames
}

/// One prepared transaction committed and one rolled back, read by two slots:
/// one whose walsender never streams and one whose walsender streams every
/// change. The pair is the whole two-phase vocabulary - `BeginPrepare`,
/// `Prepare`, `StreamPrepare`, `CommitPrepared`, `RollbackPrepared` - and
/// which half arrives on which stream is fixed by the session setting rather
/// than by how much else the server was decoding at the time.
#[compio::test]
async fn prepared_transactions_expose_every_two_phase_frame() {
    compio::time::timeout(WATCHDOG, async {
        let base = support::test_object_name("cpg two phase observe");
        let table = format!("{base}_t");
        let publication = format!("{base}_p");
        let buffered_slot = format!("{base}_b");
        let immediate_slot = format!("{base}_i");
        let commit_gid = format!("{base}_commit");
        let rollback_gid = format!("{base}_rollback");
        let setup = client().await;
        support::sweep_stale_replication_slots(&setup).await;

        let configured: String = setup
            .query_one("SHOW max_prepared_transactions", &[])
            .await
            .expect("SHOW max_prepared_transactions failed")
            .get(0);
        assert!(
            configured.parse::<u32>().expect("setting was not a u32") > 0,
            "the server cannot exercise PREPARE TRANSACTION: \
             max_prepared_transactions={configured}"
        );

        for slot in [&buffered_slot, &immediate_slot] {
            support::drop_replication_slot(&setup, slot)
                .await
                .unwrap_or_else(|error| panic!("fixture setup failed: {error}"));
        }
        setup
            .batch_execute(&format!(
                "DROP PUBLICATION IF EXISTS {publication};
                 DROP TABLE IF EXISTS {table};
                 CREATE TABLE {table}(id int primary key, payload text);
                 CREATE PUBLICATION {publication} FOR TABLE {table};"
            ))
            .await
            .expect("fixture setup failed");
        for slot in [&buffered_slot, &immediate_slot] {
            setup
                .batch_execute(&format!(
                    "SELECT pg_create_logical_replication_slot('{slot}', 'pgoutput', false, true);"
                ))
                .await
                .expect("TWO_PHASE slot setup failed");
        }

        let mut buffered = start_stream(&buffered_slot, &publication, Decoding::Buffered).await;
        let mut immediate = start_stream(&immediate_slot, &publication, Decoding::Immediate).await;

        let produce = async {
            setup.batch_execute("BEGIN").await?;
            let commit_xid = current_xid(&setup).await;
            setup
                .batch_execute(&format!("INSERT INTO {table} VALUES (1, 'commit')"))
                .await?;
            setup
                .batch_execute(&format!("PREPARE TRANSACTION '{commit_gid}'"))
                .await?;
            setup
                .batch_execute(&format!("COMMIT PREPARED '{commit_gid}'"))
                .await?;

            setup.batch_execute("BEGIN").await?;
            let rollback_xid = current_xid(&setup).await;
            setup
                .batch_execute(&format!("INSERT INTO {table} VALUES (2, 'rollback')"))
                .await?;
            setup
                .batch_execute(&format!("PREPARE TRANSACTION '{rollback_gid}'"))
                .await?;
            setup
                .batch_execute(&format!("ROLLBACK PREPARED '{rollback_gid}'"))
                .await?;

            Ok::<_, compio_postgres::Error>((commit_xid, rollback_xid))
        };
        let expected_gids = [commit_gid.as_str(), rollback_gid.as_str()];
        let (produced, (from_buffered, from_immediate)) = futures_util::future::join(
            produce,
            futures_util::future::join(
                observe(&mut buffered, &expected_gids),
                observe(&mut immediate, &expected_gids),
            ),
        )
        .await;
        let (commit_xid, rollback_xid) = produced.expect("two-phase transaction sequence failed");
        drop(buffered);
        drop(immediate);
        let mut slots_dropped = Vec::new();
        for slot in [&buffered_slot, &immediate_slot] {
            slots_dropped.push(support::drop_replication_slot(&setup, slot).await);
        }
        setup
            .batch_execute(&format!(
                "DROP PUBLICATION {publication};
                 DROP TABLE {table};"
            ))
            .await
            .expect("fixture cleanup failed");
        for dropped in slots_dropped {
            dropped.unwrap_or_else(|error| panic!("slot cleanup failed: {error}"));
        }

        for observed in [&from_buffered, &from_immediate] {
            if let Some((tag, error)) = &observed.decode_error {
                panic!("two-phase frame 0x{tag:02x} did not decode: {error:?}");
            }
        }

        let expected = BTreeMap::from([
            (commit_gid.as_str(), commit_xid),
            (rollback_gid.as_str(), rollback_xid),
        ]);
        let ours = BTreeSet::from([commit_gid.clone(), rollback_gid.clone()]);
        let our_xids = BTreeSet::from([commit_xid, rollback_xid]);
        let whole = frames(&from_buffered.messages, &expected);
        let streamed = frames(&from_immediate.messages, &expected);

        // The buffered walsender delivers each transaction whole.
        assert_eq!(whole.begins.keys().cloned().collect::<BTreeSet<_>>(), ours);
        assert_eq!(
            whole.prepares.keys().cloned().collect::<BTreeSet<_>>(),
            ours
        );
        assert!(
            whole.stream_prepares.is_empty() && whole.streamed_xids.is_disjoint(&our_xids),
            "the buffered walsender streamed a transaction it had room to hold"
        );
        for gid in &ours {
            assert_eq!(
                whole.begins.get(gid),
                whole.prepares.get(gid),
                "BeginPrepare and Prepare disagree for {gid}"
            );
        }

        // The immediate walsender delivers each transaction in chunks.
        assert_eq!(
            streamed
                .stream_prepares
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            ours
        );
        assert!(
            streamed.begins.is_empty() && streamed.prepares.is_empty(),
            "the immediate walsender delivered a transaction whole, so the session \
             setting did not reach it"
        );
        assert!(streamed.streamed_xids.is_superset(&our_xids));

        // WHERE a StreamPrepare lands, not just what it carries. `decode`
        // clears `stream_xid` on StreamPrepare, and `stream_xid` decides a
        // WIRE-FORMAT question: between StreamStart and StreamStop every
        // `R/Y/I/U/D/T/M` frame carries a leading u32 xid and outside a chunk
        // it does not. Clearing it inside an open chunk would therefore read
        // the next frame four bytes out of phase and take its relation oid
        // from the xid bytes - silent misattribution rather than a decode
        // error, which is why the field assertions above cannot see it.
        //
        // The `stream_prepares` check above is the floor: it already requires
        // every gid to have produced one on the immediate stream, so this
        // cannot pass by observing nothing. MEASURED on 16.14, 2026-08-26: 0
        // inside a chunk, and inverting the condition counts 2, so the
        // detector demonstrably fires and the zero is a verdict rather than a
        // counter that never ran.
        assert_eq!(
            from_immediate.stream_prepares_inside_a_chunk, 0,
            "{} StreamPrepare messages arrived between StreamStart and StreamStop, so \
             every frame after one is parsed without its leading xid",
            from_immediate.stream_prepares_inside_a_chunk
        );

        for frames in [&whole, &streamed] {
            assert_eq!(frames.commits, BTreeSet::from([commit_gid.clone()]));
            assert_eq!(frames.rollbacks, BTreeSet::from([rollback_gid.clone()]));
        }
    })
    .await
    .expect("two-phase live test exceeded its watchdog");
}
