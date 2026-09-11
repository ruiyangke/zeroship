use super::fixtures::CollectionFixture;
use super::*;

fn fields() -> Value {
    value!({
        "key":{"type":"string","unique":true},
        "secret":{"type":"number","encrypted":{"keyId":"array_fixture","wraps":"number"}},
        "strings":{"type":"array","items":"string"},
        "numbers":{"type":"array","items":"number"},
        "flags":{"type":"array","items":"boolean"},
        "payloads":{"type":"array","items":"json"},
        "nested":{"type":"object","shape":{"strings":{"type":"array","items":"string"}}}
    })
}

#[compio::test]
async fn sqlite_typed_arrays_validate_every_write_path() {
    let owner = CollectionFixture::sqlite("records", fields()).await;
    exercise_writes(&owner.database).await;
    owner.close().await;
}

#[compio::test]
async fn postgres_typed_arrays_validate_every_write_path() {
    let owner = CollectionFixture::postgres("records", fields()).await;
    exercise_writes(&owner.database).await;
    owner.close().await;
}

fn invalid_values() -> Vec<(&'static str, Value)> {
    vec![
        ("strings", value!(true)),
        ("strings", value!(3)),
        ("strings", value!(null)),
        ("numbers", value!("private_not_a_number")),
        ("numbers", value!(true)),
        ("numbers", value!(null)),
        ("numbers", Value::Decimal("1.5".into())),
        ("flags", value!(1)),
        ("flags", value!("true")),
        ("flags", value!(null)),
    ]
}

async fn exercise_writes(db: &Database) {
    let records = db.collection("records").unwrap();
    records
        .insert(value!({"key":"original","strings":["first"],"numbers":[1],"flags":[true]}))
        .await
        .unwrap();
    let Output::Rows { rows: before, .. } = records.find(value!({}), value!({})).await.unwrap()
    else {
        panic!("find must return rows")
    };
    for (field, invalid) in invalid_values() {
        let mut document = value!({"key":"invalid"});
        document
            .as_object_mut()
            .unwrap()
            .insert(field.into(), Value::Array(vec![invalid.clone()]));
        for operation in [
            Operation::Insert {
                document: document.clone(),
            },
            Operation::InsertMany {
                documents: Value::Array(vec![
                    value!({"key":"batch_valid","strings":["valid"]}),
                    document.clone(),
                ]),
            },
            Operation::Upsert {
                document: document.clone(),
                conflict_fields: value!(["key"]),
            },
        ] {
            assert_invalid(records.execute(operation).await.unwrap_err());
        }
        for many in [false, true] {
            for key in ["original", "missing"] {
                for operator in ["$set", "$push", "$pull", "$addToSet"] {
                    let operand = if operator == "$set" {
                        Value::Array(vec![invalid.clone()])
                    } else {
                        invalid.clone()
                    };
                    let patch = Value::Object(
                        [(
                            field.into(),
                            Value::Object([(operator.into(), operand)].into()),
                        )]
                        .into(),
                    );
                    assert_invalid(
                        records
                            .execute(Operation::Update {
                                filter: value!({"key":key}),
                                patch,
                                many,
                            })
                            .await
                            .unwrap_err(),
                    );
                }
            }
        }
    }
    for patch in [
        value!({"nested":{"strings":[false]}}),
        Value::Object([("strings".into(), Value::Json("[false]".into()))].into()),
    ] {
        assert_invalid(records.update(value!({}), patch).await.unwrap_err());
    }
    for many in [false, true] {
        assert_invalid(
            records
                .execute(Operation::Update {
                    filter: value!({"key":"missing"}),
                    patch: value!({"$set":{"secret":10,"strings":[false]}}),
                    many,
                })
                .await
                .unwrap_err(),
        );
    }
    let Output::Rows { rows: after, .. } = records.find(value!({}), value!({})).await.unwrap()
    else {
        panic!("find must return rows")
    };
    assert_eq!(
        before, after,
        "invalid array values must not mutate rows or insert a valid batch prefix"
    );

    let Output::Rows { rows, .. } = records.update(value!({}), value!({"strings":{"$push":"next"},"numbers":{"$push":2.5},"flags":{"$push":false},"payloads":{"$set":[null,true,1,"text",{"key":"value"},[2]]}})).await.unwrap() else {
        panic!("update must return rows")
    };
    assert_eq!(rows[0]["strings"], value!(["first", "next"]));
    assert_eq!(rows[0]["numbers"], value!([1, 2.5]));
    assert_eq!(rows[0]["flags"], value!([true, false]));
    assert_eq!(
        rows[0]["payloads"],
        value!([null,true,1,"text",{"key":"value"},[2]])
    );
}

fn assert_invalid(error: DbError) {
    assert!(
        matches!(
            &error,
            DbError::ValidationFailed {
                code: "invalid_array_element",
                ..
            }
        ),
        "{error:?}"
    );
    assert!(!error.message_str().contains("private_not_a_number"));
}

#[compio::test]
async fn corrupt_sqlite_array_items_are_refused_without_exposing_values() {
    let owner = CollectionFixture::sqlite("records", fields()).await;
    let records = owner.database.collection("records").unwrap();
    records
        .insert(value!({"key":"original","numbers":[1]}))
        .await
        .unwrap();
    let fixture = rusqlite::Connection::open(owner.sqlite_file.as_ref().unwrap()).unwrap();
    fixture
        .execute(
            "UPDATE records SET numbers = ?1",
            ["[\"private_not_a_number\"]"],
        )
        .unwrap();
    let error = records.find(value!({}), value!({})).await.unwrap_err();
    assert!(
        matches!(&error, DbError::Coded {code, ..} if code == "row_decode_failed"),
        "{error:?}"
    );
    assert!(error.message_str().contains("numbers"));
    assert!(!error.message_str().contains("private_not_a_number"));
    drop(fixture);
    owner.close().await;
}
