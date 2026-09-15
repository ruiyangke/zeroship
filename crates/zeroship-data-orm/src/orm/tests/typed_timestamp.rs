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
    happened: UtcInstant,
    deadline: Option<UtcInstant>,
    touched: UtcInstant,
}

#[derive(Insertable)]
#[orm(entity = moments)]
struct NewMoment<'a> {
    id: &'a str,
    happened: UtcInstant,
    deadline: Option<UtcInstant>,
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

// Commit negative and sub-second offsets and read the stored instants back.
//
// `happened` records the database clock in the same statement as `deadline`,
// so their difference is the offset the database applied. SQLite reads one
// clock for the whole statement. PostgreSQL reads `clock_timestamp()` once per
// expression, so its difference also carries the gap between those two reads;
// the allowed window stays far below the error of a misrendered sign or
// fraction.
async fn committed_offsets_shift_the_database_clock(
    table: &EntityCollection<moments::Entity>,
    postgres: bool,
) {
    let clock_read_gap_micros = if postgres { 100_000 } else { 0 };
    for (offset_micros, expression) in [
        (
            -1_500_000,
            TimestampExpr::database_now()
                .minus(Duration::from_millis(1_500))
                .unwrap(),
        ),
        (
            5_000,
            TimestampExpr::database_now()
                .plus(Duration::from_millis(5))
                .unwrap(),
        ),
    ] {
        Box::pin(
            table.update::<_, Moment>(
                moments::id.eq("row").unwrap(),
                moments::happened
                    .set_expression(TimestampExpr::database_now())
                    .unwrap()
                    .and(moments::deadline.set_expression(expression).unwrap())
                    .unwrap(),
            ),
        )
        .await
        .unwrap()
        .expect("the row exists");
        let stored = table.query().first::<Moment>().await.unwrap().unwrap();
        let applied = stored
            .deadline
            .expect("the offset was committed")
            .unix_micros()
            - stored.happened.unix_micros();
        assert!(
            (applied - offset_micros).abs() <= clock_read_gap_micros,
            "an offset of {offset_micros} us was stored {applied} us from the database clock"
        );
    }
}

// A patch whose only value names a field the descriptor reassigns on write is
// refused without writing. The same value beside a caller expression is removed
// and the expression still writes; the generator replaces the removed value.
async fn reassigned_values_leave_caller_expressions(table: &EntityCollection<moments::Entity>) {
    let reassigned = || {
        Patch::<moments::Entity>::from_assignments(
            [("touched".to_owned(), Value::TimestampMicros(0))].into(),
        )
    };
    let before = table.query().first::<Moment>().await.unwrap().unwrap();
    let error = Box::pin(table.update::<_, Moment>(moments::id.eq("row").unwrap(), reassigned()))
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            DbError::ValidationFailed {
                code: "invalid_update",
                ..
            }
        ),
        "{error:?}"
    );
    let refused = table.query().first::<Moment>().await.unwrap().unwrap();
    assert_eq!(
        (refused.deadline, refused.touched),
        (before.deadline, before.touched)
    );
    let written = Box::pin(
        table.update::<_, Moment>(
            moments::id.eq("row").unwrap(),
            reassigned()
                .and(
                    moments::deadline
                        .set_expression(
                            TimestampExpr::database_now()
                                .minus(Duration::from_secs(3_600))
                                .unwrap(),
                        )
                        .unwrap(),
                )
                .unwrap(),
        ),
    )
    .await
    .unwrap()
    .expect("the row exists");
    assert!(written.deadline < before.deadline, "the expression wrote");
    assert_ne!(
        written.touched.unix_micros(),
        0,
        "the generator replaced the removed value"
    );
}

async fn exercise(postgres: bool) {
    let owner = fixture(postgres).await;
    let table = owner.database.entity::<moments::Entity>().unwrap();
    let epoch = UtcInstant::from_unix_micros(0).unwrap();
    table
        .insert::<_, Moment>(NewMoment {
            id: "row",
            happened: epoch,
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
            let deadline = next.deadline.unwrap().unix_micros();
            assert!(deadline > next.happened.unix_micros() + 59_000_000);
            assert!(deadline < next.happened.unix_micros() + 61_000_000);
            if postgres {
                assert_eq!(
                    first.touched, next.touched,
                    "declared transaction-clock assignments retain their semantics"
                );
            }
            Ok::<_, DbError>(next)
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
            .and(moments::happened.set(epoch).unwrap())
            .is_err()
    );
    // The widest offset both backends can render: a whole number of
    // milliseconds, so SQLite's millisecond resolution refuses it for leaving
    // the calendar rather than for its precision.
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
                .update::<_, Moment>(
                    moments::id.eq("row")?,
                    moments::happened.set(UtcInstant::from_unix_millis(7)?)?,
                )
                .await?;
            Ok::<_, DbError>(())
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
        UtcInstant::from_unix_millis(7).unwrap()
    );
    Box::pin(committed_offsets_shift_the_database_clock(&table, postgres)).await;
    Box::pin(reassigned_values_leave_caller_expressions(&table)).await;
    let ids = (0..crate::budgets::MAX_PER_ROW_UPDATE_TARGETS)
        .map(|index| format!("extra_{index}")).collect::<Vec<_>>();
    for batch in ids.chunks(crate::budgets::MAX_INSERT_MANY_BATCH) {
        table.insert_many::<_, Moment>(batch.iter().map(|id| NewMoment {
            id, happened: epoch, deadline: None,
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

/// Offsets are whole microseconds. A finer duration has no exact rendering on
/// any backend, so it is refused where it is written rather than rounded.
#[test]
fn typed_timestamp_rejects_unrepresentable_offsets() {
    for nanos in [1_u64, 999, 1_001] {
        assert!(
            TimestampExpr::database_now()
                .plus(Duration::from_nanos(nanos))
                .is_err(),
            "{nanos}"
        );
        assert!(
            TimestampExpr::database_now()
                .minus(Duration::from_nanos(nanos))
                .is_err(),
            "{nanos}"
        );
    }
    assert!(TimestampExpr::database_now().plus(Duration::MAX).is_err());
    assert!(TimestampExpr::database_now().minus(Duration::MAX).is_err());
    // Control: a whole microsecond is accepted and cancels exactly, which is
    // the resolution the offset now carries.
    for duration in [Duration::from_micros(1), Duration::from_millis(1)] {
        assert_eq!(
            TimestampExpr::database_now()
                .plus(duration)
                .unwrap()
                .minus(duration)
                .unwrap(),
            TimestampExpr::database_now()
        );
    }
}

/// A value read back out of PostgreSQL matches its own row by equality.
///
/// This is the shape a reservation token depends on: a write returns the stored
/// instant, and a later statement re-binds that instant to find the same row.
/// Any rounding at the driver boundary breaks it silently.
#[compio::test]
async fn read_back_value_matches_its_row_by_equality() {
    let owner = fixture(true).await;
    let table = owner.database.entity::<moments::Entity>().unwrap();
    table
        .insert::<_, Moment>(NewMoment {
            id: "row",
            happened: UtcInstant::from_unix_micros(0).unwrap(),
            deadline: None,
        })
        .await
        .unwrap();
    let reserved = Box::pin(table.update::<_, Moment>(
        moments::id.eq("row").unwrap(),
        moments::happened
            .set_expression(TimestampExpr::database_now())
            .unwrap(),
    ))
    .await
    .unwrap()
    .expect("the row exists")
    .happened;
    // The database clock carries a fraction, so a millisecond-floored value
    // would be a different instant than the one stored.
    assert_ne!(
        reserved.unix_micros() % 1_000,
        0,
        "the database clock must supply a sub-millisecond fraction for this case to bind"
    );
    assert_eq!(
        table
            .query()
            .filter(moments::happened.eq(reserved).unwrap())
            .count()
            .await
            .unwrap(),
        1
    );
    // Control: one microsecond away matches nothing, so the equality above is
    // the stored instant and not a coarse match.
    let skewed = UtcInstant::from_unix_micros(reserved.unix_micros() + 1).unwrap();
    assert_eq!(
        table
            .query()
            .filter(moments::happened.eq(skewed).unwrap())
            .count()
            .await
            .unwrap(),
        0
    );
    owner.close().await;
}

/// SQLite stores whole milliseconds. A finer value is refused with a typed
/// code before any statement runs, and nothing is written.
#[compio::test]
async fn sqlite_refuses_sub_millisecond_values() {
    let owner = fixture(false).await;
    let table = owner.database.entity::<moments::Entity>().unwrap();
    let assert_refused = |result: Result<Option<Moment>, DbError>| {
        let error = result.unwrap_err();
        assert!(
            matches!(
                error,
                DbError::ValidationFailed {
                    code: "timestamp_precision_unsupported",
                    ..
                }
            ),
            "{error:?}"
        );
    };
    for micros in [1_001_i64, -1, 999] {
        let fine = UtcInstant::from_unix_micros(micros).unwrap();
        assert_refused(
            table
                .insert::<_, Moment>(NewMoment {
                    id: "fine",
                    happened: fine,
                    deadline: None,
                })
                .await
                .map(Some),
        );
        assert_eq!(
            table.query().count().await.unwrap(),
            0,
            "a refused write must leave the table empty"
        );
    }
    // Control: whole milliseconds round-trip exactly, on both sides of the
    // epoch, so the refusal is about the fraction and not about writing at all.
    for millis in [-1_i64, 0, 1_001] {
        let coarse = UtcInstant::from_unix_millis(millis).unwrap();
        let written = table
            .insert::<_, Moment>(NewMoment {
                id: &format!("coarse_{millis}"),
                happened: coarse,
                deadline: Some(coarse),
            })
            .await
            .unwrap();
        assert_eq!(written.happened, coarse);
        assert_eq!(written.deadline, Some(coarse));
    }
    // A sub-millisecond offset on the database clock is refused the same way.
    let error = Box::pin(table.update_many(
        Filter::all(),
        moments::deadline
            .set_expression(
                TimestampExpr::database_now()
                    .plus(Duration::from_micros(1))
                    .unwrap(),
            )
            .unwrap(),
    ))
    .await
    .unwrap_err();
    assert!(
        matches!(
            error,
            DbError::ValidationFailed {
                code: "timestamp_precision_unsupported",
                ..
            }
        ),
        "{error:?}"
    );
    owner.close().await;
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
