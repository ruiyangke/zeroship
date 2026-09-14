#![expect(
    clippy::future_not_send,
    reason = "ORM fixtures use local compio sessions"
)]

use super::fixtures::CollectionFixture;
use super::*;
use std::time::Duration;

schema! {
    pub clocks {
        moments {
            #[orm(primary_key)]
            id: Text,
            happened: Timestamp,
            deadline: Nullable<Timestamp>,
            #[orm(assign(on = write, by = now), writable = false)]
            touched: Timestamp,
        }
    }
}
use clocks::moments;

#[derive(Debug, FromRow)]
#[orm(entity = moments)]
struct Moment {
    happened: i64,
    deadline: Option<i64>,
    touched: i64,
}

#[derive(Insertable)]
#[orm(entity = moments)]
struct NewMoment<'a> {
    id: &'a str,
    happened: i64,
    deadline: Option<i64>,
}

async fn fixture(postgres: bool) -> CollectionFixture {
    let fields = value!({
        "id":{"type":"string","required":true,"primaryKey":true},
        "happened":{"type":"timestamp","required":true},
        "deadline":{"type":"timestamp"},
        "touched":{"type":"timestamp","required":true,"writable":false,"assign":{"on":"write","by":"now"}}
    });
    let columns = if postgres {
        "id TEXT PRIMARY KEY, happened TIMESTAMPTZ NOT NULL, deadline TIMESTAMPTZ, touched TIMESTAMPTZ NOT NULL DEFAULT NOW()"
    } else {
        "id TEXT PRIMARY KEY, happened TEXT NOT NULL, deadline TEXT, touched TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))"
    };
    let mut owner = if postgres {
        CollectionFixture::postgres_from_table_definition("moments", fields, columns).await
    } else {
        CollectionFixture::sqlite_from_table_definition("moments", fields, columns).await
    };
    owner.database = Database::from_schema(
        owner.database.binding.clone(),
        owner.database.backend.clone(),
        clocks::schema(),
    )
    .unwrap();
    owner
}

/// Both backends refuse an instant outside the portable calendar with the same
/// validation code, whichever side of the calendar it falls on.
fn assert_outside_calendar<T: std::fmt::Debug>(result: Result<T, DbError>) {
    let error = result.unwrap_err();
    assert!(
        matches!(
            error,
            DbError::ValidationFailed {
                code: "invalid_timestamp_expression",
                ..
            }
        ),
        "{error:?}"
    );
}

async fn exercise(postgres: bool) {
    let owner = fixture(postgres).await;
    let table = owner.database.entity::<moments::Entity>().unwrap();
    table
        .insert::<_, Moment>(NewMoment {
            id: "row",
            happened: 0,
            deadline: None,
        })
        .await
        .unwrap();
    let committed = owner
        .database
        .transaction(|tx| async move {
            let table = tx.entity::<moments::Entity>()?;
            let first = table
                .update::<_, Moment>(
                    moments::id.eq("row")?,
                    moments::happened.set_expression(TimestampExpr::database_now())?,
                )
                .await?
                .unwrap();
            compio::time::sleep(Duration::from_millis(30)).await;
            let next = table
                .update::<_, Moment>(
                    moments::id.eq("row")?,
                    moments::happened
                        .set_expression(TimestampExpr::database_now())?
                        .and(moments::deadline.set_expression(
                            TimestampExpr::database_now().plus(Duration::from_secs(60))?,
                        )?)?,
                )
                .await?
                .unwrap();
            assert!(
                next.happened > first.happened,
                "database clock must advance within the transaction"
            );
            assert!(next.deadline.unwrap() > next.happened + 59_000);
            assert!(next.deadline.unwrap() < next.happened + 61_000);
            if postgres {
                assert_eq!(
                    first.touched, next.touched,
                    "declared transaction-clock assignments retain their semantics"
                );
            }
            Ok(next)
        })
        .await
        .unwrap();
    let failure: Result<(), DbError> = owner
        .database
        .transaction(|tx| async move {
            tx.entity::<moments::Entity>()?
                .update_many(
                    Filter::all(),
                    moments::deadline.set_expression(
                        TimestampExpr::database_now().minus(Duration::from_secs(60))?,
                    )?,
                )
                .await?;
            Err(DbError::validation("rollback_test", "abort"))
        })
        .await;
    assert!(failure.is_err());
    let row = table.query().first::<Moment>().await.unwrap().unwrap();
    assert_eq!(row.deadline, committed.deadline);
    assert!(
        moments::happened
            .set_expression(TimestampExpr::database_now())
            .unwrap()
            .and(moments::happened.set(0_i64).unwrap())
            .is_err()
    );
    let span = Duration::from_millis(
        (crate::sql::temporal::MAX_TIMESTAMP_MILLIS - crate::sql::temporal::MIN_TIMESTAMP_MILLIS)
            as u64,
    );
    for expression in [
        TimestampExpr::database_now().plus(span).unwrap(),
        TimestampExpr::database_now().minus(span).unwrap(),
    ] {
        assert_outside_calendar(
            table
                .update::<_, Moment>(
                    moments::id.eq("row").unwrap(),
                    moments::deadline.set_expression(expression).unwrap(),
                )
                .await,
        );
        assert_outside_calendar(
            table
                .update_many(
                    Filter::all(),
                    moments::deadline.set_expression(expression).unwrap(),
                )
                .await,
        );
        let persisted = table.query().first::<Moment>().await.unwrap().unwrap();
        assert_eq!(persisted.deadline, committed.deadline);
        assert_eq!(
            persisted.touched, committed.touched,
            "a refused result must roll back generated assignments too"
        );
    }
    owner
        .database
        .transaction(|tx| async move {
            let table = tx.entity::<moments::Entity>()?;
            for expression in [
                TimestampExpr::database_now().plus(span)?,
                TimestampExpr::database_now().minus(span)?,
            ] {
                assert_outside_calendar(
                    table
                        .update::<_, Moment>(
                            moments::id.eq("row")?,
                            moments::deadline.set_expression(expression)?,
                        )
                        .await,
                );
            }
            table
                .update::<_, Moment>(moments::id.eq("row")?, moments::happened.set(7_i64)?)
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        table
            .query()
            .first::<Moment>()
            .await
            .unwrap()
            .unwrap()
            .happened,
        7
    );
    let ids = (0..crate::budgets::MAX_PER_ROW_UPDATE_TARGETS)
        .map(|index| format!("extra_{index}")).collect::<Vec<_>>();
    for batch in ids.chunks(crate::budgets::MAX_INSERT_MANY_BATCH) {
        table.insert_many::<_, Moment>(batch.iter().map(|id| NewMoment {
            id, happened: 0, deadline: None,
        })).await.unwrap();
    }
    let error = table.update_many(Filter::all(), moments::deadline.set_expression(TimestampExpr::database_now()).unwrap()).await.unwrap_err();
    assert!(matches!(error, DbError::ValidationFailed { code: "update_many_target_limit_exceeded", .. }));
    assert_eq!(table.query().filter(moments::deadline.is_null()).count().await.unwrap(), ids.len() as i64);
    owner.close().await;
}

#[compio::test]
async fn typed_timestamp_sqlite() {
    Box::pin(exercise(false)).await;
}

#[compio::test]
async fn typed_timestamp_postgres() {
    Box::pin(exercise(true)).await;
}

#[test]
fn typed_timestamp_rejects_unrepresentable_offsets() {
    assert!(
        TimestampExpr::database_now()
            .plus(Duration::from_nanos(1))
            .is_err()
    );
    assert!(
        TimestampExpr::database_now()
            .minus(Duration::from_nanos(1))
            .is_err()
    );
    assert!(TimestampExpr::database_now().plus(Duration::MAX).is_err());
    assert!(TimestampExpr::database_now().minus(Duration::MAX).is_err());
    assert_eq!(
        TimestampExpr::database_now()
            .plus(Duration::from_millis(1))
            .unwrap()
            .minus(Duration::from_millis(1))
            .unwrap(),
        TimestampExpr::database_now()
    );
}

schema! {
    pub protected_clocks {
        protected_moments {
            #[orm(primary_key)]
            id: Text,
            #[orm(mask(kind = "full", classification = "pii"))]
            stamp: Nullable<Timestamp>,
        }
    }
}

async fn protected_expression(postgres: bool) {
    let fields = value!({
        "id":{"type":"string","required":true,"primaryKey":true},
        "stamp":{"type":"timestamp","mask":{"kind":"full","classification":"pii"}}
    });
    let mut owner = if postgres {
        CollectionFixture::postgres_from_table_definition(
            "protected_moments",
            fields,
            "id TEXT PRIMARY KEY, stamp TEXT, __zs_raw__stamp TIMESTAMPTZ",
        )
        .await
    } else {
        CollectionFixture::sqlite_from_table_definition(
            "protected_moments",
            fields,
            "id TEXT PRIMARY KEY, stamp TEXT, __zs_raw__stamp TEXT",
        )
        .await
    };
    owner.database = Database::from_schema(
        owner.database.binding.clone(),
        owner.database.backend.clone(),
        protected_clocks::schema(),
    )
    .unwrap();
    let table = owner
        .database
        .entity::<protected_clocks::protected_moments::Entity>()
        .unwrap();
    let error = table
        .update_many(
            Filter::all(),
            protected_clocks::protected_moments::stamp
                .set_expression(TimestampExpr::database_now())
                .unwrap(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        DbError::ValidationFailed {
            code: "protected_update_operation",
            ..
        }
    ));
    owner.close().await;
}

#[compio::test]
async fn typed_timestamp_protected_sqlite() {
    Box::pin(protected_expression(false)).await;
}

#[compio::test]
async fn typed_timestamp_protected_postgres() {
    Box::pin(protected_expression(true)).await;
}
