#![expect(
    clippy::future_not_send,
    reason = "ORM fixtures use thread-local compio sessions"
)]

use super::fixtures::CollectionFixture;
use super::*;

include!("../../../tests/fixtures/mutation_expressions_schema.rs");
mutation_expressions_schema!(pub mutation_schema);
use mutation_schema::counters;

#[derive(Debug, FromRow)]
#[orm(entity = counters)]
struct Counter {
    quantity: i64,
    ratio: f64,
    optional: Option<i64>,
    tags: Value,
    moments: Value,
    dates: Value,
    document: Value,
    version: i64,
}
#[derive(Insertable)]
#[orm(entity = counters)]
struct NewCounter {
    id: String,
    quantity: i64,
    ratio: f64,
    optional: Option<i64>,
    tags: Value,
    moments: Value,
    dates: Value,
    document: Value,
}
#[derive(Changeset)]
#[orm(entity = counters)]
struct Edit {
    document: Change<Value>,
}
async fn fixture(postgres: bool) -> CollectionFixture {
    let fields = value!({
        "id": {"type":"string", "required":true, "primaryKey":true},
        "quantity":{"type":"bigint", "required":true},
        "ratio":{"type":"number", "required":true},
        "optional":{"type":"bigint"},
        "tags":{"type":"array", "items":"string", "required":true},
        "moments":{"type":"array", "items":"timestamp", "required":true},
        "dates":{"type":"array", "items":"calendarDate", "required":true},
        "document":{"type":"json", "required":true},
        "version":{"type":"integer", "required":true, "default":1, "writable":false, "assign":{"on":"write", "by":"increment(1)"}}
    });
    let columns = if postgres {
        "id TEXT PRIMARY KEY, quantity BIGINT NOT NULL, ratio DOUBLE PRECISION NOT NULL, optional BIGINT, tags JSONB NOT NULL, moments JSONB NOT NULL, dates JSONB NOT NULL, document JSONB NOT NULL, version INTEGER NOT NULL DEFAULT 1"
    } else {
        "id TEXT PRIMARY KEY, quantity BIGINT NOT NULL, ratio REAL NOT NULL, optional BIGINT, tags TEXT NOT NULL, moments TEXT NOT NULL, dates TEXT NOT NULL, document TEXT NOT NULL, version INTEGER NOT NULL DEFAULT 1"
    };
    let mut owner = if postgres {
        CollectionFixture::postgres_from_table_definition("counters", fields, columns).await
    } else {
        CollectionFixture::sqlite_from_table_definition("counters", fields, columns).await
    };
    owner.database = Database::from_schema(
        owner.database.binding.clone(),
        owner.database.backend.clone(),
        mutation_schema::schema(),
    )
    .unwrap();
    owner
}
#[expect(clippy::too_many_lines, reason = "shared backend conformance scenario")]
async fn exercise(postgres: bool) {
    let owner = fixture(postgres).await;
    let db = &owner.database;
    let table = db.entity::<counters::Entity>().unwrap();
    for id in ["a", "b"] {
        table
            .insert::<_, Counter>(NewCounter {
                id: id.into(),
                quantity: 10,
                ratio: 2.0,
                optional: None,
                tags: value!(["first"]),
                moments: value!([]),
                dates: value!([]),
                document: value!({}),
            })
            .await
            .unwrap();
    }
    let patch = counters::quantity
        .increment(3_i64)
        .unwrap()
        .and(counters::ratio.multiply(2.5_f64).unwrap())
        .unwrap()
        .and(counters::optional.increment(9_i64).unwrap())
        .unwrap()
        .and(counters::tags.push("second").unwrap())
        .unwrap()
        .and(counters::moments.push(0_i64).unwrap())
        .unwrap()
        .and(counters::dates.push("2026-09-13").unwrap())
        .unwrap()
        .and(Edit {
            document: Change::Set(value!({"$inc":17})),
        })
        .unwrap();
    let updated = table
        .update::<_, Counter>(counters::id.eq("a").unwrap(), patch)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.quantity, 13);
    assert_eq!(updated.ratio, 5.0);
    assert_eq!(updated.optional, None);
    assert_eq!(updated.tags, value!(["first", "second"]));
    assert_eq!(updated.moments, value!([0]));
    assert_eq!(updated.dates, value!(["2026-09-13"]));
    assert_eq!(updated.document, value!({"$inc":17}));
    assert_eq!(updated.version, 2);
    let updated = table
        .update::<_, Counter>(
            counters::id.eq("a").unwrap(),
            counters::quantity
                .decrement(4_i64)
                .unwrap()
                .and(counters::tags.add_to_set("second").unwrap())
                .unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.quantity, 9);
    assert_eq!(updated.tags, value!(["first", "second"]));
    let updated = table
        .update::<_, Counter>(
            counters::id.eq("a").unwrap(),
            counters::tags.pull("first").unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.tags, value!(["second"]));
    assert!(counters::quantity
        .increment(1_i64)
        .unwrap()
        .and(counters::quantity.set(20_i64).unwrap())
        .is_err());
    assert!(counters::dates.push("2026-02-30").is_err());
    assert_eq!(
        table
            .update_many(Filter::all(), counters::quantity.increment(1_i64).unwrap())
            .await
            .unwrap(),
        2
    );
    let result: Result<(), DbError> = db
        .transaction(|tx| async move {
            tx.entity::<counters::Entity>()?
                .update_many(Filter::all(), counters::quantity.multiply(3_i64)?)
                .await?;
            Err(DbError::validation("rollback_test", "abort"))
        })
        .await;
    assert!(result.is_err());
    let rows = table
        .query()
        .order_by(counters::id.asc())
        .all::<Counter>()
        .await
        .unwrap();
    assert_eq!(
        rows.iter().map(|row| row.quantity).collect::<Vec<_>>(),
        vec![10, 11]
    );
    owner.close().await;
}
#[compio::test]
async fn typed_mutation_expressions_sqlite() {
    Box::pin(exercise(false)).await;
}
#[compio::test]
async fn typed_mutation_expressions_postgres() {
    Box::pin(exercise(true)).await;
}

async fn count_comparisons(postgres: bool) {
    let owner = fixture(postgres).await;
    let db = &owner.database;
    let table = db.entity::<counters::Entity>().unwrap();
    for id in ["a", "b"] {
        table
            .insert::<_, Counter>(NewCounter {
                id: id.into(),
                quantity: 10,
                ratio: 2.0,
                optional: None,
                tags: value!(["same"]),
                moments: value!([0]),
                dates: value!(["2026-09-14"]),
                document: value!({}),
            })
            .await
            .unwrap();
    }
    let c = table.alias("c").unwrap();
    for expression in [
        c.column(counters::tags).count(),
        c.column(counters::moments).count(),
        c.column(counters::dates).count(),
    ] {
        let rows = db
            .from(&c)
            .having(expression.eq(2_i64).unwrap())
            .select(expression)
            .unwrap()
            .all()
            .await
            .unwrap();
        assert_eq!(rows, vec![2]);
    }
    let distinct = c.column(counters::tags).count_distinct();
    assert!(matches!(
        db.from(&c)
            .having(distinct.eq(1_i64).unwrap())
            .select(distinct)
            .unwrap()
            .all()
            .await,
        Err(DbError::ValidationFailed {
            code: "invalid_read",
            ..
        })
    ));
    let distinct = c.column(counters::quantity).count_distinct();
    assert_eq!(
        db.from(&c)
            .having(distinct.eq(1_i64).unwrap())
            .select(distinct)
            .unwrap()
            .all()
            .await
            .unwrap(),
        vec![1]
    );
    let absent = c.column(counters::dates).count();
    assert_eq!(
        db.from(&c)
            .filter(c.column(counters::id).eq("absent").unwrap())
            .having(absent.eq(0_i64).unwrap())
            .select(absent)
            .unwrap()
            .all()
            .await
            .unwrap(),
        vec![0]
    );
    owner.close().await;
}

#[compio::test]
async fn count_comparisons_sqlite() {
    Box::pin(count_comparisons(false)).await;
}

#[compio::test]
async fn count_comparisons_postgres() {
    Box::pin(count_comparisons(true)).await;
}
