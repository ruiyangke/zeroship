//! The pgoutput decoder against frames a REAL walsender produced.
//!
//! Every other test of this decoder builds its input by hand, with the
//! encoders in `replication.rs`'s `#[cfg(test)] mod encode`. That is a closed
//! loop: the encoder and the decoder are written from the same reading of the
//! protocol, so they agree on any field order, width, or sign that reading got
//! wrong. `replication_live.rs`'s own header records this exact failure once
//! already - hand-built `IDENTIFY_SYSTEM` rows were not the shape
//! `DataRowBody::buffer()` emits, and parser and fixture agreed anyway.
//!
//! So the expectations here are read from the SERVER, not written down:
//! `rel_id` is compared against `pg_class.oid`, `type_oid` against
//! `pg_attribute.atttypid`, and the tuple values against the rows that were
//! inserted. A field this decoder reads at the wrong offset cannot satisfy
//! them by agreeing with a fixture.

use compio_postgres::replication::pgoutput::{self, PgOutputMessage, TupleColumn};
use compio_postgres::replication::{ReplicationMessage, StartReplicationOptions};
use compio_postgres::{Client, Config, NoTls};
use std::time::Duration;

#[allow(dead_code)]
mod common;

const WATCHDOG: Duration = Duration::from_secs(30);

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

fn replication_config() -> Config {
    let parsed: Config = common::test_url().parse().expect("test DSN did not parse");
    let mut config = Config::new();
    if let Some(user) = parsed.get_user() {
        config.user(user);
    }
    if let Some(password) = parsed.get_password() {
        config.password(password);
    }
    if let Some(dbname) = parsed.get_dbname() {
        config.dbname(dbname);
    }
    for host in parsed.get_hosts() {
        match host {
            compio_postgres::config::Host::Tcp(name) => {
                config.host(name.clone());
            }
            #[cfg(unix)]
            compio_postgres::config::Host::Unix(path) => {
                panic!("this test needs a TCP endpoint, got {}", path.display())
            }
        }
    }
    for port in parsed.get_ports() {
        config.port(*port);
    }
    config.application_name("cpg_pgoutput_live");
    config
}

/// Every pgoutput message one transaction produced, in wire order.
///
/// Reads until the `Commit` that closes the first transaction, NOT until a
/// fixed message count. A count has to know how many messages the server
/// chose to send - `batch_execute` puts its statements in ONE implicit
/// transaction, so four DML statements yield one Begin/Commit pair around
/// them, not four - and a count that guesses high simply waits forever.
async fn decoded_stream(slot: &str, publication: &str) -> Vec<PgOutputMessage> {
    let mut replication =
        compio_postgres::replication::connect_replication(NoTls, &replication_config())
            .await
            .expect("replication connect failed");

    let mut stream = replication
        .start_logical_replication(StartReplicationOptions {
            slot_name: slot,
            start_lsn: "0/0",
            proto_version: 1,
            publication_names: &[publication],
        })
        .await
        .expect("START_REPLICATION failed");

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

fn text(column: &TupleColumn) -> &str {
    match column {
        TupleColumn::Text(value) => value,
        other => panic!("expected a text column, got {other:?}"),
    }
}

/// Insert, update, delete and truncate one table, then check every decoded
/// field against what the server's catalog says it should be.
#[compio::test]
async fn a_live_walsender_decodes_to_the_values_the_catalog_reports() {
    compio::time::timeout(WATCHDOG, async {
        let base = common::test_object_name("cpg pgoutput live");
        let table = format!("{base}_t");
        let publication = format!("{base}_p");
        let slot = format!("{base}_s");
        let setup = client().await;

        setup
            .batch_execute(&format!(
                "DROP PUBLICATION IF EXISTS \"{publication}\";
                 DROP TABLE IF EXISTS {table};
                 CREATE TABLE {table}(id int primary key, label text);
                 ALTER TABLE {table} REPLICA IDENTITY FULL;
                 CREATE PUBLICATION \"{publication}\" FOR TABLE {table};"
            ))
            .await
            .expect("setup failed");
        setup
            .batch_execute(&format!(
                "SELECT pg_drop_replication_slot('{slot}')
                   FROM pg_replication_slots WHERE slot_name = '{slot}';
                 SELECT pg_create_logical_replication_slot('{slot}', 'pgoutput');"
            ))
            .await
            .expect("slot setup failed");

        // REPLICA IDENTITY FULL makes the update and delete carry old tuples,
        // which is the arm that would otherwise go unexercised.
        setup
            .batch_execute(&format!(
                "INSERT INTO {table} VALUES (1, 'first');
                 UPDATE {table} SET label = 'second' WHERE id = 1;
                 DELETE FROM {table} WHERE id = 1;
                 TRUNCATE {table};"
            ))
            .await
            .expect("dml failed");

        // What the server says the answers are.
        let catalog = setup
            .query_one(
                "SELECT c.oid::int8,
                        (SELECT atttypid::int8 FROM pg_attribute
                          WHERE attrelid = c.oid AND attname = 'id'),
                        (SELECT atttypid::int8 FROM pg_attribute
                          WHERE attrelid = c.oid AND attname = 'label'),
                        n.nspname
                   FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                  WHERE c.relname = $1",
                &[&table],
            )
            .await
            .expect("catalog lookup failed");
        let rel_oid: i64 = catalog.get(0);
        let id_type_oid: i64 = catalog.get(1);
        let label_type_oid: i64 = catalog.get(2);
        let namespace: String = catalog.get(3);

        // One implicit transaction wraps all four statements, so this is a
        // single Begin / Relation / Insert / Update / Delete / Truncate /
        // Commit run rather than four transactions.
        let messages = decoded_stream(&slot, &publication).await;

        let _ = setup
            .batch_execute(&format!(
                "SELECT pg_drop_replication_slot('{slot}')
                   FROM pg_replication_slots WHERE slot_name = '{slot}';
                 DROP PUBLICATION IF EXISTS \"{publication}\";
                 DROP TABLE IF EXISTS {table};"
            ))
            .await;

        // ---- Relation: names, oids and column types come from the catalog.
        let relation = messages
            .iter()
            .find_map(|message| match message {
                PgOutputMessage::Relation {
                    rel_id,
                    namespace,
                    name,
                    columns,
                    ..
                } => Some((*rel_id, namespace.clone(), name.clone(), columns.clone())),
                _ => None,
            })
            .expect("no Relation message arrived");
        assert_eq!(
            i64::from(relation.0),
            rel_oid,
            "Relation.rel_id must be the table's pg_class.oid"
        );
        assert_eq!(relation.1, namespace, "Relation.namespace");
        assert_eq!(relation.2, table, "Relation.name");
        assert_eq!(relation.3.len(), 2, "Relation column count");
        assert_eq!(relation.3[0].name, "id");
        assert_eq!(
            i64::from(relation.3[0].type_oid),
            id_type_oid,
            "column 0 type_oid must match pg_attribute.atttypid"
        );
        assert_eq!(relation.3[1].name, "label");
        assert_eq!(
            i64::from(relation.3[1].type_oid),
            label_type_oid,
            "column 1 type_oid must match pg_attribute.atttypid"
        );
        // `int4` has no type modifier; -1 is the catalog's "none". Reading
        // this field as unsigned would surface it as 4294967295.
        assert_eq!(
            relation.3[0].type_modifier, -1,
            "an absent type modifier is -1, and must be read as signed"
        );

        // ---- Insert: the row that went in.
        let insert = messages
            .iter()
            .find_map(|message| match message {
                PgOutputMessage::Insert { new_tuple, .. } => Some(new_tuple.clone()),
                _ => None,
            })
            .expect("no Insert message arrived");
        assert_eq!(text(&insert.columns[0]), "1");
        assert_eq!(text(&insert.columns[1]), "first");

        // ---- Update: REPLICA IDENTITY FULL means both tuples are present,
        // and the OLD one must carry the pre-update value.
        let update = messages
            .iter()
            .find_map(|message| match message {
                PgOutputMessage::Update {
                    old_tuple,
                    new_tuple,
                    ..
                } => Some((old_tuple.clone(), new_tuple.clone())),
                _ => None,
            })
            .expect("no Update message arrived");
        let old = update
            .0
            .expect("REPLICA IDENTITY FULL must yield an old tuple");
        let old = old
            .full()
            .expect("REPLICA IDENTITY FULL must send a Full old tuple, not a Key");
        assert_eq!(
            text(&old.columns[1]),
            "first",
            "the old tuple must hold the pre-update value"
        );
        assert_eq!(
            text(&update.1.columns[1]),
            "second",
            "the new tuple must hold the post-update value"
        );

        // ---- Delete: the row that went out.
        let delete = messages
            .iter()
            .find_map(|message| match message {
                PgOutputMessage::Delete { old_tuple, .. } => Some(old_tuple.clone()),
                _ => None,
            })
            .expect("no Delete message arrived");
        let delete = delete
            .full()
            .expect("REPLICA IDENTITY FULL must send a Full old tuple on DELETE too");
        assert_eq!(text(&delete.columns[0]), "1");
        assert_eq!(text(&delete.columns[1]), "second");

        // ---- Truncate: names the same relation oid.
        let truncate = messages
            .iter()
            .find_map(|message| match message {
                PgOutputMessage::Truncate { relation_ids, .. } => Some(relation_ids.clone()),
                _ => None,
            })
            .expect("no Truncate message arrived");
        assert_eq!(
            truncate.iter().map(|id| i64::from(*id)).collect::<Vec<_>>(),
            vec![rel_oid],
            "Truncate must name the table's oid"
        );
    })
    .await
    .expect("the live pgoutput decode exceeded its watchdog");
}

/// Two DELETEs whose old tuples hold the SAME columns must still be told
/// apart, because only one of them is reporting values.
///
/// Under the default replica identity a DELETE sends `K`: the key column, and
/// a NULL placeholder for every other column. Under `FULL` it sends `O`: the
/// real row, where a NULL is a value. Delete a row whose non-key column is
/// genuinely NULL under `FULL`, and the two frames differ in ONE byte -
/// `4b` vs `4f` - with identical columns after it.
///
/// So a decoder that drops that byte makes "the label was NULL" and "the
/// label was never sent" the same answer. This test builds exactly that
/// collision on a live server and requires the two to remain distinguishable.
#[compio::test]
async fn a_key_only_old_tuple_is_not_confusable_with_a_row_that_held_nulls() {
    compio::time::timeout(WATCHDOG, async {
        let base = common::test_object_name("cpg ident kinds");
        let table = format!("{base}_t");
        let publication = format!("{base}_p");
        let slot = format!("{base}_s");
        let setup = client().await;

        setup
            .batch_execute(&format!(
                "DROP PUBLICATION IF EXISTS \"{publication}\";
                 DROP TABLE IF EXISTS {table};
                 CREATE TABLE {table}(id int primary key, label text);
                 CREATE PUBLICATION \"{publication}\" FOR TABLE {table};
                 INSERT INTO {table} VALUES (1, 'not null at all'), (2, NULL);"
            ))
            .await
            .expect("setup failed");
        setup
            .batch_execute(&format!(
                "SELECT pg_drop_replication_slot('{slot}')
                   FROM pg_replication_slots WHERE slot_name = '{slot}';
                 SELECT pg_create_logical_replication_slot('{slot}', 'pgoutput');"
            ))
            .await
            .expect("slot setup failed");

        // Row 1 leaves under DEFAULT: its label is NOT null, but the wire
        // carries a placeholder. Row 2 leaves under FULL: its label really is
        // null. Same decoded columns, different meaning.
        setup
            .batch_execute(&format!(
                "DELETE FROM {table} WHERE id = 1;
                 ALTER TABLE {table} REPLICA IDENTITY FULL;
                 DELETE FROM {table} WHERE id = 2;"
            ))
            .await
            .expect("deletes failed");

        let messages = decoded_stream(&slot, &publication).await;

        let _ = setup
            .batch_execute(&format!(
                "SELECT pg_drop_replication_slot('{slot}')
                   FROM pg_replication_slots WHERE slot_name = '{slot}';
                 DROP PUBLICATION IF EXISTS \"{publication}\";
                 DROP TABLE IF EXISTS {table};"
            ))
            .await;

        let deletes = messages
            .iter()
            .filter_map(|message| match message {
                PgOutputMessage::Delete { old_tuple, .. } => Some(old_tuple.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(deletes.len(), 2, "expected both deletes: {messages:?}");

        // The collision itself: identical columns.
        assert_eq!(
            deletes[0].tuple().columns[1],
            TupleColumn::Null,
            "the DEFAULT-identity delete pads the non-key column with NULL"
        );
        assert_eq!(
            deletes[1].tuple().columns[1],
            TupleColumn::Null,
            "the FULL-identity delete carries a genuine NULL"
        );

        // ...and the distinction that must survive it.
        assert!(
            deletes[0].full().is_none(),
            "a key-only tuple must not be offered as the row's prior values: \
             its NULL is a placeholder, and row 1's label was 'not null at all'"
        );
        assert!(
            deletes[1].full().is_some(),
            "a FULL old tuple must be offered as the row's prior values: \
             its NULL is what row 2 actually held"
        );
    })
    .await
    .expect("the replica-identity kind test exceeded its watchdog");
}

/// A NULL is a distinct wire kind (`n`), not an empty text value. Decoding it
/// as `Text("")` would make a nulled column indistinguishable from one that
/// really holds the empty string.
#[compio::test]
async fn a_null_column_decodes_as_null_and_not_as_empty_text() {
    compio::time::timeout(WATCHDOG, async {
        let base = common::test_object_name("cpg pgoutput null");
        let table = format!("{base}_t");
        let publication = format!("{base}_p");
        let slot = format!("{base}_s");
        let setup = client().await;

        setup
            .batch_execute(&format!(
                "DROP PUBLICATION IF EXISTS \"{publication}\";
                 DROP TABLE IF EXISTS {table};
                 CREATE TABLE {table}(id int primary key, nulled text, empty text);
                 CREATE PUBLICATION \"{publication}\" FOR TABLE {table};"
            ))
            .await
            .expect("setup failed");
        setup
            .batch_execute(&format!(
                "SELECT pg_drop_replication_slot('{slot}')
                   FROM pg_replication_slots WHERE slot_name = '{slot}';
                 SELECT pg_create_logical_replication_slot('{slot}', 'pgoutput');"
            ))
            .await
            .expect("slot setup failed");
        setup
            .batch_execute(&format!("INSERT INTO {table} VALUES (1, NULL, '');"))
            .await
            .expect("insert failed");

        let messages = decoded_stream(&slot, &publication).await;

        let _ = setup
            .batch_execute(&format!(
                "SELECT pg_drop_replication_slot('{slot}')
                   FROM pg_replication_slots WHERE slot_name = '{slot}';
                 DROP PUBLICATION IF EXISTS \"{publication}\";
                 DROP TABLE IF EXISTS {table};"
            ))
            .await;

        let insert = messages
            .iter()
            .find_map(|message| match message {
                PgOutputMessage::Insert { new_tuple, .. } => Some(new_tuple.clone()),
                _ => None,
            })
            .expect("no Insert message arrived");

        assert_eq!(
            insert.columns[1],
            TupleColumn::Null,
            "a NULL column must decode as Null"
        );
        assert_eq!(
            insert.columns[2],
            TupleColumn::Text(String::new()),
            "an empty string must decode as empty Text, not as Null"
        );
    })
    .await
    .expect("the null-column decode exceeded its watchdog");
}
