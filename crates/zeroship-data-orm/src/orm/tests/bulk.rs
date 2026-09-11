use super::*;
use crate::cdc::{
    ChangeOp,
    broker::{self, Subscription, SubscriptionMessage},
};
use fixtures::CollectionFixture;

fn fields() -> Value {
    value!({"label":{"type":"string", "unique":true}, "status":{"type":"string"}})
}

async fn bulk_counts(mut fixture: CollectionFixture) {
    fixture
        .rename_fields(
            "entries",
            &[
                ("id", "row_key"),
                ("version", "revision"),
                ("deleted_at", "removed"),
            ],
        )
        .await;
    let db = fixture.database.clone();
    let entries = db.collection("entries").unwrap();
    let total = crate::compile::MAX_QUERY_LIMIT + 17;
    let documents: Vec<_> = (0..total)
        .map(|n| value!({"label":format!("entry-{n}"), "status":"new"}))
        .collect();
    for chunk in documents.chunks(100) {
        entries
            .execute(Operation::InsertMany {
                documents: Value::Array(chunk.to_vec()),
            })
            .await
            .unwrap();
    }
    assert_eq!(
        count(
            entries
                .execute(Operation::Update {
                    filter: value!({}),
                    patch: value!({"status":"ready"}),
                    many: true,
                })
                .await
                .unwrap()
        ),
        total
    );
    assert_eq!(
        count(
            entries
                .count(value!({"status":"ready", "revision":2}), value!({}))
                .await
                .unwrap()
        ),
        total
    );
    assert_eq!(
        count(
            entries
                .execute(Operation::Update {
                    filter: value!({"status":"missing"}),
                    patch: value!({"status":"unused"}),
                    many: true,
                })
                .await
                .unwrap()
        ),
        0
    );

    // A statement failure must leave every matching row unchanged.
    assert!(
        entries
            .execute(Operation::Update {
                filter: value!({}),
                patch: value!({"label":"duplicate"}),
                many: true,
            })
            .await
            .is_err()
    );
    assert_eq!(
        count(
            entries
                .count(value!({"revision":2}), value!({}))
                .await
                .unwrap()
        ),
        total
    );

    let rollback: Result<(), DbError> = db
        .transaction(|tx| async move {
            let entries = tx.collection("entries")?;
            assert_eq!(
                count(
                    entries
                        .execute(Operation::Update {
                            filter: value!({}),
                            patch: value!({"status":"rolled-back"}),
                            many: true,
                        })
                        .await?
                ),
                total
            );
            assert_eq!(
                count(
                    entries
                        .execute(Operation::Purge {
                            filter: value!({}),
                            many: true
                        })
                        .await?
                ),
                total
            );
            assert_eq!(count(entries.count(value!({}), value!({})).await?), 0);
            Err(DbError::internal("rollback bulk writes"))
        })
        .await;
    assert!(rollback.is_err());
    assert_eq!(
        count(
            entries
                .count(value!({"status":"ready", "revision":2}), value!({}))
                .await
                .unwrap()
        ),
        total
    );

    let escaped = db
        .transaction(|tx| async move {
            let entries = tx.collection("entries")?;
            assert_eq!(
                count(
                    entries
                        .execute(Operation::Delete {
                            filter: value!({}),
                            many: true
                        })
                        .await?
                ),
                total
            );
            assert_eq!(
                count(
                    entries
                        .execute(Operation::Delete {
                            filter: value!({}),
                            many: true
                        })
                        .await?
                ),
                0
            );
            Ok(entries)
        })
        .await
        .unwrap();
    assert!(
        escaped
            .execute(Operation::Purge {
                filter: value!({}),
                many: true
            })
            .await
            .is_err()
    );
    assert_eq!(
        count(entries.count(value!({}), value!({})).await.unwrap()),
        0
    );
    assert_eq!(
        count(
            entries
                .execute(Operation::Restore {
                    filter: value!({}),
                    many: true
                })
                .await
                .unwrap()
        ),
        total
    );
    assert_eq!(
        count(
            entries
                .execute(Operation::Restore {
                    filter: value!({}),
                    many: true
                })
                .await
                .unwrap()
        ),
        0
    );
    assert_eq!(
        count(
            entries
                .execute(Operation::Purge {
                    filter: value!({"label":"entry-0"}),
                    many: true
                })
                .await
                .unwrap()
        ),
        1
    );
    assert_eq!(
        count(
            entries
                .execute(Operation::Purge {
                    filter: value!({}),
                    many: true
                })
                .await
                .unwrap()
        ),
        total - 1
    );
    assert_eq!(
        count(
            entries
                .execute(Operation::Purge {
                    filter: value!({}),
                    many: true
                })
                .await
                .unwrap()
        ),
        0
    );
    assert_eq!(
        count(
            entries
                .count(value!({}), value!({"includeDeleted":true}))
                .await
                .unwrap()
        ),
        0
    );
    fixture.close().await;
}

#[compio::test]
async fn sqlite_bulk_counts_preserve_all_matches_and_atomicity() {
    bulk_counts(CollectionFixture::sqlite("entries", fields()).await).await;
}

#[compio::test]
async fn postgres_bulk_counts_preserve_all_matches_and_atomicity() {
    bulk_counts(CollectionFixture::postgres("entries", fields()).await).await;
}

#[compio::test]
async fn postgres_bulk_counts_do_not_require_unrelated_column_reads() {
    let fixture = CollectionFixture::postgres("entries", fields()).await;
    let db = fixture.database.clone();
    let entries = db.collection("entries").unwrap();
    entries
        .execute(Operation::InsertMany {
            documents: value!([
                {"label":"first", "status":"new"},
                {"label":"second", "status":"new"},
            ]),
        })
        .await
        .unwrap();
    let backend = db
        .backend
        .get::<crate::backend::postgres::PostgresBackend>()
        .unwrap();
    let table = format!("{}.\"entries\"", db.binding.schema().quoted());
    let role = crate::compile::quote_ident(
        &zeroship_core::database_role::per_app_role_name(db.binding.schema().as_str()).unwrap(),
    );
    backend.pool().batch_execute(&format!(
        "REVOKE SELECT ON {table} FROM {role}; GRANT SELECT (id, status, version, deleted_at) ON {table} TO {role}"
    )).await.unwrap();
    assert!(
        entries
            .find(value!({}), value!({"select":["label"]}))
            .await
            .is_err()
    );
    assert_eq!(
        count(
            entries
                .execute(Operation::Update {
                    filter: value!({"status":"new"}),
                    patch: value!({"status":"ready"}),
                    many: true,
                })
                .await
                .unwrap()
        ),
        2
    );
    db.transaction(|tx| async move {
        let entries = tx.collection("entries")?;
        assert_eq!(
            count(
                entries
                    .execute(Operation::Delete {
                        filter: value!({}),
                        many: true
                    })
                    .await?
            ),
            2
        );
        assert_eq!(
            count(
                entries
                    .execute(Operation::Restore {
                        filter: value!({}),
                        many: true
                    })
                    .await?
            ),
            2
        );
        assert_eq!(
            count(
                entries
                    .execute(Operation::Purge {
                        filter: value!({"status":"ready"}),
                        many: true
                    })
                    .await?
            ),
            2
        );
        Ok(())
    })
    .await
    .unwrap();
    fixture.close().await;
}

async fn next_change(sub: &Subscription) -> std::sync::Arc<crate::cdc::ChangeEvent> {
    let message = compio::time::timeout(
        std::time::Duration::from_secs(5),
        futures::future::poll_fn(|cx| {
            sub.register_waker(cx.waker().clone());
            sub.pop()
                .map_or(std::task::Poll::Pending, std::task::Poll::Ready)
        }),
    )
    .await
    .expect("committed CDC event");
    let SubscriptionMessage::Change(event) = message else {
        panic!("unexpected CDC message: {message:?}")
    };
    event
}

async fn bulk_cdc(fixture: CollectionFixture) {
    let db = fixture.database.clone();
    let entries = db.collection("entries").unwrap();
    let sub = broker::subscribe(db.binding.app_id(), "entries");
    for label in ["first", "second"] {
        entries
            .insert(value!({"label":label, "status":"new"}))
            .await
            .unwrap();
        assert_eq!(next_change(&sub).await.op, ChangeOp::Insert);
    }
    let rollback: Result<(), DbError> = db
        .transaction(|tx| {
            let sub = sub.clone();
            async move {
                assert_eq!(
                    count(
                        tx.collection("entries")?
                            .execute(Operation::Update {
                                filter: value!({}),
                                patch: value!({"status":"rolled-back"}),
                                many: true,
                            })
                            .await?
                    ),
                    2
                );
                assert!(sub.pop().is_none(), "no delivery before commit");
                Err(DbError::internal("rollback CDC write"))
            }
        })
        .await;
    assert!(rollback.is_err());
    assert!(sub.pop().is_none());
    assert_eq!(
        count(
            entries
                .execute(Operation::Update {
                    filter: value!({"status":"missing"}),
                    patch: value!({"status":"unused"}),
                    many: true,
                })
                .await
                .unwrap()
        ),
        0
    );
    assert!(
        entries
            .execute(Operation::Update {
                filter: value!({}),
                patch: value!({"label":"duplicate"}),
                many: true,
            })
            .await
            .is_err()
    );
    assert!(sub.pop().is_none());

    db.transaction(|tx| {
        let sub = sub.clone();
        async move {
            assert_eq!(
                count(
                    tx.collection("entries")?
                        .execute(Operation::Update {
                            filter: value!({}),
                            patch: value!({"status":"committed"}),
                            many: true,
                        })
                        .await?
                ),
                2
            );
            assert!(sub.pop().is_none(), "no delivery before commit");
            Ok(())
        }
    })
    .await
    .unwrap();
    let native_capture = db.backend.publishes_committed_changes();
    let expected_events = if native_capture { 2 } else { 1 };
    for _ in 0..expected_events {
        let event = next_change(&sub).await;
        assert_eq!(event.op, ChangeOp::Update);
        if native_capture {
            assert_eq!(
                event.new_tuple.get("status").map(String::as_str),
                Some("committed")
            );
        } else {
            assert!(event.pk.is_none());
            assert!(event.new_tuple.is_empty());
            assert!(event.changed_columns.is_empty());
        }
    }
    assert!(sub.pop().is_none());
    assert_eq!(
        count(
            entries
                .execute(Operation::Purge {
                    filter: value!({}),
                    many: true
                })
                .await
                .unwrap()
        ),
        2
    );
    for _ in 0..expected_events {
        assert_eq!(next_change(&sub).await.op, ChangeOp::Delete);
    }
    assert!(sub.pop().is_none());
    sub.close();
    fixture.close().await;
}

#[compio::test]
async fn sqlite_bulk_counts_publish_only_committed_changes() {
    bulk_cdc(CollectionFixture::sqlite("entries", fields()).await).await;
}

#[compio::test]
async fn postgres_bulk_counts_publish_only_committed_changes() {
    bulk_cdc(CollectionFixture::postgres("entries", fields()).await).await;
}
