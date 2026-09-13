use super::*;

fn fields() -> Value {
    value!({"payload":{"type":"json", "nullable":true}})
}

#[compio::test]
async fn sqlite_json_values_round_trip_through_the_orm() {
    let owner = super::fixtures::CollectionFixture::sqlite("documents", fields()).await;
    let db = &owner.database;
    exercise_json_values(db).await;

    // Model a file whose JSON constraint was bypassed by an external writer.
    let fixture = rusqlite::Connection::open(owner.sqlite_file.as_ref().unwrap()).unwrap();
    fixture.execute_batch("PRAGMA ignore_check_constraints = ON; UPDATE documents SET payload = 'secret_invalid_json'").unwrap();
    let error = db
        .collection("documents")
        .unwrap()
        .find(value!({}), value!({}))
        .await
        .unwrap_err();
    let DbError::Coded { code, message, .. } = error else {
        panic!("{error}")
    };
    assert_eq!(code, "row_decode_failed");
    assert!(message.contains("payload"));
    assert!(!message.contains("secret_invalid_json"));

    fixture
        .execute("UPDATE documents SET payload = ?1", ["\"true\""])
        .unwrap();
    drop(fixture);
    let Output::Rows { rows, .. } = db
        .collection("documents")
        .unwrap()
        .find(value!({}), value!({}))
        .await
        .unwrap()
    else {
        panic!("find must return rows")
    };
    assert!(!rows.is_empty());
    assert!(rows
        .iter()
        .all(|row| row["payload"] == Value::String("true".into())));
    owner.close().await;
}

#[compio::test]
async fn postgres_json_values_round_trip_through_the_orm() {
    let owner = super::fixtures::CollectionFixture::postgres("documents", fields()).await;
    exercise_json_values(&owner.database).await;
    owner.close().await;
}

async fn exercise_json_values(db: &Database) {
    let values = value!([
        "true", "null", "42", "[1]", "{\"key\":1}", "\"nested\"", "plain text",
        true, false, 42, 1.5, null, {"key":"true"}, [false,"null"],
    ]);
    for expected in values.as_array().unwrap() {
        let payload = expected.clone();
        let id = db
            .transaction(|tx| async move {
                let collection = tx.collection("documents")?;
                let Output::Rows { rows, .. } = collection
                    .insert(value!({"payload":payload.clone()}))
                    .await?
                else {
                    panic!("insert must return rows")
                };
                assert_eq!(
                    rows[0]["payload"], payload,
                    "insert must preserve the JSON type"
                );
                let id = rows[0]["id"].clone();
                let Output::Rows { rows, .. } = collection
                    .update(
                        value!({"id":id.clone()}),
                        value!({"payload":{"$set":payload.clone()}}),
                    )
                    .await?
                else {
                    panic!("update must return rows")
                };
                assert_eq!(
                    rows[0]["payload"], payload,
                    "update must preserve the JSON type"
                );
                Ok(id)
            })
            .await
            .unwrap();
        let Output::Rows { rows, .. } = db
            .collection("documents")
            .unwrap()
            .find(value!({"id":id}), value!({}))
            .await
            .unwrap()
        else {
            panic!("find must return rows")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(
            &rows[0]["payload"], expected,
            "find must preserve the JSON type"
        );
    }

    let id = db
        .collection("documents")
        .unwrap()
        .insert(value!({"payload":{"a":1,"b":[2,3]}}))
        .await
        .unwrap();
    let Output::Rows { rows: inserted, .. } = id else {
        panic!("insert must return rows")
    };
    let id = inserted[0]["id"].clone();
    let Output::Rows { rows, .. } = db
        .collection("documents")
        .unwrap()
        .find(
            value!({"payload":{"$eq":{"b":[2.0,3.0],"a":1.0}}}),
            value!({}),
        )
        .await
        .unwrap()
    else {
        panic!("find must return rows")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], id);
}
