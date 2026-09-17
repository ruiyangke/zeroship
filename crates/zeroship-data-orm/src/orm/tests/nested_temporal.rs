use super::fixtures::CollectionFixture;
use super::*;

fn fields() -> Value {
    value!({
        "key":{"type":"string","unique":true},
        "instants":{"type":"array","items":"timestamp"},
        "days":{"type":"array","items":"calendarDate"},
        "profile":{"type":"object","shape":{
            "instant":{"type":"timestamp"},
            "schedule":{"type":"object","shape":{"instant":{"type":"timestamp"}}},
            "payload":{"type":"json"}
        }},
        "event":{"type":"union","discriminator":"kind","variants":[
            {"kind":{"type":"literal","literalValue":"dated"},"instant":{"type":"timestamp"}},
            {"kind":{"type":"literal","literalValue":"text"},"instant":{"type":"string"}}
        ]},
        "payload":{"type":"json"}
    })
}

#[compio::test]
async fn sqlite_nested_temporal_values_follow_the_descriptor() {
    let owner = CollectionFixture::sqlite("events", fields()).await;
    exercise(&owner.database).await;
    owner.close().await;
}

#[compio::test]
async fn postgres_nested_temporal_values_follow_the_descriptor() {
    let owner = CollectionFixture::postgres("events", fields()).await;
    exercise(&owner.database).await;
    owner.close().await;
}

async fn exercise(db: &Database) {
    let events = db.collection("events").unwrap();
    // A temporal value nested inside JSON is stored as whole milliseconds, so
    // the canonical text carries three fractional digits.
    let iso = "1969-12-31T23:59:59.999Z";
    let document = value!({
        "key":"created", "instants":[iso,0], "days":["0001-01-01","2000-02-29"],
        "profile":{"instant":iso,"schedule":{"instant":iso},"payload":{"instant":iso}},
        "event":{"kind":"dated","instant":iso}, "payload":[iso,0]
    });
    let Output::Rows { rows, .. } = events.insert(document.clone()).await.unwrap() else {
        panic!("insert must return rows")
    };
    check(&rows[0], &document);
    let id = rows[0]["id"].clone();
    for instants in [value!([-1, 0]), value!([iso, 0])] {
        for filter in [
            value!({"instants":instants.clone()}),
            value!({"instants":{"$in":[instants]}}),
        ] {
            let Output::Rows { rows, .. } = events.find(filter, value!({})).await.unwrap() else {
                panic!("find must return rows")
            };
            assert_eq!(rows.len(), 1, "equivalent temporal values must match");
            check(&rows[0], &document);
        }
    }
    for patch in [
        value!({"instants":[iso,0],"profile":document["profile"].clone()}),
        value!({"instants":{"$set":[iso,0]},"profile":{"$set":document["profile"].clone()}}),
        value!({"$set":{"instants":[iso,0],"profile":document["profile"].clone()}}),
    ] {
        let Output::Rows { rows, .. } = events
            .update(value!({"id":id.clone()}), patch)
            .await
            .unwrap()
        else {
            panic!("update must return rows")
        };
        check(&rows[0], &document);
    }
    let Output::Rows { rows, .. } = events
        .execute(Operation::Upsert {
            document: document.clone(),
            conflict_fields: value!(["key"]),
        })
        .await
        .unwrap()
    else {
        panic!("upsert must return rows")
    };
    assert_eq!(rows[0]["id"], id);
    check(&rows[0], &document);

    let mut batch_document = document.clone();
    batch_document["key"] = value!("batch_created");
    let Output::Rows { rows, .. } = db
        .transaction(|tx| async move {
            tx.collection("events")?
                .execute(Operation::InsertMany {
                    documents: value!([batch_document]),
                })
                .await
        })
        .await
        .unwrap()
    else {
        panic!("batch insert must return rows")
    };
    check(&rows[0], &document);
    let before = count(events.count(value!({}), value!({})).await.unwrap());
    for patch in [
        value!({"instants":["private_not_a_timestamp"]}),
        value!({"instants":[null]}),
        value!({"days":["1900-02-29"]}),
        value!({"profile":{"schedule":{"instant":"2026-02-30"}}}),
        value!({"event":{"kind":"dated","instant":"2026-02-30"}}),
    ] {
        let error = events
            .update(value!({"id":id.clone()}), patch.clone())
            .await
            .unwrap_err();
        assert!(matches!(error, DbError::ValidationFailed { .. }), "{error}");
        assert!(!error.to_string().contains("private_not_a_timestamp"));
        let mut invalid = document.clone();
        invalid["key"] = value!("invalid");
        invalid
            .as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        for operation in [
            Operation::Insert {
                document: invalid.clone(),
            },
            Operation::InsertMany {
                documents: value!([{"key":"batch_valid","instants":[0]},invalid.clone()]),
            },
            Operation::Upsert {
                document: invalid,
                conflict_fields: value!(["key"]),
            },
        ] {
            assert!(matches!(
                events.execute(operation).await.unwrap_err(),
                DbError::ValidationFailed { .. }
            ));
        }
    }
    assert_eq!(
        count(events.count(value!({}), value!({})).await.unwrap()),
        before
    );
    let Output::Rows { rows, .. } = events
        .update(
            value!({"id":id}),
            value!({"event":{"kind":"text","instant":iso}}),
        )
        .await
        .unwrap()
    else {
        panic!("update must return rows")
    };
    assert_eq!(rows[0]["event"]["instant"].as_str(), Some(iso));
    Box::pin(nested_temporal_values_refuse_a_sub_millisecond_instant(
        &events,
    ))
    .await;
}

/// JSON carries numbers of milliseconds, so a nested instant with a finer part
/// is refused rather than floored into a different instant. Nothing is written.
async fn nested_temporal_values_refuse_a_sub_millisecond_instant(events: &Collection) {
    let before = count(events.count(value!({}), value!({})).await.unwrap());
    let fine = "1969-12-31T23:59:59.999999Z";
    for document in [
        value!({"key":"fine_array","instants":[fine]}),
        value!({"key":"fine_object","instants":[0],"profile":{"instant":fine}}),
        value!({"key":"fine_variant","instants":[0],"event":{"kind":"dated","instant":fine}}),
    ] {
        let error = events.insert(document).await.unwrap_err();
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
    }
    assert_eq!(
        count(events.count(value!({}), value!({})).await.unwrap()),
        before,
        "a refused nested instant must write nothing"
    );
    // Control: the same instant at whole-millisecond resolution is accepted.
    events
        .insert(value!({"key":"coarse","instants":["1969-12-31T23:59:59.999Z"]}))
        .await
        .unwrap();
}

fn check(row: &Value, document: &Value) {
    assert_eq!(row["instants"], value!([-1, 0]));
    assert_eq!(row["profile"]["instant"].as_i64(), Some(-1));
    assert_eq!(row["profile"]["schedule"]["instant"].as_i64(), Some(-1));
    assert_eq!(row["event"]["instant"].as_i64(), Some(-1));
    assert_eq!(row["days"], document["days"]);
    assert_eq!(row["profile"]["payload"], document["profile"]["payload"]);
    assert_eq!(row["payload"], document["payload"]);
}

#[compio::test]
async fn postgres_timestamp_array_operations_compare_canonical_instants() {
    let owner = CollectionFixture::postgres("events", fields()).await;
    let events = owner.database.collection("events").unwrap();
    events
        .insert(value!({"key":"array_ops","instants":[-1,0]}))
        .await
        .unwrap();
    for (operation, expected) in [
        (
            value!({"$push":"1970-01-01T00:00:00.001Z"}),
            value!([-1, 0, 1]),
        ),
        (
            value!({"$addToSet":"1970-01-01T01:00:00.001+01:00"}),
            value!([-1, 0, 1]),
        ),
        (value!({"$pull":"1970-01-01"}), value!([-1, 1])),
    ] {
        let Output::Rows { rows, .. } = events
            .update(value!({}), value!({"instants":operation}))
            .await
            .unwrap()
        else {
            panic!("update must return rows")
        };
        assert_eq!(rows[0]["instants"], expected);
    }
    owner.close().await;
}

#[compio::test]
async fn corrupt_sqlite_temporal_json_fails_without_exposing_contents() {
    let owner = CollectionFixture::sqlite("events", fields()).await;
    let events = owner.database.collection("events").unwrap();
    events
        .insert(value!({"key":"corrupt","instants":[0]}))
        .await
        .unwrap();
    let fixture = rusqlite::Connection::open(owner.sqlite_file.as_ref().unwrap()).unwrap();
    fixture
        .execute(
            "UPDATE events SET instants = ?1",
            ["[\"private_not_a_timestamp\"]"],
        )
        .unwrap();
    let error = events.find(value!({}), value!({})).await.unwrap_err();
    let DbError::Coded { code, message, .. } = error else {
        panic!("{error}")
    };
    assert_eq!(code, "row_decode_failed");
    assert!(message.contains("instants"));
    assert!(!message.contains("private_not_a_timestamp"));
    drop(fixture);
    owner.close().await;
}
