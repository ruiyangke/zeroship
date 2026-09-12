//! SQLite cdc contracts.
use super::fixtures::*;

use crate::tests::fixtures::Host;

use std::rc::Rc;

use zeroship_data_orm::backend::sqlite::reservation::TerminalOutcome;

use zeroship_data_orm::backend::sqlite::session::TerminalIntent;

use zeroship_data_orm::backend::BackendHandle;

use zeroship_data_orm::cdc::ChangeOp;

use zeroship_data_orm::error::DbError;

use zeroship_data_orm::cdc::broker::{subscribe, Subscription, SubscriptionMessage};

#[cfg(test)]
use crate::tests::fixtures::DatabaseFixture;

/// Wait long enough for the publisher task to drain the CDC channel
/// and call `broker::publish`. The publisher loop is:
///
///   recv_async → fetch PRAGMA table_info (1 round-trip on first
///   touch) → broker::publish per event.
///
/// All steps run on the same compio thread as the test future, so a
/// single yield is the lower bound; we sleep generously for CI noise
/// tolerance.
async fn drain_publisher() {
    compio::time::sleep(std::time::Duration::from_millis(50)).await;
}

/// Drain a subscription's queue into a `Vec<SubscriptionMessage>` —
/// the tests pattern-match on the resulting shape.
fn drain(sub: &Subscription) -> Vec<SubscriptionMessage> {
    let mut out = Vec::new();
    while let Some(msg) = sub.pop() {
        out.push(msg);
    }
    out
}

/// Subscribe to this fixture's `(app_id, collection)` on the process broker.
fn subscribe_local(app_id: &str, collection: &str) -> Subscription {
    subscribe(app_id, collection)
}

#[test]
fn insert_publishes_via_preupdate_hook() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("cdc_insert")
                .await
                .expect("ensure_app_schema");

            // Create a user table the CDC hook will fire against.
            backend
                .execute_fixture(
                    "CREATE TABLE \"cdc_insert\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE items");

            // Subscribe BEFORE the mutation. The DDL above is not a
            // user-table write; it goes through `sqlite_master` which the
            // dispatcher filters, so no event is queued.
            let sub = subscribe_local("cdc_insert", "items");

            // INSERT a row — the preupdate hook fires, commit hook ships
            // the packet, publisher resolves column names + publishes.
            backend
                .execute_fixture(
                    "INSERT INTO \"cdc_insert\".\"items\" (name) VALUES ('alice')",
                    &[],
                )
                .await
                .expect("INSERT items");

            drain_publisher().await;

            let msgs = drain(&sub);
            assert_eq!(
                msgs.len(),
                1,
                "expected 1 event after a single-row INSERT; got {msgs:?}"
            );
            match &msgs[0] {
                SubscriptionMessage::Change(ev) => {
                    assert_eq!(ev.op, ChangeOp::Insert);
                    assert_eq!(ev.app_id, "cdc_insert");
                    assert_eq!(ev.collection, "items");
                    assert!(
                        !ev.new_tuple.is_empty(),
                        "INSERT event must carry a populated new_tuple; got {:?}",
                        ev.new_tuple
                    );
                    // Column names were resolved via PRAGMA table_info on
                    // the publisher — `name` should be present.
                    assert_eq!(
                        ev.new_tuple.get("name"),
                        Some(&"alice".to_string()),
                        "new_tuple should carry the inserted `name`; got {:?}",
                        ev.new_tuple
                    );
                    assert!(
                        ev.old_tuple.is_none(),
                        "INSERT must not carry an old_tuple; got {:?}",
                        ev.old_tuple
                    );
                }
                other => panic!("expected Change event, got {other:?}"),
            }
        });
    })
}

#[test]
fn attached_creator_databases_with_the_same_table_name_are_isolated() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            for app in ["cdc_app_a", "cdc_app_b"] {
                backend
                    .attach_app_file(app)
                    .await
                    .expect("attach creator database");
                backend
                    .execute_fixture(
                        &format!(
                            "CREATE TABLE \"{app}\".\"items\" (id INTEGER PRIMARY KEY, name TEXT)"
                        ),
                        &[],
                    )
                    .await
                    .expect("create creator table");
            }

            let app_a = subscribe_local("cdc_app_a", "items");
            let app_b = subscribe_local("cdc_app_b", "items");
            backend
                .execute_fixture(
                    "INSERT INTO \"cdc_app_a\".\"items\" (name) VALUES ('only-a')",
                    &[],
                )
                .await
                .expect("insert into first creator database");
            drain_publisher().await;

            let messages = drain(&app_a);
            assert_eq!(
                messages.len(),
                1,
                "first creator event missing: {messages:?}"
            );
            assert!(drain(&app_b).is_empty(), "event crossed creator databases");
        });
    })
}

#[test]
fn publisher_observes_columns_added_after_first_event() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("cdc_schema_refresh")
                .await
                .expect("attach app file");
            backend
                .execute_fixture(
                    "CREATE TABLE \"cdc_schema_refresh\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                    &[],
                )
                .await
                .expect("create table");

            let sub = subscribe_local("cdc_schema_refresh", "items");
            backend
                .execute_fixture(
                    "INSERT INTO \"cdc_schema_refresh\".\"items\" (id, name) VALUES (1, 'before')",
                    &[],
                )
                .await
                .expect("insert before schema change");
            drain_publisher().await;
            assert_eq!(drain(&sub).len(), 1);

            backend
                .execute_fixture(
                    "ALTER TABLE \"cdc_schema_refresh\".\"items\" ADD COLUMN note TEXT",
                    &[],
                )
                .await
                .expect("add column");
            backend
                .execute_fixture(
                    "INSERT INTO \"cdc_schema_refresh\".\"items\" (id, name, note) \
                     VALUES (2, 'after', 'visible')",
                    &[],
                )
                .await
                .expect("insert after schema change");
            drain_publisher().await;

            let messages = drain(&sub);
            assert_eq!(messages.len(), 1);
            let SubscriptionMessage::Change(event) = &messages[0] else {
                panic!("expected change event");
            };
            assert_eq!(
                event.new_tuple.get("note").map(String::as_str),
                Some("visible")
            );
        });
    })
}

#[test]
fn insert_publishes_logical_typed_id_not_sqlite_rowid() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("cdc_typed_id")
                .await
                .expect("ensure_app_schema");

            backend
                .execute_fixture(
                    "CREATE TABLE \"cdc_typed_id\".\"typed_items\" (\
                     id TEXT PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE typed_items");

            let sub = subscribe_local("cdc_typed_id", "typed_items");
            let typed_id = "usr_02HXSQLITECDCLOGICALPK";

            backend
                .execute_fixture(
                    &format!(
                        "INSERT INTO \"cdc_typed_id\".\"typed_items\" (id, name) \
                     VALUES ('{typed_id}', 'alice')"
                    ),
                    &[],
                )
                .await
                .expect("INSERT typed_items");

            drain_publisher().await;

            let msgs = drain(&sub);
            assert_eq!(msgs.len(), 1, "expected 1 typed-id event; got {msgs:?}");
            match &msgs[0] {
                SubscriptionMessage::Change(ev) => {
                    assert_eq!(ev.op, ChangeOp::Insert);
                    assert_eq!(ev.pk.as_deref(), Some(typed_id));
                    assert_eq!(ev.new_tuple.get("id").map(String::as_str), Some(typed_id));
                }
                other => panic!("expected Change event, got {other:?}"),
            }
        });
    })
}

#[test]
fn update_publishes_change_event_with_pre_image() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("cdc_update")
                .await
                .expect("ensure_app_schema");
            backend
                .execute_fixture(
                    "CREATE TABLE \"cdc_update\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE items");

            // Seed one row. We subscribe AFTER the seed so the INSERT
            // event is not part of what `drain` sees.
            backend
                .execute_fixture(
                    "INSERT INTO \"cdc_update\".\"items\" (id, name) VALUES (1, 'alice')",
                    &[],
                )
                .await
                .expect("INSERT seed row");

            // Give the publisher a chance to drain the seed event so it
            // doesn't show up in the subscription created below (the
            // subscribe happens on the same thread, but only AFTER the
            // publisher has fanned out the prior packet).
            drain_publisher().await;

            let sub = subscribe_local("cdc_update", "items");

            // UPDATE the row — the preupdate hook should capture both
            // OLD ('alice') and NEW ('bob') tuples.
            backend
                .execute_fixture(
                    "UPDATE \"cdc_update\".\"items\" SET name = 'bob' WHERE id = 1",
                    &[],
                )
                .await
                .expect("UPDATE items");

            drain_publisher().await;

            let msgs = drain(&sub);
            assert_eq!(
                msgs.len(),
                1,
                "expected 1 event after a single-row UPDATE; got {msgs:?}"
            );
            match &msgs[0] {
                SubscriptionMessage::Change(ev) => {
                    assert_eq!(ev.op, ChangeOp::Update);
                    assert!(
                        !ev.new_tuple.is_empty(),
                        "UPDATE event must carry a populated new_tuple; got {:?}",
                        ev.new_tuple
                    );
                    assert_eq!(
                        ev.new_tuple.get("name"),
                        Some(&"bob".to_string()),
                        "new_tuple should carry the post-image name; got {:?}",
                        ev.new_tuple
                    );
                    let old = ev
                        .old_tuple
                        .as_ref()
                        .expect("UPDATE must carry an old_tuple (pre-image)");
                    assert_eq!(
                        old.get("name"),
                        Some(&"alice".to_string()),
                        "old_tuple should carry the pre-image name; got {old:?}"
                    );
                }
                other => panic!("expected Change event, got {other:?}"),
            }
        });
    })
}

#[test]
fn rollback_does_not_publish() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("cdc_rollback")
                .await
                .expect("ensure_app_schema");
            backend
                .execute_fixture(
                    "CREATE TABLE \"cdc_rollback\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE items");

            let sub = subscribe_local("cdc_rollback", "items");

            // BEGIN / INSERT / ROLLBACK — each statement routes through
            // the session actor (same worker thread; serialised by the
            // mpsc queue). The rollback_hook clears the buffer; no packet
            // ships.
            backend.execute_fixture("BEGIN", &[]).await.expect("BEGIN");
            backend
                .execute_fixture(
                    "INSERT INTO \"cdc_rollback\".\"items\" (name) VALUES ('alice')",
                    &[],
                )
                .await
                .expect("INSERT inside tx");
            backend
                .execute_fixture("ROLLBACK", &[])
                .await
                .expect("ROLLBACK");

            drain_publisher().await;

            let msgs = drain(&sub);
            assert!(
                msgs.is_empty(),
                "ROLLBACK must not publish any events; got {msgs:?}"
            );
        });
    })
}

#[test]
fn mixed_ops_in_one_tx_ordered_by_buffer_index() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("cdc_mixed")
                .await
                .expect("ensure_app_schema");
            backend
                .execute_fixture(
                    "CREATE TABLE \"cdc_mixed\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE items");
            // Seed rows for the UPDATE + DELETE arms of the mixed-op tx
            // below. Done BEFORE subscription so the seed events don't
            // pollute the assertions.
            backend
            .execute_fixture(
                "INSERT INTO \"cdc_mixed\".\"items\" (id, name) VALUES (10, 'b_pre'), (20, 'c_pre')",
                &[],
            )
            .await
            .expect("INSERT seed rows");
            drain_publisher().await;

            let sub = subscribe_local("cdc_mixed", "items");

            // BEGIN; INSERT a; UPDATE b; DELETE c; INSERT d; COMMIT.
            // Each statement fires the preupdate hook once; the commit
            // hook ships a single CommitPacket with all 4 events in
            // buffer order.
            backend.execute_fixture("BEGIN", &[]).await.expect("BEGIN");
            backend
                .execute_fixture(
                    "INSERT INTO \"cdc_mixed\".\"items\" (id, name) VALUES (1, 'a')",
                    &[],
                )
                .await
                .expect("INSERT a");
            backend
                .execute_fixture(
                    "UPDATE \"cdc_mixed\".\"items\" SET name = 'b_post' WHERE id = 10",
                    &[],
                )
                .await
                .expect("UPDATE b");
            backend
                .execute_fixture("DELETE FROM \"cdc_mixed\".\"items\" WHERE id = 20", &[])
                .await
                .expect("DELETE c");
            backend
                .execute_fixture(
                    "INSERT INTO \"cdc_mixed\".\"items\" (id, name) VALUES (2, 'd')",
                    &[],
                )
                .await
                .expect("INSERT d");
            backend
                .execute_fixture("COMMIT", &[])
                .await
                .expect("COMMIT");

            drain_publisher().await;

            let msgs = drain(&sub);
            assert_eq!(
                msgs.len(),
                4,
                "expected 4 events after a 4-statement tx; got {msgs:?}"
            );
            // Events retain commit-buffer order.
            let ops: Vec<ChangeOp> = msgs
                .iter()
                .map(|m| match m {
                    SubscriptionMessage::Change(ev) => ev.op,
                    other => panic!("expected Change, got {other:?}"),
                })
                .collect();
            assert_eq!(
                ops,
                vec![
                    ChangeOp::Insert,
                    ChangeOp::Update,
                    ChangeOp::Delete,
                    ChangeOp::Insert,
                ],
                "events must appear in buffer order [a, b, c, d]: {ops:?}"
            );
        });
    })
}

#[test]
fn subscription_fanout_under_load() {
    Host::test(|host| {
        // A commit reaches every active subscriber in insert order without
        // crossing the overflow-to-resync path exercised elsewhere.
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_fanout")
                .await
                .expect("ensure_app_schema");
            backend
                .execute_fixture(
                    "CREATE TABLE \"app_fanout\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE items");

            // Subscribe 10 times to the same (app, collection). Each
            // returned `Subscription` is a fresh routing-table entry — the
            // broker fans the same Rc<ChangeEvent> out to each.
            let subs: Vec<Subscription> = (0..10)
                .map(|_| subscribe_local("app_fanout", "items"))
                .collect();

            // BEGIN; 100×INSERT; COMMIT. Each statement routes through the
            // session actor in order, so the buffer accumulates events in
            // INSERT order. The commit_hook then ships one CommitPacket
            // with all 100 events; the publisher iterates and fans out.
            backend.execute_fixture("BEGIN", &[]).await.expect("BEGIN");
            for i in 0..100 {
                let sql =
                    format!("INSERT INTO \"app_fanout\".\"items\" (id, name) VALUES ({i}, 'r{i}')");
                backend
                    .execute_fixture(&sql, &[])
                    .await
                    .expect("INSERT inside tx");
            }
            backend
                .execute_fixture("COMMIT", &[])
                .await
                .expect("COMMIT");

            // Let the publisher deliver the committed batch.
            compio::time::sleep(std::time::Duration::from_millis(200)).await;

            for (i, sub) in subs.iter().enumerate() {
                let msgs = drain(sub);
                assert_eq!(
                    msgs.len(),
                    100,
                    "subscriber #{i} should observe 100 events; got {} ({msgs:?})",
                    msgs.len()
                );
                // Buffer-index ordering invariant: events appear
                // in the order they fired against the preupdate hook,
                // which matches statement order under SQLite's
                // single-writer execution.
                for (idx, msg) in msgs.iter().enumerate() {
                    match msg {
                        SubscriptionMessage::Change(ev) => {
                            assert_eq!(
                                ev.op,
                                ChangeOp::Insert,
                                "subscriber #{i} event {idx} must be Insert; got {:?}",
                                ev.op
                            );
                            // The `id` column carries the per-row index. We
                            // assert ordering through that field.
                            let id_str = ev.new_tuple.get("id").unwrap_or_else(|| {
                                panic!(
                                    "subscriber #{i} event {idx} missing `id`: {:?}",
                                    ev.new_tuple
                                )
                            });
                            let id: i64 = id_str
                                .parse()
                                .unwrap_or_else(|_| panic!("non-numeric id: {id_str}"));
                            assert_eq!(
                                id, idx as i64,
                                "subscriber #{i} event {idx} must carry id={idx}; got id={id}"
                            );
                        }
                        other => {
                            panic!("subscriber #{i} event {idx} must be Change; got {other:?}")
                        }
                    }
                }
            }
        });
    })
}

#[test]
fn prefixed_shadow_table_emits_change_events() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_mv")
                .await
                .expect("ensure_app_schema");
            backend
                .execute_fixture(
                    "CREATE TABLE \"app_mv\".\"__zeroship_mv_demo\" (\
                     id INTEGER PRIMARY KEY, \
                     v TEXT NOT NULL\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE __zeroship_mv_demo");

            let sub = subscribe_local("app_mv", "__zeroship_mv_demo");

            backend
                .execute_fixture(
                    "INSERT INTO \"app_mv\".\"__zeroship_mv_demo\" (id, v) VALUES (1, 'a')",
                    &[],
                )
                .await
                .expect("INSERT into shadow");

            drain_publisher().await;

            let msgs = drain(&sub);
            assert_eq!(msgs.len(), 1, "expected the prefixed-table event: {msgs:?}");
            let SubscriptionMessage::Change(event) = &msgs[0] else {
                panic!("expected a change event: {msgs:?}")
            };
            assert_eq!(event.collection, "__zeroship_mv_demo");
            assert_eq!(event.op, ChangeOp::Insert);
        });
    })
}

#[test]
fn mixed_transaction_emits_events_for_ordinary_and_prefixed_tables() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_mv_mixed")
                .await
                .expect("ensure_app_schema");
            backend
                .execute_fixture(
                    "CREATE TABLE \"app_mv_mixed\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE items");
            backend
                .execute_fixture(
                    "CREATE TABLE \"app_mv_mixed\".\"__zeroship_mv_items\" (\
                     id INTEGER PRIMARY KEY, \
                     v TEXT NOT NULL\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE __zeroship_mv_items");

            let regular_sub = subscribe_local("app_mv_mixed", "items");
            let shadow_sub = subscribe_local("app_mv_mixed", "__zeroship_mv_items");

            backend.execute_fixture("BEGIN", &[]).await.expect("BEGIN");
            backend
                .execute_fixture(
                    "INSERT INTO \"app_mv_mixed\".\"items\" (id, name) VALUES (1, 'alice')",
                    &[],
                )
                .await
                .expect("INSERT items");
            backend
                .execute_fixture(
                    "INSERT INTO \"app_mv_mixed\".\"__zeroship_mv_items\" (id, v) VALUES (1, 'a')",
                    &[],
                )
                .await
                .expect("INSERT shadow");
            backend
                .execute_fixture("COMMIT", &[])
                .await
                .expect("COMMIT");

            drain_publisher().await;

            let regular_msgs = drain(&regular_sub);
            let shadow_msgs = drain(&shadow_sub);

            assert_eq!(
                regular_msgs.len(),
                1,
                "regular collection should observe exactly 1 INSERT; got {regular_msgs:?}"
            );
            match &regular_msgs[0] {
                SubscriptionMessage::Change(ev) => {
                    assert_eq!(ev.op, ChangeOp::Insert);
                    assert_eq!(ev.collection, "items");
                    assert_eq!(
                        ev.new_tuple.get("name"),
                        Some(&"alice".to_string()),
                        "regular collection event must carry the inserted name; got {:?}",
                        ev.new_tuple
                    );
                }
                other => panic!("expected Change event on regular collection, got {other:?}"),
            }
            assert_eq!(
                shadow_msgs.len(),
                1,
                "prefixed collection should observe its INSERT: {shadow_msgs:?}"
            );
            let SubscriptionMessage::Change(event) = &shadow_msgs[0] else {
                panic!("expected a change event: {shadow_msgs:?}")
            };
            assert_eq!(event.collection, "__zeroship_mv_items");
            assert_eq!(event.op, ChangeOp::Insert);
        });
    })
}

#[test]
fn audit_table_writes_emit_events() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_audit")
                .await
                .expect("ensure_app_schema");
            backend
                .execute_fixture(
                    "CREATE TABLE \"app_audit\".\"__zeroship_audit_users\" (\
                     id INTEGER PRIMARY KEY, \
                     event TEXT NOT NULL\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE __zeroship_audit_users");

            let sub = subscribe_local("app_audit", "__zeroship_audit_users");

            backend
                .execute_fixture(
                    "INSERT INTO \"app_audit\".\"__zeroship_audit_users\" \
                 (id, event) VALUES (1, 'delete')",
                    &[],
                )
                .await
                .expect("INSERT audit row");

            drain_publisher().await;

            let msgs = drain(&sub);
            assert_eq!(msgs.len(), 1, "expected the audit-table event: {msgs:?}");
            let SubscriptionMessage::Change(event) = &msgs[0] else {
                panic!("expected a change event: {msgs:?}")
            };
            assert_eq!(event.collection, "__zeroship_audit_users");
            assert_eq!(event.op, ChangeOp::Insert);
        });
    })
}

/// Let queued publisher work settle before assertions.
async fn drain_publisher_long() {
    compio::time::sleep(std::time::Duration::from_millis(100)).await;
}

#[test]
fn backfill_run_pauses_broker_and_emits_one_resync() {
    Host::test(|host| {
        // Subscribers see a resync after a paused backfill, while changes
        // committed inside the pause window remain suppressed.
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_backfill")
                .await
                .expect("ensure_app_schema");
            backend
                .execute_fixture(
                    "CREATE TABLE \"app_backfill\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE items");

            let sub = subscribe_local("app_backfill", "items");

            // Engage the backfill pause before issuing writes.
            let guard =
                zeroship_data_orm::cdc::broker::BrokerPauseGuard::new("app_backfill".to_string());

            // Queue writes inside the suppression window.
            for i in 0..100 {
                let sql = format!(
                    "INSERT INTO \"app_backfill\".\"items\" (id, name) VALUES ({i}, 'r{i}')"
                );
                backend
                    .execute_fixture(&sql, &[])
                    .await
                    .expect("INSERT under backfill pause");
            }

            // Drop before draining to prove suppression follows commit scope
            // rather than publisher scheduling.
            drop(guard);

            // A later sentinel proves the publisher drained the earlier queue.
            backend
                .execute_fixture(
                    "INSERT INTO \"app_backfill\".\"items\" (id, name) VALUES (1000, 'after')",
                    &[],
                )
                .await
                .expect("INSERT after the backfill window");

            drain_publisher_long().await;

            let msgs = drain(&sub);

            assert_eq!(
                msgs.len(),
                2,
                "expected exactly [Resync, Change(sentinel)] after backfill pause + drop; \
             got {} messages: {msgs:?}",
                msgs.len()
            );
            assert!(
                matches!(msgs[0], SubscriptionMessage::Resync),
                "the first message must be Resync; got {:?}",
                msgs[0]
            );
            match &msgs[1] {
                SubscriptionMessage::Change(ev) => {
                    assert_eq!(
                        ev.new_tuple.get("name").map(String::as_str),
                        Some("after"),
                        "the only Change must be the post-window sentinel; got {ev:?}"
                    );
                }
                other => panic!("expected the sentinel Change; got {other:?}"),
            }
            let in_window_changes = msgs
                .iter()
                .filter_map(|m| match m {
                    SubscriptionMessage::Change(ev) => Some(ev),
                    _ => None,
                })
                .filter(|ev| ev.new_tuple.get("name").map(String::as_str) != Some("after"))
                .count();
            assert_eq!(
                in_window_changes, 0,
                "no Change events must reach the subscriber for commits made during a \
             backfill window; got {in_window_changes}"
            );
        });
    })
}

#[test]
fn schema_pending_decoder_drops_then_resyncs() {
    Host::test(|host| {
        // Schema-pending blocks new subscriptions, suppresses queued changes,
        // and resyncs existing subscribers when the schema becomes available.
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_pending")
                .await
                .expect("ensure_app_schema");
            backend
                .execute_fixture(
                    "CREATE TABLE \"app_pending\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE items");

            let sub = subscribe_local("app_pending", "items");

            // Engage schema-pending before issuing writes.
            let guard =
                zeroship_data_orm::cdc::broker::SchemaPendingGuard::new("app_pending".to_string());

            // Queue writes inside the schema-pending window.
            for i in 0..50 {
                let sql = format!(
                    "INSERT INTO \"app_pending\".\"items\" (id, name) VALUES ({i}, 'r{i}')"
                );
                backend
                    .execute_fixture(&sql, &[])
                    .await
                    .expect("INSERT under schema-pending");
            }

            // New subscriptions receive the typed schema-pending refusal.
            let attempt =
                zeroship_data_orm::cdc::broker::try_subscribe("app_pending", "other_collection");
            match &attempt {
                Err(DbError::Coded { code, .. }) => {
                    assert_eq!(
                        code, "schema_pending",
                        "try_subscribe during schema-pending must reject \
                     with code=schema_pending; got code={code}"
                    );
                }
                other => panic!("expected Err(Coded {{ code: schema_pending }}); got {other:?}"),
            }

            // Drop before draining to prove suppression follows commit scope.
            drop(guard);

            // Post-disengage: a fresh INSERT must publish normally.
            backend
                .execute_fixture(
                    "INSERT INTO \"app_pending\".\"items\" (id, name) VALUES (999, 'after')",
                    &[],
                )
                .await
                .expect("INSERT after disengage");

            // Give the publisher time to drain the 50 in-window packets AND the
            // post-disengage one. The long budget (not `drain_publisher`) because
            // the guard now drops before any of them are dequeued.
            drain_publisher_long().await;

            let msgs = drain(&sub);

            // Expected shape: [Resync, Change(999, 'after')].
            // - The 50 INSERTs under the window produced zero events
            //   (publisher dropped them).
            // - The guard's drop pushed exactly one Resync.
            // - The post-disengage INSERT published one Change event.
            assert_eq!(
                msgs.len(),
                2,
                "expected exactly [Resync, Change]; got {} messages: {msgs:?}",
                msgs.len()
            );
            assert!(
                matches!(msgs[0], SubscriptionMessage::Resync),
                "first message must be the disengage-emitted Resync; got {:?}",
                msgs[0]
            );
            match &msgs[1] {
                SubscriptionMessage::Change(ev) => {
                    assert_eq!(ev.op, ChangeOp::Insert);
                    assert_eq!(ev.collection, "items");
                    assert_eq!(
                        ev.new_tuple.get("name"),
                        Some(&"after".to_string()),
                        "second message must be the post-disengage INSERT; \
                     new_tuple={:?}",
                        ev.new_tuple
                    );
                }
                other => panic!("second message must be Change(post-disengage); got {other:?}"),
            }

            // Defensive: zero Change events came from the pre-disengage
            // window. (Implied by len==2 + the explicit Change shape
            // above, but stating the contract here makes a future
            // regression that pre-pends events to the Resync surface
            // explicitly.)
            let pre_disengage_changes = msgs
                .iter()
                .filter_map(|m| match m {
                    SubscriptionMessage::Change(ev) => Some(ev),
                    _ => None,
                })
                .filter(|ev| ev.new_tuple.get("name").map(String::as_str) != Some("after"))
                .count();
            assert_eq!(
                pre_disengage_changes, 0,
                "no pre-disengage Change events must reach the subscriber; got {pre_disengage_changes}"
            );
        });
    })
}

#[test]
fn backfill_pauses_broker_for_a_type_erased_backend_and_resyncs() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_orch")
                .await
                .expect("ensure_app_schema");
            backend
                .execute_fixture(
                    "CREATE TABLE \"app_orch\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE items");

            // Use the same type-erased handle held by the isolate context.
            let handle = BackendHandle::new(Rc::new(backend));

            let sub = subscribe_local("app_orch", "items");

            // The ORM owns pause state independently of driver dispatch.
            let guard =
                zeroship_data_orm::cdc::broker::BrokerPauseGuard::new("app_orch".to_string());

            // Borrow the concrete backend from its type-erased owner.
            let backend_ref = handle
                .get::<zeroship_data_orm::backend::SqliteBackend>()
                .expect("the handle contains SQLite");

            // INSERT 100 rows under the suppression window. The orchestrator-
            // owned guard's contract: the publisher drops every packet for
            // `app_orch` until the guard's Drop runs.
            for i in 0..100 {
                let sql =
                    format!("INSERT INTO \"app_orch\".\"items\" (id, name) VALUES ({i}, 'r{i}')");
                backend_ref
                    .execute_fixture(&sql, &[])
                    .await
                    .expect("INSERT under orchestrator-driven backfill pause");
            }

            // Drop the guard WITHOUT draining first — the exposing order, same as
            // the two fences above. `unsuppress_app` runs and one Resync lands on
            // every active subscription on `app_orch` while all 100 packets are
            // still queued. This is the broker-pause-window lifecycle: the guard
            // binding drops at the end of the DDL/bulk-write window.
            drop(guard);

            // FIFO sentinel: observing this Change proves the publisher ran past
            // the 100 queued packets rather than never waking.
            backend_ref
                .execute_fixture(
                    "INSERT INTO \"app_orch\".\"items\" (id, name) VALUES (1000, 'after')",
                    &[],
                )
                .await
                .expect("INSERT after the orchestrator-driven backfill window");

            drain_publisher_long().await;

            let msgs = drain(&sub);

            assert_eq!(
                msgs.len(),
                2,
                "expected exactly [Resync, Change(sentinel)] after orchestrator-driven \
             pause + drop; got {} messages: {msgs:?}",
                msgs.len()
            );
            assert!(
                matches!(msgs[0], SubscriptionMessage::Resync),
                "the first message must be Resync; got {:?}",
                msgs[0]
            );
            let in_window_changes = msgs
                .iter()
                .filter_map(|m| match m {
                    SubscriptionMessage::Change(ev) => Some(ev),
                    _ => None,
                })
                .filter(|ev| ev.new_tuple.get("name").map(String::as_str) != Some("after"))
                .count();
            assert_eq!(
                in_window_changes, 0,
                "no Change events must reach the subscriber for commits made during an \
             orchestrator-driven backfill window; got {in_window_changes}"
            );
        });
    })
}

/// Both connections publish CDC. Installing the hooks on one would silently
/// drop half the change stream now that `op_conn` is a write path too.
#[test]
fn writes_on_both_connections_reach_the_broker() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("cdc_connections")
                .await
                .expect("ensure_app_schema");
            backend
                .execute_fixture(
                    "CREATE TABLE \"cdc_connections\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE items");

            let sub = subscribe_local("cdc_connections", "items");

            // op_conn: an ordinary autocommit write.
            backend
                .execute_fixture(
                    "INSERT INTO \"cdc_connections\".\"items\" (name) VALUES ('from_op_conn')",
                    &[],
                )
                .await
                .expect("autocommit INSERT");

            // tx_conn: a write inside an explicit creator transaction, committed.
            let tx = backend
                .fixture_session("cdc_connections")
                .await
                .expect("acquire tx client");
            backend
                .execute_fixture_on(&tx, "BEGIN", &[])
                .await
                .expect("BEGIN");
            backend
                .execute_fixture_on(
                    &tx,
                    "INSERT INTO \"cdc_connections\".\"items\" (name) VALUES ('from_tx_conn')",
                    &[],
                )
                .await
                .expect("transactional INSERT");
            assert_eq!(
                backend
                    .settle_transaction_for_tests(&tx, TerminalIntent::Commit)
                    .await
                    .expect("commit"),
                TerminalOutcome::Committed
            );

            drain_publisher().await;

            let msgs = drain(&sub);
            let mut names: Vec<String> = msgs
                .iter()
                .filter_map(|m| match m {
                    SubscriptionMessage::Change(ev) => ev.new_tuple.get("name").cloned(),
                    _ => None,
                })
                .collect();
            names.sort();
            assert_eq!(
                names,
                vec!["from_op_conn".to_string(), "from_tx_conn".to_string()],
                "a write on EACH connection must reach the broker; a dispatcher \
             installed on only one drops the other silently. got {msgs:?}"
            );
        });
    })
}
