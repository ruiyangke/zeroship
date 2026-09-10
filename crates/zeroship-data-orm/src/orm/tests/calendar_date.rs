use super::fixtures::CollectionFixture;
use super::*;

fn fields() -> Value {
    value!({"birthday":{"type":"calendarDate", "nullable":true}})
}

#[compio::test]
async fn calendar_dates_round_trip_through_postgres() {
    let owner = CollectionFixture::postgres("people", fields()).await;
    exercise_calendar_dates(&owner.database).await;
    owner.close().await;
}

#[compio::test]
async fn calendar_dates_round_trip_through_sqlite_and_reject_corrupt_storage() {
    let owner = CollectionFixture::sqlite("people", fields()).await;
    let db = &owner.database;
    exercise_calendar_dates(db).await;
    let fixture = rusqlite::Connection::open(owner.sqlite_file.as_ref().unwrap()).unwrap();
    fixture.execute_batch("PRAGMA ignore_check_constraints = ON; UPDATE people SET birthday = 'private_not_a_date'").unwrap();
    let error = db
        .collection("people")
        .unwrap()
        .find(value!({}), value!({}))
        .await
        .unwrap_err();
    let DbError::Coded { code, message, .. } = error else {
        panic!("{error}")
    };
    assert_eq!(code, "row_decode_failed");
    assert!(message.contains("birthday"));
    assert!(!message.contains("private_not_a_date"));
    fixture
        .execute("UPDATE people SET birthday = ?1", ["0004-02-29"])
        .unwrap();
    drop(fixture);
    let Output::Rows { rows, .. } = db
        .collection("people")
        .unwrap()
        .find(value!({}), value!({}))
        .await
        .unwrap()
    else {
        panic!("find must return rows")
    };
    assert!(!rows.is_empty());
    assert!(
        rows.iter()
            .all(|row| row["birthday"] == value!("0004-02-29"))
    );
    owner.close().await;
}

async fn exercise_calendar_dates(db: &Database) {
    let people = db.collection("people").unwrap();
    for date in [
        "0001-01-01",
        "0004-02-29",
        "0099-12-31",
        "0100-03-01",
        "1969-12-31",
        "2000-02-29",
        "9999-12-31",
    ] {
        let encoded = <&str as EncodeValue<sql_types::CalendarDate>>::encode_value(date).unwrap();
        let id = db
            .transaction(|tx| async move {
                let people = tx.collection("people")?;
                let Output::Rows { rows, .. } = people.insert(value!({"birthday":encoded})).await?
                else {
                    panic!("insert must return rows")
                };
                let decoded = <String as DecodeValue<sql_types::CalendarDate>>::decode_value(
                    rows[0]["birthday"].clone(),
                )?;
                assert_eq!(decoded, date);
                let id = rows[0]["id"].clone();
                let Output::Rows { rows, .. } = people
                    .update(
                        value!({"id":id.clone()}),
                        value!({"birthday":{"$set":date}}),
                    )
                    .await?
                else {
                    panic!("update must return rows")
                };
                assert_eq!(rows[0]["birthday"], value!(date));
                Ok(id)
            })
            .await
            .unwrap();
        let Output::Rows { rows, .. } = people
            .find(value!({"id":id.clone(),"birthday":date}), value!({}))
            .await
            .unwrap()
        else {
            panic!("find must return rows")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["birthday"], value!(date));

        for patch in [
            value!({"birthday":"2026-02-30"}),
            value!({"birthday":{"$set":"2026-02-30"}}),
            value!({"$set":{"birthday":"2026-02-30"}}),
            value!({"birthday":{"$inc":1}}),
        ] {
            let error = people
                .update(value!({"id":id.clone()}), patch)
                .await
                .unwrap_err();
            assert!(matches!(error, DbError::ValidationFailed { .. }), "{error}");
        }
        let Output::Rows { rows, .. } = people.find(value!({"id":id}), value!({})).await.unwrap()
        else {
            panic!("find must return rows")
        };
        assert_eq!(
            rows[0]["birthday"],
            value!(date),
            "rejected updates must not change the row"
        );
    }
    let before = count(people.count(value!({}), value!({})).await.unwrap());
    for date in [
        "0000-01-01",
        "1900-02-29",
        "2026-02-30",
        "2026-01-01T00:00:00Z",
        "private_not_a_date",
    ] {
        for operation in [
            Operation::Insert {
                document: value!({"birthday":date}),
            },
            Operation::InsertMany {
                documents: value!([{"birthday":"2000-02-29"},{"birthday":date}]),
            },
            Operation::Upsert {
                document: value!({"birthday":date}),
                conflict_fields: value!(["id"]),
            },
        ] {
            let error = people.execute(operation).await.unwrap_err();
            assert!(matches!(error, DbError::ValidationFailed { .. }), "{error}");
            assert!(!error.to_string().contains("private_not_a_date"));
        }
    }
    assert_eq!(
        count(people.count(value!({}), value!({})).await.unwrap()),
        before
    );
    let Output::Rows { rows, .. } = people.insert(value!({"birthday":null})).await.unwrap() else {
        panic!("insert must return rows")
    };
    assert_eq!(rows[0]["birthday"], Value::Null);
}
