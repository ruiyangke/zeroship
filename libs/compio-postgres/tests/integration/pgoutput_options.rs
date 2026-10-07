//! The pgoutput options `START_REPLICATION` can carry, checked by their
//! EFFECT on a live stream rather than by the command not erroring.
//!
//! pgoutput refuses an option it does not know (`unrecognized pgoutput
//! option: nonsense`, measured 2026-08-24 on 16.14), so "the server accepted
//! it" only proves the spelling. It does not prove the option reached the
//! decoder, and it certainly does not prove the decoder can represent what
//! the option makes the server send. Each test below therefore asserts the
//! output CHANGES, and pairs with a control that leaves the option off.
//!
//! `binary` is the one that mattered most: `TupleColumn::Binary` existed, and
//! was unreachable. The decoder had an arm for the `b` tuple kind while no
//! request this driver could build would make any server emit one.

use compio_postgres::Client;
use compio_postgres::replication::pgoutput::{self, PgOutputMessage, TupleColumn};
use compio_postgres::replication::{OriginFilter, ReplicationMessage, StartReplicationOptions};
use std::time::Duration;

use crate::support;

const WATCHDOG: Duration = Duration::from_secs(30);

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

/// Read one transaction's worth of messages, stopping at its Commit.
///
/// Safe from a concurrent test's WAL: pgoutput emits no `Begin`/`Commit` for a
/// transaction with no change the slot's options accept, so a foreign
/// transaction cannot produce the `Commit` this stops at. The premise is
/// exercised by the `OriginFilter::None` arm of
/// `the_origin_none_option_drops_changes_replayed_from_a_peer`, whose peer
/// transaction has its only row dropped by the origin filter: if pgoutput still
/// emitted that transaction's `Begin`/`Commit`, `stream_until_commit` would stop
/// on the peer's `Commit` and `first_insert` would panic on a missing Insert.
async fn stream_until_commit(
    slot: &str,
    options: StartReplicationOptions<'_>,
) -> Vec<PgOutputMessage> {
    let replication = compio_postgres::replication::connect_replication(
        support::suite_tls(),
        &support::replication_config("cpg_pgoutput_options"),
    )
    .await
    .expect("replication connect failed");

    let mut stream = replication
        .start_logical_replication(options)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "START_REPLICATION on slot {slot} failed: {}",
                support::error_chain(&error)
            )
        });

    let mut messages = Vec::new();
    loop {
        match stream.next().await.expect("replication stream failed") {
            Some(ReplicationMessage::XLogData { body, .. }) => {
                let message = pgoutput::decode(&body).expect("a live frame failed to decode");
                let done = matches!(message, PgOutputMessage::Commit { .. });
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

/// Read until the transaction `xid` ends, keeping only frames that carry it.
///
/// A logical message lives in the database's WAL rather than in a publication's
/// tables, so a slot streaming with `messages: true` receives every session's
/// `pg_logical_emit_message` payload: the `compio-postgres-flush` messages
/// `pgoutput_streaming` and `pgoutput_subtransactions` emit to flush an open
/// transaction's WAL among them. Each arrives inside its own committed
/// transaction, and a reader that stops at the first `Commit` stops on that
/// foreign transaction and returns before its own message. Taking ownership
/// from `xid` is what keeps a foreign message - and the foreign `Commit` that
/// closes it - from ending this read.
async fn stream_transaction(
    slot: &str,
    options: StartReplicationOptions<'_>,
    xid: u32,
) -> Vec<PgOutputMessage> {
    let replication = compio_postgres::replication::connect_replication(
        support::suite_tls(),
        &support::replication_config("cpg_pgoutput_options"),
    )
    .await
    .expect("replication connect failed");

    let mut stream = replication
        .start_logical_replication(options)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "START_REPLICATION on slot {slot} failed: {}",
                support::error_chain(&error)
            )
        });

    let mut ownership = support::StreamOwnership::new(xid);
    let mut messages = Vec::new();
    loop {
        match stream.next().await.expect("replication stream failed") {
            Some(ReplicationMessage::XLogData { body, .. }) => {
                let message = pgoutput::decode(&body).expect("a live frame failed to decode");
                let (keep, terminal) = ownership.classify(&message);
                if keep {
                    messages.push(message);
                }
                if terminal {
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
                 CREATE TABLE {t}(id int primary key, label text);
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

    async fn drop_all(&self) {
        support::drop_replication_slot(&self.setup, &self.slot)
            .await
            .unwrap_or_else(|error| eprintln!("could not drop slot {}: {error}", self.slot));
        let _ = self
            .setup
            .batch_execute(&format!(
                "DROP PUBLICATION IF EXISTS {p};
                 DROP TABLE IF EXISTS {t};",
                p = self.publication,
                t = self.table,
            ))
            .await;
    }
}

fn first_insert(messages: &[PgOutputMessage]) -> &pgoutput::TupleData {
    messages
        .iter()
        .find_map(|message| match message {
            PgOutputMessage::Insert { new_tuple, .. } => Some(new_tuple),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no Insert among {messages:?}"))
}

/// `binary: true` makes the server send each value in its type's binary
/// representation, which is the only way `TupleColumn::Binary` is ever
/// produced.
#[compio::test]
async fn the_binary_option_delivers_values_in_binary_format() {
    compio::time::timeout(WATCHDOG, async {
        let fixture = Fixture::create("cpg opt binary").await;
        fixture
            .setup
            .batch_execute(&format!("INSERT INTO {} VALUES (42, 'x');", fixture.table))
            .await
            .expect("insert failed");

        let messages = stream_until_commit(
            &fixture.slot,
            StartReplicationOptions {
                slot_name: &fixture.slot,
                publication_names: &[&fixture.publication],
                binary: true,
                ..Default::default()
            },
        )
        .await;
        fixture.drop_all().await;

        let tuple = first_insert(&messages);
        // int4 42 is four bytes big-endian, NOT the two characters "42".
        assert_eq!(
            tuple.columns[0],
            TupleColumn::Binary(bytes::Bytes::from_static(&[0, 0, 0, 42])),
            "with binary on, an int4 must arrive as its 4-byte network form"
        );
    })
    .await
    .expect("the binary-option test exceeded its watchdog");
}

/// The control: the SAME insert, option off, arrives as text. Without this,
/// the test above would also pass if the driver had started sending binary
/// unconditionally.
#[compio::test]
async fn without_the_binary_option_values_arrive_as_text() {
    compio::time::timeout(WATCHDOG, async {
        let fixture = Fixture::create("cpg opt text").await;
        fixture
            .setup
            .batch_execute(&format!("INSERT INTO {} VALUES (42, 'x');", fixture.table))
            .await
            .expect("insert failed");

        let messages = stream_until_commit(
            &fixture.slot,
            StartReplicationOptions {
                slot_name: &fixture.slot,
                publication_names: &[&fixture.publication],
                ..Default::default()
            },
        )
        .await;
        fixture.drop_all().await;

        assert_eq!(
            first_insert(&messages).columns[0],
            TupleColumn::Text("42".to_owned()),
            "with binary off, an int4 must arrive as text"
        );
    })
    .await
    .expect("the text-default test exceeded its watchdog");
}

/// `messages: true` delivers `pg_logical_emit_message` payloads. Off, the
/// server withholds them and the decoder's `M` arm never runs.
#[compio::test]
async fn the_messages_option_decides_whether_logical_messages_arrive() {
    compio::time::timeout(WATCHDOG, async {
        for (want_messages, expectation) in [(true, "must arrive"), (false, "must not arrive")] {
            let fixture = Fixture::create(&format!("cpg opt msg {want_messages}")).await;
            // A prefix private to this test, so a concurrent test's message on
            // the same stream cannot answer for this one.
            let prefix = support::test_object_name(&format!("cpg opt msg {want_messages}"));
            // Transactional message plus a row, so there is always a Commit to
            // stop at even when the message itself is withheld.
            fixture
                .setup
                .batch_execute("BEGIN")
                .await
                .expect("begin failed");
            let xid: i64 = fixture
                .setup
                .query_one_scalar("SELECT txid_current()", &[])
                .await
                .expect("read the transaction id");
            fixture
                .setup
                .batch_execute(&format!(
                    "SELECT pg_logical_emit_message(true, '{prefix}', 'cpg_payload');
                     INSERT INTO {} VALUES (1, 'x');
                     COMMIT;",
                    fixture.table
                ))
                .await
                .expect("emit failed");

            let messages = stream_transaction(
                &fixture.slot,
                StartReplicationOptions {
                    slot_name: &fixture.slot,
                    publication_names: &[&fixture.publication],
                    messages: want_messages,
                    ..Default::default()
                },
                xid as u32,
            )
            .await;
            fixture.drop_all().await;

            let found = messages.iter().any(|message| {
                matches!(message, PgOutputMessage::Message { prefix: observed, content, .. }
                    if observed == &prefix && content.as_ref() == b"cpg_payload")
            });
            assert_eq!(
                found, want_messages,
                "with messages={want_messages} the logical message {expectation}: {messages:?}"
            );
        }
    })
    .await
    .expect("the messages-option test exceeded its watchdog");
}

/// A logical message another session emits reaches this slot too, because a
/// message lives in the database's WAL rather than in a publication's tables.
/// The foreign transaction commits FIRST, on its own session, so the ordering
/// is fixed rather than raced: a reader that stops at the first `Commit`
/// returns the foreign message and never reaches this test's own.
#[compio::test]
async fn a_foreign_logical_message_does_not_end_the_stream() {
    compio::time::timeout(WATCHDOG, async {
        let fixture = Fixture::create("cpg opt foreign msg").await;
        let emitter = client().await;
        emitter
            .batch_execute(
                "SELECT pg_logical_emit_message(true, 'cpg_foreign_prefix', 'cpg_foreign_payload')",
            )
            .await
            .expect("the foreign message failed");

        fixture
            .setup
            .batch_execute("BEGIN")
            .await
            .expect("begin failed");
        let xid: i64 = fixture
            .setup
            .query_one_scalar("SELECT txid_current()", &[])
            .await
            .expect("read the transaction id");
        fixture
            .setup
            .batch_execute(&format!(
                "SELECT pg_logical_emit_message(true, 'cpg_prefix', 'cpg_payload');
                 INSERT INTO {} VALUES (1, 'x');
                 COMMIT;",
                fixture.table
            ))
            .await
            .expect("own emit failed");

        let messages = stream_transaction(
            &fixture.slot,
            StartReplicationOptions {
                slot_name: &fixture.slot,
                publication_names: &[&fixture.publication],
                messages: true,
                ..Default::default()
            },
            xid as u32,
        )
        .await;
        fixture.drop_all().await;

        assert!(
            messages.iter().any(|message| {
                matches!(message, PgOutputMessage::Message { prefix, content, .. }
                    if prefix == "cpg_prefix" && content.as_ref() == b"cpg_payload")
            }),
            "the foreign transaction's Commit ended the stream before this \
             test's own message: {messages:?}"
        );
        assert!(
            !messages.iter().any(|message| {
                matches!(message, PgOutputMessage::Message { prefix, .. }
                    if prefix == "cpg_foreign_prefix")
            }),
            "a foreign prefix was kept in the owned transaction's frames: {messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|message| matches!(message, PgOutputMessage::Insert { .. })),
            "the owned transaction's Insert did not arrive: {messages:?}"
        );
    })
    .await
    .expect("the foreign-message test exceeded its watchdog");
}

/// A concurrent test's logical message must not end this test's read.
///
/// `pg_logical_emit_message` writes to the WAL of the whole database, and this
/// suite shares one database, so a slot decodes every other test's committed
/// messages. A read that stopped at the first `Commit` would return the foreign
/// transaction and never reach this test's own; the per-test prefixes keep the
/// two messages apart, and `stream_transaction` keeps the foreign transaction's
/// frames out of the read.
#[compio::test]
async fn a_foreign_logical_message_does_not_end_the_read_before_this_tests_own() {
    compio::time::timeout(WATCHDOG, async {
        let fixture = Fixture::create("cpg opt foreign msg").await;
        let prefix = support::test_object_name("cpg own prefix");
        let foreign_prefix = support::test_object_name("cpg foreign prefix");

        // Another session's committed transaction, earlier in the WAL than
        // this test's. Its message carries a prefix this test never emits.
        client()
            .await
            .batch_execute(&format!(
                "SELECT pg_logical_emit_message(true, '{foreign_prefix}', 'foreign');"
            ))
            .await
            .expect("the foreign logical message failed");

        fixture
            .setup
            .batch_execute("BEGIN")
            .await
            .expect("begin failed");
        let xid: i64 = fixture
            .setup
            .query_one_scalar("SELECT txid_current()", &[])
            .await
            .expect("read the transaction id");
        fixture
            .setup
            .batch_execute(&format!(
                "SELECT pg_logical_emit_message(true, '{prefix}', 'payload');
                 INSERT INTO {} VALUES (1, 'x');
                 COMMIT;",
                fixture.table
            ))
            .await
            .expect("this test's own emit failed");

        let messages = stream_transaction(
            &fixture.slot,
            StartReplicationOptions {
                slot_name: &fixture.slot,
                publication_names: &[&fixture.publication],
                messages: true,
                ..Default::default()
            },
            xid as u32,
        )
        .await;
        fixture.drop_all().await;

        assert!(
            messages.iter().any(|message| matches!(
                message,
                PgOutputMessage::Message { prefix: observed, content, .. }
                    if observed == &prefix && content.as_ref() == b"payload"
            )),
            "this test's own logical message must be the one the read reaches: {messages:?}"
        );
        assert!(
            !messages.iter().any(|message| matches!(
                message,
                PgOutputMessage::Message { prefix: observed, .. }
                    if observed == &foreign_prefix
            )),
            "a foreign transaction's message must not be part of this test's read: {messages:?}"
        );
    })
    .await
    .expect("the foreign-message scoping test exceeded its watchdog");
}

/// `origin: None` drops changes that arrived from another replication origin
/// and keeps the ones written here.
///
/// A second server is NOT needed to produce a foreign-origin change.
/// `pg_replication_origin_session_setup` stamps an origin onto everything the
/// current session writes, which is exactly what a replayed change carries.
/// So one connection writes a row "as a peer", another writes a row plainly,
/// and the filter has something real to discriminate.
///
/// This test replaced one that only asserted `origin: None` was accepted and
/// still delivered local rows. That version passed whether or not filtering
/// worked at all - it could not fail for the reason the option exists.
#[compio::test]
async fn the_origin_none_option_drops_changes_replayed_from_a_peer() {
    compio::time::timeout(WATCHDOG, async {
        // The peer row is written first, in its own transaction, so it is the
        // first thing the stream would reach. `stream_until_commit` stops at
        // the first Commit, so whichever row it reports IS the filter's
        // answer: 'from_peer' when nothing was dropped, 'local' when the peer
        // transaction was.
        for (filter, expected) in [
            (OriginFilter::Any, "from_peer"),
            (OriginFilter::None, "local"),
        ] {
            origin_case(filter, expected).await;
        }
    })
    .await
    .expect("the origin-option test exceeded its watchdog");
}

async fn origin_case(filter: OriginFilter, expected: &str) {
    {
        let fixture = Fixture::create(&format!("cpg opt origin {expected}")).await;
        let origin = support::test_object_name(&format!("cpg peer {expected}"));

        // Session-scoped: the setup call and the INSERT must run on ONE
        // connection, so they go in a single batch. `batch_execute` is a
        // simple query, which is one round trip on one session.
        fixture
            .setup
            .batch_execute(&format!(
                "SELECT pg_replication_origin_drop('{origin}')
                   FROM pg_replication_origin WHERE roname = '{origin}';
                 SELECT pg_replication_origin_create('{origin}');
                 SELECT pg_replication_origin_session_setup('{origin}');
                 INSERT INTO {t} VALUES (1, 'from_peer');
                 SELECT pg_replication_origin_session_reset();",
                t = fixture.table,
            ))
            .await
            .expect("origin-stamped insert failed");
        fixture
            .setup
            .batch_execute(&format!(
                "INSERT INTO {} VALUES (5, 'local');",
                fixture.table
            ))
            .await
            .expect("insert failed");

        let messages = stream_until_commit(
            &fixture.slot,
            StartReplicationOptions {
                slot_name: &fixture.slot,
                publication_names: &[&fixture.publication],
                origin: filter,
                ..Default::default()
            },
        )
        .await;
        let _ = fixture
            .setup
            .batch_execute(&format!(
                "SELECT pg_replication_origin_drop('{origin}')
                   FROM pg_replication_origin WHERE roname = '{origin}';"
            ))
            .await;
        fixture.drop_all().await;

        assert_eq!(
            first_insert(&messages).columns[1],
            TupleColumn::Text(expected.to_owned()),
            "with origin={filter:?} the first change reaching the consumer must \
             be {expected:?}, got {messages:?}"
        );
    }
}
