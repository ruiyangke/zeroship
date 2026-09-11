//! PostgreSQL cdc contracts.
use super::fixtures::*;

use crate::tests::fixtures::Host;

use compio_postgres::Pool;

#[test]
fn c1_broker_event_delivered_for_insert_via_emit() {
    Host::test(|_| {
        // End-to-end of the local-emit path: the broker, attached
        // on the same thread the test runs on, receives an insert event
        // when `emit_local` is called. No Postgres needed — the broker
        // is in-process.

        // Clean slate.
        zeroship_data_orm::cdc::broker::drain_current_thread_subscriptions();
        let app = crate::tests::fixtures::test_app_id!();
        let app = app.as_str();
        let sub = zeroship_data_orm::cdc::broker::subscribe(app, "messages");

        zeroship_data_orm::cdc::broker::emit_local(
            app,
            "messages",
            zeroship_data_orm::cdc::ChangeOp::Insert,
            Some("usr_02HXINTEGRATIONSUBPK".to_string()),
            vec!["title".into()],
            std::collections::HashMap::new(),
        );

        let msg = sub.pop().expect("expected an event");
        match msg {
            zeroship_data_orm::cdc::broker::SubscriptionMessage::Change(ev) => {
                assert_eq!(ev.collection, "messages");
                assert_eq!(ev.pk.as_deref(), Some("usr_02HXINTEGRATIONSUBPK"));
                assert_eq!(ev.op, zeroship_data_orm::cdc::ChangeOp::Insert);
            }
            other => panic!("unexpected: {other:?}"),
        }
        sub.close();
        zeroship_data_orm::cdc::broker::drain_current_thread_subscriptions();
    })
}

/// Helper: build a minimal ChangeEvent for the queue-mechanics tests.
fn gapb_ev(app: &str, collection: &str, pk: i64) -> zeroship_data_orm::cdc::ChangeEvent {
    zeroship_data_orm::cdc::ChangeEvent {
        app_id: app.to_string(),
        collection: collection.to_string(),
        op: zeroship_data_orm::cdc::ChangeOp::Insert,
        pk: Some(pk.to_string()),
        changed_columns: vec![],
        new_tuple: std::collections::HashMap::new(),
        old_tuple: None,
    }
}

#[test]
fn gap_b_commit_drains_pending_emits_to_broker() {
    Host::test(|host| {
        // Subscribe BEFORE pushing events, mid-"transaction" push two,
        // then drain — the broker should receive both.
        zeroship_data_orm::cdc::broker::drain_current_thread_subscriptions();
        let app = crate::tests::fixtures::test_app_id!();
        let app = app.as_str();
        let sub = zeroship_data_orm::cdc::broker::subscribe(app, "users");

        host.push_pending_emit(gapb_ev(app, "users", 1));
        host.push_pending_emit(gapb_ev(app, "users", 2));
        // Pre-drain: subscriber must observe nothing (events still queued).
        assert!(sub.pop().is_none(), "events must not leak before commit");

        host.drain_pending_emits(app);

        let mut pks: Vec<String> = Vec::new();
        while let Some(zeroship_data_orm::cdc::broker::SubscriptionMessage::Change(ev)) = sub.pop()
        {
            pks.push(ev.pk.as_deref().unwrap().to_string());
        }
        assert_eq!(pks, vec!["1".to_string(), "2".to_string()]);

        sub.close();
        zeroship_data_orm::cdc::broker::drain_current_thread_subscriptions();
    })
}

#[test]
fn gap_b_rollback_clears_pending_emits_silently() {
    Host::test(|host| {
        // Push events, then `clear` (rollback path). The broker must
        // never see them.
        zeroship_data_orm::cdc::broker::drain_current_thread_subscriptions();
        let app = crate::tests::fixtures::test_app_id!();
        let app = app.as_str();
        let sub = zeroship_data_orm::cdc::broker::subscribe(app, "users");

        host.push_pending_emit(gapb_ev(app, "users", 42));
        host.push_pending_emit(gapb_ev(app, "users", 43));
        host.clear_pending_emits(app);

        assert!(
            sub.pop().is_none(),
            "rollback must NOT publish any broker event"
        );

        sub.close();
        zeroship_data_orm::cdc::broker::drain_current_thread_subscriptions();
    })
}

#[test]
fn gap_b_end_to_end_insert_inside_tx_defers_emit_until_commit() {
    Host::test(|host| {
        host.run(async {
            // End-to-end: real Postgres tx, real `exec_mutation_with_emit`
            // call. Pre-commit the broker stays empty; post-drain it sees
            // the insert.
            let (_postgres, url) = require_pg(host).await;
            host.set_database_url(&url);
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());

            let app = crate::tests::fixtures::test_app_id!();

            let app = app.as_str();
            // Fresh schema with one collection table.
            pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
                .await
                .unwrap();
            pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
                .await
                .unwrap();
            pool.execute(
                &format!(
                    // The seven platform system columns are here because a write's
                    // RETURNING is now an explicit list of them plus the declared
                    // fields. A fixture table missing them fails with `42703 column
                    // does not exist` - loudly, which is the point of naming columns
                    // rather than starring them.
                    r#"CREATE TABLE "{app}"."users" (
                id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                name TEXT NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                created_by TEXT,
                updated_by TEXT,
                version INTEGER NOT NULL DEFAULT 1,
                deleted_at TIMESTAMPTZ
            )"#
                ),
                &[],
            )
            .await
            .unwrap();

            zeroship_data_orm::cdc::broker::drain_current_thread_subscriptions();
            let sub = zeroship_data_orm::cdc::broker::subscribe(app, "users");

            // Open the production transaction protocol.
            host.begin_transaction(app, &url).await;

            let role = zeroship_core::database_role::per_app_role_name(app).unwrap();
            pool.batch_execute(&format!(r#"GRANT SELECT, INSERT ON "{app}"."users" TO "{role}"; GRANT USAGE ON ALL SEQUENCES IN SCHEMA "{app}" TO "{role}""#)).await.unwrap();

            // Insert via the production helper.
            let bq = zeroship_data_sql::compile::build_insert(
                &zeroship_data_sql::SchemaName::new(app).expect("fixture schema name"),
                "users",
                // The descriptor entry for the fixture table above: one declared field.
                &zeroship_data_sql::value!({ "name": { "type": "string", "required": true } }),
                &zeroship_data_sql::value!({ "name": "alice" }),
            )
            .expect("build_insert");
            let _ = host.exec_mutation_with_emit(
                bq,
                app,
                "users",
                zeroship_data_orm::cdc::ChangeOp::Insert,
            )
            .await
            .expect("insert");

            // Mid-transaction: subscriber must see nothing.
            assert!(
                sub.pop().is_none(),
                "pre-commit broker must be empty (Gap B)"
            );

            // Settlement commits the row before publishing its buffered event.
            assert!(matches!(
                zeroship_data_orm::transaction::exec_settle(app, true, None).await,
                zeroship_data_orm::transaction::SettleOutcome::Ok
            ));

            let got = sub.pop();
            match got {
                Some(zeroship_data_orm::cdc::broker::SubscriptionMessage::Change(ev)) => {
                    assert_eq!(ev.collection, "users");
                }
                other => panic!("expected Change event after commit, got: {other:?}"),
            }

            sub.close();
            zeroship_data_orm::cdc::broker::drain_current_thread_subscriptions();
            release_pg(host, pool).await;
        })
    })
}
