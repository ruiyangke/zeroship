use super::fixtures::CollectionFixture;
use super::*;

fn fields() -> Value {
    value!({"instant":{"type":"date", "nullable":true,"unique":true}})
}

#[compio::test]
async fn timestamps_preserve_instants_through_postgres() {
    let owner = CollectionFixture::postgres("events", fields()).await;
    exercise_timestamps(&owner.database).await;
    owner.close().await;
}

#[compio::test]
async fn timestamps_preserve_instants_through_sqlite() {
    let owner = CollectionFixture::sqlite("events", fields()).await;
    exercise_timestamps(&owner.database).await;
    owner.close().await;
}

async fn exercise_timestamps(db: &Database) {
    let events = db.collection("events").unwrap();
    for (text, millis) in [
        ("9999-12-31T23:59:50.001Z", 253_402_300_790_001_i64),
        ("0001-01-01", -62_135_596_800_000),
        ("1969-12-31T23:59:59.999999Z", -1),
        ("1970-01-01", 0),
        ("2000-01-01T02:00:00+02:00", 946_684_800_000),
        ("9999-12-31T23:59:59.999Z", 253_402_300_799_999),
    ] {
        let encoded = <i64 as EncodeValue<sql_types::Timestamp>>::encode_value(millis).unwrap();
        let Output::Rows { rows, .. } = db
            .transaction(|tx| async move {
                tx.collection("events")?
                    .insert(value!({"instant":encoded}))
                    .await
            })
            .await
            .unwrap()
        else {
            panic!("insert must return rows")
        };
        assert_eq!(rows[0]["instant"].as_i64(), Some(millis), "{text}");
        let id = rows[0]["id"].clone();
        for input in [value!(millis), value!(text)] {
            let Output::Rows { rows, .. } = events
                .execute(Operation::Upsert {
                    document: value!({"instant":input.clone()}),
                    conflict_fields: value!(["instant"]),
                })
                .await
                .unwrap()
            else {
                panic!("upsert must return rows")
            };
            assert_eq!(rows[0]["id"], id, "equivalent instants must conflict");
            assert_eq!(rows[0]["instant"].as_i64(), Some(millis));
            let Output::Rows { rows, .. } = events
                .update(
                    value!({"id":id.clone()}),
                    value!({"instant":{"$set":input.clone()}}),
                )
                .await
                .unwrap()
            else {
                panic!("update must return rows")
            };
            assert_eq!(rows[0]["instant"].as_i64(), Some(millis));
            for filter in [
                value!({"instant":input.clone()}),
                value!({"instant":{"$in":[input.clone()]}}),
                value!({"instant":{"$gte":input.clone(),"$lte":input.clone()}}),
            ] {
                let Output::Rows { rows, .. } = events
                    .find(value!({"$and":[{"id":id.clone()},filter]}), value!({}))
                    .await
                    .unwrap()
                else {
                    panic!("find must return rows")
                };
                assert_eq!(rows.len(), 1, "{input}");
                assert_eq!(rows[0]["instant"].as_i64(), Some(millis));
            }
        }
    }
    let before = count(events.count(value!({}), value!({})).await.unwrap());
    for invalid in [
        value!("private_not_a_timestamp"),
        value!("2026-02-30T00:00:00Z"),
        value!("2026-01-01T+1:00:00Z"),
        value!("2026-01-01T00:00:00+01:-1"),
        value!(0.5),
        value!(253_402_300_800_000_i64),
    ] {
        for operation in [
            Operation::Insert {
                document: value!({"instant":invalid.clone()}),
            },
            Operation::InsertMany {
                documents: value!([{"instant":0},{"instant":invalid.clone()}]),
            },
            Operation::Upsert {
                document: value!({"instant":invalid.clone()}),
                conflict_fields: value!(["instant"]),
            },
        ] {
            let error = events.execute(operation).await.unwrap_err();
            assert!(matches!(error, DbError::ValidationFailed { .. }), "{error}");
            assert!(!error.to_string().contains("private_not_a_timestamp"));
        }
        for patch in [
            value!({"instant":invalid.clone()}),
            value!({"instant":{"$set":invalid.clone()}}),
            value!({"$set":{"instant":invalid.clone()}}),
            value!({"instant":{"$inc":1}}),
        ] {
            let error = events.update(value!({}), patch).await.unwrap_err();
            assert!(matches!(error, DbError::ValidationFailed { .. }), "{error}");
        }
    }
    assert_eq!(
        count(events.count(value!({}), value!({})).await.unwrap()),
        before
    );
    let Output::Rows { rows, .. } = events.insert(value!({"instant":null})).await.unwrap() else {
        panic!("insert must return rows")
    };
    assert_eq!(rows[0]["instant"], Value::Null);
}

async fn exercise_timestamp_extrema(postgres: bool) {
    let owner = if postgres {
        CollectionFixture::postgres("events", fields()).await
    } else {
        CollectionFixture::sqlite("events", fields()).await
    };
    let events = owner.database.collection("events").unwrap();
    events
        .execute(Operation::InsertMany {
            documents: value!([
                {"instant":"2026-01-01T00:00:00Z"},
                {"instant":"2026-01-03T00:00:00Z"}
            ]),
        })
        .await
        .unwrap();

    let Output::Rows { rows, .. } = events
        .execute(Operation::Aggregate {
            pipeline: value!([{"$group":{
                "earliest":{"$min":"instant"},
                "latest":{"$max":"instant"}
            }}]),
            options: value!({}),
        })
        .await
        .unwrap()
    else {
        panic!("aggregate must return rows")
    };
    assert_eq!(rows.len(), 1);
    assert!(matches!(
        rows[0]["earliest"],
        Value::Timestamp(1_767_225_600_000)
    ));
    assert!(matches!(
        rows[0]["latest"],
        Value::Timestamp(1_767_398_400_000)
    ));

    let Output::Rows { rows, .. } = events
        .execute(Operation::Aggregate {
            pipeline: value!([{"$group":{"instant":{"$count":true}}}]),
            options: value!({}),
        })
        .await
        .unwrap()
    else {
        panic!("aggregate must return rows")
    };
    assert_eq!(rows, vec![value!({"instant":2})]);
    owner.close().await;
}

#[compio::test]
async fn postgres_timestamp_extrema_are_native_timestamps() {
    exercise_timestamp_extrema(true).await;
}

#[compio::test]
async fn sqlite_timestamp_extrema_are_native_timestamps() {
    exercise_timestamp_extrema(false).await;
}

#[compio::test]
async fn timestamps_reject_corrupt_sqlite_storage_without_exposing_it() {
    let owner = CollectionFixture::sqlite("events", fields()).await;
    let events = owner.database.collection("events").unwrap();
    events.insert(value!({"instant":0})).await.unwrap();
    let fixture = rusqlite::Connection::open(owner.sqlite_file.as_ref().unwrap()).unwrap();
    fixture
        .execute_batch("PRAGMA ignore_check_constraints = ON")
        .unwrap();
    for invalid in ["private_not_a_timestamp", "2026-02-30T00:00:00Z"] {
        fixture
            .execute("UPDATE events SET instant = ?1", [invalid])
            .unwrap();
        let error = events.find(value!({}), value!({})).await.unwrap_err();
        let DbError::Coded { code, message, .. } = error else {
            panic!("{error}")
        };
        assert_eq!(code, "row_decode_failed");
        assert!(message.contains("instant"));
        assert!(!message.contains(invalid));
    }
    fixture
        .execute(
            "UPDATE events SET instant = ?1",
            ["1970-01-01T00:00:00.000Z"],
        )
        .unwrap();
    drop(fixture);
    let Output::Rows { rows, .. } = events.find(value!({}), value!({})).await.unwrap() else {
        panic!("find must return rows")
    };
    assert_eq!(rows[0]["instant"].as_i64(), Some(0));
    owner.close().await;
}
