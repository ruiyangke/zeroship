use super::*;
use fixtures::CollectionFixture;

fn fields() -> Value {
    value!({
        "id": {"type":"string", "primaryKey":true, "required":true},
        "label": {"type":"string", "required":true}
    })
}

async fn fixture(postgres: bool) -> CollectionFixture {
    let columns = "id TEXT PRIMARY KEY, label TEXT NOT NULL";
    if postgres {
        CollectionFixture::postgres_from_table_definition("records", fields(), columns).await
    } else {
        CollectionFixture::sqlite_from_table_definition("records", fields(), columns).await
    }
}

fn options(level: IsolationLevel) -> TransactionOptions {
    TransactionOptions::default().isolation_level(level)
}

async fn dropping_nested_callback_cancels_parent(postgres: bool) {
    use futures::{
        channel::oneshot,
        future::{select, Either},
        FutureExt,
    };

    let fixture = fixture(postgres).await;
    let result = fixture
        .database
        .transaction(|tx| async move {
            let (started, ready) = oneshot::channel();
            let nested = tx
                .transaction(|nested| async move {
                    nested
                        .collection("records")?
                        .insert(value!({"id":"abandoned", "label":"must roll back"}))
                        .await?;
                    started.send(()).unwrap();
                    std::future::pending::<()>().await;
                    Ok::<_, DbError>(())
                })
                .boxed_local();
            match select(ready, nested).await {
                Either::Left((ready, nested)) => {
                    ready.unwrap();
                    drop(nested);
                }
                Either::Right((result, _)) => panic!("child ended before cancellation: {result:?}"),
            }
            Ok::<_, DbError>(())
        })
        .await;
    assert!(
        result.is_err(),
        "an abandoned nested callback must not let its parent commit"
    );
    fixture
        .database
        .transaction(|tx| async move {
            assert!(matches!(
                tx.collection("records")?
                    .count(value!({}), value!({}))
                    .await?,
                Output::Count(0)
            ));
            Ok::<_, DbError>(())
        })
        .await
        .unwrap();
    fixture.close().await;
}

#[compio::test]
async fn sqlite_dropped_nested_callback_cancels_its_parent() {
    dropping_nested_callback_cancels_parent(false).await;
}

#[compio::test]
async fn postgres_dropped_nested_callback_cancels_its_parent() {
    dropping_nested_callback_cancels_parent(true).await;
}

async fn retired_callback_cannot_settle_replacement(postgres: bool) {
    use futures::{
        channel::oneshot,
        future::{select, Either},
        FutureExt,
    };

    let fixture = fixture(postgres).await;
    let (old_ready, old_started) = oneshot::channel();
    let (old_resume, old_wait) = oneshot::channel();
    let old = fixture
        .database
        .transaction(|tx| async move {
            tx.collection("records")?
                .insert(value!({"id":"old", "label":"rollback"}))
                .await?;
            old_ready.send(()).unwrap();
            old_wait.await.unwrap();
            Ok::<_, DbError>(())
        })
        .boxed_local();
    let old = match select(old_started, old).await {
        Either::Left((ready, old)) => {
            ready.unwrap();
            old
        }
        Either::Right((result, _)) => panic!("old callback ended before control: {result:?}"),
    };
    fixture
        .database
        .context
        .scope(crate::transaction::driver::cancel(
            &fixture.database.binding.route(),
        ))
        .await;
    let (new_ready, new_started) = oneshot::channel();
    let (new_resume, new_wait) = oneshot::channel();
    let replacement = fixture
        .database
        .transaction(|tx| async move {
            tx.collection("records")?
                .insert(value!({"id":"replacement", "label":"commit"}))
                .await?;
            new_ready.send(()).unwrap();
            new_wait.await.unwrap();
            tx.collection("records")?
                .insert(value!({"id":"after", "label":"commit"}))
                .await?;
            Ok::<_, DbError>(())
        })
        .boxed_local();
    let replacement = match select(new_started, replacement).await {
        Either::Left((ready, replacement)) => {
            ready.unwrap();
            replacement
        }
        Either::Right((result, _)) => panic!("replacement ended before control: {result:?}"),
    };
    old_resume.send(()).unwrap();
    assert!(
        old.await.is_err(),
        "a retired callback must not settle the replacement transaction"
    );
    new_resume.send(()).unwrap();
    replacement.await.unwrap();
    assert!(matches!(
        fixture
            .database
            .collection("records")
            .unwrap()
            .count(value!({}), value!({}))
            .await
            .unwrap(),
        Output::Count(2)
    ));
    fixture.close().await;
}

#[compio::test]
async fn sqlite_retired_callback_cannot_settle_a_replacement() {
    retired_callback_cannot_settle_replacement(false).await;
}

#[compio::test]
async fn postgres_retired_callback_cannot_settle_a_replacement() {
    retired_callback_cannot_settle_replacement(true).await;
}

async fn isolation(db: &Database) -> String {
    let route = db.capture_route().bind(db.backend.clone()).unwrap();
    let rows = crate::exec::run_sql(&route, "SHOW transaction_isolation", &[])
        .await
        .unwrap();
    rows[0]["transaction_isolation"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn label(db: &Database) -> String {
    let Output::Rows { rows, .. } = db
        .collection("records")
        .unwrap()
        .find(value!({"id":"record"}), value!({}))
        .await
        .unwrap()
    else {
        panic!("expected rows")
    };
    rows[0]["label"].as_str().unwrap().to_owned()
}

#[compio::test]
async fn postgres_explicit_isolation_reaches_the_transaction_session() {
    let fixture = fixture(true).await;
    for level in [
        IsolationLevel::ReadUncommitted,
        IsolationLevel::ReadCommitted,
        IsolationLevel::RepeatableRead,
        IsolationLevel::Serializable,
    ] {
        fixture
            .database
            .transaction_with_options(options(level), |tx| async move {
                assert_eq!(isolation(&tx).await, level.ansi_name().to_lowercase());
                tx.transaction(|nested| async move {
                    assert_eq!(isolation(&nested).await, level.ansi_name().to_lowercase());
                    Ok::<_, DbError>(())
                })
                .await
            })
            .await
            .unwrap();
    }
    fixture
        .database
        .transaction(|tx| async move {
            assert_eq!(isolation(&tx).await, "read committed");
            Ok::<_, DbError>(())
        })
        .await
        .unwrap();
    fixture.close().await;
}

#[compio::test]
async fn postgres_requested_isolation_controls_statement_snapshots() {
    let fixture = fixture(true).await;
    fixture
        .database
        .collection("records")
        .unwrap()
        .insert(value!({"id":"record", "label":"original"}))
        .await
        .unwrap();
    for (level, expected) in [
        (IsolationLevel::ReadCommitted, "changed"),
        (IsolationLevel::RepeatableRead, "original"),
    ] {
        fixture
            .database
            .collection("records")
            .unwrap()
            .update(value!({"id":"record"}), value!({"label":"original"}))
            .await
            .unwrap();
        let outside = fixture.database.clone();
        fixture
            .database
            .transaction_with_options(options(level), |tx| async move {
                assert_eq!(label(&tx).await, "original");
                outside
                    .collection("records")?
                    .update(value!({"id":"record"}), value!({"label":"changed"}))
                    .await?;
                assert_eq!(label(&tx).await, expected);
                Ok::<_, DbError>(())
            })
            .await
            .unwrap();
    }
    fixture.close().await;
}

#[compio::test]
async fn sqlite_rejects_unavailable_isolation_before_invoking_callback() {
    let fixture = fixture(false).await;
    for level in [
        IsolationLevel::ReadUncommitted,
        IsolationLevel::ReadCommitted,
        IsolationLevel::RepeatableRead,
    ] {
        let called = Cell::new(false);
        let result = fixture
            .database
            .transaction_with_options(options(level), |_| async {
                called.set(true);
                Ok(())
            })
            .await;
        let error = result.expect_err("SQLite must reject unsupported isolation");
        assert!(
            matches!(
                error,
                DbError::ValidationFailed {
                    code: "unsupported_isolation_level",
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(!called.get());
        fixture
            .database
            .transaction(|_| async { Ok::<_, DbError>(()) })
            .await
            .unwrap();
    }
    fixture.close().await;
}

async fn nested_callbacks_and_rollback(fixture: CollectionFixture) {
    let db = &fixture.database;
    let escaped = db
        .transaction_with_options(options(IsolationLevel::Serializable), |tx| async move {
            tx.collection("records")?
                .insert(value!({"id":"outer", "label":"kept"}))
                .await?;
            for level in [IsolationLevel::ReadCommitted, IsolationLevel::Serializable] {
                let called = Cell::new(false);
                let error = tx
                    .transaction_with_options(options(level), |_| async {
                        called.set(true);
                        Ok(())
                    })
                    .await
                    .expect_err("a savepoint cannot select isolation");
                assert!(
                    matches!(
                        error,
                        DbError::ValidationFailed {
                            code: "nested_isolation_level",
                            ..
                        }
                    ),
                    "{error:?}"
                );
                assert!(!called.get());
            }
            tx.transaction_with_options(TransactionOptions::default(), |nested| async move {
                nested
                    .collection("records")?
                    .insert(value!({"id":"inner", "label":"kept"}))
                    .await?;
                Ok::<_, DbError>(())
            })
            .await?;
            let error = tx
                .transaction(|nested| async move {
                    nested
                        .collection("records")?
                        .insert(value!({"id":"discarded", "label":"rolled back"}))
                        .await?;
                    Err::<(), _>(DbError::validation(
                        "rollback_probe",
                        "roll back nested callback",
                    ))
                })
                .await
                .expect_err("nested rollback");
            assert!(matches!(
                error,
                DbError::ValidationFailed {
                    code: "rollback_probe",
                    ..
                }
            ));
            Ok::<_, DbError>(tx)
        })
        .await
        .unwrap();
    let error = escaped
        .transaction_with_options(TransactionOptions::default(), |_| async { Ok(()) })
        .await
        .expect_err("escaped transaction handles expire");
    assert!(
        matches!(
            error,
            DbError::ValidationFailed {
                code: "transaction_scope_expired",
                ..
            }
        ),
        "{error:?}"
    );
    let error = db
        .transaction_with_options(options(IsolationLevel::Serializable), |tx| async move {
            tx.collection("records")?
                .insert(value!({"id":"top_discarded", "label":"rolled back"}))
                .await?;
            Err::<(), _>(DbError::validation(
                "rollback_probe",
                "roll back top-level callback",
            ))
        })
        .await
        .expect_err("top-level rollback");
    assert!(matches!(
        error,
        DbError::ValidationFailed {
            code: "rollback_probe",
            ..
        }
    ));
    let Output::Rows { rows, .. } = db
        .collection("records")
        .unwrap()
        .find(value!({}), value!({"orderBy":{"id":1}}))
        .await
        .unwrap()
    else {
        panic!("expected rows")
    };
    assert_eq!(
        rows,
        vec![
            value!({"id":"inner", "label":"kept"}),
            value!({"id":"outer", "label":"kept"}),
        ]
    );
    fixture.close().await;
}

#[compio::test]
async fn postgres_options_preserve_savepoint_scopes_and_rollback() {
    nested_callbacks_and_rollback(fixture(true).await).await;
}

#[compio::test]
async fn sqlite_options_preserve_savepoint_scopes_and_rollback() {
    nested_callbacks_and_rollback(fixture(false).await).await;
}
