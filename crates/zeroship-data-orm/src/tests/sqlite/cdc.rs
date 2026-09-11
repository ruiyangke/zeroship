//! SQLite cdc contracts.
use super::fixtures::*;

use crate::tests::fixtures::Host;

use std::rc::Rc;

use zeroship_data_orm::backend::sqlite::reservation::TerminalOutcome;

use zeroship_data_orm::backend::sqlite::session::TerminalIntent;

use zeroship_data_orm::backend::BackendHandle;

use zeroship_data_orm::cdc::ChangeOp;

use zeroship_data_orm::error::DbError;

use zeroship_data_orm::cdc::broker::{Subscription, SubscriptionMessage, subscribe};

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
            // Per plan §8: order is `[a, b, c, d]` = INSERT, UPDATE,
            // DELETE, INSERT. Each msg is a Change variant carrying the
            // event.
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
        // Plan §8 / §9: a single COMMIT of N rows must reach every
        // active subscriber in INSERT order. Scaled down to 10×100 per the
        // task spec ("100 subscribers × 1000 rows would saturate dev
        // hardware; scale down to 10 × 100 for CI sanity"). The default
        // queue depth is 1024 (`broker::DEFAULT_QUEUE_DEPTH`), so 100 rows
        // fit comfortably without triggering the overflow-to-Resync path.
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

            // Generous drain — 100 publishes × 10 subscribers under the
            // single-threaded compio runtime + one PRAGMA round-trip on
            // first touch. 200ms is comfortably above the in-process
            // upper bound on dev hardware.
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
fn mv_refresh_does_not_emit_change_events() {
    Host::test(|host| {
        // Plan §6 + §9: writes to `__zeroship_mv_*` shadow tables
        // must be filtered upstream of the broker. The plan acknowledges
        // (§9) that the `db.materializedView(...).refresh()` SDK
        // primitive does not exist yet, so we exercise the filter directly
        // by writing to a shadow table whose name matches the filter
        // prefix — the dispatcher cannot distinguish a "real" MV refresh
        // from a hand-rolled shadow write.
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_mv")
                .await
                .expect("ensure_app_schema");
            // Create a shadow table that mimics what an MV refresh would
            // emit. The CREATE itself only touches sqlite_master (already
            // filtered); the INSERT below is the gate.
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

            // Subscribe to the shadow table directly so we'd observe any
            // event that leaked past the filter. (The SDK boundary refuses
            // such a subscription via `Db::open_subscription`; the broker
            // primitive does NOT, and we exercise the broker level here.)
            let sub = subscribe_local("app_mv", "__zeroship_mv_demo");

            // INSERT into the shadow — this is the write the filter must
            // drop. The preupdate hook fires, `is_filtered_relation`
            // returns `true`, no event is buffered, no packet ships.
            backend
                .execute_fixture(
                    "INSERT INTO \"app_mv\".\"__zeroship_mv_demo\" (id, v) VALUES (1, 'a')",
                    &[],
                )
                .await
                .expect("INSERT into shadow");

            drain_publisher().await;

            let msgs = drain(&sub);
            assert!(
                msgs.is_empty(),
                "writes to __zeroship_mv_* must not reach the broker; got {msgs:?}"
            );
        });
    })
}

#[test]
fn mv_refresh_emits_no_change_events_on_base_or_shadow() {
    Host::test(|host| {
        // Variant of the previous gate: when a transaction touches BOTH a
        // shadow table AND a regular collection, the shadow writes are
        // filtered and the regular writes pass through. The regular
        // subscriber observes exactly the regular events; the shadow
        // subscriber observes zero events.
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_mv_mixed")
                .await
                .expect("ensure_app_schema");
            // Regular collection.
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
            // Shadow table.
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

            // Single transaction touching both tables. The shadow write
            // is filtered at the hook; the regular write reaches the broker.
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
            assert!(
                shadow_msgs.is_empty(),
                "shadow collection must observe zero events; got {shadow_msgs:?}"
            );
        });
    })
}

#[test]
fn audit_table_writes_do_not_emit_events() {
    Host::test(|host| {
        // The `is_filtered_relation` predicate covers `__zeroship_audit_*`
        // alongside `__zeroship_mv_*`. The unit test in
        // `cdc.rs::tests::is_filtered_relation_excludes_system_tables`
        // already pins the predicate; this gate exercises the filter
        // end-to-end so a regression that drops the audit-prefix arm of the
        // predicate would fail here at the integration boundary.
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
            assert!(
                msgs.is_empty(),
                "writes to __zeroship_audit_* must not reach the broker; got {msgs:?}"
            );
        });
    })
}

/// Longer drain — the new fences move ~100 events through the
/// publisher under a paused broker. The 100 ms budget is the same
/// upper bound `subscription_fanout_under_load` uses (200 ms there
/// for 100 events × 10 subscribers; halved here because we only have
/// one subscriber).
async fn drain_publisher_long() {
    compio::time::sleep(std::time::Duration::from_millis(100)).await;
}

#[test]
fn backfill_run_pauses_broker_and_emits_one_resync() {
    Host::test(|host| {
        // Plan §7 + §9 gate - backfill pause rail end-to-end:
        //
        // 1. ensure_app_schema + CREATE TABLE.
        // 2. Subscribe BEFORE the pause window so the subscription is
        //    visible to `resume_app_with_resync` on guard drop.
        // 3. Engage `BrokerPauseGuard` — this calls `suppress_app(app_id)`
        //    on the thread-local rail.
        // 4. INSERT 100 rows. The preupdate hook still fires + buffers,
        //    the commit_hook ships packets, BUT the publisher's per-event
        //    suppression check drops each packet (debug-logged).
        // 5. Drop the guard. `unsuppress_app` clears the flag +
        //    `resume_app_with_resync` pushes ONE `Resync` per active
        //    subscription.
        // 6. Drain the subscriber → exactly ONE `Resync`, ZERO `Change`
        //    messages.
        //
        // The asymmetry between "INSERT 100 rows" and "one Resync" is the
        // load-bearing contract: backfill silently drops events; the
        // single Resync tells the subscriber to refetch + catch up via
        // the read path, NOT via the event stream.
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

            // Engage backfill pause. `BrokerPauseGuard::new` calls
            // `broker::suppress_app(app_id)`; the publisher's
            // per-event check drops every packet for this app until the
            // guard drops.
            let guard =
                zeroship_data_orm::cdc::broker::BrokerPauseGuard::new("app_backfill".to_string());

            // INSERT 100 rows under the suppression window. Each statement
            // routes through the session actor, the preupdate hook fires,
            // the commit_hook ships a one-event CommitPacket — the
            // publisher receives the packet, sees `is_app_suppressed`,
            // drops the event + emits a debug-level trace, moves on.
            for i in 0..100 {
                let sql = format!(
                    "INSERT INTO \"app_backfill\".\"items\" (id, name) VALUES ({i}, 'r{i}')"
                );
                backend
                    .execute_fixture(&sql, &[])
                    .await
                    .expect("INSERT under backfill pause");
            }

            // Drop the guard WITHOUT draining first. This is the exposing order:
            // the 100 packets are still queued, and the guard that covered their
            // commits is already gone by the time the publisher dequeues them.
            //
            // This test drained first until 2026-09-03, which made the window a
            // function of publisher scheduling rather than of the guard's scope.
            // Suppression is stamped in the commit hook now, so the queued packets
            // stay suppressed and the order below is the one worth pinning.
            drop(guard);

            // Sentinel: one INSERT *after* the window. The channel is FIFO, so
            // observing its Change proves the publisher ran past all 100 queued
            // packets — without it, "no Change events" would also be satisfied by
            // a publisher that never woke at all.
            backend
                .execute_fixture(
                    "INSERT INTO \"app_backfill\".\"items\" (id, name) VALUES (1000, 'after')",
                    &[],
                )
                .await
                .expect("INSERT after the backfill window");

            drain_publisher_long().await;

            let msgs = drain(&sub);

            // Expected shape: [Resync, Change(1000, 'after')].
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
            // Defensive: NO in-window Change leaked past the suppression stamp.
            // (Implied by len==2 plus the sentinel match, restated so a future
            // change that interleaves window events surfaces the intent.)
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
        // Plan §7 + §16.7 + §9 gate - schema-pending decoder rail
        // end-to-end:
        //
        // 1. ensure_app_schema + CREATE TABLE.
        // 2. Subscribe via the broker BEFORE engaging schema-pending.
        // 3. Engage `SchemaPendingGuard`. This sets the thread-local
        //    `schema_pending_apps` flag AND ensures the publisher's
        //    per-event check drops every packet for the app.
        // 4. INSERT 50 rows — every packet is dropped at the publisher
        //    (debug-logged).
        // 5. While engaged, `broker::try_subscribe(app_id, "other")` MUST
        //    return `DbError::Coded { code: "schema_pending" }`. This is
        //    the LOUD rail (vs the silent backfill rail above).
        // 6. Drop the guard — clears the schema-pending flag + emits one
        //    `Resync` per active subscription.
        // 7. Subsequent INSERT publishes normally (the flag is cleared).
        // 8. Drain: the subscriber observes (a) one Resync from the
        //    guard's drop, then (b) one Change from the post-disengage
        //    INSERT. No events from the pre-disengage window.
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

            // Engage schema-pending. `SchemaPendingGuard::new` calls
            // `broker::engage_schema_pending(app_id)`; both the publisher
            // suppression check AND the `Broker::try_subscribe` rejection
            // branch activate.
            let guard =
                zeroship_data_orm::cdc::broker::SchemaPendingGuard::new("app_pending".to_string());

            // INSERT 50 rows under the schema-pending window. Same shape
            // as the backfill test above — packets ship, publisher drops.
            for i in 0..50 {
                let sql = format!(
                    "INSERT INTO \"app_pending\".\"items\" (id, name) VALUES ({i}, 'r{i}')"
                );
                backend
                    .execute_fixture(&sql, &[])
                    .await
                    .expect("INSERT under schema-pending");
            }

            // The loud-rail invariant: while engaged, a NEW subscribe call
            // (via `try_subscribe`) MUST return the typed conflict
            // envelope. We don't use the legacy `subscribe()` here because
            // it is infallible by design (back-compat with ~40 in-crate
            // callers); the SDK boundary that lands later wires
            // `try_subscribe` so the JS layer can branch on
            // `e.code === "schema_pending"`.
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

            // Drop the guard WITHOUT draining first — the exposing order. The 50
            // packets are still queued and the guard that covered their commits is
            // gone before the publisher dequeues them. The post-disengage INSERT
            // below is the FIFO sentinel that proves the publisher ran past them.
            //
            // This test drained first until 2026-09-03, which hid the window
            // behind publisher scheduling.
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
