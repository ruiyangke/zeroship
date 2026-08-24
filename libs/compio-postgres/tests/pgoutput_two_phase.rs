//! Two-phase pgoutput messages, against a real PostgreSQL walsender.
//!
//! Observed from PostgreSQL 16.14 on 2026-08-24, while the decoder still
//! returned `DecodeError::UnknownTag`: Begin Prepare was `0x62`, Prepare
//! `0x50`, Commit Prepared `0x4b`, Rollback Prepared `0x72`, and Stream
//! Prepare `0x70`. These bytes came from the live walsender, not a document.

use compio_postgres::replication::pgoutput::{self, PgOutputMessage};
use compio_postgres::replication::{ReplicationMessage, StartReplicationOptions, Streaming};
use compio_postgres::{Client, NoTls};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

#[allow(dead_code)]
mod common;

const WATCHDOG: Duration = Duration::from_secs(60);
const EARLY_DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Drop a replication slot once its walsender has actually let go.
///
/// Dropping the stream closes the connection CLIENT-side; the server takes a
/// moment longer to retire the walsender, and until it does the slot is still
/// `active` and `pg_drop_replication_slot` fails with 55006. That window is
/// invisible when the test runs alone and opens up under full-suite load,
/// which is exactly the shape that produces a flake nobody can reproduce.
///
/// Polls the server rather than sleeping a fixed amount: a sleep long enough
/// to be safe on a loaded machine is wasted on every green run, and one
/// tuned on an idle machine is the flake again.
async fn drop_slot_when_released(client: &Client, slot: &str) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match client
            .execute(&format!("SELECT pg_drop_replication_slot('{slot}')"), &[])
            .await
        {
            Ok(_) => return Ok(()),
            Err(error) => {
                let still_held = error.code().is_some_and(|code| code.code() == "55006");
                if !still_held || std::time::Instant::now() >= deadline {
                    return Err(common::error_chain(&error));
                }
                compio::time::sleep(Duration::from_millis(25)).await;
            }
        }
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
        let base = common::test_object_name("cpg two phase early");
        let table = format!("{base}_t");
        let publication = format!("{base}_p");
        let slot = format!("{base}_s");
        let gid = format!("{base}_gid");
        let setup = client().await;
        common::sweep_stale_replication_slots(&setup).await;

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
            NoTls,
            &common::replication_config("cpg_two_phase_early"),
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
                    format!("replication stream failed: {}", common::error_chain(&error))
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
        let slot_dropped = drop_slot_when_released(&setup, &slot).await;
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

#[compio::test]
async fn prepared_transactions_expose_every_two_phase_frame() {
    compio::time::timeout(WATCHDOG, async {
        let base = common::test_object_name("cpg two phase observe");
        let table = format!("{base}_t");
        let publication = format!("{base}_p");
        let slot = format!("{base}_s");
        let commit_gid = format!("{base}_commit");
        let rollback_gid = format!("{base}_rollback");
        let stream_commit_gid = format!("{base}_stream_commit");
        let stream_rollback_gid = format!("{base}_stream_rollback");
        let setup = client().await;
        common::sweep_stale_replication_slots(&setup).await;

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

        setup
            .batch_execute(&format!(
                "SELECT pg_drop_replication_slot('{slot}')
                   FROM pg_replication_slots WHERE slot_name = '{slot}';
                 DROP PUBLICATION IF EXISTS {publication};
                 DROP TABLE IF EXISTS {table};
                 CREATE TABLE {table}(id int primary key, payload text);
                 CREATE PUBLICATION {publication} FOR TABLE {table};"
            ))
            .await
            .expect("fixture setup failed");
        setup
            .batch_execute(&format!(
                "SELECT pg_create_logical_replication_slot(
                    '{slot}', 'pgoutput', false, true);"
            ))
            .await
            .expect("TWO_PHASE slot setup failed");

        let mut config = common::replication_config("cpg_two_phase_observe");
        config.options("-c logical_decoding_work_mem=64kB");
        let replication = compio_postgres::replication::connect_replication(NoTls, &config)
            .await
            .expect("replication connect failed");
        let mut stream = replication
            .start_logical_replication(StartReplicationOptions {
                slot_name: &slot,
                proto_version: 4,
                publication_names: &[&publication],
                streaming: Streaming::Parallel,
                two_phase: true,
                ..Default::default()
            })
            .await
            .expect("START_REPLICATION with two_phase failed");

        let large_commit = format!(
            "INSERT INTO {table}
             SELECT n,
                    (SELECT string_agg(md5((n * 1000 + part)::text), '')
                       FROM generate_series(1, 64) AS part)
               FROM generate_series(100, 299) AS n;"
        );
        let large_rollback = format!(
            "INSERT INTO {table}
             SELECT n,
                    (SELECT string_agg(md5((n * 1000 + part)::text), '')
                       FROM generate_series(1, 64) AS part)
               FROM generate_series(300, 499) AS n;"
        );
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

            setup.batch_execute("BEGIN").await?;
            let stream_commit_xid = current_xid(&setup).await;
            setup.batch_execute(&large_commit).await?;
            setup
                .batch_execute(&format!("PREPARE TRANSACTION '{stream_commit_gid}'"))
                .await?;
            setup
                .batch_execute(&format!("COMMIT PREPARED '{stream_commit_gid}'"))
                .await?;

            setup.batch_execute("BEGIN").await?;
            let stream_rollback_xid = current_xid(&setup).await;
            setup.batch_execute(&large_rollback).await?;
            setup
                .batch_execute(&format!("PREPARE TRANSACTION '{stream_rollback_gid}'"))
                .await?;
            setup
                .batch_execute(&format!("ROLLBACK PREPARED '{stream_rollback_gid}'"))
                .await?;

            setup
                .batch_execute(&format!("INSERT INTO {table} VALUES (1000, 'sentinel')"))
                .await?;

            Ok::<_, compio_postgres::Error>((
                commit_xid,
                rollback_xid,
                stream_commit_xid,
                stream_rollback_xid,
            ))
        };
        let observe = async {
            let mut decoder = pgoutput::Decoder::new();
            let mut messages = Vec::new();
            let mut first_error = None;
            loop {
                match stream.next().await.expect("replication stream failed") {
                    Some(ReplicationMessage::XLogData { body, .. }) => {
                        // The sentinel's ordinary Commit terminates the read
                        // even while this test is red on a newly observed tag.
                        // That lets the producer finish and the fixture clean
                        // up before the stored decoder error fails the test.
                        let done = body.first() == Some(&b'C');
                        match decoder.decode(&body) {
                            Ok(message) => messages.push(message),
                            Err(error) => {
                                eprintln!(
                                    "OBSERVED two-phase tag 0x{:02x} ({:?}), len={}, \
                                     error={error:?}",
                                    body[0],
                                    body[0] as char,
                                    body.len()
                                );
                                if first_error.is_none() {
                                    first_error = Some((body[0], error));
                                }
                            }
                        }
                        if done {
                            return (messages, first_error);
                        }
                    }
                    Some(ReplicationMessage::PrimaryKeepalive { .. }) => {}
                    None => return (messages, first_error),
                }
            }
        };

        let (produced, observed) = futures_util::future::join(produce, observe).await;
        let (commit_xid, rollback_xid, stream_commit_xid, stream_rollback_xid) =
            produced.expect("two-phase transaction sequence failed");
        let (messages, decode_error) = observed;
        drop(stream);
        let slot_dropped = drop_slot_when_released(&setup, &slot).await;
        setup
            .batch_execute(&format!(
                "DROP PUBLICATION {publication};
                 DROP TABLE {table};"
            ))
            .await
            .expect("fixture cleanup failed");
        slot_dropped.unwrap_or_else(|error| panic!("slot cleanup failed: {error}"));

        if let Some((tag, error)) = decode_error {
            panic!("two-phase frame 0x{tag:02x} did not decode: {error:?}");
        }

        let expected = BTreeMap::from([
            (commit_gid.as_str(), commit_xid),
            (rollback_gid.as_str(), rollback_xid),
            (stream_commit_gid.as_str(), stream_commit_xid),
            (stream_rollback_gid.as_str(), stream_rollback_xid),
        ]);
        let mut begins = BTreeMap::new();
        let mut prepares = BTreeMap::new();
        let mut stream_prepares = BTreeMap::new();
        let mut commits = BTreeSet::new();
        let mut rollbacks = BTreeSet::new();
        let mut streamed_xids = BTreeSet::new();

        for message in &messages {
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
                        begins
                            .insert(
                                gid.clone(),
                                (*prepare_lsn, *end_lsn, *prepare_timestamp, *xid),
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
                        prepares
                            .insert(
                                gid.clone(),
                                (*prepare_lsn, *end_lsn, *prepare_timestamp, *xid),
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
                        stream_prepares
                            .insert(
                                gid.clone(),
                                (*prepare_lsn, *end_lsn, *prepare_timestamp, *xid),
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
                        commits.insert(gid.clone()),
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
                        rollbacks.insert(gid.clone()),
                        "duplicate RollbackPrepared for {gid}"
                    );
                }
                PgOutputMessage::StreamStart { xid, .. } => {
                    streamed_xids.insert(*xid);
                }
                _ => {}
            }
        }

        let small_gids = BTreeSet::from([commit_gid.clone(), rollback_gid.clone()]);
        let streamed_gids =
            BTreeSet::from([stream_commit_gid.clone(), stream_rollback_gid.clone()]);
        assert_eq!(begins.keys().cloned().collect::<BTreeSet<_>>(), small_gids);
        assert_eq!(
            prepares.keys().cloned().collect::<BTreeSet<_>>(),
            small_gids
        );
        assert_eq!(
            stream_prepares.keys().cloned().collect::<BTreeSet<_>>(),
            streamed_gids
        );
        for gid in [&commit_gid, &rollback_gid] {
            assert_eq!(
                begins.get(gid),
                prepares.get(gid),
                "BeginPrepare and Prepare disagree for {gid}"
            );
        }
        assert_eq!(
            commits,
            BTreeSet::from([commit_gid.clone(), stream_commit_gid.clone()])
        );
        assert_eq!(
            rollbacks,
            BTreeSet::from([rollback_gid.clone(), stream_rollback_gid.clone()])
        );
        assert!(streamed_xids.contains(&stream_commit_xid));
        assert!(streamed_xids.contains(&stream_rollback_xid));
    })
    .await
    .expect("two-phase live test exceeded its watchdog");
}
